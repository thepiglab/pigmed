//! SAGES source catalogue and full HTML publications.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct Sages;
const ROOT:&str="https://www.sages.org/publications/guidelines/";
fn catalog(html:&str,base:&Url)->Result<Vec<(String,String,String,String)>> {
    let doc=Html::parse_document(html);let root=doc.select(&sel("#genesis-content > .entry-content")).next().context("SAGES source catalog root missing")?;
    let mut seen=HashSet::new();let mut out=vec![];
    for ul in root.select(&sel("ul.document-list")) {
        let mut prev=ul.prev_sibling();let mut category=String::new();while let Some(n)=prev {if let Some(e)=ElementRef::wrap(n) {if e.value().name()=="h3"{category=text_of(&e);break;}if e.value().name()=="ul"{break;}}prev=n.prev_sibling();}
        for li in ul.children().filter_map(ElementRef::wrap).filter(|li|li.value().name()=="li") {let Some(a)=li.select(&sel("a[href]")).next() else {continue};let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};let u=Url::parse(&url)?;
            if u.origin()!=base.origin() || !u.path().starts_with("/publications/guidelines/") || !seen.insert(url.clone()){continue;}
            let title=text_of(&a);let date=li.select(&sel("span")).next().map(|s|text_of(&s)).unwrap_or_default().trim_matches(|c|c=='('||c==')').to_string();out.push((url,title,date,category.clone()));}
    }
    if out.is_empty(){bail!("SAGES catalog has no guideline document links")};Ok(out)
}
fn clinical(html:&str,base:&Url)->Result<(String,String,Vec<Value>,Option<String>)> {
    let doc=Html::parse_document(html);let article=doc.select(&sel("article")).next().context("SAGES publication article missing")?;
    let title=article.select(&sel("h1.entry-title")).next().map(|h|text_of(&h)).filter(|x|!x.is_empty()).context("SAGES publication title missing")?;
    let root=article.select(&sel(".entry-content")).next().context("SAGES publication content missing")?;
    if root.select(&sel("h2,h3")).next().is_none(){bail!("SAGES clinical section headings missing")}
    let rights=root.select(&sel(".copyright")).map(|x|text_of(&x)).find(|s|s.contains("All Rights Reserved"));
    let mut body=root.html();for e in root.select(&sel(".guideline-buttons,.copyright,script,style")){body=body.replace(&e.html(),"");}
    // Adjacent strong elements can split one source word (for example T/he).
    // Joining the tag boundary preserves the literal DOM text without adding
    // a space before the shared walker extracts it.
    body=body.replace("</strong><strong>","");
    let mut seen=HashSet::new();let mut links=vec![];
    for a in root.select(&sel("a[href]")) {let title=text_of(&a);let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};let lower=format!("{} {}",title.to_lowercase(),url.to_lowercase());
        let role=if lower.contains("sr-and-meta") || lower.contains("systematic review") {Some("evidence_review")} else if Url::parse(&url).ok().is_some_and(|u|u.path().to_lowercase().ends_with(".pdf")) {Some("linked_pdf")} else {None};
        if let Some(role)=role {if seen.insert(url.clone()){links.push(json!({"url":url,"title":title,"role":role,"rights":"unknown; linked asset requires separate review"}));}}
    }Ok((title,body,links,rights))
}
fn source_signal(robots:&str)->Option<String> {
    let mut general=false;
    for raw in robots.lines() {
        let line=raw.trim();if line.starts_with('#') || line.is_empty(){continue;}
        let Some((k,v))=line.split_once(':') else {continue};
        if k.trim().eq_ignore_ascii_case("user-agent") {general=v.trim()=="*";continue;}
        if general && k.trim().eq_ignore_ascii_case("content-signal") {return Some(v.trim().to_string());}
    }None
}
fn recommendations(blocks:&mut Vec<Block>)->Vec<String> {
    let grade=regex::Regex::new(r"(?i)\((conditional|strong) recommendation,\s*(very low|low|moderate|high) certainty (?:of )?evidence\)").unwrap();let mut out=vec![];let mut warnings=vec![];
    for (i,b) in blocks.iter().enumerate() {
        if !matches!(b.kind,BlockKind::Paragraph) || !b.text.trim_start().to_lowercase().starts_with("recommendation:") {continue;}
        if !b.heading_path.iter().any(|h|h.to_ascii_uppercase().starts_with("KQ")) {warnings.push("Recommendation lacks a local KQ heading; original paragraph retained.".into());continue;}
        let text=b.text.clone();if text.to_lowercase().contains("decided not to make an evidence-based recommendation") {warnings.push("Source explicitly makes no recommendation for one KQ; wording retained, no advice invented.".into());continue;}
        let gs:Vec<_>=grade.captures_iter(&text).collect();
        let (strength,certainty)=if gs.len()==1 && text.to_lowercase().matches("certainty").count()==1 {(Some(format!("{} recommendation",&gs[0][1])),Some(format!("{} certainty evidence",&gs[0][2])))} else {warnings.push("A KQ recommendation has no single safely scoped strength/certainty; source paragraph retained with grades unset.".into());(None,None)};
        out.push((i,Block{block_id:format!("{}-recommendation",b.block_id),kind:BlockKind::Recommendation{label:None,strength,certainty,population:None,grading_system:None,qualifier:None},text,heading_path:b.heading_path.clone(),page:b.page}));
    }
    for (i,b) in out.into_iter().rev(){blocks.insert(i+1,b);}warnings.push("Subgroup-specific certainty, expert-opinion basis, inline typography, embedded image-tables, figures and complete KQ coverage require source validation.".into());warnings
}
#[async_trait::async_trait]
impl Adapter for Sages {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0){return Ok(vec![])}let p=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;let entries=catalog(&p.text(),&Url::parse(&p.record.final_url)?)?;let mut rows=vec![];
        for (url,title,date,category) in entries {let work=common::slug(Url::parse(&url)?.path().trim_end_matches('/').rsplit('/').next().unwrap_or_default());let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:url.clone(),title:Some(title.clone()),rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"catalog_title":title,"catalog_date":date,"category":category,"catalog_fetch":&p.record})};
            match f.get_ok(&url).await {Ok(q)=>match clinical(&q.text(),&Url::parse(&q.record.final_url)?) {Ok((title,body,_,_))=>{let h=crate::common::sha256_hex(format!("{title}\n{body}").as_bytes());row.doc_key=format!("{work}-{}",&h[..12]);row.extra["clinical_sha256"]=json!(h);},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
            rows.push(row);if limit.is_some_and(|n|rows.len()>=n){break;}}
        Ok(rows)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("SAGES discovery: {e}")}let p=f.get_ok(&row.url).await?;let base=Url::parse(&p.record.final_url)?;
        let (title,html,assets,notice)=clinical(&p.text(),&base)?;let hash=crate::common::sha256_hex(format!("{title}\n{html}").as_bytes());if row.extra["clinical_sha256"].as_str().is_some_and(|s|s!=hash){bail!("SAGES article changed since discovery; re-plan")}
        let (mut blocks,mut warnings)=blocks_from_html(&html,Some(&base),Some(".entry-content"));warnings.extend(recommendations(&mut blocks));warnings.push("Source robots content signals and named-agent rules require review for the intended use; linked evidence reviews and images are separate assets with independent rights.".into());
        let dates=row.extra["catalog_date"].as_str().filter(|s|!s.is_empty()).map(|d|vec![common::date("Catalog publication",d,src.catalog_url.as_deref().unwrap_or(ROOT))]).unwrap_or_default();
        let mut rights=if let Some(n)=notice {Rights{status:"restricted".into(),restrictions:vec![n],evidence_urls:vec![base.to_string()],note:Some("Publisher notice for this HTML; independently linked assets need separate rights review.".into()),..Default::default()}}else{Rights{status:"unknown".into(),..Default::default()}};
        let mut fetches=vec![];if let Ok(c)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(c);}fetches.push(p.record.clone());
        let robots_url="https://www.sages.org/robots.txt";let mut signal=None;
        match f.get_ok(robots_url).await {
            Ok(r)=>{signal=source_signal(&r.text());if let Some(s)=&signal {rights.note=Some(format!("{} Publisher robots content signal: {s}; this is not a grant of corpus or model-training permission.",rights.note.as_deref().unwrap_or("Linked assets need independent rights review.")));}else{warnings.push("SAGES robots content signal not found; intended-use rights remain unknown.".into());}fetches.push(r.record);},
            Err(e)=>warnings.push(format!("SAGES robots content signal unavailable: {e:#}")),
        }
        let label=row.extra["catalog_title"].as_str().unwrap_or_default().to_lowercase();
        let item_type=if label.contains("systematic review") {Some("evidence_review".into())} else if label.contains("guideline") {Some("guideline".into())} else {None};
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:p.record.final_url.clone(),item_type,status:"unknown".into(),rights,date_observations:dates,blocks,attachments:assets,fetches,warnings,search_metadata:json!({"catalog_category":row.extra["category"],"robots_content_signal":signal,"robots_signal_evidence_url":robots_url}),..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const ROBOTS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sages_surgical_guidelines/robots.txt"));
    const REVIEW1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sages_surgical_guidelines/review-part1.html"));
    const REVIEW2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sages_surgical_guidelines/review-part2.html"));
    const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sages_surgical_guidelines/catalog.html"));
    const HIATAL:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sages_surgical_guidelines/hiatal.html"));
    const APPENDIX:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sages_surgical_guidelines/appendicitis.html"));
    const H:&str="https://www.sages.org/publications/guidelines/guidelines-for-the-surgical-treatment-of-hiatal-hernias/";
    const A:&str="https://www.sages.org/publications/guidelines/guideline-for-the-diagnosis-and-treatment-of-appendicitis/";
    #[test]
    fn real_source_catalog_roles_and_dates() {
        assert_eq!(source_signal(ROBOTS).as_deref(),Some("search=yes,ai-train=no,use=reference"));
        let rows=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();
        assert!(rows.iter().any(|(u,_,d,c)|u==H && d=="July 2024" && c=="Hernia"));
        assert!(rows.iter().any(|(u,_,d,_)|u==A && d=="May 2024"));
        assert!(rows.iter().any(|(_,t,_,_)|t.to_lowercase().contains("systematic review")));
    }
    #[test]
    fn clinical_question_scopes_and_image_tables_are_preserved() {
        let (_,html,_,_)=clinical(HIATAL,&Url::parse(H).unwrap()).unwrap();let (mut blocks,_)=blocks_from_html(&html,Some(&Url::parse(H).unwrap()),Some(".entry-content"));let warnings=recommendations(&mut blocks);
        let kq3=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.text.contains("may benefit from surgical fundoplication")).unwrap();
        assert!(kq3.text.contains("type II, III or IV") && kq3.heading_path.iter().any(|h|h.starts_with("KQ3")));
        assert!(matches!(&kq3.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="conditional recommendation" && c=="low certainty evidence"));
        assert!(!blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.text.contains("decided not to make an evidence-based recommendation")));
        assert!(warnings.iter().any(|w|w.contains("no recommendation")));
        let (_,html,_,_)=clinical(APPENDIX,&Url::parse(A).unwrap()).unwrap();let (mut blocks,_)=blocks_from_html(&html,Some(&Url::parse(A).unwrap()),Some(".entry-content"));recommendations(&mut blocks);
        for suffix in ["AppendGLTable1-1024x261.jpg","AppendGLTable2-1024x264.jpg"] {assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u.ends_with(suffix))),"missing {suffix}");}
        assert!(blocks.iter().any(|b|b.text.contains("Expert opinion") && b.text.contains("GRADE not utilized")));
        let kq2=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.heading_path.iter().any(|h|h.starts_with("KQ2")) && b.text.contains("uncomplicated appendicitis")).unwrap();
        assert!(kq2.text.contains("adult and pediatric"));
        assert!(matches!(&kq2.kind,BlockKind::Recommendation{strength:None,certainty:None,..}),"distinct adult and pediatric certainty must not be generalized");

    }
    #[tokio::test]
    async fn real_companion_review_is_distinct_and_preserves_full_image_set() {
        let review="https://www.sages.org/publications/guidelines/diagnosis-and-treatment-of-appendicitis-sr-and-meta/";
        let anchor={let doc=Html::parse_document(CAT);let x=doc.select(&sel("ul.document-list li")).find(|li|li.select(&sel("a[href]")).any(|a|a.value().attr("href")==Some(review))).unwrap().html();x};
        let catalog=format!("<main id='genesis-content'><div class='entry-content'><h3>Other</h3><ul class='document-list'>{anchor}</ul></div></main>");
        let article=format!("<article><header><h1 class='entry-title'>Diagnosis and Treatment of Appendicitis: Systematic Review and Meta-Analysis</h1></header>{REVIEW1}{REVIEW2}</article>");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(ROOT,catalog.as_bytes(),"text/html");f.seed_cache_for_tests(review,article.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.sages.org/robots.txt",ROBOTS.as_bytes(),"text/plain");
        let src:SourceEntry=serde_json::from_value(json!({"id":"sages_surgical_guidelines","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let rows=Sages.discover(&src,&f,None).await.unwrap();let d=Sages.extract(&src,&f,&rows[0]).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("evidence_review"));
        for name in ["AppendFigure2b.jpg","AppendFigure2c.jpg"] {assert!(d.blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u.contains(name))),"missing {name}");}
        assert!(d.blocks.iter().any(|b|b.text.contains("KQ1")));assert!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count()>=100);
    }
    #[tokio::test]
    async fn offline_actual_article_and_revision() {
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(H,HIATAL.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.sages.org/robots.txt",ROBOTS.as_bytes(),"text/plain");
        let src:SourceEntry=serde_json::from_value(json!({"id":"sages_surgical_guidelines","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let anchor={let doc=Html::parse_document(CAT);let x=doc.select(&sel("ul.document-list li")).find(|li|li.select(&sel("a[href]")).any(|a|a.value().attr("href")==Some(H))).unwrap().html();x};
        let exact=format!("<main id='genesis-content'><div class='entry-content'><h3>Hernia</h3><ul class='document-list'>{anchor}</ul></div></main>");f.seed_cache_for_tests(ROOT,exact.as_bytes(),"text/html");
        let rows=Sages.discover(&src,&f,None).await.unwrap();let d=Sages.extract(&src,&f,&rows[0]).await.unwrap();assert_eq!(d.search_metadata["robots_content_signal"],"search=yes,ai-train=no,use=reference");assert!(d.fetches.iter().any(|r|r.url=="https://www.sages.org/robots.txt"));assert!(d.date_observations.iter().any(|d|d.raw=="July 2024"));
        let changed=HIATAL.replace("may benefit from surgical fundoplication","may benefit from a different intervention");assert_ne!(changed,HIATAL);f.seed_cache_for_tests(H,changed.as_bytes(),"text/html");assert!(Sages.extract(&src,&f,&rows[0]).await.is_err());
    }
}
