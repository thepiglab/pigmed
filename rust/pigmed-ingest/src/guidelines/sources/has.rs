//! HAS source-labelled publication records and JSON-LD assets; blocked search is not aliased.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,FetchRecord,GuidelineDoc};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::{HashMap,HashSet};
use url::Url;
pub struct Has;
fn base(doc:&Html,response:&Url)->Result<Url>{let declared=doc.select(&sel("head base[href]")).next().and_then(|e|e.value().attr("href"));let url=declared.map(|v|response.join(v)).transpose()?.unwrap_or_else(||response.clone());if url.origin()!=response.origin(){bail!("HAS HTML base changes origin");}Ok(url)}
fn fr_date(label:&str,raw:&str,url:&str)->DateObservation{
 let mut d=common::date(label,raw,url);let words=regex::Regex::new(r"(?i)^(?:(\d{1,2})\s+)?([a-zéûôèà]+)\s+((?:19|20)\d{2})$").unwrap();let months=["janvier","février","mars","avril","mai","juin","juillet","août","septembre","octobre","novembre","décembre"];
 if let Some(c)=words.captures(&d.raw){if let Some(m)=months.iter().position(|v|*v==c[2].to_lowercase()){d.normalized=match c.get(1){Some(day)=>day.as_str().parse::<u8>().ok().filter(|v|(1..=31).contains(v)).map(|day|format!("{}-{:02}-{day:02}",&c[3],m+1)),None=>Some(format!("{}-{:02}",&c[3],m+1))};}}d
}
// Fix 2026-11-17 (group F): HAS records come as schema.org MedicalGuideline (subjectOf documents)
// or MedicalWebPage (hasPart documents). Every record is labelled by its own publication type and
// primary document instead of being refused when it is not a full "Recommandations" template.
fn json_record(doc:&Html)->Result<Value>{let mut records=vec![];for script in doc.select(&sel("script[type='application/ld+json']")){let text=script.text().collect::<String>();let value:Value=serde_json::from_str(&text)?;if matches!(value["@type"].as_str(),Some("MedicalGuideline"|"MedicalWebPage")){records.push(value);}}if records.len()!=1{bail!("HAS expected one source-declared MedicalGuideline/MedicalWebPage record, found {}",records.len());}Ok(records.remove(0))}
fn norm(name:&str)->String{name.replace(['–','—'],"-").replace('’',"'").split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()}
fn role(name:&str)->&'static str{let name=norm(name);if name.ends_with("- recommandations"){"full_recommendations"}else if name.contains("fiche de synthèse")||name.starts_with("synthèse"){"synthesis"}else if name.contains("rapport d'élaboration"){"methods_report"}else if name.contains("note de cadrage"){"scoping_note"}else if name.contains("groupe de lecture"){"reader_group_opinion"}else if name.contains("argumentaire"){"evidence_report"}else if name.ends_with("- guide"){"guide"}else{"other_source_document"}}
/// The item's type from the source's own publication-type label and the role of its primary document.
fn item_type(publication_type:&str,primary_role:Option<&str>)->String{
 match primary_role{Some("full_recommendations")=>return "clinical_practice_guideline".into(),Some("scoping_note")=>return "scoping_note".into(),_=>{}}
 let t=norm(publication_type);
 let mapped=if t.contains("parcours"){"care_pathway"}else if t.contains("outil"){"practice_improvement_tool"}else if t.contains("recommandation vaccinale"){"vaccination_recommendation"}else if t.contains("recommandation"){"recommendation"}else if t.contains("évaluation")||t.contains("rapport"){"evaluation_report"}else if t.starts_with("avis"){"opinion"}else if t.contains("guide"){"guide"}else{""};
 if mapped.is_empty(){format!("has_{}",common::slug(&t).replace('-',"_"))}else{mapped.into()}
}
struct Detail{title:String,key:String,pdf:Option<String>,primary_role:Option<String>,item_type:String,language:Option<String>,assets:Vec<Value>,dates:Vec<DateObservation>,metadata:Value,terms:Option<String>,overview:String}
fn detail(raw:&str,response:&Url)->Result<Detail>{
 let doc=Html::parse_document(raw);let effective=base(&doc,response)?;let data=json_record(&doc)?;let guideline=data["@type"]=="MedicalGuideline";
 if guideline&&data["medicalAudience"]["audienceType"]!="healthcare professional"{bail!("HAS source audience is not the validated healthcare professional guideline audience");}let section=doc.select(&sel("section.section--article .center-content")).next().context("HAS article center-content missing")?;
 let title=section.select(&sel(".content--header .title")).next().map(|e|text_of(&e)).context("HAS article title missing")?;if data["name"].as_str().map(norm)!=Some(norm(&title)){bail!("HAS clinical title differs from structured source record");}
 let canonical=doc.select(&sel("head link[rel='canonical'][href]")).next().and_then(|e|e.value().attr("href")).context("HAS record canonical missing")?;let canonical=effective.join(canonical)?;if canonical.origin()!=effective.origin()||!canonical.username().is_empty()||canonical.password().is_some(){bail!("HAS record canonical changes trusted source origin or contains userinfo");}let key=canonical.path().split('/').find(|p|p.starts_with("p_")||p.starts_with("c_")).context("HAS record JCMS identifier missing")?.to_string();
 let publication_type=section.select(&sel(".content--header .titleLabelCaps")).next().map(|e|text_of(&e)).context("HAS source publication type missing")?;
 let parts=if guideline{&data["subjectOf"]}else{&data["hasPart"]};
 let mut assets=vec![];let mut skipped=vec![];for asset in parts.as_array().into_iter().flatten(){
  let name=asset["name"].as_str().context("HAS document name missing")?;let source=asset["url"].as_str().or(asset["contentUrl"].as_str()).context("HAS document URL missing")?;let url=effective.join(source)?;
  if url.origin()!=effective.origin()||!url.username().is_empty()||url.password().is_some(){skipped.push(json!({"label":name,"raw_url":source,"reason":"leaves the HAS origin or carries userinfo; not fetched"}));continue;}
  let pdf=url.path().starts_with("/upload/docs/application/pdf/")&&asset["encodingFormat"]=="application/pdf";
  assets.push(json!({"url":url.as_str(),"label":name,"role":if pdf{role(name)}else{"non_pdf_document"},"encodingFormat":asset["encodingFormat"],"inLanguage":asset["inLanguage"],"document_group":asset["isPartOf"]["name"],"source_record":canonical.as_str()}));
 }
 let with=|r:&str|assets.iter().filter(|a|a["role"]==r).collect::<Vec<_>>();
 let full=with("full_recommendations");if full.len()>1{bail!("HAS record lists {} documents labelled Recommandations; primary document ambiguous",full.len());}
 // Primary document: the full Recommandations; else the item's own scoping note or guide; else the only
 // PDF in the source's "Documents" group; else none (the HTML overview is the body).
 let primary:Option<Value>=if full.len()==1{Some(full[0].clone())}else{let scoping=with("scoping_note");let guide=with("guide");
  if norm(&title).contains("note de cadrage")&&scoping.len()==1{Some(scoping[0].clone())}else if guide.len()==1{Some(guide[0].clone())}else{let docs=assets.iter().filter(|a|a["role"]!="non_pdf_document"&&a["document_group"]=="Documents").collect::<Vec<_>>();if docs.len()==1{Some(docs[0].clone())}else{None}}};
 let primary=primary.as_ref();
 let pdf=primary.and_then(|a|a["url"].as_str()).map(String::from);let primary_role=primary.and_then(|a|a["role"].as_str()).map(String::from);
 let language=primary.and_then(|a|a["inLanguage"].as_str()).map(String::from).or_else(||common::page_language(&doc));
 let mut dates=vec![];if let Some(value)=doc.select(&sel("section.section--article .list-info .validation .val")).next(){dates.push(fr_date("Date de validation",&text_of(&value),canonical.as_str()));}
 let subtitle=section.select(&sel(".content--header .sub-title")).next().map(|e|text_of(&e)).unwrap_or_default();let online=regex::Regex::new(r"Mis en ligne le\s+(\d{1,2}\s+[a-zéûôèà]+\s+(?:19|20)\d{2})").unwrap();if let Some(c)=online.captures(&subtitle){dates.push(fr_date("Mis en ligne le",&c[1],canonical.as_str()));}
 if let Some(v)=data["datePublished"].as_str(){let mut d=common::date("datePublished (schema.org record)",v,canonical.as_str());if d.normalized.is_none()&&regex::Regex::new(r"^(?:19|20)\d{2}-\d{2}-\d{2}T").unwrap().is_match(v){d.normalized=Some(v[..10].to_string());}dates.push(d);}
 let terms=doc.select(&sel("a[href]")).find(|e|text_of(e)=="Mentions légales").and_then(|e|common::link(&effective,e.value().attr("href")?));
 let overview_parts=section.select(&sel(".content--main .encart .wysiwyg")).collect::<Vec<_>>();let overview=overview_parts.iter().map(|e|e.html()).collect::<String>();let overview_meta=overview_parts.iter().map(|e|json!({"html":e.html(),"text":text_of(e)})).collect::<Vec<_>>();
 let aliases=section.select(&sel(".bloc-docs a[href]")).map(|a|json!({"label":text_of(&a),"raw_href":a.value().attr("href"),"url":a.value().attr("href").and_then(|h|common::link(&effective,h))})).collect::<Vec<_>>();
 let item_type=item_type(&publication_type,primary_role.as_deref());
 Ok(Detail{title,key,pdf,primary_role,item_type:item_type.clone(),language,assets,dates,terms,overview,metadata:json!({"source_schema":data,"source_publication_type":publication_type,"item_type":item_type,"primary_document":primary,"skipped_documents":skipped,"overview_blocks":overview_meta,"document_record_aliases":aliases,"canonical":canonical.as_str(),"html_base":effective.as_str(),"overview_role":"context/overview only; not a replacement for the primary document"})})
}
const RSS_RECOMMENDATIONS:&str="https://www.has-sante.fr/feed/Rss2.jsp?id=p_3081452"; // "HAS - Recommandations et guides", listed on /jcms/c_1771214/fr/nos-flux-d-information-rss
/// RSS items -> catalog entries (link, title, pubDate only; author addresses are never kept).
fn rss_items(xml:&str,base:&Url)->Result<Vec<Value>>{
 let doc=roxmltree::Document::parse(xml.trim_start())?;let mut out=vec![];
 for item in doc.descendants().filter(|n|n.has_tag_name("item")){
  let get=|t:&str|item.children().find(|c|c.has_tag_name(t)).and_then(|c|c.text()).map(|s|s.trim().to_string()).filter(|s|!s.is_empty());
  let Some(link)=get("link") else{continue};let url=base.join(&link)?;if url.origin()!=base.origin()||!url.path().starts_with("/jcms/"){continue;}
  out.push(json!({"url":url.as_str(),"title":get("title"),"source_type":null,"feed_pubDate":get("pubDate"),"scope":"RSS 'HAS - Recommandations et guides' (latest items only; not an exhaustive catalog)"}));
 }
 if out.is_empty(){bail!("HAS RSS feed has no publication items");}Ok(out)
}
fn homepage(raw:&str,response:&Url)->Result<(Vec<Value>,Vec<Value>)>{
 let doc=Html::parse_document(raw);let effective=base(&doc,response)?;let mut entries=vec![];let mut seen=HashSet::new();
 for item in doc.select(&sel("li.publication-item")){let a=item.select(&sel("p.text > a[href]")).next().context("HAS source publication card target missing")?;let url=effective.join(a.value().attr("href").unwrap())?;if url.origin()!=effective.origin(){bail!("HAS homepage publication crosses origin");}if !seen.insert(url.to_string()){continue;}let genre=item.select(&sel(".meta-info .type")).next().map(|e|text_of(&e)).context("HAS homepage publication type missing")?;
  entries.push(json!({"url":url.as_str(),"title":text_of(&a),"source_type":genre,"card_date":item.select(&sel(".meta-info .date")).next().map(|e|text_of(&e)),"scope":"homepage recent-publication selection only; not an exhaustive search catalog"}));
 }
 if entries.is_empty(){bail!("HAS inspected homepage publication selection missing");}
 let nav=doc.select(&sel("a[href]")).filter_map(|a|{let url=effective.join(a.value().attr("href")?).ok()?;((url.path().starts_with("/jcms/fc_2875208/")||url.path().starts_with("/jcms/fc_2875171/"))&&url.origin()==effective.origin()).then(||json!({"url":url.as_str(),"label":text_of(&a),"role":if url.path().starts_with("/jcms/fc_2875171/"){"publisher_robots_disallowed_search"}else{"source_publication_search_frontend"}}))}).collect::<Vec<_>>();
 Ok((entries,nav))
}
fn blocked_catalog(raw:&str,response:&Url)->Result<Value>{
 let doc=Html::parse_document(raw);let effective=base(&doc,response)?;let roots=doc.select(&sel("section.section--article .content--wrapper .wysiwyg")).filter(|e|e.select(&sel("h1")).any(|h|text_of(&h)=="Toutes nos publications")).collect::<Vec<_>>();if roots.len()!=1{bail!("HAS permitted publications hub body missing/ambiguous");}
 let routes=roots[0].select(&sel("a[href]")).filter_map(|a|{let url=effective.join(a.value().attr("href")?).ok()?;(url.origin()==effective.origin()&&url.path().starts_with("/jcms/fc_2875171/")).then(||json!({"url":url.as_str(),"label":text_of(&a),"role":"source_emitted_catalog_query","acquisition":"publisher robots disallows this exact namespace; not fetched or aliased"}))}).collect::<Vec<_>>();if !routes.iter().any(|r|r["label"]=="Recommandations du secteur sanitaire"){bail!("HAS source clinical catalog query missing");}
 Ok(json!({"permitted_hub_url":response.as_str(),"catalog_queries":routes,"robots_evidence_url":"https://www.has-sante.fr/robots.txt","blocked_namespace":"/jcms/fc_2875171/","scope":"public navigation hub is not a result inventory; source-emitted results route explicitly blocked"}))
}
fn terms(raw:&str,url:&Url)->Result<Value>{
 let doc=Html::parse_document(raw);let roots=doc.select(&sel("section.section--article .content--main .encart .wysiwyg")).collect::<Vec<_>>();if roots.len()!=1{bail!("HAS legal body missing/ambiguous");}let heading=roots[0].select(&sel("h3")).find(|e|text_of(e)=="Reproduction des travaux de la HAS").context("HAS reproduction section missing")?;let mut text=vec![];let mut next=heading.next_sibling();while let Some(n)=next{if let Some(e)=ElementRef::wrap(n){if e.value().name()=="h3"{break;}let t=text_of(&e);if !t.is_empty(){text.push(t);}}next=n.next_sibling();}let text=text.join("\n");if !text.contains("informations publiques librement et gratuitement réutilisables")||!text.contains("droits de propriété intellectuelle de tiers")||!text.contains("reproduit en intégralité"){bail!("HAS reproduction notice does not match validated source terms");}Ok(json!({"source_url":url.as_str(),"section":"Reproduction des travaux de la HAS","text":text,"scope":"HAS-owned works conditional reuse; third-party rights excluded; logo/graphic charter impose separate reproduction conditions"}))
}
fn recommendations(blocks:&mut Vec<Block>,url:&str)->Result<Vec<Value>>{
 let legends=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("Preuve scientifique établie")&&b.text.contains("Présomption scientifique")).map(|b|b.block_id.clone()).collect::<Vec<_>>();if legends.is_empty(){return Ok(vec![]);}let number=regex::Regex::new(r"^R\.\s*\d+$").unwrap();let inline=regex::Regex::new(r"(?i)\bgrade\s+(AE|[ABC])\b").unwrap();let mut records=vec![];let mut pending=vec![];
 for table in blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})){
  let mut statements=vec![];for (n,line) in table.text.lines().enumerate(){if !line.starts_with('|'){continue;}let cells=line.trim_matches('|').split('|').map(str::trim).collect::<Vec<_>>();if cells.len()!=3||!number.is_match(cells[1])||cells[2].is_empty(){continue;}let printed=cells[0];let normalized=printed.split_whitespace().collect::<Vec<_>>().join(" ");let merged_heading=(normalized=="AE À l’initiation des traitements médicamenteux").then_some("À l’initiation des traitements médicamenteux");let column=if merged_heading.is_some(){Some("AE")}else{matches!(printed,"A"|"B"|"C"|"AE").then_some(printed)};let mentions=inline.captures_iter(cells[2]).map(|c|json!({"printed":c.get(0).unwrap().as_str(),"grade":c[1].to_uppercase(),"start_byte_in_source_cell":c.get(0).unwrap().start()})).collect::<Vec<_>>();
   let conflict=column.is_some_and(|column|mentions.iter().any(|m|m["grade"]!=column));let strength=if conflict{None}else{column.map(String::from)};let id=format!("{}-{}",table.block_id,cells[1].replace(' ',""));let text=cells[2].replace("\\|","|");
   records.push(json!({"block_id":id,"source_identifier":cells[1],"printed_column_grade":printed,"normalized_column_grade":column,"following_section_heading_merged_by_conversion":merged_heading,"inline_grade_mentions":mentions,"grade_conflict":conflict,"source_table_block_id":table.block_id,"converted_table_line":n+1,"source_url":url,"grading_legend_block_ids":legends,"source_text":text,"certainty":"not synthesized from recommendation-grade definitions"}));statements.push(Block{block_id:id,kind:BlockKind::Recommendation{label:Some(cells[1].into()),strength,certainty:None,population:None,grading_system:Some("HAS — Grade des recommandations".into()),qualifier:None},text,heading_path:table.heading_path.clone(),page:None});
  }if !statements.is_empty(){pending.push((table.block_id.clone(),statements));}
 }for(table,statements)in pending{common::insert_table_recommendations(blocks,&table,statements)?;}Ok(records)
}
#[async_trait::async_trait]
impl Adapter for Has{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![]);}let response=f.get_ok(src.catalog_url.as_deref().context("HAS entrypoint missing")?).await?;let pagebase=Url::parse(&response.record.final_url)?;let(cards,nav)=homepage(&response.text(),&pagebase)?;let hub_url=nav.iter().find(|n|n["role"]=="source_publication_search_frontend").and_then(|n|n["url"].as_str()).context("HAS source permitted publications hub link missing")?;let hub=f.get_ok(hub_url).await?;let hub_state=blocked_catalog(&hub.text(),&Url::parse(&hub.record.final_url)?)?;let mut out=vec![];
  // Inventory: the publisher's "Recommandations et guides" RSS feed plus the homepage cards. Cards the
  // source labels as recommendations come first, then the feed, then the other cards.
  let rss_url=src.config.get("rss_feed").and_then(Value::as_str).unwrap_or(RSS_RECOMMENDATIONS).to_string();
  let (feed,rss_fetch,rss_error)=match f.get_ok(&rss_url).await{Ok(r)=>match Url::parse(&r.record.final_url).map_err(anyhow::Error::from).and_then(|b|rss_items(&r.text(),&b)){Ok(v)=>(v,Some(r.record),None),Err(e)=>(vec![],Some(r.record),Some(format!("{e:#}")))},Err(e)=>(vec![],None,Some(format!("{e:#}")))};
  let is_reco=|e:&Value|e["source_type"].as_str().is_some_and(|t|t.to_lowercase().contains("recommandation"));
  let mut entries=cards.iter().filter(|e|is_reco(e)).cloned().collect::<Vec<_>>();entries.extend(feed);entries.extend(cards.iter().filter(|e|!is_reco(e)).cloned());
  let mut seen=HashSet::new();
  for entry in entries{let url=entry["url"].as_str().unwrap().to_string();if !seen.insert(url.clone()){continue;}let work=common::short_hash(&url);let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url,title:entry["title"].as_str().map(String::from),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":work,"catalog_entry":entry,"catalog_fetch":response.record,"rss_feed":rss_url,"rss_fetch":rss_fetch,"rss_error":rss_error,"source_search_navigation":nav,"native_catalog_boundary":hub_state,"hub_fetch":hub.record,"inventory_scope":"homepage recent-publication cards and the 'Recommandations et guides' RSS feed; the robots-disallowed search is not queried or replaced by an invented API"})};
   let result=async{let record=f.get_ok(&row.url).await?;let base=Url::parse(&record.record.final_url)?;detail(&record.text(),&base)}.await;
   match result{Ok(parsed)=>{let anchor=parsed.pdf.clone().unwrap_or_else(||parsed.metadata["canonical"].as_str().unwrap_or_default().to_string());row.doc_key=format!("{}-{}",parsed.key,&crate::common::sha256_hex(anchor.as_bytes())[..12]);row.title=Some(parsed.title);row.rendition=Some(if parsed.pdf.is_some(){"pdf"}else{"html"}.into());row.extra["work_key"]=json!(parsed.key);row.extra["pdf_url"]=json!(parsed.pdf);row.extra["primary_role"]=json!(parsed.primary_role);row.extra["item_type"]=json!(parsed.item_type);row.extra["source_publication_type"]=parsed.metadata["source_publication_type"].clone();},Err(error)=>row.extra["discovery_error"]=json!(format!("{error:#}"))}
   out.push(row);if limit.is_some_and(|n|out.len()>=n){break;}
  }if !limit.is_some_and(|n|out.len()>=n){out.push(PlanRow{source_id:src.id.clone(),doc_key:"source-catalog-search-unresolved".into(),url:hub.record.final_url.clone(),title:Some("HAS — Recommandations du secteur sanitaire (catalog query)".into()),rendition:None,version:None,extra:json!({"source_role":"catalog_acquisition_boundary","native_catalog_boundary":hub_state,"hub_fetch":hub.record,"discovery_error":"Source permitted hub emits recommendation result queries in publisher-robots-disallowed /jcms/fc_2875171/. Homepage cards and the RSS feed are not the full catalog; blocked namespace is not fetched or aliased."})});}Ok(out)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(error)=row.extra["discovery_error"].as_str(){bail!("HAS discovery: {error}");}let record=f.get_ok(&row.url).await?;let pagebase=Url::parse(&record.record.final_url)?;let parsed=detail(&record.text(),&pagebase)?;
  let mut fetches=vec![];if let Ok(value)=serde_json::from_value::<FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(value);}fetches.push(record.record);
  let full=parsed.primary_role.as_deref()==Some("full_recommendations");
  let (mut blocks,mut warnings,source_url)=if let Some(pdf_url)=&parsed.pdf{
   let pdf=f.get_ok(pdf_url).await?;if !pdf.is_pdf(){bail!("HAS primary document is not actual PDF bytes");}if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=pdf.record.sha256){bail!("HAS full recommendations changed after planning; re-plan");}
   let(blocks,warnings,_)=blocks_for(&pdf,None,30).await?;fetches.push(pdf.record);(blocks,warnings,pdf_url.clone())
  }else{
   let html=format!("<div id='has-overview'><h1>{}</h1>{}</div>",parsed.title.replace('&',"&amp;").replace('<',"&lt;"),parsed.overview);let (blocks,mut warnings)=blocks_from_html(&html,Some(&pagebase),Some("#has-overview"));
   warnings.push("No single primary document is identified by the source record; the body is the record's HTML overview and every listed document is an attachment.".into());(blocks,warnings,pagebase.to_string())
  };
  if blocks.is_empty(){bail!("HAS primary document has no extracted body");}if parsed.pdf.is_some(){for b in &mut blocks{b.page=None;}}let recs=recommendations(&mut blocks,&source_url)?;
  let mut rights=common::unknown_rights();rights.terms_url=parsed.terms;rights.evidence_urls.push(source_url.clone());let notices=blocks.iter().filter(|b|b.text.contains("© Haute Autorité de santé")).map(|b|json!({"source_block_id":b.block_id,"text":b.text})).collect::<Vec<_>>();if !notices.is_empty(){rights.rights_holders.push("Haute Autorité de santé".into());rights.note=Some(notices.iter().map(|n|n["text"].as_str().unwrap()).collect::<Vec<_>>().join("\n"));}
  let mut rights_policy=Value::Null;if let Some(url)=rights.terms_url.clone(){match f.get_ok(&url).await{Ok(response)=>{fetches.push(response.record.clone());let evidence=Url::parse(&response.record.final_url)?;match terms(&response.text(),&evidence){Ok(policy)=>{rights.status="restricted".into();rights.license=Some("Reproduction des travaux de la HAS".into());rights.restrictions.push(policy["text"].as_str().unwrap().to_string());rights.evidence_urls.push(evidence.to_string());rights_policy=policy;},Err(error)=>warnings.push(format!("HAS linked terms mapping unresolved: {error:#}"))}},Err(error)=>warnings.push(format!("HAS linked terms unavailable: {error:#}"))}}
  warnings.push("Inventory covers homepage publication cards and the 'Recommandations et guides' RSS feed. Source search submits to publisher-robots-disallowed /jcms/fc_2875171/; no alternate API/alias is used.".into());
  if full{warnings.push("Primary clinical blocks are from the source-labelled full Recommandations PDF, not HTML overview, synthesis or the PDF's method field Fiche mémo. Validation/online/copyright dates are scoped separately. Native R.n grade-column and inline grade mentions retain source associations; certainty is not inferred. Physical PDF pages, diagrams and merged-cell fidelity remain unverified; no current status, geographic applicability or blanket third-party rights inferred.".into());}
  else{warnings.push(format!("Source publication type '{}' (item_type {}): primary document role {}; this is not a full Recommandations text. Other documents are attachments.",parsed.metadata["source_publication_type"].as_str().unwrap_or("unknown"),parsed.item_type,parsed.primary_role.as_deref().unwrap_or("none (HTML overview)")));}
  let published=parsed.dates.iter().find(|d|d.label=="Mis en ligne le").and_then(|d|d.normalized.clone());let publisher=parsed.metadata["source_schema"]["owner"]["name"].as_str().map(String::from);
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:Some(parsed.key),title:parsed.title,url:row.url.clone(),item_type:Some(parsed.item_type),status:"unknown".into(),language:parsed.language,publisher,published,date_observations:parsed.dates,rights,blocks,attachments:parsed.assets,search_metadata:json!({"source_record":parsed.metadata,"source_plan":row.extra,"printed_recommendations":recs,"pdf_rights_notices":notices,"source_reproduction_terms":rights_policy,"coverage":{"source_role":parsed.primary_role.as_deref().unwrap_or("html_overview"),"converted_blocks_available":true,"complete_visual_semantics":false,"physical_pages":"unmapped"}}),fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod item_type_tests{
 use super::*;
 const PATHWAY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/pathway-p_3603304.html"));
 const TOOL:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/tool-p_4584132.html"));
 const SCOPING:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/scoping-note-p_4559627.html"));
 const RSS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/rss-recommandations-et-guides.xml"));
 const RECORD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/detail.html"));
 fn u(s:&str)->Url{Url::parse(s).unwrap()}
 #[test]
 fn records_are_labelled_by_source_type_and_primary_document_instead_of_refused(){
  let d=detail(SCOPING,&u("https://www.has-sante.fr/jcms/p_4559627/fr/l-accompagnement-des-familles-et-la-reponse-a-leurs-besoins-par-les-saad-familles-note-de-cadrage")).unwrap();
  assert_eq!(d.item_type,"scoping_note");assert_eq!(d.primary_role.as_deref(),Some("scoping_note"));assert!(d.pdf.as_deref().unwrap().ends_with("/2026-10/dir2/nc_saadf_validee_vdef.pdf"));assert_eq!(d.metadata["source_publication_type"],"Recommandation de bonne pratique");
  let p=detail(PATHWAY,&u("https://www.has-sante.fr/jcms/p_3603304/fr/parcours-de-soins-palliatifs-pediatriques")).unwrap();
  assert_eq!(p.item_type,"care_pathway");assert_eq!(p.primary_role.as_deref(),Some("guide"));assert!(p.pdf.as_deref().unwrap().ends_with("parcours_de_soins_palliatifs_pediatriques_-_guide.pdf"));assert_eq!(p.assets.len(),11);
  assert_eq!(p.assets.iter().filter(|a|a["role"]=="evidence_report").count(),1);assert!(p.assets.iter().any(|a|a["role"]=="scoping_note"&&a["document_group"]=="Documents complémentaires"));
  assert!(p.dates.iter().any(|x|x.label=="datePublished (schema.org record)"&&x.normalized.as_deref()==Some("2026-10-15")));
  let t=detail(TOOL,&u("https://www.has-sante.fr/jcms/p_4584132/fr/analyse-des-declarations-d-eigs-lies-aux-gaz-a-usage-medical")).unwrap();
  assert_eq!(t.item_type,"practice_improvement_tool");assert_eq!(t.primary_role.as_deref(),Some("other_source_document"));assert!(t.pdf.as_deref().unwrap().ends_with("spa_389_rapport_gaz_medical_cd_20261022_vd.pdf"));
  let r=detail(RECORD,&u("https://www.has-sante.fr/jcms/p_3191108/fr/strategie-therapeutique-du-patient-vivant-avec-un-diabete-de-type-2")).unwrap();assert_eq!(r.item_type,"clinical_practice_guideline");
  assert_eq!(item_type("Recommandation vaccinale",None),"vaccination_recommendation");assert_eq!(item_type("Avis de la HAS",None),"opinion");assert_eq!(item_type("Fiche mémo",Some("other_source_document")),"has_fiche-m-mo".replace('-',"_"));
 }
 #[test]
 fn rss_feed_items_keep_link_title_date_and_never_the_author(){
  let items=rss_items(RSS,&u("https://www.has-sante.fr/feed/Rss2.jsp?id=p_3081452")).unwrap();assert_eq!(items.len(),6);
  assert_eq!(items[1]["url"],"https://www.has-sante.fr/jcms/p_3603304/fr/parcours-de-soins-palliatifs-pediatriques");assert_eq!(items[1]["title"],"Parcours de soins palliatifs pédiatriques");
  assert!(items[0]["feed_pubDate"].as_str().unwrap().contains("Nov 2026"));assert!(items.iter().all(|i|i.get("author").is_none()));
  assert!(rss_items("<rss><channel></channel></rss>",&u("https://www.has-sante.fr/")).is_err());
 }
 #[tokio::test]
 async fn discovery_merges_feed_and_cards_and_extract_labels_overview_only_records(){
  let source:SourceEntry=serde_json::from_value(json!({"id":"has","enabled":true,"access":"unknown","catalog_url":"https://www.has-sante.fr/"})).unwrap();let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,1,2<<20,"").unwrap();
  f.seed_cache_for_tests("https://www.has-sante.fr/",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/catalog.html")).as_bytes(),"text/html");f.seed_cache_for_tests("https://www.has-sante.fr/jcms/fc_2875208/fr/rechercher-une-recommandation-un-avis",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/recommendations-hub.html")).as_bytes(),"text/html");
  f.seed_cache_for_tests(RSS_RECOMMENDATIONS,RSS.as_bytes(),"text/xml");
  let pathway="https://www.has-sante.fr/jcms/p_3603304/fr/parcours-de-soins-palliatifs-pediatriques";f.seed_cache_for_tests(pathway,PATHWAY.as_bytes(),"text/html");
  let rows=Has.discover(&source,&f,None).await.unwrap();
  assert_eq!(rows.iter().filter(|r|r.url==pathway).count(),1);let row=rows.iter().find(|r|r.url==pathway).unwrap();
  assert!(row.extra["discovery_error"].is_null());assert_eq!(row.extra["item_type"],"care_pathway");assert!(row.doc_key.starts_with("p_3603304-"));assert!(row.extra["rss_error"].is_null());
  assert!(rows.iter().all(|r|!r.url.contains("/jcms/fc_2875171/")));assert_eq!(rows.last().unwrap().extra["source_role"],"catalog_acquisition_boundary");
  // An item whose record names no single primary document is extracted from its HTML overview, labelled by type.
  let mut no_docs=PATHWAY.replace("\"hasPart\"","\"hasPartRemoved\"");if no_docs==PATHWAY{no_docs=PATHWAY.replace("hasPart","hasPartRemoved");}
  f.seed_cache_for_tests(pathway,no_docs.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.has-sante.fr/jcms/c_518191/fr/mentions-legales",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/legal.html")).as_bytes(),"text/html");
  let d=Has.extract(&source,&f,row).await.unwrap();assert_eq!(d.item_type.as_deref(),Some("care_pathway"));assert!(d.attachments.is_empty());
  assert!(d.blocks.iter().any(|b|b.text.contains("Soins palliatifs pédiatriques")||b.text.contains("soins palliatifs pédiatriques")));assert!(d.warnings.iter().any(|w|w.contains("HTML overview")));assert_eq!(d.rights.status,"restricted");
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests{
 use super::*;
 const RECORD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/detail.html"));
 #[tokio::test]
 async fn genuine_full_recommendation_page_extracts_clinical_table_rights_dates_and_revision_guard(){
  const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/dt2-r49-source-page-19.pdf"));let sha="fb27656c207d0fbbea9fe89a62b625a5be3c0c0a92e3c1dae317e2b6e9166fd7";assert_eq!(PDF.len(),137109);assert_eq!(crate::common::sha256_hex(PDF),sha);let url=Url::parse("https://www.has-sante.fr/jcms/p_3191108/fr/strategie-therapeutique-du-patient-vivant-avec-un-diabete-de-type-2").unwrap();let parsed=detail(RECORD,&url).unwrap();let source:SourceEntry=serde_json::from_value(json!({"id":"has","enabled":true,"access":"unknown","catalog_url":"https://www.has-sante.fr/"})).unwrap();let mut row=PlanRow{source_id:source.id.clone(),doc_key:format!("{}-{}",parsed.key,&sha[..12]),url:url.to_string(),title:Some(parsed.title),rendition:Some("pdf".into()),version:None,extra:json!({"pdf_sha256":sha,"fixture_scope":{"physical_source_page":19,"source_pages":56,"source_sha256":"d0e25a79a1af9db1769994e4958096d27bce09d9853dff0183e207a6e7f186ce","scope":"one genuine clinical table page, not complete56-page recommendation; full golden32tables tested separately"}})};
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(url.as_str(),RECORD.as_bytes(),"text/html");f.seed_cache_for_tests(parsed.pdf.as_deref().unwrap(),PDF,"application/pdf");f.seed_cache_for_tests("https://www.has-sante.fr/jcms/c_518191/fr/mentions-legales",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/legal.html")).as_bytes(),"text/html");let d=Has.extract(&source,&f,&row).await.unwrap();assert_eq!(d.doc_key,row.doc_key);assert_eq!(d.language.as_deref(),Some("fr"));assert_eq!(d.published.as_deref(),Some("2024-06-06"));assert!(d.date_observations.iter().any(|v|v.label=="Date de validation"&&v.normalized.as_deref()==Some("2024-05-30")));assert_eq!(d.rights.status,"restricted");assert!(d.rights.restrictions.iter().any(|t|t.contains("reproduit en intégralité")&&t.contains("droits de propriété intellectuelle de tiers")));assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("R.49")&&b.text.contains("fonction rénale, cardiaque")&&b.text.contains("impact pondéral (grade C)")));assert!(d.updated.is_none()&&d.country.is_none()&&d.version.is_none());assert!(d.blocks.iter().all(|b|b.page.is_none()));row.extra["pdf_sha256"]=json!("changed");assert!(Has.extract(&source,&f,&row).await.is_err());
 }
 #[tokio::test]
 async fn genuine_discovery_keeps_explicit_catalog_block_without_fetching_its_queries(){
  let source:SourceEntry=serde_json::from_value(json!({"id":"has","enabled":true,"access":"unknown","catalog_url":"https://www.has-sante.fr/"})).unwrap();let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests("https://www.has-sante.fr/",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/catalog.html")).as_bytes(),"text/html");f.seed_cache_for_tests("https://www.has-sante.fr/jcms/fc_2875208/fr/rechercher-une-recommandation-un-avis",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/recommendations-hub.html")).as_bytes(),"text/html");
  let rows=Has.discover(&source,&f,None).await.unwrap();assert!(rows.len()>1);let boundary=rows.last().unwrap();assert_eq!(boundary.extra["source_role"],"catalog_acquisition_boundary");assert_eq!(boundary.extra["native_catalog_boundary"]["catalog_queries"].as_array().unwrap().len(),4);assert!(boundary.extra["discovery_error"].as_str().unwrap().contains("publisher-robots-disallowed"));assert!(rows.iter().all(|r|!r.url.contains("/jcms/fc_2875171/")));assert!(Has.discover(&source,&f,Some(0)).await.unwrap().is_empty());
 }
 #[test]
 fn genuine_public_hub_exposes_blocked_query_and_legal_terms_exclude_third_parties(){
  let hub=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/recommendations-hub.html"));let state=blocked_catalog(hub,&Url::parse("https://www.has-sante.fr/jcms/fc_2875208/fr/rechercher-une-recommandation-un-avis").unwrap()).unwrap();assert_eq!(state["catalog_queries"].as_array().unwrap().len(),4);assert!(state["catalog_queries"].as_array().unwrap().iter().all(|r|r["url"].as_str().unwrap().starts_with("https://www.has-sante.fr/jcms/fc_2875171/fr/resultat-de-recherche?")));assert!(state["catalog_queries"].as_array().unwrap().iter().any(|r|r["label"]=="Recommandations du secteur sanitaire"&&r["url"].as_str().unwrap().contains("RecommandationsProfessionnelles")));
  let legal=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/legal.html"));let policy=terms(legal,&Url::parse("https://www.has-sante.fr/jcms/c_518191/fr/mentions-legales").unwrap()).unwrap();let text=policy["text"].as_str().unwrap();assert!(text.contains("droits de propriété intellectuelle de tiers")&&text.contains("source et la date de leur dernière mise à jour")&&text.contains("reproduit en intégralité")&&text.contains("ni modification, ni ajout, ni publicité"));assert!(!text.contains("Liens Hypertextes"));
 }
 #[test]
 fn genuine_record_separates_full_synthesis_methods_dates_and_language(){
  let url=Url::parse("https://www.has-sante.fr/jcms/p_3191108/fr/strategie-therapeutique-du-patient-vivant-avec-un-diabete-de-type-2").unwrap();let d=detail(RECORD,&url).unwrap();assert_eq!(d.key,"p_3191108");assert_eq!(d.assets.len(),5);assert_eq!(d.language.as_deref(),Some("fr"));assert!(d.pdf.as_deref().unwrap().ends_with("strategie_therapeutique_du_patient_vivant_avec_un_diabete_de_type_2_-_recommandations.pdf"));
  for role in ["full_recommendations","synthesis","methods_report","scoping_note","reader_group_opinion"]{assert_eq!(d.assets.iter().filter(|a|a["role"]==role).count(),1);}
  assert!(d.dates.iter().any(|x|x.label=="Date de validation"&&x.normalized.as_deref()==Some("2024-05-30")));assert!(d.dates.iter().any(|x|x.label=="Mis en ligne le"&&x.normalized.as_deref()==Some("2024-06-06")));assert_eq!(d.metadata["source_schema"]["medicalAudience"]["audienceType"],"healthcare professional");
  assert!(d.metadata["document_record_aliases"].as_array().unwrap().iter().all(|a|a["url"].as_str().unwrap().starts_with("https://www.has-sante.fr/jcms/")));assert!(detail(&RECORD.replace("MedicalGuideline","OtherCreativeWork"),&url).is_err());assert!(detail(&RECORD.replace("healthcare professional","patient"),&url).is_err());
 }
 #[test]
 fn genuine_grade_rows_keep_conditions_numbered_tail_and_original_tables(){
  let gold:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/has/diabetes-full-blocks.json"))).unwrap();let mut blocks:Vec<Block>=serde_json::from_value(gold["blocks"].clone()).unwrap();let original=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).map(|b|(b.block_id.clone(),b.text.clone())).collect::<Vec<_>>();assert_eq!(original.len(),32);
  let records=recommendations(&mut blocks,gold["source_pdf_url"].as_str().unwrap()).unwrap();
  let first=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(label),strength:Some(grade),certainty:None,..} if label=="R.1"&&grade=="A")).unwrap();assert!(first.text.contains("tabac, hypertension artérielle, sédentarité, obésité, dyslipidémie"));
  let titration=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(label),strength:Some(grade),certainty:None,..} if label=="R.49"&&grade=="AE")).unwrap();assert!(titration.text.contains("adaptation de la posologie à la fonction rénale, cardiaque"));assert!(titration.text.contains("(grade AE)"));
  let tail=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(label),strength:Some(grade),certainty:None,..} if label=="R.142"&&grade=="AE")).unwrap();assert!(tail.text.contains("dégludec et glulisine")&&tail.text.contains("données complémentaires"));
  let row54=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(label),strength:Some(grade),certainty:None,..}if label=="R.54"&&grade=="AE")).unwrap();assert!(row54.text.contains("médecins traitants"));assert!(records.iter().any(|r|r["source_identifier"]=="R.54"&&r["following_section_heading_merged_by_conversion"]=="À l’initiation des traitements médicamenteux"));
  assert!(records.iter().any(|r|r["source_identifier"]=="R.49"&&r["inline_grade_mentions"].as_array().unwrap().len()==1&&r["grade_conflict"]==false));for(id,text)in original{assert_eq!(blocks.iter().find(|b|b.block_id==id).unwrap().text,text);}
 }
}
