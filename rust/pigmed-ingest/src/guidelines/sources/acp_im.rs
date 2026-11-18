//! ACP independently evidenced editorial summary, tied to its catalog-listed publisher DOI.
use super::{common, Adapter, PlanRow};
use super::{pmc_crosswalk as cw, r2a_crosswalk as r2a};
use crate::guidelines::doc::{Block, BlockKind, GuidelineDoc, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::json;
use serde_json::Value;
use url::Url;
pub struct AcpIm;
const SUMMARY:&str="https://www.acponline.org/acp-newsroom/american-college-of-physicians-recommends-cognitive-behavioral-therapy-or-second-generation";
const DOI: &str = "https://www.acpjournals.org/doi/10.7326/M22-2056";
/// DOI printed in a catalog publisher link (`/doi/<doi>` or `/doi/full/<doi>`), query dropped.
/// This is the crosswalk key (DOI -> PMCID); it is read from the source link, never derived.
fn doi_of(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    if !u.host_str()?.ends_with("acpjournals.org") {
        return None;
    }
    let re = regex::Regex::new(r"^/doi/(?:full/|abs/|pdf/|epdf/)?(10\.\d{4,9}/[^/]+)$").ok()?;
    re.captures(u.path()).map(|c| c[1].to_string())
}
fn catalog_index(html: &str, base: &Url) -> Result<(String, Vec<Block>, Vec<Value>, Value)> {
    let d = Html::parse_document(html);
    let root = d
        .select(&sel("#accordion-inactive"))
        .next()
        .context("ACP topic accordion missing")?;
    let mut current = String::new();
    let mut topics = vec![];
    let mut records = vec![];
    for node in root.children().filter_map(ElementRef::wrap) {
        match node.value().name() {
            "h3" => current = text_of(&node),
            "div" if !current.is_empty() => {
                let mut section = String::new();
                let mut subsection = String::new();
                let mut count = 0;
                for e in node.select(&sel("h4,h5,p")) {
                    match e.value().name() {
                        "h4" => {
                            section = text_of(&e);
                            subsection.clear();
                        }
                        "h5" => subsection = text_of(&e),
                        "p" => {
                            let record = text_of(&e);
                            for a in e.select(&sel("a[href]")) {
                                let Some(url) =
                                    common::link(base, a.value().attr("href").unwrap_or_default())
                                else {
                                    continue;
                                };
                                records.push(json!({"url":url,"doi":doi_of(&url),"topic":current,"section":section,"subsection":subsection,"source_record":record,"link_label":text_of(&a),"role":"source_linked_publication","status":"unresolved; publisher access not established","rights":"unknown"}));
                                count += 1;
                            }
                        }
                        _ => {}
                    }
                }
                topics.push(json!({"title":current,"source_topic_html":node.html(),"publication_links":count}));
            }
            _ => {}
        }
    }
    if topics.is_empty() || records.is_empty() {
        bail!("ACP topic inventory missing or empty")
    }
    let raw = root.html();
    let hash = crate::common::sha256_hex(raw.as_bytes());
    let (blocks, _) = blocks_from_html(&raw, Some(base), Some("#accordion-inactive"));
    Ok((
        hash,
        blocks,
        records,
        json!({"topics":topics,"inventory_scope":"source accordion only; linked publisher terminals not acquired"}),
    ))
}
fn summary(html: &str, base: &Url) -> Result<(String, String, Vec<Block>)> {
    let d = Html::parse_document(html);
    let title = d
        .select(&sel("#block-pagetitle h1"))
        .next()
        .map(|e| text_of(&e))
        .context("ACP summary title missing")?;
    let main = d
        .select(&sel("#block-mainpagecontent"))
        .next()
        .context("ACP summary body missing")?;
    if !main
        .select(&sel("a[href]"))
        .filter_map(|a| common::link(base, a.value().attr("href").unwrap_or_default()))
        .any(|u| u == DOI)
    {
        bail!("ACP summary DOI association missing")
    }
    let list = main
        .select(&sel("ul"))
        .find(|e| {
            text_of(e).contains("conditional recommendation")
                && text_of(e).contains("strong recommendation")
        })
        .context("ACP summary recommendation list missing")?;
    let raw = main.html();
    let hash = crate::common::sha256_hex(raw.as_bytes());
    let (mut blocks, _) = blocks_from_html(&raw, Some(base), Some("#block-mainpagecontent"));
    for (i, li) in list
        .children()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "li")
        .enumerate()
    {
        blocks.push(Block {
            block_id: format!("acp-summary-group-{i}"),
            kind: BlockKind::Paragraph,
            text: text_of(&li),
            heading_path: vec![title.clone(), "Source summary recommendation group".into()],
            page: None,
        });
    }
    Ok((title, hash, blocks))
}
/// The catalog's own label for a publication: its section heading ("Current Guidelines", "Practice Points", ...),
/// an "(Update Alert ...)" prefix the record prints, or its "Evidence Review:" sub-heading; never inferred from
/// the article.
fn catalog_item_type(x: &Value) -> &'static str {
    let sub = x["subsection"].as_str().unwrap_or("").to_ascii_lowercase();
    let section = x["section"].as_str().unwrap_or("").to_ascii_lowercase();
    let record = x["source_record"].as_str().unwrap_or("").trim_start().to_ascii_lowercase();
    if sub.starts_with("evidence review") {
        "evidence_review"
    } else if record.starts_with("(update alert") {
        "guideline_update_alert"
    } else if section == "current guidelines" || sub.starts_with("guideline") {
        "guideline"
    } else if section == "practice points" {
        "practice_points"
    } else if section == "best practice advice" {
        "best_practice_advice"
    } else if section == "guidance statements" {
        "guidance_statement"
    } else if section == "visual clinical guideline" {
        "visual_clinical_guideline"
    } else {
        "catalog_listed_publication"
    }
}
/// One plan row per catalog "Full text" publication, keyed by its printed DOI, for the DOI->PMC
/// crosswalk (opt-in via `catalog_doi_rows`). Patient summaries and rows already planned are skipped.
fn catalog_publication_rows(src: &SourceEntry, records: &[Value], planned: &[PlanRow]) -> Vec<PlanRow> {
    let mut seen: Vec<String> = planned
        .iter()
        .filter_map(|r| doi_of(&r.url))
        .map(|d| d.to_ascii_lowercase())
        .collect();
    let mut out = vec![];
    for r in records {
        let (Some(doi), Some(label)) = (r["doi"].as_str(), r["link_label"].as_str()) else {
            continue;
        };
        if !label.trim().eq_ignore_ascii_case("full text") || seen.contains(&doi.to_ascii_lowercase()) {
            continue;
        }
        seen.push(doi.to_ascii_lowercase());
        out.push(PlanRow {
            source_id: src.id.clone(),
            doc_key: format!("annals-doi-{}", common::slug(doi)),
            url: format!("https://www.acpjournals.org/doi/{doi}"),
            title: None,
            rendition: None,
            version: None,
            extra: json!({"kind":"catalog_publication","doi":doi,"source_link_url":r["url"],"topic":r["topic"],
                "section":r["section"],"source_record":r["source_record"],"source_evidence_url":src.catalog_url,
                "access":"publisher acpjournals.org returns HTTP 403 to automated requests; resolve via DOI->PMC crosswalk"}),
        });
    }
    out
}
#[async_trait::async_trait]
impl Adapter for AcpIm {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let cat = f
            .get_ok(src.catalog_url.as_deref().context("ACP catalog missing")?)
            .await?;
        let base = Url::parse(&cat.record.final_url)?;
        let (hash, _, records, _) = catalog_index(&cat.text(), &base)?;
        let annals_on = src.config["annals_fulltext"].as_bool() == Some(true);
        if src.config["source_linked_only"].as_bool() == Some(true) {
            return if annals_on { annals::discover(src, f, &records, limit).await } else { Ok(vec![]) };
        }
        // Reachable ACP records first (index, newsroom summary); the Annals publisher rows follow,
        // so a bounded run samples the records ACP itself serves before the 403-gated publisher.
        let mut rows = vec![];
        rows.push(PlanRow {
            source_id: src.id.clone(),
            doc_key: format!("catalog-index-{}", &hash[..12]),
            url: cat.record.url.clone(),
            title: Some("ACP Clinical Guidelines and Recommendations index".into()),
            rendition: Some("html".into()),
            version: None,
            extra: json!({"kind":"source_index","clinical_sha256":hash}),
        });
        if limit.is_some_and(|n| rows.len() >= n) {
            return Ok(rows);
        }
        if records.iter().any(|r| r["url"] == DOI) {
            match f.get_ok(SUMMARY).await {
                Ok(p) => {
                    let b = Url::parse(&p.record.final_url)?;
                    if b.origin() != base.origin() {
                        bail!("ACP summary origin changed")
                    }
                    match summary(&p.text(), &b) {
                        Ok((title, hash, _)) => rows.push(PlanRow {
                            source_id: src.id.clone(),
                            doc_key: format!("depression-summary-{}", &hash[..12]),
                            url: SUMMARY.into(),
                            title: Some(title),
                            rendition: Some("html".into()),
                            version: None,
                            extra: json!({"clinical_sha256":hash,"catalog_fetch":cat.record}),
                        }),
                        Err(e) => rows.push(PlanRow {
                            source_id: src.id.clone(),
                            doc_key: "depression-summary-unresolved".into(),
                            url: SUMMARY.into(),
                            title: None,
                            rendition: None,
                            version: None,
                            extra: json!({"discovery_error":format!("{e:#}")}),
                        }),
                    }
                }
                Err(e) => rows.push(PlanRow {
                    source_id: src.id.clone(),
                    doc_key: "depression-summary-unresolved".into(),
                    url: SUMMARY.into(),
                    title: None,
                    rendition: None,
                    version: None,
                    extra: json!({"discovery_error":format!("{e:#}")}),
                }),
            }
        }
        let remaining = |rows: &Vec<PlanRow>| limit.map(|n| n.saturating_sub(rows.len()));
        if annals_on && remaining(&rows) != Some(0) {
            let annals_rows = annals::discover(src, f, &records, remaining(&rows)).await?;
            rows.extend(annals_rows);
        }
        if src.config["catalog_doi_rows"].as_bool() == Some(true) {
            // R2a-crosswalk: each catalog DOI goes through the PMC crosswalk. An open (or text-mining author
            // manuscript) copy replaces the publisher row; otherwise the row stays and is refused with the
            // crosswalk's reason. The index document relates to every PMC record (open or relation-only).
            let index_key = rows[0].doc_key.clone();
            let catalog_url = cat.record.url.clone();
            let mut seen = std::collections::HashSet::new();
            let mut index = r2a::Outcome::default();
            for mut row in catalog_publication_rows(src, &records, &rows) {
                if remaining(&rows) == Some(0) {
                    break;
                }
                let doi = row.extra["doi"].as_str().map(String::from);
                let x = &row.extra;
                let sref = cw::SourceRef {
                    source_url: x["source_link_url"].as_str().unwrap_or(&row.url).to_string(),
                    evidence_url: Some(catalog_url.clone()),
                    source_title: None,
                    item_type: Some(catalog_item_type(x).into()),
                    key_prefix: r2a::key_prefix(src),
                    id_route: "source_doi".into(),
                    notes: vec![format!(
                        "Listed in the ACP clinical guidelines catalog ({} / {}{}): {}",
                        x["topic"].as_str().unwrap_or("?"),
                        x["section"].as_str().unwrap_or(""),
                        x["subsection"].as_str().filter(|s| !s.is_empty()).map(|s| format!(" / {s}")).unwrap_or_default(),
                        x["source_record"].as_str().unwrap_or("")
                    )],
                };
                let site = r2a::SiteRef { doc_key: Some(index_key.clone()), url: catalog_url.clone() };
                let o = r2a::adopt(src, f, &r2a::single(doi.as_deref(), None), &sref, &site, &row.extra.clone(), &mut seen).await;
                match o.rows.first() {
                    Some(open) => rows.push(open.clone()),
                    None => {
                        row.extra[r2a::CLOSED] = json!(o.closed);
                        rows.push(row);
                    }
                }
                index.merge(r2a::Outcome { rows: vec![], open: o.open, closed: o.closed });
            }
            index.record(&mut rows[0].extra);
        }
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(e) = row.extra["discovery_error"].as_str() {
            bail!("ACP discovery unresolved: {e}")
        }
        if row.extra["kind"] == "annals_primary" {
            return annals::extract(src, f, row).await;
        }
        if cw::is_crosswalk_row(row) {
            return r2a::extract(src, f, row).await;
        }
        if row.extra["kind"] == "catalog_publication" {
            // Never fetched from the publisher: acpjournals.org answers 403 to automated requests. The DOI->PMC
            // crosswalk found no open or text-mining copy at planning time (reasons below).
            let why = row.extra[r2a::CLOSED]
                .as_array()
                .map(|a| a.iter().map(|c| c["reason"].as_str().unwrap_or("?").to_string()).collect::<Vec<_>>().join("; "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "not planned through the crosswalk (re-plan)".into());
            bail!(
                "ACP catalog publication doi:{} not acquired: publisher route blocked (HTTP 403); PMC crosswalk: {why}",
                row.extra["doi"].as_str().unwrap_or("?")
            )
        }
        let p = f.get_ok(&row.url).await?;
        let b = Url::parse(&p.record.final_url)?;
        if b.origin() != Url::parse(&row.url)?.origin() {
            bail!("ACP source origin changed")
        }
        if row.extra["kind"] == "source_index" {
            let (hash, blocks, attachments, metadata) = catalog_index(&p.text(), &b)?;
            if row.extra["clinical_sha256"]
                .as_str()
                .is_some_and(|s| s != hash)
            {
                bail!("ACP source index changed since discovery")
            }
            let mut d=GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some("guideline_catalog_index".into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),..Default::default()},search_metadata:metadata,attachments,blocks,fetches:vec![p.record],warnings:vec!["Source accordion is an inventory, not clinical full text. Each linked guideline, update, evidence review and patient material is an independent unresolved publisher terminal; known publisher blocks remain in force.".into()],..Default::default()};r2a::annotate(&mut d,src,&row.extra);return Ok(d);
        }
        let (title, hash, blocks) = summary(&p.text(), &b)?;
        if row.extra["clinical_sha256"]
            .as_str()
            .is_some_and(|s| s != hash)
        {
            bail!("ACP summary changed since discovery")
        }
        let mut fetches = vec![];
        if let Ok(r) = serde_json::from_value::<crate::guidelines::doc::FetchRecord>(
            row.extra["catalog_fetch"].clone(),
        ) {
            fetches.push(r)
        }
        fetches.push(p.record);
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),title,url:row.url.clone(),item_type:Some("editorial_guideline_summary".into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),note:Some("Editorial summary and publisher guideline have independent rights.".into()),..Default::default()},attachments:vec![json!({"url":DOI,"role":"publisher_guideline","rights":"unknown; source access may be blocked"})],blocks,fetches,warnings:vec!["Source newsroom summary was independently evidenced and matched to the catalog via its explicit publisher DOI; this is not the complete guideline or evidence review. Nested treatment choices and source-labelled evidence strength are retained as grouped text.".into()],..Default::default()})
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    #[test]
    fn real_summary_preserves_conditional_child_choices_and_source_strength() {
        let text = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/acp_im/depression-summary.html"
        ));
        let base = Url::parse(SUMMARY).unwrap();
        let (_, _, blocks) = summary(text, &base).unwrap();
        assert!(blocks
            .iter()
            .any(|b| b.block_id.starts_with("acp-summary-group-")
                && b.text.contains("did not respond")
                && b.text.contains("switching to or augmenting with CBT")
                && b.text.contains("low-certainty evidence")));
        assert!(blocks
            .iter()
            .any(|b| b.text.contains("strong recommendation")
                && b.text.contains("moderate-certainty")));
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod catalog_inventory_tests {
    use super::*;
    #[test]
    fn source_accordion_keeps_distinct_update_evidence_and_patient_publications() {
        let html = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/acp_im/catalog.html"
        ));
        let b = Url::parse(
            "https://www.acponline.org/clinical-information/clinical-guidelines-recommendations",
        )
        .unwrap();
        let (_, _, records, m) = catalog_index(html, &b).unwrap();
        assert_eq!(m["topics"].as_array().unwrap().len(), 11);
        assert!(records.iter().any(|r| r["topic"] == "Depression"
            && r["url"] == DOI
            && r["source_record"].as_str().unwrap().contains("2023")));
        assert!(records.iter().any(|r| r["topic"] == "Depression"
            && r["source_record"]
                .as_str()
                .unwrap()
                .contains("Update Alert 4")
            && r["url"].as_str().unwrap().contains("ANNALS-26-00548")));
        assert!(records.iter().any(|r| r["topic"] == "Depression"
            && r["section"].as_str().unwrap().contains("Patient Materials")));
    }
    // Catalog links carry the printed DOI (query dropped, /doi/full/ accepted). NCBI's ID Converter
    // (response saved 2026-11-17) maps 13 of the 66 to PMC; none is in the PMC OA subset.
    #[tokio::test]
    async fn catalog_doi_rows_key_full_text_publications_for_the_crosswalk() {
        let cat = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/acp_im/catalog.html"));
        let idconv: Value = serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/acp_im/ncbi-idconv-catalog-dois.json"))).unwrap();
        let u = "https://www.acponline.org/clinical-information/clinical-guidelines-recommendations";
        let (_, _, records, _) = catalog_index(cat, &Url::parse(u).unwrap()).unwrap();
        assert_eq!(records.len(), 66);
        assert!(records.iter().all(|r| r["doi"].as_str().is_some_and(|d| d.starts_with("10.7326/"))));
        assert!(records.iter().any(|r| r["url"].as_str().unwrap().contains("?_ga=") && r["doi"] == "10.7326/M20-5831"));
        assert!(records.iter().any(|r| r["url"].as_str().unwrap().contains("/doi/full/") && r["doi"] == "10.7326/ANNALS-24-03547"));
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 5 << 20, "").unwrap();
        f.seed_cache_for_tests(u, cat.as_bytes(), "text/html");
        f.seed_cache_for_tests(SUMMARY, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/acp_im/depression-summary.html")).as_bytes(), "text/html");
        let src: SourceEntry = serde_json::from_value(json!({"id":"acp_im","access":"public","enabled":true,"catalog_url":u,"config":{"catalog_doi_rows":true}})).unwrap();
        let rows = AcpIm.discover(&src, &f, None).await.unwrap();
        let pubs = rows.iter().filter(|r| r.extra["kind"] == "catalog_publication").collect::<Vec<_>>();
        // 66 links less 7 patient summaries; no DOI repeats.
        assert_eq!(pubs.len(), 59);
        assert!(pubs.iter().all(|r| r.url == format!("https://www.acpjournals.org/doi/{}", r.extra["doi"].as_str().unwrap())));
        let in_pmc = pubs.iter().filter(|r| idconv["records"].as_array().unwrap().iter().any(|x| x["pmcid"].is_string() && x["doi"].as_str().unwrap().eq_ignore_ascii_case(r.extra["doi"].as_str().unwrap()))).count();
        assert_eq!(in_pmc, 13);
        let err = AcpIm.extract(&src, &f, pubs[0]).await.unwrap_err().to_string();
        assert!(err.contains("crosswalk") && err.contains("403"));
        assert_eq!(AcpIm.discover(&src, &f, Some(3)).await.unwrap().len(), 3);
    }
    #[tokio::test]
    async fn discovery_limits_count_catalog_and_summary() {
        let cat = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/acp_im/catalog.html"
        ));
        let s = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/acp_im/depression-summary.html"
        ));
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 5 << 20, "").unwrap();
        let u =
            "https://www.acponline.org/clinical-information/clinical-guidelines-recommendations";
        f.seed_cache_for_tests(u, cat.as_bytes(), "text/html");
        f.seed_cache_for_tests(SUMMARY, s.as_bytes(), "text/html");
        let src: SourceEntry = serde_json::from_value(
            json!({"id":"acp_im","access":"public","enabled":true,"catalog_url":u}),
        )
        .unwrap();
        for (n, want) in [(0, 0), (1, 1), (2, 2), (3, 2)] {
            assert_eq!(AcpIm.discover(&src, &f, Some(n)).await.unwrap().len(), want);
        }
    }
}
// Annals primary publications are separate from the ACP inventory and newsroom summary.
mod annals {
    use super::*;
    use crate::guidelines::doc::Relation;
    use std::collections::{HashMap, HashSet};
    const BREAST: &str = "https://www.acpjournals.org/doi/10.7326/ANNALS-25-05116";
    const CATALOG: &str =
        "https://www.acponline.org/clinical-information/clinical-guidelines-recommendations";
    fn role(url: &str) -> Option<&'static str> {
        match url {
            DOI => Some("living_clinical_guideline"),
            BREAST => Some("guidance_statement"),
            _ => None,
        }
    }
    fn allowed(url: &str) -> bool {
        role(url).is_some()
            && Url::parse(url).ok().is_some_and(|u| {
                u.scheme() == "https"
                    && u.host_str() == Some("www.acpjournals.org")
                    && u.query().is_none()
                    && u.fragment().is_none()
                    && u.username().is_empty()
                    && u.password().is_none()
            })
    }
    fn norm(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    fn decoded(e: ElementRef<'_>) -> String {
        e.text().collect()
    }
    fn key(e: ElementRef<'_>) -> String {
        format!("{:?}", e.id())
    }
    fn escaped(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    fn is_class(e: ElementRef<'_>, name: &str) -> bool {
        e.value().classes().any(|c| c == name)
    }
    fn excluded(e: ElementRef<'_>) -> bool {
        std::iter::once(e)
            .chain(e.ancestors().filter_map(ElementRef::wrap))
            .any(|a| {
                a.value().id() == Some("comments-section")
                    || a.value().attr("role") == Some("dialog")
                    || is_class(a, "modal")
                    || is_class(a, "core-table-tools")
            })
    }
    pub async fn discover(
        src: &SourceEntry,
        f: &Fetcher,
        records: &[Value],
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if src.catalog_url.as_deref() != Some(CATALOG) {
            bail!("Annals branch requires the observed official ACP catalog");
        }
        let mut candidates = vec![DOI.to_string(), BREAST.to_string()];
        if let Some(seeds) = src.config["source_linked_documents"].as_array() {
            if seeds.is_empty() || seeds.len() > 2 {
                bail!("Annals source-linked seed bound is 1..=2 verified publications");
            }
            let mut seen = HashSet::new();
            for seed in seeds {
                let url = seed["url"].as_str().context("Annals seed URL missing")?;
                if !allowed(url)
                    || seed["source_evidence_url"].as_str() != Some(CATALOG)
                    || !seen.insert(url)
                {
                    bail!("Annals seed is not a unique independently observed publisher leaf with official evidence");
                }
            }
            if src.config["source_linked_only"].as_bool() == Some(true) {
                candidates = seeds
                    .iter()
                    .map(|v| v["url"].as_str().unwrap().to_string())
                    .collect();
            }
        }
        let mut out = vec![];
        for url in candidates {
            if !records.iter().any(|r| r["url"] == url) {
                continue;
            }
            let result=async {let p=f.get_ok(&url).await?;let final_url=Url::parse(&p.record.final_url)?;
                if final_url.as_str()!=url||!allowed(final_url.as_str()){bail!("Annals publisher left its exact source-observed URL");}
                let doc=parse(&p.text(),&final_url)?;
                let fingerprint=doc.search_metadata["source_snapshot_sha256"].as_str().context("Annals primary snapshot missing")?.to_string();
                Ok::<_,anyhow::Error>(PlanRow{source_id:src.id.clone(),doc_key:format!("annals-{}-{}",common::slug(doc.doi.as_deref().unwrap()),&fingerprint[..12]),url:url.clone(),title:Some(doc.title),rendition:Some("html".into()),version:doc.version,extra:json!({"kind":"annals_primary","doi":doi_of(&url),"source_snapshot_sha256":fingerprint,"clinical_sha256":doc.search_metadata["clinical_sha256"],"source_evidence_url":CATALOG,"source_item_role":role(&url),"catalog_incomplete":true,"coverage":"Two observed publisher templates only; independent catalog/evidence-review/patient/update/correction coverage remains unresolved"})})
            }.await;
            out.push(match result {Ok(row)=>row,Err(e)=>PlanRow{source_id:src.id.clone(),doc_key:format!("annals-{}-unresolved",common::short_hash(&url)),url:url.clone(),title:None,rendition:None,version:None,extra:json!({"kind":"annals_primary","doi":doi_of(&url),"source_item_role":role(&url),"source_evidence_url":CATALOG,"discovery_error":format!("Primary Annals source unresolved: {e:#}; no alternate route attempted")})}});
            if limit.is_some_and(|n| out.len() >= n) {
                break;
            }
        }
        Ok(out)
    }
    pub async fn extract(src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if !allowed(&row.url)
            || row.extra["source_evidence_url"] != CATALOG
            || src.catalog_url.as_deref() != Some(CATALOG)
        {
            bail!("Annals plan source evidence invalid");
        }
        let cat = f.get_ok(CATALOG).await?;
        let (_, _, records, _) = catalog_index(&cat.text(), &Url::parse(&cat.record.final_url)?)?;
        if !records.iter().any(|r| r["url"] == row.url) {
            bail!("ACP catalog no longer emits the planned Annals primary publication");
        }
        let p = f.get_ok(&row.url).await?;
        let base = Url::parse(&p.record.final_url)?;
        if base.as_str() != row.url {
            bail!("Annals primary publisher URL changed");
        }
        let mut doc = parse(&p.text(), &base)?;
        if row.extra["clinical_sha256"] != doc.search_metadata["clinical_sha256"]
            || row.extra["source_snapshot_sha256"] != doc.search_metadata["source_snapshot_sha256"]
        {
            bail!("Annals clinical source changed; re-plan");
        }
        doc.source_id = src.id.clone();
        doc.doc_key = row.doc_key.clone();
        doc.fetches = vec![cat.record, p.record];
        doc.search_metadata["catalog_evidence"] = json!({"url":CATALOG,"source_link_url":row.url,"source_item_role":row.extra["source_item_role"],"catalog_record":records.iter().find(|r|r["url"]==row.url),"catalog_incomplete":true});
        Ok(doc)
    }
    struct Group<'a> {
        label: String,
        paragraphs: Vec<ElementRef<'a>>,
    }
    fn labeled(raw: &str, guidance: bool) -> Option<String> {
        let pattern = if guidance {
            r"(?i)^Guidance\s+Statement\s+([1-5])\s*[:.]?"
        } else {
            r"(?i)^Recommendation\s+(1a|1b|2|3)\s*[:.]?"
        };
        regex::Regex::new(pattern)
            .unwrap()
            .captures(raw.trim_start())
            .map(|c| c[1].to_ascii_lowercase())
    }
    fn groups<'a>(
        root: ElementRef<'a>,
        guidance: bool,
        abstract_occurrence: bool,
    ) -> Result<Vec<Group<'a>>> {
        let mut out = vec![];
        if abstract_occurrence {
            for section in root.select(&sel("section[id^='abs-sec-']")) {
                let heading = section
                    .children()
                    .filter_map(ElementRef::wrap)
                    .find(|e| e.value().name() == "h3");
                let Some(label) = heading.and_then(|h| labeled(&text_of(&h), guidance)) else {
                    continue;
                };
                let paragraphs = section
                    .children()
                    .filter_map(ElementRef::wrap)
                    .filter(|e| e.value().attr("role") == Some("paragraph"))
                    .collect::<Vec<_>>();
                if paragraphs.is_empty() {
                    bail!("Annals abstract labeled section has no source paragraph DIVs");
                }
                out.push(Group { label, paragraphs });
            }
        } else {
            let sections = if guidance {
                vec![root]
            } else {
                root.select(&sel("section#sec-6-1,section#sec-6-2"))
                    .collect::<Vec<_>>()
            };
            if !guidance && sections.len() != 2 {
                bail!("Annals depression detailed recommendation sections absent or changed");
            }
            // Guidance inline labels may occur under body sections; only native paragraph DIVs
            // are candidates. Rationale nodes never start formal groups without their own label.
            let candidates = if guidance {
                let section = root
                    .select(&sel("section#sec-5"))
                    .next()
                    .context("Annals guidance source section sec-5 missing")?;
                section
                    .children()
                    .filter_map(ElementRef::wrap)
                    .filter(|e| e.value().attr("role") == Some("paragraph") && !excluded(*e))
                    .collect::<Vec<_>>()
            } else {
                sections
                    .iter()
                    .flat_map(|s| {
                        s.children()
                            .filter_map(ElementRef::wrap)
                            .collect::<Vec<_>>()
                    })
                    .collect()
            };
            if guidance {
                for node in candidates {
                    if let Some(label) = labeled(&text_of(&node), true) {
                        out.push(Group {
                            label,
                            paragraphs: vec![node],
                        });
                    }
                }
            } else {
                let mut current: Option<Group<'a>> = None;
                for node in candidates {
                    if node.value().attr("role") != Some("paragraph") {
                        if let Some(group) = current.take() {
                            out.push(group);
                        }
                        continue;
                    }
                    if excluded(node) {
                        continue;
                    }
                    if let Some(label) = labeled(&text_of(&node), guidance) {
                        if let Some(group) = current.take() {
                            out.push(group);
                        }
                        current = Some(Group {
                            label,
                            paragraphs: vec![node],
                        });
                    } else if let Some(group) = current.as_mut() {
                        group.paragraphs.push(node);
                    }
                }
                if let Some(group) = current {
                    out.push(group);
                }
            }
        }
        let wanted = if guidance {
            vec!["1", "2", "3", "4", "5"]
        } else {
            vec!["1a", "1b", "2", "3"]
        };
        if out.iter().map(|g| g.label.as_str()).collect::<Vec<_>>() != wanted {
            bail!("Annals source formal label order/count differs from observed contract");
        }
        Ok(out)
    }
    fn grades(raw: &str) -> Value {
        let strength =
            regex::Regex::new(r"(?i)\b(?:strong|conditional|weak) recommendation\b").unwrap();
        let certainty = regex::Regex::new(
            r"(?i)\b(?:very[ -]low|low|moderate|high)[ -]certainty(?: evidence| of evidence)?\b",
        )
        .unwrap();
        json!({"strength":strength.find_iter(raw).map(|m|json!({"printed":m.as_str(),"text_byte_start":m.start(),"text_byte_end":m.end()})).collect::<Vec<_>>(),"certainty":certainty.find_iter(raw).map(|m|json!({"printed":m.as_str(),"text_byte_start":m.start(),"text_byte_end":m.end()})).collect::<Vec<_>>()})
    }
    fn component(e: ElementRef<'_>) -> Value {
        use scraper::Node;
        let raw = decoded(e);
        let mut position = 0;
        let mut runs = vec![];
        for n in e.descendants() {
            if let Node::Text(t) = n.value() {
                let text = t.to_string();
                let start = position;
                position += text.len();
                let ancestry=n.ancestors().take_while(|a|a.id()!=e.id()).filter_map(ElementRef::wrap).map(|a|json!({"tag":a.value().name(),"id":a.value().id(),"role":a.value().attr("role")})).collect::<Vec<_>>();
                runs.push(json!({"text":text,"decoded_text_byte_start":start,"decoded_text_byte_end":position,"source_inline_ancestors":ancestry}));
            }
        }
        json!({"source_id":e.value().id(),"decoded_source_text":raw,"source_grade_spans":grades(&raw),"source_html":e.html(),"runs":runs})
    }
    fn linked(e: ElementRef<'_>, base: &Url) -> Vec<Value> {
        e.select(&sel("a[href]"))
            .filter_map(|a| {
                let href = a.value().attr("href")?;
                let url = base.join(href).ok()?;
                if !matches!(url.scheme(), "https" | "http")
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return None;
                }
                Some(json!({"href":href,"url":url.as_str(),"label":text_of(&a)}))
            })
            .collect()
    }
    struct Render {
        replace: HashMap<String, (String, String)>,
        skip: HashSet<String>,
        ids: HashMap<String, String>,
        used: HashSet<String>,
    }
    impl Render {
        fn write(&mut self, e: ElementRef<'_>, out: &mut String) {
            if excluded(e)
                || self.skip.contains(&key(e))
                || matches!(
                    e.value().name(),
                    "script" | "style" | "noscript" | "nav" | "button" | "form" | "template"
                )
            {
                return;
            }
            if let Some((id, text)) = self.replace.get(&key(e)) {
                out.push_str(&format!("<p id=\"{}\">{}</p>", escaped(id), escaped(text)));
                return;
            }
            // Display images of a real HTML table are not independent figures.
            if e.value().name() == "img"
                && e.ancestors().filter_map(ElementRef::wrap).any(|a| {
                    a.value().name() == "figure"
                        && is_class(a, "table")
                        && a.select(&sel("table")).next().is_some()
                })
            {
                return;
            }
            let mut id = e
                .value()
                .id()
                .map(String::from)
                .unwrap_or_else(|| format!("annals-node-{}", self.ids.len() + 1));
            if !self.used.insert(id.clone()) {
                id = format!("{id}-annals-node-{}", self.ids.len() + 1);
                self.used.insert(id.clone());
            }
            self.ids.insert(key(e), id.clone());
            let mut tag = e.value().name();
            if e.value().attr("role") == Some("doc-footnote") {
                tag = "aside";
            } else if tag == "figure"
                && is_class(e, "table")
                && e.select(&sel("table")).next().is_some()
            {
                tag = "div";
            } else if e.value().attr("role") == Some("paragraph")
                && e.select(&sel("p,div,ul,ol,table,figure,img"))
                    .next()
                    .is_none()
            {
                tag = "p";
            }
            out.push('<');
            out.push_str(tag);
            out.push_str(" id=\"");
            out.push_str(&escaped(&id));
            out.push('"');
            for (name, value) in e.value().attrs() {
                if name != "id" {
                    out.push(' ');
                    out.push_str(name);
                    out.push_str("=\"");
                    out.push_str(&escaped(value));
                    out.push('"');
                }
            }
            out.push('>');
            for child in e.children() {
                match child.value() {
                    scraper::Node::Text(t) => out.push_str(&escaped(t)),
                    scraper::Node::Element(_) => {
                        if let Some(c) = ElementRef::wrap(child) {
                            self.write(c, out)
                        }
                    }
                    _ => {}
                }
            }
            if !matches!(
                tag,
                "img" | "br" | "hr" | "input" | "source" | "wbr" | "meta" | "link"
            ) {
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }
    pub(super) fn parse(raw: &str, base: &Url) -> Result<GuidelineDoc> {
        if !allowed(base.as_str()) {
            bail!("Annals terminal is outside the verified source leaves");
        }
        let d = Html::parse_document(raw);
        let all = norm(&decoded(d.root_element())).to_lowercase();
        if [
            "performing security verification",
            "verify you are human",
            "access denied",
            "sign in to access this article",
            "subscribe to read",
        ]
        .iter()
        .any(|s| all.contains(s))
        {
            bail!("Annals publisher access/security gate");
        }
        let main = d
            .select(&sel("main.content.article-page"))
            .next()
            .context("Annals article root absent; no preview fallback")?;
        let roots = main.select(&sel("#bodymatter")).collect::<Vec<_>>();
        if roots.len() != 1 {
            bail!("Annals full clinical body root missing or ambiguous");
        }
        let root = roots[0];
        let meta = |name: &str| {
            d.select(&sel(&format!("meta[name='{name}']")))
                .next()
                .and_then(|e| e.value().attr("content"))
                .map(String::from)
        };
        let title = meta("citation_title")
            .or_else(|| d.select(&sel("h1")).next().map(|e| text_of(&e)))
            .context("Annals article title missing")?;
        let doi = meta("citation_doi").context("Annals printed DOI missing")?;
        if !base
            .path()
            .strip_prefix("/doi/")
            .is_some_and(|s| s.eq_ignore_ascii_case(&doi))
        {
            bail!("Annals source DOI and exact leaf differ");
        }
        let guidance = base.as_str() == BREAST;
        if (!guidance && !title.to_lowercase().contains("guideline"))
            || (guidance && !title.to_lowercase().contains("guidance statement"))
        {
            bail!("Annals printed publication type differs from expected item role");
        }
        let detailed = groups(root, guidance, false)?;
        let abs = main.select(&sel("#abstract")).next();
        let abstract_groups = if !guidance {
            Some(groups(
                abs.context("Annals depression abstract occurrence missing")?,
                false,
                true,
            )?)
        } else {
            None
        };
        if !guidance {
            let counts = detailed
                .iter()
                .map(|g| g.paragraphs.len())
                .collect::<Vec<_>>();
            if counts != [1, 2, 1, 4] {
                bail!("Annals detailed paragraph scopes differ from the observed 1a/1b/2/3 source contract");
            }
            for group in &detailed {
                if matches!(group.label.as_str(), "1b" | "3")
                    && grades(&decoded(*group.paragraphs.last().unwrap()))["strength"]
                        .as_array()
                        .is_some_and(|v| !v.is_empty())
                {
                    bail!("Annals decision-context source paragraph unexpectedly carries a formal grade");
                }
            }
        }
        let mut render = Render {
            replace: HashMap::new(),
            skip: HashSet::new(),
            ids: HashMap::new(),
            used: HashSet::new(),
        };
        let mut statements = vec![];
        let mut warnings = vec![];
        for group in &detailed {
            if group
                .paragraphs
                .iter()
                .any(|p| p.select(&sel("table,figure,img")).next().is_some())
            {
                bail!("Annals formal statement contains an unhandled native visual object");
            }
            let id = format!("annals-recommendation-{}", group.label);
            let text = group
                .paragraphs
                .iter()
                .map(|p| norm(&decoded(*p)))
                .collect::<Vec<_>>()
                .join("\n\n");
            render
                .replace
                .insert(key(group.paragraphs[0]), (id.clone(), text.clone()));
            for p in group.paragraphs.iter().skip(1) {
                render.skip.insert(key(*p));
            }
            let abstract_group = abstract_groups
                .as_ref()
                .and_then(|groups| groups.iter().find(|g| g.label == group.label));
            let abstract_components = abstract_group.map(|g| {
                g.paragraphs
                    .iter()
                    .enumerate()
                    .map(|(index, p)| {
                        let mut value = component(*p);
                        if group.label == "3" && g.paragraphs.len() == 4 {
                            value["source_component_role"] = json!(
                                [
                                    "lead_in",
                                    "graded_alternative",
                                    "graded_alternative",
                                    "decision_context"
                                ][index]
                            );
                        }
                        value
                    })
                    .collect::<Vec<_>>()
            });
            let components = group
                .paragraphs
                .iter()
                .map(|p| component(*p))
                .collect::<Vec<_>>();
            let all_grades = grades(&text);
            let strength = all_grades["strength"].as_array().unwrap();
            let certainty = all_grades["certainty"].as_array().unwrap();
            let scalar_strength = if strength.len() == 1 {
                strength[0]["printed"].as_str().map(String::from)
            } else {
                None
            };
            let scalar_certainty = if certainty.len() == 1 {
                certainty[0]["printed"].as_str().map(String::from)
            } else {
                None
            };
            if !guidance && (strength.is_empty() || certainty.is_empty()) {
                bail!("Annals formal source group lacks native strength/certainty");
            }
            if group.label == "3" && !guidance {
                if strength.len() != 2 || certainty.len() != 2 {
                    bail!("Annals recommendation 3 detailed body must retain both native graded alternatives");
                }
                let ag = abstract_group
                    .context("Annals recommendation 3 abstract association missing")?;
                if ag.paragraphs.len() != 4
                    || ag
                        .paragraphs
                        .iter()
                        .filter(|p| {
                            grades(&decoded(**p))["strength"]
                                .as_array()
                                .is_some_and(|g| !g.is_empty())
                        })
                        .count()
                        != 2
                {
                    bail!("Annals recommendation 3 source lead-in/two graded alternatives/decision context boundary changed");
                }
                warnings.push("Recommendation 3 remains one detailed-body recommendation. Abstract's four source components retain lead-in, two separately graded alternatives and decision context. Detailed source runs/paragraphs remain explicit; grade scope is never inherited or sentence-split.".into());
            }
            let abstract_text = abstract_group.map(|g| {
                norm(
                    &g.paragraphs
                        .iter()
                        .map(|p| decoded(*p))
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            });
            let source_label =
                regex::Regex::new(r"(?i)^Recommendation\s+(?:1a|1b|2|3)\s*[:.]?\s*").unwrap();
            let body_without_label = norm(&source_label.replace(&text, ""));
            let occurrence_equal = abstract_text.as_ref().map(|a| a == &body_without_label);
            if occurrence_equal == Some(false) {
                warnings.push(format!("Recommendation {} abstract/body source text differs after removing the repeated label; both occurrences are retained without inferred clinical equivalence.",group.label));
            }
            let mut source_path = group.paragraphs[0]
                .ancestors()
                .filter_map(ElementRef::wrap)
                .filter(|e| e.value().name() == "section")
                .filter_map(|section| {
                    section
                        .children()
                        .filter_map(ElementRef::wrap)
                        .find(|e| {
                            matches!(e.value().name(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
                        })
                        .map(|h| text_of(&h))
                })
                .collect::<Vec<_>>();
            source_path.reverse();
            statements.push(json!({"label":group.label,"source_heading_path":source_path,"block_id":id,"canonical_occurrence_role":"detailed_body","text":text,"strength":scalar_strength,"certainty":scalar_certainty,"components":components,"abstract_occurrence_role":"duplicate_source_occurrence_not_independent_guidance","abstract_components":abstract_components,"all_native_grade_spans":all_grades,"grade_scope":"Each native source component/run retains its own grade position. Multiple graded alternatives keep scalar strength/certainty unknown.","abstract_detailed_paragraph_count_equal":abstract_group.map(|a|a.paragraphs.len()==group.paragraphs.len()),"abstract_detailed_text_equal_after_source_label":occurrence_equal}));
        }
        let mut clinical = String::new();
        render.write(root, &mut clinical);
        let fingerprint = crate::common::sha256_hex(clinical.as_bytes());
        let (mut blocks, extract_warnings) = blocks_from_html(&clinical, Some(base), None);
        warnings.extend(extract_warnings);
        for source in &statements {
            let id = source["block_id"].as_str().unwrap();
            let block = blocks
                .iter_mut()
                .find(|b| b.block_id == id)
                .context("Annals grouped formal statement not preserved")?;
            let label = source["label"].as_str().unwrap();
            block.text = source["text"].as_str().unwrap().into();
            if let Some(path) = source["source_heading_path"]
                .as_array()
                .filter(|p| !p.is_empty())
            {
                block.heading_path = path
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect();
            }
            block.kind = BlockKind::Recommendation {
                label: Some(format!(
                    "{} {label}",
                    if guidance {
                        "Guidance Statement"
                    } else {
                        "Recommendation"
                    }
                )),
                strength: source["strength"].as_str().map(String::from),
                certainty: source["certainty"].as_str().map(String::from),
                population: None,
                grading_system: (!guidance
                    && regex::Regex::new(r"\bGRADE\b")
                        .unwrap()
                        .is_match(&decoded(main)))
                .then(|| "GRADE".into()),
                qualifier: None,
            };
        }
        let mut tables = vec![];
        for figure in root.select(&sel("figure.table")).filter(|e| !excluded(*e)) {
            let native = figure
                .select(&sel("table"))
                .next()
                .context("Annals table figure lacks native table; image/caption is not a table")?;
            let caption = figure
                .select(&sel("figcaption"))
                .next()
                .map(|c| text_of(&c));
            let block_id = render
                .ids
                .get(&key(native))
                .context("Annals native table source map missing")?;
            if let Some(b) = blocks.iter_mut().find(|b| &b.block_id == block_id) {
                if let BlockKind::Table { caption: c } = &mut b.kind {
                    *c = caption.clone();
                }
            }
            let rows=native.select(&sel("tr")).map(|r|{let cells=common::cells(r).into_iter().map(|c|json!({"id":c.value().id(),"tag":c.value().name(),"text":decoded(c),"rowspan":c.value().attr("rowspan"),"colspan":c.value().attr("colspan"),"headers":c.value().attr("headers"),"scope":c.value().attr("scope"),"source_html":c.html(),"links":linked(c,base)})).collect::<Vec<_>>();json!({"id":r.value().id(),"section":r.ancestors().filter_map(ElementRef::wrap).find(|a|matches!(a.value().name(),"thead"|"tbody"|"tfoot")).map(|a|a.value().name()),"cells":cells})}).collect::<Vec<_>>();
            let notes=figure.select(&sel(".notes [role='doc-footnote']")).map(|n|json!({"source_id":n.value().id(),"block_id":render.ids.get(&key(n)),"text":decoded(n),"source_html":n.html(),"links":linked(n,base)})).collect::<Vec<_>>();
            tables.push(json!({"source_figure_id":figure.value().id(),"block_id":block_id,"caption":caption,"source_table_html":native.html(),"rows":rows,"external_table_footnotes":notes}));
        }
        let figures=root.select(&sel("figure:not(.table)")).filter(|e|!excluded(*e)).map(|f|{let caption=f.select(&sel("figcaption")).next().map(|c|text_of(&c));let id=f.value().id();let continued=id.is_some_and(|id|matches!(id,"f2b-M222056"|"f2c-M222056"))&&caption.as_deref().is_some_and(|s|s.to_lowercase().contains("continued"));json!({"source_figure_id":id,"caption":caption,"continuation_of":continued.then_some("f2-M222056"),"source_html":f.html(),"images":f.select(&sel("img")).map(|i|json!({"block_id":render.ids.get(&key(i)),"source_id":i.value().id(),"src":i.value().attr("src")})).collect::<Vec<_>>()})}).collect::<Vec<_>>();
        let mut dates = vec![];
        for name in [
            "citation_publication_date",
            "citation_online_date",
            "citation_date",
            "citation_issue_date",
        ] {
            if let Some(raw) = meta(name) {
                let mut date = common::date(name, &raw, base.as_str());
                let iso = raw.replace('/', "-");
                if regex::Regex::new(r"^\d{4}-\d{2}(?:-\d{2})?$")
                    .unwrap()
                    .is_match(&iso)
                {
                    date.normalized = Some(iso);
                }
                dates.push(date);
            }
        }
        let mut relations = vec![];
        let mut attachments = vec![];
        let mut seen = HashSet::new();
        for a in main.select(&sel("a[href]")).filter(|e| !excluded(*e)) {
            let label = text_of(&a);
            let Some(url) = a.value().attr("href").and_then(|h| common::link(base, h)) else {
                continue;
            };
            let u = Url::parse(&url)?;
            if !seen.insert(url.clone())
                || u.host_str() != Some("www.acpjournals.org")
                || u.query().is_some()
            {
                continue;
            }
            if url == "https://www.acpjournals.org/doi/10.7326/L23-0246" {
                relations.push(Relation {
                    rel: "erratum".into(),
                    target: url,
                    scope: Some(label),
                    evidence_url: Some(base.to_string()),
                });
            } else if url == "https://www.acpjournals.org/living/treatments-adults-depressive" {
                relations.push(Relation {
                    rel: "related".into(),
                    target: url,
                    scope: Some(label),
                    evidence_url: Some(base.to_string()),
                });
            } else if u
                .path()
                .to_ascii_lowercase()
                .starts_with(&format!("/doi/suppl/{}/", doi.to_ascii_lowercase()))
            {
                attachments.push(json!({"url":url,"label":label,"role":"source_linked_supporting_asset","bytes_verified":false,"rights":"unknown; assess separately"}));
            }
        }
        let notices = main
            .select(&sel(".copyright,.license,.article-copyright"))
            .filter(|e| !excluded(*e))
            .map(|e| text_of(&e))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        warnings.push("Annals full-body/source label parser is bounded to two observed templates, not complete topic/archive/update coverage. FREE/browser access is not reuse permission. Source assets are descriptors; byte fidelity, figure visual completeness and rights remain unverified.".into());
        let snapshot = crate::common::sha256_hex(&serde_json::to_vec(
            &json!({"clinical_sha256":fingerprint,"title":title,"doi":doi,"dates":dates,"relations":relations,"attachments":attachments,"rights_notice":notices,"abstract_source_html":abs.map(|a|a.html()),"source_recommendations":statements,"source_tables":tables,"source_figure_parts":figures}),
        )?);
        Ok(GuidelineDoc {
            title,
            url: base.to_string(),
            doi: Some(doi.clone()),
            work_key: Some(common::slug(&doi)),
            item_type: Some(role(base.as_str()).unwrap().into()),
            status: "unknown".into(),
            published: dates
                .iter()
                .find(|d| {
                    matches!(
                        d.label.as_str(),
                        "citation_online_date" | "citation_publication_date"
                    )
                })
                .and_then(|d| d.normalized.clone()),
            date_observations: dates,
            relations,
            rights: Rights {
                status: "unknown".into(),
                note: Some(if notices.is_empty() {
                    "Publisher access does not establish reuse rights.".into()
                } else {
                    notices.join("\n")
                }),
                evidence_urls: vec![base.to_string()],
                ..Default::default()
            },
            blocks,
            attachments,
            search_metadata: json!({"source_item_role":role(base.as_str()),"clinical_root":"main.content.article-page #bodymatter","clinical_sha256":fingerprint,"source_snapshot_sha256":snapshot,"source_recommendations":statements,"source_abstract_html":abs.map(|a|a.html()),"source_tables":tables,"source_figure_parts":figures,"excluded_occurrences":["abstract is associated as duplicate metadata, not independent clinical recommendation rows","comments-section and UI table tools are outside original clinical body"],"coverage":"Observed primary publication templates only; source catalog roles remain independent"}),
            warnings,
            ..Default::default()
        })
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod annals_contract_tests {
    use super::*;
    const DEPRESSION: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acp_im/annals-depression-contract-synthetic.html"
    ));
    const GUIDANCE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acp_im/annals-guidance-table-contract-synthetic.html"
    ));
    const BREAST: &str = "https://www.acpjournals.org/doi/10.7326/ANNALS-25-05116";
    const CATALOG: &str =
        "https://www.acponline.org/clinical-information/clinical-guidelines-recommendations";
    #[test]
    fn four_body_recommendations_keep_graded_alternatives_and_abstract_occurrence_roles() {
        let doc = annals::parse(DEPRESSION, &Url::parse(DOI).unwrap()).unwrap();
        let recs = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recs.len(), 4);
        let labels = recs
            .iter()
            .map(|b| match &b.kind {
                BlockKind::Recommendation { label, .. } => label.as_deref().unwrap(),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "Recommendation 1a",
                "Recommendation 1b",
                "Recommendation 2",
                "Recommendation 3"
            ]
        );
        assert!(recs[1].text.contains("shared decision context for 1b"));
        let third = recs[3];
        assert!(
            third.text.contains("Invented lead-in")
                && third.text.contains("Invented option A")
                && third.text.contains("Invented option B")
                && third.text.contains("ungraded decision context")
        );
        assert!(
            matches!(&third.kind,BlockKind::Recommendation{strength:None,certainty:None,grading_system:Some(g),..}if g=="GRADE")
        );
        assert!(
            matches!(&recs[0].kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..}if s=="strong recommendation"&&c=="moderate-certainty evidence")
        );
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Paragraph)
                && b.text
                    .contains("Invented rationale with the phrase conditional recommendation")));
        assert!(!doc
            .blocks
            .iter()
            .any(|b| b.text.contains("Invented comment update")));
        let meta = &doc.search_metadata["source_recommendations"][3];
        assert_eq!(meta["canonical_occurrence_role"], "detailed_body");
        assert_eq!(meta["abstract_components"].as_array().unwrap().len(), 4);
        assert_eq!(
            meta["abstract_components"][1]["source_component_role"],
            "graded_alternative"
        );
        assert_eq!(meta["components"].as_array().unwrap().len(), 4);
        assert_eq!(
            meta["all_native_grade_spans"]["strength"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        for component in meta["components"].as_array().unwrap() {
            let raw = component["decoded_source_text"].as_str().unwrap();
            for run in component["runs"].as_array().unwrap() {
                let a = run["decoded_text_byte_start"].as_u64().unwrap() as usize;
                let b = run["decoded_text_byte_end"].as_u64().unwrap() as usize;
                assert_eq!(&raw[a..b], run["text"].as_str().unwrap());
            }
        }
        assert_eq!(
            doc.search_metadata["source_figure_parts"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            doc.search_metadata["source_figure_parts"][1]["continuation_of"],
            "f2-M222056"
        );
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Figure { .. }))
                .count(),
            4
        );
        assert!(doc
            .relations
            .iter()
            .any(|r| r.rel == "erratum" && r.target.ends_with("L23-0246")));
        assert!(doc
            .relations
            .iter()
            .any(|r| r.rel == "related" && r.target.contains("/living/")));
        assert_eq!(doc.attachments.len(), 2);
        assert_eq!(doc.published.as_deref(), Some("2023-01-24"));
        assert_eq!(doc.rights.status, "unknown");
        let rows = crate::guidelines::doc::to_rows(
            &doc,
            &Default::default(),
            false,
            "synthetic/document.json",
            &Default::default(),
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.get("block_type").and_then(Value::as_str) == Some("recommendation"))
                .count(),
            4
        );
        assert!(rows.iter().any(|r| r
            .get("recommendation")
            .and_then(|v| v.get("statement"))
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains("Invented option A") && s.contains("Invented option B"))));
    }
    #[test]
    fn native_table_geometry_and_external_footnotes_survive_without_display_image_duplicates() {
        let doc = annals::parse(GUIDANCE, &Url::parse(BREAST).unwrap()).unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("guidance_statement"));
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            5
        );
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
                .count(),
            1
        );
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Footnote))
                .count(),
            4
        );
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Figure { .. }))
                .count(),
            0
        );
        let table = &doc.search_metadata["source_tables"][0];
        let rows = table["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 13);
        assert_eq!(rows.iter().filter(|r| r["section"] == "thead").count(), 2);
        assert_eq!(rows.iter().filter(|r| r["section"] == "tbody").count(), 11);
        assert_eq!(rows[0]["cells"][0]["rowspan"], "2");
        assert_eq!(rows[0]["cells"][1]["colspan"], "5");
        assert_eq!(
            rows[1]["cells"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["links"].as_array().unwrap().len())
                .sum::<usize>(),
            6
        );
        assert_eq!(
            table["external_table_footnotes"].as_array().unwrap().len(),
            4
        );
        assert_eq!(rows[2]["cells"][1]["text"], "");
        assert!(!doc
            .blocks
            .iter()
            .any(|b| b.text.contains("DOWNLOAD TABLE TOOL TEXT")));
        assert_eq!(doc.date_observations.len(), 2);
        assert_eq!(
            doc.date_observations[1].normalized.as_deref(),
            Some("2026-06")
        );
        assert!(doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .all(|b| matches!(
                b.kind,
                BlockKind::Recommendation {
                    strength: None,
                    certainty: None,
                    ..
                }
            )));
    }
    #[test]
    fn gates_identity_changed_labels_and_ambiguous_alternatives_are_not_silently_accepted() {
        let base = Url::parse(DOI).unwrap();
        assert!(annals::parse("<h1>Performing security verification</h1>", &base).is_err());
        assert!(annals::parse(
            "<main class='content article-page'>Free preview</main>",
            &base
        )
        .is_err());
        assert!(annals::parse(
            &DEPRESSION.replace("10.7326/M22-2056", "10.7326/M22-WRONG"),
            &base
        )
        .is_err());
        assert!(annals::parse(&DEPRESSION.replace("<div role=\"paragraph\">Invented option B (conditional recommendation; low-certainty evidence).</div>",""),&base).unwrap_err().to_string().contains("boundary changed"));
        let original = annals::parse(DEPRESSION, &base).unwrap();
        let new_date = annals::parse(
            &DEPRESSION.replace("content=\"2023-01-24\"", "content=\"2023-01-25\""),
            &base,
        )
        .unwrap();
        assert_eq!(
            original.search_metadata["clinical_sha256"],
            new_date.search_metadata["clinical_sha256"]
        );
        assert_ne!(
            original.search_metadata["source_snapshot_sha256"],
            new_date.search_metadata["source_snapshot_sha256"]
        );
        let new_comment = annals::parse(
            &DEPRESSION.replace(
                "Invented comment update, outside original guideline.",
                "An edited synthetic comment outside the guideline.",
            ),
            &base,
        )
        .unwrap();
        assert_eq!(
            original.search_metadata["source_snapshot_sha256"],
            new_comment.search_metadata["source_snapshot_sha256"]
        );
        let mixed = DEPRESSION.replace(
            "Invented option B (conditional recommendation; low-certainty evidence).",
            "Invented option B (strong recommendation; high-certainty evidence).",
        );
        let doc = annals::parse(&mixed, &base).unwrap();
        let rec = &doc.search_metadata["source_recommendations"][3];
        assert!(rec["strength"].is_null() && rec["certainty"].is_null());
        assert_eq!(
            rec["components"][2]["source_grade_spans"]["certainty"][0]["printed"],
            "high-certainty evidence"
        );
    }
    #[tokio::test]
    async fn source_linked_full_articles_add_to_inventory_news_and_pin_source_revision() {
        let index = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/acp_im/catalog.html"
        ));
        let news = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/acp_im/depression-summary.html"
        ));
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 8 << 20, "").unwrap();
        for (url, html) in [
            (CATALOG, index),
            (SUMMARY, news),
            (DOI, DEPRESSION),
            (BREAST, GUIDANCE),
        ] {
            f.seed_cache_for_tests(url, html.as_bytes(), "text/html");
        }
        let mut src:SourceEntry=serde_json::from_value(json!({"id":"acp_im","enabled":true,"access":"mixed","catalog_url":CATALOG,"config":{"annals_fulltext":true,"source_linked_documents":[{"url":DOI,"source_evidence_url":CATALOG},{"url":BREAST,"source_evidence_url":CATALOG}]}})).unwrap();
        let rows = AcpIm.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].extra["kind"], "source_index");
        assert!(rows[1].doc_key.starts_with("depression-summary-"));
        assert_eq!(rows[2].extra["kind"], "annals_primary");
        assert_eq!(rows[3].extra["kind"], "annals_primary");
        assert_eq!(rows[2].extra["doi"], "10.7326/M22-2056");
        // Bounded runs sample the ACP-served records before the publisher rows.
        assert_eq!(AcpIm.discover(&src, &f, Some(3)).await.unwrap()[2].extra["kind"], "annals_primary");
        let rows = rows[2..].iter().chain(rows[..2].iter()).cloned().collect::<Vec<_>>();
        let doc = AcpIm.extract(&src, &f, &rows[0]).await.unwrap();
        assert_eq!(doc.fetches.len(), 2);
        assert_eq!(doc.item_type.as_deref(), Some("living_clinical_guideline"));
        let mut changed = rows[0].clone();
        changed.extra["clinical_sha256"] = json!("changed");
        assert!(AcpIm
            .extract(&src, &f, &changed)
            .await
            .unwrap_err()
            .to_string()
            .contains("re-plan"));
        src.config["source_linked_only"] = json!(true);
        assert_eq!(AcpIm.discover(&src, &f, None).await.unwrap().len(), 2);
        src.config["source_linked_documents"][0]["source_evidence_url"] =
            json!("https://example.org");
        assert!(AcpIm.discover(&src, &f, None).await.is_err());
        // A denied primary leaves its own unresolved result; the independent news item
        // remains an editorial summary and is never relabeled as full Annals guidance.
        f.seed_cache_for_tests(
            DOI,
            b"<h1>Performing security verification</h1>",
            "text/html",
        );
        src.config = json!({"annals_fulltext":true});
        let rows = AcpIm.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows[2].extra["discovery_error"]
            .as_str()
            .unwrap()
            .contains("no alternate route"));
        let news = AcpIm.extract(&src, &f, &rows[1]).await.unwrap();
        assert_eq!(
            news.item_type.as_deref(),
            Some("editorial_guideline_summary")
        );
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod r2a_tests {
    use super::*;
    use crate::guidelines::sources::pmc_crosswalk::{self as cw, Lookup};
    fn fx(p: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/guidelines/{p}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }
    // Real ID Converter answer for the 66 catalog DOIs (B-acp-ada, 2026-11-17) and the real PMC OAI record of the
    // HHS author manuscript PMC11614146 (refs trimmed). Other OAI answers are not seeded: offline they fail, and
    // the crosswalk must then refuse honestly instead of guessing.
    #[tokio::test]
    async fn catalog_dois_go_through_the_pmc_crosswalk() {
        let u = "https://www.acponline.org/clinical-information/clinical-guidelines-recommendations";
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 8 << 20, "").unwrap();
        f.seed_cache_for_tests(u, &fx("acp_im/catalog.html"), "text/html");
        f.seed_cache_for_tests(SUMMARY, &fx("acp_im/depression-summary.html"), "text/html");
        let idconv: Value = serde_json::from_slice(&fx("acp_im/ncbi-idconv-catalog-dois.json")).unwrap();
        for r in idconv["records"].as_array().unwrap() {
            let d = cw::normalize_doi(r["requested-id"].as_str().unwrap()).unwrap();
            f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Doi(d)), &fx("acp_im/ncbi-idconv-catalog-dois.json"), "application/json");
        }
        f.seed_cache_for_tests(&cw::oai_url("PMC11614146").unwrap(), &fx("r2a_crosswalk/oai-PMC11614146.xml"), "application/xml");
        let src: SourceEntry = serde_json::from_value(json!({"id":"acp_im","access":"public","enabled":true,"catalog_url":u,"config":{"catalog_doi_rows":true}})).unwrap();
        let rows = AcpIm.discover(&src, &f, None).await.unwrap();
        let open: Vec<&PlanRow> = rows.iter().filter(|r| cw::is_crosswalk_row(r)).collect();
        assert_eq!(open.len(), 1, "only the seeded author manuscript is planned as text");
        assert!(open[0].doc_key.starts_with("acp_im-pmc-pmc11614146-"));
        assert_eq!(rows.iter().filter(|r| r.extra["kind"] == "catalog_publication").count(), 59, "the open copy keeps the catalog row's extra");
        let index = &rows[0];
        assert_eq!(index.extra["kind"], "source_index");
        assert_eq!(index.extra[r2a::OPEN].as_array().unwrap().len(), 1);
        assert_eq!(index.extra[r2a::CLOSED].as_array().unwrap().len(), 58);
        // 12 other catalog DOIs are in PMC: relation-only links on the index document.
        let rel_only = index.extra[r2a::CLOSED].as_array().unwrap().iter().filter(|c| !c["relation"].is_null()).count();
        assert_eq!(rel_only, 12);
        let d = AcpIm.extract(&src, &f, open[0]).await.unwrap();
        assert_eq!(d.rights.status, "restricted");
        assert!(d.rights.restrictions.iter().any(|r| r.contains("available for text mining")));
        assert!(d.relations.iter().any(|r| r.rel == "rendition_of" && r.target == "pmc:PMC11614146"));
        assert!(d.relations.iter().any(|r| r.target == format!("guideline:acp_im:{}", index.doc_key)));
        assert!(d.relations.iter().any(|r| r.target.starts_with("https://www.acpjournals.org/doi/")));
        assert_eq!(d.item_type.as_deref(), Some("guideline"), "M23-2788 is listed under Current Guidelines");
        assert_eq!(catalog_item_type(&json!({"section":"Current Guidelines","source_record":"(Update Alert 1, published online May 2024) Pharmacologic"})), "guideline_update_alert");
        assert_eq!(catalog_item_type(&json!({"section":"Practice Points","source_record":"x"})), "practice_points");
        assert!(d.warnings.iter().any(|w| w.contains("Listed in the ACP clinical guidelines catalog")));
        let idx = AcpIm.extract(&src, &f, index).await.unwrap();
        assert_eq!(idx.relations.iter().filter(|r| r.target.starts_with("pmc:PMC")).count(), 12);
        assert!(idx.relations.iter().any(|r| r.target == format!("guideline:acp_im:{}", open[0].doc_key)));
        assert_eq!(idx.search_metadata["pmc_crosswalk"]["closed"].as_array().unwrap().len(), 58);
        let not_in_pmc = rows.iter().find(|r| r.extra["kind"] == "catalog_publication" && r.extra[r2a::CLOSED][0]["reason"].as_str().is_some_and(|s| s.contains("Identifier not found"))).unwrap();
        let e = AcpIm.extract(&src, &f, not_in_pmc).await.unwrap_err().to_string();
        assert!(e.contains("403") && e.contains("PMC crosswalk") && e.contains("Identifier not found in PMC"), "{e}");
    }
}
