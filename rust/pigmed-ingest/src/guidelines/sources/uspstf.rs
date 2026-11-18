//! USPSTF recommendation topics. Catalog pagination follows the publisher's `next` link;
//! prior editions come from the recommendation page's archived-links list.
use super::{Adapter, PlanRow};
use super::common::{cells, date, insert_table_recommendations, link, page_language};
use crate::guidelines::doc::{Block, BlockKind, GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{anyhow, bail, Result};
use scraper::Html;
use std::collections::{HashSet, VecDeque};
use url::Url;

pub struct Uspstf;

fn slug_url(url: &str) -> Result<String> {
    Ok(Url::parse(url)?.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("topic URL has no slug: {url}"))?.to_string())
}

fn catalog(html: &str, base: &Url) -> Result<(Vec<(String,String)>, Option<String>)> {
    let doc = Html::parse_document(html);
    if doc.select(&sel("#uspstf-search-results table")).next().is_none() { bail!("USPSTF catalog result table missing"); }
    let mut result = Vec::new();
    for row in doc.select(&sel("#uspstf-search-results table tbody tr")) {
        let c = cells(row);
        if c.len() < 4 { bail!("USPSTF result row has fewer than four cells"); }
        let a = c[3].select(&sel("a[href]")).next().ok_or_else(|| anyhow!("USPSTF result row missing topic link"))?;
        let url = link(base, a.value().attr("href").unwrap()).ok_or_else(|| anyhow!("invalid topic link"))?;
        result.push((url, text_of(&a)));
    }
    let next = doc.select(&sel("a[title='next'][href]")).next().and_then(|a| link(base, a.value().attr("href").unwrap()));
    Ok((result,next))
}

// Archive lists can include broader or differently scoped predecessors. Equal titles
// (apart from an explicit trailing edition year) plus an archive link establish the
// narrow grouping used here; other linked works remain ungrouped.
fn same_work_title(a: &str, b: &str) -> bool {
    let edition = regex::Regex::new(r",?\s+(?:19|20)\d{2}\s*$").unwrap();
    let canonical = |s: &str| edition.replace(s.trim(), "").trim().to_lowercase();
    !canonical(a).is_empty() && canonical(a) == canonical(b)
}

fn details(html: &str, base: &Url) -> Result<(String, String, Vec<String>)> {
    let doc = Html::parse_document(html);
    let title = doc.select(&sel(".recommendation-statement-intro [data-qa='extendedtitle']")).next().map(|e| text_of(&e)).ok_or_else(|| anyhow!("USPSTF recommendation title missing"))?;
    let raw = doc.select(&sel(".recommendation-statement-intro [data-qa='pubdate']")).next().map(|e| text_of(&e)).ok_or_else(|| anyhow!("USPSTF recommendation date missing"))?;
    let normalized = date("datePublished", &raw, base.as_str()).normalized.ok_or_else(|| anyhow!("unrecognized USPSTF date {raw}"))?;
    let archived = doc.select(&sel(".archived-links a[href]")).filter_map(|a| link(base,a.value().attr("href")?)).collect();
    Ok((title,normalized,archived))
}

/// The publisher's status page for a topic under revision ("An Update for This Topic is In Progress"), reached
/// from catalog rows labelled "Update in Progress". It carries no recommendation statement or grade.
fn update_in_progress_page(html: &str) -> bool {
    let doc = Html::parse_document(html);
    doc.select(&sel(".recommendation-statement-intro [data-qa='extendedtitle']")).next().is_none()
        && doc.select(&sel("h1, h2, h3, h4")).any(|h| text_of(&h).eq_ignore_ascii_case("An Update for This Topic is In Progress"))
}

fn explicitly_archived(doc: &Html) -> bool {
    doc.select(&sel("#archivedribbon, .new_revisions")).any(|e| {
        let t = text_of(&e).to_lowercase();
        t.contains("archived") || t.contains("out of date")
    })
}

/// Identify only the source's explicit legacy statement + PDF links in an empty
/// recommendation-summary wrapper. Unexpected structures remain audit failures.
fn legacy_links(html: &str, base: &Url) -> Result<Option<(String,String)>> {
    let doc = Html::parse_document(html);
    let Some(table) = doc.select(&sel(".summary-table table")).next() else { return Ok(None) };
    if table.select(&sel("tbody tr")).next().is_some() { return Ok(None); }
    let mut statement = None; let mut pdf = None;
    for a in doc.select(&sel(".collapsable-wrapper .card-body a[href]")) {
        let label = text_of(&a);
        let Some(url) = a.value().attr("href").and_then(|h|link(base,h)) else {continue};
        if label.to_lowercase().contains("recommendation statement") { statement = Some(url); }
        else if label == "PDF File" { pdf = Some(url); }
    }
    match (statement,pdf) {
        (Some(statement),Some(pdf)) => Ok(Some((statement,pdf))),
        _ => bail!("USPSTF empty summary and explicit full-statement HTML/PDF links not found"),
    }
}

async fn extract_legacy(src: &SourceEntry, f: &Fetcher, row: &PlanRow,
    wrapper: crate::guidelines::fetch::Fetched, statement_url: String, pdf_url: String) -> Result<GuidelineDoc> {
    let wrapper_base = Url::parse(&wrapper.record.final_url)?;
    let mut visited = HashSet::new();
    visited.insert(wrapper.record.url.clone());
    visited.insert(wrapper.record.final_url.clone());
    // Exactly one HTML hop and one explicitly linked PDF; reject cross-host or cycles.
    for target in [&statement_url, &pdf_url] {
        let u = Url::parse(target)?;
        if u.origin() != wrapper_base.origin() || !visited.insert(target.clone()) {
            bail!("USPSTF legacy link crosses origin or forms a cycle: {target}");
        }
    }
    let statement = f.get_ok(&statement_url).await?;
    let terminal_origin = Url::parse(&statement.record.final_url)?;
    if terminal_origin.origin() != wrapper_base.origin()
        || statement.record.final_url == wrapper.record.final_url || statement.record.final_url == pdf_url {
        bail!("USPSTF legacy statement redirects to an earlier target");
    }
    let statement_html = statement.text();
    let statement_base = Url::parse(&statement.record.final_url)?;
    let (terminal_title, terminal_date) = {
        let doc = Html::parse_document(&statement_html);
        let root = doc.select(&sel("article.ahrquspstf")).next().ok_or_else(||anyhow!("legacy statement clinical root missing"))?;
        if root.select(&sel(".collapsable-wrapper .card-body")).next().is_none() { bail!("legacy statement has no full clinical body"); }
        let title = root.select(&sel(".recommendation-statement-intro h2")).next().map(|x|text_of(&x)).ok_or_else(||anyhow!("legacy statement title missing"))?;
        let raw = root.select(&sel(".recommendation-statement-intro .pubdate")).next().map(|x|text_of(&x)).ok_or_else(||anyhow!("legacy statement date missing"))?;
        (title,date("datePublished",&raw,statement_base.as_str()))
    };
    let pdf = f.get_ok(&pdf_url).await?;
    let pdf_origin = Url::parse(&pdf.record.final_url)?;
    if !pdf.is_pdf() || pdf_origin.origin() != wrapper_base.origin()
        || pdf.record.final_url == statement.record.final_url || pdf.record.final_url == wrapper.record.final_url {
        bail!("USPSTF legacy PDF target is not distinct PDF content");
    }
    let (mut blocks, mut warnings, _) = super::static_list::blocks_for(&pdf,None,30).await?;
    let mut found = 0;
    for b in &mut blocks {
        if matches!(b.kind, BlockKind::Paragraph) && b.heading_path.iter().any(|h|h == "Summary of Recommendation")
            && b.text.starts_with("The USPSTF found insufficient evidence to recommend for or against routine screening")
            && b.text.contains("I recommendation.") {
            b.kind = BlockKind::Recommendation { label:None, strength:Some("I recommendation.".into()), certainty:None, population:None, grading_system:None, qualifier:None };
            found += 1;
        }
    }
    if found != 1 { bail!("USPSTF legacy PDF did not yield exactly one verifiable summary recommendation and grade"); }
    let wrapper_html = wrapper.text();
    let wrapper_doc = Html::parse_document(&wrapper_html);
    let archived = explicitly_archived(&wrapper_doc);
    let raw = wrapper_doc.select(&sel(".recommendation-statement-intro [data-qa='pubdate']")).next().map(|e|text_of(&e)).unwrap_or_default();
    let mut dates = vec![date("wrapper datePublished", &raw,wrapper_base.as_str()), terminal_date.clone()];
    for entry in wrapper_doc.select(&sel(".archived-links li")) {
        let t = text_of(&entry);
        if t.contains('»') && t.contains('«') {
            let raw = t.trim_matches(|c: char| c == '»' || c == '«' || c == '(' || c == ')' || c.is_whitespace());
            dates.push(date("Archived Versions",raw,wrapper_base.as_str()));
        }
    }
    warnings.push("The source wrapper links a broader family and intimate partner violence statement; its PDF supplies the clinical recommendation. Wrapper and terminal dates describe different source observations and their relationship is not established.".into());
    let attachments = vec![serde_json::json!({"url":statement_url,"role":"linked_full_statement_html","rights":"unknown"}),
        serde_json::json!({"url":pdf_url,"role":"linked_full_statement_pdf","rights":"unknown"})];
    let relations = vec![Relation {rel:"related".into(),target:statement.record.final_url.clone(),scope:Some("Full statement linked by source wrapper; broader family and intimate partner violence scope".into()),evidence_url:Some(row.url.clone())}];
    Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:None,
        item_type:Some("final_recommendation_statement".into()),title:terminal_title,url:row.url.clone(),status:if archived {"archived"} else {"unknown"}.into(),
        published:terminal_date.normalized.clone(), date_observations:dates, relations,
        rights:Rights {status:"unknown".into(),..Default::default()},blocks,attachments,
        fetches:vec![wrapper.record,statement.record,pdf.record],warnings,..Default::default()})
}

#[async_trait::async_trait]
impl Adapter for Uspstf {
    async fn discover(&self, src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) { return Ok(vec![]); }
        let mut page = src.catalog_url.clone();
        let mut pages = HashSet::new();
        let mut page_results = HashSet::new();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while let Some(url) = page {
            if pages.len() >= 1000 { bail!("USPSTF catalog exceeded 1000 pages"); }
            if !pages.insert(url.clone()) { bail!("USPSTF catalog pagination cycle at {url}"); }
            let fetched = f.get_ok(&url).await?;
            let base = Url::parse(&fetched.record.final_url)?;
            let (topics,next) = catalog(&fetched.text(), &base)?;
            let signature = topics.iter().map(|(url,_)| url.as_str()).collect::<Vec<_>>().join("\n");
            if !topics.is_empty() && !page_results.insert(signature) { bail!("USPSTF catalog repeated identical result rows at {url}"); }
            if topics.is_empty() && next.is_some() { bail!("USPSTF catalog has next link but no result rows"); }
            for (topic, title) in topics {
                let work = slug_url(&topic)?;
                let mut pending = VecDeque::from([(topic, title, Some(work), None::<String>)]);
                while let Some((url, hint, candidate_work, parent_title)) = pending.pop_front() {
                    if seen.len() >= 20000 { bail!("USPSTF discovery exceeded 20000 topic versions"); }
                    if !seen.insert(url.clone()) { continue; }
                    // One bad catalog item must not abort discovery of the whole catalog: record it as an
                    // unresolved row (audited at ingest) and keep going. Source-labelled "update in progress"
                    // status pages are not recommendation statements and are skipped with a warning.
                    let resolved = async {
                        let body = f.get_ok(&url).await?;
                        let base = Url::parse(&body.record.final_url)?;
                        let html = body.text();
                        if update_in_progress_page(&html) { return Ok::<_, anyhow::Error>(None); }
                        Ok(Some(details(&html, &base)?))
                    }.await;
                    let (title, version, archived) = match resolved {
                        Ok(Some(d)) => d,
                        Ok(None) => { eprintln!("warning: USPSTF {url} is an 'update in progress' status page, not a recommendation statement; skipped"); continue; }
                        Err(e) => {
                            out.push(PlanRow { source_id: src.id.clone(), doc_key: format!("{}-unresolved", slug_url(&url)?), url: url.clone(),
                                title: (!hint.is_empty()).then(|| hint.clone()), rendition: Some("html".into()), version: None,
                                extra: serde_json::json!({"work_key":candidate_work,"discovery_error":format!("{e:#}")}) });
                            if limit.is_some_and(|l| out.len() >= l) { return Ok(out); }
                            continue;
                        }
                    };
                    let work = if parent_title.as_ref().is_some_and(|p| !same_work_title(p, &title)) { None } else { candidate_work };
                    out.push(PlanRow { source_id: src.id.clone(), doc_key: format!("{}-{}",slug_url(&url)?,version), url,
                        title: Some(if title.is_empty() {hint} else {title.clone()}), rendition: Some("html".into()), version: Some(version),
                        extra: serde_json::json!({"work_key":work.clone()}) });
                    if limit.is_some_and(|l| out.len() >= l) { return Ok(out); }
                    for old in archived { pending.push_back((old, String::new(), work.clone(), Some(title.clone()))); }
                }
            }
            page = next;
        }
        Ok(out)
    }

    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(e) = row.extra["discovery_error"].as_str() { bail!("USPSTF discovery: {e}"); }
        let fetched = f.get_ok(&row.url).await?;
        let base = Url::parse(&fetched.record.final_url)?;
        let html = fetched.text();
        if let Some((statement,pdf)) = legacy_links(&html,&base)? {
            return extract_legacy(src,f,row,fetched,statement,pdf).await;
        }
        let doc = Html::parse_document(&html);
        if doc.select(&sel(".recommendation-statement")).next().is_none() { bail!("USPSTF content root missing"); }
        let (title, _, _) = details(&html, &base)?;
        let raw = doc.select(&sel(".recommendation-statement-intro [data-qa='pubdate']")).next().map(|e| text_of(&e)).unwrap_or_default();
        let observed = date("datePublished", &raw, base.as_str());
        let table = doc.select(&sel(".summary-table table")).next().ok_or_else(|| anyhow!("USPSTF recommendation summary table missing"))?;
        let mut statements = Vec::new();
        for (i,tr) in table.select(&sel("tbody tr")).enumerate() {
            let c = cells(tr);
            if c.len() != 3 { bail!("USPSTF summary row has {} cells, expected three", c.len()); }
            let pop = text_of(&c[0]); let statement = text_of(&c[1]); let grade = text_of(&c[2]);
            if pop.is_empty() || statement.is_empty() || grade.is_empty() { bail!("USPSTF summary row has empty population, statement, or grade"); }
            statements.push(Block { block_id: format!("uspstf-rec-{}",i+1), kind: BlockKind::Recommendation { label: None,
                strength: Some(grade), certainty: None, population: Some(pop), grading_system: None, qualifier: None },
                text: statement, heading_path: vec![], page: None });
        }
        if statements.is_empty() {
            let links = doc.select(&sel(".collapsable-wrapper .card-body a[href]"))
                .filter_map(|a| a.value().attr("href").and_then(|h|link(&base,h)))
                .collect::<Vec<_>>();
            bail!("USPSTF recommendation summary has no rows; full statement unresolved; linked targets: {}",links.join(", "));
        }
        let root = doc.select(&sel("article.ahrquspstf .field--name-field-php-body")).next().ok_or_else(|| anyhow!("USPSTF full statement content root missing"))?;
        if root.select(&sel(".collapsable-wrapper")).next().is_none() { bail!("USPSTF full recommendation body missing"); }
        let mut statement_html = root.html();
        // The publisher renders full-statement section labels as accordion links,
        // not heading elements. Promote those visible labels for the shared walker.
        for a in root.select(&sel(".card-header a.card-title")) {
            statement_html = statement_html.replacen(&a.html(), &format!("<h2>{}</h2>", a.inner_html()), 1);
        }
        let (mut blocks, mut warnings) = blocks_from_html(&statement_html, Some(&base), Some(".field--name-field-php-body"));
        let table_id = blocks.iter().find(|b| matches!(b.kind,BlockKind::Table { .. }) && b.text.starts_with("| Population | Recommendation | Grade |"))
            .map(|b| b.block_id.clone()).ok_or_else(|| anyhow!("USPSTF summary table was not preserved"))?;
        insert_table_recommendations(&mut blocks,&table_id,statements)?;
        if root.select(&sel(".bcei")).next().is_some() { warnings.push("Introductory FAQ and clinician summary are retained as prose; formal recommendations are extracted only from the summary table.".into()); }
        let archived = explicitly_archived(&doc);
        let mut relations = Vec::new();
        for a in doc.select(&sel(".new_revisions a[href]")) {
            if let Some(target) = link(&base,a.value().attr("href").unwrap()) { relations.push(Relation { rel:"superseded_by".into(), target, scope:None, evidence_url:Some(row.url.clone()) }); }
        }
        for a in doc.select(&sel(".archived-links a[href]")) {
            if let Some(target) = link(&base,a.value().attr("href").unwrap()) {
                relations.push(Relation { rel:"related".into(), target, scope:Some(format!("Archived Versions: {}", text_of(&a))), evidence_url:Some(row.url.clone()) });
            }
        }
        let mut attachment_urls = HashSet::new();
        let attachments = doc.select(&sel(".custom-recommendation-link a.btn-download[href], .clinical-summary-pdf-links a[href], #supdocs a[href], .bcei a[href]"))
            .filter_map(|a| {
                let href = a.value().attr("href")?;
                if href.starts_with('#') { return None; }
                let url = link(&base, href)?;
                if !attachment_urls.insert(url.clone()) { return None; }
                Some(serde_json::json!({"url":url,"label":text_of(&a),"rights":"unknown; asset-specific rights require review"}))
            }).collect();
        let copyright: Vec<String> = doc.select(&sel("[id='Copyright and Source Information'] p")).map(|p|text_of(&p)).collect();
        let terms = copyright.iter().position(|p|p.starts_with("Copyright Notice:"));
        let rights = if let Some(i) = terms {
            let restrictions = copyright.iter().skip(i).take(3).cloned().collect();
            Rights { status:"restricted".into(), terms_url:Some("https://www.uspreventiveservicestaskforce.org/uspstf/recommendation-topics/copyright-notice".into()),
                attribution:copyright.get(i+2).cloned(), restrictions, evidence_urls:vec![row.url.clone()], ..Default::default() }
        } else if copyright.iter().any(|p|p == "This document is in the public domain within the United States.") {
            Rights { status:"unknown".into(), license:Some("public domain within the United States".into()),
                note:Some("This document is in the public domain within the United States.".into()),
                restrictions:copyright.iter().filter(|p|p.starts_with("Requests for linking or to incorporate content")).cloned().collect(),
                evidence_urls:vec![row.url.clone()], ..Default::default() }
        } else { Rights { status:"unknown".into(), ..Default::default() } };
        let mut date_observations = vec![observed.clone()];
        for entry in doc.select(&sel(".archived-links li")) {
            let t = text_of(&entry);
            if t.contains('»') && t.contains('«') {
                let archived_raw = t.trim_matches(|c: char| c == '»' || c == '«' || c == '(' || c == ')' || c.is_whitespace());
                let archived_date = date("Archived Versions", archived_raw, base.as_str());
                if archived_date.normalized.as_ref().zip(observed.normalized.as_ref()).is_some_and(|(a,b)| !b.starts_with(a)) {
                    warnings.push(format!("Archive listing date {} differs from page datePublished {}; their relationship is not established.", archived_raw, observed.raw));
                }
                date_observations.push(archived_date);
            }
        }
        let item_type = doc.select(&sel(".recommendation-statement-intro h4")).next().map(|e| super::common::slug(&text_of(&e)).replace('-', "_"));
        Ok(GuidelineDoc { source_id:src.id.clone(), doc_key:row.doc_key.clone(), work_key:row.extra.get("work_key").and_then(|v|v.as_str()).map(String::from),
            item_type, title, url:row.url.clone(), publisher:None, issuers:vec![],
            version:row.version.clone(), status:if archived {"archived"} else {"unknown"}.into(), published:observed.normalized.clone(), date_observations,
            relations, language:page_language(&doc), rights,
            blocks, attachments, fetches:vec![fetched.record], warnings, ..Default::default() })
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const FAMILY_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/family-violence-statement-2004.html"));
    const FAMILY_PDF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/family-violence-statement-2004.pdf"));
    const CHILD: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/child-maltreatment-2004.html"));
    const CATALOG: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/catalog.html"));
    const PAGE_TWO: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/catalog-page-2.html"));
    const Y2009: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/edition-2009.html"));
    const Y2002: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/edition-2002.html"));
    const CURRENT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/current.html"));
    const OLD: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/archived.html"));
    const TOPIC: &str = "https://www.uspreventiveservicestaskforce.org/uspstf/recommendation/breast-cancer-screening";
    const PREVIOUS: &str = "https://www.uspreventiveservicestaskforce.org/uspstf/recommendation/breast-cancer-screening-january-2016";

    #[test]
    fn archived_broader_work_does_not_share_work_identity() {
        assert!(same_work_title("Breast Cancer: Screening", "Breast Cancer: Screening, 2002"));
        assert!(same_work_title("Breast Cancer: Screening", "Breast Cancer: Screening"));
        assert!(!same_work_title("Child Maltreatment: Primary Care Interventions", "Screening for Family and Intimate Partner Violence"));
        assert!(!same_work_title("Child Maltreatment: Primary Care Interventions", "Child Maltreatment: Counseling, 2004"));
        assert!(!same_work_title("", ""));
    }

    #[tokio::test]
    async fn saved_catalog_and_recommendation_pages_offline() {
        let cat_url = "https://www.uspreventiveservicestaskforce.org/uspstf/topic_search_results";
        let (rows,next) = catalog(CATALOG,&Url::parse(cat_url).unwrap()).unwrap();
        assert_eq!(rows.len(),20);
        assert_eq!(next.as_deref(),Some("https://www.uspreventiveservicestaskforce.org/uspstf/topic_search_results?topic_status=All&PAGE=2"));
        assert!(rows.iter().any(|(url,_)| url == TOPIC));
        let page2_url = next.unwrap();
        let (page2, next2) = catalog(PAGE_TWO, &Url::parse(&page2_url).unwrap()).unwrap();
        assert_eq!(page2.len(), 20);
        assert_eq!(next2.as_deref(), Some("https://www.uspreventiveservicestaskforce.org/uspstf/topic_search_results?topic_status=All&PAGE=3"));
        assert!(page2.iter().all(|(url,_)| !rows.iter().any(|(u,_)|u == url)));
        assert!(page2.iter().any(|(url,_)| !url.contains("/recommendation/")));
        // Keep the source's actual breast-cancer result row while bounding this offline discovery to one topic.
        let marker = CATALOG.find("href='/uspstf/recommendation/breast-cancer-screening'").unwrap();
        let begin = CATALOG[..marker].rfind("<tr class=").unwrap();
        let end = marker + CATALOG[marker..].find("</tr>").unwrap() + 5;
        let one = format!("<div id='uspstf-search-results'><table><tbody>{}</tbody></table></div>",&CATALOG[begin..end]);
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(),true,500,1 << 20,"").unwrap();
        f.seed_cache_for_tests(cat_url,one.as_bytes(),"text/html");
        f.seed_cache_for_tests(TOPIC,CURRENT.as_bytes(),"text/html");
        f.seed_cache_for_tests(PREVIOUS,OLD.as_bytes(),"text/html");
        let src: SourceEntry = serde_json::from_value(serde_json::json!({"id":"uspstf","access":"public","enabled":true,"catalog_url":cat_url})).unwrap();
        let rows = Uspstf.discover(&src,&f,Some(2)).await.unwrap();
        assert_eq!(rows.len(),2);
        assert_eq!(rows[0].version.as_deref(),Some("2024-04-30"));
        assert_eq!(rows[1].version.as_deref(),Some("2016-01-11"));
        let current = Uspstf.extract(&src,&f,&rows[0]).await.unwrap();
        let old = Uspstf.extract(&src,&f,&rows[1]).await.unwrap();
        assert_eq!(current.work_key,old.work_key);
        assert_eq!(current.status,"unknown");
        assert_eq!(old.status,"archived");
        assert_eq!(current.rights.status,"restricted");
        assert!(current.rights.restrictions.iter().any(|s|s.contains("written permission")));
        assert!(current.attachments.iter().any(|a|a["url"].as_str().is_some_and(|u|u.contains("NBK603789"))));
        assert_eq!(current.published.as_deref(),Some("2024-04-30"));
        assert!(old.relations.iter().any(|r|r.rel == "superseded_by" && r.target == TOPIC));
        let recs: Vec<_> = current.blocks.iter().filter_map(|b| if let BlockKind::Recommendation {strength,population,..} = &b.kind { Some((strength.as_deref(),population.as_deref(),b.text.as_str())) } else {None}).collect();
        assert_eq!(recs.len(),3);
        assert_eq!((recs[0].0,recs[0].1),(Some("B"),Some("Women aged 40 to 74 years")));
        assert_eq!(recs[0].2,"The USPSTF recommends biennial screening mammography for women aged 40 to 74 years.");
        assert_eq!((recs[1].0,recs[1].1),(Some("I"),Some("Women 75 years or older")));
        assert_eq!((recs[2].0,recs[2].1),(Some("I"),Some("Women with dense breasts")));
        assert_eq!(old.blocks.iter().filter(|b| matches!(b.kind,BlockKind::Recommendation{..})).count(),5);
        assert!(current.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..}) && b.text.contains("Population | Recommendation | Grade")));
        assert!(current.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text == "Recommendation Summary"));
        assert!(current.blocks.iter().any(|b| matches!(b.kind,BlockKind::Heading{..}) && b.text == "Importance"));
        assert!(current.blocks.iter().any(|b| b.text.contains("Among all US women, breast cancer is the second most common cancer")));
        assert!(current.blocks.iter().any(|b| b.text.contains("To update its 2016 recommendation") && b.heading_path.iter().any(|h|h == "Supporting Evidence") && b.heading_path.iter().any(|h|h == "Scope of Review")));
        assert!(current.blocks.iter().filter(|b| matches!(b.kind,BlockKind::Table{..})).count() >= 5);
        assert!(old.blocks.iter().any(|b| b.text.contains("The USPSTF concludes with moderate certainty")));
        assert!(old.blocks.iter().any(|b| matches!(&b.kind, BlockKind::Figure { src_url: Some(url), .. } if url.ends_with("/Home/GetImage/1/1311/breastcanfinalrsfig/gif"))));
        assert!(current.attachments.iter().any(|a| a["label"].as_str().is_some_and(|s|s.contains("JAMA"))));
    }

    #[tokio::test]
    async fn differently_scoped_archive_stays_ungrouped() {
        // Preserve both real source documents; alter only the archive edge to exercise
        // a linked predecessor whose published title denotes a different work.
        let child = "https://www.uspreventiveservicestaskforce.org/uspstf/recommendation/child-maltreatment-counseling-2004";
        let current = CURRENT.replace("/uspstf/recommendation/breast-cancer-screening-january-2016", "/uspstf/recommendation/child-maltreatment-counseling-2004");
        let catalog_url = "https://www.uspreventiveservicestaskforce.org/uspstf/topic_search_results";
        let catalog = format!("<div id='uspstf-search-results'><table><tbody><tr><td></td><td></td><td></td><td><a href='{TOPIC}'>Breast Cancer: Screening</a></td></tr></tbody></table></div>");
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(),true,500,1 << 20,"").unwrap();
        f.seed_cache_for_tests(catalog_url,catalog.as_bytes(),"text/html");
        f.seed_cache_for_tests(TOPIC,current.as_bytes(),"text/html");
        f.seed_cache_for_tests(child,CHILD.as_bytes(),"text/html");
        let src: SourceEntry = serde_json::from_value(serde_json::json!({"id":"uspstf","access":"public","enabled":true,"catalog_url":catalog_url})).unwrap();
        let rows = Uspstf.discover(&src,&f,Some(2)).await.unwrap();
        assert_eq!(rows[0].extra["work_key"].as_str(),Some("breast-cancer-screening"));
        assert!(rows[1].extra["work_key"].is_null());
        let doc = Uspstf.extract(&src,&f,&rows[0]).await.unwrap();
        assert!(doc.relations.iter().any(|r|r.rel == "related" && r.target == child && r.scope.as_deref().is_some_and(|s|s.starts_with("Archived Versions:")) && r.evidence_url.as_deref() == Some(TOPIC)));
    }

    #[tokio::test]
    async fn saved_historical_editions_preserve_content_and_rights() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(),true,500,1 << 20,"").unwrap();
        let src: SourceEntry = serde_json::from_value(serde_json::json!({"id":"uspstf","access":"public","enabled":true})).unwrap();
        for (year, html, expected_date, grades) in [
            ("2009",Y2009,"2009-12-15",vec!["B","C","D","I","I","I"]),
            ("2002",Y2002,"2002-09-03",vec!["B","I","I"]),
        ] {
            let url = format!("{TOPIC}-{year}");
            f.seed_cache_for_tests(&url,html.as_bytes(),"text/html");
            let row = PlanRow {source_id:"uspstf".into(),doc_key:format!("breast-cancer-screening-{year}-{expected_date}"),
                url, title:None, rendition:Some("html".into()),version:Some(expected_date.into()),
                extra:serde_json::json!({"work_key":"breast-cancer-screening"})};
            let doc = Uspstf.extract(&src,&f,&row).await.unwrap();
            assert_eq!(doc.status,"archived");
            assert_eq!(doc.item_type.as_deref(),Some("final_recommendation_statement"));
            assert_eq!(doc.published.as_deref(),Some(expected_date));
            let found: Vec<_> = doc.blocks.iter().filter_map(|b|if let BlockKind::Recommendation {strength,..}=&b.kind {strength.as_deref()} else {None}).collect();
            assert_eq!(found,grades);
            assert!(doc.relations.iter().any(|r|r.rel == "superseded_by" && r.target == TOPIC));
            assert!(doc.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading {..}) && b.text == "Clinical Considerations"));
            if year == "2009" {
                assert_eq!(doc.rights.license.as_deref(),Some("public domain within the United States"));
                assert!(doc.rights.restrictions.iter().any(|s|s.contains("Requests for linking")));
                assert!(doc.blocks.iter().any(|b|b.text.contains("first published in") && b.text.contains("November 2009")));
            } else {
                assert_eq!(doc.rights.status,"unknown");
                assert!(doc.blocks.iter().any(|b|b.text.contains("This topic page summarizes") && b.text.contains("2002 recommendations")));
                assert!(doc.attachments.iter().any(|a|a["url"].as_str().is_some_and(|u|u.contains("NBK42742"))));
            }
        }
    }

    #[tokio::test]
    async fn archived_page_links_full_statement_with_empty_summary() {
        let url = "https://www.uspreventiveservicestaskforce.org/uspstf/recommendation/child-maltreatment-counseling-2004";
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(),true,500,1 << 20,"").unwrap();
        f.seed_cache_for_tests(url,CHILD.as_bytes(),"text/html");
        let src: SourceEntry = serde_json::from_value(serde_json::json!({"id":"uspstf","access":"public","enabled":true})).unwrap();
        let row = PlanRow {source_id:"uspstf".into(),doc_key:"child-maltreatment-counseling-2004-2019-05-20".into(),url:url.into(),
            title:None,rendition:Some("html".into()),version:Some("2019-05-20".into()),extra:serde_json::json!({"work_key":"child-maltreatment-primary-care-interventions"})};
        assert!(explicitly_archived(&Html::parse_document(CHILD)));
        let no_archive_markers = CHILD.replace("id=\"archivedribbon\"", "id=\"other-ribbon\"").replace("This Recommendation is out of date", "Recommendation notice");
        assert!(!explicitly_archived(&Html::parse_document(&no_archive_markers)));
        let statement = "https://www.uspreventiveservicestaskforce.org/Page/Document/RecommendationStatementFinal/intimate-partner-violence-and-elderly-abuse-screening-2004";
        let pdf = "https://www.uspreventiveservicestaskforce.org/Home/GetFile/1/516/famviolrs/pdf";
        f.seed_cache_for_tests(statement,FAMILY_HTML.as_bytes(),"text/html");
        f.seed_cache_for_tests(pdf,FAMILY_PDF,"application/pdf");
        let d = Uspstf.extract(&src,&f,&row).await.unwrap();
        assert_eq!(d.status,"archived");
        assert!(d.work_key.is_none());
        assert_eq!(d.title,"Intimate Partner Violence and Elderly Abuse: Screening, 2004");
        assert_eq!(d.published.as_deref(),Some("2004-03-08"));
        assert_eq!(d.fetches.len(),3);
        assert_eq!(d.rights.status,"unknown");
        assert!(d.date_observations.iter().any(|x|x.label == "wrapper datePublished" && x.normalized.as_deref() == Some("2019-05-20")));
        assert!(d.date_observations.iter().any(|x|x.label == "Archived Versions" && x.raw == "January 2004" && x.normalized.as_deref() == Some("2004-01")));
        let recs: Vec<_> = d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();
        assert_eq!(recs.len(),1);
        assert!(matches!(&recs[0].kind,BlockKind::Recommendation { strength:Some(s), .. } if s == "I recommendation."));
        assert!(recs[0].text.contains("routine screening of parents or guardians for the physical abuse or neglect of children"));
        assert!(recs[0].text.contains("of women for intimate partner violence") && recs[0].text.contains("older adults or their caregivers for elder abuse"));
        assert!(d.relations.iter().any(|r|r.rel == "related" && r.scope.as_deref().is_some_and(|s|s.contains("broader"))));
        assert!(d.blocks.iter().any(|b|b.text.contains("The USPSTF found no direct evidence")));
        let cyclic = CHILD.replace("/Page/Document/RecommendationStatementFinal/intimate-partner-violence-and-elderly-abuse-screening-2004",url);
        f.seed_cache_for_tests(url,cyclic.as_bytes(),"text/html");
        assert!(Uspstf.extract(&src,&f,&row).await.unwrap_err().to_string().contains("cycle"));

    }

}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod scale_skip_test {
    //! Full-catalog plan (2026-11-17) aborted on a catalog row labelled "Update in Progress" whose target is a
    //! status page with no recommendation title. It is now skipped; other unresolvable items become audited rows.
    use super::*;
    const DRAFT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/draft-update-in-progress.html"));
    const CURRENT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/uspstf/current.html"));
    #[test]
    fn update_in_progress_status_page_is_recognised_and_statements_are_not() {
        let base = Url::parse("https://www.uspreventiveservicestaskforce.org/uspstf/draft-update-summary/behavioral-counseling-interventions-promote-healthy-diet-physical-activity-weight-loss-prevent-cardiovascular-disease-adults").unwrap();
        assert!(details(DRAFT, &base).is_err());
        assert!(update_in_progress_page(DRAFT));
        assert!(!update_in_progress_page(CURRENT));
        assert!(!update_in_progress_page("<html><body><h1>Something else</h1></body></html>"));
    }
    #[tokio::test]
    async fn unresolved_discovery_row_is_an_audited_extract_failure() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 5 << 20, "").unwrap();
        let src: SourceEntry = serde_json::from_value(serde_json::json!({"id":"uspstf","access":"public","enabled":true})).unwrap();
        let row = PlanRow { source_id: "uspstf".into(), doc_key: "x-unresolved".into(), url: "https://www.uspreventiveservicestaskforce.org/uspstf/recommendation/x".into(),
            title: None, rendition: None, version: None, extra: serde_json::json!({"discovery_error":"USPSTF recommendation title missing"}) };
        let e = Uspstf.extract(&src, &f, &row).await.unwrap_err();
        assert!(format!("{e:#}").contains("USPSTF discovery: USPSTF recommendation title missing"));
    }
}
