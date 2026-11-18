# pmc-eg2

Card for [`mkieffer/pigmed-pmc-eg2`](https://huggingface.co/datasets/mkieffer/pigmed-pmc-eg2): PubMed Central Open
Access full text (9.3M articles, every licence, per-row `license` / `commercial_use_ok`), chunked with recipe `v2`
and embedded with EmbeddingGemma 2. [`hf_card.md`](hf_card.md) is a copy of the published card.

| Config | Rows are | Built by |
|---|---|---|
| `chunks` | text units (body chunks, tables, captions, supplement text) | `pigmed-rs ingest` + `rechunk` |
| `vectors` | 768-d float16 unit-norm vectors, text and image | embed runner |
| `media` | every file an article ships (S3 URL + md5, no bytes) | `pigmed-rs ingest`, `list-sizes` |
| `pending_*` | videos/audio and PDFs not embedded yet | `pigmed-export` |

All configs are written by [`rust/pigmed-export`](../../rust/pigmed-export). Join on `unit_id`; group by `article_id`.
