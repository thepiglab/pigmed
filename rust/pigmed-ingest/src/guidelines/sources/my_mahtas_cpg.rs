//! Malaysia MaHTAS clinical practice guidelines.
//!
//! The ministry catalog (www.moh.gov.my, registry `catalog_url`) answers 403 and is not retried. MaHTAS's own site
//! lists the CPGs: its "CPG - Clinical Practice Guidelines" menu page
//! (mymahtas.moh.gov.my/index.php/docman-list/publications/cpg-clinical-practice-guideline) embeds the public search
//! page `mymahtas2.moh.gov.my/index.php/main/carian/CPG`, whose own script loads the list by POSTing the selected
//! module (`mydokumen_modul_id=CPG`) to `main/listSearchDokumen`, and each record's files by POSTing `mydokumen_id` to
//! `main/listJsonDocModal`. We make exactly those calls (no credentials; mymahtas2 robots.txt is 404 = no rules).
//! Each record row gives title, module, discipline/category, publication year and the file types it has; the file
//! modal labels each file (CLINICAL PRACTICE GUIDELINE, QUICK REFERENCE, TRAINING MODULE, ...).
//! Full CPG and quick-reference PDFs become documents (the quick reference `summary_of` its CPG); other files are
//! recorded as attachments of the CPG.
use super::{common, registries_pdf::blocks_for_pdf, Adapter, PlanRow};
use crate::guidelines::{
    doc::{Block, DateObservation, GuidelineDoc, Relation, Rights},
    fetch::{Fetched, Fetcher, Req},
    html::{sel, text_of},
    registry::SourceEntry,
};
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::json;
use url::Url;
pub struct MyMahtasCpg;
const ROOT: &str = "https://www.moh.gov.my/en/publications-and-reports/policies-act-policies-guide-lines/penerbitan-klinikal/senarai-penerbitan-klinikal/panduan-amalan-klinikal-cpg";
const MENU: &str = "https://mymahtas.moh.gov.my/index.php/docman-list/publications/cpg-clinical-practice-guideline";
const CARIAN: &str = "https://mymahtas2.moh.gov.my/index.php/main/carian/CPG";
const LIST: &str = "https://mymahtas2.moh.gov.my/index.php/main/listSearchDokumen";
const MODAL: &str = "https://mymahtas2.moh.gov.my/index.php/main/listJsonDocModal";
const HOST: &str = "mymahtas2.moh.gov.my";

/// The exact form request the search page's script sends (same field order as its jQuery options object).
fn form(url: &str, fields: &[(&str, &str)]) -> Req {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish();
    Req {
        method: "POST".into(),
        url: url.into(),
        headers: vec![("content-type".into(), "application/x-www-form-urlencoded".into())],
        body: Some(body.into_bytes()),
        ..Default::default()
    }
}
fn list_fields() -> Vec<(&'static str, &'static str)> {
    vec![("mydokumen_modul_id", "CPG"), ("mydokumen_published_date", "")]
}

/// The search page must still be the CPG module page that loads its list from `main/listSearchDokumen`.
fn check_carian(html: &str) -> Result<()> {
    let d = Html::parse_document(html);
    let selected = d
        .select(&sel("select#mymodul_id option[selected]"))
        .filter_map(|o| o.value().attr("value"))
        .find(|v| !v.is_empty());
    if selected != Some("CPG") {
        bail!("MaHTAS search page no longer preselects module CPG (found {selected:?})");
    }
    if !html.contains("index.php/main/listSearchDokumen") || !html.contains("index.php/main/listJsonDocModal") {
        bail!("MaHTAS search page script no longer calls listSearchDokumen/listJsonDocModal");
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ListRow {
    id: String,
    title: String,
    module: String,
    discipline: String,
    year: String,
    types: String,
}

fn list_rows(html: &str) -> Result<Vec<ListRow>> {
    let d = Html::parse_fragment(html);
    let head = d.select(&sel("table thead th")).map(|t| text_of(&t)).collect::<Vec<_>>();
    let expect = ["BIL", "TAJUK", "MODUL", "DISIPLIN", "TAHUN TERBITAN", "JENIS"];
    if head.len() != expect.len() || !head.iter().zip(expect).all(|(h, e)| h.to_uppercase().starts_with(e)) {
        bail!("MaHTAS list header changed: {head:?}");
    }
    let mut out = vec![];
    for tr in d.select(&sel("table tbody tr")) {
        let tds = tr.select(&sel("td")).collect::<Vec<_>>();
        if tds.len() != 6 {
            bail!("MaHTAS list row has {} cells", tds.len());
        }
        let id = tds[1]
            .select(&sel("a[attr]"))
            .next()
            .and_then(|a| a.value().attr("attr"))
            .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
            .context("MaHTAS list row has no record id")?;
        out.push(ListRow {
            id: id.into(),
            title: text_of(&tds[1]),
            module: text_of(&tds[2]),
            discipline: text_of(&tds[3]),
            year: text_of(&tds[4]),
            types: text_of(&tds[5]),
        });
    }
    if out.is_empty() {
        bail!("MaHTAS CPG list is empty");
    }
    Ok(out)
}

/// (file type label, absolute file URL, file name) from a record's file modal. Links resolve against the search page,
/// into which the page's script inserts the modal.
fn files(html: &str) -> Result<Vec<(String, String, String)>> {
    let d = Html::parse_fragment(html);
    let base = Url::parse(CARIAN)?;
    let mut out = vec![];
    for tr in d.select(&sel("table tbody tr")) {
        let tds = tr.select(&sel("td")).collect::<Vec<_>>();
        if tds.len() != 2 {
            continue;
        }
        let Some(a) = tds[1].select(&sel("a[href]")).next() else { continue };
        let Some(url) = common::link(&base, a.value().attr("href").unwrap_or_default()) else { continue };
        out.push((text_of(&tds[0]), url, text_of(&a)));
    }
    Ok(out)
}

fn role(label: &str) -> Option<&'static str> {
    match label.trim().to_uppercase().as_str() {
        "CLINICAL PRACTICE GUIDELINE" => Some("cpg"),
        "QUICK REFERENCE" => Some("qr"),
        _ => None,
    }
}

fn complete(p: &Fetched) -> bool {
    p.is_pdf() && p.body[p.body.len().saturating_sub(2048)..].windows(5).any(|w| w == b"%%EOF")
}

/// Rights and dates printed in the PDF front matter (first 40 blocks). The MaHTAS notice permits reproduction with
/// conditions; without it rights stay unknown.
fn metadata(blocks: &[Block], url: &str) -> (Rights, Vec<DateObservation>, Option<String>) {
    let front = &blocks[..blocks.len().min(40)];
    let notice = front.iter().find(|b| b.text.contains("copyright owner of this publication is MaHTAS"));
    let rights = match notice {
        Some(n) => {
            let mut restrictions = vec![];
            if n.text.contains("content is not changed") || n.text.contains("not sold") {
                restrictions.push("Content must not be changed, sold, used to promote or endorse any product or service, or used in an inappropriate or misleading context.".into());
            }
            Rights {
                status: "restricted".into(),
                rights_holders: vec!["MaHTAS".into()],
                attribution: n.text.contains("acknowledg").then(|| "Copyright acknowledgement to MaHTAS".into()),
                restrictions,
                note: Some(n.text.clone()),
                evidence_urls: vec![url.into()],
                ..Default::default()
            }
        }
        None => Rights {
            status: "unknown".into(),
            note: Some("No MaHTAS copyright notice found in the PDF front matter.".into()),
            ..Default::default()
        },
    };
    let mut dates = vec![];
    let issued = regex::Regex::new(r"guidelines? (?:were|was) issued in ((?:19|20)\d{2})").unwrap();
    if let Some(c) = front.iter().find_map(|b| issued.captures(&b.text)) {
        dates.push(common::date("Issued", &c[1], url));
    }
    let edition = regex::Regex::new(r"\(((?:First|Second|Third|Fourth|Fifth|Sixth|Seventh|\d+(?:st|nd|rd|th)) Edition)\)").unwrap();
    let version = front.iter().take(20).find_map(|b| edition.captures(&b.text)).map(|c| c[1].to_string());
    (rights, dates, version)
}

fn issue(s: &SourceEntry, u: &str, e: String) -> PlanRow {
    PlanRow {
        source_id: s.id.clone(),
        doc_key: format!("mahtas-{}-unresolved", common::short_hash(u)),
        url: u.into(),
        title: None,
        rendition: None,
        version: None,
        extra: json!({"discovery_error":e}),
    }
}

#[async_trait::async_trait]
impl Adapter for MyMahtasCpg {
    async fn discover(&self, s: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let page = f.get_ok(CARIAN).await.context("MaHTAS CPG search page")?;
        check_carian(&page.text())?;
        let list = f.request(&form(LIST, &list_fields())).await?;
        if list.record.status != 200 {
            bail!("MaHTAS CPG list: HTTP {}", list.record.status);
        }
        let rows = list_rows(&list.text())?;
        let mut out = vec![];
        let mut deferred = vec![];
        for r in rows.iter().filter(|r| r.module == "CPG") {
            let listing = json!({"record_id":r.id,"title":r.title,"module":r.module,"discipline_category":r.discipline,"year_of_publication":r.year,"file_types":r.types,"list_endpoint":LIST,"search_page":CARIAN,"menu_page":MENU});
            let modal = match f.request(&form(MODAL, &[("mydokumen_id", r.id.as_str())])).await {
                Ok(m) if m.record.status == 200 => m,
                Ok(m) => {
                    deferred.push(issue(s, &format!("{MODAL}#{}", r.id), format!("MaHTAS file list for record {}: HTTP {}", r.id, m.record.status)));
                    continue;
                }
                Err(e) => {
                    deferred.push(issue(s, &format!("{MODAL}#{}", r.id), format!("MaHTAS file list for record {}: {e:#}", r.id)));
                    continue;
                }
            };
            let fl = files(&modal.text())?;
            let cpg_key = fl
                .iter()
                .find(|(l, u, _)| role(l) == Some("cpg") && u.to_lowercase().ends_with(".pdf"))
                .map(|(_, u, _)| format!("mahtas-{}-cpg-{}", r.id, common::short_hash(u)));
            let others = fl
                .iter()
                .filter(|(l, u, _)| role(l).is_none() || !u.to_lowercase().ends_with(".pdf"))
                .map(|(l, u, n)| json!({"url":u,"label":l,"file_name":n,"role":"other_record_file","rights":"unknown"}))
                .collect::<Vec<_>>();
            let mut emitted = false;
            for (label, url, name) in &fl {
                let Some(kind) = role(label) else { continue };
                if !url.to_lowercase().ends_with(".pdf") {
                    continue;
                }
                emitted = true;
                let work_key = format!("mahtas-{}{}", r.id, if kind == "qr" { "-qr" } else { "" });
                let other_files = if kind == "cpg" { json!(others) } else { json!([]) };
                out.push(PlanRow {
                    source_id: s.id.clone(),
                    doc_key: format!("mahtas-{}-{kind}-{}", r.id, common::short_hash(url)),
                    url: url.clone(),
                    title: Some(r.title.clone()),
                    rendition: Some("pdf".into()),
                    version: None,
                    extra: json!({"work_key":work_key,"file_role":kind,"file_label":label,"file_name":name,"listing":listing,"cpg_doc_key":cpg_key,"other_files":other_files,"list_fetch":list.record,"modal_fetch":modal.record}),
                });
                if limit.is_some_and(|n| out.len() >= n) {
                    return Ok(out);
                }
            }
            if !emitted {
                deferred.push(issue(s, &format!("{MODAL}#{}", r.id), format!("MaHTAS record {} ({}) lists no CPG or quick-reference PDF: {:?}", r.id, r.title, fl)));
            }
        }
        let rest = limit.map(|n| n.saturating_sub(out.len())).unwrap_or(usize::MAX);
        out.extend(deferred.into_iter().take(rest));
        Ok(out)
    }
    async fn extract(&self, s: &SourceEntry, f: &Fetcher, r: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(e) = r.extra["discovery_error"].as_str() {
            bail!("{e}")
        };
        let p = f.get_ok(&r.url).await?;
        if !complete(&p) || Url::parse(&p.record.final_url)?.host_str() != Some(HOST) {
            bail!("MaHTAS file is incomplete, not a PDF or redirected off {HOST}");
        }
        let (blocks, mut warnings, _) = blocks_for_pdf(&p, 30).await?;
        if blocks.is_empty() {
            bail!("MaHTAS PDF yielded no text");
        }
        let (rights, mut dates, version) = metadata(&blocks, &r.url);
        let listing = &r.extra["listing"];
        if let Some(y) = listing["year_of_publication"].as_str().filter(|y| !y.is_empty()) {
            dates.push(common::date("TAHUN TERBITAN / YEAR PUBLICATIONS", y, LIST));
        }
        let kind = r.extra["file_role"].as_str().unwrap_or_default();
        let mut relations = vec![];
        if kind == "qr" {
            if let Some(k) = r.extra["cpg_doc_key"].as_str() {
                relations.push(Relation {
                    rel: "summary_of".into(),
                    target: format!("guideline:{}:{k}", s.id),
                    scope: Some("Quick reference file of the same MaHTAS record".into()),
                    evidence_url: Some(MODAL.into()),
                });
            }
        }
        if rights.status == "unknown" {
            warnings.push("No MaHTAS copyright notice in the PDF front matter; reuse terms not established.".into());
        }
        warnings.push("Listed through MaHTAS's public CPG search (mymahtas2); the ministry catalog on www.moh.gov.my returns 403 and was not used. The list's status icons are not mapped, so currentness is not inferred.".into());
        warnings.push("PDF extraction has not verified figures, clinical algorithms, visual table alignment or complete recommendations; source headings can be merged into list items, so recommendation grades and scope are not inferred.".into());
        let mut fetches = vec![];
        for k in ["list_fetch", "modal_fetch"] {
            if let Ok(x) = serde_json::from_value(r.extra[k].clone()) {
                fetches.push(x);
            }
        }
        fetches.push(p.record);
        Ok(GuidelineDoc {
            source_id: s.id.clone(),
            doc_key: r.doc_key.clone(),
            work_key: r.extra["work_key"].as_str().map(String::from),
            url: r.url.clone(),
            title: r.title.clone().unwrap_or_default(),
            item_type: Some(if kind == "qr" { "quick_reference" } else { "clinical_practice_guideline" }.into()),
            version,
            status: "unknown".into(),
            rights,
            date_observations: dates,
            relations,
            attachments: r.extra["other_files"].as_array().cloned().unwrap_or_default(),
            search_metadata: json!({"mahtas_listing":listing,"file_label":r.extra["file_label"],"file_name":r.extra["file_name"]}),
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
    const FRONT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/my_mahtas_cpg/asthma-converted-front.json"));
    const CARIAN_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/my_mahtas_cpg/carian-cpg.html"));
    const LIST_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/my_mahtas_cpg/list-cpg.html"));
    const MODAL_695: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/my_mahtas_cpg/docmodal-695.html"));
    const MODAL_694: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/my_mahtas_cpg/docmodal-694.html"));
    const MODAL_639: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/my_mahtas_cpg/docmodal-639.html"));
    const EXCERPT: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/my_mahtas_cpg/asthma-source-pages-2-3-7.pdf"));

    #[test]
    fn genuine_front_matter_preserves_notice_dates_and_edition() {
        let blocks: Vec<Block> = serde_json::from_str(FRONT).unwrap();
        let (r, d, v) = metadata(&blocks, "https://mymahtas2.moh.gov.my/files/x.pdf");
        assert_eq!(r.status, "restricted");
        assert_eq!(r.rights_holders, ["MaHTAS"]);
        assert!(r.note.unwrap().contains("not changed") && r.restrictions[0].contains("not be changed"));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].normalized.as_deref(), Some("2024"));
        assert_eq!(v.as_deref(), Some("Second Edition"));
    }

    #[test]
    fn real_list_and_file_modals_parse() {
        check_carian(CARIAN_HTML).unwrap();
        let rows = list_rows(LIST_HTML).unwrap();
        assert_eq!(rows.len(), 100);
        assert!(rows.iter().all(|r| r.module == "CPG"));
        let asthma = rows.iter().find(|r| r.id == "695").unwrap();
        assert_eq!(asthma.title, "CPG MANAGEMENT OF ASTHMA IN ADULTS");
        assert_eq!((asthma.discipline.as_str(), asthma.year.as_str(), asthma.types.as_str()), ("RESPIRATORY MEDICINE", "2024", "CPG | QR | TM"));
        let f = files(MODAL_695).unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!(role(&f[0].0), Some("cpg"));
        assert!(f[0].1.starts_with("https://mymahtas2.moh.gov.my/files/e-CPG_Management_of_Asthma_in_Adults_(Second_Edition)%20-%20update%20June%202026.pdf"), "{}", f[0].1);
        assert_eq!(role(&f[1].0), Some("qr"));
        assert_eq!(role(&f[2].0), None);
        assert_eq!(files(MODAL_639).unwrap().len(), 1);
        assert_eq!(files(MODAL_694).unwrap().len(), 3);
    }

    #[tokio::test]
    async fn offline_discovery_and_extract_from_listing() {
        let t = tempfile::tempdir().unwrap();
        let f = Fetcher::new(t.path(), true, 500, 4 << 20, "").unwrap();
        f.seed_cache_for_tests(CARIAN, CARIAN_HTML.as_bytes(), "text/html; charset=UTF-8");
        f.seed_cache_request_for_tests(&form(LIST, &list_fields()), LIST_HTML.as_bytes(), "text/html; charset=UTF-8");
        for (id, html) in [("694", MODAL_694), ("695", MODAL_695)] {
            f.seed_cache_request_for_tests(&form(MODAL, &[("mydokumen_id", id)]), html.as_bytes(), "text/html; charset=UTF-8");
        }
        let asthma_pdf = files(MODAL_695).unwrap()[0].1.clone();
        f.seed_cache_for_tests(&asthma_pdf, EXCERPT, "application/pdf");
        let s: SourceEntry = serde_json::from_value(json!({"id":"my_mahtas_cpg","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let rows = MyMahtasCpg.discover(&s, &f, Some(4)).await.unwrap();
        // record 694 (abdominal trauma): CPG + QR; the next two records have no seeded file list (deferred); asthma CPG + QR
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(rows.iter().all(|r| r.extra["discovery_error"].is_null()));
        assert_eq!(rows[0].extra["file_role"], "cpg");
        assert_eq!(rows[1].extra["file_role"], "qr");
        let a = rows.iter().find(|r| r.url == asthma_pdf).unwrap();
        assert_eq!(a.title.as_deref(), Some("CPG MANAGEMENT OF ASTHMA IN ADULTS"));
        assert_eq!(a.extra["other_files"].as_array().unwrap().len(), 1);
        let d = MyMahtasCpg.extract(&s, &f, a).await.unwrap();
        assert_eq!(d.item_type.as_deref(), Some("clinical_practice_guideline"));
        assert_eq!(d.rights.status, "restricted");
        assert!(d.date_observations.iter().any(|x| x.label.starts_with("TAHUN TERBITAN") && x.raw == "2024"));
        assert!(d.blocks.iter().any(|b| b.text.contains("before considering stepping up or down")));
        let qr = rows.iter().find(|r| r.extra["file_role"] == "qr" && r.extra["listing"]["record_id"] == "695").unwrap();
        assert_eq!(qr.extra["cpg_doc_key"].as_str(), Some(a.doc_key.as_str()));
        assert!(MyMahtasCpg.discover(&s, &f, Some(0)).await.unwrap().is_empty());
    }
}
