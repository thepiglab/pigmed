# Synthetic query recipe v3: intents, personas, surface forms (draft, 2026-11-17)

Status: **design and pilot plan only.** Nothing has been generated with v3. Mass generation is on hold.
Companion research: `docs/INTENT_VS_QUERY_RESEARCH.md` (still landing; this draft will be reconciled with it).
Previous recipe: v2 (`rust/pigmed-synth/src/recipe.rs`): one call per article, slot specs, 5 brainstormed personas.

## 1. Why v2 is not enough

A hand-read of 16 random benchmark queries found three failure modes:

| Failure | Example | Cause in v2 |
|---|---|---|
| Document-referential wording | "Can I trust the findings of *this study*…", "mean lesion size in *the crude ES group*" | The writer sees the passage and reuses its local labels. The v2 regex catches "this study" but not study-specific group names. |
| Vague needs with many right answers | "What is necessary for effective communication between all agents…", "Why heat protein lysates before gel electrophoresis?" | `open_ended` / `mechanism_why` slots let the writer drift to textbook facts that thousands of papers answer. A single positive then teaches false negatives. |
| Exam framings | "You need… / You are planning…" (Inkling c2) | The `scenario` format plus persona slots read as a vignette instruction. |

Underneath all three is one problem. v2 asks one model, in one step, to decide *what is being asked* and *how a
person types it*, while it is looking at the passage. So the surface text inherits the passage's framing, and
personas collapse into one register. (Karpathy's point: without explicit persona diversity, outputs converge on a
single "assistant" style.)

## 2. Design in one picture

```
article + marked passages
      │  Stage A: intent call (sees the article)
      ▼
intent cards: standalone need, answer, specificity, audience, entities
      │  filter: standalone? answered? specific?  (regex + judge on cards: few, cheap)
      ▼
(intent × persona) pairs  ← persona library (deterministic sampler, ~20k cards)
      │  Stage B: surface call (does NOT see the passage)
      ▼
2–3 surface queries per pair, per the style schedule
      │  Stage C: programmatic noise (typos, casing, punctuation), deterministic
      ▼
EG2 round-trip + dedup + template caps → kept queries (positives; generic intents → multi-positive or drop)
```

The key lever is that **the surface writer never sees the passage.** It sees only the standalone need, its key
entities and a persona card. That removes the path by which "the crude ES group" and copied phrasing get into
queries, and lets the same need come out as an expert keyword string, a caregiver's question or a half-remembered
lookup.

## 3. Persona library

**Source.** Our own deterministic sampler, not a downloaded set.
- NVIDIA Nemotron-Personas (CC BY 4.0) is a reference for the *method*: attribute sampling grounded in census-like
  distributions, then an LLM-written bio. It is US-only, and its generator lineage must be checked against the
  no-Chinese-weights rule before any of its data is used. We copy the idea, not the rows.
- A persona = a set of attributes plus a short rendered card. For the pilot, cards are rendered from a template
  with no LLM. LLM-written bios (Gemma) are a later ablation (§7, arm E).

**Attributes** (sampled with compatibility rules, e.g. a high-school student has expertise ≤ 1):

| Attribute | Values (abridged) |
|---|---|
| Role group / role | Clinician: GP, ED physician, hospitalist, surgeon, oncologist, psychiatrist, paediatrician, nurse, NP, pharmacist, dentist, physiotherapist, midwife, paramedic, dietitian. Researcher: wet-lab biologist, bioinformatician, epidemiologist, trialist, PhD student, postdoc, PI. Student: medical, nursing, pharmacy, undergrad biology, high school. Lay: patient (new diagnosis, chronic condition, worried well), caregiver (parent, adult child, partner). Other: medical librarian / systematic reviewer, health journalist, policy analyst, pharma medical-affairs, regulatory writer, health-app developer, veterinarian, teacher. |
| Age band | 16–19, 20–29, 30–44, 45–64, 65+ |
| Gender | woman, man, non-binary (sampled; only shapes voice where natural) |
| English | native; fluent L2; limited L2 (with first language: Spanish, Portuguese, Hindi, Arabic, French, German, Vietnamese, Tagalog, Korean, Swahili, …) |
| Health literacy | low, medium, high |
| Domain expertise | 0 none, 1 some, 2 professional, 3 specialist in this area |
| Goal | decide for a patient/self, learn/understand, verify a claim, find a method, write a review/grant, find a number to cite |
| Context | phone (voice or thumb typing), desktop search box, PubMed-style database, chat assistant |
| Habits | terse vs verbose; lowercase; no punctuation; typo rate (0 / low / high); uses abbreviations; uses brand vs generic drug names |

**Size.** About 20k cards: enough that a mass run of ~1M articles rarely repeats a persona on the same topic.
Personas are keyed by a stable id, so every query records which persona produced it.

**Target mix** (by query, after matching; tunable): clinicians 30%, researchers 30%, students 12%, patients and
caregivers 20%, other 8%. Of all queries: L2 English ≥ 15%, low health literacy ≥ 12%.

## 4. Stage A: intent cards (one call per article)

The model reads the whole article (same marked-passage layout as v2, so prefix caching still works) and writes 3–5
intent cards per marked passage:

```json
{"passage": "P2", "need": "Whether low-dose colchicine reduces recurrent stroke after a lacunar infarct",
 "type": "result_number", "answer": "No significant reduction at 2 years (HR 0.93)",
 "specificity": "few", "audience": ["clinician", "researcher", "patient"], "min_expertise": 0,
 "entities": ["colchicine", "lacunar stroke", "recurrence"], "skip": false}
```

Rules in the prompt:
- `need` is one sentence a stranger understands without the article. Name drugs, genes, organisms, populations
  and assays. No study-local labels (arm names, group codes, cohort nicknames, "the crude ES group"). Use public
  dataset or trial names only if a searcher would know them (e.g. "UK Biobank", "SPRINT").
- `specificity`: how many papers would answer this need. `unique` (this paper, or nearly), `few` (a handful),
  `many` (textbook or very common knowledge).
- The intent taxonomy (`type`) keeps v2's 10 types so the mixes are comparable. Types are scheduled per passage
  as in v2, but as *targets for needs*, not for surface forms.

**Card filter** (cheap: about 4 cards per passage instead of ~10 queries):
1. Regex for document references, extended from v2: this/these/the present study|trial|cohort|patients|group(s);
   "the researchers", "in the experiment", "the above". Also a rule that flags "the ⟨X⟩ group|arm|cohort" unless X
   is a population word (e.g. "control", "placebo", "elderly").
2. Judge on the card, same model, a short call with no article: *standalone?* (yes/no) and *specificity* re-rated.
   Cross-model judging stays an option for the A/B (§7).
3. `specificity = many` → *generic intent*: see §6.

## 5. Stage B: matching personas and writing surface forms

**Matching (intent-first, arm B).** For each kept card, draw 1–2 personas whose role group ∈ `audience` and whose
expertise ≥ `min_expertise`. With probability 0.1, allow a *stretch*: a lay persona with an expert need, phrased
in lay words. Real patients do look up mechanisms of their own drugs.

**Persona-first (arm C).** Draw a persona from the target mix first. Give it the article and ask which passages'
needs *this* person would plausibly have. "None" is allowed, and its rate tells us coverage. This tests the open
question (persona-first vs intent-first) directly.

**Surface call** (one call per article, batching all pairs; **no passage text**). Input per pair: the persona
card, the need, the entities, and 2–3 surface styles drawn from a schedule constrained by the persona:

| Style | What | Allowed for |
|---|---|---|
| expert_terse | keyword-ish professional string ("colchicine secondary prevention lacunar stroke") | expertise ≥ 2 |
| natural_question | plain question | all |
| keyword | 2–5 terms, as typed into a search box | all |
| lay_paraphrase | jargon replaced by everyday words ("gout pill to stop another mini stroke") | literacy ≤ medium, or stretch |
| half_remembered | an incomplete or slightly wrong detail ("that colchicine heart trial… does it work for strokes") | all |
| scenario | 1–2 sentences of the person's situation, then the need (never "You are…") | clinicians, patients, caregivers |
| non_native | grammar and word choice of an L2 speaker, still understandable | L2 personas |

Prompt rules carried over from v2: never "this study", "the authors", "Figure 2"; vary openers; no
"You need/You are planning"; `skip` allowed.

**Stage C: programmatic noise** (Rust, deterministic per query). Applied by persona habit rather than asked of the
LLM, because LLM-made typos look fake:
- keyboard-adjacent substitutions, dropped and doubled letters, transpositions;
- phonetic drug and disease misspellings from a small table (e.g. "metforman", "alzhimers");
- lowercasing and dropped punctuation.

Each query stores both the clean form and the noisy form. Training uses the noisy form at the persona's rate.

**Sizing per passage.** About 4 cards → about 3 kept → 1.5 personas → 2 styles ≈ 9 queries before the EG2
filter. That is similar to v2's 4 slots + answers per passage × 5 passages, and it can be capped per article.

## 6. Filtering and generic intents

1. Query-level regex (the same document-reference and exam-framing rules), near-duplicate removal, and a cap on
   any opener bigram at 3% of kept queries.
2. **EG2 round trip**, with the gate depending on style:
   - expert styles: source chunk in top-3, as in v2;
   - lay, half-remembered and non-native styles: source *article* in top-10.

   Hard lay queries are the point, and v2's gate would discard them. Both gates are reported for every arm, so
   keep rates stay comparable with v2.
3. **Generic intents** (`specificity = many`). In the pilot they are *dropped from single-positive training* and
   counted. The planned use (v3.1) is false-negative masking: their EG2 top-20 neighbours are excluded from the
   negatives. They become multi-positive only when a judge confirms that neighbours answer them. Masking is cheap
   and safe; judged multi-positives cost one judge call per neighbour.

## 7. A/B pilot on the 1k benchmark set

**Set.** The existing bench1k inputs (same articles and passages as the v2 benchmark), so v2 numbers carry over.
Generator for all arms: Gemma 4 26B-A4B NVFP4 on 1 B200.

| Arm | Recipe | Purpose |
|---|---|---|
| A | v2 (layout c2), as benchmarked | baseline (the 10-min Gemma pilot) |
| B | v3 intent-first + personas | main candidate |
| C | v3 persona-first | the open question |
| D | v3 intent-first, *no* personas (style schedule only) | does the persona library itself add diversity? |
| E (optional) | B with LLM-written persona bios | worth the extra call? |

**Metrics** (per arm; the v2 benchmark scripts are extended rather than replaced):

| Metric | How | v3 must… |
|---|---|---|
| EG2 keep rate | v2 gate (top-3 chunk), and the v3 style-aware gate | report both. Lower under the v2 gate is acceptable if the lay share explains it. |
| Document-reference rate | regex + judge on a sample | ≤ 1% (A is the reference) |
| Vague-need rate | judge `specificity = many` on kept queries | ≤ 5% of single-positive queries |
| Exam-framing rate | regex on "You need/You are…/Imagine…" | ≈ 0 |
| Diversity | distinct-2/3; top-opener share; Vendi score and mean pairwise distance on EG2 query embeddings; length and style histograms; persona-attribute entropy | higher than A on Vendi and opener share |
| Realism vs real queries | kNN precision/recall (or MAUVE) of EG2 query embeddings against real query sets used **for evaluation only**: NFCorpus queries (lay), BioASQ/TREC-COVID questions (expert) | closer to both than A |
| **Lay-set retrieval** | 1) Zero-shot base EG2 on a non-circular lay set: plain-language-summary sentences (PLOS author summaries, eLife digests, Cochrane PLS sections in PMC) → their own article, `test` split only, plus NFCorpus. 2) Decisive: EG2 + LoRA pilot (~1 GPU-h per arm, same steps, through the S1 trainer) trained on each arm's kept queries, evaluated on the same lay set, held-out free tasks, and NanoBEIR as a regression guard. | step 2: B or C beats A on the lay set by ≥ 1 nDCG@10, with expert tasks and NanoBEIR within 1 point |
| Hand read | below | B or C preferred on standalone + natural |

**Hand-read sample.** The same 16 articles for every arm, 3 queries per article per arm: 48 per arm, 192 for
A–D. They go into one shuffled, blinded markdown table kept with the run outputs. The arm key is kept in a separate
file. Three readers each score:
- standalone (0/1);
- specific: one or a few papers (0/1);
- natural: someone would type this (1–3);
- plausible for its persona (1–3; persona shown);
- answered by the passage (0/1).

**Decision.**
- Adopt v3 if it wins the hand read and lay-set retrieval with expert retrieval within 1 point.
- Choose between B and C on those same metrics, preferring the cheaper arm if they are within noise.
- If D ≈ B, the persona library is not paying its way; keep only the style schedule.

**Cost.** About 200 articles × 2–3 calls per arm, which is minutes of B200 time per arm. The judge sample is
about 1k cards per arm. The LoRA pilots (~1 GPU-h each) dominate.

## 8. Mass-run impact

- Stage A costs about the same as v2's one call per article (same article prefix, shorter output).
- Stage B is short: no article, around 1.5k prompt tokens.
- The noise pass is CPU-only.
- Expected cost: about 1.2–1.4× v2 GPU time per query. For ~20M raw queries that is roughly 25–70 B200-hours, or
  1–3 days on 3 GPUs. This will be re-measured in the pilot.

## 9. Implementation (Rust, `pigmed-synth`)

- `persona.rs`: attribute tables, compatibility rules, deterministic sampler, card renderer, a `personas build`
  subcommand that writes JSONL, and tests.
- `recipe3.rs`: intent-card prompt and schema; card filter; matching (intent-first and persona-first); surface
  prompt and schema; new layouts `c3if`, `c3pf`, `c3np`. It reuses `post`, `par` and the bench output format.
- `noise.rs`: typo and casing noise with fixed seeds, plus the misspelling table.
- `recipe.rs`: extend the doc-reference regex, add opener caps and the extra metrics (Vendi, doc-ref and exam
  rates, style histogram).
- `filter.rs`: the style-aware EG2 gate.
- Lay eval set builder: plain-language-summary sections from `test`-split articles. This needs a section-title
  scan of the v2 chunks.

Estimated work: about a day of code and tests, then about 1 GPU-hour for arms A–D plus judging.

## 10. Pilot as built (2026-11-17)

- **Code** (`rust/pigmed-synth`):
  - `persona.rs`: 20k-card library, seed `v3`;
  - `recipe3.rs`: layouts `c3if`, `c3pf`, `c3np`, plus `judge3`;
  - `noise.rs`;
  - `pilot3.rs`: analysis.
  - All 22 crate tests pass.
- **Stage A filter in the pilot:** regex for document references, `skip`, and the model's own `specificity = many`
  (counted per arm as `card_outcomes`). There is no separate card-judge call yet.
- **Judge** (`judge3`): the same Gemma model rates every arm's queries *without the source*: standalone,
  specificity, exam framing, naturalness 1–3, register.
  - Because it is self-judging, absolute rates are optimistic.
  - The comparison between arms is fair, since every arm is judged the same way.
- **Vendi:** the order-2 Vendi score is reported (n² / Σ cos², the inverse participation ratio of the Gram
  spectrum), per query on a 4k sample. It needs no eigendecomposition.
- **Real query sets for realism** (evaluation only, never training): NanoNFCorpus queries (lay, 50) and
  NanoSciFact claims (expert, 50). Both are small, so they give a direction, not a precise number.
- **Not yet built:**
  - the plain-language-summary lay set;
  - the LoRA lay-retrieval pilot (the decisive extrinsic test). It runs after the intrinsic results, through
    the S1 trainer.

## 11. Reconciliation with `docs/INTENT_VS_QUERY_RESEARCH.md`

**Where the research agrees with this design:**
- intent card first, then several surface registers (Rec. 1);
- doc → intent → role → modifiers as the recommended order, our arm B (§8.3);
- coarse persona axes with plausibility rules (Rec. 2);
- injected and measured diversity rather than higher temperature (§8.1, Karpathy).

The research rates the evidence that personas *in themselves* help retriever training as weak, and fine-grained
persona detail as adding little beyond register constraints. That is exactly what arm D (no personas, style schedule
only) tests.

**Changes for v3.1** (after the pilot; not in the running pilot):
1. Add a TREC-style **narrative** field to the card: what counts as relevant and what does not. It is reused for
   graded, intent-based relabelling (Rec. 4) and for the held-out intent eval (Rec. 8).
2. Add a 5–10% **instruction-bearing** slice: the query plus a short instruction, and instruction-negative pairs
   (Rec. 5).
3. Set register quotas to the suggested start of about 45 / 25 / 15 / 7 / 8 (professional / lay / messy / partial
   recall / instruction). Today's style weights give roughly 50 / 25 / 12 / 10 / 0 before persona constraints.
   The pilot reports the realised mix.
4. Measure diversity **within a card** (pairs from the same card), not only across the corpus.
5. Add Nemotron-Personas-USA demographic texture (CC BY 4.0, text from gpt-oss-120b) as an option for persona
   bios (arm E). Not PersonaHub (NC, CN), and not FinePersonas until its licence is clarified.

## 12. Update (2026-11-17, after the research doc): what is in the pilot now

- **Already in the running pilot** (§11 items 1, 3 and 4 moved forward):
  - **Narrative:** cards carry a TREC-style `narrative`, plus optional `pico` (population, intervention,
    comparison, outcome).
  - **Instruction style:** a new `instruction` surface style (8%).
  - **Register quotas:** style weights retuned toward 45 / 25 / 15 / 7 / 8.
  - **Within-card diversity:** reported (mean cosine distance between the two surface forms of one card).
- **Still v3.1:**
  - instruction-negative pairs;
  - Nemotron-Personas-USA texture (arm E);
  - graded intent-based relabelling of the top 20–30 with a clean teacher (Rec. 4: the main cost and a strong
    lever). The teacher must not be Claude, because of the Anthropic-terms flag on training labels.
- **Lay set:** `pls-shard` keeps up to 3 sentences of 8–40 words per article from authors' plain-language
  summaries, in held-out articles of the 8 S1 shards. Section names matched: Author summary, eLife digest, Plain
  language summary, Lay summary, …. Gold is every other text unit of that article. It is for evaluation only, and
  for S1's LoRA comparison of arms.
- **Claude judge** (`claude-judge`, evaluation only):
  - **Model and calls:** Claude Sonnet 5.5 through the Message Batches API (raw HTTP; there is no official Rust
    SDK). Thinking `between_tools`, effort low, JSON-schema output.
  - **Sample:** up to 400 kept queries per arm, graded against the passage and, for v3, the card's need. Grades:
    standalone, specificity, naturalness, answered, need-match, register.
  - **Spend cap:** a ledger file caps total spend at $8; a batch whose estimate would cross the cap is never sent.

## 13. Pilot p1 result and recipe v3.1 (2026-11-18)

**Pilot p1 result** (1k benchmark sample, Gemma 4 26B-A4B NVFP4). Persona-first (C) is adopted as v3; it beats v2 on every quality measure.

| Measure | v2 | C (persona-first) |
|---|---|---|
| EG2 keep, same gate | 33% | 45% |
| Generic (Claude eval judge) | 39% | 15% |
| Exam-style (Gemma judge) | 17% | 3% |
| Passage answers the query | 66% | 76% |

**Operational fixes from p1.**
- Stage A's output budget scales with the number of cards (320 tokens per card).
- vLLM must run with `--structured-outputs-config '{"backend":"xgrammar","disable_any_whitespace":true}'`, otherwise guided JSON loops on whitespace.

**v3.1 changes**, from reading p1 by hand. All are in `recipe3.rs`; `RECIPE = "v3.1"` is written into every row.
1. **Need copies.** In p1, natural_question copied the card's need 44–57% of the time. Stage B is now told to use at most three words in a row from the need, and queries with token Jaccard ≥ 0.7 to the need are rejected (`copy`).
2. **Style eligibility.** Keyword style requires expertise ≥ 1, so a novice no longer types lab jargon.
3. **Openers.** half_remembered and scenario get a per-slot opener form (`HEDGES`, `SCENES`) instead of drifting to "I read somewhere" / "I am looking".
4. **L2 rate.** non_native weight is ×2 for limited L2 and ×1 for fluent L2; it was ×4 / ×2, which gave 42% L2 among kept queries.
5. **ID leaks.** Stage B sees opaque `slot-NN` ids. Queries with leaked `P1.c2.q1` / `slot-NN` ids are rejected (`leak`), and so are queries with JSON spilled into the text (`spill`).
6. **True constraints.** Instruction-style constraints are now true of the article by construction. They come from corpus metadata (`annotate-articles`: year, JATS article type, PubMed publication types) and the title (children, mouse, rat, human). Examples: "only RCTs" only for RCTs; "since Y" only if the article is from year ≥ Y. Articles with no true constraint get no instruction slot.
7. **Plausible readers.** Persona-first offers stage A three candidate readers per card. Stage A picks the one most likely to read the article, or `none`. With `none`, stage B is given a field expert: a library researcher described as "working in this article's own field".
8. **Style compliance.** Keyword queries over 7 words or with a "?" are rejected, and so are expert_terse queries over 15 words (`style`). judge3 also reports `style_ok`: whether the query reads as its labelled style.

**Release provenance per row:**
- `recipe`, `generator` (model, revision, server, sampling);
- `persona_id` and `persona_fallback`; `candidates` on the cards;
- `license` and `commercial` of the source article;
- `reject` reason.

Claude evaluation-judge outputs are kept apart from the generations and never enter released data.
