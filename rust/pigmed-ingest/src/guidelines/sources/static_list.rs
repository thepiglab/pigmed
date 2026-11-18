//! `static_list`: a generic adapter driven entirely by the registry entry's `config`, and the reference
//! implementation for real adapters. It works for any source whose document URLs are known up front (a handful of
//! PDFs or pages), and is useful to pilot a source before it gets a proper catalog crawler.
//!
//! ```json
//! "adapter": "static_list",
//! "config": {
//!   "root": "main",                       // CSS selector for HTML content (optional)
//!   "publisher": "…", "country": "…", "language": "en",
//!   "rights": {"license": "OGL-3.0", "status": "open", "terms_url": "…"},
//!   "documents": [ {"url": "https://…/guideline.pdf", "doc_key": "diabetes-2023", "title": "…", "version": "6.0"} ]
//! }
//! ```
use super::{Adapter, PlanRow};
use crate::guidelines::doc::{BlockKind, GuidelineDoc, Rights};
use crate::guidelines::fetch::{Fetched, Fetcher};
use crate::guidelines::html::{blocks_from_html, blocks_from_markdown, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{anyhow, Result};
use serde_json::Value;

pub struct StaticList;

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(String::from)
}

/// Shared by adapters: turn one fetched rendition into blocks (HTML directly; PDF/DOCX/... via `convert`).
pub async fn blocks_for(f: &Fetched, root: Option<&str>, ocr_pages: usize) -> Result<(Vec<crate::guidelines::doc::Block>, Vec<String>, Option<String>)> {
    if f.declared_pdf_but_not() {
        // .pdf URL / PDF content type, HTML bytes: a viewer, download wrapper or error shell, never the document
        return Err(anyhow!("expected a PDF at {} but got {} bytes of {} (viewer/wrapper page?)", f.record.final_url,
            f.body.len(), f.record.content_type.as_deref().unwrap_or("unknown type")));
    }
    if f.is_pdf() || !f.is_html() {
        let ext = if f.is_pdf() { ".pdf".to_string() } else {
            std::path::Path::new(url::Url::parse(&f.record.final_url)?.path()).extension()
                .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase())).unwrap_or_else(|| ".bin".into())
        };
        let dir = tempfile::tempdir()?;
        let p = dir.path().join(format!("doc{ext}"));
        std::fs::write(&p, &f.body)?;
        let conv = tokio::task::spawn_blocking(move || {
            let r = crate::convert::convert(&p, &ext, crate::convert::Opts { ocr_pages });
            drop(dir);
            r
        }).await?.map_err(|e| anyhow!("convert failed: {e}"))?;
        let mut warnings = vec![];
        if conv.pages_without_text > 0 {
            warnings.push(format!("{} image-only pages without text (OCR cap {ocr_pages})", conv.pages_without_text));
        }
        return Ok((blocks_from_markdown(&conv.md), warnings, None));
    }
    let html = f.text();
    let base = url::Url::parse(&f.record.final_url).ok();
    let (blocks, warnings) = blocks_from_html(&html, base.as_ref(), root);
    let title = scraper::Html::parse_document(&html).select(&sel("title")).next().map(|t| text_of(&t)).filter(|t| !t.is_empty());
    Ok((blocks, warnings, title))
}

#[async_trait::async_trait]
impl Adapter for StaticList {
    async fn discover(&self, src: &SourceEntry, _f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let docs = src.config.get("documents").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        let mut out = vec![];
        for (i, d) in docs.iter().enumerate() {
            let url = s(d, "url").ok_or_else(|| anyhow!("documents[{i}] has no url"))?;
            out.push(PlanRow {
                source_id: src.id.clone(),
                doc_key: s(d, "doc_key").unwrap_or_else(|| crate::common::stable_id(&[serde_json::json!(url)])[..12].to_string()),
                url, title: s(d, "title"), rendition: s(d, "rendition"), version: s(d, "version"), extra: d.clone(),
            });
            if limit.is_some_and(|l| out.len() >= l) {
                break;
            }
        }
        Ok(out)
    }

    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        let cfg = &src.config;
        let fetched = f.get_ok(&row.url).await?;
        let root = s(&row.extra, "root").or_else(|| s(cfg, "root"));
        let ocr = cfg.get("ocr_pages").and_then(|v| v.as_u64()).unwrap_or(30) as usize;
        let (blocks, warnings, page_title) = blocks_for(&fetched, root.as_deref(), ocr).await?;
        let first_heading = blocks.iter().find(|b| matches!(b.kind, BlockKind::Heading { .. })).map(|b| b.text.clone());
        let rights: Rights = cfg.get("rights").cloned().and_then(|r| serde_json::from_value(r).ok())
            .unwrap_or(Rights { status: "unknown".into(), ..Default::default() });
        Ok(GuidelineDoc {
            source_id: src.id.clone(),
            doc_key: row.doc_key.clone(),
            work_key: s(&row.extra, "work_key"),
            item_type: s(&row.extra, "item_type").or_else(|| s(cfg, "item_type")),
            title: row.title.clone().or(first_heading).or(page_title).unwrap_or_else(|| row.url.clone()),
            url: row.url.clone(),
            // publisher/country only as configured or stated: the registry's source name is the operator, and its
            // scope is a research label, neither is the document's issuer or jurisdiction
            publisher: s(&row.extra, "publisher").or_else(|| s(cfg, "publisher")),
            issuers: vec![],
            version: row.version.clone(),
            status: s(&row.extra, "status").unwrap_or_else(|| "unknown".into()),
            published: s(&row.extra, "published"),
            updated: s(&row.extra, "updated"),
            date_observations: vec![],
            relations: vec![],
            language: s(cfg, "language"),
            country: s(&row.extra, "country").or_else(|| s(cfg, "country")),
            doi: s(&row.extra, "doi"),
            pmid: s(&row.extra, "pmid"),
            rights,
            search_metadata: row.extra.get("search_metadata").cloned().unwrap_or(Value::Null),
            blocks,
            attachments: vec![],
            fetches: vec![fetched.record],
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn zero_limit_skips_documents_and_ordinary_limits_keep_the_first_row() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 20, "").unwrap();
        let mut src: SourceEntry = serde_json::from_value(json!({
            "id":"sample", "access":"public", "enabled":true,
            "config":{"documents":[{"title":"No URL: should not be visited for zero limit"}]}
        })).unwrap();
        assert!(StaticList.discover(&src, &f, Some(0)).await.unwrap().is_empty());
        assert!(StaticList.discover(&src, &f, None).await.is_err());
        src.config["documents"] = json!([
            {"url":"https://example.org/first.pdf", "doc_key":"first"},
            {"url":"https://example.org/second.pdf", "doc_key":"second"}
        ]);
        let rows = StaticList.discover(&src, &f, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].doc_key, "first");
        assert_eq!(StaticList.discover(&src, &f, None).await.unwrap().len(), 2);
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }
}
