//! Uganda Ministry of Health source records and their actual full-text PDFs.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::doc::{GuidelineDoc,Relation,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct Uganda;
const ROOT:&str="https://library.health.go.ug/node/1511";
fn record(raw:&str,base:&Url)->Result<(String,String,Value,Vec<String>)> {
 let doc=Html::parse_document(raw);let root=doc.select(&sel("main#content [data-history-node-id]")).next().context("Uganda source record missing")?;
 let title=root.select(&sel(".field--name-node-title h2")).next().map(|x|text_of(&x)).filter(|x|!x.is_empty()).context("Uganda record title missing")?;
 let links=root.select(&sel(".view-document-download .cta a[title='View full text'][href]")).filter_map(|a|a.value().attr("href").and_then(|h|common::link(base,h))).collect::<Vec<_>>();
 if links.len()!=1{bail!("Uganda record requires one explicitly labelled full-text PDF, found {}",links.len());}
 let u=Url::parse(&links[0])?;if u.origin()!=base.origin()||!u.path().to_lowercase().ends_with(".pdf"){bail!("Uganda record full-text target is not same-origin PDF")}
 let get=|s:&str|root.select(&sel(s)).next().map(|e|text_of(&e));
 let published=root.select(&sel(".field--name-field-date-of-publication time")).next().and_then(|t|t.value().attr("datetime")).map(String::from);
 let wrapper=root.select(&sel(".view-document-download .cta a[title='Download'][href]")).next().and_then(|a|a.value().attr("href")).and_then(|h|common::link(base,h));
 let metadata=json!({"record_publication_datetime":published,"record_last_modified":get(".field--name-node-changed-date .field__item"),"author":get(".field--name-field-author .field__item"),"publisher":get(".field--name-field-name-of-publisher .field__item"),"document_type":get(".field--name-field-document-type .field__item"),"record_summary":get(".field--name-body .description"),"download_wrapper":wrapper});
 let mut related=vec![];let mut seen=HashSet::new();for a in doc.select(&sel("ul.latest-pub h5 a[href]")){let t=text_of(&a);if !t.to_lowercase().contains("uganda clinical guidelines"){continue;}if let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)){if Url::parse(&url)?.origin()==base.origin() && seen.insert(url.clone()){related.push(url);}}}
 Ok((title,links[0].clone(),metadata,related))
}
#[async_trait::async_trait]
impl Adapter for Uganda {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let root=src.catalog_url.as_deref().unwrap_or(ROOT);let mut queue=vec![(root.to_string(),None::<String>)];let mut seen=HashSet::new();let mut rows=vec![];
  while let Some((url,parent))=queue.pop(){if !seen.insert(url.clone()){continue;}let key=common::slug(Url::parse(&url)?.path());let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-unresolved"),url:url.clone(),title:None,rendition:None,version:None,extra:json!({"work_key":key,"record_url":url,"related_from":parent})};
   match f.get_ok(&url).await {Ok(p)=>match record(&p.text(),&Url::parse(&p.record.final_url)?) {Ok((title,pdf,metadata,related))=>{if seen.len()==1 {for u in related.into_iter().rev(){queue.push((u,Some(url.clone())));}}
      row.title=Some(title.clone());row.url=pdf;row.rendition=Some("pdf".into());row.extra["record_fetch"]=json!(&p.record);let fingerprint=crate::common::sha256_hex(serde_json::to_string(&(&title,&row.url,&metadata))?.as_bytes());row.extra["record_metadata"]=metadata;row.extra["record_snapshot_sha256"]=json!(fingerprint);
      let year=regex::Regex::new(r"\b((?:19|20)\d{2})\b").unwrap();row.version=year.captures(&title).map(|c|c[1].to_string());
      match f.get_ok(&row.url).await {Ok(q) if q.is_pdf()=>{row.doc_key=format!("{key}-{}",&q.record.sha256[..12]);row.extra["pdf_sha256"]=json!(q.record.sha256);},Ok(_)=>row.extra["discovery_error"]=json!("Uganda full text is not PDF"),Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
     },Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
   rows.push(row);if limit.is_some_and(|n|rows.len()>=n){break;}
  }Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("Uganda discovery: {e}")}let record_url=row.extra["record_url"].as_str().context("Uganda source record URL missing")?;let rec_page=f.get_ok(record_url).await?;
  let (current_title,current_pdf,current_metadata,_)=record(&rec_page.text(),&Url::parse(&rec_page.record.final_url)?)?;
  let fingerprint=crate::common::sha256_hex(serde_json::to_string(&(&current_title,&current_pdf,&current_metadata))?.as_bytes());
  if row.extra["record_snapshot_sha256"].as_str().is_some_and(|h|h!=fingerprint){bail!("Uganda source record metadata or full text link changed; re-plan")}
  let p=f.get_ok(&row.url).await?;if !p.is_pdf(){bail!("Uganda full text is not PDF")};if row.extra["pdf_sha256"].as_str().is_some_and(|h|h!=p.record.sha256){bail!("Uganda PDF changed; re-plan")}
  let (blocks,mut warnings,_)=blocks_for(&p,None,30).await?;if blocks.is_empty(){bail!("Uganda PDF contains no extractable text")}
  let pdf_notice=blocks.iter().take(80).map(|b|b.text.as_str()).find(|t|{let l=t.to_lowercase();l.contains("copyright") && (l.contains("all rights reserved") || l.contains("permission to reproduce"))});
  let rights=if let Some(n)=pdf_notice {Rights{status:"restricted".into(),restrictions:vec![n.into()],evidence_urls:vec![row.url.clone()],note:Some("This notice applies to this PDF; linked and third-party assets require independent review.".into()),..Default::default()}} else {Rights{status:"unknown".into(),note:Some("No PDF-specific licence or asset permission established; independent review required.".into()),..Default::default()}};
  warnings.push("PDF conversion may flatten section headings, dosage tables, nested conditions, footnotes and all diagrams. No formal grades inferred. Compare original PDF and page images; absence of structured tables/figures does not mean absence in source.".into());
  warnings.push("Scope is the source record and source-emitted related clinical-guideline records only; complete ministry inventory, editions, currentness and source-item rights are unverified.".into());
  let metadata=&row.extra["record_metadata"];let mut dates=vec![];if let Some(d)=metadata["record_publication_datetime"].as_str().and_then(|s|s.get(..10)){dates.push(common::date("Record publication",d,record_url));}
  if let Some(d)=metadata["record_last_modified"].as_str(){let mut obs=common::date("Record last modified",d,record_url);let re=regex::Regex::new(r"^(\d{1,2})\s+(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec),\s+((?:19|20)\d{2})$").unwrap();
   if let Some(c)=re.captures(d){let months=["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];if let (Some(m),Ok(day))=(months.iter().position(|m|*m==&c[2]),c[1].parse::<u8>()){if (1..=31).contains(&day){obs.normalized=Some(format!("{}-{:02}-{day:02}",&c[3],m+1));}}}dates.push(obs);}
  let attachments=metadata["download_wrapper"].as_str().map(|u|vec![json!({"url":u,"role":"download_wrapper","rights":"unknown; not independently acquired"})]).unwrap_or_default();
  let relations=row.extra["related_from"].as_str().map(|u|vec![Relation{rel:"related".into(),target:u.into(),scope:Some("Source record's related clinical-guideline listing; no supersession inferred".into()),evidence_url:Some(u.into())}]).unwrap_or_default();
  let item_type=match metadata["document_type"].as_str(){Some(s) if s.eq_ignore_ascii_case("Guideline")=>Some("guideline".into()),Some(s) if s.eq_ignore_ascii_case("Text book")=>Some("textbook".into()),_=>None};
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type,version:row.version.clone(),status:"unknown".into(),publisher:metadata["publisher"].as_str().map(String::from),rights,date_observations:dates,relations,blocks,attachments,fetches:vec![rec_page.record,p.record],warnings,search_metadata:json!({"source_record":metadata,"record_url":record_url}),..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const OLD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ug_moh_ucg/record-2016.html"));
 const RECORD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ug_moh_ucg/record-2023.html"));
 #[test] fn genuine_source_record_separates_pdf_and_record_dates(){
  let (title,pdf,m,related)=record(RECORD,&Url::parse(ROOT).unwrap()).unwrap();assert_eq!(title,"Uganda Clinical Guidelines, 2023");
  assert_eq!(pdf,"https://library.health.go.ug/sites/default/files/resources/Uganda%20Clinical%20Guidelines%202023.pdf");
  assert_eq!(m["record_publication_datetime"],"2024-01-01T12:00:00Z");assert_eq!(m["record_last_modified"],"19 Jan, 2024");
  assert!(m["download_wrapper"].as_str().is_some_and(|u|u.ends_with("/file-download/download/public/1701")));
  assert!(related.iter().any(|u|u.ends_with("/uganda-clinical-guidelines-2016")));
  let old_url="https://library.health.go.ug/leadership-and-governance/guidelines/uganda-clinical-guidelines-2016";
  let (old_title,old_pdf,old_meta,_)=record(OLD,&Url::parse(old_url).unwrap()).unwrap();assert_eq!(old_title,"Uganda Clinical Guidelines 2016");
  assert!(old_pdf.ends_with("Uganda%20Clinical%20Guidelines%202016_FINAL.pdf"));assert_eq!(old_meta["document_type"],"Text book");
  assert_eq!(old_meta["record_publication_datetime"],"2016-12-18T12:00:00Z");assert_eq!(old_meta["record_last_modified"],"26 Aug, 2022");
 }
 #[tokio::test] async fn genuine_pdf_path_and_unavailable_older_edition_offline(){
  let pdf=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ug_moh_ucg/ucg-2023-page-21.pdf"));
  let old="https://library.health.go.ug/leadership-and-governance/guidelines/uganda-clinical-guidelines-2016";
  let (_,pdf_url,_,_)=record(RECORD,&Url::parse(ROOT).unwrap()).unwrap();
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
  f.seed_cache_for_tests(ROOT,RECORD.as_bytes(),"text/html");f.seed_cache_for_tests(old,OLD.as_bytes(),"text/html");f.seed_cache_for_tests(&pdf_url,pdf,"application/pdf");
  let src:SourceEntry=serde_json::from_value(json!({"id":"ug_moh_ucg","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
  let rows=Uganda.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),2);assert_eq!(rows[0].version.as_deref(),Some("2023"));assert_eq!(rows[1].version.as_deref(),Some("2016"));assert!(rows[1].extra["discovery_error"].is_string());
  let d=Uganda.extract(&src,&f,&rows[0]).await.unwrap();assert_eq!(d.version.as_deref(),Some("2023"));assert_eq!(d.item_type.as_deref(),Some("guideline"));assert!(d.blocks.iter().any(|b|b.text.contains("Uganda Clinical Guidelines")));assert!(d.warnings.iter().any(|w|w.contains("dosage tables")));assert!(!d.attachments.is_empty());
  let mut altered=pdf.to_vec();altered.extend_from_slice(b"\nchanged snapshot");f.seed_cache_for_tests(&pdf_url,&altered,"application/pdf");assert!(Uganda.extract(&src,&f,&rows[0]).await.is_err());
 }

 #[tokio::test] async fn actual_clinical_page_and_historical_foreword_are_distinct(){
  let clinical=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ug_moh_ucg/ucg-2023-clinical-page-86.pdf"));let old_pdf=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ug_moh_ucg/ucg-2016-page-25.pdf"));let old="https://library.health.go.ug/leadership-and-governance/guidelines/uganda-clinical-guidelines-2016";let (_,current_url,_,_)=record(RECORD,&Url::parse(ROOT).unwrap()).unwrap();let (_,old_url,_,_)=record(OLD,&Url::parse(old).unwrap()).unwrap();let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,RECORD.as_bytes(),"text/html");f.seed_cache_for_tests(old,OLD.as_bytes(),"text/html");f.seed_cache_for_tests(&current_url,clinical,"application/pdf");f.seed_cache_for_tests(&old_url,old_pdf,"application/pdf");let src:SourceEntry=serde_json::from_value(json!({"id":"ug_moh_ucg","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Uganda.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),2);let d=Uganda.extract(&src,&f,&rows[0]).await.unwrap();let text=d.blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ").to_lowercase();assert!(text.contains("malnourished") && text.contains("shock"));assert!(d.warnings.iter().any(|w|w.contains("dosage tables")));let old_doc=Uganda.extract(&src,&f,&rows[1]).await.unwrap();assert_eq!(old_doc.version.as_deref(),Some("2016"));assert_eq!(old_doc.item_type.as_deref(),Some("textbook"));assert!(old_doc.blocks.iter().any(|b|b.text.to_lowercase().contains("health")));assert_ne!(d.doc_key,old_doc.doc_key);
 }

}
