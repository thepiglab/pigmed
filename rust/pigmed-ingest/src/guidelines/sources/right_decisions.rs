//! Right Decisions public toolkit pages; host terms and linked asset rights remain explicit.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::{HashSet,VecDeque};
use url::Url;
pub struct RightDecisions;
const TERMS:&str="https://www.rightdecisions.scot.nhs.uk/terms-and-conditions-of-your-use-of-the-right-decisions-platform-rdp/";
fn toolkits(html:&str,base:&Url)->Result<Vec<String>>{let d=Html::parse_document(html);let mut out=vec![];let mut seen=HashSet::new();for a in d.select(&sel(".qrs-resource-grid-item a[data-type='toolkitListingPage'][href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let u=Url::parse(&url)?;if u.host_str()!=Some("www.rightdecisions.scot.nhs.uk"){continue;}if seen.insert(url.clone()){out.push(url)}}if out.is_empty(){bail!("Right Decisions toolkit listing missing");}Ok(out)}
fn chapter_links(html:&str,base:&Url,prefix:&str)->Vec<String>{let d=Html::parse_document(html);let mut out=vec![];let mut seen=HashSet::new();for a in d.select(&sel("a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let Ok(u)=Url::parse(&url)else{continue};if u.host_str()!=Some("www.rightdecisions.scot.nhs.uk")||!u.path().starts_with(prefix)||u.path()==prefix{continue;}if seen.insert(url.clone()){out.push(url)}}out}
/// Navigation-only listing nodes are part of the toolkit graph, not documents.
const NAV_ONLY:&str="Right Decisions navigation-only listing page";
/// Toolkit pages are reachable with and without the `organization` back-link parameter; it does not change
/// the page body, so it is not part of the page identity.
fn canonical(url:&str)->String{let Ok(mut u)=Url::parse(url)else{return url.to_string()};let kept:Vec<(String,String)>=u.query_pairs().filter(|(k,_)|k!="organization").map(|(k,v)|(k.into_owned(),v.into_owned())).collect();if kept.is_empty(){u.set_query(None);}else{u.query_pairs_mut().clear().extend_pairs(kept);}u.to_string()}
// scraper does not re-export ego_tree's NodeId; its Debug form is a stable per-document key.
fn node_key(id:impl std::fmt::Debug)->String{format!("{id:?}")}
fn is_descendant(e:&ElementRef,ids:&HashSet<String>)->bool{e.ancestors().any(|a|ids.contains(&node_key(a.id())))}
fn rec(id:String,label:Option<String>,qualifier:Option<String>,text:String,title:&str)->Block{Block{block_id:id,kind:BlockKind::Recommendation{label,strength:None,certainty:None,population:None,grading_system:None,qualifier},text,heading_path:vec![title.to_string()],page:None}}
/// Statements in the source's bordered recommendation boxes. Only printed labels are used: an "R" badge
/// (NICE/BTS/SIGN numbered recommendations with their bracketed provenance), a coloured label paragraph
/// ("Recommendation based on the research evidence"), or a text prefix. Boxes marked only by an icon image
/// (no text or alt) stay as prose and are counted in a warning; their meaning is not inferred.
fn boxed_recommendations(root:ElementRef,title:&str,recs:&mut Vec<Block>,consumed:&mut HashSet<String>,warnings:&mut Vec<String>){
 let number=regex::Regex::new(r"^([0-9]+(?:\.[0-9]+)+)\s").unwrap();let mut icon_only=0usize;let mut icons=std::collections::BTreeSet::new();
 for (i,bx) in root.select(&sel("div[style*='border: 3px solid']")).enumerate(){
  if is_descendant(&bx,consumed){continue;}
  let badge=bx.children().filter_map(ElementRef::wrap).find(|c|c.value().name()=="div"&&{let t=text_of(c);!t.is_empty()&&t.chars().count()<=3&&t.chars().all(|ch|ch.is_alphanumeric())}).map(|c|text_of(&c));
  let paras:Vec<String>=bx.select(&sel("p,li")).filter(|e|!(e.value().name()=="p"&&e.ancestors().filter_map(ElementRef::wrap).any(|a|a.value().name()=="li"))).map(|e|text_of(&e)).filter(|t|!t.is_empty()).collect();
  if paras.is_empty(){continue;}
  let printed_label=bx.select(&sel("p[style*='color'] > strong")).next().map(|e|text_of(&e)).filter(|t|paras.first()==Some(t));
  let id=format!("rd-box-recommendation-{i}");
  if let Some(label)=printed_label{
   if paras.len()<2{continue;}
   recs.push(rec(id,Some(label),None,paras[1..].join("\n"),title));
  }else if badge.as_deref()==Some("R"){
   let (prov,body):(Vec<&String>,Vec<&String>)=paras.iter().partition(|t|t.starts_with('[')&&t.ends_with(']'));
   let text=body.iter().map(|s|s.as_str()).collect::<Vec<_>>().join("\n");if text.is_empty(){continue;}
   let label=number.captures(&text).map(|c|c[1].to_string()).unwrap_or_else(||"R".into());
   let qualifier=(!prov.is_empty()).then(||prov.iter().map(|s|s.as_str()).collect::<Vec<_>>().join("; "));
   recs.push(rec(id,Some(label),qualifier,text,title));
  }else if let Some(label)=prefix_label(&paras[0]){
   recs.push(rec(id,Some(label.into()),None,paras.join("\n"),title));
  }else{
   icon_only+=1;for img in bx.select(&sel("img[src]")){if let Some(name)=img.value().attr("src").and_then(|s|s.split('?').next()).and_then(|s|s.rsplit('/').next()){icons.insert(name.to_string());}}
   continue;
  }
  consumed.insert(node_key(bx.id()));
 }
 if icon_only>0{warnings.push(format!("{icon_only} bordered box(es) are marked only by an icon image without text or alt label ({}); kept as prose, not classified as recommendations.",icons.into_iter().collect::<Vec<_>>().join(", ")));}
}
fn prefix_label(text:&str)->Option<&'static str>{if text.starts_with("RC:"){Some("RC")}else if text.starts_with("Good practice point:"){Some("Good practice point")}else if text.starts_with("Recommendation ")||text.starts_with("Recommendation:"){Some("Recommendation")}else{None}}
fn content(html:&str,base:&Url)->Result<(String,String,Vec<Block>,Vec<String>,Vec<Value>,Value)>{let d=Html::parse_document(html);
 let page_type=d.select(&sel("body[data-page-type]")).next().and_then(|b|b.value().attr("data-page-type")).unwrap_or("").to_string();
 if page_type=="sharedQabPathway"{bail!("Right Decisions interactive QAB pathway (JavaScript application shell with no server-rendered content; data-page-type=sharedQabPathway)");}
 let title=d.select(&sel("#toolkitPageHeader h1")).next().map(|e|text_of(&e)).filter(|x|!x.is_empty()).context("Right Decisions chapter title missing")?;
 let (root,root_sel)=match d.select(&sel("#qrs-grid-primary-content")).next(){Some(r)=>(r,"#qrs-grid-primary-content"),None if page_type=="mediaPage"=>(d.select(&sel("#qrs-grid-main .qrs-grid--primary")).next().context("Right Decisions media page body missing")?,".qrs-grid--primary"),None=>bail!("Right Decisions clinical body missing")};
 if root.select(&sel("input[type=password],iframe[src*='captcha']")).next().is_some(){bail!("Right Decisions clinical gate");}let raw=root.html();let hash=crate::common::sha256_hex(format!("{title}\n{raw}").as_bytes());let(mut blocks,mut warnings)=blocks_from_html(&raw,Some(base),Some(root_sel));
 if page_type=="listingPage"{
  // Direct-link accordion rows are navigation to child pages (crawled separately), not content.
  let nav:HashSet<String>=root.select(&sel(".qrs-acc-item--direct-link a")).map(|a|text_of(&a)).collect();
  blocks.retain(|b|!(matches!(b.kind,BlockKind::Heading{..})&&nav.contains(&b.text)));
  if !blocks.iter().any(|b|!matches!(b.kind,BlockKind::Heading{..})&&!b.text.trim().is_empty()){bail!("{NAV_ONLY}: only links to child pages");}
 }
 if !blocks.iter().any(|b|!b.text.trim().is_empty()){bail!("Right Decisions page has no server-rendered content");}
 let mut recs=vec![];let mut consumed=HashSet::new();boxed_recommendations(root,&title,&mut recs,&mut consumed,&mut warnings);
 for (i,e) in root.select(&sel("li,p")).enumerate(){if is_descendant(&e,&consumed){continue;}let text=text_of(&e);let Some(label)=prefix_label(&text)else{continue};if text.len()<20{continue;}recs.push(rec(format!("rd-source-recommendation-{i}"),Some(label.into()),None,text,&title));}
 for (i,panel) in root.select(&sel(".recommendation-panel")).enumerate(){if is_descendant(&panel,&consumed){continue;}let items:Vec<String>=panel.select(&sel("li")).map(|e|text_of(&e)).filter(|t|!t.is_empty()).collect();let items=if items.is_empty(){vec![text_of(&panel)]}else{items};for (j,t) in items.into_iter().enumerate(){if t.is_empty(){continue;}recs.push(rec(format!("rd-panel-recommendation-{i}-{j}"),None,None,t,&title));}}
 let n_recs=recs.len();blocks.extend(recs);let mut links=vec![];for(i,a)in root.select(&sel("a[href],img[src]")).enumerate(){let attr=if a.value().name()=="img"{"src"}else{"href"};let raw=a.value().attr(attr).unwrap_or_default();if raw.starts_with('#'){continue;}if let Some(url)=common::link(base,raw){let role=if attr=="src"{"source_image"}else if page_type=="mediaPage"&&a.value().classes().any(|c|c=="govuk-button"){"media_resource"}else{"source_link"};links.push(json!({"url":url,"label":text_of(&a),"role":role,"source_locator":format!("clinical-body/{i}"),"rights":"unknown; independent asset"}));}}
 if page_type=="mediaPage"{warnings.push("Right Decisions media page: the linked resource file (role media_resource) is recorded as an attachment and was not converted; only the page's own text is indexed.".into());}
 if n_recs==0{warnings.push("No recommendation is labelled on this page; its content is kept as section prose.".into());}
 let tags=d.select(&sel("#toolkitPageHeader .qrs-nav-tags a")).map(|e|text_of(&e)).collect::<Vec<_>>();let review=root.select(&sel("input#reviewdate[value]")).next().and_then(|e|e.value().attr("value")).unwrap_or("");let meta=json!({"toolkit_tags":tags,"review_date_raw":review,"page_type":page_type,"original_clinical_html":raw});warnings.push("Right Decisions chapter or recommendations-at-a-glance is a source derivative; it is not the full SIGN work. Source labels are retained without inferring recommendation grades; host and linked asset terms apply independently.".into());Ok((title,hash,blocks,warnings,links,meta))}
fn policy(html:&str)->Result<()> {let d=Html::parse_document(html);let body=d.select(&sel("body")).next().context("Right Decisions policy body missing")?;let t=text_of(&body).to_lowercase();if !t.contains("scrape or mine data without explicit permission")||!t.contains("personal and information purposes only")||!t.contains("commercial purposes"){bail!("Right Decisions source policy changed or incomplete");}Ok(())}
fn unresolved(src:&SourceEntry,url:String,e:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:None,rendition:Some("html".into()),version:None,extra:json!({"discovery_error":e})}}
#[async_trait::async_trait]impl Adapter for RightDecisions{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{if limit==Some(0){return Ok(vec![])}let cat=f.get_ok(src.catalog_url.as_deref().context("Right Decisions catalog missing")?).await?;let base=Url::parse(&cat.record.final_url)?;let mut queue:VecDeque<(String,String)>=VecDeque::new();for toolkit in toolkits(&cat.text(),&base)?{let u=Url::parse(&toolkit)?;queue.push_back((toolkit,u.path().to_string()));}let mut seen=HashSet::new();let mut rows=vec![];let mut deferred=vec![];
  while let Some((url,prefix))=queue.pop_front(){if !seen.insert(canonical(&url)){continue;}if seen.len()>5000{bail!("Right Decisions graph exceeded 5000 routes");}let page=match f.get_ok(&url).await{Ok(p)=>p,Err(e)=>{deferred.push(unresolved(src,url,format!("{e:#}")));continue;}};let b=Url::parse(&page.record.final_url)?;if b.host_str()!=Some("www.rightdecisions.scot.nhs.uk"){deferred.push(unresolved(src,url,"toolkit origin changed".into()));continue;}let html=page.text();for next in chapter_links(&html,&b,&prefix){if !seen.contains(&canonical(&next)){queue.push_back((next,prefix.clone()));}}if b.path()==prefix{continue;}match content(&html,&b){Ok((title,hash,_,_,_,_))=>{let key=common::short_hash(&canonical(&url));rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&hash[..12]),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"work_key":key,"clinical_sha256":hash,"catalog_fetch":cat.record})});},Err(e) if format!("{e:#}").starts_with(NAV_ONLY)=>{},Err(e)=>deferred.push(unresolved(src,url,format!("{e:#}")))}if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}}let rem=limit.map(|n|n.saturating_sub(rows.len())).unwrap_or(usize::MAX);rows.extend(deferred.into_iter().take(rem));Ok(rows)}
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{if let Some(e)=row.extra["discovery_error"].as_str(){bail!("Right Decisions source unresolved: {e}");}let terms=f.get_ok(TERMS).await;let p=f.get_ok(&row.url).await?;let base=Url::parse(&p.record.final_url)?;if base.host_str()!=Some("www.rightdecisions.scot.nhs.uk"){bail!("Right Decisions source origin changed");}let(title,hash,blocks,mut warnings,attachments,meta)=content(&p.text(),&base)?;if row.extra["clinical_sha256"].as_str().is_some_and(|x|x!=hash){bail!("Right Decisions chapter changed since discovery");}let mut fetches=vec![];if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(r)}fetches.push(p.record);let rights=match terms{Ok(t)=>{let valid=policy(&t.text()).is_ok();fetches.push(t.record);if valid{Rights{status:"restricted".into(),terms_url:Some(TERMS.into()),note:Some("Platform terms restrict copying, storage and commercial use and prohibit scraping or mining without explicit permission; no permission is inferred. Third-party content has independent rights.".into()),restrictions:vec!["Scraping or mining requires explicit permission".into()],evidence_urls:vec![TERMS.into()],..Default::default()}}else{warnings.push("Right Decisions policy layout or text was unrecognised; rights are unknown.".into());Rights{status:"unknown".into(),terms_url:Some(TERMS.into()),note:Some("Previously observed platform terms prohibited scraping or mining without explicit permission; current policy could not be verified.".into()),evidence_urls:vec![TERMS.into()],..Default::default()}}},Err(e)=>{warnings.push(format!("Right Decisions policy unavailable: {e:#}; rights are unknown."));Rights{status:"unknown".into(),terms_url:Some(TERMS.into()),note:Some("Previously observed platform terms prohibited scraping or mining without explicit permission; current policy could not be verified.".into()),evidence_urls:vec![TERMS.into()],..Default::default()}}};let dates=meta["review_date_raw"].as_str().filter(|s|s.len()==8&&s.chars().all(|c|c.is_ascii_digit())).map(|s|vec![common::date("Review date",&format!("{}-{}-{}",&s[..4],&s[4..6],&s[6..8]),&row.url)]).unwrap_or_default();let role=if title.to_lowercase().contains("at a glance"){"recommendation_summary"}else if meta["page_type"]=="mediaPage"{"media_page"}else{"guideline_chapter"};Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),item_type:Some(role.into()),status:"unknown".into(),date_observations:dates,rights,search_metadata:meta,blocks,attachments,fetches,warnings,..Default::default()})}
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{use super::*;const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/right_decisions/catalog.html"));const S:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/right_decisions/summary.html"));const W:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/right_decisions/warning.html"));#[test]fn genuine_toolkit_and_distinct_sign_derivatives(){let catalog_url=Url::parse("https://admin.rightdecisions.scot.nhs.uk/organisations/organisation?name=healthcare-improvement-scotland").unwrap();let rows=toolkits(C,&catalog_url).unwrap();assert!(rows.iter().any(|u|u.contains("care-of-deteriorating-patients-sign/?organization=healthcare-improvement-scotland")));let summary=Url::parse("https://www.rightdecisions.scot.nhs.uk/care-of-deteriorating-patients-sign/recommendations-at-a-glance/").unwrap();let(title,_,blocks,_,_,m)=content(S,&summary).unwrap();assert_eq!(title,"Recommendations at a glance");assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="RC")&&b.text.contains("anticipatory care plan")));assert_eq!(m["review_date_raw"],"20260622");let page=Url::parse("https://www.rightdecisions.scot.nhs.uk/care-of-deteriorating-patients-sign/early-warning-scores/").unwrap();let(_,_,blocks,_,_,_)=content(W,&page).unwrap();assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="Recommendation")&&b.text.contains("Recommendation 10")&&b.text.contains("NEWS2")));assert!(content("<html/>",&page).is_err());}}

#[cfg(all(test, feature = "guideline-fixtures"))] mod acquired_policy_test {
 use super::*;
 const TOOLKIT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/right_decisions/toolkit.html"));
 const TERMS_HTML:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/right_decisions/terms.html"));
 #[test]fn genuine_toolkit_graph_and_source_policy(){let u=Url::parse("https://www.rightdecisions.scot.nhs.uk/care-of-deteriorating-patients-sign/?organization=healthcare-improvement-scotland").unwrap();let children=chapter_links(TOOLKIT,&u,"/care-of-deteriorating-patients-sign/");assert!(children.iter().any(|u|u.contains("/recommendations-at-a-glance/?organization=healthcare-improvement-scotland")));assert!(children.iter().any(|u|u.contains("/early-warning-scores/?organization=healthcare-improvement-scotland")));assert!(policy(TERMS_HTML).is_ok());assert!(policy("<body>unknown policy</body>").is_err());}
 #[tokio::test]async fn unavailable_or_changed_policy_does_not_hide_clinical_page(){let summary=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/right_decisions/summary.html"));let u="https://www.rightdecisions.scot.nhs.uk/care-of-deteriorating-patients-sign/recommendations-at-a-glance/";for bad in [None,Some("<body>unknown policy</body>")]{let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,5<<20,"").unwrap();f.seed_cache_for_tests(u,summary.as_bytes(),"text/html");if let Some(t)=bad{f.seed_cache_for_tests(TERMS,t.as_bytes(),"text/html");}let src:SourceEntry=serde_json::from_value(json!({"id":"right_decisions","access":"public","enabled":true})).unwrap();let row=PlanRow{source_id:src.id.clone(),doc_key:"fixture".into(),url:u.into(),title:None,rendition:None,version:None,extra:json!({})};let doc=RightDecisions.extract(&src,&f,&row).await.unwrap();assert_eq!(doc.rights.status,"unknown");assert!(!doc.blocks.is_empty());assert!(doc.warnings.iter().any(|w|w.contains("policy")&&w.contains("unknown")));assert_eq!(doc.rights.terms_url.as_deref(),Some(TERMS));}}
}

#[cfg(all(test, feature = "guideline-fixtures"))] mod scale_templates_test {
 //! Page templates seen at full-catalog scale (2026-11-17): guidance pages without a recommendation block,
 //! "R"-badge NICE/BTS/SIGN boxes, coloured printed labels, recommendation panels, listing and media pages,
 //! and the JavaScript-only QAB self-evaluation app.
 use super::*;
 macro_rules! fx{($n:literal)=>{include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/right_decisions/",$n))}}
 fn u(p:&str)->Url{Url::parse(&format!("https://www.rightdecisions.scot.nhs.uk{p}")).unwrap()}
 fn recs(b:&[Block])->Vec<(Option<String>,Option<String>,String)>{b.iter().filter_map(|b|match &b.kind{BlockKind::Recommendation{label,qualifier,..}=>Some((label.clone(),qualifier.clone(),b.text.clone())),_=>None}).collect()}
 #[test]fn r_badge_boxes_keep_number_and_bracketed_provenance(){
  let(title,_,blocks,_,_,m)=content(fx!("rbox.html"),&u("/asthma-pathway-bts-nice-sign-sign-244/managing-chronic-asthma/pharmacological-management/principles-of-pharmacological-treatment/general-principles-of-pharmacological-treatment/")).unwrap();
  assert_eq!(title,"General principles of pharmacological treatment");assert_eq!(m["page_type"],"contentPage");
  let r=recs(&blocks);assert_eq!(r.iter().map(|x|x.0.clone().unwrap()).collect::<Vec<_>>(),vec!["1.6.1","1.6.2","1.6.3","1.6.4"]);
  assert_eq!(r[0].1.as_deref(),Some("[NICE 2017, BTS/SIGN 2019, amended 2024]"));assert!(r[0].2.contains("alternative diagnoses or comorbidities"));assert!(!r[0].2.contains("[NICE"));
  assert_eq!(r[1].1.as_deref(),Some("[BTS/NICE/SIGN 2024]"));
  assert!(blocks.iter().any(|b|b.text.contains("Why the committee made the recommendations")));
 }
 #[test]fn guidance_page_without_recommendation_block_is_prose_not_failure(){
  let(_,_,blocks,warnings,_,_)=content(fx!("references.html"),&u("/care-of-deteriorating-patients-sign/references/")).unwrap();
  assert!(recs(&blocks).is_empty());assert!(blocks.len()>5);assert!(warnings.iter().any(|w|w.contains("No recommendation is labelled")));
 }
 #[test]fn printed_labels_panels_and_icon_only_boxes(){
  let(_,_,b,_,_,_)=content(fx!("research.html"),&u("/asthma-pathway-bts-nice-sign-sign-244/patient-information-resource/asthma-in-children-and-young-people/")).unwrap();
  let r=recs(&b);assert!(r.len()>=20);assert!(r.iter().all(|x|x.0.as_deref().is_some_and(|l|l.starts_with("Recommendation based on")||l.starts_with("Not enough evidence"))),"{:?}",r.iter().map(|x|&x.0).collect::<Vec<_>>());
  assert!(r.iter().any(|x|x.2.contains("listen to your child’s breathing")));assert!(r.iter().all(|x|!x.2.starts_with("Recommendation based on")));
  let(_,_,b,_,_,_)=content(fx!("recpanel.html"),&u("/bloody-diarrhoea-and-clinically-suspected-or-confirmed-shiga-toxin-producing-eschericia-coli-stec-infections/stec-infection-in-adults/")).unwrap();
  let r=recs(&b);assert!(r.iter().any(|x|x.0.is_none()&&x.2.starts_with("14-day follow-up: Adults with confirmed STEC infection")));
  let(_,_,b,w,_,_)=content(fx!("gpp.html"),&u("/assessment-diagnosis-care-and-support-for-people-with-dementia-and-their-carers-sign/postdiagnostic-support/postdiagnostic-support-for-young-carers/")).unwrap();
  assert!(w.iter().any(|w|w.contains("icon image")),"{w:?}");assert!(b.iter().any(|b|!b.text.is_empty()));
 }
 #[test]fn listing_media_and_app_shell_pages(){
  let e=content(fx!("listing-nav.html"),&u("/congenital-heart-disease-chd-standards/standard-10-emergency-care/")).unwrap_err();assert!(format!("{e:#}").starts_with(NAV_ONLY));
  let(_,_,b,_,_,m)=content(fx!("listing-content.html"),&u("/asthma-pathway-bts-nice-sign-sign-244/managing-difficult-and-severe-asthma/")).unwrap();assert_eq!(m["page_type"],"listingPage");assert!(b.iter().any(|b|matches!(b.kind,BlockKind::Paragraph)&&b.text.len()>40));
  let(_,_,b,w,links,m)=content(fx!("media.html"),&u("/asthma-pathway-bts-nice-sign-sign-244/additional-information-and-resources/inhaled-corticosteroid-doses-for-the-bts-nice-and-sign-asthma-guideline/")).unwrap();
  assert_eq!(m["page_type"],"mediaPage");assert!(b.iter().any(|b|b.text.contains("inhaled corticosteroid (ICS) dosages")));
  assert!(links.iter().any(|l|l["role"]=="media_resource"&&l["url"].as_str().unwrap().ends_with("ng245-asthma-ics-dose-tables-20251112.pdf")));assert!(w.iter().any(|w|w.contains("media page")));
  let e=content(fx!("qab.html"),&u("/asthma-pathway-bts-nice-sign-sign-244/x/")).unwrap_err();assert!(format!("{e:#}").contains("sharedQabPathway"));
 }
 #[test]fn organization_parameter_is_not_page_identity(){
  assert_eq!(canonical("https://www.rightdecisions.scot.nhs.uk/a/b/?organization=healthcare-improvement-scotland"),"https://www.rightdecisions.scot.nhs.uk/a/b/");
  assert_eq!(canonical("https://www.rightdecisions.scot.nhs.uk/a/?x=1&organization=y"),"https://www.rightdecisions.scot.nhs.uk/a/?x=1");
 }
}
