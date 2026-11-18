//! KDIGO publisher topic inventory and independently identified PDF resources.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::doc::{GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct Kdigo;
const ROOT:&str="https://kdigo.org/guideline_categories/guidelines/";
fn topics(raw:&str,base:&Url)->Result<Vec<(String,String)>>{
 let d=Html::parse_document(raw);let mut seen=HashSet::new();let mut out=vec![];
 for a in d.select(&sel("a.nav-link[href]")){let title=text_of(&a);let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else{continue};let u=Url::parse(&url)?;if u.host_str()!=Some("kdigo.org")||!matches!(u.scheme(),"http"|"https")||!u.path().starts_with("/guidelines/")||u.path().trim_end_matches('/')=="/guidelines"{continue};let key=u.path().trim_end_matches('/').to_ascii_lowercase();if seen.insert(key){out.push((url,title));}}
 if out.is_empty(){bail!("KDIGO source topic navigation missing")};Ok(out)
}
fn suite(raw:&str,base:&Url)->Result<Vec<(String,String,String,Option<String>)>>{
 let d=Html::parse_document(raw);let widgets=d.select(&sel(".widget-boxed")).filter(|w|w.select(&sel("h2.widget-title")).any(|h|text_of(&h)=="Guideline Suite")).collect::<Vec<_>>();if widgets.len()!=1{bail!("KDIGO topic expected one Guideline Suite")}
 let mut seen=HashSet::new();let mut out=vec![];let year=regex::Regex::new(r"\b((?:19|20)\d{2})\b").unwrap();
 for a in widgets[0].select(&sel("ul.links a[href]")){let label=text_of(&a);let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else{continue};let u=Url::parse(&url)?;if u.host_str()!=Some("kdigo.org")||!u.path().to_lowercase().ends_with(".pdf")||!seen.insert(url.clone()){continue};let l=label.to_lowercase();let role=if l.contains("public review")||l.contains("draft"){"public_review_draft"}else if l.contains("executive summary"){"executive_summary"}else if l.contains("data supplement"){"data_supplement"}else if l.contains("scope of work"){"scope_of_work"}else if l.contains("takeaway"){"key_takeaways"}else if l.contains("guideline") && !l.contains("summary") && !l.contains("supplement"){"full_guideline"}else{"source_pdf"};let version=year.captures(&label).map(|c|c[1].to_string());out.push((url,label,role.into(),version));}
 if out.is_empty(){bail!("KDIGO Guideline Suite has no publisher-labelled PDF")};Ok(out)
}
fn pdf_rights(blocks:&[crate::guidelines::doc::Block],url:&str)->Rights{
 let notice=blocks.iter().map(|b|b.text.as_str()).find(|t|{let l=t.to_lowercase();l.contains("copyright") && l.contains("kdigo") && l.contains("cc by-nc-nd") && l.contains("permission")});
 if let Some(n)=notice{Rights{status:"restricted".into(),license:Some("CC BY-NC-ND".into()),restrictions:vec![n.into()],evidence_urls:vec![url.into()],note:Some("The PDF states a CC BY-NC-ND licence alongside explicit reproduction, amendment, transmission and storage permissions; retain both statements and assess asset-specific rights.".into()),..Default::default()}}else{Rights{status:"unknown".into(),note:Some("No verified licence for this PDF and embedded assets.".into()),..Default::default()}}
}
#[async_trait::async_trait]
impl Adapter for Kdigo {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let catalog=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;let entries=topics(&catalog.text(),&Url::parse(&catalog.record.final_url)?)?;let mut rows=vec![];let mut seen=HashSet::new();
  for (topic,title) in entries {let page=match f.get_ok(&topic).await {Ok(p)=>p,Err(e)=>{rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("kdigo-{}-unresolved",common::short_hash(&topic)),url:topic,title:Some(title),rendition:None,version:None,extra:json!({"discovery_error":format!("KDIGO topic unavailable: {e:#}")})});if limit.is_some_and(|n|rows.len()>=n){break}continue}};
   let final_url=Url::parse(&page.record.final_url)?;if final_url.host_str()!=Some("kdigo.org"){bail!("KDIGO topic redirected outside source")};let suite=match suite(&page.text(),&final_url){Ok(s)=>s,Err(e)=>{rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("kdigo-{}-unresolved",common::short_hash(&topic)),url:topic,title:Some(title),rendition:None,version:None,extra:json!({"discovery_error":format!("KDIGO topic suite unavailable: {e:#}")})});if limit.is_some_and(|n|rows.len()>=n){break}continue}};
   for (url,label,role,version) in suite {if !seen.insert(url.clone()){continue}let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("kdigo-{}-unresolved",common::short_hash(&url)),url:url.clone(),title:Some(label.clone()),rendition:Some("pdf".into()),version,extra:json!({"topic_url":topic,"topic_fetch":page.record,"catalog_fetch":catalog.record,"role":role,"source_label":label})};
    match f.get_ok(&url).await{Ok(p) if p.is_pdf() && Url::parse(&p.record.final_url).is_ok_and(|u|u.host_str()==Some("kdigo.org"))=>{row.doc_key=format!("kdigo-{}-{}",common::short_hash(&url),&p.record.sha256[..12]);row.extra["pdf_sha256"]=json!(p.record.sha256);},Ok(_)=>row.extra["discovery_error"]=json!("KDIGO source PDF unavailable or redirected outside source"),Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))};rows.push(row);if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
   }
  }Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("KDIGO discovery: {e}")};let topic=row.extra["topic_url"].as_str().context("KDIGO source topic missing")?;let t=f.get_ok(topic).await?;let u=Url::parse(&t.record.final_url)?;if u.host_str()!=Some("kdigo.org"){bail!("KDIGO topic redirected outside source")};let assets=suite(&t.text(),&u)?;if !assets.iter().any(|(url,label,role,version)|url==&row.url && Some(label)==row.title.as_ref() && Some(role.as_str())==row.extra["role"].as_str() && version==&row.version){bail!("KDIGO source suite changed; re-plan")}
  let p=f.get_ok(&row.url).await?;if !p.is_pdf() || Url::parse(&p.record.final_url)?.host_str()!=Some("kdigo.org") || row.extra["pdf_sha256"].as_str()!=Some(p.record.sha256.as_str()){bail!("KDIGO PDF changed or unavailable; re-plan")};let (blocks,mut warnings,_)=blocks_for(&p,None,30).await?;if blocks.is_empty(){bail!("KDIGO PDF yielded no text")};let rights=pdf_rights(&blocks,&row.url);
  warnings.push("PDF conversion may merge separately numbered recommendations and practice points or include section text in the same block; exact statement-grade and list boundaries require source-page review. No grades inferred.".into());warnings.push("PDF tables, algorithms, footnotes, page positions and embedded figures require source review. Topic menu and Guideline Suite may omit drafts, archives, translations and other publisher materials; absence is not complete coverage.".into());
  if t.text().contains("focused update of the") && t.text().contains("draft is currently being revised") {warnings.push("Publisher topic page states that the proposed 2027 medications focused update was being revised after public review closed on November 4, 2026; no final update or wholesale replacement of the 2024 guideline is inferred.".into());}
  let role=row.extra["role"].as_str().unwrap_or("source_pdf");Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:Some(format!("kdigo-{}",common::short_hash(&row.url))),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some(role.into()),version:row.version.clone(),status:if role=="public_review_draft"{"draft".into()}else{"unknown".into()},rights,blocks,fetches:vec![t.record,p.record],warnings,search_metadata:json!({"topic_url":topic,"publisher_role":role}),..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests{
 use super::*;
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/kdigo/catalog.html"));
 const CKD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/kdigo/ckd.html"));
 const HEART:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/kdigo/heart-failure.html"));
 const BLOCKS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/kdigo/ckd-2024-blocks.json"));
 #[test] fn genuine_menu_suites_keep_document_roles_and_review_state(){let ts=topics(CAT,&Url::parse(ROOT).unwrap()).unwrap();assert!(ts.iter().any(|(u,_)|u=="http://kdigo.org/guidelines/ckd-evaluation-and-management/"));assert_eq!(ts.iter().filter(|(u,_)|u.contains("ckd-evaluation-and-management")).count(),1);
  let entries=suite(CKD,&Url::parse("https://kdigo.org/guidelines/ckd-evaluation-and-management/").unwrap()).unwrap();assert!(entries.iter().any(|(u,_,r,y)|u=="https://kdigo.org/wp-content/uploads/2026/04/KDIGO-2024-CKD-Guideline.pdf" && r=="full_guideline" && y.as_deref()==Some("2024")));assert!(entries.iter().any(|(_,_,r,_)|r=="executive_summary"));assert!(entries.iter().any(|(_,_,r,_)|r=="data_supplement"));assert!(!entries.iter().any(|(_,_,r,_)|r=="public_review_draft"));assert!(CKD.contains("draft is currently being revised"));
  let heart=suite(HEART,&Url::parse("https://kdigo.org/guidelines/heart-failure-in-ckd/").unwrap()).unwrap();assert!(heart.iter().any(|(_,_,r,_)|r=="scope_of_work"));assert!(!heart.iter().any(|(_,_,r,_)|r=="full_guideline"));
  let b:Vec<crate::guidelines::doc::Block>=serde_json::from_str(BLOCKS).unwrap();let rights=pdf_rights(&b,"https://kdigo.org/source.pdf");assert_eq!(rights.status,"restricted");assert_eq!(rights.license.as_deref(),Some("CC BY-NC-ND"));assert!(b.iter().any(|b|b.text.contains("Recommendation 1.1.4.1") && b.text.contains("(2D)")));assert!(b.iter().any(|b|b.text.contains("Practice Point 1.2.2.2") && b.text.contains("Practice Point 1.2.2.3")));
 }
 #[tokio::test] async fn offline_topic_has_unresolved_pdf_not_fabricated_draft(){let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,CAT.as_bytes(),"text/html");let topic="http://kdigo.org/guidelines/ckd-evaluation-and-management/";f.seed_cache_for_tests(topic,CKD.as_bytes(),"text/html");let src:SourceEntry=serde_json::from_value(json!({"id":"kdigo","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Kdigo.discover(&src,&f,None).await.unwrap();assert!(rows.iter().any(|r|r.url.ends_with("/2026/04/KDIGO-2024-CKD-Guideline.pdf") && r.extra["role"]=="full_guideline" && r.extra["discovery_error"].is_string()));assert!(!rows.iter().any(|r|r.url.contains("2027-CKD-Guideline-Focused-Update-Public-Review-Draft.pdf")));}
}
