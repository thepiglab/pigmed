//! MSF homepage books -> finite source-emitted English TOC graph -> complete chapter + notes.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc,Relation};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::{HashMap,HashSet};
use url::Url;
pub struct MsfMedical;
const ARTICLE:&str="#block-msfmg-content > article";
const BODY:&str="#block-msfmg-content > article .field--name-body";
fn esc(s:&str)->String{s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;")}
fn target(base:&Url,href:&str)->Option<String>{let u=base.join(href.trim()).ok()?;(matches!(u.scheme(),"http"|"https")&&u.username().is_empty()&&u.password().is_none()).then(||u.to_string())}
fn allowed(u:&Url,base:&Url)->bool{u.host_str()==base.host_str()&&u.query().is_none()&&((u.path().starts_with("/en/viewport/")&&u.path().contains("/english/")&&u.path().ends_with(".html"))||regex::Regex::new(r"^/en/node/\d+$").unwrap().is_match(u.path()))}
fn books(raw:&str,base:&Url)->Result<Vec<(String,String,Value)>>{
 let doc=Html::parse_document(raw);let cards=doc.select(&sel("article#guidelines .homepage-guidelines__guideline")).collect::<Vec<_>>();if cards.is_empty(){bail!("MSF homepage book cards missing");}
 let mut out=vec![];let mut seen=HashSet::new();let node=regex::Regex::new(r"^/en/node/\d+$").unwrap();
 for c in cards{let title=c.select(&sel(".guideline__content-info h2")).next().map(|e|text_of(&e)).context("MSF book title missing")?;
  let mut links=vec![];for a in c.select(&sel(".guideline__languages--item.current a[href]")){let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let u=Url::parse(&url)?;
   if u.host_str()!=base.host_str(){continue;}if node.is_match(u.path())&&seen.insert(url.clone()){out.push((url,title.clone(),Value::Null));}else if u.path().to_lowercase().ends_with(".pdf"){links.push(json!({"url":url,"label":text_of(&a),"role":"book_pdf_rendition","rights":"unknown"}));}
  }
  let note=c.select(&sel(".guideline__pdf-not-updated, .guideline__pdf-warning")).next().map(|e|text_of(&e)).or_else(||text_of(&c).contains("PDF version has not been updated").then(||"Please note that the PDF version has not been updated. Refer to the online guideline for the latest version.".into()));
  if let Some(last)=out.last_mut().filter(|r|r.1==title){last.2=json!({"book_title":title,"renditions":links,"pdf_currency_notice":note});}
 }if out.is_empty(){bail!("MSF cards yielded no source-emitted English book links");}Ok(out)
}
fn toc(raw:&str,base:&Url)->Vec<(String,String)>{
 let doc=Html::parse_document(raw);let mut seen=HashSet::new();doc.select(&sel("#block-msfmg-book-navigation a[href], #block-msfmg-content > article .childpages-macro a[href], nav.book-navigation-pager a[href], head link[rel='up'][href], head link[rel='next'][href], head link[rel='prev'][href]"))
 .filter_map(|a|{let u=common::link(base,a.value().attr("href")?)?;let p=Url::parse(&u).ok()?;if !allowed(&p,base)||!seen.insert(u.clone()){return None;}Some((u,text_of(&a)))}).collect()
}
fn lists(root:ElementRef<'_>,base:&Url,html:&mut String)->Result<Vec<Value>>{
 fn render(list:ElementRef<'_>,base:&Url,key:&str,parent:Option<&str>,path:&[String],nodes:&mut Vec<Value>)->Result<String>{
  let mut result=String::new();for (i,li) in list.children().filter_map(ElementRef::wrap).filter(|e|e.value().name()=="li").enumerate(){
   let key=format!("{key}/{}",i+1);let children=li.select(&sel("ul,ol")).filter(|l|l.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().name()=="li").is_some_and(|e|e.id()==li.id())).collect::<Vec<_>>();
   let mut own=li.inner_html();for child in &children{own=own.replacen(&child.html(),"",1);}let fragment=Html::parse_fragment(&own);let text=text_of(&fragment.root_element());
   let links=fragment.select(&sel("a[href]")).map(|a|json!({"label":text_of(&a),"raw_href":a.value().attr("href"),"url":a.value().attr("href").and_then(|s|target(base,s))})).collect::<Vec<_>>();
   nodes.push(json!({"key":key,"parent":parent,"position":i+1,"source_id":li.value().id(),"list_id":list.value().id(),"list_kind":list.value().name(),"text":text,"parents":path,"links":links}));
   let mut context=path.to_vec();if !text.is_empty(){context.push(text);result.push_str(&format!("<li id=\"{}\">{}</li>",esc(&key),esc(&context.join("\n"))));}
   for (j,child) in children.into_iter().enumerate(){result.push_str(&render(child,base,&format!("{key}/list{}",j+1),Some(&key),&context,nodes)?);}
  }Ok(result)
 }
 let mut out=vec![];for list in root.select(&sel("ul,ol")){
  if list.ancestors().filter_map(ElementRef::wrap).any(|e|e.id()!=list.id()&&matches!(e.value().name(),"ul"|"ol"|"nav"|"table")){continue;}
  if !list.select(&sel("ul,ol")).any(|e|e.id()!=list.id()){continue;}
  if list.select(&sel("table,figure,img")).next().is_some(){bail!("MSF nested clinical list contains unvalidated table/media");}
  let key=format!("msf-list{}",out.len()+1);let mut nodes=vec![];let rendered=render(list,base,&key,None,&[],&mut nodes)?;
  out.push(json!({"key":key,"source_id":list.value().id(),"source_html":list.html(),"nodes":nodes}));*html=html.replacen(&list.html(),&format!("<ul>{rendered}</ul>"),1);
 }Ok(out)
}
struct Page{title:String,html:String,canonical:String,dates:Vec<DateObservation>,notes:Vec<Value>,tree:Vec<Value>,links:Vec<Value>,note_ids:Vec<(String,String)>,is_toc:bool,own_words:usize,up:Option<String>,rights_note:Option<String>,terms:Option<String>,language:Option<String>}
fn page(raw:&str,base:&Url)->Result<Page>{
 let doc=Html::parse_document(raw);let articles=doc.select(&sel(ARTICLE)).collect::<Vec<_>>();if articles.len()!=1{bail!("MSF expected one article, found {}",articles.len());}let article=articles[0];
 let bodies=doc.select(&sel(BODY)).collect::<Vec<_>>();if bodies.len()!=1{bail!("MSF expected one body inside content article, found {}",bodies.len());}let body=bodies[0];
 let title=common::first_text(&doc,"#block-msfmg-page-title h1").context("MSF chapter title missing")?;
 let canonical=doc.select(&sel("head link[rel='canonical'][href]")).next().and_then(|e|common::link(base,e.value().attr("href")?)).context("MSF canonical missing")?;
 if Url::parse(&canonical)?.host_str()!=base.host_str(){bail!("MSF canonical leaves publisher host");}
 let mut selected=body.html();
 // Hover popups repeat complete notes at every citation; their separately
 // labelled footnote/reference widgets below are the authoritative note text.
 for popup in body.select(&sel(".see-footnote__popup")){selected=selected.replacen(&popup.html(),"",1);}
 let clinical=Html::parse_fragment(&selected);let tree=lists(clinical.root_element(),base,&mut selected)?;let mut dates=vec![];
 let date_re=regex::Regex::new(r"^(Content updated|Published|Last updated):\s*(.+)$").unwrap();
 for p in body.select(&sel("p")){let text=text_of(&p);if let Some(c)=date_re.captures(&text){if text.len()<100{dates.push(common::date(&c[1],&c[2],&canonical));}}}
 let mut notes=vec![];let mut mappings=vec![];
 for (selector,role) in [(".footnotes__wrapper","footnote"),(".references__wrapper","reference")]{
  for li in article.select(&sel(&format!("{selector} li.footnote"))){
   let a=li.select(&sel("a.footnote-label[id]")).next().context("MSF note label/anchor missing")?;let label=text_of(&a);
   let text=li.select(&sel(".footnotes__item-text")).next().context("MSF note text missing")?;let id=format!("msf-{role}-{}",common::slug(&label));
   let old=a.value().id().unwrap().to_string();let back=a.value().attr("href").map(|s|s.trim_start_matches('#').to_string());
   mappings.push((old.clone(),id.clone()));if let Some(ref old_back)=back{mappings.push((old_back.clone(),format!("{id}-ref")));}
   let links=text.select(&sel("a[href]")).filter_map(|a|Some(json!({"label":text_of(&a),"url":target(base,a.value().attr("href")?)?,"raw_href":a.value().attr("href")}))).collect::<Vec<_>>();
   notes.push(json!({"role":role,"label":label,"stable_id":id,"source_id":old,"source_backlink_id":back,"text":text_of(&text),"source_html":li.html(),"links":links}));
   selected.push_str(&format!("<aside id=\"{}\"><strong>{} {}</strong> {}</aside>",esc(&id),if role=="footnote"{"Footnote"}else{"Reference"},esc(&label),text.inner_html()));
  }
 }
 // Every citation occurrence has its own generated locator, not just the
 // backlink selected by the final note widget. Retain originals in metadata.
 let mut citations=vec![];let mut occurrences=std::collections::HashMap::<String,usize>::new();
 for a in body.select(&sel("a.footnote__citation[id][href]")){
  let old=a.value().id().unwrap();let href=a.value().attr("href").unwrap();
  if let Some((_,stable))=mappings.iter().find(|(id,_)|Some(id.as_str())==href.strip_prefix('#')){
   let stable=stable.clone();let n=occurrences.entry(stable.clone()).or_default();*n+=1;
   let new=format!("{stable}-citation-{n}");citations.push((old.to_string(),new));
  }
 }
 mappings.extend(citations);
 // Source-generated note IDs are provenance, not clinical revision identifiers.
 mappings.sort_by(|a,b|b.0.len().cmp(&a.0.len()));for (old,new) in &mappings{selected=selected.replace(old,new);}
 let links=clinical.root_element().select(&sel("a[href]")).filter_map(|a|Some(json!({"label":text_of(&a),"raw_href":a.value().attr("href"),"url":target(base,a.value().attr("href")?)?}))).collect::<Vec<_>>();
 let comments=regex::Regex::new(r"(?s)<!--.*?-->").unwrap();selected=comments.replace_all(&selected,"").into_owned();
 let editor=regex::Regex::new(r#"\s(?:data-list-item-id|data-pdffiller-[\w-]+)="[^"]*""#).unwrap();selected=editor.replace_all(&selected,"").into_owned();
 let rights_note=doc.select(&sel("footer .copyright, .footer .copyright, .copyright")).next().map(|e|text_of(&e));
 let terms=doc.select(&sel("footer a[href], .footer a[href]")).find(|a|text_of(a)=="Disclaimer").and_then(|a|common::link(base,a.value().attr("href")?));
 // Words of body text outside the source's child-page list: an index page with none is navigation only.
 let mut own=body.html();for list in body.select(&sel(".childpages-macro")){own=own.replacen(&list.html(),"",1);}
 let own_words=text_of(&Html::parse_fragment(&own).root_element()).split_whitespace().count();
 let up=doc.select(&sel("head link[rel='up'][href]")).next().and_then(|e|common::link(base,e.value().attr("href")?)).filter(|u|Url::parse(u).ok().is_some_and(|p|allowed(&p,base)));
 Ok(Page{title:title.clone(),html:format!("<div id='msf-clinical'><h1>{}</h1>{selected}</div>",esc(&title)),canonical,dates,notes,tree,links,note_ids:mappings,is_toc:body.select(&sel(".childpages-macro")).next().is_some(),own_words,up,rights_note,terms,language:common::page_language(&doc)})
}
fn tables(html:&str,blocks:&mut [Block])->Result<()>{
 let doc=Html::parse_fragment(html);let positions=blocks.iter().enumerate().filter(|(_,b)|matches!(b.kind,BlockKind::Table{..})).map(|(i,_)|i).collect::<Vec<_>>();
 let source=doc.select(&sel("#msf-clinical table")).collect::<Vec<_>>();if source.len()!=positions.len(){bail!("MSF source/extracted table count mismatch");}
 for (i,table) in source.into_iter().enumerate(){
  let span=|c:ElementRef<'_>,a:&str|->Result<usize>{let n=c.value().attr(a).map(str::parse::<usize>).transpose()?.unwrap_or(1);if n==0||n>200{bail!("MSF invalid table span");}Ok(n)};
  let rows=table.select(&sel("tr")).collect::<Vec<_>>();let mut width=0;for row in &rows{let mut n=0;for c in common::cells(*row){n+=span(c,"colspan")?;}width=width.max(n);}if width==0||width>64{bail!("MSF table width outside validated bound");}
  let mut carry:Vec<Option<(String,usize)>>=vec![None;width];let mut grid=vec![];
  for row in rows{let mut cells:Vec<Option<String>>=vec![None;width];for j in 0..width{if let Some((t,n))=carry[j].clone(){cells[j]=Some(t.clone());carry[j]=if n>1{Some((t,n-1))}else{None};}}
   let mut col=0;for c in common::cells(row){while col<width&&cells[col].is_some(){col+=1;}let cs=span(c,"colspan")?;let rs=span(c,"rowspan")?;if col+cs>width{bail!("MSF carried table cells exceed declared width");}let t=text_of(&c).replace('|',"\\|");for j in col..col+cs{if cells[j].is_some(){bail!("MSF table spans overlap");}cells[j]=Some(t.clone());if rs>1{carry[j]=Some((t.clone(),rs-1));}}col+=cs;}
   grid.push(cells.into_iter().map(|v|v.unwrap_or_default()).collect::<Vec<_>>());
  }if carry.iter().any(Option::is_some){bail!("MSF rowspan exceeds observed rows");}
  let mut lines=vec![];for (r,cells) in grid.into_iter().enumerate(){lines.push(format!("| {} |",cells.join(" | ")));if r==0{lines.push(format!("| {} |",vec!["---";width].join(" | ")));}}blocks[positions[i]].text=lines.join("\n");
 }Ok(())
}
fn revision(p:&Page)->Result<String>{
 let notes=p.notes.iter().map(|n|json!({"role":n["role"],"label":n["label"],"text":n["text"],"links":n["links"]})).collect::<Vec<_>>();
 let nodes=p.tree.iter().flat_map(|g|g["nodes"].as_array().into_iter().flatten()).cloned().collect::<Vec<_>>();
 let mut payload=serde_json::to_string(&(&p.title,&p.html,&p.dates,&notes,&nodes,&p.links))?;
 for (old,new) in &p.note_ids{payload=payload.replace(old,new);}
 Ok(crate::common::sha256_hex(payload.as_bytes()))
}
struct Pending{url:String,title:String,book:Value,root:bool}
#[async_trait::async_trait]
impl Adapter for MsfMedical{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  // Depth-first over each book's source-emitted TOC graph, so `--limit` samples real chapters in
  // book order. Book roots and child-page indexes without their own text are traversed but not
  // emitted: they are recorded as `book` metadata and `part_of` targets of their chapters.
  if limit==Some(0){return Ok(vec![]);}let catalog=src.catalog_url.as_deref().context("MSF catalog missing")?;let fetched=f.get_ok(catalog).await?;let base=Url::parse(&fetched.record.final_url)?;
  let mut stack=books(&fetched.text(),&base)?.into_iter().rev().map(|(url,title,book)|Pending{url,title,book,root:true}).collect::<Vec<_>>();
  let mut seen=HashSet::new();let mut resolved=HashSet::new();let mut out=vec![];
  let mut titles:HashMap<String,String>=HashMap::new();let mut parents:HashMap<String,Option<String>>=HashMap::new();
  while let Some(mut p)=stack.pop(){if !seen.insert(p.url.clone()){continue;}if seen.len()>5000{bail!("MSF source-emitted book graph exceeds5000nodes; inventory incomplete");}
   let work=common::slug(Url::parse(&p.url)?.path());let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:p.url.clone(),title:Some(p.title.clone()),rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"book":p.book,"book_root":false})};
   let result=async{let item=f.get_ok(&row.url).await?;let base=Url::parse(&item.record.final_url)?;
    if p.root{
     let doc=Html::parse_document(&item.text());
     let paragraphs=doc.select(&sel("#block-msfmg-content > article .field--name-body p")).map(|e|text_of(&e)).filter(|t|!t.is_empty()).collect::<Vec<_>>();
     let notices=paragraphs.iter().filter(|t|t.contains("All rights reserved")||t.contains("PDF below has not been updated")).cloned().collect::<Vec<_>>();
     let isbn=common::first_text(&doc,"#block-msfmg-content .field--name-field-isbn .field__item");
     p.book["source_book_record"]=json!({"url":base.as_str(),"notices":notices,"isbn":isbn,"paragraphs":paragraphs});
     row.extra["book"]=p.book.clone();
    }
    let children=toc(&item.text(),&base);let body=page(&item.text(),&base)?;let hash=revision(&body)?;Ok::<_,anyhow::Error>((body,hash,children))}.await;
   match result{
    Ok((body,hash,children))=>{
     if p.root{if let Some(r)=p.book["source_book_record"].as_object_mut(){r.insert("canonical".into(),json!(body.canonical));r.insert("title".into(),json!(body.title));}}
     // Children first in source order (stack: push reversed), carrying the book record.
     for (url,title) in children.into_iter().rev(){if !seen.contains(&url){stack.push(Pending{url,title,book:p.book.clone(),root:false});}}
     seen.insert(body.canonical.clone());titles.insert(body.canonical.clone(),body.title.clone());parents.insert(body.canonical.clone(),body.up.clone());
     if p.root{continue;}
     if body.is_toc&&body.own_words==0{continue;}
     if !resolved.insert(body.canonical.clone()){continue;}
     let work=common::slug(Url::parse(&body.canonical)?.path());
     let mut chain=vec![];let mut cur=body.up.clone();while let Some(u)=cur{if chain.iter().any(|c:&Value|c["url"]==u.as_str())||chain.len()>20{break;}chain.push(json!({"url":u,"title":titles.get(&u)}));cur=parents.get(&u).cloned().flatten();}
     row.url=body.canonical;row.title=Some(body.title);row.doc_key=format!("{work}-{}",&hash[..12]);row.extra["work_key"]=json!(work);row.extra["clinical_sha256"]=json!(hash);
     row.extra["parent_url"]=json!(chain.first().map(|c|c["url"].clone()));row.extra["ancestors"]=json!(chain);row.extra["is_toc"]=json!(body.is_toc);
    },
    Err(e)=>{row.extra["discovery_error"]=json!(format!("{e:#}"));row.extra["book_root"]=json!(p.root);}
   }
   out.push(row);if limit.is_some_and(|n|out.len()>=n){return Ok(out);}
  }if out.is_empty(){bail!("MSF homepage yielded no book chapters");}Ok(out)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(error)=row.extra["discovery_error"].as_str(){bail!("MSF discovery: {error}");}let fetched=f.get_ok(&row.url).await?;let base=Url::parse(&fetched.record.final_url)?;let p=page(&fetched.text(),&base)?;
  if row.extra["clinical_sha256"].as_str().is_some_and(|s|revision(&p).map(|v|v!=s).unwrap_or(true)){bail!("MSF chapter revision changed after planning; re-plan");}
  let (mut blocks,mut warnings)=blocks_from_html(&p.html,Some(&base),Some("#msf-clinical"));tables(&p.html,&mut blocks)?;
  let mut rights=common::unknown_rights();rights.note=p.rights_note;rights.terms_url=p.terms;if rights.note.as_deref().is_some_and(|s|s.contains("Médecins Sans Frontières")){rights.rights_holders.push("Médecins Sans Frontières".into());}rights.evidence_urls.push(base.to_string());
  let mut fetches=vec![fetched.record];let mut terms_metadata=Value::Null;
  if let Some(terms)=rights.terms_url.clone(){match f.get_ok(&terms).await{
   Ok(response)=>{let url=Url::parse(&response.record.final_url)?;let doc=Html::parse_document(&response.text());
    if let Some(body)=doc.select(&sel(BODY)).next(){let text=text_of(&body);
     if text.contains("MSF")&&text.contains("prior written express permission"){
      rights.status="restricted".into();rights.restrictions.push(text.clone());rights.evidence_urls.push(url.to_string());terms_metadata=json!({"url":url.as_str(),"text":text});
     }else{warnings.push("Linked MSF Disclaimer did not match validated translation/application terms; no reuse permission inferred.".into());}
    }else{warnings.push("Linked MSF Disclaimer body missing; no reuse permission inferred.".into());}fetches.push(response.record);
   },Err(error)=>warnings.push(format!("Linked MSF Disclaimer unavailable: {error:#}; no reuse permission inferred."))
  }}
  if let Some(notices)=row.extra["book"]["source_book_record"]["notices"].as_array(){for notice in notices.iter().filter_map(Value::as_str){if notice.contains("All rights reserved"){rights.status="restricted".into();rights.restrictions.push(notice.to_string());}}}
  let attachments=row.extra["book"]["renditions"].as_array().cloned().unwrap_or_default();
  // Chapter hierarchy: the page's own rel=up link, plus the book root when it is a further ancestor.
  let mut relations=vec![];let parent=p.up.clone().or_else(||row.extra["parent_url"].as_str().map(String::from));
  if let Some(up)=&parent{relations.push(Relation{rel:"part_of".into(),target:up.clone(),scope:Some("source book hierarchy (the page's rel=up link)".into()),evidence_url:Some(p.canonical.clone())});}
  if let Some(book)=row.extra["book"]["source_book_record"]["canonical"].as_str().or(row.extra["book"]["source_book_record"]["url"].as_str()){if parent.as_deref()!=Some(book)&&book!=p.canonical{relations.push(Relation{rel:"part_of".into(),target:book.into(),scope:Some(format!("book: {}",row.extra["book"]["book_title"].as_str().unwrap_or("source book"))),evidence_url:Some(p.canonical.clone())});}}
  warnings.push("English books are traversed depth-first over source-emitted TOC links with a finite guard; each content page is one document with part_of relations to its parent page and book. Book roots and child-page indexes without their own text are navigation, recorded as relations and book metadata, not documents. Translations, separate PDF renditions and complete catalog reconciliation remain unverified. The homepage's PDF currency notice is retained independently; no PDF currency or blanket reuse permission is inferred. Clinical imperative lists and doses are preserved without invented recommendation strengths.".into());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:p.title,url:p.canonical,item_type:Some(if row.extra["book_root"]==true{"guideline_book_record"}else if p.is_toc{"guideline_chapter_index"}else{"guideline_chapter"}.into()),status:"unknown".into(),published:p.dates.iter().find(|d|d.label=="Published").and_then(|d|d.normalized.clone()),updated:p.dates.iter().find(|d|matches!(d.label.as_str(),"Content updated"|"Last updated")).and_then(|d|d.normalized.clone()),date_observations:p.dates,language:p.language,rights,blocks,attachments,relations,search_metadata:json!({"book":row.extra["book"],"ancestors":row.extra["ancestors"],"notes":p.notes,"clinical_lists":p.tree,"clinical_links":p.links,"disclaimer":terms_metadata}),fetches,warnings,..Default::default()})
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests{
 use super::*;
 const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/msf_medical/catalog.html"));
 const B:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/msf_medical/book.html"));
 const T:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/msf_medical/chapter-1.html"));
 const F:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/msf_medical/fever.html"));
 const D:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/msf_medical/disclaimer.html"));
 const HOME:&str="https://medicalguidelines.msf.org/";
 const BOOK:&str="https://medicalguidelines.msf.org/en/viewport/CG/english/clinical-guidelines-16686604.html";
 const TOC:&str="https://medicalguidelines.msf.org/en/viewport/CG/english/chapter-1-a-few-symptoms-and-syndromes-16689158.html";
 const FEVER:&str="https://medicalguidelines.msf.org/en/viewport/CG/english/fever-16689066.html";
 const MALARIA:&str="https://medicalguidelines.msf.org/en/viewport/CG/english/malaria-16689758.html";
 fn malaria()->String{[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/msf_medical/malaria.part-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/msf_medical/malaria.part-2.html"))].concat()}
 fn source()->SourceEntry{serde_json::from_value(json!({"id":"msf_medical","access":"public","enabled":true,"catalog_url":HOME})).unwrap()}
 #[tokio::test]
 async fn genuine_book_to_chapter_graph_and_alias_identity(){
  let books=books(C,&Url::parse(HOME).unwrap()).unwrap();assert_eq!(books.len(),7);
  let clinical=books.iter().find(|b|b.0=="https://medicalguidelines.msf.org/en/node/170").unwrap();
  assert!(clinical.2["renditions"].as_array().unwrap().iter().any(|r|r["url"]=="https://medicalguidelines.msf.org/sites/default/files/2026-05/guideline-170-en.pdf"));
  assert!(clinical.2["pdf_currency_notice"].as_str().unwrap().contains("has not been updated"));
  assert!(toc(B,&Url::parse(BOOK).unwrap()).iter().any(|r|r.0==TOC));
  let next=toc(T,&Url::parse(TOC).unwrap());assert!(next.iter().any(|r|r.0==FEVER));
  assert!(next.iter().any(|r|r.0=="https://medicalguidelines.msf.org/en/node/2332"));
  assert!(next.iter().all(|r|!r.0.contains("/flag/")&&!r.0.contains('?')));
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,1<<20,"").unwrap();
  for (u,h) in [(HOME,C),("https://medicalguidelines.msf.org/en/node/170",B),(BOOK,B),(TOC,T),(FEVER,F),("https://medicalguidelines.msf.org/en/node/1338",D)]{f.seed_cache_for_tests(u,h.as_bytes(),"text/html");}
  let rows=MsfMedical.discover(&source(),&f,None).await.unwrap();
  assert_eq!(rows.iter().filter(|r|r.url==FEVER).count(),1);
  let fever=rows.iter().find(|r|r.url==FEVER).unwrap();assert!(fever.extra["discovery_error"].is_null());
  assert!(rows.iter().any(|r|r.extra["discovery_error"].as_str().is_some_and(|s|s.contains("offline and not cached"))));
  let d=MsfMedical.extract(&source(),&f,fever).await.unwrap();
  assert_eq!(d.item_type.as_deref(),Some("guideline_chapter"));assert_eq!(d.updated.as_deref(),Some("2023-12"));assert!(d.published.is_none()&&d.country.is_none());
  assert_eq!(d.status,"unknown");assert_eq!(d.rights.status,"restricted");
  assert!(d.rights.restrictions.iter().any(|s|s.contains("prior written express permission")));
  assert!(d.rights.restrictions.iter().any(|s|s.contains("All rights reserved")));
  assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})&&!b.text.contains("new MSF Medical Guidelines app")&&!b.text.contains("On this page")));
  assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);
  assert!(d.blocks.iter().any(|b|b.text.contains("Joint pain | Rheumatic fever/chikungunya/dengue")));
  assert_eq!(d.search_metadata["notes"].as_array().unwrap().len(),2);
  assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Footnote)&&b.block_id=="msf-footnote-b"&&b.text.contains("unconjugate or anxious gaze")));
  // Book root and the text-free chapter index are navigation: no documents, but part_of targets.
  assert!(rows.iter().all(|r|r.url!=TOC&&r.url!=BOOK&&r.extra["book_root"]!=true||r.extra["discovery_error"].is_string()));
  assert_eq!(fever.extra["parent_url"],TOC);assert_eq!(fever.extra["ancestors"][1]["url"],BOOK);
  assert_eq!(fever.extra["book"]["source_book_record"]["canonical"],BOOK);
  assert!(fever.extra["book"]["source_book_record"]["isbn"].as_str().is_some_and(|s|s.contains("978-2-37585-284-2")));
  assert!(d.relations.iter().any(|r|r.rel=="part_of"&&r.target==TOC));
  assert!(d.relations.iter().any(|r|r.rel=="part_of"&&r.target==BOOK&&r.scope.as_deref().is_some_and(|s|s.starts_with("book: Clinical guidelines"))));
  assert_eq!(d.search_metadata["ancestors"][0]["title"],"Chapter 1: A few symptoms and syndromes");
  // Depth-first: the first emitted rows of a limited probe are pages of the first book, in TOC order,
  // never the seven book roots.
  let fever_at=rows.iter().position(|r|r.url==FEVER).unwrap();
  let onc_root=rows.iter().position(|r|r.extra["book_root"]==true&&r.title.as_deref()==Some("Essential obstetric and newborn care")).unwrap();
  assert!(fever_at<onc_root);
  let first=MsfMedical.discover(&source(),&f,Some(3)).await.unwrap();assert_eq!(first.len(),3);
  assert!(first[1].url.contains("/en/viewport/CG/english/")&&first[1].extra["book_root"]!=true);
 }
 #[tokio::test]
 async fn genuine_malaria_tables_nested_conditions_notes_and_scoped_revision(){
  let raw=malaria();assert_eq!(raw.len(),202560);assert_eq!(crate::common::sha256_hex(raw.as_bytes()),"676054fd60f0d16817e837effcd45b3f59a24451b66d2a58b003e47add054215");
  let url=Url::parse(MALARIA).unwrap();let p=page(&raw,&url).unwrap();assert_eq!(p.notes.iter().filter(|n|n["role"]=="footnote").count(),12);assert_eq!(p.notes.iter().filter(|n|n["role"]=="reference").count(),3);
  assert!(!p.html.contains("see-footnote__popup"));
  assert!(p.links.iter().any(|l|l["url"].as_str().is_some_and(|s|s.contains("#footnote"))));
  let hash=revision(&p).unwrap();
  let regenerated=raw.replace("Mho81kRd3w26pQSAByx38JiCymas4ekM0K8RJMFcA4_dhkkBTU2mKhB","regenerated-note-locator");assert_ne!(regenerated,raw);assert_eq!(hash,revision(&page(&regenerated,&url).unwrap()).unwrap());
  let editor=raw.replace("data-list-item-id=", r#"data-pdffiller-tried-at="999" data-list-item-id="#);assert_eq!(hash,revision(&page(&editor,&url).unwrap()).unwrap());
  assert_ne!(hash,revision(&page(&raw.replace("complete 7 days of treatment","complete 8 days of treatment"),&url).unwrap()).unwrap());
  assert!(page(&raw.replace("field--name-body","missing-clinical-body"),&url).is_err());
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,1<<20,"").unwrap();f.seed_cache_for_tests(MALARIA,raw.as_bytes(),"text/html");f.seed_cache_for_tests("https://medicalguidelines.msf.org/en/node/1338",D.as_bytes(),"text/html");
  let row=PlanRow{source_id:"msf_medical".into(),doc_key:"malaria-fixture".into(),url:MALARIA.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({"clinical_sha256":hash})};
  let d=MsfMedical.extract(&source(),&f,&row).await.unwrap();
  assert_eq!(d.updated.as_deref(),Some("2026-06"));assert_eq!(d.published.as_deref(),Some("2026-11"));assert_eq!(d.date_observations.len(),2);
  let tables=d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).collect::<Vec<_>>();assert_eq!(tables.len(),3);
  assert!(tables[0].text.contains("| ≥ 35 kg | 4 tabs of 20/120 mg OR 1 tab of 80/480 mg | 2 times daily for 3 days |"));
  assert!(tables[2].text.contains("| ≥ 80 kg | 2.5 tabs of 80/640 mg | once daily for 3 days |"));
  assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::ListItem)&&b.text.contains("Alternative for children under 6 years if artesunate IV/IM is unavailable")&&b.text.contains("Children 2 months to < 3 years")&&b.text.contains("1 rectal capsule (100 mg)")));
  assert!(d.blocks.iter().any(|b|b.block_id=="msf-footnote-l"&&b.text.contains("switching to oral amoxicillin/clavulanic acid")));
  assert!(d.blocks.iter().any(|b|b.block_id=="msf-reference-3"&&b.text.contains("10.1186/s12936-016-1529-6")));
  assert!(d.search_metadata["notes"].as_array().unwrap().iter().any(|n|n["role"]=="reference"&&n["links"].as_array().unwrap().iter().any(|l|l["url"]=="https://www.who.int/publications/i/item/guidelines-for-malaria")));
  assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..}|BlockKind::Figure{..})));
  f.seed_cache_for_tests(MALARIA,raw.replace("complete 7 days of treatment","complete 8 days of treatment").as_bytes(),"text/html");assert!(MsfMedical.extract(&source(),&f,&row).await.is_err());
 }
}
