//! PHAC's English STBBI guide/chapter graph and source-labelled clinical panels.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, DateObservation, GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use url::Url;

pub struct PhacSti;
const MAIN: &str = "main[property='mainContentOfPage']";
const PREFIX: &str = "/en/public-health/services/infectious-diseases/sexual-health-sexually-transmitted-infections/canadian-guidelines/";
const TERMS: &str = "https://www.canada.ca/en/transparency/terms.html";

fn clinical(html: &str, base: &Url) -> Result<(String, String, Vec<DateObservation>)> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(MAIN)).next().context("PHAC main clinical root missing")?;
    let title=root.select(&sel("h1#wb-cont")).next().map(|e|text_of(&e)).context("PHAC title missing")?;
    let methods=base.path().ends_with("/levels-quality-evidence.html") && title=="Levels and Quality of Evidence"
        && root.select(&sel(".mwsbodytext h2")).any(|h|text_of(&h)=="Levels of recommendations")
        && root.select(&sel(".mwsbodytext h2")).any(|h|text_of(&h)=="Quality of Evidence");
    if (!methods && root.select(&sel(".mwsgeneric-base-html")).next().is_none()) || root.select(&sel("h2")).next().is_none() {
        bail!("PHAC full clinical fields/headings missing; refusing navigation fallback");
    }
    let mut clean=root.inner_html();
    for e in root.select(&sel("script, style, .gc-stp-stp, .pagedetails, .fn-rtn")) {clean=clean.replacen(&e.html(),"",1);}
    for h in root.select(&sel("h2")) {
        if text_of(&h)=="On this page" {
            clean=clean.replacen(&h.html(),"",1);
            if let Some(next)=h.next_siblings().filter_map(ElementRef::wrap).next() {
                if next.value().name()=="ul" {clean=clean.replacen(&next.html(),"",1);}
            }
        }
    }
    let mut dates=vec![];
    for name in ["dcterms.issued","dcterms.modified"] {
        for m in doc.select(&sel(&format!("meta[name='{name}'][content]"))) {
            dates.push(common::date(name,m.value().attr("content").unwrap(),base.as_str()));
        }
    }
    for s in root.select(&sel("summary")) {
        let text=text_of(&s);
        if let Some(raw)=text.strip_prefix("Last partial content update:") {
            dates.push(common::date("Last partial content update",raw.trim(),base.as_str()));
        }
    }
    Ok((title,format!("<div id='phac-clinical'>{clean}</div>"),dates))
}

fn links(html: &str, base: &Url) -> Result<Vec<(String,String)>> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(MAIN)).next().context("PHAC catalog/chapter root missing")?;
    let mut seen=HashSet::new();let mut out=vec![];
    for a in root.select(&sel(".mwsgeneric-base-html a[href]")) {
        let Some(url)=common::link(base,a.value().attr("href").unwrap()) else {continue};
        let u=Url::parse(&url)?;
        if u.origin()!=base.origin() || !u.path().starts_with(PREFIX) || !u.path().ends_with(".html") || u.query().is_some() {continue;}
        // Committee membership and workplans are administrative, not clinical guidance.
        if u.path().ends_with("/membership.html") || u.path().ends_with("/workplan.html") {continue;}
        if seen.insert(url.clone()) {out.push((url,text_of(&a)));}
    }
    Ok(out)
}

fn snapshot(title: &str, content: &str, dates: &[DateObservation]) -> Result<String> {
    Ok(crate::common::sha256_hex(serde_json::to_string(&(title,content,dates))?.as_bytes()))
}

fn terms(html: &str) -> Result<Rights> {
    let doc=Html::parse_document(html);
    let noncommercial=doc.select(&sel("h3#non-commercial")).next().context("Canada non-commercial terms heading missing")?;
    let commercial=doc.select(&sel("h3#commercial")).next().context("Canada commercial terms heading missing")?;
    let limits=commercial.next_siblings().filter_map(ElementRef::wrap).find(|e|e.value().name()=="p").map(|e|text_of(&e)).context("Canada commercial restriction missing")?;
    let requirements=noncommercial.next_siblings().filter_map(ElementRef::wrap).take_while(|e|e.value().name()!="h3").find(|e|matches!(e.value().name(),"ul"|"ol")).map(|e|text_of(&e)).context("Canada reproduction conditions missing")?;
    Ok(Rights{status:"unknown".into(),terms_url:Some(TERMS.into()),restrictions:vec![limits],attribution:Some(requirements),evidence_urls:vec![TERMS.into()],
        note:Some("Canada.ca conditions apply unless otherwise specified; per-item and third-party rights require review. Separate linked assets are not assigned these rights.".into()),..Default::default()})
}

/// Keep an entire regimen together, including PLUS/OR clauses and its own notes.
fn recommendations(html: &str, blocks: &mut Vec<Block>) -> Vec<String> {
    let doc=Html::parse_fragment(html);let mut pending=vec![];let mut warnings=vec![];
    let grades=regex::Regex::new(r"\[[ABC]-[A-Za-z0-9]+\]").unwrap();
    let mut cursor=0;
    for panel in doc.select(&sel(".panel.panel-primary, .well")) {
        let pop=panel.select(&sel(".panel-heading .panel-title")).next().map(|e|text_of(&e));
        let anchor=pop.clone().or_else(||panel.select(&sel("p")).next().map(|p|text_of(&p)));
        let Some(anchor)=anchor else {continue};
        let well=panel.value().classes().any(|c|c=="well");
        if well && !anchor.starts_with("Interim recommendation") {continue;}
        let Some((index,original))=blocks.iter().enumerate().skip(cursor).find(|(_,b)|b.text==anchor) else {
            warnings.push(format!("Could not associate a source-labelled clinical panel with its block: {anchor}"));continue;
        };
        cursor=index+1;
        let mut text=panel.select(&sel(".panel-body")).next().map(|b|text_of(&b)).unwrap_or_else(||text_of(&panel));
        let mut in_notes=false;
        for sibling in panel.next_siblings().filter_map(ElementRef::wrap) {
            let name=sibling.value().name();if name.starts_with('h') {break;}
            let t=text_of(&sibling);
            if t.starts_with("Notes:") || t.starts_with("Note:") {in_notes=true;}
            if in_notes && matches!(name,"p"|"ul"|"ol") {text.push('\n');text.push_str(&t);} else {break;}
        }
        let printed:HashSet<_>=grades.find_iter(&text).map(|m|m.as_str().to_string()).collect();
        let strength=if printed.len()==1 {printed.iter().next().cloned()}else{None};
        if printed.len()>1 {warnings.push(format!("Panel {anchor} contains different clause-specific ratings; source tokens remain attached to the complete regimen without assigning one grade."));}
        let mut path=original.heading_path.clone();
        if matches!(original.kind,BlockKind::Heading{..}) {path.push(original.text.clone());}
        pending.push((index,Block {block_id:format!("phac-panel-{}",pending.len()+1),kind:BlockKind::Recommendation{
            label:None,strength,certainty:None,population:pop,grading_system:None,qualifier:well.then_some(anchor)},
            text,heading_path:path,page:None}));
    }
    for (i,b) in pending.into_iter().rev() {blocks.insert(i+1,b);}
    warnings
}

#[async_trait::async_trait]
impl Adapter for PhacSti {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0) {return Ok(vec![]);}
        let catalog=src.catalog_url.as_deref().context("PHAC catalog missing")?;
        let page=f.get_ok(catalog).await?;
        let mut queue=VecDeque::from(links(&page.text(),&Url::parse(&page.record.final_url)?)?);
        let mut seen=HashSet::from([catalog.to_string()]);let mut out=vec![];
        while let Some((url,hint))=queue.pop_front() {
            if !seen.insert(url.clone()) {continue;}
            if seen.len()>500 {bail!("PHAC graph exceeded 500 nodes; inventory incomplete");}
            let u=Url::parse(&url)?;
            let work=common::slug(u.path().strip_prefix(PREFIX).unwrap_or(u.path()));
            let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url,title:Some(hint),rendition:Some("html".into()),version:None,extra:json!({"work_key":work})};
            match f.get_ok(&row.url).await {
                Ok(body)=>{
                    let base=Url::parse(&body.record.final_url)?;
                    if base.origin()!=u.origin() {row.extra["discovery_error"]=json!("PHAC redirect left source origin");}
                    else {match clinical(&body.text(),&base) {
                        Ok((title,clean,dates))=>{
                            let hash=snapshot(&title,&clean,&dates)?;row.title=Some(title);row.doc_key=format!("{work}-{}",&hash[..12]);row.extra["clinical_sha256"]=json!(hash);
                            queue.extend(links(&body.text(),&base)?);
                        },
                        Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                    }}
                },
                Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
            }
            out.push(row);if limit.is_some_and(|n|out.len()>=n) {return Ok(out);}
        }
        if out.is_empty() {bail!("PHAC catalog yielded no source-linked guide pages");}
        Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str() {bail!("PHAC discovery: {e}");}
        let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;
        if base.origin()!=Url::parse(&row.url)?.origin() {bail!("PHAC redirected outside source origin");}
        let (title,clean,dates)=clinical(&page.text(),&base)?;
        if row.extra["clinical_sha256"].as_str().is_some_and(|s|snapshot(&title,&clean,&dates).map(|v|v!=s).unwrap_or(true)) {bail!("PHAC clinical revision changed after planning; re-plan");}
        let (mut blocks,mut warnings)=blocks_from_html(&clean,Some(&base),Some("#phac-clinical"));
        warnings.extend(recommendations(&clean,&mut blocks));
        let (publisher,attachments,relations,source_notices,language,merged)={
        let parsed=Html::parse_document(&page.text());
        let publisher=parsed.select(&sel("meta[name='dcterms.creator'][content]")).next().and_then(|m|m.value().attr("content")).map(String::from);
        let mut attachments=vec![];let mut relations=vec![];let mut seen=HashSet::new();
        for a in parsed.select(&sel("main a[href], link[rel='alternate'][hreflang][href]")) {
            let Some(url)=a.value().attr("href").and_then(|h|common::link(&base,h)) else {continue};
            if !seen.insert(url.clone()) {continue;}
            let path=Url::parse(&url)?.path().to_lowercase();
            if path.ends_with(".pdf") || a.value().attr("hreflang").is_some() {attachments.push(json!({"url":url,"label":text_of(&a),"role":if a.value().attr("hreflang").is_some(){"language_rendition"}else{"linked_pdf"},"rights":"unknown"}));}
            else if path.contains("/national-advisory-committee-stbbi/") && path.contains("guidance") && url!=row.url {
                relations.push(Relation{rel:"related".into(),target:url,scope:Some(text_of(&a)),evidence_url:Some(row.url.clone())});
            }
        }
        let merged=parsed.select(&sel("main table [rowspan],main table [colspan]")).next().is_some();
        let source_notices=parsed.select(&sel("main .alert,main details summary")).map(|e|text_of(&e)).collect::<Vec<_>>();
        let language=common::page_language(&parsed);
        (publisher,attachments,relations,source_notices,language,merged)};
        if merged {warnings.push("Source tables contain merged cells; Markdown span semantics and footnote association require review.".into());}
        let mut rights=Rights{status:"unknown".into(),terms_url:Some(TERMS.into()),..Default::default()};
        let mut fetches=vec![page.record];
        match f.get_ok(TERMS).await {
            Ok(terms)=>{
                match self::terms(&terms.text()) {Ok(parsed)=>rights=parsed,Err(e)=>warnings.push(format!("Canada.ca reuse conditions could not be interpreted: {e:#}"))};
                fetches.push(terms.record);
            },
            Err(e)=>warnings.push(format!("PHAC reuse terms could not be acquired: {e:#}")),
        }
        warnings.push("Discovery is bounded to source-linked English STBBI guide routes. French renditions and external clinical assets are retained as links but not claimed acquired. Only publisher-labelled clinical panels are typed recommendations; other clinical instructions, evidence and footnotes remain in source order.".into());
        let item_type=if base.path().ends_with("/levels-quality-evidence.html") && title=="Levels and Quality of Evidence" {Some("guideline_methods".into())} else {title.to_lowercase().contains("guide:").then(||"guideline_chapter".into())};
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),
            item_type,publisher,status:"unknown".into(),published:dates.iter().find(|d|d.label=="dcterms.issued").and_then(|d|d.normalized.clone()),
            updated:dates.iter().find(|d|d.label=="dcterms.modified").and_then(|d|d.normalized.clone()),date_observations:dates,language,rights,
            search_metadata:json!({"source_notices":source_notices,"source_url":row.url}),relations,blocks,attachments,fetches,warnings,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const METHODS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_sti/levels-quality-evidence.html"));
    const HOME:&str="https://www.canada.ca/en/public-health/services/infectious-diseases/sexual-health-sexually-transmitted-infections/canadian-guidelines.html";
    const TREAT:&str="https://www.canada.ca/en/public-health/services/infectious-diseases/sexual-health-sexually-transmitted-infections/canadian-guidelines/gonorrhea/treatment-follow-up.html";
    const SCREEN:&str="https://www.canada.ca/en/public-health/services/infectious-diseases/sexual-health-sexually-transmitted-infections/canadian-guidelines/gonorrhea/screening-diagnostic-testing.html";
    const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_sti/catalog.html"));
    const T:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_sti/treatment.html"));
    const S:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_sti/screening.html"));
    const R:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_sti/terms.html"));
    #[tokio::test]
    async fn source_graph_regimens_partial_updates_tables_and_footnotes_offline() {
        let inventory=links(C,&Url::parse(HOME).unwrap()).unwrap();
        assert!(inventory.len()>10);
        assert!(inventory.iter().any(|(u,_)|u==TREAT));assert!(inventory.iter().any(|(u,_)|u==SCREEN));
        // Bound to two actual source anchors while independently checking the full saved catalog.
        let original=Html::parse_document(C);
        let anchors=[TREAT,SCREEN].iter().map(|url|original.select(&sel("a[href]")).find(|a|common::link(&Url::parse(HOME).unwrap(),a.value().attr("href").unwrap()).as_deref()==Some(*url)).unwrap().html()).collect::<String>();
        let bounded=format!("<main property='mainContentOfPage'><div class='mwsgeneric-base-html'>{anchors}</div></main>");
        let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,1<<20,"").unwrap();
        for (url,body) in [(HOME,bounded.as_str()),(TREAT,T),(SCREEN,S),(TERMS,R)] {f.seed_cache_for_tests(url,body.as_bytes(),"text/html");}
        let src:SourceEntry=serde_json::from_value(json!({"id":"phac_sti","access":"public","enabled":true,"catalog_url":HOME})).unwrap();
        let rows=PhacSti.discover(&src,&f,Some(2)).await.unwrap();assert_eq!(rows.len(),2);
        assert!(rows[0].doc_key.starts_with("gonorrhea-treatment-follow-up-"));
        let d=PhacSti.extract(&src,&f,&rows[0]).await.unwrap();
        assert_eq!(d.title,"Gonorrhea guide: Treatment and follow-up");assert_eq!(d.status,"unknown");assert!(d.version.is_none());
        assert_eq!(d.publisher.as_deref(),Some("Public Health Agency of Canada"));
        assert_eq!(d.published.as_deref(),Some("2021-12-09"));assert_eq!(d.updated.as_deref(),Some("2026-06-04"));
        assert!(d.date_observations.iter().any(|d|d.label=="Last partial content update"&&d.normalized.as_deref()==Some("2026-06")));
        let recs=d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect::<Vec<_>>();
        assert_eq!(recs.len(),8,"one explicitly interim box and seven complete regimen panels");
        assert!(recs.iter().any(|b|b.text.contains("Ceftriaxone 500 mg IM")&&b.text.contains("C. trachomatis")&&b.text.contains("Test of cure")));
        assert!(recs.iter().any(|b|b.text.contains("PLUS")&&b.text.contains("OR")&&b.text.contains("[A-l]")&&b.text.contains("[B-III]")));
        assert!(recs.iter().any(|b|b.text.contains("Gentamicin")&&b.text.contains("not recommended in pregnancy")));
        assert!(d.warnings.iter().any(|w|w.contains("different clause-specific ratings")));
        assert!(d.blocks.iter().any(|b|b.text.contains("Bignell C")&&b.text.contains("10.1136/sti.2010.044586")));
        assert!(!d.blocks.iter().any(|b|b.text=="On this page"||b.text=="Language selection"));
        assert!(d.relations.iter().any(|r|r.rel=="related"&&r.target.contains("interim-guidance-treatment")));
        assert_eq!(d.rights.status,"unknown");assert!(d.rights.restrictions.iter().any(|s|s.contains("commercial redistribution")));
        assert!(d.rights.attribution.as_ref().is_some_and(|s|s.contains("version")));
        let s=PhacSti.extract(&src,&f,&rows[1]).await.unwrap();
        assert_eq!(s.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),4);
        assert!(s.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..})&&b.text=="Diagnostic testing"));
        assert_eq!(s.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),0);
        let mut stale=rows[0].clone();stale.extra["clinical_sha256"]=json!("different");
        assert!(PhacSti.extract(&src,&f,&stale).await.unwrap_err().to_string().contains("changed after planning"));
    }
    #[test]
    fn actual_evidence_methods_page_is_substantive_and_not_navigation() {
        let url=Url::parse("https://www.canada.ca/en/public-health/services/infectious-diseases/sexual-health-sexually-transmitted-infections/canadian-guidelines/levels-quality-evidence.html").unwrap();
        let (title,body,_) = clinical(METHODS,&url).unwrap();assert_eq!(title,"Levels and Quality of Evidence");
        let (blocks,_)=blocks_from_html(&body,Some(&url),Some("#phac-clinical"));
        assert!(blocks.iter().any(|b|b.text=="Levels of recommendations"));
        assert!(blocks.iter().any(|b|b.text=="Quality of Evidence"));
        assert!(blocks.iter().any(|b|b.text.contains("Level III (three)")));
        let broken=METHODS.replace("Quality of Evidence</h2>","Other</h2>");
        assert!(clinical(&broken,&url).is_err());
    }

}
