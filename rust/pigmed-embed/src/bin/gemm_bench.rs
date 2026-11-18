//! Backend evidence: FP32 GEMM throughput of candle's CPU backend (pure-Rust `gemm` crate) at the layer
//! shapes of the two models. Compare with py/gemm_bench.py (torch/MKL and ONNX Runtime/MLAS) on the same node.
//! RAYON_NUM_THREADS sets candle's thread count. Build: cargo build --release --features candle --bin gemm-bench
use candle_core::{Device, Tensor};
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let threads = candle_core::utils::get_num_threads();
    // (label, M, K, N): M = tokens per item
    let shapes = [
        ("gemma-text mlp up   512x512x2048", 512, 512, 2048),
        ("gemma-text mlp down 512x2048x512", 512, 2048, 512),
        ("gemma-vit  mlp up   2520x768x3072", 2520, 768, 3072),
        ("neomme     mlp up   2562x1024x3584", 2562, 1024, 3584),
        ("neomme     mlp down 2562x3584x1024", 2562, 3584, 1024),
    ];
    let dev = Device::Cpu;
    for (label, m, k, n) in shapes {
        let a = Tensor::randn(0f32, 1.0, (m, k), &dev)?;
        let b = Tensor::randn(0f32, 1.0, (k, n), &dev)?;
        for _ in 0..3 {
            let _ = a.matmul(&b)?;
        }
        let reps = 20;
        let t = Instant::now();
        for _ in 0..reps {
            let c = a.matmul(&b)?;
            std::hint::black_box(&c);
        }
        let s = t.elapsed().as_secs_f64() / reps as f64;
        let gflops = 2.0 * (m * k * n) as f64 / s / 1e9;
        println!("{{\"engine\":\"candle-{}\",\"threads\":{threads},\"shape\":\"{label}\",\"ms\":{:.3},\"gflops\":{:.1}}}",
                 candle_core::utils::with_avx() as u8, s * 1e3, gflops);
    }
    Ok(())
}
