# pigmed-splits

Leakage-safe train/dev/test splits for retriever training on PMC, plus an audit that fails loudly.

```
census (JATS metadata) ─▶ build (union-find over shared authors-of-record, DOIs, near-duplicates via MinHash,
                                 citation links, blocklists) ─▶ paper-group splits ─▶ audit (exit 1 on any leak)
```

| Command | What it does |
|---|---|
| `census` / `census-shard` / `census-merge` | Paper metadata census from bundles and JATS (sharded for the full corpus) |
| `minhash-links` | Near-duplicate links across the corpus |
| `blocklist` | Benchmark-overlap blocklist |
| `build` | Group connected papers and assign whole groups to splits |
| `audit` | Re-check every invariant against a manifest (and a previous manifest, for stability) |

```bash
cargo build --release && cargo test     # 19 tests
```

Design: [docs/RETRIEVER_DATA_PLAN.md](../../docs/RETRIEVER_DATA_PLAN.md) ("PMC splits").
