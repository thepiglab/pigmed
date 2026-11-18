//! RCH's server-rendered A–Z inventory and ordered clinical/reference widgets.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{BlockKind, GuidelineDoc};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;

pub struct AuRch;
const WIDGETS: &str = "#rch-primary [id$='_uxWidgetHost_uxWidgetHost_widget_CB']";

fn catalog(html: &str, base: &Url) -> Result<Vec<(String, String)>> {
    let doc = Html::parse_document(html);
    let root = doc.select(&sel("#tabnav-letter-blocks")).next().context("RCH A–Z index missing")?;
    let mut seen = HashSet::new();
    let mut out = vec![];
    for a in root.select(&sel("a[href]")) {
        let Some(url) = common::link(base, a.value().attr("href").unwrap_or_default()) else { continue };
        let u = Url::parse(&url)?;
        if u.host_str() != base.host_str() || !u.path().starts_with("/clinicalguide/guideline_index/") || u.query().is_some() { continue; }
        if u.path().trim_end_matches('/') == "/clinicalguide/guideline_index" { continue; }
        // Aliases retain the emitted request URL; identity alone is case-insensitive.
        let identity = u.path().trim_end_matches('/').to_ascii_lowercase();
        if seen.insert(identity) { out.push((url, text_of(&a))); }
    }
    if out.is_empty() { bail!("RCH A–Z contains no topic links"); }
    Ok(out)
}

fn clinical_html(html: &str) -> Result<(Html, String, String)> {
    let doc = Html::parse_document(html);
    let title = common::first_text(&doc, "#rch-primary h1.hidden-print").context("RCH clinical title missing")?;
    // Widget ordinals vary: service/support guidance has no preceding endorsement widget.
    // Locate the explicitly labelled clinical content rather than assuming ctl01/ctl02.
    let keys:Vec<_>=doc.select(&sel(WIDGETS)).filter(|w|w.select(&sel("h2")).any(|h|text_of(&h).eq_ignore_ascii_case("Key points"))).collect();
    let body = if keys.len()==1 {keys[0]} else if keys.len()>1 {bail!("multiple RCH Key points widgets; clinical scope ambiguous")} else {
        // Two observed publisher templates lack Key points. Match title and
        // multiple distinctive headings inside a single widget, never the body/nav.
        let candidates:Vec<_>=doc.select(&sel(WIDGETS)).filter(|w| {
            if title=="Acute knee injuries - emergency department" {
                let h:Vec<_>=w.select(&sel("h2")).map(|h|text_of(&h).to_lowercase()).collect();
                h.iter().any(|s|s.trim()=="1. summary") && h.iter().any(|s|s.trim()=="10. references")
            } else if title=="Admission criteria for general medicine SSU and IPU" {
                let h:Vec<_>=w.select(&sel("h3")).map(|h|text_of(&h).to_lowercase()).collect();
                h.iter().any(|s|s.contains("patients appropriate for the short stay unit team") && s.contains("all criteria")) &&
                    h.iter().any(|s|s.contains("patients more appropriate for general medicine") && s.contains("any one"))
            } else {false}
        }).collect();
        if candidates.len()!=1 {bail!("RCH clinical widget lacks recognized Key points or a unique supported template");}
        candidates[0]
    };
    let refs = doc.select(&sel(WIDGETS)).find(|w| w.id() != body.id() && w.select(&sel("h2")).any(|h|text_of(&h).eq_ignore_ascii_case("Reference List")));
    // Avoid the CMS presentation-list wrappers, endorsement logo, print title and sidebars.
    let mut selected = format!("<div id='rch-clinical'>{}{}</div>", body.html(), refs.map(|r|r.html()).unwrap_or_default());
    let selected_doc=Html::parse_fragment(&selected);
    for e in selected_doc.select(&sel("script, div[title='Open with pdfFiller']")) {
        selected = selected.replacen(&e.html(), "", 1);
    }
    // Browser-extension bookkeeping is not a clinical revision. Remove attributes only:
    // an annotated anchor or clinical node remains intact, with its href/content unchanged.
    let extension_attribute = regex::Regex::new(r#"\sdata-pdffiller-[a-zA-Z0-9_-]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+))?"#).unwrap();
    selected = extension_attribute.replace_all(&selected, "").into_owned();
    let fragment = Html::parse_fragment(&selected);
    for heading in fragment.select(&sel("h1, h2, h3, h4, h5, h6")) {
        let images = heading.select(&sel("img")).collect::<Vec<_>>();
        if images.is_empty() { continue; }
        let mut text_heading = heading.html();
        let mut after = String::new();
        for image in images {
            text_heading = text_heading.replacen(&image.html(), "", 1);
            after.push_str(&image.html());
        }
        // The shared walker treats headings as text terminals. Keep heading-contained images
        // immediately after the heading, under that heading's context, rather than losing them.
        selected = selected.replacen(&heading.html(), &format!("{text_heading}{after}"), 1);
    }
    let fragment = Html::parse_fragment(&selected);
    let list_tags = regex::Regex::new(r"(?i)</?(?:ul|ol|li)\b[^>]*>").unwrap();
    for item in fragment.select(&sel("li")) {
        // This explicitly scoped source passage has nested volume/weight conditions. A plain
        // paragraph keeps the entire parent and its descendants in one retrievable block.
        // Retain inline anchors/formatting; only replace list wrappers, never clinical nodes.
        let content=text_of(&item);
        let scoped_admission=title=="Admission criteria for general medicine SSU and IPU" &&
            (content.starts_with("Going to Dolphin ward") || content.starts_with("Patient groups that usually are more appropriate"));
        if content.starts_with("Ideal volumes for blood culture collection:") || scoped_admission {
            let grouped = list_tags.replace_all(&item.inner_html(), " ").into_owned();
            selected = selected.replacen(&item.html(), &format!("<p>{grouped}</p>"), 1);
        }
    }
    Ok((doc, title, selected))
}

#[async_trait::async_trait]
impl Adapter for AuRch {
    async fn discover(&self, src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) { return Ok(vec![]); }
        let page = f.get_ok(src.catalog_url.as_deref().context("RCH catalog URL missing")?).await?;
        let topics = catalog(&page.text(), &Url::parse(&page.record.final_url)?)?;
        let mut out = vec![];
        for (url, title) in topics.into_iter().take(limit.unwrap_or(usize::MAX)) {
            let work = common::slug(Url::parse(&url)?.path().trim_end_matches('/').rsplit('/').next().unwrap_or(""));
            let mut row = PlanRow { source_id:src.id.clone(), doc_key:format!("{work}-unresolved"), url, title:Some(title), rendition:Some("html".into()), version:None, extra:json!({"work_key":work}) };
            // Snapshot actual clinical widgets, not mutable URLs, footer clocks or external script assets.
            match f.get_ok(&row.url).await {
                Ok(body) => match clinical_html(&body.text()) {
                    Ok((_,title, selected)) => {
                        let hash = crate::common::sha256_hex(selected.as_bytes());
                        row.title = Some(title);
                        row.doc_key = format!("{work}-{}", &hash[..12]);
                        row.extra["clinical_sha256"] = json!(hash);
                    }
                    Err(e) => row.extra["discovery_error"] = json!(format!("{e:#}")),
                },
                Err(e) => row.extra["discovery_error"] = json!(format!("{e:#}")),
            }
            out.push(row);
        }
        Ok(out)
    }

    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(e) = row.extra["discovery_error"].as_str() { bail!("RCH discovery: {e}"); }
        let fetched = f.get_ok(&row.url).await?;
        let base = Url::parse(&fetched.record.final_url)?;
        let (page, title, selected) = clinical_html(&fetched.text())?;
        let hash = crate::common::sha256_hex(selected.as_bytes());
        if row.extra["clinical_sha256"].as_str().is_some_and(|s| s != hash) { bail!("RCH clinical revision changed after planning; re-plan"); }
        let (mut blocks, mut warnings) = blocks_from_html(&selected, Some(&base), Some("#rch-clinical"));
        // The publisher explicitly calls these numbered statements Key points. Preserve every other
        // clinical list and nested condition as ordered source blocks; do not invent evidence grades.
        let mut n = 0;
        for b in &mut blocks {
            if matches!(b.kind, BlockKind::ListItem) && b.heading_path.last().is_some_and(|h| h.eq_ignore_ascii_case("Key points")) {
                n += 1;
                b.kind = BlockKind::Recommendation { label:Some(n.to_string()), strength:None, certainty:None,
                    population:None, grading_system:None, qualifier:Some("Key points".into()) };
            }
        }
        let has_key_points=blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text.eq_ignore_ascii_case("Key points"));
        if has_key_points && n == 0 { bail!("RCH Key points produced no numbered statements"); }
        if !has_key_points {warnings.push("This source template has no Key points section; clinical lists and instructions are retained in context without invented recommendation labels or grades.".into());}
        let mut dates = vec![];
        let clinical=Html::parse_fragment(&selected);
        for p in clinical.select(&sel("p")) {
            let t = text_of(&p);
            if let Some(raw) = t.strip_prefix("Last updated ") { dates.push(common::date("Last updated", raw, base.as_str())); }
        }
        let mut rights = common::unknown_rights();
        rights.terms_url = page.select(&sel("a[href]"))
            .find(|a| text_of(a) == "Terms and Conditions")
            .and_then(|a| common::link(&base, a.value().attr("href")?));
        if rights.terms_url.is_some() { rights.evidence_urls.push(row.url.clone()); }
        let attachments = clinical.select(&sel("a[href]"))
            .filter_map(|a| {
                let url = common::link(&base, a.value().attr("href")?)?;
                let path = Url::parse(&url).ok()?.path().to_ascii_lowercase();
                [".pdf", ".doc", ".docx"].iter().any(|ext| path.ends_with(ext))
                    .then(|| json!({"url":url,"label":text_of(&a),"role":"linked_resource","rights":"unknown"}))
            }).collect();
        warnings.push("Only explicitly numbered Key points are structured recommendations; other clinical instructions, nested conditions and tables remain in source order without inferred grades. Algorithm images require visual/OCR interpretation; their branch text is not inferred from alt text.".into());
        if !clinical.select(&sel("h2")).any(|h|text_of(&h).eq_ignore_ascii_case("Reference List") || text_of(&h).eq_ignore_ascii_case("10. References")) {
            warnings.push("No separately labelled Reference List appears in the source clinical widgets; no references were inferred.".into());
        }
        if page.select(&sel("div[title='Open with pdfFiller']")).next().is_some() {
            warnings.push("Excluded embedded pdfFiller extension UI and its chrome-extension logo; surrounding clinical links are retained.".into());
        }
        if dates.is_empty() { warnings.push("No source-labelled Last updated date found; revision identity uses clinical-widget checksum.".into()); }
        Ok(GuidelineDoc { source_id:src.id.clone(), doc_key:row.doc_key.clone(), work_key:row.extra["work_key"].as_str().map(String::from),
            title, url:row.url.clone(), item_type:None,
            status:"unknown".into(), updated:dates.last().and_then(|d| d.normalized.clone()), date_observations:dates,
            language:common::page_language(&page), rights, blocks, attachments, warnings, fetches:vec![fetched.record],
            ..Default::default() })
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn live_uppercase_heading_and_single_widget_keep_all_clinical_content() {
        let cases=[
            ("https://www.rch.org.au/clinicalguide/guideline_index/Abdominal_pain_-_acute/",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_rch/abdominal-acute.html")),"Abdominal pain - acute",4),
            ("https://www.rch.org.au/clinicalguide/guideline_index/Aboriginal_and_Torres_Strait_Islander_health__services_and_support/",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_rch/aboriginal-services.html")),"Aboriginal and Torres Strait Islander health: services and support",3),
        ];
        let tmp=tempfile::tempdir().unwrap(); let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        let inventory=format!("<div id='tabnav-letter-blocks'><a href='{}'>{}</a><a href='{}'>{}</a></div>",cases[0].0,cases[0].2,cases[1].0,cases[1].2);
        f.seed_cache_for_tests(HOME,inventory.as_bytes(),"text/html");
        for (url,html,_,_) in cases {f.seed_cache_for_tests(url,html.as_bytes(),"text/html");}
        let src:SourceEntry=serde_json::from_value(json!({"id":"au_rch","access":"public","enabled":true,"catalog_url":HOME})).unwrap();
        let rows=AuRch.discover(&src,&f,None).await.unwrap(); assert_eq!(rows.len(),2);
        for (i,row) in rows.iter().enumerate() {
            assert!(!row.extra["discovery_error"].is_string());
            let d=AuRch.extract(&src,&f,row).await.unwrap(); assert_eq!(d.title,cases[i].2);
            assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).count(),cases[i].3);
            if i==0 {
                assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..}) && b.text.contains("Conditions requiring urgent")));
                assert!(d.blocks.iter().any(|b|b.text=="Reference List"));
                assert!(d.blocks.iter().any(|b|b.text.contains("will not mask potentially serious causes of pain")));
            } else {
                assert!(d.blocks.iter().any(|b|b.text.contains("Providing culturally safe care")));
                assert!(d.blocks.iter().any(|b|b.text=="Cultural safety and training"));
                assert!(d.warnings.iter().any(|w|w.contains("No separately labelled Reference List")));
            }
        }
    }
    #[tokio::test]
    async fn real_numbered_knee_and_admission_criteria_templates() {
        let knee="https://www.rch.org.au/clinicalguide/guideline_index/fractures/Acute_knee_injuries_-_emergency_department/";
        let admission="https://www.rch.org.au/clinicalguide/guideline_index/Admission_criteria_for_general_medicine_SSU_and_IPU/";
        let knee_html=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_rch/knee.html"));
        let admission_html=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_rch/admission.html"));
        let tmp=tempfile::tempdir().unwrap(); let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        let inventory=format!("<div id='tabnav-letter-blocks'><a href='{knee}'>Acute knee injuries</a><a href='{admission}'>Admission criteria</a></div>");
        f.seed_cache_for_tests(HOME,inventory.as_bytes(),"text/html");
        f.seed_cache_for_tests(knee,knee_html.as_bytes(),"text/html"); f.seed_cache_for_tests(admission,admission_html.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"au_rch","access":"public","enabled":true,"catalog_url":HOME})).unwrap();
        let rows=AuRch.discover(&src,&f,None).await.unwrap(); assert_eq!(rows.len(),2);
        let k=AuRch.extract(&src,&f,&rows[0]).await.unwrap();
        assert_eq!(k.title,"Acute knee injuries - emergency department");assert!(k.item_type.is_none());
        assert!(k.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text=="1. Summary"));
        assert!(k.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text=="10. References"));
        assert!(k.blocks.iter().any(|b|b.text.contains("arranging appropriate discharge") && b.text.contains("follow-up")));
        assert!(k.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..}) && b.text.contains("Type of internal knee derangement")));
        assert!(k.blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u.ends_with("/Figure_1_Moderate_haemarthrosis.png"))));
        assert!(k.blocks.iter().any(|b|b.text.contains("Fabricant") && b.heading_path.iter().any(|h|h=="10. References")));
        assert!(k.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})));
        assert!(!k.warnings.iter().any(|w|w.contains("No separately labelled Reference List")));
        let a=AuRch.extract(&src,&f,&rows[1]).await.unwrap(); assert!(a.item_type.is_none());
        assert!(a.blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{..}) && b.text.contains("should fulfil ALL criteria")));
        for clause in ["Length of stay likely to be less than 48 hours","Clear diagnosis and management plan"] {
            assert!(a.blocks.iter().any(|b|b.text.contains(clause) && b.heading_path.iter().any(|h|h.contains("ALL criteria"))));
        }
        let dolphin=a.blocks.iter().find(|b|b.text.starts_with("Going to Dolphin ward")).unwrap();
        assert!(dolphin.text.contains("only cares for patients in Dolphin") && dolphin.text.contains("NOT on inpatient wards"));
        assert!(dolphin.heading_path.iter().any(|h|h.contains("ALL criteria")));
        let patient_groups=a.blocks.iter().find(|b|b.text.starts_with("Patient groups that usually are more appropriate")).unwrap();
        assert!(patient_groups.text.contains("Feeding difficulties") && patient_groups.text.contains("Failure to thrive"));
        assert!(patient_groups.heading_path.iter().any(|h|h.contains("ANY one")));
        assert!(a.blocks.iter().any(|b|b.text.contains("Neonates <28 days") && b.text.contains("Inpatient Unit bedcard")));
        assert!(a.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})));
        assert!(a.warnings.iter().any(|w|w.contains("no Key points section")));
        let bad=admission_html.replace("should fulfil ALL criteria","criteria no longer present");
        assert!(clinical_html(&bad).is_err());
    }
    const HOME: &str = "https://www.rch.org.au/clinicalguide/";
    const ASTHMA: &str = "https://www.rch.org.au/clinicalguide/guideline_index/Acute_asthma/";
    const FEVER: &str = "https://www.rch.org.au/clinicalguide/guideline_index/Febrile_child/";
    const CATALOG: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/au_rch/catalog.html"));
    const A: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/au_rch/acute-asthma.html"));
    const F: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/au_rch/febrile-child.html"));
    #[tokio::test]
    async fn actual_az_aliases_and_complete_clinical_reference_widgets() {
        let topics = catalog(CATALOG, &Url::parse(HOME).unwrap()).unwrap();
        assert!(topics.len() > 200);
        assert_eq!(topics.iter().filter(|(u,_)| u == ASTHMA).count(), 1);
        assert_eq!(topics.iter().filter(|(u,_)| u == FEVER).count(), 1);
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 20, "").unwrap();
        // Bound discovery with two real A–Z entries and a duplicate alias; complete source inventory
        // is independently parsed above. No fabricated clinical payload is used.
        let bounded = format!("<div id='tabnav-letter-blocks'><a href='{ASTHMA}'>Acute asthma</a><a href='{ASTHMA}'>Asthma acute (see Acute asthma)</a><a href='{FEVER}'>Febrile child</a></div>");
        f.seed_cache_for_tests(HOME,bounded.as_bytes(),"text/html");
        f.seed_cache_for_tests(ASTHMA,A.as_bytes(),"text/html");
        f.seed_cache_for_tests(FEVER,F.as_bytes(),"text/html");
        let src: SourceEntry = serde_json::from_value(json!({"id":"au_rch","access":"public","enabled":true,"catalog_url":HOME})).unwrap();
        let rows = AuRch.discover(&src,&f,None).await.unwrap();
        assert_eq!(rows.len(),2);
        assert_ne!(rows[0].doc_key, rows[1].doc_key);
        for (i, expected) in [(0,"2023-07"),(1,"2026-10")] {
            let d = AuRch.extract(&src,&f,&rows[i]).await.unwrap();
            assert_eq!(d.updated.as_deref(),Some(expected));
            assert!(d.published.is_none() && d.version.is_none());
            assert_eq!(d.status,"unknown");
            assert!(d.blocks.iter().any(|b| b.text == "Reference List"));
            assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);
            assert!(!d.blocks.iter().any(|b| b.text == "In this section" || b.text == "PIC Endorsed"));
            assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Heading{level:1})));
            let recs = d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect::<Vec<_>>();
            assert_eq!(recs.len(),if i == 0 {5} else {4});
            assert!(recs.iter().all(|b|matches!(&b.kind,BlockKind::Recommendation{strength:None,certainty:None,..})));
            let figs = d.blocks.iter().filter_map(|b|if let BlockKind::Figure{src_url:Some(u),..}=&b.kind {Some(u.as_str())}else{None}).collect::<Vec<_>>();
            assert_eq!(figs.len(),if i == 0 {4} else {2});
            if i == 0 {
                assert!(recs[0].text.contains("treat for anaphylaxis"));
                assert!(d.blocks.iter().any(|b|b.text.contains("proximity to emergency care") && b.heading_path.iter().any(|h|h == "Consider discharge when")));
                assert!(d.blocks.iter().any(|b|b.text.contains("Yung M")));
                assert!(figs.contains(&"https://www.rch.org.au/uploadedImages/Main/Content/clinicalguide/guideline_index/Flowchart-4-acute-asthma-life-threatening-mgmt.png"));
                assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..}) && b.text.contains("Life threatening")));
            } else {
                assert!(recs[0].text.contains("neonates ≤21 days"));
                assert!(d.blocks.iter().any(|b|b.text.contains("Blood culture")));
                assert!(d.blocks.iter().any(|b|b.text.contains("Alejandre C")));
                assert!(figs.contains(&"https://www.rch.org.au/uploadedImages/Main/Content/clinicalguide/guideline_index/febrile%20chlid%20CPG_flowchart2_Nov2026.jpg"));
                assert!(d.blocks.iter().any(|b|b.text.contains("If the infant is clinically unwell or investigations are delayed")));
                let volumes = d.blocks.iter().find(|b|b.text.starts_with("Ideal volumes for blood culture collection:")).expect("grouped volume conditions");
                assert!(matches!(volumes.kind, BlockKind::Paragraph));
                assert!(volumes.heading_path.iter().any(|h| h == "Blood culture"));
                for condition in ["Children >20 kg: 5-10 mL x2", "Inoculate aerobic bottle preferentially", "<10 mL", "If inadequate volume for aerobic bottle", "<5 mL, use paediatric specific bottle"] {
                    assert!(volumes.text.contains(condition), "missing scoped blood-culture condition {condition}");
                }
            }
        }
        let mut changed = rows[0].clone();
        changed.extra["clinical_sha256"] = json!("changed");
        assert!(AuRch.extract(&src,&f,&changed).await.unwrap_err().to_string().contains("changed after planning"));
    }

    #[tokio::test]
    async fn extension_attribute_changes_do_not_change_revision_or_remove_clinical_links() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(),true,500,1 << 20,"").unwrap();
        let bounded = format!("<div id='tabnav-letter-blocks'><a href='{ASTHMA}'>Acute asthma</a></div>");
        f.seed_cache_for_tests(HOME,bounded.as_bytes(),"text/html");
        f.seed_cache_for_tests(ASTHMA,A.as_bytes(),"text/html");
        let src: SourceEntry = serde_json::from_value(json!({"id":"au_rch","access":"public","enabled":true,"catalog_url":HOME})).unwrap();
        let before = AuRch.discover(&src,&f,None).await.unwrap();
        let changed = A.replace("data-pdffiller-skip=\"1\"", "data-pdffiller-skip=\"changed\" data-pdffiller-tried-at=\"999999999\"");
        assert_ne!(changed,A);
        f.seed_cache_for_tests(ASTHMA,changed.as_bytes(),"text/html");
        let after = AuRch.discover(&src,&f,None).await.unwrap();
        assert_eq!(before[0].doc_key,after[0].doc_key);
        assert_eq!(before[0].extra["clinical_sha256"],after[0].extra["clinical_sha256"]);
        let timestamp_only = changed.replace("999999999", "111111111");
        f.seed_cache_for_tests(ASTHMA,timestamp_only.as_bytes(),"text/html");
        let later = AuRch.discover(&src,&f,None).await.unwrap();
        assert_eq!(after[0].doc_key,later[0].doc_key);
        let (_,_,selected) = clinical_html(&changed).unwrap();
        assert!(selected.contains("href=\"https://www.rch.org.au/clinicalguide/guideline_index/Bronchiolitis/\""));
        assert!(selected.contains("bronchiolitis"));
        assert!(!selected.contains("data-pdffiller-"));
        AuRch.extract(&src,&f,&before[0]).await.unwrap();
    }
}
