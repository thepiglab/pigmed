//! CENETEC current catalog: source-emitted section tree and public AJAX fragments.
//! A section failure is an audit error, never a successful partial guideline.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::{Fetcher,Fetched,Req,looks_like_login_wall};
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{anyhow,bail,Context,Result};
use scraper::{ElementRef,Html};
use serde::{Deserialize,Serialize};
use serde_json::json;
use std::collections::HashSet;
use url::Url;

pub struct MxCenetecCmgpc;
#[derive(Clone,Debug,Serialize,Deserialize)]
struct Section { id:String, label:String, path:Vec<String>, sha256:Option<String> }

fn card_rows(html:&str,base:&Url)->Result<Vec<(String,String,serde_json::Value)>> {
    let doc=Html::parse_document(html);
    if doc.select(&sel("#ListaGuias")).next().is_none(){bail!("CENETEC catalog root missing");}
    let mut out=Vec::new();let mut seen=HashSet::new();
    for a in doc.select(&sel("#ListaGuias h3.card-title a[href]")) {
        let Some(url)=common::link(base,a.value().attr("href").unwrap()) else {continue};
        let parsed=Url::parse(&url)?;
        if parsed.origin()!=base.origin() || parsed.path()!="/DDIMBE/DDIMBE/ContenidoGuia" {bail!("unexpected CENETEC catalog link {url}");}
        if !seen.insert(url.clone()){continue;}
        let card=a.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().classes().any(|c|c=="card-info")).context("catalog card metadata missing")?;
        let mut fields=serde_json::Map::new();
        for p in card.select(&sel("p")) {
            let raw=text_of(&p);
            if let Some((k,v))=raw.split_once(':') {fields.insert(k.trim().to_string(),json!(v.trim()));}
        }
        out.push((url,text_of(&a),serde_json::Value::Object(fields)));
    }
    if out.is_empty(){bail!("CENETEC catalog has no source cards");}
    Ok(out)
}
fn section_tree(html:&str)->Result<Vec<Section>> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel("#accordionRaiz")).next().context("CENETEC section tree missing")?;
    let mut sections=Vec::new();let mut seen=HashSet::new();
    for anchor in root.select(&sel("a.link-cargar-seccion[data-id]")) {
        let id=anchor.value().attr("data-id").unwrap_or_default();
        if id.is_empty() || !id.bytes().all(|b|b.is_ascii_digit()) {bail!("invalid source section identifier {id}");}
        if !seen.insert(id.to_string()){bail!("duplicate CENETEC section ID {id}");}
        let mut path=Vec::new();
        for parent in anchor.ancestors().filter_map(ElementRef::wrap) {
            if !parent.value().classes().any(|c|c=="accordion-item") {continue;}
            let header=parent.children().filter_map(ElementRef::wrap).find(|e|e.value().classes().any(|c|c=="accordion-header"));
            if let Some(h)=header {
                if let Some(button)=h.select(&sel("button")).next() {let t=text_of(&button);if !t.is_empty(){path.push(t);}}
            }
        }
        path.reverse();let label=text_of(&anchor);
        if label.is_empty(){bail!("section {id} has no source label");}
        sections.push(Section{id:id.into(),label,path,sha256:None});
    }
    if sections.is_empty(){bail!("CENETEC tree has no source sections");}
    if sections.len()>1000 {bail!("CENETEC tree exceeds safety bound");}
    Ok(sections)
}
fn section_req(base:&Url,id:&str)->Result<Req> {
    if id.is_empty() || !id.bytes().all(|c|c.is_ascii_digit()){bail!("invalid CENETEC section ID");}
    let mut url=base.join("/DDIMBE/DDIMBE/ObtenerContenidoSeccion")?;
    url.query_pairs_mut().append_pair("id",id);
    Ok(Req {headers:vec![("x-requested-with".into(),"XMLHttpRequest".into())],..Req::get(url.as_str())})
}
async fn section_get(f:&Fetcher,base:&Url,id:&str)->Result<Fetched> {
    let req=section_req(base,id)?;let response=f.request(&req).await?;
    if response.record.status!=200 {bail!("section {id}: HTTP {}",response.record.status);}
    let body=response.text();
    if looks_like_login_wall(&body){bail!("section {id}: login wall");}
    let lower=body.to_lowercase();
    if lower.contains("captcha") || lower.contains("checking your browser") {bail!("section {id}: access challenge");}
    if !response.is_html(){bail!("section {id}: response is not HTML");}
    let html=Html::parse_fragment(&response.text());
    if html.select(&sel("h1,h2,h3,h4,p,li,table,img")).next().is_none(){bail!("section {id}: empty or unrecognized fragment");}
    Ok(response)
}
fn fragment_blocks(html:&str,base:&Url,section:&Section)->Result<(Vec<Block>,Vec<String>)> {
    // The frontend resolves ~/... against /DDIMBE/. Rewrite URL attributes only.
    let normalized=html.replace("src=\"~/", "src=\"/DDIMBE/").replace("src='~/", "src='/DDIMBE/")
        .replace("href=\"~/", "href=\"/DDIMBE/").replace("href='~/", "href='/DDIMBE/")
        .replace("src=\"~", "src=\"/DDIMBE/").replace("src='~", "src='/DDIMBE/")
        .replace("href=\"~", "href=\"/DDIMBE/").replace("href='~", "href='/DDIMBE/");
    let parsed=Html::parse_fragment(&normalized);
    if parsed.select(&sel("h1 img,h2 img,h3 img,h4 img,h5 img,h6 img")).next().is_some(){
        bail!("section {} contains an image inside a heading; shared extractor would omit it",section.id);
    }
    let separators=parsed.select(&sel("div.separador")).map(|e|text_of(&e)).collect::<Vec<_>>();
    // The frontend opens evidence graphics using data-img, without an <img>.
    // Preserve each linked image after its actual source anchor in DOM order.
    let mut expanded=parsed.root_element().html();
    for a in parsed.select(&sel("a[data-img]")) {
        let raw=a.value().attr("data-img").unwrap_or_default();
        let src=if let Some(rest)=raw.strip_prefix('~') {format!("/DDIMBE/{}",rest.trim_start_matches('/'))} else {raw.into()};
        let url=common::link(base,&src).ok_or_else(||anyhow!("invalid section {} data-img URL",section.id))?;
        let caption=text_of(&a).replace('&',"&amp;").replace('\"',"&quot;").replace('<',"&lt;").replace('>',"&gt;");
        expanded=expanded.replacen(&a.html(),&format!("{}<img src=\"{}\" alt=\"{}\" />",a.html(),url,caption),1);
    }
    let (mut blocks,mut warnings)=blocks_from_html(&expanded,Some(base),None);
    for b in &mut blocks {
        b.block_id=format!("section-{}-{}",section.id,b.block_id);
        let mut path=section.path.clone();path.push(section.label.clone());path.extend(b.heading_path.clone());b.heading_path=path;
    }
    if section.label.starts_with("RECOMENDACIONES") || section.label.starts_with("PUNTOS DE BUENA PRÁCTICA") {
        let mut ranges=Vec::new();let mut cursor=0usize;
        for separator in &separators {
            let Some(i)=(cursor..blocks.len()).find(|&i|blocks[i].text==*separator) else {warnings.push(format!("Section {} recommendation separator not found in block stream",section.id));continue;};
            let mut end=i+1;
            while end<blocks.len() && !separators.iter().any(|t|t==&blocks[end].text) && matches!(blocks[end].kind,BlockKind::Paragraph|BlockKind::ListItem) {end+=1;}
            if end==i+1 {warnings.push(format!("Section {} recommendation has no contiguous text after source separator",section.id));cursor=end;continue;}
            ranges.push((i,end,separator.clone()));cursor=end;
        }
        if ranges.is_empty(){warnings.push(format!("Section {} is labeled {} but its statement boundaries need review",section.id,section.label));}
        for (n,(i,end,separator)) in ranges.into_iter().enumerate().rev() {
            let text=blocks[i+1..end].iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join("\n");
            let strength=if separator.starts_with("Fuerte ") {Some("Fuerte".into())} else {None};
            let grading_system=separator.split_whitespace().any(|s|s=="GRADE").then(||"GRADE".into());
            let path=blocks[i].heading_path.clone();
            blocks.splice(i+1..end,[Block {block_id:format!("section-{}-recommendation-{}",section.id,n+1),
                kind:BlockKind::Recommendation {label:None,strength,certainty:None,population:None,grading_system,qualifier:Some(section.label.clone())},
                text,heading_path:path,page:None}]);
        }
    }
    Ok((blocks,warnings))
}

#[async_trait::async_trait]
impl Adapter for MxCenetecCmgpc {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0){return Ok(vec![]);}
        let catalog_url=src.catalog_url.as_deref().context("CENETEC catalog URL missing")?;
        let index=f.get_ok(catalog_url).await?; let origin=Url::parse(&index.record.final_url)?;
        let entries=card_rows(&index.text(),&origin)?;let mut rows=Vec::new();
        for (url,title,metadata) in entries {
            let parsed=Url::parse(&url)?;
            let id=parsed.query_pairs().find(|(k,_)|k=="DocumentoID").map(|(_,v)|v.into_owned()).context("CENETEC catalog document ID missing")?;
            if !id.bytes().all(|b|b.is_ascii_digit()){bail!("CENETEC invalid document ID {id}");}
            let mut row=PlanRow {source_id:src.id.clone(),doc_key:format!("{id}-unresolved"),url,title:Some(title),rendition:Some("html".into()),version:None,
                extra:json!({"work_key":id,"catalog_fields":metadata,"catalog_url":catalog_url})};
            let result:Result<(String,Vec<Section>)>=async {
                let page=f.get_ok(&row.url).await?;
                let mut sections=section_tree(&page.text())?;
                let tree_hash=crate::common::sha256_hex(serde_json::to_string(&sections)?.as_bytes());
                for section in &mut sections {
                    let content=section_get(f,&origin,&section.id).await.with_context(||format!("unresolved CENETEC tree section {} ({})",section.id,section.label))?;
                    section.sha256=Some(content.record.sha256);
                }
                Ok((tree_hash,sections))
            }.await;
            let mut stop_source=false;
            match result {
                Ok((tree_hash,sections))=>{
                    let version_hash=crate::common::sha256_hex(serde_json::to_string(&(&sections,&row.title,&row.extra["catalog_fields"]))?.as_bytes());
                    row.doc_key=format!("{}-{}",id,&version_hash[..12]);
                    row.extra["tree_sha256"]=json!(tree_hash);row.extra["sections"]=serde_json::to_value(sections)?;
                },
                Err(error)=>{
                    let message=format!("{error:#}");let lower=message.to_lowercase();
                    stop_source=["http 401","http 403","http 429","login wall","access challenge","robots","tls","certificate","handshake"].iter().any(|s|lower.contains(s));
                    row.extra["discovery_error"]=json!(message);
                },
            }
            rows.push(row);if stop_source || limit.is_some_and(|n|rows.len()>=n){break;}
        }
        Ok(rows)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("CENETEC discovery incomplete: {e}");}
        let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;
        let page_html=page.text();
        let language={let doc=Html::parse_document(&page_html);common::page_language(&doc)};
        let tree=section_tree(&page_html)?;
        let observed_hash=crate::common::sha256_hex(serde_json::to_string(&tree)?.as_bytes());
        if row.extra["tree_sha256"].as_str()!=Some(observed_hash.as_str()){bail!("CENETEC section tree changed since planning");}
        let expected:Vec<Section>=serde_json::from_value(row.extra["sections"].clone()).context("CENETEC planned section list missing")?;
        if expected.len()!=tree.len(){bail!("CENETEC planned section count mismatch");}
        let catalog_url=row.extra["catalog_url"].as_str().context("CENETEC catalog provenance missing")?;
        let catalog=f.get_ok(catalog_url).await?;
        let catalog_base=Url::parse(&catalog.record.final_url)?;
        let matching=card_rows(&catalog.text(),&catalog_base)?.into_iter().find(|(url,_,_)|url==&row.url)
            .ok_or_else(||anyhow!("CENETEC planned catalog card disappeared"))?;
        if Some(matching.1.as_str())!=row.title.as_deref() || matching.2!=row.extra["catalog_fields"] {bail!("CENETEC catalog metadata changed since planning");}
        let mut blocks=Vec::new();let mut warnings=Vec::new();let mut fetches=vec![page.record,catalog.record];
        for (node,planned) in tree.iter().zip(expected.iter()) {
            if node.id!=planned.id||node.path!=planned.path||node.label!=planned.label{bail!("CENETEC source section order changed");}
            let section=section_get(f,&base,&node.id).await?;
            if planned.sha256.as_deref()!=Some(section.record.sha256.as_str()){bail!("CENETEC section {} changed since planning",node.id);}
            let (mut content,notes)=fragment_blocks(&section.text(),&base,node)?;warnings.extend(notes);
            if content.is_empty(){bail!("CENETEC section {} yielded no clinical blocks",node.id);}
            blocks.append(&mut content);fetches.push(section.record);
        }
        warnings.push("Default catalog scope only; historical and other filtered records are not reconciled.".into());
        if blocks.iter().any(|b|matches!(b.kind,BlockKind::Figure{..})) {warnings.push("Image-only clinical tables and algorithms require full asset acquisition and visual/OCR review; image text and branches are not inferred.".into());}
        let mut dates=Vec::new();
        if let Some(year)=row.extra["catalog_fields"]["Año de publicación"].as_str() {
            dates.push(common::date("Año de publicación",year,row.extra["catalog_url"].as_str().unwrap_or(&row.url)));
        }
        let published=dates.iter().find(|d|d.label=="Año de publicación").and_then(|d|d.normalized.clone());
        Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),
            title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some("guia_de_practica_clinica".into()),
            status:"unknown".into(),rights:Rights{status:"unknown".into(),..Default::default()},language,published,date_observations:dates,
            search_metadata:json!({"catalog_fields":row.extra["catalog_fields"],"catalog_url":row.extra["catalog_url"]}),
            blocks,warnings,fetches,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const CATALOG:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/mx_cenetec_cmgpc/catalog-fragment.html"));
    const EVIDENCE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/mx_cenetec_cmgpc/section-29611.html"));
    const RECOMMENDATION:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/mx_cenetec_cmgpc/section-29612.html"));
    const ALGORITHM:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/mx_cenetec_cmgpc/section-29696.html"));
    const TREE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/mx_cenetec_cmgpc/tree-3038.html"));
    #[test]
    fn source_tree_order_paths_and_request_identity() {
        let base=Url::parse("https://gpc.salud.gob.mx/DDIMBE/DDIMBE/").unwrap();
        let cards=card_rows(CATALOG,&base).unwrap();
        assert!(cards.iter().any(|(u,_,m)|u.ends_with("DocumentoID=3038")&&m["Clave de Catálogo Maestro"]=="SS-160-22"));
        let tree=section_tree(TREE).unwrap();assert_eq!(tree.len(),120);
        let evidence=tree.iter().find(|s|s.id=="29611").unwrap();
        let recommendation=tree.iter().find(|s|s.id=="29612").unwrap();
        assert_eq!(evidence.label,"EVIDENCIAS");assert_eq!(recommendation.label,"RECOMENDACIONES CLAVE");
        assert_eq!(recommendation.path,vec!["DIAGNÓSTICO DE LA CETOACIDOSIS DIABÉTICA","PREGUNTA 1"]);
        let algorithm=tree.iter().find(|s|s.id=="29696").unwrap();assert_eq!(algorithm.label,"FLUJOGRAMA 1");
        let request=section_req(&base,"29612").unwrap();
        assert_eq!(request.url,"https://gpc.salud.gob.mx/DDIMBE/DDIMBE/ObtenerContenidoSeccion?id=29612");
        assert_eq!(request.headers,vec![("x-requested-with".into(),"XMLHttpRequest".into())]);
    }
    #[tokio::test]
    async fn source_fragments_preserve_grades_images_and_header_identity() {
        let base=Url::parse("https://gpc.salud.gob.mx/DDIMBE/DDIMBE/").unwrap();
        let nodes=section_tree(TREE).unwrap();
        let evidence=nodes.iter().find(|s|s.id=="29611").unwrap();
        let recommendation=nodes.iter().find(|s|s.id=="29612").unwrap();
        let algorithm=nodes.iter().find(|s|s.id=="29696").unwrap();
        let (blocks,_)=fragment_blocks(RECOMMENDATION,&base,recommendation).unwrap();
        let recs=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect::<Vec<_>>();
        assert_eq!(recs.len(),1);
        assert!(matches!(&recs[0].kind,BlockKind::Recommendation {strength:Some(s),grading_system:Some(g),..} if s=="Fuerte" && g=="GRADE"));
        for group in ["CAD Leve", "CAD moderada", "CAD severa"] {assert!(recs[0].text.contains(group));}
        assert!(recs[0].heading_path.iter().any(|p|p=="PREGUNTA 1"));
        let (evidence_blocks,_)=fragment_blocks(EVIDENCE,&base,evidence).unwrap();
        let figs=evidence_blocks.iter().filter(|b|matches!(b.kind,BlockKind::Figure{..})).collect::<Vec<_>>();
        assert_eq!(figs.len(),3);
        assert!(figs.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),caption:Some(c)} if u.ends_with("/DDIMBE/imagenes/doc_3038/cuadro_de_evidencia_1.jpg") && c.contains("Cuadro de evidencia 1"))));
        let (flow,_)=fragment_blocks(ALGORITHM,&base,algorithm).unwrap();
        assert!(flow.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u.ends_with("/DDIMBE/imagenes/doc_3038/flujograma_1.jpg"))));
        let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,1<<20,"").unwrap();
        let req=section_req(&base,"29612").unwrap();f.seed_cache_request_for_tests(&req,RECOMMENDATION.as_bytes(),"text/html");
        assert!(f.get_ok(&req.url).await.is_err());
        let fetched=section_get(&f,&base,"29612").await.unwrap();assert_eq!(fetched.text(),RECOMMENDATION);
    }

    #[tokio::test]
    async fn missing_tree_or_section_never_becomes_successful_guideline() {
        let root="https://gpc.salud.gob.mx/DDIMBE/DDIMBE/";
        let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(root,CATALOG.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"mx_cenetec_cmgpc","access":"public","enabled":true,"catalog_url":root})).unwrap();
        let rows=MxCenetecCmgpc.discover(&src,&f,Some(1)).await.unwrap();
        assert_eq!(rows.len(),1);assert!(rows[0].extra["discovery_error"].is_string());
        assert!(MxCenetecCmgpc.extract(&src,&f,&rows[0]).await.is_err());
    }

    #[tokio::test]
    async fn partial_120_node_tree_cannot_become_a_complete_guideline() {
        let root="https://gpc.salud.gob.mx/DDIMBE/DDIMBE/";
        let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(root,CATALOG.as_bytes(),"text/html");
        let base=Url::parse(root).unwrap();let cards=card_rows(CATALOG,&base).unwrap();
        let url=&cards[0].0;f.seed_cache_for_tests(url,TREE.as_bytes(),"text/html");
        for (id,html) in [("29611",EVIDENCE),("29612",RECOMMENDATION),("29696",ALGORITHM)] {
            let req=section_req(&base,id).unwrap();f.seed_cache_request_for_tests(&req,html.as_bytes(),"text/html");
        }
        let src:SourceEntry=serde_json::from_value(json!({"id":"mx_cenetec_cmgpc","access":"public","enabled":true,"catalog_url":root})).unwrap();
        let rows=MxCenetecCmgpc.discover(&src,&f,Some(1)).await.unwrap();
        assert_eq!(rows.len(),1);assert!(rows[0].extra["discovery_error"].as_str().unwrap().contains("section"));
        assert!(MxCenetecCmgpc.extract(&src,&f,&rows[0]).await.is_err());
    }

}
