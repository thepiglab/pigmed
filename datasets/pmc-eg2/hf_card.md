---
license: other
license_name: per-row-article-licence
license_link: https://www.ncbi.nlm.nih.gov/pmc/tools/openftlist/
pretty_name: PigMed PMC Open Access, EmbeddingGemma 2
language:
- en
tags:
- biomedical
- pubmed-central
- embeddings
- retrieval
configs:
- config_name: chunks
  data_files: "chunks/pmcv2-*.parquet"
- config_name: vectors
  data_files: "vectors/pmcv2-*.parquet"
- config_name: media
  data_files: "media/pmcv2-*.parquet"
- config_name: pending_videos
  data_files: "pending/videos/pmcv2-*.parquet"
- config_name: pending_supplement_pdfs
  data_files: "pending/supplement_pdfs/pmcv2-*.parquet"
---

# PigMed: PubMed Central Open Access, EmbeddingGemma 2 vectors

Full-text articles from the PubMed Central (PMC) Open Access subset (latest version of each PMCID, every licence),
parsed from JATS XML plus supplementary files, chunked and embedded with EmbeddingGemma 2 (768-d, float16), for open
search. Every row comes from one pipeline run with one recipe (`recipe` = `v2`).

**This is a rolling release.** Shards are added as the run progresses through the corpus (PMCID order). Nothing is
filtered out by licence, so check the licence of each row before reuse.

## Licences and how to filter

Every row of `chunks`, `vectors` and `media` carries its article's licence:

- `license`: normalised code, e.g. `CC-BY`, `CC0`, `CC-BY-NC-SA`;
- `license_code`: the licence code as PMC publishes it, e.g. `CC BY-NC`;
- `commercial_use_ok`: `true` only for CC0, CC BY, CC BY-SA and CC BY-ND (the PMC commercial-use subset).

| `commercial_use_ok` | licences | notes |
|---|---|---|
| true | CC0, CC BY, CC BY-SA, CC BY-ND | attribution; share-alike for BY-SA; no derivatives for BY-ND |
| false | CC BY-NC, CC BY-NC-SA, CC BY-NC-ND | non-commercial use only |
| false | other / no Creative Commons licence | in the PMC Open Access subset under the publisher's terms; check before any reuse |

This card does not grant rights. Reuse of each article's text and media follows that article's licence.

Only the commercial-use subset:

```python
from datasets import load_dataset
chunks = load_dataset("mkieffer/pigmed-pmc-eg2", "chunks", split="train", streaming=True)
commercial = chunks.filter(lambda r: r["commercial_use_ok"])
```

Or filter while reading the Parquet files directly (here with DuckDB):

```sql
SELECT unit_id, article_id, text FROM 'hf://datasets/mkieffer/pigmed-pmc-eg2/chunks/*.parquet'
WHERE commercial_use_ok;
```

`vectors` rows carry the same three columns, so a vector index can be built for either subset without a join.
Every row also has `recipe` (`v2`).

## Configs
- `chunks`: one row per text unit:
  - ids: `unit_id`, `article_id` (PMCID.version), `pmcid`, `pmid`, `doi`, `title`;
  - `kind`: `text` body chunk, `table_text`, `caption`, `supplement_text` or `table_card`;
  - text and position: `text`, `section` (heading path), `section_ids`, `char_start`/`char_end`, `is_abstract`, `token_count` (words);
  - licence: `license`, `license_code`, `commercial_use_ok`;
  - provenance: `chunk_recipe` (JSON), `source_url`/`supplement_file` (supplement text), `figure_id` (captions).
- `vectors`: `unit_id`, `article_id`, `modality` (text, image), `embedding` (768 x float16, unit norm), `license`,
  `license_code`, `commercial_use_ok`, `recipe`. Body chunks were embedded as `title: none | text: <top heading>\n<chunk>`
  and mixed with a late-chunking vector (see below). Other text units used `title: none | text: <text>`, and figures
  came from the PMC S3 originals. Inference was bf16 on GPU.
- `media`: one row per file an article ships (article XML/PDF/text, figures, supplementary files, videos):
  - `s3_url` (public bucket `pmc-oa-opendata`), `md5`, `bytes`;
  - `kind`, `role`, `modality`, `extension`, `file_name`, `caption`, `figure_id`;
  - clip `start_seconds`/`end_seconds`;
  - `license`, `license_code`, `commercial_use_ok`.

  Media bytes are not stored here. Fetch them from the S3 URL and check the md5.
- `pending_videos`: video and audio files that are not embedded yet (same columns as `media`).
- `pending_supplement_pdfs`: PDFs that were not downloaded in this release, both article PDFs (`role` =
  `article_pdf`) and supplementary PDFs (`role` = `media_file`); same columns as `media`. A later pass will add them.

Join `vectors` to `chunks` or `media` on `unit_id`, and use `article_id` for article-level grouping.

## Recipe `v2`: chunking and vectors
- **Chunking.** Body text is packed per top-level section. Consecutive sections under the same top heading form one
  stream, with sub-headings inline. The packing targets 256 words (min 64, max 384) with an overlap of 32, snaps splits
  to sentence ends and folds tiny tails into the previous chunk. `chunk_recipe` holds the parameters. For body chunks,
  `char_start`/`char_end` index the packed section stream. Tables, captions and supplement text keep the per-unit
  split (512-word budget, 64 overlap).
- **Heading prefix.** The model input of each body chunk starts with its top-level section heading.
- **Late chunking.** Each article's body chunks (with their heading paths) are concatenated into one document,
  `title: <title> | text: …`, of up to 8,192 tokens. That document is embedded once, and each chunk's token span is
  pooled into a late vector. The published body-chunk vector is `normalize(naive + 0.5 * late)`. Chunks past the
  8,192-token window, and all non-body units, keep the plain (naive) vector.
- **Why.** On held-out PMC retrieval tests this improved article-level MRR@10 by about 0.03 over the earlier recipe
  (384-word chunks, no heading, no late mix).

## Shards and provenance
- `pmcv2-s<stream>-<seq>`: the v2 run over all of PMC Open Access. Videos/audio and all PDFs are held out in this
  release and listed under the `pending_*` configs.
- Coverage: the latest version of every PMCID in the PMC Open Access inventory of 2026-11-16 (9,345,085 articles,
  all licences); 44 articles could not be ingested (43 processing errors, 1 source file missing on S3). Articles
  without extractable text have media rows only. A few supplementary files that are corrupt (e.g. damaged legacy `.xls`
  workbooks) are recorded as failed supplements and contribute no text.
- `receipts/<shard>.json` lists each shard's source bundles, row counts and the sha256 of every file. Every shard was
  read back from the Hub and its sha256 checked after upload.

Source: the PMC Open Access subset in the public AWS Open Data bucket `pmc-oa-opendata` (inventory of November
2026). Citation and attribution for each article: see its `pmcid`/`doi`.
