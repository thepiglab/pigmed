//! ESO source guideline directory. Directory entries that state a DOI go through the PMC open-access crosswalk
//! (ESJ guidelines are mostly CC licensed and in PMC); entries without an OA copy keep the publisher-terminal
//! outcome (OUP/Sage answer 403) as an explicit unresolved row.
use super::pmc_crosswalk::{self as cw, Lookup, Planned, SourceRef};
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::GuidelineDoc;
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct Eso;
const ROOT: &str = "https://eso-stroke.org/guidelines/eso-guideline-directory/";
fn catalog(raw: &str, base: &Url) -> Result<Vec<(String, String)>> {
    let d = Html::parse_document(raw);
    let mut seen = HashSet::new();
    let mut rows = vec![];
    let roots = d.select(&sel(".post-entry-2143 .entry-content-wrapper")).collect::<Vec<_>>();
    if roots.is_empty() {
        bail!("ESO source directory components missing")
    };
    for root in roots {
        for a in root.select(&sel("a[href]")) {
            let title = text_of(&a);
            let l = title.to_lowercase();
            if !l.contains("guideline") || l.contains("slide") || l.contains("video") {
                continue;
            };
            let Some(url) = a.value().attr("href").and_then(|h| common::link(base, h)) else { continue };
            let u = Url::parse(&url)?;
            if matches!(u.scheme(), "http" | "https") && seen.insert(url.clone()) {
                rows.push((url, title))
            }
        }
    }
    if rows.is_empty() {
        bail!("ESO source guideline links missing")
    };
    Ok(rows)
}

/// Directory entries in a deterministic order: DOI-bearing links first (deduplicated by DOI), then links without a
/// DOI whose title does not repeat a DOI-bearing entry.
fn ordered(entries: Vec<(String, String)>) -> Vec<(String, String, Option<String>)> {
    let mut dois = HashSet::new();
    let mut titled = HashSet::new();
    let mut first = vec![];
    for (u, t) in &entries {
        if let Some(d) = cw::doi_from_url(u) {
            titled.insert(cw::norm_title(t));
            if dois.insert(d.clone()) {
                first.push((u.clone(), t.clone(), Some(d)));
            }
        }
    }
    for (u, t) in entries {
        if cw::doi_from_url(&u).is_none() && !titled.contains(&cw::norm_title(&t)) {
            first.push((u, t, None));
        }
    }
    first
}

#[async_trait::async_trait]
impl Adapter for Eso {
    async fn discover(&self, src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let c = f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;
        let base = Url::parse(&c.record.final_url)?;
        if base.origin() != Url::parse(ROOT)?.origin() {
            bail!("ESO directory redirected off source")
        };
        let entries = ordered(catalog(&c.text(), &base)?);
        let mut rows = vec![];
        for (url, title, doi) in entries {
            let mut crosswalk = "no DOI stated by the directory for this link".to_string();
            if let Some(doi) = doi {
                let sref = SourceRef {
                    source_url: url.clone(),
                    evidence_url: Some(base.to_string()),
                    source_title: Some(title.clone()),
                    item_type: Some("guideline".into()),
                    key_prefix: "eso".into(),
                    id_route: "source_doi".into(),
                    notes: vec![],
                };
                match cw::plan(src, f, Lookup::Doi(doi.clone()), sref, json!({"directory_fetch": c.record, "source_doi": doi})).await? {
                    Planned::Open(row) => {
                        rows.push(row);
                        if limit.is_some_and(|n| rows.len() >= n) {
                            break;
                        }
                        continue;
                    }
                    Planned::Closed { reason, .. } => crosswalk = format!("DOI {doi}: {reason}"),
                }
            }
            let msg = match f.get_ok(&url).await {
                Err(e) => format!("ESO source publisher terminal unavailable: {e:#}; open-access crosswalk: {crosswalk}"),
                Ok(_) => format!("ESO publisher terminal responded but its full article template and rights are unvalidated; open-access crosswalk: {crosswalk}"),
            };
            rows.push(PlanRow {
                source_id: src.id.clone(),
                doc_key: format!("eso-{}-unresolved", common::short_hash(&url)),
                url,
                title: Some(title),
                rendition: None,
                version: None,
                extra: json!({"directory_fetch":c.record,"discovery_error":msg}),
            });
            if limit.is_some_and(|n| rows.len() >= n) {
                break;
            }
        }
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if cw::is_crosswalk_row(row) {
            return cw::extract(src, f, row).await;
        }
        bail!("ESO publisher terminal unresolved: {}", row.extra["discovery_error"].as_str().unwrap_or("unknown"))
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    use crate::guidelines::doc::BlockKind;
    const CAT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/eso/catalog.html"));
    #[test]
    fn genuine_multisection_directory_has_distinct_publisher_urls() {
        let r = catalog(CAT, &Url::parse(ROOT).unwrap()).unwrap();
        assert!(r.iter().any(|(u, t)| u.contains("aakag004/8671384?preview=true&login=true") && t.contains("Blood Pressure")));
        assert!(r.iter().any(|(u, t)| u.contains("aakag043/8671382?preview=true&login=true") && t.contains("Aneurysmal")));
        assert!(!r.iter().any(|(_, t)| t.to_lowercase().contains("download the slides")));
        let o = ordered(r);
        assert!(o[0].2.is_some(), "DOI-bearing entries first");
        let dois: Vec<&str> = o.iter().filter_map(|x| x.2.as_deref()).collect();
        assert!(dois.contains(&"10.1177/23969873221078696") && dois.contains(&"10.1093/esj/aakag043"));
        assert_eq!(dois.len(), dois.iter().collect::<HashSet<_>>().len(), "deduplicated by DOI");
    }
    #[tokio::test]
    async fn blocked_terminals_remain_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(ROOT, CAT.as_bytes(), "text/html");
        let src: SourceEntry = serde_json::from_value(json!({"id":"eso","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let rows = Eso.discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.extra["discovery_error"].as_str().is_some_and(|e| e.contains("open-access crosswalk"))));
    }
    #[tokio::test]
    async fn directory_doi_reaches_open_access_jats() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
        f.seed_cache_for_tests(ROOT, CAT.as_bytes(), "text/html");
        let fx = |n: &str| std::fs::read(format!("{}/tests/fixtures/guidelines/pmc_crosswalk/{n}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let doi = "10.1177/23969873221078696";
        f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Doi(doi.into())), &fx("idconv-eso.json"), "application/json");
        f.seed_cache_for_tests(&cw::oai_url("PMC9134774").unwrap(), &fx("oai-PMC9134774.xml"), "application/xml");
        let src: SourceEntry = serde_json::from_value(json!({"id":"eso","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let rows = Eso.discover(&src, &f, None).await.unwrap();
        let open: Vec<&PlanRow> = rows.iter().filter(|r| cw::is_crosswalk_row(r)).collect();
        assert_eq!(open.len(), 1);
        assert!(rows.iter().filter(|r| !cw::is_crosswalk_row(r)).all(|r| r.extra["discovery_error"].is_string()));
        let d = Eso.extract(&src, &f, open[0]).await.unwrap();
        assert_eq!(d.doi.as_deref(), Some(doi));
        assert_eq!(d.item_type.as_deref(), Some("guideline"));
        assert!(d.relations.iter().any(|r| r.rel == "related" && r.target == format!("https://doi.org/{doi}") && r.evidence_url.as_deref() == Some(ROOT)));
        assert_eq!(d.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Recommendation { .. })).count(), 13);
    }
}
