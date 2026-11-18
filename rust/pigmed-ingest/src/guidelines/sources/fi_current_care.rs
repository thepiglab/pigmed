//! Current Care: static source catalog data -> separate professional full HTML.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct FiCurrentCare;
const ROOT:&str="#suositus article.article .duo-body.db-hoi";
fn esc(s:&str)->String{s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;")}
fn target(base:&Url,href:&str)->Option<String>{let u=base.join(href.trim()).ok()?;(matches!(u.scheme(),"http"|"https")&&u.username().is_empty()&&u.password().is_none()).then(||u.to_string())}
// Decode a literal source data envelope, never execute its surrounding JavaScript.
fn data64(raw:&str)->Result<Vec<u8>>{
 let raw=raw.as_bytes();if raw.len()%4!=0{bail!("Current Care catalog base64 length invalid");}let mut out=Vec::new();
 let value=|c:u8|->Result<u32>{Ok(match c{b'A'..=b'Z'=>(c-b'A')as u32,b'a'..=b'z'=>(c-b'a'+26)as u32,b'0'..=b'9'=>(c-b'0'+52)as u32,b'+'=>62,b'/'=>63,_=>bail!("Current Care catalog base64 alphabet invalid")})};
 for (i,q) in raw.chunks_exact(4).enumerate(){let last=(i+1)*4==raw.len();let a=value(q[0])?;let b=value(q[1])?;
  if q[2]==b'=' {if !last||q[3]!=b'='||b&15!=0{bail!("Current Care catalog base64 padding invalid");}out.push(((a<<2)|(b>>4))as u8);continue;}
  let c=value(q[2])?;out.push(((a<<2)|(b>>4))as u8);out.push(((b<<4)|(c>>2))as u8);
  if q[3]==b'=' {if !last||c&3!=0{bail!("Current Care catalog base64 padding invalid");}}else{let d=value(q[3])?;out.push(((c<<6)|d)as u8);}
 }Ok(out)
}
fn catalog(raw:&str)->Result<Vec<Value>>{
 let re=regex::Regex::new(r"var\s+khdata\s*=\s*JSON\.parse\(atob\('([A-Za-z0-9+/=]+)'\)\)").unwrap();
 let matches=re.captures_iter(raw).collect::<Vec<_>>();if matches.len()!=1{bail!("Current Care expected one static khdata catalog");}
 let value:Value=serde_json::from_slice(&data64(&matches[0][1])?)?;let items=value["items"].as_array().context("Current Care items missing")?;
 let id=regex::Regex::new(r"^hoi\d+$").unwrap();let mut seen=HashSet::new();let mut out=vec![];
 for item in items{let key=item["nimi"].as_str().context("Current Care item name missing")?;if !id.is_match(key){bail!("Current Care unvalidated professional catalog identifier {key}");}
  if item["otsikko"].as_str().is_none(){bail!("Current Care item title missing");}if seen.insert(key.to_string()){out.push(item.clone());}
 }if out.is_empty(){bail!("Current Care static catalog empty");}Ok(out)
}
fn fi_date(label:&str,raw:&str,url:&str)->DateObservation{
 let mut d=common::date(label,raw,url);let re=regex::Regex::new(r"^(\d{1,2})\.(\d{1,2})\.(\d{4})$").unwrap();
 if let Some(c)=re.captures(raw.trim()){let day=c[1].parse::<u32>().unwrap();let month=c[2].parse::<u32>().unwrap();if (1..=12).contains(&month)&&(1..=31).contains(&day){let iso=format!("{}-{month:02}-{day:02}",&c[3]);d.normalized=common::date(label,&iso,url).normalized;}}d
}
fn contextual_lists(root:ElementRef<'_>,base:&Url,selected:&mut String)->Result<Vec<Value>>{
 fn render(list:ElementRef<'_>,base:&Url,key:&str,parent:Option<&str>,parents:&[String],nodes:&mut Vec<Value>)->Result<String>{
  let mut html=list.html();
  for (i,li) in list.children().filter_map(ElementRef::wrap).filter(|e|e.value().name()=="li").enumerate(){let key=format!("{key}/{}",i+1);
   let children=li.select(&sel("ul,ol")).filter(|e|e.ancestors().filter_map(ElementRef::wrap).find(|p|p.value().name()=="li").is_some_and(|p|p.id()==li.id())).collect::<Vec<_>>();
   let mut own=li.inner_html();for child in &children{own=own.replacen(&child.html(),"",1);}
   let own_doc=Html::parse_fragment(&own);for e in own_doc.select(&sel("table,.duo-figure,figure")){own=own.replacen(&e.html(),"",1);}
   let fragment=Html::parse_fragment(&own);let text=text_of(&fragment.root_element());
   let links=fragment.select(&sel("a[href]")).map(|a|json!({"label":text_of(&a),"raw_href":a.value().attr("href"),"url":a.value().attr("href").and_then(|h|target(base,h))})).collect::<Vec<_>>();
   nodes.push(json!({"key":key,"parent":parent,"position":i+1,"list_kind":list.value().name(),"list_id":list.value().id(),"source_id":li.value().id(),"text":text,"parents":parents,"links":links}));
   let mut context=parents.to_vec();if !text.is_empty(){context.push(text);}
   let mut transformed=li.inner_html();for (j,child) in children.into_iter().enumerate(){let rendered=render(child,base,&format!("{key}/list{}",j+1),Some(&key),&context,nodes)?;transformed=transformed.replacen(&child.html(),&rendered,1);}
   // Prefix each child in its original source position. Preserve paragraphs,
   // media, tables and child-list order instead of flattening their markup.
   let prefix=if parents.is_empty(){String::new()}else{format!("<span class='fi-source-parent-context'>{} </span>",esc(&parents.join("\n")))};
   html=html.replacen(&li.html(),&format!("<li id=\"{}\">{prefix}{transformed}</li>",esc(&key)),1);
  }Ok(html)
 }
 let mut groups=vec![];for list in root.select(&sel("ul,ol")){
  if list.ancestors().filter_map(ElementRef::wrap).any(|e|e.id()!=list.id()&&matches!(e.value().name(),"ul"|"ol"|"table"|"nav")){continue;}
  if !list.select(&sel("ul,ol")).any(|e|e.id()!=list.id()){continue;}
  let key=format!("fi-list{}",groups.len()+1);let mut nodes=vec![];let rendered=render(list,base,&key,None,&[],&mut nodes)?;
  groups.push(json!({"key":key,"source_id":list.value().id(),"source_html":list.html(),"nodes":nodes}));*selected=selected.replacen(&list.html(),&rendered,1);
 }Ok(groups)
}
struct Page{key:String,title:String,html:String,dates:Vec<DateObservation>,native_state:Option<String>,authors:Option<String>,copyright:Option<String>,terms:Option<String>,history:Option<String>,assets:Vec<Value>,metadata:Value,language:Option<String>}
fn page(raw:&str,base:&Url)->Result<Page>{
 let doc=Html::parse_document(raw);if doc.select(&sel("section.error-404 h1")).any(|e|text_of(&e).to_lowercase().contains("virhe 404")){bail!("Current Care source returned a 404 error page with HTTP 200; the requested professional document is unavailable, no replacement inferred");}let roots=doc.select(&sel(ROOT)).collect::<Vec<_>>();if roots.len()!=1{bail!("Current Care expected one professional full recommendation body, found {}",roots.len());}let root=roots[0];
 let key=root.select(&sel(".duo-article-footer .duo-identifier")).next().map(|e|text_of(&e)).context("Current Care article identifier missing")?;
 if !regex::Regex::new(r"^hoi\d+$").unwrap().is_match(&key)||!root.value().classes().any(|c|c==format!("db-{key}")){bail!("Current Care professional body identity inconsistent");}
 let title=root.children().filter_map(ElementRef::wrap).find(|e|e.value().name()=="h1").map(|e|text_of(&e)).context("Current Care clinical title missing")?;
 let ui=doc.select(&sel(".kh-articlemeta")).next().context("Current Care source publication/status header missing")?;
 // UI calls this Julkaistu; .duo-updated is retained independently, never
 // re-labelled as a clinical update merely because of its CSS class name.
 let date=regex::Regex::new(r"Julkaistu:\s*(\d{1,2}\.\d{1,2}\.\d{4})").unwrap();let ui_text=text_of(&ui);let published=date.captures(&ui_text).context("Current Care Julkaistu date missing")?;
 let dates=vec![fi_date("Julkaistu",&published[1],base.as_str())];
 let status=ui.select(&sel("a[href]")).find(|a|a.value().attr("href").is_some_and(|u|u.starts_with("/nix")));let native_state=status.map(|e|text_of(&e));let history=status.and_then(|e|common::link(base,e.value().attr("href")?));
 let machine=root.select(&sel(".duo-article-header .duo-updated")).next().map(|e|json!({"source_class":"duo-updated","raw":text_of(&e),"datetime":e.value().attr("datetime"),"source_url":base.as_str()}));
 let authors=root.select(&sel(".duo-article-header .duo-authors")).next().map(|e|text_of(&e));
 let copyright=root.select(&sel(".duo-article-footer .duo-copyrights")).next().map(|e|text_of(&e));
 let terms=doc.select(&sel("a[href]")).find(|a|text_of(a).to_lowercase()=="käyttöoikeudet").and_then(|e|common::link(base,e.value().attr("href")?));
 let mut html=root.html();let groups=contextual_lists(root,base,&mut html)?;let mut assets=vec![];
 for e in root.select(&sel("a[href]")){let Some(raw)=e.value().attr("href")else{continue};let Some(url)=target(base,raw)else{continue};let u=Url::parse(&url)?;
  let role=if u.path().starts_with("/khp"){Some("patient_edition")}else if u.path().starts_with("/khr"){Some("swedish_patient_edition")}else if u.path().starts_with("/kht"){Some("summary_article")}else if u.path().starts_with("/nix"){Some("clinical_appendix")}else{None};
  if let Some(role)=role{assets.push(json!({"url":url,"role":role,"label":text_of(&e),"rendition_parity":"unverified"}));}
 }
 for e in doc.select(&sel("#otakayttoon .duo-body .duo-identifier")){if let Some(url)=common::link(base,&format!("/{}",text_of(&e))){assets.push(json!({"url":url,"role":"implementation_article","label":text_of(&e),"rendition_parity":"unverified"}));}}
 let mut figures=vec![];for (i,f) in root.select(&sel(".duo-figure")).enumerate(){
  let images=f.select(&sel("img")).collect::<Vec<_>>();let caption=f.select(&sel(".duo-figcaption")).next().map(|c|text_of(&c));let id=format!("fi-figure-{}",i+1);
  for image in &images{figures.push(json!({"source_figure_index":i+1,"block_locator":id,"raw_src":image.value().attr("src"),"url":image.value().attr("src").and_then(|s|target(base,s)),"alt":image.value().attr("alt"),"caption":caption,"acquisition":"not fetched; source /xmedia/ is robots-disallowed","rights":"independent image permission required"}));}
  let images=images.iter().map(|e|e.html()).collect::<String>();let cap=f.select(&sel(".duo-figcaption")).next().map(|e|e.inner_html()).unwrap_or_default();
  html=html.replacen(&f.html(),&format!("<figure id='{id}'>{images}<figcaption>{cap}</figcaption></figure>"),1);
 }
 let evidence=root.select(&sel("a.duo-evidence[href]")).map(|a|{
  let mut context=a.ancestors().filter_map(ElementRef::wrap).filter(|e|matches!(e.value().name(),"li"|"p"|"td"|"th")).map(|e|text_of(&e)).collect::<Vec<_>>();context.reverse();
  json!({"kind":"evidence_category_not_recommendation_strength","printed":a.select(&sel("var")).next().map(|e|text_of(&e)),"claim":a.value().attr("title"),"quoted_claim":a.select(&sel("span")).next().map(|e|text_of(&e)),"evidence_review_url":a.value().attr("href").and_then(|s|target(base,s)),"source_url":base.as_str(),"source_context":context})
 }).collect::<Vec<_>>();
 let mut legends=doc.select(&sel("#naytonastekatsaukset [data-toggle='tooltip'][title]")).filter_map(|e|e.value().attr("title")).map(String::from).collect::<HashSet<_>>().into_iter().collect::<Vec<_>>();legends.sort();
 if !evidence.is_empty()&&!legends.iter().any(|s|s.contains("A=Vahva tutkimusnäyttö")&&s.contains("D=Ei tutkimusnäyttöä")){bail!("Current Care evidence category legend missing");}
 let links=root.select(&sel("a[href]")).map(|a|json!({"label":text_of(&a),"raw_href":a.value().attr("href"),"url":a.value().attr("href").and_then(|s|target(base,s))})).collect::<Vec<_>>();
 let tables=root.select(&sel("table")).map(|t|json!({"source_id":t.value().id(),"caption":t.select(&sel("caption")).next().map(|e|text_of(&e)),"source_html":t.html(),"native_rows":t.select(&sel("tr")).map(|r|common::cells(r).iter().map(|c|json!({"text":text_of(c),"rowspan":c.value().attr("rowspan"),"colspan":c.value().attr("colspan"),"source_html":c.html()})).collect::<Vec<_>>()).collect::<Vec<_>>()})).collect::<Vec<_>>();
 let references=root.select(&sel(".duo-references li[id]")).map(|e|json!({"source_id":e.value().id(),"text":text_of(&e),"html":e.html()})).collect::<Vec<_>>();
 let editor=regex::Regex::new(r#"\s(?:data-list-item-id|data-pdffiller-[\w-]+)="[^"]*""#).unwrap();html=editor.replace_all(&html,"").into_owned();let comments=regex::Regex::new(r"(?s)<!--.*?-->").unwrap();html=comments.replace_all(&html,"").into_owned();
 Ok(Page{key,title,html:format!("<div id='fi-clinical'>{html}</div>"),dates,native_state,authors,copyright,terms,history,assets,language:common::page_language(&doc),metadata:json!({"machine_date_observation":machine,"evidence_categories":evidence,"evidence_category_legends":legends,"clinical_lists":groups,"clinical_links":links,"tables":tables,"references":references,"figures":figures,"scope":"main professional suositus only; patient, implementation, evidence review and gallery are separate roles"})})
}
fn revision(p:&Page)->Result<String>{
 let nodes=p.metadata["clinical_lists"].as_array().into_iter().flatten().flat_map(|g|g["nodes"].as_array().into_iter().flatten()).collect::<Vec<_>>();
 Ok(crate::common::sha256_hex(serde_json::to_string(&(&p.key,&p.title,&p.html,&p.dates,&p.native_state,&p.authors,&p.copyright,&p.terms,&nodes,&p.metadata["clinical_links"],&p.metadata["evidence_categories"],&p.metadata["evidence_category_legends"],&p.metadata["machine_date_observation"]))?.as_bytes()))
}
fn render_table(table:ElementRef<'_>)->Result<String>{
 let span=|c:ElementRef<'_>,a:&str|->Result<usize>{let n=c.value().attr(a).map(str::parse::<usize>).transpose()?.unwrap_or(1);if n==0||n>200{bail!("Current Care invalid table span");}Ok(n)};
 let rows=table.select(&sel("tr")).collect::<Vec<_>>();let mut width=0;for row in &rows{let mut n=0;for c in common::cells(*row){n+=span(c,"colspan")?;}width=width.max(n);}if width==0||width>64{bail!("Current Care table width outside validated bound");}
 let mut carry:Vec<Option<(String,usize)>>=vec![None;width];let mut grid=vec![];
 for row in rows{let mut cells:Vec<Option<String>>=vec![None;width];for j in 0..width{if let Some((t,n))=carry[j].clone(){cells[j]=Some(t.clone());carry[j]=if n>1{Some((t,n-1))}else{None};}}
  let mut col=0;for c in common::cells(row){while col<width&&cells[col].is_some(){col+=1;}let cs=span(c,"colspan")?;let rs=span(c,"rowspan")?;if col+cs>width{bail!("Current Care carried table cells exceed declared width");}let t=text_of(&c).replace('|',"\\|");for j in col..col+cs{if cells[j].is_some(){bail!("Current Care table spans overlap");}cells[j]=Some(t.clone());if rs>1{carry[j]=Some((t.clone(),rs-1));}}col+=cs;}
  grid.push(cells.into_iter().map(|v|v.unwrap_or_default()).collect::<Vec<_>>());
 }if carry.iter().any(Option::is_some){bail!("Current Care rowspan exceeds observed rows");}
 let mut lines=vec![];for (r,cells) in grid.into_iter().enumerate(){lines.push(format!("| {} |",cells.join(" | ")));if r==0{lines.push(format!("| {} |",vec!["---";width].join(" | ")));}}Ok(lines.join("\n"))
}
fn tables(html:&str,blocks:&mut [Block])->Result<Vec<String>>{
 let doc=Html::parse_fragment(html);let positions=blocks.iter().enumerate().filter(|(_,b)|matches!(b.kind,BlockKind::Table{..})).map(|(i,_)|i).collect::<Vec<_>>();let source=doc.select(&sel("#fi-clinical table")).collect::<Vec<_>>();if source.len()!=positions.len(){bail!("Current Care source/extracted table count mismatch");}let mut warnings=vec![];
 for (i,table) in source.into_iter().enumerate(){match render_table(table){Ok(text)=>blocks[positions[i]].text=text,Err(error)=>{let id=table.value().id().unwrap_or("unidentified");warnings.push(format!("Current Care source table {id} has ambiguous spans ({error:#}); source cells are retained in native row order without inferred column positions; original source HTML and span attributes are in metadata."));let mut lines=vec!["Source table: native row/cell order (column positions not inferred)".to_string()];for (r,row) in table.select(&sel("tr")).enumerate(){for (c,cell) in common::cells(row).into_iter().enumerate(){let rs=cell.value().attr("rowspan").unwrap_or("1");let cs=cell.value().attr("colspan").unwrap_or("1");lines.push(format!("row {} cell {} [rowspan={}, colspan={}]: {}",r+1,c+1,rs,cs,text_of(&cell)));}}blocks[positions[i]].text=lines.join("\n");}}}Ok(warnings)
}

fn rights_policy(raw:&str,url:&Url)->Result<Value>{
 let doc=Html::parse_document(raw);let article=doc.select(&sel("article.col-lg-8")).find(|a|a.select(&sel("h1.entry-title")).next().is_some_and(|h|text_of(&h)=="Käyttöoikeudet")).context("Current Care rights article missing")?;
 let body=article.select(&sel(".entry-content")).next().context("Current Care rights body missing")?;
 let text=text_of(&body);if !text.contains("Osittainenkin lainaaminen")||!text.contains("Kuvien käyttöoikeuksia"){bail!("Current Care validated reuse/image notice missing");}
 let observed=article.select(&sel(".kh-updated")).next().map(|e|text_of(&e));
 let date=observed.as_ref().and_then(|t|regex::Regex::new(r"(\d{1,2}\.\d{1,2}\.\d{4})").unwrap().captures(t).map(|c|fi_date("Päivitetty",&c[1],url.as_str())));
 Ok(json!({"url":url.as_str(),"title":"Käyttöoikeudet","text":text,"html":body.html(),"date_observation":date,"scope":"reuse policy only; clinical and image-specific notices remain independent"}))
}
fn history(raw:&str,url:&Url,title:&str)->Result<Value>{
 let doc=Html::parse_document(raw);let root=doc.select(&sel(".duo-body.db-nix")).next().context("Current Care history body missing")?;
 let heading=root.select(&sel("h1")).next().map(|e|text_of(&e)).context("Current Care history title missing")?;
 if !heading.starts_with(title)||!heading.contains("historiatiedot"){bail!("Current Care linked history does not identify this work");}
 let mut sections=vec![];for section in root.select(&sel(".duo-section")){let Some(h)=section.select(&sel("h2")).next()else{continue};
  sections.push(json!({"label":text_of(&h),"source_html":section.html(),"text":text_of(&section)}));
 }
 let date=root.select(&sel(".duo-article-header .duo-updated")).next().map(|e|json!({"source_class":"duo-updated","raw":text_of(&e),"datetime":e.value().attr("datetime")}));
 Ok(json!({"url":url.as_str(),"title":heading,"article_date_observation":date,"sections":sections,"scope":"separate history article; corrections and historical revision dates do not replace the primary Julkaistu date"}))
}
/// G2-scale-b: the static catalog's own status explains why a professional page is a source 404.
/// `poistettu` = removed from publication (korvattu `k` names the replacing work in `historynix`,
/// `p` names the history article); a "new guideline in preparation" has no professional text yet.
/// The terminal is still requested; this only labels the refusal with the publisher's own status.
fn catalog_status_note(item:&Value)->Option<String>{
 let tila=item["tila"].as_str()?;let nix=item["historynix"].as_str();let selite=item["selite"].as_str().map(str::trim).filter(|s|!s.is_empty());
 if tila=="poistettu"{
  let mut n="Current Care catalog marks this guideline withdrawn (tila=poistettu".to_string();
  if let Some(s)=selite{n.push_str(&format!(", selite: {s}"));}
  match(item["korvattu"].as_str(),nix){(Some("k"),Some(x))=>n.push_str(&format!(", replaced by {x}")),(_,Some(x))=>n.push_str(&format!(", history article {x}")),_=>{}}
  n.push_str("); withdrawn professional text is not published, no replacement substituted");Some(n)
 }else if tila.starts_with("työn alla"){Some(format!("Current Care catalog lists this as work in progress (tila={tila}); no published professional text yet"))}else{None}
}
#[async_trait::async_trait]
impl Adapter for FiCurrentCare{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![]);}let catalog_url=src.catalog_url.as_deref().context("Current Care catalog missing")?;let response=f.get_ok(catalog_url).await?;let base=Url::parse(&response.record.final_url)?;
  let mut out=vec![];let mut resolved=HashSet::new();for item in catalog(&response.text())?{
   let key=item["nimi"].as_str().unwrap();
   // Exact source bundle's getLocalizedUrl(item): '/' + (Swedish ? 'sv/' : '') + item.nimi.
   // Finnish professional catalog uses its literal nimi; no API or node range guessed.
   let url=base.join(&format!("/{key}"))?.to_string();
   let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-unresolved"),url,title:item["otsikko"].as_str().map(String::from),rendition:Some("html".into()),version:None,extra:json!({"work_key":key,"catalog_item":item,"catalog_fetch":response.record,"route_evidence":{"url":"https://www.kaypahoito.fi/wp-content/themes/duodecim_kaypahoito/assets/dist/js/suositus.bundle.63219449e301fe940c71.js","sha256":"90adeeea2ac963cd826b82660dee69909c3929c74cbb8d9184c3be074865942a","function":"getLocalizedUrl","scope":"Finnish literal catalog nimi route; code inspected, never executed"}})};
   let result=async{let response=f.get_ok(&row.url).await?;let base=Url::parse(&response.record.final_url)?;let p=page(&response.text(),&base)?;
    if p.key!=row.extra["work_key"].as_str().unwrap(){bail!("Current Care catalog/terminal identity mismatch");}let hash=revision(&p)?;Ok::<_,anyhow::Error>((p,hash,base))}.await;
   match result{Ok((p,hash,base))=>{if !resolved.insert(p.key.clone()){continue;}row.url=base.to_string();row.title=Some(p.title);row.doc_key=format!("{}-{}",p.key,&hash[..12]);row.extra["clinical_sha256"]=json!(hash);},Err(error)=>{let note=catalog_status_note(&item);if note.is_some(){row.extra["catalog_status"]=json!({"tila":item["tila"],"korvattu":item["korvattu"],"historynix":item["historynix"],"uusisuositus":item["uusisuositus"],"selite":item["selite"]});}row.extra["discovery_error"]=json!(match note{Some(n)=>format!("{n}; source response: {error:#}"),None=>format!("{error:#}")});}}
   out.push(row);if limit.is_some_and(|n|out.len()>=n){break;}
  }Ok(out)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(error)=row.extra["discovery_error"].as_str(){bail!("Current Care discovery: {error}");}
  let response=f.get_ok(&row.url).await?;let base=Url::parse(&response.record.final_url)?;let mut p=page(&response.text(),&base)?;
  if row.extra["clinical_sha256"].as_str().is_some_and(|s|revision(&p).map(|v|v!=s).unwrap_or(true)){bail!("Current Care clinical revision changed after planning; re-plan");}
  let (mut blocks,mut warnings)=blocks_from_html(&p.html,Some(&base),Some("#fi-clinical"));warnings.extend(tables(&p.html,&mut blocks)?);
  let mut rights=common::unknown_rights();rights.note=p.copyright;rights.terms_url=p.terms;rights.evidence_urls.push(base.to_string());if rights.note.as_deref().is_some_and(|s|s.contains("Suomalainen Lääkäriseura Duodecim")){rights.rights_holders.push("Suomalainen Lääkäriseura Duodecim".into());}
  let mut fetches=vec![response.record];if let Ok(record)=serde_json::from_value(row.extra["catalog_fetch"].clone()){fetches.insert(0,record);}
  if let Some(url)=rights.terms_url.clone(){match f.get_ok(&url).await{
   Ok(response)=>{let url=Url::parse(&response.record.final_url)?;match rights_policy(&response.text(),&url){Ok(value)=>{rights.status="restricted".into();rights.restrictions.push(value["text"].as_str().unwrap().into());rights.evidence_urls.push(url.to_string());p.metadata["reuse_policy"]=value;},Err(error)=>warnings.push(format!("Current Care linked rights could not be mapped: {error:#}"))}fetches.push(response.record);},Err(error)=>warnings.push(format!("Current Care linked rights unavailable: {error:#}; no blanket reuse permission inferred."))
  }}
  if let Some(url)=p.history.clone(){p.assets.push(json!({"url":url,"role":"source_history_article","label":p.native_state,"rendition_parity":"not applicable"}));match f.get_ok(&url).await{
   Ok(response)=>{let url=Url::parse(&response.record.final_url)?;match history(&response.text(),&url,&p.title){Ok(value)=>p.metadata["history_article"]=value,Err(error)=>warnings.push(format!("Current Care linked status/history retained without validated historical mapping: {error:#}"))}fetches.push(response.record);},Err(error)=>warnings.push(format!("Current Care linked history not acquired: {error:#}"))
  }}
  warnings.push("Source A–D markers denote evidence categories, not recommendation strengths. All source grade/claim/review links remain separately associated. Only the professional suositus panel is extracted; patient, summary, implementation and evidence review articles are separate roles. Main inline figures retain exact URLs/captions; /xmedia/ assets are not fetched through the source robots disallow. Native status and separate history/correction facts are retained without inferring current version, expiry or rendition parity.".into());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:Some(p.key),title:p.title,url:base.to_string(),item_type:Some("clinical_practice_guideline".into()),status:"unknown".into(),published:p.dates[0].normalized.clone(),updated:None,date_observations:p.dates,language:p.language,issuers:p.authors.into_iter().collect(),rights,blocks,attachments:p.assets,search_metadata:json!({"catalog_item":row.extra["catalog_item"],"source_native_status":p.native_state,"source_metadata":p.metadata}),fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests{
 use super::*;
 fn catalog_html()->String{[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/catalog.part-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/catalog.part-2.html"))].concat()}
 fn asthma()->String{[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/asthma.part-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/asthma.part-2.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/asthma.part-3.html"))].concat()}
 fn hypertension()->String{[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/hypertension.part-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/hypertension.part-2.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/hypertension.part-3.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/hypertension.part-4.html"))].concat()}
 const RIGHTS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/rights.html"));
 const HISTORY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/asthma-history.html"));
 const SCRIPT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/catalog-script.js"));
 const ASTHMA:&str="https://www.kaypahoito.fi/hoi06030?tab=suositus";
 const HTN:&str="https://www.kaypahoito.fi/hoi04010";
 const TERMS:&str="https://www.kaypahoito.fi/kaypa-hoito/kayttooikeudet";
 fn src()->SourceEntry{serde_json::from_value(json!({"id":"fi_current_care","enabled":true,"access":"public","catalog_url":"https://www.kaypahoito.fi/suositukset"})).unwrap()}
 fn row(url:&str)->PlanRow{PlanRow{source_id:"fi_current_care".into(),doc_key:"genuine-fixture".into(),url:url.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({})}}
 #[tokio::test]
 async fn genuine_static_catalog_route_contract_and_finite_offline_discovery(){
  let raw=catalog_html();assert_eq!(crate::common::sha256_hex(raw.as_bytes()),"f394c966ed50b7edd6ffab61c0e850e1c4a2ecdbdfa4952d2bb99c44a66458d2");
  let items=catalog(&raw).unwrap();assert_eq!(items.len(),140);assert_eq!(items.iter().map(|i|i["nimi"].as_str().unwrap()).collect::<HashSet<_>>().len(),140);
  let asthma_item=items.iter().find(|i|i["nimi"]=="hoi06030").unwrap();assert_eq!(asthma_item["tila"],"voimassa");assert_eq!(asthma_item["sisaltopvm"],"2022-03-29T00:00:00");
  assert!(asthma_item["Liitteet"].as_array().unwrap().iter().any(|a|a["title"]=="Potilasversio"&&a["url"]=="khp00003"));
  assert_eq!(crate::common::sha256_hex(SCRIPT.as_bytes()),"90adeeea2ac963cd826b82660dee69909c3929c74cbb8d9184c3be074865942a");
  assert!(SCRIPT.contains("getLocalizedUrl:function(t){var e=window.location.pathname.startsWith(\"/sv\");return\"/\".concat(e?\"sv/\":\"\").concat(t.nimi)}"));
  assert!(catalog(&raw.replace("var khdata", "var unrelated_data")).is_err());assert!(data64("AA=A").is_err());assert!(data64("AB==").is_err());
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests("https://www.kaypahoito.fi/suositukset",raw.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.kaypahoito.fi/hoi06030",asthma().as_bytes(),"text/html");
  let rows=FiCurrentCare.discover(&src(),&f,None).await.unwrap();assert_eq!(rows.len(),140);assert_eq!(rows.iter().filter(|r|r.url=="https://www.kaypahoito.fi/hoi06030").count(),1);
  let actual=rows.iter().find(|r|r.url=="https://www.kaypahoito.fi/hoi06030").unwrap();assert!(actual.extra["discovery_error"].is_null());assert!(actual.doc_key.starts_with("hoi06030-"));
  assert!(rows.iter().filter(|r|r.url!="https://www.kaypahoito.fi/hoi06030").all(|r|r.extra["discovery_error"].as_str().is_some_and(|e|e.contains("offline and not cached"))));
  assert!(FiCurrentCare.discover(&src(),&f,Some(0)).await.unwrap().is_empty());
 }
 #[tokio::test]
 async fn g2b_withdrawn_catalog_items_keep_refusal_with_publisher_status(){
  let raw=catalog_html();let items=catalog(&raw).unwrap();
  let withdrawn=items.iter().filter(|i|i["tila"]=="poistettu").count();assert!(withdrawn>=30,"fixture catalog lists {withdrawn} withdrawn items");
  assert!(catalog_status_note(&items.iter().find(|i|i["nimi"]=="hoi06030").unwrap()).is_none());
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests("https://www.kaypahoito.fi/suositukset",raw.as_bytes(),"text/html");
  let soft404=std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/soft-404.html")).unwrap();
  let gone=items.iter().find(|i|i["tila"]=="poistettu"&&i["korvattu"]=="k").unwrap();let gone_key=gone["nimi"].as_str().unwrap();
  f.seed_cache_for_tests(&format!("https://www.kaypahoito.fi/{gone_key}"),soft404.as_bytes(),"text/html");
  let rows=FiCurrentCare.discover(&src(),&f,None).await.unwrap();
  let row=rows.iter().find(|r|r.extra["work_key"]==gone_key).unwrap();let e=row.extra["discovery_error"].as_str().unwrap();
  assert!(e.contains("tila=poistettu")&&e.contains(&format!("replaced by {}",gone["historynix"].as_str().unwrap()))&&e.contains("404 error page with HTTP 200"),"{e}");
  assert_eq!(row.extra["catalog_status"]["tila"],"poistettu");
  let err=FiCurrentCare.extract(&src(),&f,row).await.unwrap_err().to_string();assert!(err.contains("withdrawn"));
  assert_eq!(rows.iter().filter(|r|r.extra["catalog_status"]["tila"]=="poistettu").count(),withdrawn);
  assert!(rows.iter().filter(|r|r.extra["catalog_item"]["tila"]=="voimassa").all(|r|r.extra["catalog_status"].is_null()));
 }
 #[tokio::test]
 async fn genuine_asthma_complete_professional_figures_tables_grades_history_and_rights(){
  let raw=asthma();assert_eq!(raw.len(),519701);assert_eq!(crate::common::sha256_hex(raw.as_bytes()),"c156d7552851a3e4218015ac20120002e82991207c4bb45810d4d3faa3b0c1f3");
  let base=Url::parse(ASTHMA).unwrap();let p=page(&raw,&base).unwrap();assert_eq!(p.key,"hoi06030");assert_eq!(p.dates[0].label,"Julkaistu");assert_eq!(p.dates[0].normalized.as_deref(),Some("2022-03-29"));assert_eq!(p.metadata["machine_date_observation"]["datetime"],"2022-03-29");
  let grades=p.metadata["evidence_categories"].as_array().unwrap();let first=grades.iter().find(|g|g["evidence_review_url"]=="https://www.kaypahoito.fi/nak07819").unwrap();let second=grades.iter().find(|g|g["evidence_review_url"]=="https://www.kaypahoito.fi/nak07820").unwrap();
  assert_eq!(first["printed"],"A");assert_eq!(second["printed"],"A");assert_eq!(first["claim"],"Äidin tupakointi raskauden aikana lisää lapsen astmariskiä.");assert_ne!(first["claim"],second["claim"]);assert_eq!(first["source_context"],second["source_context"]);
  for (review,printed) in [("nak09675","A"),("nak09674","C"),("nak09673","B")]{let entry=grades.iter().find(|g|g["evidence_review_url"]==format!("https://www.kaypahoito.fi/{review}")).unwrap();assert_eq!(entry["printed"],printed);assert!(entry["source_context"].as_array().unwrap().iter().any(|v|v.as_str().unwrap().contains("5. Epäsuorat altistuskokeet")));}
  assert!(p.metadata["evidence_category_legends"].as_array().unwrap().iter().any(|v|v.as_str().unwrap().contains("D=Ei tutkimusnäyttöä")));
  let hash=revision(&p).unwrap();let extension=raw.replace("<li>","<li data-pdffiller-tried-at=\"123\">");assert_eq!(hash,revision(&page(&extension,&base).unwrap()).unwrap());
  assert_ne!(hash,revision(&page(&raw.replace("/nak07819","/nak07820"),&base).unwrap()).unwrap());
  assert!(page(&raw.replace("db-hoi db-hoi06030","db-khp db-khp00003"),&base).is_err());
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();for(u,h)in[(ASTHMA,raw.as_str()),(TERMS,RIGHTS),("https://www.kaypahoito.fi/nix00536",HISTORY)]{f.seed_cache_for_tests(u,h.as_bytes(),"text/html");}
  let mut plan=row(ASTHMA);plan.extra["clinical_sha256"]=json!(hash);let d=FiCurrentCare.extract(&src(),&f,&plan).await.unwrap();
  assert_eq!(d.published.as_deref(),Some("2022-03-29"));assert!(d.updated.is_none()&&d.country.is_none());assert_eq!(d.status,"unknown");assert_eq!(d.language.as_deref(),Some("fi"));assert_eq!(d.search_metadata["source_native_status"],"voimassa");
  assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Heading{level:1})).count(),1);
  assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),6);assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),6);
  let t1=d.blocks.iter().find(|b|b.block_id=="T1").unwrap();assert!(t1.text.contains("| OIREET JA RISKITEKIJÄT | OIREET JA RISKITEKIJÄT |"));
  let t6=d.blocks.iter().find(|b|b.block_id=="T6").unwrap();assert!(t6.text.contains("ex-actuator-annos 80, 160 ja 320 µg"));assert!(t6.text.contains("Hienojakoinen aerosoli (Aerobec®)"));
  assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::ListItem)&&b.text.contains("Diagnoosiin riittää yksikin seuraavista kuudesta löydöksestä")&&b.text.contains("2. Kahden viikon PEF-seurannassa")&&b.text.contains("Huom!: PEF-seurannassa ei pystytä valvomaan")));
  assert!(d.search_metadata["source_metadata"]["clinical_links"].as_array().unwrap().iter().any(|l|l["url"]=="https://www.kaypahoito.fi/hoi06030?tab=suositus#R61"));
  assert!(d.blocks.iter().any(|b|b.block_id=="R338"&&b.text.contains("17938726")));assert!(d.blocks.iter().any(|b|b.text.contains("Vastuun rajaus")));
  assert!(d.blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),caption:Some(c)} if u=="https://www.kaypahoito.fi/xmedia/imk/imk01482.png"&&c.contains("0–5-vuotiaan"))));
  assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})&&!b.text.contains("Artikkelin tunnus: ook00011")));
  assert!(d.attachments.iter().any(|a|a["role"]=="patient_edition"&&a["url"]=="https://www.kaypahoito.fi/khp00003"));assert!(d.attachments.iter().any(|a|a["role"]=="implementation_article"&&a["url"]=="https://www.kaypahoito.fi/ook00011"));
  assert_eq!(d.rights.status,"restricted");assert!(d.rights.restrictions.iter().any(|s|s.contains("tekoälyohjelmiston")&&s.contains("Kuvien käyttöoikeuksia")));
  let metadata=&d.search_metadata["source_metadata"];assert_eq!(metadata["reuse_policy"]["date_observation"]["normalized"],"2026-02-02");assert_eq!(metadata["history_article"]["article_date_observation"]["datetime"],"2026-04-17");
  let sections=metadata["history_article"]["sections"].as_array().unwrap();assert!(sections.iter().any(|s|s["label"]=="Korjaus"&&s["text"].as_str().unwrap().contains("14.4.2026 korjattu Taulukko 3. Trimbow")));assert!(sections.iter().any(|s|s["label"]=="3. päivitys"&&s["text"].as_str().unwrap().contains("29.3.2022")));
  f.seed_cache_for_tests(ASTHMA,raw.replace("/nak07819","/nak07820").as_bytes(),"text/html");assert!(FiCurrentCare.extract(&src(),&f,&plan).await.is_err());
 }
 #[tokio::test]
 async fn genuine_hypertension_terminal_tail_and_separate_implementation(){
  let raw=hypertension();assert_eq!(raw.len(),574435);assert_eq!(crate::common::sha256_hex(raw.as_bytes()),"88bffe8d0ccd3773e2bdb3d80f97e0315a800cf186ee0d1c009cd47b33a18fce");
  let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(HTN,raw.as_bytes(),"text/html");f.seed_cache_for_tests(TERMS,RIGHTS.as_bytes(),"text/html");
  let d=FiCurrentCare.extract(&src(),&f,&row(HTN)).await.unwrap();assert_eq!(d.work_key.as_deref(),Some("hoi04010"));assert_eq!(d.published.as_deref(),Some("2026-09-21"));assert!(d.updated.is_none());
  assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),7);assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),7);
  let t7=d.blocks.iter().find(|b|b.block_id=="T7").unwrap();assert!(t7.text.contains("Suuren riskin potilaat")&&t7.text.contains("GFR alle 60 ml/min")&&t7.text.contains("Verenpainelääkitykseen liittyvät laboratoriokokeet"));
  assert!(d.blocks.iter().any(|b|b.block_id=="R330"&&b.text.contains("39349629")));assert!(d.blocks.iter().all(|b|!b.text.contains("Artikkelin tunnus: ook00046")));
  assert!(d.attachments.iter().any(|a|a["role"]=="implementation_article"&&a["url"]=="https://www.kaypahoito.fi/ook00046"));
 }
 #[test]fn genuine_overlapping_table_spans_preserve_native_cells(){let html=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/hoi50023-table-t8.html"));let(mut b,_)=blocks_from_html(html,None,Some("#fi-clinical"));let warnings=tables(html,&mut b).unwrap();assert!(warnings.iter().any(|w|w.contains("T8")&&w.contains("ambiguous spans")));let table=b.iter().find(|b|matches!(b.kind,BlockKind::Table{..})).unwrap();assert!(table.text.contains("Riittämättömät tiedot")&&table.text.contains("rowspan=2")&&table.text.contains("colspan=2")&&table.text.contains("Käytä mahdollisuuksien"));assert!(!table.text.contains("| --- |"));}
 #[test]fn genuine_http_200_error_page_is_not_professional_guideline(){let raw=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/fi_current_care/soft-404.html"));for u in ["https://www.kaypahoito.fi/hoi50027","https://www.kaypahoito.fi/hoi50029"]{let err=match page(raw,&Url::parse(u).unwrap()){Ok(_)=>panic!("source 404 must not produce guideline"),Err(e)=>format!("{e:#}")};assert!(err.contains("404 error page with HTTP 200")&&err.contains("no replacement inferred"));}}

}
