//! GOLD publisher-labelled full report, pocket guide and changes summary.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::doc::{Block,GuidelineDoc,Relation,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct Gold;
const ROOT:&str="https://goldcopd.org/2026-gold-report-and-pocket-guide/";
fn catalog(raw:&str,base:&Url)->Result<Vec<(String,String,String,String)>> {
 let doc=Html::parse_document(raw);let root=doc.select(&sel("article.entry-content")).next().context("GOLD source report catalog missing")?;
 let report=regex::Regex::new(r"(?i)^Download\s+((?:19|20)\d{2})\s+GOLD Report$").unwrap();
 let pocket=regex::Regex::new(r"(?i)^Download\s+((?:19|20)\d{2})\s+GOLD Pocket Guide$").unwrap();
 let changes=regex::Regex::new(r"(?i)^((?:19|20)\d{2})\s+Summary of Changes$").unwrap();
 let mut seen=HashSet::new();let mut rows=vec![];
 for a in root.select(&sel("a[href]")){let text=text_of(&a);let (role,year)=if let Some(c)=report.captures(&text){("full_report",c[1].to_string())}else if let Some(c)=pocket.captures(&text){("pocket_guide",c[1].to_string())}else if let Some(c)=changes.captures(&text){("changes_summary",c[1].to_string())}else{continue};
  let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};let u=Url::parse(&url)?;if u.origin()!=base.origin()||!u.path().to_ascii_lowercase().ends_with(".pdf"){bail!("GOLD source-labelled PDF points outside source origin or is not PDF")};if seen.insert(url.clone()){rows.push((url,text,role.into(),year));}}
 if !rows.iter().any(|r|r.2=="full_report")||!rows.iter().any(|r|r.2=="pocket_guide"){bail!("GOLD source full report or pocket guide link missing")};Ok(rows)
}
fn pdf_rights(blocks:&[Block],url:&str)->Rights {
 let notice=blocks.iter().take(60).map(|b|b.text.as_str()).find(|t|{let x=t.to_lowercase();x.contains("copyright") && x.contains("do not") && (x.contains("distribute")||x.contains("copy"))});
 if let Some(n)=notice {Rights{status:"restricted".into(),restrictions:vec![n.into()],evidence_urls:vec![url.into()],note:Some("This source PDF notice is asset-specific; other GOLD items and third-party figures have independent rights.".into()),..Default::default()}}else{Rights{status:"unknown".into(),..Default::default()}}
}
#[async_trait::async_trait]
impl Adapter for Gold {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![])}let p=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;let entries=catalog(&p.text(),&Url::parse(&p.record.final_url)?)?;
  let report=entries.iter().find(|e|e.2=="full_report").map(|e|e.0.clone());let mut rows=vec![];
  for (url,title,role,year) in entries {let work=format!("gold-{}-{}",year,role);let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:url.clone(),title:Some(title),rendition:Some("pdf".into()),version:Some(year),extra:json!({"work_key":work,"role":role,"source_report":report,"catalog_fetch":&p.record})};
   match f.get_ok(&url).await{Ok(q) if q.is_pdf()=>{row.doc_key=format!("{work}-{}",&q.record.sha256[..12]);row.extra["pdf_sha256"]=json!(q.record.sha256);},Ok(_)=>row.extra["discovery_error"]=json!("GOLD source asset is not PDF"),Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
   rows.push(row);if limit.is_some_and(|n|rows.len()>=n){break;}
  }Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("GOLD discovery: {e}")}let p=f.get_ok(&row.url).await?;if !p.is_pdf(){bail!("GOLD asset is not PDF")};if row.extra["pdf_sha256"].as_str().is_some_and(|h|h!=p.record.sha256){bail!("GOLD PDF changed; re-plan")}
  let (blocks,mut warnings,_)=blocks_for(&p,None,30).await?;if blocks.is_empty(){bail!("GOLD PDF has no extracted text")};
  let rights=pdf_rights(&blocks,&row.url);
  warnings.push("PDF text can interleave copyright watermark text with clinical content and may omit flowchart branches, visual matrices, tables, footnotes, page boundaries and embedded figures; source-page comparison is required. Source grades or clinical currentness are not inferred.".into());
  let role=row.extra["role"].as_str().unwrap_or_default();let relations=if role!="full_report" {row.extra["source_report"].as_str().map(|u|vec![Relation{rel:"related".into(),target:u.into(),scope:Some(format!("Publisher lists this {} report companion beside the full report; no content equivalence inferred",row.version.as_deref().unwrap_or("labelled"))),evidence_url:src.catalog_url.clone()}]).unwrap_or_default()}else{vec![]};
  let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(p.record);
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),version:row.version.clone(),item_type:Some(if role=="full_report"{"strategy"}else{"summary"}.into()),status:"unknown".into(),rights,relations,blocks,fetches,warnings,search_metadata:json!({"publisher_role":role}),..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const NOTICE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/gold/report-watermark-block.json"));
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/gold/catalog.html"));
 #[test] fn genuine_catalog_distinguishes_report_pocket_and_changes(){
  let rows=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();assert_eq!(rows.len(),3);
  let b:Vec<Block>=serde_json::from_str(NOTICE).unwrap();let rights=pdf_rights(&b,"https://goldcopd.org/source.pdf");assert_eq!(rights.status,"restricted");assert!(rights.restrictions.iter().any(|r|r.contains("DO NOT COPY OR DISTRIBUTE")));assert_eq!(pdf_rights(&[],"https://goldcopd.org/source.pdf").status,"unknown");
  let full=rows.iter().find(|r|r.2=="full_report").unwrap();assert_eq!(full.3,"2026");assert!(full.0.ends_with("GOLD-REPORT-2026-v1.3-8Dec2025_WMV2.pdf"));
  let pocket=rows.iter().find(|r|r.2=="pocket_guide").unwrap();assert!(pocket.0.ends_with("GOLD-Pocket-Guide-2026-v1.1-20Nov2025_WMV2.pdf"));
  let change=rows.iter().find(|r|r.2=="changes_summary").unwrap();assert!(change.0.ends_with("KEY-CHANGES-GOLD-2026-10Nov2025.pdf"));
  assert_ne!(full.0,pocket.0);assert_ne!(pocket.0,change.0);
 }
 #[tokio::test] async fn genuine_changes_excerpt_is_not_full_report(){
  let pdf=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/gold/changes-page-2.pdf"));let records=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();let url=&records.iter().find(|r|r.2=="changes_summary").unwrap().0;
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,CAT.as_bytes(),"text/html");f.seed_cache_for_tests(url,pdf,"application/pdf");let src:SourceEntry=serde_json::from_value(json!({"id":"gold","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Gold.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),3);let row=rows.iter().find(|r|r.url==*url).unwrap();let d=Gold.extract(&src,&f,row).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("summary"));assert!(d.warnings.iter().any(|w|w.contains("original-source") || w.contains("source-page")));assert!(!d.blocks.is_empty());assert!(d.relations.iter().any(|r|r.rel=="related"));let mut changed=pdf.to_vec();changed.extend_from_slice(b"\nchanged snapshot");f.seed_cache_for_tests(url,&changed,"application/pdf");assert!(Gold.extract(&src,&f,row).await.is_err());
 }

}
