//! PAHO IRIS SSR item metadata and item-emitted complete PDF renditions.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{GuidelineDoc,Relation,Rights};
use crate::guidelines::fetch::{Fetched,Fetcher};
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct PahoIris;
fn meta(d:&Html,name:&str)->Option<String>{d.select(&sel(&format!("meta[name='{name}'][content]"))).next().and_then(|e|e.value().attr("content")).filter(|s|!s.is_empty()).map(String::from)}
fn item(html:&str,base:&Url)->Result<(String,String,String,String,Vec<Value>,String)>{item_l(html,base,false)}
/// `labelled`: the item came from PAHO's own "Guidelines" document-type listing, so no title keyword is required.
fn item_l(html:&str,base:&Url,labelled:bool)->Result<(String,String,String,String,Vec<Value>,String)> {
 let d=Html::parse_document(html);let title=meta(&d,"citation_title").context("PAHO item citation title missing")?;let lower=title.to_lowercase();if !labelled&&!lower.contains("guideline")&&!lower.contains("guía")&&!lower.contains("guia")&&!lower.contains("diretriz"){bail!("PAHO item not source-labelled guideline");}
 let raw=meta(&d,"citation_pdf_url").context("PAHO item PDF link missing")?;let pdf=common::link(base,&raw).context("invalid PAHO PDF link")?;let date=meta(&d,"citation_publication_date").unwrap_or_default();let lang=meta(&d,"citation_language").unwrap_or_default();let mut related=vec![];
 for a in d.select(&sel("ds-item-page-relation-language-version a[href]")){if let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()){related.push(json!({"url":url,"label":text_of(&a),"role":"other_language_version","rights":"unknown; independent item"}));}}
 // Exclude unrelated SSR state, navigation and dynamically rendered recommendations;
 // the PDF checksum independently guards actual clinical content.
 let metadata=json!({"title":title,"pdf":pdf,"date":date,"language":lang,"related":related});let hash=crate::common::sha256_hex(serde_json::to_string(&metadata)?.as_bytes());Ok((title,pdf,date,lang,related,hash))
}
fn catalog(html:&str,base:&Url)->Vec<String>{let d=Html::parse_document(html);let mut seen=HashSet::new();let mut out=vec![];for a in d.select(&sel("a[href]")){let label=text_of(&a).to_lowercase();if !label.contains("guideline")&&!label.contains("guía")&&!label.contains("guia")&&!label.contains("diretriz"){continue;}let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};if let Ok(u)=Url::parse(&url){if u.origin()==base.origin()&&u.path().starts_with("/items/")&&seen.insert(url.clone()){out.push(url)}}}out}
const SITEMAP_INDEX:&str="https://iris.paho.org/sitemap_index.xml"; // explicitly listed in this host's robots.txt
fn sitemap(xml:&str,root:&str)->Result<Vec<String>> {let d=roxmltree::Document::parse(xml).context("PAHO sitemap XML invalid")?;if d.root_element().tag_name().name()!=root{bail!("unexpected PAHO sitemap root");}let urls=d.descendants().filter(|n|n.is_element()&&n.tag_name().name()=="loc").filter_map(|n|n.text()).map(|s|s.trim().to_string()).collect::<Vec<_>>();if urls.is_empty(){bail!("PAHO sitemap is empty");}Ok(urls)}
fn pdf_complete(f:&Fetched)->bool{f.is_pdf()&&f.body.iter().rev().take(2048).copied().collect::<Vec<_>>().windows(5).any(|w|w==b"FOE%%")}
fn pdf_rights(blocks:&[crate::guidelines::doc::Block],url:&str)->Rights {if blocks.iter().any(|b|b.text.contains("All rights reserved.")&&b.text.contains("Pan American Health Organization")){Rights{license:Some("All rights reserved".into()),status:"restricted".into(),note:Some("Item PDF requests permission to reproduce or translate; individual linked assets require separate review.".into()),restrictions:vec!["Requests for permission to reproduce or translate PAHO publications".into()],evidence_urls:vec![url.to_string()],..Default::default()}}else{Rights{status:"unknown".into(),note:Some("Item-specific notice not established; linked items require independent review.".into()),..Default::default()}}}
fn unresolved(src:&SourceEntry,url:String,e:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:None,rendition:Some("html".into()),version:None,extra:json!({"discovery_error":e})}}
#[async_trait::async_trait] impl Adapter for PahoIris {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![])}if let Some(l)=src.config["listing_url"].as_str(){return listing_discover(src,f,limit,l).await}let start=src.catalog_url.as_deref().context("PAHO catalog missing")?;let cat=f.get_ok(start).await?;let base=Url::parse(&cat.record.final_url)?;let mut urls=catalog(&cat.text(),&base);if item(&cat.text(),&base).is_ok(){urls.insert(0,start.to_string())}let mut rows=vec![];let mut deferred=vec![];
  // Sitemap item URLs have no clinical labels. Scan a bounded, resumable slice;
  // do not assume every repository item is a guideline or that this is exhaustive.
  if start=="https://iris.paho.org/" {let offset=src.config["sitemap_start"].as_u64().unwrap_or(0) as usize;let cap=src.config["max_candidates"].as_u64().unwrap_or(20).clamp(1,1000) as usize;let mut seen_items=0usize;let mut added=0usize;let mut bounded=false;
   match f.get_ok(SITEMAP_INDEX).await {Ok(index)=>match sitemap(&index.text(),"sitemapindex"){Ok(children)=>{for child in children {let u=Url::parse(&child)?;if u.origin()!=base.origin(){deferred.push(unresolved(src,child,"sitemap child crosses origin".into()));continue;}let data=match f.get_ok(&child).await{Ok(x)=>x,Err(e)=>{deferred.push(unresolved(src,child,format!("sitemap child unavailable: {e:#}")));continue;}};
    for raw in sitemap(&data.text(),"urlset")? {let u=match Url::parse(&raw){Ok(u)=>u,Err(_)=>continue};if u.origin()!=base.origin()||!u.path().starts_with("/items/")||u.path().ends_with("/full"){continue;}if seen_items<offset{seen_items+=1;continue;}if added>=cap{bounded=true;break;}seen_items+=1;added+=1;urls.push(raw);}
    if bounded{break;}
   }if bounded{deferred.push(unresolved(src,SITEMAP_INDEX.into(),format!("bounded sitemap scan: start {offset}, scanned {added} item candidates; further inventory not visited")));}},Err(e)=>deferred.push(unresolved(src,SITEMAP_INDEX.into(),format!("{e:#}")))},Err(e)=>deferred.push(unresolved(src,SITEMAP_INDEX.into(),format!("{e:#}")))}
  }
  let mut seen=HashSet::new();
  for url in urls {if !seen.insert(url.clone()){continue;}let page=match f.get_ok(&url).await{Ok(x)=>x,Err(e)=>{deferred.push(unresolved(src,url,format!("{e:#}")));continue;}};let b=Url::parse(&page.record.final_url)?;if b.host_str()!=Some("iris.paho.org"){deferred.push(unresolved(src,url,"PAHO item redirect crossed host".into()));continue;}
   let (title,pdf,date,lang,related,metadata_hash)=match item(&page.text(),&b){Ok(x)=>x,Err(e)=>{deferred.push(unresolved(src,url,format!("{e:#}")));continue;}};let body=match f.get_ok(&pdf).await{Ok(x)if pdf_complete(&x)=>x,Ok(_)=>{deferred.push(unresolved(src,url,"source download is not a complete PDF".into()));continue;},Err(e)=>{deferred.push(unresolved(src,url,format!("source PDF unavailable: {e:#}")));continue;}};
   let key=common::short_hash(&url);rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&body.record.sha256[..12]),url,title:Some(title),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":key,"item_sha256":metadata_hash,"pdf_url":pdf,"pdf_sha256":body.record.sha256,"source_date":date,"language":lang,"related":related,"catalog_fetch":cat.record})});if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
  }let rest=limit.map(|n|n.saturating_sub(rows.len())).unwrap_or(usize::MAX);rows.extend(deferred.into_iter().take(rest));if rows.is_empty(){bail!("PAHO homepage contains no source-labelled guideline links; inventory is unresolved pending sitemap mapping");}Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("PAHO discovery unresolved: {e}");}if row.extra["listing"].is_object(){return listing_extract(src,f,row).await}let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;if base.host_str()!=Some("iris.paho.org"){bail!("PAHO item origin changed");}let (title,pdf,date,lang,related,hash)=item(&page.text(),&base)?;if row.extra["item_sha256"].as_str().is_some_and(|x|x!=hash)||row.extra["pdf_url"].as_str().is_some_and(|x|x!=pdf){bail!("PAHO item changed since discovery");}
  let body=f.get_ok(&pdf).await?;if !pdf_complete(&body){bail!("PAHO PDF is incomplete or not a PDF");}if row.extra["pdf_sha256"].as_str().is_some_and(|x|x!=body.record.sha256){bail!("PAHO PDF changed since discovery");}let (blocks,mut warnings,_)=super::static_list::blocks_for(&body,None,0).await?;if blocks.is_empty(){bail!("PAHO PDF yielded no content");}
  let rights=pdf_rights(&blocks,&pdf);
  warnings.push("The PDF is a source-labelled summary if so indicated by its title; generic conversion does not verify physical/printed pages, multicolumn recommendation cells, evidence keys or figure order. Other language items have independent rights. Sitemap scans are bounded/resumable and do not establish full inventory.".into());let kind=if ["summarized version","versión resumida","versao resumida","versão resumida"].iter().any(|x|title.to_lowercase().contains(*x)){"summary"}else{"guideline"};let dates=if date.is_empty(){vec![]}else{vec![common::date("PAHO citation publication date",&date,&row.url)]};let relations=related.iter().filter_map(|x|Some(Relation{rel:"other_language_version".into(),target:x["url"].as_str()?.into(),scope:None,evidence_url:Some(row.url.clone())})).collect();
  let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(page.record);fetches.push(body.record);Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),item_type:Some(kind.into()),status:"unknown".into(),language:(!lang.is_empty()).then_some(lang),date_observations:dates,relations,rights,attachments:related,blocks,fetches,warnings,..Default::default()})
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{
 use super::*;use std::io::Read;
 const URL:&str="https://iris.paho.org/handle/10665.2/51089";const PDF:&str="https://iris.paho.org/bitstreams/a91f6109-d940-411d-8d89-360baf8392a0/download";
 fn page()->String{[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/item-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/item-2.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/item-3.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/item-4.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/item-5.html"))].concat()}fn sitemap_child()->String{let gz=[include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap0-1.gzpart")).as_slice(),include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap0-2.gzpart")).as_slice(),include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap0-3.gzpart")).as_slice(),include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap0-4.gzpart")).as_slice(),include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap0-5.gzpart")).as_slice(),include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap0-6.gzpart")).as_slice(),include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap0-7.gzpart")).as_slice(),include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap0-8.gzpart")).as_slice(),include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap0-9.gzpart")).as_slice()].concat();let mut xml=String::new();flate2::read::GzDecoder::new(gz.as_slice()).read_to_string(&mut xml).unwrap();xml}
 #[test]fn genuine_sitemap_and_related_summaries_have_distinct_metadata(){let index=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap-index.xml"));assert_eq!(sitemap(index,"sitemapindex").unwrap(),vec!["https://iris.paho.org/sitemap0.xml","https://iris.paho.org/sitemap1.xml"]);let xml=sitemap_child();assert_eq!(crate::common::sha256_hex(xml.as_bytes()),"cc653e39a1835f6cffc34462eefd1ebd9c3b79f0c4e1b6b506ede8b62b68d32f");let urls=sitemap(&xml,"urlset").unwrap();assert_eq!(urls.len(),50000);assert!(urls.iter().any(|u|u=="https://iris.paho.org/items/cb3946f4-57d1-4e9b-b147-609ccb26f825"));
  let (title,pdf,date,lang,related,_)=item(&page(),&Url::parse(URL).unwrap()).unwrap();assert!(title.contains("Summarized Version, 2017"));assert_eq!(pdf,PDF);assert_eq!(date,"2019");assert_eq!(lang,"en");assert!(related.iter().any(|v|v["url"]=="https://iris.paho.org/handle/10665.2/34949"));let spanish=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/spanish-item-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/spanish-item-2.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/spanish-item-3.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/spanish-item-4.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/spanish-item-5.html"))].concat();let (title,_,date,lang,_,_)=item(&spanish,&Url::parse(URL).unwrap()).unwrap();assert!(title.contains("Versión resumida, 2017"));assert_eq!(date,"2018");assert_eq!(lang,"es");}
 #[tokio::test]async fn actual_page_excerpts_are_not_full_source_pdfs(){let samples:[(&[u8],usize,&str);2]=[(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/rop-summary-page-3.pdf")),3,"all rights reserved"),(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/rop-summary-page-5.pdf")),5,"developer group")];for(bytes,physical,needle)in samples{let url=format!("{PDF}#page={physical}");let record=serde_json::from_value(json!({"url":url,"final_url":url,"redirects":[],"status":200,"content_type":"application/pdf","bytes":bytes.len(),"sha256":crate::common::sha256_hex(bytes),"fetched_at":"excerpt fixture","from_cache":true})).unwrap();let page=Fetched{record,body:bytes.to_vec()};let(blocks,_,_)=super::super::static_list::blocks_for(&page,None,0).await.unwrap();assert!(blocks.iter().any(|b|b.text.to_lowercase().contains(needle)));if physical==3{assert_eq!(pdf_rights(&blocks,PDF).status,"restricted");}}let mapping:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/rop-summary-excerpts.json"))).unwrap();assert!(mapping.as_array().unwrap().iter().any(|x|x["physical_page"]==5&&x["source_url"]==PDF));}
 #[tokio::test]async fn bounded_sitemap_limits_count_marker_and_missing_pdf(){let index=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/sitemap-index.xml"));let xml=sitemap_child();let home=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/catalog-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/catalog-2.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/catalog-3.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/catalog-4.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/paho_iris/catalog-5.html"))].concat();let target="https://iris.paho.org/items/cb3946f4-57d1-4e9b-b147-609ccb26f825";let offset=sitemap(&xml,"urlset").unwrap().iter().filter(|u|Url::parse(u).ok().is_some_and(|u|u.path().starts_with("/items/")&&!u.path().ends_with("/full"))).position(|u|u==target).unwrap();let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,10<<20,"").unwrap();f.seed_cache_for_tests("https://iris.paho.org/",home.as_bytes(),"text/html");f.seed_cache_for_tests(SITEMAP_INDEX,index.as_bytes(),"application/xml");f.seed_cache_for_tests("https://iris.paho.org/sitemap0.xml",xml.as_bytes(),"application/xml");f.seed_cache_for_tests(target,page().as_bytes(),"text/html");let src:SourceEntry=serde_json::from_value(json!({"id":"paho_iris","access":"public","enabled":true,"catalog_url":"https://iris.paho.org/","config":{"sitemap_start":offset,"max_candidates":1}})).unwrap();assert!(PahoIris.discover(&src,&f,Some(0)).await.unwrap().is_empty());let one=PahoIris.discover(&src,&f,Some(1)).await.unwrap();assert_eq!(one.len(),1);assert!(one[0].extra["discovery_error"].is_string());let three=PahoIris.discover(&src,&f,Some(3)).await.unwrap();assert!(three.len()<=3);assert!(three.iter().any(|r|r.extra["discovery_error"].as_str().is_some_and(|e|e.contains("bounded sitemap scan"))));}
}

// ---------------------------------------------------------------------------------------------------------------
// PAHO's own document catalog, filtered to its "Guidelines" document type (config `listing_url`, e.g.
// https://www.paho.org/en/documents?cat%5B0%5D=14). iris.paho.org disallows /search, /browse, /server/api/ (except
// bitstreams) and OAI for all agents, and its sitemap carries no item types, so the labelled paho.org listing is the
// enumerable route. Each card links an IRIS item (metadata + bitstream PDF), a paho.org document page (which links an
// IRIS item or a /sites/default/files PDF) or a WHO item. iris.paho.org asks for Crawl-delay: 10.

#[derive(Debug, Clone)]
struct Card {
    url: String,
    title: String,
    categories: String,
    date: Option<String>,
    datetime: Option<String>,
}

fn listing_cards(html: &str, base: &Url) -> Result<(Vec<Card>, Option<String>)> {
    let d = Html::parse_document(html);
    let grid = d.select(&sel(".views-view-grid")).next().context("PAHO documents listing grid missing")?;
    let mut out = vec![];
    for col in grid.select(&sel(".col")) {
        let Some(a) = col.select(&sel(".views-field-title a[href]")).next() else { continue };
        let Some(url) = common::link(base, a.value().attr("href").unwrap_or_default()) else { continue };
        let time = col.select(&sel(".views-field-created time")).next();
        out.push(Card {
            url,
            title: text_of(&a),
            categories: col.select(&sel(".views-field-field-categoria .field-content")).next().map(|e| text_of(&e)).unwrap_or_default(),
            date: time.map(|t| text_of(&t)).filter(|t| !t.is_empty()),
            datetime: time.and_then(|t| t.value().attr("datetime")).map(String::from),
        });
    }
    if out.is_empty() {
        bail!("PAHO documents listing has no cards");
    }
    let next = d
        .select(&sel("li.pager__item--next a[href]"))
        .next()
        .and_then(|a| common::link(base, a.value().attr("href").unwrap_or_default()));
    Ok((out, next))
}

/// IRIS item or bitstream-free PDF linked from a paho.org document page.
fn document_targets(html: &str, base: &Url) -> (Option<String>, Option<String>) {
    let d = Html::parse_document(html);
    let mut iris = None;
    let mut pdf = None;
    let links: Vec<scraper::ElementRef> = match d.select(&sel("main")).next() {
        Some(m) => m.select(&sel("a[href]")).collect(),
        None => d.select(&sel("a[href]")).collect(),
    };
    for a in links {
        let Some(u) = common::link(base, a.value().attr("href").unwrap_or_default()) else { continue };
        let Ok(p) = Url::parse(&u) else { continue };
        if iris.is_none() && p.host_str() == Some("iris.paho.org") && (p.path().starts_with("/items/") || p.path().starts_with("/handle/")) {
            iris = Some(u.clone());
        }
        if pdf.is_none() && p.host_str() == Some("www.paho.org") && p.path().starts_with("/sites/default/files/") && p.path().to_lowercase().ends_with(".pdf") {
            pdf = Some(u);
        }
    }
    (iris, pdf)
}

fn card_json(c: &Card, page: &str) -> Value {
    json!({"title":c.title,"categories":c.categories,"date":c.date,"datetime":c.datetime,"card_url":c.url,"listing_page":page})
}

async fn listing_discover(src: &SourceEntry, f: &Fetcher, limit: Option<usize>, start: &str) -> Result<Vec<PlanRow>> {
    let max_pages = src.config["max_listing_pages"].as_u64().unwrap_or(200) as usize;
    let mut next = Some(start.to_string());
    let mut seen_pages = HashSet::new();
    let mut seen = HashSet::new();
    let mut rows = vec![];
    let mut deferred = vec![];
    while let Some(page_url) = next.take() {
        if !seen_pages.insert(page_url.clone()) || seen_pages.len() > max_pages {
            deferred.push(unresolved(src, page_url, format!("PAHO listing pagination stopped (cycle or {max_pages}-page bound)")));
            break;
        }
        let page = f.get_ok(&page_url).await?;
        let base = Url::parse(&page.record.final_url)?;
        let (cards, n) = listing_cards(&page.text(), &base)?;
        next = n;
        for c in cards {
            if !seen.insert(c.url.clone()) {
                continue;
            }
            if !c.categories.split(',').any(|x| x.trim() == "Guidelines") {
                deferred.push(unresolved(src, c.url.clone(), format!("listing card not labelled Guidelines: {:?}", c.categories)));
                continue;
            }
            let listing = card_json(&c, &page_url);
            let host = Url::parse(&c.url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
            let (iris, direct, doc_page) = if host == "iris.paho.org" {
                (Some(c.url.clone()), None, None)
            } else if host == "www.paho.org" {
                match f.get_ok(&c.url).await {
                    Ok(dp) => {
                        let (i, p) = document_targets(&dp.text(), &Url::parse(&dp.record.final_url)?);
                        (i, p, Some(dp.record))
                    }
                    Err(e) => {
                        deferred.push(unresolved(src, c.url.clone(), format!("PAHO document page unavailable: {e:#}")));
                        continue;
                    }
                }
            } else {
                deferred.push(unresolved(src, c.url.clone(), format!("listed under PAHO Guidelines but links a {host} item (not a PAHO IRIS/paho.org publication); WHO items belong to who_publications_iris")));
                continue;
            };
            let key = common::short_hash(&c.url);
            let row = if let Some(item_url) = iris {
                let ip = match f.get_ok(&item_url).await {
                    Ok(x) => x,
                    Err(e) => {
                        deferred.push(unresolved(src, item_url, format!("{e:#}")));
                        continue;
                    }
                };
                let b = Url::parse(&ip.record.final_url)?;
                if b.host_str() != Some("iris.paho.org") {
                    deferred.push(unresolved(src, item_url, "PAHO item redirect crossed host".into()));
                    continue;
                }
                let (title, pdf, date, lang, related, hash) = match item_l(&ip.text(), &b, true) {
                    Ok(x) => x,
                    Err(e) => {
                        deferred.push(unresolved(src, item_url, format!("{e:#}")));
                        continue;
                    }
                };
                let body = match f.get_ok(&pdf).await {
                    Ok(x) if pdf_complete(&x) => x,
                    Ok(_) => {
                        deferred.push(unresolved(src, item_url, "source download is not a complete PDF".into()));
                        continue;
                    }
                    Err(e) => {
                        deferred.push(unresolved(src, item_url, format!("source PDF unavailable: {e:#}")));
                        continue;
                    }
                };
                PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&body.record.sha256[..12]),url:item_url,title:Some(title),rendition:Some("pdf".into()),version:None,extra:json!({"route":"iris","work_key":key,"item_sha256":hash,"pdf_url":pdf,"pdf_sha256":body.record.sha256,"source_date":date,"language":lang,"related":related,"listing":listing,"catalog_fetch":page.record,"document_page_fetch":doc_page})}
            } else if let Some(pdf) = direct {
                let body = match f.get_ok(&pdf).await {
                    Ok(x) if pdf_complete(&x) => x,
                    Ok(_) => {
                        deferred.push(unresolved(src, c.url.clone(), "document page PDF is not a complete PDF".into()));
                        continue;
                    }
                    Err(e) => {
                        deferred.push(unresolved(src, c.url.clone(), format!("document page PDF unavailable: {e:#}")));
                        continue;
                    }
                };
                PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&body.record.sha256[..12]),url:c.url.clone(),title:Some(c.title.clone()),rendition:Some("pdf".into()),version:None,extra:json!({"route":"paho_org_pdf","work_key":key,"pdf_url":pdf,"pdf_sha256":body.record.sha256,"listing":listing,"catalog_fetch":page.record,"document_page_fetch":doc_page})}
            } else {
                deferred.push(unresolved(src, c.url.clone(), "PAHO document page links neither an IRIS item nor a PDF".into()));
                continue;
            };
            rows.push(row);
            if limit.is_some_and(|n| rows.len() >= n) {
                return Ok(rows);
            }
        }
    }
    let rest = limit.map(|n| n.saturating_sub(rows.len())).unwrap_or(usize::MAX);
    rows.extend(deferred.into_iter().take(rest));
    if rows.is_empty() {
        bail!("PAHO Guidelines listing produced no rows");
    }
    Ok(rows)
}

async fn listing_extract(src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
    let listing = &row.extra["listing"];
    let mut fetches = vec![];
    if let Ok(r) = serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()) {
        fetches.push(r)
    }
    let (title, pdf, mut dates, lang, related) = if row.extra["route"] == "iris" {
        let page = f.get_ok(&row.url).await?;
        let base = Url::parse(&page.record.final_url)?;
        if base.host_str() != Some("iris.paho.org") {
            bail!("PAHO item origin changed");
        }
        let (title, pdf, date, lang, related, hash) = item_l(&page.text(), &base, true)?;
        if row.extra["item_sha256"].as_str().is_some_and(|x| x != hash) || row.extra["pdf_url"].as_str().is_some_and(|x| x != pdf) {
            bail!("PAHO item changed since discovery");
        }
        fetches.push(page.record);
        let dates = if date.is_empty() { vec![] } else { vec![common::date("PAHO citation publication date", &date, &row.url)] };
        (title, pdf, dates, lang, related)
    } else {
        let page = f.get_ok(&row.url).await?;
        let (_, pdf) = document_targets(&page.text(), &Url::parse(&page.record.final_url)?);
        if pdf.as_deref() != row.extra["pdf_url"].as_str() {
            bail!("PAHO document page PDF link changed since discovery");
        }
        fetches.push(page.record);
        (row.title.clone().unwrap_or_default(), pdf.context("PAHO document page PDF link missing")?, vec![], String::new(), vec![])
    };
    if let Some(raw) = listing["date"].as_str() {
        let mut d = common::date("paho.org documents listing date", raw, listing["listing_page"].as_str().unwrap_or_default());
        if let Some(dt) = listing["datetime"].as_str().and_then(|x| x.get(..10)) {
            d.normalized = Some(dt.to_string());
        }
        dates.push(d);
    }
    let body = f.get_ok(&pdf).await?;
    if !pdf_complete(&body) {
        bail!("PAHO PDF is incomplete or not a PDF");
    }
    if row.extra["pdf_sha256"].as_str().is_some_and(|x| x != body.record.sha256) {
        bail!("PAHO PDF changed since discovery");
    }
    let (blocks, mut warnings, _) = super::registries_pdf::blocks_for_pdf(&body, 30).await?;
    if blocks.is_empty() {
        bail!("PAHO PDF yielded no content");
    }
    let rights = pdf_rights(&blocks, &pdf);
    warnings.push(format!("Listed in PAHO's document catalog with type(s) \"{}\"; that label covers clinical guidelines and also operational/technical guidance. Generic PDF conversion does not verify physical/printed pages, multicolumn recommendation cells, evidence keys or figure order. Other language items have independent rights.", listing["categories"].as_str().unwrap_or_default()));
    let kind = if ["summarized version", "versión resumida", "versao resumida", "versão resumida"].iter().any(|x| title.to_lowercase().contains(*x)) { "summary" } else { "guideline" };
    let relations = related
        .iter()
        .filter_map(|x| Some(Relation { rel: "other_language_version".into(), target: x["url"].as_str()?.into(), scope: None, evidence_url: Some(row.url.clone()) }))
        .collect();
    fetches.push(body.record);
    Ok(GuidelineDoc {
        source_id: src.id.clone(),
        doc_key: row.doc_key.clone(),
        work_key: row.extra["work_key"].as_str().map(String::from),
        title,
        url: row.url.clone(),
        item_type: Some(kind.into()),
        status: "unknown".into(),
        language: (!lang.is_empty()).then_some(lang),
        date_observations: dates,
        relations,
        rights,
        attachments: related,
        search_metadata: json!({"paho_listing":listing,"pdf_url":pdf}),
        blocks,
        fetches,
        warnings,
        ..Default::default()
    })
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod listing_route {
    use super::*;
    const LISTING: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/paho_iris/paho-org-documents-guidelines-page-0.html"));
    const DOC_PAGE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/paho_iris/paho-org-document-direct-pdf.html"));
    const PDF3: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/paho_iris/rop-summary-page-3.pdf"));
    const START: &str = "https://www.paho.org/en/documents?cat%5B0%5D=14";
    const DOC: &str = "https://www.paho.org/en/documents/guidelines-internacional-transport-biologiacal-materials";

    #[test]
    fn real_guidelines_listing_cards_and_document_page() {
        let (cards, next) = listing_cards(LISTING, &Url::parse(START).unwrap()).unwrap();
        assert!(cards.len() >= 12, "{cards:?}");
        assert!(cards.iter().all(|c| c.categories.contains("Guidelines")));
        assert_eq!(cards[0].url, "https://iris.paho.org/items/3d9061cd-c40d-451c-ac70-73ef0a40d233");
        assert_eq!(cards[0].datetime.as_deref().map(|d| &d[..10]), Some("2026-10-23"));
        assert!(cards.iter().any(|c| c.url == DOC));
        assert!(cards.iter().any(|c| c.url.starts_with("https://iris.who.int/")));
        assert_eq!(next.as_deref(), Some("https://www.paho.org/en/documents?cat%5B0%5D=14&page=1"));
        let (iris, pdf) = document_targets(DOC_PAGE, &Url::parse(DOC).unwrap());
        assert!(iris.is_none());
        assert_eq!(pdf.as_deref(), Some("https://www.paho.org/sites/default/files/2026/10/poplref029-v3guidelines-international-transport-biological-materialseng.pdf"));
    }

    #[tokio::test]
    async fn offline_listing_routes_iris_items_paho_pdfs_and_defers_who_items() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 10 << 20, "").unwrap();
        f.seed_cache_for_tests(START, LISTING.as_bytes(), "text/html; charset=UTF-8");
        f.seed_cache_for_tests(DOC, DOC_PAGE.as_bytes(), "text/html; charset=UTF-8");
        let (_, pdf) = document_targets(DOC_PAGE, &Url::parse(DOC).unwrap());
        // page-3 excerpt stands in for the linked PDF bytes (offline fixture)
        f.seed_cache_for_tests(&pdf.unwrap(), PDF3, "application/pdf");
        let src: SourceEntry = serde_json::from_value(json!({"id":"paho_iris","access":"public","enabled":true,"catalog_url":"https://iris.paho.org/","config":{"listing_url":START,"max_listing_pages":1}})).unwrap();
        let rows = PahoIris.discover(&src, &f, None).await.unwrap();
        let ok = rows.iter().filter(|r| r.extra["discovery_error"].is_null()).collect::<Vec<_>>();
        assert_eq!(ok.len(), 1, "{rows:?}");
        assert_eq!(ok[0].url, DOC);
        assert_eq!(ok[0].extra["route"], "paho_org_pdf");
        assert!(rows.iter().any(|r| r.extra["discovery_error"].as_str().is_some_and(|e| e.contains("who_publications_iris"))));
        let d = PahoIris.extract(&src, &f, ok[0]).await.unwrap();
        assert_eq!(d.title, "Guidelines internacional transport biologiacal materials");
        assert!(d.date_observations.iter().any(|x| x.label == "paho.org documents listing date" && x.normalized.as_deref() == Some("2026-10-15")));
        assert_eq!(d.rights.status, "restricted");
        assert!(!d.blocks.is_empty());
    }
}
