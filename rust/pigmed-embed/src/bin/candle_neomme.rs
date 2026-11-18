//! Pure-Rust probe: NeoMME-260M-Retriever text documents on candle (CPU, FP32), hand-ported from
//! transformers 5.22.2 `modeling_neomme.py`. Same inputs / outputs / JSON as `pigmed-embed bench`
//! (tag `rsc-neomme-text-t<T>-b<B>`, runtime `rust-candle`), so compare.py can check parity and speed.
//!
//!   RAYON_NUM_THREADS is set from --threads before candle starts its pool.
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, anyhow};
use candle_core::{D, DType, Device, Module, Tensor};
use candle_nn::{Embedding, Linear, VarBuilder};
use clap::Parser;
use pigmed_embed::prep;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    threads: usize,
    #[arg(long)]
    batch: usize,
    #[arg(long, default_value_t = 32)]
    n: usize,
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    #[arg(long, default_value_t = 512)]
    max_len: usize,
    #[arg(long)]
    inputs: PathBuf,
    #[arg(long)]
    snapshot: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    save: bool,
}

const H: usize = 1024;
const NH: usize = 16;
const NKV: usize = 4;
const HD: usize = 64;
const EPS: f64 = 1e-6;
const LAYERS: usize = 17;

struct Layer {
    l0: f64,
    l1: f64,
    q: Linear,
    k: Linear,
    v: Linear,
    gate: Linear,
    o: Linear,
    up: Linear,
    down: Linear,
    xsa_scale: Tensor, // tanh(alpha), (1,1,16,1)
    full: bool,
    window: Option<usize>,
    value: bool,
}

struct NeoMME {
    word: Embedding,
    proj: Linear,
    value_emb: Embedding,
    layers: Vec<Layer>,
    late: Linear,
    ones_h: Tensor,
    ones_d: Tensor,
    rm: f64,
}

fn lin(vb: &VarBuilder, name: &str, i: usize, o: usize) -> Result<Linear> {
    Ok(candle_nn::linear_no_bias(i, o, vb.pp(name))?)
}

impl NeoMME {
    fn load(snapshot: &std::path::Path) -> Result<Self> {
        let cfg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(snapshot.join("config.json"))?)?;
        let dev = Device::Cpu;
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[snapshot.join("model.safetensors")], DType::F32, &dev)? };
        let m = vb.pp("model");
        let types: Vec<String> = serde_json::from_value(cfg["layer_types"].clone())?;
        let default_win = cfg["sliding_window"].as_u64().map(|x| x as usize);
        let plc = cfg["per_layer_config"].as_object().cloned().unwrap_or_default();
        let full_layers: Vec<usize> = (0..LAYERS).filter(|&i| types[i] == "full_attention").collect();
        let value_layers = [full_layers[0], *full_layers.last().unwrap()];
        let mut layers = Vec::with_capacity(LAYERS);
        for i in 0..LAYERS {
            let l = m.pp(format!("layers.{i}"));
            let lam = l.get(2, "lambdas")?.to_vec1::<f32>()?;
            let a = l.pp("self_attn");
            let alpha = a.pp("exclusive_self_attention").get(NH, "alpha")?;
            let full = types[i] == "full_attention";
            let window = if full {
                None
            } else {
                match plc.get(&format!("{i:02}")).and_then(|v| v.get("sliding_window")) {
                    Some(serde_json::Value::Number(n)) => n.as_u64().map(|x| x as usize),
                    _ => default_win,
                }
            };
            layers.push(Layer {
                l0: lam[0] as f64,
                l1: lam[1] as f64,
                q: lin(&a, "q_proj", H, NH * HD)?,
                k: lin(&a, "k_proj", H, NKV * HD)?,
                v: lin(&a, "v_proj", H, NKV * HD)?,
                gate: lin(&a.pp("output_projection"), "gate_proj", H, NH * HD)?,
                o: lin(&a.pp("output_projection"), "o_proj", NH * HD, H)?,
                up: lin(&l.pp("mlp"), "up_proj", H, 3584)?,
                down: lin(&l.pp("mlp"), "down_proj", 3584, H)?,
                xsa_scale: alpha.tanh()?.reshape((1, 1, NH, 1))?,
                full,
                window,
                value: value_layers.contains(&i),
            });
        }
        let e = m.pp("embed_tokens");
        Ok(Self {
            word: candle_nn::embedding(131072, 256, e.pp("word_embeddings"))?,
            proj: lin(&e, "embedding_projection", 256, H)?,
            value_emb: candle_nn::embedding(131072, NKV * HD, m.pp("value_embeddings"))?,
            layers,
            late: lin(&vb.pp("multi_vector_head"), "proj", H, 128)?,
            ones_h: Tensor::ones(H, DType::F32, &dev)?,
            ones_d: Tensor::ones(HD, DType::F32, &dev)?,
            rm: cfg["residual_multiplier"].as_f64().ok_or_else(|| anyhow!("residual_multiplier"))?,
        })
    }

    fn rope_tables(s: usize, dim: usize, theta: f64) -> Result<(Tensor, Tensor)> {
        let inv: Vec<f32> = (0..dim / 2).map(|i| 1.0 / (theta.powf((2 * i) as f64 / dim as f64)) as f32).collect();
        let pos: Vec<f32> = (0..s).map(|p| p as f32).collect();
        let f = Tensor::from_vec(pos, (s, 1), &Device::Cpu)?.matmul(&Tensor::from_vec(inv, (1, dim / 2), &Device::Cpu)?)?;
        Ok((f.cos()?, f.sin()?))
    }

    /// ids (b, s) u32, mask (b, s) f32 in {0,1}. Returns (dense (b,1024), late (b,s,128)).
    fn forward(&self, ids: &Tensor, mask: &Tensor) -> Result<(Tensor, Tensor)> {
        let (b, s) = ids.dims2()?;
        let x = self.proj.forward(&self.word.forward(ids)?)?;
        let mut h = candle_nn::ops::rms_norm(&x, &self.ones_h, EPS as f32)?;
        let init = h.clone();
        let ve = self.value_emb.forward(ids)?.reshape((b, s, NKV, HD))?.transpose(1, 2)?.contiguous()?;
        let (cs, ss) = Self::rope_tables(s, HD, 10_000.0)?; // sliding: full head rotated
        let (cf, sf) = Self::rope_tables(s, HD / 4, 1_000_000.0)?; // full: 25% of head
        // additive masks: padding (b,1,1,s) + optional band |i-j| <= w
        let pad = ((mask.reshape((b, 1, 1, s))? - 1.0)? * 1e9)?;
        let band = |w: usize| -> Result<Tensor> {
            let v: Vec<f32> = (0..s * s).map(|k| if (k / s).abs_diff(k % s) <= w { 0.0 } else { -1e9 }).collect();
            Ok(Tensor::from_vec(v, (1, 1, s, s), &Device::Cpu)?)
        };
        let mut masks = std::collections::HashMap::new();
        for l in &self.layers {
            let key = l.window.filter(|&w| w < s);
            if let std::collections::hash_map::Entry::Vacant(e) = masks.entry(key) {
                e.insert(match key {
                    Some(w) => pad.broadcast_add(&band(w)?)?,
                    None => pad.clone(),
                });
            }
        }
        for l in &self.layers {
            let mixed = ((&h * l.l0)? + (&init * l.l1)?)?;
            let xn = candle_nn::ops::rms_norm(&mixed, &self.ones_h, EPS as f32)?;
            let q = l.q.forward(&xn)?.reshape((b, s, NH, HD))?;
            let k = l.k.forward(&xn)?.reshape((b, s, NKV, HD))?;
            let v = l.v.forward(&xn)?.reshape((b, s, NKV, HD))?;
            let q = candle_nn::ops::rms_norm(&q, &self.ones_d, EPS as f32)?.transpose(1, 2)?.contiguous()?;
            let k = candle_nn::ops::rms_norm(&k, &self.ones_d, EPS as f32)?.transpose(1, 2)?.contiguous()?;
            let (q, k) = if l.full {
                let r = HD / 4;
                let rot = |t: &Tensor| -> Result<Tensor> {
                    let a = candle_nn::rotary_emb::rope(&t.narrow(3, 0, r)?.contiguous()?, &cf, &sf)?;
                    Ok(Tensor::cat(&[&a, &t.narrow(3, r, HD - r)?], 3)?)
                };
                (rot(&q)?, rot(&k)?)
            } else {
                (candle_nn::rotary_emb::rope(&q, &cs, &ss)?, candle_nn::rotary_emb::rope(&k, &cs, &ss)?)
            };
            let mut v = v.transpose(1, 2)?.contiguous()?;
            if l.value {
                v = (v + &ve)?;
            }
            let rep = |t: &Tensor| -> Result<Tensor> {
                Ok(t.unsqueeze(2)?.expand((b, NKV, NH / NKV, s, HD))?.reshape((b, NH, s, HD))?)
            };
            let (kr, vr) = (rep(&k)?, rep(&v)?);
            let att = (q.matmul(&kr.t()?)? * (1.0 / (HD as f64).sqrt()))?;
            let att = att.broadcast_add(&masks[&l.window.filter(|&w| w < s)])?;
            let att = candle_nn::ops::softmax_last_dim(&att)?;
            let o = att.matmul(&vr)?.transpose(1, 2)?; // (b, s, NH, HD)
            // exclusive self-attention: remove the component along the (unit) value of the same token
            let vu = vr.transpose(1, 2)?;
            let vu = vu.broadcast_div(&vu.sqr()?.sum_keepdim(3)?.sqrt()?.clamp(1e-12, f64::MAX)?)?;
            let p = (&o * &vu)?.sum_keepdim(3)?;
            let o = (&o - vu.broadcast_mul(&p.broadcast_mul(&l.xsa_scale)?)?)?.reshape((b, s, NH * HD))?;
            let g = candle_nn::ops::sigmoid(&l.gate.forward(&xn)?)?;
            let a_out = l.o.forward(&(o * g)?)?;
            h = (h + (a_out * self.rm)?)?;
            let x2 = candle_nn::ops::rms_norm(&h, &self.ones_h, EPS as f32)?;
            let u = l.up.forward(&x2)?.relu()?.sqr()?;
            h = (h + (l.down.forward(&u)? * self.rm)?)?;
        }
        let h = candle_nn::ops::rms_norm(&h, &self.ones_h, EPS as f32)?;
        let m3 = mask.unsqueeze(2)?;
        let pooled = h.broadcast_mul(&m3)?.sum(1)?.broadcast_div(&m3.sum(1)?.clamp(1e-9, f64::MAX)?)?;
        let dense = pooled.broadcast_div(&pooled.sqr()?.sum_keepdim(1)?.sqrt()?.clamp(1e-12, f64::MAX)?)?;
        let late = self.late.forward(&h)?;
        let late = late.broadcast_div(&late.sqr()?.sum_keepdim(D::Minus1)?.sqrt()?.clamp(1e-12, f64::MAX)?)?;
        let late = late.broadcast_mul(&m3)?;
        Ok((dense, late))
    }
}

fn main() -> Result<()> {
    let a = Args::parse();
    // SAFETY: single-threaded at this point; candle/rayon read it on first use.
    unsafe { std::env::set_var("RAYON_NUM_THREADS", a.threads.to_string()) };
    let t_load = Instant::now();
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(a.inputs.join("texts.jsonl"))?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(a.n)
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let ids: Vec<String> = rows.iter().map(|r| r["id"].as_str().unwrap_or_default().to_string()).collect();
    let texts: Vec<String> = rows.iter().map(|r| format!("<doc>{}", r["text"].as_str().unwrap_or_default())).collect();
    let tok = prep::load_tokenizer(&a.snapshot.join("tokenizer.json"), Some(a.max_len))?;
    let pad_id = prep::token_id(&tok, "<pad>")?;
    let model = NeoMME::load(&a.snapshot)?;
    let load_s = t_load.elapsed().as_secs_f64();
    let run = |batch: &[String]| -> Result<(Vec<Vec<f32>>, Vec<Vec<f32>>, Vec<usize>)> {
        let rows = prep::encode(&tok, batch, false)?;
        let (idv, maskv, s) = prep::pad(&rows, pad_id);
        let b = rows.len();
        let idt = Tensor::from_vec(idv.iter().map(|&x| x as u32).collect::<Vec<_>>(), (b, s), &Device::Cpu)?;
        let mt = Tensor::from_vec(maskv.iter().map(|&x| x as f32).collect::<Vec<_>>(), (b, s), &Device::Cpu)?;
        let (dense, late) = model.forward(&idt, &mt)?;
        let dense = dense.to_vec2::<f32>()?;
        let late = late.to_vec3::<f32>()?;
        let mut lv = Vec::with_capacity(b);
        let mut nr = Vec::with_capacity(b);
        for (bi, l) in late.into_iter().enumerate() {
            let n = rows[bi].len();
            lv.push(l.into_iter().take(n).flatten().collect());
            nr.push(n);
        }
        Ok((dense, lv, nr))
    };
    let batches: Vec<&[String]> = texts.chunks(a.batch).collect();
    for b in batches.iter().take(a.warmup) {
        run(b)?;
    }
    let (mut dense, mut late, mut nrows, mut lat) = (vec![], vec![], vec![], vec![]);
    let t0 = Instant::now();
    for b in &batches {
        let s = Instant::now();
        let (d, l, n) = run(b)?;
        lat.push(s.elapsed().as_secs_f64());
        dense.extend(d);
        late.extend(l);
        nrows.extend(n);
    }
    let wall = t0.elapsed().as_secs_f64();
    let tag = format!("rsc-neomme-text-t{}-b{}", a.threads, a.batch);
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let res = serde_json::json!({
        "tag": tag, "runtime": "rust-candle", "model": "neomme", "modality": "text", "threads": a.threads,
        "batch": a.batch, "n": texts.len(), "warmup_batches": a.warmup, "wall_s": wall,
        "items_per_s": texts.len() as f64 / wall, "batch_latency_s": lat, "load_s": load_s,
        "peak_rss_mb": ru.ru_maxrss as f64 / 1024.0, "tokens_total": nrows.iter().sum::<usize>(), "dtype": "float32",
        "candle": "0.11", "cpu": std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())),
        "host": std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim(),
        "job": std::env::var("SLURM_JOB_ID").ok(),
    });
    std::fs::create_dir_all(&a.out)?;
    if a.save {
        let flat: Vec<f32> = dense.iter().flatten().copied().collect();
        ndarray_npy::write_npy(a.out.join(format!("{tag}.dense.npy")), &ndarray::Array2::from_shape_vec((dense.len(), 1024), flat)?)?;
        let lf: Vec<f32> = late.iter().flatten().copied().collect();
        let rows: usize = nrows.iter().sum();
        ndarray_npy::write_npy(a.out.join(format!("{tag}.late.npy")), &ndarray::Array2::from_shape_vec((rows, 128), lf)?)?;
        let mut offs = vec![0i64];
        for n in &nrows {
            offs.push(offs.last().unwrap() + *n as i64);
        }
        ndarray_npy::write_npy(a.out.join(format!("{tag}.late_offsets.npy")), &ndarray::Array1::from_vec(offs))?;
        std::fs::write(a.out.join(format!("{tag}.ids.json")), serde_json::to_string(&ids)?)?;
    }
    std::fs::write(a.out.join(format!("{tag}.json")), serde_json::to_string_pretty(&res)?)?;
    let mut brief = res.clone();
    brief.as_object_mut().unwrap().remove("batch_latency_s");
    println!("{brief}");
    Ok(())
}
