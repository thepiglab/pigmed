//! ASAM guideline topics and source-labelled public full guideline PDFs.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use super::pmc_crosswalk::{self as cw,SourceRef};
use super::r2b_crosswalk::{self as xw,Cross};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct Asam;
const ROOT:&str="https://www.asam.org/quality-care/clinical-guidelines";
fn catalog(raw:&str,base:&Url)->Result<Vec<(String,String)>> {
 let doc=Html::parse_document(raw);let mut seen=HashSet::new();let mut out=vec![];
 for a in doc.select(&sel("main [data-placeholder-label='Page Content Section'] h3 a[href]")) {let title=text_of(&a);let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};let u=Url::parse(&url)?;
  if u.origin()!=base.origin() || !u.path().starts_with("/quality-care/clinical-guidelines/") || !seen.insert(url.clone()){continue;}out.push((url,title));}
 if out.is_empty(){bail!("ASAM guideline topic inventory missing")};Ok(out)
}
fn topic(raw:&str,base:&Url)->Result<(String,Vec<Value>)> {
 let doc=Html::parse_document(raw);let root=doc.select(&sel("main")).next().context("ASAM topic main missing")?;let mut seen=HashSet::new();let mut full=vec![];let mut assets=vec![];
 for a in root.select(&sel("a[href]")){let label=text_of(&a);let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};let u=Url::parse(&url)?;
  if !seen.insert(url.clone()) || !u.path().to_lowercase().ends_with(".pdf"){continue;}let l=label.to_lowercase();
  let excluded=["pocket","summary","slide","appendix","supplement","flowchart","algorithm","patient","fact sheet","quick reference"];
  let candidate=(l.contains("full guideline") || l.contains("complete guideline") || l.contains("full clinical practice guideline") || l=="download guideline" || l=="download the guideline") && !excluded.iter().any(|x|l.contains(x));
  if candidate{full.push(url.clone())}assets.push(json!({"url":url,"label":label,"role":if candidate {"candidate_full_guideline"}else{"supporting_pdf"},"rights":"unknown; independent asset"}));
 }
 if full.len()!=1 {bail!("ASAM expected one explicitly labelled full guideline PDF; found {}",full.len())}Ok((full.remove(0),assets))
}
fn special_topic(raw:&str,base:&Url)->Result<Option<Vec<(String,String,String)>>>{let doc=Html::parse_document(raw);let root=doc.select(&sel("main")).next().context("ASAM special topic main missing")?;let headings=root.select(&sel("h1")).map(|h|text_of(&h)).collect::<Vec<_>>();let has=|s:&str|headings.iter().any(|h|h==s);let mut out=vec![];for a in root.select(&sel("a[href]")){let label=text_of(&a);let role=if has("Stimulant Use Disorder")&&label=="Download Guideline"{Some("journal_guideline")}else if has("Stimulant Use Disorder")&&label.trim_matches(|c:char|c.is_whitespace()||c=='\u{a0}')=="Evidence-to-Decision Tables"{Some("evidence_to_decision_tables")}else if has("Appropriate Use of Drug Testing in Clinical Addiction Medicine Consensus Document")&&label=="Download"&&a.ancestors().filter_map(scraper::ElementRef::wrap).any(|e|e.value().classes().any(|c|c=="hero")){Some("consensus_document")}else if has("Clinical Practice Guideline Methodology")&&label=="Download Full Methodology"{Some("methodology")}else if has("Standards and Performance Measures")&&label=="Standards of Care for the Addiction Specialist Physician Document"{Some("standards_of_care")}else if has("Standards and Performance Measures")&&label=="Performance Measures for the Addiction Specialist Physician"{Some("performance_measures")}else{None};let Some(role)=role else{continue};if let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)){if !out.iter().any(|(u,_,_)|u==&url){out.push((url,role.into(),label));}}}if has("Stimulant Use Disorder")||has("Appropriate Use of Drug Testing in Clinical Addiction Medicine Consensus Document")||has("Clinical Practice Guideline Methodology")||has("Standards and Performance Measures"){if out.is_empty(){bail!("ASAM recognized special topic has no expected source-labelled terminal")};Ok(Some(out))}else{Ok(None)}}
/// LWW full-text URL parts as printed in the society link (journal slug, year, issue, article slug). No DOI is stated
/// on the ASAM page, so none is recorded; the shared PMC crosswalk resolves by citation.
fn lww_locator(url:&str)->Value{let Ok(u)=Url::parse(url) else {return Value::Null};if u.host_str()!=Some("journals.lww.com"){return Value::Null}let p:Vec<&str>=u.path_segments().map(|s|s.filter(|x|!x.is_empty()).collect()).unwrap_or_default();if p.len()==5&&p[1].eq_ignore_ascii_case("fulltext"){json!({"host":"journals.lww.com","journal":p[0],"year":p[2],"issue":p[3],"article":p[4].trim_end_matches(".aspx"),"url":url})}else{json!({"host":"journals.lww.com","url":url})}}
/// LWW journal slug → print ISSN (as Europe PMC indexes it).
fn lww_issn(journal:&str)->Option<&'static str>{match journal{"journaladdictionmedicine"=>Some("1932-0620"),_=>None}}
/// Round 2: the PMC copy of the journal-hosted guideline. ASAM states no DOI, only the LWW link (journal, year, title
/// slug); the article is identified through Europe PMC (ISSN + year + ASAM topic title, unique hit whose title starts
/// with the printed slug), then PMC OAI: full text when distributed (licence-less → restricted), else relation only.
async fn asam_cross(src:&SourceEntry,f:&Fetcher,cx:&mut xw::Session,target:&str,topic_url:&str,topic_title:&str)->(Option<PlanRow>,Cross){
 let fail=|status:&'static str,reason:String|(None,Cross{row:None,relation:None,status,evidence:json!({"status":status,"reason":reason,"url":target}),reason:Some(reason)});
 let loc=lww_locator(target);
 let (Some(issn),Some(year),Some(article))=(loc["journal"].as_str().and_then(lww_issn),loc["year"].as_str(),loc["article"].as_str()) else {return fail("no_identifier","the ASAM link is not an LWW full-text link of a known journal".into())};
 let slug=article.rsplit_once('.').filter(|(_,n)|!n.is_empty()&&n.bytes().all(|b|b.is_ascii_digit())).map(|(a,_)|a).unwrap_or(article);
 let (lookup,ev)=match xw::doi_for_slug(f,issn,year,slug,topic_title).await{Ok(x)=>x,Err(e)=>return fail("citation_unresolved",e)};
 let sref=SourceRef{source_url:target.into(),evidence_url:Some(topic_url.into()),source_title:None,item_type:Some("clinical_practice_guideline".into()),key_prefix:"asam-pmc".into(),id_route:"source_citation_slug".into(),notes:vec![]};
 let mut c=cx.try_id(src,f,lookup,sref,json!({"topic_url":topic_url,"publisher_locator":loc,"citation_resolution":ev,"source_role":"journal_guideline"}),None).await;let row=c.row.take();(row,c)}
fn recommendations(blocks:&mut Vec<Block>)->Vec<String> {
 let label=regex::Regex::new(r"(?i)Recommendation\s+([IVX]+\.\d+)\s*:").unwrap();let mut out=vec![];let mut warnings=vec![];
 for (i,b) in blocks.iter().enumerate(){if matches!(b.kind,BlockKind::Table{..}){continue;}let caps=label.captures_iter(&b.text).collect::<Vec<_>>();
  for pair in caps.windows(2){let m=pair[0].get(0).unwrap();let end=pair[1].get(0).unwrap().start();let text=b.text[m.end()..end].trim();if text.is_empty(){continue;}
   out.push((i,Block{block_id:format!("{}-recommendation-{}",b.block_id,&pair[0][1]),kind:BlockKind::Recommendation{label:Some(pair[0][1].to_string()),strength:None,certainty:None,population:None,grading_system:None,qualifier:None},text:text.into(),heading_path:b.heading_path.clone(),page:b.page}));}
  if !caps.is_empty(){warnings.push(format!("ASAM {} has a last or cross-block numbered statement not safely bounded by a following label; source retained.",b.block_id));}
 }
 for (i,b) in out.into_iter().rev(){blocks.insert(i+1,b);}warnings.push("Only explicit numbered statements bounded within one source block are structured; split labels, nested criteria, repeated summary/detail recommendations, OUD bullets, figures and all PDF page layout require source review.".into());warnings
}
fn rights(blocks:&[Block],url:&str)->Rights {
 let source=blocks.iter().take(80).map(|b|b.text.as_str()).find(|t|{let l=t.to_lowercase();l.contains("copyright") && (l.contains("unauthorized reproduction") || (l.contains("systematic reproduction")&&l.contains("permission")))});
 if let Some(n)=source{Rights{status:"restricted".into(),restrictions:vec![n.into()],evidence_urls:vec![url.into()],note:Some("Restriction is specific to this manuscript; other ASAM works and supplements require independent rights review.".into()),..Default::default()}}else{Rights{status:"unknown".into(),..Default::default()}}
}
fn pdf_dates(blocks:&[Block],role:Option<&str>,url:&str)->Vec<crate::guidelines::doc::DateObservation>{let board=regex::Regex::new(r"(?i)Adopted by (?:the )?ASAM Board of Directors\s+((?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{1,2},\s+\d{4})").unwrap();let method=regex::Regex::new(r"(?i)^Adopted\s+((?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{1,2},\s+\d{4})$").unwrap();blocks.iter().take(80).find_map(|b|board.captures(&b.text).or_else(||if role==Some("methodology"){method.captures(b.text.trim())}else{None}).map(|c|vec![common::date("Adopted",&c[1],url)])).unwrap_or_default()}
#[async_trait::async_trait]
impl Adapter for Asam {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let cat=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;let entries=catalog(&cat.text(),&Url::parse(&cat.record.final_url)?)?;let mut rows=vec![];let mut cx=xw::Session::default();
  for (url,title) in entries {let work=common::slug(Url::parse(&url)?.path());let maybe_page=f.get_ok(&url).await;let special=match &maybe_page{Ok(p)=>special_topic(&p.text(),&Url::parse(&p.record.final_url)?),Err(_)=>Ok(None)};match special{Ok(Some(assets))=>{let start=rows.len();let mut topic_cross:Vec<Cross>=vec![];let mut stop=false;for(target,role,label)in assets{let is_journal=role=="journal_guideline";let mut r=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-{}-unresolved",common::short_hash(&target)),url:target.clone(),title:Some(title.clone()),rendition:Some(if is_journal{"html"}else{"pdf"}.into()),version:None,extra:json!({"work_key":work,"topic_url":url,"catalog_fetch":cat.record,"topic_fetch":maybe_page.as_ref().ok().map(|p|&p.record),"source_role":role,"source_label":label})};match f.get_ok(&target).await{Ok(p) if !is_journal&&p.is_pdf()=>{r.doc_key=format!("{work}-{}",&p.record.sha256[..12]);r.extra["pdf_sha256"]=json!(p.record.sha256);},Ok(_) if is_journal=>r.extra["discovery_error"]=json!("ASAM publisher HTML full guideline template and rights not yet verified"),Err(e) if is_journal=>{r.extra["discovery_error"]=json!(format!("{e:#}"));r.extra["oa_route"]=json!("pmc_crosswalk_by_citation");r.extra["publisher_locator"]=lww_locator(&target);},Ok(_)=>r.extra["discovery_error"]=json!("ASAM publisher-labelled document did not return PDF"),Err(e)=>r.extra["discovery_error"]=json!(format!("{e:#}"))};
   if is_journal&&r.extra["discovery_error"].is_string(){let(open,c)=asam_cross(src,f,&mut cx,&target,&url,&title).await;match open{Some(o)=>{rows.push(o);topic_cross.push(c);if limit.is_some_and(|n|rows.len()>=n){stop=true;break}continue},None if c.status=="duplicate"=>continue,None=>{let e=r.extra["discovery_error"].as_str().unwrap_or_default().to_string();r.extra["discovery_error"]=json!(format!("{e}; PMC crosswalk {}: {}",c.status,c.reason.clone().unwrap_or_default()));xw::record_on(&mut r,&c);topic_cross.push(c);}}}
   rows.push(r);if limit.is_some_and(|n|rows.len()>=n){stop=true;break}}
   // The topic's own ASAM documents (e.g. the Evidence-to-Decision tables) carry the PMC link of the journal guideline.
   for c in &topic_cross{for r in rows[start..].iter_mut().filter(|r|r.extra["discovery_error"].is_null()&&!cw::is_crosswalk_row(r)){xw::record_on(r,c)}}
   if stop{return Ok(rows)}continue},Err(e)=>{rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:url.clone(),title:Some(title),rendition:None,version:None,extra:json!({"discovery_error":format!("{e:#}")})});if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}continue},Ok(None)=>{}}let edition=title.split_whitespace().next().filter(|v|v.len()==4 && v.chars().all(|c|c.is_ascii_digit())).map(String::from);let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:url.clone(),title:Some(title),rendition:None,version:edition,extra:json!({"work_key":work,"catalog_fetch":&cat.record})};
   match maybe_page {Ok(p)=>match topic(&p.text(),&Url::parse(&p.record.final_url)?){Ok((pdf,assets))=>{row.url=pdf;row.rendition=Some("pdf".into());row.extra["topic_url"]=json!(url);row.extra["topic_fetch"]=json!(&p.record);row.extra["assets"]=json!(assets);},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
   if row.rendition.as_deref()==Some("pdf"){match f.get_ok(&row.url).await {Ok(p) if p.is_pdf()=>{row.doc_key=format!("{work}-{}",&p.record.sha256[..12]);row.extra["pdf_sha256"]=json!(p.record.sha256);},Ok(_)=>row.extra["discovery_error"]=json!("ASAM full guideline asset is not PDF"),Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}}
   rows.push(row);if limit.is_some_and(|n|rows.len()>=n){break;}
  }Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if cw::is_crosswalk_row(row){return xw::extract(src,f,row).await}if let Some(e)=row.extra["discovery_error"].as_str(){bail!("ASAM discovery: {e}")}let p=f.get_ok(&row.url).await?;if !p.is_pdf(){bail!("ASAM full guideline is not PDF")};
  if row.extra["pdf_sha256"].as_str().is_some_and(|h|h!=p.record.sha256){bail!("ASAM PDF changed since planning; re-plan")}
  let (mut blocks,mut warnings,_)=blocks_for(&p,None,30).await?;if blocks.is_empty(){bail!("ASAM PDF has no extracted text")};let rights=rights(&blocks,&row.url);if !matches!(row.extra["source_role"].as_str(),Some("methodology"|"standards_of_care"|"performance_measures"|"evidence_to_decision_tables")){warnings.extend(recommendations(&mut blocks));}else{warnings.push("Source item is a methodology, standards, performance-measure or evidence-to-decision-table document; not a clinical guideline recommendation set.".into());}warnings.push("PDF images, flowcharts, tables, physical page boundaries and linked supplements remain unvalidated; linked assets are unacquired. No year, edition or current status is inferred from URL or fetch time.".into());
  let dates=pdf_dates(&blocks,row.extra["source_role"].as_str(),&row.url);
  let mut fetches=vec![];for key in ["catalog_fetch","topic_fetch"]{if let Ok(f)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra[key].clone()){fetches.push(f);}}fetches.push(p.record);
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some(row.extra["source_role"].as_str().unwrap_or("guideline").into()),version:row.version.clone(),status:"unknown".into(),rights,date_observations:dates,relations:xw::relations_from_extra(&row.extra,&[]),blocks,attachments:row.extra["assets"].as_array().cloned().unwrap_or_default(),fetches,warnings,search_metadata:json!({"topic_url":row.extra["topic_url"]}),..Default::default()})
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const ATOP:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/alcohol-topic.html"));
 const OTOP:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/oud-topic.html"));
 const APDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/alcohol-page-5.pdf"));
 const OPDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/oud-page-4.pdf"));
 const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/catalog.html"));
 const ALCOHOL:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/alcohol-blocks.json"));
 #[test] fn source_topic_inventory_and_label_boundaries(){
  let rows=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();
  assert!(rows.iter().any(|(u,_)|u.ends_with("/alcohol-withdrawal-management-guideline")));
  assert!(rows.iter().any(|(u,_)|u.ends_with("/national-practice-guideline")));
  let mut b:Vec<Block>=serde_json::from_str(ALCOHOL).unwrap();let warnings=recommendations(&mut b);
  let recs:Vec<_>=b.iter().filter(|x|matches!(x.kind,BlockKind::Recommendation{..})).collect();assert_eq!(recs.len(),3);
  assert!(matches!(&recs[0].kind,BlockKind::Recommendation{label:Some(l),strength:None,certainty:None,..} if l=="I.1"));assert!(recs[0].text.contains("universal screening") && !recs[0].text.contains("Recommendation I.2"));
  assert!(matches!(&recs[1].kind,BlockKind::Recommendation{label:Some(l),..} if l=="I.2") && recs[1].text.contains("even in the absence of signs and symptoms"));
  assert!(matches!(&recs[2].kind,BlockKind::Recommendation{label:Some(l),..} if l=="I.3") && recs[2].text.contains("collateral sources") && recs[2].text.contains("hepatic function"));
  assert!(!recs.iter().any(|x|matches!(&x.kind,BlockKind::Recommendation{label:Some(l),..} if l=="I.4")));
  assert!(warnings.iter().any(|w|w.contains("cross-block")));
 }
 #[tokio::test] async fn genuine_topic_pdf_paths_and_independent_rights_offline(){
  let anchors={let d=Html::parse_document(CAT);let mut out=Vec::new();for suffix in ["/alcohol-withdrawal-management-guideline","/national-practice-guideline"] {out.push(d.select(&sel("main [data-placeholder-label='Page Content Section'] h3 a[href]")).find(|a|a.value().attr("href").is_some_and(|h|h.ends_with(suffix))).unwrap().html());}out};
  let bounded=format!("<main><div data-placeholder-label='Page Content Section'><h3>{}</h3><h3>{}</h3></div></main>",anchors[0],anchors[1]);let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,16<<20,"").unwrap();f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");
  let at="https://www.asam.org/quality-care/clinical-guidelines/alcohol-withdrawal-management-guideline";let ot="https://www.asam.org/quality-care/clinical-guidelines/national-practice-guideline";
  let ap="https://downloads.asam.org/sitefinity-production-blobs/docs/default-source/quality-science/the_asam_clinical_practice_guideline_on_alcohol-1.pdf?sfvrsn=ba255c2_0";let op="https://downloads.asam.org/sitefinity-production-blobs/docs/default-source/guidelines/npg-jam-supplement.pdf?sfvrsn=a00a52c2_4";
  f.seed_cache_for_tests(at,ATOP.as_bytes(),"text/html");f.seed_cache_for_tests(ot,OTOP.as_bytes(),"text/html");f.seed_cache_for_tests(ap,APDF,"application/pdf");f.seed_cache_for_tests(op,OPDF,"application/pdf");
  let src:SourceEntry=serde_json::from_value(json!({"id":"asam_addiction","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Asam.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),2);
  let alcohol=rows.iter().find(|r|r.url==ap).unwrap();let oud=rows.iter().find(|r|r.url==op).unwrap();assert_ne!(alcohol.doc_key,oud.doc_key);
  let a=Asam.extract(&src,&f,alcohol).await.unwrap();let o=Asam.extract(&src,&f,oud).await.unwrap();
  assert_eq!(a.rights.status,"restricted");assert!(a.rights.restrictions.iter().any(|r|r.contains("Unauthorized reproduction")));assert!(a.blocks.iter().any(|b|b.text.contains("Recommendation I.1")));
  assert_eq!(o.rights.status,"restricted");assert!(o.rights.restrictions.iter().any(|r|r.contains("systematic reproduction") && r.contains("permission")));assert!(o.date_observations.iter().any(|d|d.label=="Adopted" && d.normalized.as_deref()==Some("2019-12-18")));
  let mut changed=APDF.to_vec();changed.extend_from_slice(b"\nchanged snapshot");f.seed_cache_for_tests(ap,&changed,"application/pdf");assert!(Asam.extract(&src,&f,alcohol).await.is_err());
 }

 #[test]fn genuine_special_documents_keep_exact_roles(){let base=Url::parse(ROOT).unwrap();let cases=[("stimulant.html","journal_guideline","Download Guideline"),("drug-testing.html","consensus_document","Download"),("methodology.html","methodology","Download Full Methodology"),("standards.html","standards_of_care","Standards of Care for the Addiction Specialist Physician Document")];for (name,role,label) in cases{let html=match name{"stimulant.html"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/stimulant.html")),"drug-testing.html"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/drug-testing.html")),"methodology.html"=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/methodology.html")),_=>include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/standards.html"))};let rows=special_topic(html,&base).unwrap().unwrap();assert!(rows.iter().any(|(_,r,l)|r==role&&l==label),"{name}: {rows:?}");if name=="standards.html"{assert!(rows.iter().any(|(_,r,_)|r=="performance_measures"));}}}
 #[tokio::test]async fn genuine_stimulant_journal_gate_is_explicit(){let html=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/stimulant.html"));let u="https://www.asam.org/quality-care/clinical-guidelines/stimulant-use-disorders";let anchor={let d=Html::parse_document(CAT);d.select(&sel("main [data-placeholder-label='Page Content Section'] h3 a[href]")).find(|a|a.value().attr("href").is_some_and(|h|h.ends_with("/stimulant-use-disorders"))).unwrap().html()};let bounded=format!("<main><div data-placeholder-label='Page Content Section'><h3>{anchor}</h3></div></main>");let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");f.seed_cache_for_tests(u,html.as_bytes(),"text/html");let src:SourceEntry=serde_json::from_value(json!({"id":"asam_addiction","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Asam.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),2);assert!(rows[0].url.starts_with("https://journals.lww.com/"));assert_eq!(rows[0].extra["source_role"],"journal_guideline");assert!(rows[0].extra["discovery_error"].is_string());assert_eq!(rows[0].extra["publisher_locator"]["journal"],"journaladdictionmedicine");assert_eq!(rows[1].extra["source_role"],"evidence_to_decision_tables");assert!(rows[1].extra["discovery_error"].is_string());}

 #[test]fn genuine_consensus_and_methodology_pdf_content_stays_scoped(){let drug=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/drug-testing-full-blocks.json"));let method=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/methodology-blocks.json"));let d:Vec<Block>=serde_json::from_str(drug).unwrap();let m:Vec<Block>=serde_json::from_str(method).unwrap();assert!(d.iter().any(|b|b.text.contains("CONSENSUS STATEMENT")));assert!(d.iter().any(|b|b.text.contains("supporting recovery")&&b.text.contains("punishment")));assert!(d.iter().any(|b|b.text.contains("presumptive")&&b.text.contains("definitive")));let r=rights(&d,"https://downloads.asam.org/sitefinity-production-blobs/docs/default-source/guidelines/the-asam-appropriate-use-of-drug-testing-in-clinical-addiction-medicine-full-document.pdf?sfvrsn=700a7bc2_0");assert_eq!(r.status,"restricted");assert!(r.restrictions.iter().any(|x|x.contains("Unauthorized reproduction")));assert!(m.iter().any(|b|b.text.contains("Methodology")));assert!(m.iter().any(|b|b.text.contains("Adopted April 12, 2023")));assert!(pdf_dates(&m,Some("methodology"),"https://downloads.asam.org/sitefinity-production-blobs/docs/default-source/quality-science/methdology.pdf?sfvrsn=4209f2ae_1").iter().any(|d|d.label=="Adopted"&&d.normalized.as_deref()==Some("2023-04-12")));assert!(pdf_dates(&m,None,"https://downloads.asam.org/source.pdf").is_empty());assert_eq!(rights(&m,"https://downloads.asam.org/sitefinity-production-blobs/docs/default-source/quality-science/methdology.pdf?sfvrsn=4209f2ae_1").status,"unknown");}


 #[test]fn fix_round1_stimulant_evidence_tables_and_lww_locator(){let html=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/stimulant.html"));let rows=special_topic(html,&Url::parse(ROOT).unwrap()).unwrap().unwrap();assert!(rows.iter().any(|(u,r,_)|r=="evidence_to_decision_tables"&&u.starts_with("https://downloads.asam.org/")&&u.contains("stud_guideline_etdtables")));assert!(rows.iter().any(|(_,r,_)|r=="journal_guideline"));let l=lww_locator("https://journals.lww.com/journaladdictionmedicine/fulltext/2024/05001/the_asam_aaap_clinical_practice_guideline_on_the.1.aspx");assert_eq!(l["journal"],"journaladdictionmedicine");assert_eq!(l["year"],"2024");assert_eq!(l["issue"],"05001");assert!(l.get("doi").is_none());assert!(lww_locator("https://downloads.asam.org/x.pdf").is_null());}
 #[tokio::test]async fn fix_round2_stimulant_pmc_copy_by_printed_slug(){
  let fx=|n:&str|std::fs::read(format!("{}/tests/fixtures/guidelines/r2b_crosswalk/{n}",env!("CARGO_MANIFEST_DIR"))).unwrap();
  let html=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/asam_addiction/stimulant.html"));let u="https://www.asam.org/quality-care/clinical-guidelines/stimulant-use-disorders";
  let anchor={let d=Html::parse_document(CAT);d.select(&sel("main [data-placeholder-label='Page Content Section'] h3 a[href]")).find(|a|a.value().attr("href").is_some_and(|h|h.ends_with("/stimulant-use-disorders"))).unwrap().html()};let bounded=format!("<main><div data-placeholder-label='Page Content Section'><h3>{anchor}</h3></div></main>");
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,8<<20,"").unwrap();f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");f.seed_cache_for_tests(u,html.as_bytes(),"text/html");
  let ep:Value=serde_json::from_slice(&fx("epmc-jam-stimulant.json")).unwrap();f.seed_cache_for_tests(&cw::epmc_search_url(ep["request"]["queryString"].as_str().unwrap(),100,"*"),&fx("epmc-jam-stimulant.json"),"application/json");
  f.seed_cache_for_tests(&cw::idconv_url(&cw::Lookup::Doi("10.1097/adm.0000000000001299".into())),&fx("idconv-asam-stimulant.json"),"application/json");f.seed_cache_for_tests(&cw::oai_url("PMC11105801").unwrap(),&fx("oai-PMC11105801-trimmed.xml"),"text/xml");
  let src:SourceEntry=serde_json::from_value(json!({"id":"asam_addiction","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Asam.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),2);
  // The 403 LWW row is replaced by the PMC copy; the ASAM-hosted evidence tables keep their own row.
  assert!(cw::is_crosswalk_row(&rows[0]));assert!(rows[0].doc_key.starts_with("asam-pmc-pmc11105801-"));assert_eq!(rows[0].extra["citation_resolution"]["matched"]["doi"],"10.1097/adm.0000000000001299");assert_eq!(rows[0].extra["publisher_locator"]["year"],"2024");assert_eq!(rows[1].extra["source_role"],"evidence_to_decision_tables");
  let d=Asam.extract(&src,&f,&rows[0]).await.unwrap();assert_eq!(d.rights.status,"restricted");assert!(d.rights.license.as_deref().unwrap().starts_with("This file is available for text mining"));assert_eq!(d.item_type.as_deref(),Some("clinical_practice_guideline"));
  assert!(d.relations.iter().any(|r|r.target=="pmc:PMC11105801"));assert!(d.relations.iter().any(|r|r.target.starts_with("https://journals.lww.com/journaladdictionmedicine/")));assert!(d.relations.iter().any(|r|r.target==u));assert!(d.warnings.iter().any(|w|w.contains("printed title slug")));assert!(d.blocks.len()>20);
  assert_eq!(lww_issn("journaladdictionmedicine"),Some("1932-0620"));}
}
