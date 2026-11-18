//! ACG source-labelled highlights and catalogued READ destinations have distinct roles.
//! READ-only cards are retained as unresolved primary plans, never acquired bodies.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::{Fetched, Fetcher};
use crate::guidelines::html::{sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::{json, Value};
use url::Url;

pub struct Acg;
const MAX_FOOTER_LINKS: usize = 1024;
const MAX_TERMINAL_LINKS: usize = 512;
const MAX_CATALOG_METADATA_BYTES: usize = 512 * 1024;
const MAX_PLAN_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_HREF_BYTES: usize = 8192;

#[derive(Clone)]
struct Highlight {
    url: String,
    cards: Vec<usize>,
}
struct Primary {
    url: String,
    cards: Vec<usize>,
}
struct Catalog {
    cards: Vec<Value>,
    highlights: Vec<Highlight>,
    primaries: Vec<Primary>,
}

// Keep safe source literals; rejected hrefs have only a hash/length/reason.
// No usable replacement target is made from an unsafe or empty href.
fn href_metadata(base: &Url, href: &str) -> Value {
    let trimmed = href.trim();
    let reject = |reason: &str| json!({"native_href":null,"url":null,"rejected_href":{"sha256":crate::common::sha256_hex(href.as_bytes()),"bytes":href.len(),"reason":reason}});
    if href.chars().any(char::is_control) {
        return reject("control_character");
    }
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return json!({"native_href":href,"url":null,"nonterminal_reason":if trimmed.is_empty(){"empty_href"}else{"fragment_only_href"}});
    }
    if trimmed.starts_with("://") {
        return reject("malformed_href");
    }
    if let Some((scheme, rest)) = trimmed.split_once(':') {
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            let Some(authority_and_path) = rest.strip_prefix("//") else {
                return reject("malformed_href");
            };
            let authority = authority_and_path
                .split(['/', '?', '#'])
                .next()
                .unwrap_or("");
            if authority.is_empty() || authority.contains('\\') {
                return reject("malformed_href");
            }
        }
    }
    let Ok(url) = base.join(trimmed) else {
        return reject("malformed_href");
    };
    if !url.username().is_empty() || url.password().is_some() {
        return reject("unsafe_userinfo");
    }
    if !matches!(url.scheme(), "http" | "https") {
        return reject("unsupported_scheme");
    }
    json!({"native_href":href,"url":url.as_str()})
}
#[cfg(test)]
fn source_link(base: &Url, href: &str) -> Option<String> {
    href_metadata(base, href)["url"].as_str().map(String::from)
}

fn catalog_roles(html: &str, base: &Url) -> Result<Catalog> {
    let d = Html::parse_document(html);
    let mut result = Catalog {
        cards: vec![],
        highlights: vec![],
        primaries: vec![],
    };
    let mut footer_links_seen = 0usize;
    let mut terminal_links_seen = 0usize;
    let mut metadata_bytes = 0usize;
    for (ordinal, card) in d.select(&sel(".grid-item .card")).enumerate() {
        if ordinal >= 4096 {
            bail!("ACG catalog card guard exceeded; enumeration unresolved")
        }
        let heading = card.select(&sel(".card-block h3 a[href]")).next();
        let title = heading
            .as_ref()
            .map(text_of)
            .or_else(|| {
                card.select(&sel(".card-block h3"))
                    .next()
                    .map(|n| text_of(&n))
            })
            .unwrap_or_default();
        let heading_href = heading.as_ref().and_then(|n| n.value().attr("href"));
        if heading_href.is_some_and(|href| href.len() > MAX_HREF_BYTES) {
            bail!("ACG href byte guard exceeded; catalog unresolved")
        }
        let heading_metadata = heading_href
            .map(|href| href_metadata(base, href))
            .unwrap_or(Value::Null);
        let heading_url = heading_metadata["url"].as_str().map(String::from);
        let date = card
            .select(&sel(".card-block p.date"))
            .next()
            .map(|n| text_of(&n));
        let source_card_text = card
            .select(&sel(".card-block"))
            .next()
            .map(|n| text_of(&n))
            .unwrap_or_default();
        let grid = card
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|n| n.value().classes().any(|c| c == "grid-item"));
        let mut footer = vec![];
        for anchor in card.select(&sel(".card-footer a[href]")) {
            footer_links_seen += 1;
            if footer_links_seen > MAX_FOOTER_LINKS {
                bail!("ACG footer link guard exceeded; catalog unresolved")
            }
            let href = anchor.value().attr("href").unwrap_or_default();
            if href.len() > MAX_HREF_BYTES {
                bail!("ACG href byte guard exceeded; catalog unresolved")
            }
            let mut descriptor = href_metadata(base, href);
            if descriptor["url"].is_string() {
                terminal_links_seen += 1;
                if terminal_links_seen > MAX_TERMINAL_LINKS {
                    bail!("ACG terminal link guard exceeded; catalog unresolved")
                }
            }
            descriptor["native_label"] = json!(text_of(&anchor));
            footer.push(descriptor);
        }
        let paragraphs: Vec<Value> = card
            .select(&sel(".card-block p"))
            .map(|p| json!({"native_class":p.value().attr("class"),"text":text_of(&p)}))
            .collect();
        let card_metadata = json!({"native_card_ordinal":ordinal+1,"source_url":base.as_str(),"native_grid_classes":grid.map(|n| n.value().classes().map(String::from).collect::<Vec<_>>()).unwrap_or_default(),"native_card_classes":card.value().classes().collect::<Vec<_>>(),"title":title,"heading_href":heading_metadata["native_href"],"heading_href_rejected":heading_metadata["rejected_href"],"heading_url":heading_url,"source_date":{"selector":".card-block p.date","raw":date,"scope":"catalog card only; not a clinical edition/publication/update date"},"source_card_text":source_card_text,"source_card_html_sha256":crate::common::sha256_hex(card.html().as_bytes()),"native_paragraphs":paragraphs,"footer_links":footer});
        metadata_bytes = metadata_bytes
            .checked_add(serde_json::to_vec(&card_metadata)?.len())
            .context("ACG catalog metadata overflow")?;
        if metadata_bytes > MAX_CATALOG_METADATA_BYTES {
            bail!("ACG catalog metadata byte guard exceeded; enumeration unresolved")
        }
        result.cards.push(card_metadata);
        for link in &footer {
            let Some(url) = link["url"].as_str() else {
                continue;
            };
            let label = link["native_label"].as_str().unwrap_or_default();
            if label.eq_ignore_ascii_case("READ") {
                if let Some(primary) = result.primaries.iter_mut().find(|p| p.url == url) {
                    if !primary.cards.contains(&ordinal) {
                        primary.cards.push(ordinal);
                    }
                } else {
                    result.primaries.push(Primary {
                        url: url.into(),
                        cards: vec![ordinal],
                    });
                }
            }
            // Keep the existing exact source Highlight-label rule and heading association.
            if label != "Highlight" {
                continue;
            }
            let Some(article) = heading_url.as_ref() else {
                continue;
            };
            let source_cards = &result.cards;
            if let Some(highlight) = result.highlights.iter_mut().find(|h| {
                h.url == url
                    && source_cards[h.cards[0]]["heading_url"].as_str() == Some(article.as_str())
            }) {
                if !highlight.cards.contains(&ordinal) {
                    highlight.cards.push(ordinal);
                }
            } else {
                result.highlights.push(Highlight {
                    url: url.into(),
                    cards: vec![ordinal],
                });
            }
        }
    }
    if result.cards.is_empty() {
        bail!("ACG catalog has no source cards: empty search response or unsupported template; coverage unresolved")
    }
    Ok(result)
}

#[cfg(test)]
fn catalog(html: &str, base: &Url) -> Result<Vec<(String, String, String, String, String)>> {
    let catalog = catalog_roles(html, base)?;
    Ok(catalog
        .highlights
        .iter()
        .map(|h| {
            let card = &catalog.cards[h.cards[0]];
            (
                h.url.clone(),
                card["title"].as_str().unwrap_or_default().into(),
                card["source_date"]["raw"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                card["heading_url"].as_str().unwrap_or_default().into(),
                card["source_card_text"].as_str().unwrap_or_default().into(),
            )
        })
        .collect())
}
fn plan_metadata_admission(catalog: &Catalog, limit: Option<usize>) -> Result<usize> {
    let sizes: Vec<_> = catalog
        .cards
        .iter()
        .map(|card| serde_json::to_vec(card).map(|raw| raw.len()))
        .collect::<std::result::Result<_, _>>()?;
    let catalog_bytes = serde_json::to_vec(&catalog.cards)?.len();
    let mut row_costs = vec![];
    for (cards, url) in catalog
        .highlights
        .iter()
        .map(|h| (&h.cards, &h.url))
        .chain(catalog.primaries.iter().map(|p| (&p.cards, &p.url)))
    {
        let own = cards.iter().try_fold(0usize, |total, index| {
            total
                .checked_add(sizes[*index])
                .context("ACG own-card metadata overflow")
        })?;
        // Own native cards, title/date/related URL repeats, bounded errors and
        // JSON field overhead. The full inventory is stored once, not per row.
        row_costs.push(
            own.checked_mul(4)
                .and_then(|n| n.checked_add(2 * url.len() + 32768))
                .context("ACG row metadata overflow")?,
        );
    }
    row_costs.sort_unstable_by(|a, b| b.cmp(a));
    let mut estimate = catalog_bytes
        .checked_mul(2)
        .and_then(|n| n.checked_add(16384))
        .context("ACG catalog metadata overflow")?;
    for bytes in row_costs.into_iter().take(limit.unwrap_or(usize::MAX)) {
        estimate = estimate
            .checked_add(bytes)
            .context("ACG plan metadata overflow")?;
    }
    if estimate > MAX_PLAN_METADATA_BYTES {
        bail!("ACG aggregate plan metadata budget exceeded before terminal requests; no truncation")
    }
    Ok(estimate)
}
fn own_cards(catalog: &Catalog, cards: &[usize]) -> Value {
    json!(cards.iter().map(|i| &catalog.cards[*i]).collect::<Vec<_>>())
}
fn bounded_error(error: String) -> String {
    if serde_json::to_vec(&error)
        .map(|raw| raw.len() <= 4096)
        .unwrap_or(false)
    {
        error
    } else {
        format!(
            "ACG highlight failure detail omitted: {} bytes, SHA256 {}",
            error.len(),
            crate::common::sha256_hex(error.as_bytes())
        )
    }
}
fn complete(f: &Fetched) -> bool {
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
        extra: json!({"discovery_error":bounded_error(error),"source_role":"highlight_unresolved","clinical_body_acquired":false}),
    }
}
fn highlight_error(
    src: &SourceEntry,
    catalog: &Catalog,
    highlight: &Highlight,
    error: String,
) -> PlanRow {
    let mut row = unresolved(src, highlight.url.clone(), error);
    row.extra["source_cards"] = own_cards(catalog, &highlight.cards);
    row.extra["primary_READ_urls"] = json!(read_links(catalog, &highlight.cards));
    row
}
fn read_links(catalog: &Catalog, cards: &[usize]) -> Vec<String> {
    catalog
        .primaries
        .iter()
        .filter(|p| p.cards.iter().any(|card| cards.contains(card)))
        .map(|p| p.url.clone())
        .collect()
}
fn primary_row(src: &SourceEntry, primary: &Primary, catalog: &Catalog) -> PlanRow {
    let cards: Vec<_> = primary
        .cards
        .iter()
        .map(|i| catalog.cards[*i].clone())
        .collect();
    PlanRow {
        source_id: src.id.clone(),
        doc_key: format!(
            "{}-primary-read-unresolved",
            common::short_hash(&primary.url)
        ),
        url: primary.url.clone(),
        title: catalog.cards[primary.cards[0]]["title"]
            .as_str()
            .map(String::from),
        rendition: None,
        version: None,
        extra: json!({"source_role":"primary_READ_destination","discovery_error":"ACG source-labelled READ destination is catalogued only; primary publisher extraction is not implemented and no publisher request was attempted","clinical_body_acquired":false,"source_cards":cards,"related_highlight_urls":catalog.highlights.iter().filter(|h|h.cards.iter().any(|i|primary.cards.contains(i))).map(|h|h.url.clone()).collect::<Vec<_>>(),"relation_scope":"Native READ/Highlight roles on the same catalog card; no primary body, edition or predecessor acquired","canonical_status":"unknown"}),
    }
}
fn coverage(
    catalog: &Catalog,
    rows: &[PlanRow],
    attempted: usize,
    successes: usize,
    failed: usize,
    limit: Option<usize>,
) -> Value {
    let selected_primary = rows
        .iter()
        .filter(|r| r.extra["source_role"] == "primary_READ_destination")
        .count();
    let selected_failed = rows
        .iter()
        .filter(|r| r.extra["source_role"] == "highlight_unresolved")
        .count();
    json!({"counts":{"source_cards":catalog.cards.len(),"highlight_candidates":catalog.highlights.len(),"READ_destinations":catalog.primaries.len(),"highlight_candidates_attempted":attempted,"supported_highlights_selected":successes,"highlight_failures":failed,"unsupported_primary_selected":selected_primary,"unsupported_primary_omitted_by_limit":catalog.primaries.len()-selected_primary,"failed_highlight_selected":selected_failed,"failed_highlight_omitted_by_limit":failed-selected_failed,"highlight_candidates_not_attempted_by_limit":catalog.highlights.len()-attempted,"output_rows":rows.len(),"output_limit":limit},"ordering":"Supported highlights in source order first; then unsupported READ destinations; then failed highlights. Unsupported primary rows never cause a publisher request.","catalog_enumeration_complete":false,"clinical_coverage_complete":false,"scope":"Only the fetched catalog DOM; no pagination/end-of-catalog proof. Native classes/date/update prose remain card-scoped; no currentness, predecessor or version inference."})
}

fn highlight_relations(src: &SourceEntry, row: &PlanRow) -> Vec<Relation> {
    row.extra["primary_READ_urls"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|target| *target != row.url).map(|target| Relation {
        rel: "summary_of".into(), target: target.into(),
        scope: Some("Source card labels separate Highlight and READ roles; primary content, edition equivalence and access not acquired".into()),
        evidence_url: row.extra["catalog_fetch"]["final_url"].as_str().map(String::from).or_else(||src.catalog_url.clone()),
    }).collect()
}

#[async_trait::async_trait]
impl Adapter for Acg {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let cat = f
            .get_ok(src.catalog_url.as_deref().context("ACG catalog missing")?)
            .await?;
        let base = Url::parse(&cat.record.final_url)?;
        let catalog = catalog_roles(&cat.text(), &base)?;
        if serde_json::to_vec(&cat.record)?.len() > 8192 {
            bail!("ACG catalog fetch receipt metadata guard exceeded")
        }
        let metadata_estimate = plan_metadata_admission(&catalog, limit)?;
        let mut rows = vec![];
        let mut errors = vec![];
        let mut attempted = 0;
        // Unselected failures retain no card/PlanRow clones. At most512
        // bounded error strings and source indices survive until selection.
        for (highlight_index, highlight) in catalog.highlights.iter().enumerate() {
            attempted += 1;
            let pdf = match f.get_ok(&highlight.url).await {
                Ok(pdf) if complete(&pdf) => pdf,
                Ok(_) => {
                    errors.push((
                        highlight_index,
                        "highlight PDF incomplete or unsupported".into(),
                    ));
                    continue;
                }
                Err(e) => {
                    errors.push((highlight_index, bounded_error(format!("{e:#}"))));
                    continue;
                }
            };
            let key = common::short_hash(&highlight.url);
            let source_card = &catalog.cards[highlight.cards[0]];
            rows.push(PlanRow { source_id:src.id.clone(), doc_key:format!("{key}-{}",&pdf.record.sha256[..12]), url:highlight.url.clone(), title:source_card["title"].as_str().map(String::from), rendition:Some("pdf".into()), version:None, extra:json!({"work_key":key,"source_date":source_card["source_date"]["raw"].as_str().unwrap_or_default(),"source_card_text":source_card["source_card_text"],"publisher_url":source_card["heading_url"],"pdf_sha256":pdf.record.sha256,"catalog_fetch":cat.record,"source_role":"guideline_highlights","source_cards":own_cards(&catalog,&highlight.cards),"source_card_ordinals":highlight.cards.iter().map(|i|i+1).collect::<Vec<_>>(),"primary_READ_urls":read_links(&catalog,&highlight.cards)}) });
            if limit.is_some_and(|n| rows.len() >= n) {
                break;
            }
        }
        let successes = rows.len();
        let failed = errors.len();
        let mut left = limit
            .map(|n| n.saturating_sub(rows.len()))
            .unwrap_or(usize::MAX);
        for primary in &catalog.primaries {
            if left == 0 {
                break;
            }
            rows.push(primary_row(src, primary, &catalog));
            left -= 1;
        }
        for (index, error) in errors.into_iter().take(left) {
            rows.push(highlight_error(
                src,
                &catalog,
                &catalog.highlights[index],
                error,
            ));
        }
        let mut metadata = coverage(&catalog, &rows, attempted, successes, failed, limit);
        metadata["source_catalog_sha256"] = json!(cat.record.sha256);
        metadata["inventory_plan_doc_key"] = json!(rows.first().map(|row| row.doc_key.clone()));
        metadata["metadata_admission_upper_bound_bytes"] = json!(metadata_estimate);
        let inventory = json!({"source_cards":catalog.cards,"READ_destinations":catalog.primaries.iter().map(|p|json!({"url":p.url,"source_role":"READ","source_card_ordinals":p.cards.iter().map(|i|i+1).collect::<Vec<_>>(),"clinical_body_acquired":false})).collect::<Vec<_>>()});
        for row in &mut rows {
            row.extra["catalog_discovery"] = metadata.clone();
            row.extra["catalog_fetch"] = json!(cat.record);
        }
        if let Some(first) = rows.first_mut() {
            first.extra["catalog_inventory"] = inventory;
        }
        if serde_json::to_vec(&rows)?.len() > MAX_PLAN_METADATA_BYTES {
            bail!("ACG final aggregate plan metadata budget exceeded; no rows emitted")
        }

        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(e) = row.extra["discovery_error"].as_str() {
            bail!("ACG source unresolved: {e}")
        }
        if row.extra["source_role"] == "primary_READ_destination" {
            bail!("ACG primary publisher extraction is not implemented; no publisher request attempted")
        }
        let pdf = f.get_ok(&row.url).await?;
        if !complete(&pdf)
            || row.extra["pdf_sha256"]
                .as_str()
                .is_some_and(|h| h != pdf.record.sha256)
        {
            bail!("ACG highlight is incomplete or changed")
        }
        let (blocks, mut warnings, _) = super::static_list::blocks_for(&pdf, None, 0).await?;
        if blocks.is_empty() {
            bail!("ACG highlight yielded no text")
        }
        warnings.push("Source-labelled highlight is a summary, not the full publisher guideline. READ destinations are retained as unsupported catalog roles, with bounded selection counts; no primary bodies are acquired by this adapter. This highlight contains a visual management algorithm and image/table layout; generic PDF conversion does not verify clinical flow, geometry, grades or figure text. Original source PDF is required for faithful review.".into());
        let mut fetches = vec![];
        if let Ok(r) = serde_json::from_value::<crate::guidelines::doc::FetchRecord>(
            row.extra["catalog_fetch"].clone(),
        ) {
            fetches.push(r)
        }
        fetches.push(pdf.record);
        let relations = highlight_relations(src, row);
        Ok(GuidelineDoc {
            source_id: src.id.clone(),
            doc_key: row.doc_key.clone(),
            work_key: row.extra["work_key"].as_str().map(String::from),
            title: row.title.clone().unwrap_or_default(),
            url: row.url.clone(),
            item_type: Some("guideline_highlights".into()),
            status: "unknown".into(),
            rights: Rights {
                status: "unknown".into(),
                note: Some("Highlight and publisher guideline have separate rights.".into()),
                ..Default::default()
            },
            search_metadata: json!({"source_catalog_date":row.extra["source_date"],"source_card_text":row.extra["source_card_text"],"publisher_url":row.extra["publisher_url"],"source_role":"guideline_highlights","source_cards":row.extra["source_cards"],"catalog_discovery":row.extra["catalog_discovery"],"catalog_inventory":row.extra["catalog_inventory"]}),
            relations,
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
    fn catalog_keeps_highlight_with_its_source_article_and_date() {
        let html = [
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acg/catalog-1.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/acg/catalog-2.html"
            )),
        ]
        .concat();
        let u = Url::parse("https://gi.org/guidelines/").unwrap();
        let rows = catalog(&html, &u).unwrap();
        assert!(rows.iter().any(|(u, t, d, a, card)| u
            == "https://gi.org/wp-content/uploads/2025/03/GPMC_Highlights.pdf"
            && t.contains("Gastric Premalignant")
            && d.contains("March 2025")
            && card.contains("Published March 12, 2025")
            && a.contains("diagnosis_and_management")));
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod genuine_page_gold_tests {
    use super::*;
    #[test]
    fn source_page_text_is_gold_for_labels_not_visual_algorithm_flow() {
        let pages: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/acg/gastric-highlight-page-text.json"
        )))
        .unwrap();
        let page = pages
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["physical_page"] == 1)
            .unwrap();
        let text = page["text"].as_str().unwrap();
        assert!(
            text.contains("Endoscopic")
                && text.contains("Histologic")
                && text.contains("Assessment")
        );
        assert!(
            text.contains("Non-Dysplastic GPMC Management")
                && text.contains("Dysplastic GPMC Management")
        );
        assert!(text.contains("Family history of GC in 1st degree relative"));
        let manifest: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/acg/gastric-highlight-excerpts.json"
        )))
        .unwrap();
        assert!(manifest
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["physical_page"] == 1
                && x["source_sha256"]
                    == "5c28e7cf60de6d5f5fbbee43b3632a9e8fcaaf015314ae034a213f74448ebe43"
                && x["under_200KB"] == false));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod read_discovery_tests {
    use super::*;
    const CATALOG: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acg/catalog-read-roles-synthetic.html"
    ));
    const URL: &str = "https://gi.org/guidelines/";
    const LIVER: &str = "http://www.doi.org/10.14309/ajg.0000000000004186";
    const CROHN: &str = "https://journals.lww.com/ajg/fulltext/2025/06000/acg_clinical_guideline__management_of_crohn_s.14.aspx?context=featuredarticles&collectionid=2";
    const PDF: &str = "https://gi.org/wp-content/uploads/2018/04/ACG_CrohnsHighight2025.pdf";
    fn source() -> SourceEntry {
        serde_json::from_value(
            json!({"id":"acg","adapter":"acg","access":"public","enabled":true,"catalog_url":URL}),
        )
        .unwrap()
    }
    fn parsed() -> Catalog {
        catalog_roles(CATALOG, &Url::parse(URL).unwrap()).unwrap()
    }
    fn card(title: &str, article: &str, footer: &str) -> String {
        format!("<div class='grid-item guideline'><div class='card'><div class='card-block'><h3><a href='{article}'>{title}</a></h3></div><div class='card-footer'>{footer}</div></div></div>")
    }
    #[test]
    fn read_only_native_http_locator_survives_without_becoming_a_body() {
        let catalog = parsed();
        let liver = catalog.primaries.iter().find(|p| p.url == LIVER).unwrap();
        let row = primary_row(&source(), liver, &catalog);
        assert_eq!(row.url, LIVER);
        assert_eq!(row.extra["source_role"], "primary_READ_destination");
        assert_eq!(row.extra["clinical_body_acquired"], false);
        assert!(row.extra["discovery_error"]
            .as_str()
            .unwrap()
            .contains("no publisher request"));
        assert!(row.extra["related_highlight_urls"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            row.extra["source_cards"][0]["source_date"]["raw"],
            "October 2026"
        );
        assert_eq!(row.version, None);
        assert_eq!(row.rendition, None);
    }
    #[test]
    fn read_highlight_and_podcast_keep_distinct_native_roles_and_heading_alias() {
        let catalog = parsed();
        assert_eq!(catalog.primaries.len(), 4);
        assert_eq!(catalog.highlights.len(), 1);
        let primary = catalog.primaries.iter().find(|p| p.url == CROHN).unwrap();
        assert_eq!(primary.cards, vec![1, 4]);
        assert_eq!(catalog.highlights[0].url, PDF);
        assert_eq!(
            catalog.cards[catalog.highlights[0].cards[0]]["heading_url"],
            CROHN
        );
        assert_eq!(
            read_links(&catalog, &catalog.highlights[0].cards),
            vec![CROHN.to_string()]
        );
        assert_eq!(
            catalog.cards[1]["footer_links"][1]["native_label"],
            "Podcast"
        );
        assert_eq!(
            catalog.cards[1]["footer_links"][1]["url"],
            "https://webfiles.gi.org/podcasts/ajg/long_july2025.mp3"
        );
        let row = primary_row(&source(), primary, &catalog);
        assert_eq!(row.extra["related_highlight_urls"], json!([PDF]));
        assert_ne!(row.doc_key, common::short_hash(PDF));
    }
    #[test]
    fn empty_progress_href_and_mixed_classes_remain_card_metadata() {
        let catalog = parsed();
        let progress = &catalog.cards[3];
        assert_eq!(progress["heading_href"], "");
        assert!(progress["heading_url"].is_null());
        assert!(progress["footer_links"][0]["url"].is_null());
        assert!(progress["source_date"]["raw"].is_null());
        assert!(progress["source_card_text"]
            .as_str()
            .unwrap()
            .contains("New - In Progress"));
        assert!(progress["native_grid_classes"]
            .as_array()
            .unwrap()
            .contains(&json!("Progress")));
        assert!(catalog.cards[2]["native_grid_classes"]
            .as_array()
            .unwrap()
            .contains(&json!("consensus-statements")));
        assert!(!catalog.primaries.iter().any(|p| p.url == URL));
        let base = Url::parse(URL).unwrap();
        for href in [
            "",
            "  ",
            "#",
            "#card",
            "javascript:;",
            "https://person:secret@example.org/",
        ] {
            assert!(source_link(&base, href).is_none());
        }
    }
    #[test]
    fn native_update_prose_does_not_invent_predecessor_edition_or_currentness() {
        let catalog = parsed();
        let crohn = primary_row(
            &source(),
            catalog.primaries.iter().find(|p| p.url == CROHN).unwrap(),
            &catalog,
        );
        assert!(crohn.extra["source_cards"][0]["source_card_text"]
            .as_str()
            .unwrap()
            .contains("Updated June 2025"));
        let focused = primary_row(
            &source(),
            catalog
                .primaries
                .iter()
                .find(|p| p.url.ends_with("1548"))
                .unwrap(),
            &catalog,
        );
        assert!(focused.extra["source_cards"][0]["source_card_text"]
            .as_str()
            .unwrap()
            .contains("Focused update to 2017"));
        for row in [crohn, focused] {
            assert_eq!(row.version, None);
            assert_eq!(row.extra["canonical_status"], "unknown");
            assert!(row.extra.get("relations").is_none());
            assert!(row.extra.get("predecessor_url").is_none());
            assert!(row.extra.get("published").is_none());
        }
    }
    #[test]
    fn exact_query_aliases_deduplicate_but_distinct_query_values_do_not() {
        let catalog = parsed();
        let exact = catalog.primaries.iter().find(|p| p.url == CROHN).unwrap();
        let variant = catalog
            .primaries
            .iter()
            .find(|p| p.url.ends_with("collectionid=3"))
            .unwrap();
        assert_eq!(exact.cards.len(), 2);
        assert_eq!(variant.cards.len(), 1);
        assert_ne!(
            primary_row(&source(), exact, &catalog).doc_key,
            primary_row(&source(), variant, &catalog).doc_key
        );
        assert_eq!(catalog.cards[1]["footer_links"][2]["native_href"], CROHN);
        assert_eq!(
            source_link(
                &Url::parse(URL).unwrap(),
                &(CROHN.to_owned() + "#observed-fragment")
            ),
            Some(CROHN.to_owned() + "#observed-fragment")
        );
    }
    #[tokio::test]
    async fn read_only_discovery_and_extract_fail_without_requesting_publisher() {
        let temp = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(temp.path(), true, 500, 1 << 20, "").unwrap();
        let html = card("Liver", LIVER, &format!("<a href='{LIVER}'>READ</a>"));
        fetcher.seed_cache_for_tests(URL, html.as_bytes(), "text/html");
        let rows = Acg.discover(&source(), &fetcher, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url, LIVER);
        let error = Acg
            .extract(&source(), &fetcher, &rows[0])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("primary publisher extraction is not implemented"));
        assert!(!error.contains("cache"));
        let mut without_error = rows[0].clone();
        without_error
            .extra
            .as_object_mut()
            .unwrap()
            .remove("discovery_error");
        let explicit_role_error = Acg
            .extract(&source(), &fetcher, &without_error)
            .await
            .unwrap_err()
            .to_string();
        assert!(explicit_role_error.contains("no publisher request attempted"));
        assert_eq!(
            rows[0].extra["catalog_discovery"]["counts"]["highlight_candidates_attempted"],
            0
        );
    }
    #[tokio::test]
    async fn supported_highlight_is_not_starved_by_read_or_highlight_errors_at_limit_one() {
        let temp = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(temp.path(), true, 500, 1 << 20, "").unwrap();
        let missing = card(
            "Synthetic unavailable highlight",
            LIVER,
            "<a href='https://gi.org/missing-highlight.pdf'>Highlight</a>",
        );
        fetcher.seed_cache_for_tests(URL, (missing + CATALOG).as_bytes(), "text/html");
        // Synthetic PDF framing tests discovery only; never clinical conversion.
        fetcher.seed_cache_for_tests(
            PDF,
            b"%PDF-1.4\nsynthetic framing only\n%%EOF\n",
            "application/pdf",
        );
        let rows = Acg.discover(&source(), &fetcher, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url, PDF);
        assert_eq!(rows[0].extra["source_role"], "guideline_highlights");
        let relations = highlight_relations(&source(), &rows[0]);
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].rel, "summary_of");
        assert_eq!(relations[0].target, CROHN);
        assert_eq!(relations[0].evidence_url.as_deref(), Some(URL));
        let coverage = &rows[0].extra["catalog_discovery"];
        assert_eq!(coverage["counts"]["highlight_candidates_attempted"], 2);
        assert_eq!(coverage["counts"]["highlight_failures"], 1);
        assert_eq!(coverage["counts"]["supported_highlights_selected"], 1);
        assert_eq!(
            coverage["counts"]["unsupported_primary_omitted_by_limit"],
            4
        );
        assert_eq!(
            rows[0].extra["catalog_inventory"]["READ_destinations"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(coverage["clinical_coverage_complete"], false);
    }
    #[tokio::test]
    async fn unsupported_rows_consume_output_limit_and_preserve_all_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(temp.path(), true, 500, 1 << 20, "").unwrap();
        fetcher.seed_cache_for_tests(URL, CATALOG.as_bytes(), "text/html");
        let rows = Acg.discover(&source(), &fetcher, Some(2)).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|r| r.extra["source_role"] == "primary_READ_destination"
                && r.extra["discovery_error"].is_string()));
        let coverage = &rows[0].extra["catalog_discovery"];
        assert_eq!(coverage["counts"]["unsupported_primary_selected"], 2);
        assert_eq!(
            coverage["counts"]["unsupported_primary_omitted_by_limit"],
            2
        );
        assert_eq!(coverage["counts"]["failed_highlight_omitted_by_limit"], 1);
        assert_eq!(
            rows[0].extra["catalog_inventory"]["READ_destinations"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        let unlimited = Acg.discover(&source(), &fetcher, None).await.unwrap();
        assert_eq!(unlimited.len(), 5);
        assert_eq!(
            unlimited
                .iter()
                .filter(|row| row.extra.get("catalog_inventory").is_some())
                .count(),
            1
        );
        assert!(unlimited.iter().all(|row| !row.extra["source_cards"]
            .as_array()
            .unwrap()
            .is_empty()
            && row.extra["catalog_discovery"]["inventory_plan_doc_key"] == unlimited[0].doc_key
            && row.extra["catalog_fetch"]["url"] == URL));
        assert_eq!(
            unlimited.last().unwrap().extra["source_role"],
            "highlight_unresolved"
        );
        assert_eq!(
            unlimited[0].extra["catalog_discovery"]["counts"]
                ["unsupported_primary_omitted_by_limit"],
            0
        );
    }
    #[tokio::test]
    async fn zero_limit_skips_catalog_and_empty_search_is_explicitly_unresolved() {
        let temp = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(temp.path(), true, 500, 1 << 20, "").unwrap();
        assert!(Acg
            .discover(&source(), &fetcher, Some(0))
            .await
            .unwrap()
            .is_empty());
        let error = catalog_roles(
            "<html><main>No cards</main></html>",
            &Url::parse("https://gi.org/guidelines/?search=synthetic").unwrap(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("empty search response"));
    }
    #[test]
    fn genuine_catalog_read_only_card_is_not_lost_with_unchanged_highlights() {
        let html = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/acg/catalog-1.html"
        ));
        let catalog = catalog_roles(html, &Url::parse(URL).unwrap()).unwrap();
        assert!(catalog.primaries.iter().any(|p| p.url == LIVER));
        assert!(catalog
            .highlights
            .iter()
            .any(|h| h.url == "https://gi.org/wp-content/uploads/2025/03/GPMC_Highlights.pdf"));
    }
    #[tokio::test]
    async fn unsafe_href_values_are_redacted_in_catalog_and_every_plan_metadata_field() {
        let base = Url::parse(URL).unwrap();
        let bad = [
            "https://unsafe-user:secret-marker@example.invalid/item",
            "http://[malformed-marker",
            "https://example.invalid/path\ncontrol-marker",
            "javascript:unsafe-script-marker()",
        ];
        let mut html = card(
            "Synthetic safe READ",
            LIVER,
            &format!("<a href='{LIVER}'>READ</a>"),
        );
        for href in bad {
            html.push_str(&card(
                "Synthetic unsafe href",
                href,
                &format!("<a href='{href}'>READ</a>"),
            ));
        }
        let catalog = catalog_roles(&html, &base).unwrap();
        assert_eq!(catalog.primaries.len(), 1);
        assert!(catalog.highlights.is_empty());
        for source_card in catalog.cards.iter().skip(1) {
            assert!(source_card["heading_href"].is_null());
            assert!(source_card["heading_url"].is_null());
            assert!(source_card["heading_href_rejected"]["sha256"].is_string());
            assert!(source_card["footer_links"][0]["native_href"].is_null());
            assert!(source_card["footer_links"][0]["url"].is_null());
            assert!(source_card["footer_links"][0]["rejected_href"]["reason"].is_string());
        }
        let temp = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(temp.path(), true, 500, 1 << 20, "").unwrap();
        fetcher.seed_cache_for_tests(URL, html.as_bytes(), "text/html");
        let rows = Acg.discover(&source(), &fetcher, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].extra["catalog_inventory"]["source_cards"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        let serialized = serde_json::to_string(&rows).unwrap();
        for marker in [
            "unsafe-user",
            "secret-marker",
            "malformed-marker",
            "control-marker",
            "unsafe-script-marker",
        ] {
            assert!(!serialized.contains(marker));
        }
        let valid = href_metadata(&base, CROHN);
        assert_eq!(valid["native_href"], CROHN);
        assert_eq!(valid["url"], CROHN);
        assert_eq!(href_metadata(&base, "")["native_href"], "");
    }
    #[test]
    fn footer_and_terminal_limits_reject_overflow_without_truncation() {
        let base = Url::parse(URL).unwrap();
        let footer = "<a href=''>READ</a>".repeat(MAX_FOOTER_LINKS + 1);
        let error = catalog_roles(&card("Synthetic footer overflow", LIVER, &footer), &base)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("footer link guard exceeded"));
        let footer = format!("<a href='{LIVER}'>READ</a>").repeat(MAX_TERMINAL_LINKS + 1);
        let error = catalog_roles(&card("Synthetic terminal overflow", LIVER, &footer), &base)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("terminal link guard exceeded"));
    }
    #[test]
    fn native_catalog_metadata_byte_limit_is_explicit_not_truncated() {
        let base = Url::parse(URL).unwrap();
        let html = card(
            &"x".repeat(MAX_CATALOG_METADATA_BYTES + 1),
            LIVER,
            &format!("<a href='{LIVER}'>READ</a>"),
        );
        let error = catalog_roles(&html, &base).err().unwrap().to_string();
        assert!(error.contains("catalog metadata byte guard exceeded"));
    }
    #[tokio::test]
    async fn aggregate_plan_metadata_budget_is_checked_before_terminal_fetches() {
        let base = Url::parse(URL).unwrap();
        let mut footer =
            String::from("<a href='https://gi.org/unseeded-highlight.pdf'>Highlight</a>");
        for index in 0..200 {
            footer.push_str(&format!(
                "<a href='https://example.invalid/native-primary?query={index}'>READ</a>"
            ));
        }
        let html = card("Synthetic metadata expansion", LIVER, &footer);
        let catalog = catalog_roles(&html, &base).unwrap();
        assert!(plan_metadata_admission(&catalog, Some(1)).is_ok());
        assert!(plan_metadata_admission(&catalog, None)
            .unwrap_err()
            .to_string()
            .contains("aggregate plan metadata budget"));
        let temp = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(temp.path(), true, 500, 1 << 20, "").unwrap();
        fetcher.seed_cache_for_tests(URL, html.as_bytes(), "text/html");
        let error = Acg
            .discover(&source(), &fetcher, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("aggregate plan metadata budget exceeded before terminal requests"));
    }
    #[test]
    fn mixed_case_malformed_authorities_are_redacted_and_valid_uppercase_is_preserved() {
        let base = Url::parse(URL).unwrap();
        let bad = [
            "HTTPS:/malformed-case-marker.example/path",
            "Http:\\malformed-backslash-marker.example/path",
            "hTtPs:malformed-noauthority-marker.example/path",
            "HTTP:////malformed-empty-marker.example/path",
            "hTtPs://\\malformed-authority-marker.example/path",
        ];
        let safe = "HTTPS://EXAMPLE.invalid/CaseSensitive?Query=Kept&v=2";
        let mut html = card(
            "Synthetic safe uppercase",
            safe,
            &format!("<a href='{safe}'>READ</a>"),
        );
        for href in bad {
            html.push_str(&card(
                "Synthetic mixed-case malformed",
                href,
                &format!("<a href='{href}'>READ</a>"),
            ));
        }
        let catalog = catalog_roles(&html, &base).unwrap();
        assert_eq!(catalog.primaries.len(), 1);
        assert_eq!(
            catalog.primaries[0].url,
            "https://example.invalid/CaseSensitive?Query=Kept&v=2"
        );
        assert_eq!(catalog.cards[0]["heading_href"], safe);
        assert_eq!(catalog.cards[0]["footer_links"][0]["native_href"], safe);
        for card in catalog.cards.iter().skip(1) {
            assert!(card["heading_href"].is_null() && card["heading_url"].is_null());
            assert_eq!(card["heading_href_rejected"]["reason"], "malformed_href");
            assert!(
                card["footer_links"][0]["native_href"].is_null()
                    && card["footer_links"][0]["url"].is_null()
            );
            assert_eq!(
                card["footer_links"][0]["rejected_href"]["reason"],
                "malformed_href"
            );
        }
        let serialized = serde_json::to_string(&catalog.cards).unwrap();
        for marker in [
            "malformed-case-marker",
            "malformed-backslash-marker",
            "malformed-noauthority-marker",
            "malformed-empty-marker",
            "malformed-authority-marker",
        ] {
            assert!(!serialized.contains(marker));
        }
    }
    #[tokio::test]
    async fn limited_large_all_failure_catalog_materializes_only_selected_provenance() {
        let mut footer = String::new();
        for index in 0..200 {
            footer.push_str(&format!(
                "<a href='https://gi.org/synthetic-unavailable-{index}.pdf'>Highlight</a>"
            ));
        }
        let html = card("Synthetic large all-failure source card", LIVER, &footer)
            .replace("</h3>", &format!("</h3><p>{}</p>", "x".repeat(210_000)));
        let catalog = catalog_roles(&html, &Url::parse(URL).unwrap()).unwrap();
        let native_bytes = serde_json::to_vec(&catalog.cards).unwrap().len();
        assert!((400 * 1024..MAX_CATALOG_METADATA_BYTES).contains(&native_bytes));
        assert_eq!(catalog.highlights.len(), 200);
        assert!(plan_metadata_admission(&catalog, Some(1)).is_ok());
        let temp = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(temp.path(), true, 500, 1 << 20, "").unwrap();
        fetcher.seed_cache_for_tests(URL, html.as_bytes(), "text/html");
        let rows = Acg.discover(&source(), &fetcher, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.extra["source_role"], "highlight_unresolved");
        assert_eq!(row.extra["source_cards"].as_array().unwrap().len(), 1);
        assert_eq!(
            row.extra["catalog_discovery"]["counts"]["highlight_candidates_attempted"],
            200
        );
        assert_eq!(
            row.extra["catalog_discovery"]["counts"]["highlight_failures"],
            200
        );
        assert_eq!(
            row.extra["catalog_discovery"]["counts"]["failed_highlight_selected"],
            1
        );
        assert_eq!(
            row.extra["catalog_discovery"]["counts"]["failed_highlight_omitted_by_limit"],
            199
        );
        assert_eq!(
            row.extra["catalog_inventory"]["source_cards"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(serde_json::to_vec(&rows).unwrap().len() <= MAX_PLAN_METADATA_BYTES);
        assert!(row.extra["discovery_error"].as_str().unwrap().len() <= 4096);
    }
}
