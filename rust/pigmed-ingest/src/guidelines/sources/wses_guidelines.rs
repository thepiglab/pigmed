//! Society source inventory -> original articles; executive roles and PDF fidelity remain explicit.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use super::{pmc_crosswalk as cw,r2a_crosswalk as r2a};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,FetchRecord,GuidelineDoc};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashMap;
use url::Url;
pub struct WsesGuidelines;
fn catalog_role(title:&str)->&'static str{let title=title.to_lowercase();if title.contains("grade tool"){"grading_methods_tool"}else if title.contains("chinese version"){"translated_guideline_compilation"}else if title.contains("booklet"){"guideline_compilation"}else{"source_guideline_article"}}
/// G2-scale-b: WSES guidelines are World Journal of Emergency Surgery (BMC) articles with PMC copies.
/// Record the identifiers for the shared DOI -> PMCID -> OA JATS crosswalk; nothing is fetched or substituted here.
/// Identifiers come only from (a) the source-emitted URL itself (BMC DOI path, PMC article/PDF path) or
/// (b) the registry's `config.oa_title_crosswalk`, whose entries were returned by the Europe PMC REST API for the
/// source's exact printed title (evidence kept per entry). The JATS title must still be matched in round 2.
fn norm_title(t:&str)->String{t.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(|c:char|c=='.'||c==' ').to_lowercase()}
fn oa_crosswalk(src:&SourceEntry,url:&str,title:&str,missing:bool)->Value{
 if !missing{
  let doi=regex::Regex::new(r"^https?://wjes\.biomedcentral\.com/articles/(10\.1186/s13017-[0-9a-z-]+)/?$").unwrap();
  let pmc=regex::Regex::new(r"^https?://(?:pmc\.ncbi\.nlm\.nih\.gov|www\.ncbi\.nlm\.nih\.gov/pmc)/articles/(PMC\d+)(?:/|$)").unwrap();
  if let Some(c)=doi.captures(url){return json!({"doi":c[1].to_string(),"id_source":"source_emitted_url","source_url":url,"status":"pending_pmc_crosswalk"});}
  if let Some(c)=pmc.captures(url){return json!({"pmcid":c[1].to_string(),"id_source":"source_emitted_url","source_url":url,"status":"pending_pmc_crosswalk"});}
  return Value::Null;
 }
 let key=norm_title(title);
 match src.config["oa_title_crosswalk"].as_object().and_then(|m|m.iter().find(|(k,_)|norm_title(k)==key)){
  Some((_,v))=>{let mut v=v.clone();v["id_source"]=json!("registry_config_from_europepmc_title_search");v["status"]=json!("pending_pmc_crosswalk");v}
  None=>Value::Null,
 }
}
fn push(src:&SourceEntry,rows:&mut Vec<PlanRow>,seen:&mut HashMap<String,usize>,url:String,title:String,mention:Value,missing:bool){
 let role=catalog_role(&title);if !missing{if let Some(i)=seen.get(&url){rows[*i].extra["catalog_mentions"].as_array_mut().unwrap().push(mention);return;}seen.insert(url.clone(),rows.len());}let work=if missing{format!("unlinked-{}",common::slug(&title))}else{format!("source-{}",common::short_hash(&url))};let pdf=Url::parse(&url).is_ok_and(|u|u.path().to_lowercase().ends_with(".pdf"));let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url,title:Some(title),rendition:if missing{None}else{Some(if pdf{"pdf"}else{"html"}.into())},version:None,extra:json!({"work_key":work,"source_catalog_role":role,"catalog_mentions":[mention]})};let oa=oa_crosswalk(src,&row.url,row.title.as_deref().unwrap_or_default(),missing);if !oa.is_null(){row.extra["oa_crosswalk"]=oa;}if missing||!pdf{let pending=if row.extra["oa_crosswalk"].is_null(){""}else{"; open-access PMC route recorded in extra.oa_crosswalk, pending the shared PMC crosswalk"};row.extra["discovery_error"]=json!(format!("{}{pending}",if missing{"Source names a guideline without an emitted terminal link; no download guessed"}else{"Source-emitted publisher/article HTML requires its own verified full-body template; no menu/abstract substituted for the clinical article"}));}rows.push(row);
}
fn catalog(src:&SourceEntry,raw:&str,base:&Url)->Result<Vec<PlanRow>>{
 let doc=Html::parse_document(raw);let roots=doc.select(&sel("#content .main[role='main'] > .entry-content[itemprop='mainContentOfPage']")).collect::<Vec<_>>();if roots.len()!=1{bail!("WSES expected one guideline inventory body, found {}",roots.len());}let root=roots[0];if !root.select(&sel("h2")).any(|h|text_of(&h)=="WSES Guidelines"){bail!("WSES inventory heading missing");}let mut rows=vec![];let mut seen=HashMap::new();
 for h in root.select(&sel("h3")){let title=h.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");if title.is_empty(){continue;}let mut count=0;for a in h.select(&sel("a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap())else{continue};count+=1;push(src,&mut rows,&mut seen,url,title.clone(),json!({"title":title,"link_label":text_of(&a),"source_heading_html":h.html(),"source_url":base.as_str(),"role":"source_heading_entry"}),false);}if count==0{push(src,&mut rows,&mut seen,base.to_string(),title.clone(),json!({"title":title,"source_heading_html":h.html(),"source_url":base.as_str(),"role":"named_unlinked_guideline"}),true);}}
 for panel in root.select(&sel(".panel.panel-default")){let title=panel.select(&sel(".panel-heading h5")).next().map(|h|text_of(&h).replace("- (Click here to open)","").trim().to_string()).context("WSES accordion source title missing")?;let body=panel.select(&sel(".panel-body")).next().context("WSES accordion body missing")?;let mut count=0;for a in body.select(&sel("a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap())else{continue};if !Url::parse(&url)?.path().to_lowercase().ends_with(".pdf"){continue;}count+=1;push(src,&mut rows,&mut seen,url,title.clone(),json!({"title":title,"source_panel_id":panel.value().id(),"link_label":text_of(&a),"source_abstract_html":body.html(),"source_url":base.as_str(),"role":"accordion_download_with_separate_catalog_abstract"}),false);}if count==0{push(src,&mut rows,&mut seen,base.to_string(),title.clone(),json!({"title":title,"source_abstract_html":body.html(),"role":"accordion_without_validated_terminal"}),true);}}
 if rows.is_empty(){bail!("WSES guideline inventory empty");}for row in &mut rows{row.extra["inventory_scope"]=json!({"source_url":base.as_str(),"pagination":"single source-rendered heading/accordion inventory; no unseen pages or title-derived links guessed"});}Ok(rows)
}
fn source_date(label:&str,raw:&str,url:&str)->DateObservation{let mut date=common::date(label,raw,url);let re=regex::Regex::new(r"^(\d{1,2}) ([A-Za-z]+) ((?:19|20)\d{2})$").unwrap();if let Some(c)=re.captures(&date.raw){date.normalized=common::date(label,&format!("{} {} {}",&c[2],&c[1],&c[3]),url).normalized;}date}
fn article_metadata(blocks:&[Block],url:&str)->Value{
 let doi=regex::Regex::new(r"10\.\d{4,9}/[A-Za-z0-9_.-]+").unwrap();let identifier=blocks.iter().take(30).filter(|b|b.text.trim_start().starts_with("DOI:")).find_map(|b|doi.find(&b.text).map(|m|m.as_str().to_string())).or_else(||blocks.iter().take(5).find_map(|b|doi.find(&b.text).map(|m|m.as_str().to_string())));let executive=blocks.iter().take(20).any(|b|b.text.to_lowercase().contains("this executive manuscript"));let dates=regex::Regex::new(r"(Received|Accepted|Published online):\s*(\d{1,2}\s+[A-Za-z]+\s+(?:19|20)\d{2})").unwrap();let mut observed=vec![];let mut notices=vec![];let grades=regex::Regex::new(r"(?i)\[QoE:[^\]]+\]|\((?:strong|weak) recommendation[^)]*\)|\bGOR\s+[12][A-D]\b").unwrap();let mut mentions=vec![];
 for b in blocks{for c in dates.captures_iter(&b.text){observed.push(source_date(&c[1],&c[2],url));}if b.text.contains("Creative Commons Attribution 4.0 International License"){notices.push(json!({"source_block_id":b.block_id,"source_url":url,"text":b.text,"scope":"printed licence occurrence; whole-work scope applies only to a verified single article, not a compilation"}));}for m in grades.find_iter(&b.text){mentions.push(json!({"source_block_id":b.block_id,"source_url":url,"printed":m.as_str(),"start_byte_in_source_block":m.start(),"end_byte_in_source_block":m.end(),"source_block_text":b.text,"association":"exact block-local grading occurrence; not projected across interleaved table/narrative clauses"}));}}
 json!({"doi":identifier,"source_article_role":if executive{"executive_guideline_article"}else{"guideline_article"},"executive_role_basis":blocks.iter().take(20).filter(|b|b.text.to_lowercase().contains("this executive manuscript")).map(|b|json!({"source_block_id":b.block_id,"text":b.text})).collect::<Vec<_>>(),"date_observations":observed,"article_licence_notices":notices,"copyright_occurrences":blocks.iter().filter(|b|b.text.contains('©')||b.text.contains("All rights reserved")).map(|b|json!({"source_block_id":b.block_id,"text":b.text,"source_url":url,"scope":"printed copyright occurrence; not blanket compilation rights"})).collect::<Vec<_>>(),"printed_grade_occurrences":mentions,"journal_header":blocks.first().map(|b|b.text.as_str())})
}
fn contextual_blocks(blocks:&mut Vec<Block>,url:&str)->Result<Vec<Value>>{
 let names_grade=regex::Regex::new(r"(?i)GRADE\s+(?:methodology|system|approach)|Grading of Recommendations Assessment").unwrap();let grading_system=blocks.iter().take(30).any(|b|names_grade.is_match(&b.text)).then(||"GRADE (source explicitly names methodology)".to_string());
 let grade=regex::Regex::new(r"(?i)\((?:strong|weak) recommendation[^)]*\)").unwrap();let explicit=regex::Regex::new(r"^Recommendation\s+([0-9]+(?:\.[0-9]+)+)\s+.+\[QoE:\s*([^;\]]+);\s*Strength of recommendation:\s*(Strong|Weak)\s*[;:]\s*([12][A-D])\]\.\s*$").unwrap();let mut insertions:HashMap<String,Vec<Block>>=HashMap::new();let mut records=vec![];
 for (i,b) in blocks.iter().enumerate(){
  // Complete standalone numbered statements can be associated exactly. Mixed
  // summary columns and multi-recommendation blocks remain original source text.
  if b.text.matches("Recommendation ").count()==1&&!b.text.contains("Statement "){if let Some(c)=explicit.captures(&b.text){let id=format!("{}-recommendation-{}",b.block_id,&c[1]);records.push(json!({"block_id":id,"source_block_ids":[b.block_id],"source_url":url,"printed_label":format!("Recommendation {}",&c[1]),"printed_qoe":&c[2],"printed_strength":&c[3],"printed_combined_grade":&c[4],"association":"one complete standalone numbered source paragraph; original retained"}));insertions.entry(b.block_id.clone()).or_default().push(Block{block_id:id,kind:BlockKind::Recommendation{label:Some(format!("Recommendation {}",&c[1])),strength:Some(c[3].to_string()),certainty:Some(c[2].to_string()),population:None,grading_system:grading_system.clone(),qualifier:Some(format!("Printed combined grade {}",&c[4]))},text:b.text.clone(),heading_path:b.heading_path.clone(),page:None});}}
  if i==0{continue;}let previous=&blocks[i-1];let head=b.text.chars().next();let starts_grade=b.text.starts_with("(weak recommendation")||b.text.starts_with("(strong recommendation");let split_grade=(previous.text.ends_with("(strong")||previous.text.ends_with("(weak"))&&b.text.starts_with("recommendation based on");let unfinished=previous.text.chars().last().is_some_and(|c|!".!?:;)]".contains(c));
  let prefix_end=if split_grade{b.text.find(')').filter(|n|*n<350).map(|n|n+1)}else{grade.find(&b.text).filter(|m|m.start()<350&&(starts_grade||(unfinished&&head.is_some_and(|c|c.is_lowercase())))).map(|m|m.end())};
  if let Some(mut end)=prefix_end{if b.text[end..].starts_with('.'){end+=1;}let id=format!("{}-continued-source-context",b.block_id);let joined=format!("{} {}",previous.text,&b.text[..end]);records.push(json!({"block_id":id,"source_block_ids":[previous.block_id,b.block_id],"source_second_block_end_byte":end,"source_url":url,"scope":"verbatim adjacent source continuation context; no whole-group grade or clinical inference"}));insertions.entry(b.block_id.clone()).or_default().push(Block{block_id:id,kind:BlockKind::Paragraph,text:joined,heading_path:vec!["Source recommendation continuation context".into()],page:None});}
 }
 let mut out=vec![];for block in std::mem::take(blocks){let additions=insertions.remove(&block.block_id);out.push(block);if let Some(additions)=additions{out.extend(additions);}}*blocks=out;Ok(records)
}
// R2a-crosswalk: the site scrape (unchanged) lives in `discover_site`/`extract_site`; the trait impl below adds
// the PMC crosswalk for `extra.oa_crosswalk.{doi,pmcid}` (source-emitted URL or registry exact-title lookup).
impl WsesGuidelines{
 async fn discover_site(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{if limit==Some(0){return Ok(vec![]);}let response=f.get_ok(src.catalog_url.as_deref().context("WSES catalog missing")?).await?;let mut rows=catalog(src,&response.text(),&Url::parse(&response.record.final_url)?)?;if let Some(n)=limit{rows.truncate(n);}for row in &mut rows{row.extra["catalog_fetch"]=json!(response.record);if row.extra["discovery_error"].is_string(){continue;}let result=async{let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("WSES labelled PDF returned a wrapper rather than PDF bytes");}Ok::<_,anyhow::Error>(pdf.record)}.await;match result{Ok(record)=>{row.doc_key=format!("{}-{}",row.extra["work_key"].as_str().unwrap(),&record.sha256[..12]);row.extra["pdf_sha256"]=json!(record.sha256);row.extra["pdf_fetch"]=json!(record);},Err(error)=>{let pending=if row.extra["oa_crosswalk"].is_null(){""}else{"; open-access PMC route recorded in extra.oa_crosswalk, pending the shared PMC crosswalk"};row.extra["discovery_error"]=json!(format!("{error:#}{pending}"))}}}Ok(rows)}
 async fn extract_site(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{if let Some(error)=row.extra["discovery_error"].as_str(){bail!("WSES discovery: {error}");}let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("WSES source resource is not actual PDF bytes");}if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=pdf.record.sha256){bail!("WSES PDF changed after planning; re-plan");}let(mut blocks,mut warnings,_)=blocks_for(&pdf,None,30).await?;if blocks.is_empty(){bail!("WSES PDF has no extracted body");}for b in &mut blocks{b.page=None;}let tables=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count();let figures=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count();let metadata=article_metadata(&blocks,&row.url);let role=row.extra["source_catalog_role"].as_str().unwrap_or("unresolved_resource");if role=="grading_methods_tool"&&!blocks.iter().take(3).any(|b|b.text.contains("Tool for the development of Guidelines using the GRADE methodology")){bail!("WSES source grading-tool PDF body no longer matches its declared role");}let contexts=if role=="source_guideline_article"{contextual_blocks(&mut blocks,&row.url)?}else{vec![]};
  let mut rights=common::unknown_rights();rights.evidence_urls.push(row.url.clone());let notices=metadata["article_licence_notices"].as_array().unwrap();if role=="source_guideline_article"&&!notices.is_empty(){rights.status="open".into();rights.license=Some("CC-BY-4.0".into());rights.rights_holders.push("The Author(s)".into());rights.terms_url=Some("https://creativecommons.org/licenses/by/4.0/".into());rights.note=Some(notices.iter().map(|n|n["text"].as_str().unwrap()).collect::<Vec<_>>().join("\n"));rights.restrictions.push("Separately credited third-party material is excluded when its credit line says otherwise; article-specific notice retained.".into());}
  if role=="source_guideline_article"{let reserved=blocks.iter().take(30).filter(|b|b.text.starts_with('©')&&b.text.contains("All rights reserved")).collect::<Vec<_>>();if !reserved.is_empty(){rights.status="restricted".into();rights.license=None;rights.note=Some(reserved.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join("\n"));rights.restrictions.extend(reserved.iter().map(|b|b.text.clone()));}}
  let native_title=row.title.as_deref().unwrap_or_default().to_lowercase();let item_type=if role=="source_guideline_article"{if metadata["source_article_role"]=="executive_guideline_article"{"executive_guideline_article"}else if native_title.contains("position paper"){"position_paper"}else if native_title.contains("consensus statement"){"consensus_statement"}else if native_title.contains("training curriculum"){"training_curriculum"}else{metadata["source_article_role"].as_str().unwrap()}}else{role};let mut fetches=vec![];if let Ok(record)=serde_json::from_value::<FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(record);}fetches.push(pdf.record.clone());warnings.push("Published article, source-declared executive manuscript, compilation, translation and grading tool retain separate roles. Received/accepted dates are not publication or clinical currency; no upload-path year, country or current status inferred. Source-emitted HTML publisher routes remain unresolved until their full clinical template is verified.".into());warnings.push("Original converted source blocks/references and all grading occurrences are retained. Some PDFs flatten original summary tables into paragraphs and interleave columns/page headers. Only complete standalone numbered paragraphs receive structured grades; adjacent continuation groups retain source text without projecting one grade over multiple conditions. Converted zero table/figure counts do not establish absence of original tables/figures; physical pages and visual completeness remain unverified.".into());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().context("WSES source title missing")?,url:row.url.clone(),item_type:Some(item_type.into()),status:"unknown".into(),doi:if role=="source_guideline_article"{metadata["doi"].as_str().map(String::from)}else{None},date_observations:if role=="source_guideline_article"{serde_json::from_value(metadata["date_observations"].clone())?}else{vec![]},rights,blocks,attachments:vec![json!({"url":row.url,"role":"source_pdf","sha256":pdf.record.sha256})],search_metadata:json!({"source_catalog":row.extra,"source_article_metadata":metadata,"source_context_mappings":contexts,"coverage":{"converted_table_blocks":tables,"converted_figure_blocks":figures,"physical_pages":"unmapped","complete_visual_semantics":false}}),fetches,warnings,..Default::default()})
 }
}
/// Does the PMC JATS article-title agree with the WSES heading? "exact" (normalised), "prefix" (the heading is the
/// start of the article title, as recorded for that registry entry), else "differs".
fn title_agreement(heading:&str,jats:&str,prefix_recorded:bool)->&'static str{let(h,j)=(cw::norm_title(heading),cw::norm_title(jats));if !h.is_empty()&&h==j{"exact"}else if prefix_recorded&&!h.is_empty()&&j.starts_with(&h){"prefix"}else{"differs"}}
/// R2a-crosswalk: one site row and its PMC crosswalk rows; a failing site row is replaced by its open copy.
async fn crosswalk_groups(src:&SourceEntry,f:&Fetcher,mut row:PlanRow,seen:&mut std::collections::HashSet<String>)->Vec<(PlanRow,Vec<PlanRow>)>{
   let oa=row.extra["oa_crosswalk"].clone();let(doi,pmcid)=cw::ids_from_extra(&row.extra);let lookups=r2a::single(doi.as_deref(),pmcid.as_deref());
   if !oa.is_object()||lookups.is_empty(){return vec![(row,vec![])]}
   let heading=row.title.clone().unwrap_or_default();let title_route=oa["id_source"]=="registry_config_from_europepmc_title_search";
   let page=row.extra["catalog_mentions"][0]["source_url"].as_str().map(String::from).or_else(||src.catalog_url.clone()).unwrap_or_default();
   let failing=row.extra["discovery_error"].is_string();
   let site=r2a::SiteRef{doc_key:(!failing).then(||row.doc_key.clone()),url:page.clone()};
   let route=if title_route{"exact_title"}else if pmcid.is_some(){"source_pmcid"}else{"source_doi"};
   let notes=if title_route{vec![format!("The WSES page names this guideline without a link; its PMC identifiers come from the registry's Europe PMC exact-title lookup ({}).",oa["evidence_url"].as_str().unwrap_or("evidence URL not recorded"))]}else{vec![]};
   let sref=cw::SourceRef{source_url:oa["source_url"].as_str().unwrap_or_default().to_string(),evidence_url:Some(page.clone()),source_title:Some(heading.clone()),item_type:Some("guideline".into()),key_prefix:r2a::key_prefix(src),id_route:route.into(),notes};
   let site_row=json!({"url":row.url,"doc_key":row.doc_key,"discovery_error":row.extra["discovery_error"],"oa_crosswalk":oa,"catalog_mentions":row.extra["catalog_mentions"]});
   let mut o=r2a::adopt(src,f,&lookups,&sref,&site,&json!({"wses_site_row":site_row}),seen).await;
   // A title-route id must name the same article: the JATS title has to equal the heading (or the recorded prefix).
   let prefix=oa["title_match"]=="source_title_is_prefix_of_article_title";let mut kept=vec![];
   for mut r in std::mem::take(&mut o.rows){let jats=r.title.clone().unwrap_or_default();let agree=title_agreement(&heading,&jats,prefix);
    r.extra["title_check"]=json!({"source_heading":heading,"jats_title":jats,"agreement":agree,"enforced":title_route});
    if title_route&&agree=="differs"{o.open.retain(|x|x["doc_key"]!=r.doc_key.as_str());seen.remove(&cw::row_pmcid(&r).unwrap_or_default());
     o.closed.push(json!({"requested":r.extra[r2a::BACKREF]["requested"],"ids":r.extra["pmc_crosswalk"]["ids"],"reason":format!("JATS article-title {jats:?} does not match the source heading {heading:?}; not adopted"),"relation":null}));}
    else{kept.push(r)}}
   o.rows=kept;
   if failing{
    // The site row has no content route of its own: an open PMC copy replaces it; otherwise it keeps its refusal.
    if o.rows.is_empty(){let note=o.closed_note().unwrap_or_default();row.extra["discovery_error"]=json!(format!("{}; PMC crosswalk result: {note}",row.extra["discovery_error"].as_str().unwrap_or_default()));o.record(&mut row.extra);vec![(row,vec![])]}
    else{o.rows.into_iter().map(|r|(r,vec![])).collect()}
   }else{o.record(&mut row.extra);vec![(row,o.rows)]}
}
#[async_trait::async_trait]
impl Adapter for WsesGuidelines{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  let rows=self.discover_site(src,f,limit).await?;let mut seen=std::collections::HashSet::new();let mut groups=vec![];
  for row in rows{groups.extend(crosswalk_groups(src,f,row,&mut seen).await);}
  Ok(r2a::interleave(groups,limit))
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if cw::is_crosswalk_row(row){return r2a::extract(src,f,row).await}
  let mut d=self.extract_site(src,f,row).await?;r2a::annotate(&mut d,src,&row.extra);Ok(d)
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests{
 use super::*;
 const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/research-1.html"));
 fn source()->SourceEntry{serde_json::from_value(json!({"id":"wses_guidelines","enabled":true,"access":"public","catalog_url":"https://www.wses.org.uk/scientific-resources/guidelines"})).unwrap()}
 fn diverticulitis()->(String,Vec<Block>){let d:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/diverticulitis-blocks-1.json"))).unwrap();(d["source_pdf_url"].as_str().unwrap().to_string(),serde_json::from_value(d["blocks"].clone()).unwrap())}
 fn appendicitis()->(String,Vec<Block>){let a:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/appendicitis-blocks-1.json"))).unwrap();let b:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/appendicitis-blocks-2.json"))).unwrap();let mut blocks:Vec<Block>=serde_json::from_value(a["blocks"].clone()).unwrap();blocks.extend(serde_json::from_value::<Vec<Block>>(b["blocks"].clone()).unwrap());(a["source_pdf_url"].as_str().unwrap().to_string(),blocks)}
 #[tokio::test]
 async fn genuine_complete_grading_tool_is_a_methods_work_not_a_clinical_recommendation_set(){
  const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/grading-tool.pdf"));let sha="2eeaadba8131aee73108acceef4b1496af27ca2069be4a95248f81aac25c2cc3";assert_eq!(PDF.len(),334392);assert_eq!(crate::common::sha256_hex(PDF),sha);let source=source();let mut row=catalog(&source,C,&Url::parse(source.catalog_url.as_ref().unwrap()).unwrap()).unwrap().into_iter().find(|r|r.url.ends_with("/WSES-GRADE-Tool.pdf")).unwrap();row.extra["pdf_sha256"]=json!(sha);row.doc_key=format!("{}-{}",row.extra["work_key"].as_str().unwrap(),&sha[..12]);let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(&row.url,PDF,"application/pdf");let d=WsesGuidelines.extract(&source,&f,&row).await.unwrap();assert_eq!(d.doc_key,row.doc_key);assert_eq!(d.item_type.as_deref(),Some("grading_methods_tool"));assert_eq!(d.rights.status,"unknown");assert!(d.doi.is_none()&&d.published.is_none()&&d.updated.is_none()&&d.country.is_none()&&d.version.is_none());assert!(d.date_observations.is_empty());assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);assert!(d.blocks.iter().any(|b|b.text.contains("WEAK/CONDITIONAL")));assert!(d.blocks.iter().any(|b|b.text.contains("18436948")));assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})&&b.page.is_none()));row.extra["pdf_sha256"]=json!("changed");assert!(WsesGuidelines.extract(&source,&f,&row).await.is_err());
 }
 #[test]
 fn genuine_modern_article_doi_copyright_and_each_gor_occurrence_are_preserved(){
  let gold:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/vascular-blocks-1.json"))).unwrap();let blocks:Vec<Block>=serde_json::from_value(gold["blocks"].clone()).unwrap();assert_eq!(blocks.len(),196);let metadata=article_metadata(&blocks,gold["source_pdf_url"].as_str().unwrap());assert_eq!(metadata["doi"],"10.1097/TA.0000000000004823");assert!(metadata["article_licence_notices"].as_array().unwrap().is_empty());assert!(metadata["copyright_occurrences"].as_array().unwrap().iter().any(|n|n["text"]=="© 2026 Wolters Kluwer Health, Inc. All rights reserved."));let grades=metadata["printed_grade_occurrences"].as_array().unwrap();assert!(grades.iter().any(|g|g["printed"]=="GOR 1B"));assert!(grades.iter().any(|g|g["printed"]=="GOR 2C"));assert!(grades.iter().all(|g|g["source_block_text"].is_string()&&g["start_byte_in_source_block"].is_number()));assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);assert!(blocks.last().unwrap().text.contains("All rights reserved"));
 }
 #[tokio::test]
 async fn genuine_inventory_preserves_all_pdf_mentions_unlinked_titles_and_accordion_roles(){
  assert_eq!(crate::common::sha256_hex(C.as_bytes()),"7e05c8558b494e8945bbb36545399798a07d26405ac4adafa47c446ff018b5b3");let source=source();let base=Url::parse(source.catalog_url.as_ref().unwrap()).unwrap();let rows=catalog(&source,C,&base).unwrap();assert_eq!(rows.len(),54);let pdfs=rows.iter().filter(|r|r.rendition.as_deref()==Some("pdf")).collect::<Vec<_>>();assert_eq!(pdfs.len(),40);assert_eq!(pdfs.iter().map(|r|r.extra["catalog_mentions"].as_array().unwrap().len()).sum::<usize>(),41);assert!(rows.iter().any(|r|r.extra["source_catalog_role"]=="grading_methods_tool"));assert!(rows.iter().any(|r|r.extra["source_catalog_role"]=="guideline_compilation"));assert!(rows.iter().any(|r|r.extra["source_catalog_role"]=="translated_guideline_compilation"));
  let pregnancy=rows.iter().find(|r|r.url.ends_with("/EANS.pdf")).unwrap();assert!(pregnancy.title.as_deref().unwrap().ends_with("pregnancy"));assert!(!pregnancy.title.as_deref().unwrap().contains("pregnan cy"));assert!(rows.iter().any(|r|r.title.as_deref().is_some_and(|s|s.contains("immunocompromised"))&&r.extra["discovery_error"].is_string()));assert!(rows.iter().any(|r|r.url.ends_with("/pelvic-trauma-guidleines.pdf")&&r.extra["catalog_mentions"][0]["role"]=="accordion_download_with_separate_catalog_abstract"));
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(base.as_str(),C.as_bytes(),"text/html");let actual=WsesGuidelines.discover(&source,&f,Some(1)).await.unwrap();assert!(actual[0].extra["discovery_error"].as_str().unwrap().contains("offline and not cached"));assert!(WsesGuidelines.discover(&source,&f,Some(0)).await.unwrap().is_empty());
 }
 #[test]
 fn genuine_executive_article_is_not_relabelled_comprehensive_and_dates_rights_stay_source_scoped(){
  let(url,blocks)=appendicitis();assert_eq!(blocks.len(),176);let metadata=article_metadata(&blocks,&url);assert_eq!(metadata["doi"],"10.1186/s13017-020-00306-3");assert_eq!(metadata["source_article_role"],"executive_guideline_article");assert!(metadata["executive_role_basis"][0]["text"].as_str().unwrap().contains("This executive manuscript summarizes"));let dates=metadata["date_observations"].as_array().unwrap();assert!(dates.iter().any(|d|d["label"]=="Received"&&d["raw"]=="9 January 2020"&&d["normalized"]=="2020-01-09"));assert!(dates.iter().any(|d|d["label"]=="Accepted"&&d["normalized"]=="2020-03-30"));assert!(metadata["article_licence_notices"][0]["text"].as_str().unwrap().contains("unless indicated otherwise in a credit line"));assert!(blocks.iter().any(|b|b.text.contains("Table 3 Summary of the updated 2020 guidelines")));assert!(!blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})));
 }
 #[test]
 fn genuine_continuations_preserve_distinct_conditions_and_complete_standalone_numbered_grades(){
  let(url,mut blocks)=diverticulitis();assert_eq!(blocks.len(),83);let originals=blocks.iter().map(|b|(b.block_id.clone(),b.text.clone())).collect::<Vec<_>>();let records=contextual_blocks(&mut blocks,&url).unwrap();let oral=blocks.iter().find(|b|b.block_id.ends_with("-continued-source-context")&&b.text.contains("In patients requiring antibiotic therapy")&&b.text.contains("moderate-quality evidence, 1B)")).unwrap();assert!(oral.text.contains("without signs of systemic inflammation")&&oral.text.contains("high-quality evidence, 1A)"));assert!(!matches!(oral.kind,BlockKind::Recommendation{..}));assert!(records.iter().any(|r|r["block_id"]==oral.block_id&&r["source_block_ids"].as_array().unwrap().len()==2));
  let emergency=blocks.iter().find(|b|b.block_id.ends_with("-continued-source-context")&&b.text.contains("emergency laparoscopic sigmoidectomy only if technical skills and equipment are available")).unwrap();assert!(emergency.text.contains("low-quality evidence, 2C)"));for(id,text)in originals{assert_eq!(blocks.iter().find(|b|b.block_id==id).unwrap().text,text);}
  let(url,mut blocks)=appendicitis();let source_count=blocks.len();let records=contextual_blocks(&mut blocks,&url).unwrap();assert!(blocks.len()>source_count);let suction=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(label),strength:Some(strength),certainty:Some(certainty),..}if label=="Recommendation 4.8"&&strength=="Strong"&&certainty=="Moderate")).unwrap();assert!(suction.text.contains("intra-abdominal collections undergoing laparoscopic appendectomy"));assert!(records.iter().any(|r|r["block_id"]==suction.block_id&&r["printed_combined_grade"]=="1B"));
 }
 #[tokio::test]async fn genuine_diverticulitis_cover_and_clinical_page_preserve_local_scope(){
  let cover=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/diverticulitis-conditions-page-1.pdf"));let clinical=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/diverticulitis-conditions-page-5.pdf"));assert_eq!(cover.len(),202778);assert_eq!(crate::common::sha256_hex(cover),"ff4c744f1bfb9c45b3c2e945e5d04ed5cd7008e12ad41ed90b4e0d6eaef48946");assert_eq!(crate::common::sha256_hex(clinical),"b76fe98442674bae34ea4f06fbf47d9332bc46dab19b524a95f7820664edef2e");let src=source();let u="https://www.wses.org.uk/wp-content/uploads/2020/05/update-acute-diverticulitis.pdf";let mut row=catalog(&src,C,&Url::parse(src.catalog_url.as_ref().unwrap()).unwrap()).unwrap().into_iter().find(|r|r.url==u).unwrap();let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(u,cover,"application/pdf");row.extra["pdf_sha256"]=json!(crate::common::sha256_hex(cover));let first=WsesGuidelines.extract(&src,&f,&row).await.unwrap();assert_eq!(first.item_type.as_deref(),Some("guideline_article"));assert_eq!(first.rights.status,"open","actual cover blocks: {:#?}",first.blocks);assert_eq!(first.rights.license.as_deref(),Some("CC-BY-4.0"));assert!(first.rights.note.as_deref().is_some_and(|s|s.contains("unless indicated otherwise")));assert!(first.warnings.iter().any(|s|s.contains("physical pages")||s.contains("physical pages and visual")));
  f.seed_cache_for_tests(u,clinical,"application/pdf");assert!(WsesGuidelines.extract(&src,&f,&row).await.is_err());row.extra["pdf_sha256"]=json!(crate::common::sha256_hex(clinical));let page=WsesGuidelines.extract(&src,&f,&row).await.unwrap();assert_eq!(page.rights.status,"unknown","an isolated clinical page must not inherit the cover licence");let text=page.blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ").to_lowercase();for required in ["immunocompromised","non-operative","without signs of systemic inflammation","antibiotic", "2d", "1a", "1b"]{assert!(text.contains(required),"missing {required}; blocks={:#?}",page.blocks);}assert!(page.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})),"the unnumbered/column-interleaved excerpt must not gain a projected grade");assert!(page.warnings.iter().any(|s|s.contains("interleave")||s.contains("interleaved")));
 }

}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod g2b_tests {
 use super::*;
 const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/research-1.html"));
 fn registry_source()->SourceEntry{
  let path=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs/guideline_sources.json");
  crate::guidelines::registry::get(&path,"wses_guidelines").unwrap()
 }
 #[test]
 fn oa_identifiers_come_from_source_urls_or_evidenced_title_crosswalk_only(){
  let source=registry_source();let base=Url::parse(source.catalog_url.as_ref().unwrap()).unwrap();let rows=catalog(&source,C,&base).unwrap();
  let by_url=|u:&str|rows.iter().find(|r|r.url==u).unwrap();
  let bmc=by_url("https://wjes.biomedcentral.com/articles/10.1186/s13017-023-00525-4");assert_eq!(bmc.extra["oa_crosswalk"]["doi"],"10.1186/s13017-023-00525-4");assert_eq!(bmc.extra["oa_crosswalk"]["id_source"],"source_emitted_url");
  assert!(bmc.extra["discovery_error"].as_str().unwrap().contains("pending the shared PMC crosswalk"));
  assert_eq!(by_url("https://pmc.ncbi.nlm.nih.gov/articles/PMC11823064/").extra["oa_crosswalk"]["pmcid"],"PMC11823064");
  let robots=by_url("https://www.ncbi.nlm.nih.gov/pmc/articles/PMC9580452/pdf/13017_2022_Article_443.pdf");assert_eq!(robots.extra["oa_crosswalk"]["pmcid"],"PMC9580452");assert!(robots.extra["discovery_error"].is_null());
  let named=rows.iter().filter(|r|r.extra["catalog_mentions"][0]["role"]=="named_unlinked_guideline").collect::<Vec<_>>();assert_eq!(named.len(),9);
  let mapped=named.iter().filter(|r|!r.extra["oa_crosswalk"].is_null()).collect::<Vec<_>>();assert_eq!(mapped.len(),7);
  let ibd=mapped.iter().find(|r|r.title.as_deref().unwrap().starts_with("WSES-AAST guidelines: management of inflammatory bowel")).unwrap();
  assert_eq!(ibd.extra["oa_crosswalk"]["pmcid"],"PMC8111988");assert_eq!(ibd.extra["oa_crosswalk"]["title_match"],"exact");assert!(ibd.extra["oa_crosswalk"]["evidence_url"].as_str().unwrap().starts_with("https://www.ebi.ac.uk/europepmc/webservices/rest/search?"));
  assert!(named.iter().filter(|r|r.extra["oa_crosswalk"].is_null()).all(|r|r.title.as_deref().unwrap().contains("vascular injuries")));
  assert!(named.iter().all(|r|r.extra["discovery_error"].as_str().unwrap().starts_with("Source names a guideline without an emitted terminal link")));
  let plain=catalog(&serde_json::from_value(json!({"id":"wses_guidelines","enabled":true,"access":"public"})).unwrap(),C,&base).unwrap();
  assert!(plain.iter().filter(|r|r.extra["catalog_mentions"][0]["role"]=="named_unlinked_guideline").all(|r|r.extra["oa_crosswalk"].is_null()));
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod r2a_tests {
 use super::*;
 use crate::guidelines::sources::pmc_crosswalk::{self as cw,Lookup};
 const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/wses_guidelines/research-1.html"));
 fn fx(n:&str)->Vec<u8>{std::fs::read(format!("{}/tests/fixtures/guidelines/r2a_crosswalk/{n}",env!("CARGO_MANIFEST_DIR"))).unwrap()}
 fn registry_source()->SourceEntry{let path=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs/guideline_sources.json");crate::guidelines::registry::get(&path,"wses_guidelines").unwrap()}
 fn fetcher(dir:&std::path::Path,src:&SourceEntry)->Fetcher{let f=Fetcher::new(dir,true,1,8<<20,"").unwrap();f.seed_cache_for_tests(src.catalog_url.as_deref().unwrap(),C.as_bytes(),"text/html");
  f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Pmcid("PMC8352154".into())),&fx("idconv-PMC8352154.json"),"application/json");f.seed_cache_for_tests(&cw::oai_url("PMC8352154").unwrap(),&fx("oai-PMC8352154.xml"),"application/xml");f}
 // The named-unlinked heading "Acute abdomen in the immunocompromised patient ..." (registry exact-title entry,
 // PMC8352154): its refused site row is replaced by the PMC OA copy, whose JATS title equals the heading.
 #[tokio::test]
 async fn title_route_row_is_replaced_by_its_matching_pmc_copy(){
  let src=registry_source();let dir=tempfile::tempdir().unwrap();let f=fetcher(dir.path(),&src);
  let rows=WsesGuidelines.discover(&src,&f,None).await.unwrap();
  let cwr=rows.iter().filter(|r|cw::is_crosswalk_row(r)).collect::<Vec<_>>();assert_eq!(cwr.len(),1,"only PMC8352154 is seeded");
  let r=cwr[0];assert!(r.doc_key.starts_with("wses_guidelines-pmc-pmc8352154-"));assert_eq!(r.extra["title_check"]["agreement"],"exact");assert_eq!(r.extra["title_check"]["enforced"],true);
  assert!(!rows.iter().any(|x|!cw::is_crosswalk_row(x)&&x.title.as_deref().is_some_and(|t|t.starts_with("Acute abdomen in the immunocompromised"))),"replaced, not duplicated");
  // Other crosswalk ids cannot be reached offline: their rows keep the site refusal plus the crosswalk reason.
  assert!(rows.iter().filter(|x|x.extra["oa_crosswalk"].is_object()&&!cw::is_crosswalk_row(x)).all(|x|x.extra["discovery_error"].as_str().unwrap().contains("PMC crosswalk result:")));
  let d=WsesGuidelines.extract(&src,&f,r).await.unwrap();
  assert!(d.relations.iter().any(|x|x.rel=="rendition_of"&&x.target=="pmc:PMC8352154"));assert!(d.relations.iter().any(|x|x.target==src.catalog_url.clone().unwrap()));
  assert!(d.warnings.iter().any(|w|w.contains("exact-title lookup")));assert_eq!(d.rights.license.as_deref(),Some("CC-BY-4.0"));
 }
 // A registry title entry pointing at a different article is refused (no substitution), shown with a synthetic
 // mapping of another WSES heading to PMC8352154.
 #[tokio::test]
 async fn title_route_mismatch_is_refused(){
  let mut src=registry_source();let heading="Diagnosis and management of small bowel obstruction in virgin abdomen: a WSES position paper";
  src.config["oa_title_crosswalk"]=json!({heading:{"pmcid":"PMC8352154","title_match":"exact"}});
  let dir=tempfile::tempdir().unwrap();let f=fetcher(dir.path(),&src);
  let rows=WsesGuidelines.discover(&src,&f,None).await.unwrap();assert!(!rows.iter().any(|r|cw::is_crosswalk_row(r)));
  let row=rows.iter().find(|r|r.title.as_deref().is_some_and(|t|t.starts_with("Diagnosis and management of small bowel obstruction in virgin abdomen"))).unwrap();
  assert!(row.extra["discovery_error"].as_str().unwrap().contains("does not match the source heading"));
  assert_eq!(title_agreement("Multidisciplinary management of elderly patients with rectal cancer","Multidisciplinary management of elderly patients with rectal cancer: recommendations from the SICG",true),"prefix");
  assert_eq!(title_agreement("Multidisciplinary management of elderly patients with rectal cancer","Multidisciplinary management of elderly patients with rectal cancer: recommendations",false),"differs");
 }
}
