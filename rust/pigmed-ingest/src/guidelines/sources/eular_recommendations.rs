//! EULAR recommendation directory audits, with an explicit bounded reviewed-PDF mode.
use super::{common, pmc_crosswalk as cw, static_list::StaticList, Adapter, PlanRow};
use crate::guidelines::doc::GuidelineDoc;
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::{ElementRef, Html};
use serde_json::{json, Value};
use std::collections::HashSet;
use url::Url;

pub struct Eular;
const ROOT: &str = "https://www.eular.org/recommendations-management";

fn content_mode(doc: &Value) -> Result<bool> {
    match doc.get("identity_mode") {
        None => Ok(false),
        Some(mode) if mode == "sha256" => Ok(false),
        Some(mode) if mode == "reviewed_content" => Ok(true),
        Some(_) => bail!("EULAR identity_mode must be sha256 or reviewed_content"),
    }
}

/// Reviewed visible-content identity excludes container metadata, but includes every ordered page's exact
/// text, geometry and rendered RGB bytes. Golden fingerprints must be emitted by this same recipe.
fn content_fingerprint(bytes: &[u8]) -> Result<(Value, String)> {
    use pdfium_render::prelude::*;
    if bytes.len() > 8 << 20 { bail!("EULAR reviewed PDF exceeds fingerprint byte budget"); }
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("reviewed.pdf");
    std::fs::write(&path, bytes)?;
    let _gate = crate::media::PDFIUM_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let lib = crate::media::pdfium()?;
    let pdf = lib.load_pdf_from_file(&path, None)?;
    if pdf.pages().len() == 0 || pdf.pages().len() > 100 { bail!("EULAR fingerprint page budget exceeded"); }
    let mut pages = vec![];
    let mut decoded = String::new();
    for page in pdf.pages().iter() {
        let (w, h) = (page.width().value, page.height().value);
        if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 || w * h > 4_000_000.0 {
            bail!("EULAR fingerprint raster geometry exceeds budget");
        }
        let text = page.text()?.all();
        decoded.push_str(&text);
        let cfg = PdfRenderConfig::new().set_target_width(w.ceil() as i32)
            .set_target_height(h.ceil() as i32).render_form_data(true).render_annotations(true);
        let rgb = page.render_with_config(&cfg)?.as_image().into_rgb8();
        pages.push(json!({"size_pdf_points":[w,h], "rgb_size":[rgb.width(),rgb.height()],
            "text_sha256":crate::common::sha256_hex(text.as_bytes()),
            "rgb_sha256":crate::common::sha256_hex(rgb.as_raw())}));
    }
    Ok((json!({"recipe":"eular-pdfium-72dpi-rgb-text-v1","pages":pages}), decoded))
}

async fn verify_content(doc: &Value, bytes: &[u8]) -> Result<()> {
    let bytes = bytes.to_vec();
    let (actual, decoded) = tokio::task::spawn_blocking(move || content_fingerprint(&bytes)).await??;
    if actual != doc["reviewed_content"] { bail!("EULAR reviewed whole-document content fingerprint changed"); }
    let decoded: String = decoded.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if !decoded.contains(&doc["doi"].as_str().context("EULAR DOI missing")?.to_ascii_lowercase()) {
        bail!("EULAR reviewed PDF decoded text does not contain the configured DOI");
    }
    Ok(())
}

fn reviewed_mode(src: &SourceEntry) -> Result<bool> {
    match src.config.get("route_mode") {
        None => Ok(false),
        Some(mode) if mode == "catalog" => Ok(false),
        Some(mode) if mode == "reviewed_documents" => Ok(true),
        Some(_) => bail!("EULAR route_mode must be catalog or reviewed_documents"),
    }
}

/// Translate only the explicitly reviewed subset to StaticList's existing schema. There is no
/// publisher URL rewriting, repository search, catalog fallback, or assertion of catalog coverage.
fn reviewed_source(src: &SourceEntry) -> Result<SourceEntry> {
    let docs = src.config.get("reviewed_documents").and_then(|v| v.as_array())
        .filter(|v| !v.is_empty()).context("EULAR reviewed_documents must be a nonempty array")?;
    let mut keys = HashSet::new();
    let mut urls = HashSet::new();
    for doc in docs {
        for field in ["url", "doc_key", "work_key", "title", "rendition", "version", "doi"] {
            if doc.get(field).and_then(|v| v.as_str()).is_none_or(|s| s.trim().is_empty()) {
                bail!("EULAR reviewed document missing {field}");
            }
        }
        let sha = doc.get("sha256").and_then(|v| v.as_str())
            .context("EULAR reviewed document missing source-verified sha256")?;
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("EULAR reviewed document requires a complete SHA-256 pin");
        }
        if content_mode(doc)? && (doc["reviewed_content"]["recipe"] != "eular-pdfium-72dpi-rgb-text-v1"
            || doc["reviewed_content"]["pages"].as_array().is_none_or(|p| p.is_empty() || p.len() > 100)) {
            bail!("EULAR reviewed_content requires a reviewed complete ordered-page fingerprint");
        }
        let url = Url::parse(doc["url"].as_str().unwrap())?;
        if url.scheme() != "https" || url.host_str().is_none() || !url.username().is_empty()
            || url.password().is_some() || url.fragment().is_some() || doc["rendition"] != "pdf" {
            bail!("EULAR reviewed document requires an explicit public HTTPS PDF route");
        }
        if !keys.insert(doc["doc_key"].as_str().unwrap()) || !urls.insert(doc["url"].as_str().unwrap()) {
            bail!("EULAR reviewed document keys and URLs must be unique");
        }
    }
    let mut reviewed = src.clone();
    reviewed.config["documents"] = json!(docs);
    Ok(reviewed)
}

fn previous_title(a: &ElementRef) -> Option<String> {
    let mut p = a.parent()?.prev_sibling();
    let mut n = 0;
    while let Some(node) = p {
        n += 1;
        if n > 14 { break; }
        if let Some(e) = ElementRef::wrap(node) {
            if e.value().name() == "h5" { break; }
            for strong in e.select(&sel("strong")) {
                let t = text_of(&strong);
                if t.to_lowercase().contains("recommendation") && t.len() > 25 { return Some(t); }
            }
        }
        p = node.prev_sibling();
    }
    None
}

fn catalog(raw: &str, base: &Url) -> Result<Vec<(String, String, String, String)>> {
    let d = Html::parse_document(raw);
    let root = d.select(&sel("main #wrap")).next().context("EULAR management directory root missing")?;
    if !text_of(&root).contains("Recommendations for Management") {
        bail!("EULAR management directory title missing");
    }
    let mut seen = HashSet::new();
    let mut rows = vec![];
    for a in root.select(&sel("a[href]")) {
        let label = text_of(&a);
        let l = label.trim().to_lowercase();
        let role = if l == "read recommendation" { "recommendation_article" }
            else if l.starts_with("slr") { "systematic_review" } else { continue; };
        let Some(url) = a.value().attr("href").and_then(|h| common::link(base, h)) else { continue; };
        if !seen.insert(url.clone()) { continue; }
        let title = previous_title(&a).unwrap_or_else(|| label.clone());
        rows.push((url, title, role.into(), label));
    }
    if rows.is_empty() { bail!("EULAR management article links missing"); }
    Ok(rows)
}

/// BMJ-era article id a directory link states (`/content/early/2016/07/04/annrheumdis-2016-209724.full`).
fn bmj_article_id(url: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?i)\b((?:annrheumdis|ard|rmdopen)-\d{4}-\d{6})\b").unwrap();
    re.captures(url).map(|c| c[1].to_ascii_lowercase())
}

/// Fix group H2 (catalog completeness, reviewed mode only, registry-gated):
/// 1. `config.oa_enumeration`: Europe PMC enumeration of EULAR recommendation / points-to-consider articles in the
///    PMC Open Access subset (ARD, RMD Open, EULAR Rheumatology Open) through the shared crosswalk.
/// 2. `config.directory_crosswalk`: every "Read recommendation" link of the EULAR management directory. A link whose
///    DOI or Elsevier PII (Crossref) is stated, or whose BMJ article id equals an enumerated DOI's suffix, is tied to
///    its OA row (`eular_directory`); a stated DOI not yet enumerated is planned through the crosswalk; any other
///    link stays an explicit unresolved row (publisher terminal not fetched) so the directory count stays visible.
async fn catalog_oa_rows(src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
    let mut rows = if src.config.get("oa_enumeration").is_some() {
        cw::enumerate(src, f, "eular", None, &HashSet::new()).await?
    } else {
        vec![]
    };
    if src.config["directory_crosswalk"] == json!(true) {
        let p = f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;
        let base = Url::parse(&p.record.final_url)?;
        if base.origin() != Url::parse(ROOT)?.origin() { bail!("EULAR directory redirected off source"); }
        let row_doi = |r: &PlanRow| r.extra["epmc_hit"]["doi"].as_str().or(r.extra["pmc_crosswalk"]["jats_doi"].as_str()).map(|d| d.to_ascii_lowercase());
        let mut extra_rows = vec![];
        let mut seen_dois: HashSet<String> = rows.iter().filter_map(row_doi).collect();
        for (url, title, role, label) in catalog(&p.text(), &base)? {
            if role != "recommendation_article" { continue; }
            let dir = json!({"url": url, "title": title, "link_label": label, "directory": p.record.final_url});
            let mut doi = cw::doi_from_url(&url);
            let mut route = "source_doi";
            if doi.is_none() {
                if let Some(pii) = cw::pii_from_url(&url) {
                    route = "source_pii_crossref";
                    doi = cw::doi_for_pii(f, &pii).await.unwrap_or(None);
                }
            }
            let tie = |rows: &mut Vec<PlanRow>, pred: &dyn Fn(&str) -> bool| -> bool {
                match rows.iter_mut().find(|r| row_doi(&**r).is_some_and(|d| pred(&d))) {
                    Some(r) => { r.extra["eular_directory"] = dir.clone(); true }
                    None => false,
                }
            };
            if let Some(d) = doi.clone() {
                if tie(&mut rows, &|x| x == d) || tie(&mut extra_rows, &|x| x == d) { continue; }
                if seen_dois.insert(d.clone()) {
                    let sref = cw::SourceRef { source_url: url.clone(), evidence_url: Some(p.record.final_url.clone()), source_title: Some(title.clone()),
                        item_type: Some("clinical_practice_guideline".into()), key_prefix: "eular".into(), id_route: route.into(), notes: vec![] };
                    match cw::plan_ids(src, f, Some(&d), None, sref, json!({"eular_directory": dir})).await? {
                        cw::Planned::Open(r) => { extra_rows.push(r); continue; }
                        cw::Planned::Closed { reason, .. } => {
                            extra_rows.push(directory_row(src, &url, &title, &dir, format!("EULAR directory article {d}: {reason}"), Some(&d)));
                            continue;
                        }
                    }
                }
                continue;
            }
            if let Some(id) = bmj_article_id(&url) {
                let suffix = format!("/{id}");
                if tie(&mut rows, &|x| x.ends_with(&suffix)) || tie(&mut extra_rows, &|x| x.ends_with(&suffix)) { continue; }
            }
            extra_rows.push(directory_row(src, &url, &title, &dir,
                "EULAR directory link states no DOI/PII and matches no PMC Open Access copy; publisher terminal not fetched".into(), None));
        }
        rows.extend(extra_rows);
    }
    if let Some(n) = limit { rows.truncate(n); }
    Ok(rows)
}

fn directory_row(src: &SourceEntry, url: &str, title: &str, dir: &Value, reason: String, doi: Option<&str>) -> PlanRow {
    PlanRow { source_id: src.id.clone(), doc_key: format!("eular-{}-unresolved", common::short_hash(url)), url: url.into(), title: Some(title.into()),
        rendition: None, version: None, extra: json!({"source_role": "recommendation_article", "eular_directory": dir, "doi": doi, "discovery_error": reason}) }
}

#[async_trait::async_trait]
impl Adapter for Eular {
    async fn discover(&self, src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        // Guard before StaticList: its historical loop otherwise emits its first row for limit=0.
        if limit == Some(0) { return Ok(vec![]); }
        if reviewed_mode(src)? {
            let mut rows = StaticList.discover(&reviewed_source(src)?, f, limit).await?;
            for row in &mut rows {
                if content_mode(&row.extra)? {
                    let fetched = f.get_ok(&row.url).await?;
                    if !fetched.is_pdf() { bail!("EULAR reviewed rendition did not return PDF bytes"); }
                    verify_content(&row.extra, &fetched.body).await?;
                    row.extra["wire_sha256"] = json!(crate::common::sha256_hex(&fetched.body));
                    row.extra["identity_fetch"] = serde_json::to_value(fetched.record)?;
                }
            }
            if !limit.is_some_and(|n| rows.len() >= n) {
                let room = limit.map(|n| n - rows.len());
                rows.extend(catalog_oa_rows(src, f, room).await?);
            }
            return Ok(rows);
        }
        let p = f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;
        let base = Url::parse(&p.record.final_url)?;
        if base.origin() != Url::parse(ROOT)?.origin() { bail!("EULAR directory redirected off source"); }
        let entries = catalog(&p.text(), &base)?;
        let mut out = vec![];
        for (url, title, role, label) in entries {
            let reason = match f.get_ok(&url).await {
                Err(e) => format!("EULAR source publisher terminal unavailable: {e:#}"),
                Ok(_) => "EULAR publisher terminal responded but its full article template and rights are unvalidated".into(),
            };
            out.push(PlanRow {
                source_id: src.id.clone(),
                doc_key: format!("eular-{}-unresolved", common::short_hash(&url)),
                url, title: Some(title), rendition: None, version: None,
                extra: json!({"source_role":role,"source_link_label":label,"catalog_fetch":p.record,"discovery_error":reason}),
            });
            if limit.is_some_and(|n| out.len() >= n) { break; }
        }
        Ok(out)
    }

    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if cw::is_crosswalk_row(row) {
            return cw::extract(src, f, row).await;
        }
        if reviewed_mode(src)? && row.extra["eular_directory"].is_object() {
            bail!("EULAR publisher terminal unresolved: {}", row.extra["discovery_error"].as_str().unwrap_or("unknown"));
        }
        if reviewed_mode(src)? {
            let reviewed = reviewed_source(src)?;
            let expected = StaticList.discover(&reviewed, f, None).await?.into_iter()
                .find(|r| r.doc_key == row.doc_key && r.url == row.url)
                .context("EULAR plan row is outside the explicitly reviewed subset")?;
            let visible = content_mode(&expected.extra)?;
            let mut configured = row.extra.clone();
            if visible {
                configured.as_object_mut().context("EULAR plan metadata missing")?.remove("wire_sha256");
                configured.as_object_mut().unwrap().remove("identity_fetch");
            }
            if row.source_id != expected.source_id || row.title != expected.title || row.version != expected.version
                || row.rendition != expected.rendition || configured != expected.extra {
                bail!("EULAR plan row differs from the configured reviewed rendition");
            }
            // The verified repository endpoint ends /content. Do not accept an HTML shell simply
            // because that extensionless URL lacks StaticList's normal .pdf wrapper detection.
            // Public Fetcher saves this response; StaticList's get reuses it without a second HTTP GET.
            let fetched = f.get_ok(&row.url).await?;
            if !fetched.is_pdf() { bail!("EULAR reviewed rendition did not return PDF bytes"); }
            let expected_sha = row.extra[if visible { "wire_sha256" } else { "sha256" }].as_str()
                .context("EULAR immutable plan wire SHA missing")?;
            if !crate::common::sha256_hex(&fetched.body).eq_ignore_ascii_case(expected_sha) {
                bail!("EULAR reviewed PDF SHA-256 changed; explicit source review required");
            }
            if visible { verify_content(&expected.extra, &fetched.body).await?; }
            let mut doc = StaticList.extract(&reviewed, f, row).await?;
            if doc.fetches.first().is_none_or(|fetch| !fetch.sha256.eq_ignore_ascii_case(expected_sha)) {
                bail!("EULAR wire snapshot changed during extraction replay");
            }
            if visible {
                let planned: crate::guidelines::doc::FetchRecord = serde_json::from_value(row.extra["identity_fetch"].clone())?;
                if planned.url != row.url || !planned.sha256.eq_ignore_ascii_case(expected_sha) {
                    bail!("EULAR immutable plan fetch provenance differs from wire snapshot");
                }
                doc.fetches.insert(0, planned);
            }
            // A source DOI in the manifest is not evidence that different downloaded bytes contain
            // that work. Require the decoded article to contain the DOI before accepting metadata.
            let decoded: String = doc.blocks.iter().flat_map(|b| b.text.chars())
                .filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
            let doi = row.extra["doi"].as_str().unwrap().to_ascii_lowercase();
            if !decoded.contains(&doi) {
                bail!("EULAR reviewed PDF decoded text does not contain the configured DOI");
            }
            return Ok(doc);
        }
        bail!("EULAR publisher terminal unresolved: {}", row.extra["discovery_error"].as_str().unwrap_or("unknown"))
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const CAT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/eular_recommendations/catalog.html"));
    const REVIEWED: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/eular_reviewed/source.json"));
    fn source() -> SourceEntry { serde_json::from_str(REVIEWED).unwrap() }
    fn offline(tmp: &tempfile::TempDir) -> Fetcher { Fetcher::new(tmp.path(), true, 500, 4 << 20, "").unwrap() }

    #[test]
    #[ignore = "explicit golden PDF and EULAR_FINGERPRINT_OUTPUT required"]
    fn emit_reviewed_content_fingerprint() {
        let bytes = std::fs::read(std::env::var_os("EULAR_REVIEWED_PDF_FIXTURE").unwrap()).unwrap();
        assert_eq!(crate::common::sha256_hex(&bytes), source().config["reviewed_documents"][0]["sha256"].as_str().unwrap());
        let (fingerprint, decoded) = content_fingerprint(&bytes).unwrap();
        let decoded: String = decoded.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(decoded.contains("10.1136/ard-2024-225531"));
        std::fs::write(std::env::var_os("EULAR_FINGERPRINT_OUTPUT").unwrap(),
            serde_json::to_vec_pretty(&json!({"reviewed_content":fingerprint,
                "golden_sha256":crate::common::sha256_hex(&bytes)})).unwrap()).unwrap();
    }

    #[tokio::test]
    #[ignore = "explicit old and regenerated PDF fixtures required"]
    async fn reviewed_content_accepts_regeneration_but_pins_wire_and_rejects_content_changes() {
        let old = std::fs::read(std::env::var_os("EULAR_REVIEWED_PDF_FIXTURE").unwrap()).unwrap();
        let new = std::fs::read(std::env::var_os("EULAR_REGENERATED_PDF_FIXTURE").unwrap()).unwrap();
        assert_ne!(crate::common::sha256_hex(&old), crate::common::sha256_hex(&new));
        let golden = tokio::task::spawn_blocking({let old=old.clone(); move || content_fingerprint(&old)}).await.unwrap().unwrap().0;
        let mut src = source();
        src.config["reviewed_documents"][0]["identity_mode"] = json!("reviewed_content");
        src.config["reviewed_documents"][0]["reviewed_content"] = golden.clone();
        let tmp = tempfile::tempdir().unwrap();
        let f = offline(&tmp);
        let url = src.config["reviewed_documents"][0]["url"].as_str().unwrap();
        f.seed_cache_for_tests(url, &old, "application/pdf");
        let old_row = Eular.discover(&src, &f, None).await.unwrap().remove(0);
        assert_eq!(old_row.extra["wire_sha256"], crate::common::sha256_hex(&old));
        f.seed_cache_for_tests(url, &new, "application/pdf");
        assert!(Eular.extract(&src, &f, &old_row).await.unwrap_err().to_string().contains("SHA-256 changed"));
        let new_row = Eular.discover(&src, &f, None).await.unwrap().remove(0);
        assert_eq!(new_row.extra["wire_sha256"], crate::common::sha256_hex(&new));
        let doc = Eular.extract(&src, &f, &new_row).await.unwrap();
        assert_eq!(doc.doi.as_deref(), Some("10.1136/ard-2024-225531"));
        assert_eq!(doc.fetches.len(), 2); // discovery wire observation and extraction replay
        // Challenge each independently required component of the real golden. Changing the
        // expected fingerprint versus changing the observed content is symmetric under equality.
        let mut mutations = vec![];
        let mut x=golden.clone(); x["pages"][0]["text_sha256"]=json!("clinical-text-change"); mutations.push(x);
        let mut x=golden.clone(); x["pages"][0]["rgb_sha256"]=json!("figure-change"); mutations.push(x);
        let mut x=golden.clone(); x["pages"][0]["size_pdf_points"]=json!([1,1]); mutations.push(x);
        let mut x=golden.clone(); x["pages"].as_array_mut().unwrap().swap(0,1); mutations.push(x);
        let mut x=golden.clone(); x["pages"].as_array_mut().unwrap().pop(); mutations.push(x);
        let mut x=golden.clone(); x["pages"].as_array_mut().unwrap().push(golden["pages"][0].clone()); mutations.push(x);
        for changed in mutations {
            let mut invalid = src.config["reviewed_documents"][0].clone();
            invalid["reviewed_content"] = changed;
            assert!(verify_content(&invalid, &new).await.unwrap_err().to_string().contains("fingerprint changed"));
        }
        let mut invalid = src.config["reviewed_documents"][0].clone();
        invalid["doi"]=json!("10.0000/wrong-work");
        assert!(verify_content(&invalid, &new).await.unwrap_err().to_string().contains("configured DOI"));
    }

    #[test]
    fn real_directory_keeps_article_and_review_separate() {
        let a = catalog(CAT, &Url::parse(ROOT).unwrap()).unwrap();
        let rec = a.iter().find(|(u, _, _, _)| u.contains("S0003-4967(26)00357-2/fulltext")).unwrap();
        assert_eq!(rec.2, "recommendation_article");
        assert!(rec.1.contains("polymyalgia rheumatica"));
        let slr = a.iter().find(|(u, _, _, _)| u.contains("S3050-7081(26)00083-2/fulltext")).unwrap();
        assert_eq!(slr.2, "systematic_review");
        assert_ne!(rec.0, slr.0);
    }

    #[tokio::test]
    async fn source_terminals_remain_errors_when_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let f = offline(&tmp);
        f.seed_cache_for_tests(ROOT, CAT.as_bytes(), "text/html");
        // Merely supplying reviewed documents does not opt in or replace the directory audit.
        let mut src = source();
        src.config.as_object_mut().unwrap().remove("route_mode");
        let r = Eular.discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| x.extra["discovery_error"].is_string()));
        assert!(Eular.extract(&src, &f, &r[0]).await.is_err());
    }

    #[tokio::test]
    async fn reviewed_plan_preserves_verified_identity_without_catalog_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        let f = offline(&tmp); // Empty cache: a catalog or terminal fetch would fail.
        let rows = Eular.discover(&source(), &f, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.doc_key, "psa-2023-update-2024");
        assert_eq!(row.version.as_deref(), Some("2023 update"));
        assert_eq!(row.rendition.as_deref(), Some("pdf"));
        assert_eq!(row.extra["doi"], "10.1136/ard-2024-225531");
        assert!(row.url.contains("4fda60e7-86da-4cf6-81f3-722fa4d8c1f1/content"));
        assert_eq!(row.extra["search_metadata"]["complete_catalog"], false);
        assert_eq!(row.extra["search_metadata"]["publication_year"], "2024");
    }

    #[tokio::test]
    async fn zero_limit_needs_no_http_or_reviewed_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let f = offline(&tmp);
        for config in [json!({}), json!({"route_mode":"reviewed_documents"})] {
            let mut src = source();
            src.config = config;
            assert!(Eular.discover(&src, &f, Some(0)).await.unwrap().is_empty());
        }
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn reviewed_mode_rejects_missing_config_unknown_modes_and_html_shells() {
        let tmp = tempfile::tempdir().unwrap();
        let f = offline(&tmp);
        let mut src = source();
        src.config["route_mode"] = json!("typo");
        assert!(Eular.discover(&src, &f, None).await.is_err());
        src.config = json!({"route_mode":"reviewed_documents"});
        assert!(Eular.discover(&src, &f, None).await.is_err());
        let src = source();
        let row = Eular.discover(&src, &f, None).await.unwrap().remove(0);
        f.seed_cache_for_tests(&row.url, b"<html><main>Repository download wrapper</main></html>", "text/html");
        assert!(Eular.extract(&src, &f, &row).await.unwrap_err().to_string().contains("PDF bytes"));
        let mut changed = row.clone();
        changed.extra["doi"] = json!("10.0000/wrong-work");
        assert!(Eular.extract(&src, &f, &changed).await.unwrap_err().to_string().contains("differs"));
        f.seed_cache_for_tests(&row.url, b"%PDF-1.4\nwrong rendition", "application/pdf");
        assert!(Eular.extract(&src, &f, &row).await.unwrap_err().to_string().contains("SHA-256 changed"));
    }

    #[tokio::test]
    #[ignore = "requires explicit EULAR_REVIEWED_PDF_FIXTURE pointing to the verified source PDF"]
    async fn reviewed_pdf_extracts_body_and_preserves_rights_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let f = offline(&tmp);
        let src = source();
        let row = Eular.discover(&src, &f, Some(1)).await.unwrap().remove(0);
        let path = std::env::var_os("EULAR_REVIEWED_PDF_FIXTURE")
            .expect("EULAR_REVIEWED_PDF_FIXTURE must name the verified Griffith source PDF");
        let pdf = std::fs::read(&path).expect("verified Griffith source PDF fixture required");
        f.seed_cache_for_tests(&row.url, &pdf, "application/pdf;charset=UTF-8");
        let doc = Eular.extract(&src, &f, &row).await.unwrap();
        assert_eq!(doc.doc_key, row.doc_key);
        assert_eq!(doc.work_key.as_deref(), Some("eular-psoriatic-arthritis"));
        assert_eq!(doc.title, row.title.unwrap());
        assert_eq!(doc.version.as_deref(), Some("2023 update"));
        assert_eq!(doc.doi.as_deref(), Some("10.1136/ard-2024-225531"));
        assert_eq!(doc.rights.status, "restricted");
        assert!(doc.rights.license.is_none());
        assert!(doc.rights.note.as_deref().unwrap().contains("CC BY-NC-ND"));
        assert!(doc.rights.note.as_deref().unwrap().contains("CC BY-NC 4.0"));
        assert_eq!(doc.search_metadata["publication_year"], "2024");
        let body = doc.blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join(" ").to_lowercase();
        for text in ["psoriatic arthritis", "overarching principles", "recommendation 11", "10.1136/ard-2024-225531"] {
            assert!(body.contains(text), "missing verified full-article marker: {text}");
        }
        assert_eq!(doc.fetches.len(), 1);
        assert!(doc.fetches[0].from_cache);
        // Even with the approved bytes and hash, metadata cannot project a different DOI.
        let mut wrong_doi = src.clone();
        wrong_doi.config["reviewed_documents"][0]["doi"] = json!("10.0000/wrong-work");
        let row = Eular.discover(&wrong_doi, &f, None).await.unwrap().remove(0);
        assert!(Eular.extract(&wrong_doi, &f, &row).await.unwrap_err().to_string().contains("decoded text"));
    }

    /// Fix group H2: reviewed rows stay first and unchanged; Europe PMC OA rows follow, tied to the directory link
    /// that names them; every other directory recommendation stays an explicit unresolved row.
    #[tokio::test]
    async fn oa_enumeration_and_directory_crosswalk_extend_reviewed_mode_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let f = offline(&tmp);
        f.seed_cache_for_tests(ROOT, CAT.as_bytes(), "text/html");
        let mut src = source();
        let q = "TITLE:\"EULAR\" AND (OPEN_ACCESS:y)";
        src.config["oa_enumeration"] = json!({"query": q, "max": 10, "item_type": "clinical_practice_guideline", "scope": "test"});
        src.config["directory_crosswalk"] = json!(true);
        // Europe PMC answer; the crosswalk's OA JATS fixture stands in for the article body
        let answer = json!({"hitCount": 1, "nextCursorMark": "*", "resultList": {"result": [
            {"id":"35647308","source":"MED","pmcid":"PMC9134774","doi":"10.1136/ard-2024-225531","title":"EULAR recommendations for the management of psoriatic arthritis with pharmacological therapies: 2023 update","isOpenAccess":"Y","inPMC":"Y","firstPublicationDate":"2024-03-18"}]}});
        f.seed_cache_for_tests(&cw::epmc_search_url(q, 10, "*"), answer.to_string().as_bytes(), "application/json");
        let oai = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/pmc_crosswalk/oai-PMC9134774.xml"));
        f.seed_cache_for_tests(&cw::oai_url("PMC9134774").unwrap(), oai.as_bytes(), "application/xml");
        let rows = Eular.discover(&src, &f, None).await.unwrap();
        let reviewed = Eular.discover(&source(), &f, None).await.unwrap();
        assert_eq!(rows[0].doc_key, reviewed[0].doc_key);
        let oa = &rows[1];
        assert!(cw::is_crosswalk_row(oa) && oa.doc_key.starts_with("eular-pmc9134774-"));
        assert!(oa.extra["eular_directory"]["url"].as_str().unwrap().contains("ard-2024-225531"), "BMJ article id tied to the API DOI");
        let directory = catalog(CAT, &Url::parse(ROOT).unwrap()).unwrap().into_iter().filter(|r| r.2 == "recommendation_article").count();
        assert!(directory > 60);
        let unresolved: Vec<_> = rows.iter().filter(|r| r.extra["discovery_error"].is_string()).collect();
        assert_eq!(unresolved.len(), directory - 1, "every other directory recommendation is visible");
        assert!(unresolved.iter().all(|r| r.doc_key.starts_with("eular-") && r.doc_key.ends_with("-unresolved") && r.extra["eular_directory"].is_object()));
        let urls: HashSet<_> = rows.iter().map(|r| r.url.clone()).collect();
        assert_eq!(urls.len(), rows.len());
        assert!(Eular.extract(&src, &f, unresolved[0]).await.unwrap_err().to_string().contains("unresolved"));
        let d = Eular.extract(&src, &f, oa).await.unwrap();
        assert!(d.relations.iter().any(|r| r.rel == "rendition_of" && r.target == "pmc:PMC9134774"));
        assert_eq!(Eular.discover(&src, &f, Some(3)).await.unwrap().len(), 3);
        assert_eq!(bmj_article_id("http://ard.bmj.com/content/early/2016/07/04/annrheumdis-2016-209724.full").as_deref(), Some("annrheumdis-2016-209724"));
        assert!(bmj_article_id("https://ard.bmj.com/content/80/1/31").is_none());
    }
}
