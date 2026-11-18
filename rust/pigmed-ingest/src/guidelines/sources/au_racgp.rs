//! RACGP public guideline graph and Red Book clinical accordions.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::json;
use std::collections::{HashSet,VecDeque};
use url::Url;
pub struct AuRacgp;
const PREFIX:&str="/clinical-resources/clinical-guidelines/key-racgp-guidelines/view-all-racgp-guidelines";
fn graph(html:&str,base:&Url)->Result<Vec<(String,String)>> {
 let doc=Html::parse_document(html);let root=doc.select(&sel("section.main-content")).next().context("RACGP main-content missing")?;let mut out=vec![];let mut seen=HashSet::new();
 for a in root.select(&sel("a[href]")).chain(doc.select(&sel("a.list-link[href]"))) {let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};let u=Url::parse(&url)?;
  if u.origin()!=base.origin()||!u.path().starts_with(PREFIX)||u.query().is_some(){continue;}let label=text_of(&a);
  if !label.is_empty()&&seen.insert(url.clone()){out.push((url,label));}
 } Ok(out)
}
fn content(html:&str,base:&Url)->Result<(String,String,Vec<DateObservation>,Vec<Block>,Vec<String>)> {
 let doc=Html::parse_document(html);let main=doc.select(&sel("section.main-content")).next().context("RACGP main-content missing")?;
 if main.select(&sel("input[type=password],form[action*='login'],iframe[src*='captcha']")).next().is_some(){bail!("RACGP clinical access gate");}
 let accordion=main.select(&sel("#accordion-guidelines-detail")).next().context("RACGP recognised clinical accordion missing")?;
 let root=accordion.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().classes().any(|c|c=="container-racgp") && e.select(&sel("h1")).next().is_some()).context("RACGP clinical container/title missing")?;
 let title=root.select(&sel("h1")).next().map(|e|text_of(&e)).filter(|s|!s.is_empty()).context("RACGP chapter title missing")?;
 let recommendation=root.select(&sel("#accordion-Recommendation-Content1")).next().context("RACGP recommendations content missing")?;
 let mut dates=vec![];for e in recommendation.select(&sel(".print-heading")){let t=text_of(&e);if let Some(raw)=t.strip_prefix("Recommended as of:"){let raw=raw.trim();let mut d=common::date("Recommended as of",raw,base.as_str());let re=regex::Regex::new(r"^(\d{2})/(\d{2})/(\d{4})$").unwrap();if let Some(c)=re.captures(raw){d.normalized=Some(format!("{}-{}-{}",&c[3],&c[2],&c[1]));}if !dates.iter().any(|x:&DateObservation|x.raw==d.raw){dates.push(d);}}}
 let raw=root.html();let hash=crate::common::sha256_hex(serde_json::to_string(&(&title,&raw,&dates))?.as_bytes());
 let (mut blocks,mut warnings)=blocks_from_html(&raw,Some(base),Some("div.container-racgp"));
 let source_tables=root.select(&sel("table")).collect::<Vec<_>>();let table_ids=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).map(|b|b.block_id.clone()).collect::<Vec<_>>();
 if source_tables.len()!=table_ids.len(){warnings.push("Source and extracted table counts differ; no table recommendations were inferred.".into());}
 else {for (table,id) in source_tables.iter().zip(table_ids.iter()) {
  if !table.ancestors().filter_map(ElementRef::wrap).any(|e|e.value().id()==Some("accordion-Recommendation-Content1")){continue;}
  let row_selector=sel("tr");let mut rows=table.select(&row_selector);let Some(header)=rows.next() else {continue};let fields=common::cells(header).iter().map(text_of).collect::<Vec<_>>();
  if fields.iter().map(|s|s.to_lowercase()).collect::<Vec<_>>()!=["recommendation","grade","how often","references"] {warnings.push(format!("Unrecognised recommendation table columns in {id}; source table retained"));continue;}
  let mut recs=vec![];for (n,tr) in rows.enumerate(){let cells=common::cells(tr);if cells.len()!=4||cells.iter().any(|c|c.value().attr("rowspan").is_some()||c.value().attr("colspan").is_some()){warnings.push(format!("Table {id} row {} has ambiguous cells/spans; source table retained",n+1));continue;}
   let text=text_of(&cells[0]);if text.is_empty(){continue;}let grade=text_of(&cells[1]);let frequency=text_of(&cells[2]);let refs=text_of(&cells[3]);
   recs.push(Block{block_id:format!("{id}-rec-{}",n+1),kind:BlockKind::Recommendation{label:None,strength:(!grade.is_empty()).then_some(grade),certainty:None,population:None,grading_system:None,qualifier:Some(format!("How often: {frequency}; References: {refs}"))},text,heading_path:vec![],page:None});
  }
  common::insert_table_recommendations(&mut blocks,id,recs)?;
 }}
 warnings.push("Only observed four-column clinical tables become recommendations. Preserve source table and surrounding dates; merged cells, age-bar colours, inline footnotes and other RACGP templates require review.".into());
 Ok((title,hash,dates,blocks,warnings))
}
fn unresolved(src:&SourceEntry,url:String,title:String,e:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"discovery_error":e})}}
#[async_trait::async_trait]
impl Adapter for AuRacgp {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let catalog=f.get_ok(src.catalog_url.as_deref().context("RACGP catalog missing")?).await?;let base=Url::parse(&catalog.record.final_url)?;
  let mut queue:VecDeque<_>=graph(&catalog.text(),&base)?.into();let mut seen=HashSet::new();let mut rows=vec![];let mut deferred=vec![];
  while let Some((url,hint))=queue.pop_front(){if !seen.insert(url.clone()){continue;}if seen.len()>5000{bail!("RACGP traversal exceeded 5000 routes");}
   let page=match f.get_ok(&url).await {Ok(v)=>v,Err(e)=>{deferred.push(unresolved(src,url,hint,format!("{e:#}")));continue;}};
   let b=Url::parse(&page.record.final_url)?;if b.origin()!=base.origin(){deferred.push(unresolved(src,url,hint,"cross-origin redirect".into()));continue;}
   let html=page.text();if b.path().to_lowercase().contains("/login") || crate::guidelines::fetch::looks_like_login_wall(&html) {deferred.push(unresolved(src,url,hint,"RACGP login-required route".into()));continue;}
   match graph(&html,&b) {Ok(nexts)=>for next in nexts {if !seen.contains(&next.0){queue.push_back(next);}},Err(e)=>{deferred.push(unresolved(src,url,hint,format!("{e:#}")));continue;}}
   // This accordion container is present on navigation and methodology pages too.
   // Only its populated source recommendation panel identifies the mapped clinical template.
   let doc=Html::parse_document(&html);let selector=sel("#accordion-Recommendation-Content1");
   let has_recommendations=doc.select(&selector).any(|e|e.select(&sel("table")).next().is_some());
   if !has_recommendations {deferred.push(unresolved(src,url,hint,"navigation, methods or unsupported RACGP page; no mapped recommendation table".into()));continue;}
   match content(&html,&b){Ok((title,hash,_,_,_))=>{let key=common::short_hash(&url);rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&hash[..12]),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"work_key":key,"clinical_sha256":hash,"catalog_fetch":catalog.record})});},Err(e)=>deferred.push(unresolved(src,url,hint,format!("{e:#}")))}
   if limit.is_some_and(|n|rows.len()>=n){return Ok(rows);}
  }
  // Retain access failures and unmapped templates when traversal finishes, without
  // letting navigation or errors displace verified clinical rows in bounded pilots.
  let remaining=limit.map(|n|n.saturating_sub(rows.len())).unwrap_or(usize::MAX);rows.extend(deferred.into_iter().take(remaining));
  if rows.is_empty(){bail!("RACGP graph produced no verified or unresolved pages");}Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("RACGP discovery unresolved: {e}");}let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;if base.origin()!=Url::parse(&row.url)?.origin(){bail!("RACGP redirect crossed origin");}
  let (title,hash,dates,blocks,mut warnings)=content(&page.text(),&base)?;if row.extra["clinical_sha256"].as_str().is_some_and(|h|h!=hash){bail!("RACGP clinical revision changed after planning");}
  warnings.push("Public catalog graph, other guideline templates, linked PDF/assets and item rights not yet reconciled; login-protected paths are not bypassed.".into());
  let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r);}fetches.push(page.record);
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:base.to_string(),item_type:Some("guideline_chapter".into()),status:"unknown".into(),date_observations:dates,rights:Rights{status:"unknown".into(),note:Some("Item and linked asset reuse terms require review.".into()),..Default::default()},blocks,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_racgp/catalog.html"));
 const O1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_racgp/osteoporosis-1.html"));
 const O2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_racgp/osteoporosis-2.html"));
 const H1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_racgp/hearing-1.html"));
 const H2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_racgp/hearing-2.html"));
 const V1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_racgp/view-all-1.html"));
 const V2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_racgp/view-all-2.html"));
 const R1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_racgp/red-book-1.html"));
 const R2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_racgp/red-book-2.html"));
 #[test] fn real_chapters_preserve_table_row_context() {
  let base=Url::parse("https://www.racgp.org.au/clinical-resources/clinical-guidelines").unwrap();assert!(!graph(CAT,&base).unwrap().is_empty());
  let o=format!("{O1}{O2}");let h=format!("{H1}{H2}");
  let (_,_,dates,blocks,_)=content(&o,&base).unwrap();assert!(dates.iter().any(|d|d.normalized.as_deref()==Some("2024-06-28")));
  assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),3);
  let rec=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..})&&b.text.contains("Restratify risk")).unwrap();
  assert!(matches!(&rec.kind,BlockKind::Recommendation{strength:Some(s),qualifier:Some(q),..} if s=="Conditionally recommended"&&q.contains("within 2 years")&&q.contains("9,10")));
  assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})&&b.text.contains("1300 mg")));
  let (_,_,_,b,_)=content(&h,&base).unwrap();assert!(b.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count()>=3);
  assert!(content("<section class='main-content'>Sign in</section>",&base).is_err());
 }
 #[tokio::test] async fn revision_guard_rejects_changed_source() {
  let url="https://www.racgp.org.au/clinical-resources/clinical-guidelines/key-racgp-guidelines/view-all-racgp-guidelines/preventive-activities-in-general-practice/musculoskeletal/osteoporosis";
  let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,2<<20,"").unwrap();let o=format!("{O1}{O2}");f.seed_cache_for_tests(url,o.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"au_racgp","access":"public","enabled":true})).unwrap();
  let row=PlanRow{source_id:src.id.clone(),doc_key:"changed".into(),url:url.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({"clinical_sha256":"changed"})};
  assert!(AuRacgp.extract(&src,&f,&row).await.unwrap_err().to_string().contains("revision changed"));
 }

 #[test] fn genuine_catalog_bridges_and_chapter_graph() {
  let base=Url::parse("https://www.racgp.org.au/clinical-resources/clinical-guidelines").unwrap();
  let view="https://www.racgp.org.au/clinical-resources/clinical-guidelines/key-racgp-guidelines/view-all-racgp-guidelines";
  let red="https://www.racgp.org.au/clinical-resources/clinical-guidelines/key-racgp-guidelines/view-all-racgp-guidelines/preventive-activities-in-general-practice/about-the-red-book";
  let v=format!("{V1}{V2}");let r=format!("{R1}{R2}");
  assert!(graph(CAT,&base).unwrap().iter().any(|(u,_)|u==view));
  assert!(graph(&v,&Url::parse(view).unwrap()).unwrap().iter().any(|(u,_)|u==red));
  let links=graph(&r,&Url::parse(red).unwrap()).unwrap();
  assert!(links.iter().any(|(u,_)|u.ends_with("/musculoskeletal/osteoporosis")));
  assert!(links.iter().any(|(u,_)|u.ends_with("/miscellaneous/hearing")));
 }

 #[tokio::test] async fn navigation_does_not_consume_bounded_clinical_discovery(){
  let catalog="https://www.racgp.org.au/clinical-resources/clinical-guidelines";
  let view="https://www.racgp.org.au/clinical-resources/clinical-guidelines/key-racgp-guidelines/view-all-racgp-guidelines";
  let red="https://www.racgp.org.au/clinical-resources/clinical-guidelines/key-racgp-guidelines/view-all-racgp-guidelines/preventive-activities-in-general-practice/about-the-red-book";
  let osteoporosis="https://www.racgp.org.au/clinical-resources/clinical-guidelines/key-racgp-guidelines/view-all-racgp-guidelines/preventive-activities-in-general-practice/musculoskeletal/osteoporosis";
  let hearing="https://www.racgp.org.au/clinical-resources/clinical-guidelines/key-racgp-guidelines/view-all-racgp-guidelines/preventive-activities-in-general-practice/miscellaneous/hearing";
  let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,2<<20,"").unwrap();
  f.seed_cache_for_tests(catalog,CAT.as_bytes(),"text/html");f.seed_cache_for_tests(view,format!("{V1}{V2}").as_bytes(),"text/html");f.seed_cache_for_tests(red,format!("{R1}{R2}").as_bytes(),"text/html");
  f.seed_cache_for_tests(osteoporosis,format!("{O1}{O2}").as_bytes(),"text/html");f.seed_cache_for_tests(hearing,format!("{H1}{H2}").as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"au_racgp","access":"public","enabled":true,"catalog_url":catalog})).unwrap();let rows=AuRacgp.discover(&src,&f,Some(2)).await.unwrap();
  assert_eq!(rows.len(),2);assert!(rows.iter().any(|r|r.url==osteoporosis));assert!(rows.iter().any(|r|r.url==hearing));assert!(rows.iter().all(|r|r.extra["discovery_error"].is_null()));
  let row=rows.iter().find(|r|r.url==osteoporosis).unwrap();let doc=AuRacgp.extract(&src,&f,row).await.unwrap();assert!(doc.blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{strength:Some(s),qualifier:Some(q),..} if s=="Conditionally recommended"&&q.contains("within 2 years"))));
  let mut changed=row.clone();changed.extra["clinical_sha256"]=json!("changed");assert!(AuRacgp.extract(&src,&f,&changed).await.unwrap_err().to_string().contains("revision changed"));
 }

}
