# pigmed-export

Turns embedded PigMed bundles into the Parquet shards published as
[`mkieffer/pigmed-pmc-eg2`](https://huggingface.co/datasets/mkieffer/pigmed-pmc-eg2).

```
bundle (items.jsonl.gz + manifest.json) ─┐
vectors/<model>/<bundle>/dense.npy ──────┼─▶ batch ─▶ chunks/<name>.parquet   text units
list-sizes TSVs (media bytes) ───────────┘            vectors/<name>.parquet  unit_id, modality, 768 x f16
                                                      media/<name>.parquet    S3 URL, md5, bytes, role
                                                      <name>.json             receipt: rows, sizes, sha256
```

Each vector shard must come from exactly its bundle (`meta.manifest_sha256 == sha256(manifest.json)`) and every
vector id must be a row of the bundle, or the batch fails. Other commands: `titles`, `relicense`.

```bash
cargo build --release && cargo test
```
