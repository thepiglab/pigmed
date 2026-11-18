# Testing

| Suite | Command | Needs | What it covers |
|---|---|---|---|
| Python core | `uv sync --extra test && uv run pytest -q` | nothing online | licence filters, JATS parsing, chunking, PDF render, vectors, outbox/locks, citation materializer, publish/verify/delete, re-embed, exact search, metrics |
| Retriever training | `cd train && pytest -q tests` | `torch`, `transformers`, `pillow` (CPU) | MaxSim, masks, GradCache vs direct gradients, LoRA merge/export, multimodal forward, distributed helpers (tiny random models) |
| `pigmed-ingest` | `cargo test` | Rust ≥ 1.89 | PMC/PubMed planning, JATS → units, chunking, bundles, supplements, guideline framework |
| `pigmed-ingest` adapters | `cargo test --features guideline-fixtures` | captured publisher pages ([how](rust/pigmed-ingest/tests/fixtures/README.md)) | one offline test set per guideline adapter |
| Other crates | `cargo test` in `rust/pigmed-{ctx,export,splits,synth,llmbatch}` | Rust ≥ 1.89 | see each crate's README |
| `pigmed-embed` | `cargo test` | Linux, Rust ≥ 1.92, ONNX Runtime | thread pinning, prep, flash kernels |

GPU-only checks (`train/tests/check_gpu.py`, `scripts/gpu_embed_bench.py`) and parity against the Python model-card
runtime (`rust/pigmed-embed/py/`) need the real model weights and a GPU or a large CPU node.

## What the offline tests do not establish

- live PMC/PubMed acquisition at full scale, retries over days, or inventory drift;
- model quality or throughput (see [docs/RUST_EMBED.md](docs/RUST_EMBED.md) and [docs/TEST_LADDER.md](docs/TEST_LADDER.md) for measured runs);
- scheduler behaviour, shared-filesystem `flock` semantics, or Hub upload/readback under failure;
- copyright clearance of any content.

The tests use synthetic JATS/PubMed XML, generated PDFs and figures, a fake bulk HTTP source, a local publisher
sink and labelled mock vectors.
