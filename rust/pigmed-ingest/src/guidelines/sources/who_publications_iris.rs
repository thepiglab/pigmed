//! WHO publication landing pages and their explicitly linked IRIS renditions.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{GuidelineDoc,Relation,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::{HashMap,HashSet,VecDeque};
use url::Url;
pub struct WhoPublicationsIris;
fn item(html:&str,base:&Url)->Result<(String,String,String,Vec<Value>,String)> {item_typed(html,base,false)}
/// `listed_guideline`: the WHO publications hub listed this item under its own "Guideline" publication type, which is
/// publisher evidence of the type even when the item page shows a different primary tag (e.g. "Technical document").
fn item_typed(html:&str,base:&Url,listed_guideline:bool)->Result<(String,String,String,Vec<Value>,String)> {
 let d=Html::parse_document(html);let root=d.select(&sel("article.dynamic-content__publication")).next().context("WHO publication article missing")?;
 let title=root.select(&sel("h1.dynamic-content__heading")).next().map(|e|text_of(&e)).filter(|s|!s.is_empty()).context("WHO item title missing")?;
 let tag=root.select(&sel(".dynamic-content__tag")).next().map(|e|text_of(&e)).unwrap_or_default();if !listed_guideline&&!tag.to_lowercase().contains("guideline")&&!title.to_lowercase().contains("annex"){bail!("WHO publication has no observed guideline or annex label");}
 let date=root.select(&sel(".dynamic-content__date")).next().map(|e|text_of(&e)).unwrap_or_default();
 let a=root.select(&sel(".dynamic-content__figure-container .button a[href]")).next().context("WHO item download link missing")?;let pdf=common::link(base,a.value().attr("href").unwrap_or_default()).context("WHO item invalid download URL")?;
 let mut related=vec![];for a in root.select(&sel(".dynamic-content__description a[href]")){let label=text_of(&a);if let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()){let low=label.to_lowercase();let role=if low.contains("annex"){"evidence_annex"}else if low.contains("portugu"){"translation"}else{"related_item"};related.push(json!({"url":url,"label":label,"role":role,"rights":"unknown; independent item"}));}}
 let hash=crate::common::sha256_hex(root.html().as_bytes());Ok((title,date,pdf,related,hash))
}
fn graph(html:&str,base:&Url)->Vec<String>{let d=Html::parse_document(html);let mut out=vec![];let mut seen=HashSet::new();let is_item=d.select(&sel("article.dynamic-content__publication")).next().is_some();
 for a in d.select(&sel("a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};let Ok(u)=Url::parse(&url) else {continue};if u.host_str()!=Some("www.who.int")||u.query().is_some(){continue;}
  let path=u.path();let label=text_of(&a).to_lowercase();let source_item=path.starts_with("/publications/i/item/")&&(is_item||label.contains("guideline"));let guidelines=path=="/publications/who-guidelines";
  if (source_item||guidelines)&&seen.insert(url.clone()){out.push(url)}
 }out}
fn pdf_complete(f:&crate::guidelines::fetch::Fetched)->bool{f.is_pdf()&&f.body.iter().rev().take(2048).copied().collect::<Vec<_>>().windows(5).any(|w|w==b"FOE%%")}
fn pdf_rights(blocks:&[crate::guidelines::doc::Block],url:&str)->Rights {if blocks.iter().any(|b|b.text.contains("CC BY-NC-SA 3.0 IGO")){Rights{license:Some("CC BY-NC-SA 3.0 IGO".into()),status:"restricted".into(),terms_url:Some("https://creativecommons.org/licenses/by-nc-sa/3.0/igo/".into()),note:Some("Licence observed in this PDF; third-party content and linked items require independent review.".into()),restrictions:vec!["Non-commercial; share alike".into()],evidence_urls:vec![url.to_string()],..Default::default()}}else{Rights{status:"unknown".into(),note:Some("No mapped item-specific licence notice; linked items have independent rights.".into()),..Default::default()}}}
fn unresolved(src:&SourceEntry,url:String,e:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:None,rendition:Some("html".into()),version:None,extra:json!({"discovery_error":e})}}
/// iris.who.int robots.txt asks for `Crawl-delay: 10`; the Fetcher paces per source, so gate IRIS bitstream
/// downloads here (cache hits are not delayed). robots.txt there allows `/server/api/core/bitstreams` only.
const IRIS_GAP:std::time::Duration=std::time::Duration::from_secs(60); // robots Crawl-delay is 10 s, but compute nodes saw TCP timeouts after 2-3 downloads at 10 s (H1, 2026-11-17): back off further
async fn iris_get(f:&Fetcher,url:&str)->Result<crate::guidelines::fetch::Fetched>{
 if Url::parse(url).ok().and_then(|u|u.host_str().map(String::from)).as_deref()!=Some("iris.who.int"){return f.get_ok(url).await}
 static LAST:std::sync::OnceLock<tokio::sync::Mutex<Option<std::time::Instant>>>=std::sync::OnceLock::new();
 let mut last=LAST.get_or_init(Default::default).lock().await;if let Some(t)=*last{if t.elapsed()<IRIS_GAP{tokio::time::sleep(IRIS_GAP-t.elapsed()).await;}}
 let r=f.get(url).await?;if !r.record.from_cache{*last=Some(std::time::Instant::now());}if r.record.status!=200{bail!("HTTP {} for {}",r.record.status,r.record.url);}Ok(r)
}
/// WHO publications hub (OData JSON behind the www.who.int/publications/i listing). `config.hub_api` is the listing
/// URL with WHO's own publication-type filter; the server caps `$top` at 100, so pages are walked with `$skip` until
/// the declared `@odata.count` is reached.
struct Hub{api:String,size:usize,skip:usize,declared:Option<usize>,unique:HashSet<String>,done:bool}
impl Hub{
 fn from_config(c:&Value)->Result<Option<Hub>>{let Some(api)=c["hub_api"].as_str() else {return Ok(None)};let u=Url::parse(api).context("WHO hub_api is not a URL")?;if u.host_str()!=Some("www.who.int")||!u.path().starts_with("/api/hubs/publications"){bail!("WHO hub_api must be the www.who.int publications hub");}let size=c["hub_page_size"].as_u64().unwrap_or(100).clamp(1,100) as usize;Ok(Some(Hub{api:api.into(),size,skip:0,declared:None,unique:HashSet::new(),done:false}))}
 fn page_url(&self,skip:usize)->String{format!("{}&$top={}&$skip={skip}",self.api,self.size)}
 async fn next_page(&mut self,f:&Fetcher)->Result<Option<Vec<(String,Value)>>>{
  if self.done||self.declared.is_some_and(|n|self.skip>=n){self.done=true;return Ok(None)}
  let v=f.get_json(&self.page_url(self.skip)).await?;let items=hub_items(&v)?;if let Some(n)=v["@odata.count"].as_u64(){self.declared=Some(n as usize);}
  if items.is_empty(){self.done=true;return Ok(None)}self.skip+=self.size;for (u,_) in &items{self.unique.insert(u.clone());}Ok(Some(items))
 }
 /// A declared total the walk did not reach is reported, never silently accepted.
 fn shortfall(&self)->Option<String>{let n=self.declared?;(self.unique.len()<n).then(||format!("WHO publications hub declared {n} items but paging returned {} distinct items; listing incomplete",self.unique.len()))}
}
fn hub_items(v:&Value)->Result<Vec<(String,Value)>>{
 let arr=v["value"].as_array().context("WHO hub response has no value array")?;let mut out=vec![];
 for x in arr{let path=x["ItemDefaultUrl"].as_str().context("WHO hub item without ItemDefaultUrl")?;if !path.starts_with('/')||path.contains(['?','#'])||path.len()<2{bail!("WHO hub item path not validated: {path}");}
  let url=format!("https://www.who.int/publications/i/item{path}");out.push((url,json!({"title":x["Title"],"date":x["FormatedDate"],"published":x["PublicationDateAndTime"],"tag":x["Tag"],"download_url":x["DownloadUrl"],"listing_scope":"WHO publications hub, publication type Guideline (publisher classification)"})));}
 Ok(out)
}
#[async_trait::async_trait] impl Adapter for WhoPublicationsIris {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![])}let start=src.catalog_url.as_deref().context("WHO catalog missing")?;let catalog=f.get_ok(start).await?;let base=Url::parse(&catalog.record.final_url)?;let mut queue:VecDeque<String>=graph(&catalog.text(),&base).into();if item(&catalog.text(),&base).is_ok(){queue.push_front(start.to_string());}let mut seen=HashSet::new();let mut rows=vec![];let mut deferred=vec![];
  // The publications hub listing (the JSON route behind www.who.int/publications/i, filtered to WHO's own
  // "Guideline" publication type) is paged after the page graph; its items are not expanded as graph nodes.
  let mut hub=Hub::from_config(&src.config)?;let mut listed:HashMap<String,Value>=HashMap::new();let mut hub_declared:Option<usize>=None;
  loop{
   // with --limit, unresolved items consume the sample too (a failing host must not turn a bounded plan into a full walk)
   if limit.is_some_and(|n|rows.len()+deferred.len()>=n){break}
   let Some(url)=queue.pop_front() else {
    let Some(h)=hub.as_mut() else {break};
    match h.next_page(f).await{
     Ok(Some(items))=>{if hub_declared.is_none(){if let Some(n)=h.declared{eprintln!("WHO publications hub declares {n} Guideline items");}}hub_declared=h.declared;for (u,v) in items{if !seen.contains(&u)&&!listed.contains_key(&u){queue.push_back(u.clone());listed.insert(u,v);}}continue;}
     Ok(None)=>{if let Some(e)=h.shortfall(){deferred.push(unresolved(src,h.page_url(h.skip),e));}hub=None;continue;}
     Err(e)=>{deferred.push(unresolved(src,h.page_url(h.skip),format!("WHO publications hub listing incomplete: {e:#}")));hub=None;continue;}
    }
   };
   if !seen.insert(url.clone()){continue;}if seen.len()>5000{bail!("WHO item graph exceeded 5000 links");}let hub_entry=listed.get(&url).cloned();let page=match f.get_ok(&url).await{Ok(x)=>x,Err(e)=>{deferred.push(unresolved(src,url,format!("{e:#}")));continue;}};let b=Url::parse(&page.record.final_url)?;if b.host_str()!=Some("www.who.int"){deferred.push(unresolved(src,url,"item redirect crossed source host".into()));continue;}let html=page.text();if hub_entry.is_none(){for next in graph(&html,&b){if !seen.contains(&next){queue.push_back(next)}}}
   if !b.path().starts_with("/publications/i/item/"){continue;}let (title,date,pdf,related,html_hash)=match item_typed(&html,&b,hub_entry.is_some()){Ok(x)=>x,Err(e)=>{deferred.push(unresolved(src,url,format!("{e:#}")));continue;}};
   let body=match iris_get(f,&pdf).await{Ok(x) if pdf_complete(&x)=>x,Ok(_)=>{deferred.push(unresolved(src,url,"source download is not a complete PDF".into()));continue;},Err(e)=>{eprintln!("WHO IRIS PDF unavailable for {url}: {e:#}");deferred.push(unresolved(src,url,format!("linked source PDF unavailable: {e:#}")));continue;}};
   let key=common::short_hash(&url);let mut extra=json!({"work_key":key,"pdf_url":pdf,"pdf_sha256":body.record.sha256,"item_sha256":html_hash,"source_date":date,"related":related,"catalog_fetch":catalog.record});if let Some(v)=hub_entry{extra["hub_listing"]=v;extra["hub_declared_count"]=json!(hub_declared);}
   rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&body.record.sha256[..12]),url,title:Some(title),rendition:Some("pdf".into()),version:None,extra});if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
  }let rest=limit.map(|n|n.saturating_sub(rows.len())).unwrap_or(usize::MAX);rows.extend(deferred.into_iter().take(rest));if rows.is_empty(){bail!("WHO graph produced no mapped or unresolved items")};Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("WHO discovery unresolved: {e}");}let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;if base.host_str()!=Some("www.who.int"){bail!("WHO item origin changed");}let (title,date,pdf,related,hash)=item_typed(&page.text(),&base,row.extra["hub_listing"].is_object())?;if row.extra["item_sha256"].as_str().is_some_and(|x|x!=hash)||row.extra["pdf_url"].as_str().is_some_and(|x|x!=pdf){bail!("WHO item changed since discovery");}
  let body=iris_get(f,&pdf).await?;if !pdf_complete(&body){bail!("WHO PDF is incomplete or not a PDF");}if row.extra["pdf_sha256"].as_str().is_some_and(|x|x!=body.record.sha256){bail!("WHO PDF changed since discovery");}
  let (blocks,mut warnings,_)=super::static_list::blocks_for(&body,None,0).await?;if blocks.is_empty(){bail!("WHO PDF yielded no content");}warnings.push("PDF text/tables extracted with generic conversion; recommendation grades, source table cell associations, figures and reading order require source-specific verification. Linked annexes and translations have independent rights.".into());
  let rights=pdf_rights(&blocks,&pdf);
  let kind=if title.to_lowercase().contains("annex") {"evidence_annex"} else {"guideline"};let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(page.record);fetches.push(body.record);
  let dates=if date.is_empty(){vec![]}else{vec![common::date("WHO publication date",&date,&row.url)]};let relations=related.iter().filter_map(|v|Some(Relation{rel:v["role"].as_str()?.into(),target:v["url"].as_str()?.into(),scope:None,evidence_url:Some(row.url.clone())})).collect();
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),item_type:Some(kind.into()),status:"unknown".into(),date_observations:dates,relations,rights,attachments:related,blocks,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{
 use super::*;
 const HUB0:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/hub-page-0.json"));const HUB1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/hub-page-1.json"));
 const HUB_API:&str="https://www.who.int/api/hubs/publications?sf_site=15210d59-ad60-47ff-a542-7ed76645f0c7&sf_provider=OpenAccessProvider&sf_culture=en&$orderby=PublicationDateAndTime%20desc&$select=Title,ItemDefaultUrl,FormatedDate,Tag,DownloadUrl,PublicationDateAndTime&$filter=publicationtypes/any(s:s%20eq%2074aa0c95-2bc6-4cfc-b749-5e91dc9c1e21)&$count=true";
 fn seed_json(f:&Fetcher,url:&str,body:&str){f.seed_cache_request_for_tests(&crate::guidelines::fetch::Req{headers:vec![("accept".into(),"application/json".into())],..crate::guidelines::fetch::Req::get(url)},body.as_bytes(),"application/json");}
 #[test]fn hub_items_map_item_routes_and_reject_unvalidated_paths(){let v:Value=serde_json::from_str(HUB0).unwrap();let items=hub_items(&v).unwrap();assert_eq!(items.len(),2);assert_eq!(items[0].0,"https://www.who.int/publications/i/item/9789240126121");assert_eq!(items[0].1["tag"],"Guideline");assert!(hub_items(&json!({"value":[{"ItemDefaultUrl":"https://evil.example/x"}]})).is_err());assert!(hub_items(&json!({"value":[{"ItemDefaultUrl":"/x?y=1"}]})).is_err());assert!(Hub::from_config(&json!({"hub_api":"https://example.org/api/hubs/publications?x=1"})).is_err());assert!(Hub::from_config(&json!({})).unwrap().is_none());}
 #[test]fn hub_listed_guideline_type_admits_items_with_another_primary_tag(){let d=Url::parse("https://www.who.int/publications/i/item/9789240126121").unwrap();let retagged=D.replacen(">Guideline<",">Technical document<",1);assert_ne!(retagged,D);assert!(item(&retagged,&d).is_err());assert!(item_typed(&retagged,&d,true).is_ok());}
 #[tokio::test]async fn hub_pages_are_walked_to_the_declared_count_and_shortfall_is_reported(){let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,10<<20,"").unwrap();let cat="https://www.who.int/publications/i";f.seed_cache_for_tests(cat,b"<html><body><main>listing shell</main></body></html>","text/html");f.seed_cache_for_tests("https://www.who.int/publications/i/item/9789240126121",D.as_bytes(),"text/html");
  let page=|skip:usize|format!("{HUB_API}&$top=2&$skip={skip}");seed_json(&f,&page(0),HUB0);seed_json(&f,&page(2),HUB1);seed_json(&f,&page(4),r#"{"@odata.count":961,"value":[]}"#);
  let src:SourceEntry=serde_json::from_value(json!({"id":"who_publications_iris","access":"public","enabled":true,"catalog_url":cat,"config":{"hub_api":HUB_API,"hub_page_size":2}})).unwrap();
  let rows=WhoPublicationsIris.discover(&src,&f,None).await.unwrap();let urls=rows.iter().map(|r|r.url.as_str()).collect::<Vec<_>>();
  for want in ["https://www.who.int/publications/i/item/9789240126121","https://www.who.int/publications/i/item/9789240126244","https://www.who.int/publications/i/item/9789290211051"]{assert!(urls.contains(&want),"{want} missing from {urls:?}");}
  // offline: no item PDFs are cached, so every item stays an explicit unresolved row
  assert!(rows.iter().all(|r|r.extra["discovery_error"].is_string()));assert!(rows.iter().any(|r|r.extra["discovery_error"].as_str().unwrap().contains("declared 961 items but paging returned 3")));
  // the diabetes page parsed (title/PDF link) and failed only on the uncached PDF, not on the listing
  assert!(rows.iter().any(|r|r.url.ends_with("9789240126121")&&r.extra["discovery_error"].as_str().unwrap().contains("linked source PDF unavailable")));
  let one=WhoPublicationsIris.discover(&src,&f,Some(1)).await.unwrap();assert_eq!(one.len(),1);
 }
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/catalog.html"));const G:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/who-guidelines.html"));const H:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/hypertension.html"));const A:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/annex.html"));const D:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/diabetes-2026.html"));
 const URL:&str="https://www.who.int/publications/i/item/9789240033986";const PDF:&str="https://iris.who.int/server/api/core/bitstreams/f062769d-f075-4a00-87af-0a2106e0bd04/content";
 #[test]fn genuine_catalog_item_and_distinct_pdf_roles(){let home=Url::parse("https://www.who.int/publications").unwrap();let d="https://www.who.int/publications/i/item/9789240126121";assert!(graph(CAT,&home).contains(&d.to_string()));assert!(graph(G,&home).iter().any(|u|u.ends_with("guidelines-for-malaria")));let (title,date,pdf,related,_)=item(H,&Url::parse(URL).unwrap()).unwrap();assert!(title.contains("hypertension"));assert_eq!(date,"24 August 2021");assert_eq!(pdf,PDF);assert_eq!(related.iter().filter(|a|a["role"]=="evidence_annex").count(),2);assert!(related.iter().any(|a|a["role"]=="translation"));assert!(item(A,&home).unwrap().0.contains("web annex A"));assert!(item(D,&Url::parse(d).unwrap()).unwrap().2.ends_with("8f061adc-92a9-4bda-a0e3-cf2470f11b3c/content"));}
 #[tokio::test]async fn real_page_excerpts_are_identified_as_excerpts(){let samples:[(&[u8],usize,&str);2]=[(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/hypertension-parent-page-10.pdf")),10,"recommendation"),(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/hypertension-parent-page-19.pdf")),19,"recommendation")];for(bytes,physical,needle)in samples{let url=format!("{PDF}#page={physical}");let record=serde_json::from_value(json!({"url":url,"final_url":url,"redirects":[],"status":200,"content_type":"application/pdf","bytes":bytes.len(),"sha256":crate::common::sha256_hex(bytes),"fetched_at":"excerpt fixture","from_cache":true})).unwrap();let page=crate::guidelines::fetch::Fetched{record,body:bytes.to_vec()};let(blocks,_,_)=super::super::static_list::blocks_for(&page,None,0).await.unwrap();assert!(blocks.iter().any(|b|b.text.to_lowercase().contains(needle)));}let manifest:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/hypertension-parent-excerpts.json"))).unwrap();assert!(manifest.as_array().unwrap().iter().any(|x|x["physical_page"]==19&&x["source_url"]==PDF));}
 #[tokio::test]async fn limits_count_unavailable_source_pdfs(){let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,10<<20,"").unwrap();f.seed_cache_for_tests(URL,H.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.who.int/publications/i/item/9789240033993",A.as_bytes(),"text/html");let src:SourceEntry=serde_json::from_value(json!({"id":"who_publications_iris","access":"public","enabled":true,"catalog_url":URL})).unwrap();assert_eq!(WhoPublicationsIris.discover(&src,&f,Some(0)).await.unwrap().len(),0);let one=WhoPublicationsIris.discover(&src,&f,Some(1)).await.unwrap();assert_eq!(one.len(),1);assert!(one[0].extra["discovery_error"].is_string());let three=WhoPublicationsIris.discover(&src,&f,Some(3)).await.unwrap();assert!(three.len()<=3);assert!(three.iter().all(|r|r.extra["discovery_error"].is_string()));}
 #[test]fn source_notice_is_item_specific(){let pages:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/hypertension-parent-page-text.json"))).unwrap();let text=pages.as_array().unwrap().iter().find(|x|x["physical_page"]==4).unwrap()["text"].as_str().unwrap();let blocks=crate::guidelines::html::blocks_from_markdown(text);assert_eq!(pdf_rights(&blocks,PDF).license.as_deref(),Some("CC BY-NC-SA 3.0 IGO"));assert_eq!(pdf_rights(&[],PDF).status,"unknown");}
 #[tokio::test]async fn genuine_copyright_page_pdf_has_item_specific_licence(){let bytes=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/who_publications_iris/hypertension-rights-page-4.pdf"));let url=format!("{PDF}#page=4");let record=serde_json::from_value(json!({"url":url,"final_url":url,"redirects":[],"status":200,"content_type":"application/pdf","bytes":bytes.len(),"sha256":crate::common::sha256_hex(bytes),"fetched_at":"physical-page excerpt fixture","from_cache":true})).unwrap();let page=crate::guidelines::fetch::Fetched{record,body:bytes.to_vec()};let(blocks,_,_)=super::super::static_list::blocks_for(&page,None,0).await.unwrap();let rights=pdf_rights(&blocks,PDF);assert_eq!(rights.license.as_deref(),Some("CC BY-NC-SA 3.0 IGO"));assert_eq!(rights.status,"restricted");assert_eq!(rights.evidence_urls,vec![PDF.to_string()]);}
}
