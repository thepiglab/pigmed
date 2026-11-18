//! Hypertension Canada source-emitted sidebar and clinical pages.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct HypertensionCanada;
const ROOT:&str="https://hypertension.ca/guidelines/recommendations";
fn sidebar(html:&str,base:&Url)->Result<(Vec<(String,String)>,Vec<Value>)> {
    let doc=Html::parse_document(html);let nav=doc.select(&sel("#menu-guidelines-sidebar")).next().context("Hypertension Canada sidebar missing")?;
    let mut seen=HashSet::new();let mut pages=vec![];let mut assets=vec![];
    for a in nav.select(&sel("a[href]")) {
        let title=text_of(&a);let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};
        if !seen.insert(url.clone()){continue}let u=Url::parse(&url)?;
        if u.origin()==base.origin() {pages.push((url,title));}
        else {let role=if title.contains("2025 Primary Care Guideline") {"primary_care_pdf"} else if title.contains("Patient/Public") {"patient_guideline"} else {"external_support"};assets.push(json!({"url":url,"title":title,"role":role,"rights":"unknown; independently hosted asset"}));}
    }
    if !pages.iter().any(|(_,t)|t=="Recommendations") || !pages.iter().any(|(_,t)|t=="Treatment") || !pages.iter().any(|(_,t)|t=="Diagnosis") {bail!("Hypertension Canada sidebar missing clinical chapter links");}
    Ok((pages,assets))
}
fn clinical(html:&str)->Result<(String,String,Option<String>)> {
    let doc=Html::parse_document(html);let title=doc.select(&sel("main#page-content h1.entry-title")).next().map(|e|text_of(&e)).filter(|x|!x.is_empty()).context("Hypertension Canada source title missing")?;
    let nav=doc.select(&sel("main#page-content #menu-guidelines-sidebar")).next().context("Hypertension Canada chapter navigation missing")?;
    let section=nav.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().name()=="section").context("Hypertension Canada clinical section missing")?;
    let sidebar_column=nav.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().classes().any(|c|c=="wpb_column")).context("Hypertension Canada sidebar column missing")?;
    let mut body=section.html();body=body.replacen(&sidebar_column.html(),"",1);
    let check=Html::parse_fragment(&body);let root=check.select(&sel("section")).next().context("empty clinical section")?;
    if root.select(&sel(".wpb_text_column p, table")).next().is_none(){bail!("Hypertension Canada clinical content missing");}
    // The publisher formats these clinical subsection labels as bold/italic
    // paragraphs. Promote only exact, standalone source labels to headings.
    for p in root.select(&sel(".wpb_text_column p")) {
        let text=text_of(&p);
        if (text=="Rationale" || text=="Values and preferences") && p.select(&sel("strong em, em strong")).next().is_some() {
            body=body.replacen(&p.html(),&format!("<h3>{text}</h3>"),1);
        }
    }
    let copyright=doc.select(&sel("footer p")).map(|p|text_of(&p)).find(|t|t.contains("Hypertension Canada") && t.contains("All rights reserved") && t.contains("Unauthorized"));
    Ok((title,format!("<div id='clinical'>{body}</div>"),copyright))
}
fn structured(html:&str,blocks:&mut Vec<Block>)->Vec<String> {
    let doc=Html::parse_fragment(html);let mut additions=vec![];let mut warnings=vec![];
    for table in doc.select(&sel("table")) {
        let hs:Vec<_>=table.select(&sel("thead th")).map(|x|text_of(&x)).collect();
        if hs.len()!=3 || !hs[0].contains("Recommendation") || !hs[1].contains("Strength") || !hs[2].contains("Certainty") {continue;}
        let Some(id)=table.value().id() else {warnings.push("Recommendation table lacks stable publisher locator; table retained.".into());continue};
        let Some((idx,b))=blocks.iter().enumerate().find(|(_,b)|b.block_id==id) else {warnings.push("Recommendation table could not be mapped to extracted block; table retained.".into());continue};
        let mut group=String::new();
        for tr in table.select(&sel("tbody tr")) {
            let cells:Vec<_>=tr.select(&sel("td")).collect();
            if cells.len()==1 {group=text_of(&cells[0]);continue;}
            if cells.len()!=3 {warnings.push("Recommendation table row has unexpected cell count; source table retained.".into());continue;}
            let text=text_of(&cells[0]);let strength=text_of(&cells[1]);let certainty=text_of(&cells[2]);
            if text.is_empty()||strength.is_empty()||certainty.is_empty(){warnings.push("Recommendation table row has missing cells; source table retained.".into());continue;}
            let mut path=b.heading_path.clone();if !group.is_empty(){path.push(group.clone());}
            let rowid=tr.value().id().unwrap_or("unidentified");
            additions.push((idx,Block{block_id:format!("{id}-{rowid}-recommendation"),kind:BlockKind::Recommendation{label:None,strength:Some(strength),certainty:Some(certainty),population:None,grading_system:None,qualifier:None},text,heading_path:path,page:None}));
        }
    }
    let re=regex::Regex::new(r"(?i)\((strong|conditional) recommendation,\s*(very low|low|moderate|high)-certainty evidence\)").unwrap();
    for p in doc.select(&sel(".wpb_text_column p")) {
        let text=text_of(&p);let found:Vec<_>=re.captures_iter(&text).collect();if found.len()!=1 {continue;}
        if p.select(&sel("strong em,em strong")).next().is_none(){continue;}
        let Some((idx,b))=blocks.iter().enumerate().find(|(_,b)|b.text==text) else {warnings.push("Explicitly graded clinical paragraph could not be mapped; source retained.".into());continue};
        additions.push((idx,Block{block_id:format!("{}-recommendation",b.block_id),kind:BlockKind::Recommendation{label:None,strength:Some(format!("{} recommendation",&found[0][1])),certainty:Some(format!("{}-certainty evidence",&found[0][2])),population:None,grading_system:None,qualifier:None},text,heading_path:b.heading_path.clone(),page:None}));
    }
    additions.sort_by_key(|(i,_)|*i);for (i,b) in additions.into_iter().rev(){blocks.insert(i+1,b);}
    warnings.push("Only explicit table cells and unambiguous local paragraph grades are structured; full recommendation and figure/layout coverage require source validation.".into());warnings
}
#[async_trait::async_trait]
impl Adapter for HypertensionCanada {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0){return Ok(vec![])}let catalog=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;
        let (pages,assets)=sidebar(&catalog.text(),&Url::parse(&catalog.record.final_url)?)?;let mut out=vec![];
        for (url,title) in pages {
            let key=common::slug(Url::parse(&url)?.path());let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-unresolved"),url:url.clone(),title:Some(title.clone()),rendition:Some("html".into()),version:None,extra:json!({"work_key":key,"sidebar_title":title,"catalog_fetch":&catalog.record,"external_assets":assets})};
            match f.get_ok(&url).await {Ok(p)=>match clinical(&p.text()){Ok((_,body,_))=>{let h=crate::common::sha256_hex(body.as_bytes());row.doc_key=format!("{key}-{}",&h[..12]);row.extra["clinical_sha256"]=json!(h);},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
            out.push(row);if limit.is_some_and(|n|out.len()>=n){return Ok(out);}
        }
        for asset in &assets {
            if asset["role"]!="primary_care_pdf" {continue;}let Some(url)=asset["url"].as_str() else {continue};
            let mut row=PlanRow{source_id:src.id.clone(),doc_key:"primary-care-2025-unresolved".into(),url:url.into(),title:Some("2025 Primary Care Guideline".into()),rendition:Some("pdf".into()),version:Some("2025".into()),extra:json!({"work_key":"primary-care-2025","role":"source_labelled_primary_care_pdf","catalog_fetch":&catalog.record})};
            match f.get_ok(url).await {Ok(p) if p.is_pdf()=>{row.doc_key=format!("primary-care-2025-{}",&p.record.sha256[..12]);row.extra["pdf_sha256"]=json!(p.record.sha256);},Ok(_)=>row.extra["discovery_error"]=json!("Source-labelled primary care asset did not return PDF"),Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))};
            out.push(row);if limit.is_some_and(|n|out.len()>=n){break;}
        }
        Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("Hypertension Canada discovery: {e}");}let page=f.get_ok(&row.url).await?;
        if row.rendition.as_deref()==Some("pdf") {
            if !page.is_pdf(){bail!("Primary care document is not a PDF");}
            if row.extra["pdf_sha256"].as_str().is_some_and(|h|h!=page.record.sha256){bail!("Primary care PDF changed since discovery; re-plan");}
            let (blocks,mut warnings,_)=blocks_for(&page,None,30).await?;if blocks.is_empty(){bail!("Primary care PDF yielded no text");}
            warnings.push("Full primary-care PDF is a distinct CMAJ publisher rendition; rights, recommendation boundaries, tables, footnotes and figures require independent review.".into());
            let mut fetches=vec![];if let Ok(c)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(c);}fetches.push(page.record);
            return Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some("guideline".into()),version:row.version.clone(),status:"unknown".into(),rights:Rights{status:"unknown".into(),note:Some("External CMAJ publisher rights require independent review; source sidebar labels this the 2025 primary-care guideline.".into()),..Default::default()},blocks,fetches,warnings,..Default::default()});
        }
        let (title,html,copyright)=clinical(&page.text())?;
        if row.extra["clinical_sha256"].as_str().is_some_and(|s|s!=crate::common::sha256_hex(html.as_bytes())) {bail!("Hypertension Canada clinical content changed since discovery; re-plan");}
        let base=Url::parse(&page.record.final_url)?;let (mut blocks,mut warnings)=blocks_from_html(&html,Some(&base),Some("#clinical"));warnings.extend(structured(&html,&mut blocks));
        warnings.push("Source sidebar links the 2025 primary-care PDF and patient resource; independent host and asset rights are unknown and those PDFs were not acquired. Do not infer publication dates or the comprehensive guideline status from these pages.".into());
        let rights=if let Some(c)=copyright {Rights{status:"restricted".into(),restrictions:vec![c],evidence_urls:vec![page.record.final_url.clone()],note:Some("Site footer notice; external journal and linked-asset rights require separate review.".into()),..Default::default()}} else {Rights{status:"unknown".into(),..Default::default()}};
        let mut fetches=vec![];if let Ok(c)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(c);}fetches.push(page.record.clone());
        let kind=match row.extra["sidebar_title"].as_str(){Some("Recommendations")=>Some("recommendation_summary".into()),Some("Diagnosis"|"Treatment")=>Some("guideline_chapter".into()),_=>None};
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:page.record.final_url.clone(),item_type:kind,status:"unknown".into(),rights,blocks,attachments:row.extra["external_assets"].as_array().cloned().unwrap_or_default(),fetches,warnings,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const PRIMARY_PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_hypertension_canada/primary-care-page-1.pdf"));
    const INDEX:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_hypertension_canada/recommendations.html"));
    const DIAGNOSIS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_hypertension_canada/diagnosis.html"));
    const TREATMENT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ca_hypertension_canada/treatment.html"));
    #[test]
    fn genuine_sidebar_preserves_source_pdf_and_navigation() {
        let (pages,assets)=sidebar(INDEX,&Url::parse(ROOT).unwrap()).unwrap();
        assert!(pages.iter().any(|(u,t)|t=="Diagnosis" && u=="https://hypertension.ca/diagnosis"));
        assert!(pages.iter().any(|(u,t)|t=="Treatment" && u=="https://hypertension.ca/guidelines/treatment"));
        assert!(pages.iter().any(|(_,t)|t=="Methods"));
        assert!(assets.iter().any(|a|a["role"]=="primary_care_pdf" && a["url"]=="https://www.cmaj.ca/content/cmaj/197/20/E549.full.pdf"));
        assert!(assets.iter().any(|a|a["role"]=="patient_guideline"));
    }
    #[test]
    fn authentic_table_keeps_nine_graded_recommendations() {
        let (_,html,_)=clinical(INDEX).unwrap();let (mut blocks,_)=blocks_from_html(&html,Some(&Url::parse(ROOT).unwrap()),Some("#clinical"));structured(&html,&mut blocks);
        let rows:Vec<_>=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();assert_eq!(rows.len(),9);
        assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})));
        let diagnosis=rows.iter().find(|b|b.text.contains("BP ≥ 130/80 mm Hg")).unwrap();
        assert!(matches!(&diagnosis.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Strong" && c=="Moderate"));
        assert!(diagnosis.heading_path.iter().any(|h|h=="Diagnosis"));
        let spiro=rows.iter().find(|b|b.text.contains("addition of spironolactone")).unwrap();
        assert!(spiro.text.contains("3-drug") && spiro.text.contains("maximally tolerated doses"));
        assert!(matches!(&spiro.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Conditional" && c=="Moderate"));
        assert!(spiro.heading_path.iter().any(|h|h=="Treatment"));
    }
    #[test]
    fn genuine_chapters_preserve_clinical_text_and_figure() {
        for (source,word) in [(DIAGNOSIS,"Out-of-office BP assessment"),(TREATMENT,"Pharmacotherapy initiation")] {
            let (_,html,copyright)=clinical(source).unwrap();assert!(copyright.is_some());
            let (mut blocks,_)=blocks_from_html(&html,Some(&Url::parse(ROOT).unwrap()),Some("#clinical"));structured(&html,&mut blocks);
            assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text=="Rationale"));
            let rec=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.text.contains(word)).unwrap();
            assert!(matches!(&rec.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="strong recommendation" && c.contains("certainty evidence")));
            if word=="Out-of-office BP assessment" {assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u.contains("Picture1.gif"))));}
        }
        assert!(clinical("<main>Sign in</main>").is_err());
    }
    #[tokio::test]
    async fn actual_pages_offline_and_revision_verification() {
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(ROOT,INDEX.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.cmaj.ca/content/cmaj/197/20/E549.full.pdf",PRIMARY_PDF,"application/pdf");
        f.seed_cache_for_tests("https://hypertension.ca/diagnosis",DIAGNOSIS.as_bytes(),"text/html");
        f.seed_cache_for_tests("https://hypertension.ca/guidelines/treatment",TREATMENT.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"ca_hypertension_canada","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let (pages,_)=sidebar(INDEX,&Url::parse(ROOT).unwrap()).unwrap();
        for limit in [0,1,2,3,pages.len()] {
            let bounded=HypertensionCanada.discover(&src,&f,Some(limit)).await.unwrap();assert_eq!(bounded.len(),limit,"limit {limit}");
            if limit>0 {assert!(bounded.iter().all(|r|r.extra["external_assets"].as_array().is_some_and(|a|a.iter().any(|v|v["role"]=="primary_care_pdf"))),"bounded HTML rows retain source asset roles");}
        }
        let rows=HypertensionCanada.discover(&src,&f,None).await.unwrap();let r=rows.iter().find(|r|r.url==ROOT).unwrap();
        let d=HypertensionCanada.extract(&src,&f,r).await.unwrap();assert_eq!(d.status,"unknown");assert!(d.version.is_none());
        let pdf=rows.iter().find(|r|r.rendition.as_deref()==Some("pdf")).unwrap();let pd=HypertensionCanada.extract(&src,&f,pdf).await.unwrap();
        assert_eq!(pd.version.as_deref(),Some("2025"));assert_eq!(pd.rights.status,"unknown");assert_eq!(pd.item_type.as_deref(),Some("guideline"));
        assert!(pd.blocks.iter().any(|b|b.text.to_lowercase().contains("hypertension canada")));assert!(pd.warnings.iter().any(|w|w.contains("distinct CMAJ")));
        let mut modified=PRIMARY_PDF.to_vec();modified.extend_from_slice(b"\nchanged snapshot");f.seed_cache_for_tests("https://www.cmaj.ca/content/cmaj/197/20/E549.full.pdf",&modified,"application/pdf");assert!(HypertensionCanada.extract(&src,&f,pdf).await.is_err());

        assert_eq!(d.rights.status,"restricted");assert!(d.attachments.iter().any(|a|a["role"]=="primary_care_pdf" && a["rights"].as_str().is_some_and(|s|s.contains("unknown"))));
        let changed=INDEX.replace("Healthy lifestyle changes are recommended for all adults with hypertension.","Healthy lifestyle changes are recommended for some adults with hypertension.");assert_ne!(changed,INDEX);
        f.seed_cache_for_tests(ROOT,changed.as_bytes(),"text/html");assert!(HypertensionCanada.extract(&src,&f,r).await.is_err());
    }
}
