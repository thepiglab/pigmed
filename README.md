# PigMed

Open, reproducible biomedical search: a full-text **PubMed Central** corpus, parsed and embedded with
**EmbeddingGemma 2**, plus the tools to build a multimodal retriever on top of it.

| | |
|---|---|
| 🔎 **Try it** | [PigMed Search](https://mkieffer-pigmed-search.hf.space/) (live demo) |
| 📦 **Corpus + vectors** | [`mkieffer/pigmed-pmc-eg2`](https://huggingface.co/datasets/mkieffer/pigmed-pmc-eg2): 9.3M PMC OA articles, chunks, 768-d vectors, media index |
| 🗂 **Search index** | [`mkieffer/pigmed-pmc-eg2-index`](https://huggingface.co/datasets/mkieffer/pigmed-pmc-eg2-index) |
| 💬 **Persona-written queries** | coming soon (draft card: [`datasets/pigmed-personas`](datasets/pigmed-personas)) |
| 🔗 **Related repos** | [`flashembed`](https://github.com/thepiglab/flashembed): fast EmbeddingGemma 2 runtime (CPU/GPU) used to embed the corpus · [`flashcluster`](https://github.com/thepiglab/flashcluster): GPU e-means clustering for the topic atlas |

## How it fits together

```
        PMC Open Access (AWS S3)           PubMed baseline + updates          Clinical-guideline publishers
                  │                                  │                                  │
                  ▼                                  ▼                                  ▼
   ┌───────────────────────────────────────────────────────────┐        ┌──────────────────────────────┐
   │ pigmed-rs  plan-pmc / plan-pubmed / ingest / verify       │        │ pigmed-rs guidelines         │
   │ JATS → sections, chunks, tables, captions, supplements    │        │ 117 source adapters, polite  │
   │ licence + provenance on every row                         │        │ cached fetcher, rights tiers │
   └──────────────────────────┬────────────────────────────────┘        └──────────────┬───────────────┘
                              │ canonical bundles (items.jsonl.gz + assets.tar + manifest)
                              ▼                                                        │
   ┌───────────────────────────────────────────────────────────┐                       │
   │ embed: flashembed (CPU/GPU) · pigmed-embed (CPU, ORT)     │◀──────────────────────┘
   │ EmbeddingGemma 2 · naive + late-chunking vectors          │
   └──────────────────────────┬────────────────────────────────┘
                              ▼
   ┌───────────────────────────────────────────────────────────┐        ┌──────────────────────────────┐
   │ pigmed-export → Parquet (chunks / vectors / media)        │───────▶│ HF dataset · index · Space   │
   └──────────────────────────┬────────────────────────────────┘        │ topic atlas: flashcluster    │
                              │                                         └──────────────────────────────┘
                              │
                              ▼  retriever training data
   pigmed-splits ──▶ pigmed-querygen / -synth (+ llmbatch) ──▶ pigmed-ctx ──▶ train/eg2late (PyTorch)
   leakage-safe     persona-written queries,                 citation/figure   late-interaction head
   paper splits     EG2 round-trip filter                    contexts          on EmbeddingGemma 2
```

## What's where

| Path | What | Lang |
|---|---|---|
| [`rust/pigmed-ingest`](rust/pigmed-ingest) | `pigmed-rs`: PMC/PubMed planning and ingest, bundle writer, guideline adapters | Rust |
| [`rust/pigmed-embed`](rust/pigmed-embed) | CPU embedding runner (EmbeddingGemma 2, NeoMME) on ONNX Runtime | Rust (+ Python parity checks) |
| [`rust/pigmed-export`](rust/pigmed-export) | Embedded bundles → Parquet shards for the Hub | Rust |
| [`rust/pigmed-splits`](rust/pigmed-splits) | Paper census, leakage-safe splits, split audit | Rust |
| [`rust/pigmed-synth`](rust/pigmed-synth) | Synthetic-query generation, filtering and reports | Rust |
| [`rust/pigmed-querygen`](rust/pigmed-querygen) + [`querygen/`](querygen) | Config-driven query generator (personas, intent cards, figure arm) | Rust + TOML |
| [`rust/pigmed-llmbatch`](rust/pigmed-llmbatch) | Resumable batch client for OpenAI-compatible servers (vLLM/SGLang) | Rust |
| [`rust/pigmed-ctx`](rust/pigmed-ctx) | Contextual (S2) and multimodal (S3) training data | Rust |
| [`train/`](train) | `eg2late`: retriever training and evaluation | Python |
| [`src/pigmed/`](src/pigmed) | Original Python pipeline: publish, verify, re-embed, exact search, metrics | Python |
| [`scripts/`](scripts) | Evaluation, benchmarking, Hub helpers | Python / shell |
| [`examples/slurm/`](examples/slurm) | Generic batch templates (no site settings) | Slurm |
| [`configs/`](configs) | Ingest/embedding configs, guideline source registry, `site.env.example` | JSON / shell |
| [`datasets/`](datasets) | Dataset cards ([`pmc-eg2`](datasets/pmc-eg2), [`pigmed-personas`](datasets/pigmed-personas)) | Markdown |
| [`docs/`](docs) | Design notes, benchmarks, research | Markdown |

## Quickstart

<details open><summary><b>Use the published corpus</b> (no build needed)</summary>

```python
from datasets import load_dataset
chunks  = load_dataset("mkieffer/pigmed-pmc-eg2", "chunks",  split="train", streaming=True)
vectors = load_dataset("mkieffer/pigmed-pmc-eg2", "vectors", split="train", streaming=True)
# join on unit_id; filter on commercial_use_ok / license before reuse
```
</details>

<details><summary><b>Build and test the Rust tools</b></summary>

Requires Rust ≥ 1.89 (`pigmed-embed`: ≥ 1.92, Linux only).

```bash
cd rust/pigmed-ingest && cargo test          # 63 offline tests
cargo build --release                        # → target/release/pigmed-rs
./target/release/pigmed-rs plan-pmc --help
```

The guideline-adapter tests need captured publisher pages that are not redistributed; see
[`rust/pigmed-ingest/tests/fixtures/README.md`](rust/pigmed-ingest/tests/fixtures/README.md).
</details>

<details><summary><b>Ingest a small PMC sample</b></summary>

```bash
pigmed-rs plan-pmc --out work/plans/pilot --sample-mod 1000 --sample-rem 0 --per-part 100
pigmed-rs ingest --manifest work/plans/pilot/part-00000.jsonl --root work/pilot --config configs/ingest-text-first.json
pigmed-rs verify work/pilot/runs/<run>/ready/<bundle>
```

On a cluster: copy [`configs/site.env.example`](configs/site.env.example) outside the repo, fill in paths and
`SBATCH_ACCOUNT`/`SBATCH_QOS`, `export PIGMED_ENV=<that file>`, then use
[`scripts/submit_rs_ingest.sh`](scripts/submit_rs_ingest.sh) and the templates in [`examples/slurm/`](examples/slurm).
</details>

<details><summary><b>Python tools and retriever training</b></summary>

```bash
uv sync --extra test && uv run pytest -q          # core pipeline tests
cd train && pytest -q tests                       # needs torch, transformers, pillow (CPU is fine)
```
See [`train/README.md`](train/README.md) and [`docs/RETRIEVER_TRAINING_PLAN.md`](docs/RETRIEVER_TRAINING_PLAN.md).
</details>

## Docs

| Topic | Doc |
|---|---|
| Pipeline design, bundle format | [DESIGN](docs/DESIGN.md) · [PYTHON_PIPELINE](docs/PYTHON_PIPELINE.md) · [SOURCES](docs/SOURCES.md) · [TESTING](TESTING.md) |
| Ingest test ladder and scale results | [TEST_LADDER](docs/TEST_LADDER.md) |
| CPU embedding runner: parity and speed | [RUST_EMBED](docs/RUST_EMBED.md) |
| Clinical guidelines: scraping, rights, embedding | [GUIDELINES](docs/GUIDELINES.md) · [GUIDELINES_EMBEDDING](docs/GUIDELINES_EMBEDDING.md) |
| Retriever: plan, data, query recipe | [TRAINING_PLAN](docs/RETRIEVER_TRAINING_PLAN.md) · [DATA_PLAN](docs/RETRIEVER_DATA_PLAN.md) · [QUERY_RECIPE_V3](docs/RETRIEVER_QUERY_RECIPE_V3.md) · [QUERYGEN_SPEC](docs/QUERYGEN_SPEC.md) · [TRAINING](docs/TRAINING.md) |
| Research surveys | [SEARCH_TECHNIQUES_2026](docs/SEARCH_TECHNIQUES_2026.md) · [INTENT_VS_QUERY](docs/INTENT_VS_QUERY_RESEARCH.md) |

## Licences and rights

- **Code:** MIT ([LICENSE](LICENSE)); two ported files keep their upstream terms ([THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES.md)).
- **Data is not covered by the code licence.** PMC articles keep their own licences; every row carries `license`
  and `commercial_use_ok`. Filter before reuse.
- **Clinical guidelines** belong to their publishers. This repo ships the adapters, not the content, and only
  redistributable guideline text may enter a public index ([why](docs/GUIDELINES_EMBEDDING.md)).
- **Models:** EmbeddingGemma 2 is Apache 2.0 and its use is subject to the Gemma Prohibited Use Policy.
