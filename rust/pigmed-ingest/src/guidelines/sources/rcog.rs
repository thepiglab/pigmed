//! RCOG Green-top guidelines: RCOG-hosted guideline PDFs as full documents; landings that link only the BJOG (Wiley)
//! article stay source summaries carrying the stated DOI for the PMC open-access crosswalk.
use super::{common,Adapter,PlanRow};
use super::{pmc_crosswalk as cw,r2a_crosswalk as r2a};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use super::static_list::blocks_for;
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct Rcog;
const ROOT:&str="https://www-preview.rcog.org.uk/guidance/browse-all-guidance/green-top-guidelines/";
fn catalog(raw:&str,base:&Url)->Result<Vec<(String,String)>>{let d=Html::parse_document(raw);let root=d.select(&sel("main[id^='main-content']")).next().context("RCOG catalog main missing")?;let mut seen=HashSet::new();let mut out=vec![];for a in root.select(&sel("a[href]")){let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else{continue};let u=Url::parse(&url)?;let path=u.path();if u.origin()==base.origin()&&path.starts_with("/guidance/browse-all-guidance/green-top-guidelines/")&&path.trim_end_matches('/')!="/guidance/browse-all-guidance/green-top-guidelines"&&seen.insert(url.clone()){out.push((url,text_of(&a)));}}if out.is_empty(){bail!("RCOG catalog item links missing")};Ok(out)}
/// Classify one source card link. RCOG Green-top landings link either an RCOG-hosted PDF (`/media/.../*.pdf`) or the
/// BJOG (Wiley) article, whose URL carries the DOI. The DOI is copied from the source's own href, never inferred.
fn classify_link(base:&Url,url:&str,label:&str)->Result<serde_json::Value>{
 let u=Url::parse(url)?;let lower=label.to_lowercase();
 let doi_re=regex::Regex::new(r"^/doi/(?:(?:full|epdf|pdf|abs|ftr|reader)/)?(10\.\d{4,9}/[^?#\s]+)$").unwrap();
 let mut v=json!({"url":url,"source_label":label,"rights":"unknown"});
 if u.origin()==base.origin()&&u.path().starts_with("/media/")&&u.path().to_lowercase().ends_with(".pdf")&&(lower.contains("guideline")||lower.contains("pdf version")){v["role"]=json!("rcog_hosted_guideline_pdf");}
 else if u.host_str().is_some_and(|h|h.ends_with("onlinelibrary.wiley.com")){v["role"]=json!("publisher_article");if let Some(c)=doi_re.captures(u.path()){v["doi"]=json!(c[1].to_string());}}
 else{v["role"]=json!("other_source_link");}
 Ok(v)
}
fn landing(raw:&str,base:&Url)->Result<(String,String,Vec<serde_json::Value>)>{let d=Html::parse_document(raw);let root=d.select(&sel("main[id^='main-content']")).next().context("RCOG landing main missing")?;let title=root.select(&sel("h1")).next().map(|x|text_of(&x)).context("RCOG landing title missing")?;let mut parts=vec![];for el in root.select(&sel(".component--text")){let h=el.html();if !text_of(&el).trim().is_empty(){parts.push(h)}}if parts.is_empty(){bail!("RCOG landing text component missing")};let mut links=vec![];for a in root.select(&sel(".card__text a[href]")){let label=text_of(&a);if let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)){links.push(classify_link(base,&url,&label)?);}}Ok((title,parts.join("\n"),links))}
fn distinct(links:&[serde_json::Value],role:&str,key:&str)->Vec<String>{let mut out:Vec<String>=vec![];for l in links{if l["role"]==role{if let Some(v)=l[key].as_str(){if !out.iter().any(|x|x==v){out.push(v.into())}}}}out}
fn problem(src:&SourceEntry,url:String,msg:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("rcog-{}-unresolved",common::short_hash(&url)),url,title:None,rendition:None,version:None,extra:json!({"discovery_error":msg})}}
fn summary_dates(text:&str,url:&str)->Vec<crate::guidelines::doc::DateObservation>{let mut dates=vec![];let re=regex::Regex::new(r"(?i)reviewed in ((?:19|20)\d{2})").unwrap();if let Some(c)=re.captures(text){dates.push(common::date("Reviewed",&c[1],url))}let re=regex::Regex::new(r"(?i)Next review date:\s*((?:19|20)\d{2})").unwrap();if let Some(c)=re.captures(text){dates.push(common::date("Scheduled next review",&c[1],url))}dates}
fn version_of(title:&str)->Option<String>{regex::Regex::new(r"(?i)Green-top Guideline\s+(?:No\.?\s*)?(\d+[a-z]?)").unwrap().captures(title).map(|c|format!("Green-top Guideline No. {}",&c[1]))}
// ---------------------------------------------------------------------------------------------------------------
// Green-top recommendation boxes (I2-recs-pacing)

enum Tok{Bold(String),Grade(&'static str),Plain}
const GRADES:[&str;4]=["A","B","C","D"];
fn grade_of(s:&str)->Option<&'static str>{if s=="P"||s=="✓"{return Some("✓")}GRADES.iter().find(|g|**g==s).copied()}
/// Split one converted paragraph into bold runs, grade tokens and plain text. The grade box of a Green-top
/// recommendation is a bold letter A–D or the good-practice tick, which the PDF text gives as "P" (symbol font).
fn tokens(t:&str)->Vec<Tok>{
 let bold=regex::Regex::new(r"\*\*(.+?)\*\*").unwrap();let mut out=vec![];let mut last=0;
 let plain=|seg:&str,out:&mut Vec<Tok>|{let s=seg.trim();if s.is_empty()||!s.chars().any(|c|c.is_alphanumeric()){return}
  if let Some(g)=grade_of(s){out.push(Tok::Grade(g));return}
  // "P What are the risks ...?": the tick of the previous statement, then a question heading
  if out.is_empty(){if let Some(rest)=s.strip_prefix("P ").or_else(||s.strip_prefix("✓ ")){out.push(Tok::Grade("✓"));if rest.chars().any(|c|c.is_alphanumeric()){out.push(Tok::Plain)}return}}
  out.push(Tok::Plain)};
 for m in bold.captures_iter(t){let w=m.get(0).unwrap();plain(&t[last..w.start()],&mut out);let inner=m[1].trim();last=w.end();
  // running header/footer runs ("RCOG Green-top Guideline No. 45", "30 of 31 © Royal College ...") are not text
  if inner.contains('©')||inner.starts_with("RCOG Green-top Guideline"){continue}
  match grade_of(inner){Some(g)=>out.push(Tok::Grade(g)),None=>out.push(Tok::Bold(inner.to_string()))}}
 plain(&t[last..],&mut out);out
}
struct Cur{text:String,grade:Option<&'static str>,last:usize,only_headings:bool,path:Vec<String>,page:Option<u32>,id:String}
/// RCOG Green-top PDFs print each recommendation in a box: bold statement text with its grade (A–D, or ✓ for a
/// good practice point) in a box on the right of the statement's FIRST line. The PDF conversion keeps the statement
/// as an unnumbered heading line plus bold paragraph lines and puts the grade token after the first line, e.g.
/// heading "Planned VBAC is contraindicated in ... classical caesarean scar" + "**D** **and in women ...**", or
/// "**... singleton** **B** **pregnancy of ...**". Recognised only when the PDF carries the Green-top key ("Grades of
/// recommendations" and "Good practice point"); grades are copied, never inferred. Returns warnings.
fn mark_green_top_recommendations(blocks:&mut Vec<Block>)->Vec<String>{
 let has=|s:&str|blocks.iter().any(|b|b.text.replace("**","").contains(s));
 if !(has("Grades of recommendation")&&has("Good practice point")){return vec![]}
 let numbered=regex::Regex::new(r"^\d+(?:\.\d+)*\.?\s").unwrap();
 let footer=regex::Regex::new(r"Green[-–]top Guideline No\.?\s*\d+[a-z]?\s+\d+\s+of\s+\d+").unwrap();
 let plain=|t:&str|crate::common::collapse_ws(&t.replace("**",""));
 let complete=|t:&str|{let t=t.trim_end();t.ends_with('.')||(t.ends_with(']')&&t.contains('.'))};
 let mut add:Vec<(usize,Block)>=vec![];let mut cur:Option<Cur>=None;
 let flush=|cur:&mut Option<Cur>,add:&mut Vec<(usize,Block)>|{if let Some(c)=cur.take(){if let Some(g)=c.grade{if c.text.chars().count()>=20{
  add.push((c.last,Block{block_id:format!("{}-green-top-recommendation",c.id),kind:BlockKind::Recommendation{label:Some(g.into()),strength:None,certainty:None,population:None,grading_system:Some("RCOG Green-top grades of recommendation A–D and good practice point (✓), per the guideline's classification key".into()),qualifier:(g=="✓").then(||"Good practice point".into())},text:c.text,heading_path:c.path,page:c.page}));}}}};
 for (k,b) in blocks.iter().enumerate(){
  let t=b.text.trim();let p=plain(t);
  if footer.is_match(&p){continue}
  let toks=match &b.kind{
   BlockKind::Heading{..} if !numbered.is_match(&p)&&!p.ends_with('?')=>vec![Tok::Bold(p.clone())],
   BlockKind::Paragraph=>tokens(t),
   _=>{flush(&mut cur,&mut add);continue}
  };
  let heading=matches!(b.kind,BlockKind::Heading{..});
  for tok in toks{match tok{
   Tok::Plain=>flush(&mut cur,&mut add),
   Tok::Grade(g)=>{
    // the first grade after a statement's first line is its grade; a second one after a complete statement
    // closes it (its own statement follows); an orphan grade (no open statement) is ignored
    let state=cur.as_ref().map(|c|(c.grade.is_none(),complete(&c.text)));
    match state{Some((true,_))=>{if let Some(c)=cur.as_mut(){c.grade=Some(g);c.last=k}},Some((false,true))=>flush(&mut cur,&mut add),_=>{}}
   }
   // a bold question or numbered section heading inside a paragraph ("**17. In women ... by whom?**") is a boundary
   Tok::Bold(x) if numbered.is_match(&x)||x.trim_end().ends_with('?')=>flush(&mut cur,&mut add),
   Tok::Bold(x)=>{
    let restart=match &cur{None=>true,Some(c)=>{
     let done=complete(&c.text);
     (c.grade.is_some()&&done&&!x.starts_with('['))
     // complete but ungraded: not a recommendation (prose or a heading); short heading line before a statement line
     ||(c.grade.is_none()&&done)||(c.grade.is_none()&&heading&&c.only_headings&&c.text.chars().count()<60)}};
    if restart{flush(&mut cur,&mut add);cur=Some(Cur{text:x,grade:None,last:k,only_headings:heading,path:b.heading_path.clone(),page:b.page,id:b.block_id.clone()})}
    else if let Some(c)=cur.as_mut(){c.text=format!("{} {x}",c.text);c.last=k;c.only_headings&=heading}
   }
  }}
 }
 flush(&mut cur,&mut add);
 if add.is_empty(){return vec![]}
 let n=add.len();let gpp=add.iter().filter(|(_,b)|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="✓")).count();
 for (k,b) in add.into_iter().rev(){blocks.insert(k+1,b)}
 vec![format!("{n} Green-top recommendations structured from the PDF text ({gpp} good practice points): each grade box is matched to the statement whose first line it follows; the ✓ is extracted as 'P' (symbol font). Boxes the text extraction reorders or splits across columns may be missed or merged; evidence levels in the margin are not structured.")]
}
// R2a-crosswalk: the site scrape (unchanged) lives in `discover_site`/`extract_site`; the trait impl below adds
// the PMC crosswalk for the BJOG DOIs the landings state (`extra.publisher_dois`).
impl Rcog{
 async fn discover_site(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{if limit==Some(0){return Ok(vec![])}let c=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;let base=Url::parse(&c.record.final_url)?;let entries=catalog(&c.text(),&base)?;let mut rows=vec![];
  for(url,_)in entries{let p=match f.get_ok(&url).await{Ok(p)=>p,Err(e)=>{rows.push(problem(src,url,format!("RCOG landing unavailable: {e:#}")));if limit.is_some_and(|n|rows.len()>=n){break}continue}};let u=Url::parse(&p.record.final_url)?;if u.origin()!=base.origin(){rows.push(problem(src,url,"RCOG landing redirected outside source".into()));continue}let(title,body,links)=match landing(&p.text(),&u){Ok(x)=>x,Err(e)=>{rows.push(problem(src,url,format!("RCOG landing template: {e:#}")));continue}};let digest=crate::common::sha256_hex(serde_json::to_string(&(&title,&body,&links))?.as_bytes());let work=format!("rcog-{}",common::short_hash(&url));
   let pdfs=distinct(&links,"rcog_hosted_guideline_pdf","url");let dois=distinct(&links,"publisher_article","doi");
   let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-{}",&digest[..12]),url:url.clone(),title:Some(title.clone()),rendition:Some("html".into()),version:version_of(&title),extra:json!({"role":"source_summary","work_key":work,"snapshot":digest,"assets":links,"catalog_fetch":c.record,"landing_fetch":p.record,"publisher_dois":dois})};
   if pdfs.len()==1{
    // The landing's own guideline PDF is the full text; the landing summary becomes metadata of that document.
    match f.get_ok(&pdfs[0]).await{Ok(pdf) if pdf.is_pdf()=>{row.url=pdfs[0].clone();row.rendition=Some("pdf".into());row.doc_key=format!("{work}-{}",&pdf.record.sha256[..12]);row.extra["role"]=json!("full_guideline_pdf");row.extra["landing_url"]=json!(url);row.extra["pdf_sha256"]=json!(pdf.record.sha256);},
     Ok(_)=>{row.extra["full_text_error"]=json!("RCOG-hosted guideline link did not return PDF bytes; summary kept");},
     Err(e)=>{row.extra["full_text_error"]=json!(format!("RCOG-hosted guideline PDF unavailable: {e:#}"));}}
   }else if pdfs.len()>1{row.extra["full_text_error"]=json!(format!("RCOG landing links {} distinct hosted PDFs; none chosen",pdfs.len()));}
   if row.extra["role"]=="source_summary"&&!dois.is_empty(){
    // Wiley returns 403 to our fetcher (2026-11 probe); the open-access route is the PMC copy of this DOI, resolved
    // by the shared PMC crosswalk. Nothing is requested from Wiley here.
    row.extra["full_text_route"]=json!("pmc_crosswalk_pending");
   }
   rows.push(row);if limit.is_some_and(|n|rows.len()>=n){break}
  }Ok(rows)
 }
 async fn extract_site(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{if let Some(e)=row.extra["discovery_error"].as_str(){bail!("RCOG discovery: {e}")};
  let landing_url=row.extra["landing_url"].as_str().unwrap_or(&row.url).to_string();
  let p=f.get_ok(&landing_url).await?;let base=Url::parse(&p.record.final_url)?;if base.origin()!=Url::parse(ROOT)?.origin(){bail!("RCOG landing redirected outside source")};let(title,body,assets)=landing(&p.text(),&base)?;let hash=crate::common::sha256_hex(serde_json::to_string(&(&title,&body,&assets))?.as_bytes());if row.extra["snapshot"].as_str()!=Some(hash.as_str()){bail!("RCOG summary changed; re-plan")};
  let(summary_blocks,mut warnings)=blocks_from_html(&body,Some(&base),None);let summary_text=summary_blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");let dates=summary_dates(&summary_text,&landing_url);
  let dois=distinct(&assets,"publisher_article","doi");
  let mut relations:Vec<crate::guidelines::doc::Relation>=vec![];
  if row.extra["role"]=="full_guideline_pdf"{
   let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("RCOG guideline response is not PDF")};if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=pdf.record.sha256){bail!("RCOG PDF changed since discovery; re-plan")};
   let(mut blocks,notes,_)=blocks_for(&pdf,None,30).await?;if blocks.is_empty(){bail!("RCOG PDF extracted no text")};warnings.extend(notes);
   let text=blocks.iter().take(400).map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");
   if !text.contains("Green-top Guideline")&&!text.contains("Green–top Guideline"){warnings.push("PDF text does not name itself a Green-top Guideline; identity rests on the landing link only.".into());}
   let holder=text.contains("© Royal College of Obstetricians and Gynaecologists");
   let rights=Rights{status:"unknown".into(),rights_holders:if holder{vec!["Royal College of Obstetricians and Gynaecologists".into()]}else{vec![]},evidence_urls:vec![row.url.clone()],note:Some(if holder{"The PDF carries a © RCOG notice and states no open licence; reuse terms are RCOG's.".into()}else{"No copyright or licence statement recognised in the PDF text.".into()}),..Default::default()};
   let rec_notes=mark_green_top_recommendations(&mut blocks);
   if rec_notes.is_empty(){warnings.push("Green-top recommendation boxes, their grades and evidence levels are not yet structured from the PDF text (no Green-top grade key found); layout, tables and figures require visual verification.".into());}else{warnings.extend(rec_notes);warnings.push("Layout, tables and figures require visual verification.".into());}
   for d in &dois{relations.push(crate::guidelines::doc::Relation{rel:"related".into(),target:format!("doi:{d}"),scope:Some("Journal (BJOG) publication linked from the same RCOG landing; edition and text not compared".into()),evidence_url:Some(landing_url.clone())});}
   let mut fetches=vec![];for k in ["catalog_fetch","landing_fetch"]{if let Ok(x)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra[k].clone()){fetches.push(x);}}fetches.push(p.record);fetches.push(pdf.record);
   let mut attachments=assets.clone();attachments.push(json!({"url":landing_url,"role":"source_summary_page","rights":"unknown"}));
   return Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from).or_else(||Some(format!("rcog-{}",common::short_hash(&landing_url)))),title:title.clone(),url:row.url.clone(),item_type:Some("guideline".into()),version:version_of(&title),status:"unknown".into(),rights,date_observations:dates,relations,blocks,attachments,fetches,warnings,search_metadata:json!({"landing_url":landing_url,"source_summary":summary_text,"publisher_dois":dois}),..Default::default()});
  }
  if summary_blocks.is_empty(){bail!("RCOG summary empty")};
  warnings.push("This RCOG landing contains a source summary and version history, not the complete guideline or its recommendations. The Wiley terminal and its rights must be independently resolved.".into());warnings.push("Version-history statements, prior reviews and scheduled next reviews are distinct; a scheduled date is not evidence that a review occurred.".into());
  for d in &dois{relations.push(crate::guidelines::doc::Relation{rel:"summary_of".into(),target:format!("doi:{d}"),scope:Some("Source landing summary of the BJOG-published guideline; DOI copied from the source's Wiley link. PMC open-access copy, where one exists, is planned by the PMC crosswalk (see search_metadata.pmc_crosswalk).".into()),evidence_url:Some(landing_url.clone())});}
  if let Some(e)=row.extra["full_text_error"].as_str(){warnings.push(e.into());}
  let rights=Rights{status:"unknown".into(),note:Some("Summary and full-publisher article rights require independent verification.".into()),..Default::default()};Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:Some(format!("rcog-{}",common::short_hash(&row.url))),title,url:row.url.clone(),item_type:Some("summary".into()),version:row.version.clone(),status:"unknown".into(),rights,date_observations:dates,relations,blocks:summary_blocks,attachments:assets,fetches:vec![p.record],warnings,search_metadata:json!({"publisher_dois":dois,"full_text_route":row.extra["full_text_route"]}),..Default::default()})}
}
/// R2a-crosswalk: one site row plus the PMC crosswalk rows for the BJOG DOIs its landing states.
async fn crosswalk_group(src:&SourceEntry,f:&Fetcher,mut row:PlanRow,seen:&mut HashSet<String>)->(PlanRow,Vec<PlanRow>){
   let dois:Vec<String>=row.extra["publisher_dois"].as_array().map(|a|a.iter().filter_map(|x|x.as_str().map(String::from)).collect()).unwrap_or_default();
   if row.extra["discovery_error"].is_string()||dois.is_empty(){return (row,vec![])}
   let landing=row.extra["landing_url"].as_str().unwrap_or(&row.url).to_string();
   let site=r2a::SiteRef{doc_key:Some(row.doc_key.clone()),url:landing.clone()};let mut out=r2a::Outcome::default();
   for d in &dois{
    // The Wiley href the landing printed for this DOI is kept as the publisher link (never requested).
    let linked=row.extra["assets"].as_array().and_then(|a|a.iter().find(|x|x["role"]=="publisher_article"&&x["doi"]==d.as_str())).and_then(|x|x["url"].as_str()).unwrap_or_default().to_string();
    let sref=cw::SourceRef{source_url:linked,evidence_url:Some(landing.clone()),source_title:row.title.clone(),item_type:Some("guideline".into()),key_prefix:r2a::key_prefix(src),id_route:"source_doi".into(),notes:vec![]};
    let Some(n)=cw::normalize_doi(d) else{continue};
    out.merge(r2a::adopt(src,f,&[cw::Lookup::Doi(n)],&sref,&site,&json!({"source_row_title":row.title}),seen).await);
   }
   out.record(&mut row.extra);(row,out.rows)
}
#[async_trait::async_trait]
impl Adapter for Rcog{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  let rows=self.discover_site(src,f,limit).await?;let mut seen=HashSet::new();let mut groups=vec![];
  for row in rows{groups.push(crosswalk_group(src,f,row,&mut seen).await);}
  Ok(r2a::interleave(groups,limit))
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if cw::is_crosswalk_row(row){return r2a::extract(src,f,row).await}
  let mut d=self.extract_site(src,f,row).await?;r2a::annotate(&mut d,src,&row.extra);Ok(d)
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{use super::*;const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rcog/catalog.html"));const GTD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rcog/gtd-38.html"));
 const GTG63:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rcog/gtg-63.html"));const PDF63:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rcog/gtg-63-pages-1-3.pdf"));
 const U38:&str="https://www-preview.rcog.org.uk/guidance/browse-all-guidance/green-top-guidelines/gestational-trophoblastic-disease-green-top-guideline-no-38/";
 const U63:&str="https://www-preview.rcog.org.uk/guidance/browse-all-guidance/green-top-guidelines/antepartum-haemorrhage-green-top-guideline-no-63/";
 const P63:&str="https://www-preview.rcog.org.uk/media/pwdi1tef/gtg_63.pdf";
 fn src()->SourceEntry{serde_json::from_value(json!({"id":"rcog","access":"public","enabled":true,"catalog_url":ROOT})).unwrap()}
 fn one(u:&str)->String{format!("<main id='main-content'><a href='{u}'>item</a></main>")}
 #[test]fn real_catalog_and_landing_keep_publisher_link_labels_and_doi(){let rows=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();assert!(rows.iter().any(|(x,_)|x==U38));let(title,body,links)=landing(GTD,&Url::parse(U38).unwrap()).unwrap();assert!(title.contains("No. 38"));assert!(body.contains("Next review date: 2026")&&body.contains("reviewed in 2023"));assert_eq!(links.len(),2);assert_eq!(links[0]["url"],links[1]["url"]);assert!(links.iter().any(|l|l["source_label"].as_str().is_some_and(|s|s.contains("PDF")))&&links.iter().any(|l|l["source_label"].as_str().is_some_and(|s|s.contains("web"))));assert!(links.iter().all(|l|l["role"]=="publisher_article"&&l["doi"]=="10.1111/1471-0528.16266"));
  let(_,_,links)=landing(GTG63,&Url::parse(U63).unwrap()).unwrap();assert_eq!(distinct(&links,"rcog_hosted_guideline_pdf","url"),vec![P63.to_string()]);
  let b=Url::parse(U63).unwrap();for(u,doi) in [("https://obgyn.onlinelibrary.wiley.com/doi/epdf/10.1111/1471-0528.70195","10.1111/1471-0528.70195"),("https://obgyn.onlinelibrary.wiley.com/doi/10.1111/1471-0528.17515","10.1111/1471-0528.17515"),("https://obgyn.onlinelibrary.wiley.com/doi/ftr/10.1111/1471-0528.17844","10.1111/1471-0528.17844")]{assert_eq!(classify_link(&b,u,"Access the PDF version").unwrap()["doi"],doi);}
  assert_eq!(classify_link(&b,"https://elearning.rcog.org.uk/product?catalog=x","Access the extra content").unwrap()["role"],"other_source_link");
  assert_eq!(version_of("Antepartum Haemorrhage (Green-top Guideline No. 63)").as_deref(),Some("Green-top Guideline No. 63"));}
 #[tokio::test]async fn wiley_only_landing_is_a_summary_carrying_the_stated_doi(){let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,one(U38).as_bytes(),"text/html");f.seed_cache_for_tests(U38,GTD.as_bytes(),"text/html");let rows=Rcog.discover(&src(),&f,None).await.unwrap();assert_eq!(rows.len(),1,"no request is made to the 403 Wiley terminal");assert_eq!(rows[0].extra["publisher_dois"],json!(["10.1111/1471-0528.16266"]));assert_eq!(rows[0].extra["full_text_route"],"pmc_crosswalk_pending");let d=Rcog.extract(&src(),&f,&rows[0]).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("summary"));assert!(d.relations.iter().any(|r|r.rel=="summary_of"&&r.target=="doi:10.1111/1471-0528.16266"));assert!(d.date_observations.iter().any(|x|x.label=="Reviewed"&&x.raw=="2023"));assert!(d.date_observations.iter().any(|x|x.label=="Scheduled next review"&&x.raw=="2026"));assert!(d.warnings.iter().any(|x|x.contains("not the complete guideline")));assert_eq!(d.doi,None,"the summary page is not the DOI's article");}
 #[tokio::test]async fn rcog_hosted_pdf_is_the_full_guideline_with_summary_as_metadata(){let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,one(U63).as_bytes(),"text/html");f.seed_cache_for_tests(U63,GTG63.as_bytes(),"text/html");f.seed_cache_for_tests(P63,PDF63,"application/pdf");let rows=Rcog.discover(&src(),&f,None).await.unwrap();assert_eq!(rows.len(),1,"one document per guideline, not summary plus PDF");let r=&rows[0];assert_eq!(r.url,P63);assert_eq!(r.extra["role"],"full_guideline_pdf");assert_eq!(r.extra["landing_url"],U63);
  let d=Rcog.extract(&src(),&f,r).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("guideline"));assert_eq!(d.version.as_deref(),Some("Green-top Guideline No. 63"));assert!(d.title.contains("Antepartum Haemorrhage"));assert!(d.blocks.iter().any(|b|b.text.contains("defined as bleeding from or in to the genital tract")));assert_eq!(d.rights.rights_holders,vec!["Royal College of Obstetricians and Gynaecologists".to_string()]);assert_eq!(d.rights.status,"unknown");assert!(d.search_metadata["source_summary"].as_str().is_some_and(|s|!s.is_empty()));assert!(d.attachments.iter().any(|a|a["role"]=="source_summary_page"));assert!(d.fetches.iter().any(|x|x.url==P63));
  let mut changed=PDF63.to_vec();changed.extend_from_slice(b"\nchanged");f.seed_cache_for_tests(P63,&changed,"application/pdf");assert!(Rcog.extract(&src(),&f,r).await.is_err());}
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod green_top_tests{use super::*;
 const PDF45:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rcog/gtg45-p2-4-30.pdf"));
 fn mk(id:&str,kind:BlockKind,t:&str)->Block{Block{block_id:id.into(),kind,text:t.into(),heading_path:vec![],page:None}}
 fn recs(b:&[Block])->Vec<(String,String)>{b.iter().filter_map(|x|match &x.kind{BlockKind::Recommendation{label:Some(l),..}=>Some((l.clone(),x.text.clone())),_=>None}).collect()}
 fn dump(b:&[Block])->String{b.iter().take(150).map(|x|format!("{} {:?} {:?}\n",x.block_id,x.kind,x.text.chars().take(140).collect::<String>())).collect()}
 /// Verbatim block shapes from the GTG 45 / GTG 63 probe dumps (m00013-16, m00026-34, m00045-46, m00069-72).
 #[test]fn grade_boxes_attach_to_the_statement_whose_first_line_they_follow(){
  let h=||BlockKind::Heading{level:5};let p=||BlockKind::Paragraph;
  let mut b=vec![mk("key",p(),"Grades of recommendations At least one meta-analysis **A** randomised controlled trial"),mk("gpp",p(),"**RCOG Green-top Guideline No. 45** Good practice point P **30 of 31 © Royal College of Obstetricians and Gynaecologists**"),
   mk("m13",p(),"**Planned VBAC is appropriate for and may be offered to the majority of women with a singleton** **B** **pregnancy of cephalic presentation at 37<sup>+0</sup> weeks or beyond.**"),
   mk("m14",h(),"What are the contraindications to VBAC?"),
   mk("m15",h(),"Planned VBAC is contraindicated in women with previous uterine rupture or classical caesarean scar"),mk("m16",p(),"**D** **and in women who have other absolute contraindications to vaginal birth.**"),
   mk("m24",h(),"Antenatal counselling"),mk("m25",p(),"What are the overall aims of antenatal counselling?"),
   mk("m26",h(),"The antenatal counselling of women with a previous caesarean birth should be documented in"),mk("m27",p(),"P **the notes.**"),
   mk("m28",p(),"**A final decision for mode of birth should be agreed upon by the woman and member(s) of the** P **maternity team before the expected/planned date of delivery.**"),
   mk("m30",p(),"**RCOG Green-top Guideline No. 45 2 of 31 © Royal College of Obstetricians and Gynaecologists**"),
   mk("m33",h(),"A patient information leaflet should be provided with the consultation."),mk("m34",p(),"P What are the risks and benefits of planned VBAC versus ERCS?"),
   mk("m45",h(),"Women should be informed that the success rate of planned VBAC is 72–75%."),mk("m46",p(),"**C** What factors determine the individualised likelihood of VBAC success?"),
   mk("m69",h(),"ERCS delivery should be conducted after 39<sup>+0</sup> weeks of gestation."),mk("m70",p(),"**A** **Antibiotics should be administered before making the skin incision in women undergoing ERCS.** **B** **[*New 2015*]**"),
   mk("m71",h(),"All women undergoing ERCS should receive thromboprophylaxis according to existing RCOG"),mk("m72",p(),"P **guidelines. [*New 2015*]**"),
   mk("m228",p(),"**17. In women whose pregnancy is complicated by APH, how should the neonate be managed and by whom?** **Major or massive APH may result in fetal anaemia and fetal compromise. The neonate should be** P **assessed by a senior paediatrician/neonatologist.**"),
   mk("m80",h(),"1. Purpose and scope"),mk("m81",p(),"The purpose of this guideline is to provide information.")];
  let w=mark_green_top_recommendations(&mut b);assert!(w[0].starts_with("10 Green-top recommendations structured from the PDF text (5 good practice points)"),"{w:?}");
  let want:Vec<(&str,&str)>=vec![
   ("B","Planned VBAC is appropriate for and may be offered to the majority of women with a singleton pregnancy of cephalic presentation at 37<sup>+0</sup> weeks or beyond."),
   ("D","Planned VBAC is contraindicated in women with previous uterine rupture or classical caesarean scar and in women who have other absolute contraindications to vaginal birth."),
   ("✓","The antenatal counselling of women with a previous caesarean birth should be documented in the notes."),
   ("✓","A final decision for mode of birth should be agreed upon by the woman and member(s) of the maternity team before the expected/planned date of delivery."),
   ("✓","A patient information leaflet should be provided with the consultation."),
   ("C","Women should be informed that the success rate of planned VBAC is 72–75%."),
   // gs layout of page 4: "ERCS delivery ... gestation   A" / "Antibiotics ... ERCS   B"
   ("A","ERCS delivery should be conducted after 39<sup>+0</sup> weeks of gestation."),
   ("B","Antibiotics should be administered before making the skin incision in women undergoing ERCS. [*New 2015*]"),
   ("✓","All women undergoing ERCS should receive thromboprophylaxis according to existing RCOG guidelines. [*New 2015*]"),
   // GTG 63 m00228: the bold question is not part of the statement
   ("✓","Major or massive APH may result in fetal anaemia and fetal compromise. The neonate should be assessed by a senior paediatrician/neonatologist."),
  ];
  assert_eq!(recs(&b),want.iter().map(|(l,t)|(l.to_string(),t.to_string())).collect::<Vec<_>>());
  assert!(b.iter().all(|x|!matches!(&x.kind,BlockKind::Recommendation{label:Some(l),qualifier,..} if l=="✓" && qualifier.as_deref()!=Some("Good practice point"))));
  let mut nokey:Vec<Block>=b.iter().filter(|x|x.block_id!="key"&&!matches!(x.kind,BlockKind::Recommendation{..})).cloned().collect();assert!(mark_green_top_recommendations(&mut nokey).is_empty());
 }
 /// Real GTG 45 PDF, pages 2–4 (executive summary of recommendations) and 30 (classification key).
 #[tokio::test]async fn gtg45_executive_summary_boxes_from_the_real_pdf(){
  let f=crate::guidelines::fetch::Fetched{body:PDF45.to_vec(),record:crate::guidelines::doc::FetchRecord{url:"https://www-preview.rcog.org.uk/media/kpkjwd5h/gtg_45.pdf".into(),final_url:"https://www-preview.rcog.org.uk/media/kpkjwd5h/gtg_45.pdf".into(),status:200,content_type:Some("application/pdf".into()),..Default::default()}};
  let(mut b,_,_)=blocks_for(&f,None,0).await.unwrap();let w=mark_green_top_recommendations(&mut b);let r=recs(&b);
  assert!(r.len()>=25,"{} recs {w:?}\n{:#?}\n{}",r.len(),r,dump(&b));
  for l in ["A","B","C","D","✓"]{assert!(r.iter().any(|(x,_)|x==l),"grade {l} missing: {r:#?}");}
  assert!(r.iter().any(|(l,t)|l=="D"&&t.starts_with("Planned VBAC is contraindicated in women with previous uterine rupture")&&t.contains("major placenta praevia")),"{r:#?}");
  assert!(r.iter().any(|(l,t)|l=="A"&&t.starts_with("ERCS delivery should be conducted after 39")),"{r:#?}");
  assert!(r.iter().any(|(l,t)|l=="B"&&t.starts_with("Antibiotics should be administered before making the skin incision")),"{r:#?}");
  assert!(r.iter().all(|(_,t)|!t.contains("of 31")&&!t.ends_with('?')),"{r:#?}");
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod r2a_tests{use super::*;use crate::guidelines::sources::pmc_crosswalk::Lookup;
 const GTD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rcog/gtd-38.html"));
 const U38:&str="https://www-preview.rcog.org.uk/guidance/browse-all-guidance/green-top-guidelines/gestational-trophoblastic-disease-green-top-guideline-no-38/";
 fn fx(n:&str)->Vec<u8>{std::fs::read(format!("{}/tests/fixtures/guidelines/r2a_crosswalk/{n}",env!("CARGO_MANIFEST_DIR"))).unwrap()}
 // Real ID Converter answers (2026-11-17): GTD-38's BJOG DOI is not in PMC; GTG 27a's (10.1111/1471-0528.70239) is
 // PMC13485456 without OA XML (OAI cannotDisseminateFormat) -> relation-only on the RCOG document.
 #[tokio::test]async fn stated_bjog_dois_are_crosswalked_without_touching_wiley(){
  let src:SourceEntry=serde_json::from_value(json!({"id":"rcog","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
  f.seed_cache_for_tests(ROOT,format!("<main id='main-content'><a href='{U38}'>item</a></main>").as_bytes(),"text/html");f.seed_cache_for_tests(U38,GTD.as_bytes(),"text/html");
  f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Doi("10.1111/1471-0528.16266".into())),&fx("idconv-rcog-16266.json"),"application/json");
  let rows=Rcog.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),1);
  let c=&rows[0].extra[r2a::CLOSED][0];assert_eq!(c["requested"],"doi:10.1111/1471-0528.16266");assert!(c["reason"].as_str().unwrap().contains("Identifier not found in PMC"));assert!(c["relation"].is_null());
  let d=Rcog.extract(&src,&f,&rows[0]).await.unwrap();assert!(d.warnings.iter().any(|w|w.starts_with("PMC crosswalk: 1 source-stated")));assert_eq!(d.search_metadata["pmc_crosswalk"]["closed"].as_array().unwrap().len(),1);
  // Relation-only case through the same group function (row shaped as discover_site emits it).
  f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Doi("10.1111/1471-0528.70239".into())),&fx("idconv-rcog-70239.json"),"application/json");f.seed_cache_for_tests(&cw::oai_url("PMC13485456").unwrap(),&fx("oai-PMC13485456.xml"),"application/xml");
  let mut row=rows[0].clone();row.extra=json!({"role":"source_summary","publisher_dois":["10.1111/1471-0528.70239"],"assets":[{"role":"publisher_article","doi":"10.1111/1471-0528.70239","url":"https://obgyn.onlinelibrary.wiley.com/doi/10.1111/1471-0528.70239"}]});
  let mut seen=HashSet::new();let(site,open)=crosswalk_group(&src,&f,row,&mut seen).await;assert!(open.is_empty());
  let rels=r2a::site_relations(&src,&site.extra);assert_eq!(rels.len(),1);assert_eq!(rels[0].target,"pmc:PMC13485456");assert!(rels[0].scope.as_deref().unwrap().contains("relation only"));
 }
}
