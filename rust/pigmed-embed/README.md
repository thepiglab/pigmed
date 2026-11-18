# pigmed-embed

CPU embedding runner for **EmbeddingGemma 2** (text, image, audio, video) and **NeoMME-260M-Retriever** on
ONNX Runtime, with dense and late (per-token) outputs. Built to match the model-card Python runtime exactly
(min cosine ≥ 0.9999999) while running faster on CPU nodes.

| Piece | Path |
|---|---|
| Runner (`bench`, `embed`, `serve`) | `src/main.rs`, `src/lib.rs`, `src/prep.rs`, `src/media.rs` |
| Fused kernels and checks | `src/flash.rs`, `src/bin/flash_check.rs`, `src/bin/*bench.rs` |
| Pure-Rust NeoMME (candle, optional) | `--features candle`, `src/bin/candle_neomme.rs` |
| ONNX export, Python baseline, parity and reports | `py/` |
| Batch templates | [`examples/slurm/embed-rs-*.sbatch`](../../examples/slurm) |

**Platform:** Linux only (uses `sched_getaffinity`), Rust ≥ 1.92, ONNX Runtime shared library loaded at runtime
(`ORT_DYLIB_PATH`). Audio/video need FFmpeg 9 (`examples/slurm/embed-rs-ffmpeg.sbatch` builds the matching release).

```bash
cargo build --release
cargo test
```

Design, parity and speed tables: [docs/RUST_EMBED.md](../../docs/RUST_EMBED.md).
