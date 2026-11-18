//! Endocrine Society clinical-area indexes and recommendation resource pages.
use super::{common, Adapter, PlanRow};
use crate::guidelines::{
    doc::{Block, BlockKind, FetchRecord, GuidelineDoc},
    fetch::{Fetched, Fetcher, Req},
    html::{blocks_from_html, sel, text_of},
    registry::SourceEntry,
};
use anyhow::{bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use url::Url;
pub struct EndocrineSociety;
fn links(html: &str, base: &Url) -> Vec<(String, String)> {
    let d = Html::parse_document(html);
    let mut seen = HashSet::new();
    let found = d
        .select(&sel("a[href]"))
        .filter_map(|a| {
            let u = common::link(base, a.value().attr("href")?)?;
            let p = Url::parse(&u).ok()?;
            let t = text_of(&a);
            (p.origin() == base.origin()
                && p.path().starts_with("/clinical-practice-guidelines/")
                && !t.is_empty()
                && seen.insert(u.clone()))
            .then_some((u, t))
        })
        .collect();
    found
}
fn areas(html: &str, base: &Url) -> Vec<(String, String)> {
    let d = Html::parse_document(html);
    let mut seen = HashSet::new();
    let out = d
        .select(&sel(".end-lp-promo > a[href]"))
        .filter_map(|a| {
            let u = common::link(base, a.value().attr("href")?)?;
            let p = Url::parse(&u).ok()?;
            let t = text_of(&a);
            (p.origin() == base.origin()
                && p.path().starts_with("/clinical-practice-guidelines/")
                && !t.is_empty()
                && seen.insert(u.clone()))
            .then_some((u, t))
        })
        .collect();
    out
}
struct ListingContract {
    endpoint: Url,
    page_id: String,
}
struct CatalogEntries {
    items: Vec<Value>,
    fetches: Vec<FetchRecord>,
    coverage: Value,
}
fn listing_contract(html: &str, base: &Url) -> Result<ListingContract> {
    let document = Html::parse_document(html);
    let search = regex::Regex::new(
        r#"["']?searchId["']?\s*:\s*["']dynamic-listing-([a-fA-F0-9-]{36})["']"#,
    )?;
    let endpoint = regex::Regex::new(
        r#"["']?url["']?\s*:\s*["']([^"']*EndDynamicContentListing\?[^"']+)["']"#,
    )?;
    let page_id = regex::Regex::new(r#"["']?pageId["']?\s*:\s*["']([a-fA-F0-9-]{36})["']"#)?;
    let mut found = None;
    for script in document.select(&sel("script")) {
        let text = script.text().collect::<String>();
        let configs = search.captures_iter(&text).collect::<Vec<_>>();
        for (index, config) in configs.iter().enumerate() {
            let start = config.get(0).unwrap().end();
            let end = configs
                .get(index + 1)
                .map(|next| next.get(0).unwrap().start())
                .unwrap_or(text.len());
            let slice = &text[start..end];
            let Some(url) = endpoint.captures(slice) else {
                continue;
            };
            let page = page_id
                .captures(slice)
                .context("Endocrine source listing pageId missing")?[1]
                .to_owned();
            if !page.eq_ignore_ascii_case(&config[1]) {
                bail!("Endocrine source listing and pageId disagree")
            }
            let url = base.join(&url[1])?;
            if url.origin() != base.origin()
                || url.scheme() != "https"
                || !url
                    .path()
                    .eq_ignore_ascii_case("/api/EndDynamicContentListing")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                bail!("Endocrine source listing endpoint left its permitted origin or path")
            }
            let pairs = url.query_pairs().collect::<Vec<_>>();
            let mut names = HashSet::new();
            for (key, _) in &pairs {
                if !names.insert(key.as_ref())
                    || ![
                        "listingId",
                        "showDescriptions",
                        "showDates",
                        "showContentTypes",
                        "showImages",
                        "showLinks",
                    ]
                    .contains(&key.as_ref())
                {
                    bail!("Endocrine source listing query contract changed")
                }
            }
            if !pairs
                .iter()
                .any(|(key, value)| key == "listingId" && !value.is_empty())
            {
                bail!("Endocrine source listingId missing")
            }
            if found.is_some() {
                bail!("Endocrine page has multiple dynamic listings; associations require review")
            }
            found = Some(ListingContract {
                endpoint: url,
                page_id: page,
            });
        }
    }
    found.context("Endocrine source-emitted dynamic listing configuration missing")
}
fn catalog_request(contract: &ListingContract, page: usize, per_page: usize) -> Req {
    let mut url = contract.endpoint.clone();
    url.query_pairs_mut()
        .append_pair("pageId", &contract.page_id)
        .append_pair("page", &page.to_string())
        .append_pair("perPage", &per_page.to_string())
        .append_pair("q", "")
        .append_pair("sortBy", "relevance")
        .append_pair("sortOrder", "asc")
        .append_pair("loadAllPages", "false");
    Req {
        cache_max_age_secs: Some(0),
        ..Req::get(url.as_str())
    }
}
fn count_field(value: &Value, label: &str) -> Result<usize> {
    let number = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
        .with_context(|| format!("Endocrine API {label} is not an unsigned count"))?;
    if number > 10_000 {
        bail!("Endocrine API {label} exceeds bounded contract")
    }
    Ok(number as usize)
}
fn catalog_json(page: &Fetched, base: &Url) -> Result<Value> {
    if page.record.status != 200 {
        bail!("Endocrine catalog API HTTP {}", page.record.status)
    }
    let url = Url::parse(&page.record.final_url)?;
    if url.origin() != base.origin()
        || url.scheme() != "https"
        || !url
            .path()
            .eq_ignore_ascii_case("/api/EndDynamicContentListing")
    {
        bail!("Endocrine catalog API response origin/path changed")
    }
    let mime = page
        .record
        .content_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if !mime.eq_ignore_ascii_case("application/json") {
        bail!("Endocrine catalog API did not return application/json")
    }
    let value: Value =
        serde_json::from_slice(&page.body).context("Endocrine catalog API JSON invalid")?;
    if String::from_utf8_lossy(&page.body).contains("SERVER_ERROR") {
        bail!("Endocrine catalog API returned SERVER_ERROR; request is unresolved without guessing parameters")
    }
    if !value.is_object()
        || !value["request"].is_object()
        || !value["results"].is_array()
        || !value["facets"].is_array()
    {
        bail!("Endocrine catalog API response shape changed")
    }
    Ok(value)
}
async fn catalog_entries(
    fetcher: &Fetcher,
    contract: &ListingContract,
    base: &Url,
) -> Result<CatalogEntries> {
    let (mut page, mut per_page) = (1, 4);
    let mut total = None;
    let mut items = vec![];
    let mut ids = HashSet::new();
    let mut fetches = vec![];
    let mut last_request = Value::Null;
    let mut facets = Value::Null;
    loop {
        if page > 100 {
            bail!("Endocrine catalog pagination exceeded 100 pages; inventory incomplete")
        }
        let response = fetcher
            .request(&catalog_request(contract, page, per_page))
            .await?;
        let value = catalog_json(&response, base)?;
        let actual_page = count_field(&value["request"]["page"], "request.page")?;
        let actual_size = count_field(&value["request"]["perPage"], "request.perPage")?;
        if actual_page != page
            || actual_size == 0
            || value["request"]["pageId"]
                .as_str()
                .is_none_or(|id| !id.eq_ignore_ascii_case(&contract.page_id))
        {
            bail!("Endocrine API returned inconsistent paging identity")
        }
        let count = count_field(&value["totalResults"], "totalResults")?;
        if total.is_some_and(|total| total != count) {
            bail!("Endocrine API catalog changed during pagination; replan")
        }
        total = Some(count);
        let results = value["results"].as_array().unwrap();
        if results.len() > actual_size {
            bail!("Endocrine API results exceed returned page size")
        }
        for item in results {
            let id = item["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .context("Endocrine API result source id missing")?;
            if !ids.insert(id.to_owned()) {
                bail!("Endocrine API repeated source IDs across pages; pagination unresolved")
            }
            items.push(item.clone());
        }
        fetches.push(response.record);
        last_request = value["request"].clone();
        facets = value["facets"].clone();
        if items.len() == count {
            break;
        }
        if items.len() > count || results.is_empty() {
            bail!("Endocrine API total/results disagree; catalog is incomplete")
        }
        // The source frontend adopts response.request.perPage. Both live contracts override sent4.
        per_page = actual_size;
        page += 1;
    }
    let coverage = json!({"complete_this_listing":true,"source_total_results":total,"raw_records":items.len(),"api_pages":fetches.len(),"returned_request":last_request,"source_facets":facets,"global_inventory_complete":false});
    Ok(CatalogEntries {
        items,
        fetches,
        coverage,
    })
}
fn clinical_snapshot(title: &str, blocks: &[Block], attachments: &[Value]) -> Result<String> {
    Ok(crate::common::sha256_hex(&serde_json::to_vec(
        &json!({"title":title,"blocks":blocks,"attachments":attachments}),
    )?))
}
// Only the source-observed all-image download-link layout on this exact resource is
// presentation. A row count, border, empty header or layout-looking class is not sufficient.
fn presentation_resources(table: ElementRef<'_>, base: &Url) -> Option<Vec<Value>> {
    if base.scheme() != "https"
        || base.host_str() != Some("www.endocrine.org")
        || base.path() != "/clinical-practice-guidelines/primary-aldosteronism-2"
        || table.value().attrs().next().is_some()
        || !text_of(&table).is_empty()
        || table
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|e| e.value().classes().any(|c| c.starts_with("panel-")))
    {
        return None;
    }
    if table.descendants().filter_map(ElementRef::wrap).any(|e| {
        e.id() != table.id() && !matches!(e.value().name(), "tbody" | "tr" | "td" | "a" | "img")
    }) {
        return None;
    }
    let rows = table.select(&sel("tr")).collect::<Vec<_>>();
    if rows.len() != 1 {
        return None;
    }
    let cells = common::cells(rows[0]);
    if cells.len() != 2 || cells.iter().any(|c| c.value().name() != "td") {
        return None;
    }
    let expected=[("/-/media/endocrine/files/cpg/primary_aldosteronism_medical_choice_decision_aid_single_page.pdf","/-/media/endocrine/images/clinical-practice/cpg_horizontal_download_button_primary_aldosteronism_medical_choice_decision_aid.jpg","decision_aid"),("/-/media/endocrine/files/cpg/cpg_patient_guide_understanding_primary_aldosteronism.pdf","/-/media/endocrine/images/clinical-practice/cpg_download_button_understanding_pa_patient_guide.jpg","patient_resource")];
    let mut out = vec![];
    for (cell, (pdf, image, role)) in cells.into_iter().zip(expected) {
        let anchors = cell
            .children()
            .filter_map(ElementRef::wrap)
            .collect::<Vec<_>>();
        if anchors.len() != 1 || anchors[0].value().name() != "a" {
            return None;
        }
        let a = anchors[0];
        let images = a
            .children()
            .filter_map(ElementRef::wrap)
            .collect::<Vec<_>>();
        if images.len() != 1 || images[0].value().name() != "img" {
            return None;
        }
        let img = images[0];
        if !img.value().attr("alt").unwrap_or("").trim().is_empty()
            || [a, img].iter().any(|e| {
                e.value()
                    .attr("aria-label")
                    .is_some_and(|v| !v.trim().is_empty())
            })
        {
            return None;
        }
        let href = common::link(base, a.value().attr("href")?)?;
        let target = Url::parse(&href).ok()?;
        let src = common::link(base, img.value().attr("src")?)?;
        let image_url = Url::parse(&src).ok()?;
        if target.origin() != base.origin()
            || target.path() != pdf
            || target.query().is_some()
            || target.fragment().is_some()
            || image_url.origin() != base.origin()
            || image_url.path() != image
        {
            return None;
        }
        out.push(json!({"url":href,"role":role,"source_evidence_url":base.as_str(),"role_evidence":"2026-11-14 source observation: image-only decision-aid/patient-guide download wrapper, outside question panels","presentation_wrapper":{"clinical_table":false,"source_table_html_sha256":crate::common::sha256_hex(table.html().as_bytes()),"source_anchor_html":a.html(),"source_image_html":img.html(),"image_url":src,"source_alt":img.value().attr("alt"),"source_row_count":1,"source_cells":2}}));
    }
    Some(out)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
fn content(html: &str, base: &Url) -> Result<(String, Vec<Block>, Vec<Value>, Vec<String>)> {
    let document = Html::parse_document(html);
    let title = document
        .select(&sel("h1"))
        .map(|heading| text_of(&heading))
        .find(|text| !text.is_empty())
        .context("Endocrine title missing")?;
    // Recommendation pages print "List of Recommendations" (sometimes "List of Recommendation") or, on
    // older resource pages, "Summary of Recommendations". Resource pages that print no recommendation
    // list at all are summaries of the journal guideline: their root is the block naming the Full Guideline.
    let rich_text = sel(".rich-text");
    let candidates = || {
        document.select(&rich_text).filter(|e| !e.value().classes().any(|c| c == "end-page-header"))
    };
    let root = candidates()
        .find(|element| {
            let text = text_of(element);
            (text.contains("List of Recommendation") && text.contains("Essential Points"))
                || text.contains("Summary of Recommendations")
        })
        .or_else(|| candidates().find(|element| text_of(element).contains("Full Guideline")));
    let Some(root) = root else { bail!("Endocrine recommendation page root missing") };
    // Some resource pages close the summary block before "List of Recommendations"; the list and its
    // question panels follow as siblings of that block. Rebuild one root from the block and its following
    // siblings (in source order) and parse that, so the recommendations are not lost to the summary block.
    if !text_of(&root).contains("List of Recommendation") && !text_of(&root).contains("Summary of Recommendations") {
        let following = root.next_siblings().filter_map(ElementRef::wrap).map(|e| e.html()).collect::<Vec<_>>().join("\n");
        let tail = Html::parse_fragment(&following);
        if text_of(&tail.root_element()).contains("List of Recommendation") {
            let rebuilt = format!("<html><body><h1>{}</h1><div class=\"rich-text\" data-pigmed-rebuilt=\"summary-plus-following-siblings\">{}\n{}</div></body></html>",
                escape_html(&title), root.inner_html(), following);
            let (t, b, a, mut w) = content(&rebuilt, base)?;
            w.push("The recommendation list follows the page's summary block as sibling markup; both were parsed together in source order.".into());
            return Ok((t, b, a, w));
        }
    }
    let lower = text_of(&root).to_lowercase();
    if lower.contains("performing security verification")
        || lower.contains("sign in to continue")
        || root.select(&sel("input[type=password]")).next().is_some()
    {
        bail!("Endocrine content gated")
    }
    let mut raw = root.html();
    let mut question_remarks = vec![];
    let mut presentation = HashMap::new();
    for table in root.select(&sel("table")) {
        if let Some(resources) = presentation_resources(table, base) {
            let cells = common::cells(table.select(&sel("tr")).next().unwrap());
            let inline = cells
                .iter()
                .map(ElementRef::inner_html)
                .collect::<Vec<_>>()
                .join("\n");
            raw = raw.replacen(
                &table.html(),
                &format!(
                    r#"<div data-endocrine-role="presentation-resource-links">{inline}</div>"#
                ),
                1,
            );
            for resource in resources {
                presentation.insert(resource["url"].as_str().unwrap().to_string(), resource);
            }
        }
    }
    // Legacy panels sometimes print several numbered statements in one paragraph separated by <br>.
    // Split only where the next line starts with a statement number, so each statement is its own block.
    let br_label = regex::Regex::new(r"<br\s*/?>\s*((?:R\s+)?[0-9]+\.[0-9]+\.?\s)")?;
    for paragraph in root.select(&sel("div.subcategory p")) {
        let html = paragraph.html();
        if br_label.is_match(&html) {
            raw = raw.replacen(&html, &br_label.replace_all(&html, "</p><p>$1"), 1);
        }
    }
    let modern_label = regex::Regex::new(r"^Recommendation\s+[0-9]+(?:\.[0-9]+)*[a-z]?[.:]?$")?;
    let remark_label = regex::Regex::new(r"(?i)^technical remarks?\s*:?\s*$")?;
    // Panels that print the label as a heading ("<h3>Recommendation 1.1:</h3>") followed by exactly one
    // statement paragraph: join label and statement, as for the paragraph-label form below.
    for heading in root.select(&sel("h3, h4, h5")) {
        if !modern_label.is_match(&text_of(&heading)) {
            continue;
        }
        let Some(statement) = heading.next_siblings().filter_map(ElementRef::wrap).next() else { continue };
        if statement.value().name() != "p" || text_of(&statement).is_empty() || remark_label.is_match(&text_of(&statement)) {
            continue;
        }
        let combined = format!("<p>{} {}</p>", heading.inner_html(), statement.inner_html());
        raw = raw.replacen(&heading.html(), "", 1).replacen(&statement.html(), &combined, 1);
    }
    // Legacy labels and newer question buttons are native headings. The collapsed source
    // panels remain in their original DOM position; no click or hidden-content boundary is inferred.
    for anchor in root.select(&sel("a.toggleSubCategory")) {
        let paragraph = anchor
            .parent()
            .and_then(ElementRef::wrap)
            .context("Endocrine legacy category label has no parent")?;
        if paragraph.value().name() != "p" {
            bail!("Endocrine legacy category label is not in its observed paragraph wrapper")
        }
        let panel = paragraph
            .next_siblings()
            .filter_map(ElementRef::wrap)
            .next()
            .context("Endocrine legacy category panel missing")?;
        if panel.value().name() != "div"
            || !panel.value().classes().any(|class| class == "subcategory")
        {
            bail!("Endocrine legacy category/panel association changed")
        }
        raw = raw.replacen(
            &paragraph.html(),
            &format!("<h4>{}</h4>", anchor.inner_html()),
            1,
        );
    }
    for button in root.select(&sel(
        "section.rich-text__embed > button[class^='accordion-']",
    )) {
        let panel = button
            .next_siblings()
            .filter_map(ElementRef::wrap)
            .next()
            .context("Endocrine question panel missing")?;
        if panel.value().name() != "div"
            || !panel
                .value()
                .classes()
                .any(|class| class.starts_with("panel-"))
        {
            bail!("Endocrine question/panel association changed")
        }
        let question = text_of(&button);
        // The native direct-child label (singular/plural, optional colon) starts
        // this panel's context. Do not find a phrase inside the recommendation itself.
        let marker = panel.children().filter_map(ElementRef::wrap).find(|e| {
            e.value().name() == "p"
                && (remark_label.is_match(&text_of(e))
                    || e.select(&sel("strong,b"))
                        .any(|label| remark_label.is_match(&text_of(&label))))
        });
        if let Some(marker) = marker {
            let remarks = std::iter::once(marker)
                .chain(marker.next_siblings().filter_map(ElementRef::wrap))
                .map(|e| text_of(&e))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            question_remarks.push((question, remarks));
        }
        // Modern society pages print the label in its own paragraph, followed by exactly
        // one statement paragraph. A label-only paragraph is not a clinical recommendation.
        let first = panel.children().filter_map(ElementRef::wrap).next();
        if let Some(label_paragraph) = first.filter(|element| element.value().name() == "p") {
            let label_text = text_of(&label_paragraph);
            if modern_label.is_match(&label_text) {
                let statement = label_paragraph
                    .next_siblings()
                    .filter_map(ElementRef::wrap)
                    .next()
                    .context("Endocrine modern recommendation statement missing")?;
                if statement.value().name() != "p"
                    || text_of(&statement).trim().is_empty()
                    || remark_label.is_match(&text_of(&statement))
                {
                    bail!("Endocrine modern label/statement paragraph association changed")
                }
                let panel_html = panel.html();
                let combined = format!(
                    "<p>{} {}</p>",
                    label_paragraph.inner_html(),
                    statement.inner_html()
                );
                let rewritten = panel_html
                    .replacen(&label_paragraph.html(), "", 1)
                    .replacen(&statement.html(), &combined, 1);
                raw = raw.replacen(&panel_html, &rewritten, 1);
            }
        }
        raw = raw.replacen(
            &button.html(),
            &format!("<h4>{}</h4>", button.inner_html()),
            1,
        );
    }
    // A technical-remarks label must survive as context after its recommendation, rather
    // than being stripped from the statement and silently lost or classified as a new statement.
    for label in root.select(&sel("strong,b")) {
        if matches!(
            text_of(&label)
                .trim_end_matches(':')
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "technical remark" | "technical remarks"
        ) {
            raw = raw.replacen(&label.html(), &format!("<p>{}</p>", label.inner_html()), 1);
        }
    }
    let (mut blocks, mut warnings) = blocks_from_html(&raw, Some(base), Some(root.value().name()));
    let label = regex::Regex::new(
        r"^(Recommendation\s+[0-9]+(?:\.[0-9]+)*[a-z]?|R\s+[0-9]+(?:\.[0-9]+)*[a-z]?|[0-9]+(?:\.[0-9]+)+[a-z]?|[A-Z]\.[0-9]+|[0-9]+\.)[.:]?(?:\s+|$)",
    )?;
    // Native grade markers: "(1 |⊕⊕OO)", "(1⊕◯◯◯)" (no bar), "(⊕○○○)" (certainty only) and the Ꚛ glyph
    // older pages use for a filled circle. Ungraded statements print their own label instead.
    let grade = regex::Regex::new(r"\(\s*([12])?\s*\|?\s*([⊕⚪○◯OꚚ●]{2,})\s*\)")?;
    let ungraded = regex::Regex::new(r"(?i)\((ungraded good practice statement|good clinical practice|good practice statement)\)\.?\s*$")?;
    // Signals of numbered or graded statements. Unnumbered "we recommend" sentences under an abstract's
    // "Conclusions:" are journal-abstract summary text, not the page's recommendation list.
    let numbered_signal = regex::Regex::new(r"(?i)(\brecommendation\s+[0-9]|\bR\s+[0-9]+\.[0-9]|\b[0-9]+\.[0-9]+\.?\s+(we|in|for|if)\b|\(\s*[12]\s*\|?\s*[⊕⚪○◯OꚚ●]{2,}\s*\))")?;
    let mut count = 0;
    for block in &mut blocks {
        if !matches!(block.kind, BlockKind::Paragraph | BlockKind::ListItem) {
            continue;
        }
        let Some(source_label) = label.captures(&block.text) else {
            continue;
        };
        let number = source_label[1].to_owned();
        if block.text[source_label.get(0).unwrap().end()..]
            .trim()
            .is_empty()
        {
            warnings.push(format!("Standalone label {number} was retained as prose; no statement body association was established"));
            continue;
        }
        // Keep explicitly headed technical remarks as context rather than inferring that
        // their numbered qualifications are recommendations.
        if block.heading_path.iter().any(|heading| {
            matches!(
                heading
                    .trim_end_matches(':')
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "technical remark" | "technical remarks"
            )
        }) {
            continue;
        }
        let markers = grade.captures_iter(&block.text).collect::<Vec<_>>();
        let (strength, certainty) = if markers.len() == 1 {
            (
                markers[0].get(1).map(|m| m.as_str().to_owned()),
                Some(markers[0][2].to_owned()),
            )
        } else if markers.is_empty() {
            (ungraded.captures(&block.text).map(|c| c[1].to_owned()), None)
        } else {
            (None, None)
        };
        let qualifier = (!markers.is_empty()).then(|| {
            markers
                .iter()
                .map(|capture| capture[0].to_owned())
                .collect::<Vec<_>>()
                .join("; ")
        });
        if markers.len() > 1 {
            warnings.push(format!("Recommendation {number} has multiple native grade markers; their text order is retained without assigning one grade to the whole statement"))
        }
        block.kind = BlockKind::Recommendation {
            label: Some(number),
            strength,
            certainty,
            population: None,
            grading_system: None,
            qualifier,
        };
        count += 1;
    }
    if count == 0 {
        // Never downgrade a page that prints recommendation wording anywhere in its content blocks.
        if candidates().any(|e| numbered_signal.is_match(&text_of(&e))) {
            bail!("Endocrine society resource has no recognized numbered clinical recommendations")
        }
        warnings.push("This society resource page prints no recommendation statements; it summarises the journal guideline, which is recorded as a summary_of relation and attachment. Its prose is kept as printed.".into());
    }
    // Qualifiers are scoped by the actual question/panel boundary, never bold styling.
    // Only the confirmed one-statement panel contract permits a single-statement association.
    for (question, remarks) in question_remarks {
        let matching = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| {
                matches!(block.kind, BlockKind::Recommendation { .. })
                    && block
                        .heading_path
                        .iter()
                        .any(|heading| heading == &question)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matching.len() == 1 {
            if let BlockKind::Recommendation { qualifier, .. } = &mut blocks[matching[0]].kind {
                *qualifier = Some(match qualifier.take() {
                    Some(marker) => format!("{marker}\n\n{remarks}"),
                    None => remarks,
                });
            }
        } else {
            warnings.push("Question-scoped technical remarks retained as source context; no unique recommendation scope could be established".into());
        }
    }
    let mut assets = vec![];
    let mut seen = HashSet::new();
    for anchor in root.select(&sel("a[href]")) {
        let Some(url) = common::link(base, anchor.value().attr("href").unwrap_or("")) else {
            continue;
        };
        let lower = url.to_ascii_lowercase();
        let text = text_of(&anchor);
        let label = text.to_ascii_lowercase();
        if !(lower.ends_with(".pdf")
            || lower.contains("academic.oup.com/jcem/article/")
            || lower.contains("academic.oup.com/jcem/advance-article/")
            || lower.contains("doi.org/10.1210/")
            || lower.contains("guidelines.gradepro.org/"))
            || !seen.insert(url.clone())
        {
            continue;
        }
        let full_guideline = anchor.ancestors().filter_map(ElementRef::wrap).take(3).any(|e| {
            matches!(e.value().name(), "p" | "strong" | "li")
                && text_of(&e).to_ascii_lowercase().starts_with("full guideline")
        });
        let role = if let Some(resource) = presentation.get(&url) {
            resource["role"].as_str().unwrap()
        } else if full_guideline {
            "primary_guideline"
        } else if label.contains("systematic review") {
            "supporting_systematic_review"
        } else if label.contains("decision aid") {
            "decision_aid"
        } else if label.contains("patient") {
            "patient_resource"
        } else if lower.contains("guidelines.gradepro.org/") {
            "supporting_evidence_profile"
        } else if label.contains("update") {
            "linked_update"
        } else if label.contains("guideline") {
            "primary_guideline"
        } else {
            "source_linked_resource"
        };
        let mut asset = json!({"url":url,"label":text,"role":role,"bytes_downloaded":false,"rights":"unknown; independent asset"});
        if let Some(resource) = presentation.get(&url) {
            asset["presentation_source"] = resource.clone();
        }
        assets.push(asset);
    }
    if !presentation.is_empty() {
        warnings.push("The exact source-verified aldosteronism image-only decision-aid/patient-guide table is presentation, not a clinical table. Its two image/link descriptors remain in source order and asset metadata; clinical tables are preserved independently.".into());
    }
    warnings.push("Society recommendation resource; primary journal guideline, supporting systematic review, evidence profile and patient resources are separate source works. Native strength codes and certainty symbols are retained verbatim without translating their semantics. Linked asset bytes and full clinical completeness require separate verification.".into());
    Ok((title, blocks, assets, warnings))
}
fn unresolved(src: &SourceEntry, u: String, t: String, e: String) -> PlanRow {
    PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("{}-unresolved", common::short_hash(&u)),
        url: u,
        title: Some(t),
        rendition: None,
        version: None,
        extra: json!({"discovery_error":e}),
    }
}
#[async_trait::async_trait]
impl Adapter for EndocrineSociety {
    async fn discover(
        &self,
        src: &SourceEntry,
        fetcher: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let homepage = fetcher
            .get_ok(
                src.catalog_url
                    .as_deref()
                    .context("Endocrine catalog missing")?,
            )
            .await?;
        let base = Url::parse(&homepage.record.final_url)?;
        if base.scheme() != "https" || base.host_str() != Some("www.endocrine.org") {
            bail!("Endocrine catalog origin invalid")
        }
        let mut listings = areas(&homepage.text(), &base);
        for (url, title) in links(&homepage.text(), &base) {
            if Url::parse(&url)?.path().trim_end_matches('/')
                == "/clinical-practice-guidelines/guidelines-by-year"
                && !listings.iter().any(|(known, _)| known == &url)
            {
                listings.insert(0, (url, title));
            }
        }
        if listings.is_empty() {
            bail!("Endocrine clinical-area/year source links missing")
        }
        let catalog_urls = listings
            .iter()
            .map(|(url, _)| url.trim_end_matches('/').to_owned())
            .collect::<HashSet<_>>();
        let mut rows: Vec<PlanRow> = vec![];
        let mut emitted: HashMap<String, usize> = HashMap::new();
        let mut visited = HashSet::new();
        for (area, label) in listings {
            if !visited.insert(area.clone()) {
                continue;
            }
            if visited.len() > 100 {
                rows.push(unresolved(
                    src,
                    area,
                    label,
                    "Endocrine catalog page bound exceeded; inventory incomplete".into(),
                ));
                break;
            }
            let listing = match fetcher.get_ok(&area).await {
                Ok(page) => page,
                Err(error) => {
                    rows.push(unresolved(
                        src,
                        area,
                        label,
                        format!("Catalog fetch failed: {error:#}"),
                    ));
                    if limit.is_some_and(|limit| rows.len() >= limit) {
                        return Ok(rows);
                    }
                    continue;
                }
            };
            let listing_base = Url::parse(&listing.record.final_url)?;
            if listing_base.origin() != base.origin() {
                rows.push(unresolved(
                    src,
                    area,
                    label,
                    "Catalog redirected outside source origin".into(),
                ));
                if limit.is_some_and(|limit| rows.len() >= limit) {
                    return Ok(rows);
                }
                continue;
            }
            let entries = match listing_contract(&listing.text(), &listing_base) {
                Ok(contract) => match catalog_entries(fetcher, &contract, &listing_base).await {
                    Ok(entries) => entries,
                    Err(error) => {
                        rows.push(unresolved(
                            src,
                            area,
                            label,
                            format!("Catalog API: {error:#}"),
                        ));
                        if limit.is_some_and(|limit| rows.len() >= limit) {
                            return Ok(rows);
                        }
                        continue;
                    }
                },
                Err(error) => {
                    rows.push(unresolved(
                        src,
                        area,
                        label,
                        format!("Catalog configuration: {error:#}"),
                    ));
                    if limit.is_some_and(|limit| rows.len() >= limit) {
                        return Ok(rows);
                    }
                    continue;
                }
            };
            for item in entries.items {
                let Some(url) = item["contentUrl"]
                    .as_str()
                    .and_then(|href| common::link(&listing_base, href))
                else {
                    continue;
                };
                let parsed = Url::parse(&url)?;
                // The year listing includes Blog items with a CPG topic tag. Only actual
                // society clinical-resource destinations enter this adapter's clinical path.
                if ["contentType", "primaryContentType"].iter().any(|key| {
                    item[*key]
                        .as_str()
                        .is_some_and(|value| value.trim().eq_ignore_ascii_case("Blog"))
                }) || parsed.origin() != base.origin()
                    || !parsed.path().starts_with("/clinical-practice-guidelines/")
                    || catalog_urls.contains(url.trim_end_matches('/'))
                    || matches!(
                        parsed.path().trim_end_matches('/'),
                        "/clinical-practice-guidelines/guidelines-in-development"
                            | "/clinical-practice-guidelines/endorsed-guidelines"
                    )
                {
                    continue;
                }
                let observation = json!({"source_catalog_url":area,"source_id":item.get("id"),"source_title":item.get("title"),"resource_date":item.get("date"),"source_content_type":item.get("contentType"),"source_primary_content_type":item.get("primaryContentType"),"source_secondary_content_type":item.get("secondaryContentType"),"content_url":url,"listing_coverage":entries.coverage});
                if let Some(index) = emitted.get(&url) {
                    if let Some(observations) =
                        rows[*index].extra["catalog_observations"].as_array_mut()
                    {
                        observations.push(observation)
                    }
                    continue;
                }
                let title = item["title"].as_str().unwrap_or("").to_owned();
                let page = match fetcher.get_ok(&url).await {
                    Ok(page) => page,
                    Err(error) => {
                        rows.push(unresolved(
                            src,
                            url,
                            title,
                            format!("Clinical resource fetch failed: {error:#}"),
                        ));
                        if limit.is_some_and(|limit| rows.len() >= limit) {
                            return Ok(rows);
                        }
                        continue;
                    }
                };
                let final_url = Url::parse(&page.record.final_url)?;
                if final_url.origin() != base.origin()
                    || !final_url
                        .path()
                        .starts_with("/clinical-practice-guidelines/")
                {
                    rows.push(unresolved(
                        src,
                        url,
                        title,
                        "Clinical resource redirected outside permitted society branch".into(),
                    ));
                    if limit.is_some_and(|limit| rows.len() >= limit) {
                        return Ok(rows);
                    }
                    continue;
                }
                let (clinical_title, blocks, attachments, _) =
                    match content(&page.text(), &final_url) {
                        Ok(content) => content,
                        Err(error) => {
                            rows.push(unresolved(
                                src,
                                url,
                                title,
                                format!("Clinical template: {error:#}"),
                            ));
                            if limit.is_some_and(|limit| rows.len() >= limit) {
                                return Ok(rows);
                            }
                            continue;
                        }
                    };
                let snapshot = clinical_snapshot(&clinical_title, &blocks, &attachments)?;
                let work = common::short_hash(&url);
                let mut fetches = vec![homepage.record.clone(), listing.record.clone()];
                fetches.extend(entries.fetches.clone());
                emitted.insert(url.clone(), rows.len());
                rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-{}",&snapshot[..12]),url,title:Some(clinical_title),rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"clinical_snapshot":snapshot,"catalog_observations":[observation],"catalog_fetches":fetches,"global_inventory_complete":false})});
                if limit.is_some_and(|limit| rows.len() >= limit) {
                    return Ok(rows);
                }
            }
        }
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(e) = row.extra["discovery_error"].as_str() {
            bail!("Endocrine unresolved: {e}")
        }
        let p = f.get_ok(&row.url).await?;
        let base = Url::parse(&p.record.final_url)?;
        if base.scheme() != "https"
            || base.host_str() != Some("www.endocrine.org")
            || !base.path().starts_with("/clinical-practice-guidelines/")
        {
            bail!("Endocrine clinical resource origin/route changed")
        }
        let (title, blocks, attachments, mut warnings) = content(&p.text(), &base)?;
        let item_type = if blocks.iter().any(|b| matches!(b.kind, BlockKind::Recommendation { .. })) {
            "guideline_recommendation_resource"
        } else {
            "guideline_summary_resource"
        };
        let relations = attachments
            .iter()
            .filter(|a| a["role"] == "primary_guideline")
            .filter_map(|a| a["url"].as_str())
            .map(|target| crate::guidelines::doc::Relation {
                rel: "summary_of".into(),
                target: target.into(),
                scope: Some("society resource page for the guideline, linked as \"Full Guideline\"".into()),
                evidence_url: Some(base.to_string()),
            })
            .collect::<Vec<_>>();
        if row.extra["clinical_snapshot"].as_str()
            != Some(clinical_snapshot(&title, &blocks, &attachments)?.as_str())
        {
            bail!("Endocrine clinical resource changed after planning; replan")
        }
        let observations = row.extra["catalog_observations"]
            .as_array()
            .context("Endocrine plan catalog provenance missing")?;
        let mut dates = vec![];
        let mut date_seen = HashSet::new();
        for observation in observations {
            if let Some(raw) = observation["resource_date"]
                .as_str()
                .filter(|raw| !raw.trim().is_empty())
            {
                let source = observation["source_catalog_url"]
                    .as_str()
                    .unwrap_or(&row.url);
                if date_seen.insert((raw.to_owned(), source.to_owned())) {
                    dates.push(common::date("Catalog resource date", raw, source));
                }
            }
        }
        let mut fetches =
            serde_json::from_value::<Vec<FetchRecord>>(row.extra["catalog_fetches"].clone())
                .context("Endocrine plan catalog fetch provenance invalid")?;
        fetches.push(p.record);
        warnings.push("Catalog resource dates are page/listing metadata, not inferred guideline publication dates. Each observed listing can terminate against its own total; global clinical-area/year/retired/endorsed reconciliation is not claimed complete.".into());
        Ok(GuidelineDoc {
            source_id: src.id.clone(),
            doc_key: row.doc_key.clone(),
            work_key: row.extra["work_key"].as_str().map(String::from),
            title,
            url: base.to_string(),
            item_type: Some(item_type.into()),
            relations,
            status: "unknown".into(),
            rights: common::unknown_rights(),
            blocks,
            attachments,
            date_observations: dates,
            search_metadata: json!({"catalog_observations":observations,"global_inventory_complete":false,"clinical_snapshot":row.extra["clinical_snapshot"]}),
            fetches,
            warnings,
            ..Default::default()
        })
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    #[test]
    fn legacy_recommendations_preserve_native_grade_and_ungraded_statement() {
        let b = Url::parse("https://www.endocrine.org/clinical-practice-guidelines").unwrap();
        let c = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/endocrine_society/catalog.html"
        ));
        assert_eq!(areas(c, &b).len(), 1);
        let p = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/endocrine_society/page.html"
        ));
        let (_, blocks, assets, _) = content(p, &b).unwrap();
        assert!(blocks.iter().any(|block|matches!(&block.kind,BlockKind::Recommendation{label:Some(label),strength:Some(strength),certainty:Some(certainty),..} if label=="A.1" && strength=="1" && certainty=="⊕⊕⊕⚪")));
        assert!(blocks.iter().any(|block|matches!(&block.kind,BlockKind::Recommendation{label:Some(label),strength:None,certainty:None,..} if label=="A.2")));
        assert!(blocks
            .iter()
            .any(|block| block.text == "Technical Remarks:"));
        assert!(blocks
            .iter()
            .any(|block| block.text == "Contextual qualification."
                && !matches!(block.kind, BlockKind::Recommendation { .. })));
        assert_eq!(assets.len(), 1);
        assert!(blocks
            .iter()
            .any(|x| matches!(x.kind, BlockKind::Table { .. })));
        assert!(blocks
            .iter()
            .any(|x| x.heading_path.iter().any(|h| h.contains("Update A"))));
        assert!(content("<h1>Login</h1>", &b).is_err());
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod offline_flow {
    use super::*;
    const AREA: &str =
        include_str!("../../../tests/fixtures/guidelines/endocrine_society/dynamic-area.html");
    const NEW: &str = include_str!(
        "../../../tests/fixtures/guidelines/endocrine_society/new-question-panels.html"
    );
    const OLD: &str =
        include_str!("../../../tests/fixtures/guidelines/endocrine_society/page.html");
    const FIRST: &[u8] =
        include_bytes!("../../../tests/fixtures/guidelines/endocrine_society/api-page-1.json");
    const SECOND: &[u8] =
        include_bytes!("../../../tests/fixtures/guidelines/endocrine_society/api-page-2.json");
    #[test]
    fn new_question_panels_keep_native_codes_population_context_and_independent_assets() {
        let url = Url::parse("https://www.endocrine.org/clinical-practice-guidelines/example-new")
            .unwrap();
        let (_, blocks, assets, _) = content(NEW, &url).unwrap();
        let recommendations = blocks
            .iter()
            .filter(|block| matches!(block.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recommendations.len(), 2);
        assert!(
            matches!(&recommendations[0].kind,BlockKind::Recommendation{label:Some(label),strength:Some(strength),certainty:Some(certainty),population:None,..} if label=="1.1" && strength=="2" && certainty=="⊕OOO")
        );
        assert!(
            matches!(&recommendations[1].kind,BlockKind::Recommendation{certainty:Some(certainty),..} if certainty=="⊕⊕OO")
        );
        assert!(recommendations[0]
            .heading_path
            .iter()
            .any(|heading| heading == "Question 1: Synthetic pediatric question"));
        assert!(recommendations[1]
            .heading_path
            .iter()
            .any(|heading| heading == "Question 2: Synthetic second question"));
        let remark = blocks
            .iter()
            .position(|block| block.text == "Synthetic age and sex eligibility for question 1.")
            .unwrap();
        let first = blocks
            .iter()
            .position(|block| block.text.starts_with("1.1 "))
            .unwrap();
        let second = blocks
            .iter()
            .position(|block| block.text.starts_with("2.1 "))
            .unwrap();
        assert!(first < remark && remark < second);
        assert!(!matches!(
            blocks[remark].kind,
            BlockKind::Recommendation { .. }
        ));
        assert!(blocks[remark]
            .heading_path
            .iter()
            .any(|heading| heading.contains("Question 1")));
        assert!(assets
            .iter()
            .any(|asset| asset["role"] == "supporting_systematic_review"));
        assert!(assets
            .iter()
            .any(|asset| asset["role"] == "patient_resource"));
        assert!(assets.iter().any(|asset| asset["role"] == "decision_aid"));
        assert!(assets
            .iter()
            .any(|asset| asset["role"] == "supporting_evidence_profile"));
        assert!(assets
            .iter()
            .all(|asset| asset["bytes_downloaded"] == false));
        let mixed = NEW.replace(
            "Synthetic first statement for its stated population. (2 | ⊕OOO)",
            "Synthetic first assertion. (1 | ⊕⊕OO) Synthetic second assertion. (2 | ⊕OOO)",
        );
        let (_, blocks, _, warnings) = content(&mixed, &url).unwrap();
        assert!(blocks.iter().any(|block|matches!(&block.kind,BlockKind::Recommendation{label:Some(label),strength:None,certainty:None,qualifier:Some(markers),..} if label=="1.1" && markers.contains("(1 | ⊕⊕OO)") && markers.contains("(2 | ⊕OOO)"))));
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("multiple native grade markers")));
    }
    #[test]
    fn modern_pregnancy_label_and_statement_paragraphs_keep_ten_scoped_recommendations() {
        let fixture = include_str!(
            "../../../tests/fixtures/guidelines/endocrine_society/pregnancy-question-panels.html"
        );
        let plain = fixture.replace("<strong>Technical remark:</strong>", "Technical remark:");
        let base=Url::parse("https://www.endocrine.org/clinical-practice-guidelines/preexisting-diabetes-in-pregnancy").unwrap();
        for html in [fixture, plain.as_str()] {
            let (_, blocks, _, _) = content(html, &base).unwrap();
            let recommendations = blocks
                .iter()
                .filter(|block| matches!(block.kind, BlockKind::Recommendation { .. }))
                .collect::<Vec<_>>();
            assert_eq!(recommendations.len(), 10);
            for (index, block) in recommendations.iter().enumerate() {
                assert!(
                    matches!(&block.kind,BlockKind::Recommendation{label:Some(label),strength:Some(strength),certainty:Some(certainty),..} if label==&format!("Recommendation {}",index+1) && strength=="2" && certainty==if index==1{"⊕⊕OO"}else{"⊕OOO"})
                );
                assert!(block
                    .text
                    .contains(&format!("Synthetic pregnancy statement {}.", index + 1)));
                assert!(!block.text.contains("Synthetic scoped qualification"));
            }
            let scoped=recommendations.iter().filter(|block|matches!(&block.kind,BlockKind::Recommendation{qualifier:Some(qualifier),..} if qualifier.to_ascii_lowercase().contains("technical remark"))).count();
            assert_eq!(scoped, 9);
            assert!(
                matches!(&recommendations[3].kind,BlockKind::Recommendation{qualifier:Some(qualifier),..} if !qualifier.to_ascii_lowercase().contains("technical remark"))
            );
            assert!(
                matches!(&recommendations[9].kind,BlockKind::Recommendation{qualifier:Some(qualifier),..} if qualifier.contains("Technical remark:") && qualifier.contains("Synthetic scoped qualification 10.") && qualifier.contains("Synthetic second constraint 10."))
            );
            assert!(blocks
                .iter()
                .any(|block| block.text == "Synthetic scoped qualification 10."
                    && block
                        .heading_path
                        .iter()
                        .any(|heading| heading == "Question 10: Synthetic pregnancy question")));
        }
    }
    const PRESENTATION: &str = include_str!("../../../tests/fixtures/guidelines/endocrine_society/presentation-and-remarks-synthetic.html");
    #[test]
    fn exact_image_download_wrapper_is_not_a_clinical_table_but_one_row_clinical_data_survives() {
        let base = Url::parse(
            "https://www.endocrine.org/clinical-practice-guidelines/primary-aldosteronism-2",
        )
        .unwrap();
        let (_, blocks, assets, warnings) = content(PRESENTATION, &base).unwrap();
        let tables = blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
            .collect::<Vec<_>>();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].block_id, "synthetic-one-row-clinical");
        assert!(
            tables[0].text.contains("Synthetic dose")
                && tables[0].text.contains("Synthetic clinical assessment")
        );
        let figures = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| matches!(b.kind, BlockKind::Figure { .. }))
            .collect::<Vec<_>>();
        assert_eq!(figures.len(), 2);
        let before = blocks
            .iter()
            .position(|b| b.text == "Synthetic paragraph before presentation wrapper.")
            .unwrap();
        let after = blocks
            .iter()
            .position(|b| b.text == "Synthetic paragraph after presentation wrapper.")
            .unwrap();
        assert!(before < figures[0].0 && figures[0].0 < figures[1].0 && figures[1].0 < after);
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0]["role"], "decision_aid");
        assert_eq!(assets[1]["role"], "patient_resource");
        assert_eq!(
            assets[0]["presentation_source"]["presentation_wrapper"]["clinical_table"],
            false
        );
        assert!(
            assets[0]["presentation_source"]["presentation_wrapper"]["image_url"]
                .as_str()
                .unwrap()
                .contains("hash=synthetic-public-image-hash")
        );
        assert!(warnings.iter().any(|w| w.contains("not a clinical table")));
        // The same geometry with meaningful image text, caption, unknown asset or another
        // resource route is not the exact observed presentation contract.
        for changed in [PRESENTATION.replacen("alt=\"\"","alt=\"Synthetic clinical diagram\"",1),PRESENTATION.replacen("<table>","<table><caption>Synthetic clinical caption</caption>",1),PRESENTATION.replace("cpg_horizontal_download_button_primary_aldosteronism_medical_choice_decision_aid.jpg","unknown-clinical-diagram.jpg")] {
            let (_,blocks,_,_)=content(&changed,&base).unwrap();assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),2);
        }
        let other = Url::parse(
            "https://www.endocrine.org/clinical-practice-guidelines/central-precocious-puberty",
        )
        .unwrap();
        assert_eq!(
            content(PRESENTATION, &other)
                .unwrap()
                .1
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
                .count(),
            2
        );
        let changed =
            PRESENTATION.replace("synthetic-public-image-hash", "changed-public-image-hash");
        let (title, new_blocks, new_assets, _) = content(&changed, &base).unwrap();
        let original = content(PRESENTATION, &base).unwrap();
        assert_ne!(
            clinical_snapshot(&title, &new_blocks, &new_assets).unwrap(),
            clinical_snapshot(&original.0, &original.1, &original.2).unwrap()
        );
    }
    #[test]
    fn native_singular_plural_optional_colon_remarks_keep_scoped_p_ul_p_ul_order() {
        let base = Url::parse(
            "https://www.endocrine.org/clinical-practice-guidelines/primary-aldosteronism-2",
        )
        .unwrap();
        for label in [
            "Technical remarks:",
            "Technical remark:",
            "Technical remarks",
            "Technical remark",
        ] {
            for strong in [true, false] {
                let mut html = PRESENTATION.replace("Technical remarks:", label);
                if !strong {
                    html = html.replace(&format!("<strong>{label}</strong>"), label);
                }
                let (_, blocks, _, _) = content(&html, &base).unwrap();
                let recs = blocks
                    .iter()
                    .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                    .collect::<Vec<_>>();
                assert_eq!(recs.len(), 2);
                assert!(
                    matches!(&recs[0].kind,BlockKind::Recommendation{label:Some(n),strength:Some(s),certainty:Some(c),qualifier:Some(q),..}if n=="Recommendation 2"&&s=="2"&&c=="⊕OOO"&&q.contains(label)&&q.contains("Synthetic standalone CPP Q2 paragraph.")&&q.contains("Synthetic CPP Q2 first list clause."))
                );
                assert!(
                    matches!(&recs[1].kind,BlockKind::Recommendation{label:Some(n),qualifier:Some(q),..}if n=="Recommendation 4"&&q.contains(label)&&q.find("Synthetic aldo Q4 first paragraph.")<q.find("Synthetic aldo Q4 first list.")&&q.find("Synthetic aldo Q4 first list.")<q.find("Synthetic aldo Q4 second paragraph.")&&q.find("Synthetic aldo Q4 second paragraph.")<q.find("Synthetic aldo Q4 second list."))
                );
                let q2 = blocks
                    .iter()
                    .filter(|b| {
                        b.heading_path
                            .iter()
                            .any(|h| h == "Question 2: Synthetic CPP context boundary")
                            && b.text.starts_with("Synthetic ")
                            && !matches!(b.kind, BlockKind::Recommendation { .. })
                    })
                    .map(|b| b.text.as_str())
                    .collect::<Vec<_>>();
                assert_eq!(
                    q2,
                    vec![
                        "Synthetic standalone CPP Q2 paragraph.",
                        "Synthetic CPP Q2 first list clause.",
                        "Synthetic CPP Q2 second list clause."
                    ]
                );
                let q4 = blocks
                    .iter()
                    .filter(|b| {
                        b.heading_path
                            .iter()
                            .any(|h| h == "Question 4: Synthetic aldosteronism context boundary")
                            && b.text.starts_with("Synthetic aldo Q4")
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    q4.iter().map(|b| b.text.as_str()).collect::<Vec<_>>(),
                    vec![
                        "Synthetic aldo Q4 first paragraph.",
                        "Synthetic aldo Q4 first list.",
                        "Synthetic aldo Q4 second paragraph.",
                        "Synthetic aldo Q4 second list."
                    ]
                );
                assert!(
                    matches!(q4[0].kind, BlockKind::Paragraph)
                        && matches!(q4[1].kind, BlockKind::ListItem)
                        && matches!(q4[2].kind, BlockKind::Paragraph)
                        && matches!(q4[3].kind, BlockKind::ListItem)
                );
            }
        }
    }

    #[tokio::test]
    async fn dynamic_catalog_respects_server_page_size_excludes_blogs_and_pins_clinical_assets() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        let root = "https://www.endocrine.org/clinical-practice-guidelines";
        let area = "https://www.endocrine.org/clinical-practice-guidelines/bone-health";
        let base = Url::parse(area).unwrap();
        let contract = listing_contract(AREA, &base).unwrap();
        fetcher.seed_cache_for_tests(
            root,
            include_bytes!("../../../tests/fixtures/guidelines/endocrine_society/catalog.html"),
            "text/html",
        );
        fetcher.seed_cache_for_tests(area, AREA.as_bytes(), "text/html");
        fetcher.seed_cache_request_for_tests(
            &catalog_request(&contract, 1, 4),
            FIRST,
            "application/json",
        );
        // Only perPage=2 is seeded for page2. Reusing the sent4 rather than source-returned2 fails.
        fetcher.seed_cache_request_for_tests(
            &catalog_request(&contract, 2, 2),
            SECOND,
            "application/json",
        );
        let new = "https://www.endocrine.org/clinical-practice-guidelines/example-new";
        let old = "https://www.endocrine.org/clinical-practice-guidelines/example-legacy";
        fetcher.seed_cache_for_tests(new, NEW.as_bytes(), "text/html");
        fetcher.seed_cache_for_tests(old, OLD.as_bytes(), "text/html");
        // The blog route is deliberately absent. Fetching it would create an extra unresolved row.
        let source: SourceEntry = serde_json::from_value(
            json!({"id":"endocrine_society","enabled":true,"access":"public","catalog_url":root}),
        )
        .unwrap();
        let rows = EndocrineSociety
            .discover(&source, &fetcher, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].url, new);
        assert_eq!(rows[1].url, old);
        assert_eq!(
            rows[0].extra["catalog_observations"][0]["listing_coverage"]["raw_records"],
            3
        );
        assert_eq!(
            rows[0].extra["catalog_observations"][0]["listing_coverage"]["api_pages"],
            2
        );
        let document = EndocrineSociety
            .extract(&source, &fetcher, &rows[0])
            .await
            .unwrap();
        assert_eq!(
            document.item_type.as_deref(),
            Some("guideline_recommendation_resource")
        );
        assert_eq!(document.status, "unknown");
        assert!(document.published.is_none());
        assert!(document.updated.is_none());
        assert_eq!(document.date_observations[0].label, "Catalog resource date");
        assert_eq!(document.date_observations[0].raw, "June 13, 2026");
        assert_eq!(document.fetches.len(), 5);
        assert_eq!(document.search_metadata["global_inventory_complete"], false);
        // A CPG topic tag cannot convert a native Blog category into a guideline,
        // even if a future catalog item happens to use a clinical-looking destination.
        let mut typed_blog: Value = serde_json::from_slice(FIRST).unwrap();
        typed_blog["results"][1]["contentUrl"] =
            json!("/clinical-practice-guidelines/synthetic-blog");
        fetcher.seed_cache_request_for_tests(
            &catalog_request(&contract, 1, 4),
            &serde_json::to_vec(&typed_blog).unwrap(),
            "application/json",
        );
        assert_eq!(
            EndocrineSociety
                .discover(&source, &fetcher, None)
                .await
                .unwrap()
                .len(),
            2
        );
        fetcher.seed_cache_for_tests(
            new,
            NEW.replace("/patient-resource.pdf", "/changed-patient-resource.pdf")
                .as_bytes(),
            "text/html",
        );
        assert!(EndocrineSociety
            .extract(&source, &fetcher, &rows[0])
            .await
            .unwrap_err()
            .to_string()
            .contains("changed after planning"));
    }
    #[tokio::test]
    async fn server_error_and_repeated_page_ids_stop_without_request_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        let base = Url::parse("https://www.endocrine.org/clinical-practice-guidelines/bone-health")
            .unwrap();
        let contract = listing_contract(AREA, &base).unwrap();
        fetcher.seed_cache_request_for_tests(
            &catalog_request(&contract, 1, 4),
            br#"{"Code":"SERVER_ERROR"}"#,
            "application/json",
        );
        assert!(catalog_entries(&fetcher, &contract, &base)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("SERVER_ERROR"));
        fetcher.seed_cache_request_for_tests(
            &catalog_request(&contract, 1, 4),
            FIRST,
            "application/json",
        );
        let mut repeated: Value = serde_json::from_slice(SECOND).unwrap();
        repeated["results"][0]["id"] = json!("synthetic-new-resource");
        fetcher.seed_cache_request_for_tests(
            &catalog_request(&contract, 2, 2),
            &serde_json::to_vec(&repeated).unwrap(),
            "application/json",
        );
        assert!(catalog_entries(&fetcher, &contract, &base)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("repeated source IDs"));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod scale_templates_test {
    //! Resource-page variants that failed at full-catalog scale (2026-11-17).
    use super::*;
    macro_rules! fx { ($n:literal) => { include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/endocrine_society/", $n)) } }
    fn base(slug: &str) -> Url { Url::parse(&format!("https://www.endocrine.org/clinical-practice-guidelines/{slug}")).unwrap() }
    fn recs(blocks: &[Block]) -> Vec<(String, Option<String>, Option<String>, String)> {
        blocks.iter().filter_map(|b| match &b.kind {
            BlockKind::Recommendation { label, strength, certainty, .. } => Some((label.clone().unwrap_or_default(), strength.clone(), certainty.clone(), b.text.clone())),
            _ => None,
        }).collect()
    }
    fn find<'a>(r: &'a [(String, Option<String>, Option<String>, String)], label: &str) -> &'a (String, Option<String>, Option<String>, String) {
        r.iter().find(|x| x.0 == label).unwrap_or_else(|| panic!("label {label} missing in {:?}", r.iter().map(|x| &x.0).collect::<Vec<_>>()))
    }
    #[test]
    fn summary_of_recommendations_with_heavy_circle_glyph_grades() {
        let (_, blocks, _, _) = content(fx!("scale-testosterone-therapy.html"), &base("testosterone-therapy")).unwrap();
        let r = recs(&blocks);
        let x = find(&r, "1.1");
        assert_eq!(x.1.as_deref(), Some("1"));
        assert_eq!(x.2.as_deref(), Some("ꚚꚚꚚO"));
        assert!(x.3.contains("diagnosing hypogonadism"));
        assert_eq!(find(&r, "2.2").1.as_deref(), Some("1"));
    }
    #[test]
    fn br_joined_statements_are_split_and_ungraded_labels_kept() {
        let (_, blocks, _, _) = content(fx!("scale-gender-dysphoria.html"), &base("gender-dysphoria-gender-incongruence")).unwrap();
        let r = recs(&blocks);
        assert!(r.len() >= 20, "{}", r.len());
        let x = find(&r, "1.3");
        assert_eq!(x.1.as_deref(), Some("Ungraded Good Practice Statement"));
        assert!(x.2.is_none());
        assert!(!x.3.contains("1.4."));
        let y = find(&r, "1.4");
        assert_eq!((y.1.as_deref(), y.2.as_deref()), (Some("1"), Some("⊕⊕OO")));
    }
    #[test]
    fn colon_labels_r_labels_and_barless_grades() {
        let (_, blocks, _, _) = content(fx!("scale-hypercalcemia.html"), &base("hypercalcemia")).unwrap();
        let r = recs(&blocks);
        let x = find(&r, "Recommendation 1");
        assert_eq!((x.1.as_deref(), x.2.as_deref()), (Some("1"), Some("⊕◯◯◯")));
        let (_, blocks, _, _) = content(fx!("scale-glucocorticoid-induced-adrenal-insufficiency.html"), &base("glucocorticoid-induced-adrenal-insufficiency")).unwrap();
        let r = recs(&blocks);
        assert_eq!(find(&r, "R 1.2").1.as_deref(), Some("Good clinical practice"));
        let x = find(&r, "R 2.1");
        assert_eq!((x.1.as_deref(), x.2.as_deref()), (None, Some("⊕○○○")));
        assert!(find(&r, "R 1.1").1.is_none());
    }
    #[test]
    fn singular_list_heading_split_root_and_heading_labels() {
        let (_, blocks, _, _) = content(fx!("scale-vitamin-d.html"), &base("vitamin-d-for-prevention-of-disease")).unwrap();
        let r = recs(&blocks);
        let x = find(&r, "Recommendation 1");
        assert_eq!((x.1.as_deref(), x.2.as_deref()), (Some("2"), Some("⊕⊕OO")));
        let (_, blocks, _, warnings) = content(fx!("scale-inpatient-hyperglycemia.html"), &base("inpatient-hyperglycemia-guideline-resources")).unwrap();
        let r = recs(&blocks);
        assert!(find(&r, "Recommendation 1.1").3.contains("real-time continuous"));
        assert!(warnings.iter().any(|w| w.contains("sibling markup")));
        assert!(blocks.iter().any(|b| b.text.contains("Continuous glucose monitoring systems can effectively")));
        let (_, blocks, _, _) = content(fx!("scale-diagnosis-of-cushing-syndrome.html"), &base("diagnosis-of-cushing-syndrome")).unwrap();
        assert!(recs(&blocks).iter().any(|x| x.0 == "3.1"));
    }
    #[test]
    fn abstract_conclusions_are_summary_prose_not_recommendations() {
        let (_, blocks, assets, warnings) = content(fx!("scale-pagets-disease-of-bone.html"), &base("pagets-disease-of-bone")).unwrap();
        assert!(recs(&blocks).is_empty());
        assert!(blocks.iter().any(|b| matches!(b.kind, BlockKind::ListItem) && b.text.starts_with("We recommend that plain radiographs")));
        assert!(warnings.iter().any(|w| w.contains("prints no recommendation statements")));
        assert!(assets.iter().any(|a| a["url"] == "https://doi.org/10.1210/jc.2014-2910" && a["role"] == "primary_guideline"));
    }
    #[test]
    fn summary_page_without_recommendations_links_full_guideline() {
        let (title, blocks, assets, warnings) = content(fx!("scale-acromegaly.html"), &base("acromegaly")).unwrap();
        assert_eq!(title, "Acromegaly");
        assert!(recs(&blocks).is_empty());
        assert!(blocks.iter().any(|b| b.text.contains("formulate clinical practice guidelines for acromegaly")));
        assert!(warnings.iter().any(|w| w.contains("prints no recommendation statements")));
        assert!(assets.iter().any(|a| a["url"] == "https://doi.org/10.1210/jc.2014-2700" && a["role"] == "primary_guideline"), "{assets:?}");
        // A page that shows numbered or graded statements the parser does not recognise still fails loudly.
        let html = fx!("scale-acromegaly.html").replace("The aim was to formulate", "The aim, per Recommendation 1, was to formulate");
        assert!(content(&html, &base("acromegaly")).is_err());
        let html = fx!("scale-acromegaly.html").replace("The aim was to formulate", "We recommend (1 |⊕⊕OO) to formulate");
        assert!(content(&html, &base("acromegaly")).is_err());
    }
}
