//! VA/DoD catalog -> topic landing pages -> explicitly labelled full-guideline PDFs.
//! Provider summaries, quick references and patient tools remain separate attachments; their dates never
//! replace the guideline edition. Catalog navigation is deduplicated, bounded, and does not synthesize URLs.
use super::{common, static_list::blocks_for, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, FetchRecord, GuidelineDoc, Relation};
use crate::guidelines::fetch::{Fetched, Fetcher};
use crate::guidelines::html::{sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use url::Url;

pub struct VaDod;

fn identity_url(url: &str) -> String {
    // VA emits both /HEALTHQUALITY/guidelines/... and /guidelines/... in its navigation.
    // This only deduplicates links; the request always uses the exact source-emitted URL.
    url.to_ascii_lowercase().replace("/healthquality/", "/").replace("/index.asp", "/")
}

fn topic_key(url: &Url) -> String {
    let path = identity_url(url.path());
    common::slug(path.trim_matches('/'))
}

fn full_guideline(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    // "MDD Full Text (2022)" and "Patients at Risk for Suicide - Full Guideline (2024)" are the publisher's own
    // full-guideline labels; "patient" alone must not exclude "Patients at Risk", only patient-facing renditions.
    (l.contains("full guideline") || l.contains("full cpg") || l.contains("full text") || l == "clinical practice guideline" || l == "guideline")
        && !["summary", "quick", "pocket", "patient guide", "patient version", "patient booklet"].iter().any(|w| l.contains(w))
}

struct CatalogPage {
    next: Vec<String>,
    documents: Vec<PlanRow>,
}

fn parse_catalog(src: &SourceEntry, page: &Fetched) -> Result<CatalogPage> {
    let html = Html::parse_document(&page.text());
    let base = Url::parse(&page.record.final_url)?;
    let root_selector = common::root_selector(&html, &["#tier4innerContent", "#homeinnerContent", "#innerContentWrapper"])?;
    let root = html.select(&sel(&root_selector)).next().unwrap();
    let title = common::first_text(&html, "h2.page-title, #tier4innerContent h1");
    let expected = regex::Regex::new(r"(?i)(\d+)\s+(?:evidence[- ]based\s+)?recommendations").unwrap()
        .captures(&text_of(&root)).and_then(|c| c[1].parse::<usize>().ok());
    let mut next = vec![];
    let mut pdfs = vec![];
    let mut seen = HashSet::new();
    for a in root.select(&sel("a[href]")) {
        let Some(url) = common::link(&base, a.value().attr("href").unwrap_or_default()) else { continue };
        let u = Url::parse(&url)?;
        if u.host_str() != base.host_str() { continue; }
        let p = u.path().to_ascii_lowercase();
        if p.ends_with("/cpgarchives.asp") { next.push(url); continue; }
        if !p.contains("/guidelines/") { continue; }
        let label = text_of(&a);
        if p.ends_with(".pdf") {
            // Repeated image and text links can have different labels. Prefer the explicit full-guideline label.
            let key = identity_url(&url);
            if seen.insert(key.clone()) { pdfs.push((url, label)); }
            else if full_guideline(&label) {
                if let Some(old) = pdfs.iter_mut().find(|(u, _)| identity_url(u) == key) { old.1 = label; }
            }
        } else if (p.ends_with('/') || p.ends_with(".asp") || p.ends_with(".html")) && u.query().is_none() {
            next.push(url);
        }
    }
    // A guideline topic landing (/guidelines/<area>/<topic>/index.asp) that links PDFs but none with a full-guideline
    // label is kept as an explicit unresolved row, never dropped silently.
    let topic_landing = regex::Regex::new(r"(?i)/guidelines/[^/]+/[^/]+/(index\.asp)?$").unwrap().is_match(base.path());
    let unlabelled = topic_landing && !pdfs.is_empty() && !pdfs.iter().any(|(_, label)| full_guideline(label));
    let attachments: Vec<Value> = pdfs.iter().filter(|(_, label)| !full_guideline(label))
        .map(|(url, label)| json!({"url":url,"title":label,"source_url":base.as_str(),"role":"linked_rendition_or_tool"})).collect();
    let documents = pdfs.into_iter().filter(|(_, label)| full_guideline(label)).map(|(url, label)| {
        let work_key = topic_key(&base);
        PlanRow {
            source_id: src.id.clone(), doc_key: format!("{work_key}-{}", common::short_hash(&identity_url(&url))),
            url, title: title.clone().or_else(|| Some(label.clone())), rendition: Some("pdf".into()), version: None,
            extra: json!({"work_key":work_key,"landing_url":base.as_str(),"link_label":label,
                "attachments":attachments,"catalog_fetch":page.record,"catalog_title":title,
                "expected_recommendations":expected}),
        }
    }).collect::<Vec<_>>();
    let mut documents = documents;
    if unlabelled {
        let work_key = topic_key(&base);
        documents.push(PlanRow {
            source_id: src.id.clone(), doc_key: format!("{work_key}-unresolved"), url: base.to_string(),
            title: title.clone(), rendition: None, version: None,
            extra: json!({"work_key":work_key,"landing_url":base.as_str(),"attachments":attachments,"catalog_fetch":page.record,
                "discovery_error":"VA/DoD topic page links PDFs but none carries a full-guideline label; renditions listed as attachments, none chosen by guess"}),
        });
    }
    Ok(CatalogPage { next, documents })
}

/// One bounded, numbered statement assembled from the original converted blocks.
struct PendingRecommendation {
    anchor: usize,
    label: String,
    parts: Vec<String>,
    strength: Option<String>,
    qualifier: Option<String>,
}

fn finish_recommendation(
    pending: &mut Option<PendingRecommendation>, original: &[Block],
    rows: &mut Vec<(usize, Block)>, seen: &mut HashSet<String>, warnings: &mut Vec<String>,
) {
    let Some(p) = pending.take() else { return };
    let Some(strength) = p.strength else {
        warnings.push(format!("Unparsed numbered statement {} in {}: no unambiguous explicit grade; source blocks retained.", p.label, original[p.anchor].block_id));
        return;
    };
    let text = p.parts.join("\n");
    if text.trim().is_empty() {
        warnings.push(format!("Recommendation {} has a grade but no statement; source blocks retained.",p.label));
        return;
    }
    if !seen.insert(p.label.clone()) {
        warnings.push(format!("Repeated recommendation label {} in {}; retained with distinct block ID.",p.label,original[p.anchor].block_id));
    }
    rows.push((p.anchor,Block {
        block_id:format!("{}-recommendation-{}-{}",original[p.anchor].block_id,p.label,rows.len()+1),
        kind:BlockKind::Recommendation {label:Some(p.label),strength:Some(strength),certainty:None,population:None,grading_system:None,qualifier:p.qualifier},
        text,heading_path:original[p.anchor].heading_path.clone(),page:original[p.anchor].page,
    }));
}

/// Bound each candidate before looking for its grade. The converter may place a
/// statement, list items, continuation and a standalone grade in different blocks.
fn recommendations(blocks: &mut Vec<Block>) -> (usize, Vec<String>) {
    let starts=regex::Regex::new(r"(?:^|\s)(\d{1,3}[a-z]?)\.\s+([A-Z])").unwrap();
    let grade=regex::Regex::new(r"(?s)^(.*?)\s*(?:\*\*)?\(([^|()]+)\|([^()]+)\)(?:\*\*)?\s*$").unwrap();
    let footer=regex::Regex::new(r"(?i)\s*(?:January|February|March|April|May|June|July|August|September|October|November|December)\s+(?:19|20)\d{2}\s+Page\s+\d+\s+of\s+\d+\s*$").unwrap();
    let running=regex::Regex::new(r"(?i)^<u>VA/DOD Clinical Practice Guideline[^<]*</u>$").unwrap();
    let mut in_section=false;
    let mut pending:Option<PendingRecommendation>=None;
    let mut rows:Vec<(usize,Block)>=vec![];
    let mut seen=HashSet::new();
    let mut warnings=vec![];
    for (i,b) in blocks.iter().enumerate() {
        if matches!(b.kind,BlockKind::Heading{..}) {
            let name=b.text.trim();
            if name == "Recommendation" || name == "Recommendations" {
                finish_recommendation(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);
                in_section=true;
                continue;
            }
            // Some explicit grades are rendered as headings, even after a page header.
            if in_section {
                if let Some(c)=grade.captures(name) {
                    if c[1].trim().is_empty() {
                        if let Some(p)=pending.as_mut() {
                            p.strength=Some(c[2].trim().into()); p.qualifier=Some(c[3].trim().into());
                            finish_recommendation(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);
                        } else { warnings.push(format!("Unpaired recommendation grade in {}; source block retained.",b.block_id)); }
                        continue;
                    }
                }
            }
            finish_recommendation(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);
            in_section=false;
            continue;
        }
        if !in_section || !matches!(b.kind,BlockKind::Paragraph | BlockKind::ListItem) { continue; }
        if running.is_match(b.text.trim()) { continue; }
        let clean=footer.replace(b.text.trim(), "");
        let text=clean.trim();
        if text.is_empty() { continue; }
        let markers:Vec<_>=starts.captures_iter(text).collect();
        if markers.is_empty() {
            if let Some(p)=pending.as_mut() {
                if let Some(c)=grade.captures(text) {
                    if !c[1].trim().is_empty() { p.parts.push(if matches!(b.kind,BlockKind::ListItem) {format!("- {}",c[1].trim())} else {c[1].trim().into()}); }
                    p.strength=Some(c[2].trim().into()); p.qualifier=Some(c[3].trim().into());
                    finish_recommendation(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);
                } else {
                    p.parts.push(if matches!(b.kind,BlockKind::ListItem) {format!("- {text}")} else {text.into()});
                }
            } else { warnings.push(format!("Unpaired content under Recommendation heading in {}; source block retained.",b.block_id)); }
            continue;
        }
        // The split is made before grade matching so an unfinished prior statement
        // cannot consume a subsequent statement and its grade.
        for (n,c) in markers.iter().enumerate() {
            if n==0 && c.get(0).unwrap().start()>0 {
                let prefix=text[..c.get(0).unwrap().start()].trim();
                if !prefix.is_empty() {
                    if let Some(p)=pending.as_mut() { p.parts.push(prefix.into()); }
                    else { warnings.push(format!("Unpaired prefix under Recommendation heading in {}; source block retained.",b.block_id)); }
                }
            }
            finish_recommendation(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);
            let end=markers.get(n+1).map(|next|next.get(0).unwrap().start()).unwrap_or(text.len());
            let statement=text[c.get(2).unwrap().start()..end].trim();
            let mut p=PendingRecommendation {anchor:i,label:c[1].into(),parts:vec![],strength:None,qualifier:None};
            if let Some(g)=grade.captures(statement) {
                if !g[1].trim().is_empty() { p.parts.push(g[1].trim().into()); }
                p.strength=Some(g[2].trim().into()); p.qualifier=Some(g[3].trim().into());
            } else { p.parts.push(statement.into()); }
            pending=Some(p);
            if pending.as_ref().is_some_and(|p|p.strength.is_some()) {
                finish_recommendation(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);
            }
        }
    }
    finish_recommendation(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);
    if !rows.is_empty() {
        let count=seen.len();
        // Reverse insertion keeps original blocks and source order, including
        // multiple recommendations from one original paragraph.
        for (i,b) in rows.into_iter().rev() { blocks.insert(i+1,b); }
        return (count,warnings);
    }
    // Fall back only when no detailed statements were recognized. Keep the
    // source table and never assign grades from neighboring rows.
    let tables=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).cloned().collect::<Vec<_>>();
    let number=regex::Regex::new(r"^\d+[a-z]?\.?$").unwrap();
    let mut count=0;
    for table in tables {
        let table_rows=table.text.lines().map(|line|line.trim().trim_matches('|').split('|').map(str::trim).collect::<Vec<_>>()).collect::<Vec<_>>();
        let Some(header)=table_rows.first() else {continue};
        let rec_col=header.iter().position(|h|h.to_ascii_lowercase().contains("recommendation"));
        let strength_col=header.iter().position(|h|h.to_ascii_lowercase().contains("strength"));
        let number_col=header.iter().position(|h|matches!(h.to_ascii_lowercase().as_str(),"#"|"no."|"number"|"no"));
        let (Some(rec_col),Some(strength_col),Some(number_col))=(rec_col,strength_col,number_col) else {continue};
        let mut statements=vec![];
        for (n,row) in table_rows.iter().enumerate().skip(1) {
            let (Some(text),Some(strength),Some(label))=(row.get(rec_col),row.get(strength_col),row.get(number_col)) else {continue};
            if !number.is_match(label) || text.is_empty() {continue;}
            statements.push(Block {block_id:format!("{}-recommendation-{n}",table.block_id),kind:BlockKind::Recommendation {
                label:Some((*label).into()),strength:(!strength.is_empty()).then(||(*strength).into()),certainty:None,population:None,grading_system:None,qualifier:None},
                text:(*text).into(),heading_path:table.heading_path.clone(),page:table.page});
        }
        count+=statements.len();
        common::insert_table_recommendations(blocks,&table.block_id,statements).expect("existing table");
    }
    (count,warnings)
}

fn pdf_metadata(doc: &mut GuidelineDoc) {
    // The VA alternative-text appendix explicitly introduces Module A/B subsections, but the PDF converter
    // gives the appendix and modules identical heading levels. Recover this narrowly evidenced nesting.
    let mut algorithm_appendix: Option<(String, Vec<String>)> = None;
    let mut module: Option<String> = None;
    for block in &mut doc.blocks {
        if matches!(block.kind, BlockKind::Heading { .. }) && block.text.starts_with("Appendix ") {
            algorithm_appendix = block.text.contains("Alternative Text Descriptions of Algorithm")
                .then(|| (block.text.clone(), block.heading_path.clone()));
            module = None;
        } else if let Some((appendix, ancestors)) = &algorithm_appendix {
            if matches!(block.kind, BlockKind::Heading { .. }) && block.text.starts_with("Module ") {
                block.kind = BlockKind::Heading { level: 3 };
                block.heading_path = ancestors.iter().cloned().chain([appendix.clone()]).collect();
                module = Some(block.text.clone());
            } else if let Some(name) = &module {
                block.heading_path = ancestors.iter().cloned().chain([appendix.clone(), name.clone()]).collect();
            }
        }
    }
    let front = doc.blocks.iter().take(40).map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n");
    let version = regex::Regex::new(r"(?i)\bVersion\s+([0-9]+(?:\.[0-9]+)*)").unwrap();
    doc.version = version.captures(&front).map(|c| c[1].to_string());
    // A cover date is observed as a cover date, not relabelled as publication or review.
    let year = regex::Regex::new(r"^(?:19|20)\d{2}$").unwrap();
    let cover_version = regex::Regex::new(r"(?i)^Version\s+[0-9.]+\s*[–—-]\s*((?:19|20)\d{2})$").unwrap();
    if let Some(raw) = doc.blocks.iter().take(30).map(|b| b.text.trim()).find(|t| year.is_match(t)) {
        doc.date_observations.push(common::date("Cover date", raw, &doc.url));
    } else if let Some(raw) = doc.blocks.iter().take(30).find_map(|b| cover_version.captures(&b.text).map(|c| c[1].to_string())) {
        doc.date_observations.push(common::date("Cover version year", &raw, &doc.url));
    }
    for issuer in ["Department of Veterans Affairs", "Department of Defense"] {
        if front.contains(issuer) { doc.issuers.push(issuer.into()); }
    }
    if front.contains("Clinical Practice Guideline") || front.contains("CLINICAL PRACTICE GUIDELINE") {
        doc.item_type = Some("clinical_practice_guideline".into());
    }
    // No blanket public-domain assertion: linked tools and third-party material can carry other rights.
    for b in &doc.blocks {
        if b.text.to_ascii_lowercase().contains("copyright") && b.text.len() < 2000 {
            doc.rights.note = Some(b.text.clone());
            doc.rights.evidence_urls.push(doc.url.clone());
            break;
        }
    }
}

#[async_trait::async_trait]
impl Adapter for VaDod {
    async fn discover(&self, src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) { return Ok(vec![]); }
        let root = src.catalog_url.clone().context("VA/DoD catalog_url missing")?;
        let mut queue = VecDeque::from([root]);
        let mut visited = HashSet::new();
        let mut documents = HashSet::new();
        let mut out = vec![];
        while let Some(url) = queue.pop_front() {
            if !visited.insert(identity_url(&url)) { continue; }
            if visited.len() > 250 { bail!("VA/DoD catalog exceeded 250 pages; inventory is incomplete"); }
            let page = f.get_ok(&url).await?;
            let parsed = parse_catalog(src, &page)?;
            for mut row in parsed.documents {
                if !documents.insert(identity_url(&row.url)) { continue; }
                if row.extra["discovery_error"].is_string() {
                    out.push(row);
                    if limit.is_some_and(|l| out.len() >= l) { return Ok(out); }
                    continue;
                }
                // Pin the actual PDF revision. A URL can be overwritten, and VA has reused one ETag for
                // different PDFs; neither the URL nor ETag is a version identifier.
                match f.get_ok(&row.url).await {
                    Ok(pdf) if pdf.is_pdf() => {
                        row.extra["pdf_sha256"] = json!(pdf.record.sha256);
                        row.doc_key = format!("{}-{}", row.extra["work_key"].as_str().unwrap_or("guideline"), &pdf.record.sha256[..12]);
                    }
                    Ok(_) => row.extra["discovery_error"] = json!("full-guideline link did not return PDF bytes"),
                    Err(e) => row.extra["discovery_error"] = json!(format!("{e:#}")),
                }
                out.push(row);
                if limit.is_some_and(|l| out.len() >= l) { return Ok(out); }
            }
            queue.extend(parsed.next);
        }
        if out.is_empty() { bail!("VA/DoD catalog yielded no explicitly labelled full-guideline PDFs"); }
        Ok(out)
    }

    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(error) = row.extra["discovery_error"].as_str() { bail!("VA/DoD discovery: {error}"); }
        let page = f.get_ok(&row.url).await?;
        if !page.is_pdf() { bail!("VA/DoD full guideline did not return PDF bytes: {}", row.url); }
        if row.extra["pdf_sha256"].as_str().is_some_and(|s| s != page.record.sha256) {
            bail!("VA/DoD PDF changed after planning; re-plan this revision");
        }
        let (mut blocks, mut warnings, _) = blocks_for(&page, None, 30).await?;
        if blocks.is_empty() { bail!("VA/DoD PDF has no extracted text"); }
        let (recs, recommendation_warnings) = recommendations(&mut blocks);
        warnings.extend(recommendation_warnings);
        if let Some(expected) = row.extra["expected_recommendations"].as_u64() {
            if recs != expected as usize { warnings.push(format!("Recommendation coverage incomplete: landing page declares {expected}; extracted {recs}.")); }
        }
        if recs == 0 { warnings.push("No structured recommendation rows recognized; inspect PDF table layout before clinical use.".into()); }
        warnings.push("PDF text/table conversion may merge cells or lose hyphens; recommendation count alone does not establish verbatim fidelity. Summary and detailed wording can differ; detailed source wording is retained without normalisation.".into());
        warnings.push("PDF diagram images and branch topology are not captured by the shared text converter; source PDF remains the authoritative rendition.".into());
        let mut fetches = vec![];
        if let Ok(record) = serde_json::from_value::<FetchRecord>(row.extra["catalog_fetch"].clone()) { fetches.push(record); }
        fetches.push(page.record.clone());
        let landing = row.extra["landing_url"].as_str();
        let mut doc = GuidelineDoc {
            source_id: src.id.clone(), doc_key: row.doc_key.clone(),
            work_key: row.extra["work_key"].as_str().map(String::from),
            title: row.title.clone().unwrap_or_else(|| row.url.clone()), url: row.url.clone(),
            status: "unknown".into(), rights: common::unknown_rights(), blocks, warnings, fetches,
            attachments: row.extra["attachments"].as_array().cloned().unwrap_or_default(),
            search_metadata: json!({"catalog_title":row.extra["catalog_title"],"catalog_url":landing,"link_label":row.extra["link_label"]}),
            ..Default::default()
        };
        if let Some(landing) = landing {
            doc.relations.push(Relation { rel: "related".into(), target: landing.into(), evidence_url: Some(landing.into()), scope: Some("Guideline topic landing page".into()) });
        }
        pdf_metadata(&mut doc);
        Ok(doc)
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const HOME: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/va_dod/home.html"));
    const TOPIC: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/va_dod/diabetes.html"));
    const PDF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/va_dod/recommendation.pdf"));
    const CONVERTED: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/va_dod/diabetes-blocks.json"));
    const LANDING: &str = "https://www.healthquality.va.gov/HEALTHQUALITY/guidelines/CD/diabetes/index.asp";
    const PDF_URL: &str = "https://www.healthquality.va.gov/HEALTHQUALITY/guidelines/CD/diabetes/VADOD-Diabetes-CPG_Final_508.pdf";

    fn source(catalog_url: &str) -> SourceEntry {
        serde_json::from_value(json!({"id":"va_dod","access":"public","enabled":true,"catalog_url":catalog_url,"adapter":"va_dod"})).unwrap()
    }

    fn saved(url: &str, body: &[u8]) -> Fetched {
        Fetched { body: body.to_vec(), record: FetchRecord { url:url.into(), final_url:url.into(), status:200,
            content_type:Some("text/html".into()), sha256:crate::common::sha256_hex(body), ..Default::default() } }
    }

    #[test]
    fn actual_catalog_and_topic_roles() {
        let src = source("https://www.healthquality.va.gov/");
        let catalog = parse_catalog(&src, &saved(src.catalog_url.as_deref().unwrap(), HOME)).unwrap();
        let topics: HashSet<String> = catalog.next.iter().filter(|u| u.to_ascii_lowercase().contains("/guidelines/"))
            .map(|u| identity_url(u)).collect();
        assert_eq!(topics.len(), 28);
        assert!(catalog.next.iter().any(|u| u.ends_with("/HEALTHQUALITY/CPGArchives.asp")));
        let topic = parse_catalog(&src, &saved(LANDING, TOPIC)).unwrap();
        assert_eq!(topic.documents.len(), 1);
        let row = &topic.documents[0];
        assert_eq!(row.url, PDF_URL);
        assert_eq!(row.title.as_deref(), Some("Management of Type 2 Diabetes Mellitus (2023)"));
        assert_eq!(row.extra["expected_recommendations"], 26);
        assert!(row.extra["attachments"].as_array().unwrap().iter().any(|a| a["title"].as_str().is_some_and(|t| t.contains("Provider Summary"))));
        assert!(row.extra["attachments"].as_array().unwrap().iter().any(|a| a["title"].as_str().is_some_and(|t| t.contains("2026"))));
        assert!(row.version.is_none(), "patient-tool dates are not guideline versions");
    }

    #[test]
    fn full_text_and_patients_at_risk_full_guideline_labels_are_planned() {
        // first-pass plan had 26 rows for 28 topics: MDD ("MDD Full Text (2022)") and suicide risk
        // ("Patients at Risk for Suicide - Full Guideline (2024)") were dropped without a row.
        let src = source("https://www.healthquality.va.gov/");
        let mdd_url = "https://www.healthquality.va.gov/HEALTHQUALITY/guidelines/MH/mdd/index.asp";
        let srb_url = "https://www.healthquality.va.gov/HEALTHQUALITY/guidelines/MH/srb/index.asp";
        let mdd = parse_catalog(&src, &saved(mdd_url, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/va_dod/mdd.html")))).unwrap();
        assert_eq!(mdd.documents.len(), 1);
        assert!(mdd.documents[0].url.ends_with("/MH/mdd/VADODMDDCPGFinal508.pdf"));
        assert!(mdd.documents[0].extra["discovery_error"].is_null());
        let srb = parse_catalog(&src, &saved(srb_url, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/va_dod/srb.html")))).unwrap();
        assert_eq!(srb.documents.len(), 1);
        assert!(srb.documents[0].url.ends_with("/MH/srb/VADOD-CPG-Suicide-Risk-Full-CPG-2024_Final_508.pdf"));
        assert!(srb.documents[0].extra["attachments"].as_array().unwrap().iter().any(|a| a["title"].as_str().unwrap().contains("Patient Summary")));
        for summary in ["Patient Summary (2022)", "Asthma Pocket Card (2025)", "Asthma Provider Summary (2025)", "Quick Reference Guide"] {
            assert!(!full_guideline(summary), "{summary}");
        }
        // a topic landing whose PDFs carry no full-guideline label becomes an explicit unresolved row
        let relabelled = String::from_utf8_lossy(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/va_dod/mdd.html"))).replace("MDD Full Text (2022)", "MDD (2022)");
        let gap = parse_catalog(&src, &saved(mdd_url, relabelled.as_bytes())).unwrap();
        assert_eq!(gap.documents.len(), 1);
        assert!(gap.documents[0].extra["discovery_error"].as_str().unwrap().contains("none carries a full-guideline label"));
        // the diabetes topic is unchanged
        let topic = parse_catalog(&src, &saved(LANDING, TOPIC)).unwrap();
        assert_eq!(topic.documents.len(), 1);
        assert_eq!(topic.documents[0].url, PDF_URL);
    }

    #[test]
    fn real_converted_statements_cover_and_discrepant_strength() {
        let mut doc: GuidelineDoc = serde_json::from_str(CONVERTED).unwrap();
        let (count, warnings) = recommendations(&mut doc.blocks);
        assert_eq!(count, 26);
        assert!(warnings.is_empty());
        pdf_metadata(&mut doc);
        assert_eq!(doc.version.as_deref(), Some("6.0"));
        assert!(doc.date_observations.iter().any(|d| d.label == "Cover version year" && d.normalized.as_deref() == Some("2023")));
        assert!(doc.published.is_none());
        assert_eq!(doc.issuers, ["Department of Veterans Affairs", "Department of Defense"]);
        assert_eq!(doc.status, "unknown");
        assert_eq!(doc.rights.status, "unknown");
        assert_eq!(doc.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Table { .. })).count(), 4);
        let recs: Vec<&Block> = doc.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Recommendation { .. })).collect();
        for (index, block) in recs.iter().enumerate() {
            match &block.kind {
                BlockKind::Recommendation { label, certainty, .. } => {
                    assert_eq!(label.as_deref(), Some((index + 1).to_string()).as_deref());
                    assert!(certainty.is_none());
                }
                _ => unreachable!(),
            }
            assert_eq!(block.heading_path.last().map(String::as_str), Some("Recommendation"));
            assert!(!block.text.contains("Page 55 of 165"));
        }
        assert!(recs[0].text.contains("walking 8-9 miles a week"));
        assert!(matches!(&recs[0].kind, BlockKind::Recommendation { strength:Some(s), .. } if s == "Weak for"));
        assert!(matches!(&recs[25].kind, BlockKind::Recommendation { strength:Some(s), .. } if s == "Neither for not against"));
        assert!(doc.blocks.iter().any(|b| b.text.contains("The algorithm begins with Box 1")
            && b.heading_path.iter().any(|h| h.contains("Appendix I"))
            && b.heading_path.iter().any(|h| h == "Module A: T2DM Management")));
        assert!(doc.blocks.iter().all(|b| !matches!(b.kind, BlockKind::Figure { src_url:Some(_), .. })), "fixture must not pretend missing PDF figures were recovered");
    }

    #[tokio::test]
    async fn offline_discovery_and_real_pdf_excerpt_are_honest_about_missing_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(LANDING, TOPIC, "text/html");
        f.seed_cache_for_tests(PDF_URL, PDF, "application/pdf");
        let src = source(LANDING);
        assert!(VaDod.discover(&src, &f, Some(0)).await.unwrap().is_empty());
        let plan = VaDod.discover(&src, &f, Some(1)).await.unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].doc_key, "guidelines-cd-diabetes-4f81796eff6f");
        let doc = VaDod.extract(&src, &f, &plan[0]).await.unwrap();
        assert_eq!(doc.work_key.as_deref(), Some("guidelines-cd-diabetes"));
        assert_eq!(doc.title, "Management of Type 2 Diabetes Mellitus (2023)");
        assert_eq!(doc.status, "unknown");
        assert_eq!(doc.rights.status, "unknown");
        assert_eq!(doc.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Recommendation { .. })).count(), 1);
        assert!(doc.warnings.iter().any(|w| w.contains("declares 26; extracted 1")));
        assert!(doc.warnings.iter().any(|w| w.contains("diagram images")));
        assert!(doc.fetches.iter().all(|f| f.status == 200));
    }
    #[test]
    fn live_multiblock_recommendations_preserve_criteria_and_bound_grades() {
        fn run(json: &str) -> (Vec<Block>, Vec<String>) {
            let mut blocks:Vec<Block>=serde_json::from_str(json).unwrap();
            let original=blocks.len();
            let (count,warnings)=recommendations(&mut blocks);
            let recs:Vec<_>=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).cloned().collect();
            assert_eq!(blocks.len(),original+recs.len());
            assert_eq!(count,recs.len());
            (recs,warnings)
        }
        fn rec<'a>(v:&'a [Block],n:&str) -> &'a Block {
            v.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l==n)).unwrap()
        }
        fn strength<'a>(b:&'a Block)->Option<&'a str> {
            if let BlockKind::Recommendation{strength,..}=&b.kind {strength.as_deref()} else {None}
        }
        let (a,aw)=run(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/va_dod/asthma-blocks.json")));
        assert_eq!(a.len(),3);
        assert_eq!(strength(rec(&a,"3")),Some("Weak for"));
        assert!(rec(&a,"3").text.contains("written asthma action plan"));
        assert!(!rec(&a,"3").text.contains("Page 35"));
        assert!(a.iter().all(|b|!matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="5")));
        assert!(aw.iter().any(|w|w.contains("statement 5")));
        assert_eq!(strength(rec(&a,"6")),Some("Weak for"));
        assert!(rec(&a,"6").text.contains("formoterol"));
        assert_eq!(strength(rec(&a,"11")),Some("Weak for"));
        assert!(rec(&a,"11").text.contains("stepping down (not discontinuing)"));
        assert!(!rec(&a,"11").text.contains("Page 46") && !rec(&a,"11").text.contains("VA/DOD Clinical"));
        let (i,iw)=run(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/va_dod/insomnia-blocks.json")));
        assert!(iw.is_empty()); assert_eq!(i.len(),2);
        assert_eq!(strength(rec(&i,"9")),Some("Weak for"));
        for drug in ["Daridorexant","Doxepin","Eszopiclone","Lemborexant","Suvorexant","Zaleplon","Zolpidem"] {assert!(rec(&i,"9").text.contains(drug));}
        assert_eq!(strength(rec(&i,"17")),Some("Strong for"));
        for treatment in ["Mandibular advancement devices","Positive airway pressure","Referral for surgical evaluation"] {assert!(rec(&i,"17").text.contains(treatment));}
        let (c,cw)=run(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/va_dod/ckd-blocks.json")));
        assert!(cw.is_empty()); assert_eq!(c.len(),3);
        assert_eq!(strength(rec(&c,"1")),Some("Weak for"));
        for factor in ["Age over 60 years","Diabetes","Hypertension","Cardiovascular disease, including heart failure"] {assert!(rec(&c,"1").text.contains(factor));}
        assert_eq!(strength(rec(&c,"15")),Some("Strong for"));
        for part in ["Type 2 diabetes","Albuminuria (UACR >200 mg/g)","Heart failure","until start of dialysis"] {assert!(rec(&c,"15").text.contains(part));}
        assert_eq!(strength(rec(&c,"18")),Some("Weak for"));
        for part in ["Type 2 diabetes","Albuminuria >30 mg/g","eGFR ≥25","Potassium <4.8","slowing progression"] {assert!(rec(&c,"18").text.contains(part));}
    }

}
