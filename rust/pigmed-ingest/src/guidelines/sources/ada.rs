//! ADA source-linked Standards presentation; publisher chapters are independent.
use super::{common,Adapter,PlanRow};
use super::{pmc_crosswalk as cw,r2a_crosswalk as r2a};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::{Fetched,Fetcher};
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use std::io::{Cursor,Read};
use url::Url;
pub struct Ada;
fn links(html:&str,base:&Url)->Result<Vec<Value>>{let d=Html::parse_document(html);let mut out=vec![];let mut seen=HashSet::new();for a in d.select(&sel(".text-content-description a[href],.featured-content a[href],.featured-image-cta a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let label=text_of(&a);let role=if url.starts_with("https://doi.org/10.2337/"){match label.as_str(){"Introduction and Methodology"=>"introduction_methodology","Summary of Revisions"=>"summary_of_revisions","Disclosures"=>"disclosures","Index"=>"index",_=>"publisher_chapter"}}else if url.contains("2026-ADA-SOC-Slide-Deck") {"presentation_slide_deck"}else if url.contains("2026-Abridged-Standards-of-Care"){"abridged_collection"}else{continue};if seen.insert((url.clone(),role)){out.push(json!({"url":url,"label":label,"role":role,"rights":"unknown; assess each resource separately"}));}}if out.is_empty(){anyhow::bail!("ADA source links missing")}Ok(out)}
fn pptx(f:&Fetched)->bool{let Ok(mut z)=zip::ZipArchive::new(std::io::Cursor::new(&f.body))else{return false};let types=z.by_name("[Content_Types].xml").is_ok();let deck=z.by_name("ppt/presentation.xml").is_ok();types&&deck}
const DRAWING_NS:&str="http://schemas.openxmlformats.org/drawingml/2006/main";
const OFFICE_REL_NS:&str="http://schemas.openxmlformats.org/officeDocument/2006/relationships";
fn zip_xml(z:&mut zip::ZipArchive<Cursor<&[u8]>>,name:&str)->Result<String>{let part=z.by_name(name).with_context(||format!("PPTX part missing: {name}"))?;if part.size()>16*1024*1024{bail!("PPTX XML part exceeds safety limit: {name}")}let mut text=String::new();part.take(16*1024*1024+1).read_to_string(&mut text).with_context(||format!("PPTX XML part not UTF-8: {name}"))?;Ok(text)}
fn source_slides(bytes:&[u8])->Result<Vec<Value>>{
 let mut z=zip::ZipArchive::new(Cursor::new(bytes)).context("ADA presentation ZIP invalid")?;let presentation=zip_xml(&mut z,"ppt/presentation.xml")?;let relationships=zip_xml(&mut z,"ppt/_rels/presentation.xml.rels")?;let doc=roxmltree::Document::parse(&presentation).context("PPTX presentation XML invalid")?;let rels=roxmltree::Document::parse(&relationships).context("PPTX presentation relationships invalid")?;let base=Url::parse("https://pptx.invalid/ppt/")?;let mut slides=vec![];let mut seen=HashSet::new();
 for (order,s) in doc.descendants().filter(|n|n.is_element()&&n.tag_name().name()=="sldId").enumerate(){let id=s.attribute((OFFICE_REL_NS,"id")).context("PPTX slide relationship ID missing")?;let mut matching=rels.descendants().filter(|r|r.is_element()&&r.tag_name().name()=="Relationship"&&r.attribute("Id")==Some(id));let rel=matching.next().context("PPTX slide relationship missing")?;if matching.next().is_some()||rel.attribute("TargetMode")==Some("External")||!rel.attribute("Type").is_some_and(|t|t.ends_with("/slide")){bail!("PPTX slide relationship ambiguous, external or not a slide")}
  let target=rel.attribute("Target").context("PPTX slide relationship target missing")?;let resolved=base.join(target)?;if resolved.origin()!=base.origin()||!resolved.path().starts_with("/ppt/")||resolved.query().is_some()||resolved.fragment().is_some(){bail!("PPTX slide target escapes presentation package")}let part=resolved.path().trim_start_matches('/').to_string();if !seen.insert(part.clone()){bail!("PPTX slide relationship repeats a slide part")}
  let xml=zip_xml(&mut z,&part)?;let slide=roxmltree::Document::parse(&xml).with_context(||format!("PPTX slide XML invalid: {part}"))?;let mut paragraphs=vec![];
  for (i,p) in slide.descendants().filter(|n|n.is_element()&&n.tag_name().name()=="p"&&n.tag_name().namespace()==Some(DRAWING_NS)).enumerate(){let mut runs=vec![];for child in p.children().filter(|n|n.is_element()){match child.tag_name().name(){"r"|"fld"=>{let text=child.descendants().filter(|n|n.is_element()&&n.tag_name().name()=="t"&&n.tag_name().namespace()==Some(DRAWING_NS)).filter_map(|n|n.text()).collect::<String>();let bold=child.children().find(|n|n.is_element()&&n.tag_name().name()=="rPr").and_then(|r|r.attribute("b")).is_some_and(|b|b=="1"||b=="true");runs.push(json!({"kind":child.tag_name().name(),"text":text,"bold":bold}));},"br"=>runs.push(json!({"kind":"line_break","text":"\n"})),"tab"=>runs.push(json!({"kind":"tab","text":"\t"})),_=>{}}}let text=runs.iter().filter_map(|r|r["text"].as_str()).collect::<String>();let object=p.ancestors().find(|n|n.is_element()&&matches!(n.tag_name().name(),"sp"|"graphicFrame"|"pic"));let object_kind=object.map(|o|o.tag_name().name().to_string());let object_id=object.and_then(|o|o.descendants().find(|n|n.is_element()&&n.tag_name().name()=="cNvPr")).and_then(|n|n.attribute("id")).map(String::from);let placeholder=object.and_then(|o|o.descendants().find(|n|n.is_element()&&n.tag_name().name()=="ph")).map(|ph|ph.attribute("type").unwrap_or("body").to_string());let cell=p.ancestors().find(|n|n.is_element()&&n.tag_name().name()=="tc"&&n.tag_name().namespace()==Some(DRAWING_NS));let row=cell.and_then(|c|c.ancestors().find(|n|n.is_element()&&n.tag_name().name()=="tr"&&n.tag_name().namespace()==Some(DRAWING_NS)));let cell_locator=match(row,cell){(Some(r),Some(c))=>Some(json!({"source_row":r.prev_siblings().filter(|n|n.is_element()&&n.tag_name().name()=="tr").count()+1,"source_cell":c.prev_siblings().filter(|n|n.is_element()&&n.tag_name().name()=="tc").count()+1,"grid_span":c.attribute("gridSpan"),"row_span":c.attribute("rowSpan"),"horizontal_merge":c.attribute("hMerge"),"vertical_merge":c.attribute("vMerge")})),_=>None};paragraphs.push(json!({"source_paragraph_index":i,"block_id":format!("pptx-slide-{}-paragraph-{i}",order+1),"text":text,"runs":runs,"object_type":object_kind,"object_id":object_id,"placeholder":placeholder,"table_cell":cell_locator}));}
  let count=|name:&str|slide.descendants().filter(|n|n.is_element()&&n.tag_name().name()==name).count();slides.push(json!({"presentation_order":order+1,"source_slide_part":part,"source_xml_sha256":crate::common::sha256_hex(xml.as_bytes()),"relationship_id":id,"source_slide_id":s.attribute("id"),"paragraphs":paragraphs,"native_objects":{"shapes":count("sp"),"tables":count("tbl"),"pictures":count("pic"),"graphic_frames":count("graphicFrame"),"groups":count("grpSp")}}));
 }
 if slides.is_empty(){bail!("PPTX presentation has no slide relationships")}Ok(slides)
}
/// Deck structure recovered from native slide XML: chapter footers, title placeholders and
/// numbered recommendation paragraphs (bold label run first, bold evidence letters inline).
struct Deck{blocks:Vec<Block>,recommendations:Vec<Value>,chapters:Vec<Value>,multi_letter:usize}
fn para_runs(p:&Value)->Vec<(String,bool)>{p["runs"].as_array().map(|r|r.iter().filter(|r|r["kind"]=="r"||r["kind"]=="fld").map(|r|(r["text"].as_str().unwrap_or_default().to_string(),r["bold"].as_bool()==Some(true))).collect()).unwrap_or_default()}
fn chapter_footer(p:&Value)->Option<(u32,String)>{
 // A free text box (no placeholder) whose visible runs are all bold and read "N. Chapter title".
 if !p["placeholder"].is_null(){return None}
 let runs=para_runs(p);let visible:Vec<_>=runs.iter().filter(|(t,_)|!t.trim().is_empty()).collect();
 if visible.is_empty()||!visible.iter().all(|(_,b)|*b){return None}
 let text=p["text"].as_str()?.trim();let c=regex::Regex::new(r"^(\d{1,2})\.\s+([A-Z].*)$").ok()?.captures(text)?;
 Some((c[1].parse().ok()?,c[2].trim().to_string()))
}
fn rec_parts(p:&Value)->Option<(String,Vec<String>)>{
 let runs=para_runs(p);let mut it=runs.iter().filter(|(t,_)|!t.trim().is_empty());let (first,bold)=it.next()?;
 if !*bold||!regex::Regex::new(r"^\d{1,2}\.\d{1,3}[a-z]?$").ok()?.is_match(first.trim()){return None}
 let letters=it.filter(|(t,b)|*b&&matches!(t.trim(),"A"|"B"|"C"|"E")).map(|(t,_)|t.trim().to_string()).collect();
 Some((first.trim().to_string(),letters))
}
fn deck_blocks(slides:&[Value])->Result<Deck>{
 let mut blocks=vec![];let mut recommendations=vec![];let mut chapters:Vec<Value>=vec![];let mut multi_letter=0;let mut last_title=String::new();let mut last_chapter:Option<u32>=None;
 for slide in slides{
  let order=slide["presentation_order"].as_u64().context("slide order missing")?;let paragraphs=slide["paragraphs"].as_array().context("slide paragraphs missing")?;
  let title=paragraphs.iter().filter(|p|matches!(p["placeholder"].as_str(),Some("title"|"ctrTitle"))).filter_map(|p|p["text"].as_str()).map(str::trim).filter(|t|!t.is_empty()).collect::<Vec<_>>().join(" ");
  let footer=paragraphs.iter().find_map(chapter_footer);
  let mut path=vec![];
  if let Some((n,t))=&footer{
   path.push(format!("{n}. {t}"));
   if last_chapter!=Some(*n){blocks.push(Block{block_id:format!("pptx-slide-{order}-chapter"),kind:BlockKind::Heading{level:1},text:format!("{n}. {t}"),heading_path:vec![],page:None});last_chapter=Some(*n);}
   match chapters.iter_mut().find(|c|c["chapter_number"]==*n){Some(c)=>{c["last_slide"]=json!(order)},None=>chapters.push(json!({"chapter_number":n,"deck_title":t,"first_slide":order,"last_slide":order,"recommendations":0}))}
  }
  if !title.is_empty(){
   if title!=last_title{blocks.push(Block{block_id:format!("pptx-slide-{order}-title"),kind:BlockKind::Heading{level:2},text:title.clone(),heading_path:path.clone(),page:None});last_title=title.clone();}
   path.push(title.clone());
  }
  if path.is_empty(){path.push(format!("Slide {order}"));}
  for p in paragraphs{
   let i=p["source_paragraph_index"].as_u64().context("slide paragraph index missing")?;let text=p["text"].as_str().context("slide paragraph text missing")?;
   // Title, slide number and chapter footer are carried as heading context / metadata, not body text.
   if text.trim().is_empty()||matches!(p["placeholder"].as_str(),Some("title"|"ctrTitle"|"sldNum"))||chapter_footer(p).is_some(){continue;}
   let block_id=format!("pptx-slide-{order}-paragraph-{i}");
   let kind=match rec_parts(p){
    Some((label,letters))=>{
     if letters.len()>1{multi_letter+=1;}
     if let Some(n)=footer.as_ref().map(|f|f.0){if let Some(c)=chapters.iter_mut().find(|c|c["chapter_number"]==n){c["recommendations"]=json!(c["recommendations"].as_u64().unwrap_or(0)+1);}}
     recommendations.push(json!({"block_id":block_id,"source_label":label,"source_evidence_letters":letters,"slide":order,"chapter_number":footer.as_ref().map(|f|f.0)}));
     BlockKind::Recommendation{label:Some(label),strength:None,certainty:(letters.len()==1).then(||letters[0].clone()),population:None,grading_system:None,qualifier:None}
    }
    None=>BlockKind::Paragraph,
   };
   blocks.push(Block{block_id,kind,text:text.trim().into(),heading_path:path.clone(),page:None});
  }
 }
 if !blocks.iter().any(|b|matches!(b.kind,BlockKind::Paragraph|BlockKind::Recommendation{..})){bail!("ADA presentation yielded no native paragraph text")}
 Ok(Deck{blocks,recommendations,chapters,multi_letter})
}
/// Catalog chapter DOIs related to the deck. Scope says whether a deck chapter footer matches the catalog label.
fn chapter_relations(items:&[Value],chapters:&mut [Value],catalog_url:&str)->Vec<crate::guidelines::doc::Relation>{
 let num=regex::Regex::new(r"(?i)^https://doi\.org/(10\.2337/dc\d{2}-s(\d{3}))$").unwrap();let norm=|s:&str|s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();let mut out=vec![];
 for item in items.iter().filter(|x|x["role"]=="publisher_chapter"){
  let (Some(url),Some(label))=(item["url"].as_str(),item["label"].as_str()) else{continue};let Some(c)=num.captures(url) else{continue};let n:u32=c[2].parse().unwrap_or(0);
  // Chapter number (DOI suffix sNNN vs deck footer "N.") is the key; a differing printed title is stated, not hidden.
  let matched=chapters.iter_mut().find(|ch|ch["chapter_number"]==n);
  let scope=match matched{Some(ch)=>{ch["doi"]=json!(&c[1]);ch["catalog_label"]=json!(label);let deck=ch["deck_title"].as_str().unwrap_or_default().to_string();let title_note=if norm(deck.as_str())==norm(label){String::new()}else{format!("; deck footer prints the title as \"{deck}\"")};format!("Standards chapter {n} ({label}): the deck presents this chapter's recommendations on slides {}-{}{title_note}; the journal chapter is the full text",ch["first_slide"],ch["last_slide"])},None=>format!("Catalog-listed Standards chapter {n} ({label}); no deck slide carries this chapter footer")};
  out.push(crate::guidelines::doc::Relation{rel:"related".into(),target:format!("doi:{}",&c[1]),scope:Some(scope),evidence_url:Some(catalog_url.into())});
 }
 out
}
fn notice(html:&str)->Option<String>{let d=Html::parse_document(html);let found=d.select(&sel(".featured-content")).find(|e|text_of(e).contains("Permission is required from the Association for any commercial use")&&text_of(e).contains("slide content is not altered")).map(|e|text_of(&e));found}
// R2a-crosswalk: the site scrape (unchanged) lives in `discover_site`/`extract_site`; the trait impl below adds
// relation-only PMC links for the 17 chapter DOIs the source index states (their PMC records are closed).
impl Ada{
 async fn discover_site(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{if src.config["journal_chapters"].as_bool()==Some(true){return journal::discover(src,f,limit).await;}if limit==Some(0){return Ok(vec![])}let cat=f.get_ok(src.catalog_url.as_deref().context("ADA source index missing")?).await?;let b=Url::parse(&cat.record.final_url)?;let items=links(&cat.text(),&b)?;let slide_item=items.iter().find(|x|x["role"]=="presentation_slide_deck").context("ADA slide deck link missing")?;let slide=slide_item["url"].as_str().context("ADA slide deck URL missing")?;let source_title=slide_item["label"].as_str().unwrap_or("ADA Standards of Care slide deck").to_string();let data=f.get_ok(slide).await?;if !pptx(&data){bail!("ADA presentation is not a valid PPTX container")}Ok(vec![PlanRow{source_id:src.id.clone(),doc_key:format!("standards-presentation-{}",&data.record.sha256[..12]),url:slide.into(),title:Some(source_title),rendition:Some("pptx".into()),version:None,extra:json!({"pptx_sha256":data.record.sha256,"catalog_fetch":cat.record,"catalog_url":src.catalog_url})}])}
 async fn extract_site(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{if row.extra["ada_role"]=="journal_chapter"{return journal::extract(src,f,row).await;}let catalog_url=row.extra["catalog_url"].as_str().context("ADA source index URL missing")?;let cat=f.get_ok(catalog_url).await?;let base=Url::parse(&cat.record.final_url)?;let items=links(&cat.text(),&base)?;if !items.iter().any(|x|x["role"]=="presentation_slide_deck"&&x["url"]==row.url){bail!("ADA source no longer emits presentation link")}let data=f.get_ok(&row.url).await?;if !pptx(&data)||row.extra["pptx_sha256"].as_str().is_some_and(|h|h!=data.record.sha256){bail!("ADA presentation changed or invalid")}let slides=source_slides(&data.body)?;let mut deck=deck_blocks(&slides)?;let relations=chapter_relations(&items,&mut deck.chapters,catalog_url);let blocks=deck.blocks;let mut warnings=Vec::new();warnings.push("Source slide deck is a presentation of the recommendations, not the full journal chapters (see related chapter DOIs). Recommendation blocks are paragraphs that open with a bold source label (e.g. 2.17); bold A/B/C/E letters printed inside the same paragraph are the source evidence letters and stay inline in the text. Pictures (tables, algorithms), speaker notes and visual reading order are not extracted; review the original PPTX for them.".into());if deck.multi_letter>0{warnings.push(format!("{} recommendations print more than one evidence letter for different sentences; certainty is left empty for them and the letters stay inline and in search_metadata.source_recommendations.",deck.multi_letter));}let rights=if let Some(n)=notice(&cat.text()){Rights{status:"restricted".into(),note:Some(n),evidence_urls:vec![catalog_url.into()],..Default::default()}}else{warnings.push("ADA slide deck usage notice not recognised; rights unknown.".into());Rights{status:"unknown".into(),..Default::default()}};let fetches=vec![cat.record,data.record];Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some("guideline_presentation".into()),status:"unknown".into(),rights,relations,search_metadata:json!({"source_slides":slides,"source_recommendations":deck.recommendations,"deck_chapters":deck.chapters,"source_order_note":"Slide order follows presentation relationships; paragraph and run indices follow source XML order only, not inferred visual reading order."}),attachments:items,blocks,fetches,warnings,..Default::default()})}
}
/// Chapter DOIs as the source index prints them (`https://doi.org/10.2337/dcNN-sNNN` publisher-chapter links).
fn chapter_dois(items:&[Value])->Vec<(String,String,String)>{let num=regex::Regex::new(r"(?i)^https://doi\.org/(10\.2337/dc\d{2}-s\d{3})$").unwrap();items.iter().filter(|x|x["role"]=="publisher_chapter").filter_map(|x|{let u=x["url"].as_str()?;let c=num.captures(u)?;Some((c[1].to_string(),u.to_string(),x["label"].as_str().unwrap_or_default().to_string()))}).collect()}
#[async_trait::async_trait]
impl Adapter for Ada{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  let rows=self.discover_site(src,f,limit).await?;let mut seen=HashSet::new();let mut groups=vec![];
  for mut row in rows{
   if row.extra["ada_role"]=="journal_chapter"||row.extra["catalog_url"].as_str().is_none(){groups.push((row,vec![]));continue}
   let catalog_url=row.extra["catalog_url"].as_str().unwrap_or_default().to_string();
   let cat=f.get_ok(&catalog_url).await?;let items=links(&cat.text(),&Url::parse(&cat.record.final_url)?)?;
   let site=r2a::SiteRef{doc_key:Some(row.doc_key.clone()),url:catalog_url.clone()};let mut out=r2a::Outcome::default();
   for(doi,url,label) in chapter_dois(&items){
    let sref=cw::SourceRef{source_url:url,evidence_url:Some(catalog_url.clone()),source_title:Some(label),item_type:Some("guideline_chapter".into()),key_prefix:r2a::key_prefix(src),id_route:"source_doi".into(),notes:vec![]};
    out.merge(r2a::adopt(src,f,&r2a::single(Some(&doi),None),&sref,&site,&json!({}),&mut seen).await);
   }
   out.record(&mut row.extra);groups.push((row,out.rows));
  }
  Ok(r2a::interleave(groups,limit))
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if cw::is_crosswalk_row(row){return r2a::extract(src,f,row).await}
  let mut d=self.extract_site(src,f,row).await?;r2a::annotate(&mut d,src,&row.extra);Ok(d)
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod tests{use super::*;#[test]fn genuine_index_links_chapters_and_presentation_with_own_notice(){let html=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ada/catalog.html"));let base=Url::parse("https://professional.diabetes.org/SOC").unwrap();let items=links(html,&base).unwrap();assert!(items.iter().any(|x|x["role"]=="publisher_chapter"&&x["url"]=="https://doi.org/10.2337/dc26-s002"));assert!(items.iter().any(|x|x["role"]=="presentation_slide_deck"&&x["label"].as_str().unwrap().contains("2026")));assert!(items.iter().any(|x|x["role"]=="summary_of_revisions"));assert!(items.iter().any(|x|x["role"]=="introduction_methodology"));assert!(notice(html).unwrap().contains("slide content is not altered"));}}
#[cfg(test)]mod chapter_relation_tests{
 use super::*;
 // Live deck (2026-11-17) prints chapter 5 as "Facilitating Positive Behaviors..." while the catalog says
 // "...Positive Health Behaviors..."; the chapter number keys the relation and the difference is stated.
 #[test]fn chapter_number_keys_relation_and_title_difference_is_stated(){
  let items=vec![json!({"url":"https://doi.org/10.2337/dc26-s005","label":"Facilitating Positive Health Behaviors and Well-being to Improve Health Outcomes","role":"publisher_chapter"}),json!({"url":"https://doi.org/10.2337/dc26-sint","label":"Introduction and Methodology","role":"introduction_methodology"})];
  let mut chapters=vec![json!({"chapter_number":5,"deck_title":"Facilitating Positive Behaviors and Well-being to Improve Health Outcomes","first_slide":84,"last_slide":113,"recommendations":57})];
  let rel=chapter_relations(&items,&mut chapters,"https://professional.diabetes.org/SOC");
  assert_eq!(rel.len(),1);assert_eq!(rel[0].target,"doi:10.2337/dc26-s005");let scope=rel[0].scope.as_deref().unwrap();
  assert!(scope.contains("slides 84-113")&&scope.contains("deck footer prints the title as \"Facilitating Positive Behaviors"));assert_eq!(chapters[0]["doi"],"10.2337/dc26-s005");
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod source_inventory_gold_tests{
 use super::*;
 #[test]fn source_inventory_identifies_real_attribution_and_conditional_slides(){let mut text=String::new();flate2::read::GzDecoder::new(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ada/source-slide-inventory.json.gz")).as_slice()).read_to_string(&mut text).unwrap();let v:Value=serde_json::from_str(&text).unwrap();assert_eq!(v["source_sha256"],"034f66ea0fda2bab10121e1642492922aa875afabdd674f9b72cdf00cc4f122b");let slides=v["slides"].as_array().unwrap();assert_eq!(slides.len(),347);let source=|part:&str|slides.iter().find(|x|x["part"]==part).unwrap()["text_fragments"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<String>();let a=source("ppt/slides/slide1.xml");assert!(a.contains("not altered")&&a.contains("attribution")&&a.contains("commercial use"));let b=source("ppt/slides/slide31.xml");assert!(b.contains("2.17")&&b.contains("If initial screening results are normal")&&b.contains("annually"));let c=source("ppt/slides/slide37.xml");assert!(c.contains("2.24b")&&c.contains("If an OGTT is not feasible")&&c.contains("within 3 months"));}
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod genuine_subset_extraction_tests{
 use super::*;
 #[tokio::test]async fn source_slide_excerpt_extracts_native_blocks_and_checks_revision(){
  // The test-only URL and one substituted catalog link identify an excerpt;
  // the subset is never seeded under the full source deck URL.
  let fixture=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ada/source-slides-1-31-37-small.pptx"));
  let manifest:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ada/source-slides-1-31-37-small-provenance.json"))).unwrap();
  assert_eq!(crate::common::sha256_hex(fixture),manifest["fixture_sha256"].as_str().unwrap());assert_eq!(manifest["source_sha256"],"034f66ea0fda2bab10121e1642492922aa875afabdd674f9b72cdf00cc4f122b");
  let catalog=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ada/catalog.html"));let original="/sites/dpro/files/2025-12/2026-ADA-SOC-Slide-Deck-all-recommendations-12-8-25.pptx";let fixture_path="/fixtures/2026-ADA-SOC-Slide-Deck-source-excerpt.pptx";assert!(catalog.contains(original));let catalog_fixture=catalog.replace(original,fixture_path);
  let index_url="https://professional.diabetes.org/SOC";let excerpt_url="https://professional.diabetes.org/fixtures/2026-ADA-SOC-Slide-Deck-source-excerpt.pptx";let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,8<<20,"").unwrap();f.seed_cache_for_tests(index_url,catalog_fixture.as_bytes(),"text/html");f.seed_cache_for_tests(excerpt_url,fixture,"application/vnd.openxmlformats-officedocument.presentationml.presentation");let src:SourceEntry=serde_json::from_value(json!({"id":"ada","access":"public","enabled":true,"catalog_url":index_url})).unwrap();let mut row=Ada.discover(&src,&f,Some(1)).await.unwrap().remove(0);let doc=Ada.extract(&src,&f,&row).await.unwrap();let slides=doc.search_metadata["source_slides"].as_array().unwrap();assert_eq!(slides.len(),3);assert_eq!(slides[0]["source_slide_part"],"ppt/slides/slide1.xml");assert_eq!(slides[1]["source_slide_part"],"ppt/slides/slide31.xml");assert_eq!(slides[2]["source_slide_part"],"ppt/slides/slide37.xml");for slide in slides{let part=slide["source_slide_part"].as_str().unwrap();assert_eq!(slide["source_xml_sha256"],manifest["unaltered_slide_xml_sha256"][part]);}let chapter="2. Diagnosis and Classification of Diabetes";assert!(doc.blocks.iter().any(|b|b.heading_path==[chapter,"Prediabetes and Type 2 Diabetes (continued)"]&&b.text.contains("If initial screening results are normal")));assert!(doc.blocks.iter().any(|b|b.heading_path==[chapter,"Cystic Fibrosis-Related Diabetes"]&&b.text.contains("If an OGTT is not feasible")));
  // Numbered statements keep the source label and the single printed evidence letter; a statement
  // printing two letters for different sentences keeps both inline and leaves certainty empty.
  let recs=doc.blocks.iter().filter_map(|b|match &b.kind{BlockKind::Recommendation{label,strength,certainty,..}=>{assert!(strength.is_none());Some((label.clone().unwrap(),certainty.clone(),b.text.clone()))},_=>None}).collect::<Vec<_>>();
  assert_eq!(recs.iter().map(|r|r.0.as_str()).collect::<Vec<_>>(),["2.17","2.18","2.24a","2.24b","2.25"]);
  assert_eq!(recs.iter().map(|r|r.1.as_deref()).collect::<Vec<_>>(),[Some("E"),Some("B"),Some("B"),None,Some("E")]);
  assert!(recs[3].2.contains("within 3 months. C An A1C value")&&recs[3].2.ends_with("B"));
  assert_eq!(doc.search_metadata["source_recommendations"][3]["source_evidence_letters"],json!(["C","B"]));
  assert!(doc.warnings.iter().any(|w|w.starts_with("1 recommendations print more than one")));
  // Slide numbers and the chapter footer are context, not body text; the chapter becomes one heading.
  assert!(!doc.blocks.iter().any(|b|b.text=="31"||b.text=="37"));assert_eq!(doc.blocks.iter().filter(|b|b.text==chapter&&matches!(b.kind,BlockKind::Heading{level:1})).count(),1);
  // Every catalog chapter DOI is related; chapter 2 is matched to the slides that present it.
  assert_eq!(doc.relations.len(),17);let r2=doc.relations.iter().find(|r|r.target=="doi:10.2337/dc26-s002").unwrap();assert_eq!(r2.rel,"related");assert!(r2.scope.as_deref().unwrap().contains("slides 2-3"));assert!(doc.relations.iter().find(|r|r.target=="doi:10.2337/dc26-s009").unwrap().scope.as_deref().unwrap().contains("no deck slide carries this chapter footer"));
  assert_eq!(doc.search_metadata["deck_chapters"][0]["doi"],"10.2337/dc26-s002");assert_eq!(doc.search_metadata["deck_chapters"][0]["recommendations"],5);assert!(slides.iter().flat_map(|s|s["paragraphs"].as_array().unwrap()).any(|p|p["block_id"].as_str().is_some_and(|id|doc.blocks.iter().any(|b|b.block_id==id))));row.extra["pptx_sha256"]=json!("not-the-fixture-sha");assert!(Ada.extract(&src,&f,&row).await.unwrap_err().to_string().contains("changed or invalid"));
 }
}
// Journal extraction is opt-in: the existing presentation remains a separate rendition.
mod journal {
    use super::*;
    use crate::guidelines::doc::Relation;
    use crate::guidelines::html::blocks_from_html;
    use scraper::{ElementRef, Node};
    use std::collections::HashMap;

    const ROOT: &str = ".article-body .widget-ArticleFulltext";

    // Explicit leaves must already have a publisher URL observed by the operator and a catalog DOI.
    // This is a first-choice route selection, never a retry after a challenge or entitlement error.
    fn configured_leaf(src: &SourceEntry, doi: &str, catalog: &str) -> Result<Option<String>> {
        let Some(entries) = src.config["source_linked_documents"].as_array() else {
            return Ok(None);
        };
        if entries.is_empty() || entries.len() > 17 {
            bail!("ADA explicit chapter bound must be 1..=17");
        }
        let mut found = None;
        for entry in entries {
            let declared = entry["doi"]
                .as_str()
                .context("ADA configured leaf DOI missing")?;
            let url = entry["url"]
                .as_str()
                .context("ADA configured leaf URL missing")?;
            let evidence = entry["source_evidence_url"]
                .as_str()
                .context("ADA configured leaf evidence URL missing")?;
            let u = Url::parse(url)?;
            if u.scheme() != "https"
                || u.host_str() != Some("diabetesjournals.org")
                || !u.path().starts_with("/care/article/")
                || u.query().is_some()
                || u.fragment().is_some()
                || !u.username().is_empty()
                || u.password().is_some()
                || evidence != catalog
            {
                bail!("ADA configured leaf is not an evidenced stable publisher article URL");
            }
            if declared.eq_ignore_ascii_case(doi) {
                if found.replace(url.to_string()).is_some() {
                    bail!("ADA configured DOI repeated");
                }
            }
        }
        Ok(found)
    }

    pub async fn discover(
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let cat = f
            .get_ok(
                src.catalog_url
                    .as_deref()
                    .context("ADA source index missing")?,
            )
            .await?;
        let base = Url::parse(&cat.record.final_url)?;
        let mut out = Vec::new();
        for item in links(&cat.text(), &base)? {
            if item["role"] != "publisher_chapter" {
                continue;
            }
            let url = item["url"].as_str().context("ADA chapter URL missing")?;
            let doi = url
                .strip_prefix("https://doi.org/")
                .context("ADA chapter is not a source DOI link")?;
            // Only the observed Standards chapter DOI family, never fabricated chapter numbers.
            if !regex::Regex::new(r"(?i)^10\.2337/dc\d{2}-s\d{3}$")?.is_match(doi) {
                continue;
            }
            let configured = configured_leaf(src, doi, src.catalog_url.as_deref().unwrap())?;
            if src.config["source_linked_only"].as_bool() == Some(true) && configured.is_none() {
                continue;
            }
            let terminal = configured.unwrap_or_else(|| url.to_string());
            out.push(PlanRow {
                source_id: src.id.clone(), doc_key: common::slug(doi), url: terminal,
                title: item["label"].as_str().map(String::from), rendition: Some("html".into()), version: None,
                // `doi` is the crosswalk key (DOI -> PMCID); it is the DOI printed by the catalog, never derived.
                extra: json!({"ada_role":"journal_chapter", "source_doi":doi,"doi":doi,"source_link_url":url,
                    "catalog_url":src.catalog_url,"catalog_fetch":cat.record,
                    "coverage":"Only chapter DOI links printed by this catalog; archives, corrections and complete journal enumeration unverified"}),
            });
            if limit.is_some_and(|n| out.len() >= n) {
                break;
            }
        }
        if out.is_empty() {
            bail!("ADA source index has no observed Standards chapter DOI links");
        }
        Ok(out)
    }

    pub async fn extract(src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        let cat_url = row.extra["catalog_url"]
            .as_str()
            .context("ADA chapter catalog evidence missing")?;
        if src.catalog_url.as_deref() != Some(cat_url) {
            bail!("ADA plan catalog differs from configured source");
        }
        let doi = row.extra["source_doi"]
            .as_str()
            .context("ADA source DOI missing")?;
        let link = row.extra["source_link_url"]
            .as_str()
            .context("ADA source DOI link missing")?;
        if !link.eq_ignore_ascii_case(&format!("https://doi.org/{doi}")) {
            bail!("ADA chapter DOI link mismatch");
        }
        let allowed = configured_leaf(src, doi, cat_url)?.unwrap_or_else(|| link.to_string());
        if row.url != allowed {
            bail!("ADA plan terminal differs from evidenced route selection");
        }
        let cat = f.get_ok(cat_url).await?;
        let items = links(&cat.text(), &Url::parse(&cat.record.final_url)?)?;
        if !items
            .iter()
            .any(|v| v["role"] == "publisher_chapter" && v["url"] == link)
        {
            bail!("ADA source index no longer emits this chapter DOI link");
        }
        let fetched = f.get_ok(&row.url).await?;
        let base = Url::parse(&fetched.record.final_url)?;
        if base.host_str() != Some("diabetesjournals.org")
            || !base.path().starts_with("/care/article/")
        {
            bail!("ADA chapter did not resolve to an observed publisher article route");
        }
        let mut doc = parse(&fetched.text(), &base, doi)?;
        doc.source_id = src.id.clone();
        doc.doc_key = row.doc_key.clone();
        doc.fetches = vec![cat.record, fetched.record];
        doc.search_metadata["catalog_evidence"] = json!({"url":cat_url,"link_url":link,"terminal_route":row.url,"role":"publisher_chapter","coverage":row.extra["coverage"]});
        Ok(doc)
    }

    fn norm(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    fn raw_text(e: ElementRef<'_>) -> String {
        e.text().collect::<String>()
    }
    fn key(e: ElementRef<'_>) -> String {
        format!("{:?}", e.id())
    }
    fn class(e: ElementRef<'_>, c: &str) -> bool {
        e.value().classes().any(|v| v == c)
    }
    fn modal(e: ElementRef<'_>) -> bool {
        e.ancestors()
            .filter_map(ElementRef::wrap)
            .chain(std::iter::once(e))
            .any(|a| {
                a.value().attr("role") == Some("dialog")
                    || class(a, "modal")
                    || class(a, "table-modal")
                    || class(a, "fig-modal")
            })
    }
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    fn is_footnote(e: ElementRef<'_>) -> bool {
        e.ancestors()
            .filter_map(ElementRef::wrap)
            .chain(std::iter::once(e))
            .any(|a| {
                a.value().id().is_some_and(|s| s.starts_with("tblfn"))
                    || class(a, "table-footnotes")
            })
    }
    fn native_table_id(e: ElementRef<'_>) -> Option<String> {
        let re = regex::Regex::new(r"^T\d+$").unwrap();
        std::iter::once(e)
            .chain(e.ancestors().filter_map(ElementRef::wrap))
            .filter_map(|a| a.value().id())
            .find(|s| re.is_match(s))
            .map(String::from)
    }
    fn links_in(e: ElementRef<'_>, base: &Url) -> Vec<Value> {
        e.select(&sel("a[href]"))
            .filter_map(|a| {
                let href = a.value().attr("href")?;
                let url = base.join(href).ok()?;
                (matches!(url.scheme(), "https" | "http")
                    && url.username().is_empty()
                    && url.password().is_none())
                .then(|| json!({"href":href,"url":url.as_str(),"text":raw_text(a)}))
            })
            .collect()
    }
    // Decoded source text runs retain exact adjacency and inline grade locations. No clause boundary is inferred.
    fn runs(e: ElementRef<'_>) -> Value {
        let mut pos = 0;
        let mut out = vec![];
        for n in e.descendants() {
            let Node::Text(t) = n.value() else {
                continue;
            };
            let text = t.to_string();
            let start = pos;
            pos += text.len();
            let bold = n
                .ancestors()
                .take_while(|n| n.id() != e.id())
                .filter_map(ElementRef::wrap)
                .find(|a| matches!(a.value().name(), "b" | "strong"));
            let grade = bold
                .map(raw_text)
                .map(|s| norm(&s))
                .filter(|s| matches!(s.as_str(), "A" | "B" | "C" | "E"));
            out.push(
                json!({"text":text,"decoded_text_byte_start":start,"decoded_text_byte_end":pos,
                "inline_bold_text":bold.map(raw_text),"source_evidence_letter":grade}),
            );
        }
        json!({"decoded_source_text":raw_text(e),"runs":out,
            "grade_scope":"Source inline position only; letters may qualify distinct spans. No normalized strength, certainty or inferred clause boundaries."})
    }

    struct Clean {
        ids: HashMap<String, String>,
        footnote_ids: HashSet<String>,
        used: HashSet<String>,
        skipped_modals: usize,
    }
    impl Clean {
        fn write(&mut self, e: ElementRef<'_>, out: &mut String) {
            if modal(e) {
                self.skipped_modals += 1;
                return;
            }
            if matches!(
                e.value().name(),
                "script" | "style" | "noscript" | "nav" | "button" | "form" | "template"
            ) {
                return;
            }
            let original = e.value().id();
            let mut id = original
                .map(String::from)
                .unwrap_or_else(|| format!("ada-node-{}", self.ids.len() + 1));
            if !self.used.insert(id.clone()) {
                id = format!("{id}-ada-node-{}", self.ids.len() + 1);
                self.used.insert(id.clone());
            }
            self.ids.insert(key(e), id.clone());
            if is_footnote(e) {
                self.footnote_ids.insert(id.clone());
            }
            let tag = e.value().name();
            out.push('<');
            out.push_str(tag);
            out.push_str(" id=\"");
            out.push_str(&esc(&id));
            out.push('"');
            for (name, value) in e.value().attrs() {
                if name != "id" {
                    out.push(' ');
                    out.push_str(name);
                    out.push_str("=\"");
                    out.push_str(&esc(value));
                    out.push('"');
                }
            }
            out.push('>');
            for child in e.children() {
                match child.value() {
                    Node::Text(t) => out.push_str(&esc(t)),
                    Node::Element(_) => {
                        if let Some(c) = ElementRef::wrap(child) {
                            self.write(c, out)
                        }
                    }
                    _ => {}
                }
            }
            if !matches!(
                tag,
                "img" | "br" | "hr" | "input" | "source" | "wbr" | "meta" | "link"
            ) {
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }

    fn recommendation_items<'a>(root: ElementRef<'a>) -> Result<Vec<ElementRef<'a>>> {
        let mut items = vec![];
        let mut seen = HashSet::new();
        let mut groups = 0;
        for h in root.select(&sel("h4.section-title")) {
            if modal(h)
                || !matches!(
                    norm(&raw_text(h)).as_str(),
                    "Recommendation" | "Recommendations"
                )
            {
                continue;
            }
            groups += 1;
            let id = h
                .value()
                .id()
                .context("ADA recommendation heading source ID missing")?;
            let wrappers = root
                .select(&sel(".article-section-wrapper[data-section-parent-id]"))
                .filter(|w| !modal(*w) && w.value().attr("data-section-parent-id") == Some(id))
                .collect::<Vec<_>>();
            if wrappers.len() != 1 {
                bail!("ADA recommendation heading {id} has missing or ambiguous source wrapper");
            }
            let mut count = 0;
            for child in wrappers[0].children().filter_map(ElementRef::wrap) {
                let candidates = if child.value().name() == "p" {
                    vec![child]
                } else if child.value().name() == "ul" && class(child, "list-simple") {
                    child
                        .children()
                        .filter_map(ElementRef::wrap)
                        .filter(|e| e.value().name() == "li")
                        .flat_map(|li| {
                            li.children()
                                .filter_map(ElementRef::wrap)
                                .filter(|p| p.value().name() == "p")
                        })
                        .collect()
                } else {
                    vec![]
                };
                for p in candidates {
                    if seen.insert(key(p)) {
                        items.push(p);
                        count += 1;
                    }
                }
            }
            if count == 0 {
                bail!("ADA recommendation heading {id} has no recognized source statements");
            }
        }
        if groups == 0 {
            bail!("ADA full chapter recommendation headings absent; possible preview or changed template");
        }
        Ok(items)
    }

    pub(super) fn parse(raw: &str, base: &Url, expected_doi: &str) -> Result<GuidelineDoc> {
        let d = Html::parse_document(raw);
        let text = norm(&raw_text(d.root_element())).to_ascii_lowercase();
        for gate in [
            "performing security verification",
            "verify you are human",
            "just a moment...",
            "access denied",
            "sign in to access this article",
            "purchase access",
            "subscribe to read",
        ] {
            if text.contains(gate) {
                bail!("ADA publisher access gate: {gate}");
            }
        }
        let roots = d
            .select(&sel(ROOT))
            .filter(|e| !modal(*e))
            .collect::<Vec<_>>();
        if roots.len() != 1 {
            bail!("ADA full chapter clinical root missing or ambiguous; no preview fallback");
        }
        let root = roots[0];
        if !root
            .select(&sel("h2,h3,h4,h5,h6"))
            .any(|h| norm(&raw_text(h)).eq_ignore_ascii_case("References"))
        {
            bail!("ADA chapter References endpoint absent; full-body completeness unverified");
        }
        let meta = |name: &str| {
            d.select(&sel(&format!("meta[name='{name}']")))
                .next()
                .and_then(|e| e.value().attr("content"))
                .map(String::from)
        };
        let title = common::first_text(&d, "h1.wi-article-title,h1.article-title")
            .or_else(|| meta("citation_title"))
            .context("ADA chapter title missing")?;
        let edition_re =
            regex::Regex::new(r"(?i)Standards of Care in Diabetes\s*[:—–-]?\s*((?:19|20)\d{2})")?;
        let version = edition_re
            .captures(&title)
            .map(|c| c[1].to_string())
            .context("ADA source title has no explicit Standards edition")?;
        let doi = meta("citation_doi")
            .or_else(|| {
                d.select(&sel("a[href]"))
                    .filter_map(|a| a.value().attr("href"))
                    .find_map(|h| {
                        h.strip_prefix("https://doi.org/10.2337/")
                            .map(|s| format!("10.2337/{s}"))
                    })
            })
            .context("ADA chapter DOI absent")?;
        if !doi.eq_ignore_ascii_case(expected_doi) {
            bail!("ADA terminal DOI disagrees with source catalog link");
        }
        let mut clean = Clean {
            ids: HashMap::new(),
            footnote_ids: HashSet::new(),
            used: HashSet::new(),
            skipped_modals: 0,
        };
        let mut sanitized = String::new();
        clean.write(root, &mut sanitized);
        let (mut blocks, mut warnings) = blocks_from_html(&sanitized, Some(base), None);
        let mut recommendations = vec![];
        let mut labels = HashSet::new();
        let label_re = regex::Regex::new(r"^\d+\.\d+[a-z]?$")?;
        for p in recommendation_items(root)? {
            let number = p
                .select(&sel("strong,b"))
                .next()
                .map(raw_text)
                .map(|s| norm(&s))
                .context("ADA recommendation number absent")?;
            if !label_re.is_match(&number) || !labels.insert(number.clone()) {
                bail!("ADA recommendation number invalid or repeated: {number}");
            }
            let id = clean
                .ids
                .get(&key(p))
                .context("ADA recommendation excluded from clinical root")?;
            let b = blocks
                .iter_mut()
                .find(|b| &b.block_id == id)
                .context("ADA recommendation paragraph not preserved")?;
            b.kind = BlockKind::Recommendation {
                label: Some(number.clone()),
                strength: None,
                certainty: None,
                population: None,
                grading_system: None,
                qualifier: None,
            };
            b.text = norm(&raw_text(p));
            let mut source = runs(p);
            source["block_id"] = json!(id);
            source["source_label"] = json!(number);
            source["source_html"] = json!(p.html());
            recommendations.push(source);
        }
        for b in &mut blocks {
            if clean.footnote_ids.contains(&b.block_id) {
                b.kind = BlockKind::Footnote;
            }
        }
        let mut tables = vec![];
        let mut table_ids = HashSet::new();
        for t in root.select(&sel("table")).filter(|e| !modal(*e)) {
            // Nested native tables belong to their containing table, not a second independent object.
            if t.ancestors()
                .filter_map(ElementRef::wrap)
                .any(|a| a.value().name() == "table")
            {
                continue;
            }
            let block_id = clean
                .ids
                .get(&key(t))
                .context("ADA table source map missing")?;
            let native_id = native_table_id(t);
            if native_id
                .as_ref()
                .is_some_and(|id| !table_ids.insert(id.clone()))
            {
                bail!("ADA repeated main table identity; modal duplicate not resolved");
            }
            let wrapper = native_id
                .as_deref()
                .and_then(|id| {
                    std::iter::once(t)
                        .chain(t.ancestors().filter_map(ElementRef::wrap))
                        .find(|a| a.value().id() == Some(id))
                })
                .unwrap_or(t);
            let caption = t
                .select(&sel("caption"))
                .next()
                .or_else(|| wrapper.select(&sel(".table-caption,.caption")).next())
                .map(|c| norm(&raw_text(c)));
            if let Some(b) = blocks.iter_mut().find(|b| &b.block_id == block_id) {
                if let BlockKind::Table { caption: c } = &mut b.kind {
                    *c = caption.clone();
                }
            }
            let rows=t.select(&sel("tr")).filter(|r|r.ancestors().filter_map(ElementRef::wrap).find(|a|a.value().name()=="table").is_some_and(|a|a.id()==t.id())).enumerate().map(|(r,row)| {
                let cells=common::cells(row).into_iter().enumerate().map(|(c,cell)|json!({"source_cell_index":c,"tag":cell.value().name(),"id":cell.value().id(),"rowspan":cell.value().attr("rowspan"),"colspan":cell.value().attr("colspan"),"headers":cell.value().attr("headers"),"scope":cell.value().attr("scope"),"text":raw_text(cell),"links":links_in(cell,base),"source_html":cell.html()})).collect::<Vec<_>>();
                json!({"source_row_index":r,"id":row.value().id(),"cells":cells})
            }).collect::<Vec<_>>();
            let footnote_links = wrapper
                .select(&sel("a[href^='#tblfn']"))
                .filter_map(|a| a.value().attr("href"))
                .collect::<Vec<_>>();
            tables.push(json!({"block_id":block_id,"source_container_id":native_id,"caption":caption,"rows":rows,"footnote_targets":footnote_links,"source_html":t.html(),"links":links_in(t,base)}));
        }
        let footnotes=root.select(&sel("[id^='tblfn']")).filter(|e|!modal(*e)).map(|e|json!({"source_id":e.value().id(),"block_id":clean.ids.get(&key(e)),"decoded_source_text":raw_text(e),"source_html":e.html(),"links":links_in(e,base)})).collect::<Vec<_>>();
        let figures=root.select(&sel("img")).filter(|e|!modal(*e)).map(|e|json!({"block_id":clean.ids.get(&key(e)),"source_id":e.value().id(),"source_html":e.html(),"parent_source_id":e.ancestors().filter_map(ElementRef::wrap).find_map(|a|a.value().id())})).collect::<Vec<_>>();
        let mut dates = vec![];
        for name in [
            "citation_publication_date",
            "citation_online_date",
            "citation_date",
            "citation_updated_date",
        ] {
            if let Some(raw) = meta(name) {
                let mut obs = common::date(name, &raw, base.as_str());
                let iso = raw.replace('/', "-");
                if regex::Regex::new(r"^\d{4}-\d{2}(?:-\d{2})?$")?.is_match(&iso) {
                    obs.normalized = Some(iso);
                }
                dates.push(obs);
            }
        }
        for e in d.select(&sel(".article-date,.publication-date")) {
            let raw = norm(&raw_text(e));
            if !raw.is_empty() && !dates.iter().any(|v| v.raw == raw) {
                dates.push(common::date("Source article date", &raw, base.as_str()));
            }
        }
        let published = dates
            .iter()
            .find(|v| {
                matches!(
                    v.label.as_str(),
                    "citation_publication_date" | "citation_online_date"
                )
            })
            .and_then(|v| v.normalized.clone());
        let updated = dates
            .iter()
            .find(|v| v.label == "citation_updated_date")
            .and_then(|v| v.normalized.clone());
        let notice = d
            .select(&sel(
                ".copyright,.copyright-statement,.article-copyright,.license,.license-info",
            ))
            .map(|e| norm(&raw_text(e)))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let restricted = notice.iter().any(|s| {
            let l = s.to_lowercase();
            l.contains("permission")
                && (l.contains("text and data mining")
                    || l.contains("text/data mining")
                    || l.contains("machine learning")
                    || l.contains("reproduction"))
        });
        let rights = Rights {
            status: if restricted { "restricted" } else { "unknown" }.into(),
            note: (!notice.is_empty()).then(|| notice.join("\n")),
            restrictions: if restricted { notice.clone() } else { vec![] },
            evidence_urls: vec![base.to_string()],
            ..Default::default()
        };
        if rights.status == "unknown" {
            warnings.push("ADA article reuse rights remain unknown; Free or signed-in availability does not establish reuse permission.".into());
        }
        warnings.push("ADA chapter body fingerprint includes recommendation groups and References; no archive/correction completeness claim. Evidence letters remain source-positioned runs in source_recommendations; scalar strength/certainty are intentionally absent.".into());
        if clean.skipped_modals > 0 {
            warnings.push(format!(
                "Excluded {} publisher modal/dialog duplicate subtrees from clinical blocks.",
                clean.skipped_modals
            ));
        }
        let mut attachments = vec![];
        for a in d.select(&sel("a[href]")) {
            let Some(href) = a.value().attr("href") else {
                continue;
            };
            let Some(u) = common::link(base, href) else {
                continue;
            };
            let parsed = Url::parse(&u)?;
            if parsed.host_str() == Some("diabetesjournals.org")
                && parsed.path().starts_with("/care/article-pdf/")
                && parsed.query().is_none()
                && !attachments.iter().any(|v: &Value| v["url"] == u)
            {
                attachments.push(json!({"url":u,"label":norm(&raw_text(a)),"role":"source_linked_pdf_rendition","bytes_verified":false,"rights":"unknown; assess asset separately"}));
            }
        }
        let chapter = regex::Regex::new(r"(?i)-s(\d{3})$")?
            .captures(&doi)
            .map(|c| c[1].trim_start_matches('0').to_string());
        Ok(GuidelineDoc {
            title,
            url: base.to_string(),
            work_key: chapter.map(|n| format!("standards-chapter-{n}")),
            item_type: Some("guideline_chapter".into()),
            version: Some(version),
            status: "unknown".into(),
            doi: Some(doi),
            language: common::page_language(&d),
            published,
            updated,
            date_observations: dates,
            rights,
            blocks,
            attachments,
            relations: vec![Relation {
                rel: "part_of".into(),
                target: "https://professional.diabetes.org/SOC".into(),
                scope: Some(
                    "Source-linked Standards chapter; edition printed in article title".into(),
                ),
                evidence_url: Some("https://professional.diabetes.org/SOC".into()),
            }],
            search_metadata: json!({"source_role":"full_journal_chapter_html","clinical_root":ROOT,"source_recommendations":recommendations,"source_tables":tables,"source_footnotes":footnotes,"source_figures":figures,"source_dom_note":"HTML source order; native table row/cell positions and spans retained separately from Markdown; decoded text byte offsets refer to each recommendation's decoded_source_text."}),
            warnings,
            ..Default::default()
        })
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod journal_contract_tests {
    use super::*;
    const TERMINAL:&str="https://diabetesjournals.org/care/article/49/Supplement_1/S27/163926/2-Diagnosis-and-Classification-of-Diabetes";
    const DOI: &str = "10.2337/dc26-s002";
    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/ada/journal-contract-synthetic.html"
    ));

    #[test]
    fn synthetic_source_contract_preserves_mixed_grades_geometry_and_modal_deduplication() {
        let doc = journal::parse(FIXTURE, &Url::parse(TERMINAL).unwrap(), DOI).unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("guideline_chapter"));
        assert_eq!(doc.version.as_deref(), Some("2026"));
        assert_eq!(doc.published.as_deref(), Some("2025-12-08"));
        assert_eq!(doc.date_observations.len(), 2);
        assert_eq!(
            doc.date_observations[1].normalized.as_deref(),
            Some("2026-01")
        );
        assert_eq!(doc.work_key.as_deref(), Some("standards-chapter-2"));
        assert_eq!(doc.rights.status, "restricted");
        assert!(doc.rights.license.is_none());
        assert_eq!(doc.attachments.len(), 1);
        let recs = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recs.len(), 3);
        let labels = recs
            .iter()
            .map(|b| match &b.kind {
                BlockKind::Recommendation {
                    label,
                    strength,
                    certainty,
                    ..
                } => {
                    assert!(strength.is_none());
                    assert!(certainty.is_none());
                    label.as_deref().unwrap()
                }
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(labels, ["2.1a", "2.1b", "2.31a"]);
        assert_eq!(
            recs[0].text,
            "2.1a Invented first assertion A and second assertion B."
        );
        assert_eq!(
            recs[0].heading_path,
            ["Synthetic classification", "Recommendation"]
        );
        assert_eq!(
            recs[2].heading_path,
            [
                "Synthetic classification",
                "Synthetic screening",
                "Recommendations"
            ]
        );
        let source = &doc.search_metadata["source_recommendations"];
        let letters = |i: usize| {
            source[i]["runs"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|r| r["source_evidence_letter"].as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(letters(0), ["A", "B"]);
        assert!(letters(1).is_empty());
        assert_eq!(letters(2), ["B", "E"]);
        for rec in source.as_array().unwrap() {
            let decoded = rec["decoded_source_text"].as_str().unwrap();
            for run in rec["runs"].as_array().unwrap() {
                let a = run["decoded_text_byte_start"].as_u64().unwrap() as usize;
                let b = run["decoded_text_byte_end"].as_u64().unwrap() as usize;
                assert_eq!(&decoded[a..b], run["text"].as_str().unwrap());
            }
        }
        let tables = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
            .collect::<Vec<_>>();
        assert_eq!(tables.len(), 1);
        assert!(
            matches!(&tables[0].kind,BlockKind::Table{caption:Some(c)} if c=="Table 2.1. Synthetic geometry.")
        );
        let table = &doc.search_metadata["source_tables"][0];
        assert_eq!(table["source_container_id"], "T1");
        assert_eq!(table["rows"].as_array().unwrap().len(), 3);
        assert_eq!(table["rows"][0]["cells"][1]["colspan"], "2");
        assert_eq!(table["rows"][1]["cells"][0]["rowspan"], "2");
        assert_eq!(table["rows"][1]["cells"][1]["text"], "");
        assert!(table["rows"][1]["cells"][2]["links"][0]["url"]
            .as_str()
            .unwrap()
            .starts_with("https://diabetesjournals.org/care/article/"));
        assert_eq!(table["footnote_targets"][0], "#tblfn1");
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Footnote))
                .count(),
            1
        );
        let figs = doc
            .blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Figure { src_url, .. } => src_url.as_deref(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(figs, ["https://diabetesjournals.org/synthetic/figure1.png"]);
        assert!(!doc
            .blocks
            .iter()
            .any(|b| b.text.contains("Duplicate modal")));
        assert!(doc.warnings.iter().any(|w| w.contains("modal/dialog")));
    }

    #[test]
    fn journal_rejects_gates_previews_identity_mismatch_and_changed_statement_wrappers() {
        let base = Url::parse(TERMINAL).unwrap();
        assert!(
            journal::parse("<h1>Performing security verification</h1>", &base, DOI)
                .unwrap_err()
                .to_string()
                .contains("gate")
        );
        assert!(journal::parse("<main>Free publisher preview</main>", &base, DOI).is_err());
        assert!(journal::parse(
            &FIXTURE.replace("<h2 id=\"refs\">References</h2>", ""),
            &base,
            DOI
        )
        .unwrap_err()
        .to_string()
        .contains("completeness"));
        assert!(journal::parse(FIXTURE, &base, "10.2337/dc26-s001")
            .unwrap_err()
            .to_string()
            .contains("disagrees"));
        assert!(journal::parse(
            &FIXTURE.replace(
                "data-section-parent-id=\"rec-a\"",
                "data-section-parent-id=\"unknown\""
            ),
            &base,
            DOI
        )
        .unwrap_err()
        .to_string()
        .contains("wrapper"));
        let unknown = journal::parse(
            &FIXTURE.replace("article-copyright", "unrecognized-notice"),
            &base,
            DOI,
        )
        .unwrap();
        assert_eq!(unknown.rights.status, "unknown");
        assert!(unknown.rights.license.is_none());
    }

    // Round-2 crosswalk evidence: every catalog chapter row carries a printed DOI that NCBI's ID
    // Converter (response saved 2026-11-17) maps to a PMCID. PMC holds these chapters but the publisher
    // does not allow XML download, so the crosswalk can add relations, not full text.
    #[tokio::test]
    async fn journal_rows_carry_catalog_dois_that_ncbi_maps_to_pmcids() {
        let index = "https://professional.diabetes.org/SOC";
        let catalog = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/ada/catalog.html"));
        let idconv: Value = serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/ada_journals/ncbi-idconv-dc26.json"))).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(index, catalog.as_bytes(), "text/html");
        let src: SourceEntry = serde_json::from_value(json!({"id":"ada_journals","enabled":true,"access":"mixed","catalog_url":index,"config":{"journal_chapters":true}})).unwrap();
        let rows = Ada.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 17);
        for r in &rows {
            let doi = r.extra["doi"].as_str().unwrap();
            assert_eq!(r.extra["source_doi"], doi);
            let rec = idconv["records"].as_array().unwrap().iter().find(|x| x["doi"].as_str().is_some_and(|d| d.eq_ignore_ascii_case(doi))).unwrap();
            assert!(rec["pmcid"].as_str().unwrap().starts_with("PMC"));
        }
    }

    #[tokio::test]
    async fn journal_branch_discovers_only_catalog_chapters_and_extracts_offline() {
        let index = "https://professional.diabetes.org/SOC";
        let catalog = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/ada/catalog.html"
        ));
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(index, catalog.as_bytes(), "text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"ada","enabled":true,"access":"public","catalog_url":index,"config":{"journal_chapters":true}})).unwrap();
        let rows = Ada.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 17);
        assert!(rows
            .iter()
            .all(|r| r.extra["ada_role"] == "journal_chapter"
                && r.rendition.as_deref() == Some("html")));
        assert_eq!(Ada.discover(&src, &f, Some(2)).await.unwrap().len(), 2);
        assert!(Ada.discover(&src, &f, Some(0)).await.unwrap().is_empty());
        let row = rows
            .iter()
            .find(|r| r.url == format!("https://doi.org/{DOI}"))
            .unwrap();
        let mut selected = src.clone();
        selected.config["source_linked_documents"] =
            json!([{"url":TERMINAL,"doi":DOI,"source_evidence_url":index}]);
        let routed_rows = Ada.discover(&selected, &f, None).await.unwrap();
        assert_eq!(routed_rows.len(), 17);
        assert!(routed_rows.iter().any(|r| r.url == TERMINAL));
        assert!(routed_rows.iter().any(|r| r.url.starts_with("https://doi.org/")));
        selected.config["source_linked_only"] = json!(true);
        let selected_rows = Ada.discover(&selected, &f, None).await.unwrap();
        assert_eq!(selected_rows.len(), 1);
        assert_eq!(selected_rows[0].url, TERMINAL);
        assert_eq!(selected_rows[0].extra["source_link_url"], row.url);
        f.seed_cache_for_tests(TERMINAL, FIXTURE.as_bytes(), "text/html");
        let doc = Ada.extract(&selected, &f, &selected_rows[0]).await.unwrap();
        assert_eq!(doc.fetches.len(), 2);
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            3
        );
        let mut bad = selected_rows[0].clone();
        bad.url = "https://diabetesjournals.org/care/article/unobserved".into();
        assert!(Ada
            .extract(&selected, &f, &bad)
            .await
            .unwrap_err()
            .to_string()
            .contains("route selection"));
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]mod r2a_tests{use super::*;use crate::guidelines::sources::pmc_crosswalk::Lookup;
 // The deck row gains relation-only PMC links for the 17 chapter DOIs: real ID Converter answer (B-acp-ada) and the
 // real OAI cannotDisseminateFormat answer that PMC gives for records without OA XML (ADA chapters, PMC12690183).
 #[tokio::test]async fn chapter_dois_give_relation_only_pmc_links(){
  let fixture=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ada/source-slides-1-31-37-small.pptx"));
  let catalog=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ada/catalog.html")).replace("/sites/dpro/files/2025-12/2026-ADA-SOC-Slide-Deck-all-recommendations-12-8-25.pptx","/fixtures/2026-ADA-SOC-Slide-Deck-source-excerpt.pptx");
  let idconv=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/ada_journals/ncbi-idconv-dc26.json"));let not_oa=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/pmc_crosswalk/oai-not-oa.xml"));
  let index="https://professional.diabetes.org/SOC";let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,8<<20,"").unwrap();
  f.seed_cache_for_tests(index,catalog.as_bytes(),"text/html");f.seed_cache_for_tests("https://professional.diabetes.org/fixtures/2026-ADA-SOC-Slide-Deck-source-excerpt.pptx",fixture,"application/vnd.openxmlformats-officedocument.presentationml.presentation");
  let items=links(&catalog,&Url::parse(index).unwrap()).unwrap();let chapters=chapter_dois(&items);assert_eq!(chapters.len(),17);
  let v:Value=serde_json::from_slice(idconv).unwrap();
  for(doi,_,_) in &chapters{let d=cw::normalize_doi(doi).unwrap();f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Doi(d.clone())),idconv,"application/json");
   let p=v["records"].as_array().unwrap().iter().find(|r|r["doi"].as_str().is_some_and(|x|x.eq_ignore_ascii_case(&d))).unwrap()["pmcid"].as_str().unwrap().to_string();f.seed_cache_for_tests(&cw::oai_url(&p).unwrap(),not_oa,"application/xml");}
  let src:SourceEntry=serde_json::from_value(json!({"id":"ada","access":"public","enabled":true,"catalog_url":index})).unwrap();
  let rows=Ada.discover(&src,&f,Some(5)).await.unwrap();assert_eq!(rows.len(),1,"no PMC copy is open, so no extra rows");
  let closed=rows[0].extra[r2a::CLOSED].as_array().unwrap();assert_eq!(closed.len(),17);assert!(closed.iter().all(|c|c["relation"]["target"].as_str().unwrap().starts_with("pmc:PMC1269")));
  let d=Ada.extract(&src,&f,&rows[0]).await.unwrap();assert_eq!(d.relations.len(),34);assert!(d.relations.iter().any(|r|r.target=="pmc:PMC12690183"));
  assert!(d.warnings.iter().any(|w|w.contains("17 in PMC as relation-only links")));
 }
}
