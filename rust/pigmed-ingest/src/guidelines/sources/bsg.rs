//! BSG public clinical-resource pages and separately gated publisher terminals.
use super::{common,Adapter,PlanRow};
use super::{pmc_crosswalk as cw,r2a_crosswalk as r2a};
use crate::guidelines::doc::{GuidelineDoc,Rights};
use crate::guidelines::fetch::{Fetched,Fetcher};
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct Bsg;
fn source_asset_url(url:&str)->Result<(String,bool)>{let mut u=Url::parse(url)?;let keyed=u.query_pairs().any(|(k,_)|k.eq_ignore_ascii_case("ijkey"));if keyed{let pairs=u.query_pairs().filter(|(k,_)|!k.eq_ignore_ascii_case("ijkey")).map(|(k,v)|(k.into_owned(),v.into_owned())).collect::<Vec<_>>();u.set_query(None);if !pairs.is_empty(){u.query_pairs_mut().extend_pairs(pairs);}}Ok((u.to_string(),keyed))}
const ROOT:&str="https://www.bsg.org.uk/clinical-resource?cats=Guidelines&page=1";
async fn public(f:&Fetcher,url:&str)->Result<Fetched>{let p=f.get(url).await?;if p.record.status!=200||!p.is_html(){bail!("BSG source returned HTTP {} or non-HTML",p.record.status)}if Url::parse(&p.record.final_url)?.host_str()!=Some("www.bsg.org.uk"){bail!("BSG source redirected off origin")};Ok(p)}
fn main<'a>(d:&'a Html)->Result<scraper::ElementRef<'a>>{let root=d.select(&sel("main#main-content")).next().context("BSG public main missing")?;if root.select(&sel("form input[type='password'], form iframe[src*='recaptcha'], form input[name*='captcha']")).next().is_some(){bail!("BSG source content contains credential/CAPTCHA form")};Ok(root)}
fn catalog(raw:&str,base:&Url)->Result<(Vec<(String,String)>,bool)>{let d=Html::parse_document(raw);let root=main(&d)?;let mut seen=HashSet::new();let mut out=vec![];for a in root.select(&sel("a.text-decoration-none[href]")){let Some(name)=a.select(&sel(".fw-semibold.fs-5")).next().map(|x|text_of(&x))else{continue};let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h))else{continue};let u=Url::parse(&url)?;if u.origin()==base.origin()&&u.path().starts_with("/clinical-resource/")&&seen.insert(url.clone()){out.push((url,name))}}if out.is_empty(){bail!("BSG source result cards missing")};let unresolved_next=root.select(&sel("ul.pagination li.page-item:not(.disabled) a[href='#']")).any(|a|text_of(&a)=="Next");Ok((out,unresolved_next))}
fn page_range(raw:&str)->Result<(usize,usize,usize)>{let d=Html::parse_document(raw);let root=main(&d)?;let label=root.select(&sel(".search-results-info-text")).next().map(|e|text_of(&e)).context("BSG result range missing")?;let re=regex::Regex::new(r"^Showing (\d+) to (\d+) of (\d+) results$")?;let c=re.captures(&label).context("BSG result range unrecognized")?;Ok((c[1].parse()?,c[2].parse()?,c[3].parse()?))}
fn article(raw:&str,base:&Url)->Result<(String,String,Vec<serde_json::Value>,&'static str)>{let d=Html::parse_document(raw);let root=main(&d)?;let title=root.select(&sel("h3.text-white")).next().map(|h|text_of(&h)).context("BSG clinical resource title missing")?;let body=root.select(&sel(".rich-text-events")).next().context("BSG source abstract missing")?;let role=if body.select(&sel("h2, h3, h4")).any(|h|text_of(&h)=="Abstract"){"abstract"}else if text_of(&body).contains("The BSG is pleased to endorse the BSR guideline") {"guideline_endorsement"}else if base.path().starts_with("/clinical-resource/")&&text_of(&body).len()>=80&&root.select(&sel("a.clinical-cards[href]")).next().is_some(){"clinical_resource_page"}else{bail!("BSG resource content role not recognized")};let mut assets=vec![];for a in root.select(&sel("a.clinical-cards[href]")){let label=text_of(&a);if let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)){let(url,keyed)=source_asset_url(&url)?;let role=asset_role(base,&url,&label);let mut asset=json!({"url":url,"role":role,"label":label,"source_reference_key_redacted":keyed,"url_scope":if keyed{"Identifier with source reference-key value omitted; exact href retained only in source response. Not a validated unkeyed access route; do not request automatically."}else{"Exact source-labelled target"},"rights":"unknown"});for(k,v) in stated_identifiers(&url){asset[k]=json!(v);}assets.push(asset);}}Ok((title,body.html(),assets,role))}
/// Same-origin guideline PDFs that BSG hosts itself under its CMS media routes (robots allows them).
fn is_hosted_pdf(base:&Url,url:&str)->bool{Url::parse(url).is_ok_and(|u|u.host_str()==Some("www.bsg.org.uk")&&base.host_str()==Some("www.bsg.org.uk")&&(u.path().starts_with("/getmedia/")||u.path().starts_with("/getattachment/"))&&u.path().to_lowercase().ends_with(".pdf"))}
/// A BSG-hosted PDF card is a guideline candidate unless its label names a supporting item; identity is checked
/// against the page title after conversion.
fn asset_role(base:&Url,url:&str,label:&str)->&'static str{let l=label.to_lowercase();if is_hosted_pdf(base,url)&&!["flowchart","letter","flipbook","summary","patient","infographic","poster"].iter().any(|x|l.contains(x)){"bsg_hosted_guideline_pdf"}else if l.contains("full guideline"){"full_guideline"}else if l.contains("flipbook"){"flipbook"}else{"source_asset"}}
/// Identifiers literally present in a source-emitted link: a DOI in the path, a PMCID in a PMC URL, or the BMJ
/// journal article id in a BMJ "early" URL. Nothing is derived beyond copying these strings.
fn stated_identifiers(url:&str)->Vec<(&'static str,String)>{let mut out=vec![];let Ok(u)=Url::parse(url) else{return out};let path=u.path();
 if let Some(c)=regex::Regex::new(r"/(10\.\d{4,9}/[^?#\s]+)$").unwrap().captures(path){out.push(("doi",c[1].to_string()));}
 if u.host_str().is_some_and(|h|h.ends_with("ncbi.nlm.nih.gov")){if let Some(c)=regex::Regex::new(r"/(PMC\d+)/?$").unwrap().captures(path){out.push(("pmcid",c[1].to_string()));}}
 if u.host_str().is_some_and(|h|h.ends_with(".bmj.com")){if let Some(c)=regex::Regex::new(r"/((?:gutjnl|flgastro)-\d{4}-\d{6})(?:[./]|$)").unwrap().captures(path){out.push(("bmj_article_id",c[1].to_string()));}}
 // R2a-crosswalk: a BMJ citation path (`/content/71/8/1459`, `/content/gutjnl/74/3/504.full.pdf`), copied as
 // "<host>/<volume>/<issue>/<first page>"; resolved only through Europe PMC with JATS verification.
 if let Some(h)=u.host_str().filter(|h|*h=="gut.bmj.com"||*h=="fg.bmj.com"){if let Some(c)=regex::Regex::new(r"^/content/(?:gutjnl/|flgastro/)?(\d+)/([A-Za-z0-9_]+)/([A-Za-z]{0,4}\d+)(?:\.full(?:\.pdf)?)?/?$").unwrap().captures(path){out.push(("bmj_citation",format!("{h}/{}/{}/{}",&c[1],&c[2],&c[3])));}}
 out}
fn ids_of(assets:&[serde_json::Value],key:&str)->Vec<String>{let mut out:Vec<String>=vec![];for a in assets{if let Some(v)=a[key].as_str(){if !out.iter().any(|x|x==v){out.push(v.into())}}}out}
fn significant_words(s:&str)->Vec<String>{s.to_lowercase().split(|c:char|!c.is_alphanumeric()).filter(|w|w.len()>=5).map(String::from).collect()}
/// The PDF must name the page's guideline: most of the page title's longer words appear near the start of its text.
fn pdf_matches_title(blocks:&[crate::guidelines::doc::Block],title:&str)->bool{let head=blocks.iter().take(60).map(|b|b.text.to_lowercase()).collect::<Vec<_>>().join(" ");let words=significant_words(title);if words.is_empty(){return false}let hit=words.iter().filter(|w|head.contains(w.as_str())).count();hit*10>=words.len()*7}
fn pdf_doi(blocks:&[crate::guidelines::doc::Block])->Option<String>{let re=regex::Regex::new(r"(?i)(?:doi:\s*|doi\.org/)(10\.\d{4,9}/[A-Za-z0-9._\-/]+[A-Za-z0-9])").unwrap();blocks.iter().take(300).find_map(|b|re.captures(&b.text.replace("**","")).map(|c|c[1].to_string()))}
fn pdf_rights(blocks:&[crate::guidelines::doc::Block],url:&str)->Rights{let text=blocks.iter().map(|b|b.text.replace("**","")).collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
 let (lic,status,restr)=if text.contains("(CC BY-NC 4.0)")||text.contains("Creative Commons Attribution Non Commercial (CC BY-NC 4.0)"){(Some("CC BY-NC 4.0"),"restricted",vec!["Noncommercial use only (as stated in the PDF's open-access notice).".to_string()])}else if text.contains("(CC BY 4.0)"){(Some("CC BY 4.0"),"open",vec![])}else if text.contains("permitted under CC BY-NC. No commercial re-use"){(Some("CC BY-NC"),"restricted",vec!["No commercial re-use (as stated in the PDF's copyright notice).".to_string()])}else{(None,"unknown",vec![])};
 Rights{status:status.into(),license:lic.map(String::from),evidence_urls:vec![url.into()],restrictions:restr,note:Some(if lic.is_some(){"Licence as stated in this PDF's open-access notice; figures and supplements can carry distinct rights.".into()}else{"No Creative Commons licence statement recognised in the PDF text.".into()}),..Default::default()}}
/// BSG guidelines published in Gut since ~2024 print each recommendation as "Recommendation N <statement> GRADE:
/// <certainty> certainty of evidence; <strength> recommendation; agreement ..." (PDF italics as `*`). Only that exact
/// full-text form is structured; the abstract's "REC N" summary box is left as prose.
fn mark_grade_recommendations(blocks:&mut Vec<crate::guidelines::doc::Block>)->usize{use crate::guidelines::doc::{Block,BlockKind};
 let re=regex::Regex::new(r"(?:^|\s)Recommendation (\d+) (.+?)\s+GRADE:\s*([A-Za-z]+(?: to [A-Za-z]+)?) certainty of evidence;\s*([A-Za-z]+) recommendation").unwrap();let mut add=vec![];
 for(i,b) in blocks.iter().enumerate(){if !matches!(b.kind,BlockKind::Paragraph){continue}let t=b.text.replace('*',"").split_whitespace().collect::<Vec<_>>().join(" ");let Some(c)=re.captures(&t) else{continue};
  add.push((i,Block{block_id:format!("{}-recommendation-{}",b.block_id,&c[1]),kind:BlockKind::Recommendation{label:Some(format!("Recommendation {}",&c[1])),strength:Some(c[4].to_string()),certainty:Some(c[3].to_string()),population:None,grading_system:Some("GRADE".into()),qualifier:None},text:c[2].trim().to_string(),heading_path:b.heading_path.clone(),page:b.page}));}
 let n=add.len();for(i,b) in add.into_iter().rev(){blocks.insert(i+1,b);}n}
fn issue(src:&SourceEntry,url:String,msg:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("bsg-{}-unresolved",common::short_hash(&url)),url,title:None,rendition:None,version:None,extra:json!({"discovery_error":msg})}}
// R2a-crosswalk: the site scrape (unchanged) lives in `discover_site`/`extract_site`; the trait impl below adds
// the PMC crosswalk for stated DOIs/PMCIDs and BMJ links (`stated_dois`, `stated_pmcids`, `bmj_article_ids`, `bmj_citations`).
impl Bsg{
 async fn discover_site(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{if limit==Some(0){return Ok(vec![])}let root=src.catalog_url.as_deref().unwrap_or(ROOT);let first=public(f,root).await?;let base=Url::parse(&first.record.final_url)?;if base.path()!="/clinical-resource"||!base.query_pairs().any(|(k,v)|k=="cats"&&v=="Guidelines")||!base.query_pairs().any(|(k,v)|k=="page"&&v=="1"){bail!("BSG filtered catalog seed is not the verified page 1 route")}let mut rows=vec![];let mut page=1usize;let mut first_total=None;let mut page_size=None;let mut seen=HashSet::new();let mut pending=Some(first);
  loop{if page>1000{bail!("BSG pagination safety limit exceeded")}let page_url=if page==1{root.to_string()}else{format!("{}://{}{}?page={page}&cats=Guidelines",base.scheme(),base.host_str().context("BSG host missing")?,base.path())};let c=if let Some(x)=pending.take(){x}else{match public(f,&page_url).await{Ok(x)=>x,Err(e)=>{rows.push(issue(src,page_url,format!("BSG catalog page unavailable: {e:#}")));break}}};let page_base=Url::parse(&c.record.final_url)?;let(entries,next)=catalog(&c.text(),&page_base)?;let(start,end,total)=page_range(&c.text())?;if page==1{if start!=1||end<start{bail!("BSG first range invalid")}first_total=Some(total);page_size=Some(end-start+1)}let size=page_size.context("BSG page size missing")?;if Some(total)!=first_total||start!=(page-1)*size+1||end<start||end>total||entries.len()!=end-start+1{bail!("BSG catalog range or item count changed during pagination")}
   for(url,_)in entries{if !seen.insert(url.clone()){bail!("BSG catalog repeated a topic across pages")}let p=match public(f,&url).await{Ok(p)=>p,Err(e)=>{rows.push(issue(src,url,format!("BSG source resource unavailable: {e:#}")));if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}continue}};let u=Url::parse(&p.record.final_url)?;let(title,body,assets,role)=match article(&p.text(),&u){Ok(v)=>v,Err(e)=>{rows.push(issue(src,url,format!("BSG source resource template: {e:#}")));if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}continue}};let hash=crate::common::sha256_hex(serde_json::to_string(&(&title,&body,&assets))?.as_bytes());let work=format!("bsg-{}",common::short_hash(&url));
    let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-{}",&hash[..12]),url:url.clone(),title:Some(title.clone()),rendition:Some("html".into()),version:None,extra:json!({"snapshot":hash,"role":role,"work_key":work,"source_catalog_page":page_url,"catalog_fetch":c.record,"resource_fetch":p.record,"stated_dois":ids_of(&assets,"doi"),"stated_pmcids":ids_of(&assets,"pmcid"),"bmj_article_ids":ids_of(&assets,"bmj_article_id"),"bmj_citations":ids_of(&assets,"bmj_citation"),"stated_id_links":assets.iter().filter(|a|["doi","pmcid","bmj_article_id","bmj_citation"].iter().any(|k|a[*k].is_string())).map(|a|json!({"url":a["url"],"label":a["label"],"doi":a["doi"],"pmcid":a["pmcid"],"bmj_article_id":a["bmj_article_id"],"bmj_citation":a["bmj_citation"]})).collect::<Vec<_>>()})};
    let hosted:Vec<String>={let mut v:Vec<String>=vec![];for a in &assets{if a["role"]=="bsg_hosted_guideline_pdf"{if let Some(u)=a["url"].as_str(){if !v.iter().any(|x|x==u){v.push(u.into())}}}}v};
    if hosted.len()==1&&role!="guideline_endorsement"{
     // BSG hosts the guideline's own PDF (usually the Gut / Frontline Gastroenterology article PDF): that is the
     // full document, and the resource page (abstract) becomes its metadata.
     match f.get_ok(&hosted[0]).await{Ok(pdf) if pdf.is_pdf()=>{row.url=hosted[0].clone();row.rendition=Some("pdf".into());row.doc_key=format!("{work}-{}",&pdf.record.sha256[..12]);row.extra["resource_role"]=json!(role);row.extra["role"]=json!("full_guideline_pdf");row.extra["resource_url"]=json!(url);row.extra["pdf_sha256"]=json!(pdf.record.sha256);},
      Ok(_)=>row.extra["full_text_error"]=json!("BSG-hosted guideline link did not return PDF bytes"),Err(e)=>row.extra["full_text_error"]=json!(format!("BSG-hosted guideline PDF unavailable: {e:#}"))}
    }else if hosted.len()>1{row.extra["full_text_error"]=json!(format!("BSG page hosts {} guideline PDFs (other bodies' guidelines on an endorsement page?); none chosen",hosted.len()));}
    if row.extra["role"]!="full_guideline_pdf"&&assets.iter().any(|a|a["role"]=="full_guideline"){
     // External publisher terminals (gut.bmj.com, fg.bmj.com, OUP) are not requested: the 2026-11 probes got 403.
     // Stated DOI / PMCID / BMJ ids are carried for the shared PMC open-access crosswalk.
     row.extra["full_text_route"]=json!("pmc_crosswalk_pending");
    }
    rows.push(row);if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}}
   if end==total{if next{rows.push(issue(src,page_url,"BSG last-page Next remained enabled; terminal state unresolved".into()));}break}if !next{rows.push(issue(src,page_url,"BSG Next disabled before reported final result".into()));break}page+=1;
  }if let Some(n)=limit{rows.truncate(n)}Ok(rows)}
 async fn extract_site(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{if let Some(e)=row.extra["discovery_error"].as_str(){bail!("BSG discovery: {e}")};
  let resource=row.extra["resource_url"].as_str().unwrap_or(&row.url).to_string();
  let p=public(f,&resource).await?;let base=Url::parse(&p.record.final_url)?;let(title,body,assets,role)=article(&p.text(),&base)?;
  let expected_role=row.extra["resource_role"].as_str().or(row.extra["role"].as_str()).unwrap_or_default();if expected_role!=role{bail!("BSG source role changed; re-plan")};
  let hash=crate::common::sha256_hex(serde_json::to_string(&(&title,&body,&assets))?.as_bytes());if row.extra["snapshot"].as_str()!=Some(hash.as_str()){bail!("BSG article changed; re-plan")};
  let(page_blocks,mut warnings)=blocks_from_html(&body,Some(&base),None);
  let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(p.record);
  let dois=ids_of(&assets,"doi");let pmcids=ids_of(&assets,"pmcid");let bmj=ids_of(&assets,"bmj_article_id");
  if row.extra["role"]=="full_guideline_pdf"{
   let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("BSG guideline response is not PDF")};if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=pdf.record.sha256){bail!("BSG PDF changed since discovery; re-plan")};
   let(mut blocks,notes,_)=super::static_list::blocks_for(&pdf,None,30).await?;if blocks.is_empty(){bail!("BSG PDF extracted no text")};
   if !pdf_matches_title(&blocks,&title){bail!("BSG-hosted PDF does not carry the resource page's guideline title; not treated as this guideline")}
   warnings.extend(notes);warnings.push("Full guideline from the BSG-hosted PDF; the BSG resource page (abstract/summary) is kept as metadata. Recommendation statements, grades, two-column reading order, tables and figures require visual verification.".into());
   let doi=pdf_doi(&blocks);let rights=pdf_rights(&blocks,&row.url);
   let recs=mark_grade_recommendations(&mut blocks);if recs>0{warnings.push(format!("{recs} numbered GRADE recommendations structured from the \"Recommendation N ... GRADE:\" form; statements split across blocks, good-practice statements and other formats are not structured."));}
   let mut relations=vec![];for id in &pmcids{relations.push(crate::guidelines::doc::Relation{rel:"related".into(),target:format!("pmc:{id}"),scope:Some("PMC record linked from the same BSG resource page".into()),evidence_url:Some(resource.clone())});}
   let mut attachments=assets.clone();attachments.push(json!({"url":resource,"role":"source_resource_page","source_role":role,"rights":"unknown"}));
   fetches.push(pdf.record);
   let summary=page_blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join("\n");
   return Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),item_type:Some("guideline".into()),status:"unknown".into(),doi,rights,relations,blocks,attachments,fetches,warnings,search_metadata:json!({"resource_url":resource,"resource_role":role,"resource_text":summary,"stated_dois":dois,"bmj_article_ids":bmj}),..Default::default()});
  }
  warnings.push(format!("BSG source {role} is not the full guideline. Linked documents have separate hosts, rights and completeness."));warnings.push("Generic login detection sees an optional page-level modal; strict public main excludes it and rejects actual credential/CAPTCHA gates in the content root. Filtered result pages 1, 2 and 9 were verified; unobserved middle pages and full publisher bodies remain subject to live validation.".into());
  if let Some(e)=row.extra["full_text_error"].as_str(){warnings.push(e.into());}
  let mut relations=vec![];
  if role!="guideline_endorsement"{for d in &dois{relations.push(crate::guidelines::doc::Relation{rel:"summary_of".into(),target:format!("doi:{d}"),scope:Some("DOI copied from the source page's own full-text link".into()),evidence_url:Some(resource.clone())});}for id in &pmcids{relations.push(crate::guidelines::doc::Relation{rel:"summary_of".into(),target:format!("pmc:{id}"),scope:Some("PMC record linked as the full guideline by the source page".into()),evidence_url:Some(resource.clone())});}}
  else{for d in &dois{relations.push(crate::guidelines::doc::Relation{rel:"related".into(),target:format!("doi:{d}"),scope:Some("Endorsed guideline of another society, as linked by the source page".into()),evidence_url:Some(resource.clone())});}}
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:Some(format!("bsg-{}",common::short_hash(&row.url))),title,url:row.url.clone(),item_type:Some(role.into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),..Default::default()},relations,blocks:page_blocks,attachments:assets,fetches,warnings,search_metadata:json!({"stated_dois":dois,"stated_pmcids":pmcids,"bmj_article_ids":bmj,"full_text_route":row.extra["full_text_route"]}),..Default::default()})}
}
/// R2a-crosswalk: one site row plus the PMC crosswalk rows for the ids its resource page states.
async fn crosswalk_group(src:&SourceEntry,f:&Fetcher,mut row:PlanRow,seen:&mut HashSet<String>)->(PlanRow,Vec<PlanRow>){
   if row.extra["discovery_error"].is_string(){return (row,vec![])}
   let resource=row.extra["resource_url"].as_str().unwrap_or(&row.url).to_string();
   let site=r2a::SiteRef{doc_key:Some(row.doc_key.clone()),url:resource.clone()};
   let links=row.extra["stated_id_links"].as_array().cloned().unwrap_or_default();
   let link_for=|k:&str,v:&str|links.iter().find(|a|a[k].as_str().is_some_and(|x|x.eq_ignore_ascii_case(v))).and_then(|a|a["url"].as_str()).unwrap_or_default().to_string();
   let endorsed=row.extra["role"]=="guideline_endorsement"||row.extra["resource_role"]=="guideline_endorsement";
   let notes:Vec<String>=if endorsed{vec!["Linked from a BSG endorsement page: a guideline of another society that BSG endorses.".into()]}else{vec![]};
   let mk=|url:String,route:&str|cw::SourceRef{source_url:url,evidence_url:Some(resource.clone()),source_title:None,item_type:Some("guideline".into()),key_prefix:r2a::key_prefix(src),id_route:route.into(),notes:notes.clone()};
   let mut out=r2a::Outcome::default();
   for l in cw::lookups_from_extra(&row.extra){
    let url=match &l{cw::Lookup::Doi(d)=>link_for("doi",d),cw::Lookup::Pmcid(p)=>link_for("pmcid",p),cw::Lookup::Pmid(_)=>String::new()};
    out.merge(r2a::adopt(src,f,&[l],&mk(url,"source_stated_id"),&site,&json!({}),seen).await);
   }
   let ids:Vec<String>=row.extra["bmj_article_ids"].as_array().into_iter().flatten().filter_map(|x|x.as_str().map(String::from)).collect();
   let cits:Vec<String>=row.extra["bmj_citations"].as_array().into_iter().flatten().filter_map(|x|x.as_str().map(String::from)).collect();
   let refs=ids.iter().map(|i|(r2a::BmjRef::ArticleId(i.clone()),link_for("bmj_article_id",i),"bmj_article_id_europepmc")).chain(cits.iter().filter_map(|c|r2a::BmjRef::parse_citation(c).map(|r|(r,link_for("bmj_citation",c),"bmj_citation_europepmc_jats"))));
   for(r,url,route) in refs.collect::<Vec<_>>(){
    match r2a::bmj_lookup(f,&r).await{Ok(l)=>out.merge(r2a::adopt(src,f,&[l],&mk(url,route),&site,&json!({"bmj_link":r.label()}),seen).await),Err(rec)=>out.closed.push(rec)}
   }
   out.record(&mut row.extra);(row,out.rows)
}
#[async_trait::async_trait]
impl Adapter for Bsg{
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
#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{use super::*;const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/catalog.html"));const IBD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/ibd.html"));
 #[test]fn genuine_public_main_and_optional_modal(){let base=Url::parse(ROOT).unwrap();let(rows,next)=catalog(CAT,&base).unwrap();assert_eq!(rows.len(),5);assert!(next);let u="https://www.bsg.org.uk/clinical-resource/BSG-guidelines-on-IBD-in-adults-2025";assert!(rows.iter().any(|(x,_)|x==u));let(title,body,assets,_)=article(IBD,&Url::parse(u).unwrap()).unwrap();assert!(title.contains("inflammatory bowel disease"));assert!(body.contains("Abstract"));assert!(assets.iter().any(|a|a["role"]=="full_guideline"&&a["url"]=="https://gut.bmj.com/content/74/Suppl_2/s1.full"));assert!(assets.iter().any(|a|a["role"]=="flipbook"));let gated=IBD.replace("<main id=\"main-content\"","<main id=\"main-content\"");let gated=gated.replacen("<main id=\"main-content\" class=\"main-content\">","<main id=\"main-content\" class=\"main-content\"><form><input type='password'></form>",1);assert!(article(&gated,&Url::parse(u).unwrap()).is_err());}
 #[tokio::test]async fn source_abstract_never_claims_full_guideline(){let u="https://www.bsg.org.uk/clinical-resource/BSG-guidelines-on-IBD-in-adults-2025";let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(ROOT,CAT.as_bytes(),"text/html");f.seed_cache_for_tests(u,IBD.as_bytes(),"text/html");let src:SourceEntry=serde_json::from_value(json!({"id":"bsg","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();let rows=Bsg.discover(&src,&f,None).await.unwrap();let r=rows.iter().find(|r|r.url==u).unwrap();let d=Bsg.extract(&src,&f,r).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("abstract"));assert!(d.warnings.iter().any(|w|w.contains("not the full guideline")));assert_eq!(r.extra["full_text_route"],"pmc_crosswalk_pending");assert!(!rows.iter().any(|r|r.url.starts_with("https://gut.bmj.com/")),"publisher terminals are not requested");assert!(rows.iter().any(|r|r.extra["discovery_error"].as_str().is_some_and(|e|e.contains("BSG catalog page unavailable"))));}
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod genuine_pagination_tests{use super::*;
 #[test]fn real_backend_pages_match_browser_ranges_and_cards(){let p2=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/catalog-page2.html"));let p9=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/catalog-page9.html"));let b=Url::parse("https://www.bsg.org.uk/clinical-resource?page=2&cats=Guidelines").unwrap();let(rows,next)=catalog(p2,&b).unwrap();assert_eq!(page_range(p2).unwrap(),(6,10,43));assert_eq!(rows.len(),5);assert!(next);assert!(rows[0].0.ends_with("/BSG-guidelines-on-colorectal-surveillance-in-IBD"));let(rows,next)=catalog(p9,&b).unwrap();assert_eq!(page_range(p9).unwrap(),(41,43,43));assert_eq!(rows.len(),3);assert!(!next);assert!(rows[0].0.ends_with("/Transjugular-Intrahepatic-Portosystemic"));}
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod final_live_tests{use super::*;
 #[test]fn source_heading_variants_and_endorsement_keep_distinct_roles(){for(name,raw) in [("coeliac",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/coeliac.html"))),("stents",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/stents-review.html"))),("endorsement",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/bsr-endorsement.html")))] {let (title,body,assets,role)=article(raw,&Url::parse("https://www.bsg.org.uk/").unwrap()).unwrap();assert!(!title.is_empty());assert!(!assets.is_empty());let (blocks,_)=blocks_from_html(&body,None,None);if name=="endorsement"{assert_eq!(role,"guideline_endorsement");assert!(blocks.iter().any(|b|b.text.contains("endorse the BSR guideline")));assert!(assets.iter().any(|a|a["url"].as_str().unwrap().contains("10.1093/rheumatology/keaf522")));}else{assert_eq!(role,"abstract");assert!(blocks.iter().any(|b|b.text.contains("Abstract")));assert!(blocks.iter().any(|b|b.text.len()>100));}}}
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod h3_source_abstract_tests{use super::*;#[test]fn two_genuine_h3_abstracts_keep_their_body_and_asset(){for raw in [include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/ibd-surveillance.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/autoimmune-hepatitis.html"))]{let(title,body,assets,role)=article(raw,&Url::parse("https://www.bsg.org.uk/").unwrap()).unwrap();assert!(!title.is_empty());assert_eq!(role,"abstract");assert!(body.contains("<h3>Abstract</h3>"));assert!(!assets.is_empty());if title.to_lowercase().contains("autoimmune hepatitis"){assert!(assets.iter().any(|a|a["source_reference_key_redacted"]==true));assert!(assets.iter().all(|a|!Url::parse(a["url"].as_str().unwrap()).unwrap().query_pairs().any(|(k,_)|k=="ijkey")));}let(blocks,_)=blocks_from_html(&body,None,None);assert!(blocks.iter().any(|b|b.text.len()>100));}}}

#[cfg(all(test, feature = "guideline-fixtures"))]mod remaining_public_variants{use super::*;
#[test]fn thirteen_genuine_public_resource_bodies_remain_scoped(){let cases:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/public-variant-cases.json"))).unwrap();assert_eq!(cases.as_array().unwrap().len(),13);for case in cases.as_array().unwrap(){let raw=std::fs::read_to_string(format!("{}/tests/fixtures/guidelines/bsg/{}",env!("CARGO_MANIFEST_DIR"),case["fixture"].as_str().unwrap())).unwrap();let base=Url::parse(case["url"].as_str().unwrap()).unwrap();let(title,body,assets,role)=article(&raw,&base).unwrap();assert_eq!(title,case["title"].as_str().unwrap());assert_eq!(role,case["expected_role"].as_str().unwrap());assert!(!assets.is_empty());if base.path().ends_with("/Diagnosis-management-of-autoimmune-hepatitis"){assert!(assets.iter().any(|a|a["source_reference_key_redacted"]==true));assert!(assets.iter().all(|a|!Url::parse(a["url"].as_str().unwrap()).unwrap().query_pairs().any(|(k,_)|k=="ijkey")));}let(blocks,_)=blocks_from_html(&body,Some(&base),None);assert!(blocks.iter().map(|b|b.text.len()).sum::<usize>()>80);if base.path().ends_with("/Role-of-FIT-in-the-investigation-of-IDA"){assert_eq!(role,"clinical_resource_page");assert!(body.contains("Introduction"));assert!(assets.iter().any(|a|a["role"]=="bsg_hosted_guideline_pdf"&&a["url"].as_str().unwrap().ends_with("flgastro-2025-103329.pdf")));assert!(assets.iter().any(|a|a["bmj_article_id"]=="flgastro-2025-103329"));assert!(article(&raw.replace("clinical-cards","unmapped-card"),&base).is_err());}}}
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod hosted_pdf_and_identifier_tests{use super::*;
 fn fx(name:&str)->String{std::fs::read_to_string(format!("{}/tests/fixtures/guidelines/bsg/{name}",env!("CARGO_MANIFEST_DIR"))).unwrap()}
 #[test]fn hosted_guideline_pdfs_and_stated_identifiers(){
  let b=Url::parse("https://www.bsg.org.uk/clinical-resource/BSG-Guidelines-Adult-Coeliac-Disease").unwrap();
  let(_,_,assets,role)=article(&fx("coeliac.html"),&b).unwrap();assert_eq!(role,"abstract");
  assert_eq!(assets.iter().filter(|a|a["role"]=="bsg_hosted_guideline_pdf").count(),1);
  assert!(assets.iter().any(|a|a["role"]=="bsg_hosted_guideline_pdf"&&a["url"]=="https://www.bsg.org.uk/getmedia/8afda1f3-e241-4caf-ba76-cfb5fb87c558/gutjnl-2025-337747.pdf"));
  let(_,_,assets,_)=article(&fx("public-variant-13.html"),&b).unwrap();assert_eq!(ids_of(&assets,"pmcid"),vec!["PMC8259225".to_string()]);
  let(_,_,assets,role)=article(&fx("bsr-endorsement.html"),&b).unwrap();assert_eq!(role,"guideline_endorsement");assert_eq!(ids_of(&assets,"doi"),vec!["10.1093/rheumatology/keaf522".to_string()]);
  let(_,_,assets,_)=article(&fx("ibd-surveillance.html"),&b).unwrap();assert_eq!(ids_of(&assets,"bmj_article_id"),vec!["gutjnl-2025-335023".to_string()]);
  // Two hosted PDFs of other bodies (NICE, ESGE) on one page: both are hosted guideline PDFs, so discovery picks none.
  let(_,_,assets,_)=article(&fx("public-variant-7.html"),&b).unwrap();assert_eq!(assets.iter().filter(|a|a["role"]=="bsg_hosted_guideline_pdf").count(),2);
  assert!(!is_hosted_pdf(&b,"https://gut.bmj.com/content/74/Suppl_2/s1.full"));
 }
 #[test]fn pdf_identity_doi_and_licence_come_from_pdf_text(){
  use crate::guidelines::doc::{Block,BlockKind};let mk=|t:&str|Block{block_id:"x".into(),kind:BlockKind::Paragraph,text:t.into(),heading_path:vec![],page:None};
  let blocks=vec![mk("The 2026 British Society of Gastroenterology guidelines on the diagnosis and management of adult coeliac disease"),mk("To cite: Smith A. Gut 2026. doi:10.1136/gutjnl-2025-337747"),mk("Open access This is an open access article distributed in accordance with the Creative Commons Attribution Non Commercial (CC BY-NC 4.0) license")];
  assert!(pdf_matches_title(&blocks,"The 2026 British Society of Gastroenterology guidelines on the diagnosis and management of adult coeliac disease"));
  assert!(!pdf_matches_title(&blocks,"Guidelines on the management of iron deficiency anaemia in adults"));
  assert_eq!(pdf_doi(&blocks).as_deref(),Some("10.1136/gutjnl-2025-337747"));
  let r=pdf_rights(&blocks,"https://www.bsg.org.uk/x.pdf");assert_eq!(r.license.as_deref(),Some("CC BY-NC 4.0"));assert_eq!(r.status,"restricted");
  assert_eq!(pdf_rights(&blocks[..2],"u").status,"unknown");
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod grade_recommendation_tests{use super::*;
 #[test]fn coeliac_2026_numbered_grade_recommendations(){use crate::guidelines::doc::{Block,BlockKind};
  let mut blocks:Vec<Block>=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/coeliac-2026-rec-blocks.json"))).unwrap();
  assert_eq!(mark_grade_recommendations(&mut blocks),2,"the abstract's REC 9 summary box is not a numbered recommendation");
  let recs:Vec<_>=blocks.iter().filter_map(|b|match &b.kind{BlockKind::Recommendation{label,strength,certainty,grading_system,..}=>Some((label.clone().unwrap(),strength.clone().unwrap(),certainty.clone().unwrap(),grading_system.clone().unwrap(),b.text.clone())),_=>None}).collect();
  assert_eq!(recs[0],("Recommendation 1".into(),"strong".into(),"High".into(),"GRADE".into(),"We recommend that patients consume a gluten-containing diet prior to testing for CD.".into()));
  assert_eq!(recs[1].0,"Recommendation 4");assert_eq!(recs[1].2,"Moderate");assert!(recs[1].4.starts_with("We recommend that at least four biopsies")&&recs[1].4.ends_with("suspected CD."));}
}

#[cfg(all(test, feature = "guideline-fixtures"))]mod hosted_pdf_extract_tests{use super::*;
 #[tokio::test]async fn coeliac_2026_hosted_pdf_is_the_full_guideline(){
  let u="https://www.bsg.org.uk/clinical-resource/BSG-Guidelines-Adult-Coeliac-Disease";let pdf_url="https://www.bsg.org.uk/getmedia/8afda1f3-e241-4caf-ba76-cfb5fb87c558/gutjnl-2025-337747.pdf";
  let page=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/coeliac.html"));let pdf=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bsg/coeliac-2026-pages-1-2.pdf"));
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(u,page.as_bytes(),"text/html");f.seed_cache_for_tests(pdf_url,pdf,"application/pdf");
  let(title,body,assets,role)=article(page,&Url::parse(u).unwrap()).unwrap();let hash=crate::common::sha256_hex(serde_json::to_string(&(&title,&body,&assets)).unwrap().as_bytes());
  let src:SourceEntry=serde_json::from_value(json!({"id":"bsg","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
  let row=PlanRow{source_id:"bsg".into(),doc_key:"t".into(),url:pdf_url.into(),title:Some(title.clone()),rendition:Some("pdf".into()),version:None,extra:json!({"role":"full_guideline_pdf","resource_role":role,"resource_url":u,"snapshot":hash,"work_key":"bsg-x"})};
  let d=Bsg.extract(&src,&f,&row).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("guideline"));assert_eq!(d.doi.as_deref(),Some("10.1136/gutjnl-2025-337747"));
  assert_eq!(d.rights.license.as_deref(),Some("CC BY-NC"));assert_eq!(d.rights.status,"restricted");assert!(d.blocks.iter().any(|b|b.text.contains("coeliac")));
  assert!(d.attachments.iter().any(|a|a["role"]=="source_resource_page"));assert_eq!(d.search_metadata["resource_role"],"abstract");
  let mut wrong=row.clone();wrong.extra["snapshot"]=json!("0");assert!(Bsg.extract(&src,&f,&wrong).await.is_err());}
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod r2a_tests{use super::*;use crate::guidelines::sources::pmc_crosswalk::Lookup;
 fn fx(n:&str)->Vec<u8>{std::fs::read(format!("{}/tests/fixtures/guidelines/r2a_crosswalk/{n}",env!("CARGO_MANIFEST_DIR"))).unwrap()}
 #[test]fn bmj_citation_paths_are_copied_literally(){
  for(u,want) in [("https://gut.bmj.com/content/71/8/1459","gut.bmj.com/71/8/1459"),("https://gut.bmj.com/content/74/Suppl_2/s1.full","gut.bmj.com/74/Suppl_2/s1"),("https://gut.bmj.com/content/gutjnl/74/3/504.full.pdf","gut.bmj.com/74/3/504"),("https://gut.bmj.com/content/55/suppl_4/iv1","gut.bmj.com/55/suppl_4/iv1")]{
   assert!(stated_identifiers(u).contains(&("bmj_citation",want.to_string())),"{u}");}
  for u in ["https://gut.bmj.com/content/early/2025/04/29/gutjnl-2025-335023","https://gut.bmj.com/content/gutjnl/early/2025/04/01/gutjnl-2024-333171.full.pdf","https://www.bsg.org.uk/content/71/8/1459"]{
   assert!(!stated_identifiers(u).iter().any(|(k,_)|*k=="bmj_citation"),"{u}");}
  let b=Url::parse("https://www.bsg.org.uk/clinical-resource/x").unwrap();
  let(_,_,assets,_)=article(&std::fs::read_to_string(format!("{}/tests/fixtures/guidelines/bsg/ibd-surveillance.html",env!("CARGO_MANIFEST_DIR"))).unwrap(),&b).unwrap();
  assert_eq!(ids_of(&assets,"bmj_article_id"),vec!["gutjnl-2025-335023".to_string()]);
 }
 // Real Europe PMC / ID Converter / OAI answers (2026-11-17, OAI bodies trimmed): the IBD-surveillance page's BMJ
 // article id and a Gut volume/issue/page link become OA PMC documents (CC BY-NC) without building a DOI.
 #[tokio::test]async fn bmj_links_become_open_pmc_documents(){
  let src:SourceEntry=serde_json::from_value(json!({"id":"bsg","access":"public","enabled":true})).unwrap();
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,8<<20,"").unwrap();
  let id=r2a::BmjRef::ArticleId("gutjnl-2025-335023".into());let cit=r2a::BmjRef::parse_citation("gut.bmj.com/71/8/1459").unwrap();
  f.seed_cache_for_tests(&cw::epmc_search_url(&r2a::bmj_query(&id).unwrap().0,10,"*"),&fx("epmc-gutjnl-2025-335023.json"),"application/json");
  f.seed_cache_for_tests(&cw::epmc_search_url(&r2a::bmj_query(&cit).unwrap().0,10,"*"),&fx("epmc-gut-71-8-1459.json"),"application/json");
  for p in ["PMC13018778","PMC9279848"]{f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Pmcid(p.into())),&fx(&format!("idconv-{p}.json")),"application/json");f.seed_cache_for_tests(&cw::oai_url(p).unwrap(),&fx(&format!("oai-{p}.xml")),"application/xml");}
  let early="https://gut.bmj.com/content/early/2025/04/29/gutjnl-2025-335023";let issue="https://gut.bmj.com/content/71/8/1459";
  let row=PlanRow{source_id:"bsg".into(),doc_key:"bsg-0123abcd-0123456789ab".into(),url:"https://www.bsg.org.uk/clinical-resource/BSG-guidelines-on-colorectal-surveillance-in-IBD".into(),title:Some("t".into()),rendition:Some("html".into()),version:None,
   extra:json!({"role":"abstract","stated_dois":[],"stated_pmcids":[],"bmj_article_ids":["gutjnl-2025-335023"],"bmj_citations":["gut.bmj.com/71/8/1459"],
    "stated_id_links":[{"url":early,"bmj_article_id":"gutjnl-2025-335023"},{"url":issue,"bmj_citation":"gut.bmj.com/71/8/1459"}]})};
  let mut seen=HashSet::new();let(site,open)=crosswalk_group(&src,&f,row,&mut seen).await;
  assert_eq!(open.len(),2);assert!(open[0].doc_key.starts_with("bsg-pmc-pmc13018778-"));assert!(open[1].doc_key.starts_with("bsg-pmc-pmc9279848-"));
  assert_eq!(open[0].extra["pmc_crosswalk"]["source"]["id_route"],"bmj_article_id_europepmc");assert_eq!(open[1].extra["pmc_crosswalk"]["source"]["id_route"],"bmj_citation_europepmc_jats");
  assert_eq!(site.extra[r2a::OPEN].as_array().unwrap().len(),2);
  let d=Bsg.extract(&src,&f,&open[1]).await.unwrap();assert_eq!(d.doi.as_deref(),Some("10.1136/gutjnl-2022-327326"));assert_eq!(d.rights.license.as_deref(),Some("CC-BY-NC-4.0"));
  assert!(d.relations.iter().any(|r|r.target==issue));assert!(d.relations.iter().any(|r|r.target=="guideline:bsg:bsg-0123abcd-0123456789ab"));
 }
}
