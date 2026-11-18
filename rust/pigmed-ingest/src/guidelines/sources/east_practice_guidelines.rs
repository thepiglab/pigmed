//! EAST publisher guideline inventory and full HTML articles.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct East;
const ROOT:&str="https://www.east.org/education-resources/practice-management-guidelines";
fn catalog(html:&str,base:&Url)->Result<Vec<(String,String,String)>> {
    let doc=Html::parse_document(html);let root=doc.select(&sel(".guidelines-browse")).next().context("EAST browse inventory missing")?;
    let mut seen=HashSet::new();let mut out=vec![];
    for a in root.select(&sel("a[href]")) {let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};let u=Url::parse(&url)?;
        if u.origin()!=base.origin() || !u.path().starts_with("/education-resources/practice-management-guidelines/details/") || !seen.insert(url.clone()){continue;}
        let year=a.select(&sel("span.mute")).next().map(|x|text_of(&x)).unwrap_or_default();let title=text_of(&a);out.push((url,title,year));}
    if out.is_empty(){bail!("EAST browse contains no guideline links")}Ok(out)
}
fn clinical(html:&str,base:&Url)->Result<(String,String,String,Vec<Value>,Option<String>)> {
    let doc=Html::parse_document(html);let root=doc.select(&sel(".outlet_right_large")).next().context("EAST article wrapper missing")?;
    let title=root.select(&sel("h1")).next().map(|h|text_of(&h)).filter(|x|!x.is_empty()).context("EAST article title missing")?;
    let body=root.select(&sel("#pmg-body")).next().context("EAST guideline body missing")?;
    if body.select(&sel("h2")).next().is_none(){bail!("EAST guideline body heading missing")}
    let date=root.select(&sel("p")).map(|p|text_of(&p)).find(|x|x.starts_with("Published ")).unwrap_or_default();
    let mut assets=vec![];for a in root.select(&sel("a.btn[href]")){if let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)){assets.push(json!({"url":url,"title":text_of(&a),"role":"publisher_download","rights":"unknown; asset uninspected"}));}}
    let mut content=body.html();
    // Promote exact publisher level labels, leaving level definitions in their
    // original method section and retaining all source wording.
    let level=regex::Regex::new(r"(?i)^Level\s+[123](?:\s+(?:Standards|Guidelines|Recommendations))?$").unwrap();
    for p in body.select(&sel("p")){let t=text_of(&p);if level.is_match(&t) && p.select(&sel("strong,b")).next().is_some(){content=content.replacen(&p.html(),&format!("<h4>{t}</h4>"),1);}}
    let notice=doc.select(&sel("footer li, .list_block")).map(|e|text_of(&e)).find(|s|s.contains("Eastern Association for the Surgery of Trauma") && s.contains("All rights reserved"));
    Ok((title,content,date,assets,notice))
}
fn terms(html:&str)->Result<Vec<String>> {
    let doc=Html::parse_document(html);let root=doc.select(&sel(".outlet_right_large")).next().context("EAST terms root missing")?;
    if !root.select(&sel("h1")).any(|h|text_of(&h).eq_ignore_ascii_case("Terms of use")){bail!("EAST terms title missing");}
    let text=text_of(&root);let rule="You may not obtain or attempt to obtain any materials or information through any means not intentionally made available or provided for through the EAST Websites.";
    if !text.contains(rule){bail!("EAST source access restriction not recognized");}Ok(vec![rule.into()])
}
fn recommendations(blocks:&mut Vec<Block>)->Vec<String> {
    let level=regex::Regex::new(r"(?i)^Level\s+[123](?:\s+(?:Standards|Guidelines|Recommendations))?$").unwrap();let mut out=vec![];
    for (i,b) in blocks.iter().enumerate(){if !matches!(b.kind,BlockKind::ListItem){continue;}
        let Some(rec_i)=b.heading_path.iter().position(|h|h.eq_ignore_ascii_case("Recommendations")) else {continue};
        let Some(strength)=b.heading_path.iter().skip(rec_i+1).rev().find(|h|level.is_match(h)) else {continue};
        out.push((i,Block{block_id:format!("{}-recommendation",b.block_id),kind:BlockKind::Recommendation{label:None,strength:Some(strength.clone()),certainty:None,population:None,grading_system:None,qualifier:None},text:b.text.clone(),heading_path:b.heading_path.clone(),page:b.page}));}
    // Newer GRADE-format articles place final explicit recommendations in
    // paragraphs beneath PICO sections, separate from voting and rationale.
    let direct=regex::Regex::new(r"(?i)\bwe (?:conditionally )?recommend(?: against)? [^.]+\.").unwrap();
    for (i,b) in blocks.iter().enumerate() {
        if !matches!(b.kind,BlockKind::Paragraph) || !b.heading_path.iter().any(|h|h=="Recommendation") || !b.heading_path.iter().any(|h|h.contains("PICO ")){continue;}
        for (j,m) in direct.find_iter(&b.text).enumerate() {let text=m.as_str().to_string();let strength=if text.to_lowercase().starts_with("we conditionally recommend") {Some("conditionally recommend".into())} else {None};
            out.push((i,Block{block_id:format!("{}-pico-recommendation-{}",b.block_id,j+1),kind:BlockKind::Recommendation{label:None,strength,certainty:None,population:None,grading_system:None,qualifier:None},text,heading_path:b.heading_path.clone(),page:b.page}));}
    }
    out.sort_by_key(|(i,_)|*i);
    for (i,b) in out.into_iter().rev(){blocks.insert(i+1,b);}
    vec!["Only list items under a source Recommendations heading and local level heading are structured; evidence classes, nested lists, newer GRADE templates and full source coverage require review.".into()]
}
#[async_trait::async_trait]
impl Adapter for East {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0){return Ok(vec![])}let p=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;let entries=catalog(&p.text(),&Url::parse(&p.record.final_url)?)?;let mut rows=vec![];
        for (url,title,year) in entries {let work=common::slug(Url::parse(&url)?.path().rsplit('/').next().unwrap_or_default());let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:url.clone(),title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"catalog_year":year,"catalog_fetch":&p.record})};
            match f.get_ok(&url).await {Ok(q)=>match clinical(&q.text(),&Url::parse(&q.record.final_url)?) {Ok((title,body,date,_,_))=>{let h=crate::common::sha256_hex(format!("{title}\n{date}\n{body}").as_bytes());row.doc_key=format!("{work}-{}",&h[..12]);row.extra["clinical_sha256"]=json!(h);},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
            rows.push(row);if limit.is_some_and(|n|rows.len()>=n){break;}}
        Ok(rows)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("EAST discovery: {e}")}let p=f.get_ok(&row.url).await?;let base=Url::parse(&p.record.final_url)?;
        let (title,html,date,assets,notice)=clinical(&p.text(),&base)?;let hash=crate::common::sha256_hex(format!("{title}\n{date}\n{html}").as_bytes());if row.extra["clinical_sha256"].as_str().is_some_and(|s|s!=hash){bail!("EAST content changed since discovery; re-plan")}
        let (mut blocks,mut warnings)=blocks_from_html(&html,Some(&base),Some("#pmg-body"));warnings.extend(recommendations(&mut blocks));warnings.push("Linked journal/PDF downloads, tables, figure assets and later updates require independent acquisition, rights and visual validation.".into());
        let dates=date.strip_prefix("Published ").and_then(|s|s.split_whitespace().next()).filter(|s|s.len()==4 && s.chars().all(|c|c.is_ascii_digit())).map(|y|vec![common::date("Published",y,base.as_str())]).unwrap_or_default();
        let mut rights=if let Some(n)=notice {Rights{status:"restricted".into(),restrictions:vec![n],evidence_urls:vec![base.to_string()],note:Some("Source footer notice; publisher PDF and journal rights remain separate.".into()),..Default::default()}}else{Rights{status:"unknown".into(),..Default::default()}};
        let mut fetches=vec![];if let Ok(c)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(c);}fetches.push(p.record.clone());
        let terms_url="https://www.east.org/terms-of-use";rights.terms_url=Some(terms_url.into());
        match f.get_ok(terms_url).await {Ok(t)=>{match terms(&t.text()){Ok(r)=>{rights.restrictions.extend(r);rights.evidence_urls.push(terms_url.into());},Err(e)=>warnings.push(format!("EAST source terms not recognized: {e:#}"))}fetches.push(t.record);},Err(e)=>warnings.push(format!("EAST terms unavailable: {e:#}"))}
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:p.record.final_url.clone(),item_type:Some("guideline".into()),status:"unknown".into(),rights,date_observations:dates,blocks,attachments:assets,fetches,warnings,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const TERMS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/east_practice_guidelines/terms.html"));
    const BCVI:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/east_practice_guidelines/bcvi.html"));
    const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/east_practice_guidelines/catalog.html"));
    const TRIAGE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/east_practice_guidelines/triage.html"));
    const SPLEEN:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/east_practice_guidelines/spleen.html"));
    const U:&str="https://www.east.org/education-resources/practice-management-guidelines/details/splenic-injury-blunt-selective-nonoperative-management-of";
    #[test]
    fn real_catalog_and_recommendation_levels() {
        assert!(terms(TERMS).unwrap().iter().any(|r|r.contains("not intentionally made available")));
        let links=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();assert!(links.iter().any(|(u,_,y)|u==U && y=="2012"));
        for (page,url) in [(SPLEEN,U),(TRIAGE,"https://www.east.org/education-resources/practice-management-guidelines/details/triage-of-the-trauma-patient")] {
            let (_,body,_,_,_)=clinical(page,&Url::parse(url).unwrap()).unwrap();let (mut blocks,_)=blocks_from_html(&body,Some(&Url::parse(url).unwrap()),Some("#pmg-body"));recommendations(&mut blocks);
            if page==SPLEEN {
                let recs:Vec<_>=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();assert_eq!(recs.len(),10);
                let urgent=recs.iter().find(|b|b.text.contains("hemodynamically unstable") && b.text.contains("urgently for laparotomy")).unwrap();
                assert!(matches!(&urgent.kind,BlockKind::Recommendation{strength:Some(s),certainty:None,..} if s=="Level 1"));
                let age=recs.iter().find(|b|b.text.contains("age >55") && b.text.contains("not contraindications")).unwrap();assert!(matches!(&age.kind,BlockKind::Recommendation{strength:Some(s),..} if s=="Level 2"));
                assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})));
            } else {
                let r=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.text.contains("Extrication time over 20 minutes")).unwrap();
                assert!(matches!(&r.kind,BlockKind::Recommendation{strength:Some(s),certainty:None,..} if s=="Level 3 Recommendations"));
                assert!(r.heading_path.iter().any(|h|h=="Adult Prehospital Triage"));
                assert!(!blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.text.contains("There are no Level 1")));
            }
        }
    }
    #[test]
    fn bcvi_pico_recommendations_keep_population_and_no_vote_inference() {
        let url="https://www.east.org/education-resources/practice-management-guidelines/details/blunt-cerebrovascular-injury-evaluation-and-management-of";
        let (_,body,_,_,_)=clinical(BCVI,&Url::parse(url).unwrap()).unwrap();let (mut blocks,_)=blocks_from_html(&body,Some(&Url::parse(url).unwrap()),Some("#pmg-body"));recommendations(&mut blocks);
        let recs:Vec<_>=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();
        let low=recs.iter().find(|b|b.text.contains("low-risk cervical spine injuries")).unwrap();assert!(matches!(&low.kind,BlockKind::Recommendation{strength:Some(s),certainty:None,..} if s=="conditionally recommend"));
        let high=recs.iter().find(|b|b.text.contains("high-risk cervical spine injuries")).unwrap();assert!(matches!(&high.kind,BlockKind::Recommendation{strength:None,certainty:None,..}));
        let stent=recs.iter().find(|b|b.text.contains("against the use of routine stenting")).unwrap();assert!(stent.text.contains("Grade II or III"));
        assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u.contains("blunt-cerebrovascular"))));
    }
    #[tokio::test]
    async fn offline_full_page_and_revision() {
        let anchor={let doc=Html::parse_document(CAT);let x=doc.select(&sel(".guidelines-browse a[href]")).find(|a|a.value().attr("href").is_some_and(|h|h.ends_with("/splenic-injury-blunt-selective-nonoperative-management-of"))).unwrap().html();x};
        let bounded=format!("<div class='guidelines-browse'>{anchor}</div>");let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");f.seed_cache_for_tests(U,SPLEEN.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.east.org/terms-of-use",TERMS.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"east_practice_guidelines","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let plan=East.discover(&src,&f,None).await.unwrap();assert_eq!(plan.len(),1);let d=East.extract(&src,&f,&plan[0]).await.unwrap();
        assert!(d.fetches.iter().any(|r|r.url=="https://www.east.org/terms-of-use"));
        assert!(d.rights.restrictions.iter().any(|r|r.contains("not intentionally made available")));
        assert!(d.date_observations.iter().any(|x|x.label=="Published" && x.normalized.as_deref()==Some("2012")));
        assert!(d.attachments.iter().any(|a|a["url"].as_str().is_some_and(|u|u.ends_with("Selective_nonoperative_management_of_blunt_splenic.4.pdf")) && a["rights"].as_str().is_some_and(|s|s.contains("unknown"))));
        let changed=SPLEEN.replace("taken urgently for laparotomy","taken promptly for laparotomy");assert_ne!(changed,SPLEEN);f.seed_cache_for_tests(U,changed.as_bytes(),"text/html");assert!(East.extract(&src,&f,&plan[0]).await.is_err());
    }
}
