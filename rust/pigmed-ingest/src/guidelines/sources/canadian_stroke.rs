//! Canadian Stroke Best Practices module/section catalog and clinical section HTML.
use super::{common, static_list::blocks_for, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::json;
use std::collections::{BTreeMap,HashSet};
use url::Url;

pub struct CanadianStroke;
const CLINICAL:&str=".bp-right-column";

fn catalog(html:&str,base:&Url)->Result<Vec<(String,String,Option<String>)>> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(".bp-page-modules .sections")).next().context("Stroke recommendations module catalog missing")?;
    let mut entries=BTreeMap::new();
    for section in root.select(&sel(".section")) {
        let module=section.select(&sel(".section-url a[href]")).next().and_then(|a|common::link(base,a.value().attr("href")?));
        let mut n=0;
        for a in section.select(&sel(".section-link a[href]")) {
            let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};
            let u=Url::parse(&url)?;
            if u.origin()!=base.origin() || !u.path().starts_with("/recommendations/") || u.query().is_some() {continue;}
            entries.entry(url).or_insert((text_of(&a),module.clone()));n+=1;
        }
        // Source modules without exposed children need their own scoped review.
        if n==0 {
            if let Some(url)=module {
                let u=Url::parse(&url)?;
                if u.origin()==base.origin() && u.path().starts_with("/recommendations/") {
                    let title=section.select(&sel(".section-url a[href]")).next().map(|a|text_of(&a)).unwrap_or_default();
                    entries.entry(url).or_insert((title,None));
                }
            }
        }
    }
    if entries.is_empty() {bail!("Stroke catalog has no module sections");}
    Ok(entries.into_iter().map(|(u,(t,m))|(u,t,m)).collect())
}

fn module_sections(html:&str,module:&Url)->Result<Vec<(String,String)>> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(".bp-right-column .bp-page-modules .sections")).next().context("Stroke module section inventory missing")?;
    let prefix=format!("{}/",module.path().trim_end_matches('/'));
    let mut rows=BTreeMap::new();
    for a in root.select(&sel(".section > .section-url a[href]")) {
        let Some(url)=a.value().attr("href").and_then(|h|common::link(module,h)) else {continue};
        let u=Url::parse(&url)?;
        let rest=u.path().strip_prefix(&prefix).unwrap_or("").trim_matches('/');
        if u.origin()==module.origin() && !rest.is_empty() && !rest.contains('/') && u.query().is_none() {rows.entry(url).or_insert_with(||text_of(&a));}
    }
    if rows.is_empty() {bail!("Stroke module has no recognized direct section links");}
    Ok(rows.into_iter().collect())
}
fn html_rights(html:&str,terms:&str)->Result<Rights> {
    let doc=Html::parse_document(html);
    let main=doc.select(&sel("main")).next().context("Stroke terms main content missing")?;
    let item=main.select(&sel("li")).map(|li|text_of(&li)).find(|t|t.contains("You may use our Services for your own personal, non-commercial use") && t.contains("prior written consent"))
        .context("Stroke personal-use restriction not recognized")?;
    let documentation=main.select(&sel("li")).map(|li|text_of(&li)).find_map(|t| {
        let start=t.find("Except as described in these Terms of Use")?;
        let tail=&t[start..];let end=tail.find("without the prior written consent of HSF.")?+"without the prior written consent of HSF.".len();
        Some(tail[..end].to_string())
    }).context("Stroke documentation restriction not recognized")?;
    Ok(Rights {status:"restricted".into(),terms_url:Some(terms.into()),evidence_urls:vec![terms.into()],
        restrictions:vec![item,documentation],note:Some("Terms apply to this site HTML; linked and independently acquired assets require their own rights review.".into()),..Default::default()})
}

fn clinical_html(html:&str)->Result<(String,String)> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(CLINICAL)).next().context("Stroke clinical right column missing")?;
    let title=root.select(&sel(".bp-page-details h1.title")).next().map(|x|text_of(&x)).filter(|s|!s.is_empty()).context("Stroke section title missing")?;
    if root.select(&sel(".bp-page-section h6.title")).next().is_none() {bail!("Stroke clinical section headings missing");}
    let mut html=root.html();
    for el in root.select(&sel("script,style,.page-prevnext,.bp-header-cta")) {html=html.replace(&el.html(),"");}
    // Publisher's numbered subsection labels are bold paragraphs. Promote them
    // for context, and keep accordion titles even though their controls are JS.
    let numbered=regex::Regex::new(r"^\d+(?:\.\d+){1,3}\.?\s+").unwrap();
    for p in root.select(&sel(".bp-page-section p")) {
        if let Some(strong)=p.select(&sel("strong")).next() {
            if text_of(&p)==text_of(&strong) && numbered.is_match(&text_of(&p)) {
                html=html.replacen(&p.html(),&format!("<h3>{}</h3>",strong.inner_html()),1);
            }
        }
    }
    for h in root.select(&sel(".bp-page-section h6.title")) {
        html=html.replacen(&h.html(),&format!("<h2>{}</h2>",h.inner_html()),1);
    }
    for a in root.select(&sel(".summary-item-title")) {
        if let Some(t)=a.select(&sel(".title")).next() {html=html.replacen(&a.html(),&format!("<h2>{}</h2>",t.inner_html()),1);}
    }
    Ok((title,html))
}
fn direct_text(li:ElementRef<'_>)->String {
    let mut html=li.html();
    for list in li.select(&sel("ol,ul")) {html=html.replace(&list.html(),"");}
    let fragment=Html::parse_fragment(&html);
    let text=fragment.select(&sel("li")).next().map(|e|text_of(&e)).unwrap_or_default();
    text
}

fn recommendations(html:&str,blocks:&mut Vec<Block>)->Vec<String> {
    let doc=Html::parse_fragment(html); let mut warnings=vec![];
    let grade=regex::Regex::new(r"\[([^\[\]]+)\]").unwrap();
    let mut candidates=Vec::new();
    for section in doc.select(&sel(".bp-page-section")) {
        let Some(heading)=section.select(&sel("h2")).next().map(|h|text_of(&h)) else {continue};
        for li in section.select(&sel("li")) {
            let text=direct_text(li); if text.is_empty(){continue;}
            let grades:Vec<_>=grade.captures_iter(&text).filter_map(|c| {
                let (a,b)=c[1].split_once(';').or_else(||c[1].split_once(','))?;
                let (a,b)=(a.trim(),b.trim());
                (a.to_ascii_lowercase().contains("recommendation") && b.to_ascii_lowercase().contains("evidence")).then(||(a.to_string(),b.to_string()))
            }).collect();
            if grades.is_empty(){continue;}
            let parent=li.ancestors().skip(1).filter_map(ElementRef::wrap).find(|e|e.value().name()=="li").map(direct_text).filter(|s|!s.is_empty());
            let (strength,certainty)=if grades.len()==1 {(Some(grades[0].0.clone()),Some(grades[0].1.clone()))} else {
                warnings.push("Multiple distinct grade clauses occur in one recommendation; no shared grade assigned, source wording retained.".into());(None,None)
            };
            // When the graded parent introduces nested criteria, keep that
            // full condition list with the parent; child grades remain local.
            let full=if li.select(&sel("ol,ul")).next().is_some() {text_of(&li)} else {text.clone()};
            candidates.push((heading.clone(),text,full,parent,strength,certainty));
        }
    }
    let mut used=HashSet::new();let mut rows=Vec::new();
    for (heading,text,full,parent,strength,certainty) in candidates {
        let location=blocks.iter().enumerate().find(|(i,b)|!used.contains(i) && b.text==text && b.heading_path.iter().any(|h|h==&heading)).map(|(i,_)|i);
        if let Some(i)=location {
            used.insert(i);
            rows.push((i,Block{block_id:format!("{}-explicit-recommendation",blocks[i].block_id),
                kind:BlockKind::Recommendation{label:None,strength,certainty,population:None,grading_system:None,qualifier:parent},
                text:full,heading_path:blocks[i].heading_path.clone(),page:None}));
        } else {warnings.push(format!("An explicitly graded statement under {heading} could not be mapped to a source block; source text retained."));}
    }
    for (i,b) in rows.into_iter().rev(){blocks.insert(i+1,b);}
    warnings
}

fn asset_links(html:&str,base:&Url)->Vec<serde_json::Value> {
    let doc=Html::parse_fragment(html);let mut seen=BTreeMap::<String,usize>::new();let mut out:Vec<serde_json::Value>=Vec::new();
    let box_number=regex::Regex::new(r"(?i)^\d+[a-z]$").unwrap();
    let named_box=regex::Regex::new(r"(?i)\bbox\s*\d+[a-z]\b").unwrap();
    for a in doc.select(&sel("a[href]")) {
        let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};
        if !Url::parse(&url).ok().is_some_and(|u|u.path().to_ascii_lowercase().ends_with(".pdf")) {continue;}
        let label=text_of(&a);let l=label.to_lowercase();
        let context=a.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().name()=="li").map(|e|text_of(&e)).unwrap_or_default();
        let box_path=Url::parse(&url).ok().is_some_and(|u|u.path().to_lowercase().contains("box"));
        let is_box=named_box.is_match(l.trim()) || (box_number.is_match(l.trim()) && context.to_lowercase().contains("box") && box_path);
        let role=if is_box {"clinical_box"} else if l.contains("evidence") {"evidence"} else if l.contains("appendix") {"appendix"} else {"linked_pdf"};
        let mention=json!({"title":label,"source_context":context,"role":role});
        if let Some(i)=seen.get(&url).copied() {
            if let Some(mentions)=out[i]["source_mentions"].as_array_mut(){mentions.push(mention);}
            if role=="clinical_box" && out[i]["role"]!="clinical_box" {
                out[i]["role"]=json!(role);out[i]["title"]=json!(label);out[i]["source_context"]=json!(context);
            }
            continue;
        }
        seen.insert(url.clone(),out.len());
        out.push(json!({"url":url,"title":label,"source_context":context,"source_mentions":[mention],"role":role,"source_url":base.as_str(),"rights":"unknown; linked asset not inspected"}));
    }
    out
}

#[async_trait::async_trait]
impl Adapter for CanadianStroke {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0){return Ok(vec![])}
        let entry=src.catalog_url.as_deref().context("Stroke catalog URL missing")?;
        let page=f.get_ok(entry).await?;let base=Url::parse(&page.record.final_url)?;
        let catalog_rows=catalog(&page.text(),&base)?;
        let modules:HashSet<String>=catalog_rows.iter().filter_map(|(_,_,m)|m.clone()).collect();
        let mut entries:BTreeMap<String,(String,Option<String>)>=catalog_rows.into_iter().map(|(u,t,m)|(u,(t,m))).collect();
        let mut module_errors=Vec::new();let mut mismatches=BTreeMap::new();
        let mut ordered_modules:Vec<_>=modules.into_iter().collect();ordered_modules.sort();
        for module in ordered_modules {
            let result=match f.get_ok(&module).await {Ok(p)=>module_sections(&p.text(),&Url::parse(&module)?),Err(e)=>Err(e)};
            match result {
                Ok(rows)=>{
                    let listed:HashSet<_>=entries.iter().filter(|(_,(_,m))|m.as_deref()==Some(module.as_str())).map(|(u,_)|u.clone()).collect();
                    let module_set:HashSet<_>=rows.iter().map(|(u,_)|u.clone()).collect();
                    if listed!=module_set {mismatches.insert(module.clone(),format!("Catalog lists {} sections, module lists {}; inventory sets differ.",listed.len(),module_set.len()));}
                    for (u,t) in rows {entries.entry(u).or_insert((t,Some(module.clone())));}
                }
                Err(e)=>module_errors.push((module,format!("{e:#}"))),
            }
        }
        let mut out:Vec<PlanRow>=vec![];let mut box_index=BTreeMap::<String,usize>::new();let mut box_docs=BTreeMap::<String,usize>::new();
        for (module,error) in module_errors {
            let work=common::slug(Url::parse(&module)?.path().trim_matches('/'));
            out.push(PlanRow {source_id:src.id.clone(),doc_key:format!("{work}-module-inventory-unresolved"),url:module,title:None,rendition:Some("html".into()),version:None,
                extra:json!({"work_key":work,"discovery_error":format!("Module section inventory: {error}")})});
            if limit.is_some_and(|n|out.len()>=n){return Ok(out);}
        }
        for (url,(label,module)) in entries {
            let work=common::slug(Url::parse(&url)?.path().trim_matches('/'));
            let reconciliation=module.as_ref().and_then(|m|mismatches.get(m)).cloned();
            let mut row=PlanRow {source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url,title:Some(label),rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"module_url":module,"module_reconciliation":reconciliation})};
            let mut boxes=vec![];
            match f.get_ok(&row.url).await {
                Ok(page)=>match clinical_html(&page.text()) {
                    Ok((title,html))=>{
                        let hash=crate::common::sha256_hex(html.as_bytes());row.doc_key=format!("{work}-{}",&hash[..12]);row.title=Some(title);row.extra["clinical_sha256"]=json!(hash);
                        let section_no=regex::Regex::new(r"^(\d+)\.").unwrap().captures(row.title.as_deref().unwrap_or("")).map(|c|c[1].to_string());
                        let box_no=regex::Regex::new(r"(?i)^(?:box\s*)?(\d+)[a-z]$").unwrap();
                        boxes=asset_links(&html,&Url::parse(&page.record.final_url)?).into_iter().filter(|a| {
                            if a["role"]!="clinical_box" {return false;}
                            let number=a["title"].as_str().and_then(|t|box_no.captures(t.trim())).map(|c|c[1].to_string());
                            // A cross-reference to Box 4A from Section 5 does not
                            // establish that Box 4A is part of Section 5.
                            section_no.is_some() && number==section_no
                        }).collect();
                    }
                    Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                },
                Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
            }
            let parent=row.url.clone();out.push(row);
            if limit.is_some_and(|n|out.len()>=n){break;}
            for asset in boxes {
                let Some(box_url)=asset["url"].as_str().map(str::to_string) else {continue};
                if let Some(i)=box_index.get(&box_url).copied() {
                    if let Some(parents)=out[i].extra["parent_urls"].as_array_mut() {
                        if !parents.iter().any(|v|v.as_str()==Some(parent.as_str())) {parents.push(json!(parent));}
                    }
                    continue;
                }
                // URL query is preserved for acquisition, but work identity is based
                // on the source path. The actual PDF hash identifies this revision.
                let parsed=Url::parse(&box_url)?;
                let box_work=format!("clinical-box-{}",common::short_hash(&format!("{}://{}{}",parsed.scheme(),parsed.host_str().unwrap_or(""),parsed.path())));
                let mut item=PlanRow {source_id:src.id.clone(),doc_key:format!("{box_work}-unresolved-{}",common::short_hash(&box_url)),url:box_url.clone(),
                    title:asset["title"].as_str().map(String::from),rendition:Some("pdf".into()),version:None,
                    extra:json!({"work_key":box_work,"parent_urls":[parent],"source_label":asset["title"],"source_context":asset["source_context"],"source_mentions":asset["source_mentions"]})};
                match f.get_ok(&box_url).await {
                    Ok(pdf) if pdf.is_pdf()=>{item.doc_key=format!("{box_work}-{}",&pdf.record.sha256[..12]);item.extra["pdf_sha256"]=json!(pdf.record.sha256);}
                    Ok(_)=>item.extra["discovery_error"]=json!("Source-labelled clinical box did not return PDF bytes"),
                    Err(e)=>item.extra["discovery_error"]=json!(format!("{e:#}")),
                }
                if let Some(i)=box_docs.get(&item.doc_key).copied() {
                    if let Some(parents)=out[i].extra["parent_urls"].as_array_mut() {
                        if !parents.iter().any(|v|v.as_str()==Some(parent.as_str())) {parents.push(json!(parent));}
                    }
                    box_index.insert(box_url,i);continue;
                }
                box_docs.insert(item.doc_key.clone(),out.len());box_index.insert(box_url,out.len());out.push(item);
                if limit.is_some_and(|n|out.len()>=n){return Ok(out);}
            }
        }
        Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(error)=row.extra["discovery_error"].as_str(){bail!("Stroke discovery: {error}")}
        let page=f.get_ok(&row.url).await?;let base=Url::parse(&page.record.final_url)?;
        if row.rendition.as_deref()==Some("pdf") {
            if !page.is_pdf(){bail!("Stroke clinical box did not return PDF bytes");}
            if row.extra["pdf_sha256"].as_str().is_some_and(|h|h!=page.record.sha256) {bail!("Stroke clinical box changed since planning; re-plan");}
            let (blocks,mut warnings,_)=blocks_for(&page,None,30).await?;
            if blocks.is_empty(){bail!("Stroke clinical box yielded no text");}
            warnings.push("Clinical box PDF tables, footnotes and figures require visual validation; diagram branch topology and asset rights remain unknown.".into());
            let relations=row.extra["parent_urls"].as_array().into_iter().flatten().filter_map(|v|v.as_str()).map(|parent|Relation {
                rel:"part_of".into(),target:parent.into(),scope:Some("Publisher-labelled clinical box linked from this section".into()),evidence_url:Some(parent.into())
            }).collect();
            return Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),
                title:row.title.clone().unwrap_or_else(||row.url.clone()),url:row.url.clone(),item_type:Some("clinical_box".into()),status:"unknown".into(),
                rights:Rights{status:"unknown".into(),..Default::default()},relations,blocks,fetches:vec![page.record],warnings,..Default::default()});
        }
        let (title,html)=clinical_html(&page.text())?;
        if row.extra["clinical_sha256"].as_str().is_some_and(|h|h!=crate::common::sha256_hex(html.as_bytes())) {bail!("Stroke clinical section changed since planning; re-plan");}
        let (mut blocks,mut warnings)=blocks_from_html(&html,Some(&base),Some(CLINICAL));
        warnings.extend(recommendations(&html,&mut blocks));
        if let Some(mismatch)=row.extra["module_reconciliation"].as_str(){warnings.push(mismatch.into());}
        let attachments=asset_links(&html,&base);
        let (edition_text,section_update)={
            let doc=Html::parse_fragment(&html);
            let edition=doc.select(&sel(".bp-page-details .page-details p")).map(|p|text_of(&p)).find(|t|t.contains("Edition")).unwrap_or_default();
            let update=doc.select(&sel(".bp-page-section")).find(|s|s.select(&sel("h2")).any(|h|{let t=text_of(&h); t.starts_with("5.4 ") && t.to_lowercase().contains("endovascular thrombectomy")}))
                .and_then(|s|s.select(&sel("p")).map(|p|text_of(&p)).find(|p|p.contains("supersede") && p.contains("2025")));
            (edition,update)
        };
        let mut dates=vec![];
        let month_year=regex::Regex::new(r"(?i)(January|February|March|April|May|June|July|August|September|October|November|December)\s+(?:19|20)\d{2}").unwrap();
        if let Some(m)=month_year.find(&edition_text) {dates.push(common::date("Edition update",m.as_str(),base.as_str()));}
        if edition_text.is_empty() {warnings.push("Source edition/date statement was not recognized; no date inferred.".into());}
        if edition_text.contains("Endovascular Thrombectomy Interim Update 2025") {dates.push(common::date("Endovascular thrombectomy interim update","2025",base.as_str()));}
        let mut relations=vec![];
        if let Some(module)=row.extra["module_url"].as_str(){relations.push(Relation{rel:"part_of".into(),target:module.into(),scope:Some("Source catalog module/section relationship".into()),evidence_url:src.catalog_url.clone()});}
        warnings.push("Source-labelled clinical boxes require separate discovery and ingestion; links here do not confirm acquisition. Evidence tables and other linked assets are catalogued but not acquired or validated. Asset rights require separate review.".into());
        warnings.push("Only explicitly bracketed, locally scoped grades are structured; ungraded clinical instructions and nested conditions remain in source blocks. List marker labels and complete grade coverage require source comparison.".into());
        if section_update.is_some(){warnings.push("The 2025 update applies to section 5.4 and explicitly removes boxes 4B, 4C and 5C; it is not a whole-module version update. Predecessor identifiers were not established.".into());}
        let terms="https://www.strokebestpractices.ca/terms-of-use";
        let mut rights=Rights{status:"unknown".into(),..Default::default()}; let mut fetches=vec![page.record];
        match f.get_ok(terms).await {
            Ok(terms_page)=>{
                match html_rights(&terms_page.text(),terms) {Ok(r)=>rights=r,Err(e)=>warnings.push(format!("Stroke terms could not be interpreted: {e:#}"))}
                fetches.push(terms_page.record);
            }
            Err(e)=>warnings.push(format!("Stroke terms could not be fetched: {e:#}")),
        }
        Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),
            item_type:None,status:"unknown".into(),date_observations:dates,relations,rights,
            search_metadata:json!({"edition_statement":edition_text,"section_5_4_update":section_update,"date_scope":if section_update.is_some() {Some("Interim update applies to the named endovascular thrombectomy subsection, not the entire document")} else {None}}),
            blocks,attachments,fetches,warnings,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const TERMS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/canadian_stroke/terms.html"));
    const MODULE_HTML:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/canadian_stroke/module.html"));
    const TERMS_URL:&str="https://www.strokebestpractices.ca/terms-of-use";
    const CATALOG:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/canadian_stroke/catalog.html"));
    const BOX5A:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/canadian_stroke/box5a.pdf"));
    const BOX5A_URL:&str="https://www.strokebestpractices.ca/-/media/1-stroke-best-practices/acute-stroke-management/csbpr7-asm-box5a.pdf?rev=4fcdd17e8d784e4ca371312add88fd30";
    const ACUTE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/canadian_stroke/acute-ischemic.html"));
    const ROOT:&str="https://www.strokebestpractices.ca/recommendations";
    const MODULE:&str="https://www.strokebestpractices.ca/recommendations/acute-stroke-management";
    const SECTION:&str="https://www.strokebestpractices.ca/recommendations/acute-stroke-management/acute-ischemic-stroke-treatment";
    #[tokio::test]
    async fn real_module_graph_and_clinical_section_offline() {
        let links=catalog(CATALOG,&Url::parse(ROOT).unwrap()).unwrap();
        assert!(links.iter().any(|(url,_,module)|url==SECTION && module.as_deref()==Some(MODULE)));
        assert_eq!(links.iter().filter(|(url,_,_)|url==SECTION).count(),1);
        assert!(clinical_html("<main><h1>Navigation only</h1></main>").is_err());
        let module_rows=module_sections(MODULE_HTML,&Url::parse(MODULE).unwrap()).unwrap();
        assert!(module_rows.iter().any(|(u,_)|u==SECTION));
        let single=format!("<div class='bp-page-modules'><div class='sections'><div class='section'><div class='section-url'><a href='{MODULE}'>Acute Stroke Management</a></div><div class='section-link'><a href='{SECTION}'>5. Acute Ischemic Stroke Treatment</a></div></div></div></div>");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(ROOT,single.as_bytes(),"text/html");f.seed_cache_for_tests(SECTION,ACUTE.as_bytes(),"text/html");
        // Keep the genuine module section containing this one section in an otherwise
        // bounded module inventory, so discovery remains finite in the offline test.
        let bounded_module=format!("<div class='bp-right-column'><div class='bp-page-modules'><div class='sections'><div class='section'><div class='section-url'><a href='{SECTION}'>5. Acute Ischemic Stroke Treatment</a></div></div></div></div></div>");
        f.seed_cache_for_tests(MODULE,bounded_module.as_bytes(),"text/html");f.seed_cache_for_tests(TERMS_URL,TERMS.as_bytes(),"text/html");
        f.seed_cache_for_tests(BOX5A_URL,BOX5A,"application/pdf");
        let src:SourceEntry=serde_json::from_value(json!({"id":"canadian_stroke","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let plan=CanadianStroke.discover(&src,&f,None).await.unwrap();
        let section=plan.iter().find(|r|r.url==SECTION).unwrap();
        let box5a=plan.iter().find(|r|r.url==BOX5A_URL).unwrap();
        assert_eq!(box5a.rendition.as_deref(),Some("pdf"));
        let box_doc=CanadianStroke.extract(&src,&f,box5a).await.unwrap();
        assert_eq!(box_doc.item_type.as_deref(),Some("clinical_box"));assert_eq!(box_doc.rights.status,"unknown");
        assert!(box_doc.relations.iter().any(|r|r.rel=="part_of" && r.target==SECTION));
        assert!(box_doc.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})));
        assert!(box_doc.warnings.iter().any(|w|w.contains("visual validation")));
        assert!(plan.iter().any(|r|r.url.contains("csbpr7-asm-box5b.pdf") && r.extra["discovery_error"].is_string()));
        assert!(!plan.iter().any(|r|r.url.contains("csbpr7-asm-box4a.pdf")),"a cross-referenced section 4 box is not part of section 5");
        let d=CanadianStroke.extract(&src,&f,section).await.unwrap();
        assert_eq!(d.title,"5. Acute Ischemic Stroke Treatment");
        assert_eq!(d.rights.status,"restricted");assert_eq!(d.rights.terms_url.as_deref(),Some(TERMS_URL));
        assert!(d.rights.restrictions.iter().any(|r|r.contains("personal, non-commercial use") && r.contains("prior written consent")));
        assert!(d.rights.restrictions.iter().any(|r|r.contains("Documentation") && r.contains("without the prior written consent of HSF")));
        assert!(d.fetches.iter().any(|r|r.url==TERMS_URL));
        assert!(d.relations.iter().any(|r|r.rel=="part_of" && r.target==MODULE));
        assert!(d.date_observations.iter().any(|x|x.label=="Edition update" && x.normalized.as_deref()==Some("2022-12")));
        assert!(d.date_observations.iter().any(|x|x.label=="Endovascular thrombectomy interim update" && x.normalized.as_deref()==Some("2025")));
        assert!(d.search_metadata["section_5_4_update"].as_str().is_some_and(|s|s.contains("supersede") && s.contains("4B, 4C, 5C")));
        assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text.contains("5.4.1. Endovascular Thrombectomy")));
        assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text=="Performance Measures"));
        let recs:Vec<_>=d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();
        let first=recs.iter().find(|b|b.text.contains("EVT) should be offered within a coordinated system of care")).unwrap();
        assert!(matches!(&first.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),label:None,..} if s=="Strong recommendation" && c=="High quality of evidence"));
        assert!(first.heading_path.iter().any(|h|h.contains("5.4.1. Endovascular Thrombectomy")));
        let four=recs.iter().find(|b|b.text.contains("patient, symptom, occlusion, and brain tissue criteria")).unwrap();
        assert!(four.text.contains("Age greater than 18") && four.text.contains("Baseline independent function") && four.text.contains("Last known well within 24 hours"));
        assert!(matches!(&four.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Strong Recommendation" && c=="High Quality Evidence"));
        // The parent recommendation intentionally contains its nested child text too;
        // select and check the parent and the source child separately.
        let thrombolysis_parent=recs.iter().find(|b|b.text.starts_with("Intravenous thrombolysis should be provided to all eligible patients")).unwrap();
        assert!(thrombolysis_parent.text.contains("this should be initiated while simultaneously preparing the angiography suite"));
        assert!(matches!(&thrombolysis_parent.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Strong recommendation" && c=="High quality of evidence"));
        let nested=recs.iter().find(|b|b.text.starts_with("For patients who are also eligible for intravenous thrombolysis") && b.text.contains("this should be initiated while simultaneously preparing the angiography suite")).unwrap();
        assert!(matches!(&nested.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),qualifier:Some(parent),..} if s=="Strong recommendation" && c=="High quality of evidence" && parent.contains("Intravenous thrombolysis should be provided to all eligible patients")));
        let multi=recs.iter().find(|b|b.text.contains("with STAT results requested")).unwrap();
        assert!(matches!(&multi.kind,BlockKind::Recommendation{strength:None,certainty:None,..}));
        assert!(d.warnings.iter().any(|w|w.contains("Multiple distinct grade clauses")));
        let box_mention=d.attachments.iter().find(|a|a["url"].as_str().is_some_and(|u|u.contains("csbpr7-asm-box5a.pdf?rev=4fcdd17e8d784e4ca371312add88fd30"))).unwrap();
        assert!(box_mention["source_mentions"].as_array().is_some_and(|m|m.len()>=2));
        assert!(d.attachments.iter().any(|a|a["url"].as_str().is_some_and(|u|u.contains("csbpr7-asm-box5b.pdf?rev=6fc1d07aa65b4fb8a5ecf4634568134c"))));
        assert!(d.warnings.iter().any(|w|w.contains("not acquired")));
        assert!(d.attachments.iter().filter(|a|a["role"]=="clinical_box").all(|a|a["rights"].as_str().unwrap().contains("unknown")));
        let mut altered_pdf=BOX5A.to_vec();altered_pdf.extend_from_slice(b"\nchanged snapshot");
        f.seed_cache_for_tests(BOX5A_URL,&altered_pdf,"application/pdf");
        assert!(CanadianStroke.extract(&src,&f,box5a).await.is_err());
        let changed=ACUTE.replace("within a coordinated system of care","within an altered clinical care system");
        f.seed_cache_for_tests(SECTION,changed.as_bytes(),"text/html");
        assert!(CanadianStroke.extract(&src,&f,section).await.is_err());
    }
    #[tokio::test]
    async fn real_box_5b_clinical_pages_keep_source_scope() {
        const BOX_URL:&str="https://www.strokebestpractices.ca/-/media/1-stroke-best-practices/acute-stroke-management/csbpr7-asm-box5b.pdf?rev=6fc1d07aa65b4fb8a5ecf4634568134c";
        const PAGE2:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/canadian_stroke/box-5b-page-2.pdf"));
        const PAGE3:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/canadian_stroke/box-5b-page-3.pdf"));
        let root=format!("<div class='bp-page-modules'><div class='sections'><div class='section'><div class='section-url'><a href='{MODULE}'>Acute Stroke Management</a></div><div class='section-link'><a href='{SECTION}'>5. Acute Ischemic Stroke Treatment</a></div></div></div></div>");
        let module=format!("<div class='bp-right-column'><div class='bp-page-modules'><div class='sections'><div class='section'><div class='section-url'><a href='{SECTION}'>5. Acute Ischemic Stroke Treatment</a></div></div></div></div></div>");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(ROOT,root.as_bytes(),"text/html");f.seed_cache_for_tests(MODULE,module.as_bytes(),"text/html");f.seed_cache_for_tests(SECTION,ACUTE.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"canadian_stroke","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        for (pdf,needles) in [(PAGE2,vec!["Inclusion Criteria","Absolute Exclusion Criteria","Relative Exclusion Criteria"]),(PAGE3,vec!["CT or MRI Findings","Blood glucose concentration","Platelet count"])] {
            f.seed_cache_for_tests(BOX_URL,pdf,"application/pdf");
            let plan=CanadianStroke.discover(&src,&f,None).await.unwrap();let row=plan.iter().find(|r|r.url==BOX_URL).unwrap();
            let doc=CanadianStroke.extract(&src,&f,row).await.unwrap();
            let text=doc.blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");
            for needle in needles {assert!(text.contains(needle),"missing {needle}");}
            assert_eq!(doc.item_type.as_deref(),Some("clinical_box"));assert_eq!(doc.rights.status,"unknown");
            assert!(doc.relations.iter().any(|r|r.rel=="part_of" && r.target==SECTION));
            assert!(doc.date_observations.iter().all(|d|d.normalized.as_deref()!=Some("2025")));
            assert!(doc.warnings.iter().any(|w|w.contains("visual validation")));
        }
    }

}
