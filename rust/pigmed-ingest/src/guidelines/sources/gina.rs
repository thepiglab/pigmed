//! GINA source-emitted report inventory, keeping full and summary roles separate.
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
pub struct Gina;
const ROOT:&str="https://ginasthma.org/";
fn report_hub(raw:&str,base:&Url)->Result<String>{
 let d=Html::parse_document(raw);let root=d.select(&sel("main#main")).next().context("GINA source homepage main missing")?;
 let re=regex::Regex::new(r"^((?:19|20)\d{2}) Reports$").unwrap();let mut links=HashSet::new();
 for a in root.select(&sel("a[href]")){if !re.is_match(&text_of(&a)){continue;}let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};let u=Url::parse(&url)?;if u.origin()==base.origin() && u.path().trim_end_matches('/')=="/reports" && u.query().is_none(){links.insert(url);}}
 if links.len()!=1{bail!("GINA expected one source-labelled reports hub, found {}",links.len())}Ok(links.into_iter().next().unwrap())
}
fn branches(raw:&str,base:&Url)->Result<Vec<(String,String)>>{
 let d=Html::parse_document(raw);let root=d.select(&sel("main#main")).next().context("GINA reports main missing")?;let mut out=vec![];
 for a in root.select(&sel("a[href]")){let label=text_of(&a);let role=match label.as_str(){"Strategy Report"=>"strategy_report","Summary Guide"=>"summary_guide","Severe Asthma Guide"=>"severe_asthma_guide",_=>continue};
  if let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)){if Url::parse(&url)?.origin()==base.origin() && !out.iter().any(|(r,_)|r==role){out.push((role.into(),url));}}}
 if out.len()!=3{bail!("GINA report hub expected all three publisher-labelled clinical branches; found {}",out.len())}Ok(out)
}
fn landing(raw:&str,base:&Url)->Result<(String,String,Option<String>,String)>{
 let d=Html::parse_document(raw);let root=d.select(&sel("main#main")).next().context("GINA report landing main missing")?;
 let title=root.select(&sel("h1.entry-title")).next().map(|x|text_of(&x)).context("GINA landing title missing")?;
 let links=root.select(&sel("a[href]")).filter(|a|text_of(a)=="Download Free PDF").filter_map(|a|a.value().attr("href").and_then(|h|common::link(base,h))).collect::<Vec<_>>();
 if links.len()!=1{bail!("GINA expected one source-labelled free PDF")};let u=Url::parse(&links[0])?;if u.origin()!=base.origin()||!u.path().to_lowercase().ends_with(".pdf"){bail!("GINA PDF target outside source origin or not PDF")}
 let notice=root.select(&sel("em")).map(|e|text_of(&e)).find(|s|s.contains("GINA documents are protected by copyright") && s.contains("distribution")).context("GINA source copyright notice missing")?;
 let version=regex::Regex::new(r"\b((?:19|20)\d{2})\b").unwrap().captures(&title).map(|c|c[1].to_string());Ok((title,links[0].clone(),version,notice))
}
#[async_trait::async_trait]
impl Adapter for Gina {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let home=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;let base=Url::parse(&home.record.final_url)?;if base.origin()!=Url::parse(ROOT)?.origin(){bail!("GINA home redirected outside publisher origin")}
  let hub=report_hub(&home.text(),&base)?;let h=f.get_ok(&hub).await?;let hu=Url::parse(&h.record.final_url)?;if hu.origin()!=base.origin(){bail!("GINA reports hub redirected outside publisher origin")};let entries=branches(&h.text(),&hu)?;let mut rows=vec![];
  for (role,url) in entries {let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url:url.clone(),title:Some(role.clone()),rendition:None,version:None,extra:json!({"role":role,"landing_url":url,"home_fetch":home.record,"hub_fetch":h.record})};
   match f.get_ok(&url).await {Ok(p)=>{let u=Url::parse(&p.record.final_url)?;if u.origin()!=base.origin(){row.extra["discovery_error"]=json!("GINA landing redirected outside source origin");}else {match landing(&p.text(),&u){Ok((title,pdf,version,notice))=>{row.title=Some(title);row.url=pdf;row.version=version;row.rendition=Some("pdf".into());row.extra["landing_fetch"]=json!(p.record);row.extra["notice"]=json!(notice);
     match f.get_ok(&row.url).await{Ok(q) if q.is_pdf() && Url::parse(&q.record.final_url).is_ok_and(|u|u.origin()==base.origin())=>{row.doc_key=format!("{}-{}",common::short_hash(&url),&q.record.sha256[..12]);row.extra["pdf_sha256"]=json!(q.record.sha256);},Ok(_)=>row.extra["discovery_error"]=json!("GINA free PDF unavailable or redirected outside source"),Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
   },Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}}},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))};rows.push(row);if limit.is_some_and(|n|rows.len()>=n){break;}}
  Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("GINA discovery: {e}")};let landing_url=row.extra["landing_url"].as_str().context("GINA landing missing")?;let l=f.get_ok(landing_url).await?;let origin=Url::parse(ROOT)?.origin();let u=Url::parse(&l.record.final_url)?;if u.origin()!=origin{bail!("GINA landing redirected outside source")};
  let (title,pdf,version,notice)=landing(&l.text(),&u)?;if Some(title.as_str())!=row.title.as_deref() || pdf!=row.url || version!=row.version || Some(notice.as_str())!=row.extra["notice"].as_str(){bail!("GINA landing changed; re-plan")}
  let p=f.get_ok(&row.url).await?;if !p.is_pdf() || Url::parse(&p.record.final_url)?.origin()!=origin{bail!("GINA PDF unavailable or redirected outside source")};if row.extra["pdf_sha256"].as_str().is_some_and(|h|h!=p.record.sha256){bail!("GINA PDF changed; re-plan")}
  let (blocks,mut warnings,_)=blocks_for(&p,None,30).await?;if blocks.is_empty(){bail!("GINA PDF yielded no text")};warnings.push("PDF watermark text can interrupt clinical sentences. Tables, embedded figures and algorithms require original-page review; no recommendation grades inferred.".into());warnings.push("Reports hub also links archives and translations; these have not been enumerated by this clinical-branch adapter. Summary and severe-asthma guides are not substitutes for the strategy report.".into());
  let rights=Rights{status:"restricted".into(),restrictions:vec![notice],evidence_urls:vec![landing_url.into()],note:Some("Publisher notice on the source landing applies to named GINA documents; third-party and embedded asset rights require separate review.".into()),..Default::default()};
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:Some(format!("gina-{}",row.extra["role"].as_str().unwrap_or("report"))),title,url:row.url.clone(),item_type:Some(if row.extra["role"].as_str()==Some("strategy_report"){"report"}else{"guide"}.into()),version,status:"unknown".into(),rights,blocks,fetches:vec![l.record,p.record],warnings,search_metadata:json!({"source_role":row.extra["role"],"source_landing":landing_url}),..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests{
 use super::*;
 const HOME:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/gina/home.html"));
 const HUB:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/gina/reports.html"));
 const STRATEGY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/gina/strategy.html"));
 const SEVERE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/gina/severe.html"));
 const CLINICAL:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/gina/strategy-clinical-blocks.json"));
 const SUMMARY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/gina/summary.html"));
 #[test] fn source_graph_roles_links_and_restrictions(){let root=Url::parse(ROOT).unwrap();let hub=report_hub(HOME,&root).unwrap();assert_eq!(hub,"https://ginasthma.org/reports/");let b=branches(HUB,&Url::parse(&hub).unwrap()).unwrap();assert_eq!(b.len(),3);assert_eq!(b[0].0,"strategy_report");assert_eq!(b[1].0,"summary_guide");assert_eq!(b[2].0,"severe_asthma_guide");
  let (title,pdf,year,notice)=landing(STRATEGY,&Url::parse(&b[0].1).unwrap()).unwrap();assert_eq!(title,"2026 GINA Strategy Report");assert!(pdf.ends_with("GINA-2026-Strategy-Report-WMS.pdf"));assert_eq!(year.as_deref(),Some("2026"));assert!(notice.contains("may not be made for distribution") && notice.contains("commercial or promotional"));
  let (_,pdf,_,_)=landing(SUMMARY,&Url::parse(&b[1].1).unwrap()).unwrap();assert!(pdf.ends_with("GINA-Summary-Guide-2026-WEB-WMS.pdf"));let (_,pdf,_,_)=landing(SEVERE,&Url::parse(&b[2].1).unwrap()).unwrap();assert!(pdf.ends_with("GINA-Severe-Asthma-Guide-2026-WEB-WMS.pdf"));let blocks:Vec<crate::guidelines::doc::Block>=serde_json::from_str(CLINICAL).unwrap();assert!(blocks.iter().any(|b|b.text.contains("COPYRIGHTED") && b.text.to_lowercase().contains("asthma")));}
 #[tokio::test] async fn genuine_branch_discovery_retains_unavailable_pdf_roles(){let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,HOME.as_bytes(),"text/html");f.seed_cache_for_tests("https://ginasthma.org/reports/",HUB.as_bytes(),"text/html");for (u,h) in [("https://ginasthma.org/2026-gina-strategy-report/",STRATEGY),("https://ginasthma.org/2026-gina-summary-guide/",SUMMARY),("https://ginasthma.org/2026-gina-severe-asthma-guide/",SEVERE)]{f.seed_cache_for_tests(u,h.as_bytes(),"text/html");}let src:SourceEntry=serde_json::from_value(json!({"id":"gina","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Gina.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),3);assert!(rows.iter().all(|r|r.extra["discovery_error"].is_string()));assert_eq!(rows[0].extra["role"],"strategy_report");assert_eq!(rows[1].extra["role"],"summary_guide");assert_eq!(rows[2].extra["role"],"severe_asthma_guide");}

}
