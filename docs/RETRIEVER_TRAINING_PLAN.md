# A contextual, multi-vector, multimodal retriever from EmbeddingGemma 2

Status: plan (updated 2026-11-15 with the data-provenance audit and 98k-corpus census). Nothing here has been trained yet. Data sources, splits, mixing and long context are in
`RETRIEVER_DATA_PLAN.md`. Numbers marked *reported* come from the cited papers
and model cards, not from our runs.

## Goal

One open model, built from EmbeddingGemma 2 (EG2), that gives three things:

1. **Contextual chunk vectors.** Each chunk's single vector knows its document. This is Perplexity's
   `pplx-embed-context` idea.
2. **A late-interaction head.** 128-d per-token vectors scored with MaxSim, ColBERT-style, for precise reranking and
   for long or visual documents.
3. **One space for text, figures, video and audio**, tuned for biomedical literature and clinical guidelines.

The model must be built without Chinese-origin model weights anywhere in the pipeline: no Qwen bases, teachers or
generators. Methods published by those projects are fine to reuse.

## What exists today

| | Base | Contextual | Multi-vector | Multimodal | Notes |
|---|---|---|---|---|---|
| EmbeddingGemma 2 (Google, Oct 2026) | Gemma | no | no (single vector, mean pool) | text, image, video, audio | 744M total (271M text backbone, 167M vision, 305M audio); 8,192-token window; MRL 768/512/256/128. HF tag apache-2.0, but the card links the Gemma 4 licence, so verify. |
| colgemma-300m (community) | EmbeddingGemma-300m (EG1) | no | yes, 128-d per token | text | PyLate Contrastive, batch 512, 1 epoch; NanoBEIR 0.62 (*reported*). Proof that an EmbeddingGemma base trains as a ColBERT. Single author, licence unclear. |
| pplx-embed-context-v1 / v2 (Perplexity) | Qwen3 / Qwen3.5 | yes | v2-late family | v2: VL | ConTEB 82.0 (v1-4B), 83.4 (v2-9B) vs 58.8 for the same model with naive chunks (*reported*). Methods only. |
| LateOn / DenseOn, mLateOn (LightOn) | ModernBERT / mmBERT | no | LateOn | no | BEIR 57.2 (LateOn) vs 53.7 for EmbeddingGemma-300m in their table (*reported*). Apache-2.0; PyLate (MIT). Methods only: the weights, data scores and negatives are downstream of mxbai-rerank-v2 (Qwen2.5) and Alibaba gte-modernbert (verified; see the data plan). |
| jina-embeddings-v4 | Qwen2.5-VL | no | yes, plus dense, one backbone | VL | Dense and late heads co-trained with a KL alignment term. Methods only. |
| ColPali | PaliGemma | no | yes | page images | 118k query-page pairs, LoRA; ViDoRe 81.3 (*reported*). |

Nobody has published a contextual or multi-vector EG2. That is the gap this plan fills.

## Architecture

- **Backbone:** EG2 as released (bidirectional, 8k window, shared text, vision and audio towers).
- **Dense head:** the existing one (mean pool, projection to 768, MRL). For contextual chunks, mean-pool each
  chunk's token span from a whole-document pass (late chunking), then apply the same head.
- **Late head (new):** a small MLP from the backbone's hidden states to 128-d per token (as in colgemma-300m: 768
  to 3072 to 768 to 128, no bias), L2-normalised, scored with MaxSim.
  - Storage control: hierarchical token pooling. LightOn reports 2× pooling as near-lossless, and 32 tokens per
    document keeps 94.9–99.4% of NDCG@10 when the model is trained for it.
  - Mean-centring or STE regularisation for ANN, because raw ColBERT token vectors are highly anisotropic.
- **Contextual input format:** the whole document (or 8k windows with overlap) as `title: <t> | text: <section path
  + text …>`, with chunk boundaries marked. Each chunk gets a dense vector (pooled span) and its token vectors (late
  head) from the same pass.
- **Runtime:** flashembed (our Rust CPU/GPU runtime) already runs the dense head. The late-chunking mode is being
  added now; the late head will be a small extra projection kernel.

## Training stages

Each stage has a gate: it must beat the previous checkpoint on the eval suite below before the next stage starts.

### S1. Late head plus dual-head alignment (cheapest big win)

- **Why first:** LightOn's ColBERT-Zero shows that supervised + knowledge-distillation fine-tuning from a dense
  checkpoint gets within 0.3 BEIR points of full ColBERT pretraining at about 1/10 of the compute (about 40 vs about
  408 GH200-hours, *reported*). EG2 is already a strong dense checkpoint.
- **Data:** licence-clean labelled sets, text taken from the upstream sources (not LightOn's copies).
  - NQ, HotpotQA, SQuAD v2, FEVER (CC BY-SA), MIRACL-en (Apache-2.0), PAQ and DBpedia-entity (CC BY-SA).
  - **MS MARCO is dropped** (terms: non-commercial research only, verified). TriviaQA and FiQA are dropped
    (licence unclear), as are GooAQ and the repackaged Stack Exchange (NC).
  - These are all short general passages, so S1 also takes PMC free pairs from the commercial train split
    (citation context → cited article: ~2.4M resolvable references per 58k commercial articles, *measured*).
  - Re-mine negatives ourselves with EG2 (NV-Retriever rule: top 2048, drop candidates above 0.95 × the positive's
    score, sample 7) and exclude candidates in the positive's paper group. LightOn's negatives were mined with
    Alibaba gte-modernbert, and their `rerank_scores` come from mxbai-v2 (Qwen2.5): use neither.
- **Teacher for distillation:** a non-Chinese cross-encoder, which does not exist off the shelf.
  - mxbai-rerank-v2 is Qwen2.5-based (verified). The Ettin rerankers, MICE and SparseUp were all trained on
    mxbai-v2 outputs (verified), so they are out too. mxbai-rerank-v1 is DeBERTa, but its labels came from an
    undisclosed LLM.
  - Plan: train our own cross-encoder from `jhu-clsp/ettin-encoder-*` (MIT, from scratch on open data) with labels
    from a vetted non-Chinese LLM judge (the chosen generator or a larger one), on the sets above plus PMC pairs.
    Until it exists, S1 runs contrastive-only plus the dense/late KL term, and distillation is added once the
    teacher passes NanoBEIR/biomedical gates.
- **Loss:**
  - InfoNCE on the late head, temperature about 0.02, GradCache, negatives gathered across GPUs.
  - Plus distillation of cross-encoder scores (PyLate `Distillation`).
  - Plus a KL term aligning dense and late score distributions (jina-v4) so both heads stay consistent.
  - Keep EG2's prompts unchanged: ColBERT-Zero lost 0.8 BEIR when prompts were dropped.
- **Tooling:** PyLate (MIT, on sentence-transformers). It needs a custom module that wraps EG2's multimodal forward.
- **Compute estimate:** tens of B200-hours.

### S2. Contextual training on our own long documents (InSeNT-style)

- **Why:** on ConTEB, untrained late chunking lifted ModernBERT from 52.0 to 61.0, and InSeNT training lifted it to
  75.6 (*reported*).
  - Training on concatenated unrelated short documents gave no gain (60.2). Natively long documents are required,
    and PMC full texts are exactly that.
  - Late chunking also hurt one set (COVID-QA, −21.7) when untrained, so the training matters for biomedical text.
- **Data:** our PMC corpus (about 98k articles now, about 9.5M later), chunked as variant C/CB of the chunking
  experiment.
  - Queries per chunk come from an LLM. The generator must be licence-clean, with no Chinese-origin
    generator. Recommended: an A/B of Gemma 4 26B-A4B and gpt-oss-120b (both Apache-2.0 MoE) on ~1k articles; see
    the data plan's "Synthetic queries" for the evidence, candidates and throughput.
  - Length (*measured*, 98k corpus, EG2 tokenizer): median 6.6k tokens, but 35% of articles (39% of the
    commercial subset) exceed the 8,192 window, so S2 needs overlapping 8k windows with a title + abstract prefix
    for a third of the data.
  - Free supervision also exists: abstract sentence → body chunk; figure caption → the paragraph that cites
    "Fig. N"; citation context → cited article (citation edges come from the PMC XML we already parse).
- **Loss:** λ_seq · (in-document negatives) + (1−λ_seq) · (in-batch negatives), with λ_seq about 0.1–0.2, on
  late-chunked outputs for both heads.
  - Optional later: Perplexity v2-style distillation from a token-level relevance teacher, which removes the need
    for gold chunk labels. Removing it cost 14.6 ConTEB points in their ablation (*reported*).
- **Compute estimate:** under 100 H100-hours for the ConTEB-scale run (*reported*); more for full PMC.

### S3. Multimodal: figures, pages, video

- **Data:**
  - Figure ↔ caption pairs from PMC: 343k in the 98k sample, roughly 30M corpus-wide. Hard negatives are other
    figures from the same article.
  - Figure ↔ citing paragraph.
  - Page images for scanned guidelines, ViDoRe-style. Check the `vidore/colpali_train_set` licence.
  - Video clips with their captions.
- **Recipe:** keep the vision tower frozen at first (Perplexity did) and train the projection and late head; then
  LoRA. Visual token budget matters for document pages, so keep 280 tokens per image or more for figures and
  charts. EG2's card warns that 128-d truncation hurts visual retrieval.

### S4. Biomedical and clinical specialisation

- PubMed and PMC pairs at scale: title ↔ abstract, abstract ↔ full-text chunks, citation contexts, MeSH-sharing
  pairs (weak positives), guideline recommendation ↔ the evidence it cites.
- Public biomedical evals (check licences): BEIR TREC-COVID, NFCorpus, SciFact; BioASQ (needs registration);
  PubMedQA as a QA probe.
- Do not use NCBI's MedCPT click logs: they are not public, only the model is.

### S5. Compression and release

- Int8 quantisation-aware training for the dense head (pplx's `127·tanh(x)` trick), binary as an option, MRL kept.
- Token pooling for the late head.
- Export to flashembed (CPU and GPU) with parity tests against the PyTorch checkpoint, then evaluate on the full
  suite.

## Evaluation suite (gates)

| Area | Benchmarks |
|---|---|
| General | NanoBEIR (fast), BEIR (14), decontaminated where available |
| Contextual | ConTEB (CC0); our abstract-sentence → body task; a held-out LLM-question set over PMC chunks |
| Biomedical | TREC-COVID, NFCorpus, SciFact, BioASQ (if licensed) |
| Multimodal | our caption → figure and figure → caption over the 98k corpus; ViDoRe (pages) |
| Long documents | LongEmbed, MLDR (English) |
| Regression guard | EG2's original scores, which no stage may drop by more than about 1 point outside its target area |

## Compute and people

- **GPUs:** B200 / RTX PRO 6000, budgeted at **2 GPUs at a time**. Plan S1–S3 as 2-GPU runs (FSDP across 2, GradCache for batch size). S1–S3 fit in a few
  hundred GPU-hours. Data mining, chunking and evaluation run on burst CPUs.
- **Stack:** PyTorch + PyLate + sentence-transformers for training; FSDP and GradCache; flashembed for fast
  embedding of training corpora and evaluation.

## Order and first steps

1. Finish the chunking A/B (running now: variants A, B, C, CB; D with late chunking once the runner mode lands). It
   gives the zero-shot baseline for contextual chunks on our own data and sets the S2 input format.
2. Add ConTEB to our eval jobs (public, CC0) and score EG2 with naive chunks vs zero-shot late chunking. This is
   comparable with the published 52.0 → 61.0 → 75.6 numbers.
3. Prototype S1 on 1 GPU: an EG2 + late-head PyLate wrapper and a NanoBEIR smoke run, then scale.
4. Decide which LLM generates chunk queries for S2 (non-Chinese), and its budget. The
   recommendation and the A/B design are in the data plan.
5. Build the free-supervision pairs on the leakage-safe train split (`rust/pigmed-splits`; audit passes on the
   98k corpus) and train the clean cross-encoder teacher.

## Sources

- pplx-embed v1 (arXiv 2602.11151)
- pplx-embed-v2-late (2610.29523)
- pplx-embed-v2-context (2610.38854)
- ConTEB / InSeNT (2505.24782)
- Late chunking (2409.04701)
- LightOn: PyLate, GTE-ModernColBERT, ColBERT-Zero (2602.16609), LateOn/DenseOn (2607.27178), hierarchical
  pooling (2409.14683)
- jina-embeddings-v4 (2506.18902)
- ColPali (2407.01449)
- EmbeddingGemma 2 (2610.37364)
- colgemma-300m (HF)
- MedCPT (2307.00589)
- Ettin encoders and rerankers (HF blog ettin-reranker), MICE (2610.31932), SparseUp (Linkup blog)

## S1 prototype results (1 GPU, 2026-11-15 to 2026-11-17)

Code: `train/` (`eg2late/`: model wrapper, data prep, mining, GradCache trainer, evaluator, report). Job scripts are
local only.

### Set-up

- **Model.** The EG2 text tower with its released dense head, plus a new late head: 768 → 128 per token.
  - The head is a linear layer plus a zero-initialised GELU MLP residual (768 → 1536 → 128), L2-normalised.
  - The linear part is initialised from an SVD of base-EG2 document token outputs. This "SVD init" head is the
    untrained baseline below.
  - MaxSim is the mean over query tokens of the max over document tokens. Prompt, special and pad tokens are masked.
  - EG2's prompts are kept. Only the text tower is trained. Checkpoints are exported as full HF snapshots, which
    flashembed and sentence-transformers load unchanged, plus `late_head.safetensors`.
- **Loss.**
  - Dense InfoNCE at τ 0.05, plus 0.25 × each MRL truncation (512/256/128).
  - Late InfoNCE at τ 0.02.
  - 0.1 × symmetric KL between the dense and late score distributions.
  - 0.5 × KL from a frozen base-EG2 dense "teacher" to each head. This is an anti-forgetting anchor; no
    cross-encoder teacher is cleared yet.
  - Gold documents are masked as negatives.
  - GradCache in three passes. Batch: 128 queries × (1 positive + 7 mined hard negatives), in-batch negatives,
    one source per batch.
- **Optimiser.** AdamW. Learning rate 2e-5 on the backbone and 5e-4 on the head, 100 warmup steps, cosine decay to
  10%, bf16 autocast.
- **PyLate.** Not used. PyLate 1.6 pins sentence-transformers 5.3 and transformers ≤ 5.3, but EG2 needs transformers
  ≥ 5.18 and sentence-transformers ≥ 6.1. The trainer is plain PyTorch with PyLate's formulation.
- **Wrapper checks.**
  - Dense output matches sentence-transformers (min cosine 0.9999).
  - MaxSim matches a naive loop exactly.
  - Export round trip is exact.
- **Licence.** EG2 is Apache-2.0 and not gated. The model card asks that deployments follow the Gemma Prohibited
  Use Policy. A release must ship the licence and NOTICE, mark modified files, and link the policy.

### Data and mining

- **Sources.** NQ (`sentence-transformers/natural-questions`), HotpotQA (distractor) and SQuAD v2, from upstream
  copies. After holding out 512 validation queries per source:

  | Source | Train queries | Corpus docs |
  |---|---|---|
  | NQ | 99,719 | 75,215 |
  | HotpotQA | 89,935 | 483,690 |
  | SQuAD | 54,940 | 19,029 |

- **Mining.** Hard negatives were re-mined with base EG2 (flashembed bf16). Rule (NV-Retriever TopK-PercPos): top
  256 candidates, drop any above 0.95 × the positive's score, drop gold documents and documents containing the
  answer string, keep 15, sample 7 per step.
- **Base-EG2 rank of the positive.**

  | Source | Top 1 | Top 10 |
  |---|---|---|
  | NQ | 0.773 | 0.999 |
  | HotpotQA | 0.474 | 0.927 |
  | SQuAD | 0.649 | 0.889 |

  NQ is close to saturated, so EG2 has probably seen it in training.

### Run s1-smoke-v1

- **Length.** 1,873 of 1,910 planned steps (about one pass over the data). The 5 h time cap stopped it on one RTX
  PRO 6000.
- **In-batch validation accuracy (1,536 held-out queries).**

  | Head | Step 0 | Step 1,873 |
  |---|---|---|
  | Late | 0.828 | 0.978 |
  | Dense | 0.936 | 0.954 |

- **Throughput.** About 9.5 s/step, roughly 16k real tokens/s. This is far below what the GPU can do (see
  "Throughput" below).

### Evaluation: base EG2 vs s1-smoke-v1 (best = last step)

Evaluator: `eg2late/evaluate.py` (PyTorch bf16) for both heads. The dense head was also scored through the
production path (flashembed-gpu + `scripts/eval_conteb.py` / `eval_chunking.py`); its averages match the PyTorch
evaluator to ±0.002 (individual sets to ±0.008, from bf16 batching differences). The "base late" column is the untrained SVD-init head. The 95% intervals are paired bootstraps
over queries.

**NanoBEIR nDCG@10.** Average over 13 sets:

| Head | Base | S1 | Δ (95% CI) |
|---|---|---|---|
| Dense | 0.633 | 0.636 | +0.003 (−0.018, +0.026) |
| Late | 0.561 (SVD init) | 0.644 | +0.083 (+0.057, +0.114) |

Per-set dense changes:

| Direction | Sets (Δ, with 95% CI where shown) |
|---|---|
| Up | Touche2020 +0.106; FiQA +0.044; ClimateFEVER +0.031 |
| Down | ArguAna −0.080 (−0.157, −0.004); NQ −0.028 (CI includes 0); QuoraRetrieval −0.024 (−0.047, −0.006) |
| Within ±0.01 | All other sets |

The trained late head beats the dense head on 8 of 13 sets and on the average.

**ConTEB nDCG@10.** Average over 8 sets:

| Chunking | Head | Base | S1 |
|---|---|---|---|
| Naive | Dense | 0.531 (fe 0.530) | 0.539 (fe 0.539) |
| Late chunking | Dense | 0.545 (fe 0.547) | 0.576 (fe 0.575) |
| Naive | Late | 0.570 | 0.588 |
| Late chunking | Late | 0.639 | **0.690** |

Late head plus late chunking is 0.690, +0.159 over production EG2 with naive chunks (0.530). Geography goes from
0.756 to 0.890. Small regressions: mldr (late head, naive chunks, 0.829 → 0.787) and football (dense, naive,
0.334 → 0.304).

**98k PMC corpus, article-level MRR@10.** Variant C units; 1.38M units for abstract→body (as in the chunking study).
3,000 queries per task.

| Task | Head | Base | S1 | Δ (95% CI) |
|---|---|---|---|---|
| Abstract sentence → body | Dense | 0.750 (fe 0.749) | 0.755 (fe 0.755) | +0.006 (−0.002, +0.012) |
| Abstract sentence → body | Late | 0.851 | 0.862 | +0.011 (+0.004, +0.018) |
| Title → article | Dense | 0.952 | 0.946 | −0.006 (−0.011, −0.001) |
| Title → article | Late | 0.949 | 0.967 | +0.018 (+0.013, +0.023) |

**Gate (no general score drops by more than ~1 point).**

- Every average passes: NanoBEIR, ConTEB and 98k, for both heads.
- Two per-set dense regressions exceed it: ArguAna −8.0 and QuoraRetrieval −2.4, both with CIs excluding 0.
- Both are symmetric or near-duplicate tasks (argument → counter-argument, question → duplicate question). Training
  saw only short factoid-question → passage pairs, so these regressions are expected.
- The scaled run needs symmetric and long-query pairs plus a stronger dense anchor (below).
- The late head is new and has no regression baseline. Even its untrained SVD init (0.851) beats the dense head on
  the 98k abstract→body task by 10 points.

### Disk

| Item | Size |
|---|---|
| venv | 5.8 GB |
| Raw downloads | 0.4 GB |
| Prepared data with vectors and negatives | 2.9 GB |
| Checkpoints (best and last, 1.45 GB each) | 2.9 GB |
| Eval outputs (vectors deleted after scoring) | 0.6 GB |

## S1 scaled runs (2026-11-17 to 2026-11-18, in progress)

### Throughput (*measured*)
The waste was padding: one step of 128 queries × 8 docs is 132k real tokens but 336k padded. Packing sequences into
fixed rows with block-diagonal flex attention fixes it.
- Flex attention needs small tiles at head_dim 512: the default tiles overflow shared memory on RTX PRO 6000 and
  fail with a misaligned address on B200.
- Padded calls stay on SDPA.

| GPU | Padded SDPA | Packed 32k | Packed 64k + compile |
|---|---|---|---|
| RTX PRO 6000 | 13.7k tok/s, 9.65 s/step, 73 GB | **27.1k**, 4.87 s, 57 GB (×1.98) | 30.1k, 4.38 s, 61 GB |
| B200 | 32.1k tok/s, 4.11 s/step, 73 GB | **46.7k**, 2.82 s, 58 GB (×1.46) | 39.3k, 3.36 s, 61 GB |

**GradCache check:** in fp32, GradCache equals a direct backward to 1–2e-5 relative error, on both GPUs, for the
padded and the packed path. The 0.2–0.57 errors seen earlier were bf16 rounding on a 2-query test batch. Packed
bf16 gradients are about 2× noisier than padded ones (7–13% vs 4–5% against fp32 on that batch). This is precision
noise, not a bug.

### Data provenance (S1 and S1.5 sources)
| Source | Pairs | Licence | Notes |
|---|---|---|---|
| PMC title → body chunk | 140k | PMC OA commercial subset (CC0 / CC BY / CC BY-SA / CC BY-ND) | train split, retracted excluded; base EG2 R@1 0.73 |
| PMC abstract sentence → body chunk | 377k | same | noisy: R@1 0.20, 32% short of hard negatives |
| NQ | 100k | CC BY-SA 3.0 | |
| HotpotQA | 90k | CC BY-SA 4.0 | |
| SQuAD v2 | 55k | CC BY-SA 4.0 | |
| FEVER | 102k | CC BY-SA 3.0 (BEIR packaging tagged CC BY-SA 4.0) | ablation arm A |
| MIRACL-en | 2.4k | Apache-2.0 annotations; Wikipedia text CC BY-SA 3.0 | ablation arm A |
| Base model / teacher / miner | – | EmbeddingGemma 2: Apache-2.0 + Gemma Prohibited Use Policy | no other model weights used |

Excluded: MS MARCO (non-commercial), PAQ, DBpedia, and all Chinese-origin weights.

### Ablation design
| | A `s1-scale-a-fevmir` | B `s1-scale-b-nofevmir` |
|---|---|---|
| Mix | PMC title .25, PMC abstract-sentence .35, NQ .12, HotpotQA .09, SQuAD .095, FEVER .08, MIRACL .015 | PMC .25/.35, NQ .15, HotpotQA .12, SQuAD .13 |
| Common | 6,000 steps × 128 queries, seed 0, packed 32k flex, q 96 / d 512 tokens, KD dense 1.0 / late 0.5, L2-SP 0.01, 1 RTX PRO 6000 each (~10 s/step) | same |

### Step-2,500 evaluation (*measured*, frozen copies; the PMC 98k eval is still running)
| | Base | A @2500 | B @2500 |
|---|---|---|---|
| NanoBEIR dense (13) | 0.633 | 0.642 | 0.628 |
| NanoBEIR late (13) | 0.561 (SVD init) | 0.644 | 0.642 |
| ConTEB naive / dense | 0.531 | 0.542 | 0.543 |
| ConTEB late chunking / dense | 0.545 | 0.570 | 0.565 |
| ConTEB late chunking / late | 0.639 | 0.668 | 0.665 |

Dense per-set Δ (points; ✓ = 95% CI excludes 0):

| Set | A | B |
|---|---|---|
| ArguAna | −1.3 | −0.8 |
| Quora | −1.2 | −1.8 |
| SciFact | **−6.6 ✓** | −4.6 |
| NQ | −1.4 | −3.9 |
| MSMARCO | −3.2 | −3.9 |
| ClimateFEVER | +11.0 ✓ | +1.0 |
| FEVER | +4.3 ✓ | +0.8 |
| Touche | +8.3 ✓ | +7.3 ✓ |

- The smoke run's ArguAna −8 is mostly gone.
- FEVER/MIRACL help the claim and fact-checking sets. FEVER's gain is optimistic, because NanoFEVER shares FEVER's
  corpus.
- **SciFact (biomedical claims) is the new gate failure in A.** My guess is that the noisy abstract-sentence source
  drives it; the final evals and an S1.5 down-weighting arm will test that.
- The final ablation verdict waits for the 6,000-step evals.

## Methods research (2026-11-18)
Everything in this section is *reported* by the authors; none of it is reproduced here. The full notes and sources
were kept in working notes outside this repository. Weights we cannot use as a base or teacher:
pplx-embed (Qwen3 / 3.5), GTE-ModernColBERT (Alibaba GTE, MS MARCO), SitEmb (BGE), jina-v4/v5 (Qwen),
ColBERT-Zero's KD (BGE teacher, MS MARCO), LateOn/DenseOn data, PMC-15M/BiomedCLIP (research-only), PMC-OA,
MedICaT (NC) and MedTrinity. The methods themselves are fine to reuse.

- **Contextual chunks.**
  - **InSeNT (ConTEB, arXiv 2505.24782):** L = λ_seq·L_seq + (1−λ_seq)·L_batch with λ_seq = 0.1. L_batch excludes
    the positive's own document.
    - ConTEB 61.0 → 75.6, but **NanoBEIR 63.2 → 60.4**.
    - Real long documents beat concatenated short ones (77.8 vs 60.2).
  - **pplx-embed-context v1 (arXiv 2602.11151):** α = 0.2, with L_batch keeping same-document chunks. A global
    q↔document InfoNCE (q–q negatives, dedup masking) is weighted by β on a cosine schedule from 0.2 to 0.5.
    τ = 0.02, 7k steps.
  - **pplx v2 (arXiv 2610.38854):** document score = MaxSim over chunk vectors, plus a chunk-level KL from a
    query-aware token-relevance teacher (τ_S 0.02, τ_T 0.075). Contrastive alone scores 68.1 on ConTEB;
    distillation alone 82.4.
- **Dense + late co-training.**
  - **jina-v4:** InfoNCE on both heads plus a KL between them, with MaxSim divided by query length (as ours is).
    One comparison (arXiv 2608.21494) finds separately trained heads about 4% better, but it is confounded.
  - **ColBERT-Zero:** prompts aligned with pretraining matter (−0.8 BEIR without them).
  - **PyLate:** the default τ = 1.0 is unusable, and its distillation normalises only the student's scores; don't
    copy either. Token pooling (factor 2–3) is an index-time option.
- **Symmetric tasks.** No paper fixes ArguAna specifically. Options:
  - Symmetric pairs with the same prompt on both sides (jina).
  - Bidirectional and q–q terms with false-negative masking (GTE, Qwen3-Embedding).
  - Counter-evidence: Gemini Embedding found same-tower negatives harmful.
  - EmbeddingGemma starts at ArguAna 71.5, so distillation to the base is the strongest guard.
- **Multimodal.**
  - **EmbeddingGemma 2 (arXiv 2610.37364):** trained its vision tower (MMEB-v2 52.3 trainable vs 48.2 frozen) at
    280 tokens; 560 tokens at inference helps VisDoc. It used same-tower negatives and merged the final checkpoint
    with an earlier one.
  - **ColPali:** vision frozen and LoRA r = 32 at 118k pairs.
  - **ModernVBERT:** bidirectional attention gives +10.6 for late interaction; mixing text pairs gives +1.7.
  - **MoCa / ColNomic:** same-source batches help (+1 to +2.9), as do 4–6 NV-filtered hard negatives.

## Revised recipe and code status (2026-11-18)
"CPU-tested" means the unit tests in `train/tests` pass on a tiny random EG2 built with the real tokenizer and
processor files. "GPU check" items are covered by `train/tests/check_gpu.py` (job submitted, results pending).

**S1.5: scale the text model.**
- **Data:** the S1 sources plus a symmetric slice (`source.json` with `symmetric: true` and one shared prompt) and
  the v3 persona queries. The abstract-sentence weight is a sweep axis (SciFact). Mix in `train/configs/s1_5_mix.json`.
- **Loss:** S1's, plus a reverse doc→query term on symmetric rows (`--w-rev`). Optional q–q negatives with masking
  at positive + 0.1 (`--qq-neg`) as an ablation, given Gemini's result. Keep KD to base ≥ 1:1.
- **Scale-out:** gathers capped to a node (`--gather-group`), gradients averaged over groups (`dist_utils.py`).
  `eg2late/launch.py` renders a multi-node srun/torchrun script, with a dry run that validates flags and estimates
  GPU-hours. CPU-tested, including a 4-process gloo run where grouped GradCache equals the mean of per-group direct
  gradients. **Multi-node on NCCL is still untested.**
- Resumable checkpoints and the STOP file were tested on GPU.

**S2: contextual training (`eg2late/contextual.py`, `eg2late/train_s2.py`).**
- **Input:** the real title and abstract as a header repeated in every 8k window (1k overlap), then the body chunks
  with their headings. `late_windows` is the flashembed `late_doc` port, so training pools exactly what we serve.
- **Heads:** dense = mean over the span; late = late-head token vectors of the span.
- **Loss:** per head, (1−β)·[λ_seq·L_seq + (1−λ_seq)·L_batch] + β·L_article (MaxSim over the article's chunks),
  β cosine 0.2 → 0.5, λ_seq 0.15.
  - Same-article chunks can be included in or excluded from L_batch.
  - Dense↔late KL, plus KD from a teacher: base-EG2 plain-chunk vectors, or a [Q, C] relevance hook for S2b
    chunk distillation.
- **Training:** GradCache replays one window group at a time, with negatives shared within the gather group.
  Optional interleaved S1 pair steps (`--s1-every`) against the NanoBEIR drop.
- **Status:** CPU-tested, including GradCache == direct. GPU check: late-chunk parity with flashembed and the cost of
  an 8k-window step.

**S3: figures (`EG2Dual.load(vision=True)`, `lora.py`, `images.py`).**
- **Image path:** soft tokens are scattered into the text backbone exactly as transformers does (CPU-tested to 1e-5).
  The late head runs on the soft tokens.
- **Vision tuning:** `--vision frozen|lora|full` with `--lr-vision`. LoRA is merged on export.
- **Data:** an image source is a directory with `figures.jsonl` (≤896 px JPEGs) and caption or mention queries; the
  same trainer handles it, with a reverse figure→caption loss.
- **Hard negatives:** mined from the v2 image vectors; the image teacher waits on the parity check.
- **Evals:** `evaluate.py figures` covers caption→figure, mention→figure and figure→caption for both heads
  (CPU-tested). ROCOv2 CC BY-only (2k pairs) is the external set.
- **Status:** GPU check covers image parity with flashembed and step cost (frozen vs LoRA).

## Next steps
1. Final ablation evals (about 12:00), then pick the FEVER/MIRACL arm. Decide the abstract-sentence weight from
   SciFact and the PMC tasks.
2. GPU check results: if late-chunk and image parity hold (cos ≥ 0.999), enable the v2 image-vector teacher. Size
   the S2 and S3 batches from the measured memory.
3. Data from the retriever-data session in the proposed formats: symmetric pairs, articles plus contextual queries,
   figures plus vectors.
4. An S1.5 sweep (6–8 single-node runs) if a large GPU window opens. Otherwise the best mix on 8 cai GPUs. One
   2-node NCCL smoke run before any multi-node run.
5. Then: S2 from the S1.5 best (S1 steps interleaved, NanoBEIR gate), S2b chunk distillation, and S3 stage A
   (vision frozen) then B (LoRA).
