//! Canonical clinical-guideline document model and its mapping to PigMed bundle rows.
//!
//! Every adapter produces a [`GuidelineDoc`]: ordered [`Block`]s (headings, paragraphs, list items,
//! recommendations, tables, figures, footnotes) plus document metadata, rights and fetch provenance.
//! [`to_rows`] turns it into rows with the same `kind`s the PMC path emits (`article`, `text`, `table_text`,
//! `caption`, `figure`, `media`, `audit`), so `scripts/embed_bundles.py`, the publisher and the retrieval eval work
//! unchanged. Guideline-specific facts ride along in extra fields (`source_type: "guideline"`, `block_type`,
//! `heading_path`, `recommendation`, `rights`, ...).
use crate::common::stable_id;
use crate::jats::{split_text, COUNTER_NAME};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// One provenance record per HTTP response the document was built from.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FetchRecord {
    pub url: String,
    pub final_url: String,
    pub redirects: Vec<String>,
    pub status: u16,
    pub content_type: Option<String>,
    pub bytes: u64,
    pub sha256: String,
    pub fetched_at: String,
    pub from_cache: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rights {
    /// Licence or terms label as published (e.g. "CC-BY-4.0", "Crown copyright / OGL v3", "All rights reserved").
    pub license: Option<String>,
    /// "open", "restricted", "unknown": what the terms say about reuse; never inferred from the domain.
    pub status: String,
    pub terms_url: Option<String>,
    pub note: Option<String>,
    /// Holders named in the copyright/licence statement.
    #[serde(default)]
    pub rights_holders: Vec<String>,
    /// Attribution text the licence requires, verbatim.
    #[serde(default)]
    pub attribution: Option<String>,
    /// Stated limits, verbatim or closely paraphrased: "no text and data mining without permission", "non-commercial",
    /// "no derivatives", "third-party figures excluded", "not for inclusion in retrieval systems", ...
    #[serde(default)]
    pub restrictions: Vec<String>,
    /// Pages the licence/terms facts were read from (terms page, copyright footer, the document's own notice).
    #[serde(default)]
    pub evidence_urls: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Heading { level: u8 },
    Paragraph,
    ListItem,
    /// A formal recommendation. Keep the publisher's own labels; never fill missing fields from neighbours.
    Recommendation {
        /// Publisher's own number/label ("1.1", "R3", "Recommendation 4").
        label: Option<String>,
        /// Strength/grade exactly as printed ("Strong", "Grade B", "Conditional", "Good practice statement").
        strength: Option<String>,
        /// Certainty / quality / level of evidence exactly as printed ("Moderate", "Level II-2", "⊕⊕◯◯").
        certainty: Option<String>,
        /// Population / scope text attached to this statement (age, setting, condition), as printed.
        population: Option<String>,
        /// Grading framework when the source names it ("GRADE", "USPSTF", "SIGN", "NICE wording"), else None.
        grading_system: Option<String>,
        /// Statement qualifier as printed: "draft", "consultation", "retained from previous edition", "withdrawn",
        /// "research recommendation", ... None when the source says nothing.
        qualifier: Option<String>,
    },
    /// Table as Markdown (header row + rows), cell text in source order.
    Table { caption: Option<String> },
    /// Figure / algorithm / image. `src_url` is the original asset; it is embedded later from that URL.
    Figure { src_url: Option<String>, caption: Option<String> },
    Footnote,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Block {
    /// Stable within the document: source element id if present, else a positional id ("b0042").
    pub block_id: String,
    pub kind: BlockKind,
    pub text: String,
    /// Ancestor headings, outermost first.
    pub heading_path: Vec<String>,
    pub page: Option<u32>,
}

/// A date the source states, kept with its label and raw text (sources often show several: published, last
/// reviewed, next review, effective, amended). `normalized` is ISO-8601 at the precision printed ("2024", "2024-05",
/// "2024-05-17"), or None when the text cannot be normalised. Never the fetch date.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DateObservation {
    /// The source's own label ("Published", "Last updated", "Review date", "Fecha de actualización", ...).
    pub label: String,
    pub raw: String,
    pub normalized: Option<String>,
    pub source_url: Option<String>,
}

/// A typed link from this document version to another document, version or rendition.
/// `rel`: supersedes | superseded_by | updates | updated_by | erratum | erratum_to | amends | amended_by |
/// summary_of | has_summary | translation_of | part_of | has_part | rendition_of | consultation_for | related.
/// `target` is a `guideline:<source>:<doc_key>` id when the target is in this registry, else its URL.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Relation {
    pub rel: String,
    pub target: String,
    /// What the relation covers, as stated ("section 3 only", "recommendations 4-6"); None = not stated.
    pub scope: Option<String>,
    /// Page/URL where the source states the relation.
    pub evidence_url: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GuidelineDoc {
    pub source_id: String,
    /// Stable per work+version within the source (e.g. "diabetes-2023-v6"). Never a fabricated PMCID/DOI.
    pub doc_key: String,
    /// Stable per work across versions (e.g. "diabetes"); all versions of one guideline share it. None when the
    /// source gives no way to tell versions of one work apart.
    #[serde(default)]
    pub work_key: Option<String>,
    /// What the source calls this item: "guideline", "guideline_chapter", "evidence_summary", "pathway",
    /// "consensus_statement", "position_statement", "quick_reference", "standard_treatment_guideline", ... (snake_case
    /// of the source's own label; "unknown" if it gives none).
    #[serde(default)]
    pub item_type: Option<String>,
    pub title: String,
    pub url: String,
    pub publisher: Option<String>,
    pub issuers: Vec<String>,
    pub version: Option<String>,
    /// "current", "archived", "withdrawn", "in_review", "unknown" - as the source states it.
    pub status: String,
    pub published: Option<String>,
    pub updated: Option<String>,
    /// Every date the source states, with its label (`published`/`updated` above are the normalised convenience
    /// copies of two of them).
    #[serde(default)]
    pub date_observations: Vec<DateObservation>,
    /// Supersession, updates, errata, summaries, translations, chapters (see [`Relation`]).
    #[serde(default)]
    pub relations: Vec<Relation>,
    pub language: Option<String>,
    pub country: Option<String>,
    pub doi: Option<String>,
    pub pmid: Option<String>,
    pub rights: Rights,
    /// Optional search metadata (specialty, population, setting, ...) with provenance; see the research
    /// package's search_metadata schema. Free-form JSON here; validated later.
    pub search_metadata: Value,
    pub blocks: Vec<Block>,
    /// Other renditions or attachments (PDF of an HTML guideline, quick-reference card, ...).
    pub attachments: Vec<Value>,
    pub fetches: Vec<FetchRecord>,
    pub warnings: Vec<String>,
}

impl GuidelineDoc {
    pub fn doc_id(&self) -> String {
        format!("guideline:{}:{}", self.source_id, self.doc_key)
    }
}

pub struct ChunkCfg {
    pub chunk_tokens: usize,
    pub overlap_tokens: usize,
    /// Tables longer than this many words are cut (full Markdown stays in the canonical JSON asset).
    pub table_words: usize,
    /// Prefix every text/recommendation/table/caption row's `text` with "<title> › <heading path>" (also stored as
    /// `context`). Guideline paragraphs are short and lean on their headings ("Offer metformin." under
    /// "Type 2 diabetes › Drug treatment"), so the embedded text carries them. `locator` offsets stay relative to the
    /// section body, without the prefix.
    pub context_prefix: bool,
}

impl Default for ChunkCfg {
    fn default() -> Self {
        Self { chunk_tokens: 512, overlap_tokens: 64, table_words: 2000, context_prefix: true }
    }
}

/// "Title › H1 › H2", dropping a leading heading that repeats the title.
pub fn context_line(title: &str, path: &[String]) -> String {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let t = title.trim();
    let mut parts: Vec<&str> = vec![];
    if !t.is_empty() {
        parts.push(t);
    }
    for (i, h) in path.iter().enumerate() {
        if i == 0 && norm(h) == norm(t) {
            continue;
        }
        if !h.trim().is_empty() {
            parts.push(h.trim());
        }
    }
    parts.join(" › ")
}

/// Base fields shared by every row of a document (the PMC path's `base` equivalent).
fn base(doc: &GuidelineDoc, export_approved: bool) -> Map<String, Value> {
    let mut m = Map::new();
    let id = doc.doc_id();
    for (k, v) in [
        ("article_id", json!(id)),
        ("doc_id", json!(id)),
        ("work_id", json!(doc.work_key.as_ref().map(|w| format!("guideline:{}:{}", doc.source_id, w)))),
        ("item_type", json!(doc.item_type)),
        ("source_type", json!("guideline")),
        ("source_id", json!(doc.source_id)),
        ("title", json!(doc.title)),
        ("url", json!(doc.url)),
        ("publisher", json!(doc.publisher)),
        ("version", json!(doc.version)),
        ("status", json!(doc.status)),
        ("published", json!(doc.published)),
        ("updated", json!(doc.updated)),
        ("language", json!(doc.language)),
        ("doi", json!(doc.doi)),
        ("pmid", json!(doc.pmid)),
        ("license", json!(doc.rights.license.clone().unwrap_or_else(|| "UNKNOWN".into()))),
        ("rights", serde_json::to_value(&doc.rights).unwrap_or(Value::Null)),
        ("export_approved", json!(export_approved)),
    ] {
        m.insert(k.into(), v);
    }
    m
}

/// Rows for one document: an `article` row, chunked `text` rows per heading section, one `text` row per
/// recommendation (`block_type: "recommendation"`), `table_text`, `figure` + `caption` rows. Unit ids are stable for
/// the same document content and chunk recipe; `chunk_index`/`chunk_count` number the text chunks document-wide.
/// `figure_assets` maps a figure's `block_id` to its image member in `assets.tar` (empty when images were not fetched).
pub fn to_rows(doc: &GuidelineDoc, cfg: &ChunkCfg, export_approved: bool, canonical_member: &str,
               figure_assets: &std::collections::HashMap<String, String>) -> Vec<Map<String, Value>> {
    let id = doc.doc_id();
    let b = base(doc, export_approved);
    let mut rows = Vec::new();
    let mut art = b.clone();
    art.insert("unit_id".into(), json!(stable_id(&[json!(id), json!("article")])));
    art.insert("kind".into(), json!("article"));
    art.insert("canonical_member".into(), json!(canonical_member));
    art.insert("issuers".into(), json!(doc.issuers));
    art.insert("country".into(), json!(doc.country));
    art.insert("search_metadata".into(), doc.search_metadata.clone());
    art.insert("date_observations".into(), serde_json::to_value(&doc.date_observations).unwrap_or(Value::Null));
    art.insert("relations".into(), serde_json::to_value(&doc.relations).unwrap_or(Value::Null));
    art.insert("fetches".into(), serde_json::to_value(&doc.fetches).unwrap_or(Value::Null));
    art.insert("warnings".into(), json!(doc.warnings));
    let counts = |f: fn(&BlockKind) -> bool| doc.blocks.iter().filter(|x| f(&x.kind)).count();
    art.insert("unit_counts".into(), json!({
        "blocks": doc.blocks.len(),
        "recommendations": counts(|k| matches!(k, BlockKind::Recommendation { .. })),
        "tables": counts(|k| matches!(k, BlockKind::Table { .. })),
        "figures": counts(|k| matches!(k, BlockKind::Figure { .. })),
    }));
    rows.push(art);

    let mut text_rows: Vec<Map<String, Value>> = Vec::new();
    let ctx = |path: &[String]| if cfg.context_prefix { context_line(&doc.title, path) } else { String::new() };
    let with_ctx = |c: &str, body: &str| if c.is_empty() { body.to_string() } else { format!("{c}\n\n{body}") };
    // group consecutive prose blocks that share a heading path into one section, then chunk it
    let mut i = 0;
    while i < doc.blocks.len() {
        let blk = &doc.blocks[i];
        match &blk.kind {
            // headings are carried in every row's heading_path, not emitted as text of their own
            BlockKind::Heading { .. } => {}
            BlockKind::Paragraph | BlockKind::ListItem | BlockKind::Footnote => {
                let path = &blk.heading_path;
                let mut parts: Vec<&Block> = vec![];
                let mut j = i;
                while j < doc.blocks.len()
                    && matches!(doc.blocks[j].kind, BlockKind::Paragraph | BlockKind::ListItem | BlockKind::Footnote)
                    && &doc.blocks[j].heading_path == path
                {
                    parts.push(&doc.blocks[j]);
                    j += 1;
                }
                let body: Vec<String> = parts.iter().filter(|p| !p.text.trim().is_empty())
                    .map(|p| match p.kind { BlockKind::ListItem => format!("- {}", p.text.trim()), _ => p.text.trim().to_string() })
                    .collect();
                let section = body.join("\n\n");
                if !section.trim().is_empty() {
                    let block_ids: Vec<&str> = parts.iter().map(|p| p.block_id.as_str()).collect();
                    let c = ctx(path);
                    for (part, s, e, count) in split_text(&section, cfg.chunk_tokens, cfg.overlap_tokens) {
                        let mut r = b.clone();
                        r.insert("unit_id".into(), json!(stable_id(&[json!(id), json!("text"), json!(block_ids), json!(s), json!(e),
                            json!(COUNTER_NAME), json!(cfg.chunk_tokens), json!(cfg.overlap_tokens), json!(c)])));
                        r.insert("kind".into(), json!("text"));
                        r.insert("block_type".into(), json!("section"));
                        r.insert("context".into(), json!(c));
                        r.insert("text".into(), json!(with_ctx(&c, &part)));
                        r.insert("token_count".into(), json!(count));
                        r.insert("tokenizer".into(), json!(COUNTER_NAME));
                        r.insert("heading_path".into(), json!(path));
                        r.insert("locator".into(), json!({"block_ids": block_ids, "char_start": s, "char_end": e,
                            "page": parts.first().and_then(|p| p.page), "page_end": parts.iter().rev().find_map(|p| p.page)}));
                        r.insert("chunk_recipe".into(), json!({"budget": cfg.chunk_tokens, "overlap": cfg.overlap_tokens,
                            "context_prefix": cfg.context_prefix}));
                        text_rows.push(r);
                    }
                }
                i = j.max(i + 1);
                continue;
            }
            BlockKind::Recommendation { label, strength, certainty, population, grading_system, qualifier } => {
                // the bracket line puts the publisher's label and grade next to the statement for retrieval; the
                // statement itself stays verbatim, and the structured fields are in `recommendation`
                let tags: Vec<String> = [("", label), ("strength: ", strength), ("certainty: ", certainty),
                                         ("population: ", population), ("", qualifier)]
                    .iter().filter_map(|(k, v)| v.as_ref().filter(|v| !v.trim().is_empty()).map(|v| format!("{k}{}", v.trim()))).collect();
                let head = if tags.is_empty() { "[Recommendation]".to_string() } else { format!("[Recommendation {}]", tags.join(" · ")) };
                let c = ctx(&blk.heading_path);
                let mut r = b.clone();
                r.insert("unit_id".into(), json!(stable_id(&[json!(id), json!("recommendation"), json!(blk.block_id), json!(blk.text), json!(c)])));
                r.insert("kind".into(), json!("text"));
                r.insert("block_type".into(), json!("recommendation"));
                r.insert("context".into(), json!(c));
                r.insert("text".into(), json!(with_ctx(&c, &format!("{head}\n{}", blk.text))));
                r.insert("token_count".into(), json!(blk.text.split_whitespace().count()));
                r.insert("tokenizer".into(), json!(COUNTER_NAME));
                r.insert("heading_path".into(), json!(blk.heading_path));
                r.insert("recommendation".into(), json!({"label": label, "strength": strength, "certainty": certainty,
                    "population": population, "grading_system": grading_system, "qualifier": qualifier, "statement": blk.text}));
                r.insert("locator".into(), json!({"block_ids": [blk.block_id], "page": blk.page}));
                text_rows.push(r);
            }
            BlockKind::Table { caption } => {
                let words: Vec<&str> = blk.text.split_whitespace().collect();
                let truncated = words.len() > cfg.table_words;
                let text = if truncated { words[..cfg.table_words].join(" ") } else { blk.text.clone() };
                let c = ctx(&blk.heading_path);
                let mut r = b.clone();
                r.insert("unit_id".into(), json!(stable_id(&[json!(id), json!("table"), json!(blk.block_id), json!(cfg.table_words), json!(c)])));
                r.insert("kind".into(), json!("table_text"));
                r.insert("block_type".into(), json!("table"));
                r.insert("context".into(), json!(c));
                let body = match caption { Some(cap) => format!("{cap}\n\n{text}"), None => text };
                r.insert("text".into(), json!(with_ctx(&c, &body)));
                r.insert("token_count".into(), json!(words.len().min(cfg.table_words)));
                r.insert("tokenizer".into(), json!(COUNTER_NAME));
                r.insert("heading_path".into(), json!(blk.heading_path));
                r.insert("locator".into(), json!({"block_ids": [blk.block_id], "page": blk.page, "table_words": words.len(), "truncated": truncated}));
                rows.push(r);
            }
            BlockKind::Figure { src_url, caption } => {
                if let Some(cap) = caption.as_ref().filter(|c| !c.trim().is_empty()) {
                    let c = ctx(&blk.heading_path);
                    let mut r = b.clone();
                    r.insert("unit_id".into(), json!(stable_id(&[json!(id), json!("caption"), json!(blk.block_id), json!(c)])));
                    r.insert("kind".into(), json!("caption"));
                    r.insert("context".into(), json!(c));
                    r.insert("heading_path".into(), json!(blk.heading_path));
                    r.insert("text".into(), json!(with_ctx(&c, cap)));
                    r.insert("token_count".into(), json!(cap.split_whitespace().count()));
                    r.insert("tokenizer".into(), json!(COUNTER_NAME));
                    r.insert("locator".into(), json!({"figure_id": blk.block_id, "page": blk.page}));
                    rows.push(r);
                }
                if let Some(u) = src_url {
                    let mut r = b.clone();
                    r.insert("unit_id".into(), json!(stable_id(&[json!(id), json!("figure"), json!(blk.block_id), json!(u)])));
                    r.insert("kind".into(), json!("figure"));
                    r.insert("text".into(), json!(""));
                    r.insert("caption".into(), json!(caption));
                    r.insert("heading_path".into(), json!(blk.heading_path));
                    r.insert("source_url".into(), json!(u));
                    if let Some(a) = figure_assets.get(&blk.block_id) {
                        // image bytes fetched politely at ingest and stored in assets.tar
                        r.insert("asset_member".into(), json!(a));
                    }
                    r.insert("locator".into(), json!({"figure_id": blk.block_id, "page": blk.page}));
                    rows.push(r);
                }
            }
        }
        i += 1;
    }
    let n = text_rows.len();
    for (k, mut r) in text_rows.into_iter().enumerate() {
        r.insert("chunk_index".into(), json!(k));
        r.insert("chunk_count".into(), json!(n));
        rows.push(r);
    }
    for (k, a) in doc.attachments.iter().enumerate() {
        let mut r = b.clone();
        r.insert("unit_id".into(), json!(stable_id(&[json!(id), json!("media"), json!(k), a.clone()])));
        r.insert("kind".into(), json!("media"));
        r.insert("role".into(), json!("attachment"));
        r.insert("attachment".into(), a.clone());
        rows.push(r);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(id: &str, kind: BlockKind, text: &str, path: &[&str]) -> Block {
        Block { block_id: id.into(), kind, text: text.into(), heading_path: path.iter().map(|s| s.to_string()).collect(), page: None }
    }

    #[test]
    fn rows_have_pmc_compatible_kinds_and_stable_ids() {
        let doc = GuidelineDoc {
            source_id: "demo".into(), doc_key: "topic-2026".into(), title: "Demo guideline".into(), url: "https://example.org/g".into(),
            status: "current".into(), rights: Rights { status: "unknown".into(), ..Default::default() },
            blocks: vec![
                blk("h1", BlockKind::Heading { level: 1 }, "Background", &[]),
                blk("p1", BlockKind::Paragraph, "First paragraph about the condition.", &["Background"]),
                blk("p2", BlockKind::Paragraph, "Second paragraph.", &["Background"]),
                blk("r1", BlockKind::Recommendation { label: Some("1.1".into()), strength: Some("strong".into()), certainty: None,
                    population: None, grading_system: None, qualifier: None }, "Offer X to adults with Y.", &["Recommendations"]),
                blk("t1", BlockKind::Table { caption: Some("Table 1".into()) }, "| a | b |\n|---|---|\n| 1 | 2 |", &["Recommendations"]),
                blk("f1", BlockKind::Figure { src_url: Some("https://example.org/alg.png".into()), caption: Some("Algorithm".into()) }, "", &[]),
            ],
            ..Default::default()
        };
        let cfg = ChunkCfg::default();
        let none = std::collections::HashMap::new();
        let a = to_rows(&doc, &cfg, true, "guideline:demo:topic-2026/document.json", &none);
        let b = to_rows(&doc, &cfg, true, "guideline:demo:topic-2026/document.json", &none);
        let ids = |r: &Vec<Map<String, Value>>| r.iter().map(|x| x["unit_id"].clone()).collect::<Vec<_>>();
        assert_eq!(ids(&a), ids(&b));
        let kinds: Vec<&str> = a.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        for k in ["article", "text", "table_text", "caption", "figure"] {
            assert!(kinds.contains(&k), "missing {k}: {kinds:?}");
        }
        let texts: Vec<&Map<String, Value>> = a.iter().filter(|r| r["kind"] == "text").collect();
        assert!(texts.iter().all(|r| r["chunk_count"] == json!(texts.len())));
        assert!(texts.iter().any(|r| r["block_type"] == "recommendation" && r["recommendation"]["strength"] == "strong"));
        assert!(a.iter().all(|r| r["article_id"] == "guideline:demo:topic-2026" && r["license"] == "UNKNOWN"));
        // unit ids are unique within a document
        let mut seen = std::collections::HashSet::new();
        assert!(a.iter().all(|r| seen.insert(r["unit_id"].to_string())));
        // embedded text carries title + heading path; locator offsets do not
        let sec = texts.iter().find(|r| r["block_type"] == "section").unwrap();
        assert!(sec["text"].as_str().unwrap().starts_with("Demo guideline › Background\n\nFirst paragraph"), "{}", sec["text"]);
        let rec = texts.iter().find(|r| r["block_type"] == "recommendation").unwrap();
        assert_eq!(rec["text"], "Demo guideline › Recommendations\n\n[Recommendation 1.1 · strength: strong]\nOffer X to adults with Y.");
        assert_eq!(rec["recommendation"]["statement"], "Offer X to adults with Y.");
        let plain = to_rows(&doc, &ChunkCfg { context_prefix: false, ..ChunkCfg::default() }, true, "m", &none);
        assert!(plain.iter().any(|r| r.get("text").is_some_and(|t| t == "First paragraph about the condition.\n\nSecond paragraph.")));
    }

    #[test]
    fn context_line_skips_repeated_title() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(context_line("Asthma", &p(&["Asthma", "Treatment"])), "Asthma › Treatment");
        assert_eq!(context_line("Asthma", &p(&[])), "Asthma");
        assert_eq!(context_line("", &p(&["A", "B"])), "A › B");
    }
}
