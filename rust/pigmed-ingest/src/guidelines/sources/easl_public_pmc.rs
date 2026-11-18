//! One source-verified EASL-associated patient guideline expression on PMC.
//! This is neither a PMC crawler nor access to EASL's professional CPGs.
use super::{common, Adapter, PlanRow};
use crate::guidelines::{
    doc::{BlockKind, DateObservation, GuidelineDoc, Relation, Rights},
    fetch::{Fetched, Fetcher},
    html::{blocks_from_html, sel, text_of},
    registry::SourceEntry,
};
use anyhow::{bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use url::Url;

pub struct EaslPublicPmc;
const URL: &str = "https://pmc.ncbi.nlm.nih.gov/articles/PMC8514420/";
const DOI: &str = "10.1016/j.jhepr.2021.100322";
const PMCID: &str = "PMC8514420";
const EASL_RECORD: &str =
    "https://easl.eu/publication/non-alcoholic-fatty-liver-disease-a-patient-guideline/";
const CORRECTION: &str = "https://pmc.ncbi.nlm.nih.gov/articles/PMC11228441/";
const PDF: &str = "https://pmc.ncbi.nlm.nih.gov/articles/PMC8514420/pdf/main.pdf";
const SUPPLEMENT: &str = "https://pmc.ncbi.nlm.nih.gov/articles/instance/8514420/bin/mmc1.pdf";
const ROOT: &str = "main article section.body.main-article-body";

fn config(src: &SourceEntry) -> Result<()> {
    if let Some(value) = src.config.get("expression_url") {
        if value.as_str() != Some(URL) {
            bail!("EASL public PMC adapter accepts only its verified patient expression");
        }
    }
    Ok(())
}
fn meta(document: &Html, name: &str) -> Vec<String> {
    document
        .select(&sel(&format!("meta[name=\"{name}\"]")))
        .filter_map(|e| e.value().attr("content"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}
fn escaped(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn image_url(image: ElementRef<'_>, base: &Url) -> Option<String> {
    ["src", "data-src", "data-original", "data-lazy-src"]
        .iter()
        .filter_map(|key| image.value().attr(*key))
        .chain(
            ["srcset", "data-srcset"]
                .iter()
                .filter_map(|key| image.value().attr(*key))
                .filter_map(|value| value.split(',').next())
                .filter_map(|value| value.split_whitespace().next()),
        )
        .find_map(|value| common::link(base, value))
}
// The observed licence is an owned plain-text paragraph, not a CC anchor.
// A matching URI in clinical prose, references, or a site footer is not a grant.
const LICENSE_WRAPPER: &str = "section.front-matter div.ameta.p.font-secondary.font-xs > div.d-panels.font-secondary-light > div#clp_a.d-panel.p";
fn license(article: ElementRef<'_>, base: &Url) -> Result<(Rights, Vec<Value>)> {
    let wrappers = article.select(&sel(LICENSE_WRAPPER)).collect::<Vec<_>>();
    if wrappers.len() != 1 || article.select(&sel("[id=clp_a]")).count() != 1 {
        bail!("PMC owned front-matter licence wrapper missing or ambiguous");
    }
    let wrapper = wrappers[0];
    if wrapper
        .ancestors()
        .filter_map(ElementRef::wrap)
        .any(|node| {
            node.id() != wrapper.id() && node.value().classes().any(|c| c == "main-article-body")
        })
    {
        bail!("PMC licence wrapper is inside clinical content, not owned front matter");
    }
    let children = wrapper
        .children()
        .filter_map(ElementRef::wrap)
        .collect::<Vec<_>>();
    if children.len() != 3
        || children[0].value().name() != "div"
        || children[1].value().name() != "p"
        || children[2].value().name() != "div"
        || !children[2].value().classes().any(|c| c == "p")
    {
        bail!("PMC owned licence wrapper's observed child roles changed");
    }
    let copyright = text_of(&children[0]);
    if copyright.is_empty() || copyright.len() > 6000 || wrapper.html().len() > 20000 {
        bail!("PMC owned copyright/licence notice empty or oversized");
    }
    let paragraph = children[1];
    if paragraph.children().count() != 1
        || !paragraph
            .children()
            .all(|node| matches!(node.value(), scraper::Node::Text(_)))
    {
        bail!("PMC owned licence notice is not the observed single plain-text node");
    }
    let raw_notice = paragraph
        .text()
        .next()
        .context("PMC owned licence notice text missing")?;
    if raw_notice.len() > 6000 || !raw_notice.contains("CC BY-NC-ND") {
        bail!("PMC owned CC BY-NC-ND notice missing or oversized");
    }
    let uri_pattern = regex::Regex::new(
        r"(?:^|[\s(])(?P<uri>https?://creativecommons\.org/licenses/by-nc-nd/4\.0/)(?:$|[\s).,;])",
    )?;
    let uris = uri_pattern
        .captures_iter(raw_notice)
        .filter_map(|capture| capture.name("uri").map(|value| value.as_str().to_string()))
        .collect::<Vec<_>>();
    if uris.len() != 1 {
        bail!("PMC owned plain-text licence URI missing or ambiguous");
    }
    let native_uri = uris[0].clone();
    let general = children[2]
        .select(&sel("a.usa-link[href=\"/about/copyright/\"]"))
        .collect::<Vec<_>>();
    if general.len() != 1 {
        bail!("Observed general PMC copyright-notice link missing or ambiguous");
    }
    let notice = text_of(&paragraph);
    let evidence = json!({"source_url":URL,"source_selector":LICENSE_WRAPPER,"source_wrapper_id":"clp_a","source_wrapper_html_sha256":crate::common::sha256_hex(wrapper.html().as_bytes()),"source_element_id":paragraph.value().attr("id"),"source_notice":notice,"source_notice_text_node":raw_notice,"source_notice_html":paragraph.html(),"source_notice_html_sha256":crate::common::sha256_hex(paragraph.html().as_bytes()),"native_license_uri":native_uri,"native_license_uri_representation":"plain_text","native_license_anchor_present":false,"source_copyright_notice":copyright,"source_copyright_html":children[0].html(),"general_PMC_copyright_notice":{"native_href":general[0].value().attr("href"),"url":common::link(base,general[0].value().attr("href").unwrap()),"is_article_license_target":false},"scope":"Exact owned front-matter article notice; body/reference/footer CC URIs are not licence evidence; figure-specific/third-party exceptions remain independent"});
    Ok((Rights {license:Some("CC BY-NC-ND 4.0".into()),status:"restricted".into(),terms_url:Some(native_uri),note:Some(notice),restrictions:vec!["Noncommercial; no derivatives. Asset-specific and third-party permissions remain separate.".into()],evidence_urls:vec![URL.into()],..Default::default()},vec![evidence]))
}
fn own_dates(document: &Html) -> Vec<DateObservation> {
    // Only source-labelled own article metadata. The observed YYYY Mon DD value
    // is normalized locally; Received/Accepted/collection/correction dates do not
    // enter this function and the shared date helper stays unchanged.
    ["citation_publication_date", "citation_online_date"]
        .iter()
        .flat_map(|name| {
            meta(document, name).into_iter().map(move |raw| {
                let mut observation = common::date(name, &raw, URL);
                if observation.normalized.is_none() {
                    let pattern =
                        regex::Regex::new(r"^((?:19|20)\d{2}) ([A-Z][a-z]{2}) ([0-9]{1,2})$")
                            .expect("static publication-date pattern");
                    if let Some(capture) = pattern.captures(&raw) {
                        let year = capture[1].parse::<u32>().expect("matched four digit year");
                        let month = match &capture[2] {
                            "Jan" => Some(1),
                            "Feb" => Some(2),
                            "Mar" => Some(3),
                            "Apr" => Some(4),
                            "May" => Some(5),
                            "Jun" => Some(6),
                            "Jul" => Some(7),
                            "Aug" => Some(8),
                            "Sep" => Some(9),
                            "Oct" => Some(10),
                            "Nov" => Some(11),
                            "Dec" => Some(12),
                            _ => None,
                        };
                        let day = capture[3].parse::<u32>().expect("matched day digits");
                        if let Some(month) = month {
                            let days = match month {
                                4 | 6 | 9 | 11 => 30,
                                2 => {
                                    if year % 400 == 0 || year % 4 == 0 && year % 100 != 0 {
                                        29
                                    } else {
                                        28
                                    }
                                }
                                _ => 31,
                            };
                            if (1..=days).contains(&day) {
                                observation.normalized =
                                    Some(format!("{year:04}-{month:02}-{day:02}"));
                            }
                        }
                    }
                }
                observation.raw = raw;
                observation
            })
        })
        .collect()
}
fn table_cells(table: ElementRef<'_>) -> Vec<Value> {
    table.select(&sel("tr")).enumerate().map(|(row,tr)| json!({"row":row+1,"source_row_id":tr.value().attr("id"),"cells":common::cells(tr).iter().map(|cell|json!({"tag":cell.value().name(),"text":text_of(cell),"source_html":cell.html(),"rowspan":cell.value().attr("rowspan"),"colspan":cell.value().attr("colspan"),"headers":cell.value().attr("headers"),"scope":cell.value().attr("scope"),"source_cell_id":cell.value().attr("id")})).collect::<Vec<_>>()})).collect()
}

fn citation_links(
    main: ElementRef<'_>,
    article: ElementRef<'_>,
    base: &Url,
) -> (Vec<Value>, Vec<Value>) {
    fn target(base: &Url, href: &str) -> Option<Url> {
        let url = base.join(href).ok()?;
        (matches!(url.scheme(), "https" | "http")
            && url.username().is_empty()
            && url.password().is_none())
        .then_some(url)
    }
    fn identifier(url: &Url) -> Option<Value> {
        let path = url.path().trim_matches('/');
        match url.host_str()? {
            "doi.org" | "www.doi.org" if path.starts_with("10.") => {
                Some(json!({"scheme":"DOI","value":path,"source_url":url.as_str()}))
            }
            "pubmed.ncbi.nlm.nih.gov"
                if !path.is_empty() && path.bytes().all(|c| c.is_ascii_digit()) =>
            {
                Some(json!({"scheme":"PMID","value":path,"source_url":url.as_str()}))
            }
            _ => None,
        }
    }
    let body_ids = main
        .select(&sel("[id]"))
        .filter_map(|e| e.value().attr("id"))
        .collect::<HashSet<_>>();
    let mut anchors = vec![];
    let mut references = vec![];
    let mut seen = HashSet::new();
    for (i, a) in main.select(&sel("a[href]")).enumerate() {
        let href = a.value().attr("href").unwrap();
        let resolved = target(base, href);
        let fragment = resolved
            .as_ref()
            .filter(|u| u.host_str() == base.host_str() && u.path() == base.path())
            .and_then(|u| u.fragment());
        let parent_id = a
            .ancestors()
            .filter_map(ElementRef::wrap)
            .filter(|e| e.id() != a.id())
            .find_map(|e| e.value().attr("id"));
        anchors.push(json!({"native_order":i+1,"native_anchor_id":a.value().attr("id"),"native_parent_id":parent_id,"source_href":if resolved.is_some(){Some(href)}else{None},"unresolved_href_sha256":if resolved.is_none(){Some(crate::common::sha256_hex(href.as_bytes()))}else{None},"target_url":resolved.as_ref().map(Url::as_str),"native_link_text":text_of(&a),"target_fragment":fragment,"target_in_clinical_body":fragment.map(|id|body_ids.contains(id)),"explicit_identifier":resolved.as_ref().and_then(identifier),"target_fetched":false}));
        if let Some(id) = fragment.filter(|id| !body_ids.contains(id)) {
            if seen.insert(id.to_string()) {
                let nodes = article
                    .select(&sel("[id]"))
                    .filter(|e| e.value().attr("id") == Some(id))
                    .collect::<Vec<_>>();
                let descriptors = if nodes.len() == 1 {
                    nodes[0]
                        .select(&sel("a[href]"))
                        .filter_map(|link| target(base, link.value().attr("href")?))
                        .filter_map(|url| identifier(&url))
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                };
                references.push(json!({"first_in_body_anchor_order":i+1,"native_target_id":id,"native_target_matches":nodes.len(),"source_emitted_identifier_links_in_target_order":descriptors,"full_back_reference_text_included":false,"target_role":"Source fragment target outside clinical body; not assumed to be a bibliographic study","external_graph_fetched":false}));
            }
        }
    }
    (anchors, references)
}

fn content(html: &str, base: &Url) -> Result<GuidelineDoc> {
    if base.as_str() != URL {
        bail!("Unexpected PMC patient expression URL");
    }
    let document = Html::parse_document(html);
    let dois = meta(&document, "citation_doi");
    if dois.is_empty() || dois.iter().any(|v| !v.eq_ignore_ascii_case(DOI)) {
        bail!("PMC patient expression DOI missing or mismatched");
    }
    let pmcids = meta(&document, "citation_pmcid");
    if pmcids.iter().any(|id| id != PMCID) {
        bail!("PMC patient expression identifier conflicts with its native metadata");
    }
    let articles = document.select(&sel("main article")).collect::<Vec<_>>();
    if articles.len() != 1 {
        bail!("PMC primary article root ambiguous or missing");
    }
    let article = articles[0];
    let title = article
        .select(&sel("h1"))
        .next()
        .map(|e| text_of(&e))
        .filter(|v| !v.is_empty())
        .context("PMC patient guideline title missing")?;
    if !title.to_lowercase().contains("patient guideline")
        || !title
            .to_lowercase()
            .contains("non-alcoholic fatty liver disease")
    {
        bail!("PMC work title is not the verified patient guideline");
    }
    let roots = document.select(&sel(ROOT)).collect::<Vec<_>>();
    if roots.len() != 1 {
        bail!("PMC clinical body missing or ambiguous; no body fallback");
    }
    let main = roots[0];
    let (rights, native_licenses) = license(article, base)?;
    let (native_anchors, reference_descriptors) = citation_links(main, article, base);
    let dates = own_dates(&document);
    let wrappers = main.select(&sel("section#tbl1.tw")).collect::<Vec<_>>();
    if wrappers.len() != 1 || main.select(&sel("table")).count() != 1 {
        bail!("Verified patient table wrapper/count changed");
    }
    let wrapper = wrappers[0];
    let tables = wrapper
        .select(&sel(".tbl-box table.content"))
        .collect::<Vec<_>>();
    if tables.len() != 1 {
        bail!("PMC patient table structure missing");
    }
    let table = tables[0];
    let table_id = table.value().attr("id").unwrap_or("tbl1-table").to_string();
    let caption = table
        .select(&sel("caption"))
        .next()
        .or_else(|| wrapper.select(&sel(".caption, .tw-caption")).next())
        .map(|e| text_of(&e))
        .filter(|v| !v.is_empty());
    let mut warnings = vec![];
    if caption.is_none() {
        warnings.push(
            "Native table caption shape unresolved; full wrapper HTML retained for verification."
                .into(),
        );
    }
    let cells = table_cells(table);
    if cells.is_empty() {
        bail!("PMC patient table has no native rows");
    }
    let notes = wrapper.select(&sel(".tw-foot")).collect::<Vec<_>>();
    let mut note_records = vec![];
    for (i, note) in notes.iter().enumerate() {
        if note
            .select(&sel("table, figure, img, .tw-foot"))
            .any(|child| child.id() != note.id())
        {
            bail!("Complex nested table-footnote structure unresolved");
        }
        let id = note
            .value()
            .attr("id")
            .map(String::from)
            .unwrap_or_else(|| format!("tbl1-footnotes-{}", i + 1));
        note_records.push(json!({"native_id":note.value().attr("id"),"canonical_block_id":id,"source_text":text_of(note),"source_html":note.html(),"owner_table":"tbl1","role":"table_abbreviation_footnote"}));
    }
    if notes.is_empty() {
        warnings.push(
            "Observed table footnote wrapper absent; source/context fidelity remains unresolved."
                .into(),
        );
    }
    let figures = main.select(&sel("figure")).collect::<Vec<_>>();
    let mut source_ids = HashMap::<String, usize>::new();
    let mut id_elements = main.select(&sel("[id]")).collect::<Vec<_>>();
    if main.value().attr("id").is_some() && !id_elements.iter().any(|node| node.id() == main.id()) {
        id_elements.push(main);
    }
    for node in id_elements {
        *source_ids
            .entry(node.value().attr("id").unwrap().to_string())
            .or_default() += 1;
    }
    let mut reserved_canonical_ids = HashSet::from([table_id.clone()]);
    reserved_canonical_ids.extend(
        note_records
            .iter()
            .filter_map(|note| note["canonical_block_id"].as_str())
            .map(String::from),
    );
    let mut graphic_ids = HashSet::new();
    let mut expected_graphics = Vec::<(String, Option<String>)>::new();
    let mut figure_records = vec![];
    let mut image_replacements = vec![];
    for figure in &figures {
        if figure
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|a| a.id() != figure.id() && a.value().name() == "figure")
        {
            bail!("Nested PMC figure structure unresolved");
        }
        let id = figure
            .value()
            .attr("id")
            .context("PMC figure source identifier missing")?;
        if id.is_empty() || source_ids.get(id) != Some(&1) {
            bail!("Duplicate or empty native PMC figure identifier");
        }
        let numbered = |prefix: &str| {
            id.strip_prefix(prefix).is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit())
            })
        };
        let role = if numbered("undfig") {
            "inline_graphical_node"
        } else if numbered("fig") {
            "numbered_figure_node"
        } else {
            "other_source_figure_node"
        };
        let caption = figure
            .select(&sel("figcaption"))
            .next()
            .map(|e| text_of(&e))
            .filter(|v| !v.is_empty());
        let mut image_records = vec![];
        for (i, image) in figure.select(&sel("img")).enumerate() {
            let block_id = image
                .value()
                .attr("id")
                .map(String::from)
                .unwrap_or_else(|| format!("{id}-image-{}", i + 1));
            if block_id.is_empty()
                || image.value().attr("id").is_some() && source_ids.get(&block_id) != Some(&1)
                || image.value().attr("id").is_none() && source_ids.contains_key(&block_id)
                || reserved_canonical_ids.contains(&block_id)
                || !graphic_ids.insert(block_id.clone())
            {
                bail!("Duplicate native/generated PMC graphical locator or non-graphic collision");
            }
            let source = image_url(image, base);
            expected_graphics.push((block_id.clone(), source.clone()));
            let replacement = format!(
                "<img id=\"{}\"{}>",
                escaped(&block_id),
                source
                    .as_ref()
                    .map(|url| format!(" src=\"{}\"", escaped(url)))
                    .unwrap_or_default()
            );
            image_replacements.push((image.id(), replacement));
            image_records.push(json!({"source_image_id":image.value().attr("id"),"canonical_block_id":block_id,"source_url":source,"source_alt":image.value().attr("alt"),"source_image_html":image.html(),"asset_bytes_verified":false}));
        }
        if image_records.is_empty() {
            if reserved_canonical_ids.contains(id) || !graphic_ids.insert(id.to_string()) {
                bail!("PMC graphical placeholder locator collision");
            }
            expected_graphics.push((id.to_string(), None));
        }
        figure_records.push(json!({"native_id":id,"role":role,"native_caption":caption,"source_text":text_of(figure),"source_html":figure.html(),"source_html_sha256":crate::common::sha256_hex(figure.html().as_bytes()),"images":image_records,"no_image_placeholder_block_id":if image_records.is_empty(){Some(id)}else{None},"asset_bytes_verified":false}));
    }
    // Keep prose/list/table position through the shared walker. Only image IDs and
    // the table-footnote role are adapted locally; original components remain above.
    let original = main.html();
    let mut materialized = String::new();
    let mut cursor = 0;
    for element in main.descendants().filter_map(ElementRef::wrap) {
        if element
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|p| p.id() != element.id() && p.value().classes().any(|c| c == "tw-foot"))
        {
            continue;
        }
        let replacement = if element.id() == table.id() && table.value().attr("id").is_none() {
            Some(element.html().replacen(
                "<table",
                &format!("<table id=\"{}\"", escaped(&table_id)),
                1,
            ))
        } else if let Some((_, replacement)) = image_replacements
            .iter()
            .find(|(id, _)| *id == element.id())
        {
            Some(replacement.clone())
        } else if let Some((i, _)) = notes
            .iter()
            .enumerate()
            .find(|(_, note)| note.id() == element.id())
        {
            Some(format!(
                "<aside id=\"{}\">{}</aside>",
                escaped(note_records[i]["canonical_block_id"].as_str().unwrap()),
                element.inner_html()
            ))
        } else {
            None
        };
        if let Some(replacement) = replacement {
            let markup = element.html();
            let offset = original[cursor..]
                .find(&markup)
                .context("PMC component source order could not be reconciled")?;
            let start = cursor + offset;
            materialized.push_str(&original[cursor..start]);
            materialized.push_str(&replacement);
            cursor = start + markup.len();
        }
    }
    materialized.push_str(&original[cursor..]);
    let (mut blocks, walker_warnings) = blocks_from_html(
        &materialized,
        Some(base),
        Some("section.body.main-article-body"),
    );
    warnings.extend(walker_warnings);
    let mut table_matches = 0;
    for block in &mut blocks {
        if block.block_id == table_id {
            if let BlockKind::Table { caption: c } = &mut block.kind {
                *c = caption.clone();
                table_matches += 1;
            }
        }
    }
    if table_matches != 1 || blocks.is_empty() {
        bail!("PMC native table/body did not survive canonical extraction");
    }
    let byid = blocks
        .iter()
        .map(|b| (&b.block_id, b))
        .collect::<HashMap<_, _>>();
    if byid.len() != blocks.len() {
        bail!("PMC canonical block identifiers duplicated");
    }
    // Check one-to-one identity in native source order, not a find-first lookup:
    // the shared walker may rename duplicate IDs, which must never hide a collision.
    let canonical_graphics = blocks
        .iter()
        .filter(|block| matches!(block.kind, BlockKind::Figure { .. }))
        .collect::<Vec<_>>();
    if canonical_graphics.len() != expected_graphics.len() {
        bail!("PMC graphical source/canonical count is not one-to-one");
    }
    for ((expected_id, expected_url), block) in expected_graphics.iter().zip(canonical_graphics) {
        if &block.block_id != expected_id {
            bail!("PMC graphical source/canonical ID order changed or was renamed");
        }
        if let BlockKind::Figure { src_url, .. } = &block.kind {
            if src_url != expected_url {
                bail!("PMC graphical source/canonical image target mismatch");
            }
        }
    }
    for note in &note_records {
        if !blocks.iter().any(|b| {
            b.block_id == note["canonical_block_id"].as_str().unwrap()
                && matches!(b.kind, BlockKind::Footnote)
                && b.text == note["source_text"].as_str().unwrap()
        }) {
            bail!("PMC table footnote/context locator mismatch");
        }
    }
    let mut attachments = vec![];
    let mut relations = vec![];
    let mut seen_assets = HashSet::new();
    let mut correction_links = vec![];
    for anchor in document.select(&sel("a[href]")) {
        let Some(url) = common::link(base, anchor.value().attr("href").unwrap_or_default()) else {
            continue;
        };
        if url == CORRECTION {
            correction_links.push(json!({"url":url,"native_link_label":text_of(&anchor),"source_anchor_id":anchor.value().attr("id"),"source_anchor_scope":"Exact known correction target in primary source page"}));
        }
        let role = if url == PDF {
            Some("article_pdf")
        } else if url == SUPPLEMENT {
            Some("supplementary_pdf")
        } else {
            None
        };
        if let Some(role) = role {
            if seen_assets.insert((url.clone(), role)) {
                attachments.push(json!({"url":url,"role":role,"label":text_of(&anchor),"source_evidence_url":URL,"source_anchor_id":anchor.value().attr("id"),"source_anchor_scope":"Exact verified article asset target in source page; toolbar may be outside clinical body","asset_bytes_verified":false}));
            }
        }
    }
    if !correction_links.is_empty() {
        relations.push(Relation {rel:"erratum".into(),target:CORRECTION.into(),scope:Some("Known 2024 correction supplies a missing peer-review statement, as independently source-profiled; no clinical recommendation change established. Correction content is not fetched or merged.".into()),evidence_url:Some(URL.into())});
    } else {
        warnings.push("Previously observed correction link not present in this native article response; relation remains a separate source observation, not a fabricated native link.".into());
    }
    let published = dates
        .iter()
        .find(|d| d.label == "citation_online_date")
        .or_else(|| {
            dates
                .iter()
                .find(|d| d.label == "citation_publication_date")
        })
        .and_then(|d| d.normalized.clone());
    warnings.push("One independently verified patient-guideline PMC expression only; professional EASL CPG access, catalog coverage, original XML, full clinical/visual fidelity and asset bytes are not established. No formal recommendation/GRADE entities inferred from patient advice.".into());
    Ok(GuidelineDoc {
        title,
        url: URL.into(),
        work_key: Some(format!("doi:{DOI}")),
        item_type: Some("patient_guideline".into()),
        doi: Some(DOI.into()),
        pmid: meta(&document, "citation_pmid").into_iter().next(),
        publisher: meta(&document, "citation_journal_title").into_iter().next(),
        issuers: vec!["EASL".into()],
        language: common::page_language(&document),
        country: None,
        status: "unknown".into(),
        version: None,
        published,
        updated: None,
        date_observations: dates,
        rights,
        relations,
        blocks,
        attachments,
        warnings,
        search_metadata: json!({"expression_id":PMCID,"expression_url":URL,"work_identifier":{"scheme":"DOI","value":DOI},"native_pmcid_metadata":meta(&document,"citation_pmcid"),"issuer_association":{"value":"EASL","role":"Patient-guideline catalog association, not professional CPG or inferred corporate authorship","evidence_url":EASL_RECORD},"publication_host":{"value":"PMC","evidence_url":URL,"not_clinical_issuer_or_jurisdiction":true},"audience":{"value":"patients","scope":"patient_guideline","evidence_url":URL},"clinical_applicability":null,"source_metadata_blueprint_is_not_population_proof":true,"native_in_body_anchors":native_anchors,"back_reference_target_descriptors":reference_descriptors,"reference_coverage_limit":"In-body anchor IDs/order/targets and explicit DOI/PMID links from uniquely matched referenced targets are retained. Back-reference text outside the clinical body is excluded; no reference/evidence graph or study effects are fetched or inferred.","clinical_root_sha256":crate::common::sha256_hex(original.as_bytes()),"native_table":{"wrapper_id":"tbl1","canonical_block_id":table_id,"caption":caption,"source_wrapper_html":wrapper.html(),"source_table_html":table.html(),"rows":cells,"footnotes":note_records},"native_figures":figure_records,"native_license_notices":native_licenses,"copyright_observations":native_licenses.iter().map(|notice|json!({"source_text":notice["source_copyright_notice"],"source_wrapper_id":"clp_a","scope":"Owned copyright notice, not publication/update date"})).collect::<Vec<_>>(),"known_correction":{"target":CORRECTION,"doi":"10.1016/j.jhepr.2024.101097","scope":"Missing peer-review statement; no clinical recommendation change established","scope_evidence":"easl_pmc_asset_contract.md and easl_publisher_expression_contract.md, observed 2026-11-14","native_primary_article_links":correction_links,"correction_body_fetched":false},"scope_complete":false,"clinical_completeness_accepted":false,"assets_bytes_verified":false,"formal_GRADE_inferred":false,"graphics_identity_contract":{"native_and_generated_graphical_ids_unique":true,"canonical_graphics_one_to_one_in_source_order":true},"date_scope":"Only supported own article publication meta labels are populated; header/issue/copyright/correction dates are not inferred or inherited"}),
        ..Default::default()
    })
}
fn snapshot(doc: &GuidelineDoc) -> Result<String> {
    Ok(crate::common::sha256_hex(&serde_json::to_vec(
        &json!({"title":doc.title,"work":doc.work_key,"doi":doc.doi,"pmid":doc.pmid,"publisher":doc.publisher,"issuers":doc.issuers,"language":doc.language,"published":doc.published,"dates":doc.date_observations,"rights":doc.rights,"relations":doc.relations,"blocks":doc.blocks,"attachments":doc.attachments,"metadata":doc.search_metadata}),
    )?))
}
fn response(page: &Fetched) -> Result<GuidelineDoc> {
    if page.record.status != 200
        || page.record.final_url != URL
        || !page.is_html()
        || !page.record.content_type.as_deref().is_some_and(|value| {
            matches!(
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "text/html" | "application/xhtml+xml"
            )
        })
        || page.is_pdf()
        || page.body.len() > 8 * 1024 * 1024
    {
        bail!("Expected bounded exact PMC patient expression HTML response");
    }
    content(&page.text(), &Url::parse(URL)?)
}
#[async_trait::async_trait]
impl Adapter for EaslPublicPmc {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        config(src)?;
        let result = async {
            let page = f.get_ok(URL).await?;
            let doc = response(&page)?;
            let hash = snapshot(&doc)?;
            Ok::<_, anyhow::Error>((page, doc, hash))
        }
        .await;
        let row = match result {
            Ok((page, doc, hash)) => PlanRow {
                source_id: src.id.clone(),
                doc_key: format!("pmc8514420-{}", &hash[..16]),
                url: URL.into(),
                title: Some(doc.title),
                rendition: Some("html".into()),
                version: None,
                extra: json!({"work_key":format!("doi:{DOI}"),"expression_id":PMCID,"source_role":"patient_guideline","clinical_snapshot_sha256":hash,"response_sha256":page.record.sha256,"scope_complete":false,"source_evidence_url":EASL_RECORD}),
            },
            Err(error) => PlanRow {
                source_id: src.id.clone(),
                doc_key: "pmc8514420-unresolved".into(),
                url: URL.into(),
                title: None,
                rendition: None,
                version: None,
                extra: json!({"discovery_error":format!("{error:#}"),"source_role":"patient_guideline","scope_complete":false}),
            },
        };
        Ok(vec![row])
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        config(src)?;
        if let Some(error) = row.extra["discovery_error"].as_str() {
            bail!("EASL patient expression unresolved: {error}");
        }
        if row.url != URL
            || row.version.is_some()
            || row.extra["expression_id"].as_str() != Some(PMCID)
            || row.extra["source_role"].as_str() != Some("patient_guideline")
        {
            bail!("Invalid patient-expression plan identity/role/version");
        }
        let page = f.get_ok(URL).await?;
        let mut doc = response(&page)?;
        if row.extra["clinical_snapshot_sha256"].as_str() != Some(snapshot(&doc)?.as_str()) {
            bail!(
                "PMC patient body/context/licence/assets/dates changed since planning; rediscover"
            );
        }
        doc.source_id = src.id.clone();
        doc.doc_key = row.doc_key.clone();
        doc.fetches = vec![page.record];
        Ok(doc)
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const PAGE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/easl_public_pmc/patient-synthetic.html"
    ));
    fn parsed() -> GuidelineDoc {
        content(PAGE, &Url::parse(URL).unwrap()).unwrap()
    }
    fn source(config: Value) -> SourceEntry {
        serde_json::from_value(json!({"id":"easl_public_pmc","adapter":"easl_public_pmc","enabled":true,"access":"public","config":config})).unwrap()
    }
    #[test]
    fn patient_body_preserves_order_native_table_geometry_and_owned_footnote() {
        let doc = parsed();
        assert_eq!(doc.item_type.as_deref(), Some("patient_guideline"));
        assert_eq!(
            doc.work_key.as_deref(),
            Some("doi:10.1016/j.jhepr.2021.100322")
        );
        assert_eq!(doc.search_metadata["expression_id"], PMCID);
        assert!(!doc
            .blocks
            .iter()
            .any(|b| b.text.contains("Navigation excluded")
                || b.text.contains("Footer 2026")
                || b.text.contains("References outside clinical body")
                || b.text.contains("Synthetic excluded bibliographic text")));
        let table = doc
            .blocks
            .iter()
            .position(|b| matches!(b.kind, BlockKind::Table { .. }))
            .unwrap();
        let note = doc
            .blocks
            .iter()
            .position(|b| matches!(b.kind, BlockKind::Footnote))
            .unwrap();
        assert!(table < note);
        assert_eq!(doc.blocks[table].block_id, "tbl1-table");
        assert!(
            matches!(&doc.blocks[table].kind,BlockKind::Table {caption:Some(c)} if c=="Synthetic criteria table.")
        );
        assert_eq!(
            doc.search_metadata["native_table"]["rows"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            doc.search_metadata["native_table"]["rows"][0]["cells"][0]["colspan"],
            "2"
        );
        assert_eq!(
            doc.search_metadata["native_table"]["rows"][1]["cells"][0]["rowspan"],
            "1"
        );
        assert_eq!(
            doc.search_metadata["native_table"]["footnotes"][0]["owner_table"],
            "tbl1"
        );
        assert_eq!(
            doc.blocks[note].text,
            "Synthetic abbreviation note belongs to Table 1."
        );
        let p1 = doc.blocks.iter().position(|b| b.block_id == "p1").unwrap();
        let li2 = doc.blocks.iter().position(|b| b.block_id == "li2").unwrap();
        let p3 = doc.blocks.iter().position(|b| b.block_id == "p3").unwrap();
        assert!(p1 < li2 && li2 < table && note < p3);
        assert_eq!(
            doc.blocks
                .iter()
                .map(|b| &b.block_id)
                .collect::<HashSet<_>>()
                .len(),
            doc.blocks.len()
        );
        assert!(!doc
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Recommendation { .. })));
    }
    #[test]
    fn numbered_and_inline_graphics_keep_native_parent_and_distinct_unfetched_assets() {
        let doc = parsed();
        let figures = doc.search_metadata["native_figures"].as_array().unwrap();
        assert_eq!(figures.len(), 2);
        assert_eq!(figures[0]["native_id"], "fig1");
        assert_eq!(figures[0]["role"], "numbered_figure_node");
        assert_eq!(figures[1]["role"], "inline_graphical_node");
        assert_eq!(figures[1]["native_caption"], Value::Null);
        assert!(doc.blocks.iter().any(|b|b.block_id=="fig1-image-1"&&matches!(&b.kind,BlockKind::Figure {caption:Some(c),..}if c=="Synthetic numbered figure caption.")));
        assert!(
            doc.blocks
                .iter()
                .any(|b| b.block_id == "undfig1-image-1"
                    && matches!(b.kind, BlockKind::Figure { .. }))
        );
        assert!(figures.iter().all(|f| f["asset_bytes_verified"] == false));
        assert_eq!(doc.attachments.len(), 2);
        assert!(doc
            .attachments
            .iter()
            .any(|a| a["url"] == PDF && a["role"] == "article_pdf"));
        assert!(doc
            .attachments
            .iter()
            .any(|a| a["url"] == SUPPLEMENT && a["role"] == "supplementary_pdf"));
        assert!(doc
            .attachments
            .iter()
            .all(|a| a["asset_bytes_verified"] == false));
    }
    #[test]
    fn native_licence_own_dates_issuer_and_correction_keep_separate_scope() {
        let doc = parsed();
        assert_eq!(doc.rights.license.as_deref(), Some("CC BY-NC-ND 4.0"));
        assert_eq!(doc.rights.status, "restricted");
        assert!(doc
            .rights
            .note
            .as_ref()
            .unwrap()
            .contains("This synthetic article notice"));
        assert!(doc
            .rights
            .note
            .as_ref()
            .unwrap()
            .contains("third-party figures"));
        assert_eq!(doc.published.as_deref(), Some("2021-09-17"));
        assert_eq!(doc.updated, None);
        assert_eq!(doc.date_observations.len(), 1);
        assert_eq!(doc.date_observations[0].label, "citation_publication_date");
        assert_eq!(doc.issuers, vec!["EASL"]);
        assert_eq!(doc.publisher.as_deref(), Some("JHEP Reports"));
        assert_eq!(doc.search_metadata["publication_host"]["value"], "PMC");
        assert_eq!(doc.country, None);
        assert_eq!(doc.search_metadata["clinical_applicability"], Value::Null);
        assert_eq!(doc.relations.len(), 1);
        assert_eq!(doc.relations[0].target, CORRECTION);
        assert!(doc.relations[0]
            .scope
            .as_ref()
            .unwrap()
            .contains("no clinical recommendation change"));
        let no_date = PAGE.replace(
            "<meta name=\"citation_publication_date\" content=\"2021 Sep 17\">",
            "",
        );
        let no_date = content(&no_date, &Url::parse(URL).unwrap()).unwrap();
        assert_eq!(no_date.published, None);
        assert!(no_date.date_observations.is_empty());
        assert_eq!(no_date.status, "unknown");
        assert_eq!(no_date.version, None);
    }
    #[test]
    fn citation_anchor_order_and_explicit_doi_pmid_descriptors_do_not_fetch_reference_graph() {
        let doc = parsed();
        let anchors = doc.search_metadata["native_in_body_anchors"]
            .as_array()
            .unwrap();
        let xref = anchors
            .iter()
            .find(|a| a["native_anchor_id"] == "xref1")
            .unwrap();
        assert_eq!(xref["target_fragment"], "bib1");
        assert_eq!(xref["target_in_clinical_body"], false);
        assert_eq!(xref["native_parent_id"], "p1");
        let refs = doc.search_metadata["back_reference_target_descriptors"]
            .as_array()
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["native_target_id"], "bib1");
        assert_eq!(refs[0]["native_target_matches"], 1);
        assert_eq!(
            refs[0]["source_emitted_identifier_links_in_target_order"][0]["scheme"],
            "DOI"
        );
        assert_eq!(
            refs[0]["source_emitted_identifier_links_in_target_order"][1]["value"],
            "12345678"
        );
        assert_eq!(refs[0]["full_back_reference_text_included"], false);
        assert_eq!(refs[0]["external_graph_fetched"], false);
    }
    #[test]
    fn semantic_snapshot_ignores_navigation_but_tracks_clinical_context_assets_and_rights() {
        let before = snapshot(&parsed()).unwrap();
        let nav = PAGE
            .replace("Navigation excluded", "Changed navigation")
            .replace("Footer 2026", "Changed footer");
        assert_eq!(
            before,
            snapshot(&content(&nav, &Url::parse(URL).unwrap()).unwrap()).unwrap()
        );
        for (from, to) in [
            ("Synthetic concluding context", "Changed concluding context"),
            ("Value 2", "Value changed"),
            ("Synthetic abbreviation note", "Changed abbreviation note"),
            ("gr1.jpg", "gr2.jpg"),
            (
                "This synthetic article notice",
                "This changed article notice",
            ),
            ("content=\"2021 Sep 17\"", "content=\"2021 Sep 18\""),
            ("/articles/PMC11228441/", "/articles/PMC99999999/"),
            ("10.0000/synthetic-reference", "10.0000/changed-reference"),
        ] {
            let changed = content(&PAGE.replace(from, to), &Url::parse(URL).unwrap()).unwrap();
            assert_ne!(
                before,
                snapshot(&changed).unwrap(),
                "untracked semantic change: {from}"
            );
        }
    }
    #[test]
    fn wrong_work_missing_primary_root_or_licence_fail_without_whole_page_fallback() {
        for html in [
            PAGE.replace(DOI, "10.0000/wrong-work"),
            PAGE.replace("body main-article-body", "not-clinical-body"),
            PAGE.replace("patient guideline</h1>", "professional guideline</h1>"),
            PAGE.replace("/licenses/by-nc-nd/4.0/", "/licenses/by/4.0/"),
        ] {
            assert!(content(&html, &Url::parse(URL).unwrap()).is_err());
        }
        assert!(content(
            PAGE,
            &Url::parse("https://pmc.ncbi.nlm.nih.gov/articles/PMC99999999/").unwrap()
        )
        .is_err());
    }
    #[tokio::test]
    async fn real_cached_plan_extract_and_drift_guard_are_bounded_to_one_patient_expression() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 8 << 20, "").unwrap();
        let src = source(json!({"expression_url":URL}));
        assert!(EaslPublicPmc
            .discover(&src, &f, Some(0))
            .await
            .unwrap()
            .is_empty());
        f.seed_cache_for_tests(URL, PAGE.as_bytes(), "text/html");
        let rows = EaslPublicPmc.discover(&src, &f, Some(50)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].extra["source_role"], "patient_guideline");
        assert_eq!(rows[0].extra["scope_complete"], false);
        assert_eq!(rows[0].version, None);
        let doc = EaslPublicPmc.extract(&src, &f, &rows[0]).await.unwrap();
        assert_eq!(doc.fetches.len(), 1);
        assert_eq!(doc.fetches[0].url, URL);
        assert_eq!(doc.search_metadata["clinical_completeness_accepted"], false);
        f.seed_cache_for_tests(
            URL,
            PAGE.replace("Value 2", "Changed value").as_bytes(),
            "text/html",
        );
        assert!(EaslPublicPmc.extract(&src, &f, &rows[0]).await.is_err());
    }
    #[tokio::test]
    async fn response_and_config_guards_reject_other_routes_shells_and_false_mime() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 8 << 20, "").unwrap();
        assert!(EaslPublicPmc
            .discover(
                &source(json!({"expression_url":"https://easl.eu/publication/cpg-hbv/"})),
                &f,
                None
            )
            .await
            .is_err());
        f.seed_cache_for_tests(URL, b"<html><main>Loading</main></html>", "text/html");
        let rows = EaslPublicPmc
            .discover(&source(json!({})), &f, None)
            .await
            .unwrap();
        assert!(rows[0].extra["discovery_error"].is_string());
        f.seed_cache_for_tests(URL, PAGE.as_bytes(), "text/html");
        let mut page = f.get_ok(URL).await.unwrap();
        page.record.final_url = "https://easl.eu/unverified".into();
        assert!(response(&page).is_err());
        page.record.final_url = URL.into();
        page.record.content_type = Some("application/json".into());
        assert!(response(&page).is_err());
    }

    #[test]
    fn owned_plaintext_licence_preserves_uri_spelling_and_general_notice_roles() {
        let doc = parsed();
        let notice = &doc.search_metadata["native_license_notices"][0];
        assert_eq!(notice["source_wrapper_id"], "clp_a");
        assert_eq!(
            notice["native_license_uri"],
            "http://creativecommons.org/licenses/by-nc-nd/4.0/"
        );
        assert_eq!(notice["native_license_uri_representation"], "plain_text");
        assert_eq!(notice["native_license_anchor_present"], false);
        assert!(notice["source_notice_text_node"]
            .as_str()
            .unwrap()
            .contains("(http://creativecommons.org/licenses/by-nc-nd/4.0/)"));
        assert_eq!(
            notice["general_PMC_copyright_notice"]["url"],
            "https://pmc.ncbi.nlm.nih.gov/about/copyright/"
        );
        assert_eq!(
            notice["general_PMC_copyright_notice"]["is_article_license_target"],
            false
        );
        assert_eq!(
            doc.rights.terms_url.as_deref(),
            Some("http://creativecommons.org/licenses/by-nc-nd/4.0/")
        );
        let document = Html::parse_document(PAGE);
        assert_eq!(
            document
                .select(&sel("a[href*=\"creativecommons.org\"]"))
                .count(),
            0
        );
        let https = content(
            &PAGE.replace(
                "http://creativecommons.org/licenses/by-nc-nd/4.0/",
                "https://creativecommons.org/licenses/by-nc-nd/4.0/",
            ),
            &Url::parse(URL).unwrap(),
        )
        .unwrap();
        assert_eq!(
            https.rights.terms_url.as_deref(),
            Some("https://creativecommons.org/licenses/by-nc-nd/4.0/")
        );
    }
    #[test]
    fn unowned_prose_reference_footer_licences_cannot_replace_the_article_notice() {
        let own="This synthetic article notice names the CC BY-NC-ND 4.0 licence (http://creativecommons.org/licenses/by-nc-nd/4.0/); third-party figures require separate assessment.";
        let missing = PAGE.replace(own, "No article licence notice supplied.");
        assert_ne!(missing, PAGE);
        let unrelated = r#"<p>Unrelated licence statement http://creativecommons.org/licenses/by-nc-nd/4.0/ <a href="http://creativecommons.org/licenses/by-nc-nd/4.0/">CC BY-NC-ND 4.0</a></p>"#;
        for marker in ["<p id=\"p3\">", "<p id=\"bib1\">", "<footer>"] {
            let forged = missing.replace(marker, &format!("{unrelated}{marker}"));
            assert!(
                content(&forged, &Url::parse(URL).unwrap()).is_err(),
                "accepted unowned rights from {marker}"
            );
        }
        assert!(content(
            &PAGE.replace("id=\"clp_a\"", "id=\"unowned-panel\""),
            &Url::parse(URL).unwrap()
        )
        .is_err());
        let start = PAGE.find("<section class=\"front-matter\">").unwrap();
        let stop = start + PAGE[start..].find("</section>").unwrap() + "</section>".len();
        let wrapper = &PAGE[start..stop];
        let moved = PAGE
            .replace(wrapper, "")
            .replace("<h2 id=\"sec1\">", &format!("{wrapper}<h2 id=\"sec1\">"));
        assert!(content(&moved, &Url::parse(URL).unwrap()).is_err());
    }
    #[test]
    fn duplicate_native_graphics_are_rejected_before_walker_id_renaming() {
        let duplicate_figure = PAGE.replace("id=\"undfig1\"", "id=\"fig1\"");
        assert!(content(&duplicate_figure, &Url::parse(URL).unwrap()).is_err());
        let duplicate_images = PAGE.replace("<img src=", "<img id=\"duplicate-image\" src=");
        assert!(content(&duplicate_images, &Url::parse(URL).unwrap()).is_err());
        let unique = PAGE
            .replace(
                "<figure id=\"fig1\"><img ",
                "<figure id=\"fig1\"><img id=\"native-image-one\" ",
            )
            .replace(
                "<figure id=\"undfig1\"><img ",
                "<figure id=\"undfig1\"><img id=\"native-image-two\" ",
            );
        let doc = content(&unique, &Url::parse(URL).unwrap()).unwrap();
        let ids = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Figure { .. }))
            .map(|b| b.block_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["native-image-one", "native-image-two"]);
        assert_eq!(
            doc.search_metadata["graphics_identity_contract"]
                ["canonical_graphics_one_to_one_in_source_order"],
            true
        );
    }
    #[test]
    fn generated_graphics_ids_reject_native_and_non_graphic_locator_collisions() {
        let later_native = PAGE.replace(
            "<figure id=\"undfig1\"><img ",
            "<figure id=\"undfig1\"><img id=\"fig1-image-1\" ",
        );
        let prose = PAGE.replace("id=\"p3\"", "id=\"fig1-image-1\"");
        let table = PAGE.replace(
            "<figure id=\"fig1\"><img ",
            "<figure id=\"fig1\"><img id=\"tbl1-table\" ",
        );
        let other_figure = PAGE.replace("<figure id=\"fig1\">", "<figure id=\"undfig1-image-1\">");
        for collision in [later_native, prose, table, other_figure] {
            assert!(content(&collision, &Url::parse(URL).unwrap()).is_err());
        }
    }
    #[test]
    fn native_year_month_day_publication_date_is_normalized_without_history_inference() {
        let history="<section class=\"history\"><p>Received 2021 May 27; Accepted 2021 Jun 8; Collection date 2021 Oct.</p></section>";
        let with_history = PAGE.replace(
            "</div></div></div></section>",
            &format!("</div></div></div>{history}</section>"),
        );
        assert_ne!(with_history, PAGE);
        let doc = content(&with_history, &Url::parse(URL).unwrap()).unwrap();
        assert_eq!(doc.date_observations.len(), 1);
        assert_eq!(doc.date_observations[0].label, "citation_publication_date");
        assert_eq!(doc.date_observations[0].raw, "2021 Sep 17");
        assert_eq!(
            doc.date_observations[0].normalized.as_deref(),
            Some("2021-09-17")
        );
        assert_eq!(doc.published.as_deref(), Some("2021-09-17"));
        let missing = with_history.replace(
            "<meta name=\"citation_publication_date\" content=\"2021 Sep 17\">",
            "",
        );
        let missing = content(&missing, &Url::parse(URL).unwrap()).unwrap();
        assert!(missing.date_observations.is_empty());
        assert_eq!(missing.published, None);
        let invalid = content(
            &PAGE.replace("content=\"2021 Sep 17\"", "content=\"2021 Feb 31\""),
            &Url::parse(URL).unwrap(),
        )
        .unwrap();
        assert_eq!(invalid.date_observations[0].raw, "2021 Feb 31");
        assert_eq!(invalid.date_observations[0].normalized, None);
        assert_eq!(invalid.published, None);
    }
}
