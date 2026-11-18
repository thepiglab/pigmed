//! IDSA public guideline pages; the site and publisher renditions have distinct rights.
use super::{common,Adapter,PlanRow};
use super::pmc_crosswalk::{self as cw,Lookup,SourceRef};
use super::r2b_crosswalk::{self as xw,Citation,Cross};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct Idsa;

fn article<'a>(doc:&'a Html)->Result<ElementRef<'a>> {
 let root=doc.select(&sel("article.body-container")).next().context("IDSA clinical article root missing")?;
 if root.select(&sel("input[type=password],form[action*='login'],iframe[src*='captcha']")).next().is_some(){bail!("IDSA content requires authentication or a challenge");}
 let t=text_of(&root).to_lowercase();
 if t.contains("sign in to continue reading")||t.contains("subscribe to read") {bail!("IDSA content is gated");}
 // Source's AMR guidance has question headings followed by Suggested approach and rationale.
 // Different templates are unresolved until independently inspected.
 if root.select(&sel("h3")).filter(|e|text_of(e).starts_with("Question ")).count()<3 || !t.contains("suggested approach:") || !t.contains("rationale") {bail!("IDSA landing or unrecognized clinical template; full guidance not established");}
 Ok(root)
}
fn extract_content(html:&str,base:&Url)->Result<(String,String,Vec<DateObservation>,String,Rights,Vec<Block>,Vec<String>,Vec<serde_json::Value>)> {
 let doc=Html::parse_document(html);let root=article(&doc)?;
 let title=doc.select(&sel("#PageTitle")).next().map(|e|text_of(&e)).context("IDSA article title missing")?;
 let status=doc.select(&sel(".status-section .status")).next().map(|e|text_of(&e).to_lowercase()).filter(|s|["current","archived","in development"].contains(&s.as_str())).unwrap_or_else(||"unknown".into());
 let mut dates=Vec::new();
 for p in doc.select(&sel("section p")) {
  if p.select(&sel("strong")).next().is_some_and(|s|text_of(&s)=="Published") {
   if let Some(span)=p.select(&sel("span")).last() {let raw=text_of(&span);if !raw.is_empty(){dates.push(common::date("Published",&raw,base.as_str()));break;}}
  }
 }
 let full_text=text_of(&root);
 let cutoff=regex::Regex::new(r"current as of ([A-Z][a-z]+ \d{1,2}, \d{4})").unwrap();
 if let Some(m)=cutoff.captures(&full_text) {dates.push(common::date("Evidence current as of",&m[1],base.as_str()));}
 let disclaimer=root.select(&sel("p")).map(|p|text_of(&p)).find(|s|s.contains("All rights reserved")&&s.contains("text and data mining"));
 let rights=match disclaimer {Some(text)=>Rights{status:"restricted".into(),rights_holders:vec!["IDSA".into()],restrictions:vec![text],evidence_urls:vec![base.to_string()],note:Some("Rights in linked publisher pages and third-party assets may differ.".into()),..Default::default()},None=>common::unknown_rights()};
 let raw=root.html();let hash=crate::common::sha256_hex(raw.as_bytes());
 // The article's own header contains clinical introduction and author context. The generic
 // walker skips headers, so retain it as a div while preserving everything in document order.
 let rewritten=raw.replace("<header", "<div data-original-header").replace("</header>","</div>");
 let (mut blocks,mut warnings)=blocks_from_html(&rewritten,Some(base),Some("article.body-container"));
 for block in &mut blocks {if matches!(block.kind,BlockKind::Paragraph)&&block.text.to_lowercase().starts_with("suggested approach:") {
  block.kind=BlockKind::Recommendation{label:Some("Suggested approach".into()),strength:None,certainty:None,population:None,grading_system:None,qualifier:None};
 }}
 warnings.push("Image-only dosing tables and externally hosted complete references are source links; their full content has not been verified by this HTML extraction.".into());
 let mut seen=HashSet::new();let mut attachments:Vec<serde_json::Value>=Vec::new();
 for a in root.select(&sel("a[href]")) {
  let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else{continue};let u=Url::parse(&url)?;
  if u.path().to_lowercase().ends_with(".pdf")||u.host_str()==Some("academic.oup.com") {
   let image=a.select(&sel("img[src]")).next().and_then(|i|i.value().attr("src")).and_then(|src|common::link(base,src));
   if !seen.insert(url.clone()) {
    if let Some(image_url)=image {if let Some(existing)=attachments.iter_mut().find(|x|x["url"]==url) {existing["role"]=json!("clinical_table_pdf");existing["image_url"]=json!(image_url);}}
    continue;
   }
   attachments.push(json!({"url":url,"label":text_of(&a),"role":if image.is_some(){"clinical_table_pdf"}else if u.path().to_lowercase().ends_with(".pdf"){"source_linked_pdf"}else{"publisher_page"},"image_url":image,"rights":"unknown; independent asset"}));
  }
 }
 Ok((title,hash,dates,status,rights,blocks,warnings,attachments))
}
fn unresolved(src:&SourceEntry,url:String,title:String,error:String)->PlanRow {
 PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"discovery_error":error})}
}
fn catalog(html:&str,base:&Url)->Result<Vec<(String,String)>> {
 let doc=Html::parse_document(html);let mut out=Vec::new();let mut seen=HashSet::new();
 for a in doc.select(&sel("a.list-pages__link[href]")) {
  let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
  let u=Url::parse(&url)?;
  if u.origin()!=base.origin() || !u.path().starts_with("/practice-guideline/") || u.query().is_some(){continue;}
  let title=text_of(&a);if !title.is_empty() && seen.insert(url.clone()){out.push((url,title));}
 }
 if out.is_empty(){bail!("IDSA A-Z clinical catalog missing or empty");}Ok(out)
}
/// DOI as the IDSA link states it (doi.org/10.… or a …/doi/10.…/… path); OUP volume/page links state none.
fn stated_doi(url:&str)->Option<String>{let u=Url::parse(url).ok()?;let segs:Vec<&str>=u.path_segments()?.filter(|x|!x.is_empty()).collect();if matches!(u.host_str(),Some("doi.org"|"dx.doi.org")){let d=segs.join("/");return d.starts_with("10.").then_some(d)}let i=segs.iter().position(|x|*x=="doi")?;let parts:Vec<&str>=segs[i+1..].iter().take_while(|x|!x.chars().all(|c|c.is_ascii_digit())).copied().collect();(parts.len()>=2&&parts[0].starts_with("10.")).then(||parts.join("/"))}
/// Publisher-article identity for the PMC crosswalk, from the IDSA full-article link only: the DOI it states, an
/// Elsevier PII it states (path `/article/S…` or `?pii=`; DOI via Crossref, unique match), or an OUP
/// `/<journal>/article(-pdf)/<volume>/<issue>/<first page>/…` citation (DOI via Europe PMC, unique match).
fn oup_citation(url:&str)->Option<Citation>{let u=Url::parse(url).ok()?;if u.host_str()!=Some("academic.oup.com"){return None}let segs:Vec<&str>=u.path_segments()?.filter(|x|!x.is_empty()).collect();
 if segs.len()<5||!matches!(segs[1],"article"|"article-pdf")||!segs[2].bytes().all(|b|b.is_ascii_digit()){return None}
 let issn=match segs[0]{"ajrccm"=>"1073-449X","cid"=>"1058-4838","jid"=>"0022-1899","ofid"=>"2328-8957","jpids"=>"2048-7193",_=>return None};
 Some(Citation{issn:issn.into(),volume:segs[2].into(),issue:Some(segs[3].to_string()).filter(|i|i.bytes().all(|b|b.is_ascii_digit())),first_page:segs[4].into()})}
fn query_pii(url:&str)->Option<String>{let u=Url::parse(url).ok()?;let v=u.query_pairs().find(|(k,_)|k=="pii")?.1.to_string();let c:String=v.chars().filter(|c|!matches!(c,'-'|'('|')')).collect();regex::Regex::new(r"^S[0-9]{15}[0-9X]$").unwrap().is_match(&c).then_some(c)}
async fn crosswalk(src:&SourceEntry,f:&Fetcher,cx:&mut xw::Session,target:&str,landing:&str)->(Option<PlanRow>,Cross){
 let fail=|status:&'static str,reason:String|(None,Cross{row:None,relation:None,status,evidence:json!({"status":status,"reason":reason,"url":target}),reason:Some(reason)});
 let mut resolution=serde_json::Value::Null;
 let (lookup,route)=if let Some(d)=stated_doi(target).and_then(|d|cw::normalize_doi(&d)).or_else(||cw::doi_from_url(target)){(Lookup::Doi(d),"source_doi")}
  else if let Some(pii)=cw::pii_from_url(target).or_else(||query_pii(target)){match cw::doi_for_pii(f,&pii).await{Ok(Some(d))=>{resolution=json!({"method":"Elsevier PII stated in the link, DOI via Crossref alternative-id (unique match)","query":cw::crossref_pii_url(&pii)});(Lookup::Doi(d),"source_pii")},Ok(None)=>return fail("citation_unresolved",format!("Crossref has no unique work for PII {pii}")),Err(e)=>return fail("lookup_failed",format!("Crossref PII lookup failed: {e:#}"))}}
  else if let Some(c)=oup_citation(target){match xw::doi_for_citation(f,&c).await{Ok((l,ev))=>{resolution=ev;(l,"source_citation")},Err(e)=>return fail("citation_unresolved",e)}}
  else {return fail("no_identifier","the IDSA full-article link states no DOI, PII or OUP citation".into())};
 let sref=SourceRef{source_url:target.into(),evidence_url:Some(landing.into()),source_title:None,item_type:Some("guideline".into()),key_prefix:"idsa-pmc".into(),id_route:route.into(),notes:vec![]};
 let mut extra=json!({"idsa_landing":landing,"publisher_url":target,"doi":stated_doi(target)});if !resolution.is_null(){extra["citation_resolution"]=resolution;}
 let mut c=cx.try_id(src,f,lookup,sref,extra,None).await;let row=c.row.take();(row,c)}
fn full_article(html:&str,base:&Url)->Option<String> {
 let doc=Html::parse_document(html);
 doc.select(&sel("#GuidelinePDFLink[href]")).next().and_then(|a|common::link(base,a.value().attr("href").unwrap_or_default()))
}
#[async_trait::async_trait]
impl Adapter for Idsa {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}
  let catalog_url="https://www.idsociety.org/practice-guideline/all-practice-guidelines/";
  let page=f.get_ok(catalog_url).await?;let base=Url::parse(&page.record.final_url)?;
  let mut rows=Vec::new();let mut cx=xw::Session::default();
  for (url,title) in catalog(&page.text(),&base)? {
   let p=match f.get_ok(&url).await {Ok(p)=>p,Err(e)=>{rows.push(unresolved(src,url,title,format!("{e:#}")));if limit.is_some_and(|n|rows.len()>=n){break;}continue;}};
   let final_url=Url::parse(&p.record.final_url)?;
   if final_url.origin()!=base.origin(){rows.push(unresolved(src,url,title,"IDSA article redirect crossed origin".into()));if limit.is_some_and(|n|rows.len()>=n){break;}continue;}
   let html=p.text();
   match extract_content(&html,&final_url) {
    Ok((actual,hash,dates,_,_,_,_,assets))=>{
     let mut fingerprints=Vec::new();let mut failure=None;
     for a in &assets {
      if a["role"]!="clinical_table_pdf" {continue;}
      let Some(asset_url)=a["url"].as_str() else {continue};
      match f.get_ok(asset_url).await {
       Ok(pdf) if pdf.is_pdf()=>fingerprints.push(json!({"url":asset_url,"sha256":pdf.record.sha256})),
       Ok(_)=>{failure=Some(format!("Clinical table {asset_url} did not return PDF bytes"));break;},
       Err(e)=>{failure=Some(format!("Clinical table {asset_url} unavailable: {e:#}"));break;}
      }
     }
     if let Some(err)=failure {rows.push(unresolved(src,url,title,err));if limit.is_some_and(|n|rows.len()>=n){break;}continue;}
     let identity=crate::common::sha256_hex(format!("{}{}",hash,serde_json::to_string(&fingerprints)?).as_bytes());
     let key=common::short_hash(&url);let version=dates.iter().find(|d|d.label=="Published").and_then(|d|d.normalized.clone());
     rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&identity[..12]),url,title:Some(actual),rendition:Some("html".into()),version,extra:json!({"work_key":key,"clinical_sha256":hash,"asset_fingerprints":fingerprints,"catalog_fetch":page.record})});
    },
    Err(e)=>{
     // A landing abstract is not a full guideline. Resolve only the publisher's explicitly labelled link.
     // A landing abstract is not a full guideline. The publisher copy (OUP) answered HTTP 403 in a live probe, so
     // it is not re-requested; the stated DOI goes to the shared PMC crosswalk instead.
     let target=full_article(&html,&final_url);
     let reason=match &target {Some(t)=>format!("IDSA page is not full clinical text ({e:#}); publisher full article {t} not fetched (403 recorded)"),None=>format!("IDSA clinical template not mapped: {e:#}")};
     let mut row=unresolved(src,url.clone(),title,reason);
     if let Some(t)=target {row.extra["publisher_url"]=json!(t);row.extra["doi"]=json!(stated_doi(&t));row.extra["oa_route"]=json!("pmc_crosswalk");
      // Round 2: the publisher article's PMC copy (OA or licence-less free copy as restricted); closed records stay
      // unresolved with the PMC relation recorded on the row.
      let (open,c)=crosswalk(src,f,&mut cx,&t,&url).await;
      match open {Some(r)=>{rows.push(r);if limit.is_some_and(|n|rows.len()>=n){break;}continue;},None if c.status=="duplicate"=>{continue;},None=>{let e=row.extra["discovery_error"].as_str().unwrap_or_default().to_string();row.extra["discovery_error"]=json!(format!("{e}; PMC crosswalk {}: {}",c.status,c.reason.clone().unwrap_or_default()));xw::record_on(&mut row,&c);}}}
     rows.push(row);
    }
   }
   if limit.is_some_and(|n|rows.len()>=n){break;}
  }
  Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
  if cw::is_crosswalk_row(row){return xw::extract(src,f,row).await;}
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("IDSA discovery unresolved: {e}");}
  let p=f.get_ok(&row.url).await?;let base=Url::parse(&p.record.final_url)?;
  if base.origin()!=Url::parse(&row.url)?.origin(){bail!("IDSA article redirect crossed origin");}
  let (title,hash,dates,status,rights,mut blocks,mut warnings,mut attachments)=extract_content(&p.text(),&base)?;
  if row.extra["clinical_sha256"].as_str().is_some_and(|h|h!=hash){bail!("IDSA article changed since discovery");}
  let mut fetches=Vec::new();
  if let Ok(record)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(record);}
  fetches.push(p.record);
  for (n,asset) in attachments.iter_mut().enumerate() {
   if asset["role"]!="clinical_table_pdf" {continue;}
   let Some(url)=asset["url"].as_str().map(String::from) else {continue};
   match f.get_ok(&url).await {
    Ok(pdf)=>{
     if let Some(expected)=row.extra["asset_fingerprints"].as_array().and_then(|v|v.iter().find(|x|x["url"]==url)).and_then(|x|x["sha256"].as_str()) {
      if expected!=pdf.record.sha256 {bail!("IDSA clinical table changed since discovery: {url}");}
     }
     if !pdf.is_pdf() {asset["extraction_error"]=json!("Expected PDF signature; asset not parsed");warnings.push(format!("Clinical table {url} was not a PDF; omitted from text"));continue;}
     match super::static_list::blocks_for(&pdf,None,0).await {
      Ok((mut asset_blocks,asset_warnings,_))=>{
       let heading=format!("Linked clinical table PDF {}",n+1);
       blocks.push(Block{block_id:format!("idsa-asset-{n}-heading"),kind:BlockKind::Heading{level:2},text:heading.clone(),heading_path:vec![heading.clone()],page:None});
       for b in &mut asset_blocks {b.block_id=format!("idsa-asset-{n}-{}",b.block_id);b.heading_path.insert(0,heading.clone());}
       blocks.extend(asset_blocks);warnings.extend(asset_warnings.into_iter().map(|w|format!("{url}: {w}")));
       asset["fetch"]=serde_json::to_value(&pdf.record)?;asset["extracted"]=json!(true);fetches.push(pdf.record);
      },
      Err(e)=>{asset["fetch"]=serde_json::to_value(&pdf.record)?;asset["extraction_error"]=json!(format!("{e:#}"));fetches.push(pdf.record);warnings.push(format!("Clinical table {url} extraction failed: {e:#}"));}
     }
    },
    Err(e)=>{asset["extraction_error"]=json!(format!("{e:#}"));warnings.push(format!("Clinical table {url} unavailable: {e:#}"));}
   }
  }
  warnings.push("Linked clinical tables are independent assets with unknown rights; converted text, tables, footnotes and layout require visual comparison. The document identity includes planned clinical table hashes; table rights remain independent.".into());
  let published=dates.iter().find(|d|d.label=="Published").and_then(|d|d.normalized.clone());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),item_type:Some("guidance".into()),title,url:base.to_string(),version:row.version.clone(),status,published,date_observations:dates,rights,blocks,attachments,fetches,warnings,..Default::default()})
 }
}


#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
 use super::*;
 const CATALOG:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/idsa/catalog.html"));
 const TABLE1:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/idsa/table-1.pdf"));
 const TABLE2:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/idsa/table-2.pdf"));
 const FIRST:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/idsa/amr-part-1.html"));
 const LAST:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/idsa/amr-part-2.html"));
 const CAP:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/idsa/cap-landing.html"));
 #[test]
 fn source_complete_amr_and_gated_cap_landing() {
  let source=format!("{FIRST}{LAST}");let url=Url::parse("https://www.idsociety.org/practice-guideline/amr-guidance/").unwrap();
  let (title,_,dates,status,rights,blocks,_,assets)=extract_content(&source,&url).unwrap();
  assert!(title.contains("2026 Guidance"));assert_eq!(status,"current");
  assert!(dates.iter().any(|d|d.label=="Published" && d.normalized.as_deref()==Some("2026-07-30")));
  assert!(dates.iter().any(|d|d.label=="Evidence current as of" && d.normalized.as_deref()==Some("2026-03-01")));
  assert_eq!(rights.status,"restricted");assert!(rights.restrictions.iter().any(|r|r.contains("text and data mining")));
  assert!(blocks.iter().any(|b|b.text.contains("Question 1.2")));
  assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.heading_path.iter().any(|h|h.contains("Question 1.2"))));
  assert!(blocks.iter().any(|b|b.text.contains("Question 6.6")));
  assert!(blocks.iter().any(|b|b.text.contains("The field of AMR is dynamic")));
  assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),2);
  assert_eq!(assets.iter().filter(|a|a["role"]=="clinical_table_pdf").count(),2);
  assert!(assets.iter().any(|a|a["url"].as_str().is_some_and(|u|u.ends_with("/5.0/table-1.pdf"))));
  assert!(extract_content(CAP,&Url::parse("https://www.idsociety.org/practice-guideline/community-acquired-pneumonia-cap-in-adults/").unwrap()).is_err());
 }
 #[test]
 fn real_catalog_and_full_article_boundary() {
  let base=Url::parse("https://www.idsociety.org/practice-guideline/all-practice-guidelines/").unwrap();
  let rows=catalog(CATALOG,&base).unwrap();
  assert!(rows.len()>100);
  assert!(rows.iter().any(|(u,_)|u.ends_with("/amr-guidance/")));
  assert!(rows.iter().any(|(u,_)|u.ends_with("/community-acquired-pneumonia-cap-in-adults/")));
  assert_eq!(full_article(CAP,&base).as_deref(),Some("https://academic.oup.com/ajrccm/article/200/7/e45/8496962"));
  assert!(catalog("<main>Sign in</main>",&base).is_err());
 }
 #[tokio::test]
 async fn offline_amr_discovery_and_separate_table_provenance() {
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,500,10<<20,"").unwrap();
  let catalog_url="https://www.idsociety.org/practice-guideline/all-practice-guidelines/";
  let url="https://www.idsociety.org/practice-guideline/amr-guidance/";
  let table1="https://www.idsociety.org/globalassets/idsa/practice-guidelines/amr-guidance/5.0/table-1.pdf";
  let table2="https://www.idsociety.org/globalassets/idsa/practice-guidelines/amr-guidance/5.0/table-2.pdf";
  // Single real catalog link, bounded for deterministic discovery.
  let cat="<a class='list-pages__link' href='/practice-guideline/amr-guidance/'>AMR Guidance</a>";
  f.seed_cache_for_tests(catalog_url,cat.as_bytes(),"text/html");
  let html=format!("{FIRST}{LAST}");f.seed_cache_for_tests(url,html.as_bytes(),"text/html");
  f.seed_cache_for_tests(table1,TABLE1,"application/pdf");f.seed_cache_for_tests(table2,TABLE2,"application/pdf");
  let src:SourceEntry=serde_json::from_value(json!({"id":"idsa","access":"public","enabled":true,"catalog_url":catalog_url})).unwrap();
  let rows=Idsa.discover(&src,&f,Some(1)).await.unwrap();assert_eq!(rows.len(),1);
  assert_eq!(rows[0].extra["asset_fingerprints"].as_array().unwrap().len(),2);
  let doc=Idsa.extract(&src,&f,&rows[0]).await.unwrap();
  assert_eq!(doc.attachments.iter().filter(|a|a["role"]=="clinical_table_pdf" && a["extracted"]==true).count(),2);
  assert_eq!(doc.fetches.iter().filter(|r|r.url==table1 || r.url==table2).count(),2);
  assert!(doc.attachments.iter().filter(|a|a["role"]=="clinical_table_pdf").all(|a|a["rights"].as_str().unwrap().contains("unknown")));
  assert!(doc.blocks.iter().any(|b|b.block_id.starts_with("idsa-asset-") && matches!(b.kind,BlockKind::Table{..})));
  let mut changed=rows[0].clone();changed.extra["clinical_sha256"]=json!("changed");
  assert!(Idsa.extract(&src,&f,&changed).await.unwrap_err().to_string().contains("changed since discovery"));
 }

 #[tokio::test]
 async fn fix_round1_landing_records_stated_doi_without_publisher_fetch() {
  assert_eq!(stated_doi("https://doi.org/10.1093/jpids/piad089").as_deref(),Some("10.1093/jpids/piad089"));
  assert!(stated_doi("https://academic.oup.com/ajrccm/article/200/7/e45/8496962").is_none());
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,500,1<<20,"").unwrap();
  let catalog_url="https://www.idsociety.org/practice-guideline/all-practice-guidelines/";
  let url="https://www.idsociety.org/practice-guideline/community-acquired-pneumonia-cap-in-adults/";
  f.seed_cache_for_tests(catalog_url,"<a class='list-pages__link' href='/practice-guideline/community-acquired-pneumonia-cap-in-adults/'>CAP</a>".as_bytes(),"text/html");
  f.seed_cache_for_tests(url,CAP.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"idsa","access":"public","enabled":true,"catalog_url":catalog_url})).unwrap();
  let rows=Idsa.discover(&src,&f,Some(1)).await.unwrap();
  assert_eq!(rows[0].extra["publisher_url"],"https://academic.oup.com/ajrccm/article/200/7/e45/8496962");
  assert!(rows[0].extra["doi"].is_null());assert_eq!(rows[0].extra["oa_route"],"pmc_crosswalk");
  assert!(!rows[0].extra["discovery_error"].as_str().unwrap().contains("not cached: https://academic.oup.com"));assert!(rows[0].extra["discovery_error"].as_str().unwrap().contains("PMC crosswalk citation_unresolved"));
 }
 #[tokio::test]
 async fn fix_round2_cap_oup_citation_to_pmc_copy() {
  let fx=|n:&str|std::fs::read(format!("{}/tests/fixtures/guidelines/r2b_crosswalk/{n}",env!("CARGO_MANIFEST_DIR"))).unwrap();
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,500,8<<20,"").unwrap();
  let catalog_url="https://www.idsociety.org/practice-guideline/all-practice-guidelines/";
  let url="https://www.idsociety.org/practice-guideline/community-acquired-pneumonia-cap-in-adults/";
  let oup="https://academic.oup.com/ajrccm/article/200/7/e45/8496962";
  f.seed_cache_for_tests(catalog_url,"<a class='list-pages__link' href='/practice-guideline/community-acquired-pneumonia-cap-in-adults/'>CAP</a>".as_bytes(),"text/html");
  f.seed_cache_for_tests(url,CAP.as_bytes(),"text/html");
  // The IDSA link states journal/volume/issue/first page only: resolved through Europe PMC (unique first-page match).
  let c=oup_citation(oup).unwrap();assert_eq!((c.issn.as_str(),c.volume.as_str(),c.issue.as_deref(),c.first_page.as_str()),("1073-449X","200",Some("7"),"e45"));
  f.seed_cache_for_tests(&cw::epmc_search_url(&xw::citation_query(&c),100,"*"),&fx("epmc-ajrccm-200-7.json"),"application/json");
  f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Doi("10.1164/rccm.201908-1581st".into())),&fx("idconv-idsa-cap.json"),"application/json");
  f.seed_cache_for_tests(&cw::oai_url("PMC6812437").unwrap(),&fx("oai-PMC6812437-trimmed.xml"),"text/xml");
  let src:SourceEntry=serde_json::from_value(json!({"id":"idsa","access":"public","enabled":true,"catalog_url":catalog_url})).unwrap();
  let rows=Idsa.discover(&src,&f,Some(1)).await.unwrap();assert_eq!(rows.len(),1);
  assert!(cw::is_crosswalk_row(&rows[0]));assert!(rows[0].doc_key.starts_with("idsa-pmc-pmc6812437-"));assert_eq!(rows[0].extra["citation_resolution"]["matched"]["doi"],"10.1164/rccm.201908-1581st");assert_eq!(rows[0].extra["idsa_landing"],url);
  let d=Idsa.extract(&src,&f,&rows[0]).await.unwrap();
  // JATS licence is a reprint notice, not CC: kept verbatim as restricted, never upgraded.
  assert_eq!(d.rights.status,"restricted");assert!(d.rights.license.as_deref().unwrap().starts_with("You may print one copy"));
  assert!(d.relations.iter().any(|r|r.rel=="rendition_of"&&r.target=="pmc:PMC6812437"));assert!(d.relations.iter().any(|r|r.target==oup));assert!(d.relations.iter().any(|r|r.target==url));
  assert!(d.warnings.iter().any(|w|w.contains("first page")));assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})));
  // other link forms: Elsevier ?pii= query, OUP supplement issue (left out of the query), unknown OUP journal
  assert_eq!(query_pii("https://www.heartrhythmjournal.com/action/showPdf?pii=S1547-5271%2826%2902264-2").as_deref(),Some("S1547527126022642"));
  assert!(oup_citation("https://academic.oup.com/cid/article/82/Supplement_3/i97/8368969").is_some_and(|c|c.issue.is_none()&&c.first_page=="i97"));
  assert!(oup_citation("https://academic.oup.com/cid/article-pdf/55/10/e86/17350181/cis629.pdf").is_some_and(|c|c.volume=="55"&&c.issue.as_deref()==Some("10")));
  assert!(oup_citation("https://academic.oup.com/xyz/article/1/2/3/4").is_none());
 }
}
