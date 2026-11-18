//! ACR Appropriateness Criteria topic assets and structured variant-rating narrative.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, GuidelineDoc, Rights};
use crate::guidelines::fetch::{Fetched, Fetcher};
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::{ElementRef, Html, Node};
use serde_json::{json, Value};
use std::collections::HashSet;
use url::Url;
pub struct AcrRadiology;

// The source-owned item-0 editor uses literal text/SUP/SPAN runs. Inserting
// whitespace at each inline boundary changes its text (including superscripts).
// This deliberately narrow grammar preserves source text nodes and rejects a
// new structural/visibility shape rather than silently treating it as equivalent.
fn terminal_inline_text(fragment: &str) -> Result<String> {
    if fragment.len() > 65_536 {
        bail!("ACR terminal inline fragment exceeds bounded grammar");
    }
    fn append(el: ElementRef<'_>, depth: usize, nodes: &mut usize, out: &mut String) -> Result<()> {
        if depth > 8 {
            bail!("ACR terminal inline nesting exceeds bounded grammar");
        }
        for child in el.children() {
            match child.value() {
                Node::Text(text) => out.push_str(text),
                Node::Element(_) => {
                    let inline = ElementRef::wrap(child).context("ACR inline element missing")?;
                    *nodes += 1;
                    if *nodes > 128
                        || !matches!(inline.value().name(), "sup" | "span")
                        || inline.value().attrs().any(|(name, _)| name != "title")
                    {
                        bail!("ACR terminal inline structure/attributes unsupported");
                    }
                    append(inline, depth + 1, nodes, out)?;
                }
                _ => bail!("ACR terminal inline non-text node unsupported"),
            }
        }
        Ok(())
    }
    let parsed = Html::parse_fragment(fragment);
    let mut text = String::new();
    append(parsed.root_element(), 0, &mut 0, &mut text)?;
    Ok(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

// Markdown cannot express the native spanning legend cells. Preserve their
// actual DOM attributes and cell ownership separately, without changing the
// original canonical Table blocks or inferring header/footnote semantics.
fn narrative_table_geometry(materialized: &str, blocks: &[Block]) -> Result<Vec<Value>> {
    let parsed = Html::parse_document(materialized);
    let root = parsed
        .select(&sel("#maindiv"))
        .next()
        .context("ACR materialized root missing")?;
    let tables = root.select(&sel("table")).collect::<Vec<_>>();
    let canonical = blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
        .collect::<Vec<_>>();
    if tables.len() > 256 || tables.len() != canonical.len() {
        bail!("ACR native/canonical table association exceeds supported shape");
    }
    let (mut row_count, mut cell_count, mut html_bytes) = (0usize, 0usize, 0usize);
    let mut result = Vec::new();
    for (ordinal, (table, block)) in tables.iter().zip(canonical).enumerate() {
        if table.select(&sel("table")).next().is_some() {
            bail!("ACR narrative nested table geometry unsupported");
        }
        let html = table.html();
        html_bytes += html.len();
        if html_bytes > 16 * 1024 * 1024 {
            bail!("ACR narrative table serialization exceeds bounded size");
        }
        let mut rows = Vec::new();
        for (row_ordinal, row) in table.select(&sel("tr")).enumerate() {
            row_count += 1;
            let mut cells = Vec::new();
            for (cell_ordinal, cell) in common::cells(row).into_iter().enumerate() {
                cell_count += 1;
                if row_count > 16_384 || cell_count > 65_536 {
                    bail!("ACR narrative table cells exceed bounded size");
                }
                cells.push(json!({"cell_ordinal":cell_ordinal+1,"native_tag":cell.value().name(),"text":text_of(&cell),"rowspan":cell.value().attr("rowspan"),"colspan":cell.value().attr("colspan")}));
            }
            if !cells.is_empty() {
                rows.push(json!({"row_ordinal":row_ordinal+1,"cells":cells}));
            }
        }
        result.push(json!({"source_table_ordinal":ordinal+1,"canonical_block_id":block.block_id,"native_id":table.value().attr("id"),"source_table_html":html,"source_table_html_format":"html5_DOM_serialization_not_original_response_bytes","caption":table.select(&sel("caption")).next().map(|c|text_of(&c)),"rows":rows,"header_role_inferred":false,"external_footnote_ownership_inferred":false}));
    }
    Ok(result)
}
fn catalog(html: &str, base: &Url) -> Result<Vec<(String, String, String)>> {
    let d = Html::parse_document(html);
    let rows = d
        .select(&sel("div.row.divOdd,div.row.divEven"))
        .collect::<Vec<_>>();
    if rows.is_empty() {
        bail!("ACR criteria topic listing missing");
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    for row in rows {
        let topic = row
            .select(&sel("div.col-lg-4"))
            .next()
            .map(|e| text_of(&e))
            .unwrap_or_default();
        if topic.is_empty() {
            continue;
        }
        for a in row.select(&sel("a[href]")) {
            let label = text_of(&a);
            if label.is_empty() {
                continue;
            }
            let role = match label.as_str() {
                "Narrative" => "narrative",
                "Evidence Table" => "evidence_table",
                "Lit Search" => "literature_search",
                "Ratings & Appendix" => "ratings_appendix",
                "Patient Summary" => "patient_summary",
                _ => continue,
            };
            let Some(url) = common::link(base, a.value().attr("href").unwrap_or_default()) else {
                continue;
            };
            if seen.insert((url.clone(), role.to_string())) {
                out.push((url, topic.clone(), role.into()));
            }
        }
    }
    if out.is_empty() {
        bail!("ACR criteria listing has no source-labelled assets");
    }
    Ok(out)
}
// The public page asynchronously copies these backing HTML attributes into TinyMCE iframes.
// scraper decodes the attribute's HTML entity layer once; parse its HTML content once more,
// exactly as the source editor does. Replace the backing element rather than append a copy.
fn materialized_narrative(main: ElementRef<'_>) -> Result<(String, Vec<Value>)> {
    fn attr(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('\"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    let original = main.html();
    let mut result = String::new();
    let mut cursor = 0;
    let mut sources = vec![];
    // These are the observed native section/variant display labels, not inferred clinical headings.
    let selector=".txtSectiontextReadOnly[data-origtext], div.font-size-18.font-weight-semibold > div, .author-content .d-flex.justify-content-between > div";
    for element in main.select(&sel(selector)) {
        let backing = element
            .value()
            .classes()
            .any(|class| class == "txtSectiontextReadOnly");
        let replacement = if backing {
            let fragment = element.value().attr("data-origtext").unwrap_or("");
            if fragment.trim().is_empty() {
                continue;
            }
            let section = element
                .ancestors()
                .filter_map(ElementRef::wrap)
                .find(|ancestor| {
                    ancestor
                        .value()
                        .classes()
                        .any(|class| class == "author-content")
                });
            let terminal = section.is_none()
                && element
                    .parent()
                    .and_then(ElementRef::wrap)
                    .is_some_and(|parent| {
                        parent.value().classes().any(|c| c == "displayDoc")
                            && parent.value().attr("data-val-itemid") == Some("0")
                    });
            let inline_text = if terminal {
                Some(terminal_inline_text(fragment)?)
            } else {
                None
            };
            let inline_provenance = inline_text.as_ref().map(|text| json!({"source_parent_selector":"div.displayDoc[data-val-itemid='0']","normalization":"concatenate_source_text_nodes_then_whitespace_fold","decoded_source_html":fragment,"normalized_DOM_text_sha256":crate::common::sha256_hex(text.as_bytes()),"source_inline_grammar":"text_SUP_SPAN_with_optional_title_only","iframe_copy_used":false}));
            sources.push(json!({"ordinal":sources.len()+1,"source_element_id":element.value().attr("id"),"author_content_id":section.and_then(|section|section.value().attr("id")),"source_attribute":"data-origtext","html_sha256":crate::common::sha256_hex(fragment.as_bytes()),"source_element_replaced":true,"iframe_content_duplicated":false,"inline_text_materialization":inline_provenance}));
            let id = element
                .value()
                .attr("id")
                .map(|id| format!(" id=\"{}\"", attr(id)))
                .unwrap_or_default();
            if let Some(text) = inline_text {
                format!(
                    "<p{id} data-acr-materialized-fragment=\"{}\">{}</p>",
                    sources.len(),
                    attr(&text)
                )
            } else {
                format!(
                    "<div{id} data-acr-materialized-fragment=\"{}\">{fragment}</div>",
                    sources.len()
                )
            }
        } else {
            if text_of(&element).trim().is_empty() {
                continue;
            }
            let major = element
                .parent()
                .and_then(ElementRef::wrap)
                .is_some_and(|parent| {
                    parent
                        .value()
                        .classes()
                        .any(|class| class == "font-size-18")
                });
            let level = if major { 2 } else { 3 };
            let id = element
                .value()
                .attr("id")
                .map(|id| format!(" id=\"{}\"", attr(id)))
                .unwrap_or_default();
            format!("<h{level}{id}>{}</h{level}>", element.inner_html())
        };
        let markup = element.html();
        let offset = original[cursor..].find(&markup).context(
            "ACR discussion/heading position could not be reconciled with source DOM order",
        )?;
        let start = cursor + offset;
        result.push_str(&original[cursor..start]);
        result.push_str(&replacement);
        cursor = start + markup.len();
    }
    result.push_str(&original[cursor..]);
    Ok((result, sources))
}
fn narrative(html: &str, base: &Url) -> Result<(String, String, Vec<Block>, Value, Vec<String>)> {
    let d = Html::parse_document(html);
    let title = d
        .select(&sel("h1"))
        .next()
        .map(|e| text_of(&e))
        .filter(|x| !x.is_empty())
        .context("ACR narrative title missing")?;
    let main = d
        .select(&sel("#maindiv"))
        .next()
        .context("ACR narrative body missing")?;
    let variants = main.select(&sel(".divVar")).collect::<Vec<_>>();
    if variants.is_empty() {
        bail!("ACR variant tables missing");
    }
    let raw = main.html();
    let hash = crate::common::sha256_hex(raw.as_bytes());
    let (materialized, discussion_sources) = materialized_narrative(main)?;
    let (mut blocks, mut warnings) = blocks_from_html(&materialized, Some(base), Some("#maindiv"));
    let native_tables = narrative_table_geometry(&materialized, &blocks)?;
    let mut mappings = vec![];
    for (i, v) in variants.iter().enumerate() {
        let scenario = text_of(v);
        let mut found = None;
        for sibling in v.next_siblings().filter_map(ElementRef::wrap) {
            if sibling.value().name() == "table" {
                found = Some(sibling);
                break;
            }
            if sibling.value().classes().any(|c| c == "divVar")
                || matches!(sibling.value().name(), "h1" | "h2" | "h3")
            {
                break;
            }
        }
        let table = found.context("variant rating table missing before next source section")?;
        let mut records = vec![];
        for tr in table.select(&sel("tr")).skip(1) {
            let cells = common::cells(tr);
            if cells.len() != 3 {
                bail!("unmapped ACR rating row shape");
            }
            let procedure = text_of(&cells[0]);
            let category = text_of(&cells[1]);
            let radiation = text_of(&cells[2]);
            if procedure.is_empty() {
                continue;
            }
            records.push(json!({"procedure":procedure,"appropriateness_category":category,"relative_radiation_level":radiation}));
        }
        if records.is_empty() {
            bail!("empty ACR variant rating table");
        }
        let text = records
            .iter()
            .map(|r| {
                format!(
                    "{} | {} | {}",
                    r["procedure"].as_str().unwrap_or(""),
                    r["appropriateness_category"].as_str().unwrap_or(""),
                    r["relative_radiation_level"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        blocks.push(Block {
            block_id: format!("acr-variant-ratings-{}", i + 1),
            kind: BlockKind::Paragraph,
            text,
            heading_path: vec![title.clone(), scenario.clone()],
            page: None,
        });
        mappings.push(json!({"scenario":scenario,"ratings":records,"source_table_html":table.html(),"source_locator":format!("#maindiv / variant {}",i+1)}));
    }
    warnings.push("Variant-specific procedure, appropriateness and relative radiation categories are retained together. Public TinyMCE data-origtext discussion fragments are materialized in source DOM order; iframe copies are skipped. Evidence tables, appendix, patient summary and rights are independent; no grading system is inferred.".into());
    Ok((
        title,
        hash,
        blocks,
        json!({"variant_ratings":mappings,"discussion_materialized_count":discussion_sources.len(),"discussion_sources":discussion_sources,"native_tables":native_tables}),
        warnings,
    ))
}
fn appendix(html: &str, base: &Url) -> Result<(Vec<Block>, Value, Vec<String>)> {
    let d = Html::parse_document(html);
    let root = d
        .select(&sel("div.content"))
        .next()
        .context("ACR ratings appendix content missing")?;
    let headings = root
        .select(&sel("h5"))
        .map(|e| text_of(&e))
        .filter(|t| t.starts_with("Variant "))
        .collect::<Vec<_>>();
    let tables = root
        .select(&sel("table.appendexTable.datatable-columns"))
        .collect::<Vec<_>>();
    if headings.is_empty() || headings.len() != tables.len() {
        bail!("ACR appendix variant/table association unrecognised");
    }
    let raw = root.html();
    let mappings=headings.iter().zip(tables.iter()).enumerate().map(|(i,(h,t))|json!({"scenario":h,"source_table_html":t.html(),"source_locator":format!("ratings-appendix/variant/{}",i+1)})).collect::<Vec<_>>();
    let (blocks, mut warnings) = blocks_from_html(&raw, Some(base), Some("div.content"));
    warnings.push("Source variant tables and nested tabulations retain original HTML; merged-cell geometry and source table associations require review before interpreting individual ratings.".into());
    Ok((
        blocks,
        json!({"source_appendix_html":raw,"variant_tables":mappings}),
        warnings,
    ))
}
fn pdf_complete(f: &Fetched) -> bool {
    f.is_pdf()
        && f.body
            .iter()
            .rev()
            .take(2048)
            .copied()
            .collect::<Vec<_>>()
            .windows(5)
            .any(|x| x == b"FOE%%")
}
fn unresolved(src: &SourceEntry, url: String, error: String) -> PlanRow {
    PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("{}-unresolved", common::short_hash(&url)),
        url,
        title: None,
        rendition: None,
        version: None,
        extra: json!({"discovery_error":error}),
    }
}
// These are independently observed official preview expressions, not catalog links.
// Keep the exact source spellings and provenance; never derive them from evidence-table IDs.
#[derive(Clone, Debug)]
struct VerifiedPreview {
    url: String,
    source_evidence_url: String,
    provenance_class: String,
}
fn verified_preview(value: &Value) -> Result<VerifiedPreview> {
    let object = value
        .as_object()
        .context("ACR preview seed must be an object")?;
    if object.len() != 3
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "url" | "source_evidence_url" | "provenance_class"
            )
        })
    {
        bail!("ACR preview seed requires exactly URL, evidence URL and provenance class");
    }
    let url = value["url"].as_str().context("ACR preview URL missing")?;
    let evidence = value["source_evidence_url"]
        .as_str()
        .context("ACR preview evidence URL missing")?;
    let provenance = value["provenance_class"]
        .as_str()
        .context("ACR preview provenance class missing")?;
    for raw in [url, evidence] {
        let parsed = Url::parse(raw).context("ACR preview URL invalid")?;
        let parts = parsed
            .path()
            .trim_end_matches('/')
            .split('/')
            .collect::<Vec<_>>();
        if parsed.scheme() != "https"
            || parsed.host_str() != Some("acsearch.acr.org")
            || parsed.port().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.as_str() != raw
            || raw.contains('%')
            || raw.contains('\\')
            || parts.len() != 4
            || parts[0] != ""
            || parts[1] != "docs"
            || parts[2].is_empty()
            || !parts[2].bytes().all(|byte| byte.is_ascii_digit())
            || parts[3] != "Narrative"
        {
            bail!("ACR preview URL is not an exact safe official Narrative URL");
        }
    }
    let (expected_evidence, expected_provenance) = match url {
        "https://acsearch.acr.org/docs/69416/Narrative/" => (
            url,
            "independent_search_indexed_official_preview_not_catalog_href",
        ),
        "https://acsearch.acr.org/docs/69404/Narrative/" => (
            "https://acsearch.acr.org/docs/69416/Narrative/",
            "source_printed_plaintext_reference_not_anchor",
        ),
        "https://acsearch.acr.org/docs/3196640/Narrative" => (
            url,
            "independent_search_indexed_official_preview_not_catalog_href",
        ),
        _ => bail!("ACR preview seed is not one of the independently verified expressions"),
    };
    if evidence != expected_evidence || provenance != expected_provenance {
        bail!("ACR preview source evidence or provenance class does not match its verified observation");
    }
    Ok(VerifiedPreview {
        url: url.into(),
        source_evidence_url: evidence.into(),
        provenance_class: provenance.into(),
    })
}
fn preview_config(src: &SourceEntry) -> Result<(Vec<VerifiedPreview>, bool)> {
    let only = match src.config.get("verified_previews_only") {
        None => false,
        Some(value) => value
            .as_bool()
            .context("ACR verified_previews_only must be boolean")?,
    };
    let Some(value) = src.config.get("verified_public_previews") else {
        if only {
            bail!("ACR preview-only mode requires explicit verified previews");
        }
        return Ok((vec![], false));
    };
    let seeds = value
        .as_array()
        .context("ACR verified_public_previews must be an array")?;
    if seeds.is_empty() || seeds.len() > 8 {
        bail!("ACR explicit preview count must be between 1 and 8");
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    for value in seeds {
        let seed = verified_preview(value)?;
        if !seen.insert(seed.url.trim_end_matches('/').to_string()) {
            bail!("ACR duplicate verified preview work");
        }
        out.push(seed);
    }
    Ok((out, only))
}
fn preview_provenance(seed: &VerifiedPreview) -> Value {
    json!({"url":seed.url,"source_evidence_url":seed.source_evidence_url,"provenance_class":seed.provenance_class})
}
fn preview_plan(src: &SourceEntry, seed: &VerifiedPreview, page: &Fetched) -> Result<PlanRow> {
    if page.record.status != 200
        || page.record.final_url != seed.url
        || page.is_pdf()
        || !page.is_html()
    {
        bail!("ACR verified preview did not return its exact HTML expression");
    }
    let (title, clinical_hash, _, _, _) =
        narrative(&page.text(), &Url::parse(&page.record.final_url)?)?;
    let key = common::short_hash(&seed.url);
    Ok(PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("{key}-{}", &page.record.sha256[..12]),
        url: seed.url.clone(),
        title: Some(title),
        rendition: Some("html".into()),
        version: None,
        extra: json!({"work_key":key,"source_role":"narrative","sha256":page.record.sha256,"clinical_root_sha256":clinical_hash,"verified_public_preview":preview_provenance(seed),"scope_complete":false,"catalog_preview_href_verified":false,"source_version_verified":false}),
    })
}
fn unresolved_preview(src: &SourceEntry, seed: &VerifiedPreview, error: String) -> PlanRow {
    let mut row = unresolved(src, seed.url.clone(), error);
    row.extra["source_role"] = json!("narrative");
    row.extra["verified_public_preview"] = preview_provenance(seed);
    row.extra["scope_complete"] = json!(false);
    row.extra["catalog_preview_href_verified"] = json!(false);
    row
}

#[async_trait::async_trait]
impl Adapter for AcrRadiology {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let (seeds, previews_only) = preview_config(src)?;
        let mut rows = vec![];
        let mut preview_works = HashSet::new();
        for seed in &seeds {
            preview_works.insert(seed.url.trim_end_matches('/').to_string());
            let planned = match f.get_ok(&seed.url).await {
                Ok(page) => preview_plan(src, seed, &page),
                Err(error) => Err(error),
            };
            rows.push(match planned {
                Ok(row) => row,
                Err(error) => unresolved_preview(src, seed, format!("{error:#}")),
            });
            // Failures consume the bounded sample too; never hide a failed explicit seed.
            if limit.is_some_and(|n| rows.len() >= n) {
                return Ok(rows);
            }
        }
        if previews_only {
            return Ok(rows);
        }
        let catalog_result = async {
            let cat = f
                .get_ok(
                    src.catalog_url
                        .as_deref()
                        .context("ACR criteria catalog missing")?,
                )
                .await?;
            let base = Url::parse(&cat.record.final_url)?;
            let links = catalog(&cat.text(), &base)?;
            Ok::<_, anyhow::Error>((cat, links))
        }
        .await;
        let (cat, links) = match catalog_result {
            Ok(result) => result,
            Err(error) if seeds.is_empty() => return Err(error),
            Err(error) => {
                let mut row = unresolved(
                    src,
                    src.catalog_url.clone().unwrap_or_default(),
                    format!("Independent catalog unresolved: {error:#}"),
                );
                row.extra["scope_complete"] = json!(false);
                rows.push(row);
                return Ok(rows);
            }
        };
        let mut deferred = vec![];
        for (url, title, role) in links {
            // Deduplicate only the identical official expression/work, never a guessed
            // JACR-to-preview relation or a different evidence/appendix asset role.
            if role == "narrative" && preview_works.contains(url.trim_end_matches('/')) {
                continue;
            }
            let page = match f.get_ok(&url).await {
                Ok(x) => x,
                Err(e) => {
                    deferred.push(unresolved(src, url, format!("{e:#}")));
                    continue;
                }
            };
            let ok = if page.is_pdf() {
                pdf_complete(&page)
            } else if role == "ratings_appendix" {
                appendix(&page.text(), &Url::parse(&page.record.final_url)?).is_ok()
            } else {
                narrative(&page.text(), &Url::parse(&page.record.final_url)?).is_ok()
            };
            if !ok {
                deferred.push(unresolved(
                    src,
                    url,
                    "unmapped source asset type or incomplete content".into(),
                ));
                continue;
            }
            let key = common::short_hash(&url);
            rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-{}",&page.record.sha256[..12]),url,title:Some(title),rendition:Some((if page.is_pdf(){"pdf"}else{"html"}).into()),version:None,extra:json!({"work_key":key,"source_role":role,"sha256":page.record.sha256,"catalog_fetch":cat.record})});
            if limit.is_some_and(|n| rows.len() >= n) {
                return Ok(rows);
            }
        }
        let rem = limit
            .map(|n| n.saturating_sub(rows.len()))
            .unwrap_or(usize::MAX);
        rows.extend(deferred.into_iter().take(rem));
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(e) = row.extra["discovery_error"].as_str() {
            bail!("ACR criteria source unresolved: {e}");
        }
        let data = f.get_ok(&row.url).await?;
        if row.extra["sha256"]
            .as_str()
            .is_some_and(|h| h != data.record.sha256)
        {
            bail!("ACR criteria asset changed since discovery");
        }
        let role = row.extra["source_role"].as_str().unwrap_or("source_asset");
        let preview = if let Some(value) = row.extra.get("verified_public_preview") {
            let seed = verified_preview(value)?;
            if row.url != seed.url || role != "narrative" || row.version.is_some() {
                bail!("ACR verified preview row identity/role/version invalid");
            }
            let current = preview_plan(src, &seed, &data)?;
            if row.extra["clinical_root_sha256"].as_str()
                != current.extra["clinical_root_sha256"].as_str()
                || !row.extra["sha256"].is_string()
            {
                bail!("ACR preview clinical root changed since discovery");
            }
            Some(seed)
        } else {
            None
        };
        let (blocks, mut warnings, mut metadata) = if data.is_pdf() {
            if !pdf_complete(&data) {
                bail!("ACR PDF incomplete");
            }
            let (b, w, _) = super::static_list::blocks_for(&data, None, 0).await?;
            (b, w, json!({}))
        } else if role == "ratings_appendix" {
            let (b, m, w) = appendix(&data.text(), &Url::parse(&data.record.final_url)?)?;
            (b, w, m)
        } else {
            let (_, _, b, m, w) = narrative(&data.text(), &Url::parse(&data.record.final_url)?)?;
            (b, w, m)
        };
        if blocks.is_empty() {
            bail!("ACR criteria asset has no content");
        }
        warnings.push("Source-labelled evidence, ratings, narrative and patient summaries are distinct; generic PDF table geometry, superscripts and asset rights remain unverified.".into());
        if let Some(seed) = preview {
            metadata["verified_public_preview"] = preview_provenance(&seed);
            metadata["scope_complete"] = json!(false);
            metadata["catalog_preview_href_verified"] = json!(false);
            metadata["source_version_verified"] = json!(false);
            metadata["clinical_root_sha256"] = row.extra["clinical_root_sha256"].clone();
            warnings.push("Independently verified official preview seed, not an enumerated catalog href or publisher fallback. Version/currentness and full catalog/asset coverage remain unverified; appropriateness categories are not recommendation strength.".into());
        }
        let mut fetches = vec![];
        if let Ok(r) = serde_json::from_value::<crate::guidelines::doc::FetchRecord>(
            row.extra["catalog_fetch"].clone(),
        ) {
            fetches.push(r)
        }
        fetches.push(data.record);
        Ok(GuidelineDoc {
            source_id: src.id.clone(),
            doc_key: row.doc_key.clone(),
            work_key: row.extra["work_key"].as_str().map(String::from),
            title: row.title.clone().unwrap_or_default(),
            url: row.url.clone(),
            item_type: Some(role.into()),
            status: "unknown".into(),
            rights: Rights {
                status: "unknown".into(),
                note: Some(
                    "Each source asset and publisher has independently unverified rights.".into(),
                ),
                ..Default::default()
            },
            search_metadata: metadata,
            blocks,
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
    fn source_owned_terminal_inline_preserves_dom_boundaries_and_original_fragment() {
        let fragment = "<sup>a</sup>Unit <span title=\"scope\">A&amp;B</span>. <sup>b</sup>Second";
        let encoded = fragment
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        let html = format!(
            r#"<div id="maindiv"><div class="displayDoc" data-val-itemid="0"><div id="terminal" class="txtSectiontextReadOnly" data-origtext="{encoded}">Old duplicate</div><iframe>Old duplicate</iframe></div></div>"#
        );
        let parsed = Html::parse_document(&html);
        let (materialized, sources) =
            materialized_narrative(parsed.select(&sel("#maindiv")).next().unwrap()).unwrap();
        let (blocks, _) = blocks_from_html(&materialized, None, Some("#maindiv"));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_id, "terminal");
        assert_eq!(blocks[0].text, "aUnit A&B. bSecond");
        let provenance = &sources[0]["inline_text_materialization"];
        assert_eq!(provenance["decoded_source_html"], fragment);
        assert_eq!(
            provenance["normalized_DOM_text_sha256"],
            crate::common::sha256_hex(blocks[0].text.as_bytes())
        );
        assert_eq!(sources[0]["iframe_content_duplicated"], false);
    }

    #[test]
    fn terminal_inline_nested_joins_entities_and_real_whitespace_are_preserved() {
        assert_eq!(
            terminal_inline_text("H<span><sup>2</sup>O</span> &amp; X<span>Y</span>Z").unwrap(),
            "H2O & XYZ"
        );
        assert_eq!(
            terminal_inline_text("  A <span> B </span> C\n<sup>2</sup> risk &lt; 3").unwrap(),
            "A B C 2 risk < 3"
        );
        assert_ne!(
            terminal_inline_text("A<span>B</span>").unwrap(),
            terminal_inline_text("A <span>B</span>").unwrap()
        );
    }

    #[test]
    fn terminal_inline_unsupported_visibility_structure_and_limits_fail_closed() {
        for fragment in [
            "A<p>B</p>",
            "A<img src='x'>B",
            "A<span hidden>B</span>",
            "A<span style='display:none'>B</span>",
            "A<script>B</script>",
            "A<!-- boundary -->B",
            "<span class='unknown'>A</span>",
        ] {
            assert!(
                terminal_inline_text(fragment).is_err(),
                "unsupported terminal shape accepted"
            );
        }
        assert!(terminal_inline_text(&"x".repeat(65_537)).is_err());
        assert!(terminal_inline_text(&"<span>x</span>".repeat(129)).is_err());
        assert!(
            terminal_inline_text(&format!("{}x{}", "<span>".repeat(9), "</span>".repeat(9)))
                .is_err()
        );
    }

    #[test]
    fn terminal_inline_correction_requires_native_item_zero_ownership() {
        let html = r#"<div id="maindiv"><div class="displayDoc" data-val-itemid="1"><div class="txtSectiontextReadOnly" data-origtext="A&lt;sup&gt;2&lt;/sup&gt;B"></div></div><div class="author-content"><div class="displayDoc" data-val-itemid="0"><div class="txtSectiontextReadOnly" data-origtext="C&lt;sup&gt;3&lt;/sup&gt;D"></div></div></div></div>"#;
        let parsed = Html::parse_document(html);
        let (_, sources) =
            materialized_narrative(parsed.select(&sel("#maindiv")).next().unwrap()).unwrap();
        assert!(sources
            .iter()
            .all(|s| s["inline_text_materialization"].is_null()));
    }

    #[test]
    fn native_geometry_keeps_spanning_caveat_and_table_block_identity_without_inferred_headers() {
        let html = r#"<h1>Synthetic</h1><div id="maindiv"><div class="divVar">Variant 1. Synthetic scenario.</div><table id="tblRatingTable"><tr><td>Procedure</td><td>Category</td><td>Radiation</td></tr><tr><td>Scan A</td><td>Usually Appropriate</td><td>None</td></tr></table><table id="legend"><tr><td colspan="3">Native legend title</td></tr><tr><td rowspan="2">Shared label</td><td>Adult</td><td>Child</td></tr><tr><td>Low</td><td>Lower</td></tr><tr><td colspan="3">* Native spanning caveat.</td></tr></table><p id="separate">Independent adjacent qualification.</p></div>"#;
        let parsed = Html::parse_document(html);
        let main = parsed.select(&sel("#maindiv")).next().unwrap();
        let (materialized, _) = materialized_narrative(main).unwrap();
        let (original_blocks, _) = blocks_from_html(&materialized, None, Some("#maindiv"));
        let (_, _, blocks, metadata, _) = narrative(
            html,
            &Url::parse("https://acsearch.acr.org/docs/1/Narrative/").unwrap(),
        )
        .unwrap();
        let original_tables = original_blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
            .map(|b| (&b.block_id, &b.text))
            .collect::<Vec<_>>();
        let retained_tables = blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
            .map(|b| (&b.block_id, &b.text))
            .collect::<Vec<_>>();
        assert_eq!(original_tables, retained_tables);
        let tables = metadata["native_tables"].as_array().unwrap();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[1]["canonical_block_id"], "legend");
        assert_eq!(tables[1]["rows"][0]["cells"][0]["colspan"], "3");
        assert_eq!(tables[1]["rows"][1]["cells"][0]["rowspan"], "2");
        assert_eq!(
            tables[1]["rows"][3]["cells"][0]["text"],
            "* Native spanning caveat."
        );
        assert_eq!(tables[1]["rows"][0]["cells"][0]["native_tag"], "td");
        assert_eq!(tables[1]["header_role_inferred"], false);
        assert_eq!(tables[1]["external_footnote_ownership_inferred"], false);
        assert_eq!(
            tables[1]["source_table_html_format"],
            "html5_DOM_serialization_not_original_response_bytes"
        );
        assert!(blocks
            .iter()
            .any(|b| b.block_id == "separate" && b.text == "Independent adjacent qualification."));
        assert!(!blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Recommendation { .. })));
    }

    #[test]
    fn native_geometry_rejects_unmapped_or_nested_tables() {
        assert!(narrative_table_geometry(
            "<div id='maindiv'><table><tr><td>A</td></tr></table></div>",
            &[]
        )
        .is_err());
        let html = "<div id='maindiv'><table><tr><td><table><tr><td>A</td></tr></table></td></tr></table></div>";
        let (blocks, _) = blocks_from_html(html, None, Some("#maindiv"));
        assert!(narrative_table_geometry(html, &blocks).is_err());
    }

    #[test]
    fn genuine_catalog_and_headache_variants_keep_procedure_category_radiation() {
        let catalog_html = [
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/catalog-1.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/catalog-2.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/catalog-3.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/catalog-4.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/catalog-5.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/catalog-6.html"
            )),
        ]
        .concat();
        let base = Url::parse("https://acsearch.acr.org/list").unwrap();
        let rows = catalog(&catalog_html, &base).unwrap();
        assert!(rows.iter().any(|(u, t, r)| t.contains("Headache")
            && r == "ratings_appendix"
            && u.contains("TopicId=140")));
        assert!(rows.iter().any(|(u, t, r)| t.contains("Headache")
            && r == "evidence_table"
            && u.contains("TopicId=140")));
        let html = [
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/headache-1.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/headache-2.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/headache-3.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acr_radiology/headache-4.html"
            )),
        ]
        .concat();
        let url = Url::parse("https://acsearch.acr.org/docs/69482/Narrative").unwrap();
        let (title, _, blocks, metadata, _) = narrative(&html, &url).unwrap();
        let maps = metadata["variant_ratings"].as_array().unwrap();
        assert_eq!(title, "Headache");
        assert_eq!(maps.len(), 8);
        let first = &maps[0];
        assert!(first["scenario"]
            .as_str()
            .unwrap()
            .contains("Sudden onset severe headache"));
        assert!(first["ratings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["procedure"] == "CT head without IV contrast"
                && r["appropriateness_category"] == "Usually Appropriate"));
        assert!(blocks.iter().any(|b| b
            .heading_path
            .iter()
            .any(|s| s.contains("Sudden onset severe headache"))
            && b.text.contains("Usually Appropriate")));
        assert!(html.contains("/list/TopicNarrativePdf?topicId=140"));
        assert!(narrative("<html/>", &url).is_err());
        assert_eq!(metadata["discussion_materialized_count"], 134);
        assert_eq!(
            metadata["discussion_sources"][1]["author_content_id"],
            " Sec_10808"
        );
        let intro = blocks
            .iter()
            .position(|block| {
                block
                    .text
                    .starts_with("Headache is an ancient problem plaguing")
            })
            .unwrap();
        let discussion = blocks
            .iter()
            .position(|block| {
                block
                    .text
                    .starts_with("Many of the headaches presenting to the emergency department")
            })
            .unwrap();
        let arteriography=blocks.iter().position(|block|block.text.starts_with("There is no relevant literature to support the use of cervicocerebral arteriography")).unwrap();
        assert!(intro < discussion && discussion < arteriography);
        assert!(matches!(blocks[intro].kind, BlockKind::Paragraph));
        assert!(blocks[intro].text.contains("[1,2]"));
        assert!(blocks[intro]
            .heading_path
            .iter()
            .any(|heading| heading == "Introduction/Background"));
        assert!(blocks[discussion]
            .heading_path
            .iter()
            .any(|heading| heading == "Discussion of Procedures by Variant"));
        assert!(blocks[arteriography]
            .heading_path
            .iter()
            .any(|heading| heading.contains("Arteriography cervicocerebral")));
        assert_eq!(
            blocks
                .iter()
                .filter(|block| matches!(block.kind, BlockKind::Table { .. })
                    && block.block_id.starts_with("tblRatingTable"))
                .count(),
            8
        );
        assert_eq!(
            blocks
                .iter()
                .map(|block| &block.block_id)
                .collect::<HashSet<_>>()
                .len(),
            blocks.len()
        );
        assert!(!blocks
            .iter()
            .any(|block| matches!(block.kind, BlockKind::Recommendation { .. })));
    }
    #[test]
    fn public_backing_html_replaces_rendered_copy_and_preserves_literal_ids() {
        let html = r##"<div id="maindiv"><div class="author-content" id=" Sec_10808"><div class="txtSectiontextReadOnly" id=" backing" aria-hidden="true" data-origtext="&lt;p id=&quot; statement&quot;&gt;Public discussion &amp;amp; citation [1].&lt;/p&gt;"><p>Public discussion &amp; citation [1].</p></div><iframe id="txtTiny_123_ifr" srcdoc="&lt;p&gt;Public discussion &amp;amp; citation [1].&lt;/p&gt;"></iframe></div></div>"##;
        let document = Html::parse_document(html);
        let main = document.select(&sel("#maindiv")).next().unwrap();
        let (materialized, sources) = materialized_narrative(main).unwrap();
        assert!(materialized.contains("id=\" Sec_10808\""));
        assert!(materialized.contains("id=\" backing\""));
        assert_eq!(sources[0]["author_content_id"], " Sec_10808");
        let (blocks, _) = blocks_from_html(
            &materialized,
            Some(&Url::parse("https://acsearch.acr.org/docs/69482/Narrative/").unwrap()),
            Some("#maindiv"),
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_id, " statement");
        assert_eq!(blocks[0].text, "Public discussion & citation [1].");
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod acquired_asset_test {
    use super::*;
    const E: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acr_radiology/headache-evidence-unavailable.html"
    ));
    const A: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acr_radiology/headache-appendix-1.html"
    ));
    const B: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acr_radiology/headache-appendix-2.html"
    ));
    #[test]
    fn actual_evidence_is_unavailable_and_appendix_not_misread_as_narrative() {
        let url = Url::parse("https://acsearch.acr.org/docs/69482/EvidenceTable/").unwrap();
        assert!(E.contains("evidence table for this topic is currently unavailable"));
        assert!(narrative(E, &url).is_err());
        let html = format!("{A}{B}");
        assert!(html.contains("Sudden onset severe headache"));
        assert!(html.contains("appendexTable"));
        assert!(narrative(&html, &url).is_err());
        let (_, m, _) = appendix(&html, &url).unwrap();
        let variants = m["variant_tables"].as_array().unwrap();
        assert_eq!(variants.len(), 8);
        assert!(variants[0]["scenario"]
            .as_str()
            .unwrap()
            .contains("Sudden onset severe headache"));
        assert!(variants[0]["source_table_html"]
            .as_str()
            .unwrap()
            .contains("Final Tabulations"));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod verified_preview_tests {
    use super::*;
    const DVT: &str = "https://acsearch.acr.org/docs/69416/Narrative/";
    const PE: &str = "https://acsearch.acr.org/docs/69404/Narrative/";
    const VISION: &str = "https://acsearch.acr.org/docs/3196640/Narrative";
    const CATALOG: &str = "https://acsearch.acr.org/list";
    const PAGE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acr_radiology/verified-preview-synthetic.html"
    ));
    fn seed(url: &str) -> Value {
        json!({"url":url,"source_evidence_url":if url==PE {DVT} else {url},"provenance_class":if url==PE {"source_printed_plaintext_reference_not_anchor"} else {"independent_search_indexed_official_preview_not_catalog_href"}})
    }
    fn source(config: Value) -> SourceEntry {
        serde_json::from_value(json!({"id":"acr_radiology","enabled":true,"access":"public","catalog_url":CATALOG,"config":config})).unwrap()
    }
    fn catalog_page(urls: &[&str]) -> String {
        urls.iter().enumerate().map(|(i,url)|format!(r#"<div class="row divOdd"><div class="col-lg-4">Synthetic topic {i}</div><a href="{url}">Narrative</a></div>"#)).collect::<Vec<_>>().join("")
    }
    #[test]
    fn opt_in_preview_config_preserves_three_exact_provenance_classes_and_no_default_seeds() {
        assert!(preview_config(&source(json!({}))).unwrap().0.is_empty());
        let (seeds, only) = preview_config(&source(
            json!({"verified_public_previews":[seed(DVT),seed(PE),seed(VISION)]}),
        ))
        .unwrap();
        assert!(!only);
        assert_eq!(seeds.len(), 3);
        assert_eq!(seeds[1].source_evidence_url, DVT);
        assert_eq!(
            seeds[1].provenance_class,
            "source_printed_plaintext_reference_not_anchor"
        );
        assert_eq!(seeds[2].url, VISION);
    }
    #[test]
    fn preview_config_rejects_unsafe_unknown_duplicate_and_mismatched_provenance() {
        for url in [
            "http://acsearch.acr.org/docs/69416/Narrative/",
            "https://other.example/docs/69416/Narrative/",
            "https://acsearch.acr.org:443/docs/69416/Narrative/",
            "https://user@acsearch.acr.org/docs/69416/Narrative/",
            "https://acsearch.acr.org/docs/69416/Narrative/?x=1",
            "https://acsearch.acr.org/docs/69416/Narrative/#body",
            "https://acsearch.acr.org/docs/x/../69416/Narrative/",
            "https://acsearch.acr.org/docs/%36%39%34%31%36/Narrative/",
            "https://acsearch.acr.org/docs/69416/EvidenceTable/",
            "https://acsearch.acr.org/docs/999999/Narrative/",
            "https://acsearch.acr.org/docs/69416/Narrative",
        ] {
            assert!(
                verified_preview(&seed(url)).is_err(),
                "accepted unsafe/unknown URL {url}"
            );
        }
        for config in [
            json!({"verified_public_previews":[seed(DVT),seed(DVT)]}),
            json!({"verified_public_previews":[]}),
            json!({"verified_public_previews":vec![seed(DVT);9]}),
            json!({"verified_public_previews":{}}),
            json!({"verified_previews_only":true}),
            json!({"verified_previews_only":"true","verified_public_previews":[seed(DVT)]}),
        ] {
            assert!(preview_config(&source(config)).is_err());
        }
        for (key, value) in [
            ("source_evidence_url", json!(PE)),
            ("provenance_class", json!("catalog_href")),
            ("unexpected_version", json!("2018")),
        ] {
            let mut invalid = seed(DVT);
            invalid[key] = value;
            assert!(verified_preview(&invalid).is_err());
        }
        let mut invalid = seed(PE);
        invalid["source_evidence_url"] = json!(PE);
        assert!(verified_preview(&invalid).is_err());
    }
    #[tokio::test]
    async fn genuine_preview_plan_extract_preserves_materialized_context_and_detects_drift() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(DVT, PAGE.as_bytes(), "text/html");
        let src =
            source(json!({"verified_public_previews":[seed(DVT)],"verified_previews_only":true}));
        let rows = AcrRadiology.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url, DVT);
        assert_eq!(rows[0].version, None);
        assert_eq!(rows[0].extra["source_role"], "narrative");
        assert_eq!(rows[0].extra["scope_complete"], false);
        assert!(rows[0].extra.get("catalog_fetch").is_none());
        let doc = AcrRadiology.extract(&src, &f, &rows[0]).await.unwrap();
        assert_eq!(doc.title, "Synthetic ACR preview");
        assert_eq!(doc.status, "unknown");
        assert_eq!(doc.version, None);
        assert_eq!(doc.fetches.len(), 1);
        assert_eq!(
            doc.search_metadata["variant_ratings"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            doc.search_metadata["variant_ratings"][1]["ratings"][0]["procedure"],
            "Study Beta"
        );
        assert_eq!(
            doc.search_metadata["variant_ratings"][1]["ratings"][0]["appropriateness_category"],
            "May Be Appropriate"
        );
        assert_eq!(doc.search_metadata["discussion_materialized_count"], 2);
        assert_eq!(
            doc.search_metadata["discussion_sources"][0]["author_content_id"],
            " Sec_synthetic"
        );
        assert_eq!(
            doc.search_metadata["verified_public_preview"]["source_evidence_url"],
            DVT
        );
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
                .count(),
            2
        );
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| b.text == "Synthetic first qualification [1].")
                .count(),
            1
        );
        let first = doc
            .blocks
            .iter()
            .position(|b| b.text == "Synthetic first qualification [1].")
            .unwrap();
        let second = doc
            .blocks
            .iter()
            .position(|b| b.text == "Synthetic second qualification & context [2].")
            .unwrap();
        assert!(first < second);
        assert!(!doc
            .blocks
            .iter()
            .any(|b| b.text.contains("Unrelated footer") || b.text.contains("Navigation only")));
        assert!(!doc
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Recommendation { .. })));
        assert_eq!(
            doc.blocks
                .iter()
                .map(|block| &block.block_id)
                .collect::<HashSet<_>>()
                .len(),
            doc.blocks.len()
        );
        let changed = PAGE.replace(
            "Synthetic first qualification",
            "Changed first qualification",
        );
        f.seed_cache_for_tests(DVT, changed.as_bytes(), "text/html");
        assert!(AcrRadiology.extract(&src, &f, &rows[0]).await.is_err());
        let mut root_guard_row = rows[0].clone();
        root_guard_row.extra["sha256"] = json!(crate::common::sha256_hex(changed.as_bytes()));
        assert!(AcrRadiology
            .extract(&src, &f, &root_guard_row)
            .await
            .is_err());
    }
    #[tokio::test]
    async fn supplement_seeds_precede_catalog_and_only_identical_preview_works_deduplicate() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        let normal = "https://acsearch.acr.org/docs/999/Narrative";
        f.seed_cache_for_tests(DVT, PAGE.as_bytes(), "text/html");
        f.seed_cache_for_tests(normal, PAGE.as_bytes(), "text/html");
        f.seed_cache_for_tests(
            CATALOG,
            catalog_page(&[DVT, normal]).as_bytes(),
            "text/html",
        );
        let src = source(json!({"verified_public_previews":[seed(DVT)]}));
        let rows = AcrRadiology.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].url, DVT);
        assert_eq!(rows[1].url, normal);
        assert!(rows[1].extra.get("verified_public_preview").is_none());
        assert!(rows[1].extra["catalog_fetch"].is_object());
        assert_eq!(
            AcrRadiology
                .discover(&src, &f, Some(1))
                .await
                .unwrap()
                .len(),
            1
        );
        let ordinary = AcrRadiology
            .discover(&source(json!({})), &f, None)
            .await
            .unwrap();
        assert_eq!(ordinary.len(), 2);
        assert!(ordinary
            .iter()
            .all(|row| row.extra.get("verified_public_preview").is_none()));
        assert!(AcrRadiology
            .discover(&src, &f, Some(0))
            .await
            .unwrap()
            .is_empty());
    }
    #[tokio::test]
    async fn missing_gated_seeds_are_real_failures_and_independent_catalog_is_not_fabricated() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(
            DVT,
            b"<html><h1>Sign in</h1><form><input type='password'></form></html>",
            "text/html",
        );
        let mut src = source(
            json!({"verified_public_previews":[seed(DVT),seed(PE)],"verified_previews_only":true}),
        );
        let rows = AcrRadiology.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|row| row.extra["discovery_error"].is_string()));
        assert_eq!(
            rows[1].extra["verified_public_preview"]["source_evidence_url"],
            DVT
        );
        assert!(AcrRadiology.extract(&src, &f, &rows[0]).await.is_err());
        src.config["verified_previews_only"] = json!(false);
        let supplemented = AcrRadiology.discover(&src, &f, None).await.unwrap();
        assert_eq!(supplemented.len(), 3);
        assert_eq!(supplemented[2].url, CATALOG);
        assert!(supplemented[2].extra["discovery_error"]
            .as_str()
            .unwrap()
            .contains("Independent catalog unresolved"));
        assert!(AcrRadiology
            .discover(&source(json!({})), &f, None)
            .await
            .is_err());
    }
    #[tokio::test]
    async fn preview_transport_rejects_shells_without_primary_root_and_false_mime() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        let src =
            source(json!({"verified_public_previews":[seed(DVT)],"verified_previews_only":true}));
        f.seed_cache_for_tests(DVT, PAGE.as_bytes(), "text/html");
        let mut redirected = f.get_ok(DVT).await.unwrap();
        redirected.record.final_url = "https://www.jacr.org/unrelated".into();
        assert!(preview_plan(&src, &verified_preview(&seed(DVT)).unwrap(), &redirected).is_err());
        f.seed_cache_for_tests(
            DVT,
            b"<html><h1>Preview title</h1><p>Loading</p></html>",
            "text/html",
        );
        let rows = AcrRadiology.discover(&src, &f, None).await.unwrap();
        assert!(rows[0].extra["discovery_error"].is_string());
        f.seed_cache_for_tests(DVT, b"{}", "application/json");
        let rows = AcrRadiology.discover(&src, &f, None).await.unwrap();
        assert!(rows[0].extra["discovery_error"].is_string());
    }
}
