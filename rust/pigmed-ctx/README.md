# pigmed-ctx

Training data for the contextual (S2) and multimodal (S3) retriever stages, built from the published chunk/media
shards and the PMC JATS.

| Command | Output |
|---|---|
| `shard` | Per shard: ordered body chunks per article (S2), citation and figure contexts, optional 8k-token contextual windows |
| `cite-resolve` | Citation pairs: citing sentence → cited article, resolved against the corpus |
| `fetch-figs` | Figure images from the public PMC S3 bucket, resized |
| `abssent-ctx` | Abstract-sentence → body pairs with context |
| `xml-ctx` | Debug: print the parsed context of one JATS file |

Only kept articles are used (train split, commercial-use licence, not retracted, not held out).

```bash
cargo build --release && cargo test     # 10 tests
```

Formats are what [`train/eg2late`](../../train) reads; plan: [docs/RETRIEVER_TRAINING_PLAN.md](../../docs/RETRIEVER_TRAINING_PLAN.md).
