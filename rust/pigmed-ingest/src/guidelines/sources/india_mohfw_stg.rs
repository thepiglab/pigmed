//! MoHFW's source-labelled specialty/program catalog; clinical and operational PDFs stay distinct.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,FetchRecord,GuidelineDoc};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashMap;
use url::Url;
pub struct IndiaMohfwStg;
const CATALOG:&str="#block-clinical-content article.node--type-standard-treatment-guidelines .field--name-field-standard-treatment-content > .field__item";
fn catalog(src:&SourceEntry,raw:&str,base:&Url)->Result<Vec<PlanRow>>{
 let doc=Html::parse_document(raw);let roots=doc.select(&sel(CATALOG)).collect::<Vec<_>>();if roots.len()!=1{bail!("MoHFW expected one source guideline catalog field, found {}",roots.len());}let root=roots[0];
 let mut out:Vec<PlanRow>=vec![];let mut seen=HashMap::<String,usize>::new();let mut group="STGs".to_string();let mut context=String::new();let mut loose=String::new();let mut position=0;let mut broken_mentions=HashMap::<String,Vec<Value>>::new();
 for node in root.descendants(){
  if let Some(text)=node.value().as_text(){
   if !node.ancestors().filter_map(ElementRef::wrap).any(|e|matches!(e.value().name(),"a"|"h1"|"h2"|"h3"|"h4"|"h5"|"h6")){loose.push_str(text);loose.push(' ');}continue;
  }
  let Some(e)=ElementRef::wrap(node)else{continue};
  if matches!(e.value().name(),"h2"|"h3"|"h4"){group=text_of(&e);context.clear();loose.clear();continue;}
  if e.value().name()=="br"{let line=loose.split_whitespace().collect::<Vec<_>>().join(" ");if !line.is_empty(){context=line;}loose.clear();continue;}
  if e.value().name()!="a"{continue;}let Some(href)=e.value().attr("href")else{continue};let Some(url)=common::link(base,href)else{continue};let u=Url::parse(&url)?;
  if !u.path().to_lowercase().ends_with(".pdf"){
   // The actual catalog also emits a named external TB guideline record.
   // Keep its exact route unresolved until that source template is inspected.
   let label=text_of(&e);if !label.is_empty(){let work=common::slug(&label);let asset=common::short_hash(&url);out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-{asset}-unresolved"),url,title:Some(label.clone()),rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"source_role":"clinical_guideline_record","catalog_mentions":[{"group_heading":group,"link_label":label,"source_html":e.html()}],"discovery_error":"Source-emitted external clinical guideline record retained; its download/body template has not yet been validated"})});}continue;
  }position+=1;
  let label=text_of(&e);if label.is_empty(){bail!("MoHFW emitted PDF with no source label");}
  let sublabel=label.to_lowercase().contains("full document")||label.to_lowercase().contains("quick reference");let title=if sublabel&&!context.is_empty(){format!("{context} — {label}")}else{label.clone()};
  let role=if label.to_lowercase().contains("quick reference"){"quick_reference_guide"}else if label.to_lowercase().contains("draft"){"draft_guideline"}else if label.to_lowercase().contains("operational"){"operational_guidance"}else{"source_guideline_pdf"};
  let mention=json!({"position":position,"group_heading":group,"subtopic_text":context,"link_label":label,"source_html":e.html(),"emitted_url":url,"source_role":role});
  if u.path().rsplit('/').next()==Some(".pdf"){
   // Source row20 has a broken numeric-only anchor immediately followed by
   // its actual named Epilepsy PDF before the line break. Retain that broken
   // mention on the real work instead of fabricating a second empty work.
   if label.chars().all(|c|c.is_ascii_digit()||c=='.'||c.is_whitespace()){
    let mut next=e.next_sibling();let mut paired=None;
    while let Some(node)=next{if let Some(a)=ElementRef::wrap(node){if a.value().name()=="br"{break;}if a.value().name()=="a"{if let Some(h)=a.value().attr("href"){if let Some(url)=common::link(base,h){let parsed=Url::parse(&url)?;if parsed.path().to_lowercase().ends_with(".pdf")&&parsed.path().rsplit('/').next()!=Some(".pdf"){paired=Some(url);break;}}}}}next=node.next_sibling();}
    if let Some(url)=paired{broken_mentions.entry(url).or_default().push(mention);continue;}
   }
   let work=format!("broken-catalog-link-{}",common::short_hash(&format!("{group}:{context}:{title}:{position}")));out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:base.to_string(),title:Some(title),rendition:None,version:None,extra:json!({"work_key":work,"catalog_mentions":[mention],"discovery_error":"Source emits an empty .pdf filename; no clinical target is guessed or replaced"})});continue;
  }
  let paired=broken_mentions.remove(&url).unwrap_or_default();
  if let Some(i)=seen.get(&url){if !paired.is_empty(){out[*i].extra["broken_source_aliases"]=json!(paired);}out[*i].extra["catalog_mentions"].as_array_mut().unwrap().push(mention);continue;}
  // Full and quick-reference links under the same printed topic are
  // distinct source renditions of that topic. Catalog enumeration is not ID.
  let ordinal=regex::Regex::new(r"(?i)^\s*\(?[0-9ivxlcdm]+\)?[.)]\s*").unwrap();
  let subject=if sublabel&&!context.is_empty(){context.as_str()}else{title.as_str()};
  let work=common::slug(&format!("{}-{}",ordinal.replace(&group,""),ordinal.replace(subject,"")));let asset=common::short_hash(&url);seen.insert(url.clone(),out.len());out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-{asset}-unresolved"),url,title:Some(title),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":work,"source_role":role,"catalog_mentions":[mention],"broken_source_aliases":paired})});
 }
 if out.is_empty(){bail!("MoHFW guideline field contains no source-labelled PDF entries");}
 let terms=doc.select(&sel("a[href]")).find(|a|text_of(a).trim()=="Copyright Policy").and_then(|e|common::link(base,e.value().attr("href")?));
 for row in &mut out{row.extra["inventory_scope"]=json!({"source_url":base.as_str(),"pagination":"single source-rendered specialty/program field; no unseen pagination inferred","copyright_policy_url":terms});}
 Ok(out)
}
fn pdf_metadata(blocks:&[Block],url:&str)->Value{
 let front=blocks.iter().take(10).map(|b|b.text.as_str()).collect::<Vec<_>>().join("\n");
 let operational=front.contains("OPERATIONAL GUIDELINES FOR")&&front.contains("CLINICAL ESTABLISHMENTS ACT");
 let parts=blocks.iter().filter(|b|b.text.starts_with("Part ")&&b.text.len()<40).map(|b|json!({"printed":b.text,"source_block_id":b.block_id})).collect::<Vec<_>>();
 let labels=regex::Regex::new(r"(?i)\b(?:class\s+[IVX]+[a-c]?|level\s+(?:of\s+evidence\s*)?[A-C]|grade\s+[A-D])\b").unwrap();
 let evidence=blocks.iter().flat_map(|b|labels.find_iter(&b.text).map(move|m|json!({"printed":m.as_str(),"source_block_id":b.block_id,"source_url":url,"source_block_text":b.text,"scope":"verbatim label mention; association to an individual recommendation not inferred"}))).collect::<Vec<_>>();
 json!({"operational_document":operational,"printed_parts":parts,"printed_evidence_class_mentions":evidence,"date_scope":"PDF creation/upload dates and cited literature years are not clinical publication dates"})
}
fn copyright_policy(raw:&str,url:&Url)->Result<Value>{
 let doc=Html::parse_document(raw);let body=doc.select(&sel("#block-clinical-content .view-copyright-policy .views-field-field-write-content > .field-content")).next().context("MoHFW copyright policy content missing")?;let text=text_of(&body);
 if !text.contains("after taking proper permission")||!text.contains("copyright of a third party"){bail!("MoHFW copyright policy notice mismatch");}
 Ok(json!({"source_url":url.as_str(),"text":text,"scope":"portal material conditional permission; separately credited third-party material excluded"}))
}
#[async_trait::async_trait]
impl Adapter for IndiaMohfwStg{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![]);}let response=f.get_ok(src.catalog_url.as_deref().context("MoHFW catalog missing")?).await?;let base=Url::parse(&response.record.final_url)?;let mut rows=catalog(src,&response.text(),&base)?;if let Some(n)=limit{rows.truncate(n);}
  for row in &mut rows{row.extra["catalog_fetch"]=json!(response.record);if row.extra["discovery_error"].is_string(){continue;}
   let result=async{let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("MoHFW source PDF link returned a wrapper instead of PDF bytes");}Ok::<_,anyhow::Error>(pdf.record)}.await;
   match result{Ok(record)=>{row.doc_key=format!("{}-{}-{}",row.extra["work_key"].as_str().unwrap(),row.extra["source_role"].as_str().unwrap(),&record.sha256[..12]);row.extra["pdf_sha256"]=json!(record.sha256);row.extra["pdf_fetch"]=json!(record);},Err(error)=>row.extra["discovery_error"]=json!(format!("{error:#}"))}
  }Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(error)=row.extra["discovery_error"].as_str(){bail!("MoHFW discovery: {error}");}let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("MoHFW source resource is not actual PDF bytes");}if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=pdf.record.sha256){bail!("MoHFW PDF changed after planning; re-plan");}
  let (mut blocks,mut warnings,_)=blocks_for(&pdf,None,30).await?;if blocks.is_empty(){bail!("MoHFW PDF has no extracted content; scanned/visual body remains unresolved");}for block in &mut blocks{block.page=None;}
  let metadata=pdf_metadata(&blocks,&row.url);let operational=metadata["operational_document"]==true;let text=blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join("\n");
  let source_role=row.extra["source_role"].as_str().unwrap_or("unresolved_pdf");let item_type=if operational||source_role=="operational_guidance"{"operational_guidance"}else if source_role=="quick_reference_guide"{"quick_reference_guide"}else{"clinical_guideline"};
  let mut rights=common::unknown_rights();rights.evidence_urls.push(row.url.clone());let notices=blocks.iter().filter(|b|b.text.contains("All rights reserved")||b.text.contains("Copyright")||b.text.contains("©")).map(|b|json!({"source_block_id":b.block_id,"text":b.text})).collect::<Vec<_>>();
  for notice in &notices{if notice["text"].as_str().is_some_and(|t|t.contains("All rights reserved")){rights.status="restricted".into();rights.restrictions.push(notice["text"].as_str().unwrap().to_string());}}
  let mut fetches=vec![];if let Ok(record)=serde_json::from_value::<FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(record);}fetches.push(pdf.record.clone());
  let mut policy=Value::Null;
  if let Some(url)=row.extra["inventory_scope"]["copyright_policy_url"].as_str(){match f.get_ok(url).await{
   Ok(response)=>{let url=Url::parse(&response.record.final_url)?;match copyright_policy(&response.text(),&url){Ok(value)=>{rights.status="restricted".into();rights.terms_url=Some(url.to_string());rights.evidence_urls.push(url.to_string());rights.restrictions.push(value["text"].as_str().unwrap().to_string());policy=value;},Err(error)=>warnings.push(format!("MoHFW linked copyright policy mapping unresolved: {error:#}"))}fetches.push(response.record);},Err(error)=>warnings.push(format!("MoHFW linked copyright policy not acquired: {error:#}"))
  }}
  warnings.push("Clinical specialty/full guides, quick-reference guides, drafts and operational Clinical Establishments Act documents retain separate source roles. A template is not substituted for a clinical guideline. Empty .pdf catalog placeholders remain unresolved; no replacement link, publication date or current status is inferred.".into());
  warnings.push("PDF text/tables retain original converted order and label mentions, but physical page mapping, graphical algorithms, merged-cell fidelity and original figure assets are unverified. Source PDF remains the authoritative visual. No formal recommendation grade is assigned from an isolated Class/Level/Grade mention.".into());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().context("MoHFW source title missing")?,url:row.url.clone(),item_type:Some(item_type.into()),status:if source_role=="draft_guideline"{"draft"}else{"unknown"}.into(),publisher:(text.contains("Ministry of Health & Family Welfare")||text.contains("MINISTRY OF HEALTH AND FAMILY WELFARE")).then(||"Ministry of Health & Family Welfare".into()),rights,blocks,attachments:vec![json!({"url":row.url,"role":"source_pdf","sha256":pdf.record.sha256,"clinical_role":item_type})],search_metadata:json!({"catalog":row.extra,"source_pdf_metadata":metadata,"portal_copyright_policy":policy,"source_rights_notices":notices,"coverage":{"converted_blocks_available":true,"physical_pages":"unmapped","original_figures":"not extracted","complete_visual_semantics":false}}),fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests{
 use super::*;
 const HTML:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_mohfw_stg/catalog.html"));
 fn source()->SourceEntry{serde_json::from_value(json!({"id":"india_mohfw_stg","enabled":true,"access":"public","catalog_url":"https://www.clinicalestablishments.mohfw.gov.in/en/standard-treatment-guidelines"})).unwrap()}
 #[tokio::test]
 async fn genuine_catalog_full_quick_draft_and_broken_links_keep_their_roles(){
  assert_eq!(crate::common::sha256_hex(HTML.as_bytes()),"ac2e217544437c96824b2cb61f79daa3927bb81ed5e1e16ec51c73058c6dc9bf");let base=Url::parse("https://www.clinicalestablishments.mohfw.gov.in/en/standard-treatment-guidelines").unwrap();let rows=catalog(&source(),HTML,&base).unwrap();assert_eq!(rows.len(),70);assert_eq!(rows.iter().filter(|r|r.extra["discovery_error"].is_string()).count(),7);assert!(rows.iter().any(|r|r.url=="https://tbcindia.gov.in/index1.php?lang=1&level=2&sublinkid=4573&lid=3177"&&r.extra["source_role"]=="clinical_guideline_record"));
  let full=rows.iter().find(|r|r.url.ends_with("/9451.pdf")).unwrap();let quick=rows.iter().find(|r|r.url.ends_with("/7241.pdf")).unwrap();assert!(full.title.as_deref().unwrap().contains("Major Trauma"));assert!(quick.title.as_deref().unwrap().contains("Major Trauma"));assert_eq!(quick.extra["source_role"],"quick_reference_guide");assert_eq!(full.extra["work_key"],quick.extra["work_key"]);assert_ne!(full.doc_key,quick.doc_key);assert_ne!(full.url,quick.url);
  let epilepsy=rows.iter().find(|r|r.url.ends_with("/epilepsy-guidelines.pdf")).unwrap();assert!(epilepsy.title.as_deref().unwrap().contains("Management of Epilepsy in India"));assert_eq!(epilepsy.extra["broken_source_aliases"].as_array().unwrap().len(),1);assert!(!rows.iter().any(|r|r.title.as_deref()==Some("20.")));
  assert!(rows.iter().any(|r|r.url.ends_with("/2501.pdf")&&r.extra["source_role"]=="draft_guideline"));assert!(!rows.iter().any(|r|r.url.ends_with("/2591.pdf")));
  assert!(rows.iter().filter(|r|r.extra["discovery_error"].is_string()).any(|r|r.title.as_deref().unwrap().contains("Dengue")));
  assert!(catalog(&source(),&HTML.replace("field--name-field-standard-treatment-content","missing-source-catalog"),&base).is_err());
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,1<<20,"").unwrap();f.seed_cache_for_tests(base.as_str(),HTML.as_bytes(),"text/html");let actual=IndiaMohfwStg.discover(&source(),&f,Some(1)).await.unwrap();assert_eq!(actual.len(),1);assert!(actual[0].url.ends_with("/3811.pdf"));assert!(actual[0].extra["discovery_error"].as_str().unwrap().contains("offline and not cached"));
 }
 #[test]
 fn genuine_clinical_part_and_operational_template_are_different_source_works(){
  let clinical:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_mohfw_stg/cardiovascular-blocks.json"))).unwrap();let blocks:Vec<Block>=serde_json::from_value(clinical["blocks"].clone()).unwrap();assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),5);let metadata=pdf_metadata(&blocks,clinical["source_pdf_url"].as_str().unwrap());assert_eq!(metadata["operational_document"],false);assert!(metadata["printed_parts"].as_array().unwrap().iter().any(|p|p["printed"]=="Part 1"));assert!(blocks.iter().any(|b|b.text.contains("GUIDELINES FOR THE MANAGEMENT OF CARDIOVASCULAR DISEASES IN INDIA")));
  let operational:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_mohfw_stg/operational-blocks.json"))).unwrap();let blocks:Vec<Block>=serde_json::from_value(operational["blocks"].clone()).unwrap();let metadata=pdf_metadata(&blocks,operational["source_pdf_url"].as_str().unwrap());assert_eq!(metadata["operational_document"],true);assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),110);assert!(blocks.iter().any(|b|b.text.contains("CLINICAL ESTABLISHMENTS ACT")));assert!(blocks.iter().all(|b|b.page.is_none()));
 }

 #[test]
 fn genuine_portal_policy_is_conditional_permission_with_third_party_exception(){
  const RIGHTS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_mohfw_stg/copyright.html"));let value=copyright_policy(RIGHTS,&Url::parse("https://www.clinicalestablishments.mohfw.gov.in/en/copyright-policy").unwrap()).unwrap();let text=value["text"].as_str().unwrap();assert!(text.contains("after taking proper permission")&&text.contains("source must be prominently acknowledged")&&text.contains("third party"));assert!(copyright_policy(&RIGHTS.replace("view-copyright-policy","missing-policy-root"),&Url::parse("https://www.clinicalestablishments.mohfw.gov.in/en/copyright-policy").unwrap()).is_err());
 }

 #[tokio::test]
 async fn genuine_clinical_nste_page_extracts_body_not_operational_template(){
  const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_mohfw_stg/clinical-3811-page-14.pdf"));
  const RIGHTS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_mohfw_stg/copyright.html"));
  assert_eq!(PDF.len(),461630);assert_eq!(crate::common::sha256_hex(PDF),"df8e9644fc0876f39a63062db0caade0c49063ad09afb38bae5d3cd675843dd8");
  let base=Url::parse("https://www.clinicalestablishments.mohfw.gov.in/en/standard-treatment-guidelines").unwrap();let mut rows=catalog(&source(),HTML,&base).unwrap();let mut row=rows.remove(0);assert!(row.url.ends_with("/3811.pdf"));row.extra["fixture_scope"]=json!({"physical_page":14,"source_pages":40,"source_sha256":"92ed8b17fbe6725d473b4d1e3d64650bc5998cb9636d74cb79a3597a5fa68cad","scope":"one genuine NSTE clinical page; not complete40-page guideline"});
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(&row.url,PDF,"application/pdf");f.seed_cache_for_tests("https://www.clinicalestablishments.mohfw.gov.in/en/copyright-policy",RIGHTS.as_bytes(),"text/html");let d=IndiaMohfwStg.extract(&source(),&f,&row).await.unwrap();
  let gold:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_mohfw_stg/clinical-page14-gold.json"))).unwrap();
  let expected=gold["blocks"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap();
  let diagnosis=d.blocks.iter().find(|b|b.text.starts_with("All patients presenting to a health care provider"));
  if diagnosis.map(|b|b.text.trim())!=Some(expected.trim()){
   // Bounded to the one genuine physical-page fixture, never a live/full PDF.
   let dir=tempfile::tempdir().unwrap();let path=dir.path().join("doc.pdf");std::fs::write(&path,PDF).unwrap();
   let converted=tokio::task::spawn_blocking(move||{let converted=crate::convert::convert(&path,".pdf",crate::convert::Opts{ocr_pages:30});drop(dir);converted}).await.unwrap().unwrap();
   eprintln!("MOHFW_P14_CONVERTER tool={} pages={:?}\nMOHFW_P14_RAW_MARKDOWN_BEGIN\n{}\nMOHFW_P14_RAW_MARKDOWN_END",converted.tool,converted.pages,converted.md);
   for block in &d.blocks{eprintln!("MOHFW_P14_BLOCK {} {:?}\n{}",block.block_id,block.kind,block.text);}
  }
  let diagnosis=diagnosis.expect("source diagnosis paragraph remains one contextual block");assert_eq!(diagnosis.text.trim(),expected.trim());
  assert!(diagnosis.text.contains("symptoms suggestive of ACS should be considered as high priority."));assert!(diagnosis.text.contains("Every health care centre should have a functioning ECG machine available 24 hours a day."));assert!(diagnosis.text.contains("trained to interpret the ECG so that treatment can be initiated without delay."));assert!(diagnosis.text.trim_end().ends_with("a networking between the centers can be of great value."));
  assert!(!gold["pdfium_page_text"].as_str().unwrap().contains("prevent referral"));
  assert_eq!(d.item_type.as_deref(),Some("clinical_guideline"));assert_eq!(d.search_metadata["source_pdf_metadata"]["operational_document"],false);assert!(d.blocks.iter().any(|b|b.text.contains("INTRODUCTION AND SCOPE OF THE DOCUMENT")));assert!(d.blocks.iter().any(|b|b.text.contains("DIAGNOSIS OF NSTE ACS")));assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Figure{..}|BlockKind::Recommendation{..})&&b.page.is_none()));assert!(d.published.is_none()&&d.updated.is_none()&&d.version.is_none()&&d.country.is_none());assert_eq!(d.rights.status,"restricted");assert!(d.rights.restrictions.iter().any(|r|r.contains("after taking proper permission")&&r.contains("third party")));assert_eq!(d.search_metadata["coverage"]["complete_visual_semantics"],false);
  row.extra["pdf_sha256"]=json!("changed");assert!(IndiaMohfwStg.extract(&source(),&f,&row).await.is_err());
 }
}
