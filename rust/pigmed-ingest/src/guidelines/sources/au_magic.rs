//! MAGICapp guidelines linked from Australian source pages (fix group I1-magic-pdq-hwp, 2026-11-17):
//! `au_cancer_council` topic pages and the LEAPP index of `au_department_pregnancy_care` link their current
//! guidelines only as `https://app.magicapp.org/#/guideline/<shortCode>[/section/...]` reader routes.
//!
//! User decision (2026-11-17): MAGICapp content is collected under the robots `*` group. app/files.magicapp.org
//! robots.txt names and disallows AI crawlers (ClaudeBot, Claude-User, GPTBot, CCBot, ...) and allows `*`; every
//! document records that in its warnings and `search_metadata.magic_robots_note`.
//!
//! Route (reuses `h2_catalog`'s MAGIC API code): the official MAGICapp API organisation listing
//! (`h2_catalog::magic_list_url`, organisations from `config.magic_api.org_short_names`) maps each linked shortCode to
//! its public record; `h2_catalog::magic_rows` turns that record into a plan row for the published PDF
//! (`pdfPublishedPath`, the file the reader's own PDF button opens). The source page that links the guideline
//! becomes a `related` relation (not a separate document) and the reader route a `rendition_of` relation.
//! `magicapp_public.rs` (the JSON-export reader) is pinned to one stroke publication (11482) and is not reused.
use super::h2_catalog::{magic_list_url, magic_rows};
use super::{common, PlanRow};
use crate::guidelines::doc::{FetchRecord, GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use url::Url;

pub const ROBOTS_NOTE: &str = "MAGICapp robots.txt (app.magicapp.org, files.magicapp.org) disallows named AI crawlers (e.g. ClaudeBot, Claude-User, GPTBot, CCBot) and allows `*`; PigMed's crawler identifies as PigMed-guidelines and falls under `*`. Collected under the `*` group by project decision (2026-11-17).";
pub const ROLE: &str = "magic_guideline_pdf";

/// shortCode of a MAGIC reader route: `https://app.magicapp.org/#/guideline/<code>[/...]`.
pub fn magic_code(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    if u.host_str() != Some("app.magicapp.org") {
        return None;
    }
    let frag = u.fragment()?;
    let rest = frag.trim_start_matches('/').strip_prefix("guideline/")?;
    let code = rest.split(['/', '?']).next()?;
    (!code.is_empty() && code.len() <= 32 && code.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| code.to_string())
}

/// Distinct shortCodes in link order.
pub fn codes<'a>(urls: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    urls.into_iter().filter_map(magic_code).filter(|c| seen.insert(c.clone())).collect()
}

pub fn reader(code: &str) -> String {
    format!("https://app.magicapp.org/#/guideline/{code}")
}

/// Public MAGIC records of the configured organisations, by shortCode: (record, org, list url, list fetch record).
#[derive(Default)]
pub struct MagicIndex {
    by_code: BTreeMap<String, (Value, String, String, Value)>,
    orgs: Vec<String>,
}

impl MagicIndex {
    pub fn from_lists(lists: Vec<(String, String, Value, Value)>) -> Result<Self> {
        let mut out = MagicIndex::default();
        for (org, url, list, fetch) in lists {
            out.orgs.push(org.clone());
            for g in list.as_array().context("MAGIC API answer is not a list")? {
                if g["institutionShortName"].as_str() != Some(org.as_str()) {
                    continue;
                }
                if let Some(code) = g["shortCode"].as_str() {
                    out.by_code.entry(code.to_string()).or_insert_with(|| (g.clone(), org.clone(), url.clone(), fetch.clone()));
                }
            }
        }
        Ok(out)
    }
    pub async fn load(src: &SourceEntry, f: &Fetcher) -> Result<Self> {
        let orgs = src.config["magic_api"]["org_short_names"].as_array().context("config.magic_api.org_short_names missing")?;
        let mut lists = vec![];
        for org in orgs.iter().filter_map(|o| o.as_str()) {
            let url = magic_list_url(org);
            let p = f.get_ok(&url).await?;
            let v: Value = serde_json::from_slice(&p.body).context("MAGIC API answer not JSON")?;
            lists.push((org.to_string(), url, v, serde_json::to_value(&p.record)?));
        }
        Self::from_lists(lists)
    }
    pub fn len(&self) -> usize {
        self.by_code.len()
    }
}

/// Plan rows for the MAGIC guidelines a landing page links. Unknown codes become unresolved rows (audited).
pub fn rows_for(src: &SourceEntry, index: &MagicIndex, codes: &[String], landing_url: &str, landing_title: &str, base_extra: &Value, seen: &mut HashSet<String>) -> Result<Vec<PlanRow>> {
    let mut out = vec![];
    for code in codes {
        let Some((g, org, list_url, fetch)) = index.by_code.get(code) else {
            out.push(PlanRow {
                source_id: src.id.clone(),
                doc_key: format!("magic-{}-unresolved", code.to_ascii_lowercase()),
                url: reader(code),
                title: None,
                rendition: None,
                version: None,
                extra: json!({"role": ROLE, "magic_code": code, "landing_url": landing_url, "landing_title": landing_title,
                    "discovery_error": format!("MAGIC guideline {code} (linked from {landing_url}) is not in the public MAGIC API listing of organisation(s) {:?}", index.orgs)}),
            });
            continue;
        };
        let mut more = magic_rows(src, org, &json!([g]), list_url, fetch, seen)?;
        for r in &mut more {
            if let (Some(e), Some(b)) = (r.extra.as_object_mut(), base_extra.as_object()) {
                for (k, v) in b {
                    e.entry(k.clone()).or_insert(v.clone());
                }
            }
            r.extra["role"] = json!(ROLE);
            r.extra["magic_code"] = json!(code);
            r.extra["landing_url"] = json!(landing_url);
            r.extra["landing_title"] = json!(landing_title);
        }
        out.extend(more);
    }
    Ok(out)
}

pub fn is_magic_row(row: &PlanRow) -> bool {
    row.extra["role"] == ROLE
}

/// Extract a MAGIC published-PDF row. `linked_codes` re-reads the landing page and returns the MAGIC shortCodes it
/// links now; the row is refused if the landing no longer links its guideline.
pub async fn extract(src: &SourceEntry, f: &Fetcher, row: &PlanRow, linked_codes: impl Fn(&str, &Url) -> Result<Vec<String>>) -> Result<GuidelineDoc> {
    if let Some(e) = row.extra["discovery_error"].as_str() {
        bail!("{} MAGIC route unresolved: {e}", src.id);
    }
    let code = row.extra["magic_code"].as_str().context("MAGIC shortCode missing")?;
    let landing_url = row.extra["landing_url"].as_str().context("MAGIC landing URL missing")?;
    let landing = f.get_ok(landing_url).await?;
    let lb = Url::parse(&landing.record.final_url)?;
    if lb.origin() != Url::parse(landing_url)?.origin() {
        bail!("{} landing page origin changed", src.id);
    }
    if !linked_codes(&landing.text(), &lb)?.iter().any(|c| c == code) {
        bail!("{} landing page no longer links MAGIC guideline {code}; re-plan", src.id);
    }
    let page = f.get_ok(&row.url).await?;
    if !page.is_pdf() {
        bail!("MAGIC published PDF path did not return PDF bytes");
    }
    let ocr = src.config.get("ocr_pages").and_then(|v| v.as_u64()).unwrap_or(30) as usize;
    let (blocks, mut warnings, _) = super::static_list::blocks_for(&page, None, ocr).await?;
    if blocks.is_empty() {
        bail!("MAGIC published PDF yielded no content");
    }
    let magic = &row.extra["search_metadata"]["magic"];
    let api = magic["api"].as_str().unwrap_or_default().to_string();
    let reader_url = magic["reader"].as_str().map(String::from).unwrap_or_else(|| reader(code));
    let landing_title = row.extra["landing_title"].as_str().unwrap_or_default();
    let relations = vec![
        Relation { rel: "rendition_of".into(), target: reader_url, scope: Some("published PDF of this MAGICapp guideline version (MAGIC API pdfPublishedPath)".into()), evidence_url: Some(api.clone()) },
        Relation { rel: "related".into(), target: landing_url.into(), scope: Some(format!("source page \"{landing_title}\" links this MAGICapp guideline (#/guideline/{code})")), evidence_url: Some(landing_url.into()) },
    ];
    let mut date_observations = vec![];
    if let Some(d) = magic["publish_date"].as_str() {
        date_observations.push(common::date("MAGIC API publishDate", d, &api));
    }
    warnings.push(ROBOTS_NOTE.into());
    warnings.push("MAGICapp published PDF: recommendation strength and certainty labels stay in the text; they are not structured from PDF. MAGIC status label kept verbatim in search_metadata.magic.status_label (NOTSET/null is not a currency claim).".into());
    let mut search_metadata = row.extra["search_metadata"].clone();
    search_metadata["magic_robots_note"] = json!(ROBOTS_NOTE);
    search_metadata["landing"] = json!({"url": landing_url, "title": landing_title});
    let mut fetches = vec![];
    for k in ["catalog_fetch"] {
        if let Ok(r) = serde_json::from_value::<FetchRecord>(row.extra[k].clone()) {
            fetches.push(r);
        }
    }
    if let Ok(r) = serde_json::from_value::<FetchRecord>(row.extra["search_metadata"]["api_fetch"].clone()) {
        fetches.push(r);
    }
    fetches.push(landing.record);
    fetches.push(page.record);
    Ok(GuidelineDoc {
        source_id: src.id.clone(),
        doc_key: row.doc_key.clone(),
        work_key: row.extra["work_key"].as_str().map(String::from),
        title: row.title.clone().unwrap_or_else(|| landing_title.to_string()),
        url: row.url.clone(),
        item_type: row.extra["item_type"].as_str().map(String::from),
        version: row.version.clone(),
        status: "unknown".into(),
        published: row.extra["published"].as_str().map(String::from),
        date_observations,
        relations,
        rights: Rights { status: "unknown".into(), note: Some("No licence is stated with the MAGICapp published PDF; reuse rights need review.".into()), ..Default::default() },
        search_metadata,
        blocks,
        fetches,
        warnings,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reader_routes_give_short_codes() {
        assert_eq!(magic_code("https://app.magicapp.org/#/guideline/jm83RE/section/jDeKmV").as_deref(), Some("jm83RE"));
        assert_eq!(magic_code("https://app.magicapp.org/#/guideline/Lkk3pL").as_deref(), Some("Lkk3pL"));
        assert_eq!(magic_code("https://app.magicapp.org/#/guidelines").as_deref(), None);
        assert_eq!(magic_code("https://example.org/#/guideline/jm83RE").as_deref(), None);
        assert_eq!(codes(["https://app.magicapp.org/#/guideline/jm83RE/section/a", "https://app.magicapp.org/#/guideline/jW0ZbL", "https://app.magicapp.org/#/guideline/jm83RE/section/b"]), vec!["jm83RE", "jW0ZbL"]);
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod route_tests {
    //! Genuine MAGIC API organisation answer (h2_catalog fixture, org `cari`) through the shared route, with a small
    //! landing page that links one of its guidelines. A real small PDF (Cancer Council Barrett excerpt) stands in for
    //! the MAGIC published PDF: conversion is not what is tested here.
    use super::*;
    use crate::guidelines::html::sel;
    use scraper::Html;
    const CARI: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/h2_catalog/magic-cari.json"));
    const PDF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/au_cancer_council/barrett-excerpt.pdf"));
    const LANDING: &str = "https://www.cariguidelines.org/living-guidelines/";
    const CKD_PDF: &str = "https://files.magicapp.org/guideline/2d3034bd-4d24-4963-9259-071abda1f507/published_guideline_9509-3_1.pdf";
    fn linked(html: &str, _: &Url) -> Result<Vec<String>> {
        let d = Html::parse_document(html);
        let hrefs: Vec<String> = d.select(&sel("a[href]")).filter_map(|a| a.value().attr("href").map(String::from)).collect();
        Ok(codes(hrefs.iter().map(|s| s.as_str())))
    }
    #[tokio::test]
    async fn linked_short_code_becomes_the_published_pdf_document() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 8 << 20, "").unwrap();
        f.seed_cache_for_tests(&magic_list_url("cari"), CARI.as_bytes(), "application/json");
        f.seed_cache_for_tests(LANDING, b"<main><a href=\"https://app.magicapp.org/#/guideline/Lpmozn/section/abc\">CKD</a></main>", "text/html");
        f.seed_cache_for_tests(CKD_PDF, PDF, "application/pdf");
        let src: SourceEntry = serde_json::from_value(json!({"id":"magic_route_test","access":"public","enabled":true,
            "config":{"magic_api":{"org_short_names":["cari"],"item_type":"guideline","scope":"test"}}})).unwrap();
        let idx = MagicIndex::load(&src, &f).await.unwrap();
        assert_eq!(idx.len(), 5);
        let rows = rows_for(&src, &idx, &["Lpmozn".into(), "Zz9999".into()], LANDING, "CARI living guidelines", &json!({}), &mut HashSet::new()).unwrap();
        assert_eq!(rows.len(), 2);
        let r = &rows[0];
        assert_eq!(r.doc_key, "magic-9509-v3-1");
        assert_eq!(r.url, CKD_PDF);
        assert!(is_magic_row(r) && r.extra["magic_code"] == "Lpmozn" && r.extra["landing_url"] == LANDING);
        assert_eq!(rows[1].doc_key, "magic-zz9999-unresolved");
        assert!(rows[1].extra["discovery_error"].as_str().unwrap().contains("not in the public MAGIC API listing"));
        assert!(extract(&src, &f, &rows[1], linked).await.is_err());
        let d = extract(&src, &f, r, linked).await.unwrap();
        assert_eq!(d.item_type.as_deref(), Some("guideline"));
        assert_eq!(d.version.as_deref(), Some("3.1"));
        assert_eq!(d.published.as_deref(), Some("2025-06-20"));
        assert_eq!(d.status, "unknown");
        assert!(d.relations.iter().any(|x| x.rel == "rendition_of" && x.target == "https://app.magicapp.org/#/guideline/Lpmozn"));
        assert!(d.relations.iter().any(|x| x.rel == "related" && x.target == LANDING));
        assert!(d.warnings.iter().any(|w| w.contains("named AI crawlers")));
        assert_eq!(d.search_metadata["magic"]["status_label"], "NOTSET");
        assert!(d.search_metadata["magic_robots_note"].is_string());
        assert_eq!(d.fetches.len(), 3);
        assert!(!d.blocks.is_empty());
        // The landing page no longer links the guideline: refused, re-plan.
        f.seed_cache_for_tests(LANDING, b"<main><a href=\"https://app.magicapp.org/#/guideline/LqR80n\">other</a></main>", "text/html");
        assert!(format!("{:#}", extract(&src, &f, r, linked).await.unwrap_err()).contains("no longer links"));
    }
}
