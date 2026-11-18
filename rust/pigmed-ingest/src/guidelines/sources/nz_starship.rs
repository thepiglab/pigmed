//! Starship Gatsby page-data: source sitemap -> content-ready Contentful JSON.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct NzStarship;
const ORIGIN:&str="https://www.starship.org.nz";
const SITEMAP:&str="https://www.starship.org.nz/sitemap-index.xml";
fn esc(s:&str)->String {s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;")}
// Preserve the source target, including anchors that identify a passage; the
// shared download identity helper deliberately removes fragments.
fn source_link(base:&Url,href:&str)->Option<String> {
 let u=base.join(href.trim()).ok()?;
 if !matches!(u.scheme(),"http"|"https")||!u.username().is_empty()||u.password().is_some(){return None;}
 Some(u.to_string())
}
fn sitemap(html:&str)->Result<Vec<String>> {
 let doc=roxmltree::Document::parse(html).context("Starship sitemap XML missing")?;
 let mut out=vec![];let mut seen=HashSet::new();
 for n in doc.descendants().filter(|n|n.is_element() && n.tag_name().name()=="loc"){if let Some(u)=n.text(){let u=u.trim();if seen.insert(u.to_string()){out.push(u.to_string());}}}
 if out.is_empty(){bail!("Starship sitemap has no URLs");}Ok(out)
}
fn page_data_url(page:&Url)->Result<String> {
 if page.origin()!=Url::parse(ORIGIN)?.origin() {bail!("Starship URL crosses origin");}
 let parts:Vec<_>=page.path_segments().into_iter().flatten().filter(|s|!s.is_empty()).collect();
 if parts.len()!=2||parts[0]!="guidelines"||["browse","search","my-shortlist"].contains(&parts[1])||page.query().is_some(){bail!("not a bounded Starship guideline page");}
 // Exact Gatsby loader pattern found in the publisher's cached app JS.
 Ok(format!("{ORIGIN}/page-data/guidelines/{}/page-data.json",parts[1]))
}
// Resolve the exact source-linked diagram files before mapping the enclosing page.
// An unavailable or unrecognized panel makes the item unresolved rather than partial.
fn graph_file(raw:&str)->Result<(String,usize,usize)> {
 let doc=Html::parse_document(raw);let graph=doc.select(&sel("div.mxgraph[data-mxgraph]")).next().context("diagram wrapper missing")?;
 let spec:Value=serde_json::from_str(graph.value().attr("data-mxgraph").context("diagram data missing")?)?;
 let xml=spec["xml"].as_str().context("diagram XML missing")?;let tree=roxmltree::Document::parse(xml).context("diagram XML invalid")?;
 let cells=tree.descendants().filter(|n|n.is_element()&&n.tag_name().name()=="mxCell").count();let edges=tree.descendants().filter(|n|n.is_element()&&n.tag_name().name()=="mxCell"&&n.attribute("edge")==Some("1")).count();
 if cells==0{bail!("diagram has no decoded mxCell nodes");}Ok((xml.to_string(),cells,edges))
}
fn referenced_entries(node:&Value,out:&mut HashSet<String>){if node["nodeType"]=="embedded-entry-block"{if let Some(id)=node["data"]["target"]["sys"]["id"].as_str(){out.insert(id.to_string());}}if let Some(c)=node["content"].as_array(){for child in c{referenced_entries(child,out);}}}
async fn resolve_embedded(raw:&str,page:&Url,f:&Fetcher)->Result<(String,Vec<crate::guidelines::doc::FetchRecord>)>{
 let mut v:Value=serde_json::from_str(raw)?;let g=&mut v["result"]["data"]["contentfulPageGuideline"];let tree:Value=serde_json::from_str(g["richtext"]["raw"].as_str().context("clinical body missing")?)?;let mut ids=HashSet::new();referenced_entries(&tree,&mut ids);let refs=g["richtext"]["references"].as_array_mut().context("embedded reference inventory missing")?;let mut records=vec![];
 for r in refs.iter_mut(){let id=r["contentful_id"].as_str().unwrap_or("").to_string();if !ids.contains(&id)||r["__typename"]!="ContentfulWidgetEmbedded"{continue;}
  let kind=r["type"]["type"].as_str().context("embedded widget type missing")?;if kind!="Embed file"{bail!("unmapped embedded clinical widget type {kind} ({id})");}
  let title=r["title"]["title"].as_str().context("embedded widget title missing")?.to_string();let href=r["htmlCode"]["url"].as_str().or_else(||r["htmlCode"]["file"]["url"].as_str()).context("embedded file URL missing")?;let url=source_link(page,href).context("invalid embedded clinical file URL")?;
  let fetched=f.get_ok(&url).await.with_context(||format!("embedded clinical panel unavailable: {title} ({url})"))?;let original=String::from_utf8(fetched.body.to_vec()).context("embedded clinical file is not UTF-8")?;if !original.to_ascii_lowercase().contains("</html>"){bail!("embedded clinical file HTML is incomplete");}let (xml,cells,edges)=graph_file(&original).with_context(||format!("unmapped embedded panel: {title} ({url})"))?;
  r["_source_embedded_graph"]=json!({"contentful_id":id,"title":title,"url":url,"sha256":fetched.record.sha256,"original_html":original,"graph_xml":xml,"cell_count":cells,"edge_count":edges,"rights":"unknown; independent asset"});records.push(fetched.record);
 }
 Ok((serde_json::to_string(&v)?,records))
}
fn render_children(node:&Value,refs:&[Value],base:&Url,assets:&mut Vec<Value>,loc:&str)->Result<String> {
 let mut out=String::new();for (i,child) in node["content"].as_array().context("Contentful node children missing")?.iter().enumerate() {out.push_str(&render(child,refs,base,assets,&format!("{loc}/content/{i}"))?);}Ok(out)
}
// Keep parent condition and all subordinate clauses in a single source-derived list item.
// Inline links are recorded separately by render_children before the textual group is emitted.
fn list_text(node:&Value)->Result<String> {
 let ty=node["nodeType"].as_str().context("list node type missing")?;
 if ty=="text" {return Ok(node["value"].as_str().context("list text missing")?.to_string());}
 if matches!(ty,"embedded-entry-block"|"embedded-asset-block") {let id=node["data"]["target"]["sys"]["id"].as_str().context("nested embedded ID missing")?;return Ok(format!(" [Source embedded content: {id}] "));}
 let children=node["content"].as_array().context("list content missing")?;let mut out=String::new();
 for child in children {
  let ct=child["nodeType"].as_str().unwrap_or("");
  if ct=="unordered-list"||ct=="ordered-list" {for (i,item) in child["content"].as_array().context("nested list children missing")?.iter().enumerate() {if ct=="ordered-list" {out.push_str(&format!(" {}. ",i+1));} else {out.push_str(" - ");}out.push_str(&list_text(item)?);}}
  else {out.push_str(&list_text(child)?);if ct=="paragraph" {out.push(' ');}}
 }Ok(out.trim().to_string())
}
fn nested_embedded(n:&Value)->bool {
 matches!(n["nodeType"].as_str(),Some("embedded-entry-block"|"embedded-asset-block")) || n["content"].as_array().is_some_and(|a|a.iter().any(nested_embedded))
}
fn render(node:&Value,refs:&[Value],base:&Url,assets:&mut Vec<Value>,loc:&str)->Result<String> {
 let ty=node["nodeType"].as_str().context("Contentful node type missing")?;
 Ok(match ty {
  "text"=>esc(node["value"].as_str().context("Contentful text missing")?),
  "document"=>render_children(node,refs,base,assets,loc)?,
  "paragraph"=>format!("<p>{}</p>",render_children(node,refs,base,assets,loc)?),
  "hr"=>"<hr>".to_string(),
  "blockquote"=>format!("<blockquote>{}</blockquote>",render_children(node,refs,base,assets,loc)?),
  "embedded-asset-block"|"asset-hyperlink"=>{let id=node["data"]["target"]["sys"]["id"].as_str().context("Contentful asset ID missing")?;
   let asset=refs.iter().find(|r|r["contentful_id"]==id&&r["__typename"]=="ContentfulAsset").context("unresolved Contentful asset")?;
   let href=asset["file"]["url"].as_str().context("Contentful asset URL missing")?;let url=source_link(base,href).context("invalid Contentful asset URL")?;
   let mime=asset["file"]["contentType"].as_str().context("Contentful asset MIME missing")?;let label=asset["assetTitle"].as_str().unwrap_or("");
   let role=if ty=="asset-hyperlink" {"asset_link"}else if mime.starts_with("image/"){"figure"}else{bail!("unmapped embedded non-image asset {id}: {mime}")};
   assets.push(json!({"url":url,"role":role,"label":label,"content_type":mime,"contentful_id":id,"source_locator":loc,"rights":"unknown; independent asset"}));
   if ty=="asset-hyperlink" {format!("<a href=\"{}\">{}</a>",esc(&url),render_children(node,refs,base,assets,loc)?)} else {format!("<img src=\"{}\" alt=\"{}\">",esc(&url),esc(label))}
  },
  "heading-1"|"heading-2"|"heading-3"|"heading-4"|"heading-5"|"heading-6"=>{let h=&ty[8..];format!("<h{h}>{}</h{h}>",render_children(node,refs,base,assets,loc)?)},
  "unordered-list"=>format!("<ul>{}</ul>",render_children(node,refs,base,assets,loc)?),"ordered-list"=>format!("<ol>{}</ol>",render_children(node,refs,base,assets,loc)?),"list-item"=>{
   let nested=node["content"].as_array().context("list item children missing")?.iter().any(|c|matches!(c["nodeType"].as_str(),Some("unordered-list"|"ordered-list")));
   let has_embedded=nested && nested_embedded(node);
   let rendered=render_children(node,refs,base,assets,loc)?;
   // The additional grouped text keeps ancestor conditions tied to exact source
   // embedded IDs, while the rendered subtree retains the real table/figure.
   if has_embedded {format!("<li><p>{}</p>{rendered}</li>",esc(&list_text(node)?))}
   else if nested {format!("<li>{}</li>",esc(&list_text(node)?))} else {format!("<li>{rendered}</li>")}
  },
  "hyperlink"=>{let href=node["data"]["uri"].as_str().context("hyperlink URL missing")?;
   // Contact links are retained as source context, never as downloadable assets.
   let contact=Url::parse(href.trim()).ok().filter(|u|matches!(u.scheme(),"mailto"|"tel") && !u.path().is_empty() && !href.chars().any(char::is_control));
   let (url,role)=if let Some(u)=contact {(u.to_string(),"contact_link")} else {let u=source_link(base,href).context("invalid hyperlink")?;let role=if loc.starts_with("references/"){"reference_link"}else if Url::parse(&u)?.path().to_ascii_lowercase().ends_with(".pdf"){"linked_pdf"}else{"inline_link"};(u,role)};
   assets.push(json!({"url":url,"role":role,"source_locator":loc,"rights":"unknown; independent asset"}));format!("<a href=\"{}\">{}</a>",esc(&url),render_children(node,refs,base,assets,loc)?)},
  "entry-hyperlink"=>{let id=node["data"]["target"]["sys"]["id"].as_str().context("entry link ID missing")?;let r=refs.iter().find(|r|r["contentful_id"]==id).context("unresolved entry hyperlink")?;let slug=r["slug"].as_str().context("entry hyperlink slug missing")?;assets.push(json!({"url":format!("{ORIGIN}/guidelines/{slug}"),"role":"related_guideline","source_locator":loc,"rights":"unknown"}));format!("<a href=\"{ORIGIN}/guidelines/{}\">{}</a>",esc(slug),render_children(node,refs,base,assets,loc)?)},
  "embedded-entry-block"=>{let id=node["data"]["target"]["sys"]["id"].as_str().context("embedded entry ID missing")?;let r=refs.iter().find(|r|r["contentful_id"]==id).context("unresolved embedded entry")?;
   match r["__typename"].as_str().unwrap_or("") {
    "ContentfulWidgetTable"=>{let html=r["test"]["test"].as_str().context("table HTML missing")?;let d=Html::parse_fragment(html);if d.select(&sel("table")).count()!=1{bail!("embedded table structure unrecognized");}
     if d.select(&sel("svg,canvas,object")).next().is_some(){bail!("embedded table contains unmapped non-image visual");}
     for (i,a) in d.select(&sel("a[href]")).enumerate(){if let Some(url)=source_link(base,a.value().attr("href").unwrap_or_default()){assets.push(json!({"url":url,"role":"table_link","source_locator":format!("{loc}/table-link/{i}"),"label":text_of(&a),"rights":"unknown; independent asset"}));}}html.replacen("<table",&format!("<table data-starship-source-id=\"{}\"",esc(id)),1)},
    "ContentfulWidgetImage"=>{if r["enableHyperlink"].as_bool()==Some(true)||!r["hyperlink"].is_null()||!r["hyperlinkEntry"].is_null(){bail!("linked image widget requires mapping ({id})");}
     let images=r["images"].as_array().filter(|a|!a.is_empty()).context("image widget images missing")?;let mut html=String::new();
     for (i,image) in images.iter().enumerate(){let mime=image["file"]["contentType"].as_str().context("image MIME missing")?;if !mime.starts_with("image/"){bail!("non-image file in image widget {id}");}
      let url=source_link(base,image["file"]["url"].as_str().context("image URL missing")?).context("invalid image widget URL")?;
      let title=r["imageTitle"].as_str().filter(|s|!s.is_empty()).or_else(||image["title"].as_str()).unwrap_or("");
      assets.push(json!({"url":url,"role":"figure","label":title,"image_title":image["title"],"description":image["description"],"content_type":mime,"contentful_id":id,"source_locator":format!("{loc}/images/{i}"),"rights":"unknown; independent asset"}));
      html.push_str(&format!("<img src=\"{}\" alt=\"{}\">",esc(&url),esc(title)));
     }html},
    "ContentfulWidgetEmbedded"=>{let graph=&r["_source_embedded_graph"];let title=graph["title"].as_str().context("ContentfulWidgetEmbedded clinical file bytes not acquired")?;let url=graph["url"].as_str().context("embedded graph URL missing")?;let hash=graph["sha256"].as_str().context("embedded graph hash missing")?;
     assets.push(json!({"url":url,"role":"embedded_clinical_graph","label":title,"contentful_id":id,"source_locator":loc,"sha256":hash,"rights":"unknown; independent asset"}));format!("<figure id=\"starship-graph-{}\"><figcaption>{}</figcaption></figure>",esc(id),esc(title))},
    "ContentfulWidgetAlert"=>format!("<p>{}</p>",esc(r["text"]["text"].as_str().context("alert text missing")?)),
    other=>bail!("unmapped embedded Contentful entry {other} ({id}); refusing partial body"),
   }
  },
  other=>bail!("unmapped Contentful node {other}; refusing partial body"),
 })
}
// Retain original table relationships through rowspan/colspan expansion.
fn table_spans(table:ElementRef<'_>,block_id:&str)->Result<String> {
 if table.select(&sel("table")).next().is_some(){bail!("nested table unsupported");}
 let mut active:Vec<Option<(String,usize)>>=vec![];let mut rows:Vec<Vec<String>>=vec![];
 for (ri,tr) in table.select(&sel("tr")).enumerate(){let cells=common::cells(tr);if cells.is_empty(){continue}let mut row=vec![];let(mut col,mut next)=(0,0);
  while next<cells.len()||active.iter().skip(col).any(Option::is_some){if col>=active.len(){active.push(None)}
   if let Some((v,left))=active[col].clone(){row.push(v);active[col]=if left>1{Some((row[col].clone(),left-1))}else{None};col+=1;continue;}
   if next>=cells.len(){row.push(String::new());col+=1;continue}let cell=cells[next];let ci=next;next+=1;
   let rs=cell.value().attr("rowspan").unwrap_or("1").parse::<usize>()?.clamp(1,100);let cs=cell.value().attr("colspan").unwrap_or("1").parse::<usize>()?.clamp(1,100);let mut text=text_of(&cell).replace('|',"\\|");for (ii,_) in cell.select(&sel("img")).enumerate(){text.push_str(&format!(" [image {block_id}-r{}-c{}-i{}]",ri+1,ci+1,ii+1));}
   for _ in 0..cs {if col>=active.len(){active.push(None)}if active[col].is_some(){bail!("overlapping table spans");}row.push(text.clone());if rs>1{active[col]=Some((text.clone(),rs-1));}col+=1;}
  }rows.push(row);
 }
 if rows.is_empty(){bail!("empty table");}let width=rows.iter().map(Vec::len).max().unwrap_or(0);
 let line=|r:&Vec<String>|{let mut v=r.clone();v.resize(width,String::new());format!("| {} |",v.join(" | "))};let mut out=vec![line(&rows[0]),format!("|{}|",vec!["---";width].join("|"))];out.extend(rows.iter().skip(1).map(line));Ok(out.join("\n"))
}
fn content(raw:&str,page:&Url)->Result<(String,String,String,Vec<Block>,Vec<String>,Vec<Value>,Value)> {
 let value:Value=serde_json::from_str(raw)?;let g=&value["result"]["data"]["contentfulPageGuideline"];
 let title=g["title"].as_str().filter(|s|!s.is_empty()).context("Starship content-ready title missing")?.to_string();
 let slug=g["slug"].as_str().context("Starship slug missing")?;if page.path().trim_end_matches('/').rsplit('/').next()!=Some(slug){bail!("Starship page-data slug mismatch");}
 let date=g["publishDate"].as_str().context("Starship publication date missing")?.to_string();
 for field in ["externalTables","calculators"] {if !g[field].is_null() && g[field].as_array().map_or(true,|a|!a.is_empty()){bail!("Starship {field} requires a source-specific mapping");}}
 let refs=g["richtext"]["references"].as_array().context("Starship embedded reference inventory missing")?;
 let tree:Value=serde_json::from_str(g["richtext"]["raw"].as_str().context("Starship clinical body missing")?)?;let mut assets=vec![];
 let mut html=String::from("<div id='starship-clinical'>");
 if let Some(summary)=g["summary"]["childMarkdownRemark"]["html"].as_str(){html.push_str(summary);}
 html.push_str(&render(&tree,refs,page,&mut assets,"richtext")?);
 if let Some(references)=g["references"]["raw"].as_str(){let r:Value=serde_json::from_str(references)?;html.push_str("<h3>References</h3>");html.push_str(&render(&r,&[],page,&mut assets,"references")?);}
 html.push_str("<h3>Document Control</h3>");for (key,label) in [("publishDate","Date last published"),("reviewFrequency","Review frequency"),("guidelineType","Document type"),("serviceProvider","Service provider"),("authors","Authors"),("owner","Owner"),("editor","Editor")] {let v=&g[key];if v.is_null(){continue}let display=match key {"guidelineType"=>v["guidelineType"].as_str().unwrap_or("").to_string(),"serviceProvider"=>v["name"].as_str().unwrap_or("").to_string(),"owner"|"editor"=>format!("{} {}",v["firstName"].as_str().unwrap_or(""),v["lastName"].as_str().unwrap_or("")),"authors"=>v.as_array().map(|a|a.iter().map(|x|format!("{} {}",x["firstName"].as_str().unwrap_or(""),x["lastName"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ")).unwrap_or_default(),_=>v.as_str().map(String::from).unwrap_or_else(||v.to_string())};html.push_str(&format!("<p>{}: {}</p>",label,esc(&display)));}
 html.push_str("</div>");let doc=Html::parse_fragment(&html);let tables=doc.select(&sel("#starship-clinical table")).collect::<Vec<_>>();let (mut blocks,mut warnings)=blocks_from_html(&html,Some(page),Some("#starship-clinical"));
 let positions=blocks.iter().enumerate().filter_map(|(i,b)|matches!(b.kind,BlockKind::Table{..}).then_some(i)).collect::<Vec<_>>();if positions.len()!=tables.len(){bail!("Starship table count mismatch");}
 let mut table_sources=vec![];let mut table_figures:Vec<(usize,Block)>=vec![];
 for (table,i) in tables.into_iter().zip(positions){
  let id=table.value().attr("data-starship-source-id").context("embedded table source ID missing")?;
  let source=refs.iter().find(|r|r["contentful_id"]==id).context("embedded table source missing")?;
  let table_id=blocks[i].block_id.clone();let mut images=vec![];
  for (ri,tr) in table.select(&sel("tr")).enumerate(){for (ci,cell) in common::cells(tr).iter().enumerate(){for (ii,img) in cell.select(&sel("img")).enumerate(){
   let raw=img.value().attr("src").context("table image source missing")?;let external=if raw.starts_with("data:image/")&&raw.contains(";base64,"){None}else{Some(source_link(page,raw).context("unsupported table image source")?)};
   let alt=img.value().attr("alt").filter(|s|!s.is_empty()).map(String::from);let figure_id=format!("{table_id}-r{}-c{}-i{}",ri+1,ci+1,ii+1);
   images.push(json!({"figure_block_id":figure_id,"source_row":ri+1,"source_cell":ci+1,"image_in_cell":ii+1,"cell_text":text_of(cell),"rowspan":cell.value().attr("rowspan"),"colspan":cell.value().attr("colspan"),"alt":alt,"source_src_sha256":crate::common::sha256_hex(raw.as_bytes()),"source_locator":format!("richtext.references:{id}.test.test / tr[{}] / cell[{}] / img[{}]",ri+1,ci+1,ii+1),"source_storage":if external.is_none(){"original_html data URI"}else{"external URL"},"rights":"unknown; independent asset"}));
   if let Some(url)=&external {assets.push(json!({"url":url,"role":"table_figure","source_locator":format!("{id}/tr/{}/cell/{}/img/{}",ri+1,ci+1,ii+1),"rights":"unknown; independent asset"}));}
   table_figures.push((i,Block{block_id:figure_id,kind:BlockKind::Figure{src_url:external,caption:alt},text:String::new(),heading_path:blocks[i].heading_path.clone(),page:None}));
  }}}
  if images.len()!=table.select(&sel("img")).count(){bail!("table image outside a source row/cell requires mapping");}
  table_sources.push(json!({"block_id":table_id,"contentful_id":id,"original_html":source["test"]["test"],"source_locator":format!("richtext.references:{id}.test.test"),"images":images}));
  if table.select(&sel("[rowspan],[colspan],img")).next().is_some(){blocks[i].text=table_spans(table,&table_id)?;}

 }
 for (i,figure) in table_figures.into_iter().rev(){blocks.insert(i+1,figure);}
 // Graph node labels are indexed in source XML order only; the XML, geometry and
 // edge endpoints remain authoritative and are not converted into a clinical sequence.
 for r in refs {let graph=&r["_source_embedded_graph"];let Some(xml)=graph["graph_xml"].as_str() else {continue};let id=graph["contentful_id"].as_str().context("graph ID missing")?;let panel=graph["title"].as_str().context("graph title missing")?;
  let figure_id=format!("starship-graph-{id}");if blocks.iter().filter(|b|b.block_id==figure_id||b.block_id.starts_with(&format!("{figure_id}~"))).count()!=1{bail!("repeated graph occurrence requires source-specific mapping");}let index=blocks.iter().position(|b|b.block_id==figure_id).context("graph figure block missing")?;let mut heading=blocks[index].heading_path.clone();heading.push(panel.to_string());let parsed=roxmltree::Document::parse(xml)?;let mut labels=vec![];
  for (n,node) in parsed.descendants().filter(|n|n.is_element()&&matches!(n.tag_name().name(),"mxCell"|"UserObject")).enumerate(){let raw=if node.tag_name().name()=="mxCell"{node.attribute("value")}else{node.attribute("label")};let Some(raw)=raw else {continue};
   let fragment=Html::parse_fragment(raw);let text=text_of(&fragment.root_element());if text.trim().is_empty(){continue;}let cell_id=node.attribute("id").or_else(||node.parent().and_then(|p|p.attribute("id"))).unwrap_or("");
   labels.push(Block{block_id:format!("{figure_id}-node-{n}"),kind:BlockKind::Paragraph,text:format!("{panel} / node {cell_id}: {text}"),heading_path:heading.clone(),page:None});
  }for (offset,block) in labels.into_iter().enumerate(){blocks.insert(index+1+offset,block);}
 }
 for field in ["healthPathways","relatedInformation","informationForFamilies","forms"] {if let Some(items)=g[field].as_array(){for (i,item) in items.iter().enumerate() {
  let direct=item["url"].as_str().and_then(|u|source_link(page,u));
  let related=if field=="relatedInformation" {item["slug"].as_str().and_then(|slug| {
   if slug.is_empty()||!slug.bytes().all(|c|c.is_ascii_alphanumeric()||c==b'-'){return None;}
   let url=format!("{ORIGIN}/guidelines/{slug}/");let parsed=Url::parse(&url).ok()?;page_data_url(&parsed).ok()?;Some(url)
  })} else {None};
  if let Some(url)=direct.or(related){assets.push(json!({"url":url,"role":field,"label":item["text"].as_str().or_else(||item["title"].as_str()),"source_locator":format!("{field}/{i}"),"rights":"unknown; independent asset"}));}
  else {warnings.push(format!("Starship {field} entry lacks a mapped URL; original JSON retained in metadata"));}
 }}}
 // Do not collapse distinct source occurrences or roles merely because their URLs match.
 let hash=crate::common::sha256_hex(serde_json::to_string(g)?.as_bytes());warnings.push("Known Contentful types only; unrecognized nodes/assets fail closed. Table span expansion preserves cell context but visual layout and images require review; PDF links are independent unacquired assets. Inline formatting marks and superscript semantics require verification.".into());
 if table_sources.iter().any(|x|x["images"].as_array().is_some_and(|a|!a.is_empty())){warnings.push("Table image bytes are retained in original table HTML and linked to source cell coordinates; image contents have not been transcribed.".into());}
 let mut metadata=g.clone();metadata["_source_table_mappings"]=json!(table_sources);metadata["_source_embedded_graphs"]=Value::Array(refs.iter().filter_map(|r|r.get("_source_embedded_graph").cloned()).collect());
 Ok((title,hash,date,blocks,warnings,assets,metadata))
}
fn unresolved(src:&SourceEntry,url:String,error:String)->PlanRow{PlanRow{source_id:src.id.clone(),doc_key:format!("{}-unresolved",common::short_hash(&url)),url,title:None,rendition:Some("html".into()),version:None,extra:json!({"discovery_error":error})}}
#[async_trait::async_trait]
impl Adapter for NzStarship {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![])}let index=f.get_ok(SITEMAP).await?;let mut rows=vec![];let mut seen=HashSet::new();
  for child in sitemap(&index.text())? {let u=Url::parse(&child)?;if u.origin()!=Url::parse(ORIGIN)?.origin(){bail!("Starship sitemap child crosses origin");}let child_page=f.get_ok(&child).await?;
   for url in sitemap(&child_page.text())? {let page=match Url::parse(&url){Ok(u)=>u,Err(_)=>continue};let data_url=match page_data_url(&page){Ok(u)=>u,Err(_)=>continue};if !seen.insert(url.clone()){continue;}
    let fetched=match f.get_ok(&data_url).await{Ok(p)=>p,Err(e)=>{rows.push(unresolved(src,url,format!("page-data unavailable: {e:#}")));if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}continue;}};
    let (resolved,_)=match resolve_embedded(&fetched.text(),&page,f).await {Ok(x)=>x,Err(e)=>{rows.push(unresolved(src,url,format!("embedded clinical mapping: {e:#}")));if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}continue;}};
    match content(&resolved,&page){Ok((title,hash,date,_,_,_,_))=>{let key=common::slug(page.path());rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&hash[..12]),url,title:Some(title),rendition:Some("json".into()),version:None,extra:json!({"work_key":key,"page_data_url":data_url,"clinical_sha256":hash,"published":date,"sitemap_fetch":index.record,"child_fetch":child_page.record})});},Err(e)=>rows.push(unresolved(src,url,format!("page-data clinical mapping: {e:#}")))}
    if limit.is_some_and(|n|rows.len()>=n){return Ok(rows)}
   }
  }if rows.is_empty(){bail!("Starship sitemap yielded no mapped clinical pages");}Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("Starship discovery unresolved: {e}");}let page=Url::parse(&row.url)?;let data_url=page_data_url(&page)?;if row.extra["page_data_url"].as_str().is_some_and(|s|s!=data_url){bail!("Starship page-data URL changed");}
  let data=f.get_ok(&data_url).await?;let (resolved,graph_fetches)=resolve_embedded(&data.text(),&page,f).await?;let (title,hash,date,blocks,mut warnings,attachments,g)=content(&resolved,&page)?;if row.extra["clinical_sha256"].as_str().is_some_and(|s|s!=hash){bail!("Starship clinical revision changed after planning");}
  let observed=common::date("Date last published",&date,&data_url);let published=observed.normalized.clone();let mut fetches=vec![];for key in ["sitemap_fetch","child_fetch"]{if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra[key].clone()){fetches.push(r)}}fetches.push(data.record);fetches.extend(graph_fetches);
  if g["_source_embedded_graphs"].as_array().is_some_and(|a|!a.is_empty()){warnings.push("Embedded clinical diagrams are preserved as original HTML and graph XML. Figure has no separate image rendition. Indexed source labels are in XML order only; connections and image contents have not been interpreted or transcribed.".into());}
  warnings.push("Sitemap inventory is candidate-based and filters non-clinical routes; completeness, item rights and related linked assets require review. No recommendation grade is inferred.".into());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),item_type:match g["guidelineType"]["guidelineType"].as_str(){Some("Clinical guideline"|"Clinical Guideline")=>Some("clinical_guideline".into()),Some("Clinical pathway"|"Clinical Pathway")=>Some("clinical_pathway".into()),_=>None},status:"unknown".into(),published,date_observations:vec![observed],rights:Rights{status:"unknown".into(),note:Some("Item and linked asset rights not yet verified.".into()),..Default::default()},search_metadata:json!({"service_provider":g["serviceProvider"],"authors":g["authors"],"owner":g["owner"],"editor":g["editor"],"review_frequency":g["reviewFrequency"],"disclaimer":g["disclaimer"],"forms":g["forms"],"related_information":g["relatedInformation"],"source_table_mappings":g["_source_table_mappings"],"source_embedded_graphs":g["_source_embedded_graphs"]}),blocks,attachments,fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))] mod tests {
 use super::*;
 const A:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/acute-page-data.json"));
 const L:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/life-threatening-page-data.json"));
 const URL:&str="https://www.starship.org.nz/guidelines/acute-asthma-and-wheeze/";
 #[test] fn genuine_gatsby_pages_preserve_clinical_tables_dates_and_references(){
  let page=Url::parse(URL).unwrap();let (title,_,date,blocks,_,assets,_)=content(A,&page).unwrap();assert_eq!(title,"Asthma and wheeze, management of acute");assert_eq!(date,"2026-01-05");
  for heading in ["Exclusions to this guideline","Acute Asthma Management Algorithm","References","Document Control"] {assert!(blocks.iter().any(|b|b.text==heading));}
  let table=blocks.iter().find(|b|matches!(b.kind,BlockKind::Table{..})).unwrap();assert!(table.text.contains("IMMEDIATELY ON ARRIVAL")&&table.text.contains("AFTER 1-2 HOURS")&&table.text.contains("LIFE-THREATENING"));
  assert!(assets.iter().any(|a|a["role"]=="linked_pdf"));assert!(assets.iter().any(|a|a["role"]=="related_guideline"));
  let life=Url::parse("https://www.starship.org.nz/guidelines/asthma-life-threatening/").unwrap();let (_,_,_,b,_,_,_)=content(L,&life).unwrap();assert_eq!(b.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),2);
  let broken=A.replace("embedded-entry-block","unmapped-entry");assert!(content(&broken,&page).is_err());
 }
 #[test] fn genuine_nested_conditions_keep_their_parent_and_order(){
  let page=Url::parse(URL).unwrap();let (_,_,_,blocks,_,_,_)=content(A,&page).unwrap();
  let candidates=blocks.iter().filter(|b|b.text.contains("Steroid treatment should be given")).collect::<Vec<_>>();
  assert_eq!(candidates.len(),1);let t=&candidates[0].text;
  for phrase in ["1-5 year age group", "Attack is considered severe", "History of previous severe attacks", "Emergency department stay", ">6 hours", "admission considered likely"] {assert!(t.contains(phrase),"missing {phrase}: {t}");}
  let page=Url::parse("https://www.starship.org.nz/guidelines/asthma-life-threatening/").unwrap();let (_,_,_,blocks,_,_,_)=content(L,&page).unwrap();
  let candidates=blocks.iter().filter(|b|b.text.contains("Consider IM adrenaline")).collect::<Vec<_>>();assert_eq!(candidates.len(),1);let t=&candidates[0].text;
  for phrase in ["0.01 ml/kg", "10 micrograms/kg", "1:1000", "minimum dose 0.1 mL", "maximum dose 0.5mL", "1. Peri-arrest", "2. Anaphylaxis is suspected", "3. Delay in obtaining intravenous access"] {assert!(t.contains(phrase),"missing {phrase}: {t}");}
 }
 #[test] fn genuine_links_keep_each_occurrence_role_and_original_table_html(){
  let page=Url::parse(URL).unwrap();let (_,_,_,blocks,_,assets,g)=content(A,&page).unwrap();
  let pdfs=assets.iter().filter(|a|a["role"]=="linked_pdf" && a["url"].as_str().unwrap_or("").to_ascii_lowercase().contains("management-plan")).collect::<Vec<_>>();
  assert!(pdfs.len()>=2);assert_ne!(pdfs[0]["source_locator"],pdfs[1]["source_locator"]);
  assert!(assets.iter().any(|a|a["role"]=="reference_link" && a["source_locator"].as_str().unwrap_or("").starts_with("references/")));
  let mappings=g["_source_table_mappings"].as_array().unwrap();assert_eq!(mappings.len(),1);
  let html=mappings[0]["original_html"].as_str().unwrap();assert_eq!(html.matches("<tr").count(),12);assert!(html.contains("colspan=\"4\""));
  assert!(blocks.iter().any(|b|b.block_id==mappings[0]["block_id"].as_str().unwrap()));
  let page=Url::parse("https://www.starship.org.nz/guidelines/asthma-life-threatening/").unwrap();let (_,_,date,_,_,assets,g)=content(L,&page).unwrap();assert_eq!(date,"2024-08-21");
  assert!(assets.iter().any(|a|a["role"]=="relatedInformation" && a["label"]=="Asthma and wheeze, management of acute" && a["url"]==URL && a["source_locator"]=="relatedInformation/0"));
  let mappings=g["_source_table_mappings"].as_array().unwrap();assert_eq!(mappings.len(),2);assert_eq!(mappings[0]["original_html"].as_str().unwrap().matches("<tr").count(),16);assert_eq!(mappings[1]["original_html"].as_str().unwrap().matches("<tr").count(),6);
 }
 #[test] fn unsupported_calculators_and_embedded_types_fail_closed(){
  let page=Url::parse(URL).unwrap();let mut v:Value=serde_json::from_str(A).unwrap();v["result"]["data"]["contentfulPageGuideline"]["calculators"]=json!([{"title":"unmapped"}]);assert!(content(&v.to_string(),&page).is_err());
  let mut v:Value=serde_json::from_str(A).unwrap();let refs=v["result"]["data"]["contentfulPageGuideline"]["richtext"]["references"].as_array_mut().unwrap();let entry=refs.iter_mut().find(|r|r["__typename"]=="ContentfulWidgetTable").unwrap();entry["__typename"]=json!("UnsupportedWidget");assert!(content(&v.to_string(),&page).is_err());
 }

 #[test] fn source_links_preserve_fragments_and_reject_unsafe_targets(){
  let page=Url::parse(URL).unwrap();let mut assets=vec![];
  let n=json!({"nodeType":"hyperlink","data":{"uri":"../other/#treatment"},"content":[{"nodeType":"text","value":"treatment"}]});
  let html=render(&n,&[],&page,&mut assets,"richtext/content/0").unwrap();
  assert!(html.contains("/guidelines/other/#treatment"));assert!(assets[0]["url"].as_str().unwrap().ends_with("/guidelines/other/#treatment"));
  render(&n,&[],&page,&mut assets,"references/content/0").unwrap();assert_eq!(assets[1]["role"],"reference_link");assert!(assets[1]["url"].as_str().unwrap().ends_with("#treatment"));
  let node=json!({"nodeType":"embedded-entry-block","data":{"target":{"sys":{"id":"test"}}}});
  let refs=vec![json!({"contentful_id":"test","__typename":"ContentfulWidgetTable","test":{"test":"<table><tr><td><a href=\"https://example.org/path#dose\">dose</a></td></tr></table>"}})];
  render(&node,&refs,&page,&mut assets,"richtext/content/1").unwrap();assert_eq!(assets[2]["role"],"table_link");assert_eq!(assets[2]["url"],"https://example.org/path#dose");
  for bad in ["javascript:alert(1)","https://user@example.org/x", "https://user:password@example.org/x"] {assert!(source_link(&page,bad).is_none());}
 }

 #[tokio::test] async fn genuine_sitemap_discovery_extract_and_revision_guard(){
  const INDEX:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/sitemap-index.xml"));
  const C1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/sitemap-child-1.xml"));
  const C2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/sitemap-child-2.xml"));
  let child=format!("{C1}{C2}");assert_eq!(crate::common::sha256_hex(child.as_bytes()),"40569bce92def7ac014ef6e64e1937366e3df165e31002aa02ede6d6ea218d2b");
  let child_url="https://www.starship.org.nz/sitemap-0.xml";assert_eq!(sitemap(INDEX).unwrap(),vec![child_url]);
  let mut seen=HashSet::new();let candidates=sitemap(&child).unwrap().into_iter().filter(|u|Url::parse(u).ok().and_then(|p|page_data_url(&p).ok()).is_some()&&seen.insert(u.clone())).collect::<Vec<_>>();
  let target=candidates.iter().position(|u|u==URL).expect("genuine child must list acute guideline");
  let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,2<<20,"").unwrap();
  f.seed_cache_for_tests(SITEMAP,INDEX.as_bytes(),"application/xml");f.seed_cache_for_tests(child_url,child.as_bytes(),"application/xml");
  let data=page_data_url(&Url::parse(URL).unwrap()).unwrap();f.seed_cache_for_tests(&data,A.as_bytes(),"application/json");
  let src:SourceEntry=serde_json::from_value(json!({"id":"nz_starship","access":"public","enabled":true})).unwrap();
  let mut rows=NzStarship.discover(&src,&f,Some(target+1)).await.unwrap();assert_eq!(rows.len(),target+1);
  let mut row=rows.pop().unwrap();assert_eq!(row.url,URL);assert!(row.extra["clinical_sha256"].is_string());
  let doc=NzStarship.extract(&src,&f,&row).await.unwrap();assert_eq!(doc.title,"Asthma and wheeze, management of acute");assert!(doc.blocks.iter().any(|b|b.text.contains("Steroid treatment should be given")));assert_eq!(doc.fetches.len(),3);
  row.extra["clinical_sha256"]=json!("incorrect revision");assert!(NzStarship.extract(&src,&f,&row).await.unwrap_err().to_string().contains("revision changed"));
 }

 #[test] fn genuine_clinical_images_and_asset_links_keep_source_context(){
  const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/coeliac.json"));
  const K:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/kawasaki.json"));
  const N:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/concussion.json"));
  let page=Url::parse("https://www.starship.org.nz/guidelines/coeliac-disease-assessment-and-management/").unwrap();let (_,_,_,blocks,_,assets,_)=content(C,&page).unwrap();
  assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),caption:Some(c)} if u.contains("Coeliac_Pathway_Diagram_Updated.png")&&c=="Coeliac Diagnosis Flowchart")));
  assert!(assets.iter().any(|a|a["role"]=="figure"&&a["label"]=="Coeliac Diagnosis Flowchart"&&a["contentful_id"]=="AolJD3TEoHHdTIXIRkBL0"));assert!(blocks.iter().any(|b|b.text.contains("coeliac serology can be falsely positive")));
  let page=Url::parse("https://www.starship.org.nz/guidelines/kawasaki-disease/").unwrap();let (_,_,_,blocks,_,assets,_)=content(K,&page).unwrap();assert_eq!(assets.iter().filter(|a|a["role"]=="figure").count(),2);assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{caption:Some(c),..} if c=="kawasaki criterion")));
  let page=Url::parse("https://www.starship.org.nz/guidelines/concussion/").unwrap();let (_,_,_,_,_,assets,_)=content(N,&page).unwrap();let links=assets.iter().filter(|a|a["role"]=="asset_link").collect::<Vec<_>>();assert_eq!(links.len(),4);assert!(links.iter().any(|a|a["label"]=="Concussion Service child or adolescent referral"&&a["url"].as_str().unwrap().ends_with(".pdf")));
 }
 #[test] fn genuinely_unmapped_embedded_clinical_files_fail_closed(){
  const E:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/empyema.json"));const I:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ivn.json"));
  let page=Url::parse("https://www.starship.org.nz/guidelines/empyema-clinical-pathway/").unwrap();assert!(content(E,&page).unwrap_err().to_string().contains("ContentfulWidgetEmbedded"));
  let page=Url::parse("https://www.starship.org.nz/guidelines/intravenous-nutrition-ivn-for-children-and-young-people/").unwrap();let (_,_,_,blocks,warnings,assets,g)=content(I,&page).unwrap();
  assert!(assets.iter().any(|a|a["role"]=="contact_link"&&a["url"]=="mailto:nznifrs@tewhatuora.govt.nz"&&a["source_locator"].as_str().unwrap().contains("content/110")));
  let tables=g["_source_table_mappings"].as_array().unwrap();let table=tables.iter().find(|t|t["contentful_id"]=="3h3j8cGqjh2WoSCabbVq0b").unwrap();let image=&table["images"][0];assert_eq!(image["source_row"],3);assert_eq!(image["source_cell"],2);assert!(table["original_html"].as_str().unwrap().contains("data:image/"));
  let id=image["figure_block_id"].as_str().unwrap();assert!(blocks.iter().any(|b|b.block_id==id&&matches!(b.kind,BlockKind::Figure{src_url:None,..})));assert!(blocks.iter().any(|b|b.block_id==table["block_id"].as_str().unwrap()&&b.text.contains(id)));assert!(warnings.iter().any(|w|w.contains("not been transcribed")));
 }

 #[test] fn genuine_efast_keeps_all_twelve_source_table_images(){
  const E1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/efast-1.json"));const E2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/efast-2.json"));
  let raw=format!("{E1}{E2}");let page=Url::parse("https://www.starship.org.nz/guidelines/trauma-efast/").unwrap();let (_,_,_,blocks,_,_,g)=content(&raw,&page).unwrap();let tables=g["_source_table_mappings"].as_array().unwrap();let mapped=tables.iter().flat_map(|t|t["images"].as_array().into_iter().flatten()).collect::<Vec<_>>();assert_eq!(mapped.len(),12);
  for image in mapped {let id=image["figure_block_id"].as_str().unwrap();assert!(blocks.iter().any(|b|b.block_id==id&&matches!(b.kind,BlockKind::Figure{..})));assert_eq!(image["source_storage"],"original_html data URI");}
 }

 #[test] fn genuine_diagram_files_preserve_graph_topology_geometry_and_embedded_images(){
  const R:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/referral.html"));const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ced.html"));
  const W1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ward-1.html"));const W2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ward-2.html"));
  let (xml,cells,edges)=graph_file(R).unwrap();assert_eq!(cells,33);assert_eq!(edges,2);assert!(xml.contains("data:image/png"));assert!(xml.contains("mxGeometry"));let doc=roxmltree::Document::parse(&xml).unwrap();assert!(doc.descendants().any(|n|n.has_tag_name("mxCell")&&n.attribute("source")==Some("21")&&n.attribute("target")==Some("28")));
  let (xml,cells,edges)=graph_file(C).unwrap();assert_eq!(cells,91);assert_eq!(edges,17);assert!(xml.contains("<mxCell"));assert!(xml.contains("&lt;div"));
  let (_,cells,edges)=graph_file(&format!("{W1}{W2}")).unwrap();assert_eq!(cells,59);assert_eq!(edges,2);assert!(graph_file("<div class='mxgraph' data-mxgraph='{}'></div>").is_err());
 }
 #[tokio::test] async fn missing_any_embedded_source_panel_stays_unresolved(){
  const E:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/empyema.json"));const R:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/referral.html"));const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ced.html"));const W1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ward-1.html"));const W2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ward-2.html"));
  let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,2<<20,"").unwrap();
  f.seed_cache_for_tests("https://assets.ctfassets.net/uszvm6pv23md/3KshK95xjzeDvFkGJBsmJB/d4ef0793229c45a3ae223c4abd1798b8/Empyema_pathway__final_3_October_2026-Referral_to_Starship.txt",R.as_bytes(),"text/html");
  f.seed_cache_for_tests("https://assets.ctfassets.net/uszvm6pv23md/2mApZnYFKagYL2VC3GZBC5/0874129e6c765c016a5278b02b25bac6/emp-CED.txt",C.as_bytes(),"text/html");
  f.seed_cache_for_tests("https://assets.ctfassets.net/uszvm6pv23md/28RYTsYPnGCOuG2kxlKtkb/7fece74a2b4f4e349f399537e944ef10/Empyema_pathway__final_3_October_2026-Admission_ward.txt",format!("{W1}{W2}").as_bytes(),"text/html");
  let url=Url::parse("https://www.starship.org.nz/guidelines/empyema-clinical-pathway/").unwrap();assert!(resolve_embedded(E,&url,&f).await.unwrap_err().to_string().contains("embedded clinical panel unavailable"));
 }

 #[tokio::test] async fn genuine_seven_panel_pathway_preserves_every_graph_and_revision(){
  const E:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/empyema.json"));const R:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/referral.html"));const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ced.html"));const W1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ward-1.html"));const W2:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/ward-2.html"));
  let surgery=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/surgery-1.html"))].concat();let postop=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/postop-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/postop-2.html"))].concat();let discharge=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/discharge-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/discharge-2.html"))].concat();let glossary=[include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/glossary-1.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/glossary-2.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/glossary-3.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/glossary-4.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/glossary-5.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/glossary-6.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/glossary-7.html")),include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/glossary-8.html"))].concat();let ward=format!("{W1}{W2}");
  let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,2<<20,"").unwrap();
  for (url,raw) in [
   ("https://assets.ctfassets.net/uszvm6pv23md/3KshK95xjzeDvFkGJBsmJB/d4ef0793229c45a3ae223c4abd1798b8/Empyema_pathway__final_3_October_2026-Referral_to_Starship.txt",R),
   ("https://assets.ctfassets.net/uszvm6pv23md/2mApZnYFKagYL2VC3GZBC5/0874129e6c765c016a5278b02b25bac6/emp-CED.txt",C),
   ("https://assets.ctfassets.net/uszvm6pv23md/28RYTsYPnGCOuG2kxlKtkb/7fece74a2b4f4e349f399537e944ef10/Empyema_pathway__final_3_October_2026-Admission_ward.txt",ward.as_str()),
   ("https://assets.ctfassets.net/uszvm6pv23md/6BAKZIvoHdr5BNgmTBViZJ/89290ec6479dc3214cdf99fdd173b89c/Empyema_pathway__final_3_October_2026-Surgical_planning.txt",surgery.as_str()),
   ("https://assets.ctfassets.net/uszvm6pv23md/XHTQxDFbDXVjWCjZiKqJg/7aa5a51beafc441f152cc2216d2583ad/Empyema_pathway__final_3_October_2026-Post_Op___Drain_management__1_.txt",postop.as_str()),
   ("https://assets.ctfassets.net/uszvm6pv23md/7J6wgNVuKBZ85Rt0KzWWNb/2b466ca72339303441cc982be1575dde/Empyema_pathway__final_3_October_2026-Clinical_improvement_and_Discharge_planning.txt",discharge.as_str()),
   ("https://assets.ctfassets.net/uszvm6pv23md/7qUT1Ue7NTR7r9bQmEhQy8/5611904c1e9713647d815347f60bc4a6/Empyema_pathway__final_3_October_2026__1_.txt",glossary.as_str())] {f.seed_cache_for_tests(url,raw.as_bytes(),"text/plain");}
  let url=Url::parse("https://www.starship.org.nz/guidelines/empyema-clinical-pathway/").unwrap();let data_url=page_data_url(&url).unwrap();f.seed_cache_for_tests(&data_url,E.as_bytes(),"application/json");let (resolved,records)=resolve_embedded(E,&url,&f).await.unwrap();assert_eq!(records.len(),7);let (_,hash,_,blocks,_,assets,g)=content(&resolved,&url).unwrap();let graphs=g["_source_embedded_graphs"].as_array().unwrap();assert_eq!(graphs.len(),7);assert!(graphs.iter().any(|x|x["title"]=="Glossary"&&x["cell_count"]==426&&x["edge_count"]==47));assert!(graphs.iter().all(|x|x["graph_xml"].as_str().unwrap().contains("mxGeometry")));assert_eq!(assets.iter().filter(|x|x["role"]=="embedded_clinical_graph").count(),7);assert_eq!(blocks.iter().filter(|b|b.block_id.starts_with("starship-graph-")&&matches!(b.kind,BlockKind::Figure{src_url:None,..})).count(),7);assert!(blocks.iter().any(|b|b.block_id.starts_with("starship-graph-")&&matches!(b.kind,BlockKind::Paragraph)));
  let src:SourceEntry=serde_json::from_value(json!({"id":"nz_starship","access":"public","enabled":true})).unwrap();let mut row=PlanRow{source_id:src.id.clone(),doc_key:"source-test".into(),url:url.to_string(),title:None,rendition:Some("json".into()),version:None,extra:json!({"page_data_url":data_url,"clinical_sha256":hash})};let doc=NzStarship.extract(&src,&f,&row).await.unwrap();assert_eq!(doc.item_type.as_deref(),Some("clinical_pathway"));assert_eq!(doc.search_metadata["source_embedded_graphs"].as_array().unwrap().len(),7);assert_eq!(doc.fetches.len(),8);row.extra["clinical_sha256"]=json!("changed");assert!(NzStarship.extract(&src,&f,&row).await.unwrap_err().to_string().contains("revision changed"));
 }

}

#[cfg(all(test, feature = "guideline-fixtures"))]mod embedded_scope_tests{use super::*;#[test]fn genuine_nested_tables_and_figures_keep_conditions_and_source_ids(){let spinal=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/spinal.json"));let lacerations=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_starship/lacerations.json"));let u=Url::parse("https://www.starship.org.nz/guidelines/spinal-cord-injury-assessment-and-acute-management/").unwrap();let(_,_,_,b,_,_,m)=content(spinal,&u).unwrap();assert!(b.iter().any(|b|b.text.contains("40mL/kg")&&b.text.contains("5wnUjAr0495W4FXAxSaJL")));assert!(m["_source_table_mappings"].as_array().unwrap().iter().any(|t|t["contentful_id"]=="5wnUjAr0495W4FXAxSaJL"));let u=Url::parse("https://www.starship.org.nz/guidelines/lacerations-and-wound-closure/").unwrap();let(_,_,_,b,_,assets,_)=content(lacerations,&u).unwrap();assert!(b.iter().any(|b|b.text.contains("Tendon assessment")&&b.text.contains("67Er8Vsr9ad2Jhjk4y4p0Z")));assert!(assets.iter().any(|a|a["contentful_id"]=="67Er8Vsr9ad2Jhjk4y4p0Z"&&a["role"]=="figure"));}}
