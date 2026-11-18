//! SCCM society recommendation pages; journal full articles are independent links.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct Sccm;
fn links(html:&str,base:&Url)->Result<Vec<String>> {
 let doc=Html::parse_document(html);let mut seen=HashSet::new();let mut out=vec![];
 for a in doc.select(&sel("a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};let u=Url::parse(&url)?;
  if !matches!(u.host_str(),Some("sccm.org"|"www.sccm.org"))||!u.path().starts_with("/clinical-resources/guidelines/guidelines/")||u.query().is_some(){continue;}
  if seen.insert(url.clone()){out.push(url)}
 }if out.is_empty(){bail!("SCCM catalog yielded no society guideline links");}Ok(out)
}
fn content(html:&str,base:&Url)->Result<(String,String,Vec<Block>,Vec<String>,Vec<Value>,Value)> {
 let d=Html::parse_document(html);let title=d.select(&sel("h1")).next().map(|e|text_of(&e)).filter(|s|!s.is_empty()).context("SCCM title missing")?;
 let intro=d.select(&sel("p.GuidelineLink")).next().and_then(|e|e.parent()).and_then(ElementRef::wrap).context("SCCM clinical introduction missing")?;
 let cards=d.select(&sel("div.filterData")).collect::<Vec<_>>();if cards.is_empty(){bail!("SCCM recommendation cards missing; not a complete society recommendation page");}
 let intro_html=intro.html();let mut hash_input=format!("{}{}{}{}",title,intro_html,d.select(&sel(".t-date")).map(|e|e.html()).collect::<String>(),d.select(&sel(".author-info")).map(|e|e.html()).collect::<String>());for card in &cards {hash_input.push_str(&card.html());}let hash=crate::common::sha256_hex(hash_input.as_bytes());
 let (mut blocks,mut warnings)=blocks_from_html(&intro_html,Some(base),None);let mut card_sources=vec![];
 for (i,card) in cards.iter().enumerate(){let body=card.select(&sel(".card-body p")).next().context("SCCM card statement missing")?;let text=text_of(&body);if text.is_empty(){bail!("SCCM card statement empty");}
  let badges=card.select(&sel(".card-footer .GuidelineRecommendation")).map(|e|text_of(&e)).filter(|s|!s.is_empty()).collect::<Vec<_>>();
  let strengths=card.select(&sel(".card-footer .alert-success")).map(|e|text_of(&e)).filter(|s|!s.is_empty()).collect::<Vec<_>>();let strength=if strengths.len()==1{Some(strengths[0].clone())}else{None};
  let remarks=card.select(&sel(".card-body div[id^='Remarks-']")).map(|e|text_of(&e)).filter(|s|!s.is_empty()).collect::<Vec<_>>();
  let id=format!("sccm-card-{}",i+1);blocks.push(Block{block_id:id.clone(),kind:BlockKind::Recommendation{label:None,strength,certainty:None,population:None,grading_system:None,qualifier:(!remarks.is_empty()).then_some(remarks.join(" "))},text,heading_path:vec![title.clone()],page:None});
  card_sources.push(json!({"block_id":id,"source_order":i+1,"badges":badges,"original_html":card.html()}));
 }
 let mut attachments=vec![];for (i,a) in intro.select(&sel("a[href],img[src]")).enumerate(){let attr=if a.value().name()=="img"{"src"}else{"href"};if let Ok(u)=base.join(a.value().attr(attr).unwrap_or_default()){if matches!(u.scheme(),"http"|"https")&&u.username().is_empty()&&u.password().is_none(){let context=a.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().name()=="td").map(|e|text_of(&e)).unwrap_or_default();let role=if attr=="src"{"figure"}else if a.ancestors().filter_map(ElementRef::wrap).any(|e|e.value().classes().any(|c|c=="GuidelineLink")){"journal_full_guideline"}else if context.to_lowercase().contains("executive summary"){"journal_executive_summary"}else{"linked_resource"};attachments.push(json!({"url":u.as_str(),"role":role,"label":text_of(&a),"source_context":context,"source_locator":format!("intro/{i}"),"rights":"unknown; independent asset"}));}}}
 let dates=d.select(&sel(".t-date")).map(|e|text_of(&e)).collect::<Vec<_>>();let authors=d.select(&sel(".author-info")).map(|e|text_of(&e)).collect::<Vec<_>>();warnings.push("Society recommendation view; full journal article, executive summary and their rights are independent and unverified. Certainty text is preserved with each statement rather than assigned to a single intervention.".into());
 Ok((title,hash,blocks,warnings,attachments,json!({"published_labels":dates,"authors":authors,"card_count":cards.len(),"source_card_mappings":card_sources})))
}
fn unresolved(src:&SourceEntry,url:String,e:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:None,rendition:Some("html".into()),version:None,extra:json!({"discovery_error":e})}}
#[async_trait::async_trait] impl Adapter for Sccm {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![])}
  // catalog_url (historic seed page) first, then `config.catalog_urls` (the full guideline listing); one row per path
  let mut catalogs=vec![src.catalog_url.clone().context("SCCM catalog missing")?];catalogs.extend(src.config["catalog_urls"].as_array().into_iter().flatten().filter_map(|u|u.as_str().map(String::from)));
  let mut entries:Vec<(String,Value)>=vec![];let mut paths=HashSet::new();
  for c in &catalogs{let cat=f.get_ok(c).await?;let base=Url::parse(&cat.record.final_url)?;let rec=json!(cat.record);for url in links(&cat.text(),&base)?{if paths.insert(Url::parse(&url)?.path().trim_end_matches('/').to_ascii_lowercase()){entries.push((url,rec.clone()));}}}
  let mut out=vec![];
  for (url,cat_record) in entries{let page=match f.get_ok(&url).await{Ok(x)=>x,Err(e)=>{out.push(unresolved(src,url,format!("{e:#}")));if limit.is_some_and(|n|out.len()>=n){return Ok(out)}continue;}};let b=Url::parse(&page.record.final_url)?;if !matches!(b.host_str(),Some("sccm.org"|"www.sccm.org")){out.push(unresolved(src,url,"cross-host redirect".into()));continue;}
   match content(&page.text(),&b){Ok((title,hash,_,_,_,_))=>{let key=common::short_hash(&url);out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&hash[..12]),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"work_key":key,"clinical_sha256":hash,"catalog_fetch":cat_record})});},Err(e)=>out.push(unresolved(src,url,format!("{e:#}")))}if limit.is_some_and(|n|out.len()>=n){return Ok(out)}
  }Ok(out)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("SCCM discovery unresolved: {e}");}let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;if !matches!(base.host_str(),Some("sccm.org"|"www.sccm.org")){bail!("SCCM redirect crossed host");}
  let (title,hash,blocks,warnings,attachments,metadata)=content(&page.text(),&base)?;if row.extra["clinical_sha256"].as_str().is_some_and(|x|x!=hash){bail!("SCCM recommendation revision changed");}
  let dates=metadata["published_labels"].as_array().into_iter().flatten().filter_map(|x|x.as_str()).map(|x|common::date("Published",x,&row.url)).collect();let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(page.record);
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),item_type:Some("recommendation_collection".into()),status:"unknown".into(),date_observations:dates,rights:Rights{status:"unknown".into(),note:Some("Society page and linked journal or other assets have independent unverified terms.".into()),..Default::default()},search_metadata:metadata,blocks,attachments,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/catalog.html"));
 const A:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/sepsis-2026-1.html"));const B:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/sepsis-2026-2.html"));
 const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/sepsis-2021-1.html"));const D:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/sepsis-2021-2.html"));
 const URL:&str="https://www.sccm.org/clinical-resources/guidelines/guidelines/surviving-sepsis-campaign-international-guidelines-for-management-of-sepsis-and-septic-shock-2026";
 #[test] fn genuine_cards_keep_each_intervention_certainty_and_remarks(){
  let base=Url::parse(URL).unwrap();let links=links(CAT,&base).unwrap();assert!(links.iter().any(|x|x.ends_with("sepsis-and-septic-shock-2026")));
  let h=format!("{A}{B}");let (title,_,blocks,_,assets,m)=content(&h,&base).unwrap();assert!(title.contains("2026"));assert_eq!(m["card_count"],129);assert!(m["published_labels"].to_string().contains("03/23/2026"));
  let rec=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{..})&&b.text.contains("performance improvement program")).unwrap();assert!(rec.text.contains("moderate for screening")&&rec.text.contains("very low for standard operating procedures"));assert!(matches!(&rec.kind,BlockKind::Recommendation{strength:Some(s),qualifier:Some(q),certainty:None,..} if s=="Strong"&&q.contains("vary by setting")));
  assert!(assets.iter().any(|a|a["role"]=="journal_full_guideline" && a["url"].as_str().unwrap_or("").contains("0000000000007075")));assert!(assets.iter().any(|a|a["role"]=="journal_executive_summary" && a["url"].as_str().unwrap_or("").contains("0000000000007089")));
  let old=format!("{C}{D}");let (_,_,_,_,_,m)=content(&old,&base).unwrap();assert_eq!(m["card_count"],93);assert!(content("<h1>Sign in</h1>",&base).is_err());
 }
 #[tokio::test] async fn genuine_item_extract_and_revision_guard(){
  let h=format!("{A}{B}");let url=Url::parse(URL).unwrap();let (_,hash,_,_,_,_)=content(&h,&url).unwrap();let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,2<<20,"").unwrap();f.seed_cache_for_tests(URL,h.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"sccm","access":"public","enabled":true})).unwrap();let mut row=PlanRow{source_id:src.id.clone(),doc_key:"fixture".into(),url:URL.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({"clinical_sha256":hash})};
  let doc=Sccm.extract(&src,&f,&row).await.unwrap();assert_eq!(doc.search_metadata["card_count"],129);row.extra["clinical_sha256"]=json!("changed");assert!(Sccm.extract(&src,&f,&row).await.unwrap_err().to_string().contains("revision changed"));
 }
 #[tokio::test] async fn genuine_catalog_to_emitted_society_page(){
  const X:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/sepsis-2026-nonwww-1.html"));const Y:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/sepsis-2026-nonwww-2.html"));
  let catalog="https://www.sccm.org/clinical-resources/sepsis-resources";let emitted="https://sccm.org/clinical-resources/guidelines/guidelines/surviving-sepsis-campaign-international-guidelines-for-management-of-sepsis-and-septic-shock-2026";
  let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,2<<20,"").unwrap();let body=format!("{X}{Y}");f.seed_cache_for_tests(catalog,CAT.as_bytes(),"text/html");f.seed_cache_for_tests(emitted,body.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"sccm","access":"public","enabled":true,"catalog_url":catalog})).unwrap();let mut rows=Sccm.discover(&src,&f,Some(1)).await.unwrap();let mut row=rows.pop().unwrap();assert_eq!(row.url,emitted);let doc=Sccm.extract(&src,&f,&row).await.unwrap();assert_eq!(doc.search_metadata["card_count"],129);assert_eq!(doc.fetches.len(),2);
  row.extra["clinical_sha256"]=json!("changed");assert!(Sccm.extract(&src,&f,&row).await.unwrap_err().to_string().contains("revision changed"));
 }

}

#[cfg(all(test, feature = "guideline-fixtures"))] mod full_listing {
 use super::*;
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/catalog.html"));
 const LIST:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/guidelines-listing.html"));
 const HEAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sccm/heat-stroke.html"));
 const SEPSIS:&str="https://www.sccm.org/clinical-resources/sepsis-resources";
 const LISTING:&str="https://www.sccm.org/clinical-resources/guidelines/guidelines";
 #[test] fn listing_has_every_guideline_page_and_non_sepsis_pages_have_cards(){
  let l=links(LIST,&Url::parse(LISTING).unwrap()).unwrap();assert_eq!(l.len(),42);
  assert!(l.iter().any(|u|u.ends_with("/guideline-for-the-treatment-of-heat-stroke")));
  let u="https://www.sccm.org/clinical-resources/guidelines/guidelines/guideline-for-the-treatment-of-heat-stroke";
  let (title,_,blocks,_,assets,m)=content(HEAT,&Url::parse(u).unwrap()).unwrap();assert_eq!(title,"Guideline for the Treatment of Heat Stroke");
  assert!(m["card_count"].as_u64().unwrap()>=7);assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{strength:Some(s),..} if s=="Strong")&&b.text.contains("active cooling")));
  assert!(assets.iter().any(|a|a["url"].as_str().unwrap_or("").contains("CCM.0000000000006551")));
 }
 #[tokio::test] async fn config_listing_extends_seed_catalog_without_duplicates_or_key_changes(){
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,4<<20,"").unwrap();
  f.seed_cache_for_tests(SEPSIS,CAT.as_bytes(),"text/html");f.seed_cache_for_tests(LISTING,LIST.as_bytes(),"text/html");
  let heat="https://www.sccm.org/clinical-resources/guidelines/guidelines/guideline-for-the-treatment-of-heat-stroke";f.seed_cache_for_tests(heat,HEAT.as_bytes(),"text/html");
  let mut src:SourceEntry=serde_json::from_value(json!({"id":"sccm","access":"public","enabled":true,"catalog_url":SEPSIS})).unwrap();
  let seed=Sccm.discover(&src,&f,None).await.unwrap();
  src.config=json!({"catalog_urls":[LISTING]});
  let rows=Sccm.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),42);
  // the seed page's rows come first with their original URL form, hence their original doc_keys
  for (a,b) in seed.iter().zip(&rows){assert_eq!(a.url,b.url);assert_eq!(a.doc_key,b.doc_key);}
  let paths:HashSet<_>=rows.iter().map(|r|Url::parse(&r.url).unwrap().path().to_string()).collect();assert_eq!(paths.len(),42);
  let h=rows.iter().find(|r|r.url==heat).unwrap();assert!(h.extra["discovery_error"].is_null()&&!h.doc_key.ends_with("unresolved"));
  assert_eq!(h.extra["catalog_fetch"]["url"],LISTING);
  let d=Sccm.extract(&src,&f,h).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("recommendation_collection"));assert!(d.blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{..})));
  assert_eq!(Sccm.discover(&src,&f,Some(3)).await.unwrap().len(),3);
 }
}
