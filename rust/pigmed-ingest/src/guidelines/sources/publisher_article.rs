//! Bounded source-labelled publisher article parser. No authentication or guessed endpoints.
use super::pmc_crosswalk::{self as cw, Lookup, Planned, SourceRef};
use super::{common, Adapter, PlanRow};
use crate::guidelines::{
    doc::{BlockKind, DateObservation, GuidelineDoc, Relation, Rights},
    fetch::Fetcher,
    html::{blocks_from_html, sel, text_of},
    registry::SourceEntry,
};
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use url::Url;

#[derive(Clone, Copy, Debug)]
pub enum PublisherSource {
    Aap,
    Asco,
    Ers,
    Acog,
}
pub struct PublisherAdapter(pub PublisherSource);
impl PublisherSource {
    fn name(self) -> &'static str {
        match self {
            Self::Aap => "aap",
            Self::Asco => "asco",
            Self::Ers => "ers",
            Self::Acog => "acog",
        }
    }
    fn root(self) -> &'static str {
        match self {
            Self::Aap => "https://publications.aap.org/pediatrics/pages/policy",
            Self::Asco => "https://ascopubs.org/guidelines",
            Self::Ers => "https://channel.ersnet.org/channel-25-guidelines/",
            Self::Acog => "https://www.acog.org/clinical/",
        }
    }
    fn is_catalog(self, u: &Url) -> bool {
        match self {
            Self::Aap => {
                u.host_str() == Some("publications.aap.org")
                    && (u.path() == "/pediatrics/pages/policy"
                        || u.path().starts_with("/pediatrics/collection/"))
            }
            Self::Asco => {
                u.host_str() == Some("ascopubs.org")
                    && (u.path() == "/guidelines"
                        || u.path().starts_with("/topics/asco-guidelines/"))
            }
            Self::Ers => {
                u.host_str() == Some("channel.ersnet.org") && u.path() == "/channel-25-guidelines/"
            }
            Self::Acog => {
                u.host_str() == Some("www.acog.org")
                    && matches!(
                        u.path().trim_end_matches('/'),
                        "/clinical"
                            | "/clinical/guidance/clinical-practice-guideline"
                            | "/clinical/guidance/clinical-consensus"
                    )
            }
        }
    }
    fn is_terminal(self, u: &Url) -> bool {
        match self {
            Self::Aap => {
                u.host_str() == Some("publications.aap.org")
                    && u.path().starts_with("/pediatrics/article/")
            }
            Self::Asco => {
                u.host_str() == Some("ascopubs.org")
                    && u.path().starts_with("/doi/10.1200/")
                    && !u.path().contains("/pdf/")
            }
            Self::Ers => {
                u.host_str() == Some("channel.ersnet.org")
                    && u.path() == "/container-iframe-redirect.php"
                    || u.host_str() == Some("publications.ersnet.org")
                        && u.path().starts_with("/content/erj/")
            }
            Self::Acog => {
                u.host_str() == Some("www.acog.org")
                    && (u
                        .path()
                        .starts_with("/clinical/guidance/clinical-practice-guideline/")
                        || u.path()
                            .starts_with("/clinical/guidance/clinical-consensus/"))
                    && u.path().trim_matches('/').split('/').count() > 3
                    || u.host_str() == Some("journals.lww.com")
                        && (u.path().starts_with("/10.1097/AOG.")
                            || u.path().starts_with("/greenjournal/fulltext/"))
            }
        }
    }
}
fn issue(src: &SourceEntry, u: String, e: String) -> PlanRow {
    PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("{}-{}-unresolved", src.id, common::short_hash(&u)),
        url: u,
        title: None,
        rendition: None,
        version: None,
        extra: json!({"discovery_error":e}),
    }
}
fn clinical_links(
    kind: PublisherSource,
    raw: &str,
    base: &Url,
) -> Result<(Vec<(String, String)>, Vec<String>)> {
    let d = Html::parse_document(raw);
    let root_selector = if matches!(kind, PublisherSource::Ers) {
        "#zone-contenu"
    } else {
        "main"
    };
    if d.select(&sel(root_selector)).next().is_none() {
        bail!("publisher catalog root missing")
    };
    let link_selector = if matches!(kind, PublisherSource::Ers) {
        "#zone-contenu a.guideline__container-redirect__link[href]"
    } else {
        "main a[href]"
    };
    let mut rows = vec![];
    let mut cats = vec![];
    let mut seen = HashSet::new();
    for a in d.select(&sel(link_selector)) {
        let Some(u) = a.value().attr("href").and_then(|h| common::link(base, h)) else {
            continue;
        };
        let Ok(parsed) = Url::parse(&u) else { continue };
        let title = text_of(&a);
        if title.is_empty() || !seen.insert(u.clone()) {
            continue;
        }
        // Featured Committee Opinions are not a fallback CPG inventory on an otherwise
        // unresolved dynamic branch. Publisher links need an explicit source type label.
        if matches!(kind, PublisherSource::Acog) && parsed.host_str() == Some("journals.lww.com") {
            let label = title.to_ascii_lowercase();
            if label.contains("committee opinion")
                || !(label.contains("guideline") || label.contains("clinical consensus"))
            {
                continue;
            }
        }
        if kind.is_terminal(&parsed)
            && !(matches!(kind, PublisherSource::Acog) && base.path() == "/clinical/")
        {
            rows.push((u, title))
        } else if kind.is_catalog(&parsed) && u != base.as_str() {
            cats.push(u)
        }
    }
    if matches!(kind, PublisherSource::Acog) {
        cats.sort_by_key(|u| {
            if u.contains("/clinical-practice-guideline/") {
                0
            } else if u.contains("/clinical-consensus/") {
                1
            } else {
                2
            }
        });
    }
    if rows.is_empty() && cats.is_empty() {
        bail!(
            "{} catalog has no recognized source links; possible JS shell or changed template",
            kind.name()
        )
    }
    Ok((rows, cats))
}
fn parse_article(
    kind: PublisherSource,
    raw: &str,
    base: &Url,
) -> Result<(String, String, String, Option<String>)> {
    let doc = Html::parse_document(raw);
    let text = doc
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if [
        "sign in to access",
        "performing security verification",
        "access denied",
        "subscribe to read",
    ]
    .iter()
    .any(|x| text.contains(x))
    {
        bail!("publisher access or security wall")
    }
    let (selector, label) = match kind {
        PublisherSource::Aap => ("main", "aap"),
        PublisherSource::Asco => ("article", "asco"),
        PublisherSource::Ers => ("#edit-group-article", "ers"),
        PublisherSource::Acog => ("article", "acog"),
    };
    let root = doc
        .select(&sel(selector))
        .next()
        .with_context(|| format!("{label} full article root missing"))?;
    let title = doc
        .select(&sel("meta[name=\"citation_title\"]"))
        .next()
        .and_then(|e| e.value().attr("content"))
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            doc.select(&sel("h1"))
                .map(|e| text_of(&e))
                .find(|s| !s.is_empty())
        })
        .context("article title missing")?;
    let aap_state = if matches!(kind, PublisherSource::Aap) {
        Some(aap_source_state(raw, base)?)
    } else {
        None
    };
    let template_2026 = aap_state
        .as_ref()
        .and_then(|state| state["doi"].as_str())
        .and_then(aap_2026);
    let body_2026 = if let Some(spec) = template_2026 {
        let roots = root
            .select(&sel(".article-body > .widget-ArticleFulltext"))
            .collect::<Vec<_>>();
        if roots.len() != 1
            || roots[0]
                .select(&sel(&format!(
                    "[id='{}'] .table-overflow > table[role='table']",
                    spec.summary
                )))
                .next()
                .is_none()
        {
            bail!("AAP 2026 exact article widget/summary structure missing")
        }
        Some(roots[0])
    } else {
        None
    };
    let known_retired_aap = aap_state.as_ref().is_some_and(|state| {
        state["source_state"] == "retired"
            && state["doi"].as_str().is_some_and(|doi| {
                matches!(doi, "10.1542/peds.2011-1330" | "10.1542/peds.2016-3026")
            })
    }) && root.select(&sel(".article-body")).next().is_some();
    let opioid_aap = aap_state
        .as_ref()
        .is_some_and(|state| state["doi"] == AAP_OPIOID_DOI)
        && root.select(&sel(".article-body #T1")).next().is_some();
    let structural = match kind {
        PublisherSource::Aap => {
            root.select(&sel("h2.section-title.jumplink-heading"))
                .next()
                .is_some()
                || body_2026.is_some()
        }
        PublisherSource::Asco => root.select(&sel("section#sec-1")).next().is_some(),
        PublisherSource::Ers => root.select(&sel("#TB1, #F1, h2")).next().is_some(),
        PublisherSource::Acog => root.select(&sel("h2")).count() >= 2,
    };
    if !structural && !known_retired_aap {
        bail!("{label} missing verified full-body structure")
    }
    let title_lower = title.to_lowercase();
    // A full article must have a source-structured body with substantive sections. Do not require
    // a methods section: living updates can present updated recommendations without repeating it.
    let headings: Vec<String> = root
        .select(&sel("h2, h3"))
        .map(|h| text_of(&h).to_lowercase())
        .collect();
    let recommendation_section = headings.iter().any(|h| h.contains("recommendation"))
        || (matches!(kind, PublisherSource::Ers)
            && root
                .select(&sel("div.section > h4.section-title"))
                .any(|h| matches!(text_of(&h).as_str(), "Recommendation" | "Recommendations")));
    let report_sections = headings.iter().any(|h| h.contains("method"))
        && headings
            .iter()
            .any(|h| h.contains("result") || h.contains("conclusion"));
    if !known_retired_aap
        && (headings.len() < 2
            || !(recommendation_section
                || opioid_aap
                || body_2026.is_some()
                || (title_lower.contains("technical report") && report_sections)))
    {
        bail!("{label} article is a summary, gated, or unrecognized full template")
    }

    let item = if matches!(kind, PublisherSource::Aap) && title_lower.contains("reaffirmation") {
        "guideline_reaffirmation"
    } else if title_lower.contains("technical report") {
        "technical_report"
    } else if title_lower.contains("clinical consensus") {
        "clinical_consensus"
    } else if title_lower.contains("executive summary") {
        "executive_summary"
    } else if title_lower.contains("guideline") {
        "clinical_practice_guideline"
    } else {
        bail!("article title lacks a recognized document type")
    };
    let doi = doc
        .select(&sel("meta[name='citation_doi']"))
        .next()
        .and_then(|e| e.value().attr("content"))
        .map(str::to_owned);
    let body = if let Some(root) = body_2026 {
        aap_primary_html(root)
    } else if opioid_aap {
        aap_primary_html(root)
    } else {
        root.html()
    };
    Ok((title, body, item.into(), doi))
}
// AAP source-state metadata is item-level. Collection inclusion and reaffirmation
// never imply current state, and no unobserved cross-article edge is fabricated.
const AAP_OPIOID_DOI: &str = "10.1542/peds.2024-068752";
fn aap_source_state(raw: &str, base: &Url) -> Result<serde_json::Value> {
    let d = Html::parse_document(raw);
    let metadata = |name: &str| {
        d.select(&sel(&format!("meta[name='{name}']")))
            .next()
            .and_then(|e| e.value().attr("content"))
            .map(String::from)
    };
    let title =
        metadata("citation_title").or_else(|| d.select(&sel("h1")).next().map(|h| text_of(&h)));
    let doi = metadata("citation_doi");
    let retirement = regex::Regex::new(
        r"^This Clinical Practice Guideline was retired ([A-Za-z]+ (?:19|20)\d{2})\.$",
    )?;
    let mut notices = vec![];
    let mut dates = vec![];
    for h in d.select(&sel("h2.abstract-title,h2.section-title")) {
        let raw = text_of(&h);
        let Some(c) = retirement.captures(&raw) else {
            continue;
        };
        let date = common::date("retired", &c[1], base.as_str());
        if !dates.iter().any(|d: &DateObservation| d.raw == date.raw) {
            dates.push(date);
        }
        notices.push(json!({"raw":raw,"source_id":h.value().id(),"source_selector":if h.value().classes().any(|c|c=="abstract-title"){"h2.abstract-title"}else{"h2.section-title"},"source_html":h.html(),"evidence_url":base.as_str(),"entity_level":"this source article's Clinical Practice Guideline notice"}));
    }
    let publication_fields = [
        "citation_publication_date",
        "citation_online_date",
        "citation_date",
    ]
    .iter()
    .filter_map(|name| metadata(name).map(|raw| json!({"label":name,"raw":raw})))
    .collect::<Vec<_>>();
    let reaffirmation = title
        .as_deref()
        .is_some_and(|t| t.to_lowercase().contains("reaffirmation"));
    Ok(
        json!({"source_state":if notices.is_empty(){"unknown"}else{"retired"},"state_evidence":notices,"retirement_date_observations":dates,"publication_fields":publication_fields,"doi":doi,"source_title":title,"source_event_role":if reaffirmation{Some("reaffirmation")}else{None},"identity_note":"Each source DOI remains a separate work/event; collection inclusion, age and reaffirmation do not establish current state or a replacement edge"}),
    )
}
fn publisher_snapshot(kind: PublisherSource, raw: &str, body: &str, base: &Url) -> Result<String> {
    if !matches!(kind, PublisherSource::Aap) {
        return Ok(crate::common::sha256_hex(body.as_bytes()));
    }
    let state = aap_source_state(raw, base)?;
    let mut snapshot =
        json!({"body_sha256":crate::common::sha256_hex(body.as_bytes()),"aap_source_state":state});
    if state["doi"].as_str().and_then(aap_2026).is_some() {
        snapshot["source_article_rights"] = aap_2026_rights(raw);
        snapshot["source_asset_and_companion_descriptors"] = aap_2026_assets(raw, base);
    }
    Ok(crate::common::sha256_hex(&serde_json::to_vec(&snapshot)?))
}
fn aap_table_source_id(table: scraper::ElementRef<'_>) -> Option<String> {
    use scraper::ElementRef;
    let wrapper = table
        .ancestors()
        .filter_map(ElementRef::wrap)
        .find(|e| e.value().classes().any(|c| c == "table-wrap"))?;
    wrapper
        .select(&sel("[id]"))
        .filter_map(|e| e.value().id())
        .find(|id| regex::Regex::new(r"^T\d+$").unwrap().is_match(id))
        .map(String::from)
}
fn aap_primary_html(root: scraper::ElementRef<'_>) -> String {
    use scraper::{ElementRef, Node};
    fn escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    fn walk(e: ElementRef<'_>, out: &mut String) {
        if e.value().classes().any(|c| c == "table-modal")
            || e.value().attr("role") == Some("dialog")
            || (e.value().name() == "table" && e.value().attr("aria-hidden") == Some("true"))
        {
            return;
        }
        let tag = e.value().name();
        out.push('<');
        out.push_str(tag);
        for (name, value) in e.value().attrs() {
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            out.push_str(&escape(value));
            out.push('"');
        }
        if tag == "table" && e.value().id().is_none() {
            if let Some(id) = aap_table_source_id(e) {
                out.push_str(&format!(" id=\"aap-native-table-{}\"", escape(&id)));
            }
        }
        out.push('>');
        for child in e.children() {
            match child.value() {
                Node::Text(t) => out.push_str(&escape(t)),
                Node::Element(_) => {
                    if let Some(c) = ElementRef::wrap(child) {
                        walk(c, out)
                    }
                }
                _ => {}
            }
        }
        if !matches!(
            tag,
            "img" | "br" | "hr" | "input" | "source" | "meta" | "link" | "wbr"
        ) {
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
    let mut out = String::new();
    walk(root, &mut out);
    out
}
fn aap_kas_summary(
    body: &str,
    blocks: &mut Vec<crate::guidelines::doc::Block>,
) -> Result<Option<serde_json::Value>> {
    use scraper::ElementRef;
    let d = Html::parse_document(body);
    let title = d
        .select(&sel(".article-body #T1"))
        .next()
        .context("AAP opioid source summary title T1 missing")?;
    let wrapper = title
        .ancestors()
        .filter_map(ElementRef::wrap)
        .find(|e| e.value().classes().any(|c| c == "table-wrap"))
        .context("AAP KAS summary table wrapper missing")?;
    let caption = wrapper
        .select(&sel("#caption-T1"))
        .next()
        .map(|e| text_of(&e))
        .context("AAP KAS source caption missing")?;
    if caption != "Key Action Statement Table" {
        bail!("AAP KAS summary caption differs from source contract");
    }
    let tables = wrapper
        .select(&sel(".table-overflow > table"))
        .filter(|t| t.value().attr("aria-hidden") != Some("true"))
        .collect::<Vec<_>>();
    if tables.len() != 1 {
        bail!("AAP KAS primary summary table missing or duplicated");
    }
    let table = tables[0];
    let headers = table
        .select(&sel("thead th"))
        .map(|e| text_of(&e))
        .collect::<Vec<_>>();
    if headers
        != [
            "Key Action Statement (KAS)",
            "Evidence Quality",
            "Recommendation Strength",
        ]
    {
        bail!("AAP KAS native column labels differ from observed scope");
    }
    let rows = table.select(&sel("tbody > tr")).collect::<Vec<_>>();
    if rows.len() != 13 {
        bail!("AAP KAS source summary must contain 12 formal rows and one legend");
    }
    let expected = [
        "KAS 1:", "KAS 2:", "KAS 3:", "KAS 4.1:", "KAS 4.2:", "KAS 4.3:", "KAS 4.4:", "KAS 5:",
        "KAS 6:", "KAS 7:", "KAS 8:", "KAS 9:",
    ];
    let label_re = regex::Regex::new(r"^(KAS\s+\d+(?:\.\d+)?:)\s*")?;
    let table_id = table
        .value()
        .id()
        .context("AAP primary table canonical ID missing")?;
    let mut formal = vec![];
    let mut metadata = vec![];
    for (index, row) in rows.iter().take(12).enumerate() {
        let cells = common::cells(*row);
        if cells.len() != 3
            || cells.iter().any(|c| c.value().name() != "td")
            || cells
                .iter()
                .any(|c| c.value().attr("colspan").is_some_and(|v| v != "1"))
        {
            bail!("AAP KAS formal row native cell geometry changed");
        }
        let text = cells[0]
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let printed = label_re
            .captures(&text)
            .context("AAP source KAS plain-text label missing")?[1]
            .to_string();
        if printed != expected[index] {
            bail!("AAP KAS source labels/order changed");
        }
        let quality = text_of(&cells[1]);
        let strength = text_of(&cells[2]);
        if quality.is_empty() || strength.is_empty() {
            bail!("AAP KAS native evidence-quality/strength cell absent");
        }
        let id = format!(
            "aap-kas-{}",
            printed.trim_start_matches("KAS ").trim_end_matches(':')
        );
        formal.push(crate::guidelines::doc::Block {
            block_id: id.clone(),
            kind: BlockKind::Recommendation {
                label: Some(printed.clone()),
                strength: Some(strength.clone()),
                certainty: None,
                population: None,
                grading_system: None,
                qualifier: None,
            },
            text: text.clone(),
            heading_path: vec![],
            page: None,
        });
        metadata.push(json!({"block_id":id,"source_row_index":index,"source_row_id":row.value().id(),"source_label":printed,"occurrence_role":"source_summary_table_formal_KAS","source_statement":text,"source_row_html":row.html(),"native_grading":{"evidence_quality":{"source_label":headers[1],"value":quality},"recommendation_strength":{"source_label":headers[2],"value":strength}},"certainty_mapping":"unknown; publisher Evidence Quality is retained without inference to GRADE certainty","source_cells":cells.iter().map(|c|json!({"tag":c.value().name(),"text":c.text().collect::<String>(),"source_html":c.html()})).collect::<Vec<_>>()}));
    }
    let legend = common::cells(rows[12]);
    if legend.len() != 1 || legend[0].value().attr("colspan") != Some("6") {
        bail!("AAP KAS evidence legend row geometry changed");
    }
    if let Some(block) = blocks.iter_mut().find(|b| b.block_id == table_id) {
        if let BlockKind::Table { caption: c } = &mut block.kind {
            *c = Some(caption.clone());
        }
    }
    common::insert_table_recommendations(blocks, table_id, formal)?;
    let mut profiles = vec![];
    for wrap in d.select(&sel(".article-body .table-wrap")) {
        let Some(label) = wrap
            .select(&sel(
                ".table-wrap-title > .label.title-label, .table-wrap-title .label.title-label",
            ))
            .next()
            .map(|e| text_of(&e))
        else {
            continue;
        };
        if !label_re.is_match(&label) {
            continue;
        }
        let id = wrap
            .select(&sel("[id]"))
            .filter_map(|e| e.value().id())
            .find(|id| regex::Regex::new(r"^T\d+$").unwrap().is_match(id));
        let profile_table = wrap.select(&sel(".table-overflow > table")).next();
        let summary = metadata
            .iter()
            .find(|m| m["source_label"] == label)
            .context("AAP detailed profile label has no formal summary KAS association")?;
        if let Some(table) = profile_table {
            if let Some(id) = table.value().id() {
                if let Some(block) = blocks.iter_mut().find(|b| b.block_id == id) {
                    if let BlockKind::Table { caption } = &mut block.kind {
                        *caption = wrap
                            .select(&sel(".table-wrap-title"))
                            .next()
                            .map(|title| text_of(&title));
                    }
                }
            }
        }
        profiles.push(json!({"source_table_id":id,"source_label":label,"source_summary_block_id":summary["block_id"],"occurrence_role":"detailed_evidence_profile_repeats_summary_KAS_not_an_extra_recommendation","source_html":wrap.html(),"native_table_html":profile_table.map(|t|t.html()),"statement_caption":wrap.select(&sel(".caption p > em")).next().map(|e|text_of(&e))}));
    }
    Ok(Some(
        json!({"source_summary_id":"T1","summary_caption":caption,"source_summary_native_table_html":table.html(),"source_recommendations":metadata,"source_legend_html":rows[12].html(),"source_legend_role":"evidence_grading_legend_not_a_statement","source_detailed_profiles":profiles,"source_occurrence_note":"T1 native rows are formal summary occurrences; detailed caption/profile text and modal views do not create additional KAS. Full main-body context and native primary tables remain retained."}),
    ))
}

// Exact 2026 Silverchair templates independently observed in the AAP DOM contract.
// These source-local rules do not extend the other publisher or legacy AAP paths.
#[derive(Clone, Copy)]
struct Aap2026 {
    summary: &'static str,
    kas: &'static [&'static str],
    gps: &'static [&'static str],
    kas_column: usize,
    width: usize,
}
fn aap_2026(doi: &str) -> Option<Aap2026> {
    match doi {
        "10.1542/peds.2025-075764" => Some(Aap2026 {
            summary: "18189142-content",
            kas: &["1", "2A", "2B", "3A", "3B", "4", "5", "6"],
            gps: &[
                "18189239-content",
                "18189263-content",
                "18189264-content",
                "18189292-content",
            ],
            kas_column: 1,
            width: 4,
        }),
        "10.1542/peds.2026-078565" => Some(Aap2026 {
            summary: "18672556-content",
            kas: &["1", "2", "3A", "3B", "4", "5A", "5B", "6"],
            gps: &[
                "18672800-content",
                "18672801-content",
                "18672802-content",
                "18672833-content",
            ],
            kas_column: 0,
            width: 2,
        }),
        _ => None,
    }
}
struct AapLabelNote<'a> {
    summary: scraper::ElementRef<'a>,
    target: scraper::ElementRef<'a>,
}
fn aap_kas6_note_anchor(anchor: scraper::ElementRef<'_>) -> bool {
    use scraper::{ElementRef, Node};
    if anchor.value().name() != "a" {
        return false;
    }
    let fragment = anchor.value().attr("href") == Some("#utab1fn2");
    // Independently observed hydrated markup is recognized only if present in
    // received HTML; no JavaScript, endpoint inference or URL rewrite occurs.
    let reveal = anchor.value().attr("href") == Some("javascript:;")
        && anchor.value().attr("reveal-id") == Some("utab1fn2")
        && anchor.value().attr("data-open") == Some("utab1fn2")
        && ["link", "link-ref", "link-reveal", "xref-fn", "js-xref-fn"]
            .iter()
            .all(|expected| anchor.value().classes().any(|class| class == *expected));
    if !fragment && !reveal {
        return false;
    }
    // A fragment link must not carry contradictory reveal identities either.
    if fragment
        && ["reveal-id", "data-open"].iter().any(|name| {
            anchor
                .value()
                .attr(name)
                .is_some_and(|value| value != "utab1fn2")
        })
    {
        return false;
    }
    let children = anchor
        .children()
        .filter(|child| match child.value() {
            Node::Text(value) => !value.trim().is_empty(),
            _ => true,
        })
        .collect::<Vec<_>>();
    if children.len() != 1 {
        return false;
    }
    let Some(sup) = ElementRef::wrap(children[0]) else {
        return false;
    };
    sup.value().name() == "sup"
        && text_of(&sup) == "b"
        && sup
            .children()
            .all(|child| matches!(child.value(), Node::Text(_)))
}
fn aap_verified_label_note<'a>(
    document: &'a Html,
    summary: scraper::ElementRef<'a>,
) -> Option<AapLabelNote<'a>> {
    use scraper::ElementRef;
    summary
        .ancestors()
        .filter_map(ElementRef::wrap)
        .find(|element| element.value().id() == Some("18672556-content"))?;
    let wrapper = summary
        .ancestors()
        .filter_map(ElementRef::wrap)
        .find(|element| element.value().classes().any(|class| class == "table-wrap"))?;
    if wrapper.value().name() != "div"
        || !wrapper.value().classes().any(|class| class == "table-wide")
    {
        return None;
    }
    let targets = document.select(&sel("[id='utab1fn2']")).collect::<Vec<_>>();
    if targets.len() != 1 {
        return None;
    }
    let target = targets[0];
    if target.value().name() != "div"
        || !target.value().classes().any(|class| class == "fn")
        || target.value().attr("content-id") != Some("utab1fn2")
    {
        return None;
    }
    let foot = target.parent().and_then(ElementRef::wrap)?;
    if foot.value().name() != "div"
        || !foot
            .value()
            .classes()
            .any(|class| class == "table-wrap-foot")
    {
        return None;
    }
    let owner = target
        .ancestors()
        .filter_map(ElementRef::wrap)
        .find(|element| element.value().classes().any(|class| class == "table-wrap"))?;
    if !std::ptr::eq(owner.value(), wrapper.value()) {
        return None;
    }
    let labels = target
        .select(&sel(
            "span.label.fn-label > span.end-note-link[data-fn-id='utab1fn2']",
        ))
        .collect::<Vec<_>>();
    if labels.len() != 1
        || text_of(&labels[0]) != "b"
        || target.select(&sel("span.label.fn-label")).count() != 1
        || target.select(&sel("span.end-note-link")).count() != 1
    {
        return None;
    }
    let paragraphs = target.select(&sel("p")).collect::<Vec<_>>();
    if paragraphs.len() != 1 || text_of(&paragraphs[0]).is_empty() {
        return None;
    }
    // In the independently observed primary body there is exactly one marker.
    // Hidden XREF modal copies have already been removed by the AAP serializer.
    let markers = document
        .select(&sel("a"))
        .filter(|a| {
            a.value().attr("href") == Some("#utab1fn2")
                || a.value().attr("reveal-id") == Some("utab1fn2")
                || a.value().attr("data-open") == Some("utab1fn2")
        })
        .collect::<Vec<_>>();
    if markers.len() != 1 || !aap_kas6_note_anchor(markers[0]) {
        return None;
    }
    let span = markers[0].parent().and_then(ElementRef::wrap)?;
    if span.value().name() != "span" || !span.value().classes().any(|class| class == "underline") {
        return None;
    }
    let cell = span.parent().and_then(ElementRef::wrap)?;
    if cell.value().name() != "td" {
        return None;
    }
    let table = span
        .ancestors()
        .filter_map(ElementRef::wrap)
        .find(|element| element.value().name() == "table")?;
    if !std::ptr::eq(table.value(), summary.value()) {
        return None;
    }
    Some(AapLabelNote { summary, target })
}
fn aap_text_without_label_notes(
    element: scraper::ElementRef<'_>,
    verified: Option<&AapLabelNote<'_>>,
) -> String {
    use scraper::{ElementRef, Node};
    let raw = text_of(&element);
    let Some(verified) = verified else { return raw };
    if verified.target.value().id() != Some("utab1fn2")
        || !element
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|parent| parent.value().name() == "table")
            .is_some_and(|table| std::ptr::eq(table.value(), verified.summary.value()))
    {
        return raw;
    }
    let anchors = element.select(&sel("a")).collect::<Vec<_>>();
    if anchors.len() != 1 {
        return raw;
    }
    let anchor = anchors[0];
    if !aap_kas6_note_anchor(anchor)
        || !anchor
            .parent()
            .and_then(ElementRef::wrap)
            .is_some_and(|parent| std::ptr::eq(parent.value(), element.value()))
    {
        return raw;
    }
    fn walk(element: ElementRef<'_>, skip: ElementRef<'_>, text: &mut String) {
        if std::ptr::eq(element.value(), skip.value()) {
            return;
        }
        for child in element.children() {
            match child.value() {
                Node::Text(value) => text.push_str(value),
                Node::Element(_) => {
                    if let Some(child) = ElementRef::wrap(child) {
                        walk(child, skip, text)
                    }
                }
                _ => {}
            }
        }
    }
    let mut clean = String::new();
    walk(element, anchor, &mut clean);
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    // This is one verified source identity exception, not a general footnote rule.
    if clean == "Key Action Statement 6:" {
        clean
    } else {
        raw
    }
}

#[derive(Clone)]
struct AapFragment {
    role: &'static str,
    native_id: Option<String>,
    label: String,
    marker_html: String,
    source_child_position: usize,
    html: String,
    text: String,
}
fn aap_fragments(
    owner: scraper::ElementRef<'_>,
    verified: Option<&AapLabelNote<'_>>,
) -> Result<Vec<AapFragment>> {
    use scraper::{ElementRef, Node};
    let kas = regex::Regex::new(r"^(?:KAS|Key Action Statement)\s+(\d+[A-Z]?)\s*:?\s*$")?;
    let mut out = vec![];
    let mut current: Option<AapFragment> = None;
    fn flush(current: &mut Option<AapFragment>, out: &mut Vec<AapFragment>) -> Result<()> {
        if let Some(mut fragment) = current.take() {
            let parsed = Html::parse_fragment(&fragment.html);
            fragment.text = text_of(&parsed.root_element());
            let marker = Html::parse_fragment(&fragment.marker_html);
            let marker_text = text_of(&marker.root_element());
            let tail = fragment
                .text
                .strip_prefix(&marker_text)
                .unwrap_or("")
                .trim()
                .trim_start_matches(':')
                .trim();
            if !tail.chars().any(char::is_alphanumeric) {
                bail!("AAP 2026 native label has no directly associated statement text")
            }
            out.push(fragment);
        }
        Ok(())
    }
    for (source_child_position, child) in owner.children().enumerate() {
        if let Some(element) = ElementRef::wrap(child) {
            if element.value().name() == "br" {
                flush(&mut current, &mut out)?;
                continue;
            }
            if element.value().name() == "span"
                && element.value().classes().any(|class| class == "underline")
            {
                let label = aap_text_without_label_notes(element, verified);
                let native = kas.captures(&label).map(|capture| capture[1].to_owned());
                let role = if native.is_some() {
                    Some("key_action_statement")
                } else if label.starts_with("Good Practice Statement") {
                    Some("good_practice_statement")
                } else {
                    None
                };
                if let Some(role) = role {
                    flush(&mut current, &mut out)?;
                    current = Some(AapFragment {
                        role,
                        native_id: native,
                        label,
                        marker_html: element.html(),
                        source_child_position,
                        html: element.html(),
                        text: String::new(),
                    });
                    continue;
                }
            }
            if matches!(element.value().name(), "p" | "ul" | "ol" | "div" | "table") {
                flush(&mut current, &mut out)?;
                continue;
            }
            if let Some(fragment) = &mut current {
                fragment.html.push_str(&element.html());
            }
        } else if let Node::Text(value) = child.value() {
            if let Some(fragment) = &mut current {
                fragment.html.push_str(
                    &value
                        .replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;"),
                );
            }
        }
    }
    flush(&mut current, &mut out)?;
    Ok(out)
}
struct AapGrid<'a> {
    cells: Vec<(scraper::ElementRef<'a>, usize, usize, bool)>,
    metadata: serde_json::Value,
    markdown: String,
}
fn aap_expanded_grid(table: scraper::ElementRef<'_>) -> Result<AapGrid<'_>> {
    use scraper::ElementRef;
    let rows = table
        .select(&sel("tr"))
        .filter(|row| {
            row.ancestors()
                .filter_map(ElementRef::wrap)
                .find(|e| e.value().name() == "table")
                .is_some_and(|ancestor| std::ptr::eq(ancestor.value(), table.value()))
        })
        .collect::<Vec<_>>();
    if rows.is_empty() || rows.len() > 4096 {
        bail!("AAP 2026 source table row bound exceeded")
    }
    let mut grid: Vec<Vec<Option<usize>>> = vec![vec![]; rows.len()];
    let mut cells = vec![];
    let mut native = vec![];
    for (row_index, row) in rows.iter().enumerate() {
        let mut column = 0usize;
        for cell in common::cells(*row) {
            while grid[row_index].get(column).is_some_and(Option::is_some) {
                column += 1;
            }
            let span = |name: &str| -> Result<usize> {
                let value = cell
                    .value()
                    .attr(name)
                    .unwrap_or("1")
                    .parse::<usize>()
                    .with_context(|| format!("AAP {name} is not a native integer"))?;
                if !(1..=64).contains(&value) {
                    bail!("AAP source table span bound exceeded")
                }
                Ok(value)
            };
            let rowspan = span("rowspan")?;
            let colspan = span("colspan")?;
            if row_index + rowspan > rows.len() || column + colspan > 64 {
                bail!("AAP source table span extends beyond bounded grid")
            }
            let index = cells.len();
            for expanded_row in grid.iter_mut().skip(row_index).take(rowspan) {
                expanded_row.resize(expanded_row.len().max(column + colspan), None);
                for slot in expanded_row.iter_mut().skip(column).take(colspan) {
                    if slot.is_some() {
                        bail!("AAP source table spans overlap")
                    }
                    *slot = Some(index);
                }
            }
            let in_body = row
                .ancestors()
                .filter_map(ElementRef::wrap)
                .take_while(|e| !std::ptr::eq(e.value(), table.value()))
                .any(|e| e.value().name() == "tbody");
            cells.push((cell, row_index, column, in_body));
            native.push(json!({"origin_row":row_index,"origin_column":column,"rowspan":rowspan,"colspan":colspan,"tag":cell.value().name(),"native_headers_attribute":cell.value().attr("headers"),"source_html":cell.html(),"source_text":text_of(&cell),"row_group":if in_body{"tbody"}else{"source header/footer"}}));
            column += colspan;
        }
    }
    let width = grid.iter().map(Vec::len).max().unwrap_or(0);
    let text_grid = grid
        .iter()
        .map(|row| {
            (0..width)
                .map(|column| {
                    row.get(column)
                        .copied()
                        .flatten()
                        .map(|index| text_of(&cells[index].0))
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let line = |row: &[String]| {
        format!(
            "| {} |",
            row.iter()
                .map(|cell| cell.replace('|', "\\|"))
                .collect::<Vec<_>>()
                .join(" | ")
        )
    };
    let mut markdown = vec![
        line(&text_grid[0]),
        format!("|{}|", vec!["---"; width].join("|")),
    ];
    markdown.extend(text_grid.iter().skip(1).map(|row| line(row)));
    Ok(AapGrid {
        cells,
        metadata: json!({"native_table_html":table.html(),"physical_cells":native,"expanded_origin_grid":grid,"expanded_text_grid":text_grid,"width":width,"header_policy":"native th/td tags and spans preserved; Markdown first-row rendering does not assign clinical header meaning"}),
        markdown: markdown.join("\n"),
    })
}
fn aap_2026_statements(
    body: &str,
    spec: Aap2026,
    blocks: &mut Vec<crate::guidelines::doc::Block>,
    warnings: &mut Vec<String>,
) -> Result<serde_json::Value> {
    use crate::guidelines::doc::Block;
    let d = Html::parse_document(body);
    let summaries = d
        .select(&sel(&format!(
            "[id='{}'] .table-overflow > table[role='table']",
            spec.summary
        )))
        .collect::<Vec<_>>();
    if summaries.len() != 1 {
        bail!("AAP 2026 exact primary summary table missing or duplicated")
    }
    let summary = summaries[0];
    let label_note = aap_verified_label_note(&d, summary);
    let headers = summary
        .select(&sel("thead tr"))
        .flat_map(common::cells)
        .map(|cell| text_of(&cell))
        .collect::<Vec<_>>();
    let expected = if spec.width == 4 {
        vec![
            "Clinical Question",
            "KAS",
            "Remark",
            "Good Practice Statement",
        ]
    } else {
        vec![
            "Key Action Statements",
            "Remarks and Good Practice Statements",
        ]
    };
    if headers.iter().map(String::as_str).collect::<Vec<_>>() != expected {
        bail!("AAP 2026 native summary header labels differ from source contract")
    }

    let tables = d.select(&sel("table")).collect::<Vec<_>>();
    let block_ids = blocks
        .iter()
        .filter(|block| matches!(block.kind, BlockKind::Table { .. }))
        .map(|block| block.block_id.clone())
        .collect::<Vec<_>>();
    if tables.len() != block_ids.len() {
        bail!("AAP 2026 native table/canonical table association ambiguous")
    }
    let mut source_tables = vec![];
    let mut summary_cells = None;
    let mut summary_block_id = None;
    for (table, block_id) in tables.into_iter().zip(block_ids) {
        let expanded = aap_expanded_grid(table)?;
        if std::ptr::eq(table.value(), summary.value()) {
            if expanded.metadata["width"] != spec.width {
                bail!("AAP 2026 summary column geometry differs from source contract")
            }
            summary_cells = Some(expanded.cells.clone());
            summary_block_id = Some(block_id.clone());
        }
        blocks
            .iter_mut()
            .find(|block| block.block_id == block_id)
            .context("AAP table block missing")?
            .text = expanded.markdown;
        source_tables.push(json!({"canonical_block_id":block_id,"source_table_id":table.value().id(),"geometry":expanded.metadata}));
    }
    let summary_block_id =
        summary_block_id.context("AAP summary canonical table association missing")?;
    let mut formal = vec![];
    let mut records = vec![];
    let mut summary_gps = vec![];
    let mut contexts = vec![];
    let mut labels = vec![];
    let strength = regex::Regex::new(r"(?i)\b(?:strong|conditional|weak) recommendation\b")?;
    let certainty =
        regex::Regex::new(r"(?i)\b(?:very low|low|moderate|high) certainty(?: of evidence)?\b")?;
    for (cell, row, column, in_body) in summary_cells.context("AAP summary source cells missing")? {
        if !in_body {
            continue;
        }
        let fragments = aap_fragments(cell, label_note.as_ref())?;
        for fragment in &fragments {
            if fragment.role == "good_practice_statement" {
                summary_gps.push(json!({"source_label":fragment.label,"source_html":fragment.html,"source_text":fragment.text,"source_row":row,"source_column":column,"native_number":null,"occurrence_role":"summary good-practice wording retained separately from detailed occurrence"}));
            }
        }
        if spec.width == 4 && column == 3 && !text_of(&cell).is_empty() {
            summary_gps.push(json!({"source_label":null,"source_html":cell.html(),"source_text":text_of(&cell),"source_row":row,"source_column":column,"native_number":null,"occurrence_role":"unlabelled summary good-practice cell; ordered local reference only, not a native GPS identifier"}));
        }
        for fragment in fragments
            .into_iter()
            .filter(|fragment| fragment.role == "key_action_statement" && column == spec.kas_column)
        {
            let native_id = fragment
                .native_id
                .as_deref()
                .context("AAP native KAS identity absent")?;
            labels.push(native_id.to_owned());
            let strengths = strength
                .find_iter(&fragment.text)
                .map(|m| m.as_str().to_owned())
                .collect::<Vec<_>>();
            let certainties = certainty.find_iter(&fragment.text).map(|m| json!({"raw":m.as_str(),"utf8_byte_start":m.start(),"utf8_byte_end":m.end(),"scope":"exact source statement text; dimension qualifiers retained in full text/HTML, not collapsed"})).collect::<Vec<_>>();
            let id = format!("aap-2026-kas-{native_id}");
            formal.push(Block {
                block_id: id.clone(),
                kind: BlockKind::Recommendation {
                    label: Some(fragment.label.clone()),
                    strength: if strengths.len() == 1 {
                        Some(strengths[0].clone())
                    } else {
                        None
                    },
                    certainty: if certainties.len() == 1 {
                        certainties[0]["raw"].as_str().map(str::to_owned)
                    } else {
                        None
                    },
                    population: None,
                    grading_system: None,
                    qualifier: None,
                },
                text: fragment.text.clone(),
                heading_path: vec![],
                page: None,
            });
            let footnotes = Html::parse_fragment(&fragment.html).select(&sel("a[href^='#utab1fn'], a[reveal-id='utab1fn2'][data-open='utab1fn2']")).map(|a|json!({"href":a.value().attr("href"),"source_marker":text_of(&a),"reveal_id":a.value().attr("reveal-id"),"data_open":a.value().attr("data-open"),"source_html":a.html()})).collect::<Vec<_>>();
            records.push(json!({"block_id":id,"source_role":fragment.role,"native_kas_id":native_id,"source_label":fragment.label,"source_label_html":fragment.marker_html,"source_html":fragment.html,"source_statement":fragment.text,"source_row":row,"source_column":column,"source_child_position":fragment.source_child_position,"strength_occurrences":strengths,"certainty_occurrences":certainties,"certainty_policy":"one publisher certainty phrase retained only when unambiguous; multiple source-qualified dimensions remain separate and canonical scalar certainty unknown","footnote_references":footnotes,"occurrence_role":"formal primary summary KAS; repeated details/appendices remain source context"}));
        }
        // The table keeps all remarks; separately retain native list nesting for retrieval.
        let list_items = cell.select(&sel("li")).collect::<Vec<_>>();
        for (index, li) in list_items.iter().copied().enumerate() {
            let parent = li
                .ancestors()
                .filter_map(scraper::ElementRef::wrap)
                .take_while(|e| !std::ptr::eq(e.value(), cell.value()))
                .find(|e| e.value().name() == "li" && !std::ptr::eq(e.value(), li.value()));
            let parent_block_id = parent
                .and_then(|parent| {
                    list_items
                        .iter()
                        .position(|item| std::ptr::eq(item.value(), parent.value()))
                })
                .map(|index| format!("aap-summary-context-{row}-{column}-{index}"));
            let depth = li
                .ancestors()
                .filter_map(scraper::ElementRef::wrap)
                .take_while(|e| !std::ptr::eq(e.value(), cell.value()))
                .filter(|e| e.value().name() == "li")
                .count();
            let mut own = li.html();
            for list in li.select(&sel("ul,ol")) {
                own = own.replacen(&list.html(), "", 1);
            }
            let own = Html::parse_fragment(&own);
            let text = text_of(&own.root_element());
            if text.is_empty() {
                continue;
            }
            let source_owner = std::iter::once(li)
                .chain(li.ancestors().filter_map(scraper::ElementRef::wrap))
                .find(|element| {
                    element
                        .parent()
                        .and_then(scraper::ElementRef::wrap)
                        .is_some_and(|parent| std::ptr::eq(parent.value(), cell.value()))
                });
            let source_child_position = source_owner
                .and_then(|owner| {
                    cell.children().position(|node| {
                        scraper::ElementRef::wrap(node)
                            .is_some_and(|element| std::ptr::eq(element.value(), owner.value()))
                    })
                })
                .context("AAP nested remark source owner missing")?;
            let id = format!("aap-summary-context-{row}-{column}-{index}");
            formal.push(Block {
                block_id: id.clone(),
                kind: BlockKind::ListItem,
                text,
                heading_path: vec![],
                page: None,
            });
            contexts.push(json!({"context_block_id":id,"source_role":"summary nested remark context","source_row":row,"source_column":column,"source_list_depth":depth,"source_child_position":source_child_position,"parent_context_block_id":parent_block_id,"source_html":li.html(),"statement_association":"same source cell; no finer per-statement association inferred"}));
        }
    }
    if labels.iter().map(String::as_str).collect::<Vec<_>>() != spec.kas {
        bail!("AAP 2026 actual KAS labels/order differ from verified primary summary")
    }
    // Derived statement/remark blocks follow their native cell/child sequence.
    // The original table and its span/cell HTML remain the source reading representation.
    let mut source_order = std::collections::HashMap::new();
    for record in records.iter().chain(contexts.iter()) {
        let id = record["block_id"]
            .as_str()
            .or_else(|| record["context_block_id"].as_str())
            .context("AAP derived source block identity missing")?;
        source_order.insert(
            id.to_owned(),
            (
                record["source_row"].as_u64().unwrap_or(0),
                record["source_column"].as_u64().unwrap_or(0),
                record["source_child_position"].as_u64().unwrap_or(0),
            ),
        );
    }
    formal.sort_by_key(|block| {
        source_order
            .get(&block.block_id)
            .copied()
            .unwrap_or((u64::MAX, u64::MAX, u64::MAX))
    });
    common::insert_table_recommendations(blocks, &summary_block_id, formal)?;
    let mut gps_found = 0;
    let mut unresolved = vec![];
    for (index, section) in spec.gps.iter().enumerate() {
        let candidates = d
            .select(&sel(&format!("[id='{section}'] p")))
            .filter_map(|p| {
                let fragments = aap_fragments(p, None).ok()?;
                (fragments.len() == 1 && fragments[0].role == "good_practice_statement")
                    .then_some((p, fragments[0].clone()))
            })
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            unresolved.push(json!({"source_section_id":section,"reason":"detailed GPS source paragraph absent, ambiguous, or unrecognized"}));
            continue;
        }
        let (p, fragment) = &candidates[0];
        let text = text_of(p);
        let positions = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| {
                matches!(block.kind, BlockKind::Paragraph | BlockKind::ListItem)
                    && block.text == text
            })
            .map(|(n, _)| n)
            .collect::<Vec<_>>();
        if positions.len() != 1 {
            unresolved.push(json!({"source_section_id":section,"reason":"detailed GPS canonical paragraph association ambiguous"}));
            continue;
        }
        let block = &mut blocks[positions[0]];
        block.kind = BlockKind::Recommendation {
            label: Some(fragment.label.clone()),
            strength: None,
            certainty: None,
            population: None,
            grading_system: None,
            qualifier: None,
        };
        gps_found += 1;
        records.push(json!({"block_id":block.block_id,"source_role":"good_practice_statement","source_section_id":section,"local_order":index,"native_number":null,"source_label":fragment.label,"source_html":p.html(),"source_statement":text,"occurrence_role":"formal detailed GPS; summary wording is separately retained and may differ"}));
    }
    if !unresolved.is_empty() {
        warnings.push("AAP 2026 detailed good-practice coverage unresolved; missing/ambiguous source paragraphs are recorded, not inferred from the abstract count.".into());
    }
    warnings.push("AAP 2026 figure/companion links and native surrounding text remain source context; exact caption-to-image semantics, conflicting companion DOI years and signed supplemental bytes require separate validation.".into());
    warnings.push("AAP 2026 source-observed templates only. Primary table copies are retained once with span grids; summary KAS and detailed GPS are chosen formal occurrences, while all repeated source context remains. These rules do not establish catalog, article/asset completeness or reuse rights.".into());
    Ok(
        json!({"source_summary_section":spec.summary,"source_label_note_identity_binding":{"required_for_this_template":spec.summary=="18672556-content","verified":label_note.is_some(),"source_target_id":label_note.as_ref().and_then(|note|note.target.value().id()),"scope":"Only the verified KAS6 a>sup b marker and uniquely owned primary summary footnote; literal fragment/reveal forms remain distinct and no URL is synthesized"},"source_summary_block_id":summary_block_id,"source_recommendations":records,"source_summary_good_practice_occurrences":summary_gps,"source_tables":source_tables,"source_summary_context":contexts,"coverage":{"actual_summary_KAS":labels.len(),"actual_detailed_GPS":gps_found,"expected_GPS_sections":spec.gps,"unresolved_GPS_sections":unresolved,"all_known_statement_owners_resolved":gps_found==spec.gps.len(),"full_article_or_catalog_complete":false},"reading_order":"Primary table plus expanded native origin grid is the source reading representation; derived statement/remark blocks follow physical cell and direct-child order, with nested remark parent IDs. Exact inline markup remains in native cell/statement HTML.","GPS_identity_policy":"source labels/section IDs retained; local order is not a publisher-assigned GPS number","repeat_policy":"summary/detail/appendix text is preserved independently; only selected formal source occurrences are typed as recommendations"}),
    )
}
fn aap_2026_assets(raw: &str, base: &Url) -> serde_json::Value {
    let d = Html::parse_document(raw);
    let descriptors=d.select(&sel("a[href]")).filter_map(|a| {
        let label=text_of(&a);let low=label.to_lowercase();
        if !(low.contains("pdf")||low.contains("supplement")||low.contains("companion")) { return None; }
        let href=a.value().attr("href")?;let url=common::link(base,href)?;
        Some(json!({"url":url,"source_literal":href,"source_label":label,"source_html":a.html(),"bytes_downloaded":false,"scope":"source-emitted descriptor, not verified asset content or identifier reconciliation"}))
    }).collect::<Vec<_>>();
    json!(descriptors)
}
fn aap_2026_rights(raw: &str) -> serde_json::Value {
    let d = Html::parse_document(raw);
    let notice = |element: scraper::ElementRef<'_>| {
        let text = text_of(&element);
        let low = text.to_lowercase();
        text.len() <= 4096
            && low.contains("free to read")
            && low.contains("not open access")
            && low.contains("permission")
    };
    let statements=d.select(&sel("p,div")).filter(|element|notice(*element)).filter(|element|!element.select(&sel("p,div")).any(|child|!std::ptr::eq(child.value(),element.value())&&notice(child))).map(|element|json!({"raw":text_of(&element),"source_html":element.html(),"scope":"explicit source article free-read/not-Open-Access permission notice"})).collect::<Vec<_>>();
    json!({"explicit_restricted_article_notices":statements,"free_badge_is_not_open_license":true,"absence_policy":"unknown when the exact item notice is not present; no permission inferred from public access or registry"})
}

const AAP_2026_COLLECTION: &str =
    "https://publications.aap.org/pediatrics/collection/524/Latest-AAP-Policy";
const AAP_FALTERING_2026:&str="https://publications.aap.org/pediatrics/article/157/4/e2025075764/206901/Clinical-Practice-Guideline-for-Diagnosis-and";
const AAP_UTI_2026:&str="https://publications.aap.org/pediatrics/article/158/4/e2026078565/209460/Clinical-Practice-Guideline-for-the-Diagnosis-and?searchresult=1";
fn aap_seeds(src: &SourceEntry) -> Result<Option<Vec<(&str, &'static str, usize)>>> {
    let only = src.config["source_linked_only"].as_bool() == Some(true);
    let Some(value) = src.config.get("source_linked_documents") else {
        if only {
            bail!("AAP source-linked-only scope requires explicit verified articles")
        }
        return Ok(None);
    };
    // This optional route is a deliberately finite source-observed scope, never a
    // fallback after a catalog denial and never a claim of JS collection pagination.
    if !only {
        bail!("AAP verified article seeds require explicit source_linked_only=true")
    }
    let seeds = value
        .as_array()
        .context("AAP source-linked articles must be an array")?;
    if seeds.is_empty() || seeds.len() > 2 {
        bail!("AAP source-linked scope supports only the two observed 2026 articles")
    }
    let catalog = src
        .catalog_url
        .as_deref()
        .unwrap_or(PublisherSource::Aap.root());
    if catalog != PublisherSource::Aap.root() && catalog != AAP_2026_COLLECTION {
        bail!("AAP verified seeds require the official policy hub or observed collection524")
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    for seed in seeds {
        let url = seed["url"]
            .as_str()
            .context("AAP source-linked article URL absent")?;
        if seed["source_evidence_url"].as_str() != Some(AAP_2026_COLLECTION) || !seen.insert(url) {
            bail!("AAP verified collection evidence absent or article duplicated")
        }
        let (doi,page)=match url {
            AAP_FALTERING_2026=>("10.1542/peds.2025-075764",2),
            AAP_UTI_2026=>("10.1542/peds.2026-078565",1),
            _=>bail!("AAP seed is not an exact source-observed 2026 article; query/route variants are not synthesized"),
        };
        out.push((url, doi, page));
    }
    Ok(Some(out))
}
async fn aap_seed_discover(
    src: &SourceEntry,
    f: &Fetcher,
    limit: Option<usize>,
) -> Result<Option<Vec<PlanRow>>> {
    let Some(seeds) = aap_seeds(src)? else {
        return Ok(None);
    };
    let mut rows = vec![];
    for (url, expected_doi, page) in seeds.into_iter().take(limit.unwrap_or(usize::MAX)) {
        let result:Result<PlanRow>=async {
            let fetched=f.get_ok(url).await?;
            if fetched.record.final_url!=url { bail!("AAP source-linked terminal differs from exact observed URL; no fallback") }
            let base=Url::parse(url)?;
            let (title,body,_,doi)=parse_article(PublisherSource::Aap,&fetched.text(),&base)?;
            if doi.as_deref()!=Some(expected_doi) { bail!("AAP source-linked article DOI differs from observed source metadata") }
            let hash=publisher_snapshot(PublisherSource::Aap,&fetched.text(),&body,&base)?;
            Ok(PlanRow { source_id:src.id.clone(),doc_key:format!("aap-{}-{}",common::short_hash(url),&hash[..12]),url:url.into(),title:Some(title),rendition:Some("html".into()),version:None,
                extra:json!({"snapshot":hash,"source_doi":doi,"aap_source_state":aap_source_state(&fetched.text(),&base)?,"source_linked_scope":true,"source_evidence_url":AAP_2026_COLLECTION,"source_evidence_page":page,"evidence_scope":"Independent browser observation of actual collection524 card; page number is UI provenance, not a guessed request parameter","catalog_incomplete":true,"catalog_runtime_enumerated":false}) })
        }.await;
        let row = match result {
            Ok(row) => row,
            Err(error) => {
                let mut row=issue(src,url.into(),format!("AAP exact source-linked article unresolved: {error:#}; no fallback route attempted"));
                row.extra["source_linked_scope"] = json!(true);
                row.extra["source_evidence_url"] = json!(AAP_2026_COLLECTION);
                row.extra["source_evidence_page"] = json!(page);
                row
            }
        };
        rows.push(row);
    }
    Ok(Some(rows))
}

// The independently observed publisher leaf is a first-choice bounded route,
// never a fallback after the society redirect or a publisher access gate fails.
const ERS_2026: &str = "https://publications.ersnet.org/content/erj/68/2/2600097";
const ERS_2026_CATALOG_LINK: &str =
    "https://channel.ersnet.org/container-iframe-redirect.php?contId=121740&vdoId=191682";
fn ers_seed(src: &SourceEntry) -> Result<Option<(&str, &str)>> {
    let Some(seeds) = src.config["source_linked_documents"].as_array() else {
        return Ok(None);
    };
    if seeds.len() != 1 {
        bail!("ERS explicit source-linked scope currently supports one verified 2026 article");
    }
    let seed = &seeds[0];
    let url = seed["url"]
        .as_str()
        .context("ERS source-linked URL missing")?;
    let evidence = seed["source_evidence_url"]
        .as_str()
        .context("ERS source evidence URL missing")?;
    let catalog = src
        .catalog_url
        .as_deref()
        .context("ERS official catalog missing")?;
    let u = Url::parse(url)?;
    if catalog != PublisherSource::Ers.root()
        || evidence != catalog
        || url != ERS_2026
        || u.scheme() != "https"
        || u.host_str() != Some("publications.ersnet.org")
        || u.query().is_some()
        || u.fragment().is_some()
        || !u.username().is_empty()
        || u.password().is_some()
    {
        bail!("ERS explicit leaf is not the independently observed HTTPS publisher article with official source evidence");
    }
    if seed["source_catalog_link"].as_str() != Some(ERS_2026_CATALOG_LINK) {
        bail!("ERS source-emitted catalog redirect evidence mismatch");
    }
    Ok(Some((url, evidence)))
}
async fn ers_seed_discover(src: &SourceEntry, f: &Fetcher) -> Result<Option<Vec<PlanRow>>> {
    let Some((url, evidence)) = ers_seed(src)? else {
        return Ok(None);
    };
    let terminal = match f.get_ok(url).await {
        Ok(p) => p,
        Err(e) => {
            return Ok(Some(vec![issue(
                src,
                url.into(),
                format!(
                    "source-linked publisher fetch failed: {e:#}; no alternate route attempted"
                ),
            )]))
        }
    };
    let base = Url::parse(&terminal.record.final_url)?;
    if base.as_str() != url {
        return Ok(Some(vec![issue(
            src,
            url.into(),
            "Source-linked ERS publisher left the observed canonical article; no fallback".into(),
        )]));
    }
    let (title, body, _, doi) = match parse_article(PublisherSource::Ers, &terminal.text(), &base) {
        Ok(v) => v,
        Err(e) => {
            return Ok(Some(vec![issue(
                src,
                url.into(),
                format!("source-linked publisher template: {e:#}"),
            )]))
        }
    };
    if doi.as_deref() != Some("10.1183/13993003.00097-2026") {
        bail!("ERS verified publisher leaf DOI mismatch");
    }
    let hash = crate::common::sha256_hex(body.as_bytes());
    Ok(Some(vec![PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("ers-{}-{}", common::short_hash(url), &hash[..12]),
        url: url.into(),
        title: Some(title),
        rendition: Some("html".into()),
        version: None,
        extra: json!({"snapshot":hash,"source_evidence_url":evidence,"source_catalog_link":ERS_2026_CATALOG_LINK,"source_linked_scope":true,"source_doi":doi,"catalog_incomplete":true,"catalog_runtime_enumerated":false,"coverage":"One independently source-observed article; current catalog, pagination, endorsement and archive reconciliation remain unverified"}),
    }]))
}

fn ers_detailed_recommendations(
    body: &str,
    base: &Url,
    blocks: &mut [crate::guidelines::doc::Block],
    warnings: &mut Vec<String>,
) -> Result<serde_json::Value> {
    use scraper::ElementRef;
    let d = Html::parse_document(body);
    let strength_re =
        regex::Regex::new(r"(?i)\b(?:strong|conditional|weak) recommendation(?:s)?\b")?;
    let certainty_re = regex::Regex::new(
        r"(?i)\b(?:very[ -]low|low|moderate|high)(?:[ -]quality evidence|[ -]certainty(?: of evidence| evidence)?)\b",
    )?;
    let remark_re = regex::Regex::new(r"(?i)^remarks?\b")?;
    let question_re = regex::Regex::new(r"(?i)^(?:PICO|narrative) question\s+\d+\b")?;
    let mut source = vec![];
    let mut sections = 0;
    let mut cursor = 0;
    let mut remarks = 0;
    for h in d.select(&sel("div.section > h4.section-title")) {
        let label = text_of(&h);
        if !matches!(label.as_str(), "Recommendation" | "Recommendations") {
            continue;
        }
        sections += 1;
        let question = h
            .ancestors()
            .filter_map(ElementRef::wrap)
            .filter(|e| e.value().classes().any(|c| c == "section"))
            .find_map(|section| {
                section.children().filter_map(ElementRef::wrap).find(|e| {
                    e.value().name() == "h3" && e.value().classes().any(|c| c == "section-title")
                })
            });
        let question =
            question.context("ERS recommendation section has no source question heading")?;
        let question_text = text_of(&question);
        if !question_re.is_match(&question_text) {
            bail!("ERS recommendation parent is not an observed PICO/narrative question");
        }
        let mut found = 0;
        for sibling in h.next_siblings().filter_map(ElementRef::wrap) {
            if sibling.value().name() != "p" {
                break;
            }
            let printed = text_of(&sibling);
            if remark_re.is_match(printed.trim_start()) {
                remarks += 1;
                continue;
            }
            if printed.trim().is_empty() {
                continue;
            }
            let strengths=strength_re.find_iter(&printed).map(|m|json!({"printed":m.as_str(),"canonical_text_byte_start":m.start(),"canonical_text_byte_end":m.end()})).collect::<Vec<_>>();
            let certainties=certainty_re.find_iter(&printed).map(|m|json!({"printed":m.as_str(),"canonical_text_byte_start":m.start(),"canonical_text_byte_end":m.end()})).collect::<Vec<_>>();
            if strengths.is_empty() || certainties.is_empty() {
                bail!("ERS source-delimited recommendation paragraph lacks its native strength/certainty; changed template requires review");
            }
            let position = (cursor..blocks.len())
                .find(|i| {
                    let b = &blocks[*i];
                    matches!(b.kind, BlockKind::Paragraph)
                        && b.text == printed
                        && b.heading_path.iter().any(|p| p == &question_text)
                        && b.heading_path.last() == Some(&label)
                })
                .context(
                    "ERS source recommendation paragraph not preserved under its question path",
                )?;
            cursor = position + 1;
            let strength = (strengths.len() == 1)
                .then(|| strengths[0]["printed"].as_str().unwrap().to_string());
            let certainty = (certainties.len() == 1)
                .then(|| certainties[0]["printed"].as_str().unwrap().to_string());
            if strengths.len() != 1 || certainties.len() != 1 {
                warnings.push("ERS statement has multiple native grade phrases; all source spans retained and ambiguous scalar grade fields left unknown.".into());
            }
            let b = &mut blocks[position];
            b.kind = BlockKind::Recommendation {
                label: None,
                strength,
                certainty,
                population: None,
                grading_system: None,
                qualifier: None,
            };
            // Canonical paragraph retains the generic extractor's whitespace-normalized text;
            // decoded source text and source HTML retain exact inline adjacency and evidence.
            let decoded = sibling.text().collect::<String>();
            source.push(json!({"block_id":b.block_id,"source_statement_index":found,"source_section_index":sections-1,
                "source_paragraph_id":sibling.value().id(),"source_recommendation_heading":label,"source_recommendation_heading_id":h.value().id(),
                "source_question_heading":question_text,"source_question_heading_id":question.value().id(),"decoded_source_text":decoded,"canonical_text":printed,
                "native_strength_spans":strengths,"native_certainty_spans":certainties,"source_html":sibling.html(),"scope":"One source sibling paragraph, never sentence-split; source table summaries are separate objects"}));
            found += 1;
        }
        if found == 0 {
            bail!(
                "ERS detailed recommendation heading has no recognized source statement paragraphs"
            );
        }
        if label == "Recommendation" && found != 1 {
            warnings.push("ERS singular Recommendation section has multiple source paragraphs; retained as distinct source statements for review.".into());
        }
    }
    if base.as_str() == ERS_2026 && (sections != 12 || source.len() != 13) {
        bail!("ERS observed 2026 article changed its 12 recommendation sections / 13 statement paragraphs; revalidate source boundaries");
    }
    let table_roles=d.select(&sel("#TB1,#TB2")).map(|t|json!({"source_id":t.value().id(),"role":if t.value().id()==Some("TB1"){"recommendation_summary_table"}else{"research_priorities_and_rationale"},"independent_recommendations":false,"source_html":t.html()})).collect::<Vec<_>>();
    Ok(
        json!({"source_recommendations":source,"source_recommendation_sections":sections,"source_remark_paragraphs":remarks,"source_table_roles":table_roles,"source_order_note":"Detailed source section/paragraph boundaries define statements; summary tables and research priorities are retained without duplicate recommendations"}),
    )
}

// ACOG: the society's listing pages (/clinical/guidance/clinical-practice-guideline/ and /clinical-consensus/) are
// client-rendered (`DynamicContentListing` fed by a search service), so their HTML carries no item links. The
// sitemap that www.acog.org/robots.txt declares lists every guidance page; it is the enumeration route here.
const ACOG_SITEMAP: &str = "https://www.acog.org/sitemap.xml";
const ACOG_BRANCHES: [&str; 2] = [
    "/clinical/guidance/clinical-practice-guideline/",
    "/clinical/guidance/clinical-consensus/",
];
fn acog_sitemap_items(xml: &str) -> Result<Vec<String>> {
    let re = regex::Regex::new(r"<loc>\s*([^<\s]+)\s*</loc>")?;
    let mut out: Vec<String> = vec![];
    let mut seen = HashSet::new();
    for c in re.captures_iter(xml) {
        let raw = c[1].replace("&amp;", "&");
        let Ok(u) = Url::parse(&raw) else { continue };
        if u.host_str() != Some("www.acog.org") {
            continue;
        }
        let path = u.path();
        let Some(branch) = ACOG_BRANCHES.iter().find(|b| path.starts_with(**b)) else { continue };
        // year/month/slug below the branch; the branch listing pages themselves are not documents
        if path[branch.len()..].trim_matches('/').split('/').count() != 3 {
            continue;
        }
        if seen.insert(u.to_string()) {
            out.push(u.to_string());
        }
    }
    if out.is_empty() {
        bail!("ACOG sitemap lists no clinical-practice-guideline or clinical-consensus pages");
    }
    // newest first by the /YYYY/MM/ the URLs carry, then URL; deterministic
    out.sort_by(|a, b| {
        let k = |u: &str| u.split('/').filter(|x| x.len() == 4 || x.len() == 2).filter(|x| x.chars().all(|c| c.is_ascii_digit())).collect::<Vec<_>>().join("-");
        k(b).cmp(&k(a)).then(a.cmp(b))
    });
    Ok(out)
}
struct AcogPage {
    title: String,
    eyebrow: String,
    access: Option<String>,
    meta: Vec<String>,
    journal_url: Option<String>,
    body: String,
}
fn acog_page(raw: &str, base: &Url) -> Result<AcogPage> {
    let d = Html::parse_document(raw);
    let banner = d.select(&sel("main section[data-component='PageBanner']")).next().context("ACOG page banner missing")?;
    let content = d.select(&sel("main section[data-component='ClinicalGuidanceContent']")).next().context("ACOG clinical guidance content missing")?;
    let title = banner.select(&sel("h1.page-banner__title")).next().map(|h| text_of(&h)).filter(|t| !t.is_empty()).context("ACOG title missing")?;
    let eyebrow = banner.select(&sel(".page-banner__eyebrow")).next().map(|h| text_of(&h)).unwrap_or_default();
    if !matches!(eyebrow.as_str(), "Clinical Practice Guideline" | "Clinical Consensus") {
        bail!("ACOG page type label {eyebrow:?} is not a clinical practice guideline or clinical consensus");
    }
    let access = banner.select(&sel(".page-banner__access-level")).next().map(|h| text_of(&h)).filter(|t| !t.is_empty());
    let meta = banner.select(&sel(".page-banner__meta-section")).map(|h| text_of(&h)).filter(|t| !t.is_empty()).collect();
    let journal_url = content.select(&sel(".cgc-actions__green-journal a[href]")).next().and_then(|a| a.value().attr("href")).and_then(|h| common::link(base, h));
    let layout = content.select(&sel(".clinical-guidance-content__layout")).next().context("ACOG content layout missing")?;
    if text_of(&layout).len() < 200 {
        bail!("ACOG content layout is empty or gated");
    }
    Ok(AcogPage { title, eyebrow, access, meta, journal_url, body: layout.html() })
}
fn acog_snapshot(p: &AcogPage) -> String {
    crate::common::sha256_hex(serde_json::to_string(&(&p.title, &p.eyebrow, &p.access, &p.meta, &p.journal_url, &p.body)).unwrap_or_default().as_bytes())
}
async fn acog_discover(src: &SourceEntry, f: &Fetcher, limit: Option<usize>, out: &mut Vec<PlanRow>) -> Result<()> {
    let sitemap = src.config["sitemap_url"].as_str().unwrap_or(ACOG_SITEMAP);
    let sm = f.get_ok(sitemap).await.with_context(|| format!("ACOG sitemap {sitemap}"))?;
    let items = acog_sitemap_items(&sm.text())?;
    let total = items.len();
    for url in items {
        if limit.is_some_and(|n| out.len() >= n) {
            break;
        }
        let page = match f.get_ok(&url).await {
            Ok(p) => p,
            Err(e) => {
                out.push(issue(src, url, format!("ACOG guidance page unavailable: {e:#}")));
                continue;
            }
        };
        let base = Url::parse(&page.record.final_url)?;
        let parsed = match acog_page(&page.text(), &base) {
            Ok(p) => p,
            Err(e) => {
                out.push(issue(src, url, format!("ACOG guidance page template: {e:#}")));
                continue;
            }
        };
        // The page's own "Full text & PDF" link states the journal DOI: an Open Access PMC copy wins when it exists.
        let mut crosswalk = Value::Null;
        if let Some(doi) = parsed.journal_url.as_deref().and_then(cw::doi_from_url) {
            let sref = SourceRef { source_url: parsed.journal_url.clone().unwrap_or_default(), evidence_url: Some(url.clone()), source_title: Some(parsed.title.clone()),
                item_type: Some(common::slug(&parsed.eyebrow).replace('-', "_")), key_prefix: "acog".into(), id_route: "source_doi".into(), notes: vec![] };
            match cw::plan(src, f, Lookup::Doi(doi.clone()), sref, json!({"acog_page": url, "source_doi": doi})).await? {
                Planned::Open(row) => {
                    out.push(row);
                    continue;
                }
                Planned::Closed { reason, .. } => crosswalk = json!({"doi": doi, "outcome": reason}),
            }
        }
        let hash = acog_snapshot(&parsed);
        out.push(PlanRow {
            source_id: src.id.clone(),
            doc_key: format!("acog-{}-{}", common::short_hash(&url), &hash[..12]),
            url,
            title: Some(parsed.title),
            rendition: Some("html".into()),
            version: None,
            extra: json!({"snapshot": hash, "acog_page": true, "sitemap_url": sitemap, "sitemap_items": total, "open_access_crosswalk": crosswalk,
                "catalog_note": "Listing pages are client-rendered (DynamicContentListing); enumeration uses the robots.txt-declared sitemap."}),
        });
    }
    Ok(())
}
async fn acog_extract(src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
    let p = f.get_ok(&row.url).await?;
    let base = Url::parse(&p.record.final_url)?;
    let page = acog_page(&p.text(), &base)?;
    if row.extra["snapshot"].as_str() != Some(acog_snapshot(&page).as_str()) {
        bail!("ACOG page changed; re-plan");
    }
    let (blocks, mut warnings) = blocks_from_html(&page.body, Some(&base), None);
    let gated = page.access.as_deref().is_some_and(|a| a.to_lowercase().contains("member"));
    let label = common::slug(&page.eyebrow).replace('-', "_");
    let item = if gated || page.journal_url.is_some() { format!("{label}_summary_page") } else { label };
    if gated || page.journal_url.is_some() {
        warnings.push(format!(
            "acog.org shows this item's summary only (access label: {}); the full text is the journal article ({}), which answers HTTP 403 to automated access and has no PMC Open Access copy{}.",
            page.access.as_deref().unwrap_or("none"),
            page.journal_url.as_deref().unwrap_or("no link"),
            row.extra["open_access_crosswalk"]["outcome"].as_str().map(|o| format!(" ({o})")).unwrap_or_default()
        ));
    }
    let date_observations: Vec<DateObservation> = page
        .meta
        .iter()
        .filter(|m| common::date("", m, "").normalized.is_some())
        .map(|m| common::date("page banner date", m, base.as_str()))
        .collect();
    let mut attachments = vec![];
    let mut relations = vec![];
    if let Some(j) = &page.journal_url {
        attachments.push(json!({"url": j, "label": "Full text & PDF on Obstetrics & Gynecology", "role": "journal_full_text", "rights": "unknown"}));
        relations.push(Relation { rel: "related".into(), target: j.clone(), scope: Some("full text in Obstetrics & Gynecology, as linked by the page".into()), evidence_url: Some(base.to_string()) });
    }
    let banner_terms = Html::parse_document(&p.text())
        .select(&sel(".page-banner__subtitle"))
        .next()
        .map(|e| text_of(&e))
        .filter(|t| !t.is_empty());
    let mut rights = Rights { status: "restricted".into(), terms_url: Some("https://www.acog.org/legal/terms-of-use".into()), evidence_urls: vec![base.to_string(), "https://www.acog.org/robots.txt".into()],
        note: Some("ACOG terms of use apply to the page; robots.txt states content signals for automated use.".into()), ..Default::default() };
    if let Some(t) = banner_terms {
        rights.restrictions.push(t);
    }
    rights.restrictions.push("robots.txt Content-Signal: search=yes, ai-train=no, use=reference".into());
    if let Some(a) = &page.access {
        rights.restrictions.push(format!("access label: {a}"));
    }
    Ok(GuidelineDoc {
        source_id: src.id.clone(),
        doc_key: row.doc_key.clone(),
        work_key: Some(format!("acog-{}", common::short_hash(&row.url))),
        item_type: Some(item),
        title: page.title,
        url: row.url.clone(),
        status: "unknown".into(),
        date_observations,
        relations,
        rights,
        search_metadata: json!({"acog": {"type_label": page.eyebrow, "access_label": page.access, "banner": page.meta, "sitemap_url": row.extra["sitemap_url"]}}),
        blocks,
        attachments,
        fetches: vec![p.record],
        warnings,
        ..Default::default()
    })
}

#[async_trait::async_trait]
impl Adapter for PublisherAdapter {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let kind = self.0;
        // Registry-configured Europe PMC enumeration of the society's Open Access guidelines (catalog-independent).
        let mut pre = cw::enumerate(src, f, kind.name(), limit, &HashSet::new()).await?;
        if limit.is_some_and(|n| pre.len() >= n) {
            return Ok(pre);
        }
        if matches!(kind, PublisherSource::Acog) {
            acog_discover(src, f, limit, &mut pre).await?;
            return Ok(pre);
        }
        if matches!(kind, PublisherSource::Aap) {
            if let Some(rows) = aap_seed_discover(src, f, limit.map(|n| n - pre.len())).await? {
                pre.extend(rows);
                return Ok(pre);
            }
        }
        let seed_rows = if matches!(kind, PublisherSource::Ers) {
            ers_seed_discover(src, f).await?
        } else {
            None
        };
        let mut q = VecDeque::from([src
            .catalog_url
            .clone()
            .unwrap_or_else(|| kind.root().into())]);
        let mut visited = HashSet::new();
        let mut emitted = HashSet::new();
        let mut emitted_dois = HashSet::new();
        for row in &pre {
            if let Some(doi) = row.extra["pmc_crosswalk"]["jats_doi"].as_str() {
                emitted_dois.insert(doi.to_ascii_lowercase());
            }
        }
        let mut out = pre;
        if let Some(rows) = seed_rows {
            if src.config["source_linked_only"].as_bool() == Some(true) {
                out.extend(rows);
                return Ok(out);
            }
            for row in rows {
                emitted.insert(row.url.clone());
                // This source-emitted redirect is known to identify the same observed work.
                // Do not retry it after a seed denial or emit the work twice later in catalog traversal.
                emitted.insert(ERS_2026_CATALOG_LINK.to_string());
                if let Some(doi) = row.extra["source_doi"].as_str() {
                    emitted_dois.insert(doi.to_ascii_lowercase());
                }
                out.push(row);
                if limit.is_some_and(|n| out.len() >= n) {
                    return Ok(out);
                }
            }
        }
        while let Some(url) = q.pop_front() {
            if !visited.insert(url.clone()) {
                continue;
            }
            if visited.len() > 100 {
                out.push(issue(
                    src,
                    url,
                    "catalog bound exceeded; inventory incomplete".into(),
                ));
                break;
            }
            let page = match f.get_ok(&url).await {
                Ok(p) => p,
                Err(e) => {
                    out.push(issue(src, url, format!("catalog fetch failed: {e:#}")));
                    if limit.is_some_and(|n| out.len() >= n) {
                        return Ok(out);
                    }
                    continue;
                }
            };
            let base = Url::parse(&page.record.final_url)?;
            if !kind.is_catalog(&base) {
                out.push(issue(
                    src,
                    url,
                    "catalog redirected outside recognized source routes".into(),
                ));
                if limit.is_some_and(|n| out.len() >= n) {
                    return Ok(out);
                }
                continue;
            }
            let (links, cats) = match clinical_links(kind, &page.text(), &base) {
                Ok(v) => v,
                Err(e) => {
                    out.push(issue(src, url, format!("catalog: {e:#}")));
                    if limit.is_some_and(|n| out.len() >= n) {
                        return Ok(out);
                    }
                    continue;
                }
            };
            for c in cats {
                if !visited.contains(&c) {
                    q.push_back(c)
                }
            }
            for (u, t) in links {
                if !emitted.insert(u.clone()) {
                    continue;
                }
                let terminal = match f.get_ok(&u).await {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(issue(src, u, format!("terminal fetch failed: {e:#}")));
                        if limit.is_some_and(|n| out.len() >= n) {
                            return Ok(out);
                        }
                        continue;
                    }
                };
                let final_url = Url::parse(&terminal.record.final_url)?;
                if !kind.is_terminal(&final_url) {
                    out.push(issue(
                        src,
                        u,
                        "terminal redirect outside recognized routes".into(),
                    ));
                    if limit.is_some_and(|n| out.len() >= n) {
                        return Ok(out);
                    }
                    continue;
                }
                if final_url.as_str() != u && !emitted.insert(final_url.to_string()) {
                    continue;
                }
                let (_, body, _, terminal_doi) =
                    match parse_article(kind, &terminal.text(), &final_url) {
                        Ok(v) => v,
                        Err(e) => {
                            out.push(issue(src, u, format!("terminal template: {e:#}")));
                            if limit.is_some_and(|n| out.len() >= n) {
                                return Ok(out);
                            }
                            continue;
                        }
                    };
                if matches!(kind, PublisherSource::Ers) {
                    if let Some(doi) = terminal_doi {
                        if !emitted_dois.insert(doi.to_ascii_lowercase()) {
                            continue;
                        }
                    }
                }
                let hash = publisher_snapshot(kind, &terminal.text(), &body, &final_url)?;
                let mut extra = json!({"catalog_incomplete":true,"snapshot":hash});
                if matches!(kind, PublisherSource::Aap) {
                    extra["aap_source_state"] = aap_source_state(&terminal.text(), &final_url)?;
                }
                out.push(PlanRow {
                    source_id: src.id.clone(),
                    doc_key: format!("{}-{}-{}", kind.name(), common::short_hash(&u), &hash[..12]),
                    url: u,
                    title: Some(t),
                    rendition: Some("html".into()),
                    version: None,
                    extra,
                });
                if limit.is_some_and(|n| out.len() >= n) {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(e) = row.extra["discovery_error"].as_str() {
            bail!("{e}")
        };
        if cw::is_crosswalk_row(row) {
            return cw::extract(src, f, row).await;
        }
        if matches!(self.0, PublisherSource::Acog) && row.extra["acog_page"] == true {
            return acog_extract(src, f, row).await;
        }
        if matches!(self.0, PublisherSource::Ers)
            && row.extra["source_linked_scope"].as_bool() == Some(true)
        {
            let (url, evidence) =
                ers_seed(src)?.context("ERS source-linked config missing for planned article")?;
            if row.url != url || row.extra["source_evidence_url"] != evidence {
                bail!("ERS source-linked plan evidence differs from configured route");
            }
        }
        if matches!(self.0, PublisherSource::Aap) && row.extra["source_linked_scope"] == true {
            let seeds =
                aap_seeds(src)?.context("AAP source-linked config absent for planned article")?;
            if !seeds.iter().any(|(url, doi, page)| {
                *url == row.url
                    && row.extra["source_doi"] == *doi
                    && row.extra["source_evidence_page"] == *page
            }) {
                bail!("AAP source-linked plan no longer matches exact configured observed scope")
            }
            if row.extra["source_evidence_url"] != AAP_2026_COLLECTION {
                bail!("AAP source-linked evidence changed")
            }
        }
        let p = f.get_ok(&row.url).await?;
        if matches!(self.0, PublisherSource::Aap)
            && row.extra["source_linked_scope"] == true
            && p.record.final_url != row.url
        {
            bail!("AAP source-linked terminal changed from exact observed URL; no fallback")
        }
        let base = Url::parse(&p.record.final_url)?;
        if !self.0.is_terminal(&base) {
            bail!("publisher terminal redirected outside recognized routes")
        }
        let (title, body, item, doi) = parse_article(self.0, &p.text(), &base)?;
        let hash = publisher_snapshot(self.0, &p.text(), &body, &base)?;
        if row.extra["snapshot"].as_str() != Some(hash.as_str()) {
            bail!("article changed; re-plan")
        };
        let (mut blocks, mut warnings) = blocks_from_html(&body, Some(&base), None);
        let label_re = regex::Regex::new(r"(?i)^(Recommendation\s+[0-9]+(?:\.[0-9]+)*)").unwrap();
        let strength_re =
            regex::Regex::new(r"(?i)\b(strong|conditional|weak) recommendation\b").unwrap();
        let certainty_re = regex::Regex::new(
            r"(?i)\b(very low|low|moderate|high)(?:-quality| certainty of) evidence\b",
        )
        .unwrap();
        let aap_state = if matches!(self.0, PublisherSource::Aap) {
            Some(aap_source_state(&p.text(), &base)?)
        } else {
            None
        };
        let aap_kas = if let Some(spec) = aap_state
            .as_ref()
            .and_then(|state| state["doi"].as_str())
            .and_then(aap_2026)
        {
            Some(aap_2026_statements(
                &body,
                spec,
                &mut blocks,
                &mut warnings,
            )?)
        } else if aap_state
            .as_ref()
            .is_some_and(|state| state["doi"] == AAP_OPIOID_DOI)
        {
            aap_kas_summary(&body, &mut blocks)?
        } else {
            None
        };
        let ers_source = if matches!(self.0, PublisherSource::Ers) {
            let source = ers_detailed_recommendations(&body, &base, &mut blocks, &mut warnings)?;
            if source["source_recommendation_sections"]
                .as_u64()
                .unwrap_or(0)
                > 0
            {
                Some(source)
            } else {
                None
            }
        } else {
            None
        };
        if ers_source.is_none() && aap_kas.is_none() {
            for b in &mut blocks {
                if matches!(b.kind, BlockKind::Paragraph | BlockKind::ListItem) {
                    if let Some(label) = label_re.captures(b.text.trim_start()) {
                        let strength = strength_re.find(&b.text).map(|m| m.as_str().to_string());
                        let certainty = certainty_re.find(&b.text).map(|m| m.as_str().to_string());
                        b.kind = BlockKind::Recommendation {
                            label: Some(label[1].to_string()),
                            strength,
                            certainty,
                            population: None,
                            grading_system: None,
                            qualifier: None,
                        };
                    }
                }
            }
        }
        let version_re = regex::Regex::new(r"(?i)\bversion\s+([0-9]+(?:\.[0-9]+)+)").unwrap();
        let version = version_re.captures(&title).map(|c| c[1].to_string());
        let document = Html::parse_document(&p.text());
        let mut attachments = vec![];
        let mut asset_seen = HashSet::new();
        for a in document.select(&sel("a[href]")) {
            let label = text_of(&a);
            let low = label.to_lowercase();
            if !(low.contains("pdf")
                || low.contains("supplement")
                || (matches!(self.0, PublisherSource::Ers)
                    && (low.contains("pocket") || low.contains("slide"))))
            {
                continue;
            }
            if let Some(url) = a.value().attr("href").and_then(|h| common::link(&base, h)) {
                if asset_seen.insert(url.clone()) {
                    attachments.push(json!({"url":url,"label":label,"role":if matches!(self.0,PublisherSource::Ers)&&low.contains("pocket"){"pocket_guideline"}else if matches!(self.0,PublisherSource::Ers)&&low.contains("slide"){"slide_kit"}else if low.contains("supplement"){"supplement"}else{"pdf"},"rights":"unknown"}));
                }
            }
        }
        // Citation metadata is an explicitly publisher-supplied publication date; retain raw precision.
        let mut date_observations: Vec<DateObservation> = document
            .select(&sel(
                "meta[name='citation_online_date'], meta[name='citation_publication_date']",
            ))
            .filter_map(|e| Some((e.value().attr("name")?, e.value().attr("content")?)))
            .filter(|(_, v)| !v.trim().is_empty())
            .map(|(name, value)| DateObservation {
                label: name.to_owned(),
                raw: value.to_owned(),
                normalized: if matches!(self.0, PublisherSource::Ers) {
                    common::date(name, value, base.as_str()).normalized
                } else {
                    None
                },
                source_url: Some(base.to_string()),
            })
            .collect();
        if let Some(state) = &aap_state {
            if let Some(observed) = state["retirement_date_observations"].as_array() {
                for date in observed {
                    date_observations.push(serde_json::from_value(date.clone())?);
                }
            }
        }
        // Only an explicit link label asserts a companion relation; no inferred version merging.
        let mut relations = vec![];
        let mut rel_seen = HashSet::new();
        for a in document.select(&sel("a[href]")) {
            let label = text_of(&a);
            let low = label.to_lowercase();
            if !(low.contains("companion")
                && (low.contains("guideline") || low.contains("technical report")))
            {
                continue;
            }
            if let Some(target) = a.value().attr("href").and_then(|h| common::link(&base, h)) {
                if target != base.as_str() && rel_seen.insert(target.clone()) {
                    relations.push(Relation {
                        rel: "related".into(),
                        target,
                        scope: Some(label),
                        evidence_url: Some(base.to_string()),
                    });
                }
            }
        }
        warnings.push("Catalog enumeration, structured recommendation grades, explicit version relationships, asset rights and full extraction require validation; source text remains unmodified and missing metadata is unknown.".into());
        let mut search_metadata=ers_source.map(|mut metadata|{metadata["source_route_evidence"]=json!({"source_evidence_url":row.extra["source_evidence_url"],"source_catalog_link":row.extra["source_catalog_link"],"catalog_incomplete":true,"catalog_runtime_enumerated":row.extra["catalog_runtime_enumerated"]});metadata}).unwrap_or(serde_json::Value::Null);
        let status = aap_state
            .as_ref()
            .and_then(|s| s["source_state"].as_str())
            .unwrap_or("unknown")
            .to_string();
        let mut rights = Rights {
            status: "unknown".into(),
            note: Some("Public access does not establish reuse rights.".into()),
            ..Default::default()
        };
        if aap_state
            .as_ref()
            .and_then(|state| state["doi"].as_str())
            .and_then(aap_2026)
            .is_some()
        {
            let evidence = aap_2026_rights(&p.text());
            if let Some(notices) = evidence["explicit_restricted_article_notices"]
                .as_array()
                .filter(|notices| !notices.is_empty())
            {
                rights.status = "restricted".into();
                rights.restrictions = notices
                    .iter()
                    .filter_map(|notice| notice["raw"].as_str().map(str::to_owned))
                    .collect();
                rights.evidence_urls.push(base.to_string());
                rights.note=Some("Source article expressly distinguishes free reading from Open Access and requires permission; no open reuse license inferred.".into());
            }
        }
        if let Some(state) = aap_state {
            search_metadata = json!({"source_state":state,"kas_summary":aap_kas,"source_snapshot_sha256":hash,"catalog_currentness":"unknown; inclusion does not establish active state"});
            if state["doi"].as_str().and_then(aap_2026).is_some() {
                search_metadata["source_article_rights"] = aap_2026_rights(&p.text());
                search_metadata["source_asset_and_companion_descriptors"] =
                    aap_2026_assets(&p.text(), &base);
            }
            if row.extra["source_linked_scope"] == true {
                search_metadata["source_discovery"] = json!({"source_evidence_url":row.extra["source_evidence_url"],"source_evidence_page":row.extra["source_evidence_page"],"source_linked_scope":true,"catalog_runtime_enumerated":false,"full_catalog_complete":false,"evidence_scope":row.extra["evidence_scope"]});
            }
            warnings.push("AAP retirement and reaffirmation are source article-level state/event observations. Neither catalog membership nor reaffirmation establishes current guidance. Native Evidence Quality stays source metadata without inferred GRADE certainty.".into());
        }
        Ok(GuidelineDoc {
            source_id: src.id.clone(),
            doc_key: row.doc_key.clone(),
            work_key: if matches!(self.0, PublisherSource::Aap) {
                doi.as_deref().map(common::slug)
            } else {
                None
            },
            item_type: Some(item),
            title,
            url: row.url.clone(),
            status,
            version,
            doi,
            published: if matches!(self.0, PublisherSource::Ers) {
                date_observations
                    .iter()
                    .find(|d| d.label == "citation_publication_date")
                    .and_then(|d| d.normalized.clone())
            } else {
                None
            },
            date_observations,
            relations,
            rights,
            search_metadata,
            blocks,
            attachments,
            fetches: vec![p.record],
            warnings,
            ..Default::default()
        })
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const AAP: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/aap/synthetic-article.html"
    ));
    const ASCO: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/asco/synthetic-article.html"
    ));
    const ERS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/ers/synthetic-article.html"
    ));
    const ACOG: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acog/synthetic-article.html"
    ));
    const ACOG_CATALOG: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acog/synthetic-catalog.html"
    ));
    const ACOG_GUIDELINE_LIST: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/acog/synthetic-guideline-list.html"
    ));
    const ACOG_CPG_CACHED: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/acog/catalog-cpg-cached.html"));
    const ACOG_SITEMAP_FX: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/acog/sitemap.xml"));
    const ACOG_CERCLAGE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/acog/cerclage-2026.html"));
    const CERCLAGE_URL: &str = "https://www.acog.org/clinical/guidance/clinical-practice-guideline/2026/09/obstetric-cerclage/";
    #[test]
    fn acog_cached_listing_is_client_rendered_and_sitemap_enumerates_both_branches() {
        // Diagnosis pinned on the cached 2026-11-17 page: the listing has no item links, only a JS mount.
        let base = Url::parse("https://www.acog.org/clinical/guidance/clinical-practice-guideline/").unwrap();
        assert!(ACOG_CPG_CACHED.contains("data-component=\"DynamicContentListing\""));
        // Only navigation/catalog links, never an item: that is why the first-pass probe discovered 0.
        assert!(clinical_links(PublisherSource::Acog, ACOG_CPG_CACHED, &base).map(|(items, _)| items.is_empty()).unwrap_or(true));
        let items = acog_sitemap_items(ACOG_SITEMAP_FX).unwrap();
        assert_eq!(items.iter().filter(|u| u.contains("/clinical-practice-guideline/")).count(), 14);
        assert_eq!(items.iter().filter(|u| u.contains("/clinical-consensus/")).count(), 12);
        assert!(items.iter().all(|u| !u.contains("committee-opinion") && !u.contains("practice-bulletin")));
        assert!(items[0].contains("/2026/11/"), "newest first: {}", items[0]);
        assert!(items.iter().any(|u| u == CERCLAGE_URL));
        assert!(acog_sitemap_items("<urlset></urlset>").is_err());
    }
    #[tokio::test]
    async fn acog_summary_page_is_labelled_and_keeps_journal_link_and_rights() {
        let p = acog_page(ACOG_CERCLAGE, &Url::parse(CERCLAGE_URL).unwrap()).unwrap();
        assert_eq!(p.title, "Obstetric Cerclage");
        assert_eq!(p.eyebrow, "Clinical Practice Guideline");
        assert_eq!(p.access.as_deref(), Some("Full Member Benefit"));
        assert_eq!(p.meta, vec!["NUMBER 12", "SEPTEMBER 2026"]);
        assert_eq!(p.journal_url.as_deref().and_then(cw::doi_from_url).as_deref(), Some("10.1097/aog.0000000000006379"));
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(ACOG_SITEMAP, ACOG_SITEMAP_FX.as_bytes(), "text/xml");
        f.seed_cache_for_tests(CERCLAGE_URL, ACOG_CERCLAGE.as_bytes(), "text/html");
        let src: SourceEntry = serde_json::from_value(json!({"id":"acog","enabled":true,"access":"mixed","catalog_url":"https://www.acog.org/clinical/"})).unwrap();
        let rows = PublisherAdapter(PublisherSource::Acog).discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].extra["discovery_error"].is_string(), "unseeded newest page is an explicit error");
        assert_eq!(rows[1].url, CERCLAGE_URL);
        let d = PublisherAdapter(PublisherSource::Acog).extract(&src, &f, &rows[1]).await.unwrap();
        assert_eq!(d.item_type.as_deref(), Some("clinical_practice_guideline_summary_page"));
        let heads: Vec<&str> = d.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Heading { .. }) && !b.text.trim().is_empty()).map(|b| b.text.as_str()).collect();
        assert_eq!(heads, vec!["Purpose:", "Target Population:", "Methods:", "Recommendations:"]);
        assert!(d.blocks.iter().any(|b| b.text.starts_with("Number 12 (Replaces Practice Bulletin Number 142")));
        assert_eq!(d.rights.status, "restricted");
        assert!(d.rights.restrictions.iter().any(|r| r.contains("ai-train=no")));
        assert!(d.relations.iter().any(|r| r.target.contains("10.1097/AOG.0000000000006379")));
        assert!(d.date_observations.iter().any(|o| o.raw == "SEPTEMBER 2026" && o.normalized.as_deref() == Some("2026-09")));
        assert!(d.published.is_none(), "banner date is not labelled as publication date");
        assert!(d.warnings.iter().any(|w| w.contains("summary only")));
    }
    #[test]
    fn strict_roots_keep_tables_and_figures_and_reject_summary() {
        for (k, html, url) in [
            (
                PublisherSource::Aap,
                AAP,
                "https://publications.aap.org/pediatrics/article/1/a",
            ),
            (
                PublisherSource::Asco,
                ASCO,
                "https://ascopubs.org/doi/10.1200/a",
            ),
            (
                PublisherSource::Ers,
                ERS,
                "https://publications.ersnet.org/content/erj/1/a",
            ),
            (
                PublisherSource::Acog,
                ACOG,
                "https://journals.lww.com/10.1097/AOG.1",
            ),
        ] {
            let u = Url::parse(url).unwrap();
            let (_, body, _, doi) = parse_article(k, html, &u).unwrap();
            assert_eq!(doi.as_deref(), Some("10.9999/synthetic"));
            let (blocks, _) = blocks_from_html(&body, Some(&u), None);
            assert!(blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Table { .. })));
            assert!(blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Figure { .. })));
            assert!(
                parse_article(k, "<main><h1>Summary</h1><p>Abstract only</p></main>", &u).is_err()
            );
            assert!(parse_article(k, "<main>Performing security verification</main>", &u).is_err());
        }
    }
    #[test]
    fn catalog_rejects_shell_and_selects_only_recognized_routes() {
        let u = Url::parse("https://ascopubs.org/guidelines").unwrap();
        let (rows,groups)=clinical_links(PublisherSource::Asco,"<main><a href='/topics/asco-guidelines/thoracic-cancer'>Thoracic</a><a href='/doi/10.1200/X'>Guideline</a><a href='https://example.com/doi/10.1200/Y'>Unrelated</a></main>",&u).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(groups.len(), 1);
        assert!(clinical_links(PublisherSource::Asco, "<main>Loading</main>", &u).is_err());
    }
    #[test]
    fn ers_duplicate_containers_and_acog_catalog_priority() {
        let ers = Url::parse("https://channel.ersnet.org/channel-25-guidelines/").unwrap();
        let html="<div id='zone-contenu'></div><div id='zone-contenu'><a class='guideline__container-redirect__link' href='/container-iframe-redirect.php?contId=68&amp;vdoId=112'>Synthetic document</a></div>";
        assert_eq!(
            clinical_links(PublisherSource::Ers, html, &ers)
                .unwrap()
                .0
                .len(),
            1
        );
        let acog = Url::parse("https://www.acog.org/clinical/").unwrap();
        let html="<main><a href='/clinical/guidance/committee-opinion/2019/04/example/'>Opinion</a><a href='/clinical/guidance/clinical-consensus/'>Consensus</a><a href='/clinical/guidance/clinical-practice-guideline/'>Guidelines</a></main>";
        let (items, cats) = clinical_links(PublisherSource::Acog, html, &acog).unwrap();
        assert!(items.is_empty());
        assert!(cats[0].contains("clinical-practice-guideline"));
        assert!(cats[1].contains("clinical-consensus"));
        // A featured opinion alone is not a reconciled guideline catalog and must not become
        // a fallback terminal merely because its route is a recognized article route.
        assert!(clinical_links(PublisherSource::Acog,"<main><a href='/clinical/guidance/committee-opinion/2019/04/synthetic-featured-opinion/'>Featured Committee Opinion</a></main>",&acog).is_err());
    }
    #[test]
    fn acog_dynamic_guideline_branch_cannot_fall_back_to_featured_opinions() {
        let featured="<main><a href='/clinical/guidance/committee-opinion/2019/04/example'>Featured Committee Opinion</a><a href='/clinical/guidance/committee-opinion/'>Committee Opinions</a><a href='https://journals.lww.com/10.1097/AOG.123'>Committee Opinion No. 123</a></main>";
        for path in ["clinical-practice-guideline", "clinical-consensus"] {
            let branch =
                Url::parse(&format!("https://www.acog.org/clinical/guidance/{path}/")).unwrap();
            assert!(clinical_links(PublisherSource::Acog, featured, &branch).is_err());
        }
        assert!(!PublisherSource::Acog.is_catalog(
            &Url::parse("https://www.acog.org/clinical/guidance/committee-opinion/").unwrap()
        ));
    }
    #[tokio::test]
    async fn limit_stops_after_first_invalid_terminal() {
        let catalog = "https://ascopubs.org/guidelines";
        let first = "https://ascopubs.org/doi/10.1200/first";
        let second = "https://ascopubs.org/doi/10.1200/second";
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(
            catalog,
            format!("<main><a href='{first}'>First</a><a href='{second}'>Second</a></main>")
                .as_bytes(),
            "text/html",
        );
        f.seed_cache_for_tests(
            first,
            b"<article><h1>Summary only</h1></article>",
            "text/html",
        );
        // Second is deliberately absent from offline cache: reaching it would create an extra failure row.
        let src: SourceEntry = serde_json::from_value(
            json!({"id":"asco","enabled":true,"access":"public","catalog_url":catalog}),
        )
        .unwrap();
        let rows = PublisherAdapter(PublisherSource::Asco)
            .discover(&src, &f, Some(1))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url, first);
        assert!(rows[0].extra["discovery_error"]
            .as_str()
            .unwrap()
            .contains("terminal template"));
    }
    #[tokio::test]
    async fn offline_discover_and_extract_keeps_unknown_rights() {
        for (kind, html, catalog, terminal) in [
            (
                PublisherSource::Aap,
                AAP,
                "https://publications.aap.org/pediatrics/pages/policy",
                "https://publications.aap.org/pediatrics/article/1/a",
            ),
            (
                PublisherSource::Asco,
                ASCO,
                "https://ascopubs.org/guidelines",
                "https://ascopubs.org/doi/10.1200/test",
            ),
            (
                PublisherSource::Ers,
                ERS,
                "https://channel.ersnet.org/channel-25-guidelines/",
                "https://publications.ersnet.org/content/erj/1/test",
            ),
            // ACOG is no longer enumerated from its (client-rendered) listing pages; its sitemap route is
            // covered by acog_summary_page_is_labelled_and_keeps_journal_link_and_rights.
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let f = Fetcher::new(tmp.path(), true, 500, 1 << 20, "").unwrap();
            let cat = if matches!(kind, PublisherSource::Ers) {
                format!("<div id='zone-contenu'><a class='guideline__container-redirect__link' href='{terminal}'>Synthetic guideline</a></div>")
            } else if matches!(kind, PublisherSource::Acog) {
                f.seed_cache_for_tests(
                    "https://www.acog.org/clinical/guidance/clinical-practice-guideline/",
                    ACOG_GUIDELINE_LIST.as_bytes(),
                    "text/html",
                );
                // The featured committee-opinion route is deliberately absent from the offline cache.
                // Following it would add an unresolved row and fail the exact successful-row assertion.
                ACOG_CATALOG.to_owned()
            } else {
                format!("<main><a href='{terminal}'>Synthetic guideline</a></main>")
            };
            f.seed_cache_for_tests(catalog, cat.as_bytes(), "text/html");
            f.seed_cache_for_tests(terminal, html.as_bytes(), "text/html");
            let src: SourceEntry = serde_json::from_value(
                json!({"id":kind.name(),"enabled":true,"access":"public","catalog_url":catalog}),
            )
            .unwrap();
            let adapter = PublisherAdapter(kind);
            let rows = adapter.discover(&src, &f, None).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].url, terminal);
            assert!(rows[0].extra.get("discovery_error").is_none());
            let d = adapter.extract(&src, &f, &rows[0]).await.unwrap();
            assert_eq!(d.rights.status, "unknown");
            assert!(d.warnings.iter().any(|x| x.contains("Catalog enumeration")));
            assert!(d
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Recommendation { .. })));
        }
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod ers_source_contract_tests {
    use super::*;
    const CONTRACT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/ers/question-sections-contract-synthetic.html"
    ));
    fn parsed(
        raw: &str,
    ) -> (
        Vec<crate::guidelines::doc::Block>,
        Vec<String>,
        serde_json::Value,
    ) {
        let base = Url::parse(ERS_2026).unwrap();
        let (_, body, _, _) = parse_article(PublisherSource::Ers, raw, &base).unwrap();
        let (mut blocks, mut warnings) = blocks_from_html(&body, Some(&base), None);
        let metadata =
            ers_detailed_recommendations(&body, &base, &mut blocks, &mut warnings).unwrap();
        (blocks, warnings, metadata)
    }
    #[test]
    fn source_delimited_question_sections_preserve_thirteen_statements_without_summary_duplicates()
    {
        let (blocks, _, meta) = parsed(CONTRACT);
        let recs = blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recs.len(), 13);
        assert_eq!(meta["source_recommendation_sections"], 12);
        assert_eq!(meta["source_remark_paragraphs"], 1);
        assert_eq!(meta["source_recommendations"].as_array().unwrap().len(), 13);
        assert!(recs[0].text.contains("A second sentence remains"));
        assert!(
            matches!(&recs[0].kind,BlockKind::Recommendation{label:None,strength:Some(s),certainty:Some(c),..} if s=="strong recommendation"&&c=="low certainty of evidence")
        );
        let narrative = recs
            .iter()
            .filter(|b| {
                b.heading_path
                    .iter()
                    .any(|h| h.starts_with("Narrative question 1:"))
            })
            .collect::<Vec<_>>();
        assert_eq!(narrative.len(), 2);
        assert!(
            matches!(&narrative[1].kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..} if s=="conditional recommendation"&&c=="very low certainty of evidence")
        );
        assert!(blocks.iter().any(|b| b.block_id == "remark3"
            && matches!(b.kind, BlockKind::Paragraph)
            && b.text.contains("Invented scope remark")));
        assert_eq!(
            blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
                .count(),
            2
        );
        assert!(recs.iter().all(|b| !b.text.contains("Invented summary")
            && !b.text.contains("Invented priority")
            && !b.text.contains("Invented justification")));
        assert_eq!(meta["source_table_roles"].as_array().unwrap().len(), 2);
        assert_eq!(
            meta["source_table_roles"][0]["role"],
            "recommendation_summary_table"
        );
        assert_eq!(
            meta["source_table_roles"][1]["role"],
            "research_priorities_and_rationale"
        );
    }
    #[test]
    fn mixed_grade_scopes_remain_explicit_and_changed_source_boundaries_fail() {
        let changed=CONTRACT.replacen("A second sentence remains within this source paragraph (strong recommendation; low certainty of evidence).","A second sentence remains within this source paragraph (strong recommendation; low certainty of evidence), and another clause (conditional recommendation; very low certainty of evidence).",1);
        let (blocks, warnings, meta) = parsed(&changed);
        let first = blocks.iter().find(|b| b.block_id == "statement1").unwrap();
        assert!(matches!(
            &first.kind,
            BlockKind::Recommendation {
                strength: None,
                certainty: None,
                ..
            }
        ));
        assert_eq!(
            meta["source_recommendations"][0]["native_strength_spans"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            meta["source_recommendations"][0]["native_certainty_spans"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(warnings.iter().any(|w| w.contains("multiple native grade")));
        for changed in [
            CONTRACT.replacen("<h4 class=\"section-title\" id=\"rh1\">Recommendation</h4>","<h4>Changed heading</h4>",1),
            CONTRACT.replacen("A second sentence remains within this source paragraph (strong recommendation; low certainty of evidence).", "A second sentence remains within this source paragraph.", 1),
        ] {
            let base = Url::parse(ERS_2026).unwrap();
            let (_, body, _, _) = parse_article(PublisherSource::Ers, &changed, &base).unwrap();
            let (mut b, mut w) = blocks_from_html(&body, Some(&base), None);
            assert!(ers_detailed_recommendations(&body, &base, &mut b, &mut w).is_err());
        }
    }
    #[tokio::test]
    async fn explicit_observed_leaf_is_independent_and_never_a_retry_after_a_denied_route() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(ERS_2026, CONTRACT.as_bytes(), "text/html");
        let mut src:SourceEntry=serde_json::from_value(json!({"id":"ers","enabled":true,"access":"mixed","catalog_url":PublisherSource::Ers.root(),"config":{"source_linked_documents":[{"url":ERS_2026,"source_evidence_url":PublisherSource::Ers.root(),"source_catalog_link":ERS_2026_CATALOG_LINK}]}})).unwrap();
        let adapter = PublisherAdapter(PublisherSource::Ers);
        // Society redirect/catalog are deliberately absent from cache. This explicit independently
        // observed route is selected before any request; it is not a response to a denial.
        let rows = adapter.discover(&src, &f, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url, ERS_2026);
        assert_eq!(rows[0].extra["catalog_runtime_enumerated"], false);
        let all_rows = adapter.discover(&src, &f, None).await.unwrap();
        assert_eq!(all_rows.len(), 2);
        assert!(all_rows[1].extra["discovery_error"]
            .as_str()
            .unwrap()
            .contains("catalog fetch failed"));
        src.config["source_linked_only"] = json!(true);
        assert_eq!(adapter.discover(&src, &f, None).await.unwrap().len(), 1);
        src.config["source_linked_only"] = json!(false);
        let doc = adapter.extract(&src, &f, &rows[0]).await.unwrap();
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            13
        );
        assert_eq!(doc.rights.status, "unknown");
        assert_eq!(doc.date_observations[0].raw, "2026-08-06");
        assert_eq!(doc.attachments.len(), 4);
        assert!(doc.attachments.iter().any(|a| a["role"] == "slide_kit"));
        assert!(doc
            .attachments
            .iter()
            .any(|a| a["role"] == "pocket_guideline"));
        src.config["source_linked_documents"][0]["source_evidence_url"] =
            json!("https://example.org/");
        assert!(adapter.discover(&src, &f, Some(1)).await.is_err());
        src.config = json!({});
        let unresolved = adapter.discover(&src, &f, Some(1)).await.unwrap();
        assert_eq!(unresolved.len(), 1);
        assert!(unresolved[0].extra["discovery_error"]
            .as_str()
            .unwrap()
            .contains("catalog fetch failed"));
        src.config = json!({"source_linked_documents":[{"url":ERS_2026,"source_evidence_url":PublisherSource::Ers.root(),"source_catalog_link":ERS_2026_CATALOG_LINK}]});
        let duplicate_catalog=format!("<div id='zone-contenu'><a class='guideline__container-redirect__link' href='{ERS_2026_CATALOG_LINK}'>Same source redirect</a><a class='guideline__container-redirect__link' href='{ERS_2026}'>Same canonical publisher work</a></div>");
        f.seed_cache_for_tests(
            PublisherSource::Ers.root(),
            duplicate_catalog.as_bytes(),
            "text/html",
        );
        assert_eq!(adapter.discover(&src, &f, None).await.unwrap().len(), 1);
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod aap_source_state_kas_tests {
    use super::*;
    const UTI_2011: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/aap/uti-2011-retirement-contract-synthetic.html"
    ));
    const UTI_2016: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/aap/uti-2016-retirement-contract-synthetic.html"
    ));
    const OPIOID: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/aap/opioid-kas-contract-synthetic.html"
    ));
    const OLD:&str="https://publications.aap.org/pediatrics/article/128/3/595/30724/Urinary-Tract-Infection-Clinical-Practice";
    const REAFFIRM:&str="https://publications.aap.org/pediatrics/article/138/6/e20163026/52686/Reaffirmation-of-AAP-Clinical-Practice-Guideline";
    const OPIOID_URL:&str="https://publications.aap.org/pediatrics/article/154/5/e2024068752/199482/Opioid-Prescribing-for-Acute-Pain-Management-in";
    async fn plan_and_extract(
        raw: &str,
        url: &str,
    ) -> (
        SourceEntry,
        Fetcher,
        PlanRow,
        GuidelineDoc,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let f = Fetcher::new(directory.path(), true, 500, 1 << 20, "").unwrap();
        let cat = "https://publications.aap.org/pediatrics/pages/policy";
        let collection =
            "https://publications.aap.org/pediatrics/collection/523/Clinical-Practice-Guidelines";
        f.seed_cache_for_tests(
            cat,
            format!("<main><a href='{collection}'>Clinical Practice Guidelines</a></main>")
                .as_bytes(),
            "text/html",
        );
        f.seed_cache_for_tests(collection,format!("<main><a href='{url}'>Synthetic article still included in current collection</a></main>").as_bytes(),"text/html");
        f.seed_cache_for_tests(url, raw.as_bytes(), "text/html");
        let src: SourceEntry = serde_json::from_value(
            json!({"id":"aap","enabled":true,"access":"public","catalog_url":cat}),
        )
        .unwrap();
        let adapter = PublisherAdapter(PublisherSource::Aap);
        let rows = adapter.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].extra["discovery_error"].is_null());
        let row = rows[0].clone();
        let doc = adapter.extract(&src, &f, &row).await.unwrap();
        (src, f, row, doc, directory)
    }
    #[tokio::test]
    async fn exact_retirement_notice_survives_collection_inclusion_article_and_rows_for_both_events(
    ) {
        let mut identities = vec![];
        for (raw, url, notice_id, event) in [
            (UTI_2011, OLD, "12563046", false),
            (UTI_2016, REAFFIRM, "12996615", true),
        ] {
            let (_, _, row, doc, _directory) = plan_and_extract(raw, url).await;
            assert_eq!(doc.status, "retired");
            assert_eq!(row.extra["aap_source_state"]["source_state"], "retired");
            assert_eq!(
                doc.search_metadata["source_state"]["state_evidence"][0]["source_id"],
                notice_id
            );
            assert_eq!(
                doc.search_metadata["source_state"]["state_evidence"][0]["raw"],
                "This Clinical Practice Guideline was retired May 2021."
            );
            let retired = doc
                .date_observations
                .iter()
                .find(|d| d.label == "retired")
                .unwrap();
            assert_eq!(retired.raw, "May 2021");
            assert_eq!(retired.normalized.as_deref(), Some("2021-05"));
            assert_eq!(retired.source_url.as_deref(), Some(url));
            assert_eq!(
                doc.item_type.as_deref(),
                Some(if event {
                    "guideline_reaffirmation"
                } else {
                    "clinical_practice_guideline"
                })
            );
            assert!(doc.relations.is_empty());
            let rows = crate::guidelines::doc::to_rows(
                &doc,
                &Default::default(),
                false,
                "synthetic/document.json",
                &Default::default(),
            );
            assert!(rows.len() >= 2);
            assert!(rows
                .iter()
                .all(|r| r.get("status").and_then(serde_json::Value::as_str) == Some("retired")));
            assert!(rows
                .iter()
                .any(
                    |r| r.get("kind").and_then(serde_json::Value::as_str) == Some("article")
                        && r["date_observations"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|d| d["label"] == "retired" && d["normalized"] == "2021-05")
                ));
            identities.push((doc.doc_key, doc.work_key.unwrap()));
        }
        assert_ne!(identities[0], identities[1]);
        let unknown = aap_source_state(
            &UTI_2011.replace(
                "This Clinical Practice Guideline was retired May 2021.",
                "Synthetic source has no printed currentness statement.",
            ),
            &Url::parse(OLD).unwrap(),
        )
        .unwrap();
        assert_eq!(unknown["source_state"], "unknown");
    }
    #[tokio::test]
    async fn metadata_only_source_state_snapshot_change_requires_replanning() {
        let (src, f, row, _, _directory) = plan_and_extract(UTI_2011, OLD).await;
        let changed = UTI_2011.replace("content=\"2011-09-01\"", "content=\"2011-09-02\"");
        let base = Url::parse(OLD).unwrap();
        let (_, old_body, _, _) = parse_article(PublisherSource::Aap, UTI_2011, &base).unwrap();
        let (_, new_body, _, _) = parse_article(PublisherSource::Aap, &changed, &base).unwrap();
        assert_eq!(old_body, new_body);
        assert_ne!(
            publisher_snapshot(PublisherSource::Aap, UTI_2011, &old_body, &base).unwrap(),
            publisher_snapshot(PublisherSource::Aap, &changed, &new_body, &base).unwrap()
        );
        f.seed_cache_for_tests(OLD, changed.as_bytes(), "text/html");
        assert!(PublisherAdapter(PublisherSource::Aap)
            .extract(&src, &f, &row)
            .await
            .unwrap_err()
            .to_string()
            .contains("re-plan"));
        let state_changed = UTI_2011.replace("retired May 2021.", "retired June 2021.");
        let state = aap_source_state(&state_changed, &base).unwrap();
        assert_eq!(
            state["retirement_date_observations"][0]["normalized"],
            "2021-06"
        );
    }
    #[tokio::test]
    async fn native_kas_summary_rows_keep_quality_strength_and_profile_occurrence_without_modal_duplicates(
    ) {
        let (_, _, _, doc, _directory) = plan_and_extract(OPIOID, OPIOID_URL).await;
        assert_eq!(doc.status, "unknown");
        let recs = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recs.len(), 12);
        let labels = recs
            .iter()
            .map(|b| match &b.kind {
                BlockKind::Recommendation {
                    label,
                    certainty,
                    grading_system,
                    ..
                } => {
                    assert!(certainty.is_none() && grading_system.is_none());
                    label.as_deref().unwrap()
                }
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "KAS 1:", "KAS 2:", "KAS 3:", "KAS 4.1:", "KAS 4.2:", "KAS 4.3:", "KAS 4.4:",
                "KAS 5:", "KAS 6:", "KAS 7:", "KAS 8:", "KAS 9:"
            ]
        );
        assert!(
            matches!(&recs[11].kind,BlockKind::Recommendation{strength:Some(s),..}if s=="Option")
        );
        assert!(recs[0].text.starts_with("KAS 1: Invented statement 1"));
        let metadata = &doc.search_metadata["kas_summary"];
        assert_eq!(
            metadata["source_recommendations"][0]["native_grading"]["evidence_quality"]["value"],
            "B"
        );
        assert_eq!(
            metadata["source_recommendations"][0]["native_grading"]["recommendation_strength"]
                ["value"],
            "Strong Recommendation"
        );
        assert_eq!(
            metadata["source_detailed_profiles"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            metadata["source_detailed_profiles"][0]["source_label"],
            "KAS 1:"
        );
        assert!(metadata["source_legend_html"]
            .as_str()
            .unwrap()
            .contains("colspan=\"6\""));
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
                .count(),
            2
        );
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Paragraph)
                && b.text.contains("Invented detailed discussion")));
        assert!(!doc.blocks.iter().any(|b| b.text.contains("KAS 99")));
        let rows = crate::guidelines::doc::to_rows(
            &doc,
            &Default::default(),
            false,
            "synthetic/document.json",
            &Default::default(),
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.get("block_type").and_then(serde_json::Value::as_str)
                    == Some("recommendation"))
                .count(),
            12
        );
        assert!(rows
            .iter()
            .filter_map(|r| r.get("recommendation"))
            .all(|r| r["certainty"].is_null()));
    }
    #[test]
    fn changed_native_summary_geometry_or_label_scope_is_rejected() {
        let base = Url::parse(OPIOID_URL).unwrap();
        for bad in [
            OPIOID.replace("colspan=\"6\"", "colspan=\"3\""),
            OPIOID.replace("KAS 4.4:", "KAS 4.5:"),
            OPIOID.replacen("<td>B</td>", "<td></td>", 1),
        ] {
            let (_, body, _, _) = parse_article(PublisherSource::Aap, &bad, &base).unwrap();
            let (mut blocks, _) = blocks_from_html(&body, Some(&base), None);
            assert!(aap_kas_summary(&body, &mut blocks).is_err());
        }
    }
    const FALTERING_2026: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/aap/faltering-2026-contract-synthetic.html"
    ));
    const UTI_2026: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/aap/uti-2026-contract-synthetic.html"
    ));
    const FALTERING_URL:&str="https://publications.aap.org/pediatrics/article/157/4/e2025075764/206901/Clinical-Practice-Guideline-for-Diagnosis-and";
    const UTI_2026_URL:&str="https://publications.aap.org/pediatrics/article/158/4/e2026078565/209460/Clinical-Practice-Guideline-for-the-Diagnosis-and";
    #[tokio::test]
    async fn aap_2026_shared_cells_footnote_identity_and_scoped_certainty_do_not_duplicate_statements(
    ) {
        let (_, _, _, doc, _directory) = plan_and_extract(UTI_2026, UTI_2026_URL).await;
        let metadata = &doc.search_metadata["kas_summary"];
        assert_eq!(metadata["coverage"]["actual_summary_KAS"], 8);
        assert_eq!(metadata["coverage"]["actual_detailed_GPS"], 4);
        assert_eq!(
            metadata["coverage"]["all_known_statement_owners_resolved"],
            true
        );
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|block| matches!(block.kind, BlockKind::Recommendation { .. }))
                .count(),
            12
        );
        let records = metadata["source_recommendations"].as_array().unwrap();
        let kas = records
            .iter()
            .filter(|record| record["source_role"] == "key_action_statement")
            .collect::<Vec<_>>();
        assert_eq!(
            kas.iter()
                .map(|record| record["native_kas_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["1", "2", "3A", "3B", "4", "5A", "5B", "6"]
        );
        assert_eq!(kas[7]["source_label"], "Key Action Statement 6:");
        assert_eq!(kas[7]["footnote_references"][0]["href"], "#utab1fn2");
        assert!(kas[7]["source_label_html"]
            .as_str()
            .unwrap()
            .contains("<sup>b</sup>"));
        for native in ["4", "5A", "5B"] {
            let record = kas
                .iter()
                .find(|record| record["native_kas_id"] == native)
                .unwrap();
            assert_eq!(record["certainty_occurrences"].as_array().unwrap().len(), 2);
            let block = doc
                .blocks
                .iter()
                .find(|block| block.block_id == record["block_id"])
                .unwrap();
            assert!(
                matches!(&block.kind,BlockKind::Recommendation{strength:Some(s),certainty:None,..}if s=="conditional recommendation")
            );
            assert!(block.text.contains("overall effect"));
        }
        assert!(kas[5]["certainty_occurrences"][0]["raw"]
            .as_str()
            .unwrap()
            .starts_with("high"));
        assert!(kas[5]["certainty_occurrences"][1]["raw"]
            .as_str()
            .unwrap()
            .starts_with("low"));
        assert_eq!(
            metadata["source_summary_good_practice_occurrences"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            metadata["source_summary_good_practice_occurrences"][3]["source_label"],
            "Good Practice Statement for BBD:"
        );
        let gps = records
            .iter()
            .filter(|record| record["source_role"] == "good_practice_statement")
            .collect::<Vec<_>>();
        assert_eq!(gps.len(), 4);
        assert!(gps.iter().all(|record| record["native_number"].is_null()));
        assert!(gps
            .iter()
            .all(|record| doc
                .blocks
                .iter()
                .any(|block| block.block_id == record["block_id"]
                    && matches!(
                        &block.kind,
                        BlockKind::Recommendation {
                            strength: None,
                            certainty: None,
                            ..
                        }
                    ))));
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|block| matches!(block.kind, BlockKind::Table { .. }))
                .count(),
            2
        );
        assert!(!doc.blocks.iter().any(|block| block.text.contains("KAS 99")
            || block.text.contains("hidden threshold duplicate")));
        assert!(doc
            .blocks
            .iter()
            .any(|block| matches!(block.kind, BlockKind::Paragraph)
                && block
                    .text
                    .contains("Invented repeated detailed statement context")));
        assert!(doc
            .blocks
            .iter()
            .any(|block| block.text.contains("panel dissent footnote B")));
        let contexts = metadata["source_summary_context"].as_array().unwrap();
        assert_eq!(contexts.len(), 2);
        assert!(contexts
            .iter()
            .any(|context| context["parent_context_block_id"]
                .as_str()
                .is_some_and(|id| contexts
                    .iter()
                    .any(|parent| parent["context_block_id"] == id))));
        assert!(doc
            .blocks
            .iter()
            .any(|block| matches!(block.kind, BlockKind::ListItem)
                && block.text == "Invented nested remark."));
        assert_eq!(doc.rights.status, "restricted");
        assert!(doc.rights.license.is_none());
        assert!(doc.rights.restrictions[0].contains("not Open Access"));
        assert_eq!(doc.status, "unknown");
    }
    #[tokio::test]
    async fn aap_2026_rowspans_preserve_logical_columns_td_headers_and_distinct_gps_wording() {
        let (_, _, _, doc, _directory) = plan_and_extract(FALTERING_2026, FALTERING_URL).await;
        let metadata = &doc.search_metadata["kas_summary"];
        let records = metadata["source_recommendations"].as_array().unwrap();
        let split = records
            .iter()
            .find(|record| record["native_kas_id"] == "2B")
            .unwrap();
        assert_eq!(split["source_column"], 1);
        let tables = metadata["source_tables"].as_array().unwrap();
        let physical = tables[0]["geometry"]["physical_cells"].as_array().unwrap();
        let shifted = physical
            .iter()
            .find(|cell| cell["native_headers_attribute"] == "ID0EW")
            .unwrap();
        assert_eq!(shifted["origin_column"], 1);
        assert_eq!(
            shifted["source_text"].as_str().unwrap().contains("KAS 2B"),
            true
        );
        let diagnostic = &tables[1]["geometry"];
        assert_eq!(diagnostic["width"], 4);
        let rows = diagnostic["expanded_text_grid"].as_array().unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|row| row[0] == "Invented age group"));
        assert_eq!(rows[0][1], rows[0][2]);
        assert_eq!(rows[0][2], rows[0][3]);
        assert_eq!(diagnostic["physical_cells"][0]["tag"], "td");
        assert!(
            metadata["source_summary_good_practice_occurrences"][0]["source_text"]
                .as_str()
                .unwrap()
                .contains("social support programs")
        );
        assert!(records
            .iter()
            .any(|record| record["source_role"] == "good_practice_statement"
                && record["source_statement"]
                    .as_str()
                    .is_some_and(|text| text.contains("entities and programs"))));
        assert_eq!(metadata["coverage"]["actual_detailed_GPS"], 4);
        assert!(doc
            .blocks
            .iter()
            .filter(|block| matches!(block.kind, BlockKind::Recommendation { .. }))
            .all(|block| !block.text.contains("social support programs")));
        let rows = crate::guidelines::doc::to_rows(
            &doc,
            &Default::default(),
            false,
            "synthetic/document.json",
            &Default::default(),
        );
        assert_eq!(
            rows.iter()
                .filter(
                    |row| row.get("block_type").and_then(serde_json::Value::as_str)
                        == Some("recommendation")
                )
                .count(),
            12
        );
        assert_eq!(
            rows.iter().find(|row| row["kind"] == "article").unwrap()["search_metadata"],
            doc.search_metadata
        );
    }
    #[test]
    fn aap_2026_unknown_labels_changed_geometry_and_unresolved_gps_fail_honestly() {
        let base = Url::parse(UTI_2026_URL).unwrap();
        let spec = aap_2026("10.1542/peds.2026-078565").unwrap();
        for raw in [
            UTI_2026.replacen("#utab1fn2\"><sup>b", "#unverified-note\"><sup>b", 1),
            UTI_2026.replacen("Key Action Statement 3B", "Key Action Statement 3C", 1),
            UTI_2026.replacen(
                "Remarks and Good Practice Statements",
                "Unverified column",
                1,
            ),
            UTI_2026.replacen("rowspan=\"5\"", "rowspan=\"99\"", 1),
        ] {
            let (_, body, _, _) = parse_article(PublisherSource::Aap, &raw, &base).unwrap();
            let (mut blocks, mut warnings) = blocks_from_html(&body, Some(&base), None);
            assert!(aap_2026_statements(&body, spec, &mut blocks, &mut warnings).is_err());
        }
        let missing = UTI_2026.replacen("id=\"18672833-content\"", "id=\"unrecognized-owner\"", 1);
        let (_, body, _, _) = parse_article(PublisherSource::Aap, &missing, &base).unwrap();
        let (mut blocks, mut warnings) = blocks_from_html(&body, Some(&base), None);
        let metadata = aap_2026_statements(&body, spec, &mut blocks, &mut warnings).unwrap();
        assert_eq!(metadata["coverage"]["actual_detailed_GPS"], 3);
        assert_eq!(
            metadata["coverage"]["all_known_statement_owners_resolved"],
            false
        );
        assert_eq!(
            metadata["coverage"]["unresolved_GPS_sections"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("good-practice coverage unresolved")));
        assert_eq!(
            blocks
                .iter()
                .filter(|block| matches!(block.kind, BlockKind::Recommendation { .. }))
                .count(),
            11
        );
        let label_only=UTI_2026.replacen("Key Action Statement 1</span>: Invented statement 1 (strong recommendation, very low certainty of evidence).","Key Action Statement 1</span>",1);
        let (_, body, _, _) = parse_article(PublisherSource::Aap, &label_only, &base).unwrap();
        let (mut blocks, mut warnings) = blocks_from_html(&body, Some(&base), None);
        assert!(aap_2026_statements(&body, spec, &mut blocks, &mut warnings).is_err());
        assert!(aap_2026(AAP_OPIOID_DOI).is_none());
        assert!(aap_2026("10.1542/peds.unobserved-template").is_none());
    }
    #[test]
    fn aap_2026_rights_notice_is_observed_and_part_of_snapshot_not_free_badge_inference() {
        let base = Url::parse(UTI_2026_URL).unwrap();
        let (_, body, _, _) = parse_article(PublisherSource::Aap, UTI_2026, &base).unwrap();
        let changed = UTI_2026.replace(
            "prior AAP permission is required",
            "different prior AAP permission is required",
        );
        let (_, same_body, _, _) = parse_article(PublisherSource::Aap, &changed, &base).unwrap();
        assert_eq!(body, same_body);
        assert_ne!(
            publisher_snapshot(PublisherSource::Aap, UTI_2026, &body, &base).unwrap(),
            publisher_snapshot(PublisherSource::Aap, &changed, &same_body, &base).unwrap()
        );
        let badge_only = "<p>Free to read</p>";
        assert!(
            aap_2026_rights(badge_only)["explicit_restricted_article_notices"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    fn aap_2026_seed_source() -> SourceEntry {
        serde_json::from_value(json!({"id":"aap_2026_source_linked","adapter":"aap","access":"public","enabled":true,"catalog_url":PublisherSource::Aap.root(),"config":{"source_linked_only":true,"source_linked_documents":[{"url":AAP_FALTERING_2026,"source_evidence_url":AAP_2026_COLLECTION},{"url":AAP_UTI_2026,"source_evidence_url":AAP_2026_COLLECTION}]}})).unwrap()
    }
    #[tokio::test]
    async fn aap_2026_exact_source_seeds_use_normal_plans_without_catalog_or_query_synthesis() {
        let directory = tempfile::tempdir().unwrap();
        let f = Fetcher::new(directory.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(AAP_FALTERING_2026, FALTERING_2026.as_bytes(), "text/html");
        f.seed_cache_for_tests(AAP_UTI_2026, UTI_2026.as_bytes(), "text/html");
        // No catalog entry is cached: source_linked_only must not visit the hub/JS listing.
        let src = aap_2026_seed_source();
        let adapter = PublisherAdapter(PublisherSource::Aap);
        let rows = adapter.discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].url, AAP_FALTERING_2026);
        assert_eq!(rows[1].url, AAP_UTI_2026);
        assert!(rows[1].url.ends_with("?searchresult=1"));
        assert_eq!(rows[0].extra["source_evidence_page"], 2);
        assert_eq!(rows[1].extra["source_evidence_page"], 1);
        assert!(rows.iter().all(|row| row.extra["discovery_error"].is_null()
            && row.extra["catalog_runtime_enumerated"] == false));
        for row in &rows {
            assert_eq!(row.extra["source_evidence_url"], AAP_2026_COLLECTION);
            let doc = adapter.extract(&src, &f, row).await.unwrap();
            assert_eq!(doc.url, row.url);
            assert_eq!(
                doc.search_metadata["kas_summary"]["coverage"]["actual_summary_KAS"],
                8
            );
        }
        let mut changed = src.clone();
        changed.config["source_linked_documents"][0]["source_evidence_url"] =
            json!("https://evil.test/");
        assert!(adapter.extract(&changed, &f, &rows[0]).await.is_err());
        let mut changed = rows[1].clone();
        changed.extra["source_evidence_page"] = json!(2);
        assert!(adapter.extract(&src, &f, &changed).await.is_err());
    }
    #[tokio::test]
    async fn aap_2026_seed_errors_consume_limits_and_unknown_routes_do_not_expand_scope() {
        let directory = tempfile::tempdir().unwrap();
        let f = Fetcher::new(directory.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(
            AAP_FALTERING_2026,
            b"<main>Unrecognized synthetic body</main>",
            "text/html",
        );
        f.seed_cache_for_tests(AAP_UTI_2026, UTI_2026.as_bytes(), "text/html");
        let src = aap_2026_seed_source();
        let adapter = PublisherAdapter(PublisherSource::Aap);
        let rows = adapter.discover(&src, &f, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url, AAP_FALTERING_2026);
        assert!(rows[0].extra["discovery_error"]
            .as_str()
            .unwrap()
            .contains("no fallback"));
        assert!(adapter.extract(&src, &f, &rows[0]).await.is_err());
        let rows = adapter.discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[1].extra["discovery_error"].is_null());
        let mut no_query = src.clone();
        no_query.config["source_linked_documents"][1]["url"] = json!(UTI_2026_URL);
        assert!(aap_seeds(&no_query).is_err());
        let mut unverified = src.clone();
        unverified.config["source_linked_documents"][0]["url"] =
            json!("https://publications.aap.org/pediatrics/article/other");
        assert!(aap_seeds(&unverified).is_err());
        let mut broad = src.clone();
        broad.config["source_linked_only"] = json!(false);
        assert!(aap_seeds(&broad).is_err());
        let mut missing = src.clone();
        missing.config = json!({"source_linked_only":true});
        assert!(aap_seeds(&missing).is_err());
        let mut default = src.clone();
        default.config = json!({});
        assert!(aap_seeds(&default).unwrap().is_none());
    }

    #[tokio::test]
    async fn aap_2026_primary_grade_asset_and_footnote_drift_require_replan_while_modal_copy_does_not(
    ) {
        let (src, f, row, _doc, _directory) = plan_and_extract(UTI_2026, UTI_2026_URL).await;
        let adapter = PublisherAdapter(PublisherSource::Aap);
        let modal = UTI_2026.replace(
            "KAS 99: Invented duplicate modal sentinel.",
            "Unrelated modal representation changed.",
        );
        f.seed_cache_for_tests(UTI_2026_URL, modal.as_bytes(), "text/html");
        adapter.extract(&src, &f, &row).await.unwrap();
        for changed in [
            UTI_2026.replace(
                "high certainty of evidence for test accuracy",
                "moderate certainty of evidence for test accuracy",
            ),
            UTI_2026.replace("/synthetic-figure.png", "/changed-primary-figure.png"),
            UTI_2026.replace("/synthetic-source.pdf", "/changed-source-asset.pdf"),
            UTI_2026.replace(
                "Invented panel dissent footnote B.",
                "Changed native panel dissent context.",
            ),
        ] {
            f.seed_cache_for_tests(UTI_2026_URL, changed.as_bytes(), "text/html");
            assert!(adapter
                .extract(&src, &f, &row)
                .await
                .unwrap_err()
                .to_string()
                .contains("re-plan"));
        }
    }
    const UTI_PRIMARY_NOTE:&str="<div class=\"table-wrap-foot\"><div class=\"fn\" id=\"utab1fn2\" content-id=\"utab1fn2\"><span class=\"label fn-label\"><span class=\"end-note-link\" data-fn-id=\"utab1fn2\">b</span></span><p>Invented panel dissent footnote B.</p></div></div>";
    const UTI_FRAGMENT_MARKER: &str = "<a href=\"#utab1fn2\"><sup>b</sup></a>";
    const UTI_REVEAL_MARKER:&str="<a class=\"link link-ref link-reveal xref-fn js-xref-fn\" href=\"javascript:;\" reveal-id=\"utab1fn2\" data-open=\"utab1fn2\"><sup>b</sup></a>";
    #[tokio::test]
    async fn aap_2026_kas6_note_exception_binds_exact_primary_owner_for_both_observed_marker_forms()
    {
        assert_eq!(UTI_2026.matches(UTI_PRIMARY_NOTE).count(), 1);
        assert_eq!(UTI_2026.matches(UTI_FRAGMENT_MARKER).count(), 1);
        for raw in [
            UTI_2026.to_owned(),
            UTI_2026.replacen(UTI_FRAGMENT_MARKER, UTI_REVEAL_MARKER, 1),
        ] {
            let (_, _, _, doc, _directory) = plan_and_extract(&raw, UTI_2026_URL).await;
            let metadata = &doc.search_metadata["kas_summary"];
            assert_eq!(
                metadata["source_label_note_identity_binding"]["verified"],
                true
            );
            assert_eq!(
                metadata["source_label_note_identity_binding"]["source_target_id"],
                "utab1fn2"
            );
            let record = metadata["source_recommendations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["native_kas_id"] == "6")
                .unwrap();
            assert_eq!(record["source_label"], "Key Action Statement 6:");
            assert!(record["source_label_html"]
                .as_str()
                .unwrap()
                .contains("<sup>b</sup>"));
            let reference = &record["footnote_references"][0];
            if raw.contains(UTI_REVEAL_MARKER) {
                assert_eq!(reference["href"], "javascript:;");
                assert_eq!(reference["reveal_id"], "utab1fn2");
                assert_eq!(reference["data_open"], "utab1fn2");
            } else {
                assert_eq!(reference["href"], "#utab1fn2");
                assert!(reference["reveal_id"].is_null() && reference["data_open"].is_null());
            }
            assert!(doc
                .blocks
                .iter()
                .any(|block| block.text.contains("Invented panel dissent footnote B.")));
            assert!(!doc.blocks.iter().any(|block| block.text.contains("KAS 99")));
            assert_eq!(metadata["coverage"]["actual_summary_KAS"], 8);
        }
    }
    #[test]
    fn aap_2026_label_notes_reject_unverified_shape_target_ownership_and_ambiguous_identity() {
        let base = Url::parse(UTI_2026_URL).unwrap();
        let spec = aap_2026("10.1542/peds.2026-078565").unwrap();
        assert_eq!(UTI_2026.matches(UTI_PRIMARY_NOTE).count(), 1);
        let without_note = UTI_2026.replacen(UTI_PRIMARY_NOTE, "", 1);
        let outside_owner = without_note.replace(
            "<div class=\"fig-section\">",
            &format!("{UTI_PRIMARY_NOTE}<div class=\"fig-section\">"),
        );
        let hidden_clone_only = without_note.replacen(
            "<div class=\"table-modal\">",
            &format!("<div class=\"table-modal\">{UTI_PRIMARY_NOTE}"),
            1,
        );
        let duplicate_note = UTI_2026.replacen(
            UTI_PRIMARY_NOTE,
            &format!("{UTI_PRIMARY_NOTE}{UTI_PRIMARY_NOTE}"),
            1,
        );
        let mixed_marker = UTI_2026.replacen(
            UTI_FRAGMENT_MARKER,
            &format!("{UTI_FRAGMENT_MARKER}<a href=\"#utab1fn2\"><sup>b</sup></a>"),
            1,
        );
        let invalids = vec![
            (
                "nonsup marker",
                UTI_2026.replacen(UTI_FRAGMENT_MARKER, "<a href=\"#utab1fn2\">b</a>", 1),
            ),
            (
                "unknown prefix-matching note",
                UTI_2026.replacen(
                    UTI_FRAGMENT_MARKER,
                    "<a href=\"#utab1fn999\"><sup>b</sup></a>",
                    1,
                ),
            ),
            ("missing native target", without_note),
            ("target outside primary table owner", outside_owner),
            ("target exists only in suppressed modal", hidden_clone_only),
            ("duplicate primary targets", duplicate_note),
            ("ambiguous marker", mixed_marker),
            (
                "wrong target marker",
                UTI_2026.replacen("data-fn-id=\"utab1fn2\">b", "data-fn-id=\"utab1fn2\">c", 1),
            ),
            (
                "wrong target identity",
                UTI_2026.replacen(
                    "id=\"utab1fn2\" content-id=\"utab1fn2\"",
                    "id=\"utab1fn2\" content-id=\"utab1fn999\"",
                    1,
                ),
            ),
            (
                "unverified target wrapper",
                UTI_2026.replacen(
                    "class=\"table-wrap-foot\"",
                    "class=\"unverified-foot-wrapper\"",
                    1,
                ),
            ),
            (
                "target clinical note absent",
                UTI_2026.replacen("<p>Invented panel dissent footnote B.</p>", "", 1),
            ),
            (
                "unverified nonb marker",
                UTI_2026.replacen(
                    UTI_FRAGMENT_MARKER,
                    "<a href=\"#utab1fn2\"><sup>c</sup></a>",
                    1,
                ),
            ),
            (
                "native alphanumeric identity is not a note",
                UTI_2026.replacen(
                    &format!("Key Action Statement 6{UTI_FRAGMENT_MARKER}:"),
                    "Key Action Statement 6B:",
                    1,
                ),
            ),
            (
                "exception not portable to another KAS",
                UTI_2026.replacen(
                    &format!("Key Action Statement 6{UTI_FRAGMENT_MARKER}:"),
                    &format!("Key Action Statement 5{UTI_FRAGMENT_MARKER}:"),
                    1,
                ),
            ),
            (
                "reveal mapping incomplete",
                UTI_2026.replacen(
                    UTI_FRAGMENT_MARKER,
                    &UTI_REVEAL_MARKER.replace(" data-open=\"utab1fn2\"", ""),
                    1,
                ),
            ),
            (
                "reveal mapping contradictory",
                UTI_2026.replacen(
                    UTI_FRAGMENT_MARKER,
                    &UTI_REVEAL_MARKER
                        .replace("data-open=\"utab1fn2\"", "data-open=\"utab1fn999\""),
                    1,
                ),
            ),
            (
                "unverified reveal marker class",
                UTI_2026.replacen(
                    UTI_FRAGMENT_MARKER,
                    &UTI_REVEAL_MARKER.replace("js-xref-fn", "unverified-xref"),
                    1,
                ),
            ),
        ];
        for (case, raw) in invalids {
            assert_ne!(raw, UTI_2026, "negative fixture must change: {case}");
            let (_, body, _, _) = parse_article(PublisherSource::Aap, &raw, &base).unwrap();
            let (mut blocks, mut warnings) = blocks_from_html(&body, Some(&base), None);
            assert!(
                aap_2026_statements(&body, spec, &mut blocks, &mut warnings).is_err(),
                "unverified label identity accepted: {case}"
            );
        }
    }
}
