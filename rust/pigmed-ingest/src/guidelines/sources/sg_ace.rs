//! ACE repository: advertised sitemap inventory and source HTML recommendation pages.
//! The linked full ACG and supporting PDFs are separate, uninspected assets.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, GuidelineDoc, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{anyhow, bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::json;
use std::collections::{BTreeSet, HashSet, VecDeque};
use url::Url;

pub struct SgAce;
const NS: &str = "http://www.sitemaps.org/schemas/sitemap/0.9";

fn locations(xml: &str) -> Result<(bool, Vec<String>)> {
    let doc = roxmltree::Document::parse(xml).context("invalid ACE sitemap XML")?;
    let root = doc.root_element();
    if root.tag_name().namespace() != Some(NS) { bail!("unexpected sitemap namespace"); }
    let index = match root.tag_name().name() { "sitemapindex" => true, "urlset" => false, _ => bail!("unexpected sitemap root") };
    let child = if index { "sitemap" } else { "url" };
    let mut found = Vec::new();
    for entry in root.children().filter(|n| n.is_element()) {
        if entry.tag_name().namespace() != Some(NS) || entry.tag_name().name() != child { bail!("unexpected sitemap entry"); }
        let locs: Vec<_> = entry.children().filter(|n| n.is_element() && n.tag_name().namespace() == Some(NS) && n.tag_name().name() == "loc").collect();
        if locs.len() != 1 { bail!("sitemap entry missing or duplicating loc"); }
        found.push(locs[0].text().unwrap_or_default().trim().to_string());
    }
    Ok((index, found))
}

// Use the same clinical root for extraction and revision identity.
fn clinical_html(html: &str) -> Result<String> {
    let doc = Html::parse_document(html);
    let root = doc.select(&sel("main#main-content")).next().ok_or_else(|| anyhow!("ACE content root missing"))?;
    let mut clean = root.html();
    for node in root.select(&sel("div[role='navigation'], nav, script, style")) { clean = clean.replace(&node.html(), ""); }
    for node in root.select(&sel("details")) {
        let label = node.select(&sel("summary")).next().map(|s| text_of(&s)).unwrap_or_default();
        if label.trim() == "Feedback" || label.trim() == "Related ACGs" { clean = clean.replace(&node.html(), ""); }
    }
    Ok(clean)
}
fn clinical_sha(html: &str) -> Result<String> { Ok(crate::common::sha256_hex(clinical_html(html)?.as_bytes())) }

fn title_and_date(html: &str, url: &str) -> Result<(String, Option<crate::guidelines::doc::DateObservation>)> {
    let doc = Html::parse_document(html);
    let root = doc.select(&sel("main#main-content")).next().ok_or_else(|| anyhow!("ACE content root missing"))?;
    let title = root.select(&sel("h1")).next().map(|h| text_of(&h)).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("ACE title missing"))?;
    let date = root.select(&sel("p")).map(|p| text_of(&p)).find_map(|t| {
        t.strip_prefix("Published on ").map(|d| ("Published on", d.trim().to_string()))
            .or_else(|| t.strip_prefix("Published ").map(|d| ("Published", d.trim().to_string())))
    });
    let observed = date.map(|(label, raw)| {
        let mut o = common::date(label, &raw, url);
        // Publisher uses day-month-year; the shared date helper accepts month-day-year.
        let re = regex::Regex::new(r"^(\d{1,2})\s+([A-Za-z]+)\s+(\d{4})$").unwrap();
        if let Some(c) = re.captures(&raw) { o.normalized = common::date(label, &format!("{} {}, {}", &c[2], &c[1], &c[3]), url).normalized; }
        o
    });
    Ok((title,observed))
}

/// Follow only an explicitly labelled web rendition in the publisher's ACG namespace.
fn web_link(html: &str, base: &Url) -> Result<Option<String>> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel("main#main-content")).next().context("ACE content root missing")?;
    let Some(heading)=root.select(&sel("h2")).find(|h|text_of(h).to_lowercase().contains("web version")) else {return Ok(None)};
    let container=heading.next_siblings().filter_map(ElementRef::wrap).next().context("ACE web link container missing")?;
    let links:Vec<_>=container.select(&sel("a[href]")).collect();
    if links.len()!=1 {bail!("ACE web rendition link is missing or ambiguous");}
    let url=links[0].value().attr("href").and_then(|h|common::link(base,h)).context("invalid ACE web rendition link")?;
    let u=Url::parse(&url)?;
    if u.origin()!=base.origin() || !u.path().starts_with("/healthcare-professionals/acg/") || u.query().is_some() || u.fragment().is_some() {
        bail!("ACE web rendition link outside permitted origin or namespace: {url}");
    }
    if u.path().trim_end_matches('/')==base.path().trim_end_matches('/') {bail!("ACE web rendition links back to summary");}
    Ok(Some(url))
}

/// Fetcher follows redirects first; reject any response whose recorded chain left
/// the ACE origin, or looped. This cannot prevent an external redirect request.
fn verify_web_redirects(record:&crate::guidelines::doc::FetchRecord, base:&Url)->Result<()> {
    let mut seen=HashSet::new();
    for hop in record.redirects.iter().chain(std::iter::once(&record.final_url)) {
        let u=Url::parse(hop)?;
        if u.origin()!=base.origin() {bail!("ACE web rendition redirect left origin: {hop}");}
        if !seen.insert(hop.clone()) {bail!("ACE web rendition redirect cycle: {hop}");}
    }
    let end=Url::parse(&record.final_url)?;
    if !end.path().starts_with("/healthcare-professionals/acg/") {bail!("ACE web rendition ended outside the ACG namespace");}
    Ok(())
}
async fn fetch_web(f:&Fetcher, base:&Url, url:&str) -> Result<crate::guidelines::fetch::Fetched> {
    let page=f.get_ok(url).await?;
    verify_web_redirects(&page.record,base)?;
    let final_url=Url::parse(&page.record.final_url)?;
    if !page.is_html() {bail!("ACE web rendition did not return HTML");}
    if web_link(&page.text(),&final_url)?.is_some() {bail!("ACE web rendition chains to another web rendition; manual review required");}
    Ok(page)
}

fn snapshot_sha(summary_sha:&str, web:Option<(&str,&str)>)->String {
    match web {Some((url,sha))=>crate::common::sha256_hex(format!("{summary_sha}\n{url}\n{sha}").as_bytes()), None=>summary_sha.into()}
}

fn web_update(html:&str, url:&str)->Option<crate::guidelines::doc::DateObservation> {
    let parsed=Html::parse_document(html);
    let root=parsed.select(&sel("main#main-content")).next()?;
    let re=regex::Regex::new(r"^Last updated on (\d{1,2})\s+([A-Za-z]+)\s+(\d{4})$").unwrap();
    let value=root.select(&sel("div")).map(|e|text_of(&e)).find(|t|re.is_match(t))?;
    let c=re.captures(&value)?;
    let raw=format!("{} {} {}",&c[1],&c[2],&c[3]);
    let mut obs=common::date("Last updated on",&raw,url);
    obs.normalized=common::date("Last updated on",&format!("{} {}, {}",&c[2],&c[1],&c[3]),url).normalized;
    Some(obs)
}

fn role(label: &str, section: &str) -> &'static str {
    let l = label.to_lowercase(); let h = section.to_lowercase();
    if l.contains("reference") { "references" }
    else if l.contains("composition") || l.contains("expert group") { "expert_group" }
    else if l.contains("etr") || h.contains("etr framework") { "evidence_to_recommendation" }
    else if h.contains("download the acg") { "full_acg" }
    else { "linked_pdf" }
}

fn extract_page(src: &SourceEntry, row: &PlanRow, page: &crate::guidelines::fetch::Fetched) -> Result<GuidelineDoc> {
    let html = page.text(); let base = Url::parse(&page.record.final_url)?;
    let parsed = Html::parse_document(&html);
    let root = parsed.select(&sel("main#main-content")).next().ok_or_else(|| anyhow!("ACE content root missing"))?;
    let (title, date) = title_and_date(&html, base.as_str())?;
    let clean = clinical_html(&html)?;
    let (mut blocks, mut warnings) = blocks_from_html(&clean, Some(&base), Some("main#main-content"));
    let num = regex::Regex::new(r"^(\d+[a-z]?)\s*\.\s*(.+)$").unwrap();
    // Some pages render labels through an HTML ordered list rather than text.
    // Retain the direct list items, including any nested condition list, as one
    // recommendation each. Source order and the ol start value supply the labels.
    let mut ordered = Vec::new();
    if let Some(h) = root.select(&sel("h2")).find(|h| text_of(h).trim().eq_ignore_ascii_case("ACG recommendations")) {
        if let Some(ol) = h.next_siblings().filter_map(ElementRef::wrap).next().filter(|e|e.value().name()=="ol") {
            let mut n = ol.value().attr("start").and_then(|s|s.parse::<usize>().ok()).unwrap_or(1);
            for li in ol.children().filter_map(ElementRef::wrap).filter(|e|e.value().name()=="li") {
                if let Some(v) = li.value().attr("value").and_then(|s|s.parse::<usize>().ok()) { n=v; }
                let first = li.children().filter_map(ElementRef::wrap).find(|e|e.value().name()=="p").map(|p|text_of(&p));
                if let Some(first) = first { ordered.push((n.to_string(),first,text_of(&li))); }
                else { warnings.push(format!("Ordered ACG recommendation {n} has no direct paragraph; inspect source.")); }
                n += 1;
            }
        }
    }
    let mut current: Option<usize> = None;
    let mut count = 0;
    for i in 0..blocks.len() {
        if !blocks[i].heading_path.iter().any(|h| h.eq_ignore_ascii_case("ACG recommendations")) { current = None; continue; }
        if !matches!(blocks[i].kind, BlockKind::Paragraph | BlockKind::ListItem) { continue; }
        if !ordered.is_empty() {
            if let Some((label,_,text))=ordered.iter().find(|(_,first,_)| *first==blocks[i].text) {
                blocks[i].kind=BlockKind::Recommendation { label:Some(label.clone()), strength:None, certainty:None, population:None, grading_system:None, qualifier:None };
                blocks[i].text=text.clone(); count+=1;
            }
            continue;
        }
        if let Some(c) = num.captures(&blocks[i].text) {
            let label = c[1].to_string(); let text = c[2].to_string();
            blocks[i].kind = BlockKind::Recommendation {label:Some(label), strength:None, certainty:None, population:None, grading_system:None, qualifier:None};
            blocks[i].text = text; current=Some(i); count += 1;
        } else if let Some(j) = current {
            // Bullets and lettered subpoints are part of the immediately preceding recommendation.
            // Keep their original blocks in place as well, so structure is never discarded.
            let tail = blocks[i].text.clone(); blocks[j].text.push_str("\n"); blocks[j].text.push_str(&tail);
            warnings.push(format!("Unnumbered content {} retained with preceding recommendation; verify source association.", blocks[i].block_id));
        } else { warnings.push(format!("Unclassified content in ACG recommendations: {}", blocks[i].block_id)); }
    }
    if !ordered.is_empty() && count != ordered.len() { warnings.push(format!("ACG ordered list contains {} items but {} were mapped to blocks.",ordered.len(),count)); }
    if count == 0 { warnings.push("No numbered ACG recommendations recognized; this repository record may be an external guideline or have another layout.".into()); }
    let mut attachments = Vec::new(); let mut section = String::new(); let mut seen=HashSet::new();
    for el in root.select(&sel("h1, h2, h3, a[href]")) {
        if matches!(el.value().name(), "h1" | "h2" | "h3") { section = text_of(&el); continue; }
        let Some(url) = el.value().attr("href").and_then(|h| common::link(&base,h)) else { continue };
        let label=text_of(&el);
        let is_pdf=Url::parse(&url).ok().is_some_and(|u|u.path().to_lowercase().ends_with(".pdf"));
        let is_web=section.to_lowercase().contains("web version");
        let is_etr=section.to_lowercase().contains("etr framework");
        if !(is_pdf || is_web || is_etr) || !seen.insert(url.clone()) { continue; }
        let asset_role=if is_web {"web_rendition"} else if is_etr {"evidence_to_recommendation"} else {role(&label,&section)};
        attachments.push(json!({"url":url,"title":label,"role":asset_role,"source_url":base.as_str(),"rights":"unknown; asset not inspected"}));
    }
    warnings.push("This HTML record contains recommendations and links to separate full guideline and evidence PDFs; linked PDF content, tables, figures and diagram branches were not acquired or validated.".into());
    warnings.push("Repository sitemap covers currently advertised leaves; archived or omitted records and external guideline rights are not established.".into());
    let published = date.as_ref().and_then(|d|d.normalized.clone());
    Ok(GuidelineDoc { source_id:src.id.clone(), doc_key:row.doc_key.clone(), work_key:row.extra["work_key"].as_str().map(String::from),
        title,url:row.url.clone(), item_type:Some("recommendation_page".into()), status:"unknown".into(),
        version:row.version.clone(), published, date_observations:date.into_iter().collect(),
        rights:Rights {status:"unknown".into(), ..Default::default()}, language:common::page_language(&parsed),
        blocks,attachments,fetches:vec![page.record.clone()],warnings,..Default::default() })
}

#[async_trait::async_trait]
impl Adapter for SgAce {
    async fn discover(&self, src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) { return Ok(vec![]); }
        let catalog=Url::parse(src.catalog_url.as_deref().context("ACE catalog URL missing")?)?;
        let robots=catalog.join("/robots.txt")?; let body=f.get_ok(robots.as_str()).await?;
        let advertised: Vec<_> = body.text().lines().filter_map(|l| l.split_once(':')).filter(|(k,_)| k.trim().eq_ignore_ascii_case("sitemap")).map(|(_,v)|v.trim().to_string()).collect();
        if advertised.is_empty() { bail!("ACE robots does not advertise a sitemap"); }
        let mut queue:VecDeque<_>=advertised.into(); let mut visited=HashSet::new(); let mut leaves=BTreeSet::new();
        let prefix=format!("{}/",catalog.path().trim_end_matches('/'));
        while let Some(url)=queue.pop_front() {
            if !visited.insert(url.clone()) { continue; }
            if visited.len()>100 { bail!("ACE sitemap limit exceeded"); }
            let u=Url::parse(&url)?;
            if u.scheme()!=catalog.scheme() || u.host_str()!=catalog.host_str() { bail!("cross-host ACE sitemap: {url}"); }
            let sitemap=f.get_ok(&url).await?; let (index, locs)=locations(&sitemap.text())?;
            for loc in locs {
                let u=Url::parse(&loc).context("invalid sitemap location")?;
                if index { queue.push_back(loc); }
                else if u.scheme()==catalog.scheme() && u.host_str()==catalog.host_str() && u.query().is_none() && u.fragment().is_none() && u.path().starts_with(&prefix) {
                    let rest=u.path()[prefix.len()..].trim_matches('/');
                    if !rest.is_empty() && !rest.contains('/') { leaves.insert(loc); }
                }
            }
        }
        if leaves.is_empty() { bail!("ACE sitemap contained no repository leaf pages"); }
        let mut out=Vec::new();
        for url in leaves {
            let page=f.get_ok(&url).await?;
            let (title,_date)=title_and_date(&page.text(), &url)?;
            let work=Url::parse(&url)?.path_segments().and_then(|s|s.filter(|v|!v.is_empty()).last()).context("ACE topic slug missing")?.to_string();
            let base=Url::parse(&page.record.final_url)?;
            if base.origin()!=catalog.origin() {bail!("ACE summary redirected outside catalog origin");}
            let summary_sha=clinical_sha(&page.text())?;
            let web_url=web_link(&page.text(),&base)?;
            let web_sha=if let Some(u)=&web_url {Some(clinical_sha(&fetch_web(f,&base,u).await?.text())?)} else {None};
            let hash=snapshot_sha(&summary_sha,web_url.as_deref().zip(web_sha.as_deref()));
            out.push(PlanRow { source_id:src.id.clone(), doc_key:format!("{}-{}",work,&hash[..12]), url, title:Some(title),
                rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"clinical_sha256":summary_sha,"web_url":web_url,"web_sha256":web_sha}) });
            if limit.is_some_and(|n|out.len()>=n) { break; }
        }
        Ok(out)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        let page=f.get_ok(&row.url).await?;
        if let Some(expected) = row.extra["clinical_sha256"].as_str() { if expected != clinical_sha(&page.text())? { bail!("ACE clinical content changed since planning; re-plan"); } }
        let base=Url::parse(&page.record.final_url)?;
        let planned_origin=Url::parse(&row.url)?;
        if base.origin()!=planned_origin.origin() {bail!("ACE summary redirected outside source origin");}
        let web_url=web_link(&page.text(),&base)?;
        if row.extra.get("web_url").is_some() && row.extra["web_url"].as_str()!=web_url.as_deref() {bail!("ACE web rendition link changed since planning; re-plan");}
        let mut doc=extract_page(src,row,&page)?;
        if let Some(url)=web_url {
            let web=fetch_web(f,&base,&url).await?;
            if let Some(expected)=row.extra["web_sha256"].as_str() {
                if expected!=clinical_sha(&web.text())? {bail!("ACE web rendition changed since planning; re-plan");}
            }
            let web_base=Url::parse(&web.record.final_url)?;
            let clean=clinical_html(&web.text())?;
            let (mut web_blocks, extra_warnings)=blocks_from_html(&clean,Some(&web_base),Some("main#main-content"));
            if web_blocks.is_empty() {bail!("ACE web rendition has no clinical blocks");}
            // Prefix both IDs and heading contexts to preserve source attribution and avoid collisions.
            for block in &mut doc.blocks {block.block_id=format!("summary-{}",block.block_id);block.heading_path.insert(0,"Repository recommendation page".into());}
            for block in &mut web_blocks {block.block_id=format!("web-{}",block.block_id);block.heading_path.insert(0,"Full web rendition".into());}
            doc.blocks.push(Block {block_id:"web-rendition-heading".into(),kind:BlockKind::Heading{level:1},text:"Full web rendition".into(),heading_path:vec![],page:None});
            doc.blocks.extend(web_blocks); doc.warnings.extend(extra_warnings);
            if let Some(date)=web_update(&web.text(),web_base.as_str()) {doc.date_observations.push(date);}
            doc.search_metadata=json!({"recommendation_source_url":row.url,"clinical_body_source_url":web.record.final_url});
            doc.fetches.push(web.record);
            for asset in &mut doc.attachments {if asset["url"].as_str()==Some(url.as_str()) {asset["acquired_as"]=json!("web clinical body");}}
            doc.warnings.push("Web rendition is retained as a separate source-labelled body; recommendations above come from the repository page and may differ by date. Linked PDFs and figure image bytes were not acquired; asset rights require review.".into());
        }
        // Terms apply to the ACE-hosted page. Linked resources keep their own unknown rights.
        let terms_url="https://www.ace-hta.gov.sg/terms-of-use/";
        match f.get_ok(terms_url).await {
            Ok(terms) => {
                let html=Html::parse_document(&terms.text());
                let restriction=html.select(&sel("main#main-content p")).map(|p|text_of(&p))
                    .find(|t|t.contains("shall not be reproduced") && t.contains("prior written permission"));
                if let Some(restriction)=restriction {
                    doc.rights=Rights {status:"restricted".into(),terms_url:Some(terms_url.into()),restrictions:vec![restriction],
                        evidence_urls:vec![terms_url.into()],note:Some("Website terms apply to the ACE-hosted page; separately linked assets require their own rights review.".into()),..Default::default()};
                } else {doc.warnings.push("ACE terms fetched but reproduction restriction was not recognized; review rights manually.".into());}
                doc.fetches.push(terms.record);
            }
            Err(e) => doc.warnings.push(format!("ACE terms could not be fetched; rights remain unknown: {e:#}")),
        }
        Ok(doc)
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const ROBOTS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/sg_ace/robots.txt"));
    const SITEMAP: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/sg_ace/sitemap.xml"));
    const URTI_WEB: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/sg_ace/urti-web.html"));
    const URTI_WEB_URL: &str = "https://www.ace-hta.gov.sg/healthcare-professionals/acg/upper-respiratory-tract-infections----rational-antimicrobial-use-acg/";
    const URTI: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/sg_ace/urti.html"));
    const TERMS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/sg_ace/terms.html"));
    const URTI_URL: &str = "https://www.ace-hta.gov.sg/healthcare-professionals/ace-repository-for-clinical-guidelines/upper-respiratory-tract-infections--urtis---rational-antimicrobial-use/";
    const PAGE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/sg_ace/pneumonia.html"));
    const ROOT: &str = "https://www.ace-hta.gov.sg/healthcare-professionals/ace-repository-for-clinical-guidelines/";
    const CAP: &str = "https://www.ace-hta.gov.sg/healthcare-professionals/ace-repository-for-clinical-guidelines/community-acquired-pneumonia----assessment--diagnosis-and-antibiotic-prescribing-acg/";
    #[tokio::test]
    async fn saved_sitemap_and_pneumonia_offline() {
        let (index, locs)=locations(SITEMAP).unwrap(); assert!(!index);
        let prefix=Url::parse(ROOT).unwrap();
        let leaves:Vec<_>=locs.iter().filter(|loc| Url::parse(loc).ok().is_some_and(|u| {
            let rest=u.path().strip_prefix(prefix.path()).unwrap_or("").trim_matches('/');
            u.host_str()==prefix.host_str() && !rest.is_empty() && !rest.contains('/')
        })).collect();
        assert_eq!(leaves.len(),34); assert!(leaves.iter().any(|u|u.as_str()==CAP));
        // A one-record, source-derived urlset lets discover exercise acquisition without 33 other pages.
        let one=format!("<?xml version=\"1.0\"?><urlset xmlns=\"{}\"><url><loc>{}</loc></url></urlset>",NS,CAP);
        let dir=tempfile::tempdir().unwrap(); let f=Fetcher::new(dir.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests("https://www.ace-hta.gov.sg/robots.txt",ROBOTS.as_bytes(),"text/plain");
        f.seed_cache_for_tests("https://www.ace-hta.gov.sg/sitemap.xml",one.as_bytes(),"text/xml");
        f.seed_cache_for_tests(CAP,PAGE.as_bytes(),"text/html");
        f.seed_cache_for_tests("https://www.ace-hta.gov.sg/terms-of-use/",TERMS.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"sg_ace","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let rows=SgAce.discover(&src,&f,None).await.unwrap(); assert_eq!(rows.len(),1);
        let d=SgAce.extract(&src,&f,&rows[0]).await.unwrap();
        assert_eq!(d.published.as_deref(),Some("2026-09-21")); assert_eq!(d.version,None);
        assert_eq!(d.rights.status,"restricted");
        assert!(d.rights.restrictions.iter().any(|x|x.contains("prior written permission")));
        assert!(d.rights.evidence_urls.iter().any(|x|x=="https://www.ace-hta.gov.sg/terms-of-use/"));
        assert!(d.title.starts_with("Community-acquired pneumonia"));
        let recs:Vec<_>=d.blocks.iter().filter_map(|b| if let BlockKind::Recommendation{label,strength,..}=&b.kind {Some((label.as_deref(),strength.as_deref(),b.text.as_str()))}else{None}).collect();
        assert_eq!(recs.iter().map(|r|r.0.unwrap()).collect::<Vec<_>>(),vec!["1a","1b","2","3","4","5a","5b"]);
        assert!(recs.iter().all(|r|r.1.is_none()));
        assert!(recs[1].2.contains("the diagnosis cannot be made based on clinical signs and symptoms alone, or"));
        assert!(recs[1].2.contains("other serious respiratory conditions are suspected."));
        assert!(recs[2].2.contains("CRB-65") && recs[2].2.contains("community or hospital setting"));
        assert!(recs[4].2.contains("a. Use amoxicillin for patients at low risk of complications."));
        assert!(recs[4].2.contains("b. Use amoxicillin + clavulanic acid for patients at higher risk of complications."));
        assert!(recs[4].2.contains("c. Use doxycycline, azithromycin or clarithromycin as an alternative antibiotic for patients with penicillin or beta-lactam allergy."));
        assert!(recs[5].2.contains("monitor the symptoms") && recs[5].2.contains("does not improve or worsens"));
        assert!(recs[6].2.contains("follow-up consultation") && recs[6].2.contains("high risk of complications"));
        assert!(!d.blocks.iter().any(|b| b.text == "Healthcare professionals"));
        for role in ["full_acg","references","expert_group","evidence_to_recommendation"] { assert!(d.attachments.iter().any(|a|a["role"]==role)); }
        assert!(d.warnings.iter().any(|w|w.contains("not acquired")));
        assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text=="ACG recommendations"));
        let shell=PAGE.replace("<body>","<body><aside>Changed global navigation</aside>").replace("Healthcare professionals</a>","Changed breadcrumb</a>");
        assert_eq!(clinical_sha(PAGE).unwrap(),clinical_sha(&shell).unwrap());
        let changed=PAGE.replace("Diagnose community-acquired pneumonia (CAP) based on clinical signs and symptoms.","Altered clinical recommendation.");
        assert_ne!(clinical_sha(PAGE).unwrap(),clinical_sha(&changed).unwrap());
        let dir2=tempfile::tempdir().unwrap(); let f2=Fetcher::new(dir2.path(),true,500,1<<20,"").unwrap();
        f2.seed_cache_for_tests("https://www.ace-hta.gov.sg/robots.txt",ROBOTS.as_bytes(),"text/plain");
        f2.seed_cache_for_tests("https://www.ace-hta.gov.sg/sitemap.xml",one.as_bytes(),"text/xml");
        f2.seed_cache_for_tests(CAP,shell.as_bytes(),"text/html");
        let shell_rows=SgAce.discover(&src,&f2,None).await.unwrap();
        assert_eq!(rows[0].doc_key,shell_rows[0].doc_key);
        let dir3=tempfile::tempdir().unwrap(); let f3=Fetcher::new(dir3.path(),true,500,1<<20,"").unwrap();
        f3.seed_cache_for_tests("https://www.ace-hta.gov.sg/robots.txt",ROBOTS.as_bytes(),"text/plain");
        f3.seed_cache_for_tests("https://www.ace-hta.gov.sg/sitemap.xml",one.as_bytes(),"text/xml");
        f3.seed_cache_for_tests(CAP,changed.as_bytes(),"text/html");
        let changed_rows=SgAce.discover(&src,&f3,None).await.unwrap();
        assert_ne!(rows[0].doc_key,changed_rows[0].doc_key);
        assert!(SgAce.extract(&src,&f3,&rows[0]).await.is_err());
        f.seed_cache_for_tests(URTI_URL,URTI.as_bytes(),"text/html");
        f.seed_cache_for_tests(URTI_WEB_URL,URTI_WEB.as_bytes(),"text/html");
        let urti_row=PlanRow {source_id:"sg_ace".into(),doc_key:"urti-fixture".into(),url:URTI_URL.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({"work_key":"urti"})};
        let u=SgAce.extract(&src,&f,&urti_row).await.unwrap();
        assert_eq!(u.published.as_deref(),Some("2026-05-25"));
        assert!(u.date_observations.iter().any(|d|d.label=="Last updated on" && d.normalized.as_deref()==Some("2026-09-21") && d.source_url.as_deref()==Some(URTI_WEB_URL)));
        assert!(u.fetches.iter().any(|r|r.final_url==URTI_WEB_URL));
        assert!(u.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text=="Diagnosis and severity assessment" && b.heading_path.first().is_some_and(|p|p=="Full web rendition")));
        assert!(u.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..}) && b.heading_path.first().is_some_and(|p|p=="Full web rendition")).count()>=7);
        assert!(u.blocks.iter().filter(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(_),..}) && b.heading_path.first().is_some_and(|p|p=="Full web rendition")).count()>=4);
        let mut ids=HashSet::new(); assert!(u.blocks.iter().all(|b|ids.insert(b.block_id.clone())));
        assert!(u.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).all(|b|b.heading_path.first().is_some_and(|p|p=="Repository recommendation page")));
        let sh=clinical_sha(URTI).unwrap(); let wh=clinical_sha(URTI_WEB).unwrap();
        let baseline=snapshot_sha(&sh,Some((URTI_WEB_URL,&wh)));
        let altered_web=URTI_WEB.replace("Overuse of antibiotics", "Changed clinical text on overuse of antibiotics");
        assert_ne!(baseline,snapshot_sha(&sh,Some((URTI_WEB_URL,&clinical_sha(&altered_web).unwrap()))));
        let altered_summary=URTI.replace("Do not prescribe antibiotics for patients with uncomplicated URTI.","Altered summary recommendation.");
        assert_ne!(baseline,snapshot_sha(&clinical_sha(&altered_summary).unwrap(),Some((URTI_WEB_URL,&wh))));
        let bad=URTI.replace(URTI_WEB_URL,"https://outside.example/healthcare-professionals/acg/guideline/");
        let bad=bad.replace("/healthcare-professionals/acg/upper-respiratory-tract-infections----rational-antimicrobial-use-acg/","https://outside.example/healthcare-professionals/acg/guideline/");
        assert!(web_link(&bad,&Url::parse(URTI_URL).unwrap()).is_err());
        let base=Url::parse(URTI_URL).unwrap();
        let external=crate::guidelines::doc::FetchRecord {redirects:vec![URTI_WEB_URL.into()],final_url:"https://outside.example/a".into(),..Default::default()};
        assert!(verify_web_redirects(&external,&base).is_err());
        let cycle=crate::guidelines::doc::FetchRecord {redirects:vec![URTI_WEB_URL.into()],final_url:URTI_WEB_URL.into(),..Default::default()};
        assert!(verify_web_redirects(&cycle,&base).is_err());
        let temp=tempfile::tempdir().unwrap(); let fu=Fetcher::new(temp.path(),true,500,1<<20,"").unwrap();
        let one_urti=format!("<?xml version=\"1.0\"?><urlset xmlns=\"{}\"><url><loc>{}</loc></url></urlset>",NS,URTI_URL);
        fu.seed_cache_for_tests("https://www.ace-hta.gov.sg/robots.txt",ROBOTS.as_bytes(),"text/plain");
        fu.seed_cache_for_tests("https://www.ace-hta.gov.sg/sitemap.xml",one_urti.as_bytes(),"text/xml");
        fu.seed_cache_for_tests(URTI_URL,URTI.as_bytes(),"text/html"); fu.seed_cache_for_tests(URTI_WEB_URL,URTI_WEB.as_bytes(),"text/html");
        fu.seed_cache_for_tests("https://www.ace-hta.gov.sg/terms-of-use/",TERMS.as_bytes(),"text/html");
        let urti_plan=SgAce.discover(&src,&fu,None).await.unwrap();
        assert_eq!(urti_plan[0].extra["web_url"],URTI_WEB_URL);
        let planned=SgAce.extract(&src,&fu,&urti_plan[0]).await.unwrap();
        assert!(planned.fetches.iter().any(|r|r.final_url==URTI_WEB_URL));
        let changed_dir=tempfile::tempdir().unwrap();let fc=Fetcher::new(changed_dir.path(),true,500,1<<20,"").unwrap();
        fc.seed_cache_for_tests(URTI_URL,URTI.as_bytes(),"text/html");
        fc.seed_cache_for_tests(URTI_WEB_URL,altered_web.as_bytes(),"text/html");
        assert!(SgAce.extract(&src,&fc,&urti_plan[0]).await.is_err());
        let ur:Vec<_>=u.blocks.iter().filter_map(|b|if let BlockKind::Recommendation{label,strength,certainty,..}=&b.kind {Some((label.as_deref(),strength.as_deref(),certainty.as_deref(),b.text.as_str()))}else{None}).collect();
        assert_eq!(ur.iter().map(|r|r.0.unwrap()).collect::<Vec<_>>(),vec!["1","2","3","4"]);
        assert!(ur.iter().all(|r|r.1.is_none() && r.2.is_none()));
        assert!(ur[0].3.contains("working diagnosis"));
        assert!(ur[1].3.contains("Do not prescribe antibiotics") && ur[1].3.contains("uncomplicated URTI"));
        assert!(ur[2].3.contains("acute rhinosinusitis, pharyngotonsillitis or acute otitis media") && ur[2].3.contains("bacterial infection is clinically suspected") && ur[2].3.contains("individual risk factors for severe disease outcome"));
        assert!(ur[3].3.contains("Do not routinely prescribe antivirals") && ur[3].3.contains("otherwise healthy patients"));
        assert!(u.attachments.iter().any(|a|a["role"]=="web_rendition" && a["url"]=="https://www.ace-hta.gov.sg/healthcare-professionals/acg/upper-respiratory-tract-infections----rational-antimicrobial-use-acg/"));
        assert!(u.attachments.iter().any(|a|a["role"]=="evidence_to_recommendation" && a["url"]=="https://doi.org/10.4103/singaporemedj.SMJ-2026-181"));
        for role in ["full_acg","references","expert_group"] {assert!(u.attachments.iter().any(|a|a["role"]==role && a["rights"].as_str().unwrap().contains("unknown")));}
    }
}
