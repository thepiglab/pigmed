//! BTS guideline catalog and source-emitted topic PDF assets.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{GuidelineDoc,Rights};
use crate::guidelines::fetch::{Fetched,Fetcher};
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct Bts;
fn catalog(html:&str,base:&Url)->Result<Vec<(String,String,String)>>{let d=Html::parse_document(html);let rows=d.select(&sel("tr.table__body-row")).collect::<Vec<_>>();if rows.is_empty(){bail!("BTS guideline table missing");}let mut out=vec![];let mut seen=HashSet::new();for row in rows{let title=row.select(&sel("td[data-label='Guideline title']")).next().map(|x|text_of(&x)).unwrap_or_default();let date=row.select(&sel("td[data-label='Publication date']")).next().map(|x|text_of(&x)).unwrap_or_default();for a in row.select(&sel("td.table__body-cell--title a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let u=Url::parse(&url)?;if u.origin()==base.origin()&&u.path().starts_with("/clinical-resources/guidelines/")&&seen.insert(url.clone()){out.push((url,title.clone(),date.clone()));}}}if out.is_empty(){bail!("BTS guideline table lacks topic links");}Ok(out)}
fn assets(html:&str,base:&Url)->Result<Vec<(String,String)>>{let d=Html::parse_document(html);let mut seen=HashSet::new();let mut out=vec![];for row in d.select(&sel("tr.table__body-row")){let title=row.select(&sel("td[data-label='Title']")).next().map(|x|text_of(&x)).unwrap_or_default();for a in row.select(&sel("a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let u=Url::parse(&url)?;if u.origin()==base.origin()&&u.path().starts_with("/document-library/guidelines/")&&!title.is_empty()&&seen.insert(url.clone()){out.push((url,title.clone()));}}}// G2-scale-b: the "guideline-disease" topic template lists its PDFs in a Downloads link-list, not a table.
 for a in d.select(&sel(".link-list a.link-list__item-link--download[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let u=Url::parse(&url)?;let label=a.value().attr("title").map(|t|t.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t|!t.is_empty()).unwrap_or_else(||text_of(&a));if u.origin()==base.origin()&&u.path().starts_with("/document-library/guidelines/")&&!label.is_empty()&&seen.insert(url.clone()){out.push((url,label));}}
 if out.is_empty(){let lead=d.select(&sel("article.standard-section__article p")).map(|p|text_of(&p)).find(|t|!t.is_empty());bail!("BTS topic has no observed document-library guideline asset{}",lead.map(|l|format!(" (topic text: \"{}\")",l.chars().take(240).collect::<String>())).unwrap_or_default());}Ok(out)}
/// G2-scale-b: name what a non-PDF document-library asset is, so the refusal is auditable.
fn non_pdf_reason(x:&Fetched,base:&Url,label:&str)->String{let fin=Url::parse(&x.record.final_url).ok();if fin.as_ref().is_some_and(|u|u.origin()!=base.origin()){return format!("source asset '{label}' redirects to {} (another publisher's guideline page, not a BTS PDF); recorded, not substituted",x.record.final_url);}let ct=x.record.content_type.clone().unwrap_or_else(||"unknown type".into());let l=label.to_lowercase();let kind=if l.contains("slide"){"slide set"}else if l.contains("template")||l.contains("card"){"template/form"}else{"non-PDF asset"};if x.is_pdf(){format!("source asset '{label}' is a truncated PDF (no %%EOF)")}else{format!("source asset '{label}' is not a complete PDF: {kind} served as {ct}; supporting material, not a guideline text")}}
fn asset_role(label:&str)->&'static str{let l=label.to_lowercase();if l.contains("summary") {"summary"}else if l.contains("appendix") {"appendix"}else {"source_linked_pdf"}}
fn pdf_complete(f:&Fetched)->bool{f.is_pdf()&&f.body.iter().rev().take(2048).copied().collect::<Vec<_>>().windows(5).any(|x|x==b"FOE%%")}
fn unresolved(src:&SourceEntry,url:String,error:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:None,rendition:None,version:None,extra:json!({"discovery_error":error})}}
#[async_trait::async_trait]impl Adapter for Bts{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{if limit==Some(0){return Ok(vec![])}let cat=f.get_ok(src.catalog_url.as_deref().context("BTS catalog missing")?).await?;let base=Url::parse(&cat.record.final_url)?;let mut rows=vec![];let mut deferred=vec![];
  for(url,title,date)in catalog(&cat.text(),&base)?{let page=match f.get_ok(&url).await{Ok(x)=>x,Err(e)=>{deferred.push(unresolved(src,url,format!("{e:#}")));continue;}};let b=Url::parse(&page.record.final_url)?;if b.origin()!=base.origin(){deferred.push(unresolved(src,url,"topic crossed source origin".into()));continue;}let links=match assets(&page.text(),&b){Ok(x)=>x,Err(e)=>{deferred.push(unresolved(src,url,format!("{e:#}")));continue;}};
   for (pdf,label) in links{let data=match f.get_ok(&pdf).await{Ok(x)if pdf_complete(&x)=>x,Ok(x)=>{let why=non_pdf_reason(&x,&base,&label);deferred.push(unresolved(src,pdf,why));continue;},Err(e)=>{deferred.push(unresolved(src,pdf,format!("{e:#}")));continue;}};let key=common::short_hash(&pdf);rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&data.record.sha256[..12]),url:pdf,title:Some(title.clone()),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":key,"topic_url":url,"link_label":label,"catalog_date":date,"pdf_sha256":data.record.sha256,"topic_sha256":page.record.sha256,"catalog_fetch":cat.record,"topic_fetch":page.record})});if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}}
  }let rem=limit.map(|n|n.saturating_sub(rows.len())).unwrap_or(usize::MAX);rows.extend(deferred.into_iter().take(rem));Ok(rows)}
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{if let Some(e)=row.extra["discovery_error"].as_str(){bail!("BTS source unresolved: {e}");}let topic=row.extra["topic_url"].as_str().context("BTS topic URL missing")?;let page=f.get_ok(topic).await?;if row.extra["topic_sha256"].as_str().is_some_and(|x|x!=page.record.sha256){bail!("BTS topic changed since discovery");}if !assets(&page.text(),&Url::parse(&page.record.final_url)?)?.iter().any(|(u,_)|u==&row.url){bail!("BTS topic no longer links PDF");}let pdf=f.get_ok(&row.url).await?;if !pdf_complete(&pdf){bail!("BTS PDF incomplete");}if row.extra["pdf_sha256"].as_str().is_some_and(|x|x!=pdf.record.sha256){bail!("BTS PDF changed since discovery");}let(blocks,mut warnings,_)=super::static_list::blocks_for(&pdf,None,0).await?;if blocks.is_empty(){bail!("BTS PDF has no mapped text");}warnings.push("BTS topic, source-linked PDF, publisher summary and evidence/algorithm assets are separate; full text and rights are not inferred from a topic label. Generic PDF tables, grade associations, figures and reading order remain unverified.".into());let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(page.record);fetches.push(pdf.record);Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some(asset_role(row.extra["link_label"].as_str().unwrap_or("")).into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),note:Some("BTS PDF and publisher article rights require independent assessment.".into()),..Default::default()},search_metadata:json!({"topic_url":topic,"source_link_label":row.extra["link_label"],"catalog_date_raw":row.extra["catalog_date"]}),blocks,fetches,warnings,..Default::default()})}
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{use super::*;const A:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bts/catalog-1.html"));const B:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bts/catalog-2.html"));#[test]fn real_catalog_deduplicates_responsive_topic_links(){let base=Url::parse("https://www.brit-thoracic.org.uk/clinical-resources/guidelines/").unwrap();let rows=catalog(&format!("{A}{B}"),&base).unwrap();assert_eq!(rows.iter().filter(|(u,_,_)|u.ends_with("/bronchiectasis-in-adults/")).count(),1);assert!(rows.iter().any(|(u,t,_)|u.ends_with("/bronchiectasis-in-adults/")&&t.to_lowercase().contains("bronchiectasis")));assert!(catalog("<html/>",&base).is_err());}}

#[cfg(all(test, feature = "guideline-fixtures"))] mod acquired_topic_test {
 use super::*;
 const A:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bts/bronchiectasis-1.html"));
 const B:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bts/bronchiectasis-2.html"));
 #[test] fn topic_retains_distinct_labelled_assets(){let u=Url::parse("https://www.brit-thoracic.org.uk/clinical-resources/guidelines/bronchiectasis-in-adults/").unwrap();let rows=assets(&format!("{A}{B}"),&u).unwrap();assert_eq!(rows.len(),5);assert!(rows.iter().any(|(_,l)|l.contains("summary for the general public")&&asset_role(l)=="summary"));assert!(rows.iter().any(|(_,l)|l.contains("Web Appendix 3")&&asset_role(l)=="appendix"));}
}

#[cfg(all(test, feature = "guideline-fixtures"))] mod g2b_tests {
 use super::*;
 const P:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bts/paediatric-sdb-topic.html"));
 const E:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bts/emergency-oxygen-target-topic.html"));
 #[test] fn guideline_disease_downloads_list_and_in_development_topics(){
  let u=Url::parse("https://www.brit-thoracic.org.uk/clinical-resources/guidelines/paediatric-sleep-disordered-breathing/").unwrap();
  let rows=assets(P,&u).unwrap();assert_eq!(rows.len(),2);
  assert!(rows.iter().any(|(x,l)|x=="https://www.brit-thoracic.org.uk/document-library/guidelines/paediatric-sleep-disordered-breathing/bts-guideline-paediatric-sleep-disordered-breathing/"&&l=="BTS Guideline paediatric sleep-disordered breathing"&&asset_role(l)=="source_linked_pdf"));
  assert!(rows.iter().any(|(x,l)|x.ends_with("-summary/")&&asset_role(l)=="summary"));
  let e=assets(E,&Url::parse("https://www.brit-thoracic.org.uk/clinical-resources/guidelines/emergency-oxygen-target-oxygen-saturation/").unwrap()).unwrap_err().to_string();
  assert!(e.contains("no observed document-library guideline asset")&&e.contains("currently under development"),"{e}");
 }
 #[tokio::test] async fn non_pdf_document_library_assets_are_named_not_parsed(){
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();
  let base=Url::parse("https://www.brit-thoracic.org.uk/clinical-resources/guidelines/").unwrap();
  let slides="https://www.brit-thoracic.org.uk/document-library/guidelines/ntm/bts-ntm-guideline-slide-set/";
  f.seed_cache_for_tests(slides,b"PK\x03\x04\x14\x00\x06\x00 slide-set bytes","application/vnd.ms-powerpoint");
  let x=f.get(slides).await.unwrap();assert!(!pdf_complete(&x));
  let r=non_pdf_reason(&x,&base,"BTS NTM Guideline slide set");assert!(r.contains("slide set")&&r.contains("application/vnd.ms-powerpoint")&&r.contains("not a complete PDF"),"{r}");
  let card="https://www.brit-thoracic.org.uk/document-library/guidelines/emergency-oxygen/oxygen-alert-card-template-2017/";
  f.seed_cache_for_tests(card,b"PK\x03\x04 docx bytes","application/msword");
  let r=non_pdf_reason(&f.get(card).await.unwrap(),&base,"Oxygen alert card template 2017");assert!(r.contains("template/form")&&r.contains("application/msword"),"{r}");
 }
}
