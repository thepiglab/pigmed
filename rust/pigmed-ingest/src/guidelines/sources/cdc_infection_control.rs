//! CDC infection-control library: navigational guideline families and clinical chapters.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Relation,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;

pub struct CdcInfectionControl;
const ROOT:&str="main #content";
fn catalog(html:&str,base:&Url)->Result<Vec<(String,String)>> {
    let doc=Html::parse_document(html);
    if doc.select(&sel(ROOT)).next().is_none(){bail!("CDC infection-control library root missing");}
    let mut out=Vec::new(); let mut seen=HashSet::new();
    for a in doc.select(&sel("main #content .dfe-curated-link__title a[href]")) {
        let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
        if seen.insert(url.clone()) {out.push((url,text_of(&a)));}
    }
    if out.is_empty(){bail!("CDC infection-control library has no curated guidance links");}
    Ok(out)
}
fn clinical(html:&str,base:&Url)->Result<(String,Vec<Block>,Vec<String>)> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(ROOT)).next().context("CDC infection-control content root missing")?;
    let title=doc.select(&sel("main .cdc-page-title h1")).next().map(|e|text_of(&e)).context("CDC infection-control page title missing")?;
    // The 2007 family landing has only a short summary, print link and updates;
    // it is not a clinical chapter. Never substitute it for the linked full source.
    if root.select(&sel(".cdc-dfe-body__center")).next().is_some() && text_of(&root).split_whitespace().count()<200 {
        bail!("CDC family landing lacks terminal clinical body");
    }
    let (mut blocks,mut warnings)=blocks_from_html(html,Some(base),Some(ROOT));
    let source_tables=root.select(&sel("table")).collect::<Vec<_>>();
    let table_ids=blocks.iter().filter_map(|b|if matches!(b.kind,BlockKind::Table{..}) {Some(b.block_id.clone())} else {None}).collect::<Vec<_>>();
    if source_tables.len()!=table_ids.len() {warnings.push("Source and extracted table counts differ; row extraction needs review.".into());}
    else {
        let mut additions=Vec::new();
        for (table,table_id) in source_tables.into_iter().zip(table_ids) {
            let headers=table.select(&sel("thead th")).map(|x|text_of(&x)).collect::<Vec<_>>();
            if headers != ["Infection/Condition","Type of Precaution","Duration of Precaution","Precautions/Comments"] {continue;}
            let mut statements=Vec::new();
            for (i,row) in table.select(&sel("tbody tr")).enumerate() {
                let cells=common::cells(row);
                if cells.len()!=4 || cells.iter().any(|c|c.value().attr("rowspan").is_some()||c.value().attr("colspan").is_some()) {
                    warnings.push(format!("CDC precaution table {table_id} row {} has unsupported cells/spans; preserved in table only.",i+1));continue;
                }
                let fields=cells.iter().map(text_of).collect::<Vec<_>>();
                if fields[0].is_empty(){warnings.push(format!("CDC precaution table {table_id} row {} lacks infection/condition.",i+1));continue;}
                if fields[1..].iter().all(|s|s.eq_ignore_ascii_case("n/a")) {continue;} // cross-reference row stays in table
                statements.push(Block {block_id:format!("{table_id}-row-{}",i+1),kind:BlockKind::Recommendation {label:None,strength:None,certainty:None,
                    population:Some(fields[0].clone()),grading_system:None,qualifier:None},text:fields.join(" | "),heading_path:vec![],page:None});
            }
            additions.push((table_id,statements));
        }
        for (table_id,statements) in additions.into_iter().rev() {common::insert_table_recommendations(&mut blocks,&table_id,statements)?;}
    }
    if blocks.is_empty(){bail!("CDC infection-control page has no clinical content");}
    if blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})) {
        warnings.push("Precaution tables are retained; four-cell infection rows also become source-scoped recommendations with their own comments and updates. Unsupported merged cells or nonstandard rows remain in tables only.".into());
    }
    warnings.push("Scope-specific update notices and linked baseline guideline renditions require independent reconciliation.".into());
    Ok((title,blocks,warnings))
}

fn rights(html:&str,url:&str)->Rights {
    let doc=Html::parse_document(html);
    let paragraphs=doc.select(&sel("main .syndicate p")).map(|e|text_of(&e)).collect::<Vec<_>>();
    Rights {status:"unknown".into(),terms_url:Some(url.into()),evidence_urls:vec![url.into()],
        note:Some("CDC's general agency terms have exceptions; applicability to each document and asset remains unverified.".into()),
        attribution:paragraphs.iter().find(|p|p.starts_with("1) Attribution")).cloned(),
        restrictions:paragraphs.iter().filter(|p|p.starts_with("There are, however, a few exceptions.")||p.starts_with("2) You must")||p.starts_with("3) You may not")||p.starts_with("4) You must")).cloned().collect(),..Default::default()}
}
fn hash_html(html:&str)->Result<String> {
    let doc=Html::parse_document(html);let root=doc.select(&sel(ROOT)).next().context("CDC clinical root missing")?;
    Ok(crate::common::sha256_hex(root.html().as_bytes()))
}
fn wrapper_links(html:&str,base:&Url)->Result<Vec<(String,String)>> {
    let doc=Html::parse_document(html);let root=doc.select(&sel(ROOT)).next().context("CDC family landing root missing")?;
    let mut out=Vec::new();let mut seen=HashSet::new();
    for a in root.select(&sel("a[href]")) {
        let label=text_of(&a);let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
        let target=Url::parse(&url)?;if target.origin()!=base.origin(){continue;}
        let lower=label.to_lowercase();let path=target.path().to_lowercase();
        let full_pdf=path.ends_with(".pdf") && lower.contains("guideline") && !lower.contains("summary") && !lower.contains("quick");
        let updates=path.ends_with("/updates.html");
        if (full_pdf||updates)&&seen.insert(url.clone()){out.push((url,if full_pdf {"pdf".into()} else {"updates".into()}));}
    }
    Ok(out)
}

#[async_trait::async_trait]
impl Adapter for CdcInfectionControl {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0){return Ok(vec![]);}
        let page=f.get_ok(src.catalog_url.as_deref().context("CDC library URL missing")?).await?;
        let base=Url::parse(&page.record.final_url)?;let topics=catalog(&page.text(),&base)?;
        let mut out=Vec::new();let mut seen=HashSet::new();
        for (topic,title) in topics {
            let topic_url=Url::parse(&topic)?;
            let family=topic_url.path_segments().and_then(|s|s.filter(|x|!x.is_empty()&&*x!="index.html").last()).unwrap_or("guideline").to_string();
            let mut candidates=Vec::new();
            if topic_url.origin()!=base.origin() {
                candidates.push((topic.clone(),"unresolved".to_string(),Some("External catalog resource requires source and rights review".to_string())));
            } else {
                match f.get_ok(&topic).await {
                    Ok(page)=>{
                        let body=page.text();let page_base=Url::parse(&page.record.final_url)?;
                        if page.is_pdf(){candidates.push((topic.clone(),"pdf".into(),None));}
                        else if clinical(&body,&page_base).is_ok(){candidates.push((topic.clone(),"html".into(),None));}
                        else {
                            match wrapper_links(&body,&page_base) {
                                Ok(links)=>for (url,role) in links {candidates.push((url,role,None));},
                                Err(e)=>candidates.push((topic.clone(),"unresolved".into(),Some(format!("Unrecognized family template: {e:#}")))),
                            }
                            // Exact Appendix A route documented in the source dossier and
                            // validated separately; the index itself has no chapter links.
                            if topic_url.path()=="/infection-control/hcp/isolation-precautions/index.html" {
                                candidates.push(("https://www.cdc.gov/infection-control/hcp/isolation-precautions/appendix-a-type-duration.html".into(),"appendix".into(),None));
                            }
                            if candidates.is_empty(){candidates.push((topic.clone(),"unresolved".into(),Some("Family page has no validated full clinical source".into())));}
                        }
                    },
                    Err(e)=>candidates.push((topic.clone(),"unresolved".into(),Some(format!("{e:#}")))),
                }
            }
            for (url,role,error) in candidates {
                if !seen.insert(url.clone()){continue;}
                let suffix=common::short_hash(&url);
                let mut row=PlanRow {source_id:src.id.clone(),doc_key:format!("{family}-{suffix}-unresolved"),url,title:Some(title.clone()),
                    rendition:Some(if role=="pdf" {"pdf"} else {"html"}.into()),version:None,extra:json!({"work_key":family,"parent_url":topic,"role":role})};
                if let Some(error)=error {row.extra["discovery_error"]=json!(error);}
                else { match f.get_ok(&row.url).await {
                    Ok(got)=>{
                        let hash=if role=="pdf" {if got.is_pdf(){Ok(got.record.sha256.clone())}else{Err(anyhow::anyhow!("CDC linked PDF is not PDF bytes"))}}
                            else {hash_html(&got.text())};
                        match hash {Ok(hash)=>{row.doc_key=format!("{family}-{suffix}-{}",&hash[..12]);row.extra["content_sha256"]=json!(hash);},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
                    },
                    Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                }}
                out.push(row);if limit.is_some_and(|n|out.len()>=n){return Ok(out);}
            }
        }
        Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("CDC infection-control discovery: {e}");}
        let policy_url="https://www.cdc.gov/other/agencymaterials.html";let policy=f.get_ok(policy_url).await?;
        let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;
        let role=row.extra["role"].as_str().unwrap_or("html");
        let parent=row.extra["parent_url"].as_str().unwrap_or_default();
        let mut dates=Vec::new();
        let (title,blocks,mut warnings)=if role=="pdf" {
            if !page.is_pdf(){bail!("CDC linked guideline is not PDF bytes");}
            if row.extra["content_sha256"].as_str().is_some_and(|h|h!=page.record.sha256){bail!("CDC PDF changed since planning");}
            let (blocks,mut warnings,title)=super::static_list::blocks_for(&page,None,30).await?;
            warnings.push("PDF figures, page layout and any text outside extraction need separate fidelity review.".into());
            (title.or_else(||row.title.clone()).unwrap_or_else(||row.url.clone()),blocks,warnings)
        } else {
            let html=page.text();
            if row.extra["content_sha256"].as_str().is_some_and(|h|hash_html(&html).map(|x|x!=h).unwrap_or(true)){bail!("CDC page changed since planning");}
            let result=clinical(&html,&base)?;
            let doc=Html::parse_document(&html);
            for (selector,label) in [("meta[property='cdc:first_published']","cdc:first_published"),("meta[property='cdc:last_updated']","cdc:last_updated"),("meta[property='cdc:last_reviewed']","cdc:last_reviewed")] {
                if let Some(raw)=doc.select(&sel(selector)).next().and_then(|e|e.value().attr("content")) {dates.push(common::date(label,raw,base.as_str()));}
            }
            result
        };
        warnings.push("CDC page metadata dates describe the page, not every underlying recommendation or the 2007 guideline edition.".into());
        let relations=if parent.is_empty()||parent==row.url {vec![]}else{vec![Relation{rel:if role=="updates" {"updates"} else {"part_of"}.into(),target:parent.into(),scope:None,evidence_url:Some(row.url.clone())}]};
        let policy_rights=rights(&policy.text(),policy_url);
        let mut fetches=vec![page.record,policy.record];
        if !parent.is_empty()&&parent!=row.url {let source=f.get_ok(parent).await?;fetches.push(source.record);}
        Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),
            item_type:Some(if role=="updates" {"update_history"} else if role=="appendix" {"guideline_chapter"} else {"guideline"}.into()),
            title,url:row.url.clone(),status:"unknown".into(),version:None,
            published:dates.iter().find(|d|d.label=="cdc:first_published").and_then(|d|d.normalized.clone()),
            updated:dates.iter().find(|d|d.label=="cdc:last_updated").and_then(|d|d.normalized.clone()),date_observations:dates,relations,
            rights:policy_rights,blocks,warnings,fetches,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const LIBRARY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_infection_control/catalog.html"));
    const LANDING:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_infection_control/isolation-index.html"));
    const APPENDIX:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_infection_control/appendix-a.html"));
    const TERMS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_infection_control/terms.html"));
    const UPDATES:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_infection_control/isolation-updates.html"));
    #[test]
    fn saved_library_and_isolation_roles() {
        let base=Url::parse("https://www.cdc.gov/infection-control/hcp/guidance/").unwrap();
        let links=catalog(LIBRARY,&base).unwrap();
        assert!(links.iter().any(|(u,_)|u.ends_with("/isolation-precautions/index.html")));
        let page=Url::parse("https://www.cdc.gov/infection-control/hcp/isolation-precautions/index.html").unwrap();
        assert!(clinical(LANDING,&page).unwrap_err().to_string().contains("landing"));
        let update=Url::parse("https://www.cdc.gov/infection-control/hcp/isolation-precautions/updates.html").unwrap();
        let (title,blocks,_)=clinical(UPDATES,&update).unwrap();
        assert_eq!(title,"Updates");
        assert!(blocks.iter().any(|b|b.text.contains("September 2024") && b.text.contains("Appendix A")));
        assert!(blocks.iter().any(|b|b.text.contains("June 2023") && b.text.contains("Table 5")));
    }
    #[tokio::test]
    async fn source_library_direct_pdf_is_not_parsed_as_an_html_family() {
        const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_infection_control/rr6210-page-1.pdf"));
        let library="https://www.cdc.gov/infection-control/hcp/guidance/";
        let url="https://www.cdc.gov/mmwr/pdf/rr/rr6210.pdf";
        let parsed=Html::parse_document(LIBRARY);
        let anchor=parsed.select(&sel(".dfe-curated-link__title a[href]")).find(|a|a.value().attr("href")==Some(url)||a.value().attr("href")==Some("/mmwr/pdf/rr/rr6210.pdf")).expect("genuine library direct-PDF anchor").html();
        let bounded=format!("<main><div id='content'><div class='dfe-curated-link__title'>{anchor}</div></div></main>");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(library,bounded.as_bytes(),"text/html");f.seed_cache_for_tests(url,PDF,"application/pdf");
        f.seed_cache_for_tests("https://www.cdc.gov/other/agencymaterials.html",TERMS.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"cdc_infection_control","access":"public","enabled":true,"catalog_url":library})).unwrap();
        let rows=CdcInfectionControl.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),1);
        assert_eq!(rows[0].url,url);assert_eq!(rows[0].rendition.as_deref(),Some("pdf"));assert!(rows[0].extra["discovery_error"].is_null());
        let doc=CdcInfectionControl.extract(&src,&f,&rows[0]).await.unwrap();
        let text=doc.blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");
        assert!(text.contains("Hepatitis B Virus Protection"));assert_eq!(doc.rights.status,"unknown");
        assert!(doc.warnings.iter().any(|w|w.contains("separate fidelity review")));
    }
    #[tokio::test]
    async fn actual_isolation_graph_and_appendix_rows_offline() {
        let library="https://www.cdc.gov/infection-control/hcp/guidance/";
        let landing="https://www.cdc.gov/infection-control/hcp/isolation-precautions/index.html";
        let updates="https://www.cdc.gov/infection-control/hcp/isolation-precautions/updates.html";
        let appendix="https://www.cdc.gov/infection-control/hcp/isolation-precautions/appendix-a-type-duration.html";
        let policy="https://www.cdc.gov/other/agencymaterials.html";
        let needle="<a class=\"cdc-block-link\" href=\"/infection-control/hcp/isolation-precautions/index.html\">Isolation Precautions Guideline</a>";
        assert!(LIBRARY.contains(needle));
        let one=format!("<main><div id='content'><div class='dfe-curated-link__title'>{needle}</div></div></main>");
        let d=tempfile::tempdir().unwrap();let f=Fetcher::new(d.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(library,one.as_bytes(),"text/html");f.seed_cache_for_tests(landing,LANDING.as_bytes(),"text/html");
        f.seed_cache_for_tests(updates,UPDATES.as_bytes(),"text/html");f.seed_cache_for_tests(appendix,APPENDIX.as_bytes(),"text/html");
        f.seed_cache_for_tests(policy,TERMS.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"cdc_infection_control","access":"public","enabled":true,"catalog_url":library})).unwrap();
        let rows=CdcInfectionControl.discover(&src,&f,None).await.unwrap();
        assert_eq!(rows.len(),3);
        assert!(rows.iter().any(|r|r.rendition.as_deref()==Some("pdf") && r.extra["discovery_error"].is_string()));
        assert!(rows.iter().any(|r|r.url==updates));
        let row=rows.iter().find(|r|r.url==appendix).unwrap();
        let doc=CdcInfectionControl.extract(&src,&f,row).await.unwrap();
        assert_eq!(doc.item_type.as_deref(),Some("guideline_chapter"));
        assert_eq!(doc.status,"unknown");assert!(doc.version.is_none());
        assert_eq!(doc.updated.as_deref(),Some("2026-09-11"));
        assert_eq!(doc.published.as_deref(),Some("2024-05-15"));
        assert_eq!(doc.rights.status,"unknown");assert!(!doc.rights.restrictions.is_empty());
        assert_eq!(doc.fetches.len(),3);
        assert_eq!(doc.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),24);
        let recs=doc.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect::<Vec<_>>();
        assert!(recs.len()>100);
        assert!(recs.iter().any(|b|b.text.contains("Andes virus") && b.text.contains("September 2024") && b.text.contains("N95")));
        assert!(recs.iter().any(|b|b.text.contains("Noroviruses") && b.text.contains("April 2019") && b.text.contains("Contact + Standard")));
        assert!(recs.iter().all(|b|matches!(&b.kind,BlockKind::Recommendation{strength:None,certainty:None,..})));
        assert!(doc.relations.iter().any(|r|r.rel=="part_of" && r.target==landing));
    }

}
