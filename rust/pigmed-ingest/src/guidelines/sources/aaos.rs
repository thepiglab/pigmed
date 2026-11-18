//! AAOS catalog, publisher-labelled full manuscripts, and bounded PDF summaries.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct Aaos;
const ROOT:&str="https://www.aaos.org/quality/quality-programs/clinical-practice-guidelines/";
fn catalog(raw:&str,base:&Url)->Result<Vec<(String,String,String)>> {
 let doc=Html::parse_document(raw);if !doc.select(&sel("h1")).any(|h|text_of(&h).contains("Clinical Practice Guidelines")){bail!("AAOS catalog title missing");}
 let mut out=vec![];let mut seen=HashSet::new();
 for section in doc.select(&sel(".box--section")) {let category=section.select(&sel("h2.title")).next().map(|h|text_of(&h)).unwrap_or_default();
  for a in section.select(&sel("a.list--pipe-link[href]")){let Some(h)=a.select(&sel("h3")).next() else {continue};let title=text_of(&h);let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};if seen.insert(url.clone()){out.push((url,title,category.clone()));}}
 }if out.is_empty(){bail!("AAOS catalog topic list missing");}Ok(out)
}
fn topic(raw:&str,base:&Url)->Result<(String,Vec<Value>)> {
 let doc=Html::parse_document(raw);let mut full=vec![];let mut assets=vec![];let mut seen=HashSet::new();
 for section in doc.select(&sel(".box--section")) {let heading=section.select(&sel("h2.title")).next().map(|e|text_of(&e)).unwrap_or_default();
  for a in section.select(&sel("a[href]")){let label=text_of(&a);let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};let u=Url::parse(&url)?;if !seen.insert(url.clone()){continue;}
   let l=label.to_lowercase();let is_pdf=u.path().to_lowercase().ends_with(".pdf");let clinical=heading.to_lowercase().contains("clinical practice guideline");
   // Publisher-labelled full-manuscript links may be same-source vanity URLs without a .pdf suffix.
   // Their response still must independently pass the PDF signature and access checks below.
   let full_label=(l.contains("full manuscript")||l.contains("full guideline")) && !l.contains("summary") && !l.contains("appendix");
   let is_full=clinical && full_label && (is_pdf || (u.host_str()==Some("www.aaos.org") && matches!(u.scheme(),"http"|"https")));
   // This one verified publisher card has a contradictory review-overview label; only the actual PDF's
   // independent cover and summary-of-options validation can establish the clinical-document role.
   let dental_candidate=clinical && is_pdf && l=="aaos clinical practice guidelines for dental procedures - overview of the review period";
   let dental_report_candidate=is_pdf && l=="aaos and aahks evidence-based clinical practice guidelines for dental procedures";
   if is_full||dental_candidate{full.push(url.clone())};assets.push(json!({"url":url,"label":label,"source_section":heading,"role":if dental_candidate{"candidate_requires_content_identity"}else if dental_report_candidate{"candidate_review_report_requires_content_identity"}else if is_full{"candidate_full_manuscript"}else if l.contains("appendix"){"evidence_appendix"}else{"supporting_asset"},"rights":"unknown; asset requires independent review"}));
  }
 }if full.len()!=1{bail!("AAOS expected one source-labelled full manuscript PDF, found {}",full.len())}Ok((full.remove(0),assets))
}
fn dental_identity(blocks:&[Block])->bool{let front=blocks.iter().take(20).map(|b|b.text.to_lowercase()).collect::<Vec<_>>().join(" ");let has_options=blocks.iter().take(120).any(|b|b.text.trim().eq_ignore_ascii_case("SUMMARY OF OPTIONS"));front.contains("periprosthetic")&&front.contains("dental procedures")&&front.contains("evidence-based clinical practice guideline")&&!front.contains("review period report")&&has_options}
fn dental_review_identity(blocks:&[Block])->bool{let front=blocks.iter().take(20).map(|b|b.text.to_lowercase()).collect::<Vec<_>>().join(" ");front.contains("review period report")&&front.contains("dental procedures")}
fn summary(blocks:&mut Vec<Block>)->Vec<String> {
 let strength=regex::Regex::new(r"(?i)Strength of (?:Recommendation|Option):\s*(Strong|Moderate|Limited|Consensus)(\s*\([^)]*\))?").unwrap();
 let quality=regex::Regex::new(r"(?i)Quality of Evidence:\s*(High|Moderate|Low|Very Low|Consensus)").unwrap();
 // Some single-page conversions merge an all-caps bold item title into the first statement.
 let inline_subject=regex::Regex::new(r"^\*\*([A-Z][A-Z0-9 /&(),+–-]{1,100})\*\*\s+").unwrap();
 let mut in_summary=false;let mut summary_label=String::new();let mut subject=String::new();let mut parts=vec![];let mut q=None;let mut anchor=None;let mut out=vec![];let mut warnings=vec![];
 for (i,b) in blocks.iter().enumerate(){let plain=b.text.replace('*',"");let t=plain.trim();
  if !in_summary && (t.eq_ignore_ascii_case("SUMMARY OF RECOMMENDATIONS") || t.starts_with("SUMMARY OF RECOMMENDATIONS ") || t.eq_ignore_ascii_case("SUMMARY OF OPTIONS") || t.starts_with("SUMMARY OF OPTIONS ")){in_summary=true;summary_label=if t.starts_with("SUMMARY OF OPTIONS"){"SUMMARY OF OPTIONS"}else{"SUMMARY OF RECOMMENDATIONS"}.into();continue;}
  if matches!(b.kind,BlockKind::Heading{level:1}) {if in_summary {break;}continue;}
  if !in_summary{continue;}
  if let Some(c)=inline_subject.captures(b.text.trim()) {if !parts.is_empty(){warnings.push(format!("AAOS summary item {subject} has no safely paired strength; source retained."));}subject=c[1].trim().into();parts.clear();q=None;anchor=None;let remainder=&b.text.trim()[c.get(0).unwrap().end()..];let statement=remainder.replace('*',"");if !statement.trim().is_empty(){anchor=Some(i);parts.push(statement.trim().to_string());}continue;}
  // A lowercase level-2 fragment can be a sentence continuation in an isolated-page conversion.
  if matches!(b.kind,BlockKind::Heading{level:2}) && !( !parts.is_empty() && t.chars().next().is_some_and(|c|c.is_lowercase())) {if !parts.is_empty(){warnings.push(format!("AAOS summary item {subject} has no safely paired strength; source retained."));}subject=t.into();parts.clear();q=None;anchor=None;continue;}
  // In the distal-radius source, item titles and sentence fragments are level 3.
  // Only all-caps headings may start a new item; mixed-case fragments remain statements.
  if matches!(b.kind,BlockKind::Heading{level:3}) && t.chars().any(|c|c.is_alphabetic()) && !t.chars().any(|c|c.is_lowercase()) && !strength.is_match(t) && !quality.is_match(t) {if !parts.is_empty(){warnings.push(format!("AAOS summary item {subject} has no safely paired strength; source retained."));}subject=t.into();parts.clear();q=None;anchor=None;continue;}
  // Publisher's strength-definition prose is not part of the adjacent clinical statement.
  if t.starts_with("Description:"){continue;}
  if let Some(c)=quality.captures(t){q=Some(c[1].to_string());}
  if let Some(c)=strength.captures(t) {let prefix=t[..c.get(0).unwrap().start()].trim();if !prefix.is_empty() && !quality.replace(prefix,"").trim().is_empty(){warnings.push(format!("AAOS summary item {subject} has statement text sharing its strength block; source retained without partial structure."));parts.clear();q=None;anchor=None;continue;}if subject.is_empty()||parts.is_empty(){warnings.push("AAOS strength lacks bounded statement; source retained.".into());parts.clear();q=None;anchor=None;continue;}
   let v=format!("{}{}",&c[1],c.get(2).map(|m|m.as_str()).unwrap_or(""));let pos=anchor.unwrap_or(i);out.push((pos,Block{block_id:format!("{}-recommendation",blocks[pos].block_id),kind:BlockKind::Recommendation{label:Some(subject.clone()),strength:Some(v),certainty:q.take(),population:None,grading_system:None,qualifier:if summary_label=="SUMMARY OF OPTIONS"{Some("Publisher labels this statement an option".into())}else{None}},text:parts.join(" "),heading_path:vec![summary_label.clone(),subject.clone()],page:b.page}));parts.clear();anchor=None;continue;}
  if quality.is_match(t){continue;}
  if !subject.is_empty() && (matches!(b.kind,BlockKind::Heading{level:2..=6}) || matches!(b.kind,BlockKind::Paragraph|BlockKind::ListItem)) && !t.is_empty(){anchor.get_or_insert(i);parts.push(t.to_string());}
 }
 if !parts.is_empty(){warnings.push(format!("AAOS summary item {subject} has no safely paired strength; source retained."));}
 for (i,b) in out.into_iter().rev(){blocks.insert(i+1,b);}warnings.push("Only locally bounded summary statements with explicit AAOS strength are structured; full recommendation coverage, split words, page headers, tables and figure topology need visual verification.".into());warnings
}
fn pdf_rights(blocks:&[Block],url:&str)->Rights {
 let notice=blocks.iter().take(80).map(|b|b.text.as_str()).find(|t|{let l=t.to_lowercase();l.contains("all rights reserved") && l.contains("prior written permission") && (l.contains("retrieval system") || l.contains("reproduced"))});
 if let Some(n)=notice {Rights{status:"restricted".into(),restrictions:vec![n.into()],evidence_urls:vec![url.into()],note:Some("This restriction is stated in this manuscript; asset-specific terms are independent.".into()),..Default::default()}} else {Rights{status:"unknown".into(),note:Some("PDF-specific rights not established from extracted front matter.".into()),..Default::default()}}
}

#[async_trait::async_trait]
impl Adapter for Aaos {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let cat=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;let entries=catalog(&cat.text(),&Url::parse(&cat.record.final_url)?)?;let mut rows=vec![];
  for (url,title,category) in entries {let work=common::slug(Url::parse(&url)?.path());let edition=title.split_whitespace().next().filter(|v|v.len()==4 && v.chars().all(|c|c.is_ascii_digit())).map(String::from);let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:url.clone(),title:Some(title.clone()),rendition:None,version:edition,extra:json!({"work_key":work,"catalog_fetch":&cat.record,"category":category})};
   let u=Url::parse(&url)?;if u.origin()!=Url::parse(ROOT)?.origin(){row.extra["discovery_error"]=json!("External AAOS-endorsed work requires separate source and rights review");}
   else if u.path().to_lowercase().ends_with(".pdf") {if title.to_lowercase().contains("full manuscript") {row.rendition=Some("pdf".into());}else{row.extra["discovery_error"]=json!("Unclassified publisher PDF; not assumed full guideline");}}
   else {match f.get_ok(&url).await {Ok(p)=>match topic(&p.text(),&Url::parse(&p.record.final_url)?){Ok((pdf,assets))=>{row.url=pdf;row.rendition=Some("pdf".into());row.extra["topic_url"]=json!(url);row.extra["topic_fetch"]=json!(&p.record);row.extra["verify_dental_identity"]=json!(assets.iter().any(|a|a["url"]==row.url && a["role"]=="candidate_requires_content_identity"));row.extra["assets"]=json!(assets);},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}}
   if row.rendition.as_deref()==Some("pdf") {match f.get_ok(&row.url).await {Ok(p) if p.is_pdf()=>{let valid=if row.extra["verify_dental_identity"].as_bool()==Some(true){match blocks_for(&p,None,30).await{Ok((b,_,_))=>dental_identity(&b),Err(_)=>false}}else{true};if valid{row.doc_key=format!("{work}-{}",&p.record.sha256[..12]);row.extra["pdf_sha256"]=json!(p.record.sha256);if row.extra["verify_dental_identity"].as_bool()==Some(true){if let Some(items)=row.extra["assets"].as_array_mut(){for a in items{if a["role"]!="candidate_review_report_requires_content_identity"{continue}let Some(url)=a["url"].as_str().map(String::from)else{continue};match f.get_ok(&url).await{Ok(q) if q.is_pdf()=>match blocks_for(&q,None,30).await{Ok((b,_,_)) if dental_review_identity(&b)=>{a["role"]=json!("review_period_report");a["fetch"]=json!(q.record)},Ok(_)=>a["verification_error"]=json!("Publisher report content did not establish review-period identity"),Err(e)=>a["verification_error"]=json!(format!("Report conversion failed: {e:#}"))},Ok(_)=>a["verification_error"]=json!("Publisher report did not return a PDF"),Err(e)=>a["verification_error"]=json!(format!("{e:#}"))}}}}}else{row.extra["discovery_error"]=json!("AAOS dental PDF failed independent full-guideline content identification; conflicting source labels retained");}},Ok(_)=>row.extra["discovery_error"]=json!("AAOS full manuscript URL did not serve PDF"),Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}}
   rows.push(row);if limit.is_some_and(|n|rows.len()>=n){break;}
  }Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("AAOS discovery: {e}")}let p=f.get_ok(&row.url).await?;if !p.is_pdf(){bail!("AAOS manuscript is not PDF")}
  if row.extra["pdf_sha256"].as_str().is_some_and(|h|h!=p.record.sha256){bail!("AAOS manuscript changed since planning; re-plan")}
  let (mut blocks,mut warnings,_)=blocks_for(&p,None,30).await?;if blocks.is_empty(){bail!("AAOS PDF has no extracted text")};let rights=pdf_rights(&blocks,&row.url);warnings.extend(summary(&mut blocks));
  if row.extra["verify_dental_identity"].as_bool()==Some(true){warnings.push("Publisher dental links have conflicting visible labels; this PDF was selected only after its own cover and summary-of-options content were validated. The separate review-period report remains a supporting asset.".into());}
  warnings.push("Full PDF, eAppendices, figures and independent asset rights require source/page-level validation; linked resources were not acquired. Currentness and supersession are not inferred.".into());
  let month=regex::Regex::new(r"^(?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{1,2},\s+\d{4}$").unwrap();
  let mut dates:Vec<_>=blocks.iter().take(25).enumerate().filter_map(|(i,b)|{let t=b.text.trim();if i>0 && month.is_match(t) && blocks[i-1].text.to_lowercase().contains("adopted by") {Some(common::date("Adopted",t,&row.url))}else{None}}).collect();
  let published=regex::Regex::new(r"(?i)\bPublished\s+((?:(?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{1,2},\s+)?(?:19|20)\d{2})").unwrap();
  if let Some(d)=blocks.iter().take(40).find_map(|b|published.captures(&b.text).map(|c|c[1].to_string())){dates.push(common::date("Published",&d,&row.url));}
  let mut fetches=vec![];for key in ["catalog_fetch","topic_fetch"]{if let Ok(f)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra[key].clone()){fetches.push(f);}}fetches.push(p.record);
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some("guideline".into()),version:row.version.clone(),status:"unknown".into(),rights,date_observations:dates,blocks,attachments:row.extra["assets"].as_array().cloned().unwrap_or_default(),fetches,warnings,search_metadata:json!({"topic_url":row.extra["topic_url"],"catalog_category":row.extra["category"]}),..Default::default()})
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const HIP_CLINICAL:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/hip-clinical-page-6.pdf"));
 const KTOP:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/knee-topic.html"));
 const HTOP:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/hip-topic.html"));
 const KPDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/knee-page-2.pdf"));
 const HPDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/hip-page-2.pdf"));
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/catalog.html"));
 const K:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/knee-blocks.json"));
 const H:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/hip-blocks.json"));
 #[test] fn source_topics_and_explicit_strengths(){
  let rows=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();
  assert!(rows.iter().any(|(u,t,c)|u.contains("osteoarthritis-of-the-knee/") && t.contains("2021") && c.contains("Lower Extremity")));
  assert!(rows.iter().any(|(u,t,_)|u.contains("osteoarthritis-of-the-hip/") && t.contains("2023")));
  let mut k:Vec<Block>=serde_json::from_str(K).unwrap();summary(&mut k);let r=k.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..})).unwrap();
  assert!(r.text.contains("Lateral wedge insoles are not recommended"));assert!(matches!(&r.kind,BlockKind::Recommendation{strength:Some(s),certainty:None,..} if s=="Strong"));
  let mut h:Vec<Block>=serde_json::from_str(H).unwrap();summary(&mut h);let r=h.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..})).unwrap();
  assert!(r.text.contains("tranexamic acid") && r.text.contains("reduce blood loss") && r.text.contains("transfusions"));
  assert!(matches!(&r.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Strong" && c=="High"));
 }
 #[tokio::test] async fn genuine_topics_front_matter_and_revision_offline(){
  let anchors={let d=Html::parse_document(CAT);let mut out=Vec::new();for suffix in ["/osteoarthritis-of-the-knee/","/osteoarthritis-of-the-hip/"] {out.push(d.select(&sel("a.list--pipe-link[href]")).find(|a|a.value().attr("href").is_some_and(|h|h.ends_with(suffix))).unwrap().html());}out};
  let bounded=format!("<h1>AAOS Clinical Practice Guidelines</h1><div class='box--section'><h2 class='title'>Lower Extremity Quality Programs</h2>{}{}</div>",anchors[0],anchors[1]);
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");
  let kt="https://www.aaos.org/quality/quality-programs/osteoarthritis-of-the-knee/";let ht="https://www.aaos.org/quality/quality-programs/osteoarthritis-of-the-hip/";
  let kp="https://www.aaos.org/globalassets/quality-and-practice-resources/osteoarthritis-of-the-knee/oak3cpg.pdf";let hp="https://www.aaos.org/globalassets/quality-and-practice-resources/osteoarthritis-of-the-hip/oah-cpg.pdf";
  for (u,h) in [(kt,KTOP),(ht,HTOP)] {f.seed_cache_for_tests(u,h.as_bytes(),"text/html");}f.seed_cache_for_tests(kp,KPDF,"application/pdf");f.seed_cache_for_tests(hp,HPDF,"application/pdf");
  let src:SourceEntry=serde_json::from_value(json!({"id":"aaos","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Aaos.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),2);
  for (url,version) in [(kp,"2021"),(hp,"2023")] {let row=rows.iter().find(|r|r.url==url).unwrap();assert_eq!(row.version.as_deref(),Some(version));let d=Aaos.extract(&src,&f,row).await.unwrap();assert_eq!(d.rights.status,"restricted");assert!(d.rights.restrictions.iter().any(|r|r.contains("retrieval system") && r.contains("prior written permission")));assert!(d.date_observations.iter().any(|d|d.label=="Published" && d.normalized.as_deref().is_some_and(|x|x.starts_with(version))));assert!(d.warnings.iter().any(|w|w.contains("eAppendices")));}
  let row=rows.iter().find(|r|r.url==kp).unwrap();let mut altered=KPDF.to_vec();altered.extend_from_slice(b"\nchanged snapshot");f.seed_cache_for_tests(kp,&altered,"application/pdf");assert!(Aaos.extract(&src,&f,row).await.is_err());
 }

 #[tokio::test] async fn genuine_hip_page_keeps_two_local_strengths_and_quality(){
  let url="https://www.aaos.org/globalassets/quality-and-practice-resources/osteoarthritis-of-the-hip/oah-cpg.pdf";
  let p=crate::guidelines::fetch::Fetched{body:HIP_CLINICAL.to_vec(),record:crate::guidelines::doc::FetchRecord{url:url.into(),final_url:url.into(),status:200,content_type:Some("application/pdf".into()),sha256:crate::common::sha256_hex(HIP_CLINICAL),..Default::default()}};
  let (mut blocks,_,_)=blocks_for(&p,None,30).await.unwrap();let summary_warnings=summary(&mut blocks);
  let txa=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.text.to_lowercase().contains("tranexamic acid")).unwrap_or_else(||panic!("missing TXA recommendation; warnings={summary_warnings:#?}; source blocks={blocks:#?}"));
  assert!(txa.text.contains("symptomatic osteoarthritis") && txa.text.contains("blood transfusions"));assert!(matches!(&txa.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Strong" && c=="High"));
  let therapy=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.text.contains("unsupervised home exercise")).unwrap();
  assert!(therapy.text.contains("total hip arthroplasty"));assert!(matches!(&therapy.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Moderate (Downgraded)" && c=="High"));
 }

 #[test] fn original_full_pdf_page_groups_both_recommendations(){
  let fixture=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/hip-fullpage-blocks.json"));let mut blocks:Vec<Block>=serde_json::from_str(fixture).unwrap();let warnings=summary(&mut blocks);
  let txa=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..})&&b.text.to_lowercase().contains("tranexamic acid")).unwrap_or_else(||panic!("missing TXA: {warnings:?} {blocks:#?}"));assert!(txa.text.contains("symptomatic osteoarthritis")&&txa.text.contains("blood transfusions"));assert!(matches!(&txa.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Strong"&&c=="High"));
  let therapy=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..})&&b.text.contains("unsupervised home exercise")).unwrap_or_else(||panic!("missing therapy: {warnings:?} {blocks:#?}"));assert!(therapy.text.contains("total hip arthroplasty")&&therapy.text.contains("symptomatic"));assert!(matches!(&therapy.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Moderate (Downgraded)"&&c=="High"));
 }

 #[test] fn missing_strength_never_borrows_next_items_rating(){
  let fixture=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/hip-fullpage-blocks.json"));let mut blocks:Vec<Block>=serde_json::from_str(fixture).unwrap();blocks.retain(|b|b.block_id!="m00087");let warnings=summary(&mut blocks);assert!(warnings.iter().any(|w|w.contains("TRANEXAMIC ACID")&&w.contains("no safely paired strength")));assert!(!blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})&&b.text.to_lowercase().contains("tranexamic acid")));let therapy=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..})&&b.text.contains("unsupervised home exercise")).unwrap();assert!(matches!(&therapy.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Moderate (Downgraded)"&&c=="High"));
 }

 #[test]fn genuine_compartment_full_manuscript_vanity_is_not_a_pdf_guess(){let page=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/compartment-topic.html"));let base=Url::parse("https://www.aaos.org/quality/quality-programs/acute-compartment-syndrome/?tab=all_guidelines").unwrap();let(url,assets)=topic(page,&base).unwrap();assert_eq!(url,"http://www.aaos.org/acscpg2025");assert!(assets.iter().any(|a|a["url"]==url&&a["role"]=="candidate_full_manuscript"));}
 #[tokio::test]async fn unacquired_vanity_is_explicit_not_a_guideline(){let page=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/compartment-topic.html"));let topic_url="https://www.aaos.org/quality/quality-programs/acute-compartment-syndrome/?tab=all_guidelines";let anchor={let d=Html::parse_document(CAT);d.select(&sel("a.list--pipe-link[href]")).find(|a|a.value().attr("href").is_some_and(|h|h.contains("acute-compartment-syndrome/?tab=all_guidelines"))).unwrap().html()};let catalog=format!("<h1>AAOS Clinical Practice Guidelines</h1><div class='box--section'><h2 class='title'>Trauma</h2>{anchor}</div>");let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,catalog.as_bytes(),"text/html");f.seed_cache_for_tests(topic_url,page.as_bytes(),"text/html");let src:SourceEntry=serde_json::from_value(json!({"id":"aaos","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Aaos.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),1);assert_eq!(rows[0].url,"http://www.aaos.org/acscpg2025");assert!(rows[0].extra["discovery_error"].is_string());assert!(Aaos.extract(&src,&f,&rows[0]).await.is_err());}

 #[test]fn genuine_dental_label_conflict_requires_content_and_keeps_five_options(){let page=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/dental-topic.html"));let identity=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/dental-identity-blocks.json"));let review=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/dental-review-blocks.json"));let options=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/dental-options-blocks.json"));let base=Url::parse("https://www.aaos.org/quality/quality-programs/implant-infections-in-patients-undergoing-dental-procedures/").unwrap();let(url,assets)=topic(page,&base).unwrap();assert!(url.ends_with("prevention-of-total-hip-and-knee-arthroplasty-pji-in-patients-undergoing-dental-procedures-cpg.pdf"));assert!(assets.iter().any(|a|a["url"]==url&&a["role"]=="candidate_requires_content_identity"&&a["label"].as_str().unwrap().contains("Overview of the Review Period")));assert!(assets.iter().any(|a|a["url"].as_str().is_some_and(|u|u.ends_with("review-report_dental-cpg_updated_9.13.pdf"))&&a["role"]=="candidate_review_report_requires_content_identity"&&a["label"].as_str().unwrap().contains("Evidence-Based Clinical Practice Guidelines")));let a:Vec<Block>=serde_json::from_str(identity).unwrap();let b:Vec<Block>=serde_json::from_str(review).unwrap();assert!(dental_identity(&a));assert!(!dental_identity(&b));assert!(dental_review_identity(&b));assert!(!dental_review_identity(&a));let mut blocks:Vec<Block>=serde_json::from_str(options).unwrap();summary(&mut blocks);let recs=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect::<Vec<_>>();assert_eq!(recs.len(),5);let first=recs.iter().find(|b|b.text.contains("systemic prophylactic antibiotic")).unwrap();assert!(first.text.contains("hip or knee replacement")&&first.text.contains("may not reduce"));assert!(matches!(&first.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),qualifier:Some(q),..} if s=="Limited"&&c=="Low"&&q.contains("option")));let topical=recs.iter().find(|b|b.text.contains("oral topical antiseptic wash")).unwrap();assert!(matches!(&topical.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Consensus"&&c=="Consensus"));assert!(recs.iter().any(|b|b.text.contains("active dental infection")));}

}

#[cfg(all(test, feature = "guideline-fixtures"))]mod live_distal_radius{use super::*;const GOLD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/aaos/distal-radius-summary.json"));
#[test]fn actual_level_three_source_preserves_conditions_and_local_grades(){let mut blocks:Vec<Block>=serde_json::from_str(GOLD).unwrap();let _=summary(&mut blocks);let recs=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect::<Vec<_>>();assert_eq!(recs.len(),6);let arth=recs.iter().find(|b|b.text.contains("arthroscopic assistance")).unwrap();assert!(arth.text.contains("no arthroscopic assistance")&&arth.text.contains("distal radius fractures"));assert!(matches!(&arth.kind,BlockKind::Recommendation{strength:Some(s),certainty:None,..} if s=="Moderate"));let nongeriatric=recs.iter().find(|b|b.text.contains("non-geriatric patients")).unwrap();assert!(nongeriatric.text.contains("65 years")&&nongeriatric.text.contains("3mm")&&nongeriatric.text.contains("10 degrees")&&nongeriatric.text.contains("2 mm"));assert!(matches!(&nongeriatric.kind,BlockKind::Recommendation{strength:Some(s),..} if s=="Moderate"));let older=recs.iter().find(|b|b.text.contains("geriatric patients")&&!b.text.contains("non-geriatric")).unwrap();assert!(older.text.contains("65 years of age and older")&&older.text.contains("non-operative"));assert!(matches!(&older.kind,BlockKind::Recommendation{strength:Some(s),..} if s=="Strong"));}
}
