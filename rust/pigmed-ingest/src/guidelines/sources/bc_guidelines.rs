//! BC Guidelines alphabetical listing and guideline HTML with separately linked appendices.
use super::{common, static_list::blocks_for, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, GuidelineDoc, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{anyhow, bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use url::Url;

pub struct BcGuidelines;
const ROOT: &str = ".topicContent__main > #body";
const PREFIX: &str = "/gov/content/health/practitioner-professional-resources/bc-guidelines/";

fn body<'a>(doc:&'a Html)->Result<ElementRef<'a>> {
    doc.select(&sel(ROOT)).next().ok_or_else(||anyhow!("BC guideline content root missing"))
}
fn listing(html:&str,base:&Url)->Result<Vec<(String,String)>> {
    let doc=Html::parse_document(html); let root=body(&doc)?;
    if root.select(&sel("table tr")).next().is_none() {bail!("BC alphabetical listing tables missing");}
    let mut urls=BTreeMap::new();
    for tr in root.select(&sel("table tr")) {
        let cells:Vec<_>=tr.children().filter_map(ElementRef::wrap).filter(|e|matches!(e.value().name(),"td"|"th")).collect();
        if cells.len()<2 {continue;}
        for a in cells[1].select(&sel("a[href]")) {
            let Some(url)=a.value().attr("href").and_then(|v|common::link(base,v)) else {continue};
            let u=Url::parse(&url)?;
            if u.origin()!=base.origin() || u.query().is_some() || !u.path().starts_with(PREFIX) {continue;}
            let slug=u.path()[PREFIX.len()..].trim_matches('/');
            if slug.is_empty() || slug.contains('/') || matches!(slug,"partner-guidelines"|"guidelines-by-alphabetical-listing") {continue;}
            let label=text_of(&a); if label.trim().is_empty() {continue;}
            urls.entry(url).or_insert(label);
        }
    }
    if urls.is_empty() {bail!("BC listing has no directly linked guideline pages");}
    Ok(urls.into_iter().collect())
}

fn metadata(html:&str)->Result<(String,Option<String>,Option<String>)> {
    let doc=Html::parse_document(html); let root=body(&doc)?;
    let title=doc.select(&sel(".topicMain__container h1")).next().map(|h|text_of(&h)).filter(|s|!s.is_empty()).context("BC guideline title missing")?;
    let effective=root.select(&sel("p")).map(|p|text_of(&p)).find_map(|t|t.strip_prefix("Effective Date:").map(|s|s.trim().to_string()));
    let published=doc.select(&sel(".topicMain__container .last_Updated_Text")).map(|p|text_of(&p)).find_map(|t|t.strip_prefix("Publication date:").map(|s|s.trim().to_string()));
    Ok((title,effective,published))
}

fn attachments(html:&str,base:&Url)->Result<Vec<Value>> {
    let doc=Html::parse_document(html); let _=body(&doc)?;
    let appendix=regex::Regex::new(r"(?i)^Appendix\s+([A-Z0-9]+)\s*[:.\-]").unwrap();
    let mut seen=BTreeMap::<String,usize>::new(); let mut out:Vec<Value>=Vec::new();
    for a in doc.select(&sel(".topicContent__main > #body a[href], #cmf-ui-supplementary-content a[href]")) {
        let Some(url)=a.value().attr("href").and_then(|v|common::link(base,v)) else {continue};
        let u=Url::parse(&url)?;
        let label=text_of(&a);
        let is_pdf=u.path().to_ascii_lowercase().ends_with(".pdf");
        let is_patient_tools=u.origin()==base.origin() && label.to_lowercase().contains("action plan");
        if !(is_pdf || is_patient_tools) {continue;}
        let parent=a.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().name()=="li").map(|e|text_of(&e)).unwrap_or_default();
        // Earlier inline links may carry “Appendix C: ...” in the anchor, while
        // the later inventory puts “Appendix C:” in its surrounding list item.
        let letter=appendix.captures(label.trim()).or_else(||appendix.captures(parent.trim())).map(|c|c[1].to_ascii_uppercase());
        let role=if letter.is_some() {"clinical_appendix"} else if is_patient_tools && !is_pdf {"patient_tools"} else if label.to_lowercase().contains("flow sheet") {"flow_sheet"} else if label.to_lowercase().contains("full guideline") {"full_guideline"} else {"linked_pdf"};
        let mention=json!({"title":label,"source_label":parent,"role":role});
        if let Some(i)=seen.get(&url).copied() {
            if let Some(mentions)=out[i]["source_mentions"].as_array_mut() {mentions.push(mention);}
            if role=="clinical_appendix" && out[i]["role"]!="clinical_appendix" {
                out[i]["role"]=json!(role);out[i]["appendix"]=json!(letter);
                out[i]["title"]=json!(label);out[i]["source_label"]=json!(parent);
            } else if out[i]["role"]=="linked_pdf" && role!="linked_pdf" {
                out[i]["role"]=json!(role);out[i]["title"]=json!(label);out[i]["source_label"]=json!(parent);
            }
            continue;
        }
        seen.insert(url.clone(),out.len());
        out.push(json!({"url":url,"title":label,"source_label":parent,"source_mentions":[mention],"role":role,"appendix":letter,"source_url":base.as_str(),"rights":"unknown; asset-specific review required"}));
    }
    Ok(out)
}

// Heading and table age icons carry clinical meaning; the shared text walker
// omits image alt in these positions. Promote exact alt text locally.
fn clinical_html(html:&str)->Result<String> {
    let doc=Html::parse_document(html); let root=body(&doc)?; let mut clean=root.html();
    for el in root.select(&sel("h1, h2, h3, h4, h5, h6, table")) {
        let original=el.html(); let mut changed=original.clone(); let mut lifted=String::new();
        for img in el.select(&sel("img")) {
            let markup=img.html();
            let alt=img.value().attr("alt").unwrap_or_default().trim();
            let promoted=if alt.is_empty() {String::new()} else {
                let escaped=alt.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;");
                format!(" [{escaped}] ")
            };
            changed=changed.replacen(&markup,&promoted,1);
            // Heading/table walkers do not visit nested images; retain the exact
            // element immediately after its container to emit its original URL.
            lifted.push_str(&markup);
        }
        if !lifted.is_empty() {changed.push_str(&lifted);clean=clean.replacen(&original,&changed,1);}
    }
    Ok(clean)
}
fn html_terms(html:&str)->Result<(String,String)> {
    let doc=Html::parse_document(html); let _=body(&doc)?;
    // The About page's explicit reuse answers are in its publisher FAQ sidebar,
    // distinct from the primary guideline-development article.
    let root=doc.select(&sel("#cmf-ui-supplementary-content .relatedLinks__content")).next().context("BC About reuse FAQ missing")?;
    let paragraphs:Vec<_>=root.select(&sel("p")).map(|p|text_of(&p)).collect();
    let reproduction=paragraphs.iter().find(|p|p.starts_with("GPAC authorizes the use and reproduction") && p.contains("commercial redistribution") && p.contains("acknowledged"))
        .cloned().context("BC reproduction terms not recognized")?;
    let linking=paragraphs.iter().find(|p|p.contains("external website") && p.contains("provide a link to the original guideline"))
        .cloned().context("BC external website guidance not recognized")?;
    Ok((reproduction,linking))
}
fn work_key(url:&str)->Result<String> {
    Ok(Url::parse(url)?.path_segments().and_then(|s|s.filter(|x|!x.is_empty()).last()).context("BC topic slug missing")?.to_string())
}

async fn appendix_manifest(f:&Fetcher, assets:&mut [Value])->Vec<String> {
    let mut warnings=Vec::new();
    for a in assets.iter_mut().filter(|a|a["role"]=="clinical_appendix") {
        let Some(url)=a["url"].as_str().map(str::to_string) else {continue};
        match f.get_ok(&url).await {
            Ok(pdf) if pdf.is_pdf() => {a["sha256"]=json!(pdf.record.sha256);}
            Ok(_) => {a["acquisition_error"]=json!("linked appendix did not return PDF bytes"); warnings.push(format!("Appendix {url} did not return PDF bytes."));}
            Err(e) => {a["acquisition_error"]=json!(format!("{e:#}")); warnings.push(format!("Appendix {url} could not be acquired: {e:#}"));}
        }
    }
    warnings
}
fn snapshot(html:&str,assets:&[Value])->Result<String> {
    let mut material=clinical_html(html)?;
    let (title,effective,published)=metadata(html)?;
    material.push_str(&format!("\n{title}\n{effective:?}\n{published:?}"));
    for a in assets {
        material.push_str("\n"); material.push_str(a["role"].as_str().unwrap_or(""));
        material.push_str("\n"); material.push_str(a["url"].as_str().unwrap_or(""));
        if a["role"]=="clinical_appendix" {material.push_str("\n");material.push_str(a["sha256"].as_str().unwrap_or("unavailable"));}
    }
    Ok(crate::common::sha256_hex(material.as_bytes()))
}

#[async_trait::async_trait]
impl Adapter for BcGuidelines {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0) {return Ok(vec![]);}
        let catalog=src.catalog_url.as_deref().context("BC catalog URL missing")?;
        let page=f.get_ok(catalog).await?; let base=Url::parse(&page.record.final_url)?;
        let mut out=vec![];
        for (url,label) in listing(&page.text(),&base)? {
            let work=work_key(&url)?;
            let result:Result<PlanRow>=async {
                let detail=f.get_ok(&url).await?; let final_url=Url::parse(&detail.record.final_url)?;
                if final_url.origin()!=base.origin() {bail!("BC guideline redirected outside catalog origin");}
                let (title,_,_)=metadata(&detail.text())?;
                let mut assets=attachments(&detail.text(),&final_url)?;
                let _=appendix_manifest(f,&mut assets).await;
                let hash=snapshot(&detail.text(),&assets)?;
                Ok(PlanRow {source_id:src.id.clone(),doc_key:format!("{}-{}",work,&hash[..12]),url:url.clone(),title:Some(title),rendition:Some("html".into()),version:None,
                    extra:json!({"work_key":work,"snapshot_sha256":hash,"appendices":assets})})
            }.await;
            let row=match result {
                Ok(row)=>row,
                Err(e)=>PlanRow {source_id:src.id.clone(),doc_key:format!("{}-discovery-error-{}",work,common::short_hash(&url)),url:url.clone(),title:Some(label),
                    rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"discovery_error":format!("{e:#}")})},
            };
            out.push(row);
            if limit.is_some_and(|n|out.len()>=n) {break;}
        }
        Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(error)=row.extra["discovery_error"].as_str() {bail!("BC discovery failed: {error}");}
        let page=f.get_ok(&row.url).await?; let base=Url::parse(&page.record.final_url)?;
        if base.origin()!=Url::parse(&row.url)?.origin() {bail!("BC guideline redirected outside source origin");}
        let html=page.text(); let (title,effective,published)=metadata(&html)?;
        let (has_spans,source_text,self_identifies,gpac_statement)={
            let parsed=Html::parse_document(&html); let root=body(&parsed)?;
            let has_spans=root.select(&sel("table [rowspan], table [colspan]")).next().is_some();
            let source_text=text_of(&root);
            let self_identifies=root.select(&sel("p")).map(|p|text_of(&p).to_lowercase())
                .any(|p|p.starts_with("this guideline ") || p.starts_with("this clinical practice guideline "));
            // The source explicitly names the developer in this topic's own main
            // container. Preserve that evidence even if malformed source markup
            // causes the HTML parser to close #body before the attribution paragraph.
            let gpac_statement=parsed.select(&sel(".topicMain__container p")).map(|p|text_of(&p)).any(|p|p.starts_with("This guideline was developed by the Guidelines and Protocols Advisory Committee"));
            (has_spans,source_text,self_identifies,gpac_statement)
        };
        let current_assets=attachments(&html,&base)?;
        let mut assets=row.extra["appendices"].as_array().cloned().unwrap_or_else(||current_assets.clone());
        if let Some(expected)=row.extra["snapshot_sha256"].as_str() {
            let identity=|list:&[Value]|list.iter().map(|a|format!("{}|{}|{}",a["url"],a["role"],a["appendix"])).collect::<Vec<_>>();
            if identity(&assets)!=identity(&current_assets) || expected!=snapshot(&html,&assets)? {bail!("BC guideline or planned attachment links changed; re-plan");}
        } else {let _=appendix_manifest(f,&mut assets).await;}
        let clean=clinical_html(&html)?;
        let (mut blocks,mut warnings)=blocks_from_html(&clean,Some(&base),Some("#body"));
        let mut n=0;
        for b in &mut blocks {
            if b.heading_path.iter().any(|h|h.eq_ignore_ascii_case("Key Recommendations")) && matches!(b.kind,BlockKind::ListItem) {
                b.kind=BlockKind::Recommendation {label:None,strength:None,certainty:None,population:None,grading_system:None,qualifier:None}; n+=1;
            }
        }
        if n==0 {warnings.push("No structured key-recommendation list recognized; verify this guideline's layout.".into());}
        if has_spans {warnings.push("HTML table has merged cells; Markdown conversion may lose span semantics and requires visual verification.".into());}
        let mut fetches=vec![page.record.clone()];
        for a in assets.iter_mut().filter(|a|a["role"]=="clinical_appendix") {
            let Some(url)=a["url"].as_str().map(str::to_string) else {continue};
            let Some(expected)=a["sha256"].as_str().map(str::to_string) else {warnings.push(format!("Clinical appendix not acquired: {url}"));continue};
            match f.get_ok(&url).await {
                Ok(pdf) if pdf.is_pdf() && pdf.record.sha256==expected => {
                    match blocks_for(&pdf,None,30).await {
                        Ok((mut appended,more,_)) => {
                            let label=format!("Appendix {}: {}",a["appendix"].as_str().unwrap_or(""),a["title"].as_str().unwrap_or(""));
                            let prefix=format!("appendix-{}-{}",a["appendix"].as_str().unwrap_or("unknown"),common::short_hash(&url));
                            blocks.push(Block{block_id:format!("{prefix}-heading"),kind:BlockKind::Heading{level:2},text:label.clone(),heading_path:vec![],page:None});
                            for b in &mut appended {b.block_id=format!("{prefix}-{}",b.block_id);b.heading_path.insert(0,label.clone());}
                            blocks.extend(appended);warnings.extend(more);a["extraction"] = json!("converted text; visual table fidelity not validated");
                        }
                        Err(e)=>warnings.push(format!("Clinical appendix conversion failed for {url}: {e:#}")),
                    }
                    fetches.push(pdf.record);
                }
                Ok(_) => bail!("BC clinical appendix changed since planning or is not a PDF: {url}"),
                Err(e) => warnings.push(format!("Planned clinical appendix unavailable {url}: {e:#}")),
            }
        }
        warnings.push("Appendix PDF tables, footnotes, images and diagrams require visual fidelity validation; linked flow sheets and other assets were not acquired as clinical text.".into());
        warnings.push("Alphabetical listing covers directly linked BC guideline pages; partner guidelines, archive and external links require separate inventory and rights review.".into());
        if source_text.contains("replaces two previous BC Guidelines") {
            warnings.push("Source states two prior guideline works were replaced, but no predecessor URLs or stable identifiers were established; the statement is retained as source text.".into());
        }
        let mut dates=vec![];
        if let Some(raw)=effective {dates.push(common::date("Effective Date",&raw,&row.url));}
        let pub_obs=published.map(|raw|common::date("Publication date",&raw,&row.url));
        let published=pub_obs.as_ref().and_then(|d|d.normalized.clone()); if let Some(d)=pub_obs {dates.push(d);}
        let title_lower=title.to_lowercase();
        let item_type=if self_identifies && !title_lower.contains("action plan") && !title_lower.contains("form") {Some("guideline".into())} else {None};
        let issuer=if gpac_statement || source_text.contains("Guidelines and Protocols Advisory Committee") {vec!["Guidelines and Protocols Advisory Committee".into()]} else {vec![]};
        // GPAC's website terms are scoped to the identified BC guideline HTML.
        // Appended PDFs may have different rights, and the common document-level
        // Rights is propagated to all rows, so do not mislabel the whole document.
        let mut rights=Rights{status:"unknown".into(),..Default::default()};
        let mut scoped_rights=Value::Null;
        if item_type.is_some() && !issuer.is_empty() {
            let about="https://www2.gov.bc.ca/gov/content/health/practitioner-professional-resources/bc-guidelines/about-the-guidelines";
            match f.get_ok(about).await {
                Ok(page) => {
                    match html_terms(&page.text()) {
                        Ok((reproduction,linking))=>{
                            rights.restrictions.push(format!("HTML only (not appended PDFs or partner guidelines): {reproduction}"));
                            rights.restrictions.push(format!("HTML only, publisher's external-site guidance: {linking}"));
                            rights.attribution=Some("HTML only: acknowledge GPAC as creator when reproducing a BC guideline, as specified by the publisher.".into());
                            scoped_rights=json!({"scope":"GPAC guideline HTML only; partner guidelines and appended assets require separate review", "source_url":about,
                                "reproduction_terms":reproduction,"external_website_guidance":linking});
                            rights.terms_url=Some(about.into());rights.evidence_urls.push(about.into());
                            rights.note=Some("GPAC terms below apply only to the guideline HTML; PDF appendix and partner guideline rights are unknown. Document status remains unknown because its rows include appendices.".into());
                        }
                        Err(e)=>warnings.push(format!("BC reuse terms could not be interpreted: {e:#}")),
                    }
                    fetches.push(page.record);
                }
                Err(e)=>warnings.push(format!("BC reuse terms unavailable: {e:#}")),
            }
        }
        Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),
            item_type,status:"unknown".into(),published,date_observations:dates,issuers:issuer,
            rights,search_metadata:json!({"html_rights":scoped_rights,"appendix_rights":"unknown; separately sourced assets require review"}),
            blocks,attachments:assets,fetches,warnings,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const CATALOG:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bc_guidelines/catalog.html"));
    const ASTHMA:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bc_guidelines/asthma.html"));
    const ABOUT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bc_guidelines/about.html"));
    const APPENDIX_A:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bc_guidelines/asthma_appendix_a.pdf"));
    const APPENDIX_B:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bc_guidelines/asthma_appendix_b.pdf"));
    const APPENDIX:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/bc_guidelines/asthma_appendix_c.pdf"));
    const LIST:&str="https://www2.gov.bc.ca/gov/content/health/practitioner-professional-resources/bc-guidelines/guidelines-by-alphabetical-listing";
    const TOPIC:&str="https://www2.gov.bc.ca/gov/content/health/practitioner-professional-resources/bc-guidelines/asthma";
    const PDF:&str="https://www2.gov.bc.ca/assets/gov/health/practitioner-pro/bc-guidelines/asthma_appendix_c.pdf";
    const PDF_A:&str="https://www2.gov.bc.ca/assets/gov/health/practitioner-pro/bc-guidelines/asthma_appendix_a.pdf";
    const PDF_B:&str="https://www2.gov.bc.ca/assets/gov/health/practitioner-pro/bc-guidelines/asthma_appendix_b.pdf";
    const ABOUT_URL:&str="https://www2.gov.bc.ca/gov/content/health/practitioner-professional-resources/bc-guidelines/about-the-guidelines";
    #[tokio::test]
    async fn saved_listing_asthma_and_appendix_offline() {
        let entries=listing(CATALOG,&Url::parse(LIST).unwrap()).unwrap();
        assert!(entries.iter().any(|(u,_)|u==TOPIC));
        assert_eq!(entries.iter().filter(|(u,_)|u==TOPIC).count(),1);
        assert!(entries.iter().all(|(u,_)|!u.ends_with("/partner-guidelines")));
        assert!(metadata("<h1>Wrong page</h1>").is_err());
        let date=metadata(ASTHMA).unwrap(); assert_eq!(date.0,"Asthma Diagnosis, Education and Management");
        assert_eq!(date.1.as_deref(),Some("July 26, 2023"));
        let links=attachments(ASTHMA,&Url::parse(TOPIC).unwrap()).unwrap();
        for letter in ["A","B","C"] {
            let item=links.iter().find(|a|a["role"]=="clinical_appendix" && a["appendix"]==letter).unwrap();
            assert!(item["source_mentions"].as_array().is_some_and(|m|m.len()>=2));
        }
        assert!(links.iter().any(|a|a["role"]=="full_guideline" && a["url"].as_str().is_some_and(|s|s.ends_with("/asthma_final.pdf"))));
        assert!(links.iter().any(|a|a["role"]=="patient_tools" && a["url"].as_str().is_some_and(|s|s.ends_with("/asthma-action-plans"))));
        assert!(links.iter().any(|a|a["role"]=="flow_sheet" && a["title"].as_str().is_some_and(|s|s.contains("Adults"))));
        let marker=CATALOG.find("href=\"/gov/content/health/practitioner-professional-resources/bc-guidelines/asthma\"").unwrap();
        let start=CATALOG[..marker].rfind("<tr>").unwrap();
        let end=marker+CATALOG[marker..].find("</tr>").unwrap()+5;
        let single=format!("<div class=\"topicContent__main\"><div id=\"body\"><table>{}</table></div></div>",&CATALOG[start..end]);
        let dir=tempfile::tempdir().unwrap();let f=Fetcher::new(dir.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(LIST,single.as_bytes(),"text/html");f.seed_cache_for_tests(TOPIC,ASTHMA.as_bytes(),"text/html");
        f.seed_cache_for_tests(PDF,APPENDIX,"application/pdf");
        f.seed_cache_for_tests(PDF_A,APPENDIX_A,"application/pdf");f.seed_cache_for_tests(PDF_B,APPENDIX_B,"application/pdf");
        f.seed_cache_for_tests(ABOUT_URL,ABOUT.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"bc_guidelines","access":"public","enabled":true,"catalog_url":LIST})).unwrap();
        let plan=BcGuidelines.discover(&src,&f,None).await.unwrap();assert_eq!(plan.len(),1);
        let doc=BcGuidelines.extract(&src,&f,&plan[0]).await.unwrap();
        assert_eq!(doc.published.as_deref(),Some("2023-07-26"));
        assert!(doc.date_observations.iter().any(|d|d.label=="Effective Date" && d.normalized.as_deref()==Some("2023-07-26")));
        assert_eq!(doc.item_type.as_deref(),Some("guideline"));
        assert!(doc.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text.contains("Indicated for patients over the age of 18")));
        assert!(doc.blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(url),..} if url.ends_with("/18plus.jpg")) && b.heading_path.iter().any(|h|h.contains("Indicated for patients over the age of 18"))));
        let recs:Vec<_>=doc.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();
        assert_eq!(recs.len(),12);assert!(recs.iter().any(|b|b.text.contains("presumptive diagnosis") && b.text.contains("under age 6")));
        assert!(recs.iter().all(|b|matches!(b.kind,BlockKind::Recommendation{strength:None,certainty:None,..})));
        assert!(doc.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..}) && b.heading_path.iter().any(|h|h.contains("Appendix C"))));
        assert!(doc.fetches.iter().any(|r|r.url==PDF));
        assert!(doc.blocks.iter().any(|b|b.heading_path.iter().any(|h|h.contains("Appendix A")) && b.text.to_lowercase().contains("spirometry")));
        assert!(doc.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..}) && b.heading_path.iter().any(|h|h.contains("Appendix B")) && b.text.contains("medication adherence")));
        for url in [PDF_A,PDF_B,PDF] {assert!(doc.fetches.iter().any(|r|r.url==url));}
        assert!(!doc.warnings.iter().any(|w|w.contains("Clinical appendix not acquired")));
        assert_eq!(doc.rights.status,"unknown");
        assert_eq!(doc.rights.terms_url.as_deref(),Some(ABOUT_URL));
        assert!(doc.rights.restrictions.iter().any(|r|r.starts_with("HTML only") && r.contains("commercial redistribution")));
        assert!(doc.rights.attribution.as_deref().is_some_and(|a|a.contains("HTML only") && a.contains("GPAC")));
        assert_eq!(doc.search_metadata["html_rights"]["source_url"],ABOUT_URL);
        assert!(doc.search_metadata["html_rights"]["reproduction_terms"].as_str().unwrap().contains("commercial redistribution"));
        assert!(doc.search_metadata["html_rights"]["external_website_guidance"].as_str().unwrap().contains("link to the original"));
        assert!(doc.attachments.iter().filter(|a|a["role"]=="clinical_appendix").all(|a|a["rights"].as_str().unwrap().contains("unknown")));
        assert!(doc.fetches.iter().any(|r|r.url==ABOUT_URL));
        assert!(doc.warnings.iter().any(|w|w.contains("visual fidelity")));
        let mut ids=HashSet::new();assert!(doc.blocks.iter().all(|b|ids.insert(b.block_id.clone())));
        let missing_dir=tempfile::tempdir().unwrap();let fm=Fetcher::new(missing_dir.path(),true,500,1<<20,"").unwrap();
        fm.seed_cache_for_tests(LIST,single.as_bytes(),"text/html");fm.seed_cache_for_tests(TOPIC,ASTHMA.as_bytes(),"text/html");
        fm.seed_cache_for_tests(PDF,APPENDIX,"application/pdf");fm.seed_cache_for_tests(ABOUT_URL,ABOUT.as_bytes(),"text/html");
        let missing_plan=BcGuidelines.discover(&src,&fm,None).await.unwrap();
        let missing=BcGuidelines.extract(&src,&fm,&missing_plan[0]).await.unwrap();
        assert!(missing.warnings.iter().any(|w|w.contains("Clinical appendix not acquired")));
        assert_ne!(plan[0].doc_key,missing_plan[0].doc_key);
        let dir2=tempfile::tempdir().unwrap();let f2=Fetcher::new(dir2.path(),true,500,1<<20,"").unwrap();
        f2.seed_cache_for_tests(TOPIC,ASTHMA.as_bytes(),"text/html");
        let mut changed=APPENDIX.to_vec();changed.extend_from_slice(b"\nrevision");
        f2.seed_cache_for_tests(PDF,&changed,"application/pdf");
        assert!(BcGuidelines.extract(&src,&f2,&plan[0]).await.is_err());
        // A failed listing item is represented with its actual error and does not
        // prevent the next independently valid item from being discovered.
        let two=single.replace("</table>","<tr><td>Blocked</td><td><a href=\"/gov/content/health/practitioner-professional-resources/bc-guidelines/aaa-unavailable\">Unavailable</a></td></tr></table>");
        let d3=tempfile::tempdir().unwrap();let f3=Fetcher::new(d3.path(),true,500,1<<20,"").unwrap();
        f3.seed_cache_for_tests(LIST,two.as_bytes(),"text/html");f3.seed_cache_for_tests(TOPIC,ASTHMA.as_bytes(),"text/html");f3.seed_cache_for_tests(PDF,APPENDIX,"application/pdf");
        let mixed=BcGuidelines.discover(&src,&f3,None).await.unwrap();assert_eq!(mixed.len(),2);
        let failed=mixed.iter().find(|r|r.url.ends_with("aaa-unavailable")).unwrap();
        assert!(failed.extra["discovery_error"].as_str().is_some_and(|e|e.contains("offline")));
        assert!(BcGuidelines.extract(&src,&f3,failed).await.is_err());
        assert!(mixed.iter().any(|r|r.url==TOPIC && r.extra["discovery_error"].is_null()));
    }
}
