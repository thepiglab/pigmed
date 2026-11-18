//! Diabetes Canada guideline chapter HTML, with edition-specific inventory.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::json;
use std::collections::HashSet;
use url::Url;

pub struct DiabetesCanada;
const ROOT:&str="https://www.diabetes.ca/for-professionals/full-guidelines";
const TERMS:&str="https://www.diabetes.ca/terms-of-use";
fn catalog(html:&str,base:&Url)->Result<Vec<(String,String,String,Option<String>,bool)>> {
    let doc=Html::parse_document(html);
    let table=doc.select(&sel("#table-of-contents table")).next().context("Diabetes Canada chapter inventory missing")?;
    let chapter=regex::Regex::new(r"^/chapter-(\d+)(?:-\d{4}-update)?/?$").unwrap();
    let year=regex::Regex::new(r"(?i)\b(20\d{2})\s+(UPDATE|ARCHIVED)\b").unwrap();
    let mut seen=HashSet::new();let mut rows=vec![];
    for a in table.select(&sel("tr td a[href]")) {
        let href=a.value().attr("href").unwrap_or_default();let Some(cap)=chapter.captures(href) else {continue};
        let Some(url)=common::link(base,href) else {continue};let u=Url::parse(&url)?;
        if u.origin()!=base.origin(){continue;} if !seen.insert(url.clone()){continue;}
        let title=text_of(&a);let edition=year.captures(&title).map(|c|format!("{} {}",&c[1],&c[2].to_uppercase()));
        let archived=title.to_uppercase().contains("ARCHIVED");rows.push((url,title,cap[1].to_string(),edition,archived));
    }
    if rows.is_empty(){bail!("Diabetes Canada chapter links missing");}Ok(rows)
}
fn clinical(html:&str)->Result<(String,String,Vec<serde_json::Value>)> {
    let doc=Html::parse_document(html);
    let title=doc.select(&sel(".chapter-banner h1.page-title")).next().map(|e|text_of(&e)).filter(|s|!s.is_empty()).context("Diabetes Canada chapter title missing")?;
    let body=doc.select(&sel("#chapterTabsContent #full-text .chapter-content")).next().context("Diabetes Canada full text tab missing")?;
    // Introduction/methods chapters (1, 2) have full text but no recommendation list; that is not a gate.
    if body.select(&sel(".mainText")).next().is_none(){bail!("Diabetes Canada full clinical text missing");}
    // The full-text panel can contain exact copies of figures also shown in
    // the figures tab. Suppress only byte-equivalent element markup; retain
    // differing source versions even when their IDs match.
    let full_figures:HashSet<String>=body.select(&sel(".chapterFigure")).map(|e|e.html()).collect();
    let figures=doc.select(&sel("#chapterTabsContent #figures .chapterFigure")).map(|e|e.html()).filter(|h|!full_figures.contains(h)).collect::<Vec<_>>();
    let mut attachments=vec![];
    for a in doc.select(&sel(".chapter-banner a[href]")) {
        let label=text_of(&a);let href=a.value().attr("href").unwrap_or_default();
        if label.to_lowercase().contains("download") && href.to_lowercase().contains(".pdf") {attachments.push(json!({"href":href,"role":"publisher_pdf_rendition","rights":"unknown; independently sourced asset"}));}
    }
    Ok((title,format!("<div id='clinical'>{}{}{}</div>",body.html(),"<h2>Figures from the source figures tab</h2>",figures.join("")),attachments))
}
fn source_rights(html:&str)->Result<Rights> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(".generic-page-content-wrapper .page-body, .page-body")).next().context("Diabetes Canada terms content missing")?;
    if !root.select(&sel("h2")).any(|h|text_of(&h).contains("Ownership, copyright and permitted use")){bail!("Diabetes Canada terms ownership section missing");}
    let paragraphs:Vec<_>=root.select(&sel("p")).map(|p|text_of(&p)).collect();
    let permission=paragraphs.iter().find(|p|p.contains("personal, limited, revocable") && p.contains("personal, non-commercial and not-for-profit use only")).context("Diabetes Canada terms personal-use statement missing")?.clone();
    let restriction=paragraphs.iter().find(|p|p.contains("indexed, catalogued, merged with other data") && p.contains("express written consent of Diabetes Canada")).context("Diabetes Canada terms indexing restriction missing")?.clone();
    Ok(Rights {status:"restricted".into(),terms_url:Some(TERMS.into()),evidence_urls:vec![TERMS.into()],restrictions:vec![permission,restriction],note:Some("These terms address site content; the journal publisher's permission to reproduce material on the site does not establish permission for this corpus. Independently acquired assets need separate rights review; no consent is recorded.".into()),..Default::default()})
}
fn recommendations(html:&str,blocks:&mut Vec<Block>)->Vec<String> {
    let doc=Html::parse_fragment(html);let grade=regex::Regex::new(r"\[(Grade\s+[A-D0-9]+)\s*,\s*(Level\s+[0-9]+[A-Za-z]*|Consensus)(?:\s+for\s+([^\[\]]+))?\]").unwrap();
    let paired=regex::Regex::new(r"\[(Grade\s+[A-D0-9]+)\s*,\s*(Level\s+[0-9]+[A-Za-z]*|Consensus)\s+for\s+((?:\[[^\]]+\]|[^\];])+);\s*(Grade\s+[A-D0-9]+)\s*,\s*(Level\s+[0-9]+[A-Za-z]*|Consensus)\s+for\s+([^\]]+)\]").unwrap();
    let mut out=vec![];let mut warnings=vec![];
    // Some chapters repeat the same list (inline and again in a closing section): structure it once.
    let mut lists=HashSet::new();let mut items=vec![];
    for list in doc.select(&sel(".chapterRecommendations")) {
        if !lists.insert(list.html()) {warnings.push("An exact repeat of a recommendation list is kept as text and not structured twice.".into());continue;}
        for l in list.children().filter_map(ElementRef::wrap).filter(|e|matches!(e.value().name(),"ol"|"ul")) {
            items.extend(l.children().filter_map(ElementRef::wrap).filter(|e|e.value().name()=="li"));
        }
    }
    let mut claimed=HashSet::new();
    for (n,li) in items.into_iter().enumerate() {
        let label=li.parent().and_then(ElementRef::wrap).filter(|p|p.value().name()=="ol").map(|_|(n+1).to_string());
        let text=text_of(&li);
        let shown=li.value().id().map(String::from).or_else(||label.clone()).unwrap_or_else(||"(unnumbered)".into());
        let id=match li.value().id() {
            Some(id)=>Some(id.to_string()),
            // Items without a source id: the extractor emitted a plain item as one ListItem with the same text.
            None=>blocks.iter().find(|b|matches!(b.kind,BlockKind::ListItem) && b.text==text && !claimed.contains(&b.block_id)).map(|b|b.block_id.clone()),
        };
        if let Some(id)=&id {claimed.insert(id.clone());}
        let pair=paired.captures(&text);
        let gs:Vec<_>=grade.captures_iter(&text).collect();
        if pair.is_none() && gs.is_empty(){if text.contains("Grade ") {warnings.push(format!("Recommendation {shown} has a complex grade clause; its exact text is retained without a shared grade."));}continue;}
        let Some((i,b))=id.as_ref().and_then(|id|blocks.iter().enumerate().find(|(_,b)|&b.block_id==id)) else {warnings.push(format!("Explicitly graded recommendation {shown} spans nested blocks; text retained without a structured grade."));continue};
        let id=b.block_id.clone();
        if let Some(c)=pair {
            if gs.is_empty() && paired.find_iter(&text).count()==1 && text.matches("Grade ").count()==2 {
                for (j,(strength,certainty,scope)) in [(1,2,3),(4,5,6)].into_iter().enumerate() {
                    out.push((i,Block {block_id:format!("{id}-recommendation-{}",j+1),kind:BlockKind::Recommendation{label:label.clone(),strength:Some(c[strength].to_string()),certainty:Some(c[certainty].to_string()),population:None,grading_system:None,qualifier:Some(format!("for {}",c[scope].trim()))},text:text.clone(),heading_path:b.heading_path.clone(),page:None}));
                }
            } else {warnings.push(format!("Recommendation {shown} has additional grade clauses; source text retained without shared grades."));}
            continue;
        }
        if gs.len()!=1 || text.matches("Grade ").count()!=1 {warnings.push(format!("Recommendation {shown} has multiple grade clauses; text retained without a shared grade."));continue;}
        out.push((i,Block {block_id:format!("{id}-recommendation"),kind:BlockKind::Recommendation{label:label.clone(),strength:Some(gs[0][1].to_string()),certainty:Some(gs[0][2].to_string()),population:None,grading_system:None,qualifier:gs[0].get(3).map(|m|format!("for {}",m.as_str()))},text,heading_path:b.heading_path.clone(),page:None}));
    }
    for (i,b) in out.into_iter().rev(){blocks.insert(i+1,b);}
    warnings.push("Only locally unambiguous grade clauses are structured; nested conditions, citation context, and all recommendation coverage require source comparison.".into());warnings
}
#[async_trait::async_trait]
impl Adapter for DiabetesCanada {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0){return Ok(vec![])}let page=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;
        let list=catalog(&page.text(),&Url::parse(&page.record.final_url)?)?;let mut out=vec![];
        for (url,title,chapter,edition,archived) in list {
            let work=format!("chapter-{chapter}");let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:url.clone(),title:Some(title),rendition:Some("html".into()),version:edition.clone(),extra:json!({"work_key":work,"archived":archived,"catalog_fetch":&page.record})};
            match f.get_ok(&url).await {Ok(p)=>match clinical(&p.text()) {Ok((_,body,_))=>{let sha=crate::common::sha256_hex(body.as_bytes());row.doc_key=format!("{work}-{}",&sha[..12]);row.extra["clinical_sha256"]=json!(sha);},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
            out.push(row);if limit.is_some_and(|n|out.len()>=n){break;}
        }Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("Diabetes Canada discovery: {e}");}let page=f.get_ok(&row.url).await?;
        let (title,html,mut attachments)=clinical(&page.text())?;
        if row.extra["clinical_sha256"].as_str().is_some_and(|s|s!=crate::common::sha256_hex(html.as_bytes())){bail!("Chapter changed since discovery; re-plan");}
        let base=Url::parse(&page.record.final_url)?;for a in &mut attachments {if let Some(url)=a["href"].as_str().and_then(|h|common::link(&base,h)){a["url"]=json!(url);}}
        let (mut blocks,mut warnings)=blocks_from_html(&html,Some(&base),Some("#clinical"));warnings.extend(recommendations(&html,&mut blocks));
        if !Html::parse_fragment(&html).select(&sel(".chapterRecommendations")).any(|_|true) {warnings.push("This chapter's full text has no recommendation list (e.g. an introduction or methods chapter); no recommendations are structured.".into());}
        warnings.push("HTML and linked PDF are distinct publisher renditions; PDF and figure assets have not been acquired or compared. Figure layout, tables, footnotes and nested recommendations need source validation.".into());
        let mut rights=Rights{status:"unknown".into(),terms_url:Some(TERMS.into()),note:Some("Publisher terms require review; no consent is recorded.".into()),..Default::default()};
        let mut fetches=vec![];if let Ok(c)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(c);}fetches.push(page.record.clone());
        match f.get_ok(TERMS).await {
            Ok(terms)=>{match source_rights(&terms.text()) {Ok(r)=>rights=r,Err(e)=>warnings.push(format!("Diabetes Canada rights terms not recognized: {e:#}"))}fetches.push(terms.record);},
            Err(e)=>warnings.push(format!("Diabetes Canada rights terms unavailable: {e:#}")),
        }

        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:page.record.final_url.clone(),item_type:Some("guideline".into()),version:row.version.clone(),status:if row.extra["archived"]==true {"archived".into()}else{"unknown".into()},rights,blocks,attachments,fetches,warnings,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/diabetes_canada/catalog.html"));
    const TERMS_HTML:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/diabetes_canada/terms.html"));
    const ARCHIVED:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/diabetes_canada/chapter29-archived.html"));
    const RETINOPATHY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/diabetes_canada/chapter30.html"));
    const CH:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/diabetes_canada/chapter29.html"));
    #[test]
    fn real_catalog_retains_2025_and_archived_rows() {
        let rows=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();
        assert!(rows.iter().any(|(u,_,n,v,a)|u.ends_with("/chapter-29-2025-update") && n=="29" && v.as_deref()==Some("2025 UPDATE") && !a));
        assert!(rows.iter().any(|(u,_,n,v,a)|u.ends_with("/chapter-29") && n=="29" && v.as_deref()==Some("2018 ARCHIVED") && *a));
    }
    #[test]
    fn real_chapter_full_text_and_figures_without_tab_repetition() {
        let (_,body,_)=clinical(CH).unwrap();let base=Url::parse(ROOT).unwrap();
        {
            let doc=Html::parse_fragment(&body);
            let groups:Vec<_>=doc.select(&sel(".chapterFigure")).collect();
            assert_eq!(groups.len(),13);
            for id in ["fig1","fig2","fig3","fig4","fig5","fig6","supplementary-fig1","supplementary-fig2","supplementary-fig3","supplementary-fig4","supplementary-fig5","supplementary-fig6","supplementary-fig7"] {
                assert_eq!(groups.iter().filter(|g|g.value().id()==Some(id)).count(),1,"figure group {id}");
            }
        }
        let (mut blocks,_)=blocks_from_html(&body,Some(&base),Some("#clinical"));let warnings=recommendations(&body,&mut blocks);
        let recommendation_labels:std::collections::HashSet<_>=blocks.iter().filter_map(|b|if let BlockKind::Recommendation{label:Some(s),..}=&b.kind {Some(s.as_str())} else {None}).collect();
        assert_eq!(recommendation_labels.len(),11);
        for n in 1..=11 {assert!(recommendation_labels.contains(n.to_string().as_str()));}
        let first=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(s),..} if s=="1")).unwrap();
        assert!(first.text.contains("no history of kidney disease") && first.text.contains("5 years after onset") && first.text.contains("puberty"));
        assert!(matches!(&first.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Grade D" && c=="Consensus"));
        let third=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(s),..} if s=="3")).unwrap();
        assert!(matches!(&third.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Grade 1A" && c=="Level 2"));
        let seventh:Vec<_>=blocks.iter().filter(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(s),..} if s=="7")).collect();
        assert_eq!(seventh.len(),2);
        assert!(matches!(&seventh[0].kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),qualifier:Some(q),..} if s=="Grade A" && c=="Level 1A" && q.contains("ACEi use in type 1 and type 2 diabetes") && q.contains("ARB use in type 2 diabetes")));
        assert!(matches!(&seventh[1].kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),qualifier:Some(q),..} if s=="Grade D" && c=="Consensus" && q=="for ARB use in type 1 diabetes"));
        let ninth=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(s),..} if s=="9")).unwrap();
        assert!(matches!(&ninth.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),qualifier:Some(q),..} if s=="Grade A" && c=="Level 1A" && q=="for subcutaneous semaglutide"));
        let figure_urls:Vec<_>=blocks.iter().filter_map(|b|if let BlockKind::Figure{src_url:Some(u),..}=&b.kind{Some(u.as_str())}else{None}).collect();
        for suffix in ["Ch29-2025-Supplementary-Fig5.webp", "Ch29-2025-Supplementary-Fig5b.webp", "Ch29-2025-Supplementary-Fig5c.webp", "Ch29-2025-Supplementary-Fig5d.webp"] {
            assert_eq!(figure_urls.iter().filter(|u|u.contains(suffix)).count(),1,"figure asset {suffix}");
        }
        assert!(warnings.iter().any(|w|w.contains("coverage")));
        assert!(clinical("<main>Sign in</main>").is_err());
        for alternate in [ARCHIVED,RETINOPATHY] {let (_,h,_)=clinical(alternate).unwrap();assert!(h.contains("chapterRecommendations") && h.contains("mainText"));}
        let rights=source_rights(TERMS_HTML).unwrap();
        assert!(rights.restrictions.iter().any(|s|s.contains("indexed, catalogued, merged with other data") && s.contains("express written consent")));
        assert!(rights.restrictions.iter().any(|s|s.contains("personal, non-commercial and not-for-profit use only")));

    }
    #[tokio::test]
    async fn offline_discovery_and_extraction_preserve_edition_and_assets() {
        let row={let doc=Html::parse_document(CAT);let selected=doc.select(&sel("#table-of-contents tr")).find(|tr|tr.select(&sel("a[href]")).any(|a|a.value().attr("href")==Some("/chapter-29-2025-update"))).unwrap().html();selected};
        let bounded=format!("<div id='table-of-contents'><table>{row}</table></div>");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");f.seed_cache_for_tests(TERMS,TERMS_HTML.as_bytes(),"text/html");
        let chapter="https://www.diabetes.ca/chapter-29-2025-update";f.seed_cache_for_tests(chapter,CH.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"diabetes_canada","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let rows=DiabetesCanada.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),1);let plan=&rows[0];
        assert_eq!(plan.version.as_deref(),Some("2025 UPDATE"));
        let doc=DiabetesCanada.extract(&src,&f,plan).await.unwrap();
        assert_eq!(doc.work_key.as_deref(),Some("chapter-29"));assert_eq!(doc.rights.status,"restricted");
        assert!(doc.attachments.iter().any(|a|a["role"]=="publisher_pdf_rendition" && a["url"].as_str().is_some_and(|u|u.contains("Ch29-Chronic-Kidney-Disease-in-Diabetes-2025-update.pdf"))));
        assert!(doc.warnings.iter().any(|w|w.contains("distinct publisher renditions")));
        let changed=CH.replace("Individuals with diabetes and no history of kidney disease should be screened annually", "Individuals with diabetes and no history of kidney disease should be screened differently");
        assert_ne!(changed,CH);f.seed_cache_for_tests(chapter,changed.as_bytes(),"text/html");
        assert!(DiabetesCanada.extract(&src,&f,plan).await.is_err());
    }
    const CH1:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/diabetes_canada/chapter1.html"));
    const CH3:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/diabetes_canada/chapter3.html"));
    #[test]
    fn idless_items_are_structured_and_repeated_lists_once() {
        let html="<div id='clinical'><div class='mainText'><p>Text.</p></div><div class='chapterRecommendations'><h2>Recommendations</h2><ol><li>Offer X to adults [Grade A, Level 1A].</li><li>Consider Y [Grade D, Consensus].</li><li>Discuss Z.</li></ol></div><div class='chapterRecommendations'><h2>Recommendations</h2><ol><li>Offer X to adults [Grade A, Level 1A].</li><li>Consider Y [Grade D, Consensus].</li><li>Discuss Z.</li></ol></div></div>";
        let (mut blocks,_)=blocks_from_html(html,None,Some("#clinical"));let w=recommendations(html,&mut blocks);
        let r:Vec<_>=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();
        assert_eq!(r.len(),2,"{w:?}");
        assert!(matches!(&r[0].kind,BlockKind::Recommendation{label:Some(l),strength:Some(s),certainty:Some(c),..} if l=="1" && s=="Grade A" && c=="Level 1A"));
        assert!(matches!(&r[1].kind,BlockKind::Recommendation{label:Some(l),strength:Some(s),certainty:Some(c),..} if l=="2" && s=="Grade D" && c=="Consensus"));
        assert!(w.iter().any(|x|x.contains("exact repeat")));
    }
    #[test]
    fn real_intro_chapter_and_repeated_nested_list() {
        let (title,body,_)=clinical(CH1).unwrap();assert_eq!(title,"Introduction");assert!(!body.contains("chapterRecommendations"));
        let (title,body,_)=clinical(CH3).unwrap();assert!(title.starts_with("Definition, Classification and Diagnosis"));
        let (mut blocks,_)=blocks_from_html(&body,Some(&Url::parse(ROOT).unwrap()),Some("#clinical"));let w=recommendations(&body,&mut blocks);
        assert!(w.iter().any(|x|x.contains("exact repeat")),"{w:?}");
        // both top-level statements carry several grade clauses on nested criteria: kept as text, each with a warning
        assert_eq!(w.iter().filter(|x|x.starts_with("Recommendation ")||x.starts_with("Explicitly graded recommendation ")).count(),2,"{w:?}");
        assert!(blocks.iter().any(|b|b.text.contains("FPG ≥7.0 mmol/L")));
        assert!(clinical("<div class='chapter-banner'><h1 class='page-title'>X</h1></div><div id='chapterTabsContent'><div id='full-text'><div class='chapter-content'><p>Sign in</p></div></div></div>").is_err());
    }
    #[tokio::test]
    async fn offline_intro_chapter_is_a_document() {
        let row={let doc=Html::parse_document(CAT);doc.select(&sel("#table-of-contents tr")).find(|tr|tr.select(&sel("a[href]")).any(|a|a.value().attr("href")==Some("/chapter-1"))).unwrap().html()};
        let bounded=format!("<div id='table-of-contents'><table>{row}</table></div>");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");f.seed_cache_for_tests(TERMS,TERMS_HTML.as_bytes(),"text/html");
        f.seed_cache_for_tests("https://www.diabetes.ca/chapter-1",CH1.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"diabetes_canada","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let rows=DiabetesCanada.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),1);assert!(rows[0].extra["discovery_error"].is_null(),"{:?}",rows[0].extra);
        let doc=DiabetesCanada.extract(&src,&f,&rows[0]).await.unwrap();
        assert_eq!(doc.title,"Introduction");assert_eq!(doc.work_key.as_deref(),Some("chapter-1"));
        assert!(doc.blocks.len()>10 && !doc.blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})));
        assert!(doc.warnings.iter().any(|w|w.contains("no recommendation list")));
    }
}
