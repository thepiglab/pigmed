//! LEAPP (livingevidence.org.au) index of the Australian Pregnancy Care Guidelines. The index links only MAGICapp
//! guidelines (jm83RE pregnancy care, jW0ZbL postnatal care, 2026-11-17); with `config.magic_api` those MAGIC
//! guidelines are the documents (published PDF via the MAGIC API, `au_magic`), related to the LEAPP index.
//! Without it the index stays a navigation-index record. www.health.gov.au (2020 edition) did not answer from our batch cluster
//! (no response to GET /robots.txt; see registry notes) and is not used.
use super::{au_magic,Adapter,PlanRow};
use crate::guidelines::doc::{GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct AuDepartmentPregnancyCare;
fn index(html:&str,base:&Url)->Result<(String,String,Vec<crate::guidelines::doc::Block>,Vec<Value>,Value)>{let d=Html::parse_document(html);let root=d.select(&sel(".builder-header.-guidelines")).next().context("LEAPP header missing")?;let title=root.select(&sel("h1")).next().map(|h|text_of(&h)).context("LEAPP title missing")?;let mut links=vec![];let mut seen=HashSet::new();for(i,a)in d.select(&sel("a[href]")).enumerate(){let href=a.value().attr("href").unwrap_or_default();let Ok(url)=base.join(href)else{continue};if url.host_str()!=Some("app.magicapp.org")||!matches!(url.scheme(),"http"|"https")||!url.username().is_empty()||url.password().is_some()||url.fragment().is_none(){continue;}let label=text_of(&a);if label.is_empty(){continue;}if seen.insert((url.to_string(),label.clone())){links.push(json!({"url":url.to_string(),"label":label,"role":if url.fragment().is_some_and(|f|f.contains("/section/")){"clinical_platform_section_link"}else{"clinical_platform_guideline_link"},"source_locator":format!("source-anchor/{i}"),"status":"unresolved; platform terms and access independent"}));}}if links.is_empty(){bail!("LEAPP source section links missing")}let sections=d.select(&sel(".left_right_text_with_colours")).map(|s|s.html()).collect::<Vec<_>>();if sections.is_empty(){bail!("LEAPP source navigation sections missing")}let raw=root.html();let hash=crate::common::sha256_hex(format!("{raw}{}{}",serde_json::to_string(&sections)?,serde_json::to_string(&links)?).as_bytes());let(blocks,_)=blocks_from_html(&raw,Some(base),Some(".builder-header.-guidelines"));Ok((title,hash,blocks,links,json!({"source_navigation_sections_html":sections})))}
#[async_trait::async_trait]impl Adapter for AuDepartmentPregnancyCare{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{if limit==Some(0){return Ok(vec![])}let p=f.get_ok(src.catalog_url.as_deref().context("LEAPP source index missing")?).await?;let b=Url::parse(&p.record.final_url)?;let(title,hash,_,links,_)=index(&p.text(),&b)?;
  let magic=au_magic::codes(links.iter().filter_map(|l|l["url"].as_str()));
  if !magic.is_empty()&&src.config["magic_api"].is_object(){
   let idx=au_magic::MagicIndex::load(src,f).await?;
   let mut rows=au_magic::rows_for(src,&idx,&magic,&p.record.url,&title,&json!({"catalog_fetch":p.record}),&mut HashSet::new())?;
   if let Some(n)=limit{rows.truncate(n)}
   return Ok(rows);
  }
  Ok(vec![PlanRow{source_id:src.id.clone(),doc_key:format!("leapp-index-{}",&hash[..12]),url:p.record.url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"source_sha256":hash})}])}
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{if au_magic::is_magic_row(row){return au_magic::extract(src,f,row,|html,b|{let(_,_,_,links,_)=index(html,b)?;Ok(au_magic::codes(links.iter().filter_map(|l|l["url"].as_str())))}).await;}let p=f.get_ok(&row.url).await?;let b=Url::parse(&p.record.final_url)?;let(title,hash,blocks,attachments,metadata)=index(&p.text(),&b)?;if row.extra["source_sha256"].as_str().is_some_and(|h|h!=hash){bail!("LEAPP source index changed")}Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),title,url:row.url.clone(),item_type:Some("guideline_navigation_index".into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),..Default::default()},blocks,attachments,search_metadata:metadata,fetches:vec![p.record],warnings:vec!["This is the public navigation index, not the pregnancy or postnatal guideline chapters. Linked MAGIC routes retain original fragments and remain unresolved; no platform terms agreement or bypass is assumed.".into()],..Default::default()})}
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{use super::*;use std::io::Read;#[test]fn real_index_retains_pregnancy_section_fragment(){let mut raw=String::new();flate2::read::GzDecoder::new(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_department_pregnancy_care/leapp-1.gzpart")).as_slice()).read_to_string(&mut raw).unwrap();let b=Url::parse("https://livingevidence.org.au/living-guidelines/leapp/").unwrap();let(title,_,_,links,metadata)=index(&raw,&b).unwrap();assert!(title.contains("Pregnancy and Postnatal"));assert!(metadata["source_navigation_sections_html"].to_string().contains("Version 11.0"));assert!(metadata["source_navigation_sections_html"].to_string().contains("Optimising pregnancy care"));assert!(links.iter().any(|x|x["url"]=="https://app.magicapp.org/#/guideline/jm83RE/section/jDeKmV"&&x["label"]=="Gestational Diabetes"));}}

#[cfg(all(test, feature = "guideline-fixtures"))]mod magic_route_tests{
 //! The genuine LEAPP index links exactly two MAGIC guidelines; with config.magic_api they become the documents.
 //! The API listing below is SYNTHETIC: a genuine MAGIC API record (h2_catalog cari fixture, CKD 9509 v3.1) with
 //! its shortCode/organisation changed to jm83RE/"leapp_test", to exercise the wiring offline. The real organisation
 //! answer replaces it once fetched.
 use super::*;use std::io::Read;
 const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/barrett-excerpt.pdf"));
 const LEAPP:&str="https://livingevidence.org.au/living-guidelines/leapp/";
 fn raw()->String{let mut raw=String::new();flate2::read::GzDecoder::new(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_department_pregnancy_care/leapp-1.gzpart")).as_slice()).read_to_string(&mut raw).unwrap();raw}
 #[test]fn leapp_links_exactly_the_pregnancy_and_postnatal_magic_guidelines(){let(_,_,_,links,_)=index(&raw(),&Url::parse(LEAPP).unwrap()).unwrap();let mut c=au_magic::codes(links.iter().filter_map(|l|l["url"].as_str()));c.sort();assert_eq!(c,vec!["jW0ZbL","jm83RE"]);}
 #[tokio::test]async fn magic_guidelines_replace_the_navigation_index_when_configured(){
  let cari:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/h2_catalog/magic-cari.json"))).unwrap();
  let mut rec=cari.as_array().unwrap().iter().find(|g|g["shortCode"]=="Lpmozn").unwrap().clone();rec["shortCode"]=json!("jm83RE");rec["institutionShortName"]=json!("leapp_test");
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,8<<20,"").unwrap();
  f.seed_cache_for_tests(LEAPP,raw().as_bytes(),"text/html");
  f.seed_cache_for_tests(&super::super::h2_catalog::magic_list_url("leapp_test"),serde_json::to_vec(&json!([rec])).unwrap().as_slice(),"application/json");
  let pdf_url="https://files.magicapp.org/guideline/2d3034bd-4d24-4963-9259-071abda1f507/published_guideline_9509-3_1.pdf";f.seed_cache_for_tests(pdf_url,PDF,"application/pdf");
  let plain:SourceEntry=serde_json::from_value(json!({"id":"au_department_pregnancy_care","access":"unknown","enabled":true,"catalog_url":LEAPP})).unwrap();
  let rows=AuDepartmentPregnancyCare.discover(&plain,&f,None).await.unwrap();assert_eq!(rows.len(),1);assert!(rows[0].doc_key.starts_with("leapp-index-"));
  let src:SourceEntry=serde_json::from_value(json!({"id":"au_department_pregnancy_care","access":"unknown","enabled":true,"catalog_url":LEAPP,"config":{"magic_api":{"org_short_names":["leapp_test"],"item_type":"guideline","scope":"test"}}})).unwrap();
  let rows=AuDepartmentPregnancyCare.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),2);
  let ok=rows.iter().find(|r|r.extra["magic_code"]=="jm83RE").unwrap();assert_eq!(ok.url,pdf_url);assert!(ok.extra["discovery_error"].is_null());
  let missing=rows.iter().find(|r|r.extra["magic_code"]=="jW0ZbL").unwrap();assert!(missing.extra["discovery_error"].is_string());
  let d=AuDepartmentPregnancyCare.extract(&src,&f,ok).await.unwrap();
  assert!(d.relations.iter().any(|r|r.rel=="related"&&r.target==LEAPP));assert!(d.relations.iter().any(|r|r.rel=="rendition_of"&&r.target=="https://app.magicapp.org/#/guideline/jm83RE"));
  assert!(AuDepartmentPregnancyCare.extract(&src,&f,missing).await.is_err());
 }
}
