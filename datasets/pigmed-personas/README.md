# PigMed personas and persona-written search queries

> **Draft.** Numbers marked *pilot* come from the 1,000-article pilot; the release card will be regenerated from the
> released files. Licence of the dataset itself: to be confirmed (proposed CC BY 4.0, see [Licence](#provenance-and-licence)).

Synthetic biomedical literature-search queries, each written for a specific passage of an open-access PubMed Central
article by a specific simulated searcher (a nurse on a phone, a PhD student in a literature database, a parent whose
first language is Spanish …), plus the persona library that produced them. Built to train and evaluate passage
retrievers on the queries real people actually type: terse expert strings, plain questions, half-remembered
facts, second-language English, search-box filters, and typos.

Two parts:

| Part | Rows | What it is |
|---|---|---|
| `personas` | 20,000 | Deterministic persona library (seed `v3`): role, demographics, English level, health literacy, expertise, goal, search context, typing habits, and the short card shown to the query writer. |
| `queries` | *pilot: ~12k kept per 1k articles* | One row per query: the query (noisy and clean), its style, the intent card (need, narrative, PICO, answer), the persona id, the gold passage, EG2 retrieval ranks, and per-row licence and provenance. |

## Why

Most synthetic retrieval data asks one model for "a question about this passage". Those queries reuse the
passage's words, read like exam questions, refer to "this study", and all come from the same voice. Real searchers
differ in what they want (a number to cite, a decision for a patient, a mechanism) and in how they type it (two
keywords on a phone, a full sentence in a second language, a half-remembered drug name). Recipe v3 separates **what is
asked** (the intent card, written with the passage in view) from **how it is typed** (the surface query, written
without the passage, in a persona's voice and a given style), then adds typing noise in code and keeps only queries
that a strong retriever can trace back to their passage.

## Pipeline

```
 PMC passage (train split, commercial-use licence)
      │
 [1] INTENT CARD   one LLM call per article: per passage, standalone needs that the passage answers
      │            (need, narrative of what counts as relevant, optional PICO, answer, specificity, audience)
      │            → cards that refer to the paper itself or are textbook-generic are dropped
 [2] PERSONA       stage A picks, per card, the most plausible reader among 3 library personas; if none fits,
      │            a persona matched to the card's audience and minimum expertise
 [3] QUERY         one LLM call per article that never sees the passage: 2 surface queries per (card, persona),
      │            each in a style allowed for that persona (keyword, lay paraphrase, non-native, filter, …)
 [4] NOISE         deterministic typing noise from the persona's habits (real misspellings, keyboard typos,
      │            lower case, dropped punctuation); the clean form is kept alongside
 [5] FILTERS       reject copies of the need, leaked ids, exam/doc-referential text, broken styles;
                   EG2 round trip: keep if the gold passage ranks top-3 in its article and the article top-3
                   (top-10 for lay, half-remembered and non-native styles)
```

## Persona library

Personas are sampled by our own deterministic attribute sampler (no LLM, no external persona data): persona `i` of
seed `v3` is always the same, so every query records exactly which persona wrote it. Attributes are drawn per group,
with the compatibility rules below. The card shown to the query writer reads, for example:

> general practitioner, man, age 30-44; professional knowledge in medicine/biology; native English speaker; health
> literacy high; wants to check whether a claim is true; searching on desktop search box; terse

Typing habits (case, punctuation, typos) are not in the card; they are applied afterwards in code (step 4).

<details>
<summary><b>Groups and target mix</b></summary>

| Group | Target share | Realised in library (20,000) |
|---|---|---|
| clinician | 30% | 30.0% |
| researcher | 30% | 29.5% |
| lay (patients, family, public) | 20% | 19.8% |
| student | 12% | 12.3% |
| other | 8% | 8.3% |

</details>

<details>
<summary><b>Roles (53), with expertise range and weight within the group</b></summary>

Expertise: 0 = no background, 1 = some background, 2 = professional knowledge, 3 = specialist knowledge in
medicine/biology. A persona's expertise is drawn uniformly within its role's range.

**Clinician (17)**

| Role | Expertise | Weight |
|---|---|---|
| general practitioner | 2 | 10 |
| emergency physician | 2–3 | 6 |
| hospitalist | 2 | 5 |
| surgeon | 2–3 | 5 |
| oncologist | 2–3 | 5 |
| psychiatrist | 2–3 | 4 |
| paediatrician | 2–3 | 5 |
| cardiologist | 2–3 | 4 |
| nurse | 2 | 12 |
| nurse practitioner | 2 | 5 |
| pharmacist | 2–3 | 7 |
| dentist | 2 | 3 |
| physiotherapist | 2 | 3 |
| midwife | 2 | 3 |
| paramedic | 2 | 3 |
| dietitian | 2 | 3 |
| infectious disease physician | 2–3 | 3 |

**Researcher (12)**

| Role | Expertise | Weight |
|---|---|---|
| wet-lab biologist | 2–3 | 12 |
| bioinformatician | 2–3 | 7 |
| epidemiologist | 2–3 | 6 |
| clinical trialist | 2–3 | 4 |
| PhD student | 1–2 | 12 |
| postdoc | 2–3 | 9 |
| principal investigator | 3 | 6 |
| biostatistician | 2 | 3 |
| neuroscientist | 2–3 | 4 |
| immunologist | 2–3 | 4 |
| microbiologist | 2–3 | 4 |
| pharmacologist | 2–3 | 4 |

**Student (6)**

| Role | Expertise | Weight |
|---|---|---|
| medical student | 1–2 | 12 |
| nursing student | 1 | 7 |
| pharmacy student | 1 | 4 |
| undergraduate biology student | 1 | 9 |
| high-school student | 0–1 | 4 |
| public-health master's student | 1 | 3 |

**Lay: patients, family and public (8)**

| Role | Expertise | Weight |
|---|---|---|
| patient with a new diagnosis | 0–1 | 10 |
| patient with a long-term condition | 0–1 | 10 |
| worried healthy adult | 0 | 5 |
| parent of a sick child | 0–1 | 8 |
| adult child caring for an elderly parent | 0–1 | 6 |
| partner of a patient | 0–1 | 4 |
| pregnant person | 0–1 | 3 |
| athlete | 0–1 | 2 |

**Other (10)**

| Role | Expertise | Weight |
|---|---|---|
| medical librarian | 1–2 | 4 |
| systematic reviewer | 2 | 3 |
| health journalist | 1 | 4 |
| health policy analyst | 1–2 | 3 |
| pharma medical-affairs scientist | 2–3 | 3 |
| regulatory writer | 2 | 2 |
| health-app developer | 0–1 | 2 |
| veterinarian | 2 | 2 |
| science teacher | 1 | 2 |
| patent attorney | 1 | 1 |

</details>

<details>
<summary><b>Age bands, gender</b></summary>

| Age band | Lay roles (default) | Professional roles (clinician, researcher, other) | Realised |
|---|---|---|---|
| 16-19 | 4 | – | 1.9% |
| 20-29 | 26 | 12 | 25.1% |
| 30-44 | 32 | 45 | 37.5% |
| 45-64 | 28 | 35 | 28.7% |
| 65+ | 10 | 8 | 6.8% |

Role overrides: high-school student is always 16-19; medical, nursing, pharmacy, undergraduate biology and PhD
students are 20-29 (85) or 30-44 (15); an adult child caring for an elderly parent is 30-44 (30) or 45-64 (70); a
pregnant person is 20-29 (45) or 30-44 (55).

| Gender | Weight | Realised |
|---|---|---|
| woman | 49 | 49.4% |
| man | 48 | 47.5% |
| non-binary person | 3 | 3.0% |

A pregnant person is always "woman".

</details>

<details>
<summary><b>English level and first language</b></summary>

| English | Lay roles | All other roles | Realised |
|---|---|---|---|
| native | 62 | 62 | 61.7% |
| fluent L2 | 20 | 31 | 29.0% |
| limited L2 | 18 | 7 | 9.3% |

First language (L2 personas only), with weight and realised share of the whole library:

| First language | Weight | Realised |
|---|---|---|
| Spanish | 14 | 6.5% |
| Hindi | 8 | 3.6% |
| Arabic | 8 | 3.8% |
| Portuguese | 7 | 3.2% |
| French | 6 | 3.0% |
| German | 5 | 2.3% |
| Bengali | 5 | 2.2% |
| Russian | 5 | 2.6% |
| Vietnamese | 4 | 1.9% |
| Tagalog | 4 | 1.9% |
| Korean | 4 | 2.0% |
| Turkish | 4 | 1.9% |
| Japanese | 4 | 1.9% |
| Swahili | 3 | 1.5% |

For `non_native` queries the writer is told to make at least two slips typical of the persona's first language
(v3.1b): Spanish/Portuguese/French: extra or wrong articles, adjective after noun, false friends; Hindi/Bengali/
Tagalog: wrong prepositions ("discuss about"), progressive verbs, article slips; Arabic/Turkish/Swahili: dropped
"is/are", wrong prepositions, no question inversion; Vietnamese/Korean/Japanese: missing articles and plural -s,
no past tense, topic first; Russian/German: missing articles, verb late, "since 3 years".

</details>

<details>
<summary><b>Health literacy and expertise</b></summary>

| Health literacy | Rule | Realised |
|---|---|---|
| low / medium / high | lay: 35 / 45 / 20 | 7.1% low |
| medium / high | student with expertise ≤ 1: 60 / 40 | 18.0% medium |
| medium / high | any other role with expertise ≤ 1: 50 / 50 | |
| high | everyone with expertise ≥ 2 | 74.9% high |

| Expertise | Meaning | Realised |
|---|---|---|
| 0 | no background in medicine/biology | 11.8% |
| 1 | some background | 24.9% |
| 2 | professional knowledge | 43.3% |
| 3 | specialist knowledge | 20.0% |

</details>

<details>
<summary><b>Goals</b></summary>

| Goal | Lay | Clinician | Researcher | Student and other |
|---|---|---|---|---|
| decide what to do for a patient or for themself | 45 | – | – | 12 |
| decide what to do for a patient | – | 40 | – | – |
| understand how something works | 35 | 15 | 25 | 22 |
| check whether a claim is true | 20 | 20 | 10 | 16 |
| find a method or protocol | – | 10 | 25 | 14 |
| gather evidence for a review, paper or grant | – | – | 25 | 14 |
| find a number to cite | – | 15 | 15 | 12 |

</details>

<details>
<summary><b>Search context and typing habits</b></summary>

| Context | Lay | Researcher | Clinician, student, other | Realised |
|---|---|---|---|---|
| phone, thumb-typing | 55 | 10 | 30 | 28.7% |
| phone, voice search | 15 | – | 8 | 7.3% |
| desktop search box | 30 | 45 | 42 | 40.8% |
| literature database | – | 45 | 20 | 23.3% |

| Habit | Probability | Realised |
|---|---|---|
| terse | 0.55 if expertise ≥ 2, else 0.30 | 46.6% |
| all lower case | 0.60 on phone thumb-typing, else 0.30 | 39.3% |
| drops punctuation | 0.55 on phone thumb-typing, else 0.30 | 37.4% |
| uses abbreviations | 0.50, only if expertise ≥ 2 | 31.7% |
| typos (0 / 1 / 2 per query) | P(any) = 0.75 phone + (lay or limited L2); 0.45 phone; 0.40 lay or limited L2; 0.18 otherwise; a second typo with P = 0.30 | 68.1% / 22.2% / 9.7% |

</details>

<details>
<summary><b>Compatibility rules (summary)</b></summary>

- Expertise is bounded by the role (role table above); e.g. high-school student 0–1, principal investigator 3,
  worried healthy adult 0.
- High-school student: age 16-19. University students: age 20-44. Carer of an elderly parent: age 30-64.
  Pregnant person: woman, age 20-44.
- Expertise ≥ 2 ⇒ health literacy high. Abbreviations only with expertise ≥ 2.
- Lay personas have more limited-L2 English (18% vs 7%), more phone use and more typos.
- Goals and search contexts are drawn per group (tables above), so a researcher never "decides for a patient".

</details>

## Query styles

<details>
<summary><b>Styles, weights and who may use them (recipe v3.1b)</b></summary>

Each (card, persona) gets 2 distinct styles drawn by weight from the styles allowed for that persona.

| Style | Weight | Allowed for | What the writer is asked for |
|---|---|---|---|
| expert_terse | 14 | expertise ≥ 2 | a professional's terse search string, technical terms, maybe abbreviations, no question words |
| natural_question | 24 | everyone | a plain question in the person's own words (not the need's wording) |
| keyword | 16 | expertise ≥ 1 | 2–5 search terms the person would know; no question, no sentence |
| lay_paraphrase | 14 | expertise ≤ 1, or health literacy not high | everyday words instead of jargon |
| half_remembered | 7 | everyone | vague or partly wrong details; one of 7 opener forms per slot ("that drug that…", "was it X or Y", "can't remember the name", …) |
| scenario | 8 | clinician, lay, student | 1–2 sentences about their own situation, then the question; one of 5 opener forms |
| non_native | 12 (×2 for limited L2) | L2 English only | second-language English with ≥2 slips typical of the first language |
| instruction | 8 | expertise ≥ 1, and the article has a true constraint | the search plus a filter that is **true of the gold article**, derived from metadata: "only RCTs" only for RCTs, "since 2019" only if published ≥ 2019, "in children" / "mouse studies" / "human studies" from the title |

</details>

<details>
<summary><b>Intent types and card fields</b></summary>

| Type | Meaning |
|---|---|
| fact_lookup | a specific fact |
| result_number | a measured outcome or value |
| mechanism_why | why or how something happens |
| method_protocol | how something was done or measured |
| comparison | between groups, treatments or conditions |
| clinical_decision | what to do for a patient |
| lay_question | a need a patient or family member has |
| claim | a statement someone wants checked |
| contextual | about the article's main subject, which the passage itself does not name |
| open_ended | broad but still answered here |

Card fields: `need` (8–30 words, standalone: names the drug, gene, organism, population…; never "this study"),
`narrative` (TREC-style: what a relevant document must contain and what does not count), `pico`
(population/intervention/comparison/outcome for clinical needs, else empty), `answer` (≤15 words), `specificity`
(unique / few / many papers would answer it), `audience`, `min_expertise`, `entities`.

</details>

## Worked example (pilot, recipe v3.1)

Article: an open-access MRI study comparing quantitative susceptibility mapping and R2* mapping in Parkinson's
disease (PMC5012676, CC BY, research article).

**1. Intent card** (passage P3, card c1)

| Field | Value |
|---|---|
| need | Does QSM show higher contrast between gray and white matter than R2* in Parkinson's disease patients? |
| narrative | The text should explicitly compare the contrast characteristics of QSM and R2* in brain tissue. A description of only one method is insufficient. |
| PICO | patients with Parkinson's disease / QSM and R2* mapping / QSM vs R2* / contrast between gray and white matter |
| type, specificity | comparison, few |
| answer | Yes, QSM demonstrates higher contrast, especially in basal ganglia and cortical structures. |
| audience, min expertise | researcher, 2 |

**2. Persona.** Stage A was offered three readers, a systematic reviewer (`v3-11851`) and two general practitioners
(`v3-14835`, `v3-15567`), and chose `v3-14835`:
general practitioner, man, 30-44, professional knowledge, native English, high health literacy, wants to check
whether a claim is true, desktop search box, terse; habits: lower case, keeps punctuation, no typos.

**3. Queries** (the writer sees the need, key terms and persona, not the passage)

| Style | Clean query |
|---|---|
| natural_question | Is QSM better than R2* for gray-white matter contrast in Parkinson's? |
| instruction (constraint "primary studies only", true: the article is a research article) | QSM vs R2* gray white matter contrast Parkinson's disease primary studies only |

**4. Noise** (this persona types in lower case; no typos)

| Style | Query (as released) |
|---|---|
| natural_question | is qsm better than r2* for gray-white matter contrast in parkinson's? |
| instruction | qsm vs r2* gray white matter contrast parkinson's disease primary studies only |

**5. Filters.** Neither query copies the need (token overlap < 0.7), both name their subject without "this study",
and both pass the EG2 round trip, so both are kept with the passage as the positive.

## Columns

| Column | Type | Description |
|---|---|---|
| `qid` | string | stable query id |
| `query` | string | the query as released (with typing noise) |
| `clean` | string | the query before typing noise |
| `style` | string | one of the 8 styles |
| `hint` | string | style instruction given to the writer (opener form, L1 slips, or the true constraint) |
| `intent` | string | intent type |
| `need`, `narrative`, `answer` | string | intent card |
| `pico_population`, `pico_intervention`, `pico_comparison`, `pico_outcome` | string | PICO (empty if not clinical) |
| `specificity` | string | unique / few |
| `persona_id` | string | id in `personas` |
| `persona_group`, `english` | string | copied from the persona for filtering |
| `persona_matched`, `persona_fallback` | bool | stage A rejected all 3 candidates; reader matched to the card's audience / field expert |
| `article_id`, `pmcid`, `doi` | string | source article (PMCID.version) |
| `gold` | list[string] | unit id(s) of the answering passage (published PigMed PMC chunks, v2) |
| `split` | string | article split (train / dev / test …), by near-duplicate group |
| `license_code`, `commercial` | string, bool | licence of the source article; always commercial-use |
| `eg2_chunk_rank`, `eg2_article_rank`, `eg2_within_rank` | int | rank of the gold passage / article for this query with EmbeddingGemma 2 over the corpus |
| `kept` | bool | passed the EG2 gate (default config holds kept rows only) |
| `gemma_judge_*` | mixed | source-free Gemma judge: standalone, specificity, exam, natural (1–3), register, style_ok |
| `recipe`, `generator` | string | recipe version and generator (model, revision, sampling) |
| `reject` | string | `all` config only: why a query was rejected |

Personas: `id, role, group, age, gender, english, first_language, literacy, expertise, goal, context, terse,
lowercase, no_punct, typos, abbrev, card`.

## Filter and keep statistics (pilot, 1,000 articles, 4,988 passages, Gemma 4 26B-A4B NVFP4)

| Recipe | Generated | Near-copies of the need | Leaked ids | Kept by EG2 (gate) | Notes |
|---|---|---|---|---|---|
| v2 (one call, no personas) | 19,315 | n/a | n/a | 33.0% | baseline |
| v3 persona-first | 22,347 | 17.9% | 17 | 44.9% (same gate) / 54.9% (style-aware gate) | generic 15% vs 39% for v2 (evaluation judge) |
| v3.1 | 19,893 | 0.0% | 0 | pending | reader choice collapsed to researchers (fixed in v3.1b) |
| v3.1b | pending | | | | audience-matched reader fallback, L1-specific slips |

Rejects in v3.1 (9,543 of 29,436 slots): writer skipped 6,347; copy of the need 1,407; missing 950; JSON spilled into the
text 359; doc-referential or exam 228; style violated 133; duplicate 116; leaked id 2. Card drops: 14,718 kept of
19,952; generic 3,014; doc-referential 1,665; skipped 555.

## Provenance and licence

- **Source passages:** PubMed Central Open Access articles with a **commercial-use licence only** (CC0, CC BY,
  CC BY-SA, CC BY-ND as unmodified text), not retracted. Every row carries `pmcid`, `doi` and `license_code` for
  attribution.
- **Generator:** `nvidia/Gemma-4-26B-A4B-NVFP4` (pinned revision in `generator`), served with vLLM 0.32, guided JSON,
  temperature 1.0, top-p 0.95. Outputs are subject to the Gemma terms of use *(to be quoted on release)*.
- **Personas:** our own deterministic sampler. NVIDIA Nemotron-Personas was a method inspiration only (attribute
  sampling with compatibility rules); **no Nemotron-Personas text or records are used**. No PersonaHub.
- **Retriever used for filtering:** EmbeddingGemma 2 (Apache 2.0).
- **No Claude outputs in the data.** An external model was used only to evaluate a small sample during
  development; none of its outputs or scores are in this dataset.
- **Dataset licence:** to be confirmed (proposed CC BY 4.0), in addition to the per-row source licences and the
  generator's output terms.

## Regenerate

Everything is in the Rust crate `rust/pigmed-synth` (recipe: `docs/RETRIEVER_QUERY_RECIPE_V3.md`):

| Step | Code |
|---|---|
| persona library | `persona.rs` (`pigmed-synth personas --out personas.jsonl`) |
| intent cards, reader choice, styles, constraints, reject filter | `recipe3.rs` |
| typing noise | `noise.rs` |
| article metadata for constraints | `pigmed-synth annotate-articles` |
| generation (OpenAI-compatible server, e.g. vLLM with `disable_any_whitespace`) | `pigmed-synth generate --layout c3pf` |
| EG2 round trip and keep gate | `pigmed-synth queries` / `filter`, `pilot3.rs` |
| source-free judge | `pigmed-synth judge3` |

The persona library is fully deterministic: `pigmed-synth personas` reproduces it byte for byte.
