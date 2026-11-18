# Practical retrieval techniques for PigMed (survey, November 2026)

Status: research notes, not results. Written 2026-11-17 for the PigMed serving stack:

- **Corpus:** 9.35M PMC articles, about 251M chunks, 31M figure vectors.
- **Encoder:** EmbeddingGemma 2 (EG2), 768-d Matryoshka (MRL), 256-token chunks with a section-heading prefix and a
  0.5 late-chunking mix.
- **Index:** HNSW article routing, PQ chunk scoring, then an exact int8 re-rank of the top 20 articles.
- **Serving box:** 2 vCPU, 16 GB RAM, Rust server.

**How to read the evidence labels:**

- **[PigMed]** is our own measurement.
- **[reported]** is a number from the cited paper or model card. Most are self-reported and not reproduced.
- **[inference]** is my reasoning or a back-of-envelope estimate. Treat it as a hypothesis to test.
- **[weak]** marks a claim with thin or conflicting evidence.

Licence flags use the project rules:

- No weights trained by Chinese companies anywhere in the chain, including teachers, negative miners and data
  generators.
- Commercially clean data preferred.
- MS MARCO is flagged because its terms are non-commercial.

---

## Top 10 techniques to test next

Do this first, because every ranking below depends on it. Build two extra eval sets before tuning anything:

1. **Keyword/entity/ID queries:** gene symbols, drug names, PMIDs, NCT numbers, rsIDs, and short MeSH-style
   keyword queries.
2. **Real queries:** BioASQ, TREC-COVID, NFCorpus, TREC CDS/PM, and RELISH.

Report results per query type. Our current set is LLM-generated, which likely understates lexical methods (§8). The
gains below are relative to the current stack (dense MRR@10 0.771 [PigMed]) and are mostly [inference].

| # | Technique | Expected gain | CPU cost on the 2-vCPU box | Licence / provenance | Model / library |
|---|---|---|---|---|---|
| 1 | **Article-level BM25F hybrid**: title, abstract, keywords, MeSH, section headings, captions. Fuse with dense using a tuned **convex combination** on per-query-normalised scores; RRF k=60 as the baseline. | +0.005–0.02 MRR on the synthetic set. +0.05–0.20 on keyword, entity and ID sets. Largest on BioASQ-like queries. [inference, from §1 evidence] | 5–30 ms per query. Article-level index about 2–5 GB, so it fits RAM and disk. A chunk-level index (≈40–60 GB) would not fit the 50 GB disk. [inference] | Clean (no model) | `bm25x` (Rust, LightOn, Apache-2.0) or `tantivy`. Write our own BM25F term-frequency merge (§2). |
| 2 | **Query router for IDs and rare entities**: regex for PMID, PMCID, DOI, NCT, rsID and CAS; dictionary match on gene symbols, drugs and MeSH entry terms. Use an exact-match lookup or raise the lexical weight α. | Large on those queries; ≈0 elsewhere. [inference; Sciavolino 2021] | <1 ms | MeSH and HGNC are free; UMLS has licence terms (flag) | `regex`, `aho-corasick`, `fst` crates |
| 3 | **Biomedical tokeniser and normaliser for the lexical side**: Greek letters (α → alpha), hyphen, space and digit variants (IL-6 / IL6 / IL 6), NFKC, light stemming, no stemming of gene symbols. | Feeds #1. TREC Genomics-era studies showed sizeable gains from tokenisation alone. [reported, older; Jiang & Zhai 2007] | Index-time only | Clean | Custom `tantivy` TokenFilter plus the `unicode-normalization` crate |
| 4 | **Domain-distilled small cross-encoder** to replace the MS MARCO MiniLM. Student: an Ettin *encoder* (17M–32M) or a 6-layer biomedical BERT. Teacher: MedCPT-Cross-Encoder (public domain) and/or an LLM judge, on PMC queries. Export through ONNX int8. | Keeps the current +0.031 and plausibly adds +0.01–0.02. Removes the MS MARCO flag. [inference] | 17M at depth 20–30: ≈0.2–0.6 s. 32M: ≈0.6–1.5 s. [inference from Ettin CPU numbers, scaled to 2 vCPU] | Teacher MedCPT is public domain (NCBI). Ettin encoders are JHU, open data. Do **not** use the Ettin *rerankers*: they are distilled from Qwen-based mxbai-rerank-large-v2. | `ort` (ONNX Runtime) or `candle`; `sentence-transformers` for training |
| 5 | **Learned fusion / small LTR** with LambdaMART or logistic regression on ~15 features: dense article score, best-chunk score, top-k chunk aggregate, per-field BM25F, cross-encoder score, query type, length, max IDF, article type, recency. | +0.01–0.03 over hand-tuned fusion. PubMed Best Match runs BM25 plus LambdaMART in production. [inference; Fiorini 2018] | <1 ms | Clean | LightGBM (MIT) for training; ~100 lines of Rust to evaluate the trees, or `ort` |
| 6 | **Adaptive re-rank depth (cascade / early exit)**: skip or shorten the re-rank when the top-1/top-2 dense margin is large; re-rank deeper on lexical-dense disagreement. | Same quality at 30–60% fewer re-rank CPU-ms. [inference; Petrov 2024] | Saves CPU | Clean | Server logic |
| 7 | **Selective LLM query rewriting**: rewrite only queries classified as lay or underspecified; send 2–3 rewrites through multi-query fusion; cache the results. Add a zero-latency lay→MeSH dictionary as a fallback. | +0.02–0.026 on lay queries already measured [PigMed]. Avoids the ≈0 or negative effect on expert queries. [Weller 2024] | 0 ms local; 0.5–2 s API on the routed subset only | Claude via API. For an open release, check that the outputs may be used. | Classifier: logistic regression on features, in Rust |
| 8 | **Multi-granularity score aggregation tuning**: article score = w1·article vector + w2·max chunk + w3·soft top-3 chunk mean (+ BM25F). Learn it with #5. | +0.005–0.02 [inference; MaxP/SumP literature] | ≈0 | Clean | Server logic |
| 9 | **Inference-free (doc-only) learned sparse at article level**, as a third fusion leg or a BM25 replacement. Candidates: OpenSearch doc-v2/v3-distill or SPARSEUP. Index with Seismic. Better: train our own doc-only sparse model on clean data. | +0–0.01 on top of dense+BM25 when dense is strong [weak]. Larger if it replaces BM25 on semantic queries. | Doc-only: query cost ≈ BM25 (token lookup). SPARSEUP needs a 149M query-encoder pass, ≈30–80 ms [inference]. Seismic: sub-ms [reported]. | OpenSearch: Apache-2.0, but MS MARCO in training, and "v3-gte" probably descends from Alibaba GTE (unverified). SPARSEUP: Apache-2.0, but data from the LightOn mixture (MS MARCO, gte-modernbert-mined negatives). Both are flagged. | `seismic` (Rust, TusKANNy); `ort` |
| 10 | **Cheaper routing vectors**: binary or MRL-256 int8 article vectors for HNSW traversal with float or int8 rescoring. Measure **routing recall@100**, which caps everything downstream. Use EG2 QAT int8 for query encoding. | Not a quality gain: it buys latency and RAM for #1, #4 and #5. Binary plus rescore keeps ≈96%; int8 plus rescore ≈99% [reported, HF blog]. | Saves 5–30 ms and GBs | Clean | `usearch` (Rust bindings) or own HNSW; `ort` |

Just outside the top 10:

- **Opt-in "deep search" listwise LLM re-ranking** of the top 10–20 with Claude (§4.3). Large quality ceiling, but
  1–3 s and a per-query API cost.
- **Deterministic contextual prefixes** (title, journal, MeSH) added to chunks at re-embed time (§6). Needs a full
  re-embed of 251M chunks on a GPU cluster.
- **Dense PRF** (Rocchio on vectors; §5). Nearly free, but mixed evidence.

Not recommended for this box:

- **ColBERT / late interaction as a re-ranker:** token vectors do not fit the disk, so they would be encoded at query
  time and would cost as much as a cross-encoder with less precision (§4.2).
- **doc2query at 251M chunks** (§6).
- **Local LLM re-rankers** (§4.3).

---

## 1. Hybrid lexical + dense fusion

### Fusion functions

**Reciprocal rank fusion (RRF):** score = Σ 1/(k + rank), with k=60 by default (Cormack et al., SIGIR 2009).

- Needs no score calibration and no training data.
- The usual default in Elastic, OpenSearch, Qdrant, Vespa and Weaviate.

**Convex combination (CC):** s = α·norm(s_lex) + (1−α)·norm(s_dense).

Bruch, Gai & Ingber (TOIS 2023; [arXiv 2210.11934](https://arxiv.org/abs/2210.11934)) found:

- CC **beats RRF both in-domain and out-of-domain**.
- RRF is **sensitive to its parameters**, despite its reputation.
- CC is barely affected by the normalisation choice (min-max, z-score, or "theoretical min-max").
- α is **sample-efficient**: a small labelled set tunes it.

[reported]

**Implication for PigMed:** we have labelled dev queries, so CC with per-query min-max over the union of top-k
candidates is the default to beat. Keep RRF as the baseline, and as the fallback when one leg returns nothing.

**Distribution-based normalisation:** Qdrant's DBSF clips at mean ± 3σ, and Weaviate's relativeScore is similar. Both
are pragmatic variants of z-score normalisation. Evidence beyond vendor posts is [weak].

### Learned and adaptive weighting

**Query-dependent selection** between sparse and dense, using query-performance-prediction (QPP) features: Arabzadeh,
Yan & Clarke, CIKM 2021, "Predicting efficiency/effectiveness trade-offs for dense vs. sparse retrieval strategy
selection". It shows that per-query routing recovers much of the oracle gain. [reported; exact numbers not re-checked]

**DAT, Dynamic Alpha Tuning** (Hsu & Tzeng 2025, [arXiv 2503.23013](https://arxiv.org/abs/2503.23013)):

- An LLM judges the top-1 result from each leg and sets α per query.
- It beats fixed α [reported], but costs one LLM call per query. Too expensive for the default path.
- A cheap stand-in is a logistic regression on QPP features. [inference]

**Features that predict "lexical wins"** [inference, consistent with the literature below]:

- An ID or code pattern in the query.
- A gene, drug or variant token with high IDF.
- Query length ≤ 3 tokens.
- Out-of-vocabulary word pieces in the EG2 tokeniser.
- Low agreement between the BM25 and dense top-10.

Dense retrievers fail on rare entities: Sciavolino et al., EMNLP 2021, "Simple entity-centric questions challenge
dense retrievers".

### What the benchmarks show (biomedical)

**BEIR BM25 baselines (nDCG@10)**, from the MedCPT paper's table, [arXiv 2307.00589](https://arxiv.org/abs/2307.00589),
Table 1 [reported]:

| Dataset | BM25 |
|---|---|
| TREC-COVID | 0.656 |
| NFCorpus | 0.325 |
| BioASQ | 0.465 |
| SciFact | 0.665 |

In the same table, BM25 beat many dense retrievers on biomedical tasks:

| Retriever | TREC-COVID | NFCorpus | BioASQ | SciFact |
|---|---|---|---|---|
| TAS-B | 0.481 | 0.319 | 0.383 | 0.643 |
| GTR-XXL | 0.501 | 0.342 | 0.324 | 0.662 |
| MedCPT retriever only | 0.697 | 0.340 | 0.332 | 0.724 |

**BioASQ is the clearest "lexical wins" case.** Even MedCPT's dense retriever, trained on 255M PubMed click pairs,
scores 0.332 against BM25's 0.465.

**Modern general-purpose encoders have closed much of the gap on BEIR averages.** Biomedical, entity-heavy sets are
still where fusion pays off most. [inference]

**Fusion gains** on BEIR-style sets are typically a few nDCG points over the better single leg, and larger where the
legs disagree (BioASQ, TREC-COVID, Touché). [weak: varies by paper and encoder; we have not found a 2025–26 controlled
biomedical study with a modern encoder]

**BioASQ 2024–25 participants** still mostly run a BM25 (often PubMed's own) first stage plus neural re-ranking, or a
hybrid ([BioASQ 2024 overview](https://arxiv.org/abs/2508.20532); CEUR Vol-4038 notebooks). [reported]

**PubMed's production "Best Match"** is BM25 top-500 → LambdaMART on usage, date and type features (Fiorini et al.,
PLoS Biol 2018, [PMC6112631](https://pmc.ncbi.nlm.nih.gov/articles/PMC6112631/)). It is the strongest real-world
precedent for #1 and #5.

---

## 2. BM25 variants, biomedical tokenisation, synonym and MeSH expansion

### Variants

Kamphuis, de Vries, Boytsov & Lin, ECIR 2020 ("Which BM25 do you mean?",
[PDF](https://cs.uwaterloo.ca/~jimmylin/publications/978-3-030-45442-5_4.pdf)) compared eight variants: Robertson,
Lucene default, Lucene accurate, ATIRE, BM25L, BM25+, BM25-adpt and TF_l∘δ∘p×IDF. They found **no significant
differences**. [reported, on newswire]

**Pick by engineering convenience.**

- Use ATIRE or Lucene IDF, which are never negative.
- **BM25L and BM25+** (Lv & Zhai 2011) fix over-penalisation of long documents. That matters for variable-length
  article text, not for our fixed 256-token chunks. [inference]
- Tune k1 (0.6–1.5) and b (0.3–0.9) on the dev set; expect ±1 point. [inference]

### BM25F (field weighting)

Robertson, Zaragoza & Taylor, CIKM 2004, "Simple BM25 extension to multiple weighted fields": **combine weighted term
frequencies across fields before saturation.** Summing per-field BM25 scores double-counts terms and over-rewards
documents that repeat a term in several fields.

- `tantivy` multi-field queries sum per-field scores. That is the "wrong" BM25F, though often adequate. [inference;
  check the current tantivy version]
- `bm25x` (LightOn, Rust, [docs.rs](https://docs.rs/crate/bm25x/latest)) offers five variants, mmap and streaming
  updates. Check field support. Its self-reported SciFact numbers match `bm25s`.
- **A ~300-line custom BM25F over our own postings is reasonable** for a fixed schema.

Suggested starting field weights [inference; tune on dev]:

| Field | Weight |
|---|---|
| Title | 3 |
| MeSH / keywords | 2 |
| Abstract | 1 |
| Section headings | 1 |
| Figure captions | 0.5 |

**Size.** Article-level postings for title, abstract, MeSH and headings come to about 9.35M × ~150 unique terms ≈ 1.4B
postings, or 2–4 GB compressed. Full-body chunk-level BM25 is about 251M × ~150 ≈ 38B postings, or 40–60 GB. That
exceeds the Space's 50 GB ephemeral disk alongside the vector index. [inference, back-of-envelope]

So do article-level BM25F first. Full-body lexical recall comes mostly from the dense chunks anyway.

### Biomedical tokenisation

**Jiang & Zhai, Information Retrieval 10 (2007), "An empirical study of tokenization strategies for biomedical
information retrieval" (TREC Genomics):**

- Normalising hyphens, parentheses, Greek letters and number/letter boundaries in gene names gave large MAP gains over
  a default tokeniser. [reported, older collection; exact figures not re-checked]

**Practical recipe** [inference]:

- **Unicode:** NFKC; map Greek letters to names (α→alpha, β→beta, κ→kappa); handle superscripts and subscripts.
- **Indexing:** index both split and joined forms (`IL-6` → `il`, `6`, `il6`), as Lucene's WordDelimiterGraphFilter
  does.
- **Case:** lowercase for the main field. Optionally keep a case-sensitive gene-symbol field so that `CAT` (the gene)
  is not `cat`.
- **Stemming:** light S-stemmer or Krovetz. Do not stem tokens matching a gene, chemical or variant dictionary.
- **Chemicals:** keep IUPAC fragments, and index normalised chemical names alongside them.

### Synonym, MeSH and entity expansion

**Prefer document-side expansion to query-side expansion.** Index MeSH terms, keywords and normalised entity IDs as
fields. This costs no query latency and does not dilute the query.

- **MeSH via PubMed Automatic Term Mapping** "can generally improve retrieval performance", but with limited effect for
  real users (Lu, Kim & Wilbur 2009, [PMC2747526](https://pmc.ncbi.nlm.nih.gov/articles/PMC2747526/)). [reported]
- **Gene-synonym expansion results are mixed.** At TREC Genomics it was often ineffective without careful weighting.
  [weak]
- **PubTator3** (Wei et al., NAR 2024) provides normalised gene, chemical, disease, variant and species annotations for
  PubMed and PMC-OA.
  - Index the concept IDs (e.g. `NCBIGene:7124`) as a field.
  - On the query side, map query tokens through the same dictionaries.
  - NCBI data is generally public domain; verify the PubTator3 terms. [inference on the gain]
- **UMLS** (e.g. consumer-health vocabulary to MeSH) requires a UMLS licence. MeSH alone is free to use.
- **BMX** (Mixedbread 2024, [arXiv 2408.06643](https://arxiv.org/abs/2408.06643)) is an entropy-weighted BM25 variant
  with small reported gains. [weak, vendor]

---

## 3. Learned sparse retrieval

| Model | BEIR-13 nDCG@10 | Query side | Licence / provenance |
|---|---|---|---|
| BM25 | ≈0.43–0.44 | tokeniser | clean |
| SPLADE-v3 (Naver, 2024; [arXiv 2403.06789](https://arxiv.org/abs/2403.06789)) | 0.517 [reported] | full encoder | **CC BY-NC-SA 4.0**, MS MARCO. Excluded for a clean release. |
| OpenSearch doc-v2-distill (67M) | 0.504 [reported] | **inference-free** (IDF lookup) | Apache-2.0; MS MARCO in training (flag) |
| OpenSearch doc-v3-distill (67M) | 0.517 [reported] | inference-free | Apache-2.0; MS MARCO (flag) |
| OpenSearch doc-v3-gte (133M) | 0.546 [reported] | inference-free | Apache-2.0; MS MARCO. The name implies an Alibaba GTE base; the card does not say. **Treat as excluded until verified.** |
| SPARSEUP (Linkup, Sep 2026) | 0.564 [reported] | full encoder (ModernBERT 149M) | Apache-2.0. Initialised from LateOn-unsupervised and fine-tuned on LightOn's mixture (MS MARCO included; negatives mined with Alibaba gte-modernbert-base). **Flag.** |

Biomedical per-dataset numbers for OpenSearch doc-v3-gte [reported,
[model card](https://huggingface.co/opensearch-project/opensearch-neural-sparse-encoding-doc-v3-gte)]:

| Dataset | doc-v3-gte | BM25 |
|---|---|---|
| TREC-COVID | 0.734 | 0.656 |
| NFCorpus | 0.360 | 0.325 |
| SciFact | 0.725 | 0.665 |

doc-v3-distill scores 0.724, 0.345 and 0.708 on the same three.

**Inference-free sparse models** (OpenSearch team, [arXiv 2411.04403](https://arxiv.org/abs/2411.04403), adding IDF-aware
penalties and distillation) close most of the gap to query-encoded SPLADE at BM25-like query cost. That makes them a
good fit for a 2-vCPU box. [reported]

**SPARSEUP** ([coverage](https://www.marktechpost.com/2026/09/19/linkup-research-releases-sparseup/),
[summary](https://aicybr.com/blog/sparseup-open-sparse-retrieval-model)):

- Scores 56.4, against LateOn 58.9 and DenseOn 57.9 on the same backbone and data.
- With Seismic: >97% recall against exact search at ≈380 µs per query, single-threaded. [reported]
- Needs a query-encoder forward pass.

**Seismic** (Bruch, Nardini, Rulli & Venturini, SIGIR 2024; [arXiv 2404.18812](https://arxiv.org/abs/2404.18812);
[Rust repo](https://github.com/TusKANNy/seismic)):

- Blocked inverted lists with summary sketches.
- Sub-millisecond queries on MS MARCO learned-sparse vectors.
- 1–2 orders of magnitude faster than earlier inverted-index LSR engines. [reported]
- Native Rust. OpenSearch 3.3 re-implemented it in Java.

**Learned sparse vs BM25 inside a hybrid:**

- On its own, learned sparse beats BM25 by 7–13 BEIR points. [reported]
- When the dense leg is strong, the *marginal* gain of the sparse leg in a three-way or two-way fusion is smaller than
  the standalone gap suggests. Learned sparse and dense models make more correlated errors than BM25 and dense do,
  because both are trained on the same kinds of data. [weak / inference: we found no controlled 2025–26 study with a
  modern dense model on biomedical data]
- **Test BM25F first.** Add doc-only sparse as a third leg, and measure the marginal gain.

**Size caution** [inference]:

- Learned-sparse documents carry about 100–300 non-zeros with expansion terms, so postings are larger than BM25.
- Chunk-level indexing (251M chunks) is infeasible on the Space disk. Article- or abstract-level indexing is fine.

**Clean path:** train our own inference-free sparse head on EG2 or an Ettin encoder, with PMC-only data and a
non-Chinese teacher. The SPLADE recipe is published and reusable; this is the same pattern as
`RETRIEVER_TRAINING_PLAN.md`. [inference]

---

## 4. Re-ranking

### 4.1 Cross-encoders

**Current: `cross-encoder/ms-marco-MiniLM-L6-v2`** (22M), +0.031 [PigMed].

- **Flag:** trained on MS MARCO, which is non-commercial.
- Ettin's benchmark puts it at NanoBEIR 0.631 and MTEB-retrieval re-rank 0.508 [reported].

**Ettin reranker family** (17M–1B, Apache-2.0; [HF blog](https://huggingface.co/blog/ettin-reranker)) [reported]:

| Model | NanoBEIR | CPU pairs/s (i7-13700K) |
|---|---|---|
| ettin-reranker-17m | 0.675 | 267 |
| ettin-reranker-32m | 0.683 | 92 |
| ettin-reranker-68m | 0.692 | 31 |
| ettin-reranker-150m | 0.709 | 14 |
| MiniLM-L6 (reference) | 0.631 | 144 |

- The 17M model is +0.04 over MiniLM-L6 and *faster* on CPU.
- **Excluded by our rule:** they are distilled from `mxbai-rerank-large-v2`, which is Qwen2.5-based, and their data
  includes MS MARCO.
- The **Ettin encoders** (JHU, open pre-training data) are a clean base to distil our own reranker.

**MedCPT-Cross-Encoder** (NCBI; [HF](https://huggingface.co/ncbi/MedCPT-Cross-Encoder); [arXiv 2307.00589](https://arxiv.org/abs/2307.00589)):

- PubMedBERT-base (110M).
- Trained on 18M PubMed click query-article pairs with local hard negatives.
- **Public domain** (US government work).

Retriever → retriever plus cross-encoder, nDCG@10 [reported]:

| Dataset | Retriever only | + MedCPT cross-encoder |
|---|---|---|
| TREC-COVID | 0.697 | 0.709 |
| NFCorpus | 0.340 | 0.355 |
| BioASQ | 0.332 | **0.553** |
| SciFact | 0.724 | 0.761 |

- Cross-encoders recover most where the first stage is weak.
- It is too slow for 2 vCPU at 512 tokens: roughly 0.2–0.5 s per pair [inference].
- **It is the ideal teacher**, on title plus abstract, for a 6-layer or Ettin-17M/32M student (#4).

**Other cross-encoders:**

| Model | Status |
|---|---|
| BGE / GTE / Qwen3 rerankers | Chinese origin. Excluded. |
| mxbai-rerank-v2 | Qwen2.5-based. Excluded. |
| mxbai-rerank-v1 | DeBERTa-v3. Training data undisclosed (flag). |
| jina-reranker-v2 | CC BY-NC. Flag. |
| jina-reranker-v3 / jina-embeddings-v4 | Qwen-based. Excluded. |

On Jina as a company: Jina AI GmbH is Berlin-headquartered and was acquired by Elastic in October 2025
([BusinessWire](https://www.businesswire.com/news/home/20251009619654/en)). I recall Jina also had China offices
(unverified). Decide per model; the recent models fail on their Qwen bases anyway.

**Shallow cross-encoders** (Petrov, MacAvaney & Macdonald, ECIR 2024;
[arXiv 2403.20222](https://arxiv.org/abs/2403.20222)):

- Under a ≈50 ms budget, shallow cross-encoders that score more candidates beat deep ones scoring fewer.
- On CPU, nDCG@10 drops only about 3% against GPU at 50 ms. [reported]
- This directly supports "small model, deeper list" on our box.

**Distillation from LLM rankings:** Rank-DistiLLM (Schlatt et al. 2024,
[arXiv 2405.07920](https://arxiv.org/abs/2405.07920)) distils listwise LLM rankings into cross-encoders that match
much larger models. [reported]

- With a Claude or Gemma teacher on PMC-derived queries, this is a commercially clean route, provided the API terms
  allow it. Check them.
- For controlled training choices, see the 2026 study [arXiv 2603.03010](https://arxiv.org/abs/2603.03010).

### 4.2 Late interaction (ColBERT / PLAID / LateOn) as a re-ranker

**LateOn** (LightOn, Apr 2026; 149M ModernBERT; Apache-2.0):

- BEIR 57.2 [reported].
- **Provenance flag:** the training data is scored by mxbai-rerank-large-v2 (Qwen2.5), and its negatives were mined
  with gte-modernbert (Alibaba). This is already recorded in `RETRIEVER_TRAINING_PLAN.md`.
- GTE-ModernColBERT has an Alibaba base and is excluded.

**Rust tooling:**

- [`next-plaid`](https://docs.rs/next-plaid): pure-Rust, CPU-only PLAID with mmap, 2/4-bit PQ and incremental updates.
- `fast-plaid`.
- `pylate-rs`: ColBERT inference on `candle`.

**MUVERA** (Google, NeurIPS 2024; [arXiv 2405.19504](https://arxiv.org/abs/2405.19504)): fixed-dimensional encodings
make multi-vector *candidate generation* a single-vector maximum-inner-product search. Reported: about 10% higher
recall and about 90% lower latency than PLAID on BEIR.

**Why it is low priority on our box** [inference]:

- Precomputed token vectors are the whole point of late interaction.
- Storing them is out of reach:
  - all chunks: 251M × 256 tokens × ~36 B ≈ 2.3 TB;
  - abstracts only: ≈100 GB.
- Without stored vectors, ColBERT re-ranking encodes the candidates at query time. That is the same cost class as a
  cross-encoder of equal size, and it is usually less precise.
- Revisit if serving moves to a larger disk, or for a figure / page-image index (ColPali-style).

### 4.3 LLM re-rankers

**Evidence:**

- **RankGPT** (Sun et al., EMNLP 2023; [arXiv 2304.09542](https://arxiv.org/abs/2304.09542)): listwise GPT-4 over the
  BM25 top-100 gave large gains on TREC-COVID, and was roughly on par with monoT5-3B on NFCorpus and SciFact. [reported;
  pattern recalled, exact numbers not re-checked]
- **Open listwise models:** RankZephyr (Mistral-based; trained on GPT outputs, so check the ToS), and FIRST
  (single-token decoding, ≈50% faster).
- **2025 empirical study** (EMNLP Findings; [arXiv 2508.16757](https://arxiv.org/abs/2508.16757)), 22 methods:
  - Listwise 7B LLMs are best, but costly.
  - **Lightweight fine-tuned re-rankers give the best accuracy/efficiency trade-off.**
  - LLM re-rankers generalise inconsistently to unseen (novel) queries.
  - [reported]

**On our box** [inference]:

- A local 7B model on 2 vCPU takes tens of seconds. Infeasible.
- API listwise re-ranking of the top 10–20 articles uses about 20 × 300 = 6k input tokens and ~100 output tokens per
  query, at 1–3 s latency.
- Suitable only as an opt-in "deep search" mode, an offline teacher (#4), or an eval judge (§8).

### 4.4 Cascades and early exit: best quality per CPU-ms

Recommended order [inference, supported by Petrov 2024 and the Ettin CPU numbers]:

1. **Fuse dense and BM25F first.** It is nearly free and improves recall *before* re-ranking. A re-ranker cannot
   recover missed documents.
2. **Re-rank with a ≤32M distilled cross-encoder** at depth 20–30, on a short input: query + title + best chunk
   (≤192 tokens). Truncating the input is the biggest CPU lever: attention cost grows with length squared, and the
   rest of the model linearly.
3. **Skip re-ranking** when fusion is confident: large top-1 margin, with BM25 and dense agreeing on the top-3.
4. **Optionally**, a second-stage 68M–110M model on the top 5 only.

Other levers:

- Run batches through `ort` with int8 dynamic quantisation, and set intra-op threads to 2.
- Expect int8 to cost ≤1 point and give a 2–3× speed-up. [inference, typical for BERT-class ONNX]

---

## 5. Query-side techniques

**HyDE** (Gao et al., ACL 2023; [arXiv 2212.10496](https://arxiv.org/abs/2212.10496)):

- An LLM writes a hypothetical answer passage, and the passage is embedded.
- Big gains for *unsupervised* retrievers (Contriever); much smaller for trained ones.
- Costs about 100–200 output tokens per query, roughly 1–3 s.

**query2doc** (Wang, Yang & Wei, EMNLP 2023; [arXiv 2303.07678](https://arxiv.org/abs/2303.07678)): appending a
pseudo-document gives +3–15% for BM25 on MS MARCO and TREC DL, and smaller gains for fine-tuned dense retrievers.
[reported]

**Prompting choice:** Jagerman et al. 2023, "Query expansion by prompting LLMs"
([arXiv 2305.03653](https://arxiv.org/abs/2305.03653)). Chain-of-thought prompts work best for expansion. [reported]

**When expansion fails:**

- **Weller et al.** (Findings EACL 2024; [arXiv 2309.08541](https://arxiv.org/abs/2309.08541)), 11 methods × 24
  retrievers: **a strong negative correlation between retriever strength and expansion gain.** Expansion helps weak
  models and hurts strong ones, because it adds noise that blurs the top ranks.
- **Abe et al.** (SIGIR 2025; [arXiv 2505.12694](https://arxiv.org/abs/2505.12694)): LLM expansion fails on queries
  the LLM is unfamiliar with, and on ambiguous ones. It favours the popular reading of a query.

**Fit with PigMed:**

- Our Sonnet rewriting gains came entirely from lay queries [PigMed], which fits both papers. EG2 is strong on expert
  phrasing; lay phrasing is effectively a vocabulary shift.
- **Route** rewriting to lay or underspecified queries only. Detect them with:
  - consumer-health vocabulary (CHV) hits;
  - no MeSH or entity matches;
  - question form;
  - a small classifier.
- **Test a cheaper model** (Haiku-class) against Sonnet, and **cache** rewrites.
- **Stream** results: show the dense results immediately, then merge in the rewrite results.

**Multi-query + RRF** ("RAG-Fusion"): generate 2–4 paraphrases and fuse the result lists. It is popular in practice,
but controlled evidence is [weak]. Costs one LLM call plus N searches. N extra searches are cheap here, at ~5–20 ms
each [inference].

**Dictionary expansion (zero latency):** lay term → MeSH entry term (e.g. "heart attack" → myocardial infarction).
Apply it to the lexical leg only. Uses MeSH entry terms, plus CHV if UMLS licensing allows.

**Pseudo-relevance feedback:**

- RM3 on BM25 is a classic small gain.
- Vector PRF, q' = q + β·mean(top-k), costs about one extra search.
- Li et al. (TOIS 2023, "Pseudo-relevance feedback with deep language models and dense retrievers: successes and
  pitfalls") found the gains inconsistent with strong dense models. [reported; weak for our case]

---

## 6. Document-side techniques

**doc2query / docT5query:**

- On BEIR it helps TREC-COVID (0.713 vs BM25 0.656) and hurts BioASQ (0.431 vs 0.465) [reported, MedCPT Table 1].
- **Doc2Query--** (Gospodinov, MacAvaney & Macdonald, ECIR 2023; [arXiv 2301.03266](https://arxiv.org/abs/2301.03266))
  filters hallucinated expansions. It improves effectiveness *and* shrinks the index.
- At 251M chunks, generation costs about 10¹⁰ output tokens, and the index grows. The common T5 models are MS MARCO
  trained (flag).
- **Not recommended at full scale.** It might be worth trying at article level for BM25F, with a clean generator.
  [inference]

**Contextual chunk prefixes** (Anthropic, Sep 2024, [post](https://www.anthropic.com/news/contextual-retrieval)):

- An LLM writes a 50–100-token context for each chunk, which is prepended before embedding and BM25 indexing.
- Top-20 retrieval failure went from 5.7% to:
  - 3.7% with contextual embeddings (−35%);
  - 2.9% adding contextual BM25 (−49%);
  - 1.9% adding a re-ranker (−67%).
- [reported, vendor benchmark, not independently replicated]

**At PigMed scale** [inference]:

- About 251M × ~80 output tokens ≈ 20B tokens. Infeasible by API.
- On cluster GPUs with a non-Chinese open generator (Gemma, Llama, OLMo, Mistral), it is hundreds to low thousands of
  GPU-hours.
- **Cheaper first:** a deterministic prefix of title + section path (which we already have) + MeSH/keywords. Then run
  the LLM prefix on the 98k test subset only, to measure the headroom.

**Late chunking and contextual chunk embeddings:**

- Late chunking: Günther et al. 2024, [arXiv 2409.04701](https://arxiv.org/abs/2409.04701). It gives gains of a few
  points on BEIR subsets with long documents. [reported]
- ConTEB / InSeNT: Conti et al. 2025, [arXiv 2505.24782](https://arxiv.org/abs/2505.24782).
- pplx-embed-context, already in our training plan.
- We already mix late chunking at 0.5. The remaining lever is *training* the encoder for contextual chunks; see
  `RETRIEVER_TRAINING_PLAN.md`.
- Contextual Document Embeddings (Morris & Rush 2024, [arXiv 2410.02525](https://arxiv.org/abs/2410.02525)) condition
  on corpus neighbours. Interesting, but needs model training.

**Multi-granularity scoring:**

- Passage-to-document aggregation: MaxP, FirstP and SumP (Dai & Callan, SIGIR 2019). MaxP is the usual robust default.
- PARADE (Li et al. 2020) learns the aggregation.
- For PigMed: combine the article vector, the max chunk and a soft top-3 chunk mean (and BM25F), with the weights
  learned in the LTR step (#5, #8).
- A known pitfall: pure max favours long articles with many chunks. Normalise, or use the top-k mean. [inference]

---

## 7. Efficiency at 250M scale on CPU

### Storage arithmetic for 251M chunks × 768-d [inference]

| Format | Size |
|---|---|
| fp32 | 771 GB |
| int8 | 193 GB |
| binary | 24 GB |
| PQ-96 B | 24 GB |

For 9.35M article vectors:

| Format | Size |
|---|---|
| int8, 768-d | 7.2 GB |
| int8, MRL-256 | 2.4 GB |
| binary, 768-bit | 0.9 GB |

An HNSW graph at M=32 adds about 1.2 GB.

### Space limits

The free Space is 2 vCPU, 16 GB RAM and **50 GB non-persistent disk**: the index is re-downloaded on every restart, and
the Space sleeps after 48 h idle.

Recent community reports say CPU Basic for compute Spaces may now need a paid plan
([HF docs](https://huggingface.co/docs/hub/en/spaces-overview);
[forum](https://discuss.huggingface.co/t/official-community-complaint-revert-free-cpu-basic-spaces-and-remove-anti-developer-sdk-restrictions/177703)).
**Verify before committing to the box.**

### Quantisation with rescoring

From the [HF embedding-quantization blog](https://huggingface.co/blog/embedding-quantization), mxbai-embed-large on MTEB
retrieval [reported]:

| Method | Retained | Speed-up |
|---|---|---|
| Binary | 92.5% | 25× mean |
| Binary + float rescoring | 96.5% | |
| int8 | 97% | 3.7× |
| int8 + rescoring (4× oversample) | ≈99% | |

Binary retention is **model-dependent**: e5-base-v2 keeps only 74.8%. **Measure EG2 binary retention** before relying on
it.

### Matryoshka

EmbeddingGemma (v1) on MTEB English v2 [reported,
[model card](https://huggingface.co/google/embeddinggemma-300m)]:

| Dimensions | Score |
|---|---|
| 768 | 69.67 |
| 512 | 69.18 |
| 256 | 68.37 |
| 128 | 66.66 |

QAT Q8 scores 69.49, against 69.67 at full precision.

Pattern [inference; verify on EG2]:

- Traverse HNSW with 256-d int8 or binary.
- Rescore with 768-d int8.
- Encode the query with the QAT int8 encoder.

**Query encoding is likely our largest fixed CPU cost**: an EG2 text tower of 271M parameters, at roughly 30–80 ms per
short query on 2 vCPU [inference]. Profile it.

**RaBitQ** (Gao & Long, SIGMOD 2024) and extended RaBitQ give binary-like codes with error bounds. They beat PQ at
equal bits in their reports. [reported] The method is reusable.

### Index families

- **HNSW:** best recall/latency in RAM. Graph memory is the limit, which is why we route at article level (9.35M).
  This is the right call.
- **IVF-PQ:** smallest RAM; recall drops without rescoring. LanceDB is Rust-native.
- **DiskANN / Vamana** (Subramanya et al., NeurIPS 2019): SSD-resident graph with PQ in RAM. >95% recall@1 at
  milliseconds for 1B points on a 64 GB box [reported]. Microsoft's repo main branch is being rewritten in Rust
  ([repo](https://github.com/microsoft/DiskANN)).
- **SPANN** (Microsoft, NeurIPS 2021): posting lists on disk.
- **Our two-level design** (article HNSW → chunk PQ → int8 exact) is effectively a hand-built SPANN or IVF. It is
  sound. [inference]

Rust options:

- `usearch` (Apache-2.0; C++ core with Rust bindings; int8 and binary).
- `hnsw_rs`.
- Qdrant's segment crates.
- `lancedb`.
- `seismic` and `next-plaid`.

### Caching

Query logs are Zipfian, so cache [inference]:

- query → embedding (LRU);
- query → final results, with a TTL;
- LLM rewrites;
- the mmap pages of the PQ codes for hot articles.

Precompute embeddings for the top-N frequent queries.

---

## 8. Evaluation pitfalls

**LLM-generated queries favour dense / LLM-based systems:**

- **Rahmani et al.** (CIKM 2025; [arXiv 2506.10301](https://arxiv.org/abs/2506.10301)):
  - Synthetic queries are longer than real ones.
  - Synthetic judgments favour LLM-based systems.
  - The bias matters for absolute scores more than for system rankings.
  - [reported]
- **Source bias** (Dai et al., KDD 2024): neural retrievers prefer LLM-written text.
- **TREC 2025 Tip-of-the-Tongue** participants reported a large gap between LLM-simulated dev queries and the real
  test queries ([UAmsterdam](https://trec.nist.gov/pubs/trec34/papers/UAmsterdam.tot.pdf)).
- **Our 0.771 MRR set is LLM-generated.** It is probably in-distribution for EG2, which was trained heavily on
  synthetic queries. That understates BM25 and entity routing. [inference]

**Pooling bias:** TREC-COVID judgments came from pools of mostly lexical systems. The MedCPT authors attribute BM25 plus
re-ranker's TREC-COVID lead to annotation bias. Unjudged documents count as non-relevant, so new dense systems are
penalised. Report judged@10 and condensed-list metrics. [reported, BEIR]

**Real user queries are short and navigational:** PubMed log analyses show many 1–3-word queries and author, journal and
gene lookups (Islamaj Doğan et al., Database 2009). A natural-language synthetic set misses this. [reported, older]

**Eval sets to build** (all for evaluation only; flag licences if they are redistributed):

- **Keyword set:** sample MeSH major topics and title noun phrases → short queries, with the source article plus
  MeSH-matched articles as relevant.
- **Entity and ID set:** gene symbol, drug, rsID, NCT, PMID and DOI → the expected articles. Exact-match ground truth.
- **Lay set:** NFCorpus (lay NutritionFacts queries), CLEF eHealth consumer-health search, and our lay rewrites.
- **Real expert queries:**
  - BioASQ (needs a BioASQ account, research terms);
  - TREC-COVID, TREC CDS and TREC PM;
  - RELISH (article-to-article).
- **PigMed live logs**, once launched, with consent and anonymisation.

**Method:**

- Report per-type metrics and **routing recall@100** (the ceiling) separately from final MRR or nDCG.
- Use paired significance tests (bootstrap or randomisation).
- Tune α and the LTR weights on dev, never on test.
- When using LLM judges to fill holes, spot-check them against humans. LLM judges can be misled by lexical overlap
  (see [arXiv 2601.01751](https://arxiv.org/abs/2601.01751)).

---

## Source list (main)

- Bruch, Gai, Ingber. An analysis of fusion functions for hybrid retrieval. TOIS 2023. https://arxiv.org/abs/2210.11934
- Cormack, Clarke, Büttcher. Reciprocal rank fusion. SIGIR 2009.
- Hsu, Tzeng. DAT: Dynamic alpha tuning. 2025. https://arxiv.org/abs/2503.23013
- Kamphuis et al. Which BM25 do you mean? ECIR 2020. https://cs.uwaterloo.ca/~jimmylin/publications/978-3-030-45442-5_4.pdf
- Robertson, Zaragoza, Taylor. Simple BM25 extension to multiple weighted fields. CIKM 2004.
- Jiang, Zhai. Tokenization strategies for biomedical IR. Information Retrieval 2007.
- Lu, Kim, Wilbur. Evaluation of query expansion using MeSH in PubMed. 2009. https://pmc.ncbi.nlm.nih.gov/articles/PMC2747526/
- Fiorini et al. Best Match: PubMed relevance search. PLoS Biol 2018. https://pmc.ncbi.nlm.nih.gov/articles/PMC6112631/
- Jin et al. MedCPT. Bioinformatics 2023. https://arxiv.org/abs/2307.00589 ; https://huggingface.co/ncbi/MedCPT-Cross-Encoder
- Lassance et al. SPLADE-v3. 2024. https://arxiv.org/abs/2403.06789
- OpenSearch team. Inference-free learned sparse retrievers. 2024. https://arxiv.org/abs/2411.04403
- Bruch et al. Seismic. SIGIR 2024. https://arxiv.org/abs/2404.18812 ; https://github.com/TusKANNy/seismic
- Linkup SPARSEUP (Sep 2026). https://www.marktechpost.com/2026/09/19/linkup-research-releases-sparseup/
- LightOn LateOn / DenseOn (Apr 2026). https://lighton.ai/lighton-blogs/the-retriever-you-actually-need
- Ettin rerankers. https://huggingface.co/blog/ettin-reranker
- Petrov, MacAvaney, Macdonald. Shallow cross-encoders. ECIR 2024. https://arxiv.org/abs/2403.20222
- Schlatt et al. Rank-DistiLLM. 2024. https://arxiv.org/abs/2405.07920
- Sun et al. RankGPT. EMNLP 2023. https://arxiv.org/abs/2304.09542
- How good are LLM-based rerankers? EMNLP Findings 2025. https://arxiv.org/abs/2508.16757
- Google Research (Jayaram, Dhulipala et al.). MUVERA. NeurIPS 2024. https://arxiv.org/abs/2405.19504
- next-plaid (Rust PLAID). https://docs.rs/next-plaid
- Gao et al. HyDE. ACL 2023. https://arxiv.org/abs/2212.10496
- Wang, Yang, Wei. query2doc. EMNLP 2023. https://arxiv.org/abs/2303.07678
- Jagerman et al. Query expansion by prompting LLMs. 2023. https://arxiv.org/abs/2305.03653
- Weller et al. When do generative query and document expansions fail? Findings EACL 2024. https://arxiv.org/abs/2309.08541
- Abe et al. LLM-based query expansion fails for unfamiliar and ambiguous queries. SIGIR 2025. https://arxiv.org/abs/2505.12694
- Gospodinov et al. Doc2Query--. ECIR 2023. https://arxiv.org/abs/2301.03266
- Anthropic. Contextual retrieval. 2024. https://www.anthropic.com/news/contextual-retrieval
- Günther et al. Late chunking. 2024. https://arxiv.org/abs/2409.04701
- Conti et al. ConTEB. 2025. https://arxiv.org/abs/2505.24782
- HF blog. Embedding quantization. https://huggingface.co/blog/embedding-quantization
- EmbeddingGemma model card (MRL, QAT tables). https://huggingface.co/google/embeddinggemma-300m
- Microsoft DiskANN (Rust rewrite on main). https://github.com/microsoft/DiskANN
- Rahmani et al. Synthetic test collections. CIKM 2025. https://arxiv.org/abs/2506.10301
- Sciavolino et al. Entity-centric questions challenge dense retrievers. EMNLP 2021.
- Arabzadeh, Yan, Clarke. Dense vs sparse strategy selection. CIKM 2021.
