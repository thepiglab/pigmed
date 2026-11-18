# Intent vs query: research notes for PigMed

What a user *means* (the information need) and what they *type* (the query) differ. This note covers how the
literature (weighted to 2022–2026) and industry handle that gap, and what PigMed should do about it in synthetic
query generation, retriever training, search-time handling and evaluation.

Written 2026-11-17. Companion docs: `SEARCH_TECHNIQUES_2026.md` (query-side techniques, eval sets),
`RETRIEVER_DATA_PLAN.md` (generators, query types, licences), `RETRIEVER_TRAINING_PLAN.md`.

**Evidence tags**

| Tag | Meaning |
|---|---|
| **[R]** | Reported by the cited source, and checked on the source page (or its arXiv abstract or HTML) at time of writing |
| **[R-mem]** | Reported by the cited source, from memory; not re-checked. Verify before quoting |
| **[I]** | Our inference |
| **[W]** | Weak evidence: vendor claims, secondary coverage, a single preprint, or an indirect result |
| **[P]** | PigMed measurement |

**Licence and provenance flags:** **NC** = non-commercial licence. **CN** = model or data from a Chinese company, or
built with one (Qwen, DeepSeek, BGE, GTE, and so on). **MSM** = uses MS MARCO (non-commercial terms). **?** =
licence unclear.

---

## Top recommendations for PigMed

| # | Recommendation | Expected impact | Cost | Licence / provenance |
|---|---|---|---|---|
| 1 | **Generate an intent card first, then several surface queries.** The card has: the need in one sentence; a TREC-style *narrative* (what counts as relevant and what does not); intent type; user role; optional PICO slots. Then write **4–6 surface queries per card at fixed registers**: expert keyword, clinician question, lay question, messy (typos, fragments, non-native), partial recall ("that trial where…"), and for 5–10% of cards, query plus instruction. | The largest lever on the messy-query gap. Our Sonnet rewrite gains +0.08 nDCG on messy queries and ~0 on clean ones [P]. The retriever should learn that mapping itself. E5 conditions generation on length, clarity and difficulty attributes [R]. Tamber et al. find that training on **all query types combined** beats any single type [R]. Synthetic ToT queries rank systems almost like real ones (τ = 0.847) [R]. | About 1 extra short call per card. It fits layout c (one call per article). Token cost +30–50% [I]. | Generator stays Nemotron 3.5 Super VL or Gemma 4 (clean). |
| 2 | **Personas: few coarse axes, matched to the intent.** Use a PigMed role table (about 12 roles: clinician types, researcher, student, patient, caregiver, librarian, journalist, policymaker), plus health literacy, native language, urgency and device. Draw demographic texture from **NVIDIA Nemotron-Personas-USA**. Sample **doc → plausible intents → role given intent → modifiers**, with stratified quotas on the marginals rather than the full cross product. Add a plausibility filter, so no 9-year-old asks about CRISPR off-target assays. | Diversity and lay or messy coverage. Persona prompting raises lexical diversity; **fine-grained persona detail adds little beyond explicit length or register constraints** (EMNLP Findings 2025) [R]. Direct evidence that personas improve *retriever training* is weak [W]. | About 0: the prompt gets a few slots. | Nemotron-Personas-USA is **CC BY 4.0**, commercial use OK, text from gpt-oss-120b [R]. **Do not use PersonaHub**: **NC** (CC BY-NC-SA) and **CN** (Tencent; built with GPT-4, Llama-3 and Qwen) [R]. |
| 3 | **Starting register mix:** about 45% professional (keyword plus clinician or researcher question), 25% lay, 15% messy, 7% partial recall or known item, 8% instruction-bearing. Then **tune it** with slice evals. | Covers the traffic we expect. No paper gives an optimal ratio [I]. Diversity helps most where queries are complex (the Complexity-Diversity Principle, r ≥ 0.95) [R]. | Ablation runs only. | None. |
| 4 | **Label relevance by intent, graded, with several positives.** A teacher grades the top-k candidates (k ≈ 20–30) from the current retriever *against the intent card*, not the surface query. Train with soft targets (KL), mask false negatives within paper groups, and promote better passages to positive. | Gecko: the LLM picked a better positive than the seed passage in ~15% of cases, and relabelling improved BEIR (52.3 → 53.1) [R]. Perplexity v2 trains on graded chunk targets from a relevance teacher [R]. On Robust04, adding the TREC *narrative* to an LLM judge prompt raised κ by about 0.06 (best κ ≈ 0.64) [R]. | The main cost: 20–30 judge scores per query. Use a small clean cross-encoder first and an LLM only on disagreements [I]. | The judge must be clean. Avoid mxbai-rerank-v2 (**CN**, Qwen2.5 base [R-mem]), ctxl-rerank-v2 (**NC**) and Rank1 (**CN** traces, **MSM**). |
| 5 | **Training: use fixed task prefixes and a small intent-conditioning slice. Do not build a full Promptriever.** Keep the EmbeddingGemma-style `task: … \| query:` prefixes. Add about 5–10% *instruction-negative* pairs from the intent cards: the same surface query with a different narrative has different positives. | Promptriever's gains (+14.3 p-MRR on FollowIR, +12.9 Robustness@10 on InstructIR) come from a 7B LLM backbone [R]. Instruction tuning on *task-style* instructions **hurt** robustness in INSTRUCTOR [R]. At ~300M scale and on CPU, long instructions cost latency and the benefit is unproven [I]. | Low. | Promptriever's data is **MSM**; generate our own. |
| 6 | **At search time, route instead of rewriting everything.** A cheap Rust classifier sorts queries (ID or entity, keyword, expert NL, lay, messy, partial recall). Only lay, messy and partial-recall queries get an LLM rewrite into 2–3 expert variants. **Keep the original query** and fuse the results. Cache the rewrites, and stream the dense results first. | Captures the +0.08 on messy queries [P] and avoids the ≈0 or negative effect on strong or clean queries [R] (Weller 2024; Abe 2025; Elastic). | One API call on the routed subset only. | Claude via API. Do not train on Claude outputs without checking the terms [I]. |
| 7 | **Use a zero-latency lay → expert lexicon** from MeSH entry terms plus CHV, both for BM25 expansion and as seed paraphrases in generation. | Cheap fallback when the LLM is off; feeds lay-register generation [I]. | Rust table lookup. | MeSH: free under NLM terms. **CHV** is UMLS restriction level 0 [R], but it is distributed under the UMLS licence. **Do not ship SNOMED CT or other restricted UMLS sources.** |
| 8 | **Evaluate against intents, not single targets.** Build about 300–500 held-out intent cards × 4–6 surface variants each. Pool the top 10 from several systems, grade the pool with an LLM judge given the intent card, and spot-check with humans. Report per-register nDCG@10, **Robustness@10** (the worst variant per intent), judged@10, and real-query sets (TREC-COVID, NFCorpus, CLEF eHealth, R2MED). After launch, add reformulation and abandonment rates. | Fixes the single-target problem we hit at 9M scale. MS MARCO-style single labels make better rankers look "better than perfect" [R]. LLM judges overrate passages that merely contain the query words, so keep humans in the loop [R]. | Judge calls, roughly 500 × 5 × 30 ≈ 75k [I]. | Eval-only sets may be **NC**; do not train on them. |

**Answering "instruction or intent conditioning?" directly:**

- Condition the **data** on intent (cards, registers, graded labels): high value, low risk.
- Condition the **model** only lightly (task prefixes plus a small instruction-negative slice).
- Revisit full instruction-following once we have a ≥1B student, or a reranker tier that can afford long inputs [I].

---

## 1. Concepts and terminology

- **Information need vs query.**
  - **Taylor (1968)** describes four levels of need: visceral, conscious, formalised and compromised. The query is the
    *compromised* form, shaped by what the user thinks the system can handle [R-mem]
    ([Taylor 1968](https://doi.org/10.5860/crl_29_03_178)).
  - **Belkin's Anomalous State of Knowledge (ASK)** (Belkin, Oddy & Brooks 1982). Users search *because* they cannot
    specify what they lack, so the query is an inherently underspecified description of a gap
    ([J. Documentation 1982](https://doi.org/10.1108/eb026722)) [R-mem].
  - TREC topics keep the same split: *title* (the query), *description* (the need) and *narrative* (the relevance
    criteria). Our "intent card" (Rec. 1) is a TREC topic generated per document [I].
- **Vocabulary mismatch.** Furnas et al. (CACM 1987) found that two people choose the same term for an object less
  than 20% of the time [R-mem] ([doi](https://doi.org/10.1145/32206.32212)). Dense retrieval reduces the mismatch but
  does not remove it. Lay–expert pairs are the extreme case in medicine.
- **Underspecified vs ambiguous queries.**
  - *Ambiguous*: several distinct readings ("cold": the disease or the temperature).
  - *Underspecified* or *faceted*: one topic, but many sub-needs.
  - TREC Web diversity tasks (2009–2012) model both with subtopics [R-mem].
  - LLM expansion tends to pick the *popular* reading of an ambiguous query
    ([Abe et al., SIGIR 2025](https://arxiv.org/abs/2505.12694)) [R].
- **Known-item and tip-of-the-tongue (ToT).**
  - The user knows that a specific item exists but cannot name it. Queries are long, uncertain and full of false
    memories (Arguello et al., CHIIR 2021) [R-mem].
  - **TREC ToT 2023–2025.** The 2025 track spans 53 entity types and 622 test queries: 172 Reddit-sourced MS-ToT, 150
    NIST human-elicited and **300 LLM-generated** (Llama-3.1-8B and GPT-4o). System rankings agree between MS-ToT and
    synthetic queries (Kendall τ = 0.847), less so between synthetic and NIST queries (0.737)
    ([overview, arXiv 2601.20671](https://arxiv.org/abs/2601.20671)) [R].
  - Biomedical analogue: "the RCT a few years ago showing a cheap drug cut sepsis deaths in kids" [I].
- **Intent taxonomies.**
  - Broder (2002): navigational, informational, transactional [R-mem].
  - Rose & Levinson (2004) refine it further.
  - **ORCAS-I** (Alexander, Kusa & de Vries, SIGIR 2022) labels about 18M Bing queries with intent through weak
    supervision [R-mem]. **MSM-adjacent:** ORCAS comes from the MS MARCO document set, research use.
  - The PigMed adaptation, with types we can route on, is in §4.3.
- **Lay vs expert vocabulary.**
  - The Consumer Health Vocabulary (CHV; Zeng & Tse 2006) maps lay terms to UMLS concepts. It is in UMLS at
    **restriction level 0**, contact Q. Zeng, last edition CHV2011_02 ([NLM metadata](https://nlm.nih.gov/research/umls/sourcereleasedocs/current/CHV/metadata.html)) [R].
  - In a CLEF eHealth study, lay users benefit from adding the expert term to their query, but medically trained users
    prefer their original query [R]
    ([Springer IRJ](https://link.springer.com/content/pdf/10.1007/s10791-018-9344-z.pdf)).
- **"Can't word it" searches.**
  - Users describe symptoms, mechanisms or a half-remembered paper rather than name the concept.
  - **OBLIQ-IR** (2026) formalises *oblique queries*, where relevance rests on a latent attribute with little surface
    expression. A 3B retriever trained on per-mechanism synthetic queries beats GPT-5.2 multi-hop agents by 0.010–0.150
    nDCG@10 ([arXiv 2609.29649](https://arxiv.org/abs/2609.29649)) [R]. The base model is not named on the abstract
    page.

## 2. Retrieval models and benchmarks that model intent

### 2.1 Instruction-following retrieval

| Work | What it shows | Base model / provenance | Key numbers |
|---|---|---|---|
| **FollowIR** (Weller et al., NAACL 2025; [2403.15246](https://arxiv.org/abs/2403.15246)) | Benchmark built from TREC Robust04, Core17 and News21. Each TREC narrative is edited so that relevance changes. The metric is **p-MRR** (pairwise rank change, −100 to 100). Most retrievers treat instructions as keywords. | FollowIR-7B = Mistral-7B-Instruct-v0.2 + LoRA (Mistral AI; clean). | p-MRR: Mistral-7B-Instruct +10.1 → FollowIR-7B +12.2. Many IR models score ≤ 0 [R]. |
| **Promptriever** (Weller et al., ICLR 2025; [2409.11136](https://arxiv.org/abs/2409.11136)) | A bi-encoder trained on ~490k MS MARCO queries with per-query instructions (from Llama-3-70B) and **instruction negatives**: passages relevant to the query but not to the instruction. | Llama-2-7B (RepLLaMA); later releases on Llama-3.1-8B and Mistral-7B. **MSM.** | +14.3 p-MRR / +3.1 nDCG on FollowIR; +12.9 Robustness@10 on InstructIR; +1.4 BEIR from prompt search [R]. |
| **InstructIR** (Oh et al., 2024; [2402.14334](https://arxiv.org/abs/2402.14334)) | **User-aligned, per-instance instructions**: GPT-4 writes 10 first-person scenarios (job, background, goal) per query and rewrites the target to fit each one. Introduces **Robustness@k**: the worst nDCG across instructions for the same query. | Benchmark from the MS MARCO dev set (**MSM**); GPT-4-written. | 1,267 queries, 9,906 instances. INSTRUCTOR scores *below* its GTR backbone on Robustness (~21–24 vs 34–38), which suggests overfitting to task-style instructions. E5-Mistral is the best zero-shot model (55.4) [R]. |
| **MAIR** (Sun et al., EMNLP 2024; [2410.10127](https://arxiv.org/abs/2410.10127)) | 126 tasks and 805 instructions over 6 domains, including **Medical**: NFCorpus, TREC-COVID, TREC CDS 2014–16, Clinical Trials 2021–23, Genomics, PM and CARE. | Benchmark only. | Instructions help instruction-tuned embedders (GritLM-7B gains on 106 of 126 tasks) and *hurt* most models that were not instruction-tuned. bge-reranker-v2-gemma gets worse with instructions [R]. |
| **IFIR** (Song et al., NAACL 2025; [2503.04644](https://arxiv.org/abs/2503.04644)) | 2,426 expert-validated examples in finance, law, **healthcare** and science literature, each at 3 levels of instruction complexity. | Benchmark only. | 15 retrievers, LLM-based ones included, struggle with complex domain instructions [R]. |

**Takeaway [I].** Instruction-following is real and measurable, but so far it has only been shown on 7B-class
backbones. InstructIR is the closest thing in the literature to "the same need, different users". Its Robustness@k
metric is what we should report (Rec. 8).

### 2.2 Reasoning-intensive retrieval

Here the intent sits behind a long, indirect query. The gold document shares the *reasoning*, not the words.

| Work | Base model / provenance | Numbers |
|---|---|---|
| **BRIGHT** (Su et al., ICLR 2025; [2407.12883](https://arxiv.org/abs/2407.12883)). 1,384 real StackExchange-style queries; Biology is the closest split to us. | Benchmark. | SFR-Embedding-Mistral scores 59.0 nDCG@10 on MTEB but **18.3** on BRIGHT. LLM-reasoned queries add up to +12.2 [R]. |
| **ReasonIR-8B** (Meta, 2025; [2504.20595](https://arxiv.org/abs/2504.20595)) | Llama-3.1-8B. Trained on public data (incl. **MSM**) plus ReasonIR-Synthesizer data (generator not named on the card). Weights **CC BY-NC 4.0** (**NC**) [R]. | BRIGHT 24.4 with original queries, 29.9 with GPT-4-rewritten queries, 36.9 with a reranker [R]. |
| **RaDeR** (UMass, EMNLP 2025; [2505.18405](https://arxiv.org/abs/2505.18405)) | Retrievers on Qwen2.5-Instruct, **gte-Qwen2-7B** (the headline model) and Llama-3.1-8B (**CN** for the main models). MCTS math-reasoning trajectories. | +1.1 nDCG over ReasonIR on BRIGHT, using about 2.5% of its training data (43k vs 1.73M examples) [R]. |
| **Rank1** (Weller et al., COLM 2025; [2502.18418](https://arxiv.org/abs/2502.18418)) | Reranker distilled from 635k **DeepSeek-R1** reasoning traces on **MS MARCO** (**CN**, **MSM**). Bases: Qwen2.5, plus Mistral-24B and Llama-3-8B variants [R-mem]. | Strong on BRIGHT and on instruction-following, through test-time reasoning [R-mem]. |
| **ReasonRank** (Renmin/CMU, EMNLP 2025; [2508.07050](https://arxiv.org/abs/2508.07050)) | Qwen2.5-7B/32B; data synthesised with DeepSeek-R1 (**CN**). | 40.6 on the BRIGHT leaderboard (author-reported) [R]. |
| **Reason-ModernColBERT** (LightOn, 2025) | Fine-tuned from GTE-ModernColBERT-v1 (Alibaba GTE lineage, **CN**) on ReasonIR-HQ data. **CC BY-NC 4.0** because of that data [R] ([card](https://huggingface.co/lightonai/Reason-ModernColBERT)). | About 150M params. The late-interaction recipe is relevant to us, but the weights are not usable. |
| **R2MED** (2025; [2505.14558](https://arxiv.org/abs/2505.14558)). 876 medical queries: QA reference, clinical evidence and clinical case retrieval. | Benchmark. | Best plain retriever 31.4 nDCG@10 (NV-Embed-v2, which scores 63.2 on BEIR). Reasoning-augmented pipelines (o3-mini, Query2Doc + GPT-4o) reach about **41.4** [R]. |

**PigMed fit [I].**

- Every reasoning retriever or reranker above is either **NC**, **CN** or 7B+. None is servable on 2 vCPU.
- The transferable idea is the *data recipe*: write queries that require inferring the need, with hard negatives that
  share the surface terms but not the need. That is our "partial recall" and "clinical case" registers.
- R2MED is the right biomedical stress test. Use it for eval only, and check its licence.

### 2.3 Intent-aware, ambiguity-aware retrieval and clarification

- **Clarifying questions.**
  - Qulac and ClariQ (Aliannejadi et al. 2019–2021) and MIMICS (Bing clarification panes, Zamani et al. 2020) show
    that a good clarification lifts retrieval, but users skip clarifications that are poor or not needed [R-mem].
  - For a search box, the cheap form is **facets or chips** rather than a question: population (adult or paediatric),
    article type (RCT, review, guideline) and "did you mean <MeSH term>" [I].
- **Show the system's interpretation.** The position paper by Anand et al. 2023
  ([2306.16004](https://arxiv.org/abs/2306.16004)) proposes that an LLM states its reading of the intent in natural
  language, so the user can edit it before retrieval [R]. This suits PigMed's lay queries: "Searching for: *myocardial
  infarction* (heart attack)" [I].
- **Diversification** (xQuAD, IA-Select, from the 2009–2010 era): fill the top 10 with several readings of an
  ambiguous query [R-mem]. Cheap to bolt onto fusion [I].

### 2.4 Personalised and contextual retrieval

- LaMP (Salemi et al. 2023) and its successors personalise from a user's history [R-mem]. That needs logged-in
  history, which we do not have and should not want, for privacy [I].
- InstructIR's persona instructions (above) are the stateless alternative: the user *states* a context.
- **PigMed [I].** An optional role toggle (clinician, researcher, patient or caregiver, student) mapped to a task
  prefix or a ranking prior, for example boosting reviews and plain-language summaries for patients. No profiling.
- "Contextual" in our *model* sense (chunk embedded with document context, ConTEB-style) is orthogonal: it fixes the
  document side of the gap, not the query side.

## 3. Synthetic query generation that separates intent from surface query

### 3.1 Prompting families

| Method | How it handles intent | Generator / provenance | Evidence |
|---|---|---|---|
| **InPars** v1/v2 (Bonifacio 2022; Jeronymo 2023) | Few-shot "bad question / good question" prompts; v2 filters with a monoT5 reranker. | v1 GPT-3; v2 GPT-J-6B. The monoT5 filter is trained on **MSM**. | MS MARCO MRR@10 0.215 (0.3B generator) → 0.270 (175B) [R, via `RETRIEVER_DATA_PLAN.md`]. |
| **Promptagator** (Dai et al., ICLR 2023; [2209.11755](https://arxiv.org/abs/2209.11755)) | **Task-specific** few-shot prompts (≤8 examples per task) capture each task's notion of intent. Round-trip filtering. | FLAN 137B. Re-run with open 3B–14B models ("Promptodile") reaches about parity [R]. | Filtering is worth ~2.5 BEIR points [R]. |
| **SPTAR** (Peng et al. 2023; [2307.08303](https://arxiv.org/abs/2307.08303)) | Soft-prompt tuning on a small labelled set, so the generator learns the task's query style. | LLaMA- and Vicuna-7B class [R-mem]. | Beats InPars-style hard prompts on its sets [R-mem]. |
| **EGG** (Lee, Kim & Hwang 2024; [2409.16570](https://arxiv.org/abs/2409.16570)) | Treats generation as **compiling a high-level intent into a task-adaptive query**. The search intent is stated explicitly; not every query is a question. | Generator 47× smaller than the earlier SOTA. | 4 BEIR tasks with "under-explored intents" [R]. |
| **Gecko / FRet** (Google DeepMind 2024; [2403.20327](https://arxiv.org/abs/2403.20327)) | The LLM writes a **task description and a query** from a seed passage. It then **relabels**: it retrieves neighbours, scores them by query likelihood and relevance classification fused with RRF, and picks the best positive and a hard negative. | Gemini-family LLM (Google). | The positive differs from the seed in ~15% of cases. BEIR 52.33 → 53.07 with the LLM-picked positive. Gecko-768 scores 66.31 on MTEB [R]. |
| **E5-Mistral** (Wang et al. 2024; [2401.00368](https://arxiv.org/abs/2401.00368)) | First brainstorms tasks, then generates per task with sampled attributes: query_type {extremely long-tail, long-tail, common}, length {<5, 5–15, ≥10 words}, **clarity {clear, understandable with some effort, ambiguous}** and **difficulty {high school, college, PhD}** [R]. | GPT-4 data. | The prototype for "skill-varied surface queries". |
| **SPEED** (Chen et al. 2024; [2410.18634](https://arxiv.org/abs/2410.18634)) | An aligned small generator reproduces the E5-style pipeline. | Llama-3-8B generators (Renmin + Microsoft authors). | 63.4 vs E5-Mistral's 63.1 on MTEB [R, via data plan]. |
| **Tamber et al.** (2025; [2505.19274](https://arxiv.org/abs/2505.19274)) | Six query types: question, claim, title, keyword, natural user search, few-shot natural. | Llama-3.1-8B. | **All types combined, without downsampling, gives the best results.** Keyword-only training hurts FiQA; claim-only hurts NFCorpus. Zero-shot "natural user search queries" are the best single type [R]. |
| **Complexity-Diversity Principle** (Feng et al. 2026; [2602.09448](https://arxiv.org/abs/2602.09448)) | Several queries per document. Diversity helps out-of-domain, quality helps in-domain. | Contriever, RetroMAE, Qwen3-Embedding. | The diversity benefit correlates with query complexity (content words), r ≥ 0.95 [R]. Diversity helps above ~10 content words and should be avoided below ~7 [R, from a secondary summary]. |
| **pplx-embed v1** (Perplexity, Feb 2026; [2602.11151](https://arxiv.org/abs/2602.11151)) | **Two-stage persona-based** query generation (top-5 personas per passage). | Qwen3-30B-A3B generator; Qwen3 backbones (**CN**) [R]. | ConTEB 81.96 for the 4B contextual model [R]. No persona ablation reported [W]. |

### 3.2 Intent first, then many surface queries (simulated users)

- **The UQV100 lineage.** One backstory, many crowd-written query variants. Query-variant variance is as large as the
  variance between systems (Zuccon, Moffat, Bailey et al., 2015–2016) [R-mem].
- **Alaofi et al. (2023)** generated variants from UQV100 backstories with GPT-3.5, one-shot (zero-shot gave long
  NL questions) [R] ([paper](https://arxiv.org/html/2501.17981v1)):
  - LLM variants recover similar relevant pools: up to 71.1% overlap at depth 100.
  - But they are **less diverse**: human variant pools were about 191 documents vs about 94–105 for GPT.
- **Breuer (SIGIR-AP 2024)** found that fusing synthetic variants beats single queries and PRF on four TREC newswire
  sets, and that more topic context gives better variants [R] ([2411.03881](https://arxiv.org/abs/2411.03881)).
  Ran et al. (CHIIR 2025) fuse human and LLM variants [R].
- **Persona simulation for ToT** (He et al., SIGIR 2025): persona-based LLM simulation gives ToT queries that rank
  systems like community-QA queries, and it extends coverage to new domains [W, search snippet].
- **Typos and misspellings.**
  - WordPiece models are brittle on misspelled queries. Fixes include CharacterBERT plus self-teaching (Zhuang &
    Zuccon, SIGIR 2022), ToRoDer pre-training (SIGIR-AP 2023) and ToCoTR (LREC-COLING 2024)
    ([ToRoDer](https://arxiv.org/abs/2304.08138)) [R].
  - The cheap equivalent for us is **typo augmentation in the messy register** plus self-teaching: a KL loss between
    the clean and the typo query's score distributions [I].
- **PigMed recipe [I]:**
  - Generate the card from the chunk plus its context.
  - The card's surface queries must not copy distinctive tokens. Cap lexical overlap (BM25 or Jaccard) per register:
    high is fine for expert keywords, low for lay.
  - For **messy**: apply rule-based noise (keyboard-adjacent swaps, dropped vowels, phonetic misspellings of drug
    names, no diacritics, run-ons) *after* the LLM writes a fluent lay query. LLMs write unrealistically clean typos.
  - For **partial recall**: give the LLM only a degraded view (the year ± 2, one finding, a vague population) and
    forbid the title words.

### 3.3 Query difficulty and realism checks

- **Round-trip filter.** Does the current retriever, or BM25, find the source in the top k? It removes unanswerable
  queries, but it **biases toward easy, lexical queries**. Apply it per register, with a looser threshold for lay,
  messy and partial recall [I].
- **Validating simulated queries.** Kruff, Bernard & Schaer (2026;
  [2601.11412](https://arxiv.org/abs/2601.11412)) review measures for checking simulated queries against real ones,
  organise them into a taxonomy, and release a library [R]. Once we have logs, compare length, content words, OOV and
  typo rate, question vs keyword share, and entity density against real queries [I].
- **Known realism failure modes.**
  - LLM simulators show encyclopaedic domain knowledge and unusual patience (Google ConvApparel, 2026) [W].
  - LLM variants are less diverse than human variants (Alaofi 2023) [R].
  - Persona text adds less diversity than explicit constraints (Kambhatla 2025) [R].
- **Difficulty labels.** Score each query with query performance prediction (QPP; for example the score gap between
  the top-1 and top-10 hits) and keep a target share of hard queries per register [I].

### 3.4 Labelling relevance by intent rather than by lexical match

- **Grade against the card, not the query.** The judge sees the intent, the narrative and the candidate. Thomas et al.
  (Bing, SIGIR 2024; [2309.10621](https://arxiv.org/abs/2309.10621)) tested 32 prompt variants on Robust04 [R]:
  - The best prompt (description + narrative + aspects) reached κ ≈ 0.64; the worst (role + multiple judges) ≈ 0.20.
  - Adding the narrative was worth about +0.06 κ; adding the description about +0.01.
  - At Bing, LLM labels beat third-party human labellers at a fraction of the cost, and trained better rankers.
- **Several positives, graded.**
  - Gecko relabelling (~15% of positives change) [R].
  - Perplexity v2 converts teacher token scores into **soft chunk targets with a KL loss**. Their argument: a single
    gold chunk treats useful supporting context as negative and ties labels to chunk boundaries
    ([blog](https://www.perplexity.ai/hub/blog/contextual-embedding-beyond-the-gold-passage)) [R].
- **Lexical-overlap bias in judges.** LLM judges call passages relevant just because they contain the query words
  ([Alaofi et al., SIGIR-AP 2024](https://arxiv.org/abs/2501.17969)) [R]. Grading against the *card* rather than the
  surface query should reduce this. Still audit a sample of high-overlap positives [I].
- **Within-paper false negatives.** Mask sibling chunks and duplicate-group papers as negatives (already planned in
  `RETRIEVER_DATA_PLAN.md`).

## 4. Query-side techniques at search time

### 4.1 Rewriting, HyDE, query2doc, multi-query

The numbers for HyDE, query2doc and Jagerman et al. are in `SEARCH_TECHNIQUES_2026.md` §5. New evidence:

- **Elastic Search Labs.** Template-based LLM rewriting with Claude 3.5 Sonnet and Haiku
  ([blog](https://www.elastic.co/search-labs/blog/query-rewriting-llm-search-improve)) [R]:
  - BM25 on a BEIR subset: 0.346 → 0.364–0.368 nDCG@10 with lexical keyword enrichment.
  - **Replacing** the original query lowered recall in hybrid RRF. Adding rewrite terms *alongside* the original gave
    +1–3 points of Recall@50 (tuned on test, so an upper bound).
  - Their conclusion: "hybrid search is harder to beat".
- **Weaviate Query Agent "Search Mode"** (decomposition, expansion, filters, LLM reranking)
  ([benchmark](https://weaviate.io/blog/search-mode-benchmarking), [effort tiers](https://weaviate.io/blog/search-mode-effort)) [R, vendor]:
  - BRIGHT Biology nDCG@10: 0.13 → 0.40, and 13.0 → 57.5 at "ultrahigh" effort.
  - SciFact: 0.71 → 0.78. The smallest gains (+5%) were on SciFact and BRIGHT Economics.
  - Gains are large where intent is buried and small where queries are already precise.
- **When it hurts.**
  - Weller et al. (EACL Findings 2024): the stronger the retriever, the smaller or more negative the gain [R].
  - Abe et al. (SIGIR 2025): unfamiliar and ambiguous queries [R].
  - Elastic: dropping the original query [R].
  - Our data fits all three: **+0.08 nDCG on realistic messy queries, ~0 on clean LLM queries** [P]. Clean LLM
    queries are already in the retriever's distribution.
- **Multi-query fusion.** Fusing LLM variants helps on TREC newswire (Breuer 2024) [R]. Controlled evidence with
  strong dense retrievers is thin [W].

### 4.2 LLM query understanding (industry pattern)

- Weaviate, Elastic and Vespa converge on one pipeline [R]:
  1. Parse the intent into structured parts: filters (date, article type, species), entities, and the sub-question
     list.
  2. Rewrite only the free-text part.
  3. Keep the original query.
  4. Rerank.
- Vespa exposes this as "searchers" in a search chain, which can call a local or remote LLM
  ([Vespa and LLMs](https://blog.vespa.ai/vespa-and-llms/)) [R].
- **PigMed [I]:**
  - Do the filter extraction (years, "RCT", "review", "children", species) with rules and a small Rust classifier.
  - Use the LLM only for the lay or messy text-to-expert step.

### 4.3 Routing by intent type

- Adaptive-RAG (Jeong et al., NAACL 2024) routes by question complexity with a small classifier [R-mem]. The same idea
  applies to retrieval pipelines.
- **Proposed PigMed router [I]:**

| Type | Detector | Action |
|---|---|---|
| ID or citation (PMID, DOI, NCT, rsID) | regex | exact lookup |
| Entity or keyword (1–4 tokens, MeSH or gene hit) | lexicon | BM25-heavy fusion, MeSH expansion |
| Expert NL question | default | dense (+ late interaction) |
| Lay or consumer | CHV hit, no MeSH hit, question form, second person | lexicon expansion; LLM rewrite if enabled; prior toward plain-language and review articles |
| Messy | OOV rate, edit-distance hits on the lexicon | spell-normalise against MeSH and drug names, then as lay |
| Partial recall / ToT | long, hedged ("I think", "a while ago"), descriptive | dense plus LLM rewrite, wider k, date-hint filter |

## 5. Industry work

| Org | What is relevant | Provenance / licence | Applicable to an open, CPU-served biomedical engine? |
|---|---|---|---|
| **Perplexity** | **v1** ([2602.11151](https://arxiv.org/abs/2602.11151)): diffusion-pretrained **Qwen3-0.6B/4B** backbones; persona-based synthetic queries from Qwen3-30B-A3B; no instruction prefixes needed [R]. **v2 context** ([blog](https://www.perplexity.ai/hub/blog/contextual-embedding-beyond-the-gold-passage)): a query-aware context-compression model acts as a **relevance teacher**. It scores tokens, the scores become soft chunk targets, and the student trains with KL plus a document contrastive loss. The teacher is used only at training time. Starts from an in-house 9B ColBERT [R]. | v1 **CN** (Qwen3). The v2 lineage is not disclosed; a third-party listing says MIT [W]. | **Yes, the recipe:** graded teacher targets (Rec. 4). The weights: no (CN or unclear, and 9B). |
| **LightOn** | **DenseOn / LateOn** (Apr 2026; 149M; Apache-2.0): BEIR 56.20 / 57.22, and LateOn 60.36 after decontamination [R]. The English fine-tune mixes in **MS MARCO**. mDenseOn/mLateOn use an mmBERT base, an mxbai-rerank-large-v2 teacher, gte-modernbert mining for code, and Qwen3-32B translation ([HF blog](https://huggingface.co/blog/lightonai/mdenseon-mlateon)) [R]. **PyLate**: ColBERT training library [R-mem]. **Reason-ModernColBERT**: NC (above). | **MSM**, and **CN** in the teacher and mining chain. | PyLate is usable for our late-interaction head. DenseOn/LateOn are good baselines, but not clean training parents. |
| **Weaviate** | Query Agent: intent parsing, filter extraction, decomposition, reranking; Search Mode effort tiers. Benchmarks above [R]. | Hosted product. | Pattern only. Its latency budget (several LLM calls) does not fit a default path on 2 vCPU; it fits an opt-in "deep search" [I]. |
| **Contextual AI** | ctxl-rerank-v2-instruct (1B, 2B, 6B): an **instruction-following reranker** ([blog](https://contextual.ai/blog/rerank-v2)). | **CC BY-NC-SA 4.0** (**NC**) [R]. The base is not named on the cards; it is reportedly Qwen3-based [W]. | No (NC). The idea (instructions at the reranker, not the embedder) is applicable [I]. |
| **Cohere** | Rerank 3.5; Rerank 4 (Dec 2025, Pro and Fast, 32K context, "stronger reasoning") [R]. Instruction support for Rerank 4 was not confirmed [W]. | API, closed. | No (closed); a useful judge baseline. |
| **Voyage (MongoDB)** | rerank-2.5 / 2.5-lite (Aug 2025): **instruction-following**; +12.7% over Cohere 3.5 on MAIR (vendor) [W]. | API, closed. | No. Confirms the market puts instructions in the *reranker*. |
| **Jina (Elastic since Oct 2025)** | Elastic completed the acquisition on 9 Oct 2025 [R]. jina-embeddings-v4 is built on Qwen2.5-VL-3B and is **NC** [R-mem]; jina-reranker-m0 is Qwen2-VL-based [R-mem]; jina-embeddings-v3 is CC BY-NC [R-mem]. v5-text (0.2B/0.6B) base not checked. | **CN** and/or **NC**. | No. |
| **Google** | **Gecko** (task description + query + LLM relabelling) [R]. Gemini Embedding uses Gemini for data filtering, positive and negative mining, and synthesis ([2503.07891](https://arxiv.org/abs/2503.07891)) [R-mem]. **EmbeddingGemma** task prompts (`task: search result \| query: …`) [R-mem]. Vertex AI Search offers managed spell correction, query expansion and LLM-based query understanding [W]. | Gemma licence for EG2 (our base). | Yes, the Gecko recipe and the task-prefix convention. |
| **Microsoft** | Bing LLM relevance labelling (Thomas et al.) [R]; MIMICS clarification data [R-mem]; TREC ToT co-organisation; E5 attribute prompts [R]; SPEED co-authors. | Papers; data licences vary. | Yes, the judging prompt design (Rec. 4, 8). |
| **Elastic** | The query-rewriting study above; Jina models in its Inference Service. | | The finding "keep the original query; hybrid is hard to beat" transfers directly. |

## 6. Biomedical specifics

### 6.1 Clinician vs patient queries: datasets

| Dataset | Intent signal | Licence / use |
|---|---|---|
| **TREC CDS 2014–2016** | Case reports. In 2016, the same patient appears as a MIMIC admission **note**, a **description** and a **summary**: one need at three surface forms. The raw notes retrieved worst [R-mem]. | TREC terms; MIMIC-derived text (2016) carries PhysioNet terms. Eval only. |
| **TREC PM 2017–20, Clinical Trials 2021–23** | Structured (disease, gene, demographics) → PICO-like intent. | TREC terms; eval. |
| **TREC-COVID** | Each topic has a *query*, a *question* and a *narrative*: a ready intent card with 3 surface forms. | Eval. |
| **MedQuAD** (Ben Abacha & Demner-Fushman 2019) | 47,457 QA pairs from 12 NIH sites, with question types (treatment, symptoms, …). | **CC BY 4.0**. Answers were **removed** from 3 subsets (A.D.A.M., MedlinePlus drugs, herbs) for copyright [R] ([repo](https://github.com/abachaa/MedQuAD)). Usable for consumer question *styles*. |
| **MeQSum, LiveQA-Med (TREC 2017), CHQ sets** | Real consumer health questions (long, with emotional context and misspellings) paired with expert summaries: a natural **lay surface → expert intent** pair. | NLM research releases; check each licence (**?**). |
| **iCliniq / HealthCareMagic** (via ChatDoctor) | Real patient phrasing. | Scraped; licence **?**. Internal style reference only; do not train on or redistribute [I]. |
| **CLEF eHealth CHS 2013–2021** | Lay queries. The 2015–2016 rounds had several people write query variants for the same need [R-mem]. | Research licence; eval. |
| **NFCorpus** | Lay NutritionFacts queries against PubMed. | Eval (in BEIR). |
| **R2MED, IFIR-healthcare, MAIR-medical, BRIGHT-biology** | Reasoning-heavy and instruction-heavy medical needs. | Eval; check each licence. |

### 6.2 PICO-structured intent

- PICO (Population, Intervention, Comparison, Outcome) is the clinician's intent schema. PubMed Clinical Queries and
  EBM-NLP (Nye et al. 2018) operationalise it [R-mem].
- Recent work [W]:
  - PICOs-RAG ([2510.23998](https://arxiv.org/abs/2510.23998)) adds a PICO extraction step after LLM expansion.
    Reported accuracy +6.2–8.6%, but on Chinese medical QA with LLM judges.
  - DeepRetrieval ([2503.00223](https://arxiv.org/abs/2503.00223)) trains an RL query rewriter on PICO-framed
    PubMed search.
- **PigMed [I].** Put optional PICO slots in clinician intent cards. Generate clinician surface queries from *partial*
  PICO (users rarely type all four). Use PICO extraction at search time only as a filter or boost (population →
  age-group MeSH; "RCT" → publication type), not as a rewrite.

### 6.3 Mapping lay terms to MeSH and UMLS: licensing

- **MeSH:** free from NLM under its terms of use (no implied endorsement; keep the data current). Entry terms already
  hold many lay synonyms ("heart attack"). Safe to ship as a lexicon [R-mem].
- **UMLS Metathesaurus:** a UMLS licence is required. Sources carry restriction levels 0–4 plus SNOMED CT terms
  ([appendix](https://nlm.nih.gov/research/umls/knowledge_sources/metathesaurus/release/license_agreement_appendix.html)) [R].
  - **CHV is level 0**: only the general UMLS terms apply [R]. The GW CHV Initiative also calls it open access [W].
  - Redistributing a derived lay→CUI table in an open release still falls under the UMLS licence terms. Get a
    one-line clearance from NLM, or ship only CHV-string → MeSH-heading pairs [I].
  - **Exclude SNOMED CT, MedDRA and other level 1+ sources** from anything we ship [I].
- CHV dates from 2011, so it lacks newer lay terms ("long COVID", "Ozempic"). Have the generator propose lay
  variants, and keep the ones that map back to the right MeSH heading via the retriever [I].

## 7. Evaluation: did it satisfy the intent?

- **Grade against an intent description.**
  - TREC practice (narratives) and Thomas et al. show that judges, human or LLM, agree better when they see the
    relevance criteria (+0.06 κ from the narrative) [R].
  - Open reproductions of the Bing prompt (UMBRELA, TREC RAG 2024) make this practical without Bing [R-mem].
- **LLM judges: limits.**
  - They inflate relevance when query words are present (Alaofi et al. 2024) [R]; follow-up work finds this across
    model families (2026, [2602.17170](https://arxiv.org/abs/2602.17170)) [W].
  - Their *non-relevant* labels are more reliable than their *relevant* labels [R].
  - Use a model **different** from the generator, hide the surface query or show it *after* the card, and
    human-audit about 5% [I].
- **Single-target metrics fail at scale.**
  - MS MARCO dev: about 94% of queries have exactly one known relevant passage. Assessors often **prefer the neural
    ranker's top result over the labelled one**, so MRR stops tracking real improvements
    ([Arabzadeh et al. 2022](https://arxiv.org/abs/2109.00062)) [R].
  - Gecko's 15% relabel rate and Perplexity's gold-passage critique say the same for synthetic data [R].
  - At 9M PMC articles, many sibling chunks and other papers satisfy the same intent. One-source-chunk targets punish
    correct answers. **Use pooled, graded, intent-judged qrels**, report judged@10, and treat unjudged ≠ non-relevant
    (condensed lists) [I, consistent with our 9M-scale observation].
- **Measure robustness across surface forms.**
  - **Robustness@10** (InstructIR): the minimum nDCG over an intent's variants [R].
  - The gap between the best and worst register per intent.
  - Per-register nDCG@10 (expert, lay, messy, partial recall). These expose the gap that an average hides.
- **Aspect and nugget coverage** for complex intents. DORIS-MAE (NeurIPS 2023) decomposes 100 long scientific queries
  into aspects and uses LLM aspect-level annotation at about 1/500 of the cost [R]
  ([2310.04678](https://arxiv.org/abs/2310.04678)); licence **?**. TREC RAG nugget evaluation is similar [R-mem].
- **User-satisfaction proxies (after launch).**
  - Query reformulation is a stronger dissatisfaction signal than clicks alone (Hassan et al., CIKM 2013) [R-mem].
  - Also track abandonment (good vs bad), time to first click, click depth, and "pogo-sticking".
  - PubMed's Best Match learned to rank from aggregate clicks (Fiorini et al., PLoS Biol 2018) [R-mem].
  - Collect only aggregated, consented signals [I].
- **Synthetic eval sets are valid for *ranking systems* but not for absolute scores.** In TREC ToT 2025, synthetic and
  real queries ranked systems similarly (τ 0.74–0.85), but MS-ToT real queries were *harder* [R]. Keep real-query
  sets (TREC-COVID, NFCorpus, CLEF eHealth, R2MED) as anchors.

## 8. Personas for synthetic query diversity

### 8.1 Persona sources

| Source | How it was built | Licence / provenance | Use? |
|---|---|---|---|
| **NVIDIA Nemotron-Personas-USA** ([HF](https://huggingface.co/datasets/nvidia/Nemotron-Personas-USA)) | 1M synthetic US adults. Demographics are sampled from a proprietary PGM fitted to US Census distributions (with simplifying independence assumptions). Narrative fields (professional, hobbies, skills, career goals, …) cover 567 occupations. Built with NeMo Data Designer; text from **gpt-oss-120b** (v1.1; v1.0 used Mixtral-8x22B). Adults only; finance and healthcare areas excluded [R]. | **CC BY 4.0**; the card says it may train any model, commercially [R]. | **Yes**, for demographic texture (age, sex, education, occupation, region). Health axes must be added by us, since the set excludes healthcare. |
| Nemotron-Personas UK, SG, JP, BR, IN, … | Same approach, census-grounded per country (the UK version uses IPF over suppressed tables) [R]. | **NVIDIA Dataset License** on the NGC "extended" pages (some say commercial-ready) **?** [R]. | Maybe, for non-native English personas; read the licence first. |
| **PersonaHub** (Tencent AI Lab Seattle; [2406.20094](https://arxiv.org/abs/2406.20094)) | 1B personas via Text-to-Persona (from web text) and Persona-to-Persona. Deduplicated with MinHash (0.9) and text-embedding-3-small cosine (0.9). The paper names GPT-4, Llama-3 and Qwen as the LLMs used, without per-step detail. Math data from GPT-4 and gpt-4o [R]. | Released subset **CC BY-NC-SA 4.0** (**NC**); Tencent (**CN**); generated partly with Qwen and OpenAI models [R]. | **No.** The *method* (Text-to-Persona from PMC itself, giving "the kind of reader this article serves") can be reproduced with our clean generator [I]. |
| **FinePersonas** (Argilla, 2024) | About 21M personas from FineWeb-Edu via Llama-3.1-70B-Instruct (PersonaHub method) [R]. | Licence field blank; derived sets tagged llama3 or llama3.1 **?** [R]. | Avoid unless the licence is clarified. |
| **Self-built from PMC** [I] | Text-to-Persona on our own chunks: "who would search for this?" | Clean if the generator is clean. | **Yes.** It doubles as the role-given-doc prior. |

**Karpathy on collapse.** In the Dwarkesh Podcast (17 Oct 2025), Karpathy says model samples are "silently
collapsed". They occupy a tiny manifold of possible outputs: each sample looks fine, but the distribution is narrow.
Ask for a joke and you get "like three jokes"
([transcript](https://www.dwarkesh.com/p/andrej-karpathy), ~00:52) [R].

- He argues that humans keep far more entropy, and that labs should regularise for it.
- We found **no primary source where Karpathy recommends personas specifically**. Persona-for-diversity is the
  PersonaHub and Nemotron framing, not his [R: absence in the sections checked].
- The practical lesson for us: diversity must be **injected and measured**. Re-sampling at a higher temperature
  does not provide it [I].

### 8.2 Axes for biomedical search

The **role** sets plausible intents. **Modifiers** set the surface form [I].

| Axis | Values | Main effect on the query |
|---|---|---|
| Role | physician (by specialty), nurse, pharmacist, allied health; biomedical researcher; data or bioinformatics scientist; student (pre-med, med, grad); patient; caregiver or parent; librarian or systematic reviewer; journalist; policymaker or payer | Intent types: treatment comparison, mechanism, dosing, prognosis, evidence synthesis, methods, "is it true that…" |
| Health literacy | low, medium, high (orthogonal to role for lay users) | Vocabulary register, question vs keyword |
| Language | native English; non-native (L1 hints: Spanish, Chinese, Hindi, Arabic, Portuguese); translated-in-head phrasing | Word order, article dropping, false friends, transliterated drug names |
| Urgency | browsing, task-driven, worried or urgent | Length, emotion words, symptom-first phrasing |
| Device and typing | desktop, mobile thumb, voice dictation | Typos, no punctuation, homophones (voice) |
| Age band | adult (default), older adult, teen (rare, mostly self-searching), child (only via a caregiver role) | Plausibility only, not style stereotypes |
| Gender | only where clinically relevant (pregnancy, prostate, …) | Intent plausibility; avoid stereotyped phrasing |
| Recall state | knows the term, knows the concept but not the term, half-remembers a specific paper | Keyword vs descriptive vs ToT |

### 8.3 Order and combination

- **Persona → intent** (PersonaHub style): wide user coverage, but many personas have no plausible need for a given
  chunk. You then need retrieval to find matching documents, and you waste calls [I].
- **Intent → persona** (doc-first, our pipeline):
  1. Chunk → card, with the intent type.
  2. Role ~ P(role | intent type, article type), from a hand-set table refined by an LLM plausibility score.
  3. Modifiers ~ P(modifier | role).

  This keeps every query grounded in a document and avoids implausible pairs [I]. **Recommended.**
- **Joint sampling with rejection:** sample (role, modifiers) from the marginals, then let the generator *reject*
  ("no plausible need for this user in this passage") and resample. It costs about 10–20% more calls, but it catches
  mismatches such as a 9-year-old asking about CRISPR off-target assays [I].
- **Combinatorics:** 13 roles × 3 literacy levels × 3 language levels × 3 urgency levels × 3 devices × 6 registers ≈
  6k cells. Do not fill the cross product. Instead [I]:
  - set **quotas on each marginal**, weighted toward expected traffic, with a floor (≥2%) for rare roles;
  - require **all pairwise combinations** of role × register and register × language to appear (covering-array
    style);
  - stratify per batch, so every training batch sees the mix.

### 8.4 Register mix and evidence

- **Direct evidence that persona conditioning improves retriever training is weak** [W]:
  - pplx-embed v1 used personas but reports no ablation.
  - A 2026 position-bias study uses PersonaHub persona + difficulty + length as a sampling configuration, with no
    persona ablation ([2605.26578](https://arxiv.org/abs/2605.26578)).
- **Adjacent evidence is moderate:**
  - Mixing query *types* beats any single type (Tamber 2025) [R].
  - Diversity pays off for complex queries (Feng 2026) [R].
  - Persona prompting raises lexical diversity, but fine-grained persona detail adds little beyond explicit length or
    register constraints (Kambhatla et al., EMNLP Findings 2025, [2505.17390](https://arxiv.org/abs/2505.17390)) [R].
  - Our own +0.08 on messy queries shows the traffic gap exists [P].
- **Starting ratio (Rec. 3):** about 45 / 25 / 15 / 7 / 8 (professional / lay / messy / partial recall / instruction)
  [I].
- **Ablation:** set lay + messy to 0%, 20%, 40% and 60%; watch the per-register eval *and* NanoBEIR or TREC-COVID
  expert performance for regressions.

### 8.5 Measuring diversity

Track these per batch and per register [I, metrics R-mem]:

- **Lexical:** distinct-1 and distinct-2, compression ratio (gzip) of the concatenated queries, self-BLEU.
- **Semantic:** mean pairwise cosine distance and Vendi score of query embeddings, *within* an intent card. Across
  cards this mostly measures topic spread.
- **Coverage:** the share of filled cells in the role × intent-type × register grid; MeSH tree coverage of the target
  documents.
- **Realism:** distributions of length, content words, OOV and typo rate, and source-overlap (BM25/Jaccard with the
  chunk), compared with real-query references (CLEF eHealth, MeQSum, NFCorpus, and later our logs).
- **Usefulness:** per-register nDCG and Robustness@10 on held-out cards. Diversity that does not move these is noise.

---

## Source list (primary links)

- Concepts:
  - Taylor 1968 (doi 10.5860/crl_29_03_178);
  - Belkin, Oddy & Brooks 1982 (doi 10.1108/eb026722);
  - Furnas et al. 1987 (doi 10.1145/32206.32212);
  - TREC ToT 2025 [2601.20671](https://arxiv.org/abs/2601.20671);
  - OBLIQ-IR [2609.29649](https://arxiv.org/abs/2609.29649);
  - Abe et al. 2025 [2505.12694](https://arxiv.org/abs/2505.12694).
- Instruction-following and reasoning:
  - FollowIR [2403.15246](https://arxiv.org/abs/2403.15246);
  - Promptriever [2409.11136](https://arxiv.org/abs/2409.11136);
  - InstructIR [2402.14334](https://arxiv.org/abs/2402.14334);
  - MAIR [2410.10127](https://arxiv.org/abs/2410.10127);
  - IFIR [2503.04644](https://arxiv.org/abs/2503.04644);
  - BRIGHT [2407.12883](https://arxiv.org/abs/2407.12883);
  - ReasonIR [2504.20595](https://arxiv.org/abs/2504.20595);
  - RaDeR [2505.18405](https://arxiv.org/abs/2505.18405);
  - Rank1 [2502.18418](https://arxiv.org/abs/2502.18418);
  - ReasonRank [2508.07050](https://arxiv.org/abs/2508.07050);
  - R2MED [2505.14558](https://arxiv.org/abs/2505.14558).
- Synthetic data:
  - Promptagator [2209.11755](https://arxiv.org/abs/2209.11755);
  - SPTAR [2307.08303](https://arxiv.org/abs/2307.08303);
  - EGG [2409.16570](https://arxiv.org/abs/2409.16570);
  - Gecko [2403.20327](https://arxiv.org/abs/2403.20327);
  - E5-Mistral [2401.00368](https://arxiv.org/abs/2401.00368);
  - SPEED [2410.18634](https://arxiv.org/abs/2410.18634);
  - Tamber et al. [2505.19274](https://arxiv.org/abs/2505.19274);
  - Feng et al. [2602.09448](https://arxiv.org/abs/2602.09448);
  - Alaofi et al. 2023 [2501.17981](https://arxiv.org/abs/2501.17981);
  - Breuer 2024 [2411.03881](https://arxiv.org/abs/2411.03881);
  - ToRoDer [2304.08138](https://arxiv.org/abs/2304.08138);
  - Kruff et al. [2601.11412](https://arxiv.org/abs/2601.11412).
- Judging and eval:
  - Thomas et al. [2309.10621](https://arxiv.org/abs/2309.10621);
  - Alaofi et al. 2024 [2501.17969](https://arxiv.org/abs/2501.17969);
  - Arabzadeh et al. [2109.00062](https://arxiv.org/abs/2109.00062);
  - DORIS-MAE [2310.04678](https://arxiv.org/abs/2310.04678).
- Industry:
  - pplx-embed [2602.11151](https://arxiv.org/abs/2602.11151), [v2 blog](https://www.perplexity.ai/hub/blog/contextual-embedding-beyond-the-gold-passage);
  - [LightOn mDenseOn/mLateOn](https://huggingface.co/blog/lightonai/mdenseon-mlateon);
  - [Weaviate Search Mode](https://weaviate.io/blog/search-mode-benchmarking);
  - [Elastic query rewriting](https://www.elastic.co/search-labs/blog/query-rewriting-llm-search-improve);
  - [Contextual AI rerank v2](https://contextual.ai/blog/rerank-v2);
  - [Elastic–Jina](https://ir.elastic.co/News--Events/news/news-details/2025/Elastic-Completes-Acquisition-of-Jina-AI-a-Leader-in-Frontier-Models-for-Multimodal-and-Multilingual-Search/default.aspx).
- Biomedical:
  - [MedQuAD](https://github.com/abachaa/MedQuAD);
  - [CHV in UMLS](https://nlm.nih.gov/research/umls/sourcereleasedocs/current/CHV/metadata.html);
  - PICOs-RAG [2510.23998](https://arxiv.org/abs/2510.23998);
  - DeepRetrieval [2503.00223](https://arxiv.org/abs/2503.00223).
- Personas:
  - [Nemotron-Personas-USA](https://huggingface.co/datasets/nvidia/Nemotron-Personas-USA);
  - PersonaHub [2406.20094](https://arxiv.org/abs/2406.20094);
  - Kambhatla et al. [2505.17390](https://arxiv.org/abs/2505.17390);
  - [Karpathy on Dwarkesh](https://www.dwarkesh.com/p/andrej-karpathy).
