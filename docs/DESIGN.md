# Design and decisions

## 1. What corpus can actually be built?

Maintain three layers, not a misleading single “all PubMed full text” collection:

1. **Citation catalog:** the current annual PubMed baseline plus ordered updates, across all publication years. It includes metadata/abstracts where available, not every full article.
2. **Accessible evidence:** PMC's versioned Article Dataset inventory, plus additional independently authorized sources later. Availability of XML, PDF, figures, and supplements differs per record.
3. **Publication-approved subset:** content whose specific rights permit the intended remote hosting, reuse, and commercial/noncommercial operation. The initial implementation conservatively selects CC0/CC-BY metadata codes; additional rights review remains necessary.

There is no official endpoint that delivers the full text of every paywalled PubMed record. Historical publication coverage and access rights are separate dimensions. The annual baseline is not restricted to publications from its own year. Model training eligibility and public dataset redistribution eligibility can also differ.

Use the public `pmc-oa-opendata` S3 bucket, current metadata inventories, and annual PubMed XML.gz bulk files. Do not distribute per-article requests across VPNs to evade service limits. E-utilities limits are not a reason to use E-utilities for an entire bulk corpus. Respect Retry-After, bound concurrency, and coordinate unusual aggregate transfer volume with your institution and NLM where appropriate.

## 2. Pipeline

```
PubMed baseline + updates             PMC daily inventory
          |                                  |
 citation event stream              immutable version/ETag plan
          |                                  |
 ordered snapshot                  metadata / rights / retraction gate
                                             |
                           bounded article workspace in node-local TMPDIR
                                             |
              canonical JATS + structured text + actual media manifest
                                             |
           model-specific views: text chunks / pages / figures / timed clips
                                             |
                  pinned encoder and preprocessing recipe
                                             |
               immutable, checksummed outbox bundles
                                             |
                private HF commit -> remote verify -> receipt
                                             |
                   delete committed local shard / release budget
                                             |
                   separate searchable serving indexes
```

The whole corpus need not be downloaded first. An individual article may be replayed after a crash; committed shard cursors are idempotent. Source HTTP downloads retry from the beginning of an object, not from an HTTP Range offset. Large file range-resume is an extension. One article exceeding explicit limits fails the partition rather than silently truncating it; inspect, increase limits intentionally, or place it on a separately reviewed exclusion manifest.

For a long-running service, add an inventory diff/tombstone controller and reconcile removed versions, in-place updates, retractions, and remote published snapshots. The v0.1 code constructs snapshots and handles citation updates; it does not promise ongoing automated PMC deletion propagation. Capture inventory manifests because daily inventories are retained for only 30 days.

## 3. Evidence representation: not XML versus PDF

**Canonical structured text:** parse JATS first. Preserve article/section/block/figure identities, captions, equations, table XML, citations, dates, license text, and retraction flags. Keep original XML so a parser improvement does not require redownloading the corpus.

**Text views:** start at 512 selected-model tokens with 64-token overlap, section-aware boundaries, then test 256/512/1024 and overlap 0/64/128. These are proposed ablations, not a published biomedical optimum for either model. Leave title/prompt space below the model maximum. Preserve the parent section and adjacent evidence for later expansion. The first model-agnostic pass can use provisional word-count views; final model runs use tokenizer offsets. A word is not a token.

**Page views:** render one PDF page as an image. A PDF is not directly an embedding tensor. A page-level view preserves layout, plots, and tables but can dilute relevance when several unrelated elements share a page. Retain page number, source checksum, and render settings. For NeoMME, start with the documented 2048-pixel longest-side cap, with a 1536-pixel speed/quality ablation. Do not upscale figure source images by default.

**Figure views:** index original figure + caption as separate evidence with a link to its parent article, and test figure-only versus figure-plus-caption. Figure crops and panel-level crops may improve image queries, but splitting compound figures is not implemented yet. Retain units, axes, labels, and nearby narrative. Adding page views does not eliminate the need for figure views: a phone photograph of a graph may be much more similar to a crop than to a page dominated by prose.

**Tables/equations:** keep structured originals and distinct text units; large tables may still span several budgeted views. A vector is not a lossless encoding of every number. Numerical answers should inspect the original table/chart and cite it.

**Scanned documents:** page retrieval works without OCR. Text search over scans needs trustworthy supplied OCR or a later OCR pipeline. This implementation does not silently invent missing text.

**Video/audio:** discover actual media files from metadata. Start with 20-second visual video clips and 2-second overlap, and independent 30-second audio windows with 2-second overlap. These are test settings, not measured optima. A full video may receive multiple vectors. Video decoding is visual-only in this package; soundtrack retrieval requires the audio branch. ECG/EEG numerical signals are not sound just because they can be plotted. Index waveform images visually and preserve raw arrays for a future signal encoder.

## 4. Model choice

**EmbeddingGemma 2** is the broad starting candidate for text/image/video/audio in one dense space. Load only needed modality encoders for text or text+vision workers. Begin with 768d; evaluate 512d/256d rather than assuming text compression behavior transfers to fine scientific plots. Higher vision-token budgets may help small chart labels; the bundled adapter records and uses processor defaults. Add a validated budget parameter only after confirming the installed processor interface.

**NeoMME-260M-Retriever** is the visual late-interaction candidate. It returns a dense vector plus 128d token/patch vectors. Its strongest published results are late-interaction text-to-page retrieval. Do not replace its late representation with a dense head and assume identical performance. Its denser patch representation may help precise figure content, but camera-image queries and medical sensitivity require a dedicated evaluation.

**NeoMME-800M** should be a later quality ablation; first establish the retrieval and storage trade-off on 260M. The code accepts a different NeoMME model ID/revision, but real 800M inference has not been tested here.

The code's int8 late-vector storage is not the paper's hierarchical pooling/asymmetric binary pipeline. Do not budget at 6 kB/page until the exact compression and scoring configuration has been reproduced on held-out biomedical data. Storage compression does not automatically accelerate page encoding, and a small parameter count does not eliminate long visual attention cost.

Recommended initial decision: Gemma 2 for broad dense candidate generation; evaluate NeoMME as a specialist. Keep a biomedical text baseline such as MedCPT and a biomedical image baseline such as BiomedCLIP in the research comparison. No universal PubMed winner has been established by the supplied general benchmarks.

## 5. Image-query search is its own task

Separate four tasks:
- Exact/near-duplicate source finding for a screenshot or photograph.
- Semantically related figures despite different color/layout/plot style.
- Text questions answered by a figure/page.
- Multimodal evidence synthesis, where a generator explains “what is this?” from retrieved material.

The fourth requires an answer/vision model downstream; an embedding model only retrieves similar evidence. Synthetic crops/rotation/blur/perspective tests are useful robustness checks but cannot substitute for expert-labeled medical relevance. Train/test comparisons should include real held-out photographs when possible.

A final service can run image-vector retrieval, text-vector retrieval, BM25 over titles/captions/sections, and optional OCR/caption-derived text queries, fuse rankings, and rerank source evidence. Keep each model's vector space separate. The included prototype has exact dense retrieval plus optional late reranking and an RRF helper, not a deployed hybrid server. If dense candidate recall is poor, late reranking cannot recover missed evidence; evaluate an independent late-interaction index before drawing conclusions about NeoMME.

## 6. Cluster resource plan

Site-specific allocations (accounts, QOS names, quotas, storage paths) are kept out of this repository, in a local site env file (see `configs/site.env.example`).

Start small: eight workers with eight CPU cores and 32 GiB each (64 cores, 256 GiB when all run), plus a separate small upload job. Prefer idle/low-priority CPU capacity for acquisition, and never disturb other users' GPU jobs. Ramp only after measuring network throughput, filesystem pressure, memory and queue delay. Low-priority capacity is not guaranteed; keep jobs short (hours) and restartable. Reported allocation ceilings are arithmetic headroom, not scheduler availability.

Use `$TMPDIR` (node-local scratch) for transient article XML/PDF/media/rendering, and persistent group storage for canonical committed bundles, caches, receipts, plans and logs. Keep many tiny temporary files off a shared parallel filesystem. A 200 GiB outbox is not a hard filesystem quota: model caches, other group members and other jobs share the group's storage. Reserve headroom and monitor the actual group quota.

## 7. Storage and shard policy

Raw dense bytes = units × dimensions × bytes-per-value. At 768d FP16:

| Units | Raw dense storage (decimal GB) |
|---:|---:|
| 10 million | 15.36 |
| 40 million | 61.44 |
| 100 million | 153.60 |

Hypothetical full-text scenario: 8 million articles × (10 text chunks + 10 pages + 4 figures) = 192 million evidence units, or 294.912 GB of 768d FP16 vectors. This is not a measured article count, page average, or rights-cleared corpus size. It excludes metadata, media, ANN overhead, replicas, and any abstract catalog index.

The same hypothetical 80 million page views would be 480 GB at the paper's ~6 kB compressed late-vector point, or 3.12 TB at ~39 kB/page. These are extrapolations from a general visual benchmark, not biomedical measurements. Unpooled late vectors can be orders of magnitude larger. The original media, especially rendered PNGs and videos, may dominate both.

Start with 256–512 MiB target bundles. After measuring row-size distributions, move toward 0.5–2 GiB and up to 2000 articles/partition where appropriate. Small article-count cutoffs create tiny shards and excessive Hub commits. The current publisher makes one commit per bundle; a very large release needs shard compaction and deliberate repository partitioning. No unlimited free Hub storage is assumed. Plan quota approval before a full-corpus run.

Keep full evaluation media, canonical text, and embeddings. For the large index, store original media URLs/checksums/transforms instead of every rendered page when hosting quota is tight. This changes re-embedding guarantees: mutable/removed upstream sources may prevent reproducing an old view. Explicitly label snapshot date and source version.

## 8. Release gates

1. Offline fixtures and checksum/restart tests pass.
2. Ten-article live bulk smoke test validates current metadata schema, source hashes, licenses, and asset mappings.
3. Small private-Hub test verifies upload, readback, restart, and deliberate upload failure without deleting source data.
4. Several-thousand-article pilot estimates media prevalence, units/article, storage/article, bytes/token, and end-to-end throughput.
5. Real model smoke tests confirm official interfaces, dtype, masks, input tasks, and no silent truncation; lock software versions.
6. Held-out text/image/photo retrieval evaluation selects representation, chunking, vision budget, and compression.
7. Storage approval and production ANN/metadata design precede corpus-wide embedding.
8. Biomedical adaptation precedes a final corpus-wide re-embedding run, if adaptation proves useful.

The current package completes gate 1 locally; it supplies commands and scaffolding for subsequent gates, not evidence that those gates already passed.

## 9. Educational material

Bookshelf/NLM LitArch OA books are a sensible later collection for textbook-style learning material. NLM provides book packages that may contain BITS XML, PDFs, and graphics; their provenance and license policy should remain distinct from PMC journal articles. Build a BITS-aware adapter rather than treating books as JATS research papers or scraping arbitrary book websites. This source is documented but not implemented in v0.1.
