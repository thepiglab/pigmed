//! ATA publisher catalog, preserving journal terminals and genuine access outcomes.
use super::pmc_crosswalk::{self as cw,Lookup,Planned,SourceRef};
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::GuidelineDoc;
use crate::guidelines::fetch::{Fetched,Fetcher};
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct Ata;
const ROOT:&str="https://www.thyroid.org/professionals/ata-professional-guidelines/";
async fn public(f:&Fetcher,url:&str)->Result<Fetched>{let p=f.get(url).await?;if p.record.status!=200||!p.is_html(){bail!("ATA catalog returned HTTP {} or non-HTML",p.record.status)}if Url::parse(&p.record.final_url)?.host_str()!=Some("www.thyroid.org"){bail!("ATA catalog redirected off source")};Ok(p)}
fn catalog(raw:&str,base:&Url)->Result<Vec<(String,String,String)>>{let d=Html::parse_document(raw);let root=d.select(&sel("div.container-wrap")).next().context("ATA catalog content root missing")?;if root.select(&sel("form input[type='password'],form iframe[src*='recaptcha'],form input[name*='captcha']")).next().is_some(){bail!("ATA catalog contains credential or CAPTCHA gate")};if !root.select(&sel("h2")).any(|h|text_of(&h).contains("Guidelines and Surgical Statements")){bail!("ATA catalog heading missing")};let mut seen=HashSet::new();let mut rows=vec![];for a in root.select(&sel("a[ga-event-category^='Guidelines'][href]")){let title=text_of(&a);if title.is_empty(){continue}let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h))else{continue};let u=Url::parse(&url)?;if !matches!(u.scheme(),"http"|"https")||!seen.insert(url.clone()){continue}rows.push((url,title,a.value().attr("ga-event-category").unwrap_or_default().into()));}if rows.is_empty(){bail!("ATA source guideline article cards missing")};Ok(rows)}
#[async_trait::async_trait]
impl Adapter for Ata{
 // Source-linked publisher URLs state their DOI; an Open Access PMC copy is used through the shared crosswalk.
 // Without one, the publisher outcome (Sage/Liebert 403) stays an explicit unresolved row.
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![])}
  let root=src.catalog_url.as_deref().unwrap_or(ROOT);let p=public(f,root).await?;let base=Url::parse(&p.record.final_url)?;let rows=catalog(&p.text(),&base)?;let mut out=vec![];let mut dois=HashSet::new();
  for(url,title,category)in rows{
   let mut crosswalk="no DOI stated by the source link".to_string();
   if let Some(doi)=cw::doi_from_url(&url){
    if !dois.insert(doi.clone()){continue}
    let sref=SourceRef{source_url:url.clone(),evidence_url:Some(base.to_string()),source_title:Some(title.clone()),item_type:Some("guideline".into()),key_prefix:"ata".into(),id_route:"source_doi".into(),notes:vec![]};
    match cw::plan(src,f,Lookup::Doi(doi.clone()),sref,json!({"source_category":category,"catalog_fetch":p.record,"source_doi":doi})).await?{
     Planned::Open(row)=>{out.push(row);if limit.is_some_and(|n|out.len()>=n){break}continue}
     Planned::Closed{reason,..}=>crosswalk=format!("DOI {doi}: {reason}"),
    }
   }
   let reason=match f.get_ok(&url).await{Err(e)=>format!("ATA source publisher article unavailable: {e:#}; open-access crosswalk: {crosswalk}"),Ok(_)=>format!("ATA source publisher article returned bytes, but its full clinical template and rights have not been validated; open-access crosswalk: {crosswalk}")};
   out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("ata-{}-unresolved",common::short_hash(&url)),url,title:Some(title),rendition:None,version:None,extra:json!({"source_category":category,"catalog_fetch":p.record,"discovery_error":reason})});if limit.is_some_and(|n|out.len()>=n){break}
  }Ok(out)}
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{if cw::is_crosswalk_row(row){return cw::extract(src,f,row).await}bail!("ATA publisher terminal unresolved: {}",row.extra["discovery_error"].as_str().unwrap_or("unknown"))}
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{use super::*;const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ata_thyroid/catalog.html"));
 #[test]fn actual_catalog_accepts_public_root_and_rejects_real_form(){let rows=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();assert!(rows.iter().any(|(u,t,_)|u.contains("10507256261445624")&&t.contains("Pregnancy")));assert!(rows.iter().any(|(u,_,_)|u.contains("10.1089/thy.2020.0944")));let gated=CAT.replacen("<div class=\"container-wrap\">","<div class=\"container-wrap\"><form><input type='password'></form>",1);assert!(catalog(&gated,&Url::parse(ROOT).unwrap()).is_err());}
 #[tokio::test]async fn source_articles_do_not_become_summaries_when_unavailable(){let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,CAT.as_bytes(),"text/html");let src:SourceEntry=serde_json::from_value(json!({"id":"ata_thyroid","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Ata.discover(&src,&f,Some(2)).await.unwrap();assert_eq!(rows.len(),2);assert!(rows.iter().all(|r|r.extra["discovery_error"].is_string()));assert!(Ata.extract(&src,&f,&rows[0]).await.is_err());}
}
