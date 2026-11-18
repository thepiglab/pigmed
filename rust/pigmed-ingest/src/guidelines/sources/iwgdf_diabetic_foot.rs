//! IWGDF publisher chapter catalog and actual PDF recommendations.
//! The catalog's global CSS contains the literal selector input[type="password"],
//! which trips the shared string heuristic despite no password input in this page.
use super::{common, static_list::blocks_for, Adapter, PlanRow};
use crate::guidelines::doc::{BlockKind, GuidelineDoc, Rights};
use crate::guidelines::fetch::{looks_like_login_wall, Fetcher, Fetched};
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{anyhow,bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::{BTreeMap,HashSet};
use url::Url;

pub struct Iwgdf;
const CATALOG:&str="https://iwgdfguidelines.org/guidelines-2023/";

/// Fail closed for actual forms, gating copy, challenge elements, redirects and
/// missing publisher structure; this source-specific check never submits forms.
fn public_html(page:&Fetched,expected:&str,require_catalog:bool)->Result<String> {
    if page.record.status!=200 {bail!("IWGDF public page returned HTTP {}",page.record.status);}
    if !page.is_html() {bail!("IWGDF expected HTML response");}
    let origin=Url::parse(expected)?; let final_url=Url::parse(&page.record.final_url)?;
    if origin.scheme()!="https" || origin.host_str()!=Some("iwgdfguidelines.org") {bail!("IWGDF request outside publisher origin");}
    let mut hops=HashSet::new();
    for hop in page.record.redirects.iter().chain(std::iter::once(&page.record.final_url)) {
        let u=Url::parse(hop)?;
        if u.origin()!=origin.origin() {bail!("IWGDF public page redirect left source origin");}
        if !hops.insert(hop.clone()) {bail!("IWGDF public page redirect cycle");}
    }
    let doc=Html::parse_document(&page.text());
    if doc.select(&sel("input")).any(|i|i.value().attr("type").is_some_and(|t|t.eq_ignore_ascii_case("password"))) {bail!("IWGDF page contains a real password input; access requires review");}
    if doc.select(&sel(".g-recaptcha, .h-captcha, iframe[src*='recaptcha'], iframe[src*='hcaptcha'], input[name='captcha']")).next().is_some() {bail!("IWGDF challenge detected");}
    let root=doc.select(&sel("#Content")).next().context("IWGDF public content root missing")?;
    if root.select(&sel("form")).next().is_some(){bail!("IWGDF public content contains a form; access requires manual review");}
    let gate=["sign in", "log in", "login", "member access", "institutional access", "subscribe", "access denied", "verify you are human", "captcha"];
    if root.select(&sel("a[href]")).any(|a| {
        let href=a.value().attr("href").unwrap_or_default().to_lowercase();
        href.contains("/login") || href.contains("/sign-in") || href.contains("/signin")
    }) {bail!("IWGDF content includes a login link; access requires manual review");}
    for e in root.select(&sel("h1,h2,h3,h4,p")) {
        let t=text_of(&e).to_lowercase();
        if gate.iter().any(|g|t.contains(g)) {bail!("IWGDF content displays an access challenge");}
    }
    if require_catalog {
        if root.value().attr("role")!=Some("main") {bail!("IWGDF catalog main role missing");}
        if final_url.path().trim_end_matches('/')!="/guidelines-2023" {bail!("IWGDF catalog redirected to a different page");}
        if !root.select(&sel("h2")).any(|h|text_of(&h).to_lowercase().contains("2023 iwgdf guidelines are available")) {bail!("IWGDF 2023 collection heading missing");}
        if root.select(&sel(".vc_grid-item")).next().is_none() {bail!("IWGDF catalog chapter grid missing");}
    }
    Ok(root.html())
}
fn cards(html:&str,base:&Url)->Result<Vec<(String,String)>> {
    let doc=Html::parse_fragment(html); let mut entries=BTreeMap::new();
    for card in doc.select(&sel(".vc_grid-item")) {
        let title=card.select(&sel("h4")).next().map(|h|text_of(&h)).filter(|x|!x.is_empty()).context("IWGDF chapter card missing title")?;
        let a=card.select(&sel("a.vc_gitem-link[href]")).next().context("IWGDF chapter card missing landing link")?;
        let url=common::link(base,a.value().attr("href").unwrap_or_default()).context("invalid IWGDF chapter link")?;
        let u=Url::parse(&url)?;
        if u.origin()!=base.origin() || u.query().is_some() || u.path()=="/" {bail!("IWGDF chapter card points outside permitted publisher pages");}
        entries.entry(url).or_insert(title);
    }
    if entries.is_empty(){return Err(anyhow!("IWGDF chapter cards empty"));}
    Ok(entries.into_iter().collect())
}

struct Pending { anchor:usize, label:String, parts:Vec<String> }
fn finish(pending:&mut Option<Pending>,original:&[crate::guidelines::doc::Block],rows:&mut Vec<(usize,crate::guidelines::doc::Block)>,seen:&mut HashSet<String>,warnings:&mut Vec<String>) {
    let Some(p)=pending.take() else {return}; let joined=p.parts.join("\n");
    let suffix=regex::Regex::new(r"(?is)^(.*?)\s*\(\s*(?:(?:GRADE recommendation|Strength of recommendation)\s*:\s*)?(Strong|Conditional|Weak)\s*;\s*(?:Certainty of evidence\s*:\s*)?(Very low|Low|Moderate|High)\s*\)\s*(?:©\s*\d{4})?\s*$").unwrap();
    let embedded=regex::Regex::new(r"(?i)\(\s*(?:(?:GRADE recommendation|Strength of recommendation)\s*:\s*)?(?:Strong|Conditional|Weak)\s*;\s*(?:Certainty of evidence\s*:\s*)?(?:Very low|Low|Moderate|High)\s*\)").unwrap();
    let Some(g)=suffix.captures(&joined) else {warnings.push(format!("IWGDF recommendation {} has no safely paired explicit grade; source blocks retained.",p.label));return};
    if embedded.find_iter(&joined).count()!=1 {warnings.push(format!("IWGDF recommendation {} contains multiple grade clauses; source blocks retained without a shared grade.",p.label));return;}
    if !seen.insert(p.label.clone()) {warnings.push(format!("Repeated IWGDF recommendation label {}; source retained.",p.label));return;}
    rows.push((p.anchor,crate::guidelines::doc::Block {block_id:format!("{}-recommendation-{}-{}",original[p.anchor].block_id,p.label,rows.len()+1),
        kind:BlockKind::Recommendation{label:Some(p.label),strength:Some(g[2].to_string()),certainty:Some(g[3].to_string()),population:None,
            grading_system:if joined.to_ascii_lowercase().contains("grade recommendation:"){Some("GRADE".into())} else {None},qualifier:None},
        text:g[1].trim().into(),heading_path:original[p.anchor].heading_path.clone(),page:original[p.anchor].page}));
}

/// Only parse the publisher's explicit list, never infer grades from neighboring
/// discussion or concatenate two labelled recommendations before pairing.
fn recommendations(blocks:&mut Vec<crate::guidelines::doc::Block>)->(usize,Vec<String>) {
    let marker=regex::Regex::new(r"(?:^|\s)(\d{1,2}[a-z]?)\.\s+([A-Z])").unwrap();
    let mut in_list=false;let mut pending:Option<Pending>=None;let mut rows=vec![];let mut seen=HashSet::new();let mut warnings=vec![];
    for (i,b) in blocks.iter().enumerate() {
        if matches!(b.kind,BlockKind::Heading{..}) {
            if b.text.eq_ignore_ascii_case("LIST OF RECOMMENDATIONS") {in_list=true;continue;}
            if in_list {finish(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);in_list=false;}
            continue;
        }
        if !in_list || !matches!(b.kind,BlockKind::Paragraph|BlockKind::ListItem) {continue;}
        let text=b.text.trim();let markers:Vec<_>=marker.captures_iter(text).collect();
        if markers.is_empty() {if let Some(p)=pending.as_mut(){p.parts.push(if matches!(b.kind,BlockKind::ListItem){format!("- {text}")}else{text.into()});}continue;}
        for (j,c) in markers.iter().enumerate() {
            if j==0 && c.get(0).unwrap().start()>0 {
                let prefix=text[..c.get(0).unwrap().start()].trim();if !prefix.is_empty(){if let Some(p)=pending.as_mut(){p.parts.push(prefix.into());}else{warnings.push(format!("Unpaired content before IWGDF recommendation in {}.",b.block_id));}}
            }
            finish(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);
            let end=markers.get(j+1).map(|x|x.get(0).unwrap().start()).unwrap_or(text.len());
            pending=Some(Pending{anchor:i,label:c[1].into(),parts:vec![text[c.get(2).unwrap().start()..end].trim().into()]});
        }
    }
    finish(&mut pending,blocks,&mut rows,&mut seen,&mut warnings);
    let count=rows.len();for (i,b) in rows.into_iter().rev(){blocks.insert(i+1,b);}
    if count==0 {warnings.push("No explicit IWGDF recommendation grades were safely parsed; PDF text retained for review.".into());}
    warnings.push("Recommendation coverage and PDF text, figures, table/footnote layout and per-asset rights require comparison with the original PDF.".into());
    (count,warnings)
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod source_tests {
    use super::*;
    use crate::guidelines::doc::{Block,FetchRecord};
    const CATALOG_HTML:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/catalog.html"));
    const CLASS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/classification-blocks.json"));
    const PREVENTION:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/prevention-blocks.json"));
    fn saved(html:&str,status:u16)->Fetched {Fetched{body:html.as_bytes().to_vec(),record:FetchRecord{url:CATALOG.into(),final_url:CATALOG.into(),status,content_type:Some("text/html".into()),..Default::default()}}}
    #[test]
    fn real_public_catalog_and_fail_closed_guards() {
        assert!(looks_like_login_wall(CATALOG_HTML));
        let clean=public_html(&saved(CATALOG_HTML,200),CATALOG,true).unwrap();
        let chapters=cards(&clean,&Url::parse(CATALOG).unwrap()).unwrap();
        assert_eq!(chapters.len(),10);
        assert!(chapters.iter().any(|(u,_)|u=="https://iwgdfguidelines.org/prevention-guideline-2023/"));
        assert!(chapters.iter().any(|(u,_)|u=="https://iwgdfguidelines.org/classification-2023/"));
        assert!(public_html(&saved(CATALOG_HTML,403),CATALOG,true).is_err());
        let gate=CATALOG_HTML.replace("<div id=\"Content\" role=\"main\">","<div id=\"Content\" role=\"main\"><form><input type=\"password\"></form>");
        assert_ne!(gate,CATALOG_HTML); assert!(public_html(&saved(&gate,200),CATALOG,true).is_err());
        let captcha=CATALOG_HTML.replace("<div id=\"Content\" role=\"main\">","<div id=\"Content\" role=\"main\"><div class=\"g-recaptcha\"></div>");
        assert_ne!(captcha,CATALOG_HTML);assert!(public_html(&saved(&captcha,200),CATALOG,true).is_err());
        assert!(public_html(&saved("<main><h1>Sign in to access</h1></main>",200),CATALOG,true).is_err());
    }
    #[test]
    fn source_pdf_recommendation_slices_keep_labels_and_conditions() {
        let mut c:Vec<Block>=serde_json::from_str(CLASS).unwrap();let (n,_)=recommendations(&mut c);assert_eq!(n,7);
        let recs:Vec<_>=c.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect();
        assert_eq!(recs.iter().filter_map(|b|if let BlockKind::Recommendation{label,..}=&b.kind{label.as_deref()}else{None}).collect::<Vec<_>>(),vec!["1a","1b","2","3a","3b","4","5"]);
        assert!(matches!(&recs[0].kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Strong" && c=="Low"));
        assert!(matches!(&recs[1].kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Conditional" && c=="Low"));
        let mut p:Vec<Block>=serde_json::from_str(PREVENTION).unwrap();let (n,_)=recommendations(&mut p);assert_eq!(n,2);
        let second=p.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="2")).unwrap();
        assert!(second.text.contains("history of foot ulceration") && second.text.contains("end-stage renal disease") && second.text.contains("pre-ulcerative lesion"));
        assert!(matches!(&second.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="Strong" && c=="High"));
    }
}

fn chapter(html:&str,base:&Url)->Result<(String,String,Vec<serde_json::Value>,Option<String>)> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel("#Content .post-wrapper-content")).next().context("IWGDF chapter landing body missing")?;
    let title=doc.select(&sel("#Subheader h1.title")).next().map(|e|text_of(&e)).filter(|t|!t.is_empty()).context("IWGDF chapter title missing")?;
    let mut full=BTreeMap::new();
    for a in root.select(&sel("a.vc_btn3[href]")) {
        let label=text_of(&a);let lower=label.to_lowercase();let title_lower=title.to_lowercase();
        let guideline=lower.contains("download") && lower.contains("guideline");
        // Two publisher companion types use their own exact source labels.
        // Match both the landing title and button wording to avoid treating an
        // unrelated PDF or a generic download as the primary source.
        let definitions=title_lower.starts_with("definitions & criteria") && lower.starts_with("download the definitions and criteria");
        let methodology=title_lower.starts_with("development and methodology iwgdf") && lower=="click here to download the iwgdf methodology";
        if !(guideline || definitions || methodology) {continue;}
        let url=common::link(base,a.value().attr("href").unwrap_or_default()).context("invalid IWGDF PDF link")?;
        let u=Url::parse(&url)?;
        if u.origin()!=base.origin() || !u.path().to_lowercase().ends_with(".pdf") {bail!("IWGDF full guideline button is not a same-origin PDF");}
        full.insert(url,label);
    }
    if full.len()!=1 {bail!("IWGDF landing requires exactly one publisher-labelled guideline or recognized companion PDF button; found {}",full.len());}
    let (pdf,_)=full.into_iter().next().unwrap();
    let mut seen=HashSet::new();let mut assets=Vec::new();
    for a in root.select(&sel("a[href]")) {
        let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};
        if url==pdf || !Url::parse(&url).ok().is_some_and(|u|u.path().to_lowercase().ends_with(".pdf")) || !seen.insert(url.clone()) {continue;}
        let parent=a.ancestors().filter_map(scraper::ElementRef::wrap).find(|e|e.value().name()=="p").map(|e|text_of(&e)).unwrap_or_default();
        let role=if parent.to_lowercase().contains("supplement") {"supplement"} else {"linked_pdf"};
        assets.push(json!({"url":url,"title":text_of(&a),"source_context":parent,"role":role,"rights":"unknown; asset not inspected"}));
    }
    let planned=root.select(&sel("p")).map(|p|text_of(&p)).find(|s|s.to_lowercase().contains("will be updated"));
    Ok((title,pdf,assets,planned))
}
fn item_type(title:&str)->Option<String> {
    let t=title.to_lowercase();
    if t.starts_with("practical guideline"){Some("summary".into())}
    else if t.starts_with("definitions & criteria"){Some("definitions".into())}
    else if t.starts_with("development and methodology iwgdf"){Some("methodology".into())}
    else if t.contains("guideline"){Some("guideline".into())}else{None}
}
fn publisher_rights(html:&str,url:&str)->Result<Rights> {
    let doc=Html::parse_fragment(html);
    let text=doc.select(&sel("#Content p")).map(|p|text_of(&p)).find(|p|p.contains("IWGDF") && p.contains("CC BY-NC-ND 4.0"))
        .context("IWGDF licence statement not recognized")?;
    let conditions:Vec<_>=doc.select(&sel("#Content li")).map(|li|text_of(&li)).filter(|t|t.contains("non-commercial") || t.contains("not modified") || t.contains("acknowledged") || t.contains("Reproduce substantial parts")) .collect();
    if conditions.len()<4 {bail!("IWGDF licence conditions incomplete or changed");}
    Ok(Rights {status:"restricted".into(),license:Some("CC BY-NC-ND 4.0".into()),terms_url:Some(url.into()),
        attribution:Some("The IWGDF is properly acknowledged as the original source and copyright holder.".into()),
        restrictions:conditions,rights_holders:vec!["International Working Group on the Diabetic Foot (IWGDF)".into()],evidence_urls:vec![url.into()],
        note:Some(format!("{text} Site separately asks permission for substantial reproduction in software and decision-support tools. Linked supplements require independent rights review.")),..Default::default()})
}

#[async_trait::async_trait]
impl Adapter for Iwgdf {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0){return Ok(vec![]);}
        let requested=src.catalog_url.as_deref().context("IWGDF catalog URL missing")?;
        if requested.trim_end_matches('/')!=CATALOG.trim_end_matches('/') {bail!("IWGDF public-response exception is scoped only to the verified 2023 catalog");}
        let catalog=f.get(requested).await?;
        let root=public_html(&catalog,requested,true)?;let catalog_flag=looks_like_login_wall(&catalog.text());
        let mut out=vec![];
        for (landing,card_title) in cards(&root,&Url::parse(&catalog.record.final_url)?)? {
            let work=common::slug(Url::parse(&landing)?.path().trim_matches('/'));
            let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:landing.clone(),title:Some(card_title),rendition:Some("pdf".into()),version:None,
                extra:json!({"work_key":work,"landing_url":landing,"catalog_fetch":&catalog.record,"catalog_heuristic_false_positive":catalog_flag})};
            match f.get(&landing).await {
                Ok(page)=>match public_html(&page,&landing,false).and_then(|_|chapter(&page.text(),&Url::parse(&page.record.final_url)?)) {
                    Ok((title,pdf,assets,planned))=>{
                        row.title=Some(title.clone());row.url=pdf.clone();row.version=if title.contains("2023 update"){Some("2023 update".into())}else{None};
                        row.extra["landing_fetch"]=json!(&page.record);row.extra["landing_heuristic_false_positive"]=json!(looks_like_login_wall(&page.text()));
                        row.extra["attachments"]=json!(assets);row.extra["planned_update_statement"]=json!(planned);
                        match f.get_ok(&pdf).await {
                            Ok(pdf_page) if pdf_page.is_pdf()=>{row.doc_key=format!("{work}-{}",&pdf_page.record.sha256[..12]);row.extra["pdf_sha256"]=json!(pdf_page.record.sha256);}
                            Ok(_)=>row.extra["discovery_error"]=json!("IWGDF full guideline button did not serve PDF bytes"),
                            Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                        }
                    }
                    Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                },
                Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
            }
            out.push(row);if limit.is_some_and(|n|out.len()>=n){break;}
        }
        Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("IWGDF discovery: {e}");}
        let pdf=f.get_ok(&row.url).await?;
        if !pdf.is_pdf(){bail!("IWGDF chapter is not PDF bytes");}
        if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=pdf.record.sha256){bail!("IWGDF chapter changed since planning; re-plan");}
        let (mut blocks,mut warnings,_)=blocks_for(&pdf,None,30).await?;
        if blocks.is_empty(){bail!("IWGDF chapter PDF has no extracted text");}
        let (_count,more)=recommendations(&mut blocks);warnings.extend(more);
        if row.extra["catalog_heuristic_false_positive"]==true || row.extra["landing_heuristic_false_positive"]==true {
            warnings.push("The shared raw-string login heuristic matched page CSS; source-specific validation found the publisher's expected public catalog and chapter structure and no actual password input or challenge. No authentication was used.".into());
        }
        warnings.push("Clinical PDF and any linked supplement require separate page-level table, footnote and diagram validation; linked assets were not acquired.".into());
        let mut fetches=vec![];
        for key in ["catalog_fetch","landing_fetch"] {if let Ok(r)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra[key].clone()){fetches.push(r);}}
        fetches.push(pdf.record);
        let rights_url="https://iwgdfguidelines.org/translations/copyright-and-permissions/";
        let mut rights=Rights{status:"unknown".into(),..Default::default()};
        let is_guideline=row.title.as_deref().is_some_and(|s|s.to_lowercase().contains("guideline"));
        if is_guideline {
            match f.get(rights_url).await {
                Ok(page)=>{
                    let heuristic_flag=looks_like_login_wall(&page.text());
                    match public_html(&page,rights_url,false).and_then(|html|publisher_rights(&html,rights_url)) {
                        Ok(r)=>{
                            rights=r;
                            if heuristic_flag {warnings.push("The shared raw-string login heuristic also matched the public rights page; source-specific validation found no actual password input or challenge.".into());}
                        },Err(e)=>warnings.push(format!("IWGDF publisher rights need review: {e:#}")),
                    }
                    fetches.push(page.record);
                }
                Err(e)=>warnings.push(format!("IWGDF publisher rights unavailable: {e:#}")),
            }
        }
        Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),
            title:row.title.clone().unwrap_or_else(||row.url.clone()),url:row.url.clone(),item_type:row.title.as_deref().and_then(item_type),
            version:row.version.clone(),status:"unknown".into(),rights,blocks,attachments:row.extra["attachments"].as_array().cloned().unwrap_or_default(),fetches,warnings,
            search_metadata:json!({"landing_url":row.extra["landing_url"],"planned_update_statement":row.extra["planned_update_statement"],"planned_update_is_not_a_published_version":true}),..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod landing_tests {
    use super::*;
    use crate::guidelines::doc::FetchRecord;
    const P:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/prevention.html"));
    const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/classification.html"));
    const R:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/rights.html"));
    const D:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/definitions.html"));
    const M:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/methodology.html"));
    fn saved(url:&str,html:&str)->Fetched {Fetched{body:html.as_bytes().to_vec(),record:FetchRecord{url:url.into(),final_url:url.into(),status:200,content_type:Some("text/html".into()),..Default::default()}}}
    #[test]
    fn authentic_companion_landings_have_distinct_roles_and_precise_assets() {
        let def="https://iwgdfguidelines.org/definitions-criteria-2023/";
        let met="https://iwgdfguidelines.org/methodology-2023/";
        for (url,html,suffix) in [(def,D,"/2023/05/DMRR3654.pdf"),(met,M,"/2023/07/IWGDF-2023-09-Methodology.pdf")] {
            let fetched=saved(url,html);assert!(public_html(&fetched,url,false).is_ok());
            let (title,pdf,assets,_)=chapter(html,&Url::parse(url).unwrap()).unwrap();assert!(pdf.ends_with(suffix));
            assert_eq!(item_type(&title).as_deref(),Some(if url==def {"definitions"} else {"methodology"}));
            assert!(assets.iter().all(|a|a["rights"].as_str().is_some_and(|s|s.contains("unknown"))));
            if url==met {assert!(assets.iter().any(|a|a["url"].as_str().is_some_and(|u|u.ends_with("/2023/05/IWGDF-2023-09-Methodology.pdf"))));}
        }
        let changed=D.replace("Download the Definitions and Criteria", "Download the full PDF");assert_ne!(changed,D);assert!(chapter(&changed,&Url::parse(def).unwrap()).is_err());
        let changed=M.replace("Click here to download the IWGDF Methodology", "Click here to download the document");assert_ne!(changed,M);assert!(chapter(&changed,&Url::parse(met).unwrap()).is_err());
    }
    #[test]
    fn authentic_landings_and_licence_are_narrowly_scoped() {
        for (url,html,pdf_suffix) in [
            ("https://iwgdfguidelines.org/prevention-guideline-2023/",P,"/2023/07/IWGDF-2023-02-Prevention-Guideline.pdf"),
            ("https://iwgdfguidelines.org/classification-2023/",C,"/2023/07/IWGDF-2023-03-Classification-Guideline.pdf"),
        ] {
            let page=saved(url,html);assert!(public_html(&page,url,false).is_ok());
            let (title,pdf,assets,planned)=chapter(html,&Url::parse(url).unwrap()).unwrap();
            assert!(title.contains("2023 update") && pdf.ends_with(pdf_suffix));
            assert!(assets.iter().any(|a|a["role"]=="supplement"));
            assert!(assets.iter().all(|a|a["url"]!=pdf && a["rights"].as_str().unwrap().contains("unknown")));
            if url.contains("prevention") {assert!(planned.as_deref().is_some_and(|s|s.contains("2027")));} else {assert!(planned.is_none());}
        }
        let url="https://iwgdfguidelines.org/translations/copyright-and-permissions/";
        let page=saved(url,R);let html=public_html(&page,url,false).unwrap();let r=publisher_rights(&html,url).unwrap();
        assert_eq!(r.status,"restricted");assert_eq!(r.license.as_deref(),Some("CC BY-NC-ND 4.0"));
        assert!(r.restrictions.iter().any(|s|s.contains("non-commercial")));
        assert!(r.restrictions.iter().any(|s|s.contains("Reproduce substantial parts") && s.contains("software")));
        assert_eq!(r.terms_url.as_deref(),Some(url));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod offline_integration_tests {
    use super::*;
    const CATALOG_HTML:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/catalog.html"));
    const LANDING:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/prevention.html"));
    const TERMS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/rights.html"));
    const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/prevention-page-3.pdf"));
    const LANDING_URL:&str="https://iwgdfguidelines.org/prevention-guideline-2023/";
    const PDF_URL:&str="https://iwgdfguidelines.org/wp-content/uploads/2023/07/IWGDF-2023-02-Prevention-Guideline.pdf";
    const TERMS_URL:&str="https://iwgdfguidelines.org/translations/copyright-and-permissions/";
    #[tokio::test]
    async fn genuine_classification_list_page_preserves_all_seven_exact_grades() {
        const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/iwgdf_diabetic_foot/classification-formal-page-4.pdf"));
        let url="https://iwgdfguidelines.org/wp-content/uploads/2023/07/IWGDF-2023-03-Classification-Guideline.pdf";
        let source=Fetched{body:PDF.to_vec(),record:crate::guidelines::doc::FetchRecord{url:url.into(),final_url:url.into(),status:200,content_type:Some("application/pdf".into()),sha256:crate::common::sha256_hex(PDF),..Default::default()}};
        let (mut blocks,_,_)=blocks_for(&source,None,30).await.unwrap();
        let (n,_)=recommendations(&mut blocks);assert_eq!(n,7,"all seven labelled statements are on this genuine physical page");
        let recs=blocks.iter().filter_map(|b|if let BlockKind::Recommendation{label,strength,certainty,..}=&b.kind{Some((label.as_deref().unwrap_or(""),strength.as_deref().unwrap_or(""),certainty.as_deref().unwrap_or(""),b.text.as_str()))}else{None}).collect::<Vec<_>>();
        let expected=[("1a","Strong"),("1b","Conditional"),("2","Strong"),("3a","Strong"),("3b","Conditional"),("4","Conditional"),("5","Strong")];
        for (r,(label,strength)) in recs.iter().zip(expected){assert_eq!((r.0,r.1,r.2),(label,strength,"Low"));}
        assert!(recs[2].3.starts_with("Do not use"));assert!(recs[4].3.contains("resources exist"));
        // This verifies the recommendation-list page, not the remainder of the PDF.
    }
    #[tokio::test]
    async fn real_public_pages_and_abstract_excerpt_offline() {
        // Use one unchanged publisher card for bounded offline discovery;
        // the independent complete-catalog fixture assertion checks the real inventory.
        assert_eq!(cards(CATALOG_HTML,&Url::parse(CATALOG).unwrap()).unwrap().len(),10);
        let card={
            let doc=Html::parse_document(CATALOG_HTML);
            doc.select(&sel(".vc_grid-item")).find(|c|c.select(&sel("a[href]")).any(|a|a.value().attr("href")==Some(LANDING_URL))).unwrap().html()
        };
        let bounded=format!("<div id='Content' role='main'><h2>THE 2023 IWGDF GUIDELINES ARE AVAILABLE!</h2>{card}</div>");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(CATALOG,bounded.as_bytes(),"text/html");
        f.seed_cache_for_tests(LANDING_URL,LANDING.as_bytes(),"text/html");
        f.seed_cache_for_tests(PDF_URL,PDF,"application/pdf");
        f.seed_cache_for_tests(TERMS_URL,TERMS.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"iwgdf_diabetic_foot","access":"public","enabled":true,"catalog_url":CATALOG})).unwrap();
        let plan=Iwgdf.discover(&src,&f,None).await.unwrap();assert_eq!(plan.len(),1);
        let row=&plan[0];assert_eq!(row.url,PDF_URL);assert!(!row.extra["discovery_error"].is_string());
        let d=Iwgdf.extract(&src,&f,row).await.unwrap();
        assert!(!d.blocks.is_empty());
        assert!(!d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})),"abstract narrative is not the formal recommendation list");
        assert!(d.warnings.iter().any(|w|w.contains("No explicit IWGDF recommendation grades")));
        assert!(d.warnings.iter().any(|w|w.contains("PDF text, figures")));
        assert_eq!(d.rights.terms_url.as_deref(),Some(TERMS_URL));
        assert_eq!(d.rights.status,"restricted");
        assert!(d.attachments.iter().any(|a|a["role"]=="supplement" && a["rights"].as_str().is_some_and(|s|s.contains("unknown"))));
        assert!(d.search_metadata["planned_update_statement"].as_str().is_some_and(|s|s.contains("2027")));
        let mut changed=PDF.to_vec();changed.extend_from_slice(b"\nchanged snapshot");f.seed_cache_for_tests(PDF_URL,&changed,"application/pdf");
        assert!(Iwgdf.extract(&src,&f,row).await.is_err());
    }
}
