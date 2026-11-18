//! eviQ: bounded hierarchy discovery and reader HTML. Print selection is not a PDF.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{DateObservation, GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use url::Url;

pub struct AuEviq;

// Observed clinical alerts outside #protocol. These selectors do not authorize a whole-page fallback.
const PREPANEL_ALERTS: &[&str] = &[
    "main#content > div.container.protocol-detail > div.m-b-2 > div.alert.alert-danger",
    "main#content > div.container.protocol-detail > div.m-b-2 > div.alert.alert-warning",
    "main#content > div.container.protocol-detail > div.alert.alert-warning.m-b-1 > div.alert.alert-warning",
];

fn id_from_url(u: &Url) -> Option<String> {
    let re = regex::Regex::new(r"^(\d+)(?:-|$)").unwrap();
    u.path_segments()?
        .find_map(|s| re.captures(s).map(|c| c[1].to_string()))
}
const ROOTS: &[&str] = &[
    "/medical-oncology",
    "/haematology",
    "/cellular-therapies",
    "/radiation-oncology",
    "/cancer-genetics",
    "/clinical-resources",
];
fn allowed_path(u: &Url) -> bool {
    let p = u.path().to_ascii_lowercase();
    p.starts_with("/additional-clinical-information-cancer-genetics/")
        || p.starts_with("/definitions/")
        || ROOTS
            .iter()
            .any(|r| p == *r || p.starts_with(&format!("{r}/")))
}
// Optional independently observed canonical leaves supplement the six ordinary roots.
// Canonical popup pages are allowed; query-bearing modal variants remain normal emitted links,
// never a guessed rewrite or alternate attempt after a seed failure.
fn source_seed_url(raw: &str, leaf: bool) -> Result<Url> {
    if raw.contains('%') || raw.contains('\\') || raw.split('/').any(|p| p == "." || p == "..") {
        bail!("eviQ source seed has traversal/encoded path syntax");
    }
    let u = Url::parse(raw).context("eviQ source seed must be absolute")?;
    if u.scheme() != "https"
        || u.host_str() != Some("www.eviq.org.au")
        || u.port_or_known_default() != Some(443)
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || !(allowed_path(&u) || (!leaf && u.path() == "/"))
    {
        bail!("eviQ source seed/evidence must use an official recognized canonical HTTPS route");
    }
    if leaf
        && id_from_url(&u).is_none()
        && !(u.path().starts_with("/definitions/")
            && u.path().trim_end_matches('/') != "/definitions")
    {
        bail!("eviQ independently observed seed is not a recognized clinical/definition leaf");
    }
    Ok(u)
}
fn source_seeds(src: &SourceEntry) -> Result<Vec<(String, Value)>> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    if let Some(value) = src.config.get("source_linked_documents") {
        let entries = value
            .as_array()
            .context("eviQ source_linked_documents must be an array")?;
        if entries.len() > 8 {
            bail!("eviQ independently observed seed bound exceeds8");
        }
        for entry in entries {
            let u = source_seed_url(
                entry["url"].as_str().context("eviQ seed URL missing")?,
                true,
            )?;
            let evidence = source_seed_url(
                entry["source_evidence_url"]
                    .as_str()
                    .context("eviQ seed source evidence missing")?,
                false,
            )?;
            if seen.insert(u.as_str().to_ascii_lowercase()) {
                out.push((u.to_string(),json!({"url":u.as_str(),"source_evidence_url":evidence.as_str(),"role":"independently source-observed canonical leaf; normal resource validation and semantic planning required"})));
            }
        }
    }
    if src.config["source_linked_only"].as_bool() == Some(true) && out.is_empty() {
        bail!("eviQ seed-only validation requires an independently observed leaf");
    }
    Ok(out)
}
fn catalog_links(html: &str, base: &Url) -> Vec<(String, Option<String>)> {
    let d = Html::parse_document(html);
    let mut seen = HashSet::new();
    let mut out = vec![];
    let mut add = |href: &str| {
        let Some(url) = common::link(base, href) else {
            return;
        };
        let Ok(u) = Url::parse(&url) else { return };
        if u.origin() != base.origin() || !allowed_path(&u) {
            return;
        }
        if seen.insert(url.clone()) {
            out.push((url, id_from_url(&u)));
        }
    };
    for a in d.select(&sel(
        "main#content a[href], main#content [data-url], main#content [data-remote]",
    )) {
        for attribute in ["href", "data-url", "data-remote"] {
            if let Some(value) = a
                .value()
                .attr(attribute)
                .filter(|v| v.starts_with('/') || v.starts_with("https://"))
            {
                add(value);
            }
        }
    }
    fn collect(v: &Value, out: &mut Vec<String>, depth: usize) {
        if depth > 20 {
            return;
        }
        for key in ["AliasPath", "PatientSheetAliasPath"] {
            if let Some(a) = v.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                out.push(a.into())
            }
        }
        for key in ["Categories", "Pages"] {
            if let Some(items) = v.get(key).and_then(Value::as_array) {
                for item in items {
                    collect(item, out, depth + 1)
                }
            }
        }
    }
    for script in d.select(&sel("script")) {
        let t = script.text().collect::<String>();
        if !t.contains("InitProtocolListing") {
            continue;
        }
        let Some(pos) = t.find("data:") else { continue };
        let mut stream =
            serde_json::Deserializer::from_str(t[pos + 5..].trim_start()).into_iter::<Value>();
        if let Some(Ok(v)) = stream.next() {
            let mut paths = vec![];
            collect(&v, &mut paths, 0);
            for p in paths {
                add(&p)
            }
        }
    }
    out
}
// eviQ's explicitly marked print block mirrors the live Overview/Detail schedule.
// Remove only this known replica, never tables with identical text in distinct day/cycle contexts.
fn reader_html(html: &str) -> String {
    let document = Html::parse_document(html);
    let mut reader = document.html();
    for replica in document.select(&sel("#pShow.show-print.visible-print-block")) {
        reader = reader.replacen(&replica.html(), "", 1);
    }
    reader
}
fn parsed(
    html: &str,
    base: &Url,
) -> Result<(
    String,
    String,
    String,
    Vec<crate::guidelines::doc::Block>,
    Vec<String>,
)> {
    let reader = reader_html(html);
    let d = Html::parse_document(&reader);
    let title = common::first_text(&d, "main#content h1").context("eviQ reader title missing")?;
    let root = d
        .select(&sel("main#content #protocol"))
        .next()
        .context("eviQ protocol panel root missing")?;
    if root.select(&sel(".panel-collapse")).next().is_none() {
        bail!("eviQ clinical panels missing");
    }
    let id = id_from_url(base).context("eviQ numeric document ID missing from canonical URL")?;
    let whole = common::first_text(&d, "#toolbar #protocol-id")
        .context("eviQ document ID header missing")?;
    let re = regex::Regex::new(r"(?i)\bID\s*[:#]?\s*(\d+)\s*v\.?\s*(\d+(?:\.\d+)*)").unwrap();
    let c = re
        .captures(&whole)
        .context("eviQ ID and source version not established")?;
    if c[1] != id {
        bail!("eviQ URL and document ID disagree");
    }
    let version = c[2].to_string();
    let (mut blocks, mut warnings) = blocks_from_html(&root.html(), Some(base), Some("#protocol"));
    // History alone is not the clinical body. Some PDF-backed algorithms have scope alerts
    // and an embedded PDF before #protocol, while the accordion contains only History.
    let supporting = |name: &str| {
        matches!(
            name.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end_matches(':')
                .to_ascii_lowercase()
                .as_str(),
            "history" | "version history" | "references"
        )
    };
    let panel_names = root
        .select(&sel(".panel-collapse"))
        .map(|panel| {
            panel
                .value()
                .attr("data-section-name")
                .map(str::to_owned)
                .or_else(|| {
                    panel
                        .ancestors()
                        .filter_map(scraper::ElementRef::wrap)
                        .find(|ancestor| ancestor.value().classes().any(|class| class == "panel"))
                        .and_then(|ancestor| {
                            ancestor
                                .select(&sel(".panel-heading"))
                                .next()
                                .map(|heading| text_of(&heading))
                        })
                })
                .or_else(|| {
                    panel
                        .select(&sel("h2,h3,h4,h5,h6"))
                        .next()
                        .map(|heading| text_of(&heading))
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let headings = blocks
        .iter()
        .filter(|block| {
            matches!(
                block.kind,
                crate::guidelines::doc::BlockKind::Heading { .. }
            )
        })
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>();
    let only_supporting = if panel_names.iter().all(|name| !name.trim().is_empty()) {
        panel_names.iter().all(|name| supporting(name))
    } else {
        !panel_names
            .iter()
            .any(|name| !name.trim().is_empty() && !supporting(name))
            && !headings.is_empty()
            && headings.iter().all(|name| supporting(name))
    };
    let preclinical = if base
        .path()
        .to_ascii_lowercase()
        .starts_with("/additional-clinical-information-cancer-genetics/")
    {
        d.select(&sel(
            "main#content > div.container.protocol-detail > div.m-b-2",
        ))
        .filter(|region| {
            region.select(&sel("h2,h3,h4")).next().is_some()
                && region.select(&sel("p,li,table")).next().is_some()
        })
        .map(|region| region.html())
        .collect::<Vec<_>>()
    } else {
        vec![]
    };
    if only_supporting && preclinical.is_empty() {
        if d.select(&sel(
            "main#content .protocol-pdf-link #divPDF iframe.pdfobject",
        ))
        .next()
        .is_some()
        {
            bail!("eviQ unsupported PDF-backed clinical body: #protocol contains only History/References; embedded PDF outside protocol has not been validated or extracted");
        }
        bail!("eviQ unsupported clinical body: #protocol contains only History/References");
    }
    // Extract the recognized source components in their DOM order. An alert nested
    // inside the accepted criteria component is retained once, not copied ahead of it.
    let selectors=format!("main#content #protocol, main#content #flags, main#content #warnings, {}, main#content > div.container.protocol-detail > div.m-b-2",PREPANEL_ALERTS.join(", "));
    let regions = d
        .select(&sel(&selectors))
        .filter(|e| {
            !e.value().classes().any(|c| c == "m-b-2")
                || preclinical.iter().any(|html| html == &e.html())
        })
        .collect::<Vec<_>>();
    let regions = regions
        .iter()
        .filter(|e| {
            !e.ancestors()
                .filter_map(scraper::ElementRef::wrap)
                .any(|ancestor| {
                    !std::ptr::eq(ancestor.value(), e.value())
                        && regions
                            .iter()
                            .any(|candidate| std::ptr::eq(ancestor.value(), candidate.value()))
                })
        })
        .map(|e| e.html())
        .collect::<Vec<_>>();
    (blocks, warnings) = region_blocks(&regions, base);
    if blocks.is_empty() {
        bail!("eviQ panel extraction empty");
    }
    warnings.push("Collapsed source panels are retained; verify regimen tabs, footnotes, warnings, history and related patient/algorithm assets. Print selection is HTML, not a verified PDF.".into());
    Ok((title, id, version, blocks, warnings))
}
// Roles without their own native ID/version retain independent URL identity. A parent URL
// number is a locator, never a patient-sheet version or publication state.
struct Resource {
    title: String,
    work_key: String,
    version: Option<String>,
    item_type: &'static str,
    primary_body_acquired: bool,
    blocks: Vec<crate::guidelines::doc::Block>,
    warnings: Vec<String>,
    source_metadata: Value,
}
fn region_blocks(
    regions: &[String],
    base: &Url,
) -> (Vec<crate::guidelines::doc::Block>, Vec<String>) {
    let mut blocks = vec![];
    let mut warnings = vec![];
    let mut ids = HashSet::new();
    for (index, region) in regions.iter().enumerate() {
        let (mut part, notes) = blocks_from_html(region, Some(base), None);
        for block in &mut part {
            if !ids.insert(block.block_id.clone()) {
                block.block_id = format!("eviq-region-{index}-{}", block.block_id);
                while !ids.insert(block.block_id.clone()) {
                    block.block_id.push('-');
                }
            }
        }
        blocks.extend(part);
        warnings.extend(notes);
    }
    (blocks, warnings)
}
// The observed3237 raw HTTP landing may carry only the source Download PDF
// link/empty viewer mount; PDFObject adds its iframe in the browser. This is
// descriptor evidence only and never authorizes claiming the PDF clinical body.
fn observed_algorithm_pdf_link(landing: scraper::ElementRef<'_>, base: &Url) -> Option<String> {
    if !base.path().eq_ignore_ascii_case("/clinical-resources/side-effect-and-toxicity-management/gastrointestinal/3237-algorithm-treatment-induced-diarrhoea-manag") {return None;}
    const PDF:&str="https://www.eviq.org.au/getmedia/36b47012-5532-4c12-bfef-540343a0f9bf/ID-3237-TreatmentInducedDiarrhoeaManagement-SC2526001627-May-2026-with-arrows.pdf.aspx?ext=.pdf";
    let anchor_selector = sel(".protocol-pdf-link > a[href]");
    let mut anchors = landing.select(&anchor_selector);
    let anchor = anchors.next()?;
    if anchors.next().is_some() {
        return None;
    }
    // The source button's direct text is "Download PDF", followed by the native
    // <span class="sr-only">Opens in new window</span>. Whole-anchor text includes
    // that accessibility notice; it is not the button's direct action label.
    let mut direct_text = String::new();
    for child in anchor.children() {
        if let scraper::Node::Text(text) = child.value() {
            direct_text.push_str(text);
        }
    }
    let label = direct_text.split_whitespace().collect::<Vec<_>>().join(" ");
    let url = common::link(base, anchor.value().attr("href")?)?;
    (label.eq_ignore_ascii_case("Download PDF") && url == PDF).then_some(url)
}
fn resource(html: &str, base: &Url) -> Result<Resource> {
    let reader = reader_html(html);
    let d = Html::parse_document(&reader);
    let path = base.path().to_ascii_lowercase();
    let url_key = || crate::common::sha256_hex(base.as_str().as_bytes())[..20].to_string();
    if path.ends_with("/patient-information") {
        let header = d
            .select(&sel(
                "main#content > .container.protocol-detail.patient-info-sheet",
            ))
            .next()
            .context("eviQ patient sheet header missing")?;
        let title = header
            .select(&sel("h1"))
            .next()
            .map(|e| text_of(&e))
            .context("eviQ patient title missing")?;
        let panels = d
            .select(&sel(
                "main#content #protocol.panel-group.accordion[role=tablist]",
            ))
            .next()
            .context("eviQ patient clinical accordion missing")?;
        if panels.select(&sel(".panel-collapse")).next().is_none() {
            bail!("eviQ patient clinical panels missing");
        }
        let mut regions = vec![header.html(), panels.html()];
        regions.extend(
            d.select(&sel("main#content > .container.printAvoidPageBreakInside"))
                .map(|e| e.html()),
        );
        let (blocks, warnings) = region_blocks(&regions, base);
        if blocks.is_empty() {
            bail!("eviQ patient sheet body empty");
        }
        return Ok(Resource {
            title,
            work_key: format!("patient-{}", url_key()),
            version: None,
            item_type: "patient_information",
            primary_body_acquired: true,
            blocks,
            warnings,
            source_metadata: json!({"native_id":null,"native_version":null,"metadata_scope":"patient sheet only; parent treatment metadata is not inherited"}),
        });
    }
    if path.starts_with("/definitions/") {
        let root = d
            .select(&sel("main#content > .container"))
            .find(|e| {
                e.select(&sel(".page-header > h1")).next().is_some()
                    && e.children()
                        .filter_map(scraper::ElementRef::wrap)
                        .any(|c| c.value().name() == "p")
            })
            .context("eviQ glossary definition root missing")?;
        let title = root
            .select(&sel(".page-header > h1"))
            .next()
            .map(|e| text_of(&e))
            .unwrap();
        let (blocks, warnings) = region_blocks(&[root.html()], base);
        return Ok(Resource {
            title,
            work_key: format!("definition-{}", url_key()),
            version: None,
            item_type: "glossary_definition",
            primary_body_acquired: true,
            blocks,
            warnings,
            source_metadata: json!({"native_id":null,"native_version":null}),
        });
    }
    // A source-emitted primary PDF is recorded as an unacquired asset, not interpreted as
    // HTML clinical text. Require the observed split landing/accordion template.
    if path.starts_with("/clinical-resources/") {
        if let Some(landing) = d
            .select(&sel("main#content > .container.protocol-detail"))
            .find(|e| {
                (e.select(&sel(
                    ".protocol-pdf-link > #divPDF > iframe.pdfobject[title=\"Embedded PDF\"]",
                ))
                .next()
                .is_some()
                    || observed_algorithm_pdf_link(*e, base).is_some())
                    && e.select(&sel("#protocol")).next().is_none()
            })
        {
            let title = landing
                .select(&sel("h1"))
                .next()
                .map(|e| text_of(&e))
                .context("eviQ algorithm landing title missing")?;
            if title.to_ascii_lowercase().contains("algorithm") {
                let panels = d
                    .select(&sel("main#content #protocol .panel"))
                    .collect::<Vec<_>>();
                if panels.is_empty()
                    || panels.iter().any(|panel| {
                        let heading = panel
                            .select(&sel(".panel-heading"))
                            .next()
                            .map(|e| text_of(&e))
                            .unwrap_or_default();
                        !matches!(
                            heading
                                .trim()
                                .trim_end_matches(':')
                                .to_ascii_lowercase()
                                .as_str(),
                            "history" | "version history" | "references"
                        )
                    })
                {
                    bail!(
                        "eviQ PDF algorithm has an unrecognized clinical/supporting panel layout"
                    );
                }
                let whole = common::first_text(&d, "#toolbar #protocol-id")
                    .context("eviQ algorithm ID missing")?;
                let re = regex::Regex::new(r"(?i)\bID\s*[:#]?\s*(\d+)\s*v\.?\s*(\d+(?:\.\d+)*)")
                    .unwrap();
                let caps = re
                    .captures(&whole)
                    .context("eviQ algorithm version missing")?;
                let id = id_from_url(base).context("eviQ algorithm URL ID missing")?;
                if caps[1] != id {
                    bail!("eviQ algorithm URL and ID disagree");
                }
                let (blocks, mut warnings) = region_blocks(&[landing.html()], base);
                warnings.push("Primary algorithm PDF bytes and MIME have not been acquired or validated; landing scope/notices only. History is supporting metadata, not the clinical algorithm body.".into());
                let supporting = d
                    .select(&sel("main#content > .container #protocol"))
                    .map(|e| e.html())
                    .collect::<Vec<_>>();
                return Ok(Resource {
                    title,
                    work_key: id.clone(),
                    version: Some(caps[2].to_string()),
                    item_type: "algorithm_landing",
                    primary_body_acquired: false,
                    blocks,
                    warnings,
                    source_metadata: json!({"native_id":id,"supporting_panels_html":supporting,"primary_pdf_landing_evidence":{"source_observed_download_url":observed_algorithm_pdf_link(landing,base),"viewer_iframe_in_received_html":landing.select(&sel(".protocol-pdf-link #divPDF iframe.pdfobject")).next().is_some(),"primary_body_acquired":false,"scope":"Source link/viewer descriptor only; History and landing notices are not the algorithm body"}}),
                });
            }
        }
    }
    let (title, id, version, blocks, warnings) = parsed(html, base)?;
    Ok(Resource {
        title,
        work_key: id.clone(),
        version: Some(version),
        item_type: if path.starts_with("/additional-clinical-information-cancer-genetics/") {
            "additional_clinical_information"
        } else {
            "protocol"
        },
        primary_body_acquired: true,
        blocks,
        warnings,
        source_metadata: json!({"native_id":id}),
    })
}
fn page_metadata(html: &str) -> Value {
    let d = Html::parse_document(html);
    json!({"language":d.select(&sel("html[lang]")).next().and_then(|e|e.value().attr("lang")).map(str::to_owned),
        "source_breadcrumbs":d.select(&sel("main#content .breadcrumb a, main#content .breadcrumb-nav a")).map(|e|text_of(&e)).collect::<Vec<_>>()})
}
fn clinical_hash(html: &str, base: &Url, catalog_patient: Option<&str>) -> Result<String> {
    let r = resource(html, base)?;
    let (dates, status, relations, attachments) = metadata(html, base);
    let stable = json!({"title":r.title,"work_key":r.work_key,"version":r.version,"item_type":r.item_type,
        "primary_body_acquired":r.primary_body_acquired,"source_metadata":r.source_metadata,"blocks":r.blocks,
        "dates":dates,"status":status,"relations":relations,"attachments":attachments,"catalog_patient":catalog_patient,
        "page_metadata":page_metadata(html)});
    Ok(crate::common::sha256_hex(&serde_json::to_vec(&stable)?))
}
fn catalog_patients(html: &str, base: &Url) -> HashMap<String, String> {
    fn collect(v: &Value, base: &Url, out: &mut HashMap<String, String>, depth: usize) {
        if depth > 20 {
            return;
        }
        if let (Some(p), Some(patient)) = (
            v.get("AliasPath").and_then(Value::as_str),
            v.get("PatientSheetAliasPath").and_then(Value::as_str),
        ) {
            if let (Some(page), Some(sheet)) = (common::link(base, p), common::link(base, patient))
            {
                if let (Ok(u), Ok(v)) = (Url::parse(&page), Url::parse(&sheet)) {
                    if u.origin() == base.origin()
                        && v.origin() == base.origin()
                        && allowed_path(&u)
                        && v.path()
                            .to_ascii_lowercase()
                            .contains("/patient-information")
                    {
                        out.insert(page.to_ascii_lowercase(), sheet);
                    }
                }
            }
        }
        for key in ["Categories", "Pages"] {
            if let Some(xs) = v.get(key).and_then(Value::as_array) {
                for x in xs {
                    collect(x, base, out, depth + 1)
                }
            }
        }
    }
    let d = Html::parse_document(html);
    let mut out = HashMap::new();
    for script in d.select(&sel("script")) {
        let t = script.text().collect::<String>();
        if !t.contains("InitProtocolListing") {
            continue;
        }
        let Some(p) = t.find("data:") else { continue };
        let mut stream =
            serde_json::Deserializer::from_str(t[p + 5..].trim_start()).into_iter::<Value>();
        if let Some(Ok(v)) = stream.next() {
            collect(&v, base, &mut out, 0)
        }
    }
    out
}
fn metadata(
    html: &str,
    base: &Url,
) -> (
    Vec<DateObservation>,
    Option<String>,
    Vec<Relation>,
    Vec<Value>,
) {
    let reader = reader_html(html);
    let d = Html::parse_document(&reader);
    let mut dates = vec![];
    let patient = base
        .path()
        .to_ascii_lowercase()
        .ends_with("/patient-information");
    let glossary = base.path().starts_with("/definitions/");
    let date_selector = if patient {
        "main#content > .container.printAvoidPageBreakInside dl.dl-horizontal dt"
    } else if glossary {
        "main#content > .container .page-header ~ dl.dl-horizontal dt"
    } else {
        "main#content dl.dl-horizontal dt"
    };
    for dt in d.select(&sel(date_selector)) {
        let label = text_of(&dt);
        if !["First approved:", "Last reviewed:", "Review due:"].contains(&label.as_str()) {
            continue;
        }
        let Some(dd) = dt
            .next_siblings()
            .filter_map(scraper::ElementRef::wrap)
            .next()
        else {
            continue;
        };
        if dd.value().name() != "dd" {
            continue;
        }
        let Some(t) = dd.select(&sel("time")).next() else {
            continue;
        };
        let mut date = common::date(label.trim_end_matches(':'), &text_of(&t), base.as_str());
        if let Some(iso) = t.value().attr("datetime").and_then(|x| x.get(..10)) {
            if regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$")
                .unwrap()
                .is_match(iso)
            {
                date.normalized = Some(iso.into())
            }
        }
        dates.push(date);
    }
    let status = if patient || glossary {
        None
    } else {
        d.select(&sel("#toolbar ul.meta li.item-show strong"))
            .map(|e| text_of(&e))
            .find(|s| {
                ["Endorsed", "Superseded", "Discontinued"]
                    .iter()
                    .any(|v| s.eq_ignore_ascii_case(v))
            })
    };
    let mut relations = vec![];
    let mut assets = vec![];
    let mut seen = HashSet::new();
    let alert_links = PREPANEL_ALERTS
        .iter()
        .map(|selector| format!("{selector} a[href]"))
        .collect::<Vec<_>>()
        .join(", ");
    let asset_selector=format!("#toolbar a[href], #related a[href], #flags a[href], #protocol a[href], main#content .protocol-pdf-link a[href], main#content > .container.protocol-detail > .m-b-2 a[href], main#content .patient-info-sheet a[href], {alert_links}");
    for a in d.select(&sel(&asset_selector)) {
        let Some(url) = common::link(base, a.value().attr("href").unwrap_or_default()) else {
            continue;
        };
        let Ok(u) = Url::parse(&url) else { continue };
        if u == *base || !seen.insert(url.clone()) {
            continue;
        }
        let label = text_of(&a);
        let path = u.path().to_ascii_lowercase();
        let role = if path.contains("/patient-information") {
            "patient_information"
        } else if path.ends_with(".pdf")
            || (label.to_ascii_lowercase().contains("pdf")
                && u.query_pairs()
                    .any(|(key, value)| key == "ext" && value == ".pdf"))
        {
            "linked_pdf"
        } else if path.contains("algorithm") {
            "algorithm"
        } else {
            "related"
        };
        if role == "linked_pdf" {
            assets.push(json!({"url":url,"role":role,"label":label,"bytes_downloaded":false,"mime_verified":false,"evidence_url":base.as_str()}));
        } else {
            relations.push(Relation {
                rel: "related".into(),
                target: url,
                scope: Some(format!("{role}: {label}")),
                evidence_url: Some(base.to_string()),
            });
        }
    }
    for frame in d.select(&sel(
        "main#content .protocol-pdf-link > #divPDF > iframe.pdfobject[src]",
    )) {
        if let Some(url) = common::link(base, frame.value().attr("src").unwrap_or_default()) {
            if seen.insert(url.clone()) {
                assets.push(json!({"url":url,"role":"linked_pdf","source_attribute":"iframe.src","bytes_downloaded":false,"mime_verified":false,"evidence_url":base.as_str()}));
            }
        }
    }
    for control in d.select(&sel("main#content [data-url], main#content [data-remote]")) {
        for attribute in ["data-url", "data-remote"] {
            let Some(raw) = control
                .value()
                .attr(attribute)
                .filter(|v| v.starts_with('/') || v.starts_with("https://"))
            else {
                continue;
            };
            let Some(url) = common::link(base, raw) else {
                continue;
            };
            assets.push(json!({"url":url,"role":"source_popup_target","source_attribute":attribute,"source_value":raw,"bytes_downloaded":false,"mime_verified":false,"evidence_url":base.as_str()}));
            if seen.insert(url.clone()) {
                relations.push(Relation {
                    rel: "related".into(),
                    target: url,
                    scope: Some(format!("source popup target ({attribute})")),
                    evidence_url: Some(base.to_string()),
                });
            }
        }
    }
    (dates, status, relations, assets)
}
#[async_trait::async_trait]
impl Adapter for AuEviq {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let start = Url::parse(src.catalog_url.as_deref().context("eviQ catalog missing")?)?;
        if start.scheme() != "https"
            || start.host_str() != Some("www.eviq.org.au")
            || start.port_or_known_default() != Some(443)
            || !start.username().is_empty()
            || start.password().is_some()
        {
            bail!("eviQ catalog origin invalid");
        }
        let seeds = source_seeds(src)?;
        let seed_evidence = seeds
            .iter()
            .map(|(url, evidence)| (url.to_ascii_lowercase(), evidence.clone()))
            .collect::<HashMap<_, _>>();
        let seed_paths = seeds
            .iter()
            .map(|(url, _)| Url::parse(url).unwrap().path().to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let mut queue = VecDeque::from(
            seeds
                .into_iter()
                .map(|(url, _)| (url, 0usize))
                .collect::<Vec<_>>(),
        );
        let seed_only = src.config["source_linked_only"].as_bool() == Some(true);
        if !seed_only {
            for root in ROOTS {
                queue.push_back((start.join(root)?.to_string(), 0usize));
            }
        }
        let mut seen = HashSet::new();
        let mut works = HashSet::new();
        let mut rows = vec![];
        let mut patients = HashMap::new();
        let mut coverage = vec![];
        let mut ordinary_root_attempts = 0usize;
        while let Some((url, depth)) = queue.pop_front() {
            let requested = Url::parse(&url)?;
            let seen_key = if seed_paths.contains(&requested.path().to_ascii_lowercase()) {
                format!(
                    "{}{}",
                    requested.origin().ascii_serialization(),
                    requested.path()
                )
                .to_ascii_lowercase()
            } else {
                url.to_ascii_lowercase()
            };
            if !seen.insert(seen_key) {
                continue;
            }
            let explicit_seed = seed_evidence.get(&url.to_ascii_lowercase());
            if seen.len() > 1000 {
                coverage.push(
                    "Traversal stopped at 1000 pages; remaining branches unvisited".to_string(),
                );
                break;
            }
            if ROOTS.contains(&requested.path()) {
                ordinary_root_attempts += 1;
            }
            let page = match f.get_ok(&url).await {
                Ok(v) => v,
                Err(e) => {
                    coverage.push(format!("Unresolved catalog or clinical route {url}: {e:#}"));
                    continue;
                }
            };
            let base = Url::parse(&page.record.final_url)?;
            if base.origin() != start.origin()
                || base.scheme() != "https"
                || !base.username().is_empty()
                || base.password().is_some()
            {
                coverage.push(format!(
                    "Source route left the permitted official eviQ origin: {url}"
                ));
                continue;
            }
            if explicit_seed.is_some()
                && (base.query().is_some()
                    || base.fragment().is_some()
                    || !base
                        .path()
                        .trim_end_matches('/')
                        .eq_ignore_ascii_case(requested.path().trim_end_matches('/')))
            {
                coverage.push(format!("Independently observed seed redirected outside its canonical source leaf: {url}"));
                continue;
            }
            if explicit_seed.is_some()
                || id_from_url(&base).is_some()
                || base.path().starts_with("/definitions/")
            {
                let resource = match resource(&page.text(), &base) {
                    Ok(v) => v,
                    Err(e) => {
                        coverage.push(format!("Unrecognized clinical template {url}: {e:#}"));
                        continue;
                    }
                };
                let patient = patients.get(&url.to_ascii_lowercase()).map(String::as_str);
                let hash = clinical_hash(&page.text(), &base, patient)?;
                if works.insert(resource.work_key.clone()) {
                    rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{}-{}-{}",resource.work_key,resource.version.as_deref().unwrap_or("snapshot"),&hash[..12]),url:base.to_string(),title:Some(resource.title),rendition:Some("html".into()),version:resource.version,extra:json!({"work_key":resource.work_key,"item_type":resource.item_type,"primary_body_acquired":resource.primary_body_acquired,"clinical_sha256":hash,"catalog_patient":patients.get(&url.to_ascii_lowercase())})});
                }
                if let Some(evidence) = explicit_seed {
                    if let Some(row) = rows.last_mut().filter(|row| row.url == base.as_str()) {
                        row.extra["source_link_evidence"] = evidence.clone();
                    }
                }
                // Follow only emitted non-regimen child routes; never synthesize a patient,
                // definition, popup, or additional-information endpoint from a parent ID.
                if depth < 5 {
                    for (link, _) in catalog_links(&page.text(), &base) {
                        let child = Url::parse(&link)?;
                        let path = child.path().to_ascii_lowercase();
                        if path.ends_with("/patient-information")
                            || path.starts_with("/definitions/")
                            || path.starts_with("/additional-clinical-information-cancer-genetics/")
                        {
                            queue.push_back((link, depth + 1));
                        }
                    }
                }
                if limit.is_some_and(|n| rows.len() >= n) {
                    coverage
                        .push("Requested item limit reached; remaining routes unvisited".into());
                    break;
                }
                continue;
            }
            let links = catalog_links(&page.text(), &base);
            patients.extend(catalog_patients(&page.text(), &base));
            if depth >= 5 {
                if !links.is_empty() {
                    coverage.push(format!(
                        "Depth limit reached at {url}; descendant routes unvisited"
                    ));
                }
                continue;
            }
            if links.is_empty() {
                coverage.push(format!("No recognized catalog links at {url}"));
                continue;
            }
            for (link, _) in links {
                queue.push_back((link, depth + 1));
            }
        }
        if rows.is_empty() {
            bail!(
                "eviQ discovery found no validated clinical documents; {}",
                coverage.join("; ")
            );
        }
        for row in &mut rows {
            row.extra["coverage_notes"] = json!(coverage);
            if !seed_evidence.is_empty() {
                row.extra["source_discovery_scope"] = json!({"independent_seed_count":seed_evidence.len(),"ordinary_roots_enqueued":!seed_only,"ordinary_root_attempts":ordinary_root_attempts,"source_linked_only":seed_only,"catalog_complete":false});
            }
        }
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        let page = f.get_ok(&row.url).await?;
        let base = Url::parse(&page.record.final_url)?;
        if base.scheme() != "https"
            || base.host_str() != Some("www.eviq.org.au")
            || base.port_or_known_default() != Some(443)
            || !base.username().is_empty()
            || base.password().is_some()
        {
            bail!("eviQ clinical response left its official origin");
        }
        if let Some(evidence) = row.extra.get("source_link_evidence") {
            let original = source_seed_url(
                evidence["url"]
                    .as_str()
                    .context("eviQ planned source seed URL missing")?,
                true,
            )?;
            source_seed_url(
                evidence["source_evidence_url"]
                    .as_str()
                    .context("eviQ planned source evidence missing")?,
                false,
            )?;
            if base.query().is_some()
                || base.fragment().is_some()
                || !base
                    .path()
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case(original.path().trim_end_matches('/'))
            {
                bail!("eviQ planned source seed identity changed");
            }
        }
        let hash = clinical_hash(&page.text(), &base, row.extra["catalog_patient"].as_str())?;
        if row.extra["clinical_sha256"].as_str() != Some(hash.as_str()) {
            bail!("eviQ clinical content changed since planning");
        }
        let resource = resource(&page.text(), &base)?;
        if row.version != resource.version {
            bail!("eviQ version changed since planning");
        }
        let (dates, native_status, mut relations, attachments) = metadata(&page.text(), &base);
        let mut warnings = resource.warnings;
        if let Some(patient) = row.extra["catalog_patient"].as_str() {
            if !relations
                .iter()
                .any(|r| r.target.eq_ignore_ascii_case(patient))
            {
                relations.push(Relation {
                    rel: "related".into(),
                    target: patient.into(),
                    scope: Some("patient_information: catalog link".into()),
                    evidence_url: Some(row.url.clone()),
                });
            }
        }
        if let Some(notes) = row.extra["coverage_notes"].as_array() {
            for note in notes.iter().filter_map(Value::as_str) {
                warnings.push(note.to_string());
            }
        }
        if row.extra["source_discovery_scope"]["source_linked_only"] == true {
            warnings.push("Seed-only validation uses independently source-observed canonical leaves and emitted non-regimen children; ordinary catalog roots were not traversed. No complete inventory/template coverage claim. Linked assets remain descriptors; native endorsement is not automatically current status.".into());
        } else {
            warnings.push("Discovery uses bounded observed clinical category roots; full inventory, hidden asset variants and template coverage are not established. Native endorsement is not automatically mapped to current status. Linked PDFs and companion documents are descriptors/relations, not downloaded clinical content.".into());
        }
        let mut search_metadata = json!({"source_status":native_status,"source_status_evidence":base.as_str(),"resource_role":resource.item_type,
                "primary_body_acquired":resource.primary_body_acquired,"source_fields":resource.source_metadata,
                "page_metadata":page_metadata(&page.text())});
        for key in ["source_link_evidence", "source_discovery_scope"] {
            if let Some(value) = row.extra.get(key) {
                search_metadata[key] = value.clone();
            }
        }
        Ok(GuidelineDoc {
            source_id: src.id.clone(),
            doc_key: row.doc_key.clone(),
            work_key: Some(resource.work_key),
            title: resource.title,
            item_type: Some(resource.item_type.into()),
            language: page_metadata(&page.text())["language"]
                .as_str()
                .map(str::to_owned),
            url: row.url.clone(),
            version: resource.version,
            status: "unknown".into(),
            date_observations: dates,
            relations,
            attachments,
            search_metadata,
            rights: Rights {
                status: "unknown".into(),
                terms_url: Some("https://www.eviq.org.au/copyright".into()),
                ..Default::default()
            },
            blocks: resource.blocks,
            warnings,
            fetches: vec![page.record],
            ..Default::default()
        })
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn patient_source_link_plans_and_extracts_an_independent_snapshot_offline() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        let base = Url::parse("https://www.eviq.org.au/").unwrap();
        let leaf="https://www.eviq.org.au/haematology/lymphoma/other-b-cell-lymphoma/4662-non-hodgkin-lymphoma-tafasitamab-lenalidomide/patient-information";
        let patient = include_str!(
            "../../../tests/fixtures/guidelines/au_eviq/patient-nonregimen-synthetic.html"
        );
        for root in ROOTS {
            let page = if *root == "/haematology" {
                format!("<main id='content'><a href='{leaf}'>Patient information</a></main>")
            } else {
                "<main id='content'></main>".into()
            };
            fetcher.seed_cache_for_tests(
                base.join(root).unwrap().as_str(),
                page.as_bytes(),
                "text/html",
            );
        }
        fetcher.seed_cache_for_tests(leaf, patient.as_bytes(), "text/html");
        let source:SourceEntry=serde_json::from_value(json!({"id":"au_eviq","name":"eviQ","enabled":true,"access":"public","adapter":"au_eviq","catalog_url":base.as_str(),"config":{}})).unwrap();
        let rows = AuEviq.discover(&source, &fetcher, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].version.is_none());
        assert_eq!(rows[0].extra["item_type"], "patient_information");
        let doc = AuEviq.extract(&source, &fetcher, &rows[0]).await.unwrap();
        assert!(doc.version.is_none() && doc.status == "unknown");
        assert_eq!(doc.item_type.as_deref(), Some("patient_information"));
        assert_eq!(doc.language.as_deref(), Some("en"));
        assert_eq!(doc.search_metadata["primary_body_acquired"], true);
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| b.text.starts_with("synthetic clinical panel"))
                .count(),
            7
        );
        fetcher.seed_cache_for_tests(
            leaf,
            patient.replace("2027-08-17", "2028-08-17").as_bytes(),
            "text/html",
        );
        assert!(AuEviq
            .extract(&source, &fetcher, &rows[0])
            .await
            .unwrap_err()
            .to_string()
            .contains("changed since planning"));
    }
    #[test]
    fn patient_panels_are_independent_of_parent_identity_status_and_dates() {
        let page = include_str!(
            "../../../tests/fixtures/guidelines/au_eviq/patient-nonregimen-synthetic.html"
        );
        let base=Url::parse("https://www.eviq.org.au/haematology/lymphoma/other-b-cell-lymphoma/4662-non-hodgkin-lymphoma-tafasitamab-lenalidomide/patient-information").unwrap();
        let r = resource(page, &base).unwrap();
        assert_eq!(r.item_type, "patient_information");
        assert!(r.primary_body_acquired && r.version.is_none());
        assert!(r.work_key.starts_with("patient-") && r.work_key != "4662");
        assert!(r.source_metadata["native_id"].is_null());
        let panels = r
            .blocks
            .iter()
            .filter(|b| b.text.starts_with("synthetic clinical panel"))
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            panels,
            (1..=7)
                .map(|n| format!("synthetic clinical panel {n}"))
                .collect::<Vec<_>>()
        );
        let safety = r
            .blocks
            .iter()
            .position(|b| b.text.contains("Patient safety flag"))
            .unwrap();
        assert!(
            safety
                > r.blocks
                    .iter()
                    .position(|b| b.text == "synthetic clinical panel 7")
                    .unwrap()
        );
        let (dates, status, links, _) = metadata(page, &base);
        assert!(status.is_none());
        assert_eq!(dates.len(), 3);
        assert_eq!(dates[2].normalized.as_deref(), Some("2027-08-17"));
        assert!(links.iter().any(|r| r.target.contains("/Haematology/")
            && r.scope
                .as_deref()
                .unwrap()
                .contains("Back to treatment protocol")));
        assert_eq!(
            clinical_hash(page, &base, None).unwrap(),
            clinical_hash(
                &page
                    .replace("ID4662 v99", "ID4662 v100")
                    .replace("Superseded", "Endorsed"),
                &base,
                None
            )
            .unwrap()
        );
        assert_ne!(
            clinical_hash(page, &base, None).unwrap(),
            clinical_hash(&page.replace("2027-08-17", "2028-08-17"), &base, None).unwrap()
        );
    }
    #[test]
    fn prepanel_criteria_and_literal_popup_targets_survive_without_fabricated_routes() {
        let page = include_str!(
            "../../../tests/fixtures/guidelines/au_eviq/criteria-nonregimen-synthetic.html"
        );
        let base=Url::parse("https://www.eviq.org.au/additional-clinical-information-cancer-genetics/3176-multiple-endocrine-neoplasia-type-1-men1-an").unwrap();
        let r = resource(page, &base).unwrap();
        assert_eq!(r.item_type, "additional_clinical_information");
        assert_eq!(r.version.as_deref(), Some("6"));
        let criterion = r
            .blocks
            .iter()
            .position(|b| b.text == "Nested criterion synthetic.")
            .unwrap();
        let references = r
            .blocks
            .iter()
            .position(|b| b.text == "Reference one synthetic.")
            .unwrap();
        assert!(criterion < references);
        assert!(r
            .blocks
            .iter()
            .any(|b| b.text == "Clinical criterion synthetic."));
        let (dates, status, links, assets) = metadata(page, &base);
        assert_eq!(status.as_deref(), Some("Endorsed"));
        assert_eq!(dates[1].normalized.as_deref(), Some("2026-09-25"));
        assert_eq!(
            assets
                .iter()
                .filter(|a| a["role"] == "source_popup_target")
                .count(),
            2
        );
        assert!(links
            .iter()
            .any(|r| r.target.ends_with("/definitions/pathogenic-variant")));
        assert!(links
            .iter()
            .any(|r| r.target.ends_with("?target=popupmodal")));
        assert!(!links.iter().any(|r| r.target.ends_with("/true")));
        assert!(catalog_links(page, &base)
            .iter()
            .any(|(url, _)| url.ends_with("?target=popupmodal")));
        let glossary = include_str!(
            "../../../tests/fixtures/guidelines/au_eviq/glossary-nonregimen-synthetic.html"
        );
        let leaf = Url::parse("https://www.eviq.org.au/definitions/pathogenic-variant").unwrap();
        let definition = resource(glossary, &leaf).unwrap();
        assert_eq!(definition.item_type, "glossary_definition");
        assert!(definition.version.is_none() && metadata(glossary, &leaf).0.is_empty());
        assert!(definition
            .blocks
            .iter()
            .any(|b| b.text == "Synthetic independent glossary definition."));
    }
    #[test]
    fn split_algorithm_is_only_a_landing_with_an_unacquired_primary_asset() {
        let page = include_str!(
            "../../../tests/fixtures/guidelines/au_eviq/algorithm-landing-synthetic.html"
        );
        let base=Url::parse("https://www.eviq.org.au/clinical-resources/3237-algorithm-treatment-induced-diarrhoea-manag").unwrap();
        let r = resource(page, &base).unwrap();
        assert_eq!(r.item_type, "algorithm_landing");
        assert!(!r.primary_body_acquired);
        assert_eq!(r.version.as_deref(), Some("4"));
        assert!(r
            .blocks
            .iter()
            .any(|b| b.text == "Scope and exclusion notice synthetic."));
        assert!(!r
            .blocks
            .iter()
            .any(|b| b.text.contains("Administrative history marker")));
        assert!(r.source_metadata["supporting_panels_html"][0]
            .as_str()
            .unwrap()
            .contains("Administrative history marker"));
        let (_, _, _, assets) = metadata(page, &base);
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0]["bytes_downloaded"], false);
        assert_eq!(assets[0]["mime_verified"], false);
        assert_ne!(
            clinical_hash(page, &base, None).unwrap(),
            clinical_hash(&page.replace("with-arrows.pdf", "revised.pdf"), &base, None).unwrap()
        );
    }
    #[test]
    fn exact_print_replica_is_removed_without_deduplicating_clinical_day_tables() {
        let page =
            include_str!("../../../tests/fixtures/guidelines/au_eviq/print-schedule-replica.html");
        let base = Url::parse("https://www.eviq.org.au/haematology/5005-example").unwrap();
        let (_, _, _, blocks, _) = parsed(page, &base).unwrap();
        let tables = blocks
            .iter()
            .filter(|block| matches!(block.kind, crate::guidelines::doc::BlockKind::Table { .. }))
            .collect::<Vec<_>>();
        assert_eq!(tables.len(), 6);
        assert!(tables[0].text.contains("Loading dose"));
        assert!(tables[1].text.contains("Subsequent dose"));
        assert_eq!(tables[3].text, tables[4].text);
        assert_eq!(tables[4].text, tables[5].text);
        assert_ne!(tables[3].heading_path, tables[4].heading_path);
        assert_ne!(tables[4].heading_path, tables[5].heading_path);
        assert!(tables[3]
            .heading_path
            .iter()
            .any(|heading| heading == "Day 2"));
        assert!(tables[5]
            .heading_path
            .iter()
            .any(|heading| heading == "Day 4"));
        assert!(!blocks.iter().any(|block| block.text.contains("PRINT COPY")));
        assert_eq!(
            clinical_hash(page, &base, None).unwrap(),
            clinical_hash(
                &page.replace("PRINT COPY", "CHANGED PRINT COPY"),
                &base,
                None
            )
            .unwrap()
        );
        let ordinary = page.replace(
            "class=\"show-print visible-print-block\"",
            "class=\"ordinary-content\"",
        );
        assert_eq!(
            parsed(&ordinary, &base)
                .unwrap()
                .3
                .iter()
                .filter(|block| matches!(
                    block.kind,
                    crate::guidelines::doc::BlockKind::Table { .. }
                ))
                .count(),
            12
        );
    }
    #[test]
    fn history_only_pdf_algorithm_is_not_a_clinical_body() {
        let page =
            include_str!("../../../tests/fixtures/guidelines/au_eviq/history-only-algorithm.html");
        let base=Url::parse("https://www.eviq.org.au/clinical-resources/side-effect-and-toxicity-management/gastrointestinal/3237-treatment-induced-diarrhoea").unwrap();
        let error = parsed(page, &base).unwrap_err().to_string();
        assert!(
            error.contains("unsupported PDF-backed clinical body")
                && error.contains("History/References")
        );
        assert!(clinical_hash(page, &base, None).is_err());
        let (dates, status, _, assets) = metadata(page, &base);
        assert_eq!(status.as_deref(), Some("Superseded"));
        assert_eq!(dates[0].normalized.as_deref(), Some("2031-01-02"));
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0]["bytes_downloaded"], false);
        assert_eq!(assets[0]["mime_verified"], false);
    }
    #[test]
    fn scoped_prepanel_alerts_and_unlabelled_references_are_preserved() {
        let page = include_str!(
            "../../../tests/fixtures/guidelines/au_eviq/prepanel-clinical-alerts.html"
        );
        let base = Url::parse(
            "https://www.eviq.org.au/clinical-resources/1287-synthetic-clinical-management",
        )
        .unwrap();
        let (_, _, _, blocks, _) = parsed(page, &base).unwrap();
        let alert = blocks
            .iter()
            .position(|block| {
                block
                    .text
                    .contains("Synthetic clinical scope and exclusion warning")
            })
            .unwrap();
        let prerequisite = blocks
            .iter()
            .position(|block| {
                block
                    .text
                    .contains("Synthetic prerequisite clinical warning")
            })
            .unwrap();
        let replacement = blocks
            .iter()
            .position(|block| block.text.contains("Synthetic superseded notice"))
            .unwrap();
        let management = blocks
            .iter()
            .position(|block| block.text == "Synthetic clinical management.")
            .unwrap();
        assert!(alert < prerequisite && prerequisite < replacement && replacement < management);
        // The final References panel has no data-section-name attribute and remains in the body.
        assert!(blocks
            .iter()
            .any(|block| block.text == "Synthetic retained reference."));
        let (dates, status, relations, _) = metadata(page, &base);
        assert_eq!(status.as_deref(), Some("Superseded"));
        assert_eq!(dates[0].normalized.as_deref(), Some("2031-01-02"));
        assert!(relations
            .iter()
            .any(|relation| relation.target.ends_with("1288-replacement")
                && relation.evidence_url.as_deref() == Some(base.as_str())));
        assert!(relations
            .iter()
            .any(|relation| relation.target.ends_with("4050-related-part")));
        let original = clinical_hash(page, &base, None).unwrap();
        assert_ne!(
            original,
            clinical_hash(
                &page.replace(
                    "Synthetic prerequisite clinical warning.",
                    "Changed prerequisite warning."
                ),
                &base,
                None
            )
            .unwrap()
        );
    }
    #[test]
    fn hierarchy_dedup_and_hidden_tables() {
        let base = Url::parse("https://www.eviq.org.au/medical-oncology/respiratory").unwrap();
        let links=catalog_links("<main id='content'><a href='/medical-oncology/a/1261-example'>A</a><a href='/medical-oncology/a/1261-example'>A</a><a href='/medical-oncology/a/1261-example/Patient-information'>Patient</a></main>",&base);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].1.as_deref(), Some("1261"));
        let embedded = r#"<script>function InitProtocolListing(){ call({data: {"AliasPath":"/medical-oncology","Categories":[{"AliasPath":"/medical-oncology/respiratory","Pages":[{"AliasPath":"/medical-oncology/respiratory/1261-example","PatientSheetAliasPath":"/medical-oncology/respiratory/1261-example/patient-information"}]}]}});}</script>"#;
        assert!(catalog_links(embedded, &base)
            .iter()
            .any(|(_, id)| id.as_deref() == Some("1261")));
        let page="<main id='content'><h1>Example regimen</h1><div id='toolbar'><ul class='meta'><li id='protocol-id' class='item-show'>ID: 1261 v.5</li><li class='item-show'><strong>Endorsed</strong></li></ul></div><div id='protocol' class='panel-group accordion'><div class='panel-collapse collapse' id='collapse17445'><h2>Schedule</h2><table><tr><th>Drug</th><th>Day</th></tr><tr><td>A</td><td>1</td></tr></table><p>Footnote: see details.</p></div></div></main>";
        let (_, id, version, blocks, _) = parsed(page, &Url::parse(&links[0].0).unwrap()).unwrap();
        assert_eq!(id, "1261");
        assert_eq!(version, "5");
        assert!(blocks
            .iter()
            .any(|b| matches!(b.kind, crate::guidelines::doc::BlockKind::Table { .. })));
        assert!(blocks.iter().any(|b| b.text.contains("Footnote")));
        assert!(parsed(
            &page.replace("ID: 1261 v.5", "ID: 999 v.5"),
            &Url::parse(&links[0].0).unwrap()
        )
        .is_err());
        let (dates, status, _, _) = metadata(page, &base);
        assert!(dates.is_empty());
        assert_eq!(status.as_deref(), Some("Endorsed"));
        let clinical_url = Url::parse(&links[0].0).unwrap();
        let original = clinical_hash(page, &clinical_url, None).unwrap();
        let shell =
            format!("<html><head><meta name='dynamic' content='changed'></head>{page}</html>");
        assert_eq!(
            original,
            clinical_hash(&shell, &clinical_url, None).unwrap()
        );
        assert_ne!(
            original,
            clinical_hash(
                &page.replace("Footnote: see details.", "Footnote: changed."),
                &clinical_url,
                None
            )
            .unwrap()
        );
        let with_flag = page.replace(
            "<div id='protocol'",
            "<div id='flags'><p>Important safety warning.</p></div><div id='protocol'",
        );
        let (_, _, _, blocks, _) = parsed(&with_flag, &clinical_url).unwrap();
        assert!(blocks
            .iter()
            .any(|b| b.text.contains("Important safety warning")));
        let patient = catalog_patients(embedded, &base);
        assert_eq!(patient.len(), 1);
        let asset_a = page.replace(
            "</main>",
            "<div id='related'><a href='/asset-a.pdf'>Companion</a></div></main>",
        );
        let asset_b = asset_a.replace("asset-a.pdf", "asset-b.pdf");
        assert_ne!(
            clinical_hash(&asset_a, &clinical_url, None).unwrap(),
            clinical_hash(&asset_b, &clinical_url, None).unwrap()
        );
        assert_ne!(
            clinical_hash(
                page,
                &clinical_url,
                Some("https://www.eviq.org.au/patient-information/a")
            )
            .unwrap(),
            clinical_hash(
                page,
                &clinical_url,
                Some("https://www.eviq.org.au/patient-information/b")
            )
            .unwrap()
        );
    }
    fn seed_contract() -> (SourceEntry, Vec<(&'static str, &'static str)>) {
        let patient="https://www.eviq.org.au/haematology/lymphoma/other-b-cell-lymphoma/4662-non-hodgkin-lymphoma-tafasitamab-lenalidomide/patient-information";
        let criteria="https://www.eviq.org.au/additional-clinical-information-cancer-genetics/3176-multiple-endocrine-neoplasia-type-1-men1-an";
        let glossary = "https://www.eviq.org.au/definitions/pathogenic-variant";
        let algorithm="https://www.eviq.org.au/clinical-resources/side-effect-and-toxicity-management/gastrointestinal/3237-algorithm-treatment-induced-diarrhoea-manag";
        let evidence="https://www.eviq.org.au/cancer-genetics/adult/risk-management/192-men1-or-cdkn1b-risk-management";
        let entries = vec![
            json!({"url":patient,"source_evidence_url":patient.trim_end_matches("/patient-information")}),
            json!({"url":criteria,"source_evidence_url":evidence}),
            json!({"url":glossary,"source_evidence_url":evidence}),
            json!({"url":algorithm,"source_evidence_url":algorithm}),
        ];
        let source=serde_json::from_value(json!({"id":"au_eviq","enabled":true,"access":"public","adapter":"au_eviq","catalog_url":"https://www.eviq.org.au/","config":{"source_linked_documents":entries,"source_linked_only":true}})).unwrap();
        (source,vec![(patient,include_str!("../../../tests/fixtures/guidelines/au_eviq/patient-nonregimen-synthetic.html")),(criteria,include_str!("../../../tests/fixtures/guidelines/au_eviq/criteria-nonregimen-synthetic.html")),(glossary,include_str!("../../../tests/fixtures/guidelines/au_eviq/glossary-nonregimen-synthetic.html")),(algorithm,include_str!("../../../tests/fixtures/guidelines/au_eviq/algorithm-landing-synthetic.html"))])
    }
    #[tokio::test]
    async fn independent_source_seeds_use_real_resource_snapshots_and_keep_catalog_roots_additive()
    {
        let (mut source, pages) = seed_contract();
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        for (url, html) in &pages {
            f.seed_cache_for_tests(url, html.as_bytes(), "text/html");
        }
        assert!(AuEviq
            .discover(&source, &f, Some(0))
            .await
            .unwrap()
            .is_empty());
        let limited = AuEviq.discover(&source, &f, Some(2)).await.unwrap();
        assert_eq!(limited.len(), 2);
        let rows = AuEviq.discover(&source, &f, Some(4)).await.unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter()
                .map(|r| r.extra["item_type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "patient_information",
                "additional_clinical_information",
                "glossary_definition",
                "algorithm_landing"
            ]
        );
        for (row, (url, html)) in rows.iter().zip(&pages) {
            assert_eq!(&row.url, url);
            assert_eq!(
                row.extra["clinical_sha256"],
                clinical_hash(html, &Url::parse(url).unwrap(), None).unwrap()
            );
            assert_eq!(row.extra["source_link_evidence"]["url"], *url);
            let doc = AuEviq.extract(&source, &f, row).await.unwrap();
            assert_eq!(
                doc.search_metadata["source_discovery_scope"]["ordinary_roots_enqueued"],
                false
            );
            assert_eq!(
                doc.search_metadata["source_link_evidence"],
                row.extra["source_link_evidence"]
            );
        }
        assert!(rows[0].version.is_none() && rows[2].version.is_none());
        assert_eq!(rows[1].version.as_deref(), Some("6"));
        assert_eq!(rows[3].extra["primary_body_acquired"], false);
        f.seed_cache_for_tests(
            pages[1].0,
            b"<main id='content'><h1>Unknown clinical template</h1></main>",
            "text/html",
        );
        let partial = AuEviq.discover(&source, &f, Some(4)).await.unwrap();
        assert_eq!(partial.len(), 3);
        assert!(partial[0].extra["coverage_notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v
                .as_str()
                .unwrap()
                .contains("Unrecognized clinical template")));
        f.seed_cache_for_tests(pages[1].0, pages[1].1.as_bytes(), "text/html");
        source.config["source_linked_only"] = json!(false);
        let first = AuEviq.discover(&source, &f, Some(1)).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].extra["source_discovery_scope"]["ordinary_roots_enqueued"],
            true
        );
        assert_eq!(
            first[0].extra["source_discovery_scope"]["ordinary_root_attempts"],
            0
        );
        for root in ROOTS {
            f.seed_cache_for_tests(
                &format!("https://www.eviq.org.au{root}"),
                b"<main id='content'></main>",
                "text/html",
            );
        }
        let all = AuEviq.discover(&source, &f, None).await.unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(
            all[0].extra["source_discovery_scope"]["ordinary_root_attempts"],
            6
        );
        f.seed_cache_for_tests(
            pages[1].0,
            pages[1]
                .1
                .replace(
                    "Nested criterion synthetic.",
                    "Changed synthetic criterion.",
                )
                .as_bytes(),
            "text/html",
        );
        assert!(AuEviq
            .extract(&source, &f, &rows[1])
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("changed since planning"));
        // Duplicate canonical seeds never create additional work or a fake catalog record.
        let duplicate = source.config["source_linked_documents"][0].clone();
        source.config["source_linked_documents"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert_eq!(source_seeds(&source).unwrap().len(), 4);
    }
    #[test]
    fn independent_seed_validation_rejects_unsafe_routes_and_requires_literal_source_evidence() {
        for raw in [
            "https://evil.test/definitions/pathogenic-variant",
            "http://www.eviq.org.au/definitions/pathogenic-variant",
            "https://u:p@www.eviq.org.au/definitions/pathogenic-variant",
            "https://www.eviq.org.au:444/definitions/pathogenic-variant",
            "https://www.eviq.org.au/definitions/pathogenic-variant?target=popupmodal",
            "https://www.eviq.org.au/definitions/pathogenic-variant#part",
            "https://www.eviq.org.au/clinical-resources/../definitions/pathogenic-variant",
            "https://www.eviq.org.au/definitions/%2e%2e/pathogenic-variant",
            "https://www.eviq.org.au/getmedia/unverified.pdf",
            "https://www.eviq.org.au/haematology",
            "/definitions/pathogenic-variant",
        ] {
            assert!(source_seed_url(raw, true).is_err(), "invalid seed accepted");
        }
        assert!(source_seed_url(
            "https://www.eviq.org.au/definitions/pathogenic-variant",
            true
        )
        .is_ok());
        let (mut source, _) = seed_contract();
        source.config["source_linked_documents"][0]["source_evidence_url"] =
            json!("https://evil.test/");
        assert!(source_seeds(&source).is_err());
        source.config = json!({"source_linked_only":true});
        assert!(source_seeds(&source).is_err());
        source.config = json!({});
        assert!(source_seeds(&source).unwrap().is_empty());
    }
    const ALGORITHM_PLACEHOLDER: &str = include_str!(
        "../../../tests/fixtures/guidelines/au_eviq/algorithm-link-placeholder-synthetic.html"
    );
    #[test]
    fn exact_source_pdf_link_recognizes_unacquired_algorithm_without_browser_iframe() {
        let url="https://www.eviq.org.au/clinical-resources/side-effect-and-toxicity-management/gastrointestinal/3237-algorithm-treatment-induced-diarrhoea-manag";
        let base = Url::parse(url).unwrap();
        let resource = resource(ALGORITHM_PLACEHOLDER, &base).unwrap();
        assert_eq!(resource.item_type, "algorithm_landing");
        assert!(!resource.primary_body_acquired);
        assert_eq!(resource.version.as_deref(), Some("4"));
        assert!(resource
            .blocks
            .iter()
            .any(|b| b.text == "Synthetic source scope and exclusion notice."));
        assert!(!resource
            .blocks
            .iter()
            .any(|b| b.text == "Synthetic administrative history only."));
        assert_eq!(
            resource.source_metadata["primary_pdf_landing_evidence"]
                ["viewer_iframe_in_received_html"],
            false
        );
        assert!(resource.source_metadata["supporting_panels_html"]
            .as_array()
            .unwrap()[0]
            .as_str()
            .unwrap()
            .contains("Synthetic administrative history"));
        let (dates, status, _, assets) = metadata(ALGORITHM_PLACEHOLDER, &base);
        assert_eq!(dates.len(), 3);
        assert_eq!(status.as_deref(), Some("Endorsed"));
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0]["role"], "linked_pdf");
        assert_eq!(assets[0]["bytes_downloaded"], false);
        // The browser's PDFObject-created iframe has no required title and adds a
        // viewer fragment; the source Download PDF link remains the authority.
        let viewed=ALGORITHM_PLACEHOLDER.replace("<div id=\"divPDF\" class=\"pdfobject-container\"></div>","<div id=\"divPDF\" class=\"pdfobject-container\"><iframe class=\"pdfobject\" src=\"/getmedia/36b47012-5532-4c12-bfef-540343a0f9bf/ID-3237-TreatmentInducedDiarrhoeaManagement-SC2526001627-May-2026-with-arrows.pdf.aspx?ext=.pdf#view=FitH\"></iframe></div>");
        assert!(
            !super::resource(&viewed, &base)
                .unwrap()
                .primary_body_acquired
        );
        for changed in [
            ALGORITHM_PLACEHOLDER.replace("Download PDF", "Unverified asset"),
            ALGORITHM_PLACEHOLDER.replace("with-arrows.pdf.aspx", "unverified-new.pdf.aspx"),
            ALGORITHM_PLACEHOLDER.replace(
                "class=\"protocol-pdf-link\"",
                "class=\"unverified-wrapper\"",
            ),
            ALGORITHM_PLACEHOLDER.replace("<h2>History</h2>", "<h2>Clinical treatment</h2>"),
        ] {
            assert!(super::resource(&changed, &base).is_err());
        }
        let other = Url::parse(
            "https://www.eviq.org.au/clinical-resources/other/9999-unverified-algorithm",
        )
        .unwrap();
        assert!(super::resource(ALGORITHM_PLACEHOLDER, &other).is_err());
    }
    const ACCESSIBLE_ALGORITHM: &str = include_str!(
        "../../../tests/fixtures/guidelines/au_eviq/algorithm-accessible-link-synthetic.html"
    );
    #[test]
    fn observed_accessible_pdf_button_keeps_native_label_and_exact_media_guards() {
        let base = Url::parse("https://www.eviq.org.au/clinical-resources/side-effect-and-toxicity-management/gastrointestinal/3237-algorithm-treatment-induced-diarrhoea-manag").unwrap();
        let document = Html::parse_document(ACCESSIBLE_ALGORITHM);
        let anchor = document
            .select(&sel(".protocol-pdf-link > a"))
            .next()
            .unwrap();
        assert_eq!(text_of(&anchor), "Download PDF Opens in new window");
        let parsed = resource(ACCESSIBLE_ALGORITHM, &base).unwrap();
        assert_eq!(parsed.item_type, "algorithm_landing");
        assert!(!parsed.primary_body_acquired);
        assert_eq!(parsed.version.as_deref(), Some("4"));
        assert_eq!(
            parsed.source_metadata["primary_pdf_landing_evidence"]
                ["viewer_iframe_in_received_html"],
            false
        );
        assert!(parsed.blocks.iter().all(|block| !matches!(
            block.kind,
            crate::guidelines::doc::BlockKind::Recommendation { .. }
        )));
        let duplicates = ACCESSIBLE_ALGORITHM.replace("</a>", "</a><a href=\"/getmedia/36b47012-5532-4c12-bfef-540343a0f9bf/ID-3237-TreatmentInducedDiarrhoeaManagement-SC2526001627-May-2026-with-arrows.pdf.aspx?ext=.pdf\">Download PDF</a>");
        for changed in [
            ACCESSIBLE_ALGORITHM.replace("Download PDF<span", "Unverified asset<span"),
            ACCESSIBLE_ALGORITHM.replace("with-arrows.pdf.aspx", "unverified-new.pdf.aspx"),
            ACCESSIBLE_ALGORITHM.replace(
                "class=\"protocol-pdf-link\"",
                "class=\"unverified-wrapper\"",
            ),
            ACCESSIBLE_ALGORITHM.replace("ID3237 v4", "ID9999 v4"),
            ACCESSIBLE_ALGORITHM.replace("ID3237 v4", "ID3237"),
            ACCESSIBLE_ALGORITHM.replace("<h2>History</h2>", "<h2>Clinical treatment</h2>"),
            ACCESSIBLE_ALGORITHM
                .replace("<a href=", "<div><a href=")
                .replace("</a>", "</a></div>"),
            duplicates,
        ] {
            assert!(resource(&changed, &base).is_err());
        }
        // A misleading direct label cannot borrow authority from the sr-only span.
        let span_only = ACCESSIBLE_ALGORITHM.replace(
            "Download PDF<span class=\"sr-only\">Opens in new window</span>",
            "<span class=\"sr-only\">Download PDF</span>",
        );
        assert!(resource(&span_only, &base).is_err());
    }
    #[tokio::test]
    async fn accessible_button_algorithm_real_plan_keeps_pdf_unacquired_and_body_false() {
        let url = "https://www.eviq.org.au/clinical-resources/side-effect-and-toxicity-management/gastrointestinal/3237-algorithm-treatment-induced-diarrhoea-manag";
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        fetcher.seed_cache_for_tests(url, ACCESSIBLE_ALGORITHM.as_bytes(), "text/html");
        let source: SourceEntry = serde_json::from_value(json!({"id":"au_eviq","enabled":true,"access":"public","adapter":"au_eviq","catalog_url":"https://www.eviq.org.au/","config":{"source_linked_only":true,"source_linked_documents":[{"url":url,"source_evidence_url":url}]}})).unwrap();
        let plan = AuEviq.discover(&source, &fetcher, Some(1)).await.unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].extra["primary_body_acquired"], false);
        let doc = AuEviq.extract(&source, &fetcher, &plan[0]).await.unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("algorithm_landing"));
        assert_eq!(doc.version.as_deref(), Some("4"));
        assert_eq!(doc.search_metadata["primary_body_acquired"], false);
        assert_eq!(doc.fetches.len(), 1);
        assert_eq!(doc.attachments.len(), 1);
        assert_eq!(doc.attachments[0]["bytes_downloaded"], false);
    }
    #[tokio::test]
    async fn source_link_only_algorithm_plan_preserves_descriptor_and_never_fetches_pdf() {
        let url="https://www.eviq.org.au/clinical-resources/side-effect-and-toxicity-management/gastrointestinal/3237-algorithm-treatment-induced-diarrhoea-manag";
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(url, ALGORITHM_PLACEHOLDER.as_bytes(), "text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"au_eviq","enabled":true,"access":"public","adapter":"au_eviq","catalog_url":"https://www.eviq.org.au/","config":{"source_linked_only":true,"source_linked_documents":[{"url":url,"source_evidence_url":url}]}})).unwrap();
        let rows = AuEviq.discover(&src, &f, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].extra["primary_body_acquired"], false);
        let doc = AuEviq.extract(&src, &f, &rows[0]).await.unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("algorithm_landing"));
        assert_eq!(doc.search_metadata["primary_body_acquired"], false);
        assert_eq!(doc.fetches.len(), 1);
        assert_eq!(doc.attachments.len(), 1);
        assert!(doc.blocks.iter().all(|b| !matches!(
            b.kind,
            crate::guidelines::doc::BlockKind::Recommendation { .. }
        )));
    }
}
