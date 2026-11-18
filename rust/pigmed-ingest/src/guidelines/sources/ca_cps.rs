//! CPS English all-years date index, following the publisher's emitted Next links.
//! The immutable plan pins the complete statement wrapper, including header events.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, DateObservation, FetchRecord, GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::{json, Value};
use std::collections::HashSet;
use url::Url;

pub struct CaCps;
const CATALOG: &str = "https://cps.ca/en/documents/statements-by-date";
const INDEX_PATH: &str = "/en/documents/statements-by-date";
const POSITION_PATH: &str = "/en/documents/position/";

fn index_url(u: &Url) -> bool {
    let suffix = u.path().trim_end_matches('/').strip_prefix(INDEX_PATH);
    u.scheme() == "https" && u.host_str() == Some("cps.ca") && u.port().is_none()
        && u.username().is_empty() && u.password().is_none() && u.query().is_none()
        && suffix.is_some_and(|s| s.is_empty() || s.strip_prefix("/P").is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())))
}
fn position_url(u: &Url) -> bool {
    u.scheme() == "https" && u.host_str() == Some("cps.ca") && u.port().is_none()
        && u.username().is_empty() && u.password().is_none() && u.query().is_none()
        && u.path().strip_prefix(POSITION_PATH).is_some_and(|s| !s.trim_matches('/').is_empty() && !s.trim_matches('/').contains('/'))
}
fn absolute(base: &Url, href: &str) -> Option<String> {
    let u = base.join(href.trim()).ok()?;
    (matches!(u.scheme(), "https" | "http") && u.username().is_empty() && u.password().is_none()).then(|| u.to_string())
}

#[derive(Debug)]
struct Listing { rows: Vec<(String, String, String)>, next: Option<String> }
fn listing(html: &str, base: &Url) -> Result<Listing> {
    if !index_url(base) { bail!("CPS catalog redirected outside its English all-years index: {base}"); }
    let doc = Html::parse_document(html);
    let root = doc.select(&sel(".main-body")).next().context("CPS catalog .main-body missing")?;
    if root.select(&sel("#year option[selected]")).next().is_some_and(|o| text_of(&o) != "All years") {
        bail!("CPS catalog is year-filtered; all-years coverage required");
    }
    let mut rows = vec![];
    let mut seen = HashSet::new();
    for a in root.select(&sel(".stmt-title a[href]")) {
        let raw = a.value().attr("href").unwrap_or_default();
        let url = common::link(base, raw).context("CPS statement link is not public HTTP")?;
        if !position_url(&Url::parse(&url)?) { bail!("CPS catalog statement link outside supported position route: {url}"); }
        let title = text_of(&a);
        if title.is_empty() { bail!("CPS catalog statement title empty: {url}"); }
        let metadata = a.ancestors().filter_map(ElementRef::wrap).find(|e| e.value().classes().any(|c| c == "cps-row"))
            .and_then(|r| r.next_siblings().filter_map(ElementRef::wrap).next())
            .and_then(|r| r.select(&sel(".stmt-type")).next()).map(|e| text_of(&e)).unwrap_or_default();
        if seen.insert(url.clone()) { rows.push((url, title, metadata)); }
    }
    if rows.is_empty() { bail!("CPS catalog returned no statements (empty HTTP-200 shell is not a terminal page)"); }
    let nexts: HashSet<String> = root.select(&sel(".pagination-next a[href], a[rel='next']"))
        .map(|a| {
            let u = common::link(base, a.value().attr("href").unwrap_or_default()).context("CPS Next URL invalid")?;
            if !index_url(&Url::parse(&u)?) { bail!("CPS Next URL leaves English all-years index: {u}"); }
            Ok(u)
        }).collect::<Result<_>>()?;
    if nexts.len() > 1 { bail!("CPS catalog has conflicting Next links"); }
    // Missing pagination entirely is a changed template, not proof of exhaustion.
    if root.select(&sel(".cps-pagination")).next().is_none() { bail!("CPS catalog pagination landmark missing"); }
    Ok(Listing { rows, next: nexts.into_iter().next() })
}

fn source_date(label: &str, raw: &str, base: &Url) -> DateObservation {
    let mut out = common::date(label, raw, base.as_str());
    // CPS prints abbreviated English month names; the shared helper accepts full names.
    let re = regex::Regex::new(r"(?i)^(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Sept|Oct|Nov|Dec)\.?\s+(\d{1,2}),?\s+(\d{4})$").unwrap();
    if let Some(c) = re.captures(&out.raw) {
        let month = match c[1].to_ascii_lowercase().as_str() { "jan"=>1,"feb"=>2,"mar"=>3,"apr"=>4,"may"=>5,"jun"=>6,"jul"=>7,"aug"=>8,"sep"|"sept"=>9,"oct"=>10,"nov"=>11,_=>12 };
        let day = c[2].parse::<u32>().unwrap_or(0);
        let year = c[3].parse::<u32>().unwrap_or(0);
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days = match month { 2 if leap=>29,2=>28,4|6|9|11=>30,_=>31 };
        out.normalized = (year >= 1000 && (1..=days).contains(&day)).then(|| format!("{year:04}-{month:02}-{day:02}"));
    }
    out
}
fn events(root: ElementRef<'_>, base: &Url) -> Vec<DateObservation> {
    let mut out = vec![];
    for el in root.select(&sel(".posted-date p")) {
        for part in text_of(&el).split('|') {
            if let Some((label, raw)) = part.split_once(':') {
                if !label.trim().is_empty() && !raw.trim().is_empty() { out.push(source_date(label.trim(), raw.trim(), base)); }
            }
        }
    }
    out
}

#[derive(Debug)]
struct Statement {
    title: String, html: String, sha256: String, item_type: Option<String>, dates: Vec<DateObservation>,
    language: Option<String>, publisher: Option<String>, issuers: Vec<String>, doi: Option<String>,
    metadata: Value, attachments: Vec<Value>, relations: Vec<Relation>, rights: Rights,
}
fn statement(html: &str, base: &Url) -> Result<Statement> {
    if base.path().trim_end_matches('/') == "/en/404/statement-retired" { bail!("CPS statement retired redirect: {base}"); }
    if !position_url(base) { bail!("CPS statement final URL outside supported English position route: {base}"); }
    let doc = Html::parse_document(html);
    let roots: Vec<_> = doc.select(&sel(".statement-wrapper")).collect();
    if roots.len() != 1 { bail!("CPS requires exactly one full .statement-wrapper, found {}", roots.len()); }
    let root = roots[0];
    let title = root.select(&sel("h1")).map(|h| text_of(&h)).find(|t| !t.is_empty()).context("CPS statement title missing")?;
    let callout = root.select(&sel(".cps-callout")).next().context("CPS statement metadata callout missing")?;
    if !root.select(&sel("h2, h3")).any(|h| !callout.descendants().any(|n| n.id() == h.id()))
        || root.select(&sel("p, li, table")).filter(|e| !callout.descendants().any(|n| n.id() == e.id())).map(|e| text_of(&e).len()).sum::<usize>() < 100 {
        bail!("CPS statement wrapper contains only a header/abstract or an empty shell");
    }
    let wrapper = root.html();
    let sha256 = crate::common::sha256_hex(wrapper.as_bytes());
    let mut clean = wrapper;
    // Only observed decorative/UI elements are removed; all clinical wrapper children remain.
    for el in root.select(&sel(".sharethis-inline-share-buttons, .cps-logo, a[href='/en/education/test-your-knowledge']")) { clean = clean.replacen(&el.html(), "", 1); }
    // The shared table walker retains cell text but does not visit nested images.
    // Preserve their assets immediately after the containing outer table as well.
    for table in root.select(&sel("table")).filter(|t| !t.ancestors().filter_map(ElementRef::wrap).any(|e| e.id()!=t.id() && e.value().name()=="table")) {
        let images=table.select(&sel("img")).map(|i|i.html()).collect::<Vec<_>>().join("");
        if !images.is_empty() { let original=table.html(); clean=clean.replacen(&original,&format!("{original}{images}"),1); }
    }
    // The generic helper skips <header>; a source clinical header is metadata, not navigation.
    clean = regex::Regex::new(r"(?i)<(/?)header\b").unwrap().replace_all(&clean, "<${1}div").into_owned();
    let item_type = root.select(&sel(".paper-format")).next().map(|e| text_of(&e)).filter(|s| !s.is_empty()).map(|s| common::slug(&s).replace('-', "_"));
    let authors = callout.select(&sel("h3")).find(|h| text_of(h).eq_ignore_ascii_case("Principal author(s)"))
        .and_then(|h| h.next_siblings().filter_map(ElementRef::wrap).find(|e| e.value().name() == "p")).map(|p| text_of(&p));
    let keywords = callout.select(&sel("p")).map(|p| text_of(&p)).find_map(|t| t.strip_prefix("Keywords:").map(|s| s.trim().to_string()));
    let issuers: Vec<_> = callout.select(&sel("a[href*='/documents/authors-auteurs/']")).map(|a| text_of(&a)).filter(|s| !s.is_empty()).collect();
    let author_meta = doc.select(&sel("meta[name='author']")).next().and_then(|m| m.value().attr("content")).filter(|s| !s.trim().is_empty()).map(String::from);
    let publisher = doc.select(&sel("meta[name='citation_publisher']")).next().and_then(|m| m.value().attr("content")).filter(|s| !s.trim().is_empty()).map(String::from);
    // Never harvest DOIs from the reference list and attribute them to this statement.
    let doi = doc.select(&sel("meta[name='citation_doi']")).next().and_then(|m| m.value().attr("content")).filter(|s| !s.trim().is_empty()).map(String::from)
        .or_else(|| callout.select(&sel("a[href]")).filter_map(|a| a.value().attr("href")).find_map(|h| h.strip_prefix("https://doi.org/").or_else(|| h.strip_prefix("http://doi.org/")).map(String::from)))
        .or_else(|| regex::Regex::new(r"(?i)\bdoi:\s*(10\.\d{4,9}/[^\s<>]+)").unwrap().captures(&text_of(&callout)).map(|c| c[1].trim_end_matches(|ch:char| matches!(ch,'.'|','|';')).to_string()));
    let mut attachments = vec![];
    let mut relations = vec![];
    let mut links = vec![];
    let mut seen = HashSet::new();
    for a in root.select(&sel("a[href]")) {
        if let Some(u) = a.value().attr("href").and_then(|h| absolute(base, h)) {
            links.push(json!({"url":u,"text":text_of(&a),"source_element_id":a.value().id(),"source_url":base.as_str()}));
            let parsed = Url::parse(&u)?;
            if parsed.path().to_ascii_lowercase().ends_with(".pdf") && seen.insert(u.clone()) {
                let journal = a.ancestors().any(|e| e.id() == callout.id()) && parsed.host_str() == Some("academic.oup.com");
                let role = if journal { "linked_journal_pdf" } else { "linked_pdf" };
                attachments.push(json!({"url":u,"title":text_of(&a),"role":role,"source_url":base.as_str(),"acquired":false}));
                if journal { relations.push(Relation { rel:"related".into(),target:u,scope:Some("Publisher-linked journal PDF; equivalence and access not verified".into()),evidence_url:Some(base.to_string()) }); }
            }
        }
    }
    for a in doc.select(&sel("a[href]")) {
        let label = text_of(&a);
        let Some(u) = a.value().attr("href").and_then(|h| absolute(base, h)) else { continue; };
        let p = Url::parse(&u)?;
        if p.host_str() == Some("cps.ca") && p.path().starts_with("/fr/documents/position/")
            && (label.eq_ignore_ascii_case("French") || a.value().attr("hreflang") == Some("fr")) && seen.insert(u.clone()) {
            attachments.push(json!({"url":u,"title":label,"role":"language_version","language":"fr","source_url":base.as_str(),"acquired":false}));
            relations.push(Relation { rel:"translation_of".into(),target:u,scope:Some("French language counterpart linked by CPS; text not compared".into()),evidence_url:Some(base.to_string()) });
        }
    }
    let mut rights = Rights { status:"unknown".into(), ..Default::default() };
    let notices: Vec<_> = root.select(&sel("p")).map(|e| text_of(&e)).filter(|t| t.contains("permission to print single copies") || t.contains("permission to reprint or reproduce multiple copies")).collect();
    let copyright = doc.select(&sel(".footer__copyright")).next().map(|e| {
        e.children().filter_map(|n| n.value().as_text()).map(|t| t.to_string()).collect::<String>().trim().to_string()
    }).filter(|s| !s.is_empty());
    rights.terms_url = root.select(&sel("a[href*='/policies-politiques/copyright']")).next().and_then(|a| a.value().attr("href")).and_then(|h| absolute(base, h));
    if copyright.as_deref().is_some_and(|s| s.contains("All rights reserved")) { rights.license=Some("All rights reserved".into()); rights.status="restricted".into(); }
    if let Some(c) = &copyright {
        rights.note=Some(c.clone());
        if let Some(c) = c.strip_prefix('©') { let holder = c.trim().split_once(' ').map(|(_,h)| h.split('.').next().unwrap_or_default().trim()).unwrap_or_default(); if !holder.is_empty() { rights.rights_holders.push(holder.to_string()); } }
    }
    rights.restrictions=notices;
    if rights.note.is_some() || !rights.restrictions.is_empty() { rights.evidence_urls.push(base.to_string()); }
    let metadata=json!({"principal_authors_raw":authors,"author_meta":author_meta,"authoring_groups":issuers,"keywords_raw":keywords,"source_links":links,"source_url":base.as_str(),"clinical_sha256":sha256});
    Ok(Statement { title,html:clean,sha256,item_type,dates:events(root,base),language:common::page_language(&doc),publisher,issuers,doi,
        metadata,attachments,relations,rights })
}

fn recommendation_kind() -> BlockKind {
    BlockKind::Recommendation { label:None,strength:None,certainty:None,population:None,grading_system:None,qualifier:None }
}

fn recommendations(html: &str, blocks: &mut Vec<Block>) -> (Vec<String>, Vec<Value>) {
    let mut warnings = vec![];
    let mut groups = vec![];
    let doc = Html::parse_document(html);
    // Match source top-level LI spans rather than guessing from flattened block
    // kinds. The shared walker emits Paragraph for LI containing P or a nested
    // list; nested support LI are not independent source recommendations.
    let mut path:Vec<(u8,String)>=vec![];
    let mut cursor=0;
    for element in doc.select(&sel(".statement-wrapper h1, .statement-wrapper h2, .statement-wrapper h3, .statement-wrapper h4, .statement-wrapper h5, .statement-wrapper h6, .statement-wrapper li")) {
        if element.ancestors().filter_map(ElementRef::wrap).any(|e|e.value().name()=="table") {continue;}
        let tag=element.value().name();
        if tag!="li" {
            let level=tag[1..].parse::<u8>().unwrap_or(6);
            path.retain(|(l,_)|*l<level);
            path.push((level,text_of(&element)));
            continue;
        }
        if element.ancestors().filter_map(ElementRef::wrap).any(|e|e.id()!=element.id() && e.value().name()=="li") {continue;}
        let headings:Vec<String>=path.iter().map(|(_,h)|h.clone()).collect();
        let scoped=headings.iter().any(|h|h.eq_ignore_ascii_case("Recommendations"));
        let closed=headings.iter().any(|h|matches!(h.to_ascii_lowercase().as_str(),"acknowledgements"|"acknowledgments"|"references"|"funding"|"potential conflict of interest"));
        if !scoped || closed {continue;}
        // Running the identical generic walker on one source LI yields exactly
        // its ordered block span, including nested supporting lists and P nodes.
        let fragment=format!("<div id='cps-source-list-item'>{}</div>",element.html());
        let (local,_)=blocks_from_html(&fragment,None,Some("#cps-source-list-item"));
        if local.is_empty() {warnings.push("Empty source recommendation list item retained without a structured statement".into());continue;}
        let found=(cursor..blocks.len()).find(|&start| {
            start+local.len()<=blocks.len() && blocks[start].heading_path==headings
                && local.iter().zip(&blocks[start..start+local.len()]).all(|(a,b)|a.text==b.text && std::mem::discriminant(&a.kind)==std::mem::discriminant(&b.kind))
        });
        let Some(start)=found else {warnings.push(format!("Source recommendation item in {} could not be mapped unambiguously to its ordered HTML blocks; original content retained",headings.join(" > ")));continue;};
        let end=start+local.len();
        let parent_id=blocks[start].block_id.clone();
        let item_ids:Vec<_>=blocks[start..end].iter().map(|b|b.block_id.clone()).collect();
        let supporting_ids:Vec<_>=blocks[start+1..end].iter().filter(|b|matches!(b.kind,BlockKind::ListItem)).map(|b|b.block_id.clone()).collect();
        let continuation_ids:Vec<_>=blocks[start+1..end].iter().filter(|b|!matches!(b.kind,BlockKind::ListItem)).map(|b|b.block_id.clone()).collect();
        blocks[start].kind=recommendation_kind();
        // Classification changes only the parent's kind. Complete ordered item
        // content is represented by block IDs, preserving qualifiers and source
        // grouping without copying nested support text into the parent block.
        groups.push(json!({"recommendation_block_id":parent_id,"source_item_id":element.value().id(),"source_heading_path":headings,"source_item_block_ids":item_ids,"supporting_block_ids":supporting_ids,"continuation_block_ids":continuation_ids,"body_text_unchanged":true}));
        cursor=end;
    }
    if doc.select(&sel(".statement-wrapper table img")).next().is_some() {
        warnings.push("Images nested in tables are emitted after their containing table; original cell text is retained, while exact visual placement requires source comparison".into());
    }
    let tables: Vec<_> = doc.select(&sel(".statement-wrapper table")).filter(|t| !t.ancestors().filter_map(ElementRef::wrap).any(|e| e.id()!=t.id() && e.value().name()=="table")).collect();
    let table_ids: Vec<_> = blocks.iter().filter(|b| matches!(b.kind,BlockKind::Table{..})).map(|b| b.block_id.clone()).collect();
    for (i, table) in tables.iter().enumerate() {
        let first = table.select(&sel("caption, th, p strong")).next().map(|e| text_of(&e)).unwrap_or_default();
        if !first.to_ascii_lowercase().starts_with("recommendations") { continue; }
        let mut recs=vec![];
        for (j,li) in table.select(&sel("li")).filter(|li|!li.ancestors().filter_map(ElementRef::wrap).any(|e|e.id()!=li.id() && e.value().name()=="li")).enumerate() {
            recs.push(Block { block_id:format!("cps-table-{}-recommendation-{}",i+1,j+1),kind:BlockKind::Recommendation{label:None,strength:None,certainty:None,population:None,grading_system:None,qualifier:None},text:text_of(&li),heading_path:vec![],page:None });
        }
        if recs.is_empty() { warnings.push(format!("Recommendation table {} retained but row-level recommendation structure requires review",i+1)); }
        if let Some(id)=table_ids.get(i) { if let Err(e)=common::insert_table_recommendations(blocks,id,recs) { warnings.push(e.to_string()); } }
    }
    warnings.push("Recommendation prose and complex table grades remain verbatim; no strength, certainty or currentness is inferred. Linked journal PDFs and language versions are unacquired and not compared. Figure URLs may expire; asset acquisition is handled by ingest.".into());
    (warnings, groups)
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const INDEX: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/index.html"));
    const PAGE2: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/index-p10.html"));
    const STATEMENT: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/statement.html"));
    const EMPTY: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/empty-shell.html"));
    const RETIRED: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/retired.html"));
    const FIRST: &str="https://cps.ca/en/documents/position/teen-dating-violence-among-adolescents";
    const SECOND: &str="https://cps.ca/en/documents/position/retinopathy-of-prematurity-screening";
    const THIRD: &str="https://cps.ca/en/documents/position/premedication-for-laryngoscopy";
    fn base() -> Url { Url::parse("https://cps.ca/en/documents/position/positive-parenting").unwrap() }
    fn source(config:Value)->SourceEntry { serde_json::from_value(json!({"id":"ca_cps","access":"public","enabled":true,"catalog_url":CATALOG,"config":config})).unwrap() }
    fn terminal() -> String { PAGE2.replace("<li class=\"pagination-next\"><a href=\"/en/documents/statements-by-date/P20\" aria-label=\"Next page\">Next <span class=\"show-for-sr\">page</span></a></li>", "") }
    fn seed(f:&Fetcher) {
        f.seed_cache_for_tests(CATALOG,INDEX.as_bytes(),"text/html");
        f.seed_cache_for_tests(&format!("{CATALOG}/P10"),terminal().as_bytes(),"text/html");
        for u in [FIRST,SECOND,THIRD] { f.seed_cache_for_tests(u,STATEMENT.as_bytes(),"text/html"); }
    }
    #[test]
    fn live_dom_recommendation_ancestors_keep_twelve_parents_and_four_support_items() {
        const HTML:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/recommendation-hierarchy.html"));
        const CONTRACT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/recommendation-dom-contract.json"));
        let contract:Value=serde_json::from_str(CONTRACT).unwrap();
        assert_eq!(contract["articles"][0]["explicit_recommendations_direct_li"],12);
        assert_eq!(contract["articles"][0]["separate_prevention_direct_li"],6);
        assert_eq!(contract["articles"][0]["nested_support_direct_li"],4);
        for html in [HTML.to_string(),HTML.replace("<li><em>Diagnosis item 1</em></li>","<li><p><em>Diagnosis item 1</em></p></li>"),HTML.replace("<li><em>Diagnosis item 1</em></li>","<li><p><em>Diagnosis item 1</em></p><p>Continuation qualifier.</p></li>")] {
            let (mut blocks,_)=blocks_from_html(&html,None,Some(".statement-wrapper"));
            let original_len=blocks.len();
            let original_ids:Vec<_>=blocks.iter().map(|b|b.block_id.clone()).collect();
            let original_texts:Vec<_>=blocks.iter().map(|b|b.text.clone()).collect();
            let original_words:usize=blocks.iter().map(|b|b.text.split_whitespace().count()).sum();
            let (warnings,groups)=recommendations(&html,&mut blocks);
            assert!(!warnings.iter().any(|w|w.contains("could not be mapped")),"{warnings:?}");
            assert_eq!(blocks.len(),original_len);
            assert_eq!(blocks.iter().map(|b|b.block_id.clone()).collect::<Vec<_>>(),original_ids);
            assert_eq!(blocks.iter().map(|b|b.text.clone()).collect::<Vec<_>>(),original_texts);
            assert_eq!(blocks.iter().map(|b|b.text.split_whitespace().count()).sum::<usize>(),original_words);
            let recs:Vec<_>=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();
            assert_eq!(recs.len(),12);assert_eq!(groups.len(),12);
            for (heading,count) in [("Diagnosis",4),("Initial episode",3),("First recurrence",1),("Second or later recurrences and secondary prophylaxis",4)] {
                assert_eq!(recs.iter().filter(|b|b.heading_path.last().map(String::as_str)==Some(heading)).count(),count,"{heading}");
            }
            assert!(recs.iter().all(|b|matches!(&b.kind,BlockKind::Recommendation{label:None,strength:None,certainty:None,..})));
            assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::ListItem) && b.heading_path.last().map(String::as_str)==Some("Prevention")).count(),6);
            let supporting:Vec<_>=blocks.iter().filter(|b|b.text.starts_with("Supporting diagnostic outcome")).collect();
            assert_eq!(supporting.len(),4);assert!(supporting.iter().all(|b|matches!(b.kind,BlockKind::ListItem)));
            let parent=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.text.starts_with("Diagnostic parent qualifiers")).unwrap();
            assert_eq!(parent.text,"Diagnostic parent qualifiers");
            let group=groups.iter().find(|g|g["recommendation_block_id"]==parent.block_id).unwrap();
            assert_eq!(group["body_text_unchanged"],true);
            assert_eq!(group["source_item_block_ids"].as_array().unwrap().len(),5);
            assert_eq!(group["supporting_block_ids"].as_array().unwrap().len(),4);
            assert_eq!(group["continuation_block_ids"].as_array().unwrap().len(),0);
            if html.contains("Continuation qualifier.") {
                assert_eq!(groups[0]["continuation_block_ids"].as_array().unwrap().len(),1);
                assert_eq!(groups[0]["source_item_block_ids"].as_array().unwrap().len(),2);
                assert!(blocks.iter().any(|b|b.text=="Continuation qualifier." && matches!(b.kind,BlockKind::Paragraph)));
            }
            let position=blocks.iter().position(|b|b.block_id==parent.block_id).unwrap();
            for (i,child) in supporting.iter().enumerate() {assert_eq!(blocks[position+i+1].block_id,child.block_id);}
            assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);
            assert!(blocks.iter().filter(|b|b.heading_path.iter().any(|h|matches!(h.as_str(),"Acknowledgements"|"References"))).all(|b|!matches!(b.kind,BlockKind::Recommendation{..})));
        }
    }
    #[test]
    fn distributed_lists_and_recommendation_prose_are_not_formal_list_items() {
        let html="<div class='statement-wrapper'><h1>Structural negative controls</h1><h2>Tele-ophthalmology</h2><ol><li>Distributed guidance item.</li></ol><h2>Recommendations for practice</h2><p>Source recommendation prose remains prose.</p><div id='table-2'><table><tr><td>Example CUES script<ul><li>Nested table example.</li></ul></td></tr></table></div><h2>Suspected and disclosed TDV</h2><ul><li>Other section item.</li></ul><h2>References</h2><ol><li>Reference.</li></ol></div>";
        let (mut blocks,_)=blocks_from_html(html,None,Some(".statement-wrapper"));
        let before=blocks.len();let (_,groups)=recommendations(html,&mut blocks);
        assert!(groups.is_empty());assert_eq!(blocks.len(),before);
        assert!(blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})));
        assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..}) && b.text.contains("Nested table example.")));
        // Even a lower-level reference heading closes semantic recommendation scope.
        let html="<div class='statement-wrapper'><h1>Reference boundary</h1><h2>Recommendations</h2><h3>Diagnosis</h3><ul><li>Formal item.</li></ul><h3>References</h3><ol><li>Ordinary citation.</li></ol></div>";
        let (mut blocks,_)=blocks_from_html(html,None,Some(".statement-wrapper"));recommendations(html,&mut blocks);
        assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).count(),1);
        assert!(blocks.iter().any(|b|b.text=="Ordinary citation." && matches!(b.kind,BlockKind::ListItem)));
    }
    #[test]
    fn live_derived_rows_header_dates_and_complete_recommendation_table() {
        const LIVE_INDEX:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/live-derived/date_index.html"));
        const LIVE_P10:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/live-derived/date_index_p10.html"));
        const LIVE_STATEMENT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_cps/live-derived/positive_parenting.html"));
        // The live derivation retains real record rows, but deliberately excludes
        // the main-body wrapper and pagination. Restore those already observed
        // in Chrome explicitly, rather than weakening production validation.
        let page=|rows:&str,next:&str|format!("<div class='main-body'>{rows}<ul class='cps-pagination'><li class='pagination-next'><a href='{next}' aria-label='Next page'>Next <span class='show-for-sr'>page</span></a></li></ul></div>");
        let first=listing(&page(LIVE_INDEX,"/en/documents/statements-by-date/P10"),&Url::parse(CATALOG).unwrap()).unwrap();
        assert_eq!(first.rows.len(),10);assert_eq!(first.rows[0].0,FIRST);
        assert_eq!(first.rows[0].2,"Practice point | Posted: Nov 5, 2026");
        assert_eq!(first.rows[2].1,"Clostridioides difficile in children");
        let second=listing(&page(LIVE_P10,"/en/documents/statements-by-date/P20"),&Url::parse(first.next.as_ref().unwrap()).unwrap()).unwrap();
        assert_eq!(second.rows.len(),10);assert_eq!(second.rows[0].0,THIRD);
        assert!(second.rows[1].0.ends_with("/Assessing-GDD-IDD"));
        assert!(second.next.unwrap().ends_with("/P20"));
        let statement=statement(LIVE_STATEMENT,&base()).unwrap();
        assert_eq!(statement.title,"Relationships matter: How clinicians can support positive parenting in the early years");
        assert_eq!(statement.dates.len(),2);
        assert_eq!(statement.dates[0].normalized.as_deref(),Some("2019-07-25"));
        assert_eq!(statement.dates[1].label,"Reaffirmed");
        assert_eq!(statement.dates[1].normalized.as_deref(),Some("2025-04-30"));
        assert_eq!(statement.item_type.as_deref(),Some("position_statement"));
        assert_eq!(statement.issuers,["Early Years Committee"]);
        assert!(statement.metadata["principal_authors_raw"].as_str().unwrap().contains("Anne Biscaro; Jean Clinton"));
        assert_eq!(statement.metadata["keywords_raw"],"ACEs; Adverse childhood experiences; Attachment; Parenting");
        assert_eq!(statement.rights.status,"restricted");
        assert!(statement.attachments.iter().any(|a|a["role"]=="linked_journal_pdf" && a["url"].as_str().unwrap().ends_with("pxz063.pdf")));
        let (mut blocks,_)=blocks_from_html(&statement.html,Some(&base()),Some(".statement-wrapper"));
        recommendations(&statement.html,&mut blocks);
        assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);
        let recs:Vec<_>=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();
        assert_eq!(recs.len(),7);
        assert_eq!(recs[0].text,"Promote responsiveness and proactive comforting for crying or signs of child stress.");
        assert_eq!(recs[6].text,"Recommend targeted reading, community supports, and skills training, as appropriate.");
        assert!(recs.iter().all(|r|matches!(&r.kind,BlockKind::Recommendation{strength:None,certainty:None,..})));
        assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::ListItem) && b.text.contains("sk questions")));
        // Excerpt boundaries and duplicated derivation fragments are not evidence
        // of original body counts, body equality, language absence or completeness.
        assert!(statement.language.is_none());
    }
    #[test]
    fn actual_navigation_and_metadata_are_not_guessed_offsets() {
        let a=listing(INDEX,&Url::parse(CATALOG).unwrap()).unwrap();
        assert_eq!(a.rows.len(),2); assert_eq!(a.rows[0].0,FIRST);
        assert_eq!(a.rows[0].2,"Practice point | Posted: Nov 5, 2026");
        assert_eq!(a.next.as_deref(),Some("https://cps.ca/en/documents/statements-by-date/P10"));
        let b=listing(PAGE2,&Url::parse(a.next.as_ref().unwrap()).unwrap()).unwrap();
        assert_eq!(b.rows[0].0,THIRD); assert_eq!(b.next.as_deref(),Some("https://cps.ca/en/documents/statements-by-date/P20"));
        // Offset values are opaque emitted links, even when not the historical stride.
        let odd=INDEX.replace("/P10","/P37");
        assert!(listing(&odd,&Url::parse(CATALOG).unwrap()).unwrap().next.unwrap().ends_with("/P37"));
        for target in ["https://example.org/en/documents/statements-by-date/P10","/en/documents/statements-by-date/2025/P10","/fr/documents/statements-by-date/P10","/en/documents/statements-by-date/P10?year=2025"] {
            assert!(listing(&INDEX.replace("/en/documents/statements-by-date/P10",target),&Url::parse(CATALOG).unwrap()).is_err());
        }
        assert!(listing("<div class='main-body'><ul class='cps-pagination'></ul></div>",&Url::parse(CATALOG).unwrap()).is_err());
        assert!(listing(&INDEX.replace("cps-pagination","removed"),&Url::parse(CATALOG).unwrap()).is_err());
        let duplicate=INDEX.replace("</div></body>",&format!("<div class='stmt-title'><a href='{FIRST}#x'>Duplicate</a></div></div></body>"));
        assert_eq!(listing(&duplicate,&Url::parse(CATALOG).unwrap()).unwrap().rows.len(),2);
    }
    #[test]
    fn header_dates_are_distinct_and_do_not_attribute_reference_doi() {
        let mut html=STATEMENT.replace("| <strong>Reaffirmed:","| <strong>Updated: </strong>Jan 24, 2025 | <strong>Reaffirmed:");
        let s=statement(&html,&base()).unwrap();
        assert_eq!(s.item_type.as_deref(),Some("position_statement"));assert_eq!(s.language.as_deref(),Some("en"));
        assert_eq!(s.dates.iter().map(|d|d.label.as_str()).collect::<Vec<_>>(),["Posted","Updated","Reaffirmed"]);
        assert_eq!(s.dates.iter().map(|d|d.normalized.as_deref()).collect::<Vec<_>>(),[Some("2019-07-25"),Some("2025-01-24"),Some("2025-04-30")]);
        assert!(s.doi.is_none());assert_eq!(s.issuers,["Early Years Committee"]);
        assert!(s.metadata["principal_authors_raw"].as_str().unwrap().contains("Robin C. Williams"));
        assert_eq!(s.metadata["keywords_raw"],"ACEs; Adverse childhood experiences; Attachment; Parenting");
        assert_eq!(s.rights.license.as_deref(),Some("All rights reserved"));assert_eq!(s.rights.status,"restricted");
        assert_eq!(s.rights.rights_holders,["Canadian Paediatric Society"]);
        assert!(s.rights.restrictions[0].contains("single copies"));assert!(s.rights.terms_url.unwrap().ends_with("/copyright"));
        assert!(s.attachments.iter().any(|a|a["role"]=="linked_journal_pdf" && a["url"].as_str().unwrap().ends_with("pxz063.pdf")));
        assert!(s.relations.iter().any(|r|r.rel=="translation_of" && r.target.ends_with("parentales-positives")));
        assert!(s.metadata["source_links"].as_array().unwrap().iter().any(|l|l["url"].as_str().unwrap().ends_with("#ref1")));
        html=html.replace("</head>","<meta name='citation_doi' content='10.1093/pch/pxz063'></head>");
        assert_eq!(statement(&html,&base()).unwrap().doi.as_deref(),Some("10.1093/pch/pxz063"));
        assert_eq!(source_date("Updated","Feb 29, 2024",&base()).normalized.as_deref(),Some("2024-02-29"));
        assert!(source_date("Updated","Feb 29, 2025",&base()).normalized.is_none());
        assert!(source_date("Updated","not specified",&base()).normalized.is_none());
    }
    #[test]
    fn full_wrapper_keeps_order_table_recommendations_figure_and_notes() {
        let html=STATEMENT.replace("<h2>RECOMMENDATIONS</h2>","<figure id='algorithm'><img src='https://assets.example.org/clinical.jpg?signature=fixture' alt=''><figcaption>Figure 1. Clinical algorithm</figcaption></figure><aside id='note1'>* Source footnote.</aside><h2>RECOMMENDATIONS</h2>")
            .replace("</td>","<img src='/uploads/table-algorithm.png' alt='Table algorithm'></td>");
        let s=statement(&html,&base()).unwrap();
        let (mut blocks,_)=blocks_from_html(&s.html,Some(&base()),Some(".statement-wrapper"));
        recommendations(&s.html,&mut blocks);
        assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);
        assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),2);
        assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).count(),2);
        assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),caption:Some(c)} if u=="https://assets.example.org/clinical.jpg?signature=fixture" && c=="Figure 1. Clinical algorithm")));
        assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u=="https://cps.ca/uploads/table-algorithm.png")));
        assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Footnote) && b.text=="* Source footnote."));
        assert!(blocks.iter().any(|b|b.block_id=="ref1" && matches!(b.kind,BlockKind::ListItem)));
        let table=blocks.iter().position(|b|matches!(b.kind,BlockKind::Table{..})).unwrap();
        assert!(blocks[table].text.contains("Recommendations for family-centred guidance"));
        assert!(matches!(blocks[table+1].kind,BlockKind::Recommendation{strength:None,certainty:None,..}));
        let rec=blocks.iter().find(|b|b.text=="Model responsive communication and use motivational interviewing techniques.").unwrap();
        assert_eq!(rec.heading_path.last().map(String::as_str),Some("RECOMMENDATIONS"));
        assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Heading{..})).count(),6);
        assert!(!s.html.contains("test-your-knowledge-en.png"));
        assert!(statement(EMPTY,&base()).is_err());assert!(statement(RETIRED,&Url::parse("https://cps.ca/en/404/statement-retired").unwrap()).unwrap_err().to_string().contains("retired"));
    }
    #[tokio::test]
    async fn discovery_limits_zero_terminal_loops_and_body_changes_are_offline() {
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        assert!(CaCps.discover(&source(json!({})),&f,Some(0)).await.unwrap().is_empty());
        // Only first index and first statement are seeded: limit=1 must not fetch another item/page.
        f.seed_cache_for_tests(CATALOG,INDEX.as_bytes(),"text/html");f.seed_cache_for_tests(FIRST,STATEMENT.as_bytes(),"text/html");
        let one=CaCps.discover(&source(json!({})),&f,Some(1)).await.unwrap();assert_eq!(one.len(),1);
        assert_eq!(one[0].extra["discovery_coverage"]["complete"],false);
        seed(&f);let rows=CaCps.discover(&source(json!({})),&f,None).await.unwrap();assert_eq!(rows.len(),3);
        assert_eq!(rows[2].extra["discovery_coverage"]["complete"],true);assert_eq!(rows[0].extra["discovery_coverage"]["catalog_pages"],2);
        let doc=CaCps.extract(&source(json!({})),&f,&rows[0]).await.unwrap();
        assert_eq!(doc.status,"unknown");assert_eq!(doc.published.as_deref(),Some("2019-07-25"));assert!(doc.updated.is_none());assert!(doc.version.is_none());
        assert_eq!(doc.date_observations.len(),2);assert!(doc.doc_key.contains(&doc.search_metadata["clinical_sha256"].as_str().unwrap()[..12]));
        f.seed_cache_for_tests(FIRST,STATEMENT.replace("Model responsive communication","Model changed communication").as_bytes(),"text/html");
        assert!(CaCps.extract(&source(json!({})),&f,&rows[0]).await.unwrap_err().to_string().contains("changed"));
        assert!(CaCps.discover(&source(json!({"max_catalog_pages":1})),&f,None).await.unwrap_err().to_string().contains("incomplete"));
        f.seed_cache_for_tests(&format!("{CATALOG}/P10"),PAGE2.replace("/P20","").as_bytes(),"text/html");
        assert!(CaCps.discover(&source(json!({})),&f,None).await.unwrap_err().to_string().contains("loop"));
        f.seed_cache_for_tests(FIRST,EMPTY.as_bytes(),"text/html");
        let failures=CaCps.discover(&source(json!({})),&f,Some(1)).await.unwrap();assert!(failures[0].extra["discovery_error"].is_string());
        assert!(CaCps.extract(&source(json!({})),&f,&failures[0]).await.is_err());
    }
}

#[async_trait::async_trait]
impl Adapter for CaCps {
    async fn discover(&self, src:&SourceEntry, f:&Fetcher, limit:Option<usize>) -> Result<Vec<PlanRow>> {
        if limit==Some(0) { return Ok(vec![]); }
        let mut url=src.catalog_url.clone().unwrap_or_else(||CATALOG.into());
        if !index_url(&Url::parse(&url)?) { bail!("CPS catalog_url must be the English all-years date index"); }
        let max_pages=src.config["max_catalog_pages"].as_u64().unwrap_or(500);
        if max_pages==0 { bail!("CPS max_catalog_pages must be positive"); }
        let mut pages=HashSet::new(); let mut seen=HashSet::new(); let mut out=vec![]; let mut fetches=vec![];
        let (complete, stop_reason) = loop {
            if pages.len() as u64>=max_pages { bail!("CPS discovery incomplete: max_catalog_pages cap {max_pages} reached with Next remaining"); }
            if !pages.insert(url.clone()) { bail!("CPS discovery pagination loop at {url}"); }
            let page=f.get_ok(&url).await?;
            let base=Url::parse(&page.record.final_url)?;
            if base.as_str()!=url && pages.contains(base.as_str()) { bail!("CPS discovery redirected into a visited page: {base}"); }
            let list=listing(&page.text(),&base)?; fetches.push(page.record.clone());
            let before=out.len(); let page_len=list.rows.len();
            for (i,(u,title,metadata)) in list.rows.into_iter().enumerate() {
                if !seen.insert(u.clone()) { continue; }
                let work=Url::parse(&u)?.path().trim_end_matches('/').strip_prefix(POSITION_PATH).context("CPS work slug missing")?.to_string();
                let mut row=PlanRow { source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:u.clone(),title:Some(title),rendition:Some("html".into()),version:None,
                    extra:json!({"work_key":work,"catalog_url":base.as_str(),"catalog_metadata_raw":metadata}) };
                match f.get_ok(&u).await {
                    Ok(p)=>{
                        row.extra["discovery_document_fetch"]=json!(p.record);
                        match statement(&p.text(),&Url::parse(&p.record.final_url)?) {
                            Ok(s)=>{row.doc_key=format!("{work}-{}",&s.sha256[..12]);row.extra["clinical_sha256"]=json!(s.sha256);},
                            Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                        }
                    },
                    Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                }
                out.push(row);
                if limit.is_some_and(|n|out.len()>=n) {
                    let terminal=i+1==page_len && list.next.is_none();
                    for r in &mut out { r.extra["discovery_coverage"]=json!({"complete":terminal,"stop_reason":if terminal{"terminal_next_absent"}else{"requested_limit"},"catalog_pages":fetches.len(),"unique_selected":seen.len()});r.extra["catalog_fetches"]=json!(fetches); }
                    return Ok(out);
                }
            }
            if out.len()==before { bail!("CPS discovery page adds no unique statements; shifted or repeating catalog at {base}"); }
            match list.next { Some(next)=>url=next,None=>break (true,"terminal_next_absent") }
        };
        for r in &mut out { r.extra["discovery_coverage"]=json!({"complete":complete,"stop_reason":stop_reason,"catalog_pages":fetches.len(),"unique_selected":seen.len()});r.extra["catalog_fetches"]=json!(fetches); }
        Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str() { bail!("CPS discovery document failure: {e}"); }
        let page=f.get_ok(&row.url).await?; let base=Url::parse(&page.record.final_url)?; let s=statement(&page.text(),&base)?;
        let planned=row.extra["clinical_sha256"].as_str().context("CPS plan missing statement wrapper hash; re-plan")?;
        if planned!=s.sha256 { bail!("CPS statement changed since discovery; re-plan"); }
        let (mut blocks,mut warnings)=blocks_from_html(&s.html,Some(&base),Some(".statement-wrapper"));
        let (recommendation_warnings,recommendation_groups)=recommendations(&s.html,&mut blocks);
        warnings.extend(recommendation_warnings);
        if blocks.is_empty() { bail!("CPS statement yielded no clinical blocks"); }
        if row.extra["discovery_coverage"]["complete"]!=true { warnings.push("CPS plan covers a requested subset; English catalog completeness is not established for this plan".into()); }
        let published=s.dates.iter().find(|d| d.label.eq_ignore_ascii_case("Posted")).and_then(|d|d.normalized.clone());
        let updated=s.dates.iter().find(|d| d.label.eq_ignore_ascii_case("Updated")).and_then(|d|d.normalized.clone());
        let mut fetches:Vec<FetchRecord>=serde_json::from_value(row.extra["catalog_fetches"].clone()).unwrap_or_default();
        if let Ok(p)=serde_json::from_value::<FetchRecord>(row.extra["discovery_document_fetch"].clone()) { fetches.push(p); } fetches.push(page.record);
        let mut metadata=s.metadata; metadata["recommendation_groups"]=json!(recommendation_groups);metadata["discovery_coverage"]=row.extra["discovery_coverage"].clone();metadata["catalog_metadata_raw"]=row.extra["catalog_metadata_raw"].clone();
        Ok(GuidelineDoc { source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:s.title,url:base.to_string(),
            item_type:s.item_type,publisher:s.publisher,issuers:s.issuers,status:"unknown".into(),published,updated,date_observations:s.dates,language:s.language,doi:s.doi,
            rights:s.rights,relations:s.relations,search_metadata:metadata,blocks,attachments:s.attachments,fetches,warnings,..Default::default() })
    }
}
