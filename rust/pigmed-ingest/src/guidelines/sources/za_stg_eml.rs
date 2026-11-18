//! South African essential medicines and STG catalog: source-labelled care-level PDF assets.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{GuidelineDoc,Rights};
use crate::guidelines::fetch::{Fetched,Fetcher};
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashMap;
use url::Url;
pub struct ZaStgEml;
fn role(title:&str,table:&str)->&'static str{let s=format!("{} {}",title,table).to_lowercase();if s.contains("errat") {"erratum"}else if s.contains("chapter")&&s.contains("nemlc report") {"guideline_chapter_with_evidence_report"}else if s.contains("evidence review")||s.contains("nemlc report") {"evidence_review"}else if s.contains("appendix") {"appendix"}else if s.contains("guideline")||s.contains("treatment")||s.contains("stg") {"guideline"}else{"supporting_resource"}}
fn catalog(html:&str,base:&Url)->Result<Vec<(String,Vec<Value>)>>{
 let d=Html::parse_document(html);let mut rows:Vec<(String,Vec<Value>)>=vec![];let mut positions:HashMap<String,usize>=HashMap::new();let tabs=d.select(&sel("div.elementor-tab-content[role='tabpanel']")).collect::<Vec<_>>();if tabs.is_empty(){bail!("South Africa care-level tabs missing");}
 for tab in tabs {let label=tab.value().attr("aria-labelledby").and_then(|id|d.select(&sel("[role=tab]")).find(|e|e.value().id()==Some(id))).map(|e|text_of(&e)).unwrap_or_default();
  for table in tab.select(&sel("table")){let head=table.select(&sel("thead")).next().map(|e|text_of(&e)).unwrap_or_default();let table_id=table.value().id().unwrap_or("");
   for a in table.select(&sel("a[href]")){let title=text_of(&a);if title.is_empty(){continue;}let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};let u=Url::parse(&url)?;if u.origin()!=base.origin(){continue;}
    let mention=json!({"label":title,"care_level":label,"table_heading":head,"table_id":table_id,"role":role(&title,&head)});
    if let Some(i)=positions.get(&url).copied(){rows[i].1.push(mention)}else{positions.insert(url.clone(),rows.len());rows.push((url,vec![mention]));}
   }
  }
 }if rows.is_empty(){bail!("South Africa catalog tables contain no linked assets");}Ok(rows)
}
fn pdf_rights(blocks:&[crate::guidelines::doc::Block],url:&str)->Rights{
 let notice=blocks.iter().find(|b|{let t=b.text.to_lowercase();t.contains("may be reproduced, copied or adapted")&&t.contains("distributed free of charge")&&t.contains("not for profit")});
 if notice.is_some(){Rights{status:"restricted".into(),note:Some("This PDF permits reproduction or adaptation for free, not-for-profit distribution; embedded third-party content and other PDFs require separate review.".into()),restrictions:vec!["Distributed free of charge or at no cost, not for profit".into()],evidence_urls:vec![url.to_string()],..Default::default()}}else{Rights{status:"unknown".into(),note:Some("This PDF's reuse terms and third-party content have not been established.".into()),..Default::default()}}
}
fn pdf_complete(f:&Fetched)->bool{f.is_pdf()&&f.body.iter().rev().take(2048).copied().collect::<Vec<_>>().windows(5).any(|w|w==b"FOE%%")}
fn unresolved(src:&SourceEntry,url:String,mentions:Vec<Value>,e:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:mentions.first().and_then(|v|v["label"].as_str()).map(String::from),rendition:None,version:None,extra:json!({"discovery_error":e,"source_mentions":mentions})}}
#[async_trait::async_trait] impl Adapter for ZaStgEml {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![])}let cat=f.get_ok(src.catalog_url.as_deref().context("South Africa catalog missing")?).await?;let base=Url::parse(&cat.record.final_url)?;let mut rows=vec![];let mut deferred=vec![];
  for (url,mentions) in catalog(&cat.text(),&base)?{let parsed=Url::parse(&url)?;if !parsed.path().to_lowercase().ends_with(".pdf"){deferred.push(unresolved(src,url,mentions,"unmapped non-PDF catalog asset".into()));continue;}
   let body=match f.get_ok(&url).await{Ok(x)if pdf_complete(&x)=>x,Ok(_)=>{deferred.push(unresolved(src,url,mentions,"source asset is not a complete PDF".into()));continue;},Err(e)=>{deferred.push(unresolved(src,url,mentions,format!("{e:#}")));continue;}};
   let key=common::short_hash(&url);rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&body.record.sha256[..12]),url,title:mentions.first().and_then(|v|v["label"].as_str()).map(String::from),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":key,"pdf_sha256":body.record.sha256,"source_mentions":mentions,"catalog_fetch":cat.record})});if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
  }let remaining=limit.map(|n|n.saturating_sub(rows.len())).unwrap_or(usize::MAX);rows.extend(deferred.into_iter().take(remaining));Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("South Africa source unresolved: {e}");}let body=f.get_ok(&row.url).await?;if !pdf_complete(&body){bail!("South Africa PDF incomplete");}if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=body.record.sha256){bail!("South Africa PDF changed since discovery");}
  let (blocks,mut warnings,_)=super::static_list::blocks_for(&body,None,0).await?;if blocks.is_empty(){bail!("South Africa PDF yielded no text blocks");}
  let rights=pdf_rights(&blocks,&row.url);
  let mention=&row.extra["source_mentions"][0];let role=mention["role"].as_str().unwrap_or("supporting_resource");let label=mention["label"].as_str().unwrap_or("");let date_re=regex::Regex::new(r"\((\d{1,2}\s+[A-Za-z]+\s+\d{4})\)").unwrap();let dates=date_re.captures(label).map(|c|vec![common::date("Catalog label date",&c[1],row.extra["catalog_fetch"]["final_url"].as_str().unwrap_or(&row.url))]).unwrap_or_default();
  warnings.push("Generic PDF conversion does not verify page coordinates, multi-column table cells, source algorithms, grade/footnote associations or the exact applicability of separate errata. Source catalog roles and edition labels are retained without inferred supersession.".into());let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(body.record);
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some(role.into()),status:"unknown".into(),date_observations:dates,rights,search_metadata:json!({"source_mentions":row.extra["source_mentions"]}),blocks,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests{
 use super::*;
 fn catalog_fixture()->String{[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/za_stg_eml/catalog-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/za_stg_eml/catalog-2.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/za_stg_eml/catalog-3.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/za_stg_eml/catalog-4.html"))].concat()}
 #[test] fn genuine_index_retains_care_level_edition_and_erratum_as_distinct(){let base=Url::parse("https://www.health.gov.za/nhi-edp-stgs-eml/").unwrap();let rows=catalog(&catalog_fixture(),&base).unwrap();let full=rows.iter().find(|(u,_)|u.contains("2024-Edition-Updated-3-September-2026.pdf")).unwrap();assert!(full.1.iter().any(|m|m["role"]=="guideline"&&m["care_level"]=="Primary Healthcare"&&m["label"].as_str().unwrap().contains("3 September 2026")));let err=rows.iter().find(|(u,_)|u.contains("Chapter-13-Immunisation-13-October-2026-1.pdf")).unwrap();assert!(err.1.iter().any(|m|m["role"]=="erratum"&&m["table_heading"].as_str().unwrap().contains("Errata")));assert_ne!(full.0,err.0);assert!(catalog("<html/>",&base).is_err());}

 #[tokio::test] async fn genuine_full_edition_pages_and_distinct_erratum_notice(){
  let source="https://www.health.gov.za/wp-content/uploads/2026/09/Primary-Healthcare-Standard-Treatment-Guidelines-and-Essential-Medicines-List-2024-Edition-Updated-3-September-2026.pdf";
  let samples:[(&[u8],usize,&str);3]=[(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/za_stg_eml/phc-front-page-2.pdf")),2,"not for profit"),(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/za_stg_eml/phc-clinical-page-415.pdf")),415,"Immunisation"),(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/za_stg_eml/phc-clinical-page-417.pdf")),417,"Immunisation")];
  for (bytes,physical_page,needle) in samples{let record=serde_json::from_value(json!({"url":source,"final_url":source,"redirects":[],"status":200,"content_type":"application/pdf","bytes":bytes.len(),"sha256":crate::common::sha256_hex(bytes),"fetched_at":"fixture","from_cache":true})).unwrap();let fetched=Fetched{record,body:bytes.to_vec()};let (blocks,_,_)=super::super::static_list::blocks_for(&fetched,None,0).await.unwrap();assert!(blocks.iter().any(|b|b.text.to_lowercase().contains(&needle.to_lowercase())),"source physical page {physical_page}");if physical_page==2{assert_eq!(pdf_rights(&blocks,source).status,"restricted");}}
  let erratum:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/za_stg_eml/phc-erratum-page-text.json"))).unwrap();let text=erratum.as_array().unwrap().iter().filter_map(|x|x["text"].as_str()).collect::<Vec<_>>().join(" ");assert!(text.contains("must still be incorporated"));let blocks=crate::guidelines::html::blocks_from_markdown(&text);assert_eq!(pdf_rights(&blocks,"erratum").status,"unknown");
 }
}
