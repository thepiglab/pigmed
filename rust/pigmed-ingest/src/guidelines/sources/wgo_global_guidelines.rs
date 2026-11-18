//! WGO guideline index, topic landing pages and language-specific HTML guidance.
//! Figure diagrams and linked PDFs remain separate, rights-constrained assets.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{BlockKind, GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{anyhow, bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::json;
use std::collections::HashSet;
use url::Url;

pub struct WgoGlobalGuidelines;

fn catalog(html: &str, base: &Url) -> Result<Vec<(String,String,String,String)>> {
    let doc=Html::parse_document(html);
    let table=doc.select(&sel("table.table")).find(|t| {
        let t=text_of(t); t.contains("Title") && t.contains("Year") && t.contains("Cascades")
    }).context("WGO guidelines catalog table missing")?;
    let mut result=Vec::new(); let mut seen=HashSet::new();
    for tr in table.select(&sel("tr")) {
        let cells=common::cells(tr);
        if cells.len()!=3 {continue;}
        let Some(a)=cells[0].select(&sel("a[href]")).next() else {continue};
        let Some(url)=common::link(base,a.value().attr("href").unwrap()) else {continue};
        let parsed=Url::parse(&url)?;
        if parsed.origin()!=base.origin() || !parsed.path().starts_with("/guidelines/") {bail!("unsupported WGO catalog topic URL: {url}");}
        if seen.insert(url.clone()) {result.push((url,text_of(&a),text_of(&cells[1]),text_of(&cells[2])));}
    }
    if result.is_empty() {bail!("WGO catalog contains no guideline topic rows");}
    Ok(result)
}

/// Expand source rowspans/colspans so resource context stays attached to every
/// cascade instruction instead of being silently lost in Markdown conversion.
fn table_with_spans(table: ElementRef<'_>) -> Result<String> {
    if table.select(&sel("table")).next().is_some() {bail!("nested table requires source-specific handling");}
    let mut active:Vec<Option<(String,usize)>>=Vec::new();
    let mut rows:Vec<Vec<String>>=Vec::new();
    for tr in table.select(&sel("tr")) {
        let source=common::cells(tr); if source.is_empty() {continue;}
        let mut row=Vec::new(); let mut col=0usize; let mut next=0usize;
        while next<source.len() || active.iter().skip(col).any(Option::is_some) {
            if col>=active.len() {active.push(None);}
            if let Some((value,left))=active[col].clone() {
                row.push(value); active[col]=if left>1 {Some((row[col].clone(),left-1))} else {None}; col+=1; continue;
            }
            if next>=source.len() {row.push(String::new()); col+=1; continue;}
            let cell=source[next]; next+=1;
            let rowspan=cell.value().attr("rowspan").unwrap_or("1").parse::<usize>().unwrap_or(1).clamp(1,100);
            let colspan=cell.value().attr("colspan").unwrap_or("1").parse::<usize>().unwrap_or(1).clamp(1,100);
            let text=text_of(&cell).replace('|',"\\|");
            for _ in 0..colspan {
                if col>=active.len() {active.push(None);}
                if active[col].is_some() {bail!("overlapping table spans");}
                row.push(text.clone());
                if rowspan>1 {active[col]=Some((text.clone(),rowspan-1));}
                col+=1;
            }
        }
        rows.push(row);
    }
    if rows.is_empty() {bail!("empty table");}
    let width=rows.iter().map(Vec::len).max().unwrap_or(0);
    let line=|r:&Vec<String>| {let mut cells=r.clone();cells.resize(width,String::new());format!("| {} |",cells.join(" | "))};
    let mut out=vec![line(&rows[0]),format!("|{}|",vec!["---";width].join("|"))];
    out.extend(rows.iter().skip(1).map(line)); Ok(out.join("\n"))
}

fn clinical(html: &str, base: &Url) -> Result<(String,Option<String>,Vec<crate::guidelines::doc::Block>,Vec<String>,Vec<serde_json::Value>)> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel("#accordion-example")).next().context("WGO clinical accordion missing")?;
    if root.select(&sel("h3")).next().is_none() || root.select(&sel(".AccordianText")).next().is_none() {bail!("WGO clinical accordion incomplete");}
    let title=doc.select(&sel("section.section-full h1")).map(|e|text_of(&e)).find(|s|!s.is_empty()).context("WGO title missing")?;
    let raw_date=doc.select(&sel("section.section-full h1 + p")).map(|e|text_of(&e)).find(|s|!s.is_empty());
    // Malformed source markup can swallow the page share UI into an unfinished clinical list.
    // Keep the source evidence but remove that UI before indexing the partial clinical body.
    let partial=root.select(&sel(".article-smShares")).next().is_some();
    let mut clean=root.html();
    if partial {for e in root.select(&sel(".article-smShares,.article-disclaimers")) {clean=clean.replacen(&e.html(),"",1);}}
    let (mut blocks,mut warnings)=blocks_from_html(&clean,Some(base),Some("#accordion-example"));
    if partial {warnings.push("PARTIAL SOURCE HTML: clinical accordion runs into site share controls inside an unfinished list; the publisher-linked PDF is a separate, unverified rendition. This HTML must not be treated as a complete guideline.".into());}
    let tables=root.select(&sel("table")).collect::<Vec<_>>();
    let indices=blocks.iter().enumerate().filter_map(|(i,b)|matches!(b.kind,BlockKind::Table{..}).then_some(i)).collect::<Vec<_>>();
    if tables.len()!=indices.len() {warnings.push(format!("Found {} source tables but {} extracted; table structure requires review.",tables.len(),indices.len()));}
    else {
        for (table,index) in tables.into_iter().zip(indices) {
            if table.select(&sel("[rowspan], [colspan]")).next().is_some() {
                match table_with_spans(table) { Ok(text)=>blocks[index].text=text, Err(e)=>warnings.push(format!("Could not preserve table spans: {e}")) }
            }
        }
    }
    let mut attachments:Vec<serde_json::Value>=doc.select(&sel("section.section-full h2 a[href]"))
        .filter_map(|a| {let url=common::link(base,a.value().attr("href")?)?;
            Url::parse(&url).ok()?.path().to_lowercase().ends_with(".pdf").then(||json!({"url":url,"role":"pdf_rendition","rights":"unknown; linked asset not validated"})) }).collect();
    // Retain image URLs as Figures, and separately record image-table role only when source filename labels it.
    let table_image=regex::Regex::new(r"(?i)(?:^|[-_])table[-_]?\d+").unwrap();
    for b in &blocks {if let BlockKind::Figure{src_url:Some(url),..}=&b.kind {
        if Url::parse(url).ok().is_some_and(|u|u.path_segments().and_then(|mut s|s.next_back()).is_some_and(|name|table_image.is_match(name))) {
            attachments.push(json!({"url":url,"role":"image_based_table","source_block_id":b.block_id,"classification_basis":"source filename","rights":"unknown; independent asset","content_status":"not transcribed"}));
        }
    }}
    if blocks.iter().any(|b|matches!(b.kind,BlockKind::Figure{..})) {warnings.push("Image-based tables and algorithms require image/OCR review; diagram branches were not inferred from alt text.".into());}
    warnings.push("Resource-stratified recommendations remain in cascade tables and prose; formal statement labels, grades and individual recommendation rows are not inferred. Explicit table spans are expanded; blank resource cells and nested-list structure still require source review.".into());
    Ok((title,raw_date,blocks,warnings,attachments))
}

fn rights(html: &str, policy_url: &str) -> Rights {
    let doc=Html::parse_document(html);
    let lead=doc.select(&sel("section.section-full p")).map(|p|text_of(&p))
        .find(|t|t.starts_with("The copyright of the WGO Global Guidelines is retained"));
    match lead {
        Some(t)=>{
            let mut restrictions=vec![t];
            restrictions.extend(doc.select(&sel("section.section-full ol li")).map(|e|text_of(&e)));
            Rights {status:"restricted".into(),rights_holders:vec!["World Gastroenterology Organisation".into()],
                terms_url:Some(policy_url.into()), restrictions, evidence_urls:vec![policy_url.into()], ..Default::default()}
        },
        None=>common::unknown_rights(),
    }
}

/// Follow only the links in the source's Translated Versions table. A cell can
/// contain decorative, empty and NEW! links in addition to the real label.
fn renditions(html: &str, base: &Url) -> Result<Vec<(String,String,Option<String>)>> {
    let doc=Html::parse_document(html);
    let heading=doc.select(&sel("section.section-full h2")).find(|h|text_of(h)=="Translated Versions")
        .context("WGO Translated Versions heading missing")?;
    let table=heading.next_siblings().filter_map(ElementRef::wrap).find(|e|e.value().name()=="table")
        .context("WGO Translated Versions table missing")?;
    let mut seen=HashSet::new(); let mut out=Vec::new();
    for a in table.select(&sel("a[href]")) {
        let label=text_of(&a); let language=label.split_whitespace().next().unwrap_or("");
        if language.is_empty() || language == "NEW!" {continue;}
        let code=match language {"English"=>Some("en"),"French"=>Some("fr"),"Mandarin"=>Some("zh"),"Persian"=>Some("fa"),
            "Portuguese"=>Some("pt"),"Russian"=>Some("ru"),"Spanish"=>Some("es"),_=>None};
        let Some(url)=common::link(base,a.value().attr("href").unwrap()) else {continue};
        if Url::parse(&url)?.origin()!=base.origin() {bail!("cross-origin WGO language link requires review: {url}");}
        if !seen.insert(url.clone()) {continue;}
        out.push((url,label,code.map(String::from)));
    }
    if out.is_empty() {bail!("WGO topic has no recognized translated-version links");}
    Ok(out)
}

fn revision_hash(html: &str) -> Result<String> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel("#accordion-example")).next().context("WGO clinical accordion missing")?;
    let title=doc.select(&sel("section.section-full h1")).map(|e|text_of(&e)).find(|s|!s.is_empty()).context("WGO title missing")?;
    let date=doc.select(&sel("section.section-full h1 + p")).map(|e|text_of(&e)).find(|s|!s.is_empty()).unwrap_or_default();
    Ok(crate::common::sha256_hex(format!("{}\n{}\n{}",title,date,root.html()).as_bytes()))
}

#[async_trait::async_trait]
impl Adapter for WgoGlobalGuidelines {
    async fn discover(&self, src:&SourceEntry, f:&Fetcher, limit:Option<usize>) -> Result<Vec<PlanRow>> {
        if limit==Some(0) {return Ok(vec![]);}
        let url=src.catalog_url.as_deref().context("WGO catalog URL missing")?;
        let response=f.get_ok(url).await?;
        let topics=catalog(&response.text(),&Url::parse(&response.record.final_url)?)?;
        let mut out=Vec::new(); let mut seen=HashSet::new();
        for (topic,title,year,cascade) in topics {
            let work=Url::parse(&topic)?.path_segments().and_then(|s|s.filter(|x|!x.is_empty()).last()).context("WGO topic slug missing")?.to_string();
            let versions_result = match f.get_ok(&topic).await {
                Ok(topic_page) => renditions(&topic_page.text(),&Url::parse(&topic_page.record.final_url)?),
                Err(e) => Err(e),
            };
            let versions=match versions_result {
                Ok(versions)=>versions,
                Err(error)=>{
                    out.push(PlanRow {source_id:src.id.clone(),doc_key:format!("{}-topic-unresolved",work),url:topic.clone(),title:Some(title.clone()),rendition:None,version:None,
                        extra:json!({"work_key":work,"discovery_error":format!("topic discovery failed: {error:#}")})});
                    if limit.is_some_and(|n|out.len()>=n) {return Ok(out);}
                    continue;
                }
            };
            for (url,language_label,language_code) in versions {
                let language_key=common::slug(&language_label);
                if !seen.insert(url.clone()) {continue;}
                let mut row=PlanRow {source_id:src.id.clone(),doc_key:format!("{}-{}-unresolved",work,language_key),url,
                    title:Some(title.clone()),rendition:Some("html".into()),version:None,
                    extra:json!({"work_key":work,"language":language_code,"language_label":language_label,"topic_url":topic,"catalog_year_english":year,"catalog_cascades":cascade})};
                match f.get_ok(&row.url).await {
                    Ok(page)=>match revision_hash(&page.text()) {
                        Ok(hash)=>{ row.doc_key=format!("{}-{}-{}",work,language_key,&hash[..12]);row.extra["clinical_sha256"]=json!(hash); },
                        Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                    },
                    Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                }
                out.push(row);
                if limit.is_some_and(|n|out.len()>=n) {return Ok(out);}
            }
        }
        Ok(out)
    }
    async fn extract(&self, src:&SourceEntry, f:&Fetcher, row:&PlanRow) -> Result<GuidelineDoc> {
        if let Some(error)=row.extra["discovery_error"].as_str() {bail!("WGO discovery: {error}");}
        let policy_url="https://www.worldgastroenterology.org/guidelines/guidelines-re-use-policy";
        let policy=f.get_ok(policy_url).await?;
        let page=f.get_ok(&row.url).await?;
        let html=page.text(); let base=Url::parse(&page.record.final_url)?;
        let hash=revision_hash(&html)?;
        if row.extra["clinical_sha256"].as_str().is_some_and(|v|v!=hash) {bail!("WGO clinical revision changed after planning; re-plan");}
        let (title,raw,blocks,mut warnings,attachments)=clinical(&html,&base)?;
        let partial=warnings.iter().any(|w|w.starts_with("PARTIAL SOURCE HTML:"));
        let dates=raw.map(|d|vec![common::date("Document date",&d,base.as_str())]).unwrap_or_default();
        let r=rights(&policy.text(),policy_url);
        if r.status=="unknown" {warnings.push("WGO reuse policy did not match expected text; rights require review.".into());}
        warnings.push("Source-linked PDF and image assets were not validated by this extraction; access and per-asset rights remain unresolved.".into());
        warnings.push("Catalog lists English edition years only; translation dates and currentness are not inferred from the catalog.".into());
        let topic=row.extra["topic_url"].as_str().unwrap_or_default();
        let relations=if topic.is_empty() {vec![]} else {vec![Relation {rel:"related".into(),target:topic.into(),scope:Some("Translated Versions".into()),evidence_url:Some(topic.into())}]};
        Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),
            title,url:row.url.clone(),item_type:Some(if partial {"partial_guideline_html"} else {"global_guideline"}.into()),status:"unknown".into(),version:None,
            date_observations:dates,language:row.extra["language"].as_str().map(String::from),rights:r,relations,blocks,attachments,warnings,
            search_metadata:json!({"catalog_year_english":row.extra["catalog_year_english"],"catalog_cascades":row.extra["catalog_cascades"],"source_language_label":row.extra["language_label"],"source_url":src.catalog_url}),
            fetches:vec![page.record,policy.record],..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const MANDARIN: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/wgo_global_guidelines/acute-diarrhea-mandarin.html"));
    const CATALOG: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/wgo_global_guidelines/catalog.html"));
    const POLICY: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/wgo_global_guidelines/reuse-policy.html"));
    const TOPIC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/wgo_global_guidelines/celiac-topic.html"));
    const CELIAC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/wgo_global_guidelines/celiac-english.html"));
    const ROOT: &str = "https://www.worldgastroenterology.org/guidelines";
    const PAGE: &str = "https://www.worldgastroenterology.org/guidelines/celiac-disease/celiac-disease-english";
    #[test]
    fn saved_celiac_clinical_body_and_span_context() {
        let topics=catalog(CATALOG,&Url::parse(ROOT).unwrap()).unwrap();
        assert!(topics.iter().any(|(u,t,y,c)|u.ends_with("/guidelines/global-guidelines/celiac-disease") && t=="Celiac Disease" && y=="2016" && c=="Yes"));
        assert!(topics.iter().any(|(u,_,_,_)|u.ends_with("/guidelines/helicobacter-pylori")));
        let (title,raw,blocks,warnings,attachments)=clinical(CELIAC,&Url::parse(PAGE).unwrap()).unwrap();
        assert_eq!(title,"Celiac disease"); assert_eq!(raw.as_deref(),Some("July 2016"));
        assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text=="7. Appendix"));
        assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text=="References"));
        assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Figure{src_url:Some(ref u),..} if u.ends_with("/UserFiles/image/guidelines/english/celiac-english-table4.png"))));
        let tables=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).collect::<Vec<_>>();
        assert_eq!(tables.len(),5);
        assert!(tables.iter().any(|b|b.text.matches("Gold standard").count()>=6 && b.text.contains("anti-tTG")));
        assert!(attachments.iter().any(|a|a["url"].as_str().is_some_and(|u|u.ends_with("/celiac-disease-english-2016.pdf"))));
        assert!(warnings.iter().any(|w|w.contains("image/OCR")));
        let r=rights(POLICY,"https://www.worldgastroenterology.org/guidelines/guidelines-re-use-policy"); assert_eq!(r.status,"restricted");
        assert!(r.restrictions.iter().any(|s|s.contains("without specific authorization")));
    }
    #[tokio::test]
    async fn actual_landing_renditions_and_offline_discovery() {
        let landing="https://www.worldgastroenterology.org/guidelines/global-guidelines/celiac-disease";
        let languages=renditions(TOPIC,&Url::parse(landing).unwrap()).unwrap();
        assert_eq!(languages.len(),7);
        assert_eq!(languages[0],(PAGE.into(),"English".into(),Some("en".into())));
        assert!(languages.iter().any(|(u,_,l)|u.ends_with("/celiac-disease-persian") && l.as_deref()==Some("fa")));
        assert_eq!(languages.iter().filter(|(_,_,l)|l.as_deref()==Some("pt")).count(),1);
        let augmented=TOPIC.replace("</table>","<tr><td><a href='/guidelines/celiac-disease/celiac-disease-klingon'>Klingon</a></td></tr></table>");
        let extra=renditions(&augmented,&Url::parse(landing).unwrap()).unwrap();
        assert!(extra.iter().any(|(u,label,code)|u.ends_with("-klingon") && label=="Klingon" && code.is_none()));
        let marker=CATALOG.find("href=\"/guidelines/global-guidelines/celiac-disease\"").unwrap();
        let begin=CATALOG[..marker].rfind("<tr>").unwrap();
        let end=marker+CATALOG[marker..].find("</tr>").unwrap()+5;
        let bounded=format!("<table class='table'><tr><td>Title</td><td>Year</td><td>Cascades</td></tr>{}</table>",&CATALOG[begin..end]);
        let dir=tempfile::tempdir().unwrap();
        let f=Fetcher::new(dir.path(),true,500,1<<20,"").unwrap();
        let policy="https://www.worldgastroenterology.org/guidelines/guidelines-re-use-policy";
        f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");
        f.seed_cache_for_tests(landing,TOPIC.as_bytes(),"text/html");
        f.seed_cache_for_tests(PAGE,CELIAC.as_bytes(),"text/html");
        f.seed_cache_for_tests(policy,POLICY.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"wgo_global_guidelines","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let rows=WgoGlobalGuidelines.discover(&src,&f,Some(1)).await.unwrap();
        assert_eq!(rows.len(),1);assert_eq!(rows[0].url,PAGE);
        let doc=WgoGlobalGuidelines.extract(&src,&f,&rows[0]).await.unwrap();
        assert_eq!(doc.title,"Celiac disease"); assert_eq!(doc.language.as_deref(),Some("en"));
        assert_eq!(doc.status,"unknown");assert_eq!(doc.rights.status,"restricted");
        assert!(doc.published.is_none() && doc.version.is_none());
        assert!(doc.date_observations.iter().any(|d|d.raw=="July 2016" && d.normalized.as_deref()==Some("2016-07")));
        assert!(doc.relations.iter().any(|r|r.target==landing && r.scope.as_deref()==Some("Translated Versions")));
        assert_eq!(doc.fetches.len(),2);
        let mut altered=rows[0].clone();altered.extra["clinical_sha256"]=json!("changed");
        assert!(WgoGlobalGuidelines.extract(&src,&f,&altered).await.unwrap_err().to_string().contains("changed after planning"));
        let missing=bounded.replace("/guidelines/global-guidelines/celiac-disease","/guidelines/global-guidelines/missing-fixture");
        f.seed_cache_for_tests(ROOT,missing.as_bytes(),"text/html");
        let unresolved=WgoGlobalGuidelines.discover(&src,&f,Some(1)).await.unwrap();
        assert_eq!(unresolved.len(),1);
        assert!(unresolved[0].extra["discovery_error"].as_str().is_some_and(|e|e.contains("topic discovery failed")));
        assert!(WgoGlobalGuidelines.extract(&src,&f,&unresolved[0]).await.is_err());
    }

    #[test]
    fn publisher_mandarin_html_is_partial_and_keeps_pdf_link() {
        let url=Url::parse("https://www.worldgastroenterology.org/guidelines/acute-diarrhea/acute-diarrhea-mandarin").unwrap();
        let (_,_,blocks,warnings,attachments)=clinical(MANDARIN,&url).unwrap();
        assert!(warnings.iter().any(|w|w.starts_with("PARTIAL SOURCE HTML:")));
        assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Heading{level:3})).count(),2);
        assert!(blocks.iter().any(|b|b.text.contains("HuCVs")));
        assert!(!blocks.iter().any(|b|b.text=="Share" || b.text.contains("Website Disclaimer")));
        assert!(attachments.iter().any(|a|a["url"].as_str().is_some_and(|u|u.ends_with("acute-diarrhea-mandarin-2012.pdf"))));
    }

}
