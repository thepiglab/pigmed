# pigmed-ingest test fixtures

The guideline adapters (`src/guidelines/sources/*.rs`) are tested offline against **unmodified captures of
publisher pages, PDFs and API responses** (1,082 files, ~78 MB). That content belongs to the guideline publishers
and is **not redistributed** in this repository. Only its manifest is: [`MANIFEST.tsv`](MANIFEST.tsv) lists
`sha256`, size and path for every file.

| Test set | Needs captures? | How to run |
|---|---|---|
| Core ingest, chunking, bundle, PMC/PubMed, registry and framework tests | no | `cargo test` |
| Per-adapter guideline tests (209 test modules) | yes | `cargo test --features guideline-fixtures` |

The per-adapter test modules are compiled only with the `guideline-fixtures` feature, because they embed the
captures with `include_str!` / `include_bytes!`. Without the feature, `cargo test` builds and runs everything else.

## Getting the captures

If you have a copy (for example, inside your own organisation), restore and verify it with:

```bash
scripts/fetch_guideline_fixtures.sh <dir | archive.tar.gz | hf://datasets/OWNER/REPO>
cargo test --features guideline-fixtures
```

The script copies files into `tests/fixtures/guidelines/` and checks every file against `MANIFEST.tsv`.
Files that are missing or differ are reported. The adapter tests assert on exact text, so they need the
original bytes; re-capturing a live page usually gives different content.

To make your own captures, run an adapter against the live source with a cache directory
(`pigmed-rs guidelines probe --source <id> --cache <dir>` or `... ingest`; see `docs/GUIDELINES.md`). The cached
responses are the same kind of file as the fixtures, but the assertions in the tests will need updating.
