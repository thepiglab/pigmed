# Test ladder (site-neutral copy; the run log with job IDs is kept locally)

Each rung is sized so a failure is cheap and so it answers a question the next rung depends on.
Run on low-priority CPU capacity of a Slurm cluster. Site accounts, QOS, storage paths and job IDs live
in a local site env file and run log, not in this repository. Keep the pilot outbox well under the group quota.

| # | Question | Status (2026-11-11) |
|---|---|---|
| 1 | Do compute nodes reach S3; does the v0.1 Python pipeline run at all? | OK: 10 articles. First PMCIDs are 1867 OCR scans with no `<p>`, so 0 text units (not representative). |
| 2 | Where does Python time go on a real all-years sample? | Text-only: 94% waiting on serial HTTPS reads. Pages + figures: 59% PNG encode, 27% network, 7% pdfium. 25/50 articles are CC-BY/CC0. |
| 3 | Does the Rust port produce compatible bundles, faster? | Yes. See README table. One pdfium segfault (concurrent documents) fixed with a process-wide render gate. |
| 4 | Throughput, licence mix, error rate, media prevalence, bytes per article at ~10k articles | done (see below) |
| 5 | All-years citation catalog: parse rate, bytes per file, delete/update handling | next |
| 6 | Python vs Rust CPU runtime: parity per modality, per context length; items/s; RSS; per-op profile (are custom kernels worth it?) | in progress |
| 7 | End-to-end units/s and vector bytes per article | after 6 |
| 8 | `anydoc` / `pdf-inspector` on PMC supplementary files: worth converting and embedding? | supplements: done (see below); PDF-vs-JATS comparison still open |
| 9 | Does verify-then-delete hold against the real Hub? | needs explicit approval (external upload) |
| 10 | Release gates 4–6 in `docs/DESIGN.md` | later |

## Rung 4 results (2026-11-11, inventory 2026-11-11T01-00Z, pigmed-rs, all publication years, stable hash samples)

| | 1/1,000 text-only (`pmc-s1000`) | 1/10,000 pages + figures (`pmc-s10000`) |
|---|---|---|
| Articles planned | 9,321 (19 parts × 500) | 906 (10 parts × 100) |
| Ingested (CC-BY / CC0) | 5,432 (58%) | 533 (59%) |
| Skipped by licence (NC, ND, TDM-only, none) | 3,889 | 373 |
| Content errors | 0 | 0 |
| Units per ingested article | 21.4 text, 1.6 table, 4.1 caption | + 4.1 figures, 11.8 pages |
| Bundle bytes per ingested article | 188 KB | 9.4 MB (page PNGs dominate) |
| Task wall time (8 CPUs, concurrency 8) | ~13 s per 500 articles, CPU ~23% (network-bound) | ~48 s per 100 articles, peak RSS 1.7 GB |
| Articles with video (filename) | 1.7% (95% CI 1.4–2.1%) | 1.8% |
| Articles with audio (filename) | 0% (CI ≤0.08%) | 0% |
| Articles with "other" supplements (docx/xlsx/pdf/zip…) | 39% | 39% |

Extrapolation to the full inventory (~9.5M latest versions, ~5.5M CC-BY/CC0), arithmetic only:
- Text-only bundles: about 1 TB. At ~40 articles/s per 8-CPU task × 8 tasks, roughly 8–10 h of burst time.
  That is network-bound, so raise per-task concurrency before adding tasks.
- Archiving rendered pages + figures: about 50 TB. Not feasible on typical 2 TB group storage. For the full corpus,
  page/figure views must be transient (`archive_media=false`, embed then drop). Archive views only for
  evaluation sets.
- 39% of articles carry supplementary documents. That is where `anydoc` (docx/xlsx/pptx→Markdown) would add text.

Operational finding: PMC rewrites metadata objects in place after the daily inventory. 3 articles in this
sample (modified 05:36Z and 21:39Z, after the 01:00Z inventory) failed their MD5 check. pigmed-rs now takes
the current object (PMCID and version still checked) and marks every row `source_drift: true` with
`planned_etag`. The three failed partitions were rerun and resumed from their checkpoints.

## Audit-fix verification (2026-11-12)

- Tests: Rust 13/13, Python 41 passed (1 skipped: needs torch).
- All-licence ingest of the 50-article sample: 50/50 articles ingested; 25 redistributable. Licence present on every
  content row. 375 `media` rows. Modalities: text 50, figure 43, pdf 43, table 32, document 5, archive 3,
  spreadsheet 2, video 1.
- A plan row for an article removed after planning (HTTP 404) became one `source-missing` audit row; the partition completed.
- PubMed IDs vs each record's own `PubmedData/ArticleIdList`. Baseline file 0001 (30,000 old records): Python 0
  wrong, Rust 0 wrong. Update file 1335 (14,278 recent records): **Python 2,420 wrong (17%)**, Rust 0. Rust parses a
  file in 8–14 s.
- Transparent PNG → white background, 16-bit image → correct grey, publisher skips `.partial` bundles: unit tests.

## Supplementary material probe (2026-11-12, `pigmed-rs probe-supplements`, 9,321-article all-licence sample)

Inventory (from per-file `media` rows; no extra downloads):
- 32% of articles (2,980) ship non-image/video supplements: 6,691 files, 0.72 per article.
  - By format: PDF 2,484, docx 1,752, xlsx 1,165, zip 497, doc 214, xls 120, csv 68, pptx 61, xml 50, txt 42,
    html 39, cif 36; long tail of domain data (hkl, bed, wig, pdb, fasta, sav, …).
- Video: 522 files, 56 per 1,000 articles. Images: 43,848 (mostly the article's own figures).

Conversion of a deterministic stratified sample (638 files, ≤40 per extension; anydoc for office/CSV/RTF/EPUB,
pdf-inspector for PDF):

| Format | Converted | Median words | Median size | Median convert time |
|---|---|---|---|---|
| pdf | 37/40 (36 text-based, 1 mixed, 3 empty = scans) | 574 | 0.34 MB | 25 ms |
| docx / doc | 40/40, 40/40 | 604 / 515 | 0.03 / 0.08 MB | 4 / 1 ms |
| xlsx / xls | 39/40, 37/40 | 4,119 / 2,320 | 0.05 / 0.04 MB | 6 / 1 ms |
| csv / txt | 40/40, 40/40 | 5,187 / 6,888 | 0.03 / 0.08 MB | 2 / 0.1 ms |
| pptx / ppt | 40/40, 18/19 | 65 / 41 | 0.4 / 0.5 MB | 2 / 1 ms |
| zip, rar, xml, cif, hkl, bed, wig, … | no converter (archives / domain data) | – | zip 1.7 MB | – |

Failures worth noting: xlsx with >2M XML nodes (anydoc resource limit), old .xls not recognised (3/40), and
html supplements. For html, 37/39 failed because the `?md5=` that PMC publishes in `media_urls` does not match the object.

Text volume (main text = 45.4M words, ~4,870 words/article):
- Prose supplements (pdf/docx/doc/rtf/odt/tex/ppt/pptx): +0.13× main text, about 1.7 extra 512-token chunks per
  article. Cheap to convert and mostly methods, protocols and supplementary tables/figure legends.
- Tables (xlsx/xls/csv/tsv/txt): +2.5× main text uncapped, driven by data dumps (p90 47k words, p99 700k, max 9.1M
  words: proteomics/GWAS lists). Capped at 2,000 words per file: +0.05×.

Recommendation:
1. Convert and embed prose supplements as `supplement_text` units (same chunking as body text, linked to the file's
   `media` row).
2. For tables, embed a capped "table card" (file/sheet names, header, first rows, ≤2k words) and archive the full
   Markdown for exact lookup.
3. Unpack zip/rar (7.4% of supplement files) in a later pass and route the inner files the same way.
4. Catalogue domain data (cif/hkl/pdb/bed/wig/fasta/…) only.
5. Leave scanned PDFs (≈7% of PDFs) for an OCR pass.
6. Supplementary videos go to the EmbeddingGemma video path once the runner benchmarks land.

## Supplement ingestion (2026-11-12, `pigmed-rs ingest` with `"supplements": "prose+tables"`)

Rust-only config keys (absent = off, so existing run IDs are unchanged): `supplements` (`none` | `prose` |
`prose+tables`), `table_card_words` (2000), `supplement_max_words` (50,000), `supplement_max_bytes` (256 MiB),
`supplement_archive_bytes` (4 MiB), `supplement_concurrency` (6), `supplement_ocr_pages` (0; OCR image-only PDF
pages up to this many per file with tesseract, which must be on PATH). Example: `configs/ingest-text-supp.json` (the Python ingest rejects these keys).

- Prose files (pdf, doc/docx, odt, rtf, ppt/pptx, html, tex, md, prose .txt) → `supplement_text` chunks, same
  splitter as the body. Tables (xls/xlsx/ods, csv/tsv, delimited .txt) → one `table_card` each: file name, JATS
  `<supplementary-material>` label/caption, line/word counts, then rows up to the word cap.
- Full Markdown is archived as `{article}/supplements/{key}.md` (≤4 MiB) and referenced by `markdown_member`.
- Each supplement's `media` row gets `supplement` (status, tool, words, bytes, timings) and `embedded_as_text`.
  Statuses: ok, no-text (scanned PDF), convert-failed, too-large, download-failed, skipped-class.

Same 9,321-article all-licence sample, text-first config, 16 CPUs:

| | Without supplements | With supplements |
|---|---|---|
| Wall time | 146 s | 980 s |
| Bundle bytes | 1.6 GB | 1.9 GB |
| Embeddable units | 242,627 | 265,174 (+9.3%: 21,227 supplement chunks, 1,320 table cards) |
| Words embedded | 45.4M | 56.8M (prose +21%, table cards +4.5%) |

- Files: 5,636 of 5,977 converted (94%); 234 scanned PDFs (no text), 57 conversion failures, 13 over 64 MiB,
  37 download failures (all 39 HTML supplements fail the published MD5).
- 2,518 of 9,321 articles (27%) gained supplement units. 62% of table cards hit the 2,000-word cap.
- Median per file: 101 ms download, 6 ms conversion. Peak RSS was 6.6–7.4 GB on one partition, from big spreadsheets.

Parallel supplements: each article now fetches and converts up to `supplement_concurrency` (default 6) files at once.
Output was identical: same units and file statuses.

| Article / HTTP concurrency | Sequential per article | Parallel per article |
|---|---|---|
| 16 / 32 | 980 s | 707 s |
| 48 / 64 | – | 519 s (3.5× the no-supplement time) |

- Summed over all files: 1,498 s of download and 755 s of conversion.
- Conversion is concentrated in a few large PDFs and spreadsheets: one PDF took 71 s, and the four slowest PDFs
  took 136 s together. Because commits are written in manifest order, one such article stalls the window. The
  remaining gap is that tail, not the network.
- Use 48/64 for supplement runs.

## Hugging Face streaming and bundle size (2026-11-12, private test dataset)

Sweep (random bytes so Xet cannot deduplicate; one commit per bundle of 3 files; full readback verify):

| Bundle | Upload | Readback verify | End to end |
|---|---|---|---|
| 1 MiB (8 commits) | median 1.7 s per commit; 2 of 8 took 31 s | – | – |
| 8 MiB | 2.3 s / 31.9 s | 6–7 MB/s | 0.3–2.4 MB/s |
| 32 MiB | 2.3 s / 32.5 s | 21 MB/s | 1–9 MB/s |
| 128 MiB | 33 s, 33 s | 36–47 MB/s | 3.7 MB/s |
| 512 MiB | 74–125 MB/s | 76 MB/s | 38–47 MB/s |
| 2 GiB | 368 MB/s | 73 MB/s | 61 MB/s |

- Every commit pays about 1.5 s, and an intermittent ~30 s stall (cause unknown, server side) hit 7 of the
  13 commits under 512 MiB and none of the 3 at 512 MiB or more. Small bundles are therefore dominated by
  per-commit cost.
- Above 512 MiB the full readback verify (~75 MB/s, one stream per file) is the bottleneck.
- Recommendation: target 1 GiB bundles (`target_shard_bytes` 1073741824), with readback verify kept. That is
  about 1,000 commits and 4,000 files per TB, well within the Hub's 100k files per repo and 10k per folder.

Progressive ingest→publish (`examples/slurm/stream-publish.sbatch`, 1,000 articles with supplements, 64 MiB bundles,
1 GiB outbox): 4 bundles (236 MB) uploaded, verified and deleted locally while the producer ran, 41 s median
from bundle ready to verified commit, nothing left on local disk, no backpressure needed. Upload was only
3.4 MB/s at this bundle size (the stalls above).

Streaming read-back with `datasets` (`load_dataset("json", data_files="hf://datasets/<repo>/data/**/items.jsonl.gz",
streaming=True)`): first row after 2.1 s, then 6,900 rows/s, all row kinds including supplement units.

Storage: free accounts get 100 GB of private storage. The full PMC corpus is about 1 TB text-only (more with
supplements), so a full private release needs a paid plan or a rights-cleared public release.

## Throughput, fidelity and OCR (2026-11-12/13, same 9,321-article sample, one node, 64 CPUs of an AMD EPYC 7702)

Pipelining: every article is its own task; finished articles wait in a reorder buffer and are committed in
manifest order, so bundles and checkpoints are byte-for-byte the sequential order. Text + supplements on one
process went from 519 s (48/64) to 403 s. Supplements, figures, pages and video/audio now run concurrently
within an article; CPU-heavy conversions share a per-process gate (`PIGMED_CPU_SLOTS`).

Processes × concurrency (several `pigmed-rs ingest` processes pulling manifest partitions):

| Config | Partitions | Wall | Articles/s | CPU busy |
|---|---|---|---|---|
| Text + supplements, 1 × 192 | 19 × ~500 | 279 s | 33 | 8% |
| Text + supplements, 19 × 32 | 19 × ~500 | 46–71 s | 132–202 | 35–49% |
| Text + supplements, 32 × 16 | 187 × 50 | 56 s | 168 | 45% |
| Full media, 19 × 32 | 19 × ~500 | 380 s | 24.5 | 51% |
| Full media, 32 × 16 | 187 × 50 | 237–246 s | 38–39 | 84% |
| Full media, 64 × 8 | 187 × 50 | 243 s | 38 | 85% |

- One process plateaus at ~33 articles/s; several processes scale until the node is saturated. Use small
  partitions (~50 articles) so idle processes pick up work instead of waiting on the slowest large partition.
- Text runs are network-bound and vary 2× between runs (46–105 s); full-media runs are CPU-bound.
- Full media for the sample: 85 GB of bundles, 92,978 page images, 32,636 figures, 616 audio clips.
- Outbox fix: each bundle reserves `target_shard_bytes + max_article_bytes` (~2.2 GiB) while open; that
  reservation is now settled to the committed size (also on resume). Before, ~90 committed bundles exhausted a
  200 GiB outbox and the ingest stopped with backpressure.

CPU profile of full media (`perf`, 1,900 articles): zlib deflate inside PNG encoding ~40%, pdfium rendering
~27%, PNG filtering/pixel conversion ~7%, pdf-inspector table detection ~2%, ffmpeg <1%. A/B of PNG effort
(lossless either way; `PIGMED_PNG_COMPRESSION`): `default` 237 s / 85 GB, `fast` 204 s / 139 GB. Default kept:
the +63% bytes cost more in storage and upload than the 14% wall time saves.

PDF routing: pdf-inspector is superlinear in drawing objects (one 1.38M-object PDF took 71–112 s). PDFs with
≥100k page objects (counted with pdfium) use pdfium's text layer instead (0.6–5 s); pdfium is also the fallback
when pdf-inspector finds no text. On a 40-file sample the median is 23 ms (pdf-inspector) vs 9 ms (pdfium).

Fidelity audit (`pigmed-rs supp-audit`): re-converts every previously failed supplement plus 40 successful files
per extension, and scores token recall against an independent reference (pdfium text layer, DOCX `w:t` runs,
spreadsheet cells via calamine).

- Of 427 failed files: 184 recovered by content sniffing (ZIP/OLE files with the wrong extension, HTML saved
  as .doc, large spreadsheets streamed with calamine, MD5-mismatch files accepted and flagged) and
  pdfium fallback; with OCR on up to 30 pages per file, 322 recovered (220 of 234 scanned PDFs).
- Remaining: 14 pure-image PDFs with no legible text, 5 corrupt or unsupported files.
- Recall on successful files: non-PDF ≈ 1.0; PDF 0.95–1.0 except form-field PDFs (0.81).

OCR engine (`scripts/ocr_bench.py`): 153 born-digital PDF pages (their text layer is the reference; rendered to
images and OCR'd as scans) plus 53 real image-only pages. Legible pages; word F1, median CER, CPU per page:

| Engine | CPU s/page | Word F1 | CER | Tables F1 |
|---|---|---|---|---|
| tesseract 5.3, 300 dpi | 1.8 | 0.901 | 0.108 | 0.857 |
| tesseract 5.3, 200 dpi | 1.7 | 0.933 | 0.057 | 0.951 |
| tesseract, 200 dpi + rotation (both reads, keep confident) | 1.3–2.0 | 0.935 | 0.057 | 0.951 |
| OnnxTR fast (docTR, Apache-2.0) | 3.6 | 0.921 | 0.152 | 0.924 |
| OnnxTR accurate | 38 | 0.919 | 0.161 | 0.932 |
| ocrs 0.14 | 46 | 0.833 | 0.212 | 0.788 |
| granite-docling-258M (CPU) | >1,000 | did not finish 50 pages in 6 h | | |

- Chosen: tesseract at 300 dpi with rotation handling. 200 dpi wins on the synthetic pages, but OCR only runs
  on real scans, where 300 dpi read 27% to 3× more confident text. Supplement audit: 4.8 s convert time per
  OCR page, 322 of 427 recovered.
- Rotation: tesseract OSD (`--psm 0`) proposes a rotation; it is wrong on some number-heavy tables and charts
  (180° at confidence 2–6 on upright pages) and right on real sideways pages (270° at 6, 90° at 12). So a
  proposal only adds a second, rotated read, and the read with more characters in words of confidence ≥60 wins.
  The sideways table went from 16% to 80% recall with no upright page lost.
- Two pages of figures with thousands of vector-text gene labels are below any engine's resolution and are
  reported separately as illegible.

## ~100k-article end-to-end run (2026-11-13/14)

Two disjoint 1/190 hash samples of the PMC inventory: 97,947 articles, all licences (licence on every row),
490 partitions of 200 articles.

**Ingest** (`pigmed-rs`):
- 15 min wall on 2–6 burst tasks of 32 CPUs.
- 1 content error.
- Bundles are text-only: figures, video and audio are re-fetched from their source URLs at embed time.
- 21 GB of bundles; OCR on for scanned supplements.
- Partitions are claimed with flock. A run killed with kill -9 mid-bundle and then resumed produces the same rows as
  a clean run (8,909 rows, no duplicates, no leftover partials).

**Embedding** (`scripts/embed_bundles.py` + flashembed CPU runner, fp32, 16-thread workers):

| model | items | worker-hours (16 CPU) | text items/s/worker | image items/s/worker |
|---|---|---|---|---|
| EmbeddingGemma 2 | 3.15M (2.81M text, 343k figures, 4,718 videos, 37 audio) | 406 | 3.72 | 0.56 |
| NeoMME-260M (dense; late vectors kept for 26 bundles) | 3.15M (2.81M text, 343k figures) | 254 | 3.71 | 2.19 |

- About 10,600 CPU-hours in total. Wall time was about 17 h on ~830 burst CPUs, including a 3 h stall when the
  6-hour task time limit was hit.
- About 230 media items per model were skipped, each with its reason recorded: 404 at source, too large, tiny, or
  no video stream.
- 6 bundles whose video batch failed (one file with no video stream) were repaired before publishing. All Gemma video
  vectors were later re-embedded with the fixed video decoder (pts-or-dts, exact frame times) and re-uploaded.

**Streaming publish**:
- 490/490 bundles plus both models' dense vectors uploaded to a private HF dataset.
- Every file was read back and hash-checked, then deleted locally.
- 21.7 GB of bundles; no outbox backpressure.
- Re-reading all bundles back from HF for a second run took 37–42 s, with all hashes OK.

**Retrieval fidelity** (`scripts/eval_retrieval.py`, self-supervised tasks over the full embedded corpus, 2,000
queries each; card query formats):

| task | corpus | Gemma MRR@10 / R@10 | NeoMME dense MRR@10 / R@10 | Gemma − NeoMME MRR@10 (95% CI) |
|---|---|---|---|---|
| caption → figure image | 342,966 figures | 0.442 / 0.569 | 0.165 / 0.230 | +0.277 (0.255, 0.298) |
| title → article (any text chunk) | 2,433,162 chunks | 0.924 / 0.956 | 0.852 / 0.925 | +0.072 (0.061, 0.083) |
| figure image → its caption | 373,390 captions | 0.451 / 0.586 | 0.095 / 0.137 | +0.356 (0.336, 0.377) |

NeoMME late-interaction rerank of its dense top-100, on the late-vector subset:
- caption → figure: MRR@10 0.245 → 0.346 (95 queries, 17k figures);
- title → article: MRR@10 0.911 → 0.953 (121 queries, 126k chunks).

Gemma was not scored on that subset yet, so these numbers are not comparable to the full-corpus rows.

Reading:
- EmbeddingGemma 2 is clearly better as the single dense space for text and figures.
- NeoMME's dense head is weak on natural figures. Its late interaction recovers a good part of that, consistent with
  its design (late interaction, page retrieval).
- Still open:
  - a fair late-interaction comparison (Gemma on the same subset, and NeoMME late on a full corpus);
  - page-image retrieval, where NeoMME is meant to be strong;
  - expert-labelled clinical queries.

  These proxies measure in-corpus self-retrieval, not clinical relevance.

**GPU bf16 corpus vs CPU fp32 corpus** (same 98k articles re-embedded by flashembed-gpu on one B200 in bf16 in 3.5 h,
searched with the same CPU-embedded fp32 queries, the planned production setup):

| task | model | CPU fp32 MRR@10 | GPU bf16 MRR@10 | diff |
|---|---|---|---|---|
| caption → figure | Gemma / NeoMME | 0.4418 / 0.1647 | 0.4418 / 0.1648 | +0.0000 / +0.0001 |
| title → article | Gemma / NeoMME | 0.9243 / 0.8525 | 0.9248 / 0.8528 | +0.0004 / +0.0003 |
| figure → caption | Gemma / NeoMME | 0.4510 / 0.0946 | 0.4522 / 0.0945 | +0.0012 / −0.0001 |

Per-unit cosine, GPU bf16 vs CPU fp32, on a 1/7 sample of bundles:

| model | text (median / min, n=401k) | images (median / min, n=49k) |
|---|---|---|
| Gemma | 0.99999 / 0.9989 | 0.99997 / 0.9954 (44 below 0.999) |
| NeoMME | 0.999997 / 0.9993 | 0.99998 / 0.9987 (4 below 0.999) |

Retrieval is unchanged, so the corpus can be embedded on GPUs in bf16 and queried with CPU-embedded vectors.

The 3.5 h GPU run spent only ~2,900 s of it on GPU compute. The rest went to:
- one runner launch per bundle and modality (~3 s each, 980 per model);
- downloading figures with the GPU idle;
- tokenization and JPEG decode on 2 CPUs.

The fixes are a persistent `serve` mode in flashembed, shared and prefetched media in `embed_bundles.py --serve`, and
8–16 CPUs per GPU job.

**Decision (2026-11-14): EmbeddingGemma 2 only.** EmbeddingGemma 2 at its native 768 dimensions (no Matryoshka
truncation) is PigMed's embedding model. It wins every retrieval task above and covers text, images, video and
audio. NeoMME is discontinued for the corpus and its vectors were deleted locally. flashembed keeps NeoMME support
as a general runtime.

## Chunking A/B (2026-11-14/15, 98k corpus, EmbeddingGemma 2, GPU serve)

Body text only: tables, captions and figures are left out of this test. Variants come from `scripts/chunk_variants.py`
and are scored with `scripts/eval_chunking.py`. Ranking is at article level. MRR@10 differences against A carry
bootstrap 95% CIs.

| Variant | Units | Median words | < 20 words | abstract sentence → body MRR@10 | Δ vs A (95% CI) |
|---|---|---|---|---|---|
| A: today's chunks, `title: none` | 2,021,579 | 157 | 10.1% | 0.746 | — |
| B: A + `title: <title> \| text: <section path>` | 2,021,579 | 157 | 10.1% | 0.711 | −0.034 (−0.044, −0.024) |
| C: balanced packing (target 384, min 100, max 512) | 1,468,168 | 353 | 4.7% | 0.750 | +0.004 (−0.004, +0.012) |
| CB: C + B's prefix | 1,468,168 | 353 | 4.7% | 0.686 | −0.060 (−0.071, −0.048) |
| D: C by late chunking (`title: <title>` prefix, one pass of up to 8,192 tokens per article, flashembed-gpu-late1) | 1,468,168 | 353 | 4.7% | 0.730 | −0.016 (−0.027, −0.004) |

- 3,000 queries per task.
- Title → article: A 0.948 and C 0.953 (+0.004, CI +0.001 to +0.008). B and CB reach 0.997, but that is leaky:
  the title is in their prompt.

Findings:

- **Prefixing the title and section path hurts** the untrained model by 3–6 MRR points on the fair task. A likely
  cause is that the shared prefix pulls every chunk of an article toward its title and dilutes the chunk's own
  content. So context should come from training (InSeNT) or late chunking, not from a prompt prefix.
- **Balanced packing (C) costs nothing in retrieval and cuts units by 27%**, and with them vectors, storage and
  embed time. Tiny chunks drop from 10.1% to 4.7%.
- **Zero-shot late chunking (D) is also slightly worse:** −0.016 vs A and −0.020 vs C on the fair task. Title →
  article is 0.982, but that is leaky (the title is in D's document prefix).
  - Embedding D took 31 min on one B200 for 94.5k articles.
  - EG2 was not trained to pool spans from a long pass, so each chunk's own signal is diluted. That is consistent
    with ConTEB, where untrained late chunking hurt some sets (COVID-QA −21.7).
  - Context has to be trained in (S2 / InSeNT); prompts and untrained pooling do not supply it.
- **Recommendation for production chunking now:** C (balanced packing, plain `title: none` prompt).

## ConTEB, EmbeddingGemma 2 zero-shot (2026-11-15, flashembed-gpu-late1, bf16, `scripts/eval_conteb.py`)

These are ConTEB's 8 evaluation sets, scored on nDCG@10 over every chunk of each set, as in the ConTEB harness.

- **Naive:** each chunk is embedded alone, as `title: none | text: <chunk>`.
- **Late:** a document's chunks are joined with `\n` into one prompted text and each chunk's span is pooled. The
  reference implementation joins with [SEP] tokens instead.
- Empty chunks (football: 224 of 6,259) get a zero vector, as in the reference pooling.

| Set | Queries | Naive | Late | Late − naive (95% CI) |
|---|---|---|---|---|
| covid-qa | 1,111 | 0.500 | 0.428 | −0.072 (−0.095, −0.050) |
| geography | 5,283 | 0.698 | 0.741 | +0.042 (+0.034, +0.051) |
| esg-reports | 36 | 0.388 | 0.319 | −0.070 (−0.213, +0.061) |
| insurance | 120 | 0.100 | 0.338 | +0.238 (+0.169, +0.306) |
| football | 2,682 | 0.333 | 0.515 | +0.182 (+0.168, +0.198) |
| squad-conteb-eval | 2,067 | 0.680 | 0.595 | −0.086 (−0.098, −0.073) |
| narrative-qa | 8,575 | 0.766 | 0.704 | −0.062 (−0.068, −0.056) |
| mldr-conteb-eval | 100 | 0.774 | 0.733 | −0.040 (−0.102, +0.021) |
| **Average** | | **0.530** | **0.547** | +0.017 |

- **Published comparison** (*reported*):
  - ModernBERT-large: 52.0 naive → 61.0 late (untrained) → 75.6 InSeNT-trained.
  - pplx-embed-context: 58.8 naive → 82–83 trained.
- **Untrained late chunking helps where chunks need their document** to be understood (insurance, football,
  geography: the entity is named elsewhere in the document). It hurts where chunks are self-contained (covid-qa,
  squad, narrative-qa).
- This matches the PMC chunking A/B, where D was −0.016. Trained contextual pooling (S2) is what should turn the
  context-dependent gains into gains everywhere.
- **Throughput:** each set takes 3–6 s per variant on one GPU, so ConTEB is a cheap gate for every training
  checkpoint.
