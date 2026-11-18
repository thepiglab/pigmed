//! FP32 Linear-layer GEMM throughput of the `gemm` crate at the models' layer shapes (Y[M,N] = X[M,K] W[N,K]^T,
//! W in nn.Linear layout), in two threading modes: gemm's own Rayon parallelism, and output-column blocks split
//! over plain threads with one single-threaded gemm call each (how an ORT custom op on the intra-op pool would run).
//! Compare with py/gemm_bench.py (torch/MKL, ONNX Runtime/MLAS) on the same node.
//!   linear-bench <threads> [MxKxN,...]
use gemm::{Parallelism, gemm};
use std::time::Instant;

fn run(m: usize, k: usize, n: usize, x: &[f32], w: &[f32], y: &mut [f32], par: Parallelism, cols: std::ops::Range<usize>) {
    let nb = cols.len();
    unsafe {
        gemm(
            m, nb, k,
            y.as_mut_ptr().add(cols.start), 1, n as isize, // dst [M,N] col stride 1, row stride n
            false,
            x.as_ptr(), 1, k as isize,                      // lhs [M,K] row-major
            w.as_ptr().add(cols.start * k), k as isize, 1,  // rhs = W^T [K,N]: element (kk, j) at j*k + kk
            0.0, 1.0, false, false, false, par,
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let t: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(16);
    let shapes: Vec<(usize, usize, usize)> = args
        .get(2)
        .map(|s| s.split(',').map(|x| { let v: Vec<usize> = x.split('x').map(|d| d.parse().unwrap()).collect(); (v[0], v[1], v[2]) }).collect())
        .unwrap_or_else(|| vec![(674, 1024, 3584), (674, 3584, 1024), (674, 1024, 1024), (2562, 1024, 3584)]);
    rayon::ThreadPoolBuilder::new().num_threads(t).build_global().ok();
    for (m, k, n) in shapes {
        let x: Vec<f32> = (0..m * k).map(|i| ((i * 7919) % 1000) as f32 * 1e-3 - 0.5).collect();
        let w: Vec<f32> = (0..n * k).map(|i| ((i * 104729) % 1000) as f32 * 1e-3 - 0.5).collect();
        let mut y = vec![0f32; m * n];
        let flop = 2.0 * (m * k * n) as f64;
        // mode 1: gemm's rayon parallelism
        let reps = 20;
        for _ in 0..3 { run(m, k, n, &x, &w, &mut y, Parallelism::Rayon(t), 0..n); }
        let s = Instant::now();
        for _ in 0..reps { run(m, k, n, &x, &w, &mut y, Parallelism::Rayon(t), 0..n); }
        let g1 = flop * reps as f64 / s.elapsed().as_secs_f64() / 1e9;
        // mode 2: column blocks over scoped threads, single-threaded gemm per block
        let blocks = |y: &mut Vec<f32>| {
            let nb = n.div_ceil(t * 2).next_multiple_of(16).min(n);
            let yp = y.as_mut_ptr() as usize;
            let ranges: Vec<_> = (0..n).step_by(nb).map(|c| c..(c + nb).min(n)).collect();
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|sc| {
                for _ in 0..t {
                    let (ranges, next, x, w) = (&ranges, &next, &x, &w);
                    sc.spawn(move || loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= ranges.len() { break; }
                        let ys = unsafe { std::slice::from_raw_parts_mut(yp as *mut f32, m * n) };
                        run(m, k, n, x, w, ys, Parallelism::None, ranges[i].clone());
                    });
                }
            });
        };
        for _ in 0..3 { blocks(&mut y); }
        let s = Instant::now();
        for _ in 0..reps { blocks(&mut y); }
        let g2 = flop * reps as f64 / s.elapsed().as_secs_f64() / 1e9;
        println!("{{\"engine\":\"gemm-rayon\",\"threads\":{t},\"shape\":\"{m}x{k}x{n}\",\"gflops\":{g1:.1}}}");
        println!("{{\"engine\":\"gemm-colblocks\",\"threads\":{t},\"shape\":\"{m}x{k}x{n}\",\"gflops\":{g2:.1}}}");
    }
}
