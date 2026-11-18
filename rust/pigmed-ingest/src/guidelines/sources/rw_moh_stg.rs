//! Rwanda ministry source-emitted guideline category and public PDF terminals.
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
pub struct Rwanda;
const ROOT:&str="https://www.moh.gov.rw/guidelines-protocols/guideline/";
fn page(raw:&str,base:&Url)->Result<(Vec<(String,String,Option<String>)>,Option<String>)>{
 let doc=Html::parse_document(raw);let cards=doc.select(&sel(".card_document")).collect::<Vec<_>>();if cards.is_empty(){bail!("Rwanda guideline catalog cards missing")}
 let mut seen=HashSet::new();let mut out=vec![];
 for card in cards {let Some(a)=card.select(&sel(".articletype-1 a[href]")).next() else {continue};let title=a.select(&sel("h4")).next().map(|h|text_of(&h)).unwrap_or_else(||text_of(&a));
  let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};let u=Url::parse(&url)?;if u.origin()!=base.origin() || !seen.insert(url.clone()){continue;}
  let date=card.select(&sel("time[itemprop='datePublished']")).next().and_then(|t|t.value().attr("datetime")).map(String::from);out.push((url,title,date));}
 if out.is_empty(){bail!("Rwanda category page has no source document links")}
 let next=doc.select(&sel(".f3-widget-paginator li.next a[href]")).next().and_then(|a|a.value().attr("href")).and_then(|h|common::link(base,h));
 if let Some(u)=&next {let n=Url::parse(u)?;if n.origin()!=base.origin() || n.path().trim_end_matches('/')!="/guidelines-protocols/guideline" {bail!("Rwanda next-page link left guideline category")}}
 Ok((out,next))
}
fn unresolved(src:&SourceEntry,url:String,e:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("catalog-{}-unresolved",common::short_hash(&url)),url,title:Some("Rwanda guideline catalog continuation".into()),rendition:None,version:None,extra:json!({"discovery_error":e})}}
#[async_trait::async_trait]
impl Adapter for Rwanda {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let mut next=Some(src.catalog_url.as_deref().unwrap_or(ROOT).to_string());let mut seen_pages=HashSet::new();let mut seen_docs=HashSet::new();let mut rows=vec![];
  while let Some(url)=next.take(){if !seen_pages.insert(url.clone()){rows.push(unresolved(src,url,"Rwanda catalog pagination cycle; inventory incomplete".into()));break;}if seen_pages.len()>100{rows.push(unresolved(src,url,"Rwanda pagination limit reached; inventory incomplete".into()));break;}
   let catalog=match f.get_ok(&url).await{Ok(p)=>p,Err(e)=>{rows.push(unresolved(src,url,format!("Rwanda catalog continuation unavailable: {e:#}")));break;}};
   let (entries,n)=match page(&catalog.text(),&Url::parse(&catalog.record.final_url)?){Ok(x)=>x,Err(e)=>{rows.push(unresolved(src,url,format!("Rwanda catalog parsing failed: {e:#}")));break;}};next=n;
   for (pdf,title,date) in entries {if !seen_docs.insert(pdf.clone()){continue;}let work=format!("rwanda-{}",common::short_hash(&pdf));let edition=regex::Regex::new(r"(?i)\b\d+(?:st|nd|rd|th)\s+Edition\b").unwrap().find(&title).map(|m|m.as_str().to_string());let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:pdf.clone(),title:Some(title),rendition:Some("pdf".into()),version:edition,extra:json!({"work_key":work,"catalog_fetch":&catalog.record,"catalog_date":date})};
    match f.get_ok(&pdf).await {Ok(p) if p.is_pdf()=>{row.doc_key=format!("{work}-{}",&p.record.sha256[..12]);row.extra["pdf_sha256"]=json!(p.record.sha256);},Ok(_)=>row.extra["discovery_error"]=json!("Rwanda source document did not return PDF"),Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
    rows.push(row);if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
   }
  }Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("Rwanda discovery: {e}")}let p=f.get_ok(&row.url).await?;if !p.is_pdf(){bail!("Rwanda document is not PDF")};if row.extra["pdf_sha256"].as_str().is_some_and(|h|h!=p.record.sha256){bail!("Rwanda PDF changed; re-plan")}
  let (blocks,mut warnings,_)=blocks_for(&p,None,30).await?;if blocks.is_empty(){bail!("Rwanda PDF yielded no text")}
  let pdf_notice=blocks.iter().take(80).map(|b|b.text.as_str()).find(|t|{let l=t.to_lowercase();l.contains("copyright") && (l.contains("all rights reserved") || l.contains("permission to reproduce"))});
  let rights=if let Some(n)=pdf_notice {Rights{status:"restricted".into(),restrictions:vec![n.into()],evidence_urls:vec![row.url.clone()],note:Some("This notice applies to this PDF; linked and third-party assets require independent review.".into()),..Default::default()}} else {Rights{status:"unknown".into(),note:Some("No PDF-specific licence or asset permission established; independent review required.".into()),..Default::default()}};
  warnings.push("PDF tables, level-of-care qualifiers, nested clinical conditions, figures, algorithms and physical-page fidelity require original-page review; absence of extracted figures is not evidence of absence. No recommendation grades inferred.".into());
  warnings.push("The observed category listing is not an exhaustive ministry inventory; separately researched 2022 STG volumes and supplements are not assumed linked or equivalent to these catalog records.".into());
  let dates=row.extra["catalog_date"].as_str().map(|d|vec![common::date("Catalog record date",d,row.extra["catalog_fetch"]["final_url"].as_str().unwrap_or(ROOT))]).unwrap_or_default();
  let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(p.record.clone());
  let title=row.title.clone().unwrap_or_default();let l=title.to_lowercase();let item_type=if l.contains("guideline"){Some("guideline".into())}else if l.contains("guide"){Some("guide".into())}else{None};
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),item_type,version:row.version.clone(),status:"unknown".into(),rights,date_observations:dates,blocks,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const CAT2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rw_moh_stg/catalog-page-2.html"));
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rw_moh_stg/catalog-page-1.html"));
 #[test] fn genuine_category_first_page_and_exact_next(){
  let (items,next)=page(CAT,&Url::parse(ROOT).unwrap()).unwrap();assert_eq!(items.len(),10);
  assert!(items[0].0.ends_with("Rwanda_National_Guideline_for_Management_of_Marburg_Virus_Disease_Final_Version_13-10-2024.pdf"));assert_eq!(items[0].2.as_deref(),Some("2024-10-01"));
  let n=next.unwrap();assert!(n.contains("tx_news_pi1%5BcurrentPage%5D=2") && n.contains("cHash=4691e52edd22562fab8017d127a1e41e"));
  let (second,next2)=page(CAT2,&Url::parse(&n).unwrap()).unwrap();assert_eq!(second.len(),4);assert!(next2.is_none());
  assert!(second.iter().any(|(_,t,_)|t.contains("4th Edition, 2011")));
  assert!(!items.iter().chain(second.iter()).any(|(u,_,_)|u.contains("f=92526") || u.contains("f=92531")),"2022 volumes are not linked by these observed category pages");
 }
 #[tokio::test] async fn genuine_clinical_pdf_and_bounded_inventory_offline(){
  let pdf=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rw_moh_stg/marburg-page-12.pdf"));
  let (items,next)=page(CAT,&Url::parse(ROOT).unwrap()).unwrap();let url=&items[0].0;
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
  f.seed_cache_for_tests(ROOT,CAT.as_bytes(),"text/html");f.seed_cache_for_tests(&next.unwrap(),CAT2.as_bytes(),"text/html");f.seed_cache_for_tests(url,pdf,"application/pdf");
  let src:SourceEntry=serde_json::from_value(json!({"id":"rw_moh_stg","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
  let rows=Rwanda.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),14);assert_eq!(rows[0].url,*url);
  assert!(rows.iter().skip(1).all(|r|r.extra["discovery_error"].is_string()));
  let d=Rwanda.extract(&src,&f,&rows[0]).await.unwrap();let text=d.blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");
  assert!(text.contains("specimen") && text.contains("Marburg"));assert!(d.warnings.iter().any(|w|w.contains("physical-page fidelity")));
  let mut altered=pdf.to_vec();altered.extend_from_slice(b"\nchanged snapshot");f.seed_cache_for_tests(url,&altered,"application/pdf");assert!(Rwanda.extract(&src,&f,&rows[0]).await.is_err());
 }

}
