//! Bounded society-linked public Springer HTML and distinct native PDF pages.
//! Catalog failure never implies complete discovery; PDF table geometry stays unresolved.
use super::{common, Adapter, PlanRow};
use super::pmc_crosswalk::{self as cw, Lookup, SourceRef};
use super::r2b_crosswalk::{self as xw, Cross};
use crate::guidelines::{
    doc::{Block, BlockKind, GuidelineDoc, Relation, Rights},
    fetch::{Fetched, Fetcher, Req},
    html::{blocks_from_html, sel, text_of},
    registry::SourceEntry,
};
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::{json, Value};
use std::collections::HashSet;
use url::Url;

pub struct AasmSleep;
const LEAF: &str = "https://link.springer.com/article/10.1007/s44470-025-00038-8";
const BODY: &str = ".c-article-body > div.main-content";
const DOI: &str = "10.1007/s44470-025-00038-8";
const PDF: &str = "https://link.springer.com/content/pdf/10.1007/s44470-025-00038-8.pdf";
const PDF_PROCESSING: &str = "aasm-pdfium-physical-pages-v1";
const PDF_BYTES: usize = 16 * 1024 * 1024;
const PDF_PAGES: usize = 32;
const PAGE_TEXT_BYTES: usize = 1024 * 1024;
const PDF_TEXT_BYTES: usize = 8 * 1024 * 1024;

fn catalog(html: &str, base: &Url) -> Result<Vec<(String, String)>> {
    let d = Html::parse_document(html);
    let mut seen = HashSet::new();
    let mut out = vec![];
    for a in d.select(&sel("a[href]")) {
        let t = text_of(&a);
        let Some(u) = common::link(base, a.value().attr("href").unwrap_or("")) else {
            continue;
        };
        let parsed = Url::parse(&u)?;
        if (t == "Guideline" || t.to_lowercase().contains("clinical practice guideline"))
            && matches!(
                parsed.host_str(),
                Some("link.springer.com" | "pmc.ncbi.nlm.nih.gov" | "doi.org")
            )
            && seen.insert(u.clone())
        {
            out.push((u, t));
        }
    }
    if out.is_empty() {
        bail!("AASM guideline links missing")
    }
    Ok(out)
}

struct Parsed {
    doc: GuidelineDoc,
    tables: Vec<(String, String, String)>, // native anchor, source-linked URL, caption
}
fn content(html: &str, base: &Url) -> Result<Parsed> {
    let d = Html::parse_document(html);
    let title =
        common::first_text(&d, "article#main h1").context("Springer article title missing")?;
    let root = d
        .select(&sel(BODY))
        .next()
        .context("Springer precise clinical body missing")?;
    let text = text_of(&root);
    if !text.contains("Introduction") || root.select(&sel("#Sec5, #Sec7")).count() != 2 {
        bail!("Springer bounded native recommendation structure missing")
    }
    let mut body = root.html();
    let mut tables = vec![];
    for figure in root.select(&sel("figure")) {
        if let Some(a) = figure.select(&sel("a[data-test=table-link][href]")).next() {
            let u = common::link(base, a.value().attr("href").unwrap_or(""))
                .context("invalid linked table")?;
            if ![format!("{LEAF}/tables/1"), format!("{LEAF}/tables/2")].contains(&u) {
                bail!("Unmapped Springer table URL: {u}")
            }
            let caption = figure
                .select(&sel("figcaption"))
                .next()
                .map(|c| text_of(&c))
                .unwrap_or_default();
            let anchor = figure
                .select(&sel("[data-test=table-caption]"))
                .next()
                .and_then(|e| e.value().id())
                .map(String::from)
                .unwrap_or_else(|| format!("Tab{}", tables.len() + 1));
            body = body.replace(
                &figure.html(),
                &format!("<p id=\"{anchor}\">Linked table: {caption}</p>"),
            );
            tables.push((anchor, u, caption));
        }
    }
    let (mut blocks, mut warnings) = blocks_from_html(&body, Some(base), Some("div.main-content"));
    for block in &mut blocks {
        if matches!(block.block_id.as_str(), "Sec5" | "Sec7") {
            let expected = if block.block_id == "Sec5" {
                "Recommendation 1"
            } else {
                "Recommendation 2"
            };
            let label = block
                .text
                .split_once(':')
                .filter(|(label, _)| *label == expected)
                .map(|(label, _)| label.to_string());
            let strength = ["Conditional recommendation", "Strong recommendation"]
                .into_iter()
                .find(|s| block.text.contains(s))
                .map(String::from);
            let certainty = [
                "very low certainty of evidence",
                "low certainty of evidence",
                "moderate certainty of evidence",
                "high certainty of evidence",
            ]
            .into_iter()
            .find(|s| block.text.contains(s))
            .map(String::from);
            if label.is_none() || strength.is_none() || certainty.is_none() {
                bail!(
                    "Native recommendation label/grade missing: {}",
                    block.block_id
                )
            }
            block.kind = BlockKind::Recommendation {
                label,
                strength,
                certainty,
                population: None,
                grading_system: None,
                qualifier: None,
            };
        }
    }
    // Springer renders the implementation remark as a sibling h3. Keep its association
    // with the immediately preceding native recommendation in retrieval context.
    let second_statement = blocks
        .iter()
        .find(|b| b.block_id == "Sec7")
        .map(|b| b.text.clone())
        .context("Recommendation 2 missing")?;
    let remark_title = root.select(&sel("#FPar1")).next().map(|e| text_of(&e));
    for b in &mut blocks {
        if b.block_id == "FPar1"
            || remark_title
                .as_ref()
                .is_some_and(|t| b.heading_path.contains(t))
        {
            let position = b
                .heading_path
                .len()
                .saturating_sub(usize::from(b.block_id != "FPar1"));
            b.heading_path.insert(position, second_statement.clone());
        }
    }
    if let Some(abs) = d.select(&sel("#Abs1-section")).next() {
        let (mut abstract_blocks, w) =
            blocks_from_html(&abs.html(), Some(base), Some("#Abs1-section"));
        warnings.extend(w);
        for b in &mut abstract_blocks {
            b.block_id = format!("abstract-{}", b.block_id);
            b.heading_path.insert(0, "Abstract".into());
        }
        abstract_blocks.extend(blocks);
        blocks = abstract_blocks;
    } else {
        warnings.push("Separate abstract not found".into());
    }
    // Back matter is separate from clinical text, and never used to detect recommendations.
    if let Some(suffix) = d.select(&sel("#MagazineFulltextArticleBodySuffix")).next() {
        let (mut back, w) = blocks_from_html(
            &suffix.html(),
            Some(base),
            Some("#MagazineFulltextArticleBodySuffix"),
        );
        warnings.extend(w);
        for b in &mut back {
            b.block_id = format!("backmatter-{}", b.block_id);
            b.heading_path.insert(0, "Back matter".into());
        }
        blocks.extend(back);
    } else {
        warnings.push("References/back matter container not found".into());
    }
    let meta = |name: &str| {
        d.select(&sel(&format!("meta[name='{name}']")))
            .next()
            .and_then(|e| e.value().attr("content"))
            .map(String::from)
    };
    let mut dates = vec![];
    for name in [
        "citation_online_date",
        "citation_publication_date",
        "citation_cover_date",
        "dc.date",
        "dc.date.issued",
    ] {
        if let Some(raw) = meta(name) {
            dates.push(common::date(name, &raw, base.as_str()));
        }
    }
    for e in d.select(&sel("li:has(time), time[itemprop=datePublished]")) {
        let time = if e.value().name() == "time" {
            Some(e)
        } else {
            e.select(&sel("time")).next()
        };
        let Some(time) = time else { continue };
        let raw = time
            .value()
            .attr("datetime")
            .map(String::from)
            .unwrap_or_else(|| text_of(&time));
        let label = text_of(&e);
        let label =
            if label.contains("Published") || e.value().attr("itemprop") == Some("datePublished") {
                "Published online".to_string()
            } else {
                label
            };
        dates.push(common::date(&label, &raw, base.as_str()));
    }
    let rights_text = common::first_text(&d, "#rightslink-content");
    let rights = if rights_text
        .as_deref()
        .is_some_and(|s| s.contains("Creative Commons Attribution-NonCommercial-NoDerivatives 4.0"))
    {
        Rights {
            status: "restricted".into(),
            license: Some("CC BY-NC-ND 4.0".into()),
            terms_url: Some("http://creativecommons.org/licenses/by-nc-nd/4.0/".into()),
            note: rights_text.clone(),
            attribution: rights_text.clone(),
            restrictions: vec![rights_text.unwrap()],
            evidence_urls: vec![format!("{base}#rightslink-content")],
            ..Default::default()
        }
    } else {
        warnings.push("Published rights notice not mapped".into());
        common::unknown_rights()
    };
    let mut attachments = vec![];
    if let Some(pdf) = meta("citation_pdf_url").and_then(|s| common::link(base, &s)) {
        attachments.push(json!({"url":pdf,"role":"alternative_rendition","rendition":"pdf","rights":"assess independently"}));
    }
    let mut relations = vec![];
    let review_doi = "10.1007/s44470-025-00039-7";
    if d.select(&sel("a[href]")).any(|a| {
        a.value()
            .attr("href")
            .is_some_and(|u| u.contains(review_doi))
    }) {
        relations.push(Relation {
            rel: "related".into(),
            target: format!("https://doi.org/{review_doi}"),
            scope: Some("Companion systematic review; separate work".into()),
            evidence_url: Some(base.to_string()),
        });
    }
    let published = dates
        .iter()
        .find(|date| date.label == "Published online" || date.label == "citation_online_date")
        .and_then(|date| date.normalized.clone());
    let issuer = text
        .contains("American Academy of Sleep Medicine")
        .then(|| "American Academy of Sleep Medicine".into());
    Ok(Parsed {
        doc: GuidelineDoc {
            title,
            url: base.to_string(),
            item_type: Some("clinical_practice_guideline".into()),
            status: "unknown".into(),
            doi: meta("citation_doi"),
            publisher: meta("citation_publisher").or_else(|| meta("dc.publisher")),
            issuers: issuer.into_iter().collect(),
            language: common::page_language(&d),
            published,
            date_observations: dates,
            rights,
            blocks,
            attachments,
            relations,
            warnings,
            ..Default::default()
        },
        tables,
    })
}

fn table_content(html: &str, base: &Url, id: &str, caption: &str) -> Result<(Vec<Block>, Value)> {
    let d = Html::parse_document(html);
    let root = d
        .select(&sel("div.c-article-table-container"))
        .next()
        .context("Springer table container missing")?;
    let table = root
        .select(&sel("table.data"))
        .next()
        .context("Springer native table missing")?;
    let (mut blocks, warnings) = blocks_from_html(
        &root.html(),
        Some(base),
        Some("div.c-article-table-container"),
    );
    if !warnings.is_empty() {
        bail!("Linked table parsing warnings: {warnings:?}")
    }
    let mut n = 0;
    for b in &mut blocks {
        n += 1;
        b.block_id = format!("{id}-detail-{}-{n}", b.block_id);
        if matches!(b.kind, BlockKind::Table { .. }) {
            b.kind = BlockKind::Table {
                caption: Some(caption.into()),
            };
        } else if !matches!(b.kind, BlockKind::Heading { .. }) {
            b.kind = BlockKind::Footnote;
        }
    }
    // The shared block model has no spans. Preserve native table HTML alongside Markdown.
    Ok((
        blocks,
        json!({"url":base.as_str(),"role":"source_linked_table","source_anchor":id,"caption":caption,"table_html":table.html(),"rights":"article notice; inspect credit lines for exceptions"}),
    ))
}
async fn fresh(f: &Fetcher, url: &str) -> Result<Fetched> {
    let mut req = Req::get(url);
    req.cache_max_age_secs = Some(0);
    let page = f.request(&req).await?;
    if !(200..300).contains(&page.record.status) {
        bail!("HTTP {} for {url}", page.record.status)
    }
    if !page.is_html() {
        bail!("Expected public Springer HTML: {url}")
    }
    Ok(page)
}
async fn publisher(f: &Fetcher, url: &str) -> Result<Parsed> {
    let p = fresh(f, url).await?;
    let base = Url::parse(&p.record.final_url)?;
    if base.as_str() != LEAF {
        bail!("Unmapped Springer terminal; no generic fallback: {base}")
    }
    let mut parsed = content(&p.text(), &base)?;
    parsed.doc.fetches.push(p.record);
    Ok(parsed)
}
async fn resolve(f: &Fetcher, url: &str) -> Result<GuidelineDoc> {
    let Parsed { mut doc, tables } = publisher(f, url).await?;
    for (id, url, caption) in tables {
        doc.attachments.push(json!({"url":url,"role":"source_linked_table_reference","source_anchor":id,"caption":caption}));
        match fresh(f, &url).await {
            Ok(page) => {
                let parsed = table_content(
                    &page.text(),
                    &Url::parse(&page.record.final_url)?,
                    &id,
                    &caption,
                );
                doc.fetches.push(page.record);
                match parsed {
                    Ok((mut blocks, asset)) => {
                        if let Some(i) = doc.blocks.iter().position(|b| b.block_id == id) {
                            for b in &mut blocks {
                                b.heading_path = doc.blocks[i].heading_path.clone();
                            }
                            doc.blocks.splice(i..i + 1, blocks);
                            doc.attachments.push(asset);
                        } else {
                            bail!("Linked table placeholder missing: {id}")
                        }
                    }
                    Err(e) => doc
                        .warnings
                        .push(format!("Linked table {id} unresolved: {e:#}")),
                }
            }
            Err(e) => doc.warnings.push(format!(
                "Linked table {id} unresolved (no alternate access attempted): {e:#}"
            )),
        }
    }
    Ok(doc)
}
fn snapshot(doc: &GuidelineDoc) -> Result<String> {
    // Ignore response/nav boilerplate and fetch time; retain clinical, version, rights and table evidence.
    let payload = json!({"title":doc.title,"doi":doc.doi,"dates":doc.date_observations,"version":doc.version,"publisher":doc.publisher,"issuers":doc.issuers,
        "rights":doc.rights,"blocks":doc.blocks,"assets":doc.attachments,"relations":doc.relations});
    Ok(crate::common::sha256_hex(&serde_json::to_vec(&payload)?))
}

// This rendition preserves the PDFium text layer page by page. It deliberately does
// not pass through Markdown/table heuristics or borrow HTML recommendation grades.
#[derive(Clone, serde::Deserialize)]
struct PdfPage {
    text: String,
    width_points: f32,
    height_points: f32,
}

fn pdf_descriptor(source: &GuidelineDoc) -> Result<()> {
    if source.url != LEAF || source.doi.as_deref() != Some(DOI) {
        bail!("AASM PDF publisher work identity missing or changed")
    }
    let links: Vec<_> = source
        .attachments
        .iter()
        .filter(|a| {
            a["role"].as_str() == Some("alternative_rendition")
                && a["rendition"].as_str() == Some("pdf")
        })
        .collect();
    if links.len() != 1 || links[0]["url"].as_str() != Some(PDF) {
        bail!("AASM exact source-linked native PDF descriptor missing or changed")
    }
    Ok(())
}

fn expected_doi(text: &str) -> bool {
    // Literal contiguous DOI, with boundaries: a longer DOI with this prefix is
    // not accepted. First-page occurrence supports identity, not clinical currency.
    // A punctuation delimiter must itself terminate at whitespace/end, so '.1'
    // and '.supplement' cannot be mistaken for a terminal full stop.
    regex::Regex::new(
        r"(?i)(?:^|[^a-z0-9_])10\.1007/s44470-025-00038-8(?:$|\s|[.,;:!?()\[\]{}]+(?:$|\s))",
    )
    .expect("constant DOI regex")
    .is_match(text)
}

fn pdf_pages(body: &[u8]) -> Result<Vec<PdfPage>> {
    if body.len() > PDF_BYTES || !body.starts_with(b"%PDF-") {
        bail!("AASM native PDF magic/byte bound failed")
    }
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("aasm.pdf");
    std::fs::write(&path, body)?;
    let _gate = crate::media::PDFIUM_GATE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let lib = crate::media::pdfium()?;
    let pdf = lib
        .load_pdf_from_file(&path, None)
        .map_err(|e| anyhow::anyhow!("AASM PDFium load: {e}"))?;
    let count = pdf.pages().len() as usize;
    collect_pdf_pages(count, |index| {
        // PdfPages::iter() stops at the first get() error. Address every declared
        // physical page instead so a failed middle/tail page cannot look complete.
        let page = pdf
            .pages()
            .get(index as u16)
            .map_err(|e| anyhow::anyhow!("AASM PDFium load physical page {}: {e}", index + 1))?;
        let text = page
            .text()
            .map_err(|e| anyhow::anyhow!("AASM PDFium text page {}: {e}", index + 1))?
            .all();
        Ok(PdfPage {
            text,
            width_points: page.width().value,
            height_points: page.height().value,
        })
    })
}

fn collect_pdf_pages(
    count: usize,
    mut load: impl FnMut(usize) -> Result<PdfPage>,
) -> Result<Vec<PdfPage>> {
    if count == 0 || count > PDF_PAGES {
        bail!("AASM PDF physical page count {count} exceeds finite 1..={PDF_PAGES}; not truncated")
    }
    let mut pages = Vec::with_capacity(count);
    let mut total = 0;
    for index in 0..count {
        let page = load(index)?;
        total += page.text.len();
        if page.text.len() > PAGE_TEXT_BYTES || total > PDF_TEXT_BYTES {
            bail!(
                "AASM PDF text bound exceeded on physical page {}; not truncated",
                index + 1
            )
        }
        pages.push(page);
    }
    if pages.len() != count {
        bail!("AASM PDF declared/loaded physical page count mismatch; not complete")
    }
    Ok(pages)
}

fn pdf_document(
    source: &GuidelineDoc,
    byte_sha: &str,
    pages: Vec<PdfPage>,
) -> Result<GuidelineDoc> {
    pdf_descriptor(source)?;
    if byte_sha.len() != 64
        || !byte_sha.bytes().all(|b| b.is_ascii_hexdigit())
        || pages.is_empty()
        || pages.len() > PDF_PAGES
    {
        bail!("AASM PDF content identity or page bounds invalid")
    }
    if !expected_doi(&pages[0].text) {
        bail!("AASM native PDF first-page expected DOI missing; work identity unverified")
    }
    let mut blocks = vec![];
    let mut evidence = vec![];
    let mut total = 0;
    for (i, page) in pages.into_iter().enumerate() {
        total += page.text.len();
        if page.text.trim().is_empty()
            || page.text.len() > PAGE_TEXT_BYTES
            || total > PDF_TEXT_BYTES
            || !page.width_points.is_finite()
            || !page.height_points.is_finite()
            || page.width_points <= 0.0
            || page.height_points <= 0.0
        {
            bail!("AASM PDF physical page {} text/dimensions incomplete or over bound; no OCR or silent omission", i + 1)
        }
        let number = (i + 1) as u32;
        let id = format!("pdf-page-{number:04}");
        evidence.push(json!({"block_id":id,"physical_page":number,
            "text_utf8_bytes":page.text.len(),"exact_text_sha256":crate::common::sha256_hex(page.text.as_bytes()),
            "row_section_leading_whitespace_chars":page.text.chars().take_while(|c| c.is_whitespace()).count(),
            "row_section_trimmed_chars":page.text.trim().chars().count(),
            "width_points":page.width_points,"height_points":page.height_points}));
        blocks.push(Block {
            block_id: id,
            kind: BlockKind::Paragraph,
            text: page.text,
            // A synthetic locator context, not an inferred source heading. The unique
            // path also prevents the shared row writer grouping adjacent PDF pages.
            heading_path: vec![format!(
                "PDF physical page {number} (unclassified text layer)"
            )],
            page: Some(number),
        });
    }
    let mut rights = common::unknown_rights();
    rights.note = Some("PDF-specific rights and third-party credit lines have not been mapped. The linked HTML article notice is retained separately as scoped evidence, not inherited as this asset's reuse licence.".into());
    rights.evidence_urls = vec![PDF.into(), format!("{LEAF}#rightslink-content")];
    Ok(GuidelineDoc {
        title:source.title.clone(), url:PDF.into(), doi:Some(DOI.into()),
        item_type:source.item_type.clone(), publisher:source.publisher.clone(),
        issuers:source.issuers.clone(), language:source.language.clone(),
        // Publisher HTML observations retain their own source URLs. Neither the
        // byte digest nor this processing revision is a clinical version/date.
        published:source.published.clone(), date_observations:source.date_observations.clone(),
        version:None, status:"unknown".into(), rights, blocks,
        relations:vec![Relation { rel:"rendition_of".into(), target:LEAF.into(),
            scope:Some("Source-linked full article PDF; text layer only, not a replacement for the HTML rendition".into()),
            evidence_url:Some(LEAF.into()) }],
        attachments:vec![json!({"url":LEAF,"role":"publisher_metadata_and_link_evidence",
            "doi":source.doi,"rights_notice_scope":"HTML article only; PDF rights unverified", "html_rights":source.rights})],
        search_metadata:json!({"aasm_pdf":{
            "rendition":"pdf", "role":"alternative_full_article_rendition", "processing_revision":PDF_PROCESSING,
            "source_article_url":LEAF,"source_pdf_url":PDF,"source_pdf_sha256":byte_sha,
            "adapter_acceptance_byte_cap":PDF_BYTES,
            "transport_byte_cap_scope":"source Fetcher max_bytes; independent of adapter post-fetch acceptance cap",
            "work_identity_evidence":"publisher citation_doi plus exact first-page PDF text DOI",
            "metadata_provenance":"source-linked publisher HTML; source URLs retained on date observations",
            "physical_page_count":evidence.len(),"pages":evidence,
            "text_method":"PDFium page.text().all(); exact returned UTF-8 retained without Markdown conversion",
            "page_numbering":"one-based physical PDF pages; printed labels not inferred",
            "heading_path_method":"synthetic physical-page locator, not source heading inference",
            "row_text_method":"shared writer trims page-edge whitespace and chunks by words; char_start/end address trimmed page text; exact untrimmed text remains canonical",
            "OCR_performed":false,"all_physical_pages_have_text":true,
            "table_geometry_verified":false,"table_count":null,"recommendation_grades_mapped":false,
            "footnote_ownership_verified":false,"reading_order_clinically_verified":false,
            "HTML_table_routes_coverage":"separate; this PDF acquisition does not resolve robots-disallowed HTML routes",
            "clinical_currency_verified":false}}),
        warnings:vec!["PDF text layer retained for every physical page. PDFium text order is not verified reading order; table cells/spans, headers, footnote ownership and recommendation grades are not inferred. No heuristic table count is clinical table evidence. OCR is disabled; blank/unreadable pages reject this rendition.".into(),
            "Publisher HTML metadata and rights notice have their own scope. PDF-specific reuse rights, printed page labels, clinical edition and currentness remain unverified.".into()],
        ..Default::default()
    })
}

fn pdf_snapshot(doc: &GuidelineDoc) -> Result<String> {
    let mut content_evidence = doc.search_metadata.clone();
    // Operational transport bounds are provenance, not a new source edition.
    if let Some(evidence) = content_evidence["aasm_pdf"].as_object_mut() {
        evidence.remove("transport_byte_cap");
    }
    Ok(crate::common::sha256_hex(&serde_json::to_vec(&json!({
        "document_snapshot":snapshot(doc)?,"url":doc.url,"language":doc.language,
        "published":doc.published,"status":doc.status,"item_type":doc.item_type,
        "source_and_page_evidence":content_evidence}))?))
}

async fn resolve_pdf(f: &Fetcher, source: &GuidelineDoc) -> Result<GuidelineDoc> {
    pdf_descriptor(source)?;
    let mut req = Req::get(PDF);
    req.cache_max_age_secs = Some(0);
    let pdf = f.request(&req).await?;
    if !(200..300).contains(&pdf.record.status)
        || !pdf.is_pdf()
        || !pdf.body.starts_with(b"%PDF-")
        || pdf.body.len() > PDF_BYTES
    {
        bail!(
            "AASM native PDF unavailable, wrapper or over byte bound (HTTP {})",
            pdf.record.status
        )
    }
    let digest = crate::common::sha256_hex(&pdf.body);
    if digest != pdf.record.sha256 {
        bail!("AASM PDF fetch/body hash mismatch")
    }
    let source_bytes = pdf.body.len();
    let Fetched { record, body } = pdf;
    let pages = tokio::task::spawn_blocking(move || pdf_pages(&body)).await??;
    let mut doc = pdf_document(source, &digest, pages)?;
    // Only the publisher descriptor and this rendition's actual fetch belong to
    // its acquisition provenance; unrelated HTML table requests are not inherited.
    doc.fetches.extend(source.fetches.first().cloned());
    doc.fetches.push(record);
    doc.search_metadata["aasm_pdf"]["transport_byte_cap"] = json!(f.max_bytes);
    doc.search_metadata["aasm_pdf"]["source_pdf_bytes"] = json!(source_bytes);
    Ok(doc)
}

fn pdf_plan(
    src: &SourceEntry,
    doc: &GuidelineDoc,
    catalog_error: &Option<String>,
    cat: &str,
) -> Result<PlanRow> {
    let digest = pdf_snapshot(doc)?;
    let work = common::short_hash(LEAF);
    Ok(PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("{work}-pdf-{}", &digest[..16]),
        url: PDF.into(),
        title: Some(doc.title.clone()),
        rendition: Some("pdf".into()),
        version: None,
        extra: json!({"work_key":work,"snapshot_sha256":digest,"processing_revision":PDF_PROCESSING,
            "source_pdf_sha256":doc.search_metadata["aasm_pdf"]["source_pdf_sha256"],
            "source_article_url":LEAF,"source_evidence_url":cat,
            "catalog_enumerated":false,"catalog_error":catalog_error}),
    })
}

fn validate_pdf_plan(src: &SourceEntry, row: &PlanRow, doc: &GuidelineDoc) -> Result<()> {
    let expected = pdf_plan(src, doc, &None, src.catalog_url.as_deref().unwrap_or(""))?;
    if row.source_id != src.id
        || row.doc_key != expected.doc_key
        || row.title != expected.title
        || row.url != PDF
        || row.rendition.as_deref() != Some("pdf")
        || row.version.is_some()
        || row.extra["work_key"] != expected.extra["work_key"]
        || row.extra["snapshot_sha256"] != expected.extra["snapshot_sha256"]
        || row.extra["source_pdf_sha256"] != expected.extra["source_pdf_sha256"]
        || row.extra["processing_revision"] != json!(PDF_PROCESSING)
        || row.extra["source_article_url"] != json!(LEAF)
        || row.extra["source_evidence_url"] != expected.extra["source_evidence_url"]
    {
        bail!("AASM PDF plan identity/content snapshot changed; rediscover")
    }
    Ok(())
}
fn unresolved(src: &SourceEntry, url: String, title: String, error: String) -> PlanRow {
    PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("{}-unresolved", common::short_hash(&url)),
        url,
        title: Some(title),
        rendition: None,
        version: None,
        extra: json!({"discovery_error":error,"catalog_enumerated":false}),
    }
}

// ---- Fix round 1 (group C): full AASM catalog routes ------------------------------------------------
// The society catalog lists, per topic (h4), a primary journal link labelled "Guideline", "Guidance Statement",
// "Position Paper", ... Many are society-hosted PDFs (aasm.org/wp-content/uploads, robots-allowed), sometimes as a
// "Download PDF" link beside a JCSM DOI. Those PDFs are acquired here. Items that are only a PMC or DOI link carry
// the identifier the catalog states (never a looked-up one) for the shared PMC crosswalk.
#[derive(Debug, Clone, PartialEq)]
struct CatalogItem {
    topic: String,
    label: String,
    url: String,
    pdf: Option<String>,
    doi: Option<String>,
    pmcid: Option<String>,
    title: Option<String>,
}
const CLINICAL_LABELS: [(&str, &str); 6] = [
    ("Guideline", "clinical_practice_guideline"),
    ("Guidance Statement", "clinical_guidance_statement"),
    ("Position Paper", "position_paper"),
    ("Position", "position_paper"),
    ("Protocol Paper", "protocol_paper"),
    ("Clinical Practice Guideline", "clinical_practice_guideline"),
];
fn label_item_type(label: &str) -> Option<&'static str> {
    // Source labels may carry an edition prefix ("2005 Guideline", "2021 Protocol Paper"); the prefix stays in the label.
    let core = label.trim_start_matches(|c: char| c.is_ascii_digit() || c == ' ');
    CLINICAL_LABELS.iter().find(|(l, _)| *l == core).map(|(_, t)| *t)
}
fn stated_doi(url: &Url) -> Option<String> {
    (url.host_str() == Some("doi.org")).then(|| url.path().trim_start_matches('/').to_string()).filter(|d| d.starts_with("10."))
}
fn stated_pmcid(url: &Url) -> Option<String> {
    if url.host_str() != Some("pmc.ncbi.nlm.nih.gov") { return None; }
    url.path_segments()?.filter(|s| !s.is_empty()).find(|s| s.starts_with("PMC") && s[3..].chars().all(|c| c.is_ascii_digit()) && s.len() > 3).map(String::from)
}
fn society_pdf(url: &Url) -> bool {
    url.host_str() == Some("aasm.org") && url.path().starts_with("/wp-content/uploads/") && url.path().to_lowercase().ends_with(".pdf")
}
fn catalog_items(html: &str, base: &Url) -> Result<Vec<CatalogItem>> {
    let d = Html::parse_document(html);
    // "Most Recently Published" paragraphs state full article titles for some of the same links.
    let mut titles = std::collections::HashMap::new();
    for p in d.select(&sel("main p")) {
        let Some(a) = p.select(&sel("a[href]")).next() else { continue };
        let t = text_of(&a);
        if let Some(u) = common::link(base, a.value().attr("href").unwrap_or("")) {
            if t.len() > 40 { titles.entry(u).or_insert(t); }
        }
    }
    let mut out: Vec<CatalogItem> = vec![];
    let mut seen = HashSet::new();
    for h in d.select(&sel("main div > h4")) {
        let topic = text_of(&h);
        let Some(div) = h.parent().and_then(scraper::ElementRef::wrap) else { continue };
        for li in div.select(&sel("li")) {
            // Only the li's own first link: nested lists are visited as their own li.
            let links: Vec<_> = li.children().filter_map(scraper::ElementRef::wrap).filter(|e| e.value().name() == "a").collect();
            let Some(primary) = links.first() else { continue };
            let label = text_of(primary);
            if label_item_type(&label).is_none() { continue; }
            let Some(url) = common::link(base, primary.value().attr("href").unwrap_or("")) else { continue };
            let parsed = Url::parse(&url)?;
            if parsed.path().contains("members-only") { continue; }
            let pdf = if society_pdf(&parsed) { Some(url.clone()) } else {
                links.iter().skip(1).filter(|a| text_of(a) == "Download PDF")
                    .filter_map(|a| common::link(base, a.value().attr("href").unwrap_or("")))
                    .find(|u| Url::parse(u).is_ok_and(|x| society_pdf(&x)))
            };
            if !seen.insert(url.clone()) { continue; }
            out.push(CatalogItem { topic: topic.clone(), label, doi: stated_doi(&parsed), pmcid: stated_pmcid(&parsed), title: titles.get(&url).cloned(), url, pdf });
        }
    }
    if out.is_empty() { bail!("AASM catalog topic items missing") }
    Ok(out)
}
fn pdf_route_row(src: &SourceEntry, item: &CatalogItem, cat: &str) -> PlanRow {
    let pdf = item.pdf.clone().unwrap_or_default();
    let work = common::short_hash(&item.url);
    PlanRow { source_id: src.id.clone(), doc_key: format!("{work}-unresolved"), url: pdf, title: Some(item.title.clone().unwrap_or_else(|| item.topic.clone())),
        rendition: Some("pdf".into()), version: None,
        extra: json!({"route":"aasm_hosted_pdf","work_key":work,"source_label":item.label,"topic":item.topic,"catalog_url":item.url,
            "doi":item.doi,"pmcid":item.pmcid,"source_evidence_url":cat,"catalog_enumerated":true}) }
}
fn crosswalk_row(src: &SourceEntry, item: &CatalogItem, cat: &str) -> PlanRow {
    let mut r = unresolved(src, item.url.clone(), item.title.clone().unwrap_or_else(|| item.topic.clone()),
        "AASM-linked journal article with no society-hosted PDF; full text only via PMC (pending shared pmc_crosswalk)".into());
    r.extra["route"] = json!("pmc_crosswalk");
    r.extra["doi"] = json!(item.doi);
    r.extra["pmcid"] = json!(item.pmcid);
    r.extra["source_label"] = json!(item.label);
    r.extra["topic"] = json!(item.topic);
    r.extra["source_evidence_url"] = json!(cat);
    r.extra["catalog_enumerated"] = json!(true);
    r
}
/// Round 2: the PMC copy of a catalog item, from the PMCID or DOI the catalog link states (never looked up otherwise).
/// Full text when PMC distributes the XML (OA, author manuscript, or licence-less → restricted); else relation only.
async fn aasm_cross(src: &SourceEntry, f: &Fetcher, cx: &mut xw::Session, item: &CatalogItem, cat: &str, source_doc: Option<String>) -> (Option<PlanRow>, Cross) {
    let (lookup, route) = match (&item.pmcid, item.doi.as_deref().and_then(cw::normalize_doi)) {
        (Some(p), _) => (Lookup::Pmcid(p.clone()), "source_pmcid"),
        (None, Some(d)) => (Lookup::Doi(d), "source_doi"),
        _ => {
            let reason = "the catalog link states no PMCID or DOI".to_string();
            return (None, Cross { row: None, relation: None, status: "no_identifier", evidence: json!({"status": "no_identifier", "url": item.url}), reason: Some(reason) });
        }
    };
    let sref = SourceRef { source_url: item.url.clone(), evidence_url: Some(cat.into()), source_title: item.title.clone(),
        item_type: label_item_type(&item.label).map(String::from), key_prefix: "aasm-pmc".into(), id_route: route.into(), notes: vec![] };
    let extra = json!({"topic": item.topic, "source_label": item.label, "catalog_url": item.url, "source_evidence_url": cat, "catalog_enumerated": true});
    let mut c = cx.try_id(src, f, lookup, sref, extra, source_doc).await;
    let row = c.row.take();
    (row, c)
}
fn hosted_pdf_rights(blocks: &[Block], url: &str) -> Rights {
    let notice = blocks.iter().take(120).map(|b| b.text.as_str())
        .find(|t| { let l = t.to_lowercase(); l.contains("copyright") && (l.contains("all rights reserved") || l.contains("american academy of sleep medicine")) });
    match notice {
        Some(n) => Rights { status: "restricted".into(), restrictions: vec![n.into()], evidence_urls: vec![url.into()],
            note: Some("Notice printed in the society-hosted PDF; journal and PMC copies carry their own terms.".into()), ..Default::default() },
        None => Rights { status: "unknown".into(), note: Some("No licence or copyright notice found in the converted PDF text.".into()), ..Default::default() },
    }
}
/// Numbered AASM statements closed by a printed grade, in one converted block: current GRADE wording
/// ("1. The AASM suggests … (CONDITIONAL)") and legacy practice-parameter levels ("3.1.1 … (Guideline)",
/// "1. … [6.2.1] (Standard)"). The statement (number, text, grade) is inserted as its own Recommendation block right
/// after the source block, which keeps the following rationale. Returns (structured, graded fragments left in prose).
fn hosted_pdf_recommendations(blocks: &mut Vec<Block>) -> (usize, usize) {
    let markup = regex::Regex::new(r"</?u>|\*\*|<sup>[^<]*</sup>|</?sup>").unwrap();
    let stmt = regex::Regex::new(r"^(?:Recommendation\s+)?(\d{1,2}(?:\.\d{1,2}){0,3}[a-z]?)[.:]?\s+(\S.{14,}?)\s*\((STRONG|CONDITIONAL|Standard|Guideline|Option|STANDARD|GUIDELINE|OPTION)\)").unwrap();
    let grade = regex::Regex::new(r"\((STRONG|CONDITIONAL|Standard|Guideline|Option|STANDARD|GUIDELINE|OPTION)\)").unwrap();
    let mut inserts = vec![];
    let mut fragments = 0;
    for (i, b) in blocks.iter().enumerate() {
        if !matches!(b.kind, BlockKind::Paragraph | BlockKind::ListItem | BlockKind::Heading { .. }) { continue; }
        let clean = markup.replace_all(&b.text, "").split_whitespace().collect::<Vec<_>>().join(" ");
        match stmt.captures(&clean) {
            Some(c) => inserts.push((i, Block { block_id: format!("{}-rec", b.block_id), kind: BlockKind::Recommendation {
                label: Some(c[1].to_string()), strength: Some(c[3].to_string()), certainty: None, population: None, grading_system: None, qualifier: None },
                text: format!("{} ({})", c[2].trim(), &c[3]), heading_path: b.heading_path.clone(), page: b.page })),
            None => if grade.is_match(&clean) { fragments += 1 },
        }
    }
    let n = inserts.len();
    for (i, r) in inserts.into_iter().rev() { blocks.insert(i + 1, r); }
    (n, fragments)
}
async fn hosted_pdf_doc(src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
    let p = f.get_ok(&row.url).await?;
    if !p.is_pdf() { bail!("AASM society-hosted PDF did not return PDF bytes") }
    if row.extra["pdf_sha256"].as_str().is_some_and(|h| h != p.record.sha256) { bail!("AASM PDF changed since planning; re-plan") }
    let (mut blocks, mut warnings, _) = super::static_list::blocks_for(&p, None, 30).await?;
    if blocks.is_empty() { bail!("AASM PDF has no extracted text") }
    let (recs, fragments) = hosted_pdf_recommendations(&mut blocks);
    if recs == 0 { warnings.push("No numbered AASM statement closed by a printed grade was recognised in the converted text; recommendations remain in prose blocks.".into()); }
    if fragments > 0 { warnings.push(format!("{fragments} converted blocks carry a printed grade but no complete numbered statement (statement split across PDF lines/headings); left as prose, not structured.")); }
    warnings.push("PDF layout, tables, figures and page boundaries come from automatic conversion and are not visually reconciled; Guideline-at-a-Glance (members only) and reviews are separate items, not acquired.".into());
    let rights = hosted_pdf_rights(&blocks, &row.url);
    let cat = row.extra["source_evidence_url"].as_str().map(String::from);
    let mut relations = vec![];
    if let Some(doi) = row.extra["doi"].as_str() {
        relations.push(Relation { rel: "rendition_of".into(), target: format!("doi:{doi}"), scope: None, evidence_url: cat.clone() });
    }
    if let Some(pmcid) = row.extra["pmcid"].as_str() {
        relations.push(Relation { rel: "rendition_of".into(), target: format!("pmc:{pmcid}"), scope: None, evidence_url: cat.clone() });
    }
    let extra_rel = xw::relations_from_extra(&row.extra, &relations);
    relations.extend(extra_rel);
    let item_type = row.extra["source_label"].as_str().and_then(label_item_type).unwrap_or("clinical_practice_guideline");
    Ok(GuidelineDoc { source_id: src.id.clone(), doc_key: row.doc_key.clone(), work_key: row.extra["work_key"].as_str().map(String::from),
        title: row.title.clone().unwrap_or_default(), url: row.url.clone(), item_type: Some(item_type.into()), status: "unknown".into(),
        doi: row.extra["doi"].as_str().map(String::from), rights, relations, blocks, fetches: vec![p.record], warnings,
        attachments: vec![json!({"url":row.extra["catalog_url"],"role":"journal_article_link","label":row.extra["source_label"],"rights":"unknown; independent publisher"})],
        search_metadata: json!({"topic":row.extra["topic"],"source_label":row.extra["source_label"]}), ..Default::default() })
}

#[async_trait::async_trait]
impl Adapter for AasmSleep {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let cat = src.catalog_url.as_deref().context("AASM catalog missing")?;
        let mut candidates = vec![];
        if let Some(seeds) = src.config["source_linked_documents"].as_array() {
            if seeds.len() > 8 {
                bail!("AASM explicit seed bound exceeded")
            }
            for seed in seeds {
                let url = seed["url"].as_str().context("AASM seed URL missing")?;
                if url != LEAF || seed["source_evidence_url"].as_str() != Some(cat) {
                    bail!("AASM seed is not the verified society-linked public leaf")
                }
                candidates.push((url.to_string(), "Source-linked public guideline".into()));
            }
        }
        let catalog_result = match f.get_ok(cat).await {
            Ok(p) => {
                let base = Url::parse(&p.record.final_url)?;
                catalog_items(&p.text(), &base).or_else(|_| catalog(&p.text(), &base).map(|links| links.into_iter()
                    .map(|(url, label)| CatalogItem { topic: label.clone(), label, url, pdf: None, doi: None, pmcid: None, title: None }).collect()))
            }
            Err(e) => Err(e),
        };
        let mut items = vec![];
        let catalog_error = match catalog_result {
            Ok(found) => {
                for item in found {
                    if item.url == LEAF { candidates.push((item.url.clone(), item.label.clone())); } else { items.push(item); }
                }
                None
            }
            Err(e) => Some(format!("{e:#}")),
        };
        let mut rows = vec![];
        let mut seen = HashSet::new();
        // Resolvable routes first (Springer leaf, society-hosted PDFs), identifier-only journal links after.
        // Round 2: PMC-stated items go through the PMC crosswalk; a full-text PMC row stays in catalog order, an item
        // PMC does not distribute (or that fails) stays an unresolved row after every resolvable row, with the
        // crosswalk outcome. Society-hosted PDFs keep their route and gain the PMC copy or a relation-only PMC link.
        let mut deferred = vec![];
        let mut cx = xw::Session::default();
        for item in &items {
            if limit.is_some_and(|n| rows.len() >= n) { break; }
            if !seen.insert(item.url.clone()) { continue; }
            if item.pdf.is_none() {
                let (open, c) = aasm_cross(src, f, &mut cx, item, cat, None).await;
                match open {
                    Some(r) => rows.push(r),
                    None if c.status == "duplicate" => {}
                    None => {
                        let mut r = crosswalk_row(src, item, cat);
                        r.extra["discovery_error"] = json!(format!("AASM-linked journal article with no society-hosted PDF; PMC crosswalk {}: {}", c.status, c.reason.clone().unwrap_or_default()));
                        xw::record_on(&mut r, &c);
                        deferred.push(r);
                    }
                }
                continue;
            }
            let mut row = pdf_route_row(src, item, cat);
            match f.get_ok(&row.url).await {
                Ok(p) if p.is_pdf() => { row.doc_key = format!("{}-{}", row.extra["work_key"].as_str().unwrap_or("aasm"), &p.record.sha256[..12]); row.extra["pdf_sha256"] = json!(p.record.sha256); }
                Ok(_) => row.extra["discovery_error"] = json!("AASM society-hosted PDF link did not return PDF bytes"),
                Err(e) => row.extra["discovery_error"] = json!(format!("{e:#}")),
            }
            let mut open = None;
            if item.doi.is_some() || item.pmcid.is_some() {
                let source_doc = row.extra["discovery_error"].is_null().then(|| format!("guideline:{}:{}", src.id, row.doc_key));
                let (o, c) = aasm_cross(src, f, &mut cx, item, cat, source_doc).await;
                xw::record_on(&mut row, &c);
                open = o;
            }
            rows.push(row);
            rows.extend(open);
        }
        if let Some(n) = limit { rows.truncate(n); }
        for (u, t) in candidates {
            if limit.is_some_and(|n| rows.len() >= n) {
                break;
            }
            if !seen.insert(u.clone()) {
                continue;
            }
            if u != LEAF {
                rows.push(unresolved(src, u, t, "Source-linked terminal template not mapped; no generic or alternate host fallback".into()));
            } else {
                match resolve(f, &u).await {
                    Ok(doc) => {
                        let sha = snapshot(&doc)?;
                        let work = common::short_hash(&u);
                        rows.push(PlanRow { source_id: src.id.clone(), doc_key: format!("{work}-{}", &sha[..16]), url: u, title: Some(doc.title.clone()), rendition: Some("html".into()), version: doc.version.clone(),
                            extra: json!({"work_key":work,"snapshot_sha256":sha,"catalog_enumerated":false,"catalog_error":catalog_error,"source_evidence_url":cat}) });
                        if !limit.is_some_and(|n| rows.len() >= n) {
                            match resolve_pdf(f, &doc).await {
                                Ok(pdf) => rows.push(pdf_plan(src, &pdf, &catalog_error, cat)?),
                                Err(e) => {
                                    let mut failed = unresolved(
                                        src,
                                        PDF.into(),
                                        "Source-linked PDF rendition unresolved".into(),
                                        format!("{e:#}"),
                                    );
                                    failed.rendition = Some("pdf".into());
                                    failed.extra["source_article_url"] = json!(LEAF);
                                    failed.extra["source_evidence_url"] = json!(cat);
                                    rows.push(failed);
                                }
                            }
                        }
                    }
                    Err(e) => rows.push(unresolved(src, u, t, format!("{e:#}"))),
                }
            }
            if limit.is_some_and(|n| rows.len() >= n) {
                break;
            }
        }
        let room = limit.map(|n| n.saturating_sub(rows.len())).unwrap_or(usize::MAX);
        rows.extend(deferred.into_iter().take(room));
        if !limit.is_some_and(|n| rows.len() >= n) {
            if let Some(error) = catalog_error {
                rows.push(unresolved(
                    src,
                    cat.into(),
                    "AASM catalog not enumerated".into(),
                    error,
                ));
            }
        }
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if cw::is_crosswalk_row(row) {
            return xw::extract(src, f, row).await;
        }
        if let Some(e) = row.extra["discovery_error"].as_str() {
            bail!("AASM terminal unresolved: {e}")
        }
        if row.extra["route"] == "aasm_hosted_pdf" {
            return hosted_pdf_doc(src, f, row).await;
        }
        let mut doc = if row.rendition.as_deref() == Some("pdf") {
            if row.url != PDF || row.source_id != src.id {
                bail!("AASM PDF plan source or URL mismatch")
            }
            let source = publisher(f, LEAF).await?.doc;
            let doc = resolve_pdf(f, &source).await?;
            validate_pdf_plan(src, row, &doc)?;
            doc
        } else {
            let doc = resolve(f, &row.url).await?;
            if row.extra["snapshot_sha256"].as_str() != Some(snapshot(&doc)?.as_str()) {
                bail!("AASM clinical/version/table/asset snapshot changed after discovery; rediscover")
            }
            doc
        };
        doc.source_id = src.id.clone();
        doc.doc_key = row.doc_key.clone();
        doc.work_key = row.extra["work_key"].as_str().map(String::from);
        doc.warnings.push("Catalog not completely enumerated; this is one verified society-linked public publisher leaf. CrossMark update status not checked.".into());
        if let Some(e) = row.extra["catalog_error"].as_str() {
            doc.warnings.push(format!(
                "Society catalog unresolved independently of public publisher leaf: {e}"
            ));
        }
        Ok(doc)
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const PAGE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/aasm_sleep/page.html"
    ));
    const TABLE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/aasm_sleep/table.html"
    ));
    fn parsed() -> GuidelineDoc {
        content(PAGE, &Url::parse(LEAF).unwrap()).unwrap().doc
    }
    const SYNTHETIC_PDF: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/aasm_sleep/physical-pages-synthetic.pdf"
    ));
    fn synthetic_pages() -> Vec<PdfPage> {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/aasm_sleep/pdf-pages-synthetic.json"
        )))
        .unwrap()
    }
    fn pdf_fixture_doc() -> GuidelineDoc {
        pdf_document(
            &parsed(),
            &crate::common::sha256_hex(SYNTHETIC_PDF),
            synthetic_pages(),
        )
        .unwrap()
    }
    fn test_source() -> SourceEntry {
        serde_json::from_value(json!({"id":"aasm_sleep","access":"public","enabled":true,
            "catalog_url":"https://aasm.org/clinical-resources/practice-standards/practice-guidelines/",
            "config":{"source_linked_documents":[{"url":LEAF,
            "source_evidence_url":"https://aasm.org/clinical-resources/practice-standards/practice-guidelines/"}]}})).unwrap()
    }

    #[test]
    fn pdf_descriptor_requires_exact_source_link_and_work_identity() {
        let original = parsed();
        pdf_descriptor(&original).unwrap();
        for (field, replacement) in [("doi", "10.1007/other-work"), ("url", PDF)] {
            let mut changed = original.clone();
            if field == "doi" {
                changed.doi = Some(replacement.into());
            } else {
                changed.url = replacement.into();
            }
            assert!(pdf_descriptor(&changed).is_err());
        }
        let mut changed = original.clone();
        changed.attachments[0]["url"] = json!("https://example.org/copied.pdf");
        assert!(pdf_descriptor(&changed).is_err());
        changed = original.clone();
        changed.attachments.clear();
        assert!(pdf_descriptor(&changed).is_err());
        changed = original.clone();
        changed.attachments.push(changed.attachments[0].clone());
        assert!(pdf_descriptor(&changed).is_err());
        assert!(expected_doi(&format!("https://doi.org/{DOI}.")));
        assert!(!expected_doi(&format!("{DOI}-companion")));
        assert!(!expected_doi(&format!("{DOI}9")));
        assert!(!expected_doi(&format!("{DOI}.1")));
        assert!(!expected_doi(&format!("{DOI}.supplement")));
        assert!(expected_doi(&format!("{DOI}.\n")));
        let doc = pdf_fixture_doc();
        assert_eq!(doc.rights.status, "unknown");
        assert!(doc.rights.license.is_none());
        assert_eq!(
            doc.attachments[0]["html_rights"]["license"],
            json!("CC BY-NC-ND 4.0")
        );
        assert_eq!(doc.relations[0].rel, "rendition_of");
        assert_eq!(doc.relations[0].target, LEAF);
    }

    #[test]
    fn pdf_page_text_and_physical_locators_survive_row_writer_without_table_inference() {
        use crate::guidelines::doc::{to_rows, ChunkCfg};
        let pages = synthetic_pages();
        let mut doc = pdf_fixture_doc();
        doc.source_id = "aasm_sleep".into();
        doc.doc_key = "synthetic-pdf-snapshot".into();
        doc.work_key = Some(common::short_hash(LEAF));
        assert_eq!(doc.blocks.len(), 2);
        for (i, block) in doc.blocks.iter().enumerate() {
            assert_eq!(block.text, pages[i].text); // Includes source whitespace and symbols.
            assert_eq!(block.page, Some((i + 1) as u32));
            assert!(matches!(block.kind, BlockKind::Paragraph));
            assert_eq!(
                doc.search_metadata["aasm_pdf"]["pages"][i]["exact_text_sha256"],
                json!(crate::common::sha256_hex(pages[i].text.as_bytes()))
            );
        }
        let rows = to_rows(
            &doc,
            &ChunkCfg {
                chunk_tokens: 12,
                overlap_tokens: 0,
                context_prefix: false,
                ..Default::default()
            },
            false,
            "canonical.json",
            &Default::default(),
        );
        assert!(!rows
            .iter()
            .any(|r| r["kind"] == "table_text" || r.get("recommendation").is_some()));
        let text: Vec<_> = rows.iter().filter(|r| r["kind"] == "text").collect();
        assert!(text.len() > 2); // Exercise actual page-local chunking, not one row per document.
        for (i, page) in pages.iter().enumerate() {
            let number = json!(i + 1);
            let id = format!("pdf-page-{:04}", i + 1);
            let scoped: Vec<_> = text
                .iter()
                .filter(|r| r["locator"]["page"] == number)
                .collect();
            assert!(!scoped.is_empty());
            assert!(scoped.iter().all(|r| r["locator"]["page_end"] == number
                && r["locator"]["block_ids"] == json!([id])
                && r["doc_id"] == json!(doc.doc_id())
                && r["article_id"] == json!(doc.doc_id())));
            let mut assembled = String::new();
            let mut covered = vec![false; page.text.trim().chars().count()];
            for row in scoped {
                let start = row["locator"]["char_start"].as_u64().unwrap() as usize;
                let end = row["locator"]["char_end"].as_u64().unwrap() as usize;
                let expected = page
                    .text
                    .trim()
                    .chars()
                    .skip(start)
                    .take(end - start)
                    .collect::<String>();
                assert_eq!(row["text"], json!(expected));
                for flag in &mut covered[start..end] {
                    *flag = true;
                }
                assembled.push_str(row["text"].as_str().unwrap());
            }
            assert!(page
                .text
                .trim()
                .chars()
                .zip(covered)
                .all(|(c, hit)| c.is_whitespace() || hit));
            assert!(!assembled.is_empty());
        }
        assert_eq!(
            text.iter()
                .filter(|r| r["locator"]["block_ids"].as_array().unwrap().len() != 1)
                .count(),
            0
        );
        assert_eq!(doc.search_metadata["aasm_pdf"]["table_count"], Value::Null);
        assert_eq!(
            doc.search_metadata["aasm_pdf"]["table_geometry_verified"],
            false
        );
    }

    #[test]
    fn pdf_rejects_missing_identity_blank_pages_and_finite_bounds_without_truncation() {
        let source = parsed();
        let digest = "a".repeat(64);
        let mut pages = synthetic_pages();
        pages[0].text = "Synthetic unrelated work".into();
        assert!(pdf_document(&source, &digest, pages).is_err());
        let mut pages = synthetic_pages();
        pages[1].text = " \n\r ".into();
        assert!(pdf_document(&source, &digest, pages).is_err());
        let mut pages = synthetic_pages();
        pages[1].width_points = f32::NAN;
        assert!(pdf_document(&source, &digest, pages).is_err());
        assert!(pdf_document(&source, &digest, vec![]).is_err());
        assert!(pdf_document(&source, "not-a-digest", synthetic_pages()).is_err());
        let pages = vec![synthetic_pages().remove(0); PDF_PAGES + 1];
        assert!(pdf_document(&source, &digest, pages).is_err());
        let mut pages = synthetic_pages();
        pages[1].text = "x".repeat(PAGE_TEXT_BYTES + 1);
        assert!(pdf_document(&source, &digest, pages).is_err());
        let mut page = synthetic_pages().remove(0);
        page.text
            .push_str(&"x".repeat(PAGE_TEXT_BYTES - page.text.len()));
        assert!(pdf_document(&source, &digest, vec![page; 9]).is_err());
        assert!(pdf_pages(b"<html>PDF download viewer</html>").is_err());
        let mut oversized = b"%PDF-1.4\n".to_vec();
        oversized.resize(PDF_BYTES + 1, 0);
        assert!(pdf_pages(&oversized).is_err());
    }

    #[test]
    fn pdf_content_snapshot_and_plan_bind_bytes_metadata_pages_and_rendition_identity() {
        let src = test_source();
        let original = pdf_fixture_doc();
        let row = pdf_plan(&src, &original, &None, src.catalog_url.as_deref().unwrap()).unwrap();
        validate_pdf_plan(&src, &row, &original).unwrap();
        assert_eq!(row.version, None);
        assert!(row.doc_key.contains("-pdf-"));
        assert_eq!(row.extra["work_key"], json!(common::short_hash(LEAF)));
        let before = pdf_snapshot(&original).unwrap();
        let mut changed = original.clone();
        changed.fetches.push(Default::default());
        assert_eq!(pdf_snapshot(&changed).unwrap(), before); // Transport time/cache state are not content.
        changed.search_metadata["aasm_pdf"]["transport_byte_cap"] = json!(PDF_BYTES);
        assert_eq!(pdf_snapshot(&changed).unwrap(), before);
        changed = original.clone();
        changed.blocks[1].text.push_str(" changed");
        assert!(validate_pdf_plan(&src, &row, &changed).is_err());
        changed = original.clone();
        changed.search_metadata["aasm_pdf"]["source_pdf_sha256"] = json!("b".repeat(64));
        assert!(validate_pdf_plan(&src, &row, &changed).is_err());
        changed = original.clone();
        changed.attachments[0]["html_rights"]["note"] = json!("Changed source notice");
        assert!(validate_pdf_plan(&src, &row, &changed).is_err());
        for field in [
            "source_id",
            "doc_key",
            "title",
            "url",
            "rendition",
            "version",
            "work_key",
            "source_evidence_url",
        ] {
            let mut value = serde_json::to_value(&row).unwrap();
            if matches!(field, "work_key" | "source_evidence_url") {
                value["extra"][field] = json!("wrong");
            } else {
                value[field] = json!("wrong");
            }
            let bad: PlanRow = serde_json::from_value(value).unwrap();
            assert!(validate_pdf_plan(&src, &bad, &original).is_err(), "{field}");
        }
    }

    #[test]
    fn pdfium_reads_every_page_of_genuine_synthetic_pdf_with_identity_and_tail_text() {
        let pages = pdf_pages(SYNTHETIC_PDF).unwrap();
        assert_eq!(pages.len(), 2);
        assert!(expected_doi(&pages[0].text));
        assert!(pages[1].text.contains("Synthetic final page tail 2099"));
        assert!(pages
            .iter()
            .all(|p| p.width_points == 612.0 && p.height_points == 792.0));
        let doc =
            pdf_document(&parsed(), &crate::common::sha256_hex(SYNTHETIC_PDF), pages).unwrap();
        assert_eq!(doc.search_metadata["aasm_pdf"]["physical_page_count"], 2);
        // A declared page load failure must reject the entire rendition, including
        // a missing final page. No accepted prefix may be returned as complete.
        for failed_index in [1, 2] {
            let mut visited = vec![];
            let result = collect_pdf_pages(3, |index| {
                visited.push(index);
                if index == failed_index {
                    bail!("synthetic indexed page load failure")
                }
                Ok(synthetic_pages().remove(0))
            });
            assert!(result.is_err());
            assert_eq!(visited, (0..=failed_index).collect::<Vec<_>>());
        }
        let mut visited = vec![];
        let complete = collect_pdf_pages(3, |index| {
            visited.push(index);
            Ok(synthetic_pages().remove(0))
        })
        .unwrap();
        assert_eq!(visited, [0, 1, 2]);
        assert_eq!(complete.len(), 3);
    }

    #[tokio::test]
    async fn pdf_discovery_extract_limits_and_byte_or_publisher_drift_are_source_bound() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, PDF_BYTES as u64, "").unwrap();
        let src = test_source();
        f.seed_cache_for_tests(LEAF, PAGE.as_bytes(), "text/html");
        f.seed_cache_for_tests(PDF, SYNTHETIC_PDF, "application/pdf");
        assert!(AasmSleep
            .discover(&src, &f, Some(0))
            .await
            .unwrap()
            .is_empty());
        let limited = AasmSleep.discover(&src, &f, Some(1)).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].rendition.as_deref(), Some("html"));
        let rows = AasmSleep.discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].rendition.as_deref(), Some("pdf"));
        assert_ne!(rows[0].doc_key, rows[1].doc_key);
        let doc = AasmSleep.extract(&src, &f, &rows[1]).await.unwrap();
        assert_eq!(doc.doc_key, rows[1].doc_key);
        assert_eq!(doc.url, PDF);
        assert_eq!(doc.fetches.len(), 2);
        assert_eq!(doc.fetches[0].url, LEAF);
        assert_eq!(doc.fetches[1].url, PDF);
        assert_eq!(
            doc.search_metadata["aasm_pdf"]["transport_byte_cap"],
            PDF_BYTES as u64
        );
        assert_eq!(
            doc.blocks
                .iter()
                .map(|b| b.page.unwrap())
                .collect::<Vec<_>>(),
            [1, 2]
        );
        let stable = AasmSleep.discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(stable[1].doc_key, rows[1].doc_key);
        let mut changed = SYNTHETIC_PDF.to_vec();
        changed.extend_from_slice(b"\n% Changed acquisition bytes\n");
        f.seed_cache_for_tests(PDF, &changed, "application/pdf");
        assert!(AasmSleep.extract(&src, &f, &rows[1]).await.is_err());
        f.seed_cache_for_tests(PDF, SYNTHETIC_PDF, "application/pdf");
        f.seed_cache_for_tests(
            LEAF,
            PAGE.replace("Synthetic third-party", "Changed third-party")
                .as_bytes(),
            "text/html",
        );
        assert!(AasmSleep.extract(&src, &f, &rows[1]).await.is_err());
        f.seed_cache_for_tests(
            LEAF,
            PAGE.replace(DOI, "10.1007/unrelated").as_bytes(),
            "text/html",
        );
        assert!(AasmSleep.extract(&src, &f, &rows[1]).await.is_err());
        f.seed_cache_for_tests(LEAF, PAGE.as_bytes(), "text/html");
        f.seed_cache_for_tests(
            PDF,
            b"<html>Download viewer, not PDF bytes</html>",
            "text/html",
        );
        let failed = AasmSleep.discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(failed[1].url, PDF);
        assert!(failed[1].extra["discovery_error"].is_string());
        assert!(AasmSleep.extract(&src, &f, &failed[1]).await.is_err());
    }
    #[test]
    fn native_recommendations_abstract_remark_and_no_invented_figures() {
        let doc = parsed();
        let recs: Vec<_> = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .collect();
        assert_eq!(recs.len(), 2);
        assert_eq!(
            recs.iter().map(|b| b.block_id.as_str()).collect::<Vec<_>>(),
            ["Sec5", "Sec7"]
        );
        assert!(
            matches!(&recs[0].kind, BlockKind::Recommendation { label: Some(label), .. } if label == "Recommendation 1")
        );
        assert!(content(
            &PAGE.replace(
                "Recommendation 1: Synthetic recommendation",
                "1. Synthetic recommendation"
            ),
            &Url::parse(LEAF).unwrap()
        )
        .is_err());
        assert!(recs.iter().all(|b| matches!(&b.kind, BlockKind::Recommendation { strength: Some(s), certainty: Some(c), .. } if s == "Conditional recommendation" && c == "low certainty of evidence")));
        assert!(recs[1].heading_path.iter().any(|p| p.contains("against")));
        let remark = doc
            .blocks
            .iter()
            .find(|b| b.text == "Synthetic remark belonging to recommendation 2.")
            .unwrap();
        assert!(remark
            .heading_path
            .iter()
            .any(|p| p.starts_with("Recommendation 2:")));
        assert!(doc
            .blocks
            .iter()
            .any(|b| b.block_id.starts_with("abstract-") && b.text.starts_with("1.")));
        assert!(!doc
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Figure { .. })));
        assert_eq!(doc.doi.as_deref(), Some("10.1007/s44470-025-00038-8"));
        assert_eq!(doc.published.as_deref(), Some("2026-04-13"));
        assert!(doc.date_observations.iter().any(|d| d.raw == "2026/12"));
        assert!(doc.rights.note.unwrap().contains("third-party"));
    }
    #[test]
    fn linked_table_notes_spans_and_semantic_drift() {
        let (blocks, asset) = table_content(
            TABLE,
            &Url::parse(&format!("{LEAF}/tables/1")).unwrap(),
            "Tab1",
            "Table 1",
        )
        .unwrap();
        assert!(blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Table { .. }) && b.text.contains("Group A")));
        assert!(blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Footnote) && b.text.contains("Synthetic note")));
        assert!(asset["table_html"]
            .as_str()
            .unwrap()
            .contains("colspan=\"3\""));
        let doc = parsed();
        let before = snapshot(&doc).unwrap();
        assert_eq!(
            before,
            snapshot(
                &content(
                    &PAGE.replace("Navigation A", "Navigation B"),
                    &Url::parse(LEAF).unwrap()
                )
                .unwrap()
                .doc
            )
            .unwrap()
        );
        let mut changed = parsed();
        changed.blocks[0].text.push_str(" changed");
        assert_ne!(before, snapshot(&changed).unwrap());
        changed = parsed();
        changed.attachments.push(asset.clone());
        let table_before = snapshot(&changed).unwrap();
        changed.attachments.last_mut().unwrap()["table_html"] = json!("changed cells");
        assert_ne!(table_before, snapshot(&changed).unwrap());
    }
    #[tokio::test]
    async fn explicit_seed_survives_independent_catalog_failure_and_detects_drift() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        let cat = "https://aasm.org/clinical-resources/practice-standards/practice-guidelines/";
        let mut src: SourceEntry = serde_json::from_value(json!({"id":"aasm_sleep","access":"public","enabled":true,"catalog_url":cat,"config":{"source_linked_documents":[{"url":LEAF,"source_evidence_url":cat}]}})).unwrap();
        f.seed_cache_for_tests(LEAF, PAGE.as_bytes(), "text/html");
        for n in [1, 2] {
            f.seed_cache_for_tests(&format!("{LEAF}/tables/{n}"), TABLE.as_bytes(), "text/html");
        }
        let rows = AasmSleep.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].extra["catalog_error"].is_string());
        assert!(rows[1].extra["discovery_error"].is_string());
        assert_eq!(rows[1].rendition.as_deref(), Some("pdf"));
        assert_eq!(rows[2].url, cat);
        assert!(rows[2].extra["discovery_error"].is_string());
        let doc = AasmSleep.extract(&src, &f, &rows[0]).await.unwrap();
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
                .count(),
            2
        );
        assert_eq!(doc.fetches.len(), 3);
        f.seed_cache_for_tests(
            &format!("{LEAF}/tables/1"),
            TABLE.replace("Group A", "Group changed").as_bytes(),
            "text/html",
        );
        assert!(AasmSleep.extract(&src, &f, &rows[0]).await.is_err());
        src.config = json!({});
        let without = AasmSleep.discover(&src, &f, None).await.unwrap();
        assert_eq!(without.len(), 1);
        assert_eq!(without[0].url, cat);
        let catalog_page = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/aasm_sleep/catalog.html"
        ));
        assert_eq!(
            catalog(catalog_page, &Url::parse(cat).unwrap()).unwrap()[0].0,
            LEAF
        );
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod fix_round1_catalog_routes {
    use super::*;
    const MAIN: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/aasm_sleep/catalog-main.html"));
    const PDF_BYTES_FIXTURE: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/aasm_sleep/physical-pages-synthetic.pdf"));
    const CAT: &str = "https://aasm.org/clinical-resources/practice-standards/practice-guidelines/";
    #[test]
    fn genuine_catalog_routes_hosted_pdfs_dois_and_pmcids() {
        let items = catalog_items(MAIN, &Url::parse(CAT).unwrap()).unwrap();
        // Society-hosted PDF beside a stated JCSM DOI.
        let csa = items.iter().find(|i| i.url == "https://doi.org/10.5664/jcsm.11858").unwrap();
        assert_eq!(csa.label, "Guideline");
        assert_eq!(csa.doi.as_deref(), Some("10.5664/jcsm.11858"));
        assert_eq!(csa.pdf.as_deref(), Some("https://aasm.org/wp-content/uploads/2026/02/central-sleep-apnea-guideline-AASM-2025.pdf"));
        assert_eq!(csa.topic, "Treatment of Central Sleep Apnea Syndromes in Adults");
        assert!(csa.title.as_deref().is_some_and(|t| t.starts_with("Treatment of central sleep apnea in adults")));
        // The primary link itself is a society PDF.
        let ext = items.iter().find(|i| i.url.ends_with("/PP_CircadianRhythm.pdf")).unwrap();
        assert_eq!(ext.pdf.as_deref(), Some(ext.url.as_str()));
        assert!(ext.doi.is_none() && ext.pmcid.is_none());
        // PMC-only items keep the PMCID the catalog states; no PDF is invented.
        let rls = items.iter().find(|i| i.url == "https://pmc.ncbi.nlm.nih.gov/articles/PMC11701286/").unwrap();
        assert_eq!(rls.pmcid.as_deref(), Some("PMC11701286"));
        assert!(rls.pdf.is_none());
        // Edition-prefixed labels are kept verbatim; reviews, members-only and patient material are excluded.
        assert!(items.iter().any(|i| i.label == "2005 Guideline" && i.url.ends_with("/PP_MSLTMWT.pdf")));
        assert!(items.iter().any(|i| i.label == "Guidance Statement"));
        assert!(!items.iter().any(|i| ["Review", "Patient Guide", "Guideline-at-a-Glance", "Podcast"].contains(&i.label.as_str())));
        assert!(!items.iter().any(|i| i.url.contains("members-only") || i.url.contains("Review_")));
        assert!(!items.iter().any(|i| i.url == "https://doi.org/10.5664/jcsm.11860"));
        assert!(items.len() >= 20, "{}", items.len());
    }
    #[test]
    fn printed_grades_only_on_numbered_aasm_statements() {
        let mk = |id: &str, t: &str| Block { block_id: id.into(), kind: BlockKind::Paragraph, text: t.into(), heading_path: vec!["H".into()], page: None };
        // Texts as converted from the genuine society PDFs (PP_CircadianRhythm, PP_MSLTMWT) plus current GRADE wording.
        let mut b = vec![
            mk("a", "1. The AASM suggests that clinicians use transvenous phrenic nerve stimulation to treat central sleep apnea in adults. (CONDITIONAL)"),
            mk("b", "<u>3.1.1 Use of a sleep log or diary is indicated in the assessment of patients with a suspected CRSD. (Guideline)</u> This recommendation was determined by inclusion of the use of sleep logs."),
            mk("c", "**1. The MSLT is indicated as part of the evaluation of patients with** **suspected narcolepsy to confirm the diagnosis. [6.2.1; 6.2.2]** **(Standard)** The usefulness of the mean sleep latency value"),
            mk("d", "phase markers are useful to determine circadian phase but there is insufficient evidence to recommend their routine use (Option). Additionally,"),
            mk("e", "3. The AASM suggests clinicians use something without a printed grade."),
        ];
        assert_eq!(hosted_pdf_recommendations(&mut b), (3, 1));
        assert_eq!(b.len(), 8);
        assert!(matches!(&b[1].kind, BlockKind::Recommendation { label: Some(l), strength: Some(s), certainty: None, .. } if l == "1" && s == "CONDITIONAL"));
        assert!(matches!(&b[3].kind, BlockKind::Recommendation { label: Some(l), strength: Some(s), .. } if l == "3.1.1" && s == "Guideline"));
        assert_eq!(b[3].text, "Use of a sleep log or diary is indicated in the assessment of patients with a suspected CRSD. (Guideline)");
        assert!(matches!(&b[5].kind, BlockKind::Recommendation { strength: Some(s), .. } if s == "Standard"));
        assert!(b[5].text.contains("suspected narcolepsy to confirm the diagnosis. [6.2.1; 6.2.2]") && !b[5].text.contains("**"));
        assert!(matches!(b[2].kind, BlockKind::Paragraph) && b[2].text.contains("This recommendation was determined"));
        assert!(matches!(b[6].kind, BlockKind::Paragraph) && matches!(b[7].kind, BlockKind::Paragraph));
    }
    #[tokio::test]
    async fn hosted_pdf_route_plans_binds_bytes_and_defers_pmc_items() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 4 << 20, "").unwrap();
        let src: SourceEntry = serde_json::from_value(json!({"id":"aasm_sleep","access":"public","enabled":true,"catalog_url":CAT})).unwrap();
        f.seed_cache_for_tests(CAT, MAIN.as_bytes(), "text/html");
        let first_pdf = "https://aasm.org/wp-content/uploads/2017/07/PP_CircadianRhythm.pdf";
        f.seed_cache_for_tests(first_pdf, PDF_BYTES_FIXTURE, "application/pdf");
        let rows = AasmSleep.discover(&src, &f, Some(3)).await.unwrap();
        assert_eq!(rows.len(), 3);
        let r0 = &rows[0];
        assert_eq!(r0.url, first_pdf);
        assert_eq!(r0.extra["route"], "aasm_hosted_pdf");
        assert!(r0.extra["pdf_sha256"].is_string() && r0.extra["discovery_error"].is_null());
        assert_eq!(r0.extra["topic"], "Evaluation and Treatment of Extrinsic CRSWDs");
        // Uncached PDFs fail offline and are recorded, not dropped.
        assert!(rows[1].extra["discovery_error"].is_string());
        let doc = AasmSleep.extract(&src, &f, r0).await.unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("clinical_practice_guideline"));
        assert!(!doc.blocks.is_empty() && doc.doi.is_none() && doc.published.is_none());
        let all = AasmSleep.discover(&src, &f, None).await.unwrap();
        let rls = all.iter().find(|r| r.url == "https://pmc.ncbi.nlm.nih.gov/articles/PMC11701286/").unwrap();
        assert_eq!(rls.extra["route"], "pmc_crosswalk");
        assert_eq!(rls.extra["pmcid"], "PMC11701286");
        assert!(AasmSleep.extract(&src, &f, rls).await.is_err());
        let csa = all.iter().find(|r| r.url.ends_with("central-sleep-apnea-guideline-AASM-2025.pdf")).unwrap();
        assert_eq!(csa.extra["doi"], "10.5664/jcsm.11858");
        // The stated DOI of a society-PDF item is tried in the crosswalk too (offline: lookup failure recorded).
        assert_eq!(csa.extra["pmc_crosswalk_results"][0]["id"], "10.5664/jcsm.11858");
        assert!(rls.extra["discovery_error"].as_str().unwrap().contains("PMC crosswalk lookup_failed"));
        assert_eq!(rls.extra["pmc_crosswalk_results"][0]["id"], "PMC11701286");
        // Deferred identifier-only rows come after every resolvable row.
        let first_deferred = all.iter().position(|r| r.extra["route"] == "pmc_crosswalk").unwrap();
        assert!(all[first_deferred..].iter().all(|r| r.extra["route"] != "aasm_hosted_pdf"));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod fix_round2_crosswalk {
    use super::*;
    const MAIN: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/aasm_sleep/catalog-main.html"));
    const NOT_OA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/pmc_crosswalk/oai-not-oa.xml"));
    const CAT: &str = "https://aasm.org/clinical-resources/practice-standards/practice-guidelines/";
    /// A catalog-stated PMCID that PMC serves only as a free page (OAI cannotDisseminateFormat) stays unresolved with a
    /// relation-only PMC link and the outcome; nothing is taken from PMC.
    #[tokio::test]
    async fn stated_pmcid_not_distributed_is_relation_only() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 4 << 20, "").unwrap();
        let src: SourceEntry = serde_json::from_value(json!({"id":"aasm_sleep","access":"public","enabled":true,"catalog_url":CAT})).unwrap();
        f.seed_cache_for_tests(CAT, MAIN.as_bytes(), "text/html");
        f.seed_cache_for_tests(&cw::oai_url("PMC11701286").unwrap(), NOT_OA, "text/xml");
        let all = AasmSleep.discover(&src, &f, None).await.unwrap();
        let rls = all.iter().find(|r| r.url == "https://pmc.ncbi.nlm.nih.gov/articles/PMC11701286/").unwrap();
        assert!(!cw::is_crosswalk_row(rls));
        let e = rls.extra["discovery_error"].as_str().unwrap();
        assert!(e.contains("PMC crosswalk relation_only") && e.contains("does not distribute"), "{e}");
        assert_eq!(rls.extra["pmc_relations"][0]["target"], "pmc:PMC11701286");
        assert_eq!(rls.extra["pmc_relations"][0]["rel"], "related");
        assert_eq!(rls.extra["pmc_crosswalk_results"][0]["status"], "relation_only");
        assert!(AasmSleep.extract(&src, &f, rls).await.is_err());
        // An OAI request that fails (here: not cached offline) is a lookup failure, never a relation.
        let other = all.iter().find(|r| r.url == "https://pmc.ncbi.nlm.nih.gov/articles/PMC8636351/").unwrap();
        assert_eq!(other.extra["pmc_crosswalk_results"][0]["status"], "lookup_failed");
        assert!(other.extra["pmc_relations"].is_null());
    }
}
