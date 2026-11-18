//! Port of Pillow's `ImagingResample` (libImaging/Resample.c) for 8-bit RGB: separable convolution,
//! horizontal pass first, per-output-pixel normalized coefficients, 22-bit fixed point, clip to 0..255.
//! torchvision's uint8 antialias resize (what transformers' TorchvisionBackend runs) was ported from the same
//! code, so this reproduces the HF Gemma 4 / NeoMME image processors to within rare 1/255 rounding.

#[derive(Clone, Copy)]
pub enum Filter {
    Bilinear,
    Bicubic,
}

impl Filter {
    fn support(self) -> f64 {
        match self {
            Filter::Bilinear => 1.0,
            Filter::Bicubic => 2.0,
        }
    }
    fn eval(self, x: f64) -> f64 {
        let x = x.abs();
        match self {
            Filter::Bilinear => {
                if x < 1.0 { 1.0 - x } else { 0.0 }
            }
            Filter::Bicubic => {
                const A: f64 = -0.5;
                if x < 1.0 {
                    ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
                } else if x < 2.0 {
                    (((x - 5.0) * x + 8.0) * x - 4.0) * A
                } else {
                    0.0
                }
            }
        }
    }
}

/// Pillow: fixed 22-bit coefficients. ATen/torchvision uint8 AVX kernel (what HF actually runs): int16 coefficients
/// with the largest precision that keeps max_weight < 2^15, chosen per pass.
const PIL_PRECISION_BITS: u32 = 32 - 8 - 2;
pub static TORCH_INT16_WEIGHTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// precompute_coeffs + coefficient quantization. Returns (ksize, bounds [(xmin, n)], fixed-point coeffs, precision).
fn coeffs(in_size: usize, out_size: usize, f: Filter) -> (usize, Vec<(usize, usize)>, Vec<i32>, u32) {
    let scale = in_size as f64 / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = f.support() * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let mut bounds = Vec::with_capacity(out_size);
    let mut kd = vec![0f64; out_size * ksize];
    let mut k = vec![0f64; ksize];
    for xx in 0..out_size {
        let center = (xx as f64 + 0.5) * scale;
        let ss = 1.0 / filterscale;
        let xmin = ((center - support + 0.5) as i64).max(0) as usize; // C (int) truncates toward zero
        let xmax = ((center + support + 0.5) as i64).min(in_size as i64) as usize - xmin;
        let mut ww = 0.0;
        for x in 0..xmax {
            let w = f.eval((x as f64 + xmin as f64 - center + 0.5) * ss);
            k[x] = w;
            ww += w;
        }
        for x in 0..ksize {
            kd[xx * ksize + x] = if x < xmax { if ww != 0.0 { k[x] / ww } else { k[x] } } else { 0.0 };
        }
        bounds.push((xmin, xmax));
    }
    let prec = if TORCH_INT16_WEIGHTS.load(std::sync::atomic::Ordering::Relaxed) {
        let maxw = kd.iter().cloned().fold(0.0f64, f64::max);
        let mut p = 0u32;
        while p < 22 {  // ATen: for (p = 0; p < 22; ++p) ...
            let next = (0.5 + maxw * (1u64 << (p + 1)) as f64) as i64;
            if next >= (1 << 15) {
                break;
            }
            p += 1;
        }
        p
    } else {
        PIL_PRECISION_BITS
    };
    let kk = kd
        .iter()
        .map(|&v| {
            let fp = v * (1u64 << prec) as f64;
            if v < 0.0 { (-0.5 + fp) as i32 } else { (0.5 + fp) as i32 }
        })
        .collect();
    (ksize, bounds, kk, prec)
}

#[inline]
fn clip8(v: i32, prec: u32) -> u8 {
    (v >> prec).clamp(0, 255) as u8
}

/// Resize an interleaved RGB8 buffer (w x h) to (ow x oh).
pub fn resize_rgb8(src: &[u8], w: usize, h: usize, ow: usize, oh: usize, f: Filter) -> Vec<u8> {
    if ow == w && oh == h {
        return src.to_vec();
    }
    let need_h = ow != w;
    let need_v = oh != h;
    let (ksv, mut bv, kv, pv) = coeffs(h, oh, f);
    let ybox_first = bv[0].0;
    let ybox_last = bv[oh - 1].0 + bv[oh - 1].1;
    let (mut cur, mut cw, mut ch) = (src.to_vec(), w, h);
    let mut row_off = 0usize;
    if need_h {
        for b in bv.iter_mut() {
            b.0 -= ybox_first;
        }
        let (ksh, bh, kh, ph) = coeffs(w, ow, f);
        let rows = ybox_last - ybox_first;
        let mut tmp = vec![0u8; ow * rows * 3];
        let half = 1i32 << (ph - 1);
        for yy in 0..rows {
            let srow = &src[(yy + ybox_first) * w * 3..(yy + ybox_first + 1) * w * 3];
            let drow = &mut tmp[yy * ow * 3..(yy + 1) * ow * 3];
            for xx in 0..ow {
                let (xmin, n) = bh[xx];
                let k = &kh[xx * ksh..xx * ksh + n];
                let (mut s0, mut s1, mut s2) = (half, half, half);
                for (x, &kx) in k.iter().enumerate() {
                    let p = (xmin + x) * 3;
                    s0 += srow[p] as i32 * kx;
                    s1 += srow[p + 1] as i32 * kx;
                    s2 += srow[p + 2] as i32 * kx;
                }
                drow[xx * 3] = clip8(s0, ph);
                drow[xx * 3 + 1] = clip8(s1, ph);
                drow[xx * 3 + 2] = clip8(s2, ph);
            }
        }
        cur = tmp;
        cw = ow;
        ch = rows;
    } else {
        row_off = 0;
    }
    let _ = ch;
    if need_v {
        let mut out = vec![0u8; ow * oh * 3];
        let half = 1i32 << (pv - 1);
        for yy in 0..oh {
            let (ymin, n) = bv[yy];
            let ymin = ymin + row_off;
            let k = &kv[yy * ksv..yy * ksv + n];
            let drow = &mut out[yy * ow * 3..(yy + 1) * ow * 3];
            for xx in 0..ow * 3 {
                let mut s = half;
                for (y, &ky) in k.iter().enumerate() {
                    s += cur[(ymin + y) * cw * 3 + xx] as i32 * ky;
                }
                drow[xx] = clip8(s, pv);
            }
        }
        out
    } else {
        cur
    }
}
