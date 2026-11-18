//! Independently observed ASTRO pocket summary; the blocked catalog remains separate.
use super::{common, static_list::blocks_for, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, GuidelineDoc, Rights};
use crate::guidelines::fetch::{Fetched, Fetcher};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub struct Astro;
const POCKET: &str = "https://www.astro.org/ASTRO/media/ASTRO/Patient%20Care%20and%20Research/PDFs/PMRT_Pocketcard.pdf";

const SITEMAP: &str = "https://astro.org/sitemap/index";
const TOPIC_PREFIX: &str = "/provider-resources/guidelines/clinical-practice-guidelines/";

/// `<loc>` values of a sitemap or sitemap index, in document order.
fn sitemap_locs(xml: &str) -> (bool, Vec<String>) {
    let is_index = xml.contains("<sitemapindex");
    let re = regex::Regex::new(r"<loc>\s*([^<\s]+)\s*</loc>").unwrap();
    (is_index, re.captures_iter(xml).map(|c| c[1].replace("&amp;", "&")).collect())
}
fn topic_urls(locs: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    locs.iter().filter(|u| url::Url::parse(u).is_ok_and(|x| {
        let p = x.path().to_lowercase();
        p.starts_with(TOPIC_PREFIX) && p.trim_end_matches('/').len() > TOPIC_PREFIX.len()
    })).filter(|u| seen.insert(u.to_lowercase())).cloned().collect()
}
/// Bounded walk: the index plus at most 40 child sitemaps.
async fn sitemap_topics(fetcher: &Fetcher, index: &str) -> (Vec<String>, String) {
    let root = match fetcher.get_ok(index).await { Ok(p) => p, Err(e) => return (vec![], format!("ASTRO sitemap unavailable: {e:#}")) };
    let (is_index, locs) = sitemap_locs(&root.text());
    if !is_index { let t = topic_urls(&locs); let n = format!("sitemap {} locs, {} guideline topics", locs.len(), t.len()); return (t, n); }
    let mut topics = vec![]; let mut notes = vec![];
    for child in locs.iter().take(40) {
        match fetcher.get_ok(child).await {
            Ok(p) => { let (_, l) = sitemap_locs(&p.text()); topics.extend(topic_urls(&l)); }
            Err(e) => notes.push(format!("{child}: {e:#}")),
        }
    }
    let mut seen = std::collections::HashSet::new();
    topics.retain(|u| seen.insert(u.to_lowercase()));
    let note = format!("sitemap index {} children (walked {}), {} guideline topics; child errors: {}", locs.len(), locs.len().min(40), topics.len(), notes.join(" | "));
    (topics, note)
}

fn complete(response: &Fetched) -> bool {
    response.is_pdf() && response.body[response.body.len().saturating_sub(2048)..]
        .windows(5).any(|w| w == b"%%EOF")
}

fn source_metadata(blocks: &[Block], url: &str) -> Result<Value> {
    if !blocks.iter().take(3).any(|b| b.text == "Postmastectomy Radiation")
        || !blocks.iter().take(3).any(|b| b.text == "Therapy")
        || !blocks.iter().any(|b| b.text.contains("This pocket guide is derived from recommendations in the ASTRO-ASCO-SSO Guideline")) {
        bail!("ASTRO asset is no longer the independently observed PMRT pocket summary");
    }
    let doi = regex::Regex::new(r"10\.1200/JCO\.[0-9.]+")?;
    let printed = regex::Regex::new(r"\((Strong|Conditional) Recommendation;\s*([^)]*)\)")?;
    let mut citations = vec![];
    let mut grades = vec![];
    for block in blocks {
        for m in doi.find_iter(&block.text) {
            citations.push(json!({
                "printed_doi": m.as_str().trim_end_matches('.'),
                "source_block_id": block.block_id, "source_text": block.text,
                "source_url": url,
                "scope": "underlying guideline citation; not the pocket summary's own DOI"
            }));
        }
        for c in printed.captures_iter(&block.text) {
            grades.push(json!({
                "printed": &c[0], "printed_strength": &c[1], "printed_quality": &c[2],
                "source_block_id": block.block_id, "source_text": block.text,
                "scope": "block-local occurrence only; no whole-table statement association inferred"
            }));
        }
    }
    Ok(json!({
        "underlying_guideline_citations": citations,
        "printed_grade_occurrences": grades,
        "source_disclaimer": blocks.iter().filter(|b| b.text.contains("This pocket guide is derived"))
            .map(|b| json!({"source_block_id":b.block_id,"text":b.text})).collect::<Vec<_>>(),
        "coverage": {
            "converted_blocks": blocks.len(),
            "converted_tables": blocks.iter().filter(|b| matches!(b.kind,BlockKind::Table{..})).count(),
            "physical_page_mapping": "unverified", "complete_visual_semantics": false
        }
    }))
}

fn rights(blocks: &[Block], url: &str) -> Rights {
    let notices = blocks.iter().filter(|b| b.text.contains("Copyright © 2025")
        && (b.text.contains("All rights reserved") || b.text.contains("American Society for Radiation Oncology")))
        .map(|b| b.text.clone()).collect::<Vec<_>>();
    if notices.is_empty() { return common::unknown_rights(); }
    let joined = notices.join("\n");
    let holders = ["American Society for Radiation Oncology", "American Society of Clinical Oncology", "Society of Surgical Oncology"]
        .into_iter().filter(|name| joined.contains(name)).map(String::from).collect();
    Rights { status: "restricted".into(), license: Some("All rights reserved".into()),
        restrictions: notices, rights_holders: holders, evidence_urls: vec![url.into()],
        note: Some("Printed pocket-summary notice; underlying journal and other assets have independent rights.".into()),
        ..Default::default() }
}

#[async_trait::async_trait]
impl Adapter for Astro {
    async fn discover(&self, source: &SourceEntry, fetcher: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) { return Ok(vec![]); }
        // This exact independently evidenced public asset is not a guessed replacement
        // for a blocked catalog or a claimed catalog enumeration.
        let mut row = PlanRow { source_id: source.id.clone(), doc_key: "pmrt-pocket-unresolved".into(),
            url: POCKET.into(), title: Some("Postmastectomy Radiation Therapy".into()), rendition: Some("pdf".into()),
            version: None, extra: json!({"role":"guideline_pocket_summary","inventory_scope":"one independently evidenced source asset; complete catalog unavailable"}) };
        match fetcher.get_ok(POCKET).await {
            Ok(pdf) if complete(&pdf) => {
                row.doc_key = format!("pmrt-pocket-{}", &pdf.record.sha256[..12]);
                row.extra["pdf_sha256"] = json!(pdf.record.sha256);
            }
            Ok(_) => row.extra["discovery_error"] = json!("ASTRO pocket URL did not return complete PDF bytes"),
            Err(error) => row.extra["discovery_error"] = json!(format!("{error:#}")),
        }
        let mut rows = vec![row];
        if limit.is_some_and(|n| rows.len() >= n) { return Ok(rows); }
        let url = source.catalog_url.as_deref().context("ASTRO source catalog missing")?;
        let error = match fetcher.get_ok(url).await {
            Err(error) => format!("ASTRO catalog unavailable: {error:#}"),
            Ok(_) => "ASTRO catalog responded, but source-wide cards/status/terminal enumeration is not validated; no empty or complete inventory inferred".into(),
        };
        rows.push(PlanRow { source_id: source.id.clone(), doc_key: "catalog-unresolved".into(), url: url.into(),
            title: None, rendition: None, version: None, extra: json!({"role":"catalog_disposition","discovery_error":error}) });
        // Official route declared in robots.txt ("Sitemap: https://astro.org/sitemap/index"): enumerate guideline
        // topic pages from it. Diagnostic in fix round 1: each topic's response is recorded, nothing is extracted yet.
        if limit.is_some_and(|n| rows.len() >= n) { return Ok(rows); }
        let (topics, sitemap_note) = sitemap_topics(fetcher, SITEMAP).await;
        if topics.is_empty() {
            rows.push(PlanRow { source_id: source.id.clone(), doc_key: "sitemap-unresolved".into(), url: SITEMAP.into(), title: None,
                rendition: None, version: None, extra: json!({"role":"sitemap_disposition","discovery_error":sitemap_note}) });
            return Ok(rows);
        }
        for topic in topics {
            if limit.is_some_and(|n| rows.len() >= n) { break; }
            let why = match fetcher.get(&topic).await {
                Ok(p) => format!("ASTRO topic answered HTTP {} at {} ({}; {} bytes); topic template not yet validated", p.record.status, p.record.final_url,
                    p.record.content_type.as_deref().unwrap_or("unknown type"), p.body.len()),
                Err(e) => format!("ASTRO topic unavailable: {e:#}"),
            };
            rows.push(PlanRow { source_id: source.id.clone(), doc_key: format!("topic-{}-unresolved", common::short_hash(&topic)), url: topic,
                title: None, rendition: None, version: None, extra: json!({"role":"sitemap_topic","sitemap":SITEMAP,"sitemap_note":sitemap_note,"discovery_error":why}) });
        }
        Ok(rows)
    }

    async fn extract(&self, source: &SourceEntry, fetcher: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(error) = row.extra["discovery_error"].as_str() { bail!("ASTRO discovery: {error}"); }
        if row.url != POCKET || row.extra["role"] != "guideline_pocket_summary" { bail!("ASTRO unverified source rendition"); }
        let pdf = fetcher.get_ok(&row.url).await?;
        if !complete(&pdf) || row.extra["pdf_sha256"].as_str() != Some(pdf.record.sha256.as_str()) {
            bail!("ASTRO PDF changed or is incomplete; re-plan");
        }
        let (mut blocks, mut warnings, _) = blocks_for(&pdf, None, 30).await?;
        for block in &mut blocks { block.page = None; }
        let metadata = source_metadata(&blocks, &row.url)?;
        let rights = rights(&blocks, &row.url);
        let mut dates = vec![];
        if blocks.iter().any(|b| b.text.contains("Copyright © 2025 All rights reserved")) {
            dates.push(common::date("Printed pocket-summary copyright year", "2025", &row.url));
        }
        for b in &blocks {
            if b.text.contains("Published 2025 Sept 16") && b.text.contains("J Clin Oncol") {
                dates.push(common::date("Underlying guideline citation: Published", "2025 Sept 16", &row.url));
            }
        }
        warnings.push("One independently observed public pocket summary, not the full guideline or a reconciled ASTRO catalog. Catalog403 and journal access remain separate. The pocket guide prints two conflicting underlying-work DOI forms; neither is assigned as this summary's DOI or silently corrected.".into());
        warnings.push("All converted source blocks, six observed tables, dose/fraction values, implementation notes, abbreviations, disclaimer and copyright tail are retained. Columns/merged cells, detached superscripts and the Figure1 algorithm are not visually reconciled. A converted table is not proof of faithful branch geometry; block-local grade occurrences are not projected over mixed conditions, and physical pages remain unverified.".into());
        Ok(GuidelineDoc { source_id: source.id.clone(), doc_key: row.doc_key.clone(), work_key: Some("pmrt-pocket-summary".into()),
            title: "Postmastectomy Radiation Therapy".into(), url: row.url.clone(), item_type: Some("guideline_pocket_summary".into()),
            status: "unknown".into(), rights, date_observations: dates, search_metadata: metadata,
            blocks, fetches: vec![pdf.record], warnings, ..Default::default() })
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const PDF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/astro/pmrt-pocketcard.pdf"));
    const CATALOG: &str = "https://www.astro.org/provider-resources/guidelines/clinical-practice-guidelines";
    #[tokio::test]
    async fn genuine_pocket_summary_keeps_all_tables_conditions_citations_and_independent_catalog_failure() {
        assert_eq!(PDF.len(), 377944);
        assert_eq!(crate::common::sha256_hex(PDF), "95555ade0a8f74078bed7fc7cc91b87c6266d9f7b64443fea4bf227d3df69e4f");
        let temp = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(temp.path(), true, 1, 2 << 20, "").unwrap();
        fetcher.seed_cache_for_tests(POCKET, PDF, "application/pdf");
        let source: SourceEntry = serde_json::from_value(json!({"id":"astro","enabled":true,"access":"public","catalog_url":CATALOG})).unwrap();
        assert!(Astro.discover(&source, &fetcher, Some(0)).await.unwrap().is_empty());
        assert_eq!(Astro.discover(&source, &fetcher, Some(1)).await.unwrap().len(), 1);
        let rows = Astro.discover(&source, &fetcher, Some(3)).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[1].extra["discovery_error"].as_str().unwrap().contains("offline and not cached"));
        assert_eq!(rows[2].extra["role"], "sitemap_disposition");
        assert!(rows[2].extra["discovery_error"].as_str().unwrap().contains("sitemap unavailable"));
        let doc = Astro.extract(&source, &fetcher, &rows[0]).await.unwrap();
        assert_eq!(doc.blocks.len(), 39);
        assert_eq!(doc.blocks.iter().filter(|b| matches!(b.kind,BlockKind::Table{..})).count(), 6);
        assert!(doc.blocks.iter().any(|b| b.text.contains("pN1mic") && b.text.contains("favorable clinicopathologic features")));
        assert!(doc.blocks.iter().any(|b| b.text.contains("cT4 or cN2-3") && b.text.contains("regardless of pathologic response")));
        assert!(doc.blocks.iter().any(|b| b.text.contains("Ipsilateral lung") && b.text.contains("5000–5040 cGy")));
        assert!(doc.blocks.iter().any(|b| b.text.contains("complete pathological response") && b.text.contains("ypT0N0")));
        assert!(doc.blocks.last().unwrap().text.contains("ASCOPMRT08253"));
        let citations = doc.search_metadata["underlying_guideline_citations"].as_array().unwrap();
        assert!(citations.iter().any(|c| c["printed_doi"] == "10.1200/JCO.25.25.01747"));
        assert!(citations.iter().any(|c| c["printed_doi"] == "10.1200/JCO.25.01747"));
        assert_eq!(doc.item_type.as_deref(), Some("guideline_pocket_summary"));
        assert_eq!(doc.rights.status, "restricted");
        assert!(doc.doi.is_none() && doc.published.is_none() && doc.updated.is_none() && doc.country.is_none() && doc.version.is_none());
        assert!(doc.blocks.iter().all(|b| b.page.is_none() && !matches!(b.kind,BlockKind::Recommendation{..})));
        let mut changed = rows[0].clone(); changed.extra["pdf_sha256"] = json!("changed");
        assert!(Astro.extract(&source, &fetcher, &changed).await.is_err());
    }

    #[test]
    fn sitemap_topics_are_bounded_to_guideline_pages() {
        let xml = "<?xml version='1.0'?><urlset><url><loc>https://www.astro.org/provider-resources/guidelines/clinical-practice-guidelines</loc></url><url><loc> https://www.astro.org/provider-resources/guidelines/clinical-practice-guidelines/postmastectomy-radiation-therapy </loc></url><url><loc>https://www.astro.org/news</loc></url><url><loc>https://www.astro.org/Provider-Resources/Guidelines/Clinical-Practice-Guidelines/Postmastectomy-Radiation-Therapy</loc></url></urlset>";
        let (index, locs) = sitemap_locs(xml);
        assert!(!index && locs.len() == 4);
        assert_eq!(topic_urls(&locs), vec!["https://www.astro.org/provider-resources/guidelines/clinical-practice-guidelines/postmastectomy-radiation-therapy".to_string()]);
        assert!(sitemap_locs("<sitemapindex><sitemap><loc>https://astro.org/sitemap/1</loc></sitemap></sitemapindex>").0);
    }
}
