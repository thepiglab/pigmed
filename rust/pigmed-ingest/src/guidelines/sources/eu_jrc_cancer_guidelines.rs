//! ECIBC / ECICC publisher recommendation accordions, with audience and topic scope.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::{HashSet,VecDeque};
use url::Url;
pub struct EuJrcCancerGuidelines;
fn graph(html:&str,base:&Url)->Result<Vec<String>> {
 let d=Html::parse_document(html);if d.select(&sel("nav.rec-topics")).next().is_none(){bail!("JRC topic navigation missing");}
 let mut seen=HashSet::new();let mut out=vec![];
 for a in d.select(&sel("a[id^='topic-'][href],a[id^='profile-'][href],a[href*='/ecicc/european-colorectal-cancer-guidelines']")) {
  let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};let p=Url::parse(&u)?;
  if p.origin()!=base.origin(){continue;}if !(p.path().ends_with("/ecibc/european-breast-cancer-guidelines")||p.path().ends_with("/ecicc/european-colorectal-cancer-guidelines")){continue;}
  if seen.insert(u.clone()){out.push(u);}
 }Ok(out)
}
fn ids(html:&str)->Result<Vec<String>>{let d=Html::parse_document(html);if d.select(&sel("nav.rec-topics")).next().is_none(){bail!("JRC navigation missing");}
 Ok(d.select(&sel("details.ecl-accordion__item[id^='rec-']")).filter_map(|e|e.value().id().map(String::from)).collect())}
fn content(html:&str,base:&Url,id:&str)->Result<(String,String,Vec<Block>,Vec<String>,Vec<Value>,Value)> {
 let doc=Html::parse_document(html);let detail=doc.select(&sel("details.ecl-accordion__item[id^='rec-']")).find(|d|d.value().id()==Some(id)).context("JRC recommendation ID missing")?;
 let title=detail.select(&sel("summary .rec-accordion__label")).next().map(|e|text_of(&e)).filter(|s|!s.is_empty()).context("JRC recommendation title missing")?;
 let body=detail.select(&sel("div.ecl-accordion__content")).next().context("JRC recommendation content missing")?;
 let presentation=doc.select(&sel("a.recommendations-profile-link--active")).next().map(|e|text_of(&e)).unwrap_or_default();
 let summary=plain_language(&presentation);
 let box_=body.select(&sel(".rec-recommendation-box")).next();
 let source_statement=presentation=="For healthcare professionals"&&box_.is_none()&&body.select(&sel("h3")).any(|h|text_of(&h)=="Aim")&&body.select(&sel("h3")).any(|h|text_of(&h)=="Statement")&&body.select(&sel("h3")).any(|h|text_of(&h)=="Justification");
 if !summary && box_.is_none() && !source_statement{bail!("JRC recommendation box missing for unsupported presentation");}
 if text_of(&body).trim().is_empty(){bail!("empty JRC item");}
 let labels=body.select(&sel(".rec-rating__label")).map(|e|text_of(&e)).collect::<Vec<_>>();
 let strength=labels.iter().find(|x|x.to_lowercase().contains("recommendation")).cloned();let certainty=labels.iter().find(|x|x.to_lowercase().contains("certainty")).cloned();
 let original=detail.html();let hash=crate::common::sha256_hex(format!("{presentation}\n{original}").as_bytes());let (mut blocks,mut warnings)=blocks_from_html(&original,Some(base),Some("details"));
 if let Some(box_)=box_ {let clinical=text_of(&box_);if clinical.is_empty(){bail!("empty JRC recommendation");}blocks.insert(0,Block{block_id:format!("{id}-recommendation"),kind:BlockKind::Recommendation{label:Some(id.into()),strength,certainty,population:None,grading_system:None,qualifier:None},text:clinical,heading_path:vec![title.clone()],page:None});}
 let mut attachments=vec![];for (i,a) in body.select(&sel("a[href],img[src]")).enumerate(){let attr=if a.value().name()=="img"{"src"}else{"href"};let href=a.value().attr(attr).unwrap_or_default();if let Ok(u)=base.join(href){if matches!(u.scheme(),"http"|"https")&&u.username().is_empty()&&u.password().is_none(){attachments.push(json!({"url":u.as_str(),"role":if attr=="src"{"figure"}else if text_of(&a).contains("Evidence-to-Decision Framework"){"evidence_to_decision_framework"}else if text_of(&a).to_lowercase().contains("previous version"){"previous_version"}else{"supporting_link"},"label":text_of(&a),"source_locator":format!("{id}/{i}"),"rights":"unknown; independent asset"}));}}}
 let dates=body.select(&sel("p.inoneline")).map(|e|text_of(&e)).collect::<Vec<_>>();
 warnings.push(if source_statement {"Publisher evidence-monitoring statement, not a graded recommendation; original item HTML retained and linked rights remain independent.".into()}else{"Publisher recommendation view; linked evidence frameworks, previous versions, figures and their rights are independent and unverified. Original item HTML retained for nested conditions and source labels.".into()});
 Ok((title,hash,blocks,warnings,attachments,json!({"source_locator":id,"presentation":presentation,"source_statement":source_statement,"rating_labels":labels,"issued_labels":dates,"original_item_html":original})))
}
/// G2-scale-b: the breast (ECIBC) view labels its lay presentation "Summary in plain language", the
/// colorectal (ECICC) view "Summary information in plain language"; both are the publisher's lay summary.
fn plain_language(presentation:&str)->bool{let l=presentation.to_lowercase();l.starts_with("summary")&&l.contains("plain language")}
/// G2-scale-b: reconcile an empty topic with the page's own statement instead of guessing.
fn empty_topic_reason(html:&str)->String{let d=Html::parse_document(html);let list=d.select(&sel("section.rec-list")).next().map(|e|text_of(&e)).unwrap_or_default();
 if list.to_lowercase().contains("recommendations coming soon"){"source topic/audience view states 'Recommendations coming soon': no published recommendations yet (empty topic reconciled, nothing substituted)".into()}
 else{"no recommendation blocks; empty topic versus incomplete response not reconciled".into()}}
fn unresolved(src:&SourceEntry,url:String,e:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:None,rendition:Some("html".into()),version:None,extra:json!({"discovery_error":e})}}
#[async_trait::async_trait]
impl Adapter for EuJrcCancerGuidelines {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![])}let start=src.catalog_url.as_deref().context("JRC catalog missing")?;let first=f.get_ok(start).await?;let base=Url::parse(&first.record.final_url)?;let mut queue:VecDeque<String>=graph(&first.text(),&base)?.into();queue.push_front(base.to_string());let mut seen=HashSet::new();let mut out=vec![];
  while let Some(url)=queue.pop_front(){let mut p=Url::parse(&url)?;p.set_fragment(None);let url=p.to_string();if !seen.insert(url.clone()){continue;}if seen.len()>1000{bail!("JRC navigation exceeded 1000 routes");}
   let page=match f.get_ok(&url).await{Ok(x)=>x,Err(e)=>{out.push(unresolved(src,url,format!("{e:#}")));if limit.is_some_and(|n|out.len()>=n){return Ok(out)}continue;}};
   let b=Url::parse(&page.record.final_url)?;if b.origin()!=base.origin(){out.push(unresolved(src,url,"cross-origin redirect".into()));continue;}let html=page.text();
   for next in graph(&html,&b)? {if !seen.contains(&next){queue.push_back(next)}}let recs=ids(&html)?;
   if recs.is_empty(){out.push(unresolved(src,url,empty_topic_reason(&html)));if limit.is_some_and(|n|out.len()>=n){return Ok(out)}continue;}
   for id in recs{match content(&html,&b,&id){Ok((title,hash,_,_,_,_))=>{let key=format!("{}-{id}",common::short_hash(&url));out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&hash[..12]),url:url.clone(),title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"work_key":key,"recommendation_id":id,"clinical_sha256":hash,"catalog_fetch":first.record})});},Err(e)=>out.push(unresolved(src,format!("{url}#{id}"),format!("{e:#}")))}if limit.is_some_and(|n|out.len()>=n){return Ok(out)}}
  }if out.is_empty(){bail!("JRC navigation yielded no items");}Ok(out)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("JRC discovery unresolved: {e}");}let id=row.extra["recommendation_id"].as_str().context("recommendation ID missing")?;let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;if base.origin()!=Url::parse(&row.url)?.origin(){bail!("JRC origin changed");}
  let (title,hash,blocks,warnings,attachments,metadata)=content(&page.text(),&base,id)?;if row.extra["clinical_sha256"].as_str().is_some_and(|x|x!=hash){bail!("JRC recommendation revision changed");}
  let dates=metadata["issued_labels"].as_array().into_iter().flatten().filter_map(|d|d.as_str()).map(|d|common::date("Source issued label",d,&row.url)).collect();let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r);}fetches.push(page.record);
  let mut item=Url::parse(&row.url)?;item.set_fragment(Some(id));Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:item.to_string(),item_type:Some((if metadata["presentation"].as_str().is_some_and(plain_language) {"summary"}else if metadata["source_statement"]==true {"evidence_monitoring_statement"}else{"recommendation"}).into()),status:"unknown".into(),date_observations:dates,rights:Rights{status:"unknown".into(),note:Some("Item and independent asset rights not established.".into()),..Default::default()},search_metadata:metadata,blocks,attachments,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const H:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/eu_jrc_cancer_guidelines/english-topic62.html"));
 const START:&str="https://cancer-screening-and-care.jrc.ec.europa.eu/en/ecibc/european-breast-cancer-guidelines";
 #[test] fn genuine_professional_topic_preserves_recommendation_grade_and_scope(){
  let base=Url::parse(START).unwrap();let edges=graph(H,&base).unwrap();assert!(edges.iter().any(|x|x.contains("topic=63")&&x.contains("usertype=60")));assert_eq!(ids(H).unwrap().len(),6);
  let (title,_,blocks,_,assets,m)=content(H,&base,"rec-75").unwrap();assert!(title.contains("organised mammography"));
  assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s.contains("Strong")&&c.contains("Moderate"))&&b.text.contains("asymptomatic women")));
  assert!(m["issued_labels"].to_string().contains("November 2017"));assert!(m["original_item_html"].as_str().unwrap().contains("Justification"));assert!(assets.iter().any(|x|x["role"]=="supporting_link"));
  assert!(content(H,&base,"rec-missing").is_err());assert!(graph("<html>empty</html>",&base).is_err());
 }
 #[tokio::test] async fn genuine_topic_discover_extract_revision_guard(){
  let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,2<<20,"").unwrap();f.seed_cache_for_tests(START,H.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"eu_jrc_cancer_guidelines","access":"public","enabled":true,"catalog_url":START})).unwrap();
  let mut rows=EuJrcCancerGuidelines.discover(&src,&f,Some(1)).await.unwrap();let mut row=rows.pop().unwrap();assert_eq!(row.extra["recommendation_id"],"rec-75");
  let doc=EuJrcCancerGuidelines.extract(&src,&f,&row).await.unwrap();assert!(doc.url.ends_with("#rec-75"));assert!(doc.blocks.iter().any(|b|b.text.contains("asymptomatic women")));
  row.extra["clinical_sha256"]=json!("changed");assert!(EuJrcCancerGuidelines.extract(&src,&f,&row).await.unwrap_err().to_string().contains("revision changed"));
 }
 #[test] fn genuine_english_topic_and_colorectal_views_keep_audience_and_grades(){
  const E1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/eu_jrc_cancer_guidelines/english-topic63-1.html"));
  const E2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/eu_jrc_cancer_guidelines/english-topic63-2.html"));
  const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/eu_jrc_cancer_guidelines/english-colorectal.html"));
  let breast=Url::parse("https://cancer-screening-and-care.jrc.ec.europa.eu/en/ecibc/european-breast-cancer-guidelines?topic=63&usertype=60").unwrap();let h=format!("{E1}{E2}");assert_eq!(ids(&h).unwrap().len(),13);
  assert!(graph(&h,&breast).unwrap().iter().any(|x|x.contains("topic=84")&&x.contains("usertype=59")));
  let (_,_,blocks,_,assets,m)=content(&h,&breast,"rec-76").unwrap();assert_eq!(m["presentation"],"For healthcare professionals");assert!(m["issued_labels"].to_string().contains("June 2016"));assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s.contains("Conditional")&&c.contains("Moderate"))));assert!(assets.iter().any(|a|a["role"]=="evidence_to_decision_framework"));
  let colorectal=Url::parse("https://cancer-screening-and-care.jrc.ec.europa.eu/en/ecicc/european-colorectal-cancer-guidelines?topic=289&usertype=282").unwrap();assert_eq!(ids(C).unwrap().len(),4);let (title,_,blocks,_,assets,m)=content(C,&colorectal,"rec-373").unwrap();assert!(title.contains("alcohol consumption"));assert!(m["issued_labels"].to_string().contains("September 2023"));assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{strength:None,certainty:None,..})));assert!(assets.iter().any(|a|a["label"]=="Supporting checklist"));
 }

 #[test] fn genuine_plain_language_view_is_distinct_from_professional_recommendations(){
  const P:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/eu_jrc_cancer_guidelines/english-plain-topic84.html"));
  let url=Url::parse("https://cancer-screening-and-care.jrc.ec.europa.eu/en/ecibc/european-breast-cancer-guidelines?topic=84&usertype=59").unwrap();assert_eq!(ids(P).unwrap().len(),4);
  let (title,_,blocks,_,_,m)=content(P,&url,"rec-332").unwrap();assert!(title.contains("40-44"));assert_eq!(m["presentation"],"Summary in plain language");assert!(m["rating_labels"].as_array().unwrap().is_empty());
  assert!(blocks.iter().any(|b|b.text.contains("Who are these recommendations for?")));let original=m["original_item_html"].as_str().unwrap();assert!(original.contains("do not have a high risk"));assert!(original.contains("do not have symptoms"));assert!(!blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})));
 }

}

#[cfg(all(test, feature = "guideline-fixtures"))]mod evidence_statement_tests{use super::*;#[test]fn genuine_risk_stratification_is_statement_without_invented_grade(){let raw=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/eu_jrc_cancer_guidelines/pro-topic-302.html"));let b=Url::parse("https://cancer-screening-and-care.jrc.ec.europa.eu/en/ecibc/european-breast-cancer-guidelines?topic=302&usertype=60").unwrap();let(title,_,blocks,_,_,m)=content(raw,&b,"rec-385").unwrap();assert_eq!(title,"2.1 Risk stratification");assert_eq!(m["source_statement"],true);assert!(m["issued_labels"].to_string().contains("October 2026"));assert!(blocks.iter().any(|b|b.text.contains("recommendations for organised population-based screening are unchanged")));assert!(blocks.iter().any(|b|b.text.contains("additional evidence is needed")));assert!(!blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})));assert!(m["rating_labels"].as_array().unwrap().is_empty());}}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod g2b_tests {
 use super::*;
 const PLAIN:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/eu_jrc_cancer_guidelines/colorectal-plain-topic295.html"));
 const SOON:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/eu_jrc_cancer_guidelines/colorectal-plain-topic364-coming-soon.html"));
 const U:&str="https://cancer-screening-and-care.jrc.ec.europa.eu/en/ecicc/european-colorectal-cancer-guidelines?topic=295&usertype=286";
 #[tokio::test]
 async fn colorectal_plain_language_summaries_extract_and_coming_soon_topics_are_reconciled(){
  assert!(plain_language("Summary in plain language")&&plain_language("Summary information in plain language")&&!plain_language("For healthcare professionals"));
  let base=Url::parse(U).unwrap();assert_eq!(ids(PLAIN).unwrap(),vec!["rec-377","rec-378","rec-379","rec-380"]);
  let (title,_,blocks,_,_,m)=content(PLAIN,&base,"rec-377").unwrap();assert_eq!(title,"1.1 Alcohol consumption");assert_eq!(m["presentation"],"Summary information in plain language");
  assert!(blocks.iter().any(|b|b.text.contains("recommends minimising alcohol consumption")));
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(U,PLAIN.as_bytes(),"text/html");
  let row=PlanRow{source_id:"eu_jrc_cancer_guidelines".into(),doc_key:"g2b".into(),url:U.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({"work_key":"g2b-rec-378","recommendation_id":"rec-378"})};
  let src:SourceEntry=serde_json::from_value(json!({"id":"eu_jrc_cancer_guidelines","enabled":true,"access":"public"})).unwrap();
  let d=EuJrcCancerGuidelines.extract(&src,&f,&row).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("summary"));assert!(d.url.ends_with("#rec-378"));assert!(d.blocks.iter().any(|b|b.text.contains("not smoke")));
  assert!(ids(SOON).unwrap().is_empty());assert!(empty_topic_reason(SOON).contains("Recommendations coming soon"));
  assert!(empty_topic_reason("<section class='rec-list'><p>x</p></section>").contains("not reconciled"));
 }
}
