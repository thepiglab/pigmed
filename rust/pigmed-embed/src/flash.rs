//! Fused attention for the PigMed ONNX graphs, as an ONNX Runtime custom operator written in Rust.
//!
//! `pigmed.ai::FlashAttention(Q[B,Hq,S,D], K[B,Hkv,S,D], V[B,Hkv,S,Dv], key_mask[B,S] int64) -> O[B,S,Hq,Dv]`
//! attributes: `scale` (float), `window` (int; |i-j| <= window, or -1 for full attention).
//!
//! Same math as transformers' eager attention for these models (scores * scale, key-padding and symmetric
//! sliding-window masks, fp32 softmax, GQA by kv head = h / (Hq/Hkv)), computed FlashAttention-style:
//! query blocks x key blocks with an online softmax, so no S x S score or mask tensor is ever materialised.
//! Key blocks outside a query block's window are skipped, so sliding-window layers cost O(S * window).
//! Tile GEMMs use the `gemm` crate; tasks (batch, head, query block) run on ONNX Runtime's intra-op pool.

use ort::operator::{ComputeContext, Kernel, KernelContext, Operator, OperatorDomain, OperatorInput, OperatorOutput};
use ort::value::TensorElementType;

/// Tile sizes (rows of Q per task, keys per inner block); override with PIGMED_FA_BQ / PIGMED_FA_BK.
fn tiles() -> (usize, usize) {
    static T: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    *T.get_or_init(|| {
        let g = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        (g("PIGMED_FA_BQ", 64), g("PIGMED_FA_BK", 256))
    })
}

/// Optional phase timers (ns) for tuning: set PIGMED_FA_PROF=1.
pub static PROF: [std::sync::atomic::AtomicU64; 3] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];
fn prof_on() -> bool {
    static P: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *P.get_or_init(|| std::env::var("PIGMED_FA_PROF").is_ok())
}

#[cfg(target_arch = "x86_64")]
mod avx {
    use std::arch::x86_64::*;

    /// 8-wide Cephes expf (same polynomial as `fast_exp`).
    #[inline]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn exp8(x: __m256) -> __m256 {
        unsafe {
            let x = _mm256_min_ps(_mm256_max_ps(x, _mm256_set1_ps(-87.3)), _mm256_set1_ps(88.7));
            let fx = _mm256_floor_ps(_mm256_fmadd_ps(x, _mm256_set1_ps(std::f32::consts::LOG2_E), _mm256_set1_ps(0.5)));
            let g = _mm256_fnmadd_ps(fx, _mm256_set1_ps(0.693_359_4), x);
            let g = _mm256_fnmadd_ps(fx, _mm256_set1_ps(-2.121_944_4e-4), g);
            let mut y = _mm256_set1_ps(1.987_569_1e-4);
            y = _mm256_fmadd_ps(y, g, _mm256_set1_ps(1.398_199_9e-3));
            y = _mm256_fmadd_ps(y, g, _mm256_set1_ps(8.333_452e-3));
            y = _mm256_fmadd_ps(y, g, _mm256_set1_ps(4.166_579_6e-2));
            y = _mm256_fmadd_ps(y, g, _mm256_set1_ps(0.166_666_65));
            y = _mm256_fmadd_ps(y, g, _mm256_set1_ps(0.5));
            y = _mm256_fmadd_ps(_mm256_mul_ps(y, g), g, _mm256_add_ps(g, _mm256_set1_ps(1.0)));
            let e = _mm256_slli_epi32(_mm256_add_epi32(_mm256_cvttps_epi32(fx), _mm256_set1_epi32(127)), 23);
            _mm256_mul_ps(y, _mm256_castsi256_ps(e))
        }
    }

    #[inline]
    unsafe fn hsum(v: __m256) -> f32 {
        unsafe {
            let mut a = [0f32; 8];
            _mm256_storeu_ps(a.as_mut_ptr(), v);
            a.iter().sum()
        }
    }

    #[inline]
    unsafe fn hmax(v: __m256) -> f32 {
        unsafe {
            let mut a = [0f32; 8];
            _mm256_storeu_ps(a.as_mut_ptr(), v);
            a.iter().fold(f32::NEG_INFINITY, |x, &y| x.max(y))
        }
    }

    /// row[c] = row[c] * scale + bias[c] for c in range; returns max over the whole row.
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn scale_bias_max(row: &mut [f32], bias: &[f32], scale: f32) -> f32 {
        unsafe {
            let n = row.len();
            let (sc, mut mx) = (_mm256_set1_ps(scale), _mm256_set1_ps(f32::NEG_INFINITY));
            let mut c = 0;
            while c + 8 <= n {
                let x = _mm256_fmadd_ps(_mm256_loadu_ps(row.as_ptr().add(c)), sc, _mm256_loadu_ps(bias.as_ptr().add(c)));
                _mm256_storeu_ps(row.as_mut_ptr().add(c), x);
                mx = _mm256_max_ps(mx, x);
                c += 8;
            }
            let mut m = hmax(mx);
            while c < n {
                row[c] = row[c] * scale + bias[c];
                m = m.max(row[c]);
                c += 1;
            }
            m
        }
    }

    /// row <- exp(row - m), returns the sum.
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn exp_sum(row: &mut [f32], m: f32) -> f32 {
        unsafe {
            let n = row.len();
            let (mm, mut acc) = (_mm256_set1_ps(m), _mm256_setzero_ps());
            let mut c = 0;
            while c + 8 <= n {
                let p = exp8(_mm256_sub_ps(_mm256_loadu_ps(row.as_ptr().add(c)), mm));
                _mm256_storeu_ps(row.as_mut_ptr().add(c), p);
                acc = _mm256_add_ps(acc, p);
                c += 8;
            }
            let mut s = hsum(acc);
            while c < n {
                let p = super::fast_exp(row[c] - m);
                row[c] = p;
                s += p;
                c += 1;
            }
            s
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod avx512 {
    use std::arch::x86_64::*;

    #[inline]
    #[target_feature(enable = "avx512f")]
    unsafe fn exp16(x: __m512) -> __m512 {
        let x = _mm512_min_ps(_mm512_max_ps(x, _mm512_set1_ps(-87.3)), _mm512_set1_ps(88.7));
        let fx = _mm512_roundscale_ps::<{ _MM_FROUND_TO_NEG_INF | _MM_FROUND_NO_EXC }>(_mm512_fmadd_ps(
            x,
            _mm512_set1_ps(std::f32::consts::LOG2_E),
            _mm512_set1_ps(0.5),
        ));
        let g = _mm512_fnmadd_ps(fx, _mm512_set1_ps(0.693_359_4), x);
        let g = _mm512_fnmadd_ps(fx, _mm512_set1_ps(-2.121_944_4e-4), g);
        let mut y = _mm512_set1_ps(1.987_569_1e-4);
        y = _mm512_fmadd_ps(y, g, _mm512_set1_ps(1.398_199_9e-3));
        y = _mm512_fmadd_ps(y, g, _mm512_set1_ps(8.333_452e-3));
        y = _mm512_fmadd_ps(y, g, _mm512_set1_ps(4.166_579_6e-2));
        y = _mm512_fmadd_ps(y, g, _mm512_set1_ps(0.166_666_65));
        y = _mm512_fmadd_ps(y, g, _mm512_set1_ps(0.5));
        y = _mm512_fmadd_ps(_mm512_mul_ps(y, g), g, _mm512_add_ps(g, _mm512_set1_ps(1.0)));
        let e = _mm512_slli_epi32::<23>(_mm512_add_epi32(_mm512_cvttps_epi32(fx), _mm512_set1_epi32(127)));
        _mm512_mul_ps(y, _mm512_castsi512_ps(e))
    }

    #[target_feature(enable = "avx512f")]
    pub unsafe fn scale_bias_max(row: &mut [f32], bias: &[f32], scale: f32) -> f32 {
        unsafe {
            let n = row.len();
            let (sc, mut mx) = (_mm512_set1_ps(scale), _mm512_set1_ps(f32::NEG_INFINITY));
            let mut c = 0;
            while c + 16 <= n {
                let x = _mm512_fmadd_ps(_mm512_loadu_ps(row.as_ptr().add(c)), sc, _mm512_loadu_ps(bias.as_ptr().add(c)));
                _mm512_storeu_ps(row.as_mut_ptr().add(c), x);
                mx = _mm512_max_ps(mx, x);
                c += 16;
            }
            let mut m = _mm512_reduce_max_ps(mx);
            while c < n {
                row[c] = row[c] * scale + bias[c];
                m = m.max(row[c]);
                c += 1;
            }
            m
        }
    }

    #[target_feature(enable = "avx512f")]
    pub unsafe fn exp_sum(row: &mut [f32], m: f32) -> f32 {
        unsafe {
            let n = row.len();
            let (mm, mut acc) = (_mm512_set1_ps(m), _mm512_setzero_ps());
            let mut c = 0;
            while c + 16 <= n {
                let p = exp16(_mm512_sub_ps(_mm512_loadu_ps(row.as_ptr().add(c)), mm));
                _mm512_storeu_ps(row.as_mut_ptr().add(c), p);
                acc = _mm512_add_ps(acc, p);
                c += 16;
            }
            let mut s = _mm512_reduce_add_ps(acc);
            while c < n {
                let p = super::fast_exp(row[c] - m);
                row[c] = p;
                s += p;
                c += 1;
            }
            s
        }
    }
}

fn has_avx512() -> bool {
    static A: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *A.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            is_x86_feature_detected!("avx512f") && std::env::var("PIGMED_NO_AVX512").is_err()
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    })
}

fn has_avx2() -> bool {
    static A: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *A.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma")
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    })
}

#[inline(always)]
fn max8(v: &[f32]) -> f32 {
    let mut m = [f32::NEG_INFINITY; 8];
    let ch = v.chunks_exact(8);
    let rem = ch.remainder();
    for c in ch {
        for k in 0..8 {
            m[k] = m[k].max(c[k]);
        }
    }
    let mut r = m.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
    for &x in rem {
        r = r.max(x);
    }
    r
}

/// x <- exp(x - m) in place, returns the sum (8-lane accumulators so it vectorises).
#[inline(always)]
fn exp_sum(v: &mut [f32], m: f32) -> f32 {
    let mut acc = [0f32; 8];
    let mut ch = v.chunks_exact_mut(8);
    for c in &mut ch {
        for k in 0..8 {
            let p = fast_exp(c[k] - m);
            c[k] = p;
            acc[k] += p;
        }
    }
    let mut s: f32 = acc.iter().sum();
    for x in ch.into_remainder() {
        let p = fast_exp(*x - m);
        *x = p;
        s += p;
    }
    s
}

pub struct FlashAttention;

pub struct FlashKernel {
    scale: f32,
    window: i64,
}

impl Operator for FlashAttention {
    type Kernel<'attr> = FlashKernel;

    fn name(&self) -> &str {
        "FlashAttention"
    }

    fn inputs(&self) -> impl IntoIterator<Item = OperatorInput> {
        [
            OperatorInput::required(TensorElementType::Float32),
            OperatorInput::required(TensorElementType::Float32),
            OperatorInput::required(TensorElementType::Float32),
            OperatorInput::required(TensorElementType::Int64),
        ]
    }

    fn outputs(&self) -> impl IntoIterator<Item = OperatorOutput> {
        [OperatorOutput::required(TensorElementType::Float32)]
    }

    fn create_kernel<'attr>(&self, k: &KernelContext<'attr>) -> ort::Result<Self::Kernel<'attr>> {
        Ok(FlashKernel { scale: k.get::<f32>("scale").unwrap_or(1.0), window: k.get::<i64>("window").unwrap_or(-1) })
    }
}

pub fn domain() -> ort::Result<OperatorDomain> {
    OperatorDomain::new("pigmed.ai")?.add(FlashAttention)?.add(AudioChunkAttention)?.add(ChannelLayerNorm)
}

#[derive(Clone, Copy)]
struct SendPtr(*mut f32);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

/// Cephes-style expf (rel. error ~1e-7), branch-free so the row loops vectorise (AVX2/FMA).
#[inline(always)]
fn fast_exp(x: f32) -> f32 {
    let x = x.clamp(-87.3, 88.7);
    let fx = (x * std::f32::consts::LOG2_E + 0.5).floor();
    let g = x - fx * 0.693_359_4 - fx * -2.121_944_4e-4;
    let mut y = 1.987_569_1e-4f32;
    y = y * g + 1.398_199_9e-3;
    y = y * g + 8.333_452e-3;
    y = y * g + 4.166_579_6e-2;
    y = y * g + 0.166_666_65;
    y = y * g + 0.5;
    y = y * g * g + g + 1.0;
    y * f32::from_bits(((fx as i32 + 127) << 23) as u32)
}


#[derive(Clone, Copy, Debug)]
pub struct Dims {
    pub b: usize,
    pub hq: usize,
    pub hk: usize,
    pub s: usize,
    pub d: usize,
    pub dv: usize,
}

impl Dims {
    pub fn tasks(&self) -> usize {
        self.b * self.hq * self.s.div_ceil(tiles().0)
    }
}

/// One (batch, head, query-block) task. Writes rows [i0, i1) of head h of `out` ([B, S, Hq, Dv]).
///
/// # Safety
/// `out` must point to a [B, S, Hq, Dv] f32 buffer; tasks write disjoint rows.
#[allow(clippy::too_many_arguments)]
pub unsafe fn run_task(dm: &Dims, qd: &[f32], kd: &[f32], vd: &[f32], md: &[i64], scale: f32, window: i64, out: *mut f32, task: usize) {
    let Dims { hq, hk, s, d, dv, .. } = *dm;
    let (bq, bk) = tiles();
    let avx2 = has_avx2();
    let avx512 = has_avx512();
    let rep = hq / hk;
    let nqb = s.div_ceil(bq);
    let qb = task % nqb;
    let h = (task / nqb) % hq;
    let bi = task / (nqb * hq);
    let kh = h / rep;
    let (i0, i1) = (qb * bq, ((qb + 1) * bq).min(s));
    let nq = i1 - i0;
    let (jlo, jhi) = if window >= 0 {
        let w = window as usize;
        (i0.saturating_sub(w), (i1 - 1 + w + 1).min(s))
    } else {
        (0, s)
    };
    let qp = &qd[((bi * hq + h) * s + i0) * d..];
    let kbase = (bi * hk + kh) * s;
    let mask = &md[bi * s..(bi + 1) * s];
    let mut acc = vec![0f32; nq * dv];
    let mut mrow = vec![f32::NEG_INFINITY; nq];
    let mut lrow = vec![0f32; nq];
    let mut sblk = vec![0f32; nq * bk];
    let mut kbias = vec![0f32; bk];
    let mut jb = jlo;
    while jb < jhi {
        let j1 = (jb + bk).min(jhi);
        let nk = j1 - jb;
        if mask[jb..j1].iter().all(|&x| x == 0) {
            jb = j1;
            continue;
        }
        for (c, kb) in kbias[..nk].iter_mut().enumerate() {
            *kb = if mask[jb + c] != 0 { 0.0 } else { f32::NEG_INFINITY };
        }
        let prof = prof_on();
        let t0 = if prof { Some(std::time::Instant::now()) } else { None };
        unsafe {
            // S[nq, nk] = Q[nq, d] . K[nk, d]^T
            gemm::gemm(
                nq, nk, d,
                sblk.as_mut_ptr(), 1, nk as isize, false,
                qp.as_ptr(), 1, d as isize,
                kd.as_ptr().add((kbase + jb) * d), d as isize, 1,
                0.0, 1.0, false, false, false, gemm::Parallelism::None,
            );
        }
        let t1 = t0.map(|t| {
            PROF[0].fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
            std::time::Instant::now()
        });
        for r in 0..nq {
            let i = i0 + r;
            let (lo, hi) = if window >= 0 {
                let w = window as usize;
                (i.saturating_sub(w), (i + w + 1).min(s))
            } else {
                (0, s)
            };
            let row = &mut sblk[r * nk..(r + 1) * nk];
            // window as a column range for this row (no per-element branches), key padding as a bias
            let clo = lo.saturating_sub(jb).min(nk);
            let chi = hi.saturating_sub(jb).min(nk).max(clo);
            row[..clo].fill(f32::NEG_INFINITY);
            row[chi..].fill(f32::NEG_INFINITY);
            let mx = if avx512 {
                #[cfg(target_arch = "x86_64")]
                let m = unsafe { avx512::scale_bias_max(&mut row[clo..chi], &kbias[clo..chi], scale) };
                #[cfg(not(target_arch = "x86_64"))]
                let m = f32::NEG_INFINITY;
                m
            } else if avx2 {
                // SAFETY: AVX2+FMA checked at runtime
                #[cfg(target_arch = "x86_64")]
                let m = unsafe { avx::scale_bias_max(&mut row[clo..chi], &kbias[clo..chi], scale) };
                #[cfg(not(target_arch = "x86_64"))]
                let m = f32::NEG_INFINITY;
                m
            } else {
                for (x, kb) in row[clo..chi].iter_mut().zip(&kbias[clo..chi]) {
                    *x = *x * scale + *kb;
                }
                max8(row)
            };
            if mx == f32::NEG_INFINITY {
                row.iter_mut().for_each(|x| *x = 0.0);
                continue;
            }
            let mnew = mrow[r].max(mx);
            let alpha = if mrow[r] == f32::NEG_INFINITY { 0.0 } else { fast_exp(mrow[r] - mnew) };
            // exp(-inf - m) clamps to ~1e-38: below f32 resolution of any non-empty softmax row
            #[cfg(target_arch = "x86_64")]
            let sum = if avx512 {
                unsafe { avx512::exp_sum(row, mnew) }
            } else if avx2 {
                unsafe { avx::exp_sum(row, mnew) }
            } else {
                exp_sum(row, mnew)
            };
            #[cfg(not(target_arch = "x86_64"))]
            let sum = exp_sum(row, mnew);
            lrow[r] = lrow[r] * alpha + sum;
            mrow[r] = mnew;
            if alpha != 1.0 {
                acc[r * dv..(r + 1) * dv].iter_mut().for_each(|a| *a *= alpha);
            }
        }
        let t2 = t1.map(|t| {
            PROF[1].fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
            std::time::Instant::now()
        });
        unsafe {
            // acc[nq, dv] += P[nq, nk] . V[nk, dv]
            gemm::gemm(
                nq, dv, nk,
                acc.as_mut_ptr(), 1, dv as isize, true,
                sblk.as_ptr(), 1, nk as isize,
                vd.as_ptr().add((kbase + jb) * dv), 1, dv as isize,
                1.0, 1.0, false, false, false, gemm::Parallelism::None,
            );
        }
        if let Some(t) = t2 {
            PROF[2].fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
        }
        jb = j1;
    }
    for r in 0..nq {
        let inv = if lrow[r] > 0.0 { 1.0 / lrow[r] } else { 0.0 };
        let o = ((bi * s + i0 + r) * hq + h) * dv;
        for e in 0..dv {
            unsafe { *out.add(o + e) = acc[r * dv + e] * inv };
        }
    }
}

impl Kernel for FlashKernel {
    fn compute(&mut self, ctx: &ComputeContext) -> ort::Result<()> {
        let err = |m: &str| ort::Error::new(m.to_string());
        let q = ctx.input(0)?.ok_or_else(|| err("q"))?;
        let k = ctx.input(1)?.ok_or_else(|| err("k"))?;
        let v = ctx.input(2)?.ok_or_else(|| err("v"))?;
        let m = ctx.input(3)?.ok_or_else(|| err("mask"))?;
        let (qs, qd) = q.try_extract_tensor::<f32>()?;
        let (ks, kd) = k.try_extract_tensor::<f32>()?;
        let (vs, vd) = v.try_extract_tensor::<f32>()?;
        let (_, md) = m.try_extract_tensor::<i64>()?;
        let (b, hq, s, d) = (qs[0] as usize, qs[1] as usize, qs[2] as usize, qs[3] as usize);
        let hk = ks[1] as usize;
        let dv = vs[3] as usize;
        if hq % hk != 0 || ks[2] as usize != s || ks[3] as usize != d {
            return Err(err("FlashAttention: shape mismatch"));
        }
        let dims = Dims { b, hq, hk, s, d, dv };
        let mut out = ctx.output(0, vec![b as i64, s as i64, hq as i64, dv as i64])?.ok_or_else(|| err("out"))?;
        let (_, od) = out.try_extract_tensor_mut::<f32>()?;
        let optr = SendPtr(od.as_mut_ptr());
        let (scale, window) = (self.scale, self.window);
        let total = dims.tasks();
        ctx.par_for(total, total, move |task| {
            let optr = optr;
            unsafe { run_task(&dims, qd, kd, vd, md, scale, window, optr.0, task) };
        })?;
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------------- audio
/// `pigmed.ai::AudioChunkAttention(Q[B,T,H,D], K[B,T,H,D], V[B,T,H,D], RelK[left+1,H,D], key_mask[B,T]) -> O[B,T,H*D]`
/// Gemma 4 audio chunked local attention (q/k already scaled): query q sees context slots o in [0, 2*left) of its
/// block (keys j = blk*left + o - left); logit = q.k_j + q.relk[o - i] (when 0 <= o-i <= left), soft-capped;
/// allowed iff 0 <= q-j < left, j in range and valid, else `invalid`. Softmax over the 2*left slots, out = sum p v_j.
pub struct AudioChunkAttention;

pub struct AudioKernel {
    left: usize,
    softcap: f32,
    invalid: f32,
}

impl Operator for AudioChunkAttention {
    type Kernel<'attr> = AudioKernel;

    fn name(&self) -> &str {
        "AudioChunkAttention"
    }

    fn inputs(&self) -> impl IntoIterator<Item = OperatorInput> {
        [
            OperatorInput::required(TensorElementType::Float32),
            OperatorInput::required(TensorElementType::Float32),
            OperatorInput::required(TensorElementType::Float32),
            OperatorInput::required(TensorElementType::Float32),
            OperatorInput::required(TensorElementType::Int64),
        ]
    }

    fn outputs(&self) -> impl IntoIterator<Item = OperatorOutput> {
        [OperatorOutput::required(TensorElementType::Float32)]
    }

    fn create_kernel<'attr>(&self, k: &KernelContext<'attr>) -> ort::Result<Self::Kernel<'attr>> {
        Ok(AudioKernel {
            left: k.get::<i64>("left").unwrap_or(12) as usize,
            softcap: k.get::<f32>("softcap").unwrap_or(50.0),
            invalid: k.get::<f32>("invalid").unwrap_or(-1e9),
        })
    }
}

impl Kernel for AudioKernel {
    fn compute(&mut self, ctx: &ComputeContext) -> ort::Result<()> {
        let err = |m: &str| ort::Error::new(m.to_string());
        let inp = |i: usize| ctx.input(i).and_then(|x| x.ok_or_else(|| err("missing input")));
        let (q, k, v, rk, m) = (inp(0)?, inp(1)?, inp(2)?, inp(3)?, inp(4)?);
        let (qs, qd) = q.try_extract_tensor::<f32>()?;
        let (_, kd) = k.try_extract_tensor::<f32>()?;
        let (_, vd) = v.try_extract_tensor::<f32>()?;
        let (rs, rd) = rk.try_extract_tensor::<f32>()?;
        let (_, md) = m.try_extract_tensor::<i64>()?;
        let (b, t, h, d) = (qs[0] as usize, qs[1] as usize, qs[2] as usize, qs[3] as usize);
        let (left, softcap, invalid) = (self.left, self.softcap, self.invalid);
        if rs[0] as usize != left + 1 {
            return Err(err("AudioChunkAttention: relk rows != left+1"));
        }
        let mut out = ctx.output(0, vec![b as i64, t as i64, (h * d) as i64])?.ok_or_else(|| err("out"))?;
        let (_, od) = out.try_extract_tensor_mut::<f32>()?;
        let optr = SendPtr(od.as_mut_ptr());
        let nblk = t.div_ceil(left);
        let total = b * h * nblk;
        let nctx = 2 * left;
        ctx.par_for(total, total, move |task| {
            let optr = optr;
            let blk = task % nblk;
            let hh = (task / nblk) % h;
            let bi = task / (nblk * h);
            let at = |_: &[f32], tt: usize| -> usize { ((bi * t + tt) * h + hh) * d };
            let mut lg = vec![0f32; nctx];
            for i in 0..left {
                let qq = blk * left + i;
                if qq >= t {
                    break;
                }
                let qv = &qd[at(qd, qq)..at(qd, qq) + d];
                for (o, l) in lg.iter_mut().enumerate() {
                    let j = (blk * left + o) as isize - left as isize;
                    let inr = j >= 0 && (j as usize) < t;
                    let mut acc = 0f32;
                    if inr {
                        let kv = &kd[at(kd, j as usize)..at(kd, j as usize) + d];
                        acc += qv.iter().zip(kv).map(|(a, b)| a * b).sum::<f32>();
                    }
                    let p = o as isize - i as isize;
                    if p >= 0 && p as usize <= left {
                        let r = &rd[(p as usize * h + hh) * d..(p as usize * h + hh) * d + d];
                        acc += qv.iter().zip(r).map(|(a, b)| a * b).sum::<f32>();
                    }
                    let capped = (acc / softcap).tanh() * softcap;
                    let dd = qq as isize - j;
                    let ok = inr && dd >= 0 && (dd as usize) < left && md[bi * t + j as usize] != 0;
                    *l = if ok { capped } else { invalid };
                }
                let mx = lg.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                let mut z = 0f32;
                for l in lg.iter_mut() {
                    *l = (*l - mx).exp();
                    z += *l;
                }
                let o0 = at(qd, qq);
                let dst = unsafe { std::slice::from_raw_parts_mut(optr.0.add(o0), d) };
                dst.iter_mut().for_each(|x| *x = 0.0);
                for (o, p) in lg.iter().enumerate() {
                    let j = (blk * left + o) as isize - left as isize;
                    if j >= 0 && (j as usize) < t && *p != 0.0 {
                        let w = p / z;
                        let vv = &vd[at(vd, j as usize)..at(vd, j as usize) + d];
                        dst.iter_mut().zip(vv).for_each(|(x, y)| *x += w * y);
                    }
                }
            }
        })?;
        Ok(())
    }
}


/// `ChannelLayerNorm(X[N,C,H,W], W[C]; eps, relu) -> Y[N,C,H,W]`: LayerNorm over the channel axis of an NCHW tensor
/// (biased variance, no bias term), optionally followed by ReLU. Gemma 4's audio subsampling convs write this as
/// permute(0,2,3,1) -> LayerNorm -> permute(0,3,1,2); ONNX Runtime ran the 48 MB channel-last transpose of a 30 s
/// clip single-threaded (~190 ms, 12 % of the audio tower). This normalises in place of layout, in tiles of
/// contiguous positions (f64 moments), on ORT's intra-op pool.
pub struct ChannelLayerNorm;

pub struct ChannelLnKernel {
    eps: f32,
    relu: bool,
}

impl Operator for ChannelLayerNorm {
    type Kernel<'attr> = ChannelLnKernel;

    fn name(&self) -> &str {
        "ChannelLayerNorm"
    }

    fn inputs(&self) -> impl IntoIterator<Item = OperatorInput> {
        [OperatorInput::required(TensorElementType::Float32), OperatorInput::required(TensorElementType::Float32)]
    }

    fn outputs(&self) -> impl IntoIterator<Item = OperatorOutput> {
        [OperatorOutput::required(TensorElementType::Float32)]
    }

    fn create_kernel<'attr>(&self, k: &KernelContext<'attr>) -> ort::Result<Self::Kernel<'attr>> {
        Ok(ChannelLnKernel { eps: k.get::<f32>("eps").unwrap_or(1e-6), relu: k.get::<i64>("relu").unwrap_or(0) != 0 })
    }
}

impl Kernel for ChannelLnKernel {
    fn compute(&mut self, ctx: &ComputeContext) -> ort::Result<()> {
        let err = |m: &str| ort::Error::new(m.to_string());
        let inp = |i: usize| ctx.input(i).and_then(|x| x.ok_or_else(|| err("missing input")));
        let (x, w) = (inp(0)?, inp(1)?);
        let (xs, xd) = x.try_extract_tensor::<f32>()?;
        let (_, wd) = w.try_extract_tensor::<f32>()?;
        if xs.len() < 2 {
            return Err(err("ChannelLayerNorm: rank < 2"));
        }
        let (n, c) = (xs[0] as usize, xs[1] as usize);
        let p: usize = xs[2..].iter().map(|&v| v as usize).product();
        if wd.len() != c {
            return Err(err("ChannelLayerNorm: weight length != channels"));
        }
        let mut out = ctx.output(0, xs.to_vec())?.ok_or_else(|| err("out"))?;
        let (_, od) = out.try_extract_tensor_mut::<f32>()?;
        let optr = SendPtr(od.as_mut_ptr());
        const TP: usize = 256;
        let tiles = p.div_ceil(TP);
        let (eps, relu) = (self.eps as f64, self.relu);
        ctx.par_for(n * tiles, n * tiles, move |task| {
            let optr = optr;
            let (ni, p0) = (task / tiles, (task % tiles) * TP);
            let len = TP.min(p - p0);
            let base = ni * c * p + p0;
            let row = |ci: usize| &xd[base + ci * p..base + ci * p + len];
            let mut mean = [0f64; TP];
            let mut m2 = [0f64; TP];
            for ci in 0..c {
                for (a, &v) in mean.iter_mut().zip(row(ci)) {
                    *a += v as f64;
                }
            }
            mean[..len].iter_mut().for_each(|a| *a /= c as f64);
            for ci in 0..c {
                for ((a, &v), mu) in m2.iter_mut().zip(row(ci)).zip(&mean) {
                    let d = v as f64 - mu;
                    *a += d * d;
                }
            }
            let rstd: Vec<f32> = m2[..len].iter().map(|s| (1.0 / (s / c as f64 + eps).sqrt()) as f32).collect();
            let meanf: Vec<f32> = mean[..len].iter().map(|&m| m as f32).collect();
            for ci in 0..c {
                let dst = unsafe { std::slice::from_raw_parts_mut(optr.0.add(base + ci * p), len) };
                let wc = wd[ci];
                for (((y, &v), mu), r) in dst.iter_mut().zip(row(ci)).zip(&meanf).zip(&rstd) {
                    let z = (v - mu) * r * wc;
                    *y = if relu && z < 0.0 { 0.0 } else { z };
                }
            }
        })?;
        Ok(())
    }
}
