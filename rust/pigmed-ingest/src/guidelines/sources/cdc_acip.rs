//! ACIP vaccine-specific index -> linked topic page -> MMWR recommendation articles.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct CdcAcip;
fn topics(html:&str,base:&Url)->Result<Vec<(String,String)>> {
 let doc=Html::parse_document(html);let root=doc.select(&sel("main")).next().context("ACIP index main missing")?;let mut seen=HashSet::new();let mut rows=vec![];
 for a in root.select(&sel("a[href]")) {let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};let u=Url::parse(&url)?;
  if u.origin()==base.origin()&&u.path().starts_with("/acip-recs/hcp/vaccine-specific/")&&u.path().ends_with(".html")&&!u.path().ends_with("/index.html")&&seen.insert(url.clone()){rows.push((url,text_of(&a)));}}
 if rows.is_empty(){bail!("ACIP topic index empty");}Ok(rows)
}
fn articles(html:&str,base:&Url)->Result<Vec<(String,String)>> {
 let doc=Html::parse_document(html);let root=doc.select(&sel("main")).next().context("ACIP topic main missing")?;let mut seen=HashSet::new();let mut rows=vec![];
 for a in root.select(&sel("a[href]")) {let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};let u=Url::parse(&url)?;
  if u.origin()==base.origin() && (u.path().starts_with("/mmwr/") || u.path().starts_with("/acip/evidence-to-recommendations/") || u.path().starts_with("/vaccines/acip/recs/grade/")) && (u.path().ends_with(".htm")||u.path().ends_with(".html")) && seen.insert(url.clone()){rows.push((url,text_of(&a)));}}
 if rows.is_empty(){bail!("ACIP topic has no source-linked MMWR HTML articles");}Ok(rows)
}
fn clinical_target(base:&Url,href:&str)->Option<String>{let url=base.join(href.trim()).ok()?;(matches!(url.scheme(),"http"|"https")&&url.username().is_empty()&&url.password().is_none()).then(||url.to_string())}
fn content_root(doc:&Html)->Result<(ElementRef<'_>,&'static str)>{
 for (selector,role) in [("div.content.content-fullwidth","modern_fullwidth"),("div.mSyndicate","legacy_syndicate"),("div#content-main > .main-inner","legacy_content_main")]{
  let roots=doc.select(&sel(selector)).collect::<Vec<_>>();if roots.len()>1{bail!("MMWR ambiguous article content root {selector}");}if let Some(root)=roots.first(){return Ok((*root,role));}
 }
 // 2000/2002 reports predate div templates: their named content anchor
 // identifies the report's dedicated table cell, not the whole layout table.
 if doc.select(&sel("meta[name='Date'][content]")).next().is_some()&&doc.select(&sel("meta[name='MMWR_Type'][content]")).next().is_some(){
  // HTML5 parsing can reconstruct misnested legacy anchors more than once.
  // All occurrences must still identify one dedicated report cell.
  let mut cells:Vec<ElementRef<'_>>=vec![];
  for anchor in doc.select(&sel("a[name='content_area']")) {
   if let Some(cell)=anchor.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().name()=="td"&&e.value().attr("valign")==Some("top")&&e.value().attr("width")==Some("100%")) {
    if cell.select(&sel("h1")).any(|h|!text_of(&h).is_empty())&&!cells.iter().any(|existing|existing.id()==cell.id()){cells.push(cell);}
   }
  }
  if cells.len()==1{return Ok((cells[0],"legacy_named_report_cell"));}
 }bail!("MMWR article content root missing")
}
fn publication(dates:&[DateObservation],warnings:&mut Vec<String>)->Option<String>{
 let printed=dates.iter().find(|d|d.label=="MMWR issue date").and_then(|d|d.normalized.clone());
 let online=dates.iter().find(|d|d.label=="cdc:first_published").or_else(||dates.iter().find(|d|d.label=="article:published_time")).and_then(|d|d.normalized.clone());
 let year=|s:&str|s.get(..4).and_then(|s|s.parse::<i32>().ok());
 if let (Some(a),Some(b))=(printed.as_deref().and_then(year),online.as_deref().and_then(year)){
  if (a-b).abs()>1{warnings.push("Source printed MMWR dateline conflicts with explicit online publication metadata by more than a year; both observations are retained and the primary publication date is unresolved. No date is inferred from the URL or title.".into());return None;}
 }
 printed.or_else(||dates.iter().find(|d|d.label=="Date").and_then(|d|d.normalized.clone())).or(online)
}
fn expand_source_tables(html:&str,blocks:&mut [Block])->Result<()>{
 let doc=Html::parse_fragment(html);let positions=blocks.iter().enumerate().filter(|(_,b)|matches!(b.kind,BlockKind::Table{..})).map(|(i,_)|i).collect::<Vec<_>>();
 let source=doc.select(&sel("#mmwr-content table")).collect::<Vec<_>>();if source.len()!=positions.len(){bail!("MMWR source/extracted table count mismatch");}
 for (i,table) in source.into_iter().enumerate(){
  let span=|c:ElementRef<'_>,a:&str|->Result<usize>{let n=c.value().attr(a).map(str::parse::<usize>).transpose()?.unwrap_or(1);if n==0||n>200{bail!("MMWR invalid table span");}Ok(n)};
  let rows=table.select(&sel("tr")).collect::<Vec<_>>();let mut width=0;for row in &rows{let mut n=0;for c in common::cells(*row){n+=span(c,"colspan")?;}width=width.max(n);}if width==0||width>64{bail!("MMWR table width outside validated bound");}
  let mut carry:Vec<Option<(String,usize)>>=vec![None;width];let mut grid=vec![];
  for row in rows{let mut cells:Vec<Option<String>>=vec![None;width];for j in 0..width{if let Some((t,n))=carry[j].clone(){cells[j]=Some(t.clone());carry[j]=if n>1{Some((t,n-1))}else{None};}}
   let mut col=0;for c in common::cells(row){while col<width&&cells[col].is_some(){col+=1;}let cs=span(c,"colspan")?;let rs=span(c,"rowspan")?;if col+cs>width{bail!("MMWR carried table cells exceed declared width");}let t=text_of(&c).replace('|',"\\|");for j in col..col+cs{if cells[j].is_some(){bail!("MMWR table spans overlap");}cells[j]=Some(t.clone());if rs>1{carry[j]=Some((t.clone(),rs-1));}}col+=cs;}
   grid.push(cells.into_iter().map(|v|v.unwrap_or_default()).collect::<Vec<_>>());
  }if carry.iter().any(Option::is_some){bail!("MMWR rowspan exceeds observed rows");}
  let mut lines=vec![];for (r,cells) in grid.into_iter().enumerate(){lines.push(format!("| {} |",cells.join(" | ")));if r==0{lines.push(format!("| {} |",vec!["---";width].join(" | ")));}}blocks[positions[i]].text=lines.join("\n");
 }Ok(())
}
fn content(html:&str,base:&Url)->Result<(String,String,Vec<DateObservation>,Vec<Block>,Vec<String>,Vec<serde_json::Value>,serde_json::Value)> {
 let doc=Html::parse_document(html);
 let (root,template)=content_root(&doc)?;
 if root.select(&sel("input[type=password],form[action*='login'],iframe[src*='captcha']")).next().is_some(){bail!("MMWR access gate");}
 let title=root.select(&sel("h1")).next().map(|h|text_of(&h)).context("MMWR article title missing")?;
 let role_evidence=root.select(&sel("h1,h2,h3,h4")).map(|h|text_of(&h)).filter(|t|t.to_lowercase().contains("recommendation")||t=="Indications and Guidance for Use").collect::<Vec<_>>();
 if role_evidence.is_empty()||!text_of(&root).contains("Advisory Committee on Immunization Practices"){bail!("MMWR recommendation content missing");}
 if !root.select(&sel("h2,h3,h4")).any(|h|text_of(&h)=="References"){bail!("MMWR full report references heading missing");}
 let mut dates=vec![];for name in ["Date","article:published_time","cdc:first_published","cdc:last_updated","cdc:last_reviewed"] {
  for m in doc.select(&sel(&format!("meta[name='{name}'][content],meta[property='{name}'][content]"))) {dates.push(common::date(name,m.value().attr("content").unwrap(),base.as_str()));}
 }
 if let Some(d)=root.select(&sel(".dateline p")).next(){let raw=text_of(&d);if let Some(part)=raw.split('/').nth(1){dates.push(common::date("MMWR issue date",part.trim(),base.as_str()));}}
 let mut clean=root.inner_html();for e in root.select(&sel("script,style,.title-bar,.tp-related-pages,[data-mmwr-metrics],.no-syndicate,.col-md-3.cr,#archivedbox,img[src*='spacer.gif'],img[src*='arrow_up.gif']")){clean=clean.replacen(&e.html(),"",1);}
 for p in root.select(&sel("p.text-right")){if text_of(&p)=="Top"{clean=clean.replacen(&p.html(),"",1);}}
 // The oldest notice wraps the agency trade-name disclaimer in a one-cell
 // presentation table. Preserve the notice as prose, not a clinical data grid.
 for t in root.select(&sel("table")){let text=text_of(&t);if text.starts_with("Use of trade names and commercial sources is for identification only")&&text.contains("References to non-CDC sites"){clean=clean.replacen(&t.html(),&format!("<p class='mmwr-agency-notice'>{}</p>",text.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;")),1);}}
 for a in root.select(&sel("a[href]")){if matches!(a.value().attr("href"),Some("#top"|"#"))&&matches!(text_of(&a).as_str(),"Return to top."|"Return to top"|"Top"){clean=clean.replacen(&a.html(),"",1);}}
 let scoped=Html::parse_fragment(&clean);
 let links=scoped.select(&sel("a[href]")).map(|a|json!({"label":text_of(&a),"raw_href":a.value().attr("href"),"url":a.value().attr("href").and_then(|h|clinical_target(base,h)),"source_id":a.value().id(),"source_name":a.value().attr("name")})).collect::<Vec<_>>();
 let locators=scoped.select(&sel("[id],a[name]")).map(|e|json!({"id":e.value().id(),"name":e.value().attr("name"),"tag":e.value().name()})).collect::<Vec<_>>();
 let tables=scoped.select(&sel("table")).map(|t|json!({"source_id":t.value().id(),"source_html":t.html(),"caption":t.select(&sel("caption")).next().map(|e|text_of(&e)),"scope":"source table markup retains native spans, labels and footnotes"})).collect::<Vec<_>>();
 let wrapped=format!("<div id='mmwr-content'>{clean}</div>");let hash=crate::common::sha256_hex(serde_json::to_string(&(&title,&wrapped,&dates))?.as_bytes());
 let (mut blocks,mut warnings)=blocks_from_html(&wrapped,Some(base),Some("#mmwr-content"));expand_source_tables(&wrapped,&mut blocks)?;
 // Source gives distinct titled paragraphs within the 2019 Recommendations section. Preserve each whole paragraph;
 // do not project a grade mentioned for a subgroup onto the whole statement.
 let mut rows=vec![];for (i,b) in blocks.iter().enumerate() {
  if matches!(b.kind,BlockKind::Paragraph)&&b.heading_path.last().is_some_and(|h|h=="Recommendations") && ["Children and adults aged 9 through 26 years.","Adults aged >26 years.","Administration.","Cervical cancer screening.","Special populations and medical conditions."].iter().any(|p|b.text.starts_with(p)) {
   rows.push((i,Block{block_id:format!("{}-recommendation",b.block_id),kind:BlockKind::Recommendation{label:None,strength:None,certainty:None,population:None,grading_system:None,qualifier:None},text:b.text.clone(),heading_path:b.heading_path.clone(),page:b.page}));
  }
 }
 for (i,b) in rows.into_iter().rev(){blocks.insert(i+1,b);}
 warnings.push("Older MMWR recommendations and subgroup-specific evidence ratings remain in source blocks without inferred whole-statement grades; review each template, figure, superscript and table span.".into());
 let mut attachments=vec![];let mut seen=HashSet::new();
 for a in root.select(&sel("a[href]")){if let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default()){let target=a.value().attr("href").and_then(|h|clinical_target(base,h));if Url::parse(&u)?.path().to_lowercase().ends_with(".pdf")&&target.as_ref().is_some_and(|t|seen.insert(t.clone())){attachments.push(json!({"url":target,"fetch_identity_url":u,"label":text_of(&a),"role":"linked_pdf","rights":"unknown; separate asset"}));}}}
 Ok((title,hash,dates,blocks,warnings,attachments,json!({"source_template":template,"clinical_role_evidence":role_evidence,"clinical_links":links,"source_locators":locators,"tables":tables,"dates_scope":"printed MMWR issue dateline and online/page metadata are independent observations"})))
}
fn unresolved(src:&SourceEntry,url:String,title:String,e:String)->PlanRow {PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"discovery_error":e})}}
#[async_trait::async_trait]
impl Adapter for CdcAcip {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let index=f.get_ok(src.catalog_url.as_deref().context("ACIP index missing")?).await?;let base=Url::parse(&index.record.final_url)?;let mut rows=vec![];let mut seen=HashSet::new();
  for (topic,label) in topics(&index.text(),&base)? {
   let page=match f.get_ok(&topic).await {Ok(p)=>p,Err(e)=>{rows.push(unresolved(src,topic,label,format!("{e:#}")));if limit.is_some_and(|n|rows.len()>=n){break;}continue;}};
   let topicbase=Url::parse(&page.record.final_url)?;if topicbase.origin()!=base.origin(){rows.push(unresolved(src,topic,label,"topic redirect crossed origin".into()));continue;}
   let candidates=match articles(&page.text(),&topicbase){Ok(v)=>v,Err(e)=>{rows.push(unresolved(src,topic,label,format!("{e:#}")));if limit.is_some_and(|n|rows.len()>=n){break;}continue;}};
   for (url,title) in candidates {if !seen.insert(url.clone()){continue;}
    if !Url::parse(&url)?.path().starts_with("/mmwr/") {rows.push(unresolved(src,url,title,"ACIP evidence framework is a separate source role; its clinical template has not been mapped".into()));if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}continue;}
    let p=match f.get_ok(&url).await {Ok(p)=>p,Err(e)=>{rows.push(unresolved(src,url,title,format!("{e:#}")));if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}continue;}};
    let articlebase=Url::parse(&p.record.final_url)?;if articlebase.origin()!=base.origin(){rows.push(unresolved(src,url,title,"article redirect crossed origin".into()));continue;}
    match content(&p.text(),&articlebase) {Ok((title,hash,_,_,_,_,_))=>{let key=common::short_hash(&url);rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&hash[..12]),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"work_key":key,"clinical_sha256":hash,"topic_url":topic,"index_fetch":index.record,"topic_fetch":page.record})});},Err(e)=>rows.push(unresolved(src,url,title,format!("{e:#}")))}
    if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
   }
  }Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("ACIP discovery unresolved: {e}");}let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;if base.origin()!=Url::parse(&row.url)?.origin(){bail!("MMWR redirect crossed origin");}
  let (title,hash,dates,blocks,mut warnings,attachments,metadata)=content(&page.text(),&base)?;if row.extra["clinical_sha256"].as_str().is_some_and(|s|s!=hash){bail!("MMWR content changed since discovery");}
  let mut fetches=vec![];for field in ["index_fetch","topic_fetch"]{if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra[field].clone()){fetches.push(r);}}fetches.push(page.record);
  let published=publication(&dates,&mut warnings);
  warnings.push("MMWR article status and item/third-party rights require independent review; topic listing may link evidence frameworks and other roles not yet mapped.".into());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),item_type:Some("recommendation_report".into()),title,url:base.to_string(),status:"unknown".into(),published,date_observations:dates,rights:Rights{status:"unknown".into(),note:Some("CDC agency policy and item-specific/third-party exceptions have not been verified for this article.".into()),..Default::default()},blocks,attachments,search_metadata:metadata,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/catalog.html"));
 const NEW:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mmwr-2019.html"));
 const OLD1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mmwr-2014-part-1.html"));
 const OLD2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mmwr-2014-part-2.html"));
 #[test] fn actual_index_and_two_mmwr_layouts() {
  let base=Url::parse("https://www.cdc.gov/acip-recs/hcp/vaccine-specific/index.html").unwrap();assert!(topics(CAT,&base).unwrap().iter().any(|(u,_)|u.ends_with("/hpv.html")));
  let url=Url::parse("https://www.cdc.gov/mmwr/volumes/68/wr/mm6832a3.htm").unwrap();let (_,_,dates,blocks,_,_,_)=content(NEW,&url).unwrap();
  assert!(dates.iter().any(|d|d.label=="MMWR issue date"&&d.normalized.as_deref()==Some("2019-08-16")));
  assert!(blocks.iter().any(|b|b.text.contains("shared clinical decision-making")));assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})));
  let old=Url::parse("https://www.cdc.gov/mmwr/preview/mmwrhtml/rr6305a1.htm").unwrap();let old_html=format!("{OLD1}{OLD2}");let (_,_,dates,blocks,_,_,_)=content(&old_html,&old).unwrap();
  assert!(dates.iter().any(|d|d.label=="Date"&&d.normalized.as_deref()==Some("2014-08-29")));
  assert!(blocks.iter().any(|b|b.text.contains("male GRADE recommendation category")));assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),10);
  assert!(content("<main>Sign in</main>",&url).is_err());
 }
 #[tokio::test] async fn content_revision_guard_fails_closed() {
  let url="https://www.cdc.gov/mmwr/volumes/68/wr/mm6832a3.htm";let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,10<<20,"").unwrap();
  f.seed_cache_for_tests(url,NEW.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"cdc_acip","access":"public","enabled":true})).unwrap();
  let row=PlanRow{source_id:src.id.clone(),doc_key:"guard".into(),url:url.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({"clinical_sha256":"changed"})};
  assert!(CdcAcip.extract(&src,&f,&row).await.unwrap_err().to_string().contains("changed since discovery"));
 }

}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod live_tests{
 use super::*;
 fn fixture(name:&str)->String{match name{
 "rr5906a1"=>[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/rr5906a1.part-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/rr5906a1.part-2.html"))].concat(),
 "rr6804a1"=>[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/rr6804a1.part-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/rr6804a1.part-2.html"))].concat(),
 "rr4915a1"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/rr4915a1.html")).into(),
 "mm5145a4"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm5145a4.html")).into(),
 "mm6905a5"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm6905a5.html")).into(),
 "mm7145a2"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm7145a2.html")).into(),
 "mm7103a4"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm7103a4.html")).into(),
 "mm7045e1"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm7045e1.html")).into(),
 "mm7044e2"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm7044e2.html")).into(),_=>panic!("unknown fixture")}}
 fn source()->SourceEntry{serde_json::from_value(json!({"id":"cdc_acip","enabled":true,"access":"public","catalog_url":"https://www.cdc.gov/acip-recs/hcp/vaccine-specific/index.html"})).unwrap()}
 #[tokio::test]
 async fn actual_anthrax_topic_three_legacy_generations_and_conflicting_dates(){
  const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/catalog.html"));
  const TOPIC:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/anthrax-topic.html"));
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();
  f.seed_cache_for_tests("https://www.cdc.gov/acip-recs/hcp/vaccine-specific/index.html",CAT.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.cdc.gov/acip-recs/hcp/vaccine-specific/anthrax.html",TOPIC.as_bytes(),"text/html");
  let latest=fixture("rr6804a1");let earlier=fixture("rr5906a1");let notice=fixture("mm5145a4");
  for(u,h)in[("https://www.cdc.gov/mmwr/volumes/68/rr/rr6804a1.htm",latest.as_str()),("https://www.cdc.gov/mmwr/preview/mmwrhtml/rr5906a1.htm",earlier.as_str()),("https://www.cdc.gov/mmwr/preview/mmwrhtml/mm5145a4.htm",notice.as_str())]{f.seed_cache_for_tests(u,h.as_bytes(),"text/html");}
  let rows=CdcAcip.discover(&source(),&f,Some(3)).await.unwrap();assert_eq!(rows.len(),3);assert!(rows.iter().all(|r|r.extra["discovery_error"].is_null()));
  let src=source();
  let docs=futures::future::join_all(rows.iter().map(|r|CdcAcip.extract(&src,&f,r))).await.into_iter().map(Result::unwrap).collect::<Vec<_>>();
  let current=docs.iter().find(|d|d.url.ends_with("rr6804a1.htm")).unwrap();assert!(current.published.is_none());
  assert!(current.date_observations.iter().any(|d|d.label=="MMWR issue date"&&d.raw=="July 2, 1950"&&d.normalized.as_deref()==Some("1950-07-02")));
  assert!(current.date_observations.iter().any(|d|d.label=="cdc:first_published"&&d.normalized.as_deref()==Some("2019-12-12")));
  assert!(current.warnings.iter().any(|w|w.contains("dateline conflicts")));assert_eq!(current.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),5);
  assert!(current.blocks.iter().all(|b|b.text!="Print"&&b.text!="On This Page"&&b.text!="Top"));
  let old=docs.iter().find(|d|d.url.ends_with("rr5906a1.htm")).unwrap();assert_eq!(old.search_metadata["source_template"],"legacy_content_main");assert_eq!(old.published.as_deref(),Some("2010-07-23"));assert_eq!(old.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),4);
  assert!(old.blocks.iter().any(|b|b.text.contains("persons with repeated occupational exposures")||matches!(&b.kind,BlockKind::Table{caption:Some(c)} if c.contains("persons with repeated occupational exposures"))));
  let notice=docs.iter().find(|d|d.url.ends_with("mm5145a4.htm")).unwrap();assert_eq!(notice.search_metadata["source_template"],"legacy_named_report_cell");assert_eq!(notice.published.as_deref(),Some("2002-11-15"));
  assert_eq!(notice.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..}|BlockKind::Figure{..})).count(),0);
  assert!(notice.blocks.iter().any(|b|b.text.contains("Brachman PS, Gold H, Plotkin SA")));assert!(notice.blocks.iter().any(|b|b.text.contains("antimicrobial PEP regimens")));
  assert!(notice.search_metadata["clinical_links"].as_array().unwrap().iter().any(|l|l["url"]=="https://www.cdc.gov/mmwr/preview/mmwrhtml/rr4915a1.htm"));
 }
 #[tokio::test]
 async fn all_five_live_modern_failures_preserve_full_tables_and_independent_conditions(){
  for (name,path,table_count,tail) in [
   ("mm6905a5","69/wr",1,"Schillie S, Vellozzi C, Reingold A"),
   ("mm7145a2","71/wr",1,"unable to receive an mRNA vaccine"),
   ("mm7103a4","71/wr",2,"Estimated COVID-19 hospitalizations prevented"),
   ("mm7045e1","70/wr",1,"Diluent"),
   ("mm7044e2","70/wr",1,"benefits of vaccination outweigh the risks of GBS and TTS")]{
   let raw=fixture(name);let url=format!("https://www.cdc.gov/mmwr/volumes/{path}/{name}.htm");let base=Url::parse(&url).unwrap();
   // These genuine reports have no Recommendations h2/h3/h4. The source title
   // or explicit Indications and Guidance for Use identifies the report role.
   let parsed=Html::parse_document(&raw);let (root,_)=content_root(&parsed).unwrap();assert!(!root.select(&sel("h2,h3,h4")).any(|h|text_of(&h).to_lowercase().contains("recommendation")));
   let (_,hash,_,_,_,_,_)=content(&raw,&base).unwrap();let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(&url,raw.as_bytes(),"text/html");
   let row=PlanRow{source_id:"cdc_acip".into(),doc_key:name.into(),url:url.clone(),title:None,rendition:Some("html".into()),version:None,extra:json!({"clinical_sha256":hash})};
   let d=CdcAcip.extract(&source(),&f,&row).await.unwrap();assert_eq!(d.search_metadata["source_template"],"modern_fullwidth");assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),table_count);
   assert!(d.blocks.iter().any(|b|b.text.contains(tail)||matches!(&b.kind,BlockKind::Table{caption:Some(c)} if c.contains(tail))),"{name}: missing clinical/reference tail {tail}");assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{strength:Some(_),..})));
   if name=="mm6905a5"{assert!(d.search_metadata["clinical_links"].as_array().unwrap().iter().any(|l|l["url"]=="http://www.ncbi.nlm.nih.gov/pubmed/29939980"));}
   if name=="mm7145a2"{let table=d.blocks.iter().find(|b|matches!(b.kind,BlockKind::Table{..})).unwrap();assert_eq!(table.text.lines().filter(|l|l.starts_with("| 6 mos–4 yrs |")).count(),2);assert!(table.text.contains("3-dose Moderna (0, 4, 8 wks)"));assert!(d.blocks.iter().any(|b|b.text.contains("Novavax monovalent booster is authorized ≥6 months")));}
   assert!(!d.search_metadata["clinical_links"].as_array().unwrap().is_empty());assert!(content(&raw.replace("content-fullwidth","missing-clinical-root"),&base).is_err());
  }
 }
 #[test]
 fn genuine_2000_image_tables_and_strict_named_cell_boundary(){
  let raw=fixture("rr4915a1");let base=Url::parse("https://www.cdc.gov/mmwr/preview/mmwrhtml/rr4915a1.htm").unwrap();let (_,_,dates,blocks,_,_,metadata)=content(&raw,&base).unwrap();
  assert_eq!(metadata["source_template"],"legacy_named_report_cell");assert!(dates.iter().any(|d|d.label=="Date"&&d.normalized.as_deref()==Some("2000-12-15")));
  assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),3);assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),0);
  for n in 1..=3{assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u==&format!("https://www.cdc.gov/mmwr/preview/mmwrhtml/figures/r915a1t{n}.gif"))));}
  assert!(blocks.iter().any(|b|b.text.contains("Postexposure prophylaxis against experimental inhalation anthrax")));assert!(blocks.iter().any(|b|b.text.contains("LD50=a lethal dose of 50%")));
  assert!(blocks.iter().all(|b|!b.text.contains("Skip Navigation Links")&&!b.text.contains("CDC Home")));
  assert!(content(&raw.replace(r#"name="content_area""#,r#"name="missing_content_area""#),&base).is_err());
 }

 #[test]
 fn genuine_live_response_checksums(){
  {let provenance:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/rr5906a1.source.json"))).unwrap();let raw=fixture("rr5906a1");assert_eq!(crate::common::sha256_hex(raw.as_bytes()),provenance["sha256"].as_str().unwrap());assert_eq!(raw.len()as u64,provenance["bytes"].as_u64().unwrap());}
  {let provenance:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm5145a4.source.json"))).unwrap();let raw=fixture("mm5145a4");assert_eq!(crate::common::sha256_hex(raw.as_bytes()),provenance["sha256"].as_str().unwrap());assert_eq!(raw.len()as u64,provenance["bytes"].as_u64().unwrap());}
  {let provenance:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/rr4915a1.source.json"))).unwrap();let raw=fixture("rr4915a1");assert_eq!(crate::common::sha256_hex(raw.as_bytes()),provenance["sha256"].as_str().unwrap());assert_eq!(raw.len()as u64,provenance["bytes"].as_u64().unwrap());}
  {let provenance:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm6905a5.source.json"))).unwrap();let raw=fixture("mm6905a5");assert_eq!(crate::common::sha256_hex(raw.as_bytes()),provenance["sha256"].as_str().unwrap());assert_eq!(raw.len()as u64,provenance["bytes"].as_u64().unwrap());}
  {let provenance:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm7145a2.source.json"))).unwrap();let raw=fixture("mm7145a2");assert_eq!(crate::common::sha256_hex(raw.as_bytes()),provenance["sha256"].as_str().unwrap());assert_eq!(raw.len()as u64,provenance["bytes"].as_u64().unwrap());}
  {let provenance:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm7103a4.source.json"))).unwrap();let raw=fixture("mm7103a4");assert_eq!(crate::common::sha256_hex(raw.as_bytes()),provenance["sha256"].as_str().unwrap());assert_eq!(raw.len()as u64,provenance["bytes"].as_u64().unwrap());}
  {let provenance:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm7045e1.source.json"))).unwrap();let raw=fixture("mm7045e1");assert_eq!(crate::common::sha256_hex(raw.as_bytes()),provenance["sha256"].as_str().unwrap());assert_eq!(raw.len()as u64,provenance["bytes"].as_u64().unwrap());}
  {let provenance:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/mm7044e2.source.json"))).unwrap();let raw=fixture("mm7044e2");assert_eq!(crate::common::sha256_hex(raw.as_bytes()),provenance["sha256"].as_str().unwrap());assert_eq!(raw.len()as u64,provenance["bytes"].as_u64().unwrap());}
  {let provenance:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_acip/rr6804a1.source.json"))).unwrap();let raw=fixture("rr6804a1");assert_eq!(crate::common::sha256_hex(raw.as_bytes()),provenance["sha256"].as_str().unwrap());assert_eq!(raw.len()as u64,provenance["bytes"].as_u64().unwrap());}
 }
}
