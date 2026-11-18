//! Utilities owned by the source adapters (the shared guideline helpers are maintained separately).
use crate::guidelines::doc::{Block, BlockKind, DateObservation, Rights};
use crate::guidelines::html::{sel, text_of};
use anyhow::{bail, Result};
use scraper::{ElementRef, Html};
use std::collections::HashSet;
use url::Url;

/// Resolve only public HTTP links. Fragments identify locations, not distinct downloads.
pub fn link(base: &Url, href: &str) -> Option<String> {
    let mut u = base.join(href.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    u.set_fragment(None);
    Some(u.to_string())
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

pub fn short_hash(s: &str) -> String {
    crate::common::sha256_hex(s.as_bytes())[..12].to_string()
}

/// Explicit root selection: a changed template must not silently index the whole site's navigation.
pub fn root_selector(html: &Html, candidates: &[&str]) -> Result<String> {
    candidates.iter().find(|s| html.select(&sel(s)).next().is_some()).map(|s| (*s).to_string())
        .ok_or_else(|| anyhow::anyhow!("content root not found (tried {})", candidates.join(", ")))
}

pub fn first_text(html: &Html, selector: &str) -> Option<String> {
    html.select(&sel(selector)).map(|e| text_of(&e)).find(|t| !t.is_empty())
}

pub fn page_language(html: &Html) -> Option<String> {
    html.select(&sel("html[lang]")).next().and_then(|e| e.value().attr("lang")).map(String::from)
}

pub fn unknown_rights() -> Rights {
    Rights { status: "unknown".into(), ..Default::default() }
}

/// A date at exactly the precision the publisher printed; no fetch timestamp or URL-year inference.
pub fn date(label: &str, raw: &str, source: &str) -> DateObservation {
    let raw = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let iso = regex::Regex::new(r"^(?:19|20)\d{2}(?:-\d{2}(?:-\d{2})?)?$").unwrap();
    let mut normalized = iso.is_match(&raw).then(|| raw.clone());
    if normalized.is_none() {
        let re = regex::Regex::new(r"(?i)^(January|February|March|April|May|June|July|August|September|October|November|December)\s+(?:(\d{1,2}),?\s+)?((?:19|20)\d{2})$").unwrap();
        if let Some(c) = re.captures(&raw) {
            let months = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
            let m = months.iter().position(|m| *m == c[1].to_ascii_lowercase()).unwrap() + 1;
            normalized = match c.get(2) {
                Some(d) => d.as_str().parse::<u8>().ok().filter(|d| (1..=31).contains(d)).map(|d| format!("{}-{m:02}-{d:02}", &c[3])),
                None => Some(format!("{}-{m:02}", &c[3])),
            };
        }
    }
    DateObservation { label: label.into(), raw, normalized, source_url: Some(source.into()) }
}

/// Preserve tables, then insert formal statements immediately after their containing table.
/// Table row/cell classification is source-specific; this function only maintains block order and IDs.
pub fn insert_table_recommendations(blocks: &mut Vec<Block>, table_id: &str, statements: Vec<Block>) -> Result<()> {
    if statements.is_empty() { return Ok(()); }
    let position = blocks.iter().position(|b| matches!(b.kind, BlockKind::Table { .. }) && b.block_id == table_id);
    let Some(i) = position else { bail!("recommendation table was not preserved by the HTML extractor"); };
    let path = blocks[i].heading_path.clone();
    let mut used: HashSet<String> = blocks.iter().map(|b| b.block_id.clone()).collect();
    let rows = statements.into_iter().enumerate().map(|(n, mut b)| {
        b.heading_path = path.clone();
        if !used.insert(b.block_id.clone()) {
            b.block_id = format!("{}-rec-{}", blocks[i].block_id, n + 1);
            while !used.insert(b.block_id.clone()) { b.block_id.push('-'); }
        }
        b
    }).collect::<Vec<_>>();
    blocks.splice(i + 1..i + 1, rows);
    Ok(())
}

/// Direct table cells only: nested table text must not turn into extra recommendation columns.
pub fn cells(row: ElementRef<'_>) -> Vec<ElementRef<'_>> {
    row.children().filter_map(ElementRef::wrap).filter(|e| matches!(e.value().name(), "td" | "th")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_dates_keep_precision_and_unknowns() {
        assert_eq!(date("Published", "April 30, 2024", "https://example.org").normalized.as_deref(), Some("2024-04-30"));
        assert_eq!(date("Reviewed", "January 2016", "https://example.org").normalized.as_deref(), Some("2016-01"));
        assert_eq!(date("Edition", "2023", "https://example.org").normalized.as_deref(), Some("2023"));
        assert!(date("Updated", "last spring", "https://example.org").normalized.is_none());
    }

    #[test]
    fn public_links_and_strict_roots() {
        let base = Url::parse("https://example.org/guidelines/a").unwrap();
        assert_eq!(link(&base, "../b#recommendation").as_deref(), Some("https://example.org/b"));
        assert!(link(&base, "mailto:info@example.org").is_none());
        assert!(link(&base, "https://user:pass@example.org/").is_none());
        assert!(root_selector(&Html::parse_document("<body>Navigation only</body>"), &["main", "article"]).is_err());
    }
}
