//! Correctness + speed check of the fused attention kernel (src/flash.rs) against a naive f64 reference,
//! on random data at the head layouts of the PigMed models. `flash-check [threads]`
use pigmed_embed::flash::{Dims, run_task};
use std::time::Instant;

fn rnd(n: usize, seed: &mut u64) -> Vec<f32> {
    (0..n)
        .map(|_| {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            ((*seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32
        })
        .collect()
}

fn naive(dm: &Dims, q: &[f32], k: &[f32], v: &[f32], m: &[i64], scale: f32, w: i64) -> Vec<f32> {
    let Dims { b, hq, hk, s, d, dv } = *dm;
    let mut out = vec![0f32; b * s * hq * dv];
    for bi in 0..b {
        for h in 0..hq {
            let kh = h / (hq / hk);
            for i in 0..s {
                let mut sc = vec![f64::NEG_INFINITY; s];
                for (j, x) in sc.iter_mut().enumerate() {
                    let ok = m[bi * s + j] != 0 && (w < 0 || (i as i64 - j as i64).abs() <= w);
                    if ok {
                        let mut a = 0f64;
                        for e in 0..d {
                            a += q[((bi * hq + h) * s + i) * d + e] as f64 * k[((bi * hk + kh) * s + j) * d + e] as f64;
                        }
                        *x = a * scale as f64;
                    }
                }
                let mx = sc.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                if mx == f64::NEG_INFINITY {
                    continue;
                }
                let p: Vec<f64> = sc.iter().map(|x| (x - mx).exp()).collect();
                let z: f64 = p.iter().sum();
                for e in 0..dv {
                    let mut a = 0f64;
                    for j in 0..s {
                        a += p[j] * v[((bi * hk + kh) * s + j) * dv + e] as f64;
                    }
                    out[((bi * s + i) * hq + h) * dv + e] = (a / z) as f32;
                }
            }
        }
    }
    out
}

fn main() {
    let threads: usize = std::env::args().nth(1).and_then(|x| x.parse().ok()).unwrap_or(16);
    // (label, b, hq, hk, s, d, window, padded tail of item 1)
    let cases = [
        ("gemma text sliding  b1 S1400 4/2 d256 w512", 1, 4, 2, 1400, 256, 512i64, 0usize),
        ("gemma text global   b1 S1400 4/1 d512 full", 1, 4, 1, 1400, 512, -1, 0),
        ("neomme sliding      b2 S700 16/4 d64 w256 pad", 2, 16, 4, 700, 64, 256, 300),
        ("neomme global       b2 S700 16/4 d64 full pad", 2, 16, 4, 700, 64, -1, 300),
        ("gemma vision        b1 S2520 12/12 d64 full pad", 1, 12, 12, 2520, 64, -1, 63),
    ];
    let mut seed = 0x9E3779B97F4A7C15u64;
    for (label, b, hq, hk, s, d, w, pad) in cases {
        let dm = Dims { b, hq, hk, s, d, dv: d };
        let q = rnd(b * hq * s * d, &mut seed);
        let k = rnd(b * hk * s * d, &mut seed);
        let v = rnd(b * hk * s * d, &mut seed);
        let mut m = vec![1i64; b * s];
        if b > 1 || pad > 0 {
            let bi = b - 1;
            for j in s - pad..s {
                m[bi * s + j] = 0;
            }
        }
        let scale = (d as f32).powf(-0.5) * 3.0;
        let mut out = vec![0f32; b * s * hq * d];
        let optr = out.as_mut_ptr() as usize;
        let tasks = dm.tasks();
        let reps = 6;
        let mut best = f64::MAX;
        for rep in 0..reps {
        if rep == 1 {
            for p in &pigmed_embed::flash::PROF { p.store(0, std::sync::atomic::Ordering::Relaxed); }
        }
        let t = Instant::now();
        let next = std::sync::atomic::AtomicUsize::new(0); // dynamic scheduling, like ORT's pool
        std::thread::scope(|sc| {
            for _ in 0..threads {
                let (q, k, v, m, next) = (&q, &k, &v, &m, &next);
                sc.spawn(move || loop {
                    let task = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if task >= tasks {
                        break;
                    }
                    unsafe { run_task(&dm, q, k, v, m, scale, w, optr as *mut f32, task) };
                });
            }
        });
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        let ms = best;
        let pr: Vec<f64> = pigmed_embed::flash::PROF.iter().map(|p| p.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6 / (reps - 1) as f64 / threads as f64).collect();
        let r = naive(&dm, &q, &k, &v, &m, scale, w);
        let (mut maxd, mut num, mut na, mut nb) = (0f64, 0f64, 0f64, 0f64);
        for (x, y) in out.iter().zip(&r) {
            maxd = maxd.max((*x as f64 - *y as f64).abs());
            num += *x as f64 * *y as f64;
            na += (*x as f64).powi(2);
            nb += (*y as f64).powi(2);
        }
        println!("{label}: best {ms:7.2} ms  max|diff| {maxd:.2e}  cos {:.9}  [per-thread ms: qk {:.1} softmax {:.1} pv {:.1}]",
                 num / (na.sqrt() * nb.sqrt()), pr[0], pr[1], pr[2]);
    }
}
