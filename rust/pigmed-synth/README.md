# pigmed-synth

Synthetic retrieval queries for PMC passages and figures: persona library, query recipes, LLM generation through
any OpenAI-compatible server, an EmbeddingGemma 2 round-trip filter, and reports.

```
sample ─▶ generate (persona + intent card ─▶ surface query, per recipe) ─▶ queries ─▶ embed (external)
                                                                                         │
                     export (HF rows, per-row licence) ◀── report ◀── filter (gold passage must rank) ◀┘
```

| Command | What it does |
|---|---|
| `sample` / `sample-hf` | Pick articles and chunks from splits and bundles or from the published dataset |
| `generate` | Call the LLM server for one arm (recipe v2/v3, figures, plain-language summaries) |
| `queries` | Collect generated queries into embedder input + metadata |
| `filter` | Keep queries whose gold passage ranks high under EG2 |
| `report`, `export` | A/B reports and dataset rows |

`generators.json` maps arm-name prefixes to the exact generator model, revision, quantization and engine, so every
row records which model wrote it. Recipe design: [docs/RETRIEVER_QUERY_RECIPE_V3.md](../../docs/RETRIEVER_QUERY_RECIPE_V3.md);
dataset card draft: [datasets/pigmed-personas](../../datasets/pigmed-personas).

```bash
cargo build --release && cargo test     # 26 tests
```
