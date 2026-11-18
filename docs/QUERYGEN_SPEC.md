# PigMed query generator: spec (recipes v3.1d → v3.1e, v3.2; 2026-11-18)

The generator writes the synthetic training queries for the PigMed retriever. Everything that defines a query — who
types it, what they want, how they type it, what it is looking for, and which outputs are rejected — lives in one config,
`querygen/querygen.toml`, plus prompt templates in `querygen/templates/`. The code (`rust/pigmed-querygen`) only reads it.

The configs form a chain, each file `extends` the one before it:

| File | Recipe | What it adds |
|---|---|---|
| `v3.1d.toml` | v3.1d | The piloted persona-first text recipe, reproduced exactly: the persona library is byte-identical, and every stage-A/B request and output field is identical (see *Parity*). |
| `fig.toml` | v3.1d-fig | The **figure arm** (about 30% of queries look for a figure) and a larger output budget for reasoning models. Figure cards never use a generic field expert. Text is unchanged, so this is the recipe of the model / precision / reasoning-level grid (§8), comparable with the v3.1d 4-model trial. |
| `v3.1e.toml` | v3.1e | From the v3.1d trial read (§4b): a MeSH / journal **domain rule**, a quota-driven **lay pass**, and a **field → role fallback** with no generic expert. |
| `querygen.toml` | v3.2 | A wider persona library: 154 roles, 35 first languages. |

## 1. Pipeline

```
                     persona library (20k, deterministic, from [personas])
                              │  3 candidate readers per card: lay | clinician/student | researcher/other
 article + marked passages    ▼
 ───────────────────────► STAGE A text (1 call / article) ──► intent cards: reader (a/b/c/none→field expert),
                          4 cards / passage + 1 HEALTH card      need, narrative, PICO, answer, specificity,
                                                                 audience, min expertise, entities
 figures of the article   ┌──────────────────────────────────────────────────────────────────────────┐
 (image ≤1280 px +        │ plan: figure cards sized so figure + composed queries ≈ target share     │
  caption + citing text)  │ (per-input boost, card types per figure: find_figure, finding,           │
 ───────────────────────► │  appearance [scans, micrographs], composed [needs a citing chunk])       │
                          └──────────────┬───────────────────────────────────────────────────────────┘
                                         ▼
                          STAGE A figure (1 call / figure, WITH the image) ──► figure kind, legible,
                                         figure need cards (reader, need, visible, answer, …)
                                         │
       card filter: off_domain · skip · doc_ref · generic (specificity "many", research agenda)
                                         │
                                         ▼
       STAGE B (1 call / article per arm; NO passage, NO image, NO caption)
         person card + need + key terms + 2 styles drawn from the persona-constrained schedule
         (+ per-style hints: hedges, scene openers, L2 slips, true constraints, figure-kind phrasings)
                                         │
                                         ▼
       STAGE C  typing noise from the persona's habits (misspellings, typos, case, punctuation)
                                         │
                                         ▼
       reject chain (config order): empty · leak · spill · banned (doc ref / exam) · tex · unresolved ·
       local_label · generic · copy (Jaccard ≥ 0.7 to the need) · caption_copy (≥ 0.5, figures) · style ·
       duplicate · skip
                                         │
                                         ▼
       gates: EG2 round trip (text: expert styles chunk top-3, lay styles article top-10;
              figures: image top-100) + cross-model judges (judge3 for text; judge-fig sees the image)
```

**Output.** Each arm writes two directories:
- `<arm>-c3pf` (text) and `<arm>-c3fig` (figures), with `generations.jsonl` in the same row format as
  `rust/pigmed-synth`, so its `queries`, `filter`, `metrics`, `judge3` and `export` read them unchanged;
- a `<arm>.joint.json` covering both arms (shared wall clock, kept per GPU-hour, realised figure share).

**Per-slot provenance.** Each slot records:
- recipe, generator and need;
- persona id, role, group, English level and expertise;
- style, register and hint;
- modality, query modality and target id;
- figure kind, caption type and caption overlap;
- licence and commercial flag;
- clean and noisy query, and the reject reason.

## 2. Config schema (`querygen.toml`)

| Section | What it holds |
|---|---|
| `extends` | Parent config file. Tables merge key by key; any other value replaces the parent's; `personas.extra_roles` is appended. |
| `recipe`, `layouts` | Version written into every row (`v3.2`); arms to run (`c3pf`, `c3fig`). |
| `[personas]` | `seed`, `size`; group shares; `roles` as `[role, group, min, max expertise, weight]`; `l1` list. |
| `[[personas.attrs.<a>]]` | Rules for age, gender, english, first_language, literacy, goal and context. The first matching `when` wins (`fixed` or weighted `table`). |
| `[[personas.habits.<h>]]` | Probability of terse, lowercase, no_punct, typos and abbrev by first matching rule. |
| `[personas.card]` | Card template and wording for expertise, English level and style. |
| `[text]` | Cards per passage, candidate strata, reader labels, health card, styles per pair, fallback expert wording, intent types, audience, `budget`, template names and line templates. |
| `[[styles]]` | Each style has: name, weight, register, description, `allowed` clauses (an OR of ANDs over expertise, groups, English, literacy and stretch), `weight_mult`, `max_words`, `no_question_mark`, `requires_constraint`, `hints` and `skip_checks`. |
| `[hints]` | Constraint and L2 templates, hedges, scene openers (lay and other), and L2 slips by first language. |
| `[constraints]` | Instruction constraints that are true of the article by construction: PubMed and JATS type rules (with `exclusive` groups), year forms and title rules. |
| `[filters]` | Card-drop order, reject-chain order, thresholds, regexes and word lists; `[filters.figure]` holds the figure reject chain and figure doc-ref. |
| `[noise]` | Keyboard rows, misspelling table and removed punctuation. |
| `[modality]` | Targets: text_passage 0.70, figure 0.24, figure_composed 0.06; table and supplement 0, reserved until those units exist; video and audio reserved. Also `kept_ratio`. |
| `[figure]` | Per-article and per-figure caps; image size; citing text, abstract and caption limits; caption-type regexes; rights regex; figure kinds; `card_types`; templates; `styles`; `kind_forms`. |
| `[gates]` | EG2 query prefix and gate rules per modality and style. |
| `[fallback]` | `mode` = generic (v3.1d) or role_or_map (v3.1e); `vague_roles`; `mesh_roles` (tree prefix → role, longest prefix wins); `journal_roles` (title substring → role, checked first). `figure.fallback_mode` overrides the mode for figure cards. |
| `[domain]` (v3.1e) | `trees_any` (A B C D N G03–G12), `trees_with_other` (E counts only with another tree), `journal_list` (file, one title or NLM id per line), `no_mesh` (llm_or_journal / journal). |
| `[lay]` (v3.1e) | `patient_trees` (C, D) + `clinical_pub_types`, or `trees_alone` (N02, N05); `target_share` 0.12 (`target_min` 0.10, `target_max` 0.15); `article_rate`; lay-only `candidates`; the LAY CARD `line`. |

**Modalities: what a query looks for.** `text_passage` is a body chunk. `figure` is the figure unit (image + caption).
`figure_composed` is image + question → the citing paragraph. `table`, `supplement`, `video` and `audio` are reserved
with share 0.

Audio, checked 2026-11-18:
- None of the approved generators (Gemma 4 26B-A4B / 31B, Muse Glimmer 30B, Inkling 2 Small) takes audio. Inkling 2
  Small's chat template rejects audio input and has no video type. Gemma 4 audio exists only in the E2B, E4B and 12B
  sizes.
- Audio and video queries ("the lecture where…", spoken symptom descriptions) would therefore need a different
  generator, or video given as frames (Gemma 4, Glimmer).

## 3. Persona library

- **Sampling.** The library is deterministic: persona `i` = hash(seed, i, attribute) draws, under the compatibility
  rules.
- **Mix check.** `pigmed-querygen mix --config C` reports every attribute's mix against its target, with checks for:
  - group shares within 2 points;
  - L2 English share ≥ 15%;
  - low health literacy share;
  - every role drawn;
  - the compatibility rules.

v3.2 library (20k):

| Attribute | Mix |
|---|---|
| Groups | clinician 30%, researcher 30%, lay 20%, student 12%, other 8% |
| Roles | 154 (v3.1d: 53) |
| English | native 62%, fluent L2 29%, limited L2 9% |
| First languages | 35 |
| Health literacy | high 74%, medium 19%, low 7% |
| Contexts | desktop search box 36%, phone thumb-typing 31%, literature database 20%, hospital workstation 7%, voice search 6% |
| Goals | figure-relevant goals ("see what something looks like", "find an image to show a patient or colleague", "compare a figure with their own results", …) about 14% |

## 4. Figure arm (c3fig)

**Inputs** (`fig-inputs`). Per article, up to 6 figures:
- train split, commercial licence;
- no rights phrase in the caption and no `<permissions>` in the JATS `<fig>`;
- held-out articles excluded;
- optionally only figures with an EG2 image vector, so the round trip can score them.

For each figure the builder fetches the image from the PMC Open Data bucket and resizes it so its longest side is
≤ 1280 px. It also extracts up to 2 citing paragraphs from the JATS, and the citing chunk (the text unit holding the
first citing paragraph).

**Plan.** Figure cards per article are set so that figure slots ≈ text slots × s / (1 − s), with s = figure +
figure_composed share. A per-input `boost` (found by search, recorded in the summary) lets articles with many figures
make up for those with few. On a PMC-like spread of 0–8 usable figures per article the planned figure share is 29.9%
at boost 1.45.

Card types:

| Type | Weight | Target | Query | Notes |
|---|---|---|---|---|
| find_figure | 40 | figure | text | "find a figure showing…", "Kaplan–Meier curve for…", "western blot of…", "diagram of the … pathway", "microscopy of…" |
| finding | 25 | figure | text | the result the figure shows |
| appearance | 15 | figure | text | what something looks like; radiology, clinical photos, micrographs and multi-panel figures only; lay-friendly |
| composed | 20 | citing paragraph | image + text | the question typed while showing the image; exempt from `unresolved` ("this curve"), still banned for document references |

**Figure styles.** find_figure 26, visual_keyword 14, natural_question 14, expert_terse 12, lay_visual 14,
half_remembered 6, non_native 10, plus composed (fixed for composed cards). Persona rules apply as for text.
find_figure slots get a kind-specific phrasing hint.

**Filters.** The text chain plus `caption_copy`. The figure doc-ref rule allows "find the figure showing…" but still
bans figure numbers and panel letters.

## 4b. v3.1e: changes from the v3.1d trial read

The v3.1d 4-model trial found:
- 26B-A4B and 31B give about equal quality, and 26B is about 5× faster.
- Lay queries made up only 0.4–0.7% of the output.
- Chemistry and materials papers passed the LLM's domain flag.
- The generic field-expert persona read poorly.

v3.1e responds as follows. Its per-article input is an article-meta table (`--article-meta`, JSONL):
`{article_id, mesh_trees: [MeSH tree numbers], journal: full title, nlm_id, pub_types}`.

| Change | Rule |
|---|---|
| Domain | With MeSH, the tree rule decides; the journal list overrides it to biomedical. Without MeSH, the LLM flag or the journal list decides. Without `[domain]` (v3.1d), the LLM flag alone. |
| Lay pass | Patient-relevant articles (MeSH C or D plus a clinical publication type, or N02/N05; 24.5% of train articles) get `cards_per_article` extra cards (default 5, one per distinct passage) with **lay-only** candidates; one card per article would cap lay at ~1.7%. Reader `none` → skip. `article_rate` is tuned per run so lay cards make up 10–15% of final queries; the realised share is in `mix` (`kept_lay_card_share`). |
| Fallback | When no candidate fits: stage A's `expert_role` if concrete (not in `vague_roles`), else the journal map, else the MeSH map, else the card is dropped (`no_reader`). No "expert working in this article's own field". |

## 5. CLI (`rust/pigmed-querygen`)

| Command | Purpose |
|---|---|
| `personas --config C --out F` | Library with rendered cards (parity input). |
| `render --config C --sample S --articles A [--figs F] --n N --out D` | Prompts and schemas of N articles; stage B rendered from a mock stage-A reply. |
| `mix --config C [--sample S --articles A --figs F] [--gen D…]` | Library, planned (pre-generation) and realised mixes. |
| `fig-inputs …` | Figure inputs (see §4). |
| `generate --config C --sample S --articles A [--figs F] [--article-meta M] --endpoint … --arm … [--deadline-unix T] [--text-only\|--fig-only] --out D` | Text and figure arms; append-only partial files and progress for live monitoring. |
| `judge-fig` | Cross-model VLM judge for figure queries: relevant 0–2, needs_image, caption_paraphrase, standalone, natural. |
| `queries --modality text\|figure` | Inputs for the pigmed-synth EG2 `filter`. |
| `report --trial T` | Results table: quality vs throughput, GPU-hours for 2M kept. |
| `blind` | Blinded, stratified hand-read sample, with a separate key. |
| `peek` | Random kept queries from a running or finished arm. |
| `mock-serve` | Schema-faithful fake OpenAI server for tests and dry runs. |
| `compare` | Parity checker. |

## 6. Parity and tests

- **`querygen/parity.sh SAMPLE ARTICLES [N]`** builds pigmed-synth into a temporary target directory and checks two
  things. It never touches pigmed-synth's source.
  - (a) `personas` output is byte-identical (20k cards, same sha256).
  - (b) Both generators talk to the same deterministic mock server. Every request (system, user, schema, budget,
    sampling, extra) and every output field (queries, noise, reject reason, card drops, persona fields) must be
    identical.
- **Result, 2026-11-18,** on 300 pilot passages (60 articles): 117/117 requests identical, 808 slots, 0 mismatches.
  Every reject reason and card-drop reason was exercised.
- **`cargo test`** in `rust/pigmed-querygen` (13 tests) covers:
  - config extension;
  - persona rules in v3.2;
  - the text and figure reject chains (caption copy, the figure doc-ref, composed exemptions);
  - true-by-construction constraints;
  - the figure planner and figure schema/fill;
  - JATS citing paragraphs and permissions;
  - an end-to-end `fig-inputs` run on fixtures (rights phrase, non-commercial and held-out drops, resize to 1280 px);
  - the config chain;
  - the v3.1e domain rule, lay pass, field → role map and no-generic-expert fallback.

## 7. Rules carried over

- LLMs run quantized only (NVFP4 or FP8 checkpoints). GPU jobs never download weights: they are prefetched on CPU.
- No Claude outputs as training labels; Claude is for evaluation only.
- No Chinese-origin models.
- Inputs come from the train split, commercial licences only; held-out shards are excluded.

## 8. Model / precision / reasoning-level grid (prepared, not run)

**Recipe.** `fig.toml`: v3.1d text plus the figure arm. Inputs are the ~300 articles (1,500 passages) of the v3.1d
4-model trial, plus their figures (up to 6 per article).

**Servers.** One server per model and precision. Its reasoning levels are request-level settings run in sequence on
that server, each a separate `generate` with its own throughput.

| Server | GPUs | Levels |
|---|---|---|
| Gemma 4 26B-A4B NVFP4 | 1 | thinking off, on |
| Gemma 4 26B-A4B FP8-dynamic | 1 | off, on |
| Gemma 4 31B NVFP4 | 1 | off, on |
| Gemma 4 31B FP8-dynamic | 1 | off, on |
| Muse Glimmer 30B FP8-block | 1 | low, medium, high, xhigh |
| Inkling 2 Small NVFP4 (stock SGLang TP4 / EP4) | 4 | none, low, medium, high, xhigh |

**Reuse.** At the four settings the v3.1d trial already ran (26B and 31B NVFP4 thinking off, Glimmer low, Inkling low),
the text arm is imported from that run (same inputs, recipe and sampling). Only the figure arm is generated there.

**Read-out.**
- Generated and kept per GPU-hour, and GPU-hours for 2M kept.
- EG2 keep: text gated by style, figures top-100.
- Cross-model judges, never the arm's own family: judge3 for text, judge-fig for figures.
- Persona, style and modality mix, and reject counts.
- A blinded 60-query hand-read with about a third figure rows.
