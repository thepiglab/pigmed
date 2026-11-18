//! NCI PDQ professional evidence summaries; source explicitly distinguishes these from CPGs.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::{HashSet,VecDeque};
use url::Url;
pub struct NciPdq;
const ARTICLE:&str="main#main-content > article";
const BODY:&str="main#main-content > article #cgvBody .summary-sections > .accordion";
fn escape(s:&str)->String{s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;")}
// Fetch identity deliberately drops fragments in common::link. Clinical anchors are
// locators, so preserve them here and keep every source mention independently.
fn clinical_link(base:&Url,href:&str)->Option<String>{
    let url=base.join(href.trim()).ok()?;
    (matches!(url.scheme(),"http"|"https")&&url.username().is_empty()&&url.password().is_none()).then(||url.to_string())
}
/// Tables, figures and bare images that belong to this list item itself (not to a nested list, not inside
/// another table/figure), in document order.
fn own_media<'a>(li:ElementRef<'a>)->Vec<ElementRef<'a>>{
    li.select(&sel("table,figure,img")).filter(|m|{
        for p in m.ancestors().filter_map(ElementRef::wrap){
            if p.id()==li.id(){return true;}
            if matches!(p.value().name(),"li"|"table"|"figure"|"ul"|"ol"){return false;}
        }
        false
    }).collect()
}
fn list_nodes(list:ElementRef<'_>,base:&Url,key:&str,parent_key:Option<&str>,parents:&[String],records:&mut Vec<Value>,lift:bool,notes:&mut Vec<String>)->Result<String>{
    let mut rendered=String::new();
    for (position,li) in list.children().filter_map(ElementRef::wrap).filter(|e|e.value().name()=="li").enumerate(){
        let node_key=format!("{key}/item-{}",position+1);
        // Start from actual descendant lists and explicitly exclude self. This is
        // independent of whether ego-tree's ancestors iterator includes its start node.
        let children=li.select(&sel("ul,ol")).filter(|nested|{
            nested.ancestors().filter_map(ElementRef::wrap).find(|p|p.value().name()=="li").is_some_and(|p|p.id()==li.id())
        }).collect::<Vec<_>>();
        // Professional summaries: a table or figure inside a list item is kept as its own block at its source
        // position (after the item's text, in document order with the item's nested lists).
        let media=if lift{own_media(li)}else{vec![]};
        let mut own_html=li.inner_html();for child in &children{own_html=own_html.replacen(&child.html(),"",1);}
        for m in &media{own_html=own_html.replacen(&m.html(),"",1);}
        let fragment=Html::parse_fragment(&own_html);let own_text=text_of(&fragment.root_element());
        let links=fragment.select(&sel("a[href]")).map(|a|json!({"label":text_of(&a),"raw_href":a.value().attr("href"),"target_url":a.value().attr("href").and_then(|v|clinical_link(base,v)),"source_id":a.value().id()})).collect::<Vec<_>>();
        let mut record=json!({"structural_key":node_key,"rendered_block_id":node_key,"parent_key":parent_key,"position":position+1,"source_item_id":li.value().id(),"source_list_id":list.value().id(),"list_kind":list.value().name(),"list_start":list.value().attr("start"),"list_type":list.value().attr("type"),"item_value":li.value().attr("value"),"own_text":own_text,"own_html":own_html,"parent_text_path":parents,"links":links});
        if !media.is_empty(){
            record["lifted_media"]=json!(media.iter().map(|m|json!({"tag":m.value().name(),"source_id":m.value().id(),"caption":m.select(&sel("caption,figcaption")).next().map(|c|text_of(&c))})).collect::<Vec<_>>());
        }
        records.push(record);
        let mut context=parents.to_vec();if !own_text.is_empty(){context.push(own_text.clone());}
        // Each child remains a distinct block, with its source parent condition in
        // the same retrievable text. Original tree/text/links are retained separately.
        if !own_text.is_empty(){rendered.push_str(&format!("<li id=\"{}\">{}</li>",escape(&node_key),escape(&context.join("\n"))));}
        // Document order of this item's nested lists and lifted media.
        let order=li.descendants().enumerate().map(|(n,d)|(d.id(),n)).collect::<std::collections::HashMap<_,_>>();
        let mut parts:Vec<(usize,Option<usize>,Option<ElementRef<'_>>)>=children.iter().enumerate().map(|(n,c)|(order[&c.id()],Some(n),None)).collect();
        parts.extend(media.iter().map(|m|(order[&m.id()],None,Some(*m))));parts.sort_by_key(|p|p.0);
        for (_,child,m) in parts{
            if let Some(n)=child{
                rendered.push_str(&list_nodes(children[n],base,&format!("{node_key}/list-{}",n+1),Some(&node_key),&context,records,lift,notes)?);
            }else if let Some(m)=m{
                // The contextual rendering is a flat <ul>: close it around the source media markup.
                rendered.push_str(&format!("</ul>{}<ul>",m.html()));
                let label=m.select(&sel("caption,figcaption")).next().map(|c|text_of(&c)).filter(|t|!t.is_empty()).map(|t|format!(" \"{t}\"")).unwrap_or_default();
                notes.push(format!("PDQ {} {}{label} sits inside nested clinical list item {node_key}; it is kept as its own block right after that item's text, so its list nesting is not carried on the block (source tree in search_metadata.source_metadata.clinical_lists).",m.value().name(),m.value().id().unwrap_or("(no id)")));
            }
        }
    }Ok(rendered)
}
fn clinical_lists(root:ElementRef<'_>,base:&Url,selected:&mut String,patient_manifestation:bool,notes:&mut Vec<String>)->Result<Vec<Value>>{
    let mut groups=vec![];
    for list in root.select(&sel("ul,ol")){
        if list.ancestors().filter_map(ElementRef::wrap).any(|p|p.id()!=list.id()&&matches!(p.value().name(),"ul"|"ol"|"nav"|"table")){continue;}
        if !list.select(&sel("ul,ol")).any(|e|e.id()!=list.id()){continue;}
        let has_media=list.select(&sel("figure,table,img")).next().is_some();
        // Professional summaries lift media out of the list (see list_nodes); patient pages keep original markup.
        let lift=has_media&&!patient_manifestation;
        let key=list.value().id().map(String::from).unwrap_or_else(||format!("clinical-list-{}",groups.len()+1));let mut nodes=vec![];
        let rendered=list_nodes(list,base,&key,None,&[],&mut nodes,lift,notes)?;
        let contextual=!has_media||lift;
        groups.push(json!({"source_list_id":list.value().id(),"structural_key":key,"source_html":list.html(),"nodes":nodes,"contextual_rendering":contextual,"representation":if lift{"contextual clinical list items; tables/figures inside items kept as separate blocks at their source position"}else if has_media{"original patient markup and separate source tree; not professional ingestion"}else{"contextual clinical list items"}}));
        // Patient diagnostic lists embed figures among their children (ALL _20).
        // Retain their entire original markup, IDs, order and media, plus the
        // explicit tree above. They are audience inspection, never professional ingestion.
        if contextual{*selected=selected.replacen(&list.html(),&format!("<div class='nci-contextual-list'><ul>{rendered}</ul></div>"),1);}
    }Ok(groups)
}
/// Category indexes are either PDQ articles or NCI landing pages ("PDQ® Cancer Information Summaries: Supportive
/// and Palliative Care", 2026-11-17) whose content blocks list each summary with its audience links. Only the
/// landing content blocks are used, never the site navigation.
fn category_root<'a>(doc:&'a Html)->Result<ElementRef<'a>>{
    if let Some(a)=doc.select(&sel(ARTICLE)).next(){return Ok(a);}
    let main=doc.select(&sel("main#main-content")).next().context("PDQ catalog article missing")?;
    let heading=main.select(&sel("h1")).next().map(|e|text_of(&e)).unwrap_or_default();
    if !heading.starts_with("PDQ® Cancer Information Summaries"){bail!("PDQ catalog article missing (landing heading {heading:?} not a PDQ summary index)");}
    let blocks=main.select(&sel(".cgdp-landing-content-block")).count();
    if blocks!=1{bail!("PDQ landing index has {blocks} content blocks; expected exactly one");}
    Ok(main.select(&sel(".cgdp-landing-content-block")).next().unwrap())
}
fn indexes(raw:&str,base:&Url)->Result<Vec<String>>{
    let doc=Html::parse_document(raw);
    // The genuine hub is an NCI landing page with no article; category indexes
    // are articles. Validate the exact hub role rather than broadening every root.
    let root=if base.path()=="/publications/pdq/information-summaries"{
        let main=doc.select(&sel("main#main-content")).next().context("PDQ catalog main missing")?;
        if main.select(&sel("h1")).next().map(|e|text_of(&e)).as_deref()!=Some("PDQ® Cancer Information Summaries for Health Professionals"){bail!("PDQ professional catalog heading missing");}
        main.select(&sel(".cgdp-list")).find(|e|e.select(&sel("h2.cgdp-list__title")).next().is_some_and(|h|text_of(&h)=="Cancer Information Summaries")).context("PDQ catalog summary collection missing")?
    }else{category_root(&doc)?};
    let mut seen=HashSet::new();let mut out=vec![];
    for a in root.select(&sel("a[href]")){
        let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let parsed=Url::parse(&u)?;
        if parsed.host_str()==base.host_str()&&parsed.path().starts_with("/publications/pdq/information-summaries/")&&parsed.query().is_none()&&seen.insert(u.clone()){out.push(u);}
    }Ok(out)
}
fn topics(raw:&str,base:&Url)->Result<Vec<(String,String)>>{
    if base.path()=="/publications/pdq/information-summaries"{indexes(raw,base)?;return Ok(vec![]);}
    let doc=Html::parse_document(raw);let root=category_root(&doc)?;
    let mut seen=HashSet::new();let mut out=vec![];
    for a in root.select(&sel("a[href]")){
        // The index explicitly labels audience. A professional route need not contain /hp/.
        if !text_of(&a).eq_ignore_ascii_case("health professional"){continue;}
        let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let parsed=Url::parse(&u)?;
        if parsed.host_str()!=base.host_str()||parsed.query().is_some()||!seen.insert(u.clone()){continue;}
        let hint=a.ancestors().filter_map(ElementRef::wrap).filter(|e|e.value().name()=="li").find_map(|e|{
            let t=e.children().filter_map(|n|n.value().as_text()).map(|t|t.to_string()).collect::<String>();let t=t.trim();(!t.is_empty()&&t!="|").then(||t.to_string())
        })
        // Landing-page indexes print the summary title in <strong> within the same paragraph as its audience links.
        .or_else(||a.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().name()=="p").and_then(|p|p.select(&sel("strong")).next()).map(|s|text_of(&s)).filter(|t|!t.is_empty()))
        .unwrap_or_else(||text_of(&a));out.push((u,hint));
    }Ok(out)
}
struct Page{title:String,html:String,audience:String,language:Option<String>,dates:Vec<DateObservation>,rights:Rights,metadata:Value,assets:Vec<Value>,notes:Vec<String>}
fn page(raw:&str,base:&Url)->Result<Page>{
    let doc=Html::parse_document(raw);
    let declared=doc.select(&sel("meta[name='dcterms.type']")).next().and_then(|e|e.value().attr("content"));
    if declared!=Some("pdqCancerInfoSummary"){bail!("NCI item is not source-declared PDQ cancer information summary");}
    let audience=doc.select(&sel("meta[name='dcterms.audience']")).next().and_then(|e|e.value().attr("content")).context("PDQ audience missing")?.to_string();
    if !matches!(audience.as_str(),"Health Professionals"|"Patients"){bail!("PDQ unvalidated audience template: {audience}");}
    let root=doc.select(&sel(BODY)).collect::<Vec<_>>();if root.len()!=1{bail!("PDQ expected exactly one complete summary body, found {}",root.len());}let root=root[0];
    let ids=root.select(&sel("[id]")).filter_map(|e|e.value().id().map(String::from)).collect::<HashSet<_>>();
    let mut expected_sections=vec![];
    for a in doc.select(&sel("main#main-content > article #cgvBody .summary-sections > nav.on-this-page a[href^='#']")){
        let id=a.value().attr("href").unwrap().trim_start_matches('#');
        if !ids.contains(id){bail!("PDQ full summary lacks source TOC section {id}; incomplete content");}
        expected_sections.push(json!({"source_id":id,"title":text_of(&a)}));
    }
    if expected_sections.is_empty(){bail!("PDQ complete-summary TOC missing");}
    let title=common::first_text(&doc,"main#main-content > article h1").context("PDQ summary title missing")?;
    let purpose=root.select(&sel("section")).find(|s|s.select(&sel("h3")).next().is_some_and(|h|text_of(&h)=="Purpose of This Summary"));
    if audience=="Health Professionals"&&!purpose.is_some_and(|p|text_of(&p).contains("does not provide formal guidelines or recommendations")){bail!("PDQ professional evidence-summary role notice missing");}
    let mut dates=vec![];
    for li in doc.select(&sel("main#main-content > article footer .document-dates li")){
        let Some(label)=li.select(&sel("strong")).next().map(|e|text_of(&e))else{continue};
        let Some(time)=li.select(&sel("time")).next()else{continue};dates.push(common::date(label.trim_end_matches(':'),&text_of(&time),base.as_str()));
    }
    if !dates.iter().any(|d|d.label=="Updated"){bail!("PDQ source clinical Updated date missing");}
    let permission=root.select(&sel("section")).find(|s|s.select(&sel("h3")).next().is_some_and(|h|text_of(&h)=="Permission to Use This Summary")).context("PDQ per-summary permissions missing")?;
    let rights_text=text_of(&permission);let mut rights=common::unknown_rights();rights.status="restricted".into();rights.note=Some(rights_text.clone());
    rights.terms_url=doc.select(&sel("main#main-content > article footer .cgdp-public-use a[href]")).find(|a|text_of(a)=="Reuse of NCI Information").and_then(|a|common::link(base,a.value().attr("href")?));
    let mut rights_url=base.clone();rights_url.set_fragment(permission.value().id());rights.evidence_urls.push(rights_url.to_string());
    for p in permission.select(&sel("p")) {let t=text_of(&p);if t.contains("Bethesda")&&t.contains("Available at:"){rights.attribution=Some(t);}else if t.contains("PDQ is a registered trademark")||t.contains("Images in this summary"){rights.restrictions.push(t);}}
    let mut html=root.html();let mut assets=vec![];
    for e in root.select(&sel("nav, .pdq-hp-patient-toggle")){
        for a in e.select(&sel("a[href]")){if e.value().classes().any(|c|c=="pdq-hp-patient-toggle"){if let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default()){assets.push(json!({"url":u,"label":text_of(&a),"role":"other_audience_version","rights":"independent"}));}}}
        html=html.replacen(&e.html(),"",1);
    }
    // do-not-show=toc suppresses only TOC entries: the References themselves are essential.
    for h in root.select(&sel("h6[do-not-show='toc']")){if text_of(&h)=="References"{html=html.replacen(&h.html(),"<h3>References</h3>",1);}}
    let mut notes=vec![];let lists=clinical_lists(root,base,&mut html,audience=="Patients",&mut notes)?;
    let clinical_links=root.select(&sel("a[href]")).filter(|a|!a.ancestors().filter_map(ElementRef::wrap).any(|p|p.value().name()=="nav"||p.value().classes().any(|c|c=="pdq-hp-patient-toggle"))).map(|a|{
        let mut locators=a.ancestors().filter_map(ElementRef::wrap).filter(|p|matches!(p.value().name(),"section"|"ul"|"ol"|"li"|"p")).filter_map(|p|p.value().id().map(String::from)).collect::<Vec<_>>();locators.reverse();
        json!({"label":text_of(&a),"raw_href":a.value().attr("href"),"target_url":a.value().attr("href").and_then(|v|clinical_link(base,v)),"source_id":a.value().id(),"source_ancestor_ids":locators})
    }).collect::<Vec<_>>();
    let references=root.select(&sel("li[id]")).filter(|e|e.value().id().is_some_and(|id|id.starts_with("section_"))).map(|e|json!({"source_id":e.value().id(),"text":text_of(&e),"url":format!("{}#{}",base,e.value().id().unwrap())})).collect::<Vec<_>>();
    let reference_ids=references.iter().filter_map(|v|v["source_id"].as_str()).collect::<HashSet<_>>();
    let citations=root.select(&sel("a[href^='#cit/']")).map(|a|{
        let key=a.value().attr("href").and_then(|s|s.strip_prefix("#cit/"));let resolved=key.filter(|id|reference_ids.contains(id));
        json!({"label":text_of(&a),"raw_href":a.value().attr("href"),"source_reference_key":key,"reference_source_id":resolved,"resolved_in_body":resolved.is_some()})
    }).collect::<Vec<_>>();
    let mut figures=vec![];
    let mut tables=vec![];
    for table in root.select(&sel("table")){
        let permissions=table.select(&sel("tfoot")).map(|e|text_of(&e)).filter(|t|t.to_lowercase().contains("permission")||t.to_lowercase().contains("copyright")).collect::<Vec<_>>();
        for notice in &permissions{if !rights.restrictions.contains(notice){rights.restrictions.push(notice.clone());}}
        tables.push(json!({"source_id":table.value().id(),"caption":table.select(&sel("caption")).next().map(|e|text_of(&e)),"source_permission_notices":permissions,"reuse":"independent rights; no general text-policy inheritance"}));
    }
    for figure in root.select(&sel("figure")){
        let original=figure.select(&sel("a.article-image-enlarge[href]")).next().and_then(|a|common::link(base,a.value().attr("href")?));
        for img in figure.select(&sel("img")){
            figures.push(json!({"source_id":img.value().id(),"display_url":img.value().attr("src").and_then(|s|common::link(base,s)),"original_url":original,"alt":img.value().attr("alt"),"permission_notice":rights_url.as_str(),"reuse":"owner permission required outside PDQ context"}));
        }
    }
    let issued=doc.select(&sel("meta[name='dcterms.issued']")).next().and_then(|e|e.value().attr("content")).map(String::from);
    let clean=regex::Regex::new(r#"\s(?:data-list-item-id|data-pdffiller-[\w-]+)="[^"]*""#).unwrap();html=clean.replace_all(&html,"").into_owned();
    let comments=regex::Regex::new(r"(?s)<!--.*?-->").unwrap();html=comments.replace_all(&html,"").into_owned();
    Ok(Page{title:title.clone(),html:format!("<div id='nci-pdq-clinical'><h1>{}</h1>{html}</div>",escape(&title)),audience,language:common::page_language(&doc),dates,rights,assets,notes,metadata:json!({"web_metadata_issued":issued,"source_toc_sections":expected_sections,"references":references,"citations":citations,"tables":tables,"figures":figures,"clinical_lists":lists,"clinical_links":clinical_links})})
}
fn policy(raw:&str,url:&Url)->Result<Value>{
    let doc=Html::parse_document(raw);let body=doc.select(&sel("main#main-content > article .cgdp-article-body .usa-prose")).next().context("NCI reuse policy body missing")?;
    let mut sections=vec![];let mut label=String::new();let mut text=vec![];
    for e in body.children().filter_map(ElementRef::wrap){
        if e.value().name()=="h2"{
            if !label.is_empty(){sections.push(json!({"label":label,"text":text.join("\n")}));}text.clear();
            let heading=text_of(&e);label=if matches!(heading.as_str(),"Reuse of Text"|"Reuse of Graphics"|"Reuse of NCI Logo and PDQ Trademark"){heading}else{String::new()};
        }else if !label.is_empty(){text.push(text_of(&e));}
    }
    if !label.is_empty(){sections.push(json!({"label":label,"text":text.join("\n")}));}
    if sections.len()!=3{bail!("NCI reuse policy expected text, graphics and trademark sections");}
    let dates=doc.select(&sel("main#main-content > article footer .cgdp-document-dates li")).filter_map(|li|{
        let label=li.select(&sel("strong")).next().map(|e|text_of(&e))?;let time=li.select(&sel("time")).next()?;
        Some(common::date(label.trim_end_matches(':'),&text_of(&time),url.as_str()))
    }).collect::<Vec<_>>();
    Ok(json!({"source_url":url.as_str(),"scope":"general policy; per-summary and third-party notices remain independent","date_observations":dates,"sections":sections}))
}
fn span(cell:ElementRef<'_>,attr:&str)->Result<usize>{let n=cell.value().attr(attr).map(str::parse::<usize>).transpose()?.unwrap_or(1);if n==0||n>500{bail!("PDQ invalid table span");}Ok(n)}
fn clinical_tables(html:&str,blocks:&mut [Block])->Result<()>{
    let doc=Html::parse_fragment(html);let tables=doc.select(&sel("#nci-pdq-clinical table")).collect::<Vec<_>>();
    let ids=blocks.iter().enumerate().filter(|(_,b)|matches!(b.kind,BlockKind::Table{..})).map(|(i,_)|i).collect::<Vec<_>>();
    if tables.len()!=ids.len(){bail!("PDQ extracted/source table count mismatch");}
    for (i,table) in tables.iter().enumerate(){
        if table.select(&sel("table")).next().is_some(){bail!("PDQ nested clinical table requires validated mapping");}
        let rows=table.select(&sel("tr")).collect::<Vec<_>>();let mut width=0;
        for row in &rows{let mut w=0;for c in common::cells(*row){w+=span(c,"colspan")?;}width=width.max(w);}
        if width==0||width>64{bail!("PDQ unsupported clinical table width {width}");}
        let mut carry:Vec<Option<(String,usize)>>=vec![None;width];let mut grid=vec![];
        for row in rows{
            let mut values:Vec<Option<String>>=vec![None;width];
            for j in 0..width{if let Some((t,n))=carry[j].clone(){values[j]=Some(t.clone());carry[j]=if n>1{Some((t,n-1))}else{None};}}
            let mut col=0;
            for c in common::cells(row){while col<width&&values[col].is_some(){col+=1;}let cs=span(c,"colspan")?;let rs=span(c,"rowspan")?;
                if col+cs>width{bail!("PDQ clinical table spans exceed validated width");}let text=text_of(&c).replace('|',"\\|");
                for j in col..col+cs{if values[j].is_some(){bail!("PDQ overlapping table spans");}values[j]=Some(text.clone());if rs>1{carry[j]=Some((text.clone(),rs-1));}}col+=cs;
            }
            grid.push(values.into_iter().map(|v|v.unwrap_or_default()).collect::<Vec<_>>());
        }
        if carry.iter().any(Option::is_some){bail!("PDQ rowspan extends beyond source table");}
        let mut lines=vec![];for (n,row) in grid.into_iter().enumerate(){lines.push(format!("| {} |",row.join(" | ")));if n==0{lines.push(format!("| {} |",vec!["---";width].join(" | ")));}}
        blocks[ids[i]].text=lines.join("\n");
    }Ok(())
}
fn snapshot(p:&Page)->Result<String>{
    // Exact source HTML remains provenance metadata. Identity uses clinical hierarchy,
    // labels and targets, excluding known volatile editor attributes in raw markup.
    let nodes=p.metadata["clinical_lists"].as_array().into_iter().flatten().flat_map(|g|g["nodes"].as_array().into_iter().flatten()).map(|node|{
        let mut value=node.clone();if let Some(fields)=value.as_object_mut(){fields.remove("own_html");}value
    }).collect::<Vec<_>>();
    Ok(crate::common::sha256_hex(serde_json::to_string(&(&p.title,&p.html,&p.audience,&p.language,&p.dates,&p.rights,&p.assets,&nodes,&p.metadata["clinical_links"]))?.as_bytes()))
}
#[async_trait::async_trait]
impl Adapter for NciPdq{
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
        if limit==Some(0){return Ok(vec![]);}
        let start=src.catalog_url.clone().context("PDQ catalog missing")?;let mut queue=VecDeque::from([start]);let mut seen=HashSet::new();let mut emitted=HashSet::new();let mut resolved=HashSet::new();let mut out=vec![];
        while let Some(url)=queue.pop_front(){
            if !seen.insert(url.clone()){continue;}if seen.len()>30{bail!("PDQ category graph exceeds30source-emitted indexes");}
            let fetched=match f.get_ok(&url).await{Ok(x)=>x,Err(e)=>{
                if seen.len()==1{return Err(e);}
                let work=common::slug(Url::parse(&url)?.path());out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:url.clone(),title:None,rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"discovery_error":format!("PDQ category index fetch: {e:#}")})});
                if limit.is_some_and(|n|out.len()>=n){return Ok(out);}continue;}};
            let base=Url::parse(&fetched.record.final_url)?;
            // The hub must parse; an unrecognised category index is recorded as an unresolved row (audited
            // at ingest) instead of aborting discovery of every other category.
            let parsed=indexes(&fetched.text(),&base).and_then(|i|Ok((i,topics(&fetched.text(),&base)?)));
            let (next_indexes,found)=match parsed{Ok(v)=>v,Err(e)=>{
                if base.path()=="/publications/pdq/information-summaries"{return Err(e);}
                let work=common::slug(base.path());out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:url.clone(),title:None,rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"discovery_error":format!("PDQ category index: {e:#}")})});
                if limit.is_some_and(|n|out.len()>=n){return Ok(out);}continue;}};
            for u in next_indexes{if !seen.contains(&u){queue.push_back(u);}}
            for (url,hint) in found{
                if !emitted.insert(url.clone()){continue;}let work=common::slug(Url::parse(&url)?.path());
                let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url,title:Some(hint),rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"catalog_fetch":fetched.record})};
                let result=async{let item=f.get_ok(&row.url).await?;let base=Url::parse(&item.record.final_url)?;let p=page(&item.text(),&base)?;
                    if p.audience!="Health Professionals"{bail!("PDQ index professional link resolved to other audience");}let hash=snapshot(&p)?;Ok::<_,anyhow::Error>((p.title,hash,base))}.await;
                match result{Ok((title,hash,base))=>{if !resolved.insert(base.to_string()){continue;}let work=common::slug(base.path());row.extra["source_index_url"]=json!(row.url);row.url=base.to_string();row.extra["work_key"]=json!(work);row.title=Some(title);row.doc_key=format!("{work}-{}",&hash[..12]);row.extra["clinical_sha256"]=json!(hash);},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
                out.push(row);if limit.is_some_and(|n|out.len()>=n){return Ok(out);}
            }
        }if out.is_empty(){bail!("PDQ category graph yielded no source-labelled professional summaries");}Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("PDQ discovery: {e}");}
        let fetched=f.get_ok(&row.url).await?;let base=Url::parse(&fetched.record.final_url)?;let mut p=page(&fetched.text(),&base)?;
        if p.audience!="Health Professionals"{bail!("PDQ source scope requires Health Professionals; patient edition retained only as separate-audience link");}
        if row.extra["clinical_sha256"].as_str().is_some_and(|expected|snapshot(&p).map(|v|v!=expected).unwrap_or(true)){bail!("PDQ clinical revision changed after planning; re-plan");}
        let (mut blocks,mut warnings)=blocks_from_html(&p.html,Some(&base),Some("#nci-pdq-clinical"));clinical_tables(&p.html,&mut blocks)?;warnings.extend(p.notes.iter().cloned());
        warnings.push("PDQ is a source-declared evidence summary, not a formal practice guideline. Levels of evidence, adverse-event grades and treatment-option wording are not mapped to recommendation strength. Figure reuse requires separate owner permission outside the PDQ context. Patient and translated editions remain separate manifestations; current status and rendition parity are not inferred.".into());
        let mut fetches=vec![fetched.record];if let Ok(record)=serde_json::from_value(row.extra["catalog_fetch"].clone()){fetches.insert(0,record);}
        if let Some(url)=p.rights.terms_url.clone(){
            match f.get_ok(&url).await{
                Ok(response)=>{
                    let policy_url=Url::parse(&response.record.final_url)?;
                    match policy(&response.text(),&policy_url){Ok(value)=>{p.metadata["general_reuse_policy"]=value;p.rights.evidence_urls.push(policy_url.to_string());},Err(e)=>warnings.push(format!("General NCI reuse policy could not be mapped: {e:#}; exact per-summary permission notices remain retained."))}
                    fetches.push(response.record);
                },Err(e)=>warnings.push(format!("Linked NCI reuse policy not acquired: {e:#}; no general text permission inferred for third-party assets."))
            }
        }
        let publisher=p.rights.attribution.as_ref().filter(|s|s.contains("Bethesda")&&s.contains("National Cancer Institute")).map(|_|"National Cancer Institute".to_string());
        let pmid=regex::Regex::new(r"\[PMID:\s*(\d+)\]").unwrap().captures(p.rights.attribution.as_deref().unwrap_or_default()).map(|c|c[1].to_string());
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:p.title,url:base.to_string(),item_type:Some("evidence_summary".into()),status:"unknown".into(),publisher,pmid,language:p.language,updated:p.dates.iter().find(|d|d.label=="Updated").and_then(|d|d.normalized.clone()),date_observations:p.dates,rights:p.rights,blocks,attachments:p.assets,search_metadata:json!({"audience":p.audience,"source_metadata":p.metadata}),fetches,warnings,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests{
    use super::*;
    const HUB:&str="https://www.cancer.gov/publications/pdq/information-summaries";
    const INDEX:&str="https://www.cancer.gov/publications/pdq/information-summaries/adult-treatment";
    const HP:&str="https://www.cancer.gov/types/leukemia/hp/adult-all-treatment-pdq";
    const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/catalog.html"));
    const I:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/adult-treatment-index.html"));
    const H:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/adult-all.html"));
    const P:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/patient-all.html"));
    const POLICY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/copyright.html"));
    #[tokio::test]
    async fn genuine_catalog_professional_audience_and_full_clinical_summary(){
        let categories=indexes(C,&Url::parse(HUB).unwrap()).unwrap();assert_eq!(categories.len(),7);assert!(categories.contains(&INDEX.to_string()));
        assert!(!C.contains("<article>")); // Genuine hub landing template differs from category articles.
        assert_eq!(categories[0],INDEX);
        assert!(indexes(&C.replace("PDQ® Cancer Information Summaries for Health Professionals","Unrelated landing page"),&Url::parse(HUB).unwrap()).is_err());
        assert!(topics(C,&Url::parse(HUB).unwrap()).unwrap().is_empty());
        let entries=topics(I,&Url::parse(INDEX).unwrap()).unwrap();assert!(entries.len()>30);
        assert_eq!(entries.iter().filter(|(u,_)|u==HP).count(),1);
        assert!(entries.iter().any(|(u,_)|u.ends_with("agnostic-cancer-therapies-hp-pdq")));
        assert!(entries.iter().all(|(u,_)|!u.contains("/patient/")));
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        for (url,raw) in [(HUB,C),(INDEX,I),(HP,H)]{f.seed_cache_for_tests(url,raw.as_bytes(),"text/html");}
        f.seed_cache_for_tests("https://www.cancer.gov/policies/copyright-reuse",POLICY.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"nci_pdq","access":"public","enabled":true,"catalog_url":HUB})).unwrap();
        let rows=NciPdq.discover(&src,&f,Some(1)).await.unwrap();assert_eq!(rows[0].url,HP);
        let d=NciPdq.extract(&src,&f,&rows[0]).await.unwrap();
        assert_eq!(d.item_type.as_deref(),Some("evidence_summary"));assert_eq!(d.status,"unknown");assert_eq!(d.updated.as_deref(),Some("2025-03-17"));assert!(d.published.is_none()&&d.country.is_none());
        assert_eq!(d.search_metadata["source_metadata"]["web_metadata_issued"],"2026-10-28");
        assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),2);
        assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),1);
        assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})&&!b.text.contains("On This Page")&&!b.text.contains("In This Section")));
        assert!(d.blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u=="https://nci-media.cancer.gov/pdq/media/images/526538-750.jpg")));
        assert_eq!(d.search_metadata["source_metadata"]["figures"][0]["original_url"],"https://nci-media.cancer.gov/pdq/media/images/526538.jpg");
        let table=d.blocks.iter().find(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("Disease Status")).unwrap();
        assert_eq!(table.text.lines().filter(|s|s.starts_with("| Recurrent ALL |")).count(),6);
        assert!(table.text.contains("BMT = bone marrow transplant; CNS = central nervous system."));
        assert!(d.blocks.iter().any(|b|b.block_id=="section_7.12"&&b.heading_path.last().is_some_and(|s|s=="References")));
        assert!(d.blocks.iter().any(|b|b.text.contains("does not provide formal guidelines or recommendations")));
        assert!(d.blocks.iter().any(|b|b.text.contains("cannot be granted by the National Cancer Institute")));
        assert!(d.rights.attribution.as_deref().unwrap().contains("26389171"));
        assert!(d.rights.restrictions.iter().any(|t|t.contains("presented in its entirety")));
        assert!(d.attachments.iter().any(|a|a["role"]=="other_audience_version"&&a["url"].as_str().unwrap().contains("/patient/")));
        let before=page(H,&Url::parse(HP).unwrap()).unwrap();let header_only=H.replace("content=\"2026-10-28\"","content=\"2026-10-29\"");
        assert_eq!(snapshot(&before).unwrap(),snapshot(&page(&header_only,&Url::parse(HP).unwrap()).unwrap()).unwrap());
        assert!(page(&H.replace("<section id=\"_135\">","<section id=\"missing-source-section\">"),&Url::parse(HP).unwrap()).is_err());
        f.seed_cache_for_tests(HP,H.replace("New cases: 6,100.","New cases: 6,101.").as_bytes(),"text/html");assert!(NciPdq.extract(&src,&f,&rows[0]).await.is_err());
    }
    #[tokio::test]
    async fn genuine_patient_manifestation_is_rejected_from_professional_ingestion(){
        let url="https://www.cancer.gov/types/leukemia/patient/adult-all-treatment-pdq";
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(url,P.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"nci_pdq","enabled":true,"access":"public"})).unwrap();
        let row=PlanRow{source_id:"nci_pdq".into(),doc_key:"patient-fixture".into(),url:url.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({})};
        let error=NciPdq.extract(&src,&f,&row).await.err().unwrap();
        assert!(error.to_string().contains("source scope requires Health Professionals"));
    }
    #[test]
    fn genuine_patient_body_is_a_separate_manifestation(){
        let p=page(P,&Url::parse("https://www.cancer.gov/types/leukemia/patient/adult-all-treatment-pdq").unwrap()).unwrap();assert_eq!(p.audience,"Patients");
        let (blocks,_)=blocks_from_html(&p.html,None,Some("#nci-pdq-clinical"));assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),9);
        assert!(blocks.iter().any(|b|b.text=="Key Points"));
        assert_eq!(p.metadata["figures"].as_array().unwrap().len(),9);
        let diagnostic=p.metadata["clinical_lists"].as_array().unwrap().iter().find(|g|g["source_list_id"]=="_20").unwrap();
        assert_eq!(diagnostic["contextual_rendering"],false);
        let patient_dom=Html::parse_document(P);let source_root=patient_dom.select(&sel(BODY)).next().unwrap();
        let mut strict_html=source_root.html();
        let mut notes=vec![];let lifted=clinical_lists(source_root,&Url::parse("https://www.cancer.gov/types/leukemia/patient/adult-all-treatment-pdq").unwrap(),&mut strict_html,false,&mut notes).unwrap();
        assert_eq!(lifted.iter().find(|g|g["source_list_id"]=="_20").unwrap()["contextual_rendering"],true);assert!(!notes.is_empty());
        let original=diagnostic["source_html"].as_str().unwrap();
        for id in ["_80","_173","figure_136","figure_179","figure_180"]{assert!(original.contains(&format!("id=\"{id}\"")),"missing original diagnostic locator {id}");assert!(p.html.contains(&format!("id=\"{id}\"")));}
        let nodes=diagnostic["nodes"].as_array().unwrap();
        let cbc=nodes.iter().find(|n|n["source_list_id"]=="_80"&&n["position"]==1).unwrap();
        assert_eq!(cbc["parent_key"],"_20/item-1");
        assert!(cbc["parent_text_path"][0].as_str().unwrap().contains("Complete blood count (CBC) with differential"));
        assert_eq!(cbc["own_text"],"the number of red blood cells and platelets");
        let cyto=nodes.iter().find(|n|n["source_list_id"]=="_173"&&n["position"]==1).unwrap();
        assert_eq!(cyto["parent_key"],"_20/item-4");
        assert!(cyto["own_text"].as_str().unwrap().contains("Philadelphia chromosome"));
        assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),caption:Some(c)} if u=="https://nci-media.cancer.gov/pdq/media/images/533336-750.jpg"&&c.contains("fusion gene")&&c.contains("Ph chromosome"))));
        // Production keeps its professional role boundary even for a fully parsed
        // patient manifestation; source-labelled links do not confer role parity.
        assert!(blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod lung_tests{
    use super::*;
    fn lung()->String{[
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/lung-hp.part-1.html")),
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/lung-hp.part-2.html")),
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/lung-hp.part-3.html")),
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/lung-hp.part-4.html")),
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/lung-hp.part-5.html")),
    ].concat()}
    const POLICY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/copyright.html"));
    #[tokio::test]
    async fn genuine_second_professional_complete_tnm_tables_and_last_reference(){
        let raw=lung();assert_eq!(raw.len(),817101);assert_eq!(crate::common::sha256_hex(raw.as_bytes()),"2e16eb7e1784c93008f8ff113ddc3971ce86b69eae123cf9f7d73b7835f746de");
        let url="https://www.cancer.gov/types/lung/hp/non-small-cell-lung-treatment-pdq";
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,2<<20,"").unwrap();
        f.seed_cache_for_tests(url,raw.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.cancer.gov/policies/copyright-reuse",POLICY.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"nci_pdq","enabled":true,"access":"public"})).unwrap();
        let row=PlanRow{source_id:"nci_pdq".into(),doc_key:"lung-fixture".into(),url:url.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({})};
        let d=NciPdq.extract(&src,&f,&row).await.unwrap();
        assert_eq!(d.updated.as_deref(),Some("2025-05-15"));assert_eq!(d.date_observations.len(),1);assert!(d.published.is_none());assert_eq!(d.pmid.as_deref(),Some("26389304"));
        assert_eq!(d.search_metadata["source_metadata"]["source_toc_sections"].as_array().unwrap().len(),14);
        assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),5);assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),1);
        assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})));
        let t4=d.blocks.iter().find(|b|matches!(&b.kind,BlockKind::Table{caption:Some(c)} if c.starts_with("Table 4."))).unwrap();
        assert_eq!(t4.text.lines().filter(|l|l.starts_with("| IIIA |")).count(),9);
        let t5=d.blocks.iter().find(|b|matches!(&b.kind,BlockKind::Table{caption:Some(c)} if c.starts_with("Table 5."))).unwrap();
        assert_eq!(t5.text.lines().filter(|l|l.starts_with("| Stage IIIA NSCLC |")).count(),13);
        assert!(t5.text.lines().any(|l|l.starts_with("| Stage IIIA NSCLC | Unresectable disease |")&&l.contains("Chemoradiation therapy")));
        assert!(d.blocks.iter().any(|b|b.block_id=="section_12.69"&&b.heading_path.last().is_some_and(|h|h=="References")));
        assert!(d.blocks.iter().any(|b|b.text.contains("Latest Updates to This Summary (05/15/2025)")));
        assert!(d.rights.restrictions.iter().any(|r|r.contains("Reprinted with permission from American Joint Committee on Cancer")&&r.contains("2025")));
        let tables=d.search_metadata["source_metadata"]["tables"].as_array().unwrap();assert_eq!(tables.len(),5);assert_eq!(tables.iter().filter(|t|!t["source_permission_notices"].as_array().unwrap().is_empty()).count(),4);
        let general=&d.search_metadata["source_metadata"]["general_reuse_policy"];
        assert_eq!(general["sections"].as_array().unwrap().len(),3);
        assert!(general["sections"][0]["text"].as_str().unwrap().contains("Unless otherwise indicated"));
        assert!(general["sections"][1]["text"].as_str().unwrap().contains("mix of copyrighted and copyright-free materials"));
        assert!(general["sections"][2]["text"].as_str().unwrap().contains("wholly unchanged NCI products"));
        assert!(d.rights.license.is_none());assert!(d.fetches.iter().any(|r|r.sha256=="84073a8bad11753ace5d81fc2bd17794a1588314f382b4abe4d420aedc4173ba"));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod clinical_context_tests{
    use super::*;
    const RAW:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/adult-all.html"));
    const HP:&str="https://www.cancer.gov/types/leukemia/hp/adult-all-treatment-pdq";
    #[tokio::test]
    async fn genuine_nested_treatment_children_keep_parent_structure_and_fragment_targets(){
        let base=Url::parse(HP).unwrap();
        {
            let source=Html::parse_document(RAW);let list=source.select(&sel("ol#_84")).next().unwrap();
            let first=list.children().filter_map(ElementRef::wrap).find(|e|e.value().name()=="li").unwrap();
            let self_seen=first.ancestors().any(|n|n.id()==first.id());
            let old_guard=first.ancestors().filter_map(ElementRef::wrap).any(|p|p.value().name()=="li"||p.value().name()=="nav");
            eprintln!("Genuine first remission item: ancestors_include_self={self_seen}; previous_flatten_guard_skips={old_guard}");
            // Observe the actual shared walker on genuine source HTML, rather than
            // assuming the old adapter flatten branch executed.
            let baseline=format!("<div id='baseline'>{}</div>",list.html());
            let (blocks,_)=blocks_from_html(&baseline,Some(&base),Some("#baseline"));
            assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::ListItem)&&b.text=="Combination chemotherapy."&&b.heading_path.is_empty()));
        }
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(HP,RAW.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"nci_pdq","access":"public","enabled":true})).unwrap();
        let row=PlanRow{source_id:"nci_pdq".into(),doc_key:"context-fixture".into(),url:HP.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({})};
        let d=NciPdq.extract(&src,&f,&row).await.unwrap();
        let metadata=&d.search_metadata["source_metadata"];
        let group=metadata["clinical_lists"].as_array().unwrap().iter().find(|g|g["source_list_id"]=="_84").unwrap();
        assert!(group["source_html"].as_str().unwrap().contains("id=\"_206\""));
        let nodes=group["nodes"].as_array().unwrap();
        let parent=nodes.iter().find(|n|n["own_text"].as_str().unwrap().starts_with("Remission induction therapy")).unwrap();
        assert_eq!(parent["source_list_id"],"_84");assert_eq!(parent["list_kind"],"ol");
        let parent_key=parent["structural_key"].as_str().unwrap();
        let children=nodes.iter().filter(|n|n["parent_key"]==parent_key).collect::<Vec<_>>();assert_eq!(children.len(),4);
        let expected=["Combination chemotherapy.","Imatinib mesylate (for patients with Philadelphia chromosome [Ph]–positive ALL).","Imatinib mesylate combined with combination chemotherapy (for patients with Ph-positive ALL).","Supportive care."];
        let mut child_block_positions=vec![];
        for (i,(child,text)) in children.iter().zip(expected).enumerate(){
            assert_eq!(child["own_text"],text);assert_eq!(child["position"],i+1);assert_eq!(child["source_list_id"],"_206");assert_eq!(child["list_kind"],"ul");
            assert_eq!(child["parent_text_path"],json!([parent["own_text"]]));
            let b=d.blocks.iter().find(|b|b.block_id==child["rendered_block_id"].as_str().unwrap()).unwrap();
            child_block_positions.push(d.blocks.iter().position(|candidate|candidate.block_id==b.block_id).unwrap());
            assert!(b.text.starts_with(parent["own_text"].as_str().unwrap()));assert!(b.text.ends_with(text));
            if i==0||i==3{assert!(!b.text.contains("Ph-positive")&&!b.text.contains("[Ph]–positive"));}
        }
        assert!(child_block_positions.windows(2).all(|pair|pair[0]<pair[1]));
        let target=format!("{HP}#_87");assert!(parent["links"].as_array().unwrap().iter().any(|l|l["target_url"]==target&&l["raw_href"]=="/types/leukemia/hp/adult-all-treatment-pdq#_87"));
        assert!(metadata["clinical_links"].as_array().unwrap().iter().any(|l|l["target_url"]==target&&l["source_ancestor_ids"].as_array().unwrap().iter().any(|id|id=="_84")));
        let cns=nodes.iter().find(|n|n["own_text"].as_str().unwrap().starts_with("Central nervous system")).unwrap();
        let cns_children=nodes.iter().filter(|n|n["parent_key"]==cns["structural_key"]).collect::<Vec<_>>();assert_eq!(cns_children.len(),3);
        for child in cns_children{let b=d.blocks.iter().find(|b|b.block_id==child["rendered_block_id"].as_str().unwrap()).unwrap();assert!(b.text.contains("Central nervous system (CNS) prophylaxis therapy"));assert!(!b.text.contains("Remission induction therapy"));}
        // The clinical words are unchanged; a source section-target change must still
        // change the scoped revision, even though the walker does not retain hrefs.
        let old="<ol id=\"_84\"><li><a href=\"/types/leukemia/hp/adult-all-treatment-pdq#_87\">";
        let new="<ol id=\"_84\"><li><a href=\"/types/leukemia/hp/adult-all-treatment-pdq#_101\">";
        let changed=RAW.replacen(old,new,1);assert_ne!(changed,RAW);
        let p=page(RAW,&base).unwrap();let q=page(&changed,&base).unwrap();assert_eq!(p.html,q.html);assert_ne!(snapshot(&p).unwrap(),snapshot(&q).unwrap());
        assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),2);assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),1);
        assert!(d.blocks.iter().any(|b|b.block_id=="section_7.12"));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod landing_index_tests{
    //! Full-catalog plan (2026-11-17) aborted with "PDQ catalog article missing": the Supportive and Palliative
    //! Care category index is an NCI landing page (no <article>) listing each summary as a paragraph with
    //! "patient | health professional" links.
    use super::*;
    const SC:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/supportive-care-index.html"));
    #[test]
    fn landing_category_index_yields_professional_links_with_titles(){
        let base=Url::parse("https://www.cancer.gov/publications/pdq/information-summaries/supportive-care").unwrap();
        assert!(indexes(SC,&base).unwrap().is_empty());
        let t=topics(SC,&base).unwrap();
        assert_eq!(t.len(),23);
        assert!(t.iter().all(|(u,_)|u.contains("-hp-pdq")),"{t:?}");
        assert!(t.iter().any(|(u,h)|u=="https://www.cancer.gov/about-cancer/treatment/side-effects/pain/pain-hp-pdq"&&h=="Cancer Pain (PDQ®)"));
        assert!(!t.iter().any(|(u,_)|u.ends_with("/pain/pain-pdq")));
    }
    #[test]
    fn non_pdq_landing_page_is_still_refused(){
        let base=Url::parse("https://www.cancer.gov/publications/pdq/information-summaries/other").unwrap();
        let html=SC.replace("PDQ® Cancer Information Summaries: Supportive and Palliative Care","Something else");
        assert!(topics(&html,&base).is_err());
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod nested_media_tests{
    //! Full run (2026-11-17): 10 professional summaries (paediatric, genomics, late effects, vulvar) were refused
    //! with "nested clinical list contains media/table". Genuine vulvar page (cache, 164,341 bytes): Table 6 (`_546`)
    //! sits inside item 1 of the nested list `_505` under `ol#_504` (SLND evidence).
    use super::*;
    const RAW:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/vulvar-hp.html"));
    const POLICY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nci_pdq/copyright.html"));
    const HP:&str="https://www.cancer.gov/types/vulvar/hp/vulvar-treatment-pdq";
    #[tokio::test]
    async fn genuine_vulvar_table_inside_nested_list_is_its_own_block_in_place(){
        assert_eq!(crate::common::sha256_hex(RAW.as_bytes()),"4880f204fd9f5b17dd458bb75a9cad98ac55a8f975ef92142c46da3c5b153cb5");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(HP,RAW.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.cancer.gov/policies/copyright-reuse",POLICY.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"nci_pdq","access":"public","enabled":true})).unwrap();
        let row=PlanRow{source_id:"nci_pdq".into(),doc_key:"vulvar-fixture".into(),url:HP.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({})};
        let d=NciPdq.extract(&src,&f,&row).await.unwrap();
        assert_eq!(d.item_type.as_deref(),Some("evidence_summary"));
        let tables=d.blocks.iter().enumerate().filter(|(_,b)|matches!(b.kind,BlockKind::Table{..})).collect::<Vec<_>>();
        assert_eq!(tables.len(),6);
        let (t6,table)=tables.iter().find(|(_,b)|matches!(&b.kind,BlockKind::Table{caption:Some(c)} if c.starts_with("Table 6."))).copied().expect("Table 6 kept");
        assert!(table.text.lines().any(|l|l=="| Wound breakdown | 11.7 | 34 |"),"{}",table.text);
        assert!(table.text.contains("SLND = Sentinel lymph node dissection."));
        // In place: after the item that cites it, before the next sibling item.
        let item=d.blocks.iter().position(|b|b.text.contains("Local morbidity was much lower")).unwrap();
        let next=d.blocks.iter().position(|b|b.text.contains("The mean hospital stay was 8.4 days")).unwrap();
        assert!(item<t6&&t6<next,"{item} {t6} {next}");
        // The item keeps its parent evidence context and no longer carries the table cells as prose.
        assert!(d.blocks[item].text.starts_with("In a multicenter case series, 403 patients"));
        assert!(!d.blocks[item].text.contains("Wound breakdown"));
        assert!(d.blocks[next].text.starts_with("In a multicenter case series"));
        assert!(d.warnings.iter().any(|w|w.contains("table _546")&&w.contains("Table 6.")&&w.contains("nested clinical list item")),"{:?}",d.warnings);
        let group=d.search_metadata["source_metadata"]["clinical_lists"].as_array().unwrap().iter().find(|g|g["source_list_id"]=="_504").unwrap();
        assert_eq!(group["contextual_rendering"],true);
        assert!(group["source_html"].as_str().unwrap().contains("id=\"_546\""));
        let node=group["nodes"].as_array().unwrap().iter().find(|n|n["lifted_media"].is_array()).unwrap();
        assert_eq!(node["source_list_id"],"_505");assert_eq!(node["lifted_media"][0]["source_id"],"_546");assert_eq!(node["lifted_media"][0]["tag"],"table");
        assert!(!node["own_text"].as_str().unwrap().contains("Wound breakdown"));
        // Lists without media keep their node records unchanged (no lifted_media key).
        assert_eq!(d.search_metadata["source_metadata"]["clinical_lists"].as_array().unwrap().iter().flat_map(|g|g["nodes"].as_array().unwrap()).filter(|n|n.get("lifted_media").is_some()).count(),1);
        assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).count(),1);
    }
}
