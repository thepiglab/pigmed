# pigmed-ingest (`pigmed-rs`)

Bulk acquisition for PigMed: plans and ingests **PMC Open Access** (from the public `pmc-oa-opendata` S3 bucket) and
**PubMed** baseline/update files into canonical bundles, and scrapes **clinical guidelines** through per-source
adapters.

```
plan-pmc / plan-pubmed ──▶ plan parts (JSONL) ──▶ ingest ──▶ <root>/runs/<run>/ready/<bundle>/ ──▶ verify
                                                      │        manifest.json · items.jsonl.gz · assets.tar
                                                      └─ JATS → sections, chunks, tables, captions, formulas,
                                                         supplements (PDF/Word/slides/sheets), licence per row
```

| Command | What it does |
|---|---|
| `plan-pmc` | Read the PMC S3 inventory, keep the latest version per PMCID, stable hash sampling, write plan parts |
| `plan-pubmed [--updates]` | Plan annual baseline or daily update files |
| `ingest --manifest PART --root DIR [--config C]` | Download, parse and write bundles (resumable; per-article audit rows on failure) |
| `verify BUNDLE` | Check manifest hashes and row/asset consistency |
| `rechunk` | Re-pack text units (recipe v2) and write late-chunking documents |
| `list-sizes`, `media-census`, `probe-media`, `probe-pdf`, `probe-supplements`, `supp-audit`, `pdf-bench`, `convert-file` | Audits and probes |
| `guidelines list / plan / probe / ingest` | Clinical-guideline adapters driven by [`configs/guideline_sources.json`](../../configs/guideline_sources.json) |

## Build and test

```bash
cargo build --release          # Rust >= 1.89; PDF rendering loads libpdfium at runtime
cargo test                     # 63 offline tests
cargo test --features guideline-fixtures   # + 209 adapter test modules; needs the captures (tests/fixtures/README.md)
```

Guideline scraping rules (robots, pacing, rights tiers, `blocked:`/`needs_auth` states):
[docs/GUIDELINES.md](../../docs/GUIDELINES.md). Test ladder and measured runs: [docs/TEST_LADDER.md](../../docs/TEST_LADDER.md).
