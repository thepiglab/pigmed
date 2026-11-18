//! Public FMS guideline families and individual Dutch clinical modules.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc,Relation,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::json;
use std::collections::{HashSet,VecDeque};
use url::Url;
pub struct NlFmsRichtlijnendatabase;

fn links(root:ElementRef<'_>,base:&Url, family:bool)->Result<Vec<(String,String)>> {
 let mut out=Vec::new();let mut seen=HashSet::new();
 for a in root.select(&sel("a[href]")) {
  let Some(link)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
  let u=Url::parse(&link)?;
  if u.origin()!=base.origin() {continue;}
  let segments:Vec<_>=u.path_segments().map(|s|s.collect()).unwrap_or_default();
  let valid=segments.first()==Some(&"richtlijn") && if family {segments.len()==2} else {segments.len()>=3 && segments.last().is_some_and(|s|s.ends_with(".html"))};
  if valid && seen.insert(link.clone()){out.push((link,text_of(&a)));}
 }
 Ok(out)
}
fn families(html:&str,base:&Url)->Result<Vec<(String,String)>> {
 let doc=Html::parse_document(html);
 let nav=doc.select(&sel("nav.table-of-contents")).next().context("FMS alphabetical catalog missing")?;
 let rows=links(nav,base,true)?;if rows.is_empty(){bail!("FMS alphabetical catalog is empty");}Ok(rows)
}
fn modules(html:&str,base:&Url,family:&str)->Result<Vec<(String,String)>> {
 let doc=Html::parse_document(html);
 let nav=doc.select(&sel("nav ol.nav-ordered")).next().context("FMS family module navigation missing")?;
 let path=Url::parse(family)?.path().trim_end_matches('/').to_owned()+"/";
 let rows=links(nav,base,false)?.into_iter().filter(|(u,_)|Url::parse(u).is_ok_and(|u|u.path().starts_with(&path))).collect::<Vec<_>>();
 if rows.is_empty(){bail!("FMS family navigation has no module links");} Ok(rows)
}
fn date(label:&str,raw:&str,url:&str)->DateObservation {
 let mut d=common::date(label,raw,url);
 let re=regex::Regex::new(r"^(\d{2})-(\d{2})-(\d{4})$").unwrap();
 if let Some(c)=re.captures(raw.trim()) {d.normalized=Some(format!("{}-{}-{}",&c[3],&c[2],&c[1]));}d
}
/// Page roles observed in the public FMS module tree (2026-11 cache): clinical modules (uitgangsvraag /
/// aanbeveling), family start pages, prose chapters without recommendations (e.g. "Algemeen"), modules whose only
/// substantive content is one linked PDF (nuclear-medicine procedure guidelines), and navigation-only parents.
#[derive(Clone,Copy,Debug,PartialEq)]
enum PageKind {Clinical,Family,TextChapter,AttachmentOnly,Navigation}
/// Characters of prose a page must carry outside its header and accordion metadata to count as a chapter.
const MIN_OWN_TEXT:usize=200;
fn body<'a>(doc:&'a Html)->Result<ElementRef<'a>> {
 let root=doc.select(&sel("article.module.article")).next().context("FMS module article missing")?;
 if root.select(&sel("input[type=password],form[action*='login'],iframe[src*='captcha']")).next().is_some(){bail!("FMS module requires authentication or challenge");}
 Ok(root)
}
/// Text the page itself carries: direct article children except the header and the `*_wrapper` accordion sections
/// (accountability, validity, related products, references), which every module of a family repeats.
fn own_text(root:ElementRef<'_>)->String {
 let mut out=String::new();
 for child in root.children() {
  if let Some(el)=ElementRef::wrap(child) {
   let name=el.value().name();let id=el.value().attr("id").unwrap_or_default();
   if matches!(name,"header"|"script"|"style") || (name=="section" && id.ends_with("_wrapper")) {continue;}
   out.push(' ');out.push_str(&text_of(&el));
  } else if let Some(t)=child.value().as_text() {out.push(' ');out.push_str(t);}
 }
 out.replace('\u{a0}'," ").split_whitespace().collect::<Vec<_>>().join(" ")
}
/// Same-origin PDF attachments listed in the module's own "Bijlagen" (related products) block.
fn attachment_pdfs(root:ElementRef<'_>,base:&Url)->Vec<(String,String)> {
 let mut out=Vec::new();let mut seen=HashSet::new();
 for a in root.select(&sel("#related_products a[href]")) {
  let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
  let Ok(u)=Url::parse(&url) else {continue};
  if u.origin()==base.origin() && u.path().starts_with("/gerelateerde_documenten/file/") && u.path().to_lowercase().ends_with(".pdf") && seen.insert(url.clone()) {out.push((url,text_of(&a)));}
 }
 out
}
fn kind_of(root:ElementRef<'_>,base:&Url)->PageKind {
 if root.select(&sel("#starting_question, #recommendations")).next().is_some() {return PageKind::Clinical;}
 let title=root.select(&sel("header h1")).next().map(|e|text_of(&e)).unwrap_or_default();
 let own=own_text(root).chars().count();
 let metadata=root.select(&sel("#whatsnew, #accountability")).count();
 if (root.select(&sel("#whatsnew")).next().is_some() || title.starts_with("Startpagina")) && (own>=MIN_OWN_TEXT || metadata>=2) {return PageKind::Family;}
 if own>=MIN_OWN_TEXT {return PageKind::TextChapter;}
 if attachment_pdfs(root,base).len()==1 {return PageKind::AttachmentOnly;}
 PageKind::Navigation
}
fn page_kind(html:&str,base:&Url)->Result<PageKind> {let doc=Html::parse_document(html);let root=body(&doc)?;Ok(kind_of(root,base))}
#[cfg(test)]
fn is_family_metadata(html:&str)->Result<bool> {
 let doc=Html::parse_document(html);let root=body(&doc)?;
 Ok(kind_of(root,&Url::parse("https://richtlijnendatabase.nl/").unwrap())==PageKind::Family)
}
fn content(html:&str,base:&Url)->Result<(String,String,Vec<DateObservation>,Vec<Block>,Vec<String>,Vec<serde_json::Value>)> {
 let doc=Html::parse_document(html);let root=body(&doc)?;
 let title=root.select(&sel("header h1")).next().map(|e|text_of(&e)).context("FMS module title missing")?;
 let mut dates=Vec::new();
 for v in root.select(&sel("header div.text-body-secondary")) {
  let label=v.select(&sel("strong")).next().map(|e|text_of(&e)).unwrap_or_default();
  let raw=text_of(&v).trim_start_matches(label.as_str()).trim().to_string();
  if ["Publicatiedatum:","Beoordeeld op geldigheid:"].contains(&label.as_str())&&!raw.is_empty(){dates.push(date(label.trim_end_matches(':'),&raw,base.as_str()));}
 }
 let raw=root.html();let hash=crate::common::sha256_hex(raw.as_bytes());let clinical=kind_of(root,base)==PageKind::Clinical;
 let (mut blocks,mut warnings)=blocks_from_html(&raw,Some(base),Some("article.module.article"));
 for block in &mut blocks {
  if clinical && block.heading_path.last().is_some_and(|h|h=="Aanbeveling") && matches!(block.kind,BlockKind::Paragraph) && !block.text.starts_with("NB:") {
   block.kind=BlockKind::Recommendation{label:None,strength:None,certainty:None,population:None,grading_system:None,qualifier:None};
  }
 }
 if blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count()!=root.select(&sel("table")).count(){warnings.push("Source and extracted table counts differ; review nested tables.".into());}
 warnings.push("Table cell spans, superscript references and images inside tables require visual verification. Evidence certainty is not inferred as recommendation strength.".into());
 let mut seen=HashSet::new();let mut attachments=Vec::new();
 for a in root.select(&sel("a[href]")) {
  let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
  let u=Url::parse(&url)?;
  if (u.path().contains("/gerelateerde_documenten/") || u.path().to_lowercase().ends_with(".pdf")) && seen.insert(url.clone()) {
   attachments.push(json!({"url":url,"label":text_of(&a),"role":"source_linked_related_document","rights":"unknown; linked asset not inspected"}));
  }
 }
 Ok((title,hash,dates,blocks,warnings,attachments))
}
const TERMS_WRAPPER:&str="https://richtlijnendatabase.nl/gebruikersvoorwaarden.html";
fn policy_link(html:&str,base:&Url)->Result<String> {
 let doc=Html::parse_document(html);let root=doc.select(&sel("main.main-content article.article")).next().context("FMS terms wrapper content missing")?;
 let a=root.select(&sel("a[href]")).find(|a|text_of(a).to_lowercase().contains("gebruikersvoorwaarden") && text_of(a).to_lowercase().contains("pdf")).context("FMS source-labelled terms PDF link missing")?;
 let url=common::link(base,a.value().attr("href").unwrap_or_default()).context("invalid FMS policy link")?;let u=Url::parse(&url)?;
 if u.origin()!=base.origin() || !u.path().to_lowercase().ends_with(".pdf"){bail!("FMS policy link is not a same-origin PDF");}Ok(url)
}
fn policy_rights(blocks:&[Block],wrapper:&str,pdf:&str)->Result<Rights> {
 let find=|needle:&str|blocks.iter().find(|b|b.text.to_lowercase().contains(needle)).map(|b|b.text.clone()).with_context(||format!("FMS policy clause missing: {needle}"));
 let ai=find("gebruik van de database voor onderzoek")?;
 let copy=find("te verveelvoudigen")?;
 let automated=find("tekstanalyse (text mining)")?;
 let date=blocks.iter().find(|b|b.text.to_lowercase().contains("versie 1 augustus 2026")).context("FMS policy version date not recognized")?;
 Ok(Rights{status:"restricted".into(),terms_url:Some(pdf.into()),restrictions:vec![ai,copy,automated],evidence_urls:vec![wrapper.into(),pdf.into()],
 note:Some(format!("Policy version stated: {}. Guideline holders and adapted/third-party components can have distinct rights; linked assets require separate review. This policy date is not the clinical module publication date.",date.text)),..Default::default()})
}
fn unresolved(src:&SourceEntry,url:String,title:String,error:String)->PlanRow {
 PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"discovery_error":error})}
}
#[async_trait::async_trait]
impl Adapter for NlFmsRichtlijnendatabase {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}
  let home=f.get_ok(src.catalog_url.as_deref().context("FMS catalog missing")?).await?;
  let base=Url::parse(&home.record.final_url)?;let families=families(&home.text(),&base)?;
  let mut rows=Vec::new();let mut seen=HashSet::new();let mut navigation:Vec<serde_json::Value>=Vec::new();
  for (family,title) in families {
   let page=match f.get_ok(&family).await {Ok(p)=>p,Err(e)=>{let err=format!("{e:#}");let stop=err.contains("403")||err.contains("robots")||err.contains("401");rows.push(unresolved(src,family,title,err));if stop||limit.is_some_and(|n|rows.len()>=n){break;}continue;}};
   let family_base=Url::parse(&page.record.final_url)?;
   if family_base.origin()!=base.origin(){bail!("FMS family redirect crosses origin");}
   let candidates=match modules(&page.text(),&family_base,&family) {Ok(v)=>v,Err(e)=>{rows.push(unresolved(src,family,title,format!("{e:#}")));if limit.is_some_and(|n|rows.len()>=n){break;}continue;}};
   let mut queued:HashSet<String>=candidates.iter().map(|(u,_)|u.clone()).collect();
   let mut queue:VecDeque<_>=candidates.into();
   while let Some((url,title))=queue.pop_front() {
    if !seen.insert(url.clone()){continue;}
    if seen.len()>100000 {bail!("FMS graph exceeded traversal safety bound");}
    let page=match f.get_ok(&url).await {Ok(v)=>v,Err(e)=>{let err=format!("{e:#}");let stop=err.contains("403")||err.contains("robots")||err.contains("401");rows.push(unresolved(src,url,title,err));if stop{return Ok(rows)}if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}continue;}};
    let module_base=Url::parse(&page.record.final_url)?;if module_base.origin()!=base.origin(){rows.push(unresolved(src,url,title,"FMS module redirect crosses origin".into()));continue;}let html=page.text();
    if let Ok(extra)=modules(&html,&module_base,&family){for entry in extra {if queued.insert(entry.0.clone()){queue.push_back(entry);}}}
    let kind=match page_kind(&html,&module_base) {Ok(k)=>k,Err(e)=>{rows.push(unresolved(src,url,title,format!("{e:#}")));if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}continue;}};
    if kind==PageKind::Navigation {
     // A parent page with no prose and no single attachment: its children are queued above; nothing to extract.
     let chars={let doc=Html::parse_document(&html);body(&doc).map(|r|own_text(r).chars().count()).unwrap_or(0)};
     navigation.push(json!({"url":url,"title":title,"own_text_chars":chars}));continue;
    }
    match content(&html,&module_base) {
     Ok((actual,hash,_,_,_,_))=>{
      let key=common::short_hash(&url);
      let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{}-{}",key,&hash[..12]),url:url.clone(),title:Some(actual),rendition:Some("html".into()),version:None,extra:json!({"work_key":key,"family_url":family,"clinical_sha256":hash,"page_kind":format!("{kind:?}")})};
      if kind==PageKind::AttachmentOnly {
       let pdfs={let doc=Html::parse_document(&html);body(&doc).map(|r|attachment_pdfs(r,&module_base)).unwrap_or_default()};
       let (pdf,label)=pdfs.into_iter().next().context("FMS attachment-only page lost its attachment")?;
       row.url=pdf;row.rendition=Some("pdf".into());row.extra["module_url"]=json!(url);row.extra["attachment_label"]=json!(label);
      }
      if !navigation.is_empty(){row.extra["navigation_pages_without_content"]=json!(std::mem::take(&mut navigation));}
      rows.push(row);
     },
     Err(e)=>rows.push(unresolved(src,url,title,format!("{e:#}"))),
    }
    if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
   }
  }
  if !navigation.is_empty() {if let Some(last)=rows.last_mut(){last.extra["navigation_pages_without_content"]=json!(navigation);}}
  Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("FMS discovery unresolved: {e}");}
  let module_url=row.extra["module_url"].as_str().unwrap_or(&row.url).to_string();
  let page=f.get_ok(&module_url).await?;let base=Url::parse(&page.record.final_url)?;
  if base.origin()!=Url::parse(&module_url)?.origin(){bail!("FMS module redirect crosses origin");}
  let html=page.text();let kind=page_kind(&html,&base)?;let family_metadata=kind==PageKind::Family;
  if kind==PageKind::Navigation {bail!("FMS page is navigation-only (no prose, no single attachment); children are separate documents");}
  let (title,hash,dates,mut blocks,mut warnings,mut attachments)=content(&html,&base)?;
  if row.extra["clinical_sha256"].as_str().is_some_and(|h|h!=hash){bail!("FMS clinical article changed since discovery");}
  let mut pdf_fetch=None;
  if row.rendition.as_deref()==Some("pdf") {
   // Attachment-only module: the module page holds metadata; the substantive text is its one linked PDF.
   if kind!=PageKind::AttachmentOnly {bail!("FMS module is no longer attachment-only; re-plan");}
   let pdfs={let listed=Html::parse_document(&html);attachment_pdfs(body(&listed)?,&base)};
   if pdfs.len()!=1 || pdfs[0].0!=row.url {bail!("FMS module attachment changed since discovery; re-plan");}
   let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("FMS module attachment did not return PDF bytes");}
   let (pdf_blocks,notes,_)=super::static_list::blocks_for(&pdf,None,30).await?;
   if pdf_blocks.is_empty(){bail!("FMS module attachment PDF extracted no text");}
   warnings.extend(notes.into_iter().map(|n|format!("FMS attachment PDF: {n}")));
   warnings.push("Composite document: module page metadata (HTML) followed by the module's single linked PDF attachment, which carries the substantive text. PDF layout, figures and tables require visual verification.".into());
   for a in attachments.iter_mut() {if a["url"]==json!(row.url){a["role"]=json!("substantive_attachment_extracted");a["sha256"]=json!(pdf.record.sha256);}}
   blocks.extend(pdf_blocks.into_iter().map(|mut b|{b.block_id=format!("pdf-{}",b.block_id);b}));
   pdf_fetch=Some(pdf.record);
  }
  let mut rights=Rights {status:"unknown".into(),terms_url:Some(TERMS_WRAPPER.into()),note:Some("Source policy could not yet be verified; per-item and asset rights remain unresolved.".into()),..Default::default()};
  let mut fetches=vec![page.record];fetches.extend(pdf_fetch);
  match f.get_ok(TERMS_WRAPPER).await {
   Ok(wrapper)=>{
    let resolved=policy_link(&wrapper.text(),&Url::parse(&wrapper.record.final_url)?);fetches.push(wrapper.record);
    match resolved {
     Ok(url)=>match f.get_ok(&url).await {
      Ok(pdf)=>{
       if !pdf.is_pdf(){warnings.push("FMS source-linked policy did not return PDF bytes; rights remain unknown.".into());fetches.push(pdf.record);}
       else {match super::static_list::blocks_for(&pdf,None,0).await {
        Ok((policy,notes,_))=>{warnings.extend(notes.into_iter().map(|n|format!("FMS policy: {n}")));match policy_rights(&policy,TERMS_WRAPPER,&url) {Ok(r)=>rights=r,Err(e)=>warnings.push(format!("FMS policy could not be verified: {e:#}"))}},
        Err(e)=>warnings.push(format!("FMS policy PDF conversion failed: {e:#}")),
       }fetches.push(pdf.record);}
      },Err(e)=>warnings.push(format!("FMS policy PDF unavailable: {e:#}")),
     },Err(e)=>warnings.push(format!("FMS policy link unavailable: {e:#}")),
    }
   },Err(e)=>warnings.push(format!("FMS policy wrapper unavailable: {e:#}")),
  }
  if family_metadata {warnings.push("Family metadata and overview only; individual clinical modules are separate documents, not contained here.".into());}
  if kind==PageKind::TextChapter {warnings.push("Prose chapter without a source uitgangsvraag/aanbeveling section; no recommendations are structured.".into());}
  warnings.push("Homepage and observed module navigation are traversed; completeness across alternate templates, appendices and historical editions is unverified. Related documents other than an attachment-only module's single PDF are links, not extracted bodies.".into());
  let relations=row.extra["family_url"].as_str().map(|u|vec![Relation{rel:"part_of".into(),target:u.into(),scope:Some("Individual module of this guideline family".into()),evidence_url:Some(module_url.clone())}]).unwrap_or_default();
  let published=dates.iter().find(|d|d.label=="Publicatiedatum").and_then(|d|d.normalized.clone());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),item_type:Some(if family_metadata {"guideline_family_metadata"} else {"guideline_chapter"}.into()),title,url:module_url.clone(),status:"unknown".into(),published,date_observations:dates,rights,relations,language:None,country:None,blocks,attachments,fetches,warnings,..Default::default()})
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
 use super::*;
 const TERMS_HTML:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/terms-wrapper.html"));
 const TERMS_PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/terms-2026-08-01.pdf"));
 const HOME:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/home.html"));
 const FAMILY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/family.html"));
 const SIBLING:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/sibling.html"));
 const MODULE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/module.html"));
 const FAMILY_URL:&str="https://richtlijnendatabase.nl/richtlijn/diabetes_mellitus_type_2_in_de_tweede_lijn";
 const START:&str="https://richtlijnendatabase.nl/richtlijn/diabetes_mellitus_type_2_in_de_tweede_lijn/startpagina_-_diabetes_mellitus_type_2_in_de_tweede_lijn.html";
 const MODULE_URL:&str="https://richtlijnendatabase.nl/richtlijn/diabetes_mellitus_type_2_in_de_tweede_lijn/farmacotherapie_bij_dm2_in_de_tweede_lijn/glp-1_receptor_agonist_versus_basaal_insuline.html";
 #[test]
 fn actual_index_family_and_full_module() {
  let home=Url::parse("https://richtlijnendatabase.nl/").unwrap();
  assert!(families(HOME,&home).unwrap().iter().any(|(u,_)|u==FAMILY_URL));
  let list=modules(FAMILY,&Url::parse(START).unwrap(),FAMILY_URL).unwrap();
  assert!(list.len()>=13);assert!(list.iter().any(|(u,_)|u==MODULE_URL));
  let base=Url::parse(MODULE_URL).unwrap();let (title,_,dates,blocks,_,links)=content(MODULE,&base).unwrap();
  assert_eq!(title,"GLP-1 receptor agonist versus basaal insuline");
  assert!(dates.iter().any(|d|d.label=="Publicatiedatum"&&d.normalized.as_deref()==Some("2018-04-16")));
  assert!(dates.iter().any(|d|d.label=="Beoordeeld op geldigheid"&&d.normalized.as_deref()==Some("2018-04-03")));
  assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),17);
  assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),3);
  assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})&&b.text.contains("Geef insulinebehandeling")));
  assert!(blocks.iter().any(|b|b.text.contains("bij wie het vermijden van hypoglykemieën")));
  assert!(blocks.iter().any(|b|b.text.contains("oktober 2016")));
  assert!(blocks.iter().any(|b|b.text.contains("Beoordelingsdatum en geldigheid")));
  assert!(links.iter().any(|v|v["label"]=="Implementatieplan"));
  let (title,_,dates,family_blocks,_,_)=content(FAMILY,&Url::parse(START).unwrap()).unwrap();
  assert!(is_family_metadata(FAMILY).unwrap());assert!(title.starts_with("Startpagina"));
  assert!(dates.iter().any(|d|d.label=="Publicatiedatum" && d.normalized.as_deref()==Some("2026-05-02")));
  for heading in ["Beoordelingsdatum en geldigheid","Initiatief en autorisatie","Bijlagen"] {assert!(family_blocks.iter().any(|b|b.text.contains(heading)));}
  assert!(family_blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})));
 }
 #[test]
 fn actual_sibling_module_has_its_own_content_and_tables() {
  let url=Url::parse("https://richtlijnendatabase.nl/richtlijn/diabetes_mellitus_type_2_in_de_tweede_lijn/farmacotherapie_bij_dm2_in_de_tweede_lijn/dpp-4-remmer_versus_bolus_insuline.html").unwrap();
  let (title,_,dates,blocks,warnings,_)=content(SIBLING,&url).unwrap();
  assert_eq!(title,"DPP-4-remmer versus bolus insuline");
  assert!(dates.iter().any(|d|d.label=="Publicatiedatum" && d.normalized.as_deref()==Some("2018-04-16")));
  assert!(dates.iter().any(|d|d.label=="Beoordeeld op geldigheid" && d.normalized.as_deref()==Some("2021-12-16")));
  let recs=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect::<Vec<_>>();
  assert_eq!(recs.len(),4);assert!(recs.iter().any(|b|b.text.contains("ten minste 5 mmol/mol")));
  assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),5);
  assert!(blocks.iter().any(|b|b.heading_path.iter().any(|h|h=="Aanbeveling")));
  assert!(!warnings.iter().any(|w|w.contains("table counts differ")));
 }
 #[tokio::test]
 async fn cached_module_keeps_own_date_and_family_relationship() {
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,500,1<<20,"").unwrap();
  f.seed_cache_for_tests(MODULE_URL,MODULE.as_bytes(),"text/html");
  let source_link=HOME.lines().find(|line|line.contains("diabetes_mellitus_type_2_in_de_tweede_lijn")).unwrap();
  let one_family=format!("<nav class=\"table-of-contents\"><ul>{source_link}</ul></nav>");
  f.seed_cache_for_tests("https://richtlijnendatabase.nl/",one_family.as_bytes(),"text/html");
  f.seed_cache_for_tests(FAMILY_URL,FAMILY.as_bytes(),"text/html");f.seed_cache_for_tests(START,FAMILY.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"nl_fms_richtlijnendatabase","access":"public","enabled":true,"catalog_url":"https://richtlijnendatabase.nl/"})).unwrap();
  let discovered=NlFmsRichtlijnendatabase.discover(&src,&f,Some(1)).await.unwrap();
  assert_eq!(discovered.len(),1);assert_eq!(discovered[0].url,START);
  assert!(discovered[0].extra["clinical_sha256"].is_string());
  let family_doc=NlFmsRichtlijnendatabase.extract(&src,&f,&discovered[0]).await.unwrap();
  assert_eq!(family_doc.item_type.as_deref(),Some("guideline_family_metadata"));
  assert_eq!(family_doc.published.as_deref(),Some("2026-05-02"));
  let (_,hash,_,_,_,_)=content(MODULE,&Url::parse(MODULE_URL).unwrap()).unwrap();
  let row=PlanRow{source_id:src.id.clone(),doc_key:"test".into(),url:MODULE_URL.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({"work_key":"module","family_url":FAMILY_URL,"clinical_sha256":hash})};
  let doc=NlFmsRichtlijnendatabase.extract(&src,&f,&row).await.unwrap();
  assert_eq!(doc.published.as_deref(),Some("2018-04-16"));assert_eq!(doc.status,"unknown");
  assert_eq!(doc.rights.status,"unknown");assert_eq!(doc.relations[0].target,FAMILY_URL);
  assert!(doc.blocks.iter().any(|b|b.text.contains("GLP-1 receptor agonist")));
 }
 #[tokio::test]
 async fn source_policy_pdf_is_restricted_with_distinct_provenance() {
  let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,1<<20,"").unwrap();
  let policy="https://richtlijnendatabase.nl/uploaded/docs/Gebruikersvoorwaarden_Richtlijnendatabase.pdf?u=1gSfkV";
  f.seed_cache_for_tests(MODULE_URL,MODULE.as_bytes(),"text/html");f.seed_cache_for_tests(TERMS_WRAPPER,TERMS_HTML.as_bytes(),"text/html");f.seed_cache_for_tests(policy,TERMS_PDF,"application/pdf");
  let src:SourceEntry=serde_json::from_value(json!({"id":"nl_fms_richtlijnendatabase","access":"public","enabled":true})).unwrap();
  let (_,hash,_,_,_,_)=content(MODULE,&Url::parse(MODULE_URL).unwrap()).unwrap();
  let row=PlanRow{source_id:src.id.clone(),doc_key:"policy-fixture".into(),url:MODULE_URL.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({"clinical_sha256":hash})};
  let doc=NlFmsRichtlijnendatabase.extract(&src,&f,&row).await.unwrap();
  assert_eq!(doc.rights.status,"restricted");assert!(doc.rights.restrictions.iter().any(|s|s.contains("kunstmatige intelligentie")));
  assert!(doc.rights.restrictions.iter().any(|s|s.contains("tekstanalyse (text mining)")));
  assert_eq!(doc.rights.terms_url.as_deref(),Some(policy));assert!(doc.rights.note.as_deref().unwrap().contains("1 augustus 2026"));
  assert_eq!(doc.published.as_deref(),Some("2018-04-16"));assert!(doc.fetches.iter().any(|r|r.url==policy));
  assert!(policy_link("<main>empty</main>",&Url::parse(TERMS_WRAPPER).unwrap()).is_err());
 }

}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod page_kind_tests {
 //! Template variants seen in the 2026-11-14 probe (22q13 family) and the cached nuclear-medicine family.
 use super::*;
 const START:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/start-22q13.html"));
 const ALGEMEEN:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/algemeen-22q13.html"));
 const ATTACH:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/attachment-only-carbon-14.html"));
 const NAV:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/navigation-part-i.html"));
 const C14_PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/carbon-14-urea-text-only.pdf"));
 const MODULE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/module.html"));
 const FAMILY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nl_fms_richtlijnendatabase/family.html"));
 const FAM22:&str="https://richtlijnendatabase.nl/richtlijn/22q13_deletiesyndroom_pms";
 const START_URL:&str="https://richtlijnendatabase.nl/richtlijn/22q13_deletiesyndroom_pms/startpagina_-_22q13ds_pms.html";
 const ALG_URL:&str="https://richtlijnendatabase.nl/richtlijn/22q13_deletiesyndroom_pms/algemeen_-_22q13ds_pms.html";
 const C14_URL:&str="https://richtlijnendatabase.nl/richtlijn/nucleaire_geneeskunde/part_iii_-_radiopharmaceuticals/carbon-14_ang.html";
 const C14_PDF_URL:&str="https://richtlijnendatabase.nl/gerelateerde_documenten/file/17275/1/14C%20Urea.pdf";
 const NAV_URL:&str="https://richtlijnendatabase.nl/richtlijn/nucleaire_geneeskunde/part_i_-_diagnostic_methods.html";
 fn u(s:&str)->Url{Url::parse(s).unwrap()}
 fn src()->SourceEntry{serde_json::from_value(json!({"id":"nl_fms_richtlijnendatabase","access":"public","enabled":true,"catalog_url":"https://richtlijnendatabase.nl/"})).unwrap()}
 #[test]
 fn every_observed_template_has_a_role() {
  assert_eq!(page_kind(START,&u(START_URL)).unwrap(),PageKind::Family);
  assert_eq!(page_kind(ALGEMEEN,&u(ALG_URL)).unwrap(),PageKind::TextChapter);
  assert_eq!(page_kind(ATTACH,&u(C14_URL)).unwrap(),PageKind::AttachmentOnly);
  assert_eq!(page_kind(NAV,&u(NAV_URL)).unwrap(),PageKind::Navigation);
  assert_eq!(page_kind(MODULE,&u(C14_URL)).unwrap(),PageKind::Clinical);
  assert_eq!(page_kind(FAMILY,&u(START_URL)).unwrap(),PageKind::Family);
  let doc=Html::parse_document(ATTACH);let pdfs=attachment_pdfs(body(&doc).unwrap(),&u(C14_URL));
  assert_eq!(pdfs.len(),1);assert_eq!(pdfs[0].0,C14_PDF_URL);assert_eq!(pdfs[0].1,"14C Urea.pdf");
  let doc=Html::parse_document(NAV);assert!(own_text(body(&doc).unwrap()).chars().count()<MIN_OWN_TEXT);
  let doc=Html::parse_document(START);assert!(own_text(body(&doc).unwrap()).contains("Waar gaat deze richtlijn over?"));
 }
 #[tokio::test]
 async fn start_page_and_prose_chapter_are_documents_not_failures() {
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
  f.seed_cache_for_tests("https://richtlijnendatabase.nl/",format!("<nav class=\"table-of-contents\"><ul><li><a href=\"/richtlijn/22q13_deletiesyndroom_pms\">22q13</a></li></ul></nav>").as_bytes(),"text/html");
  f.seed_cache_for_tests(FAM22,START.as_bytes(),"text/html");f.seed_cache_for_tests(START_URL,START.as_bytes(),"text/html");f.seed_cache_for_tests(ALG_URL,ALGEMEEN.as_bytes(),"text/html");
  let rows=NlFmsRichtlijnendatabase.discover(&src(),&f,Some(2)).await.unwrap();
  assert_eq!(rows.len(),2);assert!(rows.iter().all(|r|r.extra["discovery_error"].is_null()));
  assert_eq!(rows[0].url,START_URL);assert_eq!(rows[0].extra["page_kind"],"Family");
  assert_eq!(rows[1].url,ALG_URL);assert_eq!(rows[1].extra["page_kind"],"TextChapter");
  let start=NlFmsRichtlijnendatabase.extract(&src(),&f,&rows[0]).await.unwrap();
  assert_eq!(start.item_type.as_deref(),Some("guideline_family_metadata"));assert_eq!(start.published.as_deref(),Some("2018-12-07"));
  assert!(start.blocks.iter().any(|b|b.text.contains("Phelan-McDermid")));
  let alg=NlFmsRichtlijnendatabase.extract(&src(),&f,&rows[1]).await.unwrap();
  assert_eq!(alg.item_type.as_deref(),Some("guideline_chapter"));assert_eq!(alg.title,"Algemeen - 22q13DS (PMS)");
  assert!(alg.blocks.iter().any(|b|b.text.contains("SHANK3")));
  assert!(!alg.blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})));
  assert!(alg.warnings.iter().any(|w|w.contains("Prose chapter")));
  assert_eq!(alg.relations[0].target,FAM22);
 }
 #[tokio::test]
 async fn attachment_only_module_extracts_its_single_pdf_and_navigation_is_recorded() {
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
  let fam="https://richtlijnendatabase.nl/richtlijn/nucleaire_geneeskunde";
  f.seed_cache_for_tests("https://richtlijnendatabase.nl/",b"<nav class=\"table-of-contents\"><ul><li><a href=\"/richtlijn/nucleaire_geneeskunde\">NG</a></li></ul></nav>","text/html");
  // Bounded family navigation: the real navigation parent, then the real attachment-only module.
  let nav=format!("<nav><ol class=\"nav-ordered\"><li><a href=\"{NAV_URL}\">Part I</a></li><li><a href=\"{C14_URL}\">Carbon-14 ANG</a></li></ol></nav>");
  f.seed_cache_for_tests(fam,nav.as_bytes(),"text/html");
  f.seed_cache_for_tests(NAV_URL,NAV.as_bytes(),"text/html");f.seed_cache_for_tests(C14_URL,ATTACH.as_bytes(),"text/html");f.seed_cache_for_tests(C14_PDF_URL,C14_PDF,"application/pdf");
  let rows=NlFmsRichtlijnendatabase.discover(&src(),&f,Some(1)).await.unwrap();
  assert_eq!(rows.len(),1);let r=&rows[0];
  assert_eq!(r.url,C14_PDF_URL);assert_eq!(r.rendition.as_deref(),Some("pdf"));assert_eq!(r.extra["module_url"],C14_URL);
  assert_eq!(r.extra["navigation_pages_without_content"][0]["url"],NAV_URL);
  let d=NlFmsRichtlijnendatabase.extract(&src(),&f,r).await.unwrap();
  assert_eq!(d.item_type.as_deref(),Some("guideline_chapter"));assert_eq!(d.url,C14_URL);assert_eq!(d.title,"Carbon-14 ANG");
  assert!(d.blocks.iter().any(|b|b.block_id.starts_with("pdf-") && b.text.contains("Helicobacter pylori")));
  assert!(d.attachments.iter().any(|a|a["role"]=="substantive_attachment_extracted"));
  assert!(d.fetches.iter().any(|x|x.url==C14_PDF_URL));
  assert!(d.warnings.iter().any(|w|w.contains("Composite document")));
  let nav_row=PlanRow{source_id:"nl_fms_richtlijnendatabase".into(),doc_key:"nav".into(),url:NAV_URL.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({})};
  assert!(NlFmsRichtlijnendatabase.extract(&src(),&f,&nav_row).await.is_err());
 }
}
