# Embedding clinical practice guidelines (plan, not yet run)

Status: plan only (2026-11-17). The first-pass guideline download stores text only (`documents` / `blocks` /
`attachments` Parquet). Nothing here has been embedded yet. This note records how we intend to do it, so the
first embedding run starts from decisions rather than defaults.

It builds on what we measured on PMC (see `TEST_LADDER.md`, "Chunking A/B" and "ConTEB"):

- **Title and section-path prompt prefixes hurt.** For the untrained EmbeddingGemma 2 they cost 3–6 MRR points on
  the fair task (B −0.034, CB −0.060): the shared prefix pulls every chunk toward the title.
- **Balanced packing is free.** C (target 384, min 100, max 512 tokens) matches A on retrieval (+0.004) with 27%
  fewer units and half as many tiny chunks.
- **Zero-shot late chunking is slightly worse** (D −0.016) and helps only where a chunk needs its document. Context
  has to be trained in (S2 / InSeNT), not supplied by prompts or untrained pooling.

Guidelines differ from articles in one way that matters: their sentences are short and lean on their headings
("Offer metformin." under "Type 2 diabetes › Drug treatment"). So we cannot just copy the PMC answer. We treat
context as an experiment arm and do not bake it into the stored text.

### Update (2026-11-17): adopt the PMC v2 recipe

The later PMC chunking follow-up revised the findings above. One prefix is fine: the chunk's top-level heading on
its own line, without the title. Mixing in a late vector also helps. That recipe, v2, measured on 289k pilot
articles:
- balanced-c with target 256, min 64, max 384 and overlap 32;
- input `title: none | text: <top heading>\n<chunk>`;
- vector `normalize(v_naive + 0.5·v_late)`, where the late pass covers the whole document with
  `title: <title>` and a section-path line before each chunk.

It beat C by +0.031 MRR@10 on LLM queries and +0.027 on abstract→body queries, and won on every query-generator arm.
The title-plus-full-path prefix (B) still hurts, and late-only still hurts; the mix is what helps.

Guidelines adopt v2 as the default, which supersedes the G0 default in §4:
- **Section units:** balanced-c 256/64/384/32 within one heading path. Input = top-level heading line + chunk.
  Vector = naive + 0.5·late.
- **Late pass:** one EG2 pass per document of up to 8,192 tokens. Longer guidelines (many exceed this) get one pass
  per top-level section, each with the document title prefix.
- **Recommendation units:** never chunked. Input = nearest heading line + verbatim statement. Late mix applies,
  because the statement's span is in the document pass. Grade, strength, population and label stay in metadata.
- **Tables, captions and document units:** `title: none | text:`, naive only, as for PMC tables and captions.
- **Figures:** image vectors, as for PMC.
- **Query side:** 768-d, `task: search result | query: `. Document score = max + 0.25 × mean of its chunk scores.

§4's arms still run as the guideline-specific check: does the population line help recommendations, and is the
nearest heading better than the top-level heading for very short statements?

**Size of the first pass:** about 87M words in 15k documents, giving ~350–400k section chunks, 17.7k
recommendations, 76.7k tables, 15k document units and 10.7k figures. That is about 470k text units, and naive plus
late is roughly 1–2 GPU-hours on one RTX PRO 6000 / B200.

**Index:** use the same EG2 v2 space and index as PMC, with `source_type = guideline` and the §2 filters.
- Assign guidelines to the existing PMC atlas clusters and labels, and add a guideline-only clustering view if
  needed.
- Before mixing, dedup guideline PMC-crosswalk copies against PMC by PMCID/DOI, keeping the guideline record linked
  to the PMC article.

## 1. Units

Every unit is built from the `blocks` table (ordered, typed, with `heading_path`), not from the ingest `text`
rows. Ingest's rows carry a `<title> › <heading path>` prefix by default, so the stored blocks stay the single
source of truth and every chunking variant can be rebuilt without re-scraping.

| unit | built from | one per | notes |
|---|---|---|---|
| **recommendation** | `kind = recommendation` | statement | Never chunked or merged. Text = the verbatim statement. Label / strength / certainty / population / grading system go into metadata, not the embedded text, by default. Its `heading_path` and population are kept for the context arms (§4). |
| **section** | consecutive paragraph / list_item / footnote blocks under one `heading_path` | balanced chunk | Balanced packing (C: target 384, min 100, max 512 tokens) **within** a heading path: a chunk never straddles two sections. A recommendation's rationale stays in the following section units, so it is retrievable beside the statement. |
| **table** | `kind = table` | table (or row group) | Markdown. Tables up to 512 tokens = one unit. Longer ones split by row groups with the header row repeated. Recommendation tables are also emitted as recommendation units (one per row), as ingest already does. |
| **figure** | `kind = figure` | figure | Flowcharts, algorithms and pathway diagrams are often where the decision logic lives. Embed the **image** with the multimodal model (NeoMME / EG2-vision path) plus the caption as text. Images come from the bundle's `assets.tar` / HTTP cache, never re-fetched from the site at embed time. |
| **document summary** | title + issuer + scope/introduction + the recommendation labels list | document | One vector per document version for document-level search ("which guideline covers X") and for grouping. Built from text the document itself states. No generated summary in the first version; an LLM summary is a later arm. |

Attachments (PDF renditions, quick-reference cards) are not separate units when their text equals the document's.
When they differ (a summary card, a stale PDF) the adapter already emits them as separate documents linked by
`summary_of` / `rendition_of`.

## 2. Metadata stored beside each vector (filters, not text)

- `source_id`, `issuers`, `publisher`, `country`, `language`
- `status` (current / archived / withdrawn / in_review / unknown, as the source states it), `version`
- dates: `published`, `updated`, plus the full `date_observations`
- `item_type` (guideline, chapter, evidence summary, pathway, consensus statement, quick reference, ...)
- specialty / population / setting from `search_metadata` when the source states them. Specialty is otherwise
  derived later from a controlled list (MeSH tree), and the derived value carries its provenance.
- for recommendations: `rec_label`, `rec_strength`, `rec_certainty`, `rec_grading_system`, `rec_population`,
  `rec_qualifier`
- `license`, `rights_status`, so publication policy can filter exactly as for PMC licences
- locator: `doc_id`, `work_id`, `block_ids`, `heading_path`, `page`

## 3. Versions

- Every document version is embedded, archived ones included. Versions are immutable (`doc_key` = work + version).
- Search groups hits by `work_id`. Default ranking prefers `status = current`, then the latest stated date.
  Archived and withdrawn versions are down-ranked or filtered, never deleted. They stay answerable for "what did
  the 2019 guideline say".
- Re-runs: a new version is a new document. Units of unchanged versions are not re-embedded; the vector key is
  (`doc_id`, unit id, recipe hash). `superseded_by` / `updates` relations drive the "newer version exists" flag at
  query time, not at embed time.
- Focused updates (`updates`, with a stated scope) keep the old version current for the untouched parts, so
  ranking uses relation scope, not just status.

## 4. Context: experiment arms

The PMC result says no prompt prefix. Guidelines get a small A/B before we commit:

| arm | embedded text |
|---|---|
| G0 | block text only (`title: none \| text: <statement or chunk>`): the PMC winner |
| G1 | G0 + nearest heading only (not the full path, not the title) |
| G2 | G0 + full heading path, no title |
| G3 | recommendations only: G0 + population text |
| G4 | late chunking over each section (zero-shot), then the S2-trained context model when it exists |

Expectation: G0 or G1 wins on section units (as on PMC). Recommendations may gain from G1 or G3 because the
statements are so short. Whatever wins, metadata filters (§2) do most of the "which population, which status"
work. Ship G0 for sections unless the guideline eval says otherwise.

## 5. Evaluation (guideline question → recommendation)

- **Queries:** clinical questions whose answer is a specific recommendation.
  - Generated from held-out recommendations with the approved generator (see `RETRIEVER_DATA_PLAN.md`; no
    Chinese-origin models), rewritten so the statement's words are not copied.
  - Plus a small hand-checked set (~200) across specialties, countries and both current and archived versions.
- **Targets:** primary = the recommendation unit; secondary = its section units (rationale) and its document.
- **Metrics:**
  - MRR@10 and nDCG@10 on recommendation units;
  - Recall@50 on the document;
  - "current version hit rate" (share of top-1 hits from the current version of the right `work_id`);
  - a status-filter sanity check (no withdrawn guideline in the top 10 when the filter is on).
- **Splits:** by `work_id`, so no version of an evaluated guideline is in training data. The same leakage rule as
  the PMC splits.
- **Mixed corpus check:** run the same queries against PMC + guidelines together. Guideline recommendations should
  win over primary articles for "what should I do" questions and lose for "what did trial X find" questions.
- **Gate:** the PMC chunking eval and ConTEB stay the cheap regression gates. The guideline eval is added to the
  test ladder once the hand-checked set exists.

## 6. Open items before the first embedding run

- The ingest `text` rows carry the context prefix by default. Embedding should build units from `blocks` (as
  above), or ingest should run with `--no-context-prefix`.
- `scripts/embed_bundles.py` downloads figures from `source_url`. For guidelines it must read the image from the
  bundle/cache instead, so embedding never hits guideline sites.
- Non-English guidelines (es, pt, fr, de, nl, no, sv, fi, ja, ko, id, ...): EG2 is multilingual. The eval needs
  per-language query slices before we trust the cross-lingual ranking.
- Rights: embedding is internal. Publishing vectors or text follows each source's recorded terms, decided later.

## 7. Public vs private index tiers

The public search Space stores text for snippets. So **only redistributable guideline content may go into any public
index or repo.** Everything else goes into a **private** index only. Whatever the tier, all of it stays in the
private HF dataset `mkieffer/pigmed-guidelines`.

**The tier is decided per document, not per source,** because many sources mix rights. For example, a society's site
pages are "all rights reserved", while its open-access PMC copies are CC BY. A document is `public` only if all of
these hold:
- `rights_status == "open"`;
- the licence is CC BY, CC BY-SA, CC0, public domain or an open government licence that allows redistribution;
- `rights.restrictions` is empty (no terms or AI-use note, no TDM-only notice);
- `collection_notes` is empty.

Everything else is `private`:
- unknown licence;
- restricted;
- any terms or AI-use finding (IDSA, MAGICapp-hosted, bpac, Diabetes Canada, Right Decisions, AAO-HNSF, EFDA,
  ACOG);
- TDM-only author manuscripts;
- publisher "free to read" notices.

**Needs a maintainer decision before going public** (marked `review` below):
- CC BY-NC / NC-ND / NC-SA copies (e.g. WHO's CC BY-NC-SA 3.0 IGO, ESMO Open, ATS, BSG, EULAR). A non-commercial
  public-good search may be compatible with NC, and ND allows verbatim snippets, but that is a licence call.
- Norway's NLOD content, obtained through a dev/test API key.

**US federal works** (CDC, USPSTF, VA/DoD, NIH HIV, NCI PDQ) are generally public domain under 17 USC 105. But the
adapters record a licence only when the document states one, and do not infer it from the publisher. So most of
these docs are `unknown` today: only 44 USPSTF docs carry the printed "public domain within the United States"
notice. To make them public:
- teach those adapters to record the publisher's stated public-domain notice;
- keep third-party copyrighted figures and tables out, since these works often say they include them.

**First-pass numbers** (HF documents as of 07:30, 15,005 docs): only **33 documents** qualify as `public` today,
all of them open-access PMC copies (CC BY) from asco, eso, wses, eular, easl, brain_trauma, esmo and bsg. Most
sources record an unknown or restricted licence, so the public tier stays small until rights are recorded per
document (federal notices, open government licences) and a maintainer decides on NC/ND. An unknown licence never
becomes public by default.

Per-source table for review. Columns: documents on HF; documents public today under the rule above; the top
licence / rights_status values; the proposed tier.

| source | docs | public now | top licence / status | proposed tier |
|---|---|---|---|---|
| aad_dermatology | 12 | 0 | none / unknownx12 | private (licence unknown or restricted) |
| aao_hnsf | 3 | 0 | none / unknownx3 | private (terms/AI-use note) |
| aao_ophthalmology | 20 | 0 | none / restrictedx20 | private (licence unknown or restricted) |
| aaos | 21 | 0 | none / restrictedx21 | private (licence unknown or restricted) |
| aasld | 24 | 0 | none / unknownx22; CC-BY-NC-4.0 / openx1; This file is available for text mining. / restrictedx1 | review: CC BY-NC / NC-ND copies (non-commercial snippets); rest private |
| aasm_sleep | 8 | 0 | none / unknownx5; none / restrictedx2; CC BY-NC-ND 4.0 / restrictedx1 | private (licence unknown or restricted) |
| acep_clinical_policies | 3 | 0 | none / unknownx3 | private (licence unknown or restricted) |
| acg | 23 | 0 | none / unknownx23 | private (licence unknown or restricted) |
| acog | 26 | 0 | none / restrictedx26 | private (terms/AI-use note) |
| acp_im | 6 | 0 | This file is available for text mining. / restrictedx4; none / unknownx2 | private (licence unknown or restricted) |
| acr | 299 | 0 | none / unknownx299 | private (licence unknown or restricted) |
| acr_radiology | 582 | 0 | none / unknownx582 | private (licence unknown or restricted) |
| ada | 1 | 0 | none / restrictedx1 | private (licence unknown or restricted) |
| ada_dental | 4 | 0 | none / unknownx2; CC-BY-NC-ND-4.0 / openx2 | private (terms/AI-use note) |
| asam_addiction | 9 | 0 | none / unknownx5; none / restrictedx3; This file is available for text mining. / restrictedx1 | private (licence unknown or restricted) |
| asco | 6 | 5 | CC-BY-4.0 / openx5; CC-BY-NC-ND-4.0 / openx1 | mixed: 5 doc(s) public (CC BY / open), rest private |
| ash_hematology | 37 | 0 | none / unknownx31; CC-BY-NC-ND-4.0 / openx6 | review: CC BY-NC / NC-ND copies (non-commercial snippets); rest private |
| astro | 1 | 0 | All rights reserved / restrictedx1 | private (licence unknown or restricted) |
| ats | 59 | 0 | none / unknownx33; CC-BY-NC-ND-4.0 / openx14; CC-BY-NC-4.0 / openx8 | review: CC BY-NC / NC-ND copies (non-commercial snippets); rest private |
| au_cancer_council | 19 | 0 | none / unknownx19 | private (licence unknown or restricted) |
| au_department_pregnancy_care | 1 | 0 | none / unknownx1 | private (terms/AI-use note) |
| au_eviq | 838 | 0 | none / unknownx838 | private (licence unknown or restricted) |
| au_living_evidence | 3 | 0 | none / unknownx2; All rights reserved / restrictedx1 | private (licence unknown or restricted) |
| au_nz_cari | 84 | 0 | none / unknownx84 | private (terms/AI-use note) |
| au_racgp | 68 | 0 | none / unknownx68 | private (licence unknown or restricted) |
| au_rch | 245 | 0 | none / unknownx245 | private (licence unknown or restricted) |
| au_sti | 44 | 0 | none / unknownx44 | private (licence unknown or restricted) |
| bashh | 60 | 0 | none / unknownx60 | private (licence unknown or restricted) |
| bc_guidelines | 63 | 0 | none / unknownx63 | private (licence unknown or restricted) |
| bmj_rapid_recommendations | 45 | 0 | none / unknownx35; CC-BY-NC-4.0 / openx10 | private (terms/AI-use note) |
| brain_trauma_foundation | 26 | 2 | none / unknownx24; CC-BY-4.0 / openx2 | mixed: 2 doc(s) public (CC BY / open), rest private |
| brazil_conitec_pcdt | 140 | 0 | none / unknownx140 | private (licence unknown or restricted) |
| bsg | 49 | 1 | none / unknownx28; CC BY-NC 4.0 / restrictedx9; CC-BY-NC-4.0 / openx9 | mixed: 1 doc(s) public (CC BY / open), rest private |
| bts | 89 | 0 | none / unknownx89 | private (licence unknown or restricted) |
| ca_cps | 196 | 0 | All rights reserved / restrictedx196 | private (licence unknown or restricted) |
| ca_hypertension_canada | 15 | 0 | none / restrictedx14; none / unknownx1 | private (licence unknown or restricted) |
| ca_inesss | 204 | 0 | none / unknownx185; none / restrictedx19 | private (licence unknown or restricted) |
| canadian_stroke | 102 | 0 | none / restrictedx93; none / unknownx9 | private (licence unknown or restricted) |
| cdc_acip | 162 | 0 | none / unknownx162 | private now; candidate public (US federal work) once the adapter records the stated public-domain notice |
| cdc_infection_control | 35 | 0 | none / unknownx35 | private now; candidate public (US federal work) once the adapter records the stated public-domain notice |
| cdc_sti | 60 | 0 | none / unknownx60 | private now; candidate public (US federal work) once the adapter records the stated public-domain notice |
| chest_guidelines | 18 | 0 | none / unknownx10; Since January 2020 Elsevier has created  / restrictedx5; CC-BY-NC-ND-4.0 / openx3 | review: CC BY-NC / NC-ND copies (non-commercial snippets); rest private |
| cl_minsal_ges | 75 | 0 | none / unknownx75 | private (licence unknown or restricted) |
| ctfphc | 22 | 0 | none / unknownx22 | private (licence unknown or restricted) |
| diabetes_canada | 46 | 0 | none / restrictedx46 | private (terms/AI-use note) |
| eaes | 23 | 0 | none / unknownx13; CC-BY-4.0 / openx9; This article is made available via the P / restrictedx1 | private (terms/AI-use note) |
| easl_public_pmc | 11 | 2 | CC-BY-NC-ND-4.0 / openx4; CC-BY-4.0 / openx2; Since January 2020 Elsevier has created  / restrictedx2 | mixed: 2 doc(s) public (CC BY / open), rest private |
| east_practice_guidelines | 98 | 0 | none / restrictedx98 | private (licence unknown or restricted) |
| eau | 240 | 0 | none / restrictedx187; none / unknownx53 | private (licence unknown or restricted) |
| endocrine_society | 29 | 0 | none / unknownx29 | private (licence unknown or restricted) |
| ers | 4 | 0 | CC-BY-NC-4.0 / openx4 | review: CC BY-NC / NC-ND copies (non-commercial snippets); rest private |
| es_guiasalud | 79 | 0 | none / unknownx74; none / restrictedx5 | private (licence unknown or restricted) |
| esc | 31 | 0 | none / restrictedx31 | private (licence unknown or restricted) |
| esmo | 80 | 1 | none / unknownx70; CC-BY-NC-ND-4.0 / openx8; This file is available for text mining. / restrictedx1 | mixed: 1 doc(s) public (CC BY / open), rest private |
| esmo_living | 196 | 0 | none / unknownx196 | private (licence unknown or restricted) |
| eso | 10 | 6 | CC-BY-4.0 / openx6; CC-BY-NC-4.0 / openx4 | mixed: 6 doc(s) public (CC BY / open), rest private |
| et_efda_stg | 3 | 0 | none / unknownx3 | private (terms/AI-use note) |
| eu_jrc_cancer_guidelines | 122 | 0 | none / unknownx122 | private (licence unknown or restricted) |
| eular_recommendations | 26 | 3 | CC-BY-NC-4.0 / openx15; CC-BY-NC-3.0 / openx3; CC-BY-NC-ND-4.0 / openx2 | mixed: 3 doc(s) public (CC BY / open), rest private |
| fi_current_care | 106 | 0 | none / restrictedx106 | private (licence unknown or restricted) |
| gin_library | 4135 | 0 | none / unknownx4135 | private (licence unknown or restricted) |
| gina | 3 | 0 | none / restrictedx3 | private (licence unknown or restricted) |
| gold | 3 | 0 | none / restrictedx2; none / unknownx1 | private (licence unknown or restricted) |
| has | 38 | 0 | Reproduction des travaux de la HAS / restrictedx38 | private (licence unknown or restricted) |
| icrc_clinical_manuals | 25 | 0 | none / unknownx25 | private (licence unknown or restricted) |
| idsa | 12 | 0 | This file is available for text mining. / restrictedx3; CC-BY-NC-ND-4.0 / openx2; You may print one copy of this document  / restrictedx2 | private (terms/AI-use note) |
| india_icmr_stw | 190 | 0 | none / unknownx163; none / restrictedx27 | private (licence unknown or restricted) |
| india_mohfw_stg | 61 | 0 | none / restrictedx61 | private (licence unknown or restricted) |
| iwgdf_diabetic_foot | 10 | 0 | CC BY-NC-ND 4.0 / restrictedx7; none / unknownx3 | private (licence unknown or restricted) |
| jp_minds | 517 | 0 | none / unknownx517 | private (licence unknown or restricted) |
| kdigo | 112 | 0 | none / unknownx100; CC BY-NC-ND / restrictedx12 | private (licence unknown or restricted) |
| ke_moh_mental | 7 | 0 | none / unknownx5; none / restrictedx2 | private (licence unknown or restricted) |
| kr_komgi | 81 | 0 | none / unknownx81 | private (licence unknown or restricted) |
| magicapp_stroke_public | 95 | 0 | none / unknownx95 | private (terms/AI-use note) |
| msf_medical | 935 | 0 | none / restrictedx935 | private (licence unknown or restricted) |
| mx_cenetec_cmgpc | 55 | 0 | none / unknownx55 | private (licence unknown or restricted) |
| my_mahtas_cpg | 155 | 0 | none / unknownx109; none / restrictedx46 | private (licence unknown or restricted) |
| nci_pdq | 212 | 0 | none / restrictedx212 | private now; candidate public (US federal work) once the adapter records the stated public-domain notice |
| ng_fmoh_stg | 11 | 0 | none / unknownx8; none / restrictedx3 | private (licence unknown or restricted) |
| nice_html | 78 | 0 | none / restrictedx78 | private (licence unknown or restricted) |
| nih_hiv | 214 | 0 | none / unknownx214 | private now; candidate public (US federal work) once the adapter records the stated public-domain notice |
| no_helsedirektoratet | 50 | 0 | Norsk lisens for offentlige data (NLOD) / restrictedx50 | review: NLOD (Norwegian open government licence) via a dev/test API key |
| nz_bpac | 38 | 0 | none / restrictedx38 | private (terms/AI-use note) |
| nz_starship | 981 | 0 | none / unknownx981 | private (licence unknown or restricted) |
| phac_naci | 120 | 0 | none / unknownx120 | private (licence unknown or restricted) |
| phac_sti | 57 | 0 | none / unknownx57 | private (licence unknown or restricted) |
| rcog | 70 | 0 | none / unknownx70 | private (licence unknown or restricted) |
| right_decisions | 205 | 0 | none / restrictedx205 | private (terms/AI-use note) |
| rw_moh_stg | 14 | 0 | none / unknownx14 | private (licence unknown or restricted) |
| sages_surgical_guidelines | 64 | 0 | none / restrictedx64 | private (licence unknown or restricted) |
| sccm | 27 | 0 | none / unknownx27 | private (licence unknown or restricted) |
| se_socialstyrelsen | 311 | 0 | none / unknownx170; none / restrictedx141 | private (licence unknown or restricted) |
| sg_ace | 34 | 0 | none / restrictedx34 | private (licence unknown or restricted) |
| sign | 45 | 0 | none / unknownx33; CC BY-NC-ND 4.0 / restrictedx12 | private (licence unknown or restricted) |
| tz_moh_stg_nemlit | 2 | 0 | none / unknownx2 | private (licence unknown or restricted) |
| ug_moh_ucg | 2 | 0 | none / unknownx2 | private (licence unknown or restricted) |
| uspstf | 295 | 0 | none / unknownx144; none / restrictedx107; public domain within the United States / unknownx44 | private now; candidate public (US federal work) once the adapter records the stated public-domain notice |
| va_dod | 26 | 0 | none / unknownx26 | private now; candidate public (US federal work) once the adapter records the stated public-domain notice |
| wgo_global_guidelines | 150 | 0 | none / restrictedx150 | private (licence unknown or restricted) |
| who_publications_iris | 9 | 0 | CC BY-NC-SA 3.0 IGO / restrictedx9 | review: CC BY-NC-SA 3.0 IGO (non-commercial, share-alike) |
| wses_guidelines | 52 | 13 | CC-BY-4.0 / openx36; none / unknownx13; CC-BY-NC-ND-4.0 / openx2 | mixed: 13 doc(s) public (CC BY / open), rest private |
| za_stg_eml | 593 | 0 | none / unknownx590; none / restrictedx3 | private (licence unknown or restricted) |
