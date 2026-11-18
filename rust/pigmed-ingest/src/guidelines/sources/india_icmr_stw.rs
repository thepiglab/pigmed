//! Source-rendered ICMR volume/specialty tables -> individual workflows and overlapping collections.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,FetchRecord,GuidelineDoc};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::{HashMap,HashSet};
use url::Url;
pub struct IndiaIcmrStw;
fn pdf_url(base:&Url,href:&str)->Option<String>{let u=Url::parse(&common::link(base,href)?).ok()?;(u.host_str()==base.host_str()&&u.query().is_none()&&u.path().starts_with("/icmrobject/uploads/STWs/")&&u.path().to_lowercase().ends_with(".pdf")).then(||u.to_string())}
fn catalog(src:&SourceEntry,raw:&str,base:&Url)->Result<Vec<PlanRow>>{
 let doc=Html::parse_document(raw);let mains=doc.select(&sel("main#main-content")).collect::<Vec<_>>();if mains.len()!=1{bail!("ICMR expected one source main, found {}",mains.len());}
 // Each source volume has its own sibling .inner__body-content container.
 // Validate those boundaries, then traverse every volume in source order.
 let roots=mains[0].select(&sel(".inner__body-content > .container")).collect::<Vec<_>>();if roots.is_empty(){bail!("ICMR volume containers missing");}
 let mut volume_labels=HashSet::new();for root in &roots{let headings=root.select(&sel("h2")).filter(|h|text_of(h).starts_with("STWs Volume ")).collect::<Vec<_>>();if headings.len()!=1{bail!("ICMR volume container must have one source volume heading, found {}",headings.len());}if !volume_labels.insert(text_of(&headings[0])){bail!("ICMR duplicate source volume heading");}}
 let mut out:Vec<PlanRow>=vec![];let mut seen=HashMap::<String,usize>::new();let mut volume=String::new();let mut tables=0;let mut empty_source_rows=vec![];
 for root in &roots{volume.clear();for e in root.descendants().filter_map(ElementRef::wrap){
  if e.value().name()=="h2"&&text_of(&e).starts_with("STWs Volume "){
   volume=text_of(&e);let parent=e.parent().and_then(ElementRef::wrap).context("ICMR volume header container missing")?;
   for a in parent.select(&sel("a[href]")){let Some(url)=pdf_url(base,a.value().attr("href").unwrap_or_default())else{continue};
    let mention=json!({"volume":volume,"specialty":null,"title":volume,"role":"volume_compilation","link_label":text_of(&a),"source_header":e.html()});push_row(src,&mut out,&mut seen,url,volume.clone(),"volume_compilation",&volume,None,mention);
   }
  }
  if e.value().name()!="table"{continue;}tables+=1;
  if volume.is_empty(){bail!("ICMR specialty table precedes source volume heading");}
  let card=e.ancestors().filter_map(ElementRef::wrap).find(|p|p.value().classes().any(|c|c=="card")).context("ICMR table specialty card missing")?;
  let specialty=card.select(&sel(".card-header")).next().map(|e|text_of(&e)).context("ICMR specialty label missing")?;
  let headers=e.select(&sel("thead th")).map(|e|text_of(&e)).collect::<Vec<_>>();if headers.iter().map(String::as_str).collect::<Vec<_>>()!=vec!["Sr. No.","Title","Document"]{bail!("ICMR unexpected specialty table columns: {headers:?}");}
  for (index,tr) in e.select(&sel("tbody > tr")).enumerate(){let cells=common::cells(tr);if cells.is_empty()&&tr.children().next().is_none()&&tr.value().attrs().next().is_none(){empty_source_rows.push(json!({"volume":volume,"specialty":specialty,"row_position":index+1,"source_html":tr.html(),"role":"empty source spacer row; no child nodes, text or asset"}));continue;}if cells.len()!=3{bail!("ICMR specialty row has {} cells in volume {volume}, specialty {specialty}: {}",cells.len(),tr.html());}let title=text_of(&cells[1]);if title.is_empty(){bail!("ICMR workflow title empty");}
   let role=if title.to_lowercase().ends_with("(all)"){"specialty_compilation"}else{"workflow_sheet"};let links=cells[2].select(&sel("a[href]")).collect::<Vec<_>>();let mut row_urls=HashSet::new();
   for a in links{let Some(url)=pdf_url(base,a.value().attr("href").unwrap_or_default())else{continue};
    let mention=json!({"volume":volume,"specialty":specialty,"title":title,"role":role,"row_position":index+1,"printed_row_number":text_of(&cells[0]),"displayed_document":text_of(&cells[2]),"link_label":text_of(&a),"link_aria_label":a.value().attr("aria-label"),"source_row":tr.html()});row_urls.insert(url.clone());
    push_row(src,&mut out,&mut seen,url,title.clone(),role,&volume,Some(&specialty),mention);
   }
   if row_urls.is_empty(){bail!("ICMR named workflow has no source-emitted PDF: {title}");}
  }
 }
 }
 if tables==0||out.is_empty(){bail!("ICMR catalog no specialty table/workflow PDFs");}
 let footer=doc.select(&sel("footer .footer__bottom .info")).next().map(|e|text_of(&e));let terms=doc.select(&sel("footer a[href]")).find(|e|text_of(e)=="Disclaimer").and_then(|a|common::link(base,a.value().attr("href")?));
 for row in &mut out{row.extra["inventory_scope"]=json!({"source_url":base.as_str(),"specialty_tables":tables,"volume_containers":roots.len(),"empty_source_rows":empty_source_rows,"pagination":"single saved source-rendered catalog; no query or unseen page inferred","website_copyright_notice":footer,"disclaimer_url":terms});}
 Ok(out)
}
fn push_row(src:&SourceEntry,out:&mut Vec<PlanRow>,seen:&mut HashMap<String,usize>,url:String,title:String,role:&str,volume:&str,specialty:Option<&str>,mention:Value){
 if let Some(i)=seen.get(&url){out[*i].extra["catalog_mentions"].as_array_mut().unwrap().push(mention);return;}
 let work=common::slug(&format!("{volume}-{}-{title}",specialty.unwrap_or("volume")));seen.insert(url.clone(),out.len());
 out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url,title:Some(title),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":work,"source_role":role,"volume":volume,"specialty":specialty,"catalog_mentions":[mention],"collection_overlap":"individual, specialty All and volume compilations remain separate source assets; overlap is not deduplicated by presumed clinical equivalence"})});
}
fn printed_metadata(blocks:&[Block],url:&str)->(Vec<DateObservation>,Value){
 let date=regex::Regex::new(r"(?i)^\s*(January|February|March|April|May|June|July|August|September|October|November|December)\s*/?\s*((?:19|20)\d{2})(?:\s*\||\s*$)").unwrap();
 let fragment=regex::Regex::new(r"(?i)^[\s|*_]*(January|February|March|April|May|June|July|August|September|October|November|December)[\s|*_]*/[\s|*_]*(\d{1,3})\b").unwrap();
 let icd=regex::Regex::new(r"(?i)ICD\s*[- ]?\s*(10|11)\s*[-:]?\s*([A-Z][A-Z0-9.]+(?:[-/][A-Z0-9.]+)?)").unwrap();let mut dates=vec![];let mut date_records=vec![];let mut date_fragments=vec![];let mut codes=vec![];let mut editions=vec![];
 for block in blocks{if fragment.is_match(&block.text){date_fragments.push(json!({"source_block_id":block.block_id,"raw_header":block.text,"normalized":null,"scope":"source conversion has incomplete printed year; not filled from filename, compilation or upload date"}));}let start=block.text.trim_start_matches('|');if let Some(c)=date.captures(start){let normalized_input=format!("{} {}",&c[1],&c[2]);let raw=c.get(0).unwrap().as_str().trim().trim_end_matches('|').trim().to_string();let mut observed=common::date("Printed workflow date",&normalized_input,url);observed.raw=raw.clone();date_records.push(json!({"raw":raw,"source_block_id":block.block_id,"source_url":url}));dates.push(observed);}
  for c in icd.captures_iter(&block.text){codes.push(json!({"system":format!("ICD-{}",&c[1]),"code":&c[2],"printed":c.get(0).unwrap().as_str(),"source_block_id":block.block_id,"source_url":url}));}
  if block.text.starts_with("Suggested Citation:"){editions.push(json!({"text":block.text,"source_block_id":block.block_id,"scope":"collection citation; does not date each included/revised workflow"}));}
 }
 (dates,json!({"unresolved_date_header_fragments":date_fragments,"printed_workflow_dates":date_records,"classification_codes":codes,"collection_citations":editions}))
}
fn portal_disclaimer(raw:&str,url:&Url)->Result<Value>{
 let doc=Html::parse_document(raw);let body=doc.select(&sel("main#main-content .inner__body-content .content-area")).next().context("ICMR portal disclaimer body missing")?;let text=text_of(&body);
 if !text.contains("This Portal is hosted by National Informatics Centre")||!text.contains("ICMR accepts no responsibility"){bail!("ICMR portal disclaimer content mismatch");}
 Ok(json!({"source_url":url.as_str(),"text":text,"scope":"portal liability/accuracy terms; not a blanket copyright licence for individual PDF workflows"}))
}
#[async_trait::async_trait]
impl Adapter for IndiaIcmrStw{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![]);}let response=f.get_ok(src.catalog_url.as_deref().context("ICMR catalog missing")?).await?;let base=Url::parse(&response.record.final_url)?;let mut rows=catalog(src,&response.text(),&base)?;if let Some(n)=limit{rows.truncate(n);}
  for row in &mut rows{row.extra["catalog_fetch"]=json!(response.record);let result=async{let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("ICMR source-emitted PDF returned a wrapper rather than PDF bytes");}Ok::<_,anyhow::Error>(pdf.record)}.await;
   match result{Ok(record)=>{row.doc_key=format!("{}-{}",row.extra["work_key"].as_str().unwrap(),&record.sha256[..12]);row.extra["pdf_sha256"]=json!(record.sha256);row.extra["pdf_fetch"]=json!(record);},Err(error)=>row.extra["discovery_error"]=json!(format!("{error:#}"))}
  }Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(error)=row.extra["discovery_error"].as_str(){bail!("ICMR discovery: {error}");}let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("ICMR workflow is not actual PDF bytes");}if row.extra["pdf_sha256"].as_str().is_some_and(|sha|sha!=pdf.record.sha256){bail!("ICMR workflow bytes changed after planning; re-plan");}
  let (mut blocks,mut warnings,_)=blocks_for(&pdf,None,30).await?;if blocks.is_empty(){bail!("ICMR workflow PDF has no extracted text; visual/scanned content remains unresolved");}for block in &mut blocks{block.page=None;}
  let text=blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join("\n");if !text.contains("Standard Treatment Workflow")&&!text.contains("STW"){bail!("ICMR downloaded PDF does not identify standard treatment workflows");}
  let (dates,metadata)=printed_metadata(&blocks,&row.url);let mut rights=common::unknown_rights();rights.evidence_urls.push(row.url.clone());
  let mut notices=vec![];for block in &blocks{if block.text.starts_with("© DHR and ICMR")||block.text.starts_with("All rights reserved."){notices.push(json!({"source_block_id":block.block_id,"text":block.text}));if block.text.starts_with("All rights reserved."){rights.status="restricted".into();rights.restrictions.push(block.text.clone());}}
   if block.text.starts_with("Suggested Citation:"){rights.attribution=Some(block.text.clone());}}
  if text.contains("©")&&text.contains("Indian Council of Medical Research"){rights.rights_holders.push("Indian Council of Medical Research".into());}if text.contains("© DHR and ICMR"){rights.rights_holders.push("Department of Health Research".into());}
  let mut fetches=vec![];if let Ok(record)=serde_json::from_value::<FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(record);}fetches.push(pdf.record.clone());
  let mut disclaimer=Value::Null;
  if let Some(url)=row.extra["inventory_scope"]["disclaimer_url"].as_str(){match f.get_ok(url).await{
   Ok(response)=>{let url=Url::parse(&response.record.final_url)?;match portal_disclaimer(&response.text(),&url){Ok(value)=>disclaimer=value,Err(error)=>warnings.push(format!("ICMR linked portal disclaimer mapping unresolved: {error:#}"))}fetches.push(response.record);},Err(error)=>warnings.push(format!("ICMR linked portal disclaimer not acquired: {error:#}"))
  }}
  warnings.push("Individual sheets, specialty All compilations and full volumes overlap and remain separately identified. Collection cover/citation dates and website footer years are not inherited as individual workflow publication dates; no current/supersession status or formal recommendation grade is inferred.".into());
  warnings.push("These PDFs contain untagged graphical treatment pathways. Text/table conversion does not validate arrow direction, drug-to-dose pairing, parallel risk scores or care-level branch relationships. Graphical algorithm semantics and physical page locators remain unverified; source workflow PDF is the authoritative visual attachment. Original figure URLs are not invented.".into());
  let role=row.extra["source_role"].as_str().unwrap_or("unresolved_pdf");let figures=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count();
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().context("ICMR planned source title missing")?,url:row.url.clone(),item_type:Some(if role=="workflow_sheet"{"treatment_workflow"}else{"workflow_collection"}.into()),status:"unknown".into(),publisher:text.contains("Indian Council of Medical Research").then(||"Indian Council of Medical Research".into()),date_observations:dates,rights,blocks,attachments:vec![json!({"url":row.url,"role":"authoritative_workflow_pdf","sha256":pdf.record.sha256,"rendition":"source PDF; graphical algorithm reading order not validated"})],search_metadata:json!({"catalog":row.extra,"source_pdf_metadata":metadata,"portal_disclaimer":disclaimer,"source_rights_notices":notices,"coverage":{"converted_blocks_available":true,"converted_figure_blocks":figures,"graphical_algorithms":"unverified","physical_pages":"unmapped","complete_clinical_semantics":false}}),fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests{
 use super::*;
 fn html()->String{[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_icmr_stw/catalog.part-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_icmr_stw/catalog.part-2.html"))].concat()}
 fn source()->SourceEntry{serde_json::from_value(json!({"id":"india_icmr_stw","enabled":true,"access":"public","catalog_url":"https://www.icmr.gov.in/standard-treatment-workflows-stws"})).unwrap()}
 #[tokio::test]
 async fn genuine_catalog_all_volumes_tables_and_duplicate_downloads(){
  let raw=html();assert_eq!(crate::common::sha256_hex(raw.as_bytes()),"a07d144b99fea049d284f8781986e20cb28d5c4fd42956be2a5296a4450d9583");let base=Url::parse("https://www.icmr.gov.in/standard-treatment-workflows-stws").unwrap();
  let rows=catalog(&source(),&raw,&base).unwrap();assert_eq!(rows.len(),193);assert_eq!(rows.iter().map(|r|&r.url).collect::<HashSet<_>>().len(),193);assert_eq!(rows.iter().map(|r|r.extra["catalog_mentions"].as_array().unwrap().len()).sum::<usize>(),381);
  assert!(rows.iter().all(|r|r.extra["inventory_scope"]["specialty_tables"]==28&&r.extra["inventory_scope"]["volume_containers"]==4));
  let empty=rows[0].extra["inventory_scope"]["empty_source_rows"].as_array().unwrap();assert_eq!(empty.len(),1);assert_eq!(empty[0]["source_html"],"<tr></tr>");assert_eq!(empty[0]["specialty"],"Cardiology");
  let volumes=rows.iter().flat_map(|r|r.extra["catalog_mentions"].as_array().unwrap()).filter_map(|m|m["volume"].as_str()).collect::<HashSet<_>>();assert_eq!(volumes,HashSet::from(["STWs Volume I","STWs Volume II","STWs Volume III","STWs Volume IV"]));
  let epilepsy=rows.iter().find(|r|r.title.as_deref()==Some("Epilepsy")).unwrap();assert_eq!(epilepsy.extra["source_role"],"workflow_sheet");assert_eq!(epilepsy.extra["volume"],"STWs Volume I");assert_eq!(epilepsy.extra["specialty"],"Neurology");assert_eq!(epilepsy.extra["catalog_mentions"].as_array().unwrap().len(),2);
  assert!(rows.iter().any(|r|r.extra["source_role"]=="specialty_compilation"&&r.title.as_deref()==Some("Neurology (All)")));
  assert!(rows.iter().any(|r|r.extra["source_role"]=="volume_compilation"&&r.extra["volume"]=="STWs Volume IV"));
  assert!(catalog(&source(),&raw.replacen("<tr></tr>","<tr><td>Unresolved source record</td></tr>",1),&base).is_err());
  assert!(!rows.iter().any(|r|r.url.contains("/Events/")||r.url.contains("/Call/")));assert!(catalog(&source(),&raw.replace("id=\"main-content\"","id=\"missing-clinical-catalog\""),&base).is_err());
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,1<<20,"").unwrap();f.seed_cache_for_tests(base.as_str(),raw.as_bytes(),"text/html");
  let actual=IndiaIcmrStw.discover(&source(),&f,Some(2)).await.unwrap();assert_eq!(actual.len(),2);assert!(actual.iter().all(|r|r.extra["discovery_error"].as_str().is_some_and(|s|s.contains("offline and not cached"))));assert!(IndiaIcmrStw.discover(&source(),&f,Some(0)).await.unwrap().is_empty());
 }
 #[test]
 fn genuine_single_workflow_and_mixed_date_collection_metadata_are_scoped(){
  let epilepsy:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_icmr_stw/epilepsy-blocks.json"))).unwrap();let blocks:Vec<Block>=serde_json::from_value(epilepsy["blocks"].clone()).unwrap();assert_eq!(blocks.len(),1);assert!(matches!(blocks[0].kind,BlockKind::Table{..}));
  let (dates,metadata)=printed_metadata(&blocks,epilepsy["source_pdf_url"].as_str().unwrap());assert_eq!(dates.len(),1);assert_eq!(dates[0].raw,"October/ 2019");assert_eq!(dates[0].normalized.as_deref(),Some("2019-10"));assert!(metadata["classification_codes"].as_array().unwrap().iter().any(|c|c["system"]=="ICD-10"&&c["code"]=="G40"));
  assert!(blocks[0].text.contains("REFRACTORY SE 60 MIN")&&blocks[0].text.contains("DOSE (MAINTENANCE: MG/D)"));assert!(blocks[0].text.contains("Folic Acid 5 mg/day"));
  // Genuine source converter emits a mixed-column table, not a validated
  // algorithm graph. No test equates adjacency with drug-dose association.
  assert!(blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..}|BlockKind::Figure{..})&&b.page.is_none()));
  let all:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_icmr_stw/cardiology-blocks.json"))).unwrap();let blocks:Vec<Block>=serde_json::from_value(all["blocks"].clone()).unwrap();assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),17);
  let (_,metadata)=printed_metadata(&blocks,all["source_pdf_url"].as_str().unwrap());assert!(metadata["collection_citations"].as_array().unwrap().iter().any(|c|c["text"].as_str().unwrap().contains("2019 Edition, Vol. 1")));
  assert!(blocks.iter().any(|b|b.text.contains("No part of these workflows may be transmitted or reproduced")));
 }

 #[tokio::test]
 async fn genuine_small_copyright_page_is_not_full_clinical_workflow_coverage(){
  const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_icmr_stw/cardiology-rights-page-2.pdf"));
  const TERMS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_icmr_stw/disclaimer.html"));
  assert!(PDF.len()<200000);assert_eq!(crate::common::sha256_hex(PDF),"027f4b8a597bff83e9829dc029a6d0da825a2ad13760aeba8936e23d24915a40");
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();let url="https://www.icmr.gov.in/icmrobject/uploads/STWs/1793170714_cardiologyall.pdf";f.seed_cache_for_tests(url,PDF,"application/pdf");f.seed_cache_for_tests("https://www.icmr.gov.in/disclaimer",TERMS.as_bytes(),"text/html");
  let row=PlanRow{source_id:"india_icmr_stw".into(),doc_key:"genuine-rights-excerpt".into(),url:url.into(),title:Some("Cardiology (All)".into()),rendition:Some("pdf".into()),version:None,extra:json!({"source_role":"specialty_compilation","inventory_scope":{"disclaimer_url":"https://www.icmr.gov.in/disclaimer"},"fixture_scope":{"physical_page":2,"source_pages":17,"source_sha256":"b8f77f595c64b1786d51ddfa090cf4118bb1b1e670c98b41b3caffff9061e6af"}})};
  let d=IndiaIcmrStw.extract(&source(),&f,&row).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("workflow_collection"));assert_eq!(d.rights.status,"restricted");assert!(d.rights.attribution.as_deref().unwrap().contains("2019 Edition, Vol. 1"));assert!(d.rights.restrictions.iter().any(|s|s.contains("without prior permission")));assert!(d.published.is_none()&&d.updated.is_none()&&d.version.is_none());assert_eq!(d.search_metadata["coverage"]["complete_clinical_semantics"],false);assert_eq!(d.search_metadata["coverage"]["converted_figure_blocks"],0);assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})&&b.page.is_none()));assert!(d.search_metadata["portal_disclaimer"]["text"].as_str().unwrap().contains("ICMR accepts no responsibility"));
  let mut changed=row.clone();changed.extra["pdf_sha256"]=json!("changed");assert!(IndiaIcmrStw.extract(&source(),&f,&changed).await.is_err());
 }

 #[test]
 fn genuine_current_individual_af_has_its_own_incomplete_printed_date(){
  let source:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/india_icmr_stw/af-workflow-blocks.json"))).unwrap();let blocks:Vec<Block>=serde_json::from_value(source["blocks"].clone()).unwrap();let(dates,metadata)=printed_metadata(&blocks,source["source_pdf_url"].as_str().unwrap());assert!(dates.is_empty());assert_eq!(metadata["unresolved_date_header_fragments"].as_array().unwrap().len(),1);assert!(metadata["classification_codes"].as_array().unwrap().iter().any(|c|c["system"]=="ICD-11"&&c["code"]=="BC81.31"));
  assert!(blocks.iter().any(|b|b.text.contains("VKA only if severe M.S or metallic valve")));assert!(blocks.iter().any(|b|b.text.contains("STROKE RISK SCORE")));assert!(blocks.iter().any(|b|b.text.contains("BLEEDING RISK SCORE")));assert!(blocks.iter().any(|b|b.text.contains("VanGelder")));
 }
}
