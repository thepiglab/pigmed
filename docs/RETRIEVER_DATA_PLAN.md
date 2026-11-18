# Retriever training data: sources, splits, long context

Status: plan plus first measurements (2026-11-15). Companion to `RETRIEVER_TRAINING_PLAN.md`. The split code and
the 98k-corpus census are built in Rust (`rust/pigmed-splits`) (see "PMC splits" and "Measured on the 98k corpus"). Figures marked *reported* come
from the cited papers and cards; *measured* figures are ours. Licence tags: **V** = verified on the official card,
page or paper (2026-11-14); **U** = unverified (inferred, secondary source, or no licence stated). The full
provenance audit, with quotes and URLs, is kept outside the repo; the tables below are its summary.

## Decisions

Settled (2026-11-14):

1. **Release terms.** The released model must be usable by anyone for anything, so every training source must allow
   commercial use and redistribution.
2. **PMC:** commercial-use subset only (CC0, CC BY, CC BY-SA, CC BY-ND) in the released model. The non-commercial
   subset (CC BY-NC*, PMC "TDM", unknown) is for internal ablations only.
3. **MS MARCO is out.** Its terms say "intended for non-commercial research purposes only" (**V**,
   microsoft.github.io/msmarco). Every derivative goes with it (LightOn `msmarco` splits, `rerank_msmarco`, the
   MIT-tagged `msmarco-decontaminated`, MS MARCO-trained cross-encoders).
4. **MLDR** (MIT, GPT-3.5 queries, BAAI): allowed, used cautiously (long-document coverage only, small share).
5. **Scope:** biomedical-first with general replay.
6. **No Chinese-origin model weights anywhere** (base, teacher, generator, miner, filter). Text published by Chinese
   organisations is fine only if no Chinese model generated, filtered, scored or selected it.

Open:

7. **Synthetic-query generator: benchmarked, final pick pending (2026-11-16).**
   - The generator must be multimodal: the retriever is a multimodal EG2 and needs figure queries. Lightning
     (text-only) was dropped.
   - Candidates: Nemotron 3.5 Super VL, Bolt 74B, Gemma 4 26B/31B and Muse Glimmer 30B, at several precisions and
     drafters.
   - Benchmark summary:
     - Gemma 4 26B NVFP4 for text throughput;
     - Glimmer for quality, especially figures;
     - a custom Glimmer runtime was being explored.
8. **Lineage rule scope.** Rule 6 is about weights. Nemotron 3.5 Super VL has NVIDIA weights, but much of its
   synthetic training data came from Qwen, DeepSeek, Kimi and GLM models (below). It was cleared explicitly.
   Inkling 2 Small (Kimi-generated data) is the same kind of case and has not been cleared.
9. **ShareAlike.** The clean general sets (NQ, HotpotQA, SQuAD v2, FEVER, PAQ, DBpedia) are CC BY-SA. Redistributing
   derived *datasets* must stay BY-SA; whether BY-SA reaches trained weights is legally unsettled. Needs counsel if
   we publish training data.

## Biomedical or general

**Recommendation: biomedical-first, general-safe.** Aim at biomedical literature and clinical guidelines, keep a
general replay share in every stage, and gate every stage on a general benchmark.

Why:

- **EG2 is already general.** Out-training Google on general retrieval needs Google-scale data. A biomedical,
  contextual, multi-vector EG2 does not exist and is the public-good gap.
- **Domain tuning helps, but general models are close.**
  - On NFCorpus, SciFact, SciDocs and TREC-COVID, MedCPT (biomedical, 255M PubMed click pairs) averages 0.471
    nDCG@10. General E5-Large-v2 averages 0.491 and BGE-Large 0.511. BMRetriever-7B, biomedical-tuned, averages
    0.551 (BMRetriever Table 2, *reported*).
  - So biomedical tuning pays off when it starts from a strong general model, which EG2 is. It does not pay off as
    a replacement for one.
- **Forgetting is real without replay.** ConTEB's contextual training lifted ConTEB from 52.0 to 75.6, but NanoBEIR
  fell from 63.2 to 60.4 (ModernBERT). ModernColBERT fell from 67.7 to 59.2. The authors suggest replay or model
  merging but did not test it (*reported*).
- **Real queries are not paper-speak.** Clinician and patient questions (TREC CDS, iCliniq, MedQuAD, guidelines)
  need general language skill.

No paper reports a measured general/domain mixing ratio, so we measure it. Mixing ratios ("rings"):

| Ring | Share (start, then sweep) | Sources |
|---|---|---|
| Core biomedical | ~60% | our PMC full text, PubMed abstracts, clinical guidelines |
| Adjacent science | ~10% | peS2o / S2ORC, arXiv full text (licence per paper) |
| General replay | ~30% | NQ, HotpotQA, SQuAD v2, FEVER, MIRACL-en, PAQ, DBpedia-entity (all CC BY-SA or Apache, **V**); FineWeb-Edu and FinePDFs long documents (ODC-BY); MLDR-en (small share) |

- **Sweep:** general share 0 / 15 / 30 / 50% on a 1-GPU pilot. Score the biomedical eval and NanoBEIR, and pick the
  knee.
- **Model soup as a fallback:** EmbeddingGemma itself averages runs trained on different mixtures. If replay alone
  cannot hold NanoBEIR, average the biomedical checkpoint with the S1 general checkpoint and re-gate.
- **Batches are single-source**, as in EmbeddingGemma, so in-batch negatives are not trivially easy cross-domain
  ones.
- **The S1 checkpoint is a general multi-vector EG2** in its own right, trained mostly on general labelled data.
  If it passes its gate it is worth releasing on its own.

## Sources

### Our own data (the core; licence known per row)

| Source | What we get | Pairs |
|---|---|---|
| PMC full text (98k now, ~9.5M later) | Natively long documents (median ~4,870 words, ~6.3k tokens), sections, figures, tables, references | listed below |
| PubMed abstracts | ~37M title+abstract records | title → abstract; citation context → cited abstract |
| Clinical guidelines (`pigmed-rs guidelines` adapters) | Long recommendation documents with evidence citations | recommendation → cited evidence; question → recommendation |

**Free supervision, no LLM needed.** All of these come from the JATS we already parse:

- **Abstract sentence → body chunk.** Already our eval task, so it needs the split below.
- **Citing sentence → cited article.** References carry PMID or DOI. This should be the largest set: tens of
  millions of pairs at full scale.
- **"Fig. N" sentence → figure, and caption → figure.** About 343k figures in the 98k sample, ~30M corpus-wide.
  Hard negatives are the article's other figures.
- **Question-form titles → abstract conclusion.** This is the PubMedQA construction. These are real natural-language
  questions.
- **Section heading + article title → section.** A weak positive, best used for the contextual objective.
- **Guideline recommendation → the trials it cites.**

**LLM synthetic queries** (S2 contextual; see "Synthetic queries"):

- One to three queries per chunk on a subset of training articles.
- Include *contextual* queries: ones that name the document's entity even though the target chunk does not (the
  dose paragraph never says the drug name).

### External text sets

**Finding (V): nothing LightOn released can be used as-is.** Every score, negative list, curated subset and trained
model in the DenseOn/LateOn family and its descendants sits downstream of a Chinese-origin model:

| Model in their pipeline | Origin | Role | Evidence |
|---|---|---|---|
| mxbai-rerank-large-v2 | Mixedbread (DE) on **Qwen2.5-1.5B** | filter of the curated pre-training set (`similarity >= 3.0`); `rerank_scores` in every `embeddings-fine-tuning-filtered-*`; KD teacher of mDenseOn/mLateOn; sole teacher of the Ettin rerankers and MICE | config `Qwen2ForCausalLM`, vocab 151,936; Mixedbread blog "RL-optimized Qwen-2.5" (**V**) |
| gte-modernbert-base | **Alibaba** | hard-negative miner of `embeddings-fine-tuning` (2,048 candidates/query); candidate pools of the Ettin reranker data | fine-tuning card (**V**) |
| snowflake-arctic-embed-l-v2.0 | Snowflake, "builds on **BAAI/bge-m3**-retromae" | miner for MIRACL and MLDR in the filtered sets | arctic card (**V**) |
| Qwen3-32B | **Alibaba** | translator of all multilingual fine-tuning sets | paper, mDenseOn blog (**V**) |
| mGTE "MTP" mixture | Alibaba recipe | `mtp` pre-training subset (98 GB), no card | provenance unknown (**U**) |

So the tweet's claim ("SOTA for free") holds for the *text*: the queries and positives LightOn gathered are reusable
once we take them from the original sources, drop every LightOn score and negative, and re-mine with EG2.

**Datasets (LightOn and descendants)**

| Set | Licence (where) | Commercial + redistribution | Generator / miner / filter / teacher | Size and length | Use |
|---|---|---|---|---|---|
| lightonai/embeddings-pre-training | none in YAML; card: "Each source in this dataset retains its original license" (**V**) | per source; several sources NC | rule filters (FastText LID, Web 1T unigrams), MD5 dedup; `similarity` column = mxbai-v2 score | 2.05 TB, ~1.4B pairs, mostly short | raw text of clean sources only (list below) |
| lightonai/embeddings-pre-training-curated (+ multilingual) | card: "This dataset is not openly licensed" (**V**) | per source | **row selection by mxbai-v2 (Qwen)**; multilingual seed translated by Mistral-Small-3.1 | 665M pairs EN; 2.8B multilingual | **no** |
| lightonai/embeddings-fine-tuning | no licence field or text (**V**) | per source; includes MS MARCO (NC), TriviaQA and FiQA (unclear) | **negatives mined by Alibaba gte-modernbert-base** | 1.22M queries, 50.6M docs | queries + positive ids only, clean sources only |
| lightonai/embeddings-fine-tuning-filtered-en | no licence field (**V**) | per source, + MIRACL (Apache-2.0), MLDR (MIT) | gte-modernbert miner; arctic (BGE-M3) miner for MIRACL/MLDR; **all rows carry mxbai-v2 `rerank_scores`** | 1.00M queries, 8.5M docs. Median doc tokens: msmarco 69, nq 48, hotpotqa 70, fever 142, squadv2 147, fiqa 146, miracl 22, **mldr 3,921** | **no** (use upstream text) |
| lightonai/embeddings-fine-tuning-filtered-{fr,de,…} | no licence field (**U** per card) | per source | **translated by Qwen3-32B** | 16.3M samples incl. code | **no** |
| cross-encoder/ettin-reranker-v1-data, lightonai-embeddings-fine-tuning-reranked-v1 | YAML apache-2.0 (**V**), cannot override MS MARCO / GooAQ NC sources | no as a whole | **every label is mxbai-v2** | 143M triples | **no** |

**Models (LightOn and descendants)**: all **disqualified** under rule 6.

| Model | Licence | Base | Why out |
|---|---|---|---|
| DenseOn / LateOn (149M) | Apache-2.0 (**V**) | ModernBERT-base | pre-trained on mxbai-v2-filtered data; fine-tuned on Alibaba-mined negatives |
| mDenseOn / mLateOn (307M) | Apache-2.0 (**V**) | mmBERT-base | as above, plus Qwen3-32B translations and mxbai-v2 KD teacher |
| cross-encoder/ettin-reranker-{17m…1b}-v1 | Apache-2.0 (**V**) | jhu-clsp/ettin-encoder (MIT) | **pointwise MSE on mxbai-rerank-large-v2 logits** (blog, card, data card) |
| xpmir/Mice-P-61m, -130m (MICE, arXiv 2610.31932) | Apache-2.0 tag; repo has no licence file (**V**) | Ettin-68m; Ettin-reranker-150m | "MSE on mxbai-rerank-large-v2"; 130m starts from a disqualified checkpoint |
| Linkup SparseUp (linkup-sparseup-embed-v1) | Apache-2.0 (**V**) | LateOn-unsupervised | mxbai-v2-filtered pre-training; Alibaba-mined negatives |
| xpmir/cross-encoder-ettin-*-{BCE,…} | Apache-2.0 (**V**) | Ettin | trained on MS MARCO (NC) |
| mxbai-rerank-large-v2 / base-v2 | Apache-2.0 (**V**) | **Qwen2** (**V**) | Chinese base |
| mxbai-rerank-large-v1 | Apache-2.0 (**V**) | DeBERTa-v2 arch (DeBERTa-v3-large shape; checkpoint unnamed, **U**) | labels from an **unnamed LLM** over undisclosed "real-life search queries": not certifiable |

**What replaces MS MARCO.** Upstream text only, re-mined with EG2 (NV-Retriever rule: top 2,048, drop candidates
above 0.95 × positive score, sample 7):

| Set | Licence (where) | Comm. | Text provenance | Size, length | Use |
|---|---|---|---|---|---|
| Natural Questions | cc-by-sa-3.0 (HF card, **V**) | yes, BY-SA | human queries | ~307k queries; Wikipedia passages, median ~48 tokens | S1 |
| HotpotQA | cc-by-sa-4.0 (**V**) | yes, BY-SA | human | ~85k; median ~70 | S1, multi-hop |
| SQuAD v2 | cc-by-sa-4.0 (**V**) | yes, BY-SA | human | ~130k queries over 19k docs | S1 |
| FEVER | cc-by-sa-3.0 + gpl-3.0 tags (**V**; which covers what **U**) | yes, BY-SA | human claims | ~110k; median ~142 | S1 |
| MIRACL-en | apache-2.0 (**V**) | yes | human | 2.6k | S1 |
| PAQ | data "CC-BY-SA" (GitHub, **V**) | yes, BY-SA | questions generated by Meta BART | ~65M QA pairs | S1 weak/replay |
| DBpedia-entity | cc-by-sa-4.0 (**V**) | yes | human | small | S1 |
| S2ORC (title/abstract/citation) | ODC-By 1.0 (allenai/s2orc, **V**) | yes | — | ~40M papers | adjacent-science ring |
| FineWeb-Edu (raw `fw_edu`, not the curated top third) | odc-by (**V**) | yes | classifier is Llama-based (**U**) | 1.5B docs | general long-document replay |
| MLDR-en | mit (**V**) | yes | **queries by GPT-3.5** | 10k train queries; median doc 3,921 tokens | long docs, small share (decision 4) |
| ConTEB train (illuin) | CC0 paper (**U** data) | — | GPT-4o queries | 9.9k docs, 307k queries | contextual pilot / eval |
| HuggingFaceFW/finepdfs | ODC-BY (**U**, card not fully read) | — | — | 100M+ docs | long PDFs for S2 |

**Excluded:** MS MARCO (NC); GooAQ ("should not be used for any commercial purposes", **V**); Stack Exchange as
repackaged by flax (cc-by-nc-sa-4.0; re-derive from the CC BY-SA dumps if wanted); TriviaQA and FiQA (licence
unclear); `mtp` (unknown); `hermes` / OpenHermes (GPT-4 outputs, no licence); Amazon, Yahoo, Quora, WikiHow, AG News,
Reddit subsets (no licence on cards, commonly research-only, **U**); anything in the curated or `filtered-*` sets.

None of the clean general sets is biomedical, and all but MLDR are short passages. **The real biomedical replacement
for MS MARCO is our own PMC commercial-subset supervision**: free pairs (below) plus filtered synthetic queries.

### Multimodal

- **Our own PMC figures first.** We hold the licence per row, the article context, and the citing paragraph. Third-party
  PMC-derived sets add little.
  - axiong/pmc_oa: no licence stated, and captions were split with ChatGPT.
  - ROCO, MedICaT, PMC-15M: licences per source (*verify*).
- **Pages:** vidore/colpali_train_set, 118k page/query pairs with queries from Claude-3 Sonnet and GPT-4V. Its
  academic subsets keep their own licences.
- **Video:** PMC supplementary video with captions (ours, 4.7k in the sample). MedVidQA is an eval candidate
  (*verify*).
- **Audio:** almost no paired biomedical audio exists. Use it only as an eval-scale probe until a source is found.

### Not for training

- **MIMIC.** The PhysioNet credentialed agreement and public weights do not mix. Eval only, if ever.
- **BioASQ.** Registration, and redistribution terms. Eval only.
- **MedCPT click logs.** Not public.
- **LongEmbed.** A benchmark, with no train split.
- Anything filtered, mined, scored, translated or generated with Qwen, DeepSeek, Yi, GLM, Kimi, MiniMax or BGE/GTE
  models, which rules out all LightOn scores, negatives and curated subsets (above).

## Cross-encoder teacher and query filter

No released reranker passes: Ettin rerankers and MICE are distillations of mxbai-v2 (Qwen); mxbai-v1's labeller is
undisclosed; MS MARCO cross-encoders are NC. Plan:

1. **Base:** `jhu-clsp/ettin-encoder-{150m,400m,1b}` (MIT, **V**; trained from scratch by JHU on open data; 8k
   context). Caveat (**U**): synthetic math/Flan slices in the later phases were not audited.
2. **Labels:** a non-Chinese LLM judge scored pointwise (the chosen generator, or a larger one for labels only), on
   the clean general sets plus PMC synthetic pairs and free pairs.
3. **Target:** the Ettin-reranker-1b ceiling (NanoNFCorpus 0.4485, NanoSciFact 0.7775, *reported*) shows what the
   architecture reaches with a strong teacher.
4. Until it exists, the synthetic-query filter is round-trip top-3 with base EG2 plus an LLM-judge relevance check.

## PMC splits (leakage-safe)

The unit of splitting is a **paper group**, never a chunk. `src/pigmed/evaluation.py` already hashes
`article_group(row)` (duplicate_cluster > PMCID > DOI > PMID) into train 96% / dev 2% / test 2%. What is missing is
making the group complete.

### 1. Identity graph

Union-find over these links:

- PMCID ↔ PMID ↔ DOI, using NCBI's PMC ID mapping file, so a PubMed abstract and its PMC full text land in the same
  group.
- Preprint ↔ published version, from JATS `related-article` links and the NIH Preprint Pilot.
- Corrections and errata → their target. Retraction notices → their target; flag retracted papers and drop them as
  positives.
- A guideline published as a PMC article joins its PMC group.

Today a row with only a DOI and a row with only a PMCID for the same paper hash to different splits. This step
fixes that.

**Built** in `rust/pigmed-splits` (`src/splits.rs` graph and assignment, `src/jats.rs` JATS metadata). Identity links are JATS `related-article` /
`related-object` types that mean the same paper or a notice about it: corrected-article, correction-forward,
retracted-article, retraction-forward, expression/object-of-concern, addendum, preprint, published-article,
updated-article, alt-language. Commentary, companion, reply, peer-review reports (editor-report, reviewed-article),
article-reference and series links (continues) are different papers and are only counted. Retracted papers
(`is_retracted`, PubMed "Retracted Publication") are flagged per group, to be dropped as positives. Not yet wired:
the NCBI PMC ID mapping file (needed once PubMed abstracts join the corpus) and the NIH Preprint Pilot list.

### 2. Near-duplicate clusters

Merge a group with every near-duplicate paper. Catches duplicate publication, translations, conference-abstract
supplements and versioned preprints that lack links.

- **Embedding signal:** e-means E3 dedup supplies unit-level edges (cos ≥ 0.97).
  - **Finding (measured):** E3's `duplicate_cluster.json` links two articles on any single unit pair, so template
    text (peer-review statements, data-availability and conflict sections) and shared logos chain unrelated papers:
    one component holds **5,595 articles**, and 14,754 articles sit in 2,431 groups. Used as-is, one hash decides
    the split of 5.7% of the corpus.
  - **Rebuilt** by an article-level rule, first in this workstream, now built into e-means' `dedup.py`, which
    writes `article_links.jsonl` (the input we use):
    - Drop units that match ≥ 5 other articles (templates), template sections, and supplement cards.
    - Link two articles on an Abstract match, or on ≥ min(3, ⌈0.2 × the smaller article's units⌉) distinct matched
      units (images: ≥ 2).
    - Result (e-means relink): 872 links, 1,114 articles in 405 groups, the **largest 16 articles**. The audit reports how many raw E3 pairs still cross
    train/held-out, as the residual-leakage estimate.
- **Lexical signal:** MinHash (128 permutations, 16 bands × 8 rows) on word 5-gram shingles of abstract + first
  non-template body section, estimated Jaccard ≥ 0.8, documents with ≥ 50 shingles. 1,369 pairs on the 98k sample
  (*measured*), but only 6 involve research articles: the rest are retraction-notice templates, which are excluded
  (notices group by identity links only).

### 3. Assignment

| Split | Rule | Use |
|---|---|---|
| `test_temporal` | the group's earliest publication date ≥ cutoff (e.g. 2026-07-01) | the honest test of new literature. Never trained on |
| `test_journal` | 50 journals held out entirely: hash-ordered, round-robin over strata, journals with ≥ 10 articles and ≤ 0.5% of the corpus (no mega-journals). Journals are ISSN + NLM-TA union-find components | out-of-venue generalisation |
| `test` / `dev` / `train` | hash 2 / 2 / 96%. A singleton keeps exactly the split `assign_split` gave it; a merged group takes its most held-out member's split, so growth can only move a group train → held-out | main in-distribution split |
| `eval_reserved` | groups containing any positive of an external benchmark (below). Highest priority | never a training query or positive |

Priority: `eval_reserved` > `test_temporal` > `test_journal` > hash. Strata are log2 journal size for now; subject
strata need the NLM Catalog broad subject terms (`select_test_journals` takes an optional strata map).

### 4. Pair rule

A training pair is allowed only if **both endpoints' groups are `train`**. This covers:

- query article and cited article for citation pairs;
- figure and article;
- guideline and evidence.

A test query must come from a test group. The retrieval corpus at eval time holds every split, which is
realistic: train documents may be retrieved, but test queries and positives were never trained on. Hard negatives
are mined from train only.

### 5. External benchmark decontamination

- **Document side.** Build a blocklist of benchmark positives and map each to PMID/DOI/PMCID. These groups become
  `eval_reserved`. Built (`pigmed-splits blocklist`): TREC CDS 2014–16 (PMC ids) and TREC PM 2017/2019
  abstracts (PMIDs), 23,226 positive judgments. Still to add (need id mapping): TREC-COVID (CORD-19 metadata),
  NFCorpus (raw release PubMed URLs), SciFact (S2 ids), BioASQ, MIRAGE snippets.
- **Query side.** Drop training queries, synthetic ones included, with 13-gram overlap or cosine ≥ 0.95 against any
  benchmark query.
- **Ablation.** Report each benchmark with and without the blocklist, so the contamination effect is measured
  rather than assumed.

### 6. Batch hygiene

- Mask in-batch false negatives where queries or positives are equal or share a group (EmbeddingGemma does this
  for exact duplicates).
- In-document negatives for the contextual loss keep all of an article's chunks in one group by construction.

### 7. Checks in code

`pigmed-splits audit` exits 1 if:

- any group spans two splits, or an article's split differs from its group's;
- any training pair touches a non-train group (checked on all citation pairs);
- any blocklisted id is outside `eval_reserved`;
- any near-duplicate link used for grouping crosses splits;
- with `--previous <manifest>`, any article moved from a held-out split into train.

`pigmed-splits build --previous <manifest>` prevents that last failure. Held-out is sticky: a group containing an
article that an already-used manifest held out keeps the strongest such split. Without it, a new near-duplicate
link can merge a recent paper with an older one, the group then loses `test_temporal`, and the paper becomes
trainable. The first Rust run caught exactly this case.

Warnings: groups over 200 articles (chaining), and `--loose-clusters` crossings (raw E3 pairs not used for
grouping). It writes split counts. Run it before every training job.

### 8. Result on the 98k corpus (*measured*, 2026-11-15)

Defaults: cutoff 2026-07-01; held-out journals until ~2% of the corpus (18 journals here); seed `pigmed-v1`.
Near-duplicate input: e-means `article_links.jsonl` plus MinHash. The manifest and audit report stay with the run
outputs on the cluster (not in git).

| Split | Articles | Commercial-use | Groups | Retracted (group flag) |
|---|---|---|---|---|
| train | 89,908 (91.9%) | 53,512 | 88,990 | 386 |
| dev | 1,923 | 1,165 | 1,834 | 28 |
| test | 2,078 | 1,197 | 1,971 | 15 |
| test_temporal | 1,611 | 1,069 | 1,608 | 9 |
| test_journal | 2,172 | 1,051 | 2,131 | 9 |
| eval_reserved | 159 | 80 | 153 | 2 |

- **Audit: OK**, with 0 failures. No group spans splits, no training pair touches a held-out group, every
  blocklisted id is reserved, no grouping link crosses splits, and no article moved from held-out to train against
  the earlier (Python) manifest. 9 articles moved the safe way (train → dev/test/test_temporal), because the
  e-means links are slightly looser.
- **Citation pairs** (citing article → cited id, all 3.84M resolvable references):
  - train: 3.38M;
  - dropped: 151k (train query with a held-out positive);
  - eval pairs: test 82k, dev 77k, test_journal 75k, test_temporal 72k.
- **Graph:** 6,540 identity links were used. 293 mass-notice links were skipped: one notice per retracted batch, and
  without the degree cap a single notice joined 76 papers. 6,750 soft links (commentary, peer review, companion)
  were only counted. 510 links were unresolvable.
- **Near-duplicate links:** 752 text and 110 image (e-means), plus 6 MinHash. The other 1,373 MinHash pairs were
  publisher retraction-notice templates. Hindawi notices alone made a 54-article group. Notices now join their
  targets through identity links only, never through text similarity. Largest group: 16 articles; 697 groups have
  more than one article.
- **Residual-leakage warning:** 4,199 of the 31,504 raw E3 pairs (old `duplicate_cluster.json`) that were *not*
  used cross train and held-out. That is 13%, below the ~20% that random pairs would show, so most rejected E3 pairs
  are probably not real duplicates, but a manual review of a sample is still open.

**Implementation and parity.**
- `rust/pigmed-splits` (binary `pigmed-splits`: `census`, `blocklist`, `build`, `audit`; 16 tests) replaces a first
  Python version, per the rule that PigMed code is Rust wherever possible.
- Checked against the Python run on the full 98k corpus:
  - On the same inputs, all 97,851 articles get the identical split and identical group members.
  - All 16 compared record fields (ids, ISSNs, dates, EG2 token counts, reference and citation counts, licence,
    article type) match for all 97,851 articles.
  - The hash split matches `evaluation.assign_split` exactly (fixture of 3,003 keys).
- Speed: the census parses all 98k cached JATS files with EG2 tokenisation in 42 s on 32 CPUs.
- Cluster build note: `rust-lld` intermittently dies with SIGBUS when linking onto a Lustre filesystem. Set
  `RUSTFLAGS="-C link-arg=-Wl,--no-mmap-output-file"`.

## Measured on the 98k corpus

One 32-CPU job (8 min): JATS for every article fetched from the public PMC bucket and parsed;
EG2 tokens counted with the EG2 tokenizer over title + abstract + body paragraphs and tables (what we index).
97,851 of 97,946 articles parsed; 95 older article versions are no longer in the bucket (404).

| | All | Commercial-use subset |
|---|---|---|
| Articles | 97,851 | **58,074 (59.3%)** |
| EG2 tokens, median / p90 / p99 | 6,607 / 13,112 / 25,553 | 7,136 / 13,488 / 25,482 |
| **Share over 8,192 tokens** | **35.1%** | **39.4%** |
| Share over 16,384 / 32,768 | 4.6% / 0.5% | 5.0% / 0.5% |
| References / with PMID, PMCID or DOI | 4.35M / 3.84M (88%) | 2.75M / 2.43M |
| Unique cited ids | 2.86M | 1.95M |
| Citation-context pairs (paragraph × resolvable cited ref) | 4.73M in 1.35M paragraphs | 3.05M |
| Question-form titles (with abstract) | 1,505 (1,149) | 762 (647) |
| Figures / in-text figure mentions | 378k / 1.12M | 242k / 711k |

Licences: CC BY 56,061; CC0 1,870; CC BY-ND 116; CC BY-SA 27 (commercial); CC BY-NC 10,519; CC BY-NC-ND 11,644;
CC BY-NC-SA 3,191; PMC "TDM" 10,319; unknown 4,102. Notices: 1,184 corrections, 190 retraction notices, 205
retracted articles; 531 preprint and 331 published-article links.

Consequences:

- **The 8k window is not enough for a third of PMC.** The earlier "median ~6.3k fits" holds for the median only.
  S2 needs the 8k windows with ~1k overlap and title + abstract prefix for 35–39% of articles; 32k would cover
  99.5%, so a later NTK-scaled 32k run is worth testing.
- **Citation pairs are the largest free set:** ~2.4M resolvable references per 58k commercial articles, so tens of
  millions at full scale, as expected. The positive is often outside the corpus (PubMed abstract), which the
  identity graph and pair rule already handle.
- **Question-form titles are rare (1.5%)**: ~150k at 9.5M articles. Useful as real questions, not as a main source.
- **Commercial subset is 59%**: ~5.6M of ~9.5M articles at full scale.

## Long context

- **PMC is the long-document source.** Measured: median 6.6k EG2 tokens, but 35% of articles exceed the 8,192
  window (39% of the commercial subset) and 0.5% exceed 32k. ConTEB showed that natively long documents are needed:
  concatenated short documents gave 60.2 vs 75.6 (*reported*).
  - Longer articles get 8k windows with ~1k overlap. Whether each window is prefixed with title + abstract is now
    an open question: in our chunking A/B (`docs/TEST_LADDER.md`), prompt prefixes of title and section path
    *hurt* untrained EG2 by 3–6 MRR points on abstract-sentence → body. Context should come from training (InSeNT)
    or late chunking. Test the prefix only inside S2 training, not zero-shot.
  - Zero-shot baselines (*measured*, TEST_LADDER): late chunking is −0.016 MRR vs plain chunks on our
    abstract → body task. On ConTEB, EG2 averages 0.530 naive vs 0.547 untrained late. It helps context-dependent
    sets (insurance +0.24, football +0.18, geography +0.04) and hurts self-contained ones (covid-qa −0.07, squad
    −0.09, narrative-qa −0.06). This matches ConTEB's own finding, and it is what S2 training must fix: keep the
    context gain without losing self-contained chunks.
- **Length mix by stage.**
  - S1 (late head) is mostly short pairs. The labelled sets are short.
  - S2 (contextual) is long by construction: one to four whole documents per device, every chunk pooled from one
    pass (ConTEB used 4 docs/device, 2 epochs, λ_seq 0.1, *reported*).
  - Length-bucketed batching keeps padding waste down. GradCache keeps 8k-token batches in memory.
- **General long documents for replay:** FinePDFs and FineWeb-Edu long tail, MLDR-en, arXiv full text, guidelines.
- **Beyond 8k.** No position extension is needed for v1. If we later want 32k, LongEmbed reports that training-free
  NTK/SelfExtend scaling is a cheap first step (E5-Mistral 64.4 → 75.3 at 32k with NTK, *reported*).
- **Short-text regression.** S2 must hold NanoBEIR and our short-query tasks within ~1 point (the gate in the
  training plan).
- **Eval:** ConTEB, LongEmbed and MLDR-en dev, plus our own long-document test (whole-article queries on
  `test_temporal`).

## Synthetic queries

### What complexity each query type needs

Evidence on generator size (*reported*; details and URLs kept outside this repository):

- **InPars** (MS MARCO MRR@10): 0.3B → 0.215, 1.2B → 0.252, 6B ≈ 0.26, 175B → 0.270. Gains saturate early.
- **Promptagator** re-run with open models ("Promptodile", 2510.02241): 3B–14B generators give 43.7–46.2 BEIR vs
  44.7 (unfiltered) / 46.0 (filtered) for FLAN-137B; only 1B collapses (39.6). Round-trip filtering adds ~2.5.
- **E5-Mistral** used GPT-4 data; SPEED (2410.18634) matched it with an aligned Llama-3-8B (63.4 vs 63.1 MTEB).
- **BMRetriever:** GPT-3.5 queries, round-trip filter kept 84% (420k of 500k); no generator ablation.
- **ConTEB:** GPT-4o with the document summary in the prompt; no generator ablation.
- **pplx-embed-context v1:** Qwen3-30B-A3B (~3B active) with neighbouring chunks in the prompt, and reached SOTA. v2
  names no generator and moves the work to a relevance teacher that grades every chunk.
- Filtering or LLM relabelling moves results by 1.3–4 points; generator size above ~3B (or ~3–4B active) by ≤ ~1.

| Query type | Needs | Size verdict |
|---|---|---|
| Keyword / short lexical | term choice | small is fine (≥ 1–3B); spend on filtering |
| Clinician question | specific, answerable from the chunk | ~3–8B active plus round-trip filter |
| Patient-language question | paraphrase into lay terms, no jargon leakage | same, plus a lexical-overlap cap (low BM25 overlap with the chunk) |
| Contextual (needs document context the chunk lacks) | read title/abstract/neighbours, name the entity the chunk omits | no ablation; ~4B-active MoE with neighbour-aware prompts; 1–2B risky. The relevance labeller matters more |
| Multi-hop / citation | combine two chunks or citing + cited | moderate evidence that bigger helps (+2.8 vs +1.5 diversity gain, 2602.09448); use the larger arm or a verifier |
| Figure | read the image or caption | needs image input (Gemma 4, Inkling) or caption-only text prompts |

### Main generator: NVIDIA Nemotron 3.5 Super VL (verified 2026-11-15; cleared)

| Check | Finding (**V**: model card, config, HF API) |
|---|---|
| Repo | `nvidia/NVIDIA-Nemotron-3.5-Super-VL-120B-A12B-NVFP4`, revision `3acb09951cff`, not gated. Also BF16 and Base-BF16 repos |
| Size | 120B total / 12B active; hybrid Mamba-2 + MoE (512 routed experts per MoE layer) + attention; shared MTP head; up to 1M context |
| Components | LLM backbone initialised from Nemotron 3 Super Base (NVIDIA), then 1T tokens distilled from Nemotron 3 Ultra Base; **C-RADIO** vision encoder (NVIDIA, `model_type: radio`, ViT-H/16); NVIDIA connector. No Qwen base, no InternViT |
| Training-data lineage | The card's own tables list Qwen3/3.5, DeepSeek-V3.2/V4/R1, Kimi K2/K3, GLM-4.x/5.x and MiniMax as generators for **57 of 70** pre-training synthetic sets and **63 of 109** post-training synthetic sets (gpt-oss, phi-4, Gemma 4, GPT-5 and Gemini for others) |
| Licence | OpenMDW-1.1 (permissive open model licence). No clause restricts using outputs to train other models |
| Fit on one GPU | NVFP4 75 GB: the card lists 1× RTX PRO 6000, 1× H200, 1× GB200 and 1× DGX Spark, so one B200 (192 GB) fits with ample KV cache. BF16 is 249 GB and needs 4× H100 or 2× GB200 |
| Serving | vLLM v0.32.0 (Blackwell, NVFP4), SGLang, TensorRT-LLM. Reasoning off via the chat template (`enable_thinking=False`); recommended text sampling temperature 1.0, top_p 0.95 |

### Generator candidates (verified 2026-11-14)

| Model | Origin | Total / active | Licence | Output use for an open release | Notes |
|---|---|---|---|---|---|
| **Gemma 4 26B-A4B-it** | Google | 25.2B / 3.8B MoE, 256K ctx, image input | plain Apache 2.0 (Gemma 4 dropped the old Gemma terms, **V**) | yes | fast; can write figure queries |
| **gpt-oss-120b** / 20b | OpenAI | 117B / 5.1B; 21B / 3.6B MoE | Apache 2.0 (**V**) | yes | reasoning tokens add output; fits one GPU |
| Inkling 2 Small | Thinking Machines Lab (US) | 594B / 17B MoE, image input | Apache 2.0 + use policy (no output clause, **V**) | yes | weights from scratch, **but training data includes Kimi K2.5/K3 outputs** (decision 8) |
| LFM3-1.7B | Liquid AI (US) | 1.7B **dense**; no LFM3 MoE exists (Liquid MoEs: LFM2-8B-A1B, LFM2.5-24B-A2B) | LFM Open v1.0: commercial use above $10M revenue not licensed; non-profit research exempt (**V**) | probably; needs UF counsel | floor arm only |
| Meta Muse Glimmer | Meta (US) | 29.6B **dense**, not MoE | Apache 2.0 + usage policy (**V**) | yes | 5–8× the compute per token of a 4B-active MoE |
| Mistral Small 4 | Mistral (FR) | 119B / 6B MoE | Apache 2.0 (**V**) | yes | |
| Nemotron 3 Nano / Super | NVIDIA | 31.6B/3.6B; 120B/12B | NVIDIA open licences (**V**) | yes | exclude the Qwen-based Nemotron variants |
| Llama 4 Scout / Maverick | Meta | 109B/17B; 400B/17B | Llama 4 Community (**V**) | **no**: derivatives must be named "Llama…" and show "Built with Llama" | avoid |

### Throughput

**Measured** results replace the estimates below: see the summary in "Synthetic queries" below. The estimates are kept
for reference.

#### Pre-benchmark estimates

Workload: ~700 prompt tokens (chunk + title/abstract/neighbours), ~120 output tokens for 3 queries in JSON (250 for
gpt-oss, to cover reasoning). Pilot ~650k queries ≈ 217k calls; full scale 10–20M queries ≈ 3.3–6.7M calls. Rates
are fitted from published TensorRT-LLM numbers, derated 40% for vLLM, JSON and retries, and checked against
SemiAnalysis's vLLM gpt-oss-120b figure (25–44k tok/s per B200).

| Class | B200 GPU-h, pilot / full | RTX PRO 6000 GPU-h, pilot / full |
|---|---|---|
| ~2B dense or 1B-active MoE | 0.4 / 6–12 | 1.1 / 17–35 |
| gpt-oss-20b | 0.9 / 14–27 | 2.6 / 40–81 |
| Gemma 4 26B-A4B | 1.6 / 24–49 | 4.7 / 73–145 |
| gpt-oss-120b | 1.8 / 27–55 | 5.2 / 80–161 |
| Inkling 2 Small (594B / 17B) | 6.5 / 100–200 | ~22 / 330–670 (8 cards over PCIe; risky) |

Generation is cheap at every size, but the budget was **2 GPUs at a time**. So the pilot is a few hours on 2 GPUs; full scale with
a Gemma-class model is about 1–2 days on 2 B200s (B200 availability permitting), or 3–4 days on 2 RTX PRO 6000s. An
Inkling-class model does not fit in 2 GPUs' memory (NVFP4 needs ≥ 500 GB) and is out unless that changes. The choice
is about quality on the hard query types, and licence.

### A/B harness (built 2026-11-15, `rust/pigmed-synth`)

- `sample`: 1,000 commercial-licence **train** articles, round-robin over e-means level-1 clusters. The result is
  4,988 chunks from 489 of 511 clusters (5 per article, spread through the body, template sections excluded; mean
  292 words).
- `generate`: a concurrent client for vLLM's OpenAI API. One request per chunk, four query types back as
  schema-constrained JSON, latency and token usage per request.
- `filter`: EG2 query embedding (flashembed, CPU), then brute-force cosine against all **2.8M** text units of the
  98k corpus: source-chunk rank in the corpus, rank within its article, and source-article rank; plus
  near-duplicate queries (cos ≥ 0.95). Ranking 20k queries takes 110 s on 32 CPUs.
- `report`: keep-rate per arm and type (keep = chunk top-3 in its article **and** article top-3 in the corpus),
  passage overlap, copying, contextual naming, and a side-by-side review sample.
- Generation runs on one GPU with weights on node-local disk, never on `/blue`.

**No-LLM reference arm** (queries copied from the chunk; *measured*):

| Type | Keep (rule) | Chunk top-3 corpus | Article top-3 | Passage overlap |
|---|---|---|---|---|
| keyword (6 most frequent terms) | 43.7% | 37.7% | 55.0% | 1.00 |
| first sentence (as clinician/patient) | 68.0% | 65.3% | 70.0% | 0.99 |
| title (as contextual) | 11.7% | 10.0% | 98.6% | 0.42 |

An LLM arm should beat copying on keep-rate while copying far less. The title row is the contextual-query problem
in miniature: the right article is found 99% of the time, but the right chunk only 12% of the time.

### Benchmark (done 2026-11-16)

The benchmark ran on 1,000 articles (4,988 chunks) and 1,000 figures. Arms: 5 models × precisions × drafters ×
3 text layouts + a figure track. Full per-arm results are not part of this repository.
- Keep-rates are close (text 53–57%, figures 47–54%); throughput differs about 9×.
- Generated queries go through a degenerate-output validator: no query equal to its key, no field empty, not all
  fields identical. Quantised NVFP4 Glimmer emitted such outputs on about 0.8% of chunks.

### Original A/B design

- **Arms:** (1) Gemma 4 26B-A4B-it, thinking off; (2) gpt-oss-120b; optional (3) Inkling 2 Small on ~50k hard
  queries (contextual, multi-hop), only if decision 8 allows its data lineage. A floor arm (LFM3-1.7B) only if
  counsel clears its licence.
- **Sample:** ~1k commercial-licence **train** articles, topic-balanced by e-means cluster (one draw per cluster
  stratum), ~25k chunks, the same prompts and query-type mix for every arm.
- **Filter (identical across arms):** round-trip with base EG2 (the source chunk in the top 3 of its own article's
  chunks *and* of the whole train corpus for non-contextual types) plus an LLM-judge margin (positive judged
  relevant; top non-positive judged not). Dedup queries by MinHash and cos ≥ 0.95 (EG2), and drop 13-gram or
  cos ≥ 0.95 overlap with benchmark queries.
- **Measure:** keep-rate per type; distinctness (unique 3-grams, cluster spread); then train a 1-GPU S2 pilot per arm
  (same steps) and score our `test` / `test_temporal` contextual tasks, ConTEB and NanoBEIR.
- **Cost:** under 10 B200-hours for all arms together, including the judge; runs inside the 2-GPU cap.
- **Gate:** ConTEB, which runs in seconds per set on one GPU with the ConTEB eval script (`scripts/eval_conteb.py`), plus our own
  `test` / `test_temporal` tasks.

### Use of e-means

- **Topic-balanced sampling** of articles for synthetic queries: draw per cluster (k = 4,096 text clusters), capped
  per cluster, so dominant fields do not swamp the set.
- **Cluster-aware hard negatives:** mine EG2 negatives from the query's own and neighbouring clusters; exclude any
  candidate in the positive's paper group (split manifest) to avoid false negatives from duplicates.
- **Cluster-stratified dev/test:** report metrics per cluster stratum; check that `test` covers every top-level
  cluster.
- **Duplicate synthetic queries:** cluster query embeddings and drop near-identical queries within a cluster.
- **Near-dup groups for splits:** E3 edges, with the article-level rule above.

## Evaluation sets to hold out

| Area | Set | Queries | Notes |
|---|---|---|---|
| Ours | PMC `test`, `test_temporal`, `test_journal` | abstract sentence → body, citing sentence → cited, caption ↔ figure, synthetic (filtered, test groups only) | the main signal |
| Biomedical (real queries) | TREC-COVID (50), NFCorpus, SciFact, BioASQ b (if registered) | expert and lay | blocklisted from training |
| Clinical | TREC CDS 2014–16 (PMC corpus), TREC PM 2017–20, TREC Clinical Trials 2021–23 | clinician case descriptions | topics public on trec.nist.gov |
| Medical RAG | MIRAGE (7,663 questions over MedQA, MedMCQA, PubMedQA, BioASQ, MMLU-Med) | exam-style | end-to-end RAG check |
| Contextual / long | ConTEB, LongEmbed, MLDR-en | | |
| General guard | NanoBEIR → BEIR | | regression gate |
| Guidelines | held-out guideline organisations (from the guideline corpus) | recommendation lookup, written from guideline questions | build once adapters land |

## First steps

1. ~~Identity graph and `splits audit` over the 98k corpus.~~ Done (2026-11-15, Rust); audit passes. Next: NCBI PMC ID
   mapping and PubMed abstracts in the graph; TREC-COVID / NFCorpus / SciFact in the blocklist; manual review of a
   sample of the raw E3 pairs the article-level rule rejected.
2. ~~Length census, question-title and citation-pair counts.~~ Done; see "Measured on the 98k corpus".
3. Build the free-supervision pairs on the train split (citation context → cited, abstract sentence → body,
   "Fig. N" → figure, question title → conclusion). Rerun our eval tasks on `test` / `test_temporal`.
4. User picks the generator (decision 7) and the lineage scope (decision 8); then the 1k-article A/B.
5. Train the clean Ettin cross-encoder teacher.
6. Run the mixing sweep on the S1/S2 pilot.

## Sources

- BMRetriever (arXiv 2404.18443)
- GPL (2112.07577)
- E5-Mistral synthetic data (2401.00368)
- EmbeddingGemma (2509.20354)
- LongEmbed (2404.12096)
- ConTEB / InSeNT (2505.24782)
- Promptagator (2209.11755)
- MIRAGE / MedRAG (2402.13178)
- MedQuAD (1901.08079)
- voyage-context-3 blog (2025-07-23)
- HF dataset cards: lightonai/embeddings-pre-training, lightonai/embeddings-fine-tuning, lightonai/mgte-en
  (curated), sentence-transformers/embedding-training-data, nomic-ai/nomic-embed-unsupervised-data,
  microsoft/ms_marco, HuggingFaceFW/fineweb-edu, HuggingFaceFW/finepdfs, Shitao/MLDR, dwzhu/LongEmbed,
  vidore/colpali_train_set, axiong/pmc_oa, epfl-llm/guidelines, orionweller/contrastive-pretraining
- trec-cds.org, bioasq.org
- LightOn DenseOn/LateOn (2607.27178; HF blogs denseon-lateon, mdenseon-mlateon), Ettin rerankers (HF blog
  ettin-reranker), MICE (2610.31932), SparseUp (linkup.so blog), Mixedbread rerank v1/v2 blogs and configs
- InPars (2202.05144), Promptodile (2510.02241), SPEED (2410.18634), 2602.09448
- Model cards: google/gemma-4-26B-A4B-it, openai/gpt-oss-120b, thinkingmachines/Inkling-2-Small,
  LiquidAI/LFM3-1.7B, meta-models/Muse-Glimmer-30B, jhu-clsp/ettin-encoder-*
