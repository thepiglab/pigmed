//! Tanzania Ministry resource-center STG/NEMLIT editions, source-linked PDF renditions.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{GuidelineDoc,Rights};
use crate::guidelines::fetch::{Fetched,Fetcher};
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct TzMohStgNemlit;
fn catalog(html:&str,base:&Url)->Result<Vec<(String,String,String)>> {
 let d=Html::parse_document(html);let mut seen=HashSet::new();let mut out=vec![];
 for li in d.select(&sel("li.list-group-item")){for a in li.select(&sel("a[href]")){let title=text_of(&a);let low=title.to_lowercase();if !low.contains("nemlit")||!low.contains("stg"){continue;}
  let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};let u=Url::parse(&url)?;if u.origin()!=base.origin()||!u.path().to_lowercase().ends_with(".pdf"){continue;}
  if seen.insert(url.clone()){let t=text_of(&li);let date=t.split_once("Posted on:").map(|(_,s)|s.trim().to_string()).unwrap_or_default();out.push((url,title,date));}
 }}if out.is_empty(){bail!("Tanzania catalog contains no source-labelled STG/NEMLIT PDFs");}Ok(out)
}
fn pdf_complete(f:&Fetched)->bool{f.is_pdf()&&f.body.iter().rev().take(2048).copied().collect::<Vec<_>>().windows(5).any(|w|w==b"FOE%%")}
fn unresolved(src:&SourceEntry,url:String,title:String,e:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:Some(title),rendition:Some("pdf".into()),version:None,extra:json!({"discovery_error":e})}}
#[async_trait::async_trait] impl Adapter for TzMohStgNemlit {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![])}let cat=f.get_ok(src.catalog_url.as_deref().context("Tanzania catalog missing")?).await?;let base=Url::parse(&cat.record.final_url)?;let mut rows=vec![];
  for (url,title,date) in catalog(&cat.text(),&base)?{let body=match f.get_ok(&url).await{Ok(x) if pdf_complete(&x)=>x,Ok(_)=>{rows.push(unresolved(src,url,title,"source did not return a complete PDF".into()));continue;},Err(e)=>{rows.push(unresolved(src,url,title,format!("{e:#}")));continue;}};let key=common::short_hash(&url);rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&body.record.sha256[..12]),url,title:Some(title),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":key,"pdf_sha256":body.record.sha256,"catalog_posted":date,"catalog_fetch":cat.record})});if limit.is_some_and(|n|rows.len()>=n){break;}}
  if let Some(n)=limit{rows.truncate(n)}Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("Tanzania source unresolved: {e}");}let body=f.get_ok(&row.url).await?;if !pdf_complete(&body){bail!("Tanzania PDF incomplete");}if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=body.record.sha256){bail!("Tanzania PDF changed since discovery");}
  let (blocks,mut warnings,_)=super::static_list::blocks_for(&body,None,0).await?;if blocks.is_empty(){bail!("Tanzania PDF yielded no text blocks");}warnings.push("Generic PDF conversion does not establish complete 778-page structure, table/footnote associations, dosing or graphical algorithms. Catalog posting date is not clinical publication date; no supersession between editions is inferred.".into());
  let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(body.record);let dates=row.extra["catalog_posted"].as_str().filter(|s|!s.is_empty()).map(|s|vec![common::date("Catalog posted on",s,row.extra["catalog_fetch"]["final_url"].as_str().unwrap_or(&row.url))]).unwrap_or_default();
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some("standard_treatment_guideline_and_essential_medicines_list".into()),status:"unknown".into(),date_observations:dates,rights:Rights{status:"unknown".into(),note:Some("Ministry copyright noted in source; reuse terms and third-party assets unverified.".into()),..Default::default()},blocks,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {use super::*;const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/tz_moh_stg_nemlit/catalog.html"));#[test]fn genuine_catalog_retains_distinct_files_and_posting_dates(){let b=Url::parse("https://www.moh.go.tz/index.php/resource-center").unwrap();let rows=catalog(C,&b).unwrap();assert!(rows.iter().any(|(u,t,d)|u.ends_with("6a9441f6340f0043649176.pdf")&&t.contains("7TH EDITION 2026")&&d.contains("August 30th, 2026")));assert!(rows.iter().any(|(u,t,d)|u.ends_with("6a8c97408b3be007728156.pdf")&&t.contains("Final Version A")&&d.contains("August 24th, 2026")));assert!(catalog("<html/>",&b).is_err());}
 #[tokio::test] async fn genuine_pages_keep_list_guideline_and_asthma_table_context(){
  let source="https://www.moh.go.tz/storage/app/uploads/public/6a9/441/f63/6a9441f6340f0043649176.pdf";let samples:[(&[u8],usize,&str);3]=[(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/tz_moh_stg_nemlit/stg-clinical-page-34.pdf")),34,"NATIONAL ESSENTIAL MEDICINES LIST"),(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/tz_moh_stg_nemlit/stg-clinical-page-65.pdf")),65,"STANDARD TREATMENT GUIDELINE"),(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/tz_moh_stg_nemlit/stg-clinical-page-246.pdf")),246,"Asthma")];
  for (bytes,physical_page,needle) in samples{let record=serde_json::from_value(json!({"url":source,"final_url":source,"redirects":[],"status":200,"content_type":"application/pdf","bytes":bytes.len(),"sha256":crate::common::sha256_hex(bytes),"fetched_at":"fixture","from_cache":true})).unwrap();let fetched=Fetched{record,body:bytes.to_vec()};let (blocks,_,_)=super::super::static_list::blocks_for(&fetched,None,0).await.unwrap();assert!(blocks.iter().any(|b|b.text.to_lowercase().contains(&needle.to_lowercase())),"source physical page {physical_page}");}
  let mapping:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/tz_moh_stg_nemlit/stg-clinical-excerpts.json"))).unwrap();assert!(mapping.as_array().unwrap().iter().any(|x|x["physical_page"]==246&&x["source_pages"]==778));
 }
}
