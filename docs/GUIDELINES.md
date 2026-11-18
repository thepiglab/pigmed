# Clinical practice guidelines

PigMed's corpus is PubMed / PubMed Central plus clinical practice guidelines. The goal is a public-good search over
the primary literature and the recommendations clinicians actually follow. Guidelines go through the same
plan → ingest → bundle → embed → publish → evaluate pipeline as PMC articles, so the embed workers, the verified HF
publisher and the retrieval evaluation need no changes.

Code: `rust/pigmed-ingest/src/guidelines/` (subcommand `pigmed-rs guidelines`). Registry:
`configs/guideline_sources.json`. Source research (119 source dossiers, content paths, access and rights notes,
proposed schemas) lives in a separate research package. `scripts/make_guideline_registry.py` builds the registry
from it.

## Commands

```bash
pigmed-rs guidelines list [--implemented]
pigmed-rs guidelines probe  --source ID [--limit 3] [--dump DIR] [--report FILE]   # dev loop: discover + extract, no bundles
pigmed-rs guidelines plan   --source ID --out plans/guidelines/ID.jsonl [--limit N] # discovery -> immutable plan
pigmed-rs guidelines ingest --plan plans/guidelines/ID.jsonl --root ROOT           # plan -> checksummed bundle
```

All four take `--registry` (default `configs/guideline_sources.json`) and `--cache` (default `cache/guidelines`,
one subdirectory per source). `--offline` serves only from the cache, for tests and reruns that must not touch the
site. `--contact` is optional text appended to the User-Agent.

## Layout

| file | role |
|---|---|
| `fetch.rs` | Polite HTTP: per-host pacing (`min_interval_ms`, ≥ 500 ms), robots.txt (see Robots below), redirect chain recording, size cap, truncated-body detection, retries with Retry-After, response cache, login-wall detection. `get_ok`, `get_json`, `post_form_ok` (ASP.NET postbacks, public search forms), `post_json_ok` and the general `request(&Req)`; the cache key covers method, headers and body. Never handles credentials or cookies. |
| `html.rs` | Generic extraction: HTML → ordered blocks (headings keep a `heading_path`; paragraphs, list items, Markdown tables, figures with absolute URLs, footnotes). Block ids are unique per document (repeated HTML ids get a suffix). Images inside links/paragraphs, tables inside `<figure>`, lazy-load `data-src`/`srcset`, and `aria-hidden`/CSS-hidden panels that hold real content are all kept. Markdown from `convert::convert` (PDF/DOCX/...) → blocks with page numbers. |
| `doc.rs` | Canonical `GuidelineDoc` (identity incl. `work_key`, `item_type`, status, `date_observations`, `relations`, rights, ordered blocks, attachments, fetch provenance) and `to_rows`. |
| `registry.rs` | Registry loader and the run gate (`check_runnable`). |
| `sources/` | One adapter per source. `static_list` is the generic, config-driven reference adapter; `_template.rs.txt` is the starting point for new ones. |
| `mod.rs` | CLI: list / plan / probe / ingest, bundle writing, figure image download into `assets.tar`, per-document audit rows, unit-id uniqueness check. |

## Adapter contract

An adapter implements `discover` (catalog → `PlanRow`s, deterministic and finite, de-duplicated by `doc_key`) and
`extract` (one document → `GuidelineDoc`).

- **Identity.** `doc_id = guideline:<source_id>:<doc_key>`. `doc_key` is a stable work slug plus a version (e.g.
  `diabetes-2023-v6`); `work_key` is the slug alone (`diabetes`), shared by every version. Never invent a PMCID or DOI;
  record a real DOI or PMID when the guideline states one. `item_type` is the source's own label for the item
  (guideline, guideline_chapter, evidence_summary, pathway, consensus_statement, quick_reference, ...).
- **Fidelity.** Keep every heading, the block order, tables (as Markdown) and figures (original URL plus caption).
  Recommendations become `BlockKind::Recommendation`, carrying the publisher's own label, strength, certainty and
  population text. A part you cannot parse becomes a `warnings` entry; never drop it silently.
- **Metadata.** Status (current / archived / withdrawn / in_review / unknown), version, dates, issuer and publisher
  are taken from the document. If a field is missing it stays `None` / `"unknown"`. Never fill it from the domain,
  the fetch date, the registry's source name or scope, or a neighbouring document. Every date the source prints goes
  into `date_observations` with its own label and raw text; `published` / `updated` are normalised copies of the
  ones labelled that way. Optional `search_metadata` (specialty, population, setting, ...) carries its own
  provenance.
- **Recommendations.** `label`, `strength`, `certainty`, `population`, `grading_system` and `qualifier` exactly as
  printed, per statement, `None` when absent (good-practice statements often have no grade; never inherit a
  neighbour's). Keep the statement text verbatim. If recommendations are presented as a table, emit one
  `Recommendation` block per row and keep the table too.
- **Rights.** Record the licence or terms as published (`Rights.license`, `status` open / restricted / unknown,
  `terms_url`, `rights_holders`, `attribution`, `restrictions`, `evidence_urls`). Every row carries `license` and
  `rights`. Publication policy is decided later, exactly as for PMC licences.
- **Access.** Use only the `Fetcher`. A login wall, paywall, CAPTCHA, 403 or robots.txt disallow is a recorded
  failure, never something to work around (no UA spoofing, no alternate hosts or mirrors to dodge a block). Sources
  marked `needs_auth` are never run.
- **Bytes.** A `.pdf` link that serves HTML (viewer, download wrapper, error shell) is an error, not a document
  (`Fetched::declared_pdf_but_not`; `blocks_for` refuses it). Follow the wrapper's real link if it has one.
- **Tests.** Every adapter ships an offline fixture test: saved pages under `tests/fixtures/guidelines/<id>/`, or
  pre-seeded fetch cache. It pins block counts, heading paths, recommendation labels and strengths, tables, figure
  URLs and metadata. `cargo test` never touches the network.

## Rows

`to_rows` emits the PMC row kinds, so downstream code needs no changes:

| kind | content |
|---|---|
| `article` | one per document: title, status, version, dates, issuers, rights, `search_metadata`, `fetches`, `warnings`, `unit_counts`, `canonical_member` (full `GuidelineDoc` JSON in `assets.tar`) |
| `text` | `block_type: "section"`: consecutive prose under the same heading path, chunked (512 tokens / 64 overlap by default) with `heading_path`, `context`, `locator.block_ids` / `char_start` / `char_end` / `page` / `page_end`, `chunk_index` / `chunk_count`. `block_type: "recommendation"`: one row per recommendation with a `recommendation` object (structured fields + verbatim `statement`). |
| `table_text` | one per table (Markdown, capped at `table_words`), with caption |
| `caption`, `figure` | figure caption text; figure row with `source_url` and, when ingest stored the image, `asset_member` (the image in `assets.tar`) |
| `media` | attachments (other renditions, quick-reference cards, ...) |
| `audit` | `ingest-error` for a document that could not be fetched or parsed (with the error) |

Every row has `article_id = doc_id`, `work_id` (`guideline:<source>:<work_key>` or null), `item_type`,
`source_type: "guideline"`, `source_id`, `title`, `url`, `status`, `version`, `license`, `rights` and
`export_approved`. The `article` row also has `date_observations` and `relations`.

**Embedded text carries its context.** `text`, `table_text` and `caption` rows start with
`<title> › <heading path>` (a leading heading equal to the title is dropped), then a blank line, then the body;
recommendation rows add a bracket line with the publisher's label and grade, e.g.
`[Recommendation 1.2.3 · strength: Strong · certainty: Moderate]`, before the verbatim statement. Guideline
paragraphs are short and lean on their headings ("Offer metformin." under "Type 2 diabetes › Drug treatment"), so
the vector must see the path. The prefix is also stored as `context`; `locator` offsets are relative to the body.
`ingest --no-context-prefix` turns it off (recorded in the run recipe and unit ids).

## Access classes (registry `access`)

| class | meaning | behaviour |
|---|---|---|
| `public` | public routes observed | scrape public routes |
| `mixed` | access differs per item | skip items behind a login |
| `unknown` | authentication not established | stop at any login wall |
| `needs_auth` | account, service key or paid/member entitlement required | disabled until an operator provides access; never scraped |

## Design decisions (2026-11-13)

### Block and row model
One canonical `GuidelineDoc` per document version, ordered blocks, rows in the PMC kinds. This holds up against
the research package's acceptance cases: headings, recommendation boxes, tables, figures and footnotes all map to a
block kind, and the full block list stays in `document.json` inside `assets.tar` for audit and re-chunking.
Prose is grouped per heading path before chunking, so a chunk never straddles two sections. Recommendations are
never chunked or merged: one statement, one row. A recommendation's rationale and evidence text stays in the
following section rows (same heading path), so it is retrievable next to the statement.

### Versions, supersession, errata
- Every version is its own immutable document (`doc_key` = work + version) and is never overwritten. Older
  versions stay in the corpus with their own `status`.
- `status` is what the source states. Ingest never computes "current" from dates. Search time groups by `work_id`
  and prefers `status == "current"`, then the latest stated date; archived/withdrawn versions are filtered or
  down-ranked, not deleted.
- Relations are typed edges with a stated scope (`Relation { rel, target, scope, evidence_url }`):
  `supersedes` / `superseded_by`, `updates` (focused/partial updates; the old version may stay current for the
  untouched parts), `erratum` (with scope and whether the edition already incorporates it, as stated),
  `summary_of` (pocket guide, executive summary), `translation_of`, `part_of` (chapters with their own revision
  dates, e.g. NIH HIV), `rendition_of`, `consultation_for` (drafts: `status: in_review`).
- Renditions (HTML vs PDF, full vs summary) are separate attachments of one document when they are the same text,
  or separate documents linked by `rendition_of` / `summary_of` when their content or dates differ (e.g. MSF's
  stale PDF). Never assume the PDF is the latest.

### Robots and crawler identity
User-Agent `PigMed-guidelines/<version> (research corpus; respects robots.txt)`. A robots group naming us wins
over `*` (RFC 9309). A robots.txt that is an HTML page,
a 5xx or unreachable leaves the origin unresolved, and nothing there is fetched; a 4xx means no restrictions.

### Figures
`ingest` downloads each figure image through the `Fetcher` (same robots, pacing, cache), checks image magic, caps
size (`--figure-max-bytes`), and stores it as `<doc>/figures/<block_id>.<ext>` in `assets.tar`, recorded as the
figure row's `asset_member`. A failure is a document warning. `scripts/embed_bundles.py` still downloads from
`source_url`; before guideline bundles are embedded it must read `asset_member` from the bundle instead, so
embedding never hits guideline sites (open item, Python side).

### JavaScript-only sources
No headless browser tonight. Most "JS-only" sites still have a fetchable route, so check in this order:
(1) the JSON/XHR endpoint the page's own frontend calls (visible in the HTML/JS bundle; e.g. CENETEC's
`ObtenerContenidoSeccion`) via `get_json` / `post_json_ok`; (2) a sitemap or RSS/Atom feed; (3) server-rendered
print/PDF renditions; (4) embedded JSON state (`__NEXT_DATA__`, `application/ld+json`, `window.__INITIAL_STATE__`).
Use only endpoints the public page itself calls without credentials, and never guess undocumented APIs beyond
them. If none works, record `blocked: js_only` with what was tried. Later, if enough sources stay blocked, a
separate render stage can be added: headless Chromium driven from Rust over CDP (`chromiumoxide`) in a
container on the batch cluster, producing a cached "rendered" response that goes through the same robots/pacing gate and is
recorded as such in `fetches`. Adapters would then consume rendered HTML exactly like fetched HTML.

### Research schema fields: what is emitted, what is deferred
The research package's proposed schemas (`dataset_row`, `document_manifest`, `canonical_block_graph`) are a
superset. Mapping:

| research field | PigMed |
|---|---|
| `document.work_id` / `document_version_id` | `work_id` / `doc_id` |
| `item_type`, `publication_state`, `source_declared_version` | `item_type`, `status`, `version` |
| `date_observations[]` | `date_observations` (label, raw, normalized, source_url) |
| `relations[]` (manifest) | `relations` (rel, target, scope, evidence_url) |
| `section_path[]` | `heading_path` (titles) + block ids of headings in `document.json` |
| `locator.block_ids`, `char_start/end`, `pdf_page_start/end` | `locator.block_ids`, `char_start/end`, `page`, `page_end` |
| `rights.*` | `rights` (license, status, terms_url, note, rights_holders, attribution, restrictions, evidence_urls) |
| `source.*` retrieval provenance | article row `fetches` (url, final_url, redirects, status, type, bytes, sha256, time) |
| `quality.structural_warnings` | `warnings` |
| `recipe.*` | `tokenizer`, `chunk_recipe`, run recipe in the bundle checkpoint |

Deferred (not needed for search tonight; can be derived later from `document.json` and the fetch cache):
per-rendition ids, jurisdiction/audience arrays (go in `search_metadata` when a source states them), the
manifest `coverage` block (catalog enumeration / terminal resolution status per source: recorded in PROGRESS.md
for now), `text_sha256` per row, canonical-view source maps, and table structure beyond Markdown (merged cells,
spans). Embedding fields are the embed stage's business, as for PMC.
