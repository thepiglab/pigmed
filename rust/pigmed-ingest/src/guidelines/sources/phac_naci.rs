//! NACI statements: source catalog, separately identified summaries and conservative numbered statements.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct PhacNaci;
const MAIN:&str="main[property='mainContentOfPage']";
fn catalog(html:&str,base:&Url)->Result<Vec<(String,String)>> {
 let doc=Html::parse_document(html);let root=doc.select(&sel(MAIN)).next().context("NACI catalog root missing")?;
 let mut seen=HashSet::new();let mut rows=Vec::new();
 for a in root.select(&sel("a[href]")) {
  let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};let u=Url::parse(&url)?;
  if u.origin()!=base.origin() || !u.path().ends_with(".html") || u.query().is_some(){continue;}
  let p=u.path();if !p.starts_with("/en/public-health/services/publications/vaccines-immunization/") && !p.starts_with("/en/public-health/services/immunization/national-advisory-committee-on-immunization-naci/") {continue;}
  let label=text_of(&a);if !label.is_empty()&&seen.insert(url.clone()){rows.push((url,label));}
 }
 if rows.is_empty(){bail!("NACI catalog has no candidate links");}Ok(rows)
}
fn clinical(html:&str,base:&Url)->Result<(String,String,Vec<DateObservation>,Vec<Block>,Vec<String>,Vec<serde_json::Value>)> {
 let doc=Html::parse_document(html);let root=doc.select(&sel(MAIN)).next().context("NACI main missing")?;
 let title=root.select(&sel("h1#wb-cont")).next().map(|e|text_of(&e)).context("NACI title missing")?;
 if root.select(&sel("input[type=password],form[action*='login'],iframe[src*='captcha']")).next().is_some(){bail!("NACI gated or challenged");}
 let low_title=title.to_lowercase();let summary=low_title.starts_with("summary of ")||low_title.starts_with("archived: summary of ");
 let has_recommendations=root.select(&sel("h2")).any(|h|text_of(&h).to_lowercase().contains("recommendation"));
 let has_summary_body=summary && root.select(&sel("h2")).any(|h|text_of(&h).to_lowercase().contains("what you need to know")) && root.select(&sel("ul li")).next().is_some();
 if !has_recommendations&&!has_summary_body{bail!("NACI recognised recommendation or summary section missing; not a verified terminal");}
 let mut dates=vec![];
 for name in ["dcterms.issued","dcterms.modified"] {for m in doc.select(&sel(&format!("meta[name='{name}'][content]"))) {dates.push(common::date(name,m.value().attr("content").unwrap(),base.as_str()));}}
 let mut clean=root.inner_html();
 for e in root.select(&sel("script,style,.pagedetails,.fn-rtn")){clean=clean.replacen(&e.html(),"",1);}
 for h in root.select(&sel("h2")) {if text_of(&h)=="On this page" {clean=clean.replacen(&h.html(),"",1);if let Some(n)=h.next_siblings().filter_map(ElementRef::wrap).next(){if n.value().name()=="ul"{clean=clean.replacen(&n.html(),"",1);}}}}
 let wrapped=format!("<div id='naci-content'>{clean}</div>");
 let hash=crate::common::sha256_hex(serde_json::to_string(&(&title,&wrapped,&dates))?.as_bytes());
 let (mut blocks,mut warnings)=blocks_from_html(&wrapped,Some(base),Some("#naci-content"));
 if summary {for (i,li) in root.select(&sel("li")).enumerate(){if !li.children().filter_map(ElementRef::wrap).any(|e|matches!(e.value().name(),"ul"|"ol")){continue;}
  let text=text_of(&li);if let Some((index,parent))=blocks.iter().enumerate().find(|(_,b)|matches!(b.kind,BlockKind::ListItem)&&b.text.len()>20&&text.contains(&b.text)){
   let block=Block{block_id:format!("naci-nested-summary-{i}"),kind:BlockKind::ListItem,text:text.clone(),heading_path:parent.heading_path.clone(),page:None};blocks.insert(index,block);
  }else {warnings.push(format!("Summary nested list {i}: full source context could not be associated"));}
 }}
 let re=regex::Regex::new(r"^Recommendation\s+(\d+[A-Za-z]?)\.\s").unwrap();let mut inserts=Vec::new();
 for p in root.select(&sel("p")) {
  let text=text_of(&p);let Some(c)=re.captures(&text) else {continue};let label=c[1].to_string();
  let mut pieces=vec![text.clone()];let mut strength=None;
  for s in p.next_siblings().filter_map(ElementRef::wrap) {
   let t=text_of(&s);if t.starts_with("(Strong NACI recommendation)")||t.starts_with("(Discretionary NACI recommendation)"){strength=Some(t.trim_matches(|c|c=='('||c==')').to_string());break;}
   if s.value().name()=="ul"||s.value().name()=="ol" {pieces.push(t);} else {break;}
  }
  if let Some((i,b))=blocks.iter().enumerate().find(|(_,b)|b.text==text) {
   if strength.is_none(){warnings.push(format!("Recommendation {label}: no locally bounded strength; text kept without inferred grade"));}
   inserts.push((i,Block{block_id:format!("naci-recommendation-{label}"),kind:BlockKind::Recommendation{label:Some(label),strength,certainty:None,population:None,grading_system:None,qualifier:None},text:pieces.join("\n"),heading_path:b.heading_path.clone(),page:None}));
  } else {warnings.push(format!("Recommendation {label} could not be associated with source block"));}
 }
 for (i,b) in inserts.into_iter().rev(){blocks.insert(i+1,b);}
 let mut seen=HashSet::new();let mut attachments=vec![];
 for a in root.select(&sel("a[href]")) {if let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default()) {let context=a.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().name()=="p").map(|e|text_of(&e)).unwrap_or_default();let role=if summary&&context.to_lowercase().contains("full statement"){"full_statement"}else if Url::parse(&u)?.path().to_lowercase().ends_with(".pdf"){"linked_pdf"}else{continue};if seen.insert((u.clone(),role.to_string())){attachments.push(json!({"url":u,"label":text_of(&a),"role":role,"source_context":context,"rights":"unknown; independently assessed asset"}));}}}
 warnings.push("Numbered recommendation grouping only covers observed local paragraph/list/strength pattern; scope, superscripts, table spans, figures and all other templates require review. Linked assets are not extracted.".into());
 Ok((title,hash,dates,blocks,warnings,attachments))
}
fn unresolved(src:&SourceEntry,url:String,title:String,e:String)->PlanRow {PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"discovery_error":e})}}
#[async_trait::async_trait]
impl Adapter for PhacNaci {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let page=f.get_ok(src.catalog_url.as_deref().context("NACI catalog missing")?).await?;let base=Url::parse(&page.record.final_url)?;let mut rows=vec![];let mut deferred=vec![];
  for (url,label) in catalog(&page.text(),&base)? {
   let p=match f.get_ok(&url).await {Ok(p)=>p,Err(e)=>{deferred.push(unresolved(src,url,label,format!("{e:#}")));continue;}};
   let final_url=Url::parse(&p.record.final_url)?;if final_url.origin()!=base.origin(){deferred.push(unresolved(src,url,label,"cross-origin redirect".into()));continue;}
   match clinical(&p.text(),&final_url) {Ok((title,hash,_,_,_,_))=>{let key=common::short_hash(&url);let archived=label.to_lowercase().starts_with("archived")||title.to_lowercase().starts_with("archived:");let summary=title.to_lowercase().starts_with("summary of ")||title.to_lowercase().starts_with("archived: summary of ");rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&hash[..12]),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"work_key":key,"clinical_sha256":hash,"archived":archived,"summary":summary,"catalog_fetch":page.record})});},Err(e)=>deferred.push(unresolved(src,url,label,format!("{e:#}")))}
   if limit.is_some_and(|n|rows.len()>=n){return Ok(rows);}
  }let remaining=limit.map(|n|n.saturating_sub(rows.len())).unwrap_or(usize::MAX);rows.extend(deferred.into_iter().take(remaining));Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("NACI discovery unresolved: {e}");}let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;if base.origin()!=Url::parse(&row.url)?.origin(){bail!("NACI redirect crossed origin");}
  let (title,hash,dates,blocks,mut warnings,attachments)=clinical(&page.text(),&base)?;if row.extra["clinical_sha256"].as_str().is_some_and(|s|s!=hash){bail!("NACI content changed since discovery");}
  let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r);}fetches.push(page.record);
  let published=dates.iter().find(|d|d.label=="dcterms.issued").and_then(|d|d.normalized.clone());let updated=dates.iter().find(|d|d.label=="dcterms.modified").and_then(|d|d.normalized.clone());
  warnings.push("Catalog paths and unvisited items are not reconciled; status current is never inferred from age.".into());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:base.to_string(),item_type:Some(if row.extra["summary"]==true {"summary"}else{"guidance"}.into()),status:if row.extra["archived"]==true {"archived"}else{"unknown"}.into(),published,updated,date_observations:dates,rights:Rights{status:"unknown".into(),note:Some("Item and linked asset reuse terms require source-specific review.".into()),..Default::default()},blocks,attachments,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const CATALOG:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_naci/catalog.html"));
 const COVID:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_naci/covid.html"));
 const URL:&str="https://www.canada.ca/en/public-health/services/publications/vaccines-immunization/national-advisory-committee-immunization-statement-guidance-covid-19-vaccines-2025-summer-2026.html";
 #[test] fn source_catalog_and_clinical_statement() {
  let base=Url::parse(URL).unwrap();assert!(catalog(CATALOG,&base).unwrap().iter().any(|(u,_)|u==URL));
  let (_,_,dates,blocks,_,_)=clinical(COVID,&base).unwrap();
  assert!(dates.iter().any(|d|d.label=="dcterms.issued"&&d.normalized.as_deref()==Some("2025-01-21")));
  let recs=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect::<Vec<_>>();assert_eq!(recs.len(),4);
  let r=recs.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="1A")).unwrap();
  assert!(r.text.contains("long-term care")&&r.text.contains("First Nations"));
  assert!(matches!(&r.kind,BlockKind::Recommendation{strength:Some(s),..} if s=="Strong NACI recommendation"));
  assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),2);
  assert!(blocks.iter().any(|b|b.text.contains("Research priorities")));
  assert!(clinical("<main>Sign in</main>",&base).is_err());
 }
 #[tokio::test] async fn content_revision_guard_fails_closed() {
  let url="https://www.canada.ca/en/public-health/services/publications/vaccines-immunization/national-advisory-committee-immunization-statement-guidance-covid-19-vaccines-2025-summer-2026.html";let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,10<<20,"").unwrap();
  f.seed_cache_for_tests(url,COVID.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"phac_naci","access":"public","enabled":true})).unwrap();
  let row=PlanRow{source_id:src.id.clone(),doc_key:"guard".into(),url:url.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({"clinical_sha256":"changed"})};
  assert!(PhacNaci.extract(&src,&f,&row).await.unwrap_err().to_string().contains("changed since discovery"));
 }

 #[test] fn genuine_summary_preserves_nested_scope_and_full_statement_role(){
  const H:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_naci/hepatitis-summary.html"));
  const F:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_naci/covid-2026-summary.html"));
  let url=Url::parse("https://www.canada.ca/en/public-health/services/publications/vaccines-immunization/national-advisory-committee-immunization-summary-recommendations-routine-pediatric-hepatitis-b-vaccination.html").unwrap();
  let (title,_,dates,blocks,_,links)=clinical(H,&url).unwrap();assert!(title.starts_with("Summary of NACI"));assert!(dates.iter().any(|d|d.label=="dcterms.issued"&&d.normalized.as_deref()==Some("2026-11-02")));
  assert!(blocks.iter().any(|b|b.block_id.starts_with("naci-nested-summary-")&&b.text.contains("all infants")&&b.text.contains("first 24 hours")&&b.text.contains("two months")));
  assert!(links.iter().any(|a|a["role"]=="full_statement"&&a["url"].as_str().unwrap_or("").ends_with("HP40-411-2026-eng.pdf")));
  let url=Url::parse("https://www.canada.ca/en/public-health/services/publications/vaccines-immunization/national-advisory-committee-immunization-summary-guidance-covid-19-vaccines-starting-fall-2026.html").unwrap();let (_,_,_,blocks,_,_)=clinical(F,&url).unwrap();assert!(blocks.iter().any(|b|b.text.contains("80 years of age and over")));assert!(blocks.iter().any(|b|b.text.contains("65 to 79 years")));
 }
 #[tokio::test] async fn genuine_catalog_prioritizes_summary_and_statement_before_unresolved_admin(){
  const H:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/phac_naci/hepatitis-summary.html"));
  let catalog_url="https://www.canada.ca/en/public-health/services/immunization/national-advisory-committee-on-immunization-naci.html";let summary_url="https://www.canada.ca/en/public-health/services/publications/vaccines-immunization/national-advisory-committee-immunization-summary-recommendations-routine-pediatric-hepatitis-b-vaccination.html";
  let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,10<<20,"").unwrap();f.seed_cache_for_tests(catalog_url,CATALOG.as_bytes(),"text/html");f.seed_cache_for_tests(summary_url,H.as_bytes(),"text/html");f.seed_cache_for_tests(URL,COVID.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"phac_naci","access":"public","enabled":true,"catalog_url":catalog_url})).unwrap();let rows=PhacNaci.discover(&src,&f,Some(2)).await.unwrap();assert_eq!(rows.len(),2);assert!(rows.iter().any(|r|r.url==summary_url));assert!(rows.iter().any(|r|r.url==URL));assert!(rows.iter().all(|r|r.extra["discovery_error"].is_null()));
  let mut row=rows.into_iter().find(|r|r.url==summary_url).unwrap();let doc=PhacNaci.extract(&src,&f,&row).await.unwrap();assert_eq!(doc.item_type.as_deref(),Some("summary"));assert!(doc.blocks.iter().any(|b|b.text.contains("first 24 hours")&&b.text.contains("all infants")));row.extra["clinical_sha256"]=json!("changed");assert!(PhacNaci.extract(&src,&f,&row).await.unwrap_err().to_string().contains("changed since discovery"));
 }

}
