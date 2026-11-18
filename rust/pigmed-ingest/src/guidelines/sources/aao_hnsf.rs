//! AAO-HNSF paginated guideline listings, source detail pages and independently linked assets.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::{Fetched,Fetcher};
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct AaoHnsf;
/// Every guideline the catalog lists under its "Published Clinical Practice Guidelines" heading (up to the next h3):
/// detail pages under the CPG path (`cpg-*`, but also e.g. `menieres-disease/`, `tympanostomy-tubes-in-children/`) and
/// the older `/resource/clinical-practice-guideline-*/` entries (which 301 to their CPG-path detail page). Without
/// that heading the historic rule (CPG-path `cpg-*` links anywhere in main) applies.
fn catalog(html:&str,base:&Url)->Result<Vec<(String,String)>>{
 let d=Html::parse_document(html);let main=d.select(&sel("main#main-container")).next().context("AAO-HNSF catalog main missing")?;
 const CPG:&str="/quality-practice/quality-products/clinical-practice-guidelines/";
 let mut rows=Vec::new();let mut seen=HashSet::new();let mut state=0u8;
 for node in main.descendants(){let Some(e)=scraper::ElementRef::wrap(node) else {continue};match e.value().name(){
  "h3"=>{let t=text_of(&e).to_lowercase();if t.contains("published clinical practice guidelines"){state=1}else if state==1{state=2}}
  "a" if state==1=>{let Some(url)=e.value().attr("href").and_then(|h|common::link(base,h))else{continue};let u=Url::parse(&url)?;if u.origin()!=base.origin(){continue}
   let p=u.path();let ok=(p.starts_with(CPG)&&p.len()>CPG.len()&&p.trim_end_matches('/')!=CPG.trim_end_matches('/'))||p.starts_with("/resource/clinical-practice-guideline-");
   let title=text_of(&e);if ok&&!title.is_empty()&&seen.insert(url.clone()){rows.push((url,title));}}
  _=>{}}}
 if !rows.is_empty(){return Ok(rows)}
 for a in main.select(&sel("a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let u=Url::parse(&url)?;if u.origin()!=base.origin()||!u.path().starts_with("/quality-practice/quality-products/clinical-practice-guidelines/cpg-"){continue;}let title=text_of(&a);if !title.is_empty()&&seen.insert(url.clone()){rows.push((url,title));}}
 if rows.is_empty(){anyhow::bail!("AAO-HNSF guideline topic list missing")}Ok(rows)
}

fn detail(html:&str,base:&Url)->Result<Vec<(String,String)>>{let d=Html::parse_document(html);let root=d.select(&sel("main#main-container")).next().context("AAO-HNSF topic main missing")?;let mut out=vec![];let mut seen=HashSet::new();for a in root.select(&sel("a[href]")){let label=text_of(&a);if !label.to_lowercase().contains("summary of guideline key action statements"){continue;}let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let u=Url::parse(&url)?;if u.origin()==base.origin()&&u.path().to_lowercase().ends_with(".pdf")&&seen.insert(url.clone()){out.push((url,label));}}if out.is_empty(){bail!("AAO-HNSF topic has no source-labelled key action summary PDF")}Ok(out)}
fn external_key_action_links(html:&str,base:&Url)->Result<Vec<(String,String)>>{let d=Html::parse_document(html);let root=d.select(&sel("main#main-container")).next().context("AAO-HNSF topic main missing")?;let mut seen=HashSet::new();let mut out=vec![];for a in root.select(&sel("a[href]")){let label=text_of(&a);if !label.to_lowercase().contains("summary of guideline key action statements"){continue}let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let u=Url::parse(&url)?;if u.origin()!=base.origin()&&matches!(u.scheme(),"http"|"https")&&seen.insert(url.clone()){out.push((url,label))}}Ok(out)}
fn pdf_complete(f:&Fetched)->bool{f.is_pdf()&&f.body.iter().rev().take(2048).copied().collect::<Vec<_>>().windows(5).any(|x|x==b"FOE%%")}
fn unresolved(src:&SourceEntry,url:String,error:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:None,rendition:None,version:None,extra:json!({"discovery_error":error})}}
fn map_key_actions(blocks:&mut Vec<Block>,warnings:&mut Vec<String>)->Result<usize>{
 let mut work=Vec::new();
 for block in blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})){
  let mut lines=block.text.lines();
  let Some(header)=lines.next() else {continue};
  let fields=header.trim().trim_matches('|').split('|').map(str::trim).collect::<Vec<_>>();
  if fields!=["STATEMENT","ACTION","STRENGTH"]{continue;}
  let _separator=lines.next();let mut statements=Vec::new();
  for (i,line) in lines.enumerate(){let cells=line.trim().trim_matches('|').split('|').map(str::trim).collect::<Vec<_>>();
   if cells.len()!=3 || !cells[0].starts_with("KAS ") || cells[1].is_empty() || cells[2].is_empty(){warnings.push(format!("AAO-HNSF table {} row {} not mapped; original table retained",block.block_id,i+1));continue;}
   statements.push(Block{block_id:format!("{}-kas-{}",block.block_id,i+1),kind:BlockKind::Recommendation{label:Some(cells[0].into()),strength:Some(cells[2].into()),certainty:None,population:None,grading_system:None,qualifier:None},text:cells[1].into(),heading_path:block.heading_path.clone(),page:block.page});
  }
  work.push((block.block_id.clone(),statements));
 }
 let count=work.iter().map(|(_,rows)|rows.len()).sum();for(id,rows)in work{common::insert_table_recommendations(blocks,&id,rows)?;}Ok(count)
}

#[async_trait::async_trait]impl Adapter for AaoHnsf{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{if limit==Some(0){return Ok(vec![])}let root=src.catalog_url.as_deref().context("AAO-HNSF catalog missing")?;let first=f.get_ok(root).await?;let base=Url::parse(&first.record.final_url)?;let mut rows=vec![];let mut deferred=vec![];for(url,title)in catalog(&first.text(),&base)?{let topic=match f.get_ok(&url).await{Ok(x)=>x,Err(e)=>{deferred.push(unresolved(src,url,format!("{e:#}")));continue}};let tb=Url::parse(&topic.record.final_url)?;if tb.origin()!=base.origin(){deferred.push(unresolved(src,url,"topic origin changed".into()));continue;}let links=match detail(&topic.text(),&tb){Ok(x)=>x,Err(e)=>{for(target,label)in external_key_action_links(&topic.text(),&tb)?{let mut r=unresolved(src,target,"Source-labelled key action summary uses an external terminal; access, MIME and rights not validated".into());r.extra["source_detail"]=json!(url);r.extra["link_label"]=json!(label);r.extra["role"]=json!("key_action_summary_unverified");deferred.push(r)}deferred.push(unresolved(src,url,format!("{e:#}")));continue}};for(pdf,label)in links{let data=match f.get_ok(&pdf).await{Ok(x)if pdf_complete(&x)=>x,Ok(_)=>{deferred.push(unresolved(src,pdf,"not a complete PDF".into()));continue},Err(e)=>{deferred.push(unresolved(src,pdf,format!("{e:#}")));continue}};let key=common::short_hash(&pdf);rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&data.record.sha256[..12]),url:pdf,title:Some(title.clone()),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":key,"detail_url":url,"link_label":label,"pdf_sha256":data.record.sha256,"detail_sha256":topic.record.sha256,"catalog_fetch":first.record})});if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}}}let rem=limit.map(|n|n.saturating_sub(rows.len())).unwrap_or(usize::MAX);rows.extend(deferred.into_iter().take(rem));Ok(rows)}
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{if let Some(e)=row.extra["discovery_error"].as_str(){bail!("AAO-HNSF source unresolved: {e}");}let detail_url=row.extra["detail_url"].as_str().context("detail URL missing")?;let d=f.get_ok(detail_url).await?;if row.extra["detail_sha256"].as_str().is_some_and(|x|x!=d.record.sha256){bail!("AAO-HNSF detail changed since discovery");}if !detail(&d.text(),&Url::parse(&d.record.final_url)?)?.iter().any(|(u,_)|u==&row.url){bail!("AAO-HNSF PDF no longer linked by detail page");}let pdf=f.get_ok(&row.url).await?;if !pdf_complete(&pdf){bail!("AAO-HNSF PDF incomplete");}if row.extra["pdf_sha256"].as_str().is_some_and(|x|x!=pdf.record.sha256){bail!("AAO-HNSF PDF changed since discovery");}let (mut blocks,mut warnings,_)=super::static_list::blocks_for(&pdf,None,0).await?;if blocks.is_empty(){bail!("AAO-HNSF PDF has no mapped text");}let mapped=map_key_actions(&mut blocks,&mut warnings)?;if mapped==0{warnings.push("No known KAS table shape was mapped; original PDF text remains.".into());}
  warnings.push("Source detail and linked PDF are separate assets; label and clinical role are retained without assuming every PDF is a full guideline. Generic PDF conversion does not verify tables, grades, figures or page order.".into());let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(d.record);fetches.push(pdf.record);Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some("key_action_summary".into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),note:Some("PDF, publisher article, and related materials have independent unverified rights.".into()),..Default::default()},search_metadata:json!({"detail_url":detail_url,"source_pdf_link_label":row.extra["link_label"]}),blocks,fetches,warnings,..Default::default()})}
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{use super::*;#[test]fn genuine_catalog_and_topic_emit_specific_summary(){let cat=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/catalog-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/catalog-2.html"))].concat();let topic=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/sinusitis-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/sinusitis-2.html"))].concat();let u=Url::parse("https://www.entnet.org/quality-practice/quality-products/clinical-practice-guidelines/").unwrap();assert!(catalog(&cat,&u).unwrap().iter().any(|(u,t)|u.ends_with("cpg-adult-sinusitis/")&&t.contains("Adult Sinusitis")));assert!(detail(&topic,&u).unwrap().iter().any(|(u,_)|u=="https://www.entnet.org/wp-content/uploads/2025/07/CPG_ASU_SummaryGuide_V3.pdf"));}}

#[cfg(all(test, feature = "guideline-fixtures"))]mod pdf_tests{
 use super::*;use serde_json::json;
 #[tokio::test]async fn genuine_summary_keeps_kas_scope_action_and_strength(){
  let bytes=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/adult-sinusitis-summary.pdf"));let url="https://www.entnet.org/wp-content/uploads/2025/07/CPG_ASU_SummaryGuide_V3.pdf";
  let record=serde_json::from_value(json!({"url":url,"final_url":url,"redirects":[],"status":200,"content_type":"application/pdf","bytes":bytes.len(),"sha256":crate::common::sha256_hex(bytes),"fetched_at":"source fixture","from_cache":true})).unwrap();let f=crate::guidelines::fetch::Fetched{record,body:bytes.to_vec()};
  let (mut blocks,mut warnings,_)=super::super::static_list::blocks_for(&f,None,0).await.unwrap();assert!(map_key_actions(&mut blocks,&mut warnings).unwrap()>10);assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),strength:Some(s),..} if l.contains("KAS 1A")&&s=="Strong recommendation")&&b.text.contains("10 days")&&b.text.contains("double worsening")));assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),strength:Some(s),..} if l.contains("KAS 1B")&&s.contains("against"))));
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod external_kas_live{use super::*;const TOPIC:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/tinnitus.html"));
#[test]fn source_label_external_target_is_preserved_without_access_assumption(){let base=Url::parse("https://www.entnet.org/quality-practice/quality-products/clinical-practice-guidelines/cpg-tinnitus-update/").unwrap();assert!(detail(TOPIC,&base).is_err());let links=external_key_action_links(TOPIC,&base).unwrap();assert_eq!(links.len(),1);assert!(links[0].0.starts_with("https://entnet.sharepoint.com/"));assert!(links[0].1.contains("Summary of Guideline Key Action Statements"));}
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod published_section{use super::*;
 const BASE:&str="https://www.entnet.org/quality-practice/quality-products/clinical-practice-guidelines/";
 #[test]fn every_published_guideline_is_listed_once_in_source_order(){
  let cat=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/catalog-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/catalog-2.html"))].concat();
  let rows=catalog(&cat,&Url::parse(BASE).unwrap()).unwrap();assert_eq!(rows.len(),22);
  assert!(rows[0].0.ends_with("/cpg-tinnitus-update/"));assert!(rows.iter().any(|(u,t)|u.ends_with("/menieres-disease/")&&t.contains("Ménière")));
  assert!(rows.iter().any(|(u,t)|u.ends_with("/resource/clinical-practice-guideline-benign-paroxysmal-positional-vertigo-bppv/")&&t.contains("BPPV")));
  assert!(rows.iter().all(|(u,_)|!u.contains("pocket-guides")&&!u.contains("development-tools")&&!u.contains("partnerships")));
  // without the published heading the historic cpg-* rule still applies
  let old=cat.replace("Published Clinical Practice Guidelines","Guidelines");assert_eq!(catalog(&old,&Url::parse(BASE).unwrap()).unwrap().len(),5);
 }
 #[tokio::test]async fn journal_only_detail_page_stays_an_explicit_unresolved_row(){
  let cat=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/catalog-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/catalog-2.html"))].concat();
  let men=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aao_hnsf/menieres-disease.html"));
  let u=format!("{BASE}menieres-disease/");assert!(detail(men,&Url::parse(&u).unwrap()).is_err());
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,4<<20,"").unwrap();f.seed_cache_for_tests(BASE,cat.as_bytes(),"text/html");f.seed_cache_for_tests(&u,men.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"aao_hnsf","access":"public","enabled":true,"catalog_url":BASE})).unwrap();
  let rows=AaoHnsf.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),22,"one row per published guideline (offline: all unresolved)");
  let m=rows.iter().find(|r|r.url==u).unwrap();assert!(m.extra["discovery_error"].as_str().unwrap().contains("key action summary"));
 }
}
