//! `h2_static_catalog` (fix group H2-catalog-small): a `static_list` source whose registry seed list is kept
//! verbatim (same rows, same doc_keys) and extended with the publisher's own catalog routes, all registry-driven:
//!
//! - `config.documents`: the static seed list, planned exactly as `static_list` plans it (rows first).
//! - `config.magic_api = {"org_short_names": ["cari"], "item_type": "clinical_practice_guideline", "scope": "..."}`:
//!   the official MAGICapp API (`https://api.magicapp.org/`, which names itself "the MAGICapp API" and links its
//!   Swagger documentation; `api.magicapp.org/robots.txt` is 404 = unrestricted). `GET /api/v1/guidelines
//!   ?orgShortName=<org>&limit=500` lists the organisation's public guidelines with `pdfPublishedPath`, the
//!   published PDF the MAGIC reader's own PDF button opens. The API states that path on the bucket's S3 path-style
//!   host (`s3.amazonaws.com/files.magicapp.org/...`); the reader and the seed list use the same object on the
//!   bucket's own public host `files.magicapp.org` (robots `*`: Allow), so the path is kept and only the S3
//!   path-style prefix is replaced. A published path already in the seed list is not planned twice.
//! - `config.site_pdf_pages = {"sitemap": "<robots-declared WordPress page sitemap>", "include_path_prefixes": [..],
//!   "exclude_path_prefixes": [..], "content_root": ".post-content", "item_type": "..."}`: every sitemap page under
//!   the prefixes is fetched and the PDFs its content root embeds (pdf.js viewer `file=` targets) or links on the
//!   same host become rows (one row per distinct PDF; the page is kept as `source_page`).
//! - `config.oa_enumeration`: Europe PMC enumeration of open-access PMC copies through `pmc_crosswalk::enumerate`
//!   (same config schema as group A's asco/ers).
//!
//! Extraction: crosswalk rows go to `pmc_crosswalk::extract`; every other row is a single public rendition and is
//! extracted exactly like `static_list` (same fields from `row.extra`).
use super::pmc_crosswalk as cw;
use super::static_list::StaticList;
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::GuidelineDoc;
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::{json, Value};
use std::collections::HashSet;
use url::Url;

pub struct H2StaticCatalog;

pub const MAGIC_API: &str = "https://api.magicapp.org/api/v1/guidelines";

pub fn magic_list_url(org: &str) -> String {
    let org: String = url::form_urlencoded::byte_serialize(org.as_bytes()).collect();
    format!("{MAGIC_API}?orgShortName={org}&limit=500")
}

/// `https://s3.amazonaws.com/files.magicapp.org/<path>` (API) -> `https://files.magicapp.org/<path>` (reader, seeds).
pub fn magic_public_pdf(raw: &str) -> Option<String> {
    let u = Url::parse(raw.trim()).ok()?;
    let host = u.host_str()?;
    let path = match host {
        "s3.amazonaws.com" => u.path().strip_prefix("/files.magicapp.org/")?.to_string(),
        "files.magicapp.org" => u.path().trim_start_matches('/').to_string(),
        _ => return None,
    };
    if !path.starts_with("guideline/") || !path.to_ascii_lowercase().ends_with(".pdf") {
        return None;
    }
    Some(format!("https://files.magicapp.org/{path}"))
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::trim).filter(|x| !x.is_empty()).map(String::from)
}

/// Rows for one MAGIC organisation listing (pure; the caller fetched `list_url`).
pub fn magic_rows(src: &SourceEntry, org: &str, list: &Value, list_url: &str, fetch: &Value, seen: &mut HashSet<String>) -> Result<Vec<PlanRow>> {
    let cfg = &src.config["magic_api"];
    let arr = list.as_array().context("MAGIC API answer is not a list")?;
    let item_type = s(cfg, "item_type").unwrap_or_else(|| "clinical_practice_guideline".into());
    let mut items: Vec<&Value> = arr.iter().filter(|g| g["institutionShortName"].as_str() == Some(org)).collect();
    items.sort_by_key(|g| (g["publishedId"].as_i64().unwrap_or(i64::MAX), g["guidelineId"].as_i64().unwrap_or(i64::MAX)));
    let mut out = vec![];
    for g in items {
        let name = s(g, "name");
        let short = s(g, "shortCode");
        let published = g["publishedId"].as_i64();
        let version = s(g, "versionLabel");
        let magic = json!({
            "api": list_url, "org_short_name": org, "institution_name": g["institutionName"], "owner": g["owner"],
            "guideline_id": g["guidelineId"], "published_id": g["publishedId"], "short_code": g["shortCode"],
            "version_label": g["versionLabel"], "status_label": g["status"], "is_public": g["isPublic"],
            "is_latest_published": g["isLatestPublished"], "publish_date": g["publishDate"], "language": g["language"],
            "published_recommendation_count": g["publishedRecommendationCount"], "json_path": g["jsonPath"],
            "pdf_published_path": g["pdfPublishedPath"],
            "reader": short.as_ref().map(|c| format!("https://app.magicapp.org/#/guideline/{c}")),
            "scope": cfg["scope"], "status_note": "MAGIC status label kept verbatim; NOTSET/null is not a currency claim",
        });
        let key_id = published.map(|p| p.to_string()).unwrap_or_else(|| g["guidelineId"].to_string());
        let vslug = version.as_deref().map(common::slug).unwrap_or_default();
        let Some(pdf) = g["pdfPublishedPath"].as_str().and_then(magic_public_pdf) else {
            let raw = g["pdfPublishedPath"].as_str().unwrap_or("").to_string();
            out.push(PlanRow { source_id: src.id.clone(), doc_key: format!("magic-{key_id}-v{vslug}-unresolved"),
                url: short.as_ref().map(|c| format!("https://app.magicapp.org/#/guideline/{c}")).unwrap_or_else(|| list_url.into()),
                title: name, rendition: None, version,
                extra: json!({"discovery_error": format!("MAGIC API record has no files.magicapp.org published PDF path ({raw:?})"), "magic": magic}) });
            continue;
        };
        if !seen.insert(pdf.clone()) {
            continue;
        }
        out.push(PlanRow {
            source_id: src.id.clone(),
            doc_key: format!("magic-{key_id}-v{vslug}"),
            url: pdf,
            title: name,
            rendition: Some("pdf".into()),
            version,
            extra: json!({
                "work_key": short.as_ref().map(|c| format!("magic-{c}")),
                "item_type": item_type, "status": "unknown", "published": g["publishDate"],
                "search_metadata": {"magic": magic, "catalog_route": "magic_api", "api_fetch": fetch},
            }),
        });
    }
    Ok(out)
}

/// PDF targets a WordPress page embeds or links inside its content root (pure).
pub fn page_pdfs(html: &str, base: &Url, root: &str) -> Result<(Option<String>, Vec<String>)> {
    let d = Html::parse_document(html);
    let r = d.select(&sel(root)).next().with_context(|| format!("content root {root} missing"))?;
    let title = d.select(&sel("h1")).map(|h| text_of(&h)).find(|t| !t.is_empty())
        .or_else(|| d.select(&sel("title")).next().map(|t| text_of(&t)).filter(|t| !t.is_empty()));
    let mut out = vec![];
    let mut seen = HashSet::new();
    let mut push = |raw: &str| {
        let Some(u) = common::link(base, raw).and_then(|x| Url::parse(&x).ok()) else { return };
        if u.host_str() != base.host_str() {
            return;
        }
        // pdf.js viewer: the document is its `file` parameter
        let target = if u.path().contains("pdfjs") {
            match u.query_pairs().find(|(k, _)| k == "file").and_then(|(_, v)| common::link(base, &v)) {
                Some(t) => t,
                None => return,
            }
        } else {
            u.to_string()
        };
        let Ok(t) = Url::parse(&target) else { return };
        if t.host_str() == base.host_str() && t.path().to_ascii_lowercase().ends_with(".pdf") && seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    };
    for e in r.select(&sel("a[href], iframe[src], embed[src], object[data]")) {
        for attr in ["href", "src", "data"] {
            if let Some(v) = e.value().attr(attr) {
                push(v);
            }
        }
    }
    Ok((title, out))
}

pub fn sitemap_locs(xml: &str) -> Vec<String> {
    let re = regex::Regex::new(r"<loc>\s*([^<\s]+)\s*</loc>").unwrap();
    re.captures_iter(xml).map(|c| c[1].replace("&amp;", "&")).collect()
}

async fn site_rows(src: &SourceEntry, f: &Fetcher, seen: &mut HashSet<String>, limit: Option<usize>) -> Result<Vec<PlanRow>> {
    let cfg = &src.config["site_pdf_pages"];
    let sitemap = s(cfg, "sitemap").context("site_pdf_pages.sitemap missing")?;
    let root = s(cfg, "content_root").context("site_pdf_pages.content_root missing")?;
    let list = |k: &str| cfg[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
    let (inc, exc) = (list("include_path_prefixes"), list("exclude_path_prefixes"));
    let sm = f.get_ok(&sitemap).await?;
    let host = Url::parse(&sm.record.final_url)?.host_str().map(String::from);
    let mut pages: Vec<String> = sitemap_locs(&sm.text()).into_iter().filter(|u| {
        Url::parse(u).is_ok_and(|x| x.host_str().map(String::from) == host && inc.iter().any(|p| x.path().starts_with(p.as_str()))
            && !exc.iter().any(|p| x.path().starts_with(p.as_str())))
    }).collect();
    pages.sort();
    pages.dedup();
    let item_type = s(cfg, "item_type");
    let mut rows = vec![];
    let mut empty = vec![];
    let mut failed = vec![];
    for page in &pages {
        if limit.is_some_and(|n| rows.len() >= n) {
            break;
        }
        let p = match f.get_ok(page).await {
            Ok(p) => p,
            Err(e) => {
                failed.push(json!({"page": page, "error": format!("{e:#}")}));
                continue;
            }
        };
        let base = Url::parse(&p.record.final_url)?;
        let (title, pdfs) = match page_pdfs(&p.text(), &base, &root) {
            Ok(x) => x,
            Err(e) => {
                failed.push(json!({"page": page, "error": format!("{e:#}")}));
                continue;
            }
        };
        if pdfs.is_empty() {
            empty.push(page.clone());
        }
        let n = pdfs.len();
        for (i, pdf) in pdfs.into_iter().enumerate() {
            if !seen.insert(pdf.clone()) {
                continue;
            }
            let t = match (&title, n) {
                (Some(t), 1) => Some(t.clone()),
                (Some(t), _) => Some(format!("{t} ({})", i + 1)),
                (None, _) => None,
            };
            rows.push(PlanRow {
                source_id: src.id.clone(),
                doc_key: format!("site-{}", common::short_hash(&pdf)),
                url: pdf,
                title: t,
                rendition: Some("pdf".into()),
                version: None,
                extra: json!({"work_key": format!("site-{}", common::short_hash(page)), "item_type": item_type, "status": "unknown",
                    "search_metadata": {"catalog_route": "site_sitemap_pdf", "source_page": page, "source_page_title": title,
                        "sitemap": sitemap, "page_fetch": p.record, "scope": cfg["scope"]}}),
            });
        }
    }
    let summary = json!({"sitemap": sitemap, "pages_in_scope": pages.len(), "pages_without_pdf": empty, "pages_failed": failed});
    for r in &mut rows {
        r.extra["search_metadata"]["site_scan"] = summary.clone();
    }
    if rows.is_empty() && !pages.is_empty() && limit != Some(0) {
        bail!("site_pdf_pages: {} in-scope pages yielded no PDF ({} failed)", pages.len(), summary["pages_failed"].as_array().map_or(0, |a| a.len()));
    }
    Ok(rows)
}

#[async_trait::async_trait]
impl Adapter for H2StaticCatalog {
    async fn discover(&self, src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let room = |n: usize| limit.map(|l| l.saturating_sub(n));
        let full = |n: usize| limit.is_some_and(|l| n >= l);
        let mut rows = if src.config.get("documents").is_some() { StaticList.discover(src, f, limit).await? } else { vec![] };
        let mut seen: HashSet<String> = rows.iter().map(|r| r.url.clone()).collect();
        if let Some(orgs) = src.config["magic_api"]["org_short_names"].as_array() {
            for org in orgs.iter().filter_map(|o| o.as_str()) {
                if full(rows.len()) {
                    break;
                }
                let url = magic_list_url(org);
                let p = f.get_ok(&url).await?;
                let v: Value = serde_json::from_slice(&p.body).context("MAGIC API answer not JSON")?;
                let fetch = serde_json::to_value(&p.record)?;
                let mut more = magic_rows(src, org, &v, &url, &fetch, &mut seen)?;
                if let Some(r) = room(rows.len()) {
                    more.truncate(r);
                }
                rows.extend(more);
            }
        }
        if src.config.get("site_pdf_pages").is_some() && !full(rows.len()) {
            let more = site_rows(src, f, &mut seen, room(rows.len())).await?;
            rows.extend(more);
        }
        if src.config.get("oa_enumeration").is_some() && !full(rows.len()) {
            let prefix = s(&src.config["oa_enumeration"], "key_prefix").unwrap_or_else(|| format!("{}-oa", src.id));
            rows.extend(cw::enumerate(src, f, &prefix, room(rows.len()), &HashSet::new()).await?);
        }
        if let Some(l) = limit {
            rows.truncate(l);
        }
        Ok(rows)
    }

    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if cw::is_crosswalk_row(row) {
            return cw::extract(src, f, row).await;
        }
        if let Some(e) = row.extra["discovery_error"].as_str() {
            bail!("{} discovery unresolved: {e}", src.id);
        }
        let mut doc = StaticList.extract(src, f, row).await?;
        if let Some(page) = row.extra["search_metadata"]["source_page"].as_str() {
            doc.relations.push(crate::guidelines::doc::Relation { rel: "related".into(), target: page.into(),
                scope: Some("source page that embeds or links this PDF".into()), evidence_url: Some(page.into()) });
        }
        if let Some(reader) = row.extra["search_metadata"]["magic"]["reader"].as_str() {
            doc.relations.push(crate::guidelines::doc::Relation { rel: "rendition_of".into(), target: reader.into(),
                scope: Some("published PDF of this MAGICapp guideline version (MAGIC API pdfPublishedPath)".into()),
                evidence_url: row.extra["search_metadata"]["magic"]["api"].as_str().map(String::from) });
        }
        Ok(doc)
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const FX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/h2_catalog");

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{FX}/{name}")).unwrap()
    }

    fn registry_source(id: &str) -> SourceEntry {
        let reg: Value = serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../configs/guideline_sources.json")).unwrap()).unwrap();
        let s = reg["sources"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone();
        serde_json::from_value(s).unwrap()
    }

    #[test]
    fn magic_paths_keep_the_object_and_drop_only_the_s3_prefix() {
        assert_eq!(magic_public_pdf("https://s3.amazonaws.com/files.magicapp.org/guideline/2d3034bd-4d24-4963-9259-071abda1f507/published_guideline_9509-3_1.pdf").as_deref(),
            Some("https://files.magicapp.org/guideline/2d3034bd-4d24-4963-9259-071abda1f507/published_guideline_9509-3_1.pdf"));
        assert!(magic_public_pdf("https://s3.amazonaws.com/other-bucket/guideline/x.pdf").is_none());
        assert!(magic_public_pdf("https://s3.amazonaws.com/files.magicapp.org/guideline/x/guideline_1-1_0.json").is_none());
        assert!(magic_public_pdf("not a url").is_none());
    }

    #[tokio::test]
    async fn registry_sources_keep_seed_rows_and_add_magic_org_guidelines_offline() {
        // (source id, org, saved API answer, seed rows, MAGIC rows after dedup against seeds)
        for (id, org, file, seeds, magic) in [
            ("au_nz_cari", "cari", "magic-cari.json", 2, 4),
            ("ada_dental", "upsdm", "magic-upsdm.json", 1, 1),
            ("eaes", "eaes", "magic-eaes.json", 2, 11),
            ("bmj_rapid_recommendations", "magicevidence", "magic-magicevidence.json", 1, 34),
        ] {
            let mut src = registry_source(id);
            assert_eq!(src.adapter.as_deref(), Some("h2_static_catalog"), "{id}");
            // network-dependent catalog routes are covered by their own tests
            for k in ["oa_enumeration", "site_pdf_pages"] {
                src.config.as_object_mut().unwrap().remove(k);
            }
            let tmp = tempfile::tempdir().unwrap();
            let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
            f.seed_cache_for_tests(&magic_list_url(org), fx(file).as_bytes(), "application/json");
            let rows = H2StaticCatalog.discover(&src, &f, None).await.unwrap();
            let static_rows = StaticList.discover(&src, &f, None).await.unwrap();
            assert_eq!(static_rows.len(), seeds, "{id}");
            for (a, b) in static_rows.iter().zip(&rows) {
                assert_eq!(a.doc_key, b.doc_key, "{id}: seed doc_keys must not change");
            }
            let m: Vec<_> = rows.iter().filter(|r| r.doc_key.starts_with("magic-")).collect();
            assert_eq!(m.len(), magic, "{id}");
            assert!(m.iter().all(|r| r.url.starts_with("https://files.magicapp.org/guideline/") && r.url.ends_with(".pdf")));
            let urls: HashSet<_> = rows.iter().map(|r| &r.url).collect();
            assert_eq!(urls.len(), rows.len(), "{id}: no PDF planned twice");
            let keys: HashSet<_> = rows.iter().map(|r| &r.doc_key).collect();
            assert_eq!(keys.len(), rows.len(), "{id}: unique doc_keys");
            assert!(m.iter().all(|r| r.extra["search_metadata"]["magic"]["status_label"].is_string() || r.extra["search_metadata"]["magic"]["status_label"].is_null()));
            assert_eq!(H2StaticCatalog.discover(&src, &f, Some(2)).await.unwrap().len(), 2.min(rows.len()));
            assert!(H2StaticCatalog.discover(&src, &f, Some(0)).await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn magic_row_extracts_through_static_pipeline_with_reader_relation() {
        let mut src = registry_source("au_nz_cari");
        for k in ["oa_enumeration", "site_pdf_pages"] {
            src.config.as_object_mut().unwrap().remove(k);
        }
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
        f.seed_cache_for_tests(&magic_list_url("cari"), fx("magic-cari.json").as_bytes(), "application/json");
        let rows = H2StaticCatalog.discover(&src, &f, None).await.unwrap();
        let r = rows.iter().find(|r| r.doc_key == "magic-6446-v2-3").expect("urate-lowering guideline planned");
        assert_eq!(r.version.as_deref(), Some("2.3"));
        assert_eq!(r.extra["published"], "2022-11-30");
        assert_eq!(r.extra["search_metadata"]["magic"]["status_label"], "UPDATED");
        // a real (small) PDF stands in for the MAGIC rendition: the static pipeline converts it unchanged
        let pdf = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/aao_hnsf/adult-sinusitis-summary.pdf")).unwrap();
        f.seed_cache_for_tests(&r.url, &pdf, "application/pdf");
        let d = H2StaticCatalog.extract(&src, &f, r).await.unwrap();
        assert_eq!(d.doc_key, "magic-6446-v2-3");
        assert_eq!(d.published.as_deref(), Some("2022-11-30"));
        assert_eq!(d.item_type.as_deref(), Some("clinical_practice_guideline"));
        assert!(!d.blocks.is_empty());
        assert!(d.relations.iter().any(|x| x.rel == "rendition_of" && x.target.starts_with("https://app.magicapp.org/#/guideline/")));
    }

    #[test]
    fn cari_leaf_page_yields_its_embedded_pdf_and_index_pages_none() {
        let base = Url::parse("https://www.cariguidelines.org/guidelines/dialysis/vascular-access/selection-of-type-of-access/").unwrap();
        let (title, pdfs) = page_pdfs(&fx("cari-leaf.html"), &base, ".post-content").unwrap();
        assert_eq!(title.as_deref(), Some("Selection of type of access"));
        assert_eq!(pdfs, vec!["https://www.cariguidelines.org/wp-content/uploads/Dialysis/dialysis%20vascular%20access/Selection_of_access_type.pdf".to_string()]);
        assert!(page_pdfs("<html><body><p>x</p></body></html>", &base, ".post-content").is_err());
        let locs = sitemap_locs(&fx("cari-pages-sitemap.xml"));
        assert!(locs.len() > 400);
        assert!(locs.iter().any(|u| u.ends_with("/guidelines/dialysis/vascular-access/selection-of-type-of-access/")));
    }

    #[tokio::test]
    async fn cari_site_scan_keeps_scope_and_reports_pages_without_pdf() {
        let mut src = registry_source("au_nz_cari");
        src.config.as_object_mut().unwrap().remove("oa_enumeration");
        let cfg = src.config["site_pdf_pages"].clone();
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
        // two in-scope pages only: a leaf with a PDF and a topic index without one; the archive is out of scope
        let leaf = "https://www.cariguidelines.org/guidelines/dialysis/vascular-access/selection-of-type-of-access/";
        let index = "https://www.cariguidelines.org/guidelines/dialysis/vascular-access/";
        let archived = "https://www.cariguidelines.org/guidelines/archived-guidelines/dialysis-acceptance-onto-dialysis/";
        let sm = format!("<urlset><url><loc>{leaf}</loc></url><url><loc>{index}</loc></url><url><loc>{archived}</loc></url><url><loc>https://www.cariguidelines.org/about/</loc></url></urlset>");
        f.seed_cache_for_tests(cfg["sitemap"].as_str().unwrap(), sm.as_bytes(), "application/xml");
        f.seed_cache_for_tests(leaf, fx("cari-leaf.html").as_bytes(), "text/html");
        f.seed_cache_for_tests(index, b"<html><body><div class='post-content'><h1>Vascular access</h1><a href='/guidelines/dialysis/vascular-access/selection-of-type-of-access/'>x</a></div></body></html>", "text/html");
        let mut seen = HashSet::new();
        let rows = site_rows(&src, &f, &mut seen, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].url.ends_with("Selection_of_access_type.pdf"));
        assert_eq!(rows[0].extra["search_metadata"]["source_page"], leaf);
        assert_eq!(rows[0].extra["search_metadata"]["site_scan"]["pages_in_scope"], 2);
        assert_eq!(rows[0].extra["search_metadata"]["site_scan"]["pages_without_pdf"][0], index);
        assert_eq!(rows[0].doc_key, format!("site-{}", common::short_hash(&rows[0].url)));
    }

    #[tokio::test]
    async fn oa_enumeration_rows_follow_seed_and_magic_rows() {
        let mut src = registry_source("ada_dental");
        let q = src.config["oa_enumeration"]["query"].as_str().unwrap().to_string();
        src.config["oa_enumeration"]["max"] = json!(10);
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
        f.seed_cache_for_tests(&magic_list_url("upsdm"), fx("magic-upsdm.json").as_bytes(), "application/json");
        // Europe PMC answer shape with the crosswalk's own OA JATS fixture standing in for the hit
        let answer = json!({"hitCount": 1, "nextCursorMark": "*", "resultList": {"result": [
            {"id":"35647308","source":"MED","pmid":"35647308","pmcid":"PMC9134774","doi":"10.1177/23969873221078696","title":"European Stroke Organisation guidelines on stroke in women","isOpenAccess":"Y","inPMC":"Y","firstPublicationDate":"2022-03-29"}]}});
        f.seed_cache_for_tests(&cw::epmc_search_url(&q, 10, "*"), answer.to_string().as_bytes(), "application/json");
        if let Some(cq) = src.config["oa_enumeration"]["coverage_query"].as_str() {
            f.seed_cache_for_tests(&cw::epmc_search_url(cq, 1, "*"), json!({"hitCount": 12, "resultList": {"result": []}}).to_string().as_bytes(), "application/json");
        }
        f.seed_cache_for_tests(&cw::oai_url("PMC9134774").unwrap(), std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/pmc_crosswalk/oai-PMC9134774.xml")).unwrap().as_bytes(), "application/xml");
        let rows = H2StaticCatalog.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].doc_key, "oral-cancer-11365-v4");
        assert!(rows[1].doc_key.starts_with("magic-10794-"));
        assert!(cw::is_crosswalk_row(&rows[2]) && rows[2].doc_key.starts_with("ada_dental-oa-pmc9134774-"));
        let d = H2StaticCatalog.extract(&src, &f, &rows[2]).await.unwrap();
        assert!(d.relations.iter().any(|r| r.rel == "rendition_of" && r.target == "pmc:PMC9134774"));
    }
}
