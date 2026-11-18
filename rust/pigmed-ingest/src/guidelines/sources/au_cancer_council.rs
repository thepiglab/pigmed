//! Cancer Council clinical guidelines: topic pages, and the full guideline PDFs those pages link.
//! Topics whose guideline lives only on MAGICapp emit those MAGIC guidelines (published PDF via the MAGIC API,
//! `au_magic`), related to the topic page; topics with neither stay metadata records (topic indexes).
use super::{au_magic,common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,DateObservation,FetchRecord,GuidelineDoc,Relation,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct AuCancerCouncil;
fn catalog(html:&str,base:&Url)->Result<Vec<String>>{let d=Html::parse_document(html);let main=d.select(&sel("main.body-content")).next().context("Cancer Council catalog main missing")?;let mut out=vec![];let mut seen=HashSet::new();for a in main.select(&sel("a[href]")){let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let p=Url::parse(&u)?;let path=p.path();if p.origin()==base.origin()&&path.starts_with("/health-professionals/clinical-practice-guidelines/")&&path!="/health-professionals/clinical-practice-guidelines/"&&seen.insert(u.clone()){out.push(u)}}if out.is_empty(){bail!("Cancer Council topic links missing")}Ok(out)}
fn topic(html:&str,base:&Url)->Result<(String,String,Vec<crate::guidelines::doc::Block>,Vec<Value>)>{let d=Html::parse_document(html);let main=d.select(&sel("main.body-content")).next().context("Cancer Council topic main missing")?;let title=main.select(&sel("h1")).next().map(|x|text_of(&x)).context("Cancer Council title missing")?;let mut links=vec![];let mut seen=HashSet::new();for (i,a) in main.select(&sel("a[href]")).enumerate(){let href=a.value().attr("href").unwrap_or_default();let Ok(u)=base.join(href)else{continue};if !matches!(u.scheme(),"http"|"https")||!u.username().is_empty()||u.password().is_some(){continue;}let label=text_of(&a);let role=if u.host_str()==Some("app.magicapp.org"){"clinical_platform_link"}else if label.to_lowercase().contains("archived")&&(u.host_str()==Some("cancercouncilaustralia.mediavalet.com")||u.host_str()==Some("www.cancer.org.au")){"archived_guideline_resource"}else if u.host_str()==Some("cdn.mediavalet.com")&&u.path().to_ascii_lowercase().ends_with(".pdf")&&label.to_lowercase().contains("guideline"){"source_guideline_pdf"}else if u.host_str()==Some("cancercouncilaustralia.mediavalet.com")&&label.to_lowercase().contains("clinical practice guideline"){"source_guideline_gallery"}else{continue};if seen.insert((u.to_string(),label.clone())){links.push(json!({"url":u.to_string(),"label":label,"role":role,"source_locator":format!("main/link/{i}"),"status":"unresolved; independent access and terms","rights":"unknown"}));}}if !links.iter().any(|x|matches!(x["role"].as_str(),Some("clinical_platform_link"|"source_guideline_pdf"|"source_guideline_gallery"))){bail!("Cancer Council source topic has no mapped clinical guideline link")}let raw=main.html();let hash=crate::common::sha256_hex(raw.as_bytes());let(blocks,_)=blocks_from_html(&raw,Some(base),Some("main.body-content"));Ok((title,hash,blocks,links))}
fn unresolved(src:&SourceEntry,url:String,error:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:None,rendition:None,version:None,extra:json!({"discovery_error":error})}}

/// The topic page's own guideline PDF links (Cancer Council's asset CDN), one per URL, in page order.
/// The CTA text carries an icon label ("CC_Information") and button text ("Download") around the title.
fn guideline_pdfs(links:&[Value])->Vec<(String,String)>{
    let mut seen=HashSet::new();
    links.iter().filter(|l|l["role"]=="source_guideline_pdf").filter_map(|l|{
        let url=l["url"].as_str()?.to_string();
        let label=l["label"].as_str().unwrap_or_default();
        let title=label.trim().trim_start_matches("CC_Information").trim().trim_end_matches("Download").trim().to_string();
        seen.insert(url.clone()).then_some((url,title))
    }).collect()
}
fn published_years(html:&str,source:&str)->Vec<DateObservation>{
    let d=Html::parse_document(html);let mut out=vec![];
    if let Some(main)=d.select(&sel("main.body-content")).next(){
        let re=regex::Regex::new(r"Published:\s*(\d{4})\b").unwrap();
        for cap in re.captures_iter(&text_of(&main)){out.push(common::date("Published",&cap[1],source));}
    }
    out
}
/// A DOI printed in the guideline PDF's opening pages (the citation of its journal publication), verbatim.
fn printed_dois(blocks:&[Block])->Vec<String>{
    let re=regex::Regex::new(r"(?i)\bdoi:\s*(10\.\d{4,9}/[^\s;,]+)").unwrap();
    let mut out:Vec<String>=vec![];
    for b in blocks.iter().take(60){for c in re.captures_iter(&b.text){let d=c[1].trim_end_matches('.').to_string();if !out.contains(&d){out.push(d);}}}
    out
}
fn stale_notice(blocks:&[Block])->Option<String>{
    let text=blocks.iter().take(60).map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");
    let re=regex::Regex::new(r"This resource has been developed, reviewed or revised more than five years ago\.[^.]*\.").unwrap();
    re.find(&text).map(|m|m.as_str().to_string())
}
#[async_trait::async_trait]impl Adapter for AuCancerCouncil{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![])}
  let cat=f.get_ok(src.catalog_url.as_deref().context("Cancer Council catalog missing")?).await?;let b=Url::parse(&cat.record.final_url)?;
  let mut rows=vec![];let mut errors=vec![];let mut pdf_seen=HashSet::new();let mut magic_seen=HashSet::new();let mut magic_index:Option<au_magic::MagicIndex>=None;
  for url in catalog(&cat.text(),&b)?{
   let p=match f.get_ok(&url).await{Ok(p)=>p,Err(e)=>{errors.push(unresolved(src,url,format!("{e:#}")));continue}};
   let u=Url::parse(&p.record.final_url)?;if u.origin()!=b.origin(){errors.push(unresolved(src,url,"topic origin changed".into()));continue;}
   let html=p.text();
   match topic(&html,&u){
    Ok((title,hash,_,links))=>{
     let pdfs=guideline_pdfs(&links);
     let magic=au_magic::codes(links.iter().filter(|l|l["role"]=="clinical_platform_link").filter_map(|l|l["url"].as_str()));
     if pdfs.is_empty()&&!magic.is_empty()&&src.config["magic_api"].is_object(){
      // The topic's guideline lives on MAGICapp: the MAGIC guideline is the document, the topic page its landing.
      if magic_index.is_none(){magic_index=Some(au_magic::MagicIndex::load(src,f).await?);}
      let extra=json!({"parent_url":url,"topic_title":title,"catalog_fetch":cat.record});
      for r in au_magic::rows_for(src,magic_index.as_ref().unwrap(),&magic,&url,&title,&extra,&mut magic_seen)?{rows.push(r);if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}}
     } else if pdfs.is_empty(){
      // Only gallery routes (or MAGIC without config.magic_api): the public topic page stays a metadata record.
      let key=common::short_hash(&url);
      rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&hash[..12]),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"clinical_sha256":hash,"catalog_fetch":cat.record})});
     } else {
      // The topic page links the full guideline as a PDF: that PDF is the document, the topic page its landing.
      for (pdf_url,label) in pdfs {
       if !pdf_seen.insert(pdf_url.clone()){continue;}
       let name=Url::parse(&pdf_url)?.path_segments().and_then(|s|s.last()).unwrap_or("guideline").to_string();let work=common::slug(&name);
       let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:pdf_url.clone(),title:Some(if label.is_empty(){title.clone()}else{label}),rendition:Some("pdf".into()),version:None,
        extra:json!({"work_key":work,"role":"full_guideline_pdf","parent_url":url,"topic_title":title,"catalog_fetch":cat.record})};
       match f.get_ok(&pdf_url).await{
        Ok(pdf) if pdf.is_pdf()=>{row.doc_key=format!("{work}-{}",&pdf.record.sha256[..12]);row.extra["content_sha256"]=json!(pdf.record.sha256);},
        Ok(pdf)=>row.extra["discovery_error"]=json!(format!("linked guideline PDF did not return PDF bytes ({} bytes of {})",pdf.body.len(),pdf.record.content_type.as_deref().unwrap_or("unknown type"))),
        Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
       }
       rows.push(row);if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
      }
     }
     if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
    },
    Err(e)=>errors.push(unresolved(src,url,format!("{e:#}")))
   }
  }
  let n=limit.map(|n|n.saturating_sub(rows.len())).unwrap_or(usize::MAX);rows.extend(errors.into_iter().take(n));Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("Cancer Council topic unresolved: {e}")}
  if row.extra["role"]=="full_guideline_pdf"{return extract_pdf(src,f,row).await;}
  if au_magic::is_magic_row(row){return au_magic::extract(src,f,row,|html,b|{let(_,_,_,links)=topic(html,b)?;Ok(au_magic::codes(links.iter().filter(|l|l["role"]=="clinical_platform_link").filter_map(|l|l["url"].as_str())))}).await;}
  let p=f.get_ok(&row.url).await?;let b=Url::parse(&p.record.final_url)?;if b.origin()!=Url::parse(&row.url)?.origin(){bail!("Cancer Council topic origin changed")}
  let(title,hash,blocks,attachments)=topic(&p.text(),&b)?;let dates=published_years(&p.text(),&row.url);
  if row.extra["clinical_sha256"].as_str().is_some_and(|x|x!=hash){bail!("Cancer Council topic changed since discovery")}
  let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(p.record);
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),title,url:row.url.clone(),item_type:Some("guideline_topic_index".into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),..Default::default()},date_observations:dates,blocks,attachments,fetches,warnings:vec!["Topic overview is not the full clinical guideline. Source-linked PDFs, galleries and MAGIC routes are independent unresolved resources with their own access and rights; no platform agreement or API use is assumed.".into()],..Default::default()})
 }
}
async fn extract_pdf(src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
 let page=f.get_ok(&row.url).await?;
 if !page.is_pdf(){bail!("Cancer Council guideline link did not return PDF bytes")}
 if row.extra["content_sha256"].as_str().is_some_and(|h|h!=page.record.sha256){bail!("Cancer Council guideline PDF changed since discovery")}
 let parent=row.extra["parent_url"].as_str().context("Cancer Council PDF landing missing")?;
 let landing=f.get_ok(parent).await?;let lb=Url::parse(&landing.record.final_url)?;
 if lb.origin()!=Url::parse(parent)?.origin(){bail!("Cancer Council topic origin changed")}
 let landing_html=landing.text();
 let(topic_title,_,_,links)=topic(&landing_html,&lb)?;
 if !guideline_pdfs(&links).iter().any(|(u,_)|u==&row.url){bail!("Cancer Council PDF is no longer linked from its topic page")}
 let mut dates=published_years(&landing_html,parent);for d in &mut dates{d.label=format!("Source landing {}",d.label);}
 let (blocks,mut warnings,_)=super::static_list::blocks_for(&page,None,30).await?;
 if blocks.is_empty(){bail!("Cancer Council PDF conversion yielded no content")}
 let mut relations=vec![Relation{rel:"related".into(),target:parent.into(),scope:Some(format!("Topic page \"{topic_title}\" links this full guideline PDF")),evidence_url:Some(parent.into())}];
 for d in printed_dois(&blocks){relations.push(Relation{rel:"related".into(),target:format!("doi:{d}"),scope:Some("Journal publication cited in the guideline PDF's opening pages".into()),evidence_url:Some(row.url.clone())});}
 let mut search_metadata=Value::Null;
 if let Some(n)=stale_notice(&blocks){warnings.push(format!("Publisher notice in the PDF: {n}"));search_metadata=json!({"source_status_notice":{"text":n,"source_url":row.url}});}
 warnings.push("PDF is the publisher-linked rendition (several are exports of the retired wiki.cancer.org.au guideline pages). Layout, tables, figures and recommendation boxes need fidelity review; recommendation grades are not structured from PDF text.".into());
 let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(landing.record);fetches.push(page.record);
 Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or(topic_title),url:row.url.clone(),
  item_type:Some("guideline".into()),status:"unknown".into(),date_observations:dates,relations,search_metadata,
  rights:Rights{status:"unknown".into(),note:Some("Cancer Council Australia asks for permission for substantial reproduction; no licence is stated with this PDF. Rights need review.".into()),..Default::default()},
  blocks,fetches,warnings,..Default::default()})
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{use super::*;use std::io::Read;fn decode(x:&[u8])->String{let mut s=String::new();flate2::read::GzDecoder::new(x).read_to_string(&mut s).unwrap();s}#[test]fn genuine_melanoma_topic_keeps_magic_fragment(){let cat=decode(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/catalog-1.gzpart")));let raw=decode(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/melanoma-1.gzpart")));let b=Url::parse("https://www.cancer.org.au/health-professionals/clinical-practice-guidelines").unwrap();assert!(catalog(&cat,&b).unwrap().iter().any(|u|u.ends_with("/melanoma")));let(_,_,_,links)=topic(&raw,&b).unwrap();assert!(links.iter().any(|x|x["url"]=="https://app.magicapp.org/#/guideline/Lkk3pL"));assert!(links.iter().any(|x|x["role"]=="archived_guideline_resource"));}}

#[cfg(all(test, feature = "guideline-fixtures"))]mod live_pdf_and_gallery_tests{use super::*;use std::io::Read;fn decode(raw:&[u8])->String{let mut s=String::new();flate2::read::GzDecoder::new(raw).read_to_string(&mut s).unwrap();s}
#[test]fn source_pdf_and_gallery_are_distinct_from_platform_and_archive(){let b=Url::parse("https://www.cancer.org.au/health-professionals/clinical-practice-guidelines/barretts-oesophagus").unwrap();let raw=decode(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/barrett.html.gz")));let(_,_,blocks,links)=topic(&raw,&b).unwrap();assert!(blocks.iter().any(|x|x.text.contains("Published: 2014")));assert!(blocks.iter().any(|x|x.text.contains("more than five years")));assert!(links.iter().any(|x|x["role"]=="source_guideline_pdf"&&x["url"].as_str().unwrap().ends_with("guidelines-barrett-s.pdf")));assert!(links.iter().any(|x|x["role"]=="archived_guideline_resource"));let raw=decode(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/lung-prevention.html.gz")));let(_,_,_,links)=topic(&raw,&b).unwrap();assert!(links.iter().any(|x|x["role"]=="source_guideline_gallery"));}
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod guideline_pdf_route_tests{use super::*;use std::io::Read;
 const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/barrett-excerpt.pdf"));
 const CATALOG:&str="https://www.cancer.org.au/health-professionals/clinical-practice-guidelines";
 const TOPIC:&str="https://www.cancer.org.au/health-professionals/clinical-practice-guidelines/barretts-oesophagus";
 const PDF_URL:&str="https://cdn.mediavalet.com/aunsw/cancercouncilaustralia/P4F3yc6uakGPwpAGdbKVsA/4TYXarD1skesLeV8i_eRQw/Original/guidelines-barrett-s.pdf";
 fn decode(raw:&[u8])->String{let mut s=String::new();flate2::read::GzDecoder::new(raw).read_to_string(&mut s).unwrap();s}
 #[test]fn topic_pdf_links_are_titled_without_ui_text(){
  let raw=decode(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/barrett.html.gz")));
  let(_,_,_,links)=topic(&raw,&Url::parse(TOPIC).unwrap()).unwrap();let pdfs=guideline_pdfs(&links);
  assert_eq!(pdfs.len(),1);assert_eq!(pdfs[0].0,PDF_URL);
  assert!(!pdfs[0].1.contains("CC_Information") && !pdfs[0].1.ends_with("Download") && pdfs[0].1.to_lowercase().contains("barrett"),"{}",pdfs[0].1);
  let melanoma=decode(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/melanoma-1.gzpart")));
  let(_,_,_,links)=topic(&melanoma,&Url::parse(TOPIC).unwrap()).unwrap();assert!(guideline_pdfs(&links).is_empty());
 }
 #[tokio::test]async fn offline_topic_pdf_becomes_the_guideline_document(){
  let topic_html=decode(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/barrett.html.gz")));
  // bounded catalog: one real topic link inside the catalog's content root
  let catalog="<main class=\"body-content\"><a href=\"/health-professionals/clinical-practice-guidelines/barretts-oesophagus\">Barrett's Oesophagus</a></main>";
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,8<<20,"").unwrap();
  f.seed_cache_for_tests(CATALOG,catalog.as_bytes(),"text/html");f.seed_cache_for_tests(TOPIC,topic_html.as_bytes(),"text/html");f.seed_cache_for_tests(PDF_URL,PDF,"application/pdf");
  let src:SourceEntry=serde_json::from_value(json!({"id":"au_cancer_council","access":"mixed","enabled":true,"catalog_url":CATALOG})).unwrap();
  let rows=AuCancerCouncil.discover(&src,&f,None).await.unwrap();
  assert_eq!(rows.len(),1);let row=&rows[0];
  assert_eq!(row.url,PDF_URL);assert_eq!(row.rendition.as_deref(),Some("pdf"));assert!(row.extra["discovery_error"].is_null(),"{:?}",row.extra);
  assert!(row.doc_key.starts_with("guidelines-barrett-s-pdf-"));
  let doc=AuCancerCouncil.extract(&src,&f,row).await.unwrap();
  assert_eq!(doc.item_type.as_deref(),Some("guideline"));assert_eq!(doc.status,"unknown");
  assert!(doc.blocks.iter().any(|b|b.text.contains("endoscopic mucosal resection")),"{:?}",doc.blocks.iter().take(8).map(|b|&b.text).collect::<Vec<_>>());
  assert!(doc.relations.iter().any(|r|r.target==TOPIC));
  assert!(doc.relations.iter().any(|r|r.target=="doi:10.1111/jgh.12913"),"{:?}",doc.relations);
  assert!(doc.date_observations.iter().any(|d|d.label=="Source landing Published" && d.raw=="2014"));
  assert!(doc.published.is_none());
  assert!(doc.search_metadata["source_status_notice"]["text"].as_str().is_some_and(|t|t.contains("more than five years ago")));
  assert_eq!(doc.fetches.len(),3);
  f.seed_cache_for_tests(PDF_URL,b"<html>viewer</html>","text/html");assert!(AuCancerCouncil.extract(&src,&f,row).await.is_err());
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod magic_route_tests{
 //! Genuine melanoma topic page (links only MAGIC guideline Lkk3pL). The API listing is SYNTHETIC: a genuine MAGIC
 //! API record (h2_catalog cari fixture) with shortCode/organisation changed to Lkk3pL/"cca_test", to exercise the
 //! wiring offline; a real small PDF stands in for the MAGIC rendition.
 use super::*;use std::io::Read;
 const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/barrett-excerpt.pdf"));
 const CATALOG:&str="https://www.cancer.org.au/health-professionals/clinical-practice-guidelines";
 const TOPIC:&str="https://www.cancer.org.au/health-professionals/clinical-practice-guidelines/melanoma";
 fn decode(raw:&[u8])->String{let mut s=String::new();flate2::read::GzDecoder::new(raw).read_to_string(&mut s).unwrap();s}
 #[tokio::test]async fn magic_only_topic_emits_the_magic_guideline_related_to_the_topic(){
  let melanoma=decode(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_cancer_council/melanoma-1.gzpart")));
  let cari:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/h2_catalog/magic-cari.json"))).unwrap();
  let mut rec=cari.as_array().unwrap().iter().find(|g|g["shortCode"]=="Lpmozn").unwrap().clone();rec["shortCode"]=json!("Lkk3pL");rec["institutionShortName"]=json!("cca_test");
  let catalog="<main class=\"body-content\"><a href=\"/health-professionals/clinical-practice-guidelines/melanoma\">Melanoma</a></main>";
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,8<<20,"").unwrap();
  f.seed_cache_for_tests(CATALOG,catalog.as_bytes(),"text/html");f.seed_cache_for_tests(TOPIC,melanoma.as_bytes(),"text/html");
  f.seed_cache_for_tests(&super::super::h2_catalog::magic_list_url("cca_test"),serde_json::to_vec(&json!([rec])).unwrap().as_slice(),"application/json");
  let pdf_url="https://files.magicapp.org/guideline/2d3034bd-4d24-4963-9259-071abda1f507/published_guideline_9509-3_1.pdf";f.seed_cache_for_tests(pdf_url,PDF,"application/pdf");
  // Without config.magic_api the topic stays a topic index (unchanged behaviour).
  let plain:SourceEntry=serde_json::from_value(json!({"id":"au_cancer_council","access":"mixed","enabled":true,"catalog_url":CATALOG})).unwrap();
  let rows=AuCancerCouncil.discover(&plain,&f,None).await.unwrap();assert_eq!(rows.len(),1);assert_eq!(rows[0].url,TOPIC);
  let src:SourceEntry=serde_json::from_value(json!({"id":"au_cancer_council","access":"mixed","enabled":true,"catalog_url":CATALOG,"config":{"magic_api":{"org_short_names":["cca_test"],"item_type":"guideline","scope":"test"}}})).unwrap();
  let rows=AuCancerCouncil.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),1);let row=&rows[0];
  assert_eq!(row.url,pdf_url);assert!(row.extra["discovery_error"].is_null(),"{:?}",row.extra);assert_eq!(row.extra["magic_code"],"Lkk3pL");
  let d=AuCancerCouncil.extract(&src,&f,row).await.unwrap();
  assert_eq!(d.item_type.as_deref(),Some("guideline"));
  assert!(d.relations.iter().any(|r|r.rel=="related"&&r.target==TOPIC));
  assert!(d.relations.iter().any(|r|r.rel=="rendition_of"&&r.target=="https://app.magicapp.org/#/guideline/Lkk3pL"));
  assert!(d.warnings.iter().any(|w|w.contains("named AI crawlers")));
  assert_eq!(d.fetches.len(),4);
 }
}
