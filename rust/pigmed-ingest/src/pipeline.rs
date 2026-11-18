//! Model-agnostic ingest (backend "none"). Port of `src/pigmed/pipeline.py::run`.
//! Differences from Python, all deliberate and recorded in rows:
//!  * articles are fetched/processed concurrently, committed strictly in manifest order;
//!  * content failures (bad XML, undecodable image, >max_pages PDF, 4xx on a listed asset) become
//!    audited rows/entries instead of aborting the whole partition (`--strict` restores abort);
//!  * network exhaustion / 5xx / IO errors still abort so a restart retries them.
use crate::bundle::{Budget, BundleWriter, ExclusiveFile};
use crate::common::*;
use crate::jats::{parse_jats, resolve_asset, text_units};
use crate::media::{image_view, media_kind, render_pdf};
use crate::net::{s3_url, Http, TooLarge};
use crate::pubmed;
use anyhow::{anyhow, bail, Context, Result};
use futures::{stream, StreamExt};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Python `RunConfig` defaults with Python JSON types (so run_id hashes match).
fn defaults() -> Map<String, Value> {
    let v = json!({
        "backend": "none", "model_id": null, "revision": null, "device": "cpu", "dimension": null,
        "modalities": "vision", "batch_size": 1, "chunk_tokens": 512, "overlap_tokens": 64,
        "tokenizer_id": null, "tokenizer_revision": null, "render_pages": false, "figures": true,
        "image_caption": false, "video": false, "audio": false, "archive_media": true, "longest_side": 2048,
        "max_pages": 200, "video_seconds": 20, "audio_seconds": 30, "media_overlap": 2,
        "max_asset_bytes": 134217728u64, "max_article_bytes": 2147483648u64, "target_shard_bytes": 268435456u64,
        "articles_per_shard": 100, "outbox_limit_bytes": 214748364800u64, "contact": "",
        "license_policy": "redistributable"
    });
    v.as_object().unwrap().clone()
}

#[derive(Clone)]
pub struct Cfg {
    pub raw: Map<String, Value>,
    pub render_pages: bool,
    pub figures: bool,
    pub image_caption: bool,
    pub archive_media: bool,
    pub video: bool,
    pub audio: bool,
    pub video_seconds: f64,
    pub audio_seconds: f64,
    pub media_overlap: f64,
    pub longest_side: u32,
    pub max_pages: usize,
    pub chunk_tokens: usize,
    pub overlap_tokens: usize,
    pub max_asset_bytes: u64,
    pub max_article_bytes: u64,
    pub target_shard_bytes: u64,
    pub articles_per_shard: usize,
    pub outbox_limit_bytes: u64,
    /// "redistributable" (CC0/CC-BY only, v0.1 default), "commercial" (CC0/CC-BY/CC-BY-SA/CC-BY-ND) or "all"
    /// (ingest every licence; filter downstream).
    pub license_policy: String,
    /// Rust-only extension keys (absent from Python RunConfig, so they are only hashed into run_id when set).
    /// `supplements`: "none" (default) | "prose" | "prose+tables".
    pub supplements: String,
    pub supplement_max_bytes: u64,
    pub table_card_words: usize,
    pub supplement_max_words: usize,
    pub supplement_archive_bytes: u64,
    pub supplement_concurrency: usize,
    /// OCR at most this many image-only pages per supplementary PDF (0 = off). Needs `tesseract`.
    pub supplement_ocr_pages: usize,
    /// Do not download supplementary PDFs: their media row gets supplement status "skipped-pdf" (held out for a
    /// later PDF pass). Article PDFs are only fetched with render_pages.
    pub supplement_skip_pdf: bool,
}

const RUST_ONLY: &[&str] = &["supplements", "chunk_recipe", "chunk_target", "chunk_min", "chunk_max", "chunk_overlap", "chunk_heading_prefix", "supplement_max_bytes", "table_card_words", "supplement_max_words", "supplement_archive_bytes", "supplement_concurrency", "supplement_ocr_pages", "supplement_skip_pdf"];

pub fn load_config(path: Option<&Path>, overrides: &Map<String, Value>) -> Result<Cfg> {
    let mut raw = defaults();
    if let Some(p) = path {
        let file: Value = serde_json::from_slice(&fs::read(p)?)?;
        for (k, v) in file.as_object().context("config must be an object")? {
            if !raw.contains_key(k) && !RUST_ONLY.contains(&k.as_str()) {
                bail!("Unknown config field {k:?} (RunConfig.__init__ would reject it)");
            }
            raw.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in overrides {
        raw.insert(k.clone(), v.clone());
    }
    let u = |k: &str| raw[k].as_u64().with_context(|| format!("{k} must be a non-negative integer"));
    let b = |k: &str| raw[k].as_bool().with_context(|| format!("{k} must be bool"));
    if raw["backend"] != "none" {
        bail!("pigmed-rs ingest is model-agnostic (backend=none); embed bundles afterwards");
    }
    let f = |k: &str| raw[k].as_f64().with_context(|| format!("{k} must be a number"));
    let opt_u = |k: &str, d: u64| -> Result<u64> {
        raw.get(k).map_or(Ok(d), |v| v.as_u64().with_context(|| format!("{k} must be a non-negative integer")))
    };
    let c = Cfg {
        supplements: raw.get("supplements").and_then(|v| v.as_str()).unwrap_or("none").to_string(),
        supplement_max_bytes: opt_u("supplement_max_bytes", 256 << 20)?,
        table_card_words: opt_u("table_card_words", 2000)? as usize,
        supplement_max_words: opt_u("supplement_max_words", 50_000)? as usize,
        supplement_archive_bytes: opt_u("supplement_archive_bytes", 4 << 20)?,
        supplement_concurrency: opt_u("supplement_concurrency", 6)? as usize,
        supplement_ocr_pages: opt_u("supplement_ocr_pages", 0)? as usize,
        supplement_skip_pdf: match raw.get("supplement_skip_pdf") {
            None => false,
            Some(v) => v.as_bool().context("supplement_skip_pdf must be bool")?,
        },
        render_pages: b("render_pages")?, figures: b("figures")?, image_caption: b("image_caption")?,
        archive_media: b("archive_media")?, video: b("video")?, audio: b("audio")?,
        video_seconds: f("video_seconds")?, audio_seconds: f("audio_seconds")?, media_overlap: f("media_overlap")?,
        longest_side: u("longest_side")? as u32, max_pages: u("max_pages")? as usize,
        chunk_tokens: u("chunk_tokens")? as usize, overlap_tokens: u("overlap_tokens")? as usize,
        max_asset_bytes: u("max_asset_bytes")?, max_article_bytes: u("max_article_bytes")?,
        target_shard_bytes: u("target_shard_bytes")?, articles_per_shard: u("articles_per_shard")? as usize,
        outbox_limit_bytes: u("outbox_limit_bytes")?,
        license_policy: raw["license_policy"].as_str().unwrap_or("").to_string(), raw: raw.clone(),
    };
    if !matches!(c.license_policy.as_str(), "redistributable" | "commercial" | "all") {
        bail!("license_policy must be \"redistributable\", \"commercial\" or \"all\"");
    }
    if !matches!(c.supplements.as_str(), "none" | "prose" | "prose+tables") {
        bail!("supplements must be \"none\", \"prose\" or \"prose+tables\"");
    }
    if c.video && c.video_seconds > 32.0 {
        bail!("v0.1 video windows limited to 32 seconds to avoid default frame-budget truncation");
    }
    if c.articles_per_shard < 1 || c.overlap_tokens >= c.chunk_tokens {
        bail!("Invalid articles_per_shard/overlap");
    }
    if c.target_shard_bytes + c.max_article_bytes >= c.outbox_limit_bytes {
        bail!("Outbox must hold at least one full shard reservation");
    }
    if c.max_asset_bytes > c.max_article_bytes {
        bail!("An asset cannot exceed the article workspace budget");
    }
    Ok(c)
}

enum Fail {
    Content(String),
    Fatal(anyhow::Error),
}

fn classify(e: anyhow::Error) -> Fail {
    if e.downcast_ref::<TooLarge>().is_some() {
        return Fail::Content(format!("{e:#}"));
    }
    let s = format!("{e:#}");
    let fatal_net = s.contains("Retry budget exhausted") || s.contains("retry budget exhausted") || s.contains("HTTP 5");
    if s.contains("MD5 mismatch") || s.contains("HTTP 4") {
        return Fail::Content(s);
    }
    if fatal_net {
        Fail::Fatal(e)
    } else {
        Fail::Content(s)
    }
}

struct Done {
    rows: Vec<Map<String, Value>>,
    canonical: Option<(String, Vec<u8>, String, PathBuf)>, // (article json member, bytes, xml member, xml path)
    assets: Vec<(String, PathBuf)>,
    _scratch: tempfile::TempDir,
    seconds: f64,
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

async fn process(source: Value, cfg: Arc<Cfg>, http: Http, strict: bool) -> Result<Done> {
    let t0 = Instant::now();
    let scratch = tempfile::Builder::new().prefix("pigmed-article-").tempdir()?;
    let dir = scratch.path().to_path_buf();
    let planned_etag = source["etag"].as_str().unwrap_or("").to_string();
    let md5 = (planned_etag.len() == 32 && planned_etag.chars().all(|c| c.is_ascii_hexdigit())).then(|| planned_etag.clone());
    let meta_path = dir.join("metadata.json");
    let key = source["key"].as_str().context("plan row without key")?;
    // PMC updates metadata in place (licence, retraction flags) after the daily inventory. Take the
    // current object (identity is still checked below) and record the drift on every row.
    let mut etag = planned_etag.clone();
    let mut drift = false;
    match http.download(&s3_url(key), &meta_path, 16 << 20, md5.as_deref()).await {
        Ok(_) => {}
        Err(e) if format!("{e:#}").contains("MD5 mismatch") => {
            let d = http.download(&s3_url(key), &meta_path, 16 << 20, None).await?;
            etag = d.md5;
            drift = true;
        }
        // Removed/withdrawn after the plan was made: audit it, don't abort the partition.
        Err(e) if !strict && (format!("{e:#}").contains("HTTP 404") || format!("{e:#}").contains("HTTP 403")) => {
            let aid = format!("{}.{}", source["pmcid"].as_str().unwrap_or("?"), source["version"]);
            let mut done = Done { rows: vec![], canonical: None, assets: vec![], _scratch: scratch, seconds: 0.0 };
            done.rows.push(obj(json!({"unit_id": stable_id(&[source.clone(), json!("source-missing")]), "kind": "audit",
                "article_id": aid, "status": "source-missing", "error": format!("{e:#}"),
                "export_approved": true, "contains_article_content": false})));
            done.seconds = t0.elapsed().as_secs_f64();
            return Ok(done);
        }
        Err(e) => return Err(e),
    }
    let metadata: Value = serde_json::from_slice(&fs::read(&meta_path)?)?;
    let mut actual = match &metadata["pmcid"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    };
    if !actual.starts_with("PMC") {
        actual = format!("PMC{actual}");
    }
    let mver = metadata["version"].as_i64().unwrap_or(1);
    if Some(actual.as_str()) != source["pmcid"].as_str() || Some(mver) != source["version"].as_i64() {
        bail!("Metadata identity does not match immutable inventory plan");
    }
    let article_id = format!("{}.{}", source["pmcid"].as_str().unwrap(), source["version"]);
    let mut done = Done { rows: vec![], canonical: None, assets: vec![], _scratch: scratch, seconds: 0.0 };
    // Every row carries the article's licence (+ `redistributable` = CC0/CC-BY) so users can filter downstream.
    let license = normal_license(metadata["license_code"].as_str());
    let redistributable = allowed_license(metadata["license_code"].as_str());
    let excluded = match cfg.license_policy.as_str() {
        "redistributable" => !redistributable,
        "commercial" => !crate::common::commercial_license(metadata["license_code"].as_str()),
        _ => false,
    };
    if excluded {
        done.rows.push(obj(json!({"unit_id": stable_id(&[source.clone(), json!("license-skip")]), "kind": "audit",
            "article_id": article_id, "status": "excluded-by-license-policy", "license": license,
            "license_code": metadata["license_code"], "redistributable": false,
            "export_approved": true, "contains_article_content": false})));
    } else {
        match views(&source, &metadata, &dir, &cfg, &http, &etag, redistributable, &mut done).await {
        Ok(()) => {}
        Err(e) => match classify(e) {
            Fail::Fatal(e) => return Err(e),
            Fail::Content(msg) if strict => bail!("{article_id}: {msg}"),
            Fail::Content(msg) => {
                done.rows.clear();
                done.assets.clear();
                done.canonical = None;
                done.rows.push(obj(json!({"unit_id": stable_id(&[source.clone(), json!("ingest-error")]), "kind": "audit",
                    "article_id": article_id, "status": "ingest-error", "error": msg,
                    "license": license, "license_code": metadata["license_code"], "redistributable": redistributable,
                    "export_approved": true, "contains_article_content": false})));
            }
        },
        }
    }
    // Optional chunk recipe on the body text (Rust-only config key; absent = the per-section split above).
    match cfg.raw.get("chunk_recipe").and_then(|v| v.as_str()) {
        None | Some("sections") => {}
        Some("balanced-c") => {
            let d = crate::chunk::Recipe::default();
            let n = |k: &str, dv: usize| cfg.raw.get(k).and_then(|v| v.as_u64()).map(|x| x as usize).unwrap_or(dv);
            let r = crate::chunk::Recipe { target: n("chunk_target", d.target), min: n("chunk_min", d.min), max: n("chunk_max", d.max),
                overlap: n("chunk_overlap", d.overlap),
                heading_prefix: cfg.raw.get("chunk_heading_prefix").and_then(|v| v.as_bool()).unwrap_or(false) };
            let rows = std::mem::take(&mut done.rows);
            done.rows = crate::chunk::rechunk_rows(rows, r).0;
        }
        Some(o) => bail!("unknown chunk_recipe {o:?}"),
    }
    if drift {
        for r in done.rows.iter_mut() {
            r.insert("source_drift".into(), json!(true));
            r.insert("planned_etag".into(), json!(planned_etag));
            r.insert("source_etag".into(), json!(etag));
        }
    }
    done.seconds = t0.elapsed().as_secs_f64();
    Ok(done)
}

fn suffix_of(url: &str) -> String {
    let file = crate::jats::url_basename(url);
    match file.rfind('.') {
        Some(i) if i > 0 => file[i..].to_string(),
        _ => String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn views(source: &Value, metadata: &Value, dir: &Path, cfg: &Cfg, http: &Http, etag: &str, redistributable: bool, done: &mut Done) -> Result<()> {
    let xml_path = dir.join("article.xml");
    let xml_url = metadata["xml_url"].as_str().context("metadata without xml_url")?;
    let xml_info = http.download(xml_url, &xml_path, cfg.max_asset_bytes, None).await?;
    let bytes = fs::read(&xml_path)?;
    let meta = metadata.clone();
    let mut article = tokio::task::spawn_blocking(move || parse_jats(&bytes, &meta)).await??;
    let urls: Vec<String> = match &metadata["media_urls"] {
        Value::Null => vec![],
        Value::Array(a) => a.iter().map(|u| u.as_str().map(String::from).ok_or_else(|| anyhow!("PMC media_urls schema changed; refusing to guess"))).collect::<Result<_>>()?,
        _ => bail!("PMC media_urls schema changed; refusing to guess"),
    };
    let mut media_audit: Vec<Value> = vec![];
    let inventory: Vec<Value> = urls.iter().map(|u| json!({"url": u, "kind": media_kind(u)})).collect();
    let mut units: Vec<Value> = text_units(&article, cfg.chunk_tokens, cfg.overlap_tokens);
    let mut media: Vec<(String, PathBuf)> = vec![]; // media_key -> view path
    let guard = |d: &Path| -> Result<()> {
        if tree_bytes(d) > cfg.max_article_bytes {
            return Err(TooLarge("Article workspace budget exceeded; adjust limits explicitly or quarantine this article".into()).into());
        }
        Ok(())
    };

    // Figures/pages and supplements are independent downloads + conversions: run them concurrently.
    let media_fut = async {
        if cfg.figures {
            for fig in &article.figures {
                if fig.rights_review_required {
                    media_audit.push(json!({"figure": fig.figure_id, "status": "rights-review-required"}));
                    continue;
                }
                for href in &fig.hrefs {
                    let Some(url) = resolve_asset(href, &urls).filter(|u| media_kind(u) == "image") else {
                        media_audit.push(json!({"figure": fig.figure_id, "href": href, "status": "unresolved-or-unsupported"}));
                        continue;
                    };
                    let key = stable_id(&[json!(url)]);
                    let original = dir.join(format!("{key}{}", suffix_of(url)));
                    let info = match http.download(url, &original, cfg.max_asset_bytes, None).await {
                        Ok(i) => i,
                        Err(e) => match classify(e) {
                            Fail::Fatal(e) => return Err(e),
                            Fail::Content(m) => {
                                media_audit.push(json!({"figure": fig.figure_id, "href": href, "status": "download-failed", "error": m}));
                                continue;
                            }
                        },
                    };
                    let view = dir.join(format!("{key}.png"));
                    let (o, v, longest) = (original.clone(), view.clone(), cfg.longest_side);
                    let transform = match tokio::task::spawn_blocking(move || image_view(&o, &v, longest, 178_956_970)).await? {
                        Ok(t) => t,
                        Err(e) => {
                            media_audit.push(json!({"figure": fig.figure_id, "href": href, "status": "decode-failed", "error": format!("{e:#}")}));
                            let _ = fs::remove_file(&original);
                            continue;
                        }
                    };
                    let _ = fs::remove_file(&original); // the archived view replaces the source bytes in the workspace
                    media.push((key.clone(), view.clone()));
                    units.push(json!({"unit_id": stable_id(&[json!(article.article_id), json!("figure"), json!(fig.figure_id), json!(key), transform.clone()]),
                        "kind": "figure", "text": if cfg.image_caption { fig.caption.clone() } else { String::new() },
                        "caption": fig.caption, "media_path": view.to_string_lossy(), "media_key": key,
                        "source_url": url, "source_sha256": info.sha256,
                        "locator": {"figure_id": fig.figure_id, "href": href}, "transform": transform}));
                    guard(dir)?;
                }
            }
        }
        if cfg.render_pages {
            if let Some(pdf_url) = metadata["pdf_url"].as_str() {
                let pdf = dir.join("article.pdf");
                match http.download(pdf_url, &pdf, cfg.max_asset_bytes, None).await {
                    Err(e) => match classify(e) {
                        Fail::Fatal(e) => return Err(e),
                        Fail::Content(m) => media_audit.push(json!({"pdf": pdf_url, "status": "download-failed", "error": m})),
                    },
                    Ok(info) => {
                        let (p, out, l, mp) = (pdf.clone(), dir.join("pages"), cfg.longest_side, cfg.max_pages);
                        match tokio::task::spawn_blocking(move || render_pdf(&p, &out, l, mp)).await? {
                            Err(e) => media_audit.push(json!({"pdf": pdf_url, "status": "render-failed", "error": format!("{e:#}")})),
                            Ok(pages) => {
                                for pg in pages {
                                    let key = stable_id(&[json!(info.sha256), pg.locator.clone()]);
                                    media.push((key.clone(), pg.path.clone()));
                                    units.push(json!({"unit_id": stable_id(&[json!(article.article_id), json!("page"), json!(key)]),
                                        "kind": "page", "text": "", "media_path": pg.path.to_string_lossy(), "media_key": key,
                                        "source_url": pdf_url, "source_sha256": info.sha256, "locator": pg.locator}));
                                }
                            }
                        }
                        let _ = fs::remove_file(&pdf);
                        guard(dir)?;
                    }
                }
            }
        }
        // Video/audio (port of pipeline.py): probe, then cut fixed windows with ffmpeg. Each clip is a unit.
        for url in &urls {
            let kind = media_kind(url);
            if !((kind == "video" && (cfg.video || cfg.audio)) || (kind == "audio" && cfg.audio)) {
                continue;
            }
            let original = dir.join(format!("{}{}", stable_id(&[json!(url)]), suffix_of(url)));
            let info = match http.download(url, &original, cfg.max_asset_bytes, None).await {
                Ok(i) => i,
                Err(e) => match classify(e) {
                    Fail::Fatal(e) => return Err(e),
                    Fail::Content(m) => {
                        media_audit.push(json!({"url": url, "status": "download-failed", "error": m}));
                        continue;
                    }
                },
            };
            let o = original.clone();
            let (duration, has_audio, has_video) = match tokio::task::spawn_blocking(move || crate::media::probe_media(&o)).await? {
                Ok(p) => p,
                Err(e) => {
                    media_audit.push(json!({"url": url, "status": "probe-failed", "error": format!("{e:#}")}));
                    let _ = fs::remove_file(&original);
                    continue;
                }
            };
            media_audit.push(json!({"url": url, "duration": duration, "has_audio": has_audio, "has_video": has_video}));
            let mut modes: Vec<&str> = vec![];
            if kind == "video" && cfg.video {
                modes.push("video");
            }
            if cfg.audio && has_audio {
                modes.push("audio");
            }
            for mode in modes {
                let seconds = if mode == "video" { cfg.video_seconds } else { cfg.audio_seconds };
                for (start, end) in crate::media::time_windows(duration, seconds, cfg.media_overlap)? {
                    let key = stable_id(&[json!(url), json!(mode), json!(start), json!(end)]);
                    let view = dir.join(format!("{key}{}", if mode == "video" { ".mp4" } else { ".wav" }));
                    let (o, v, m) = (original.clone(), view.clone(), mode.to_string());
                    let permit = cpu_gate().acquire_owned().await.map_err(|e| anyhow!("cpu gate closed: {e}"))?;
                    let clip = tokio::task::spawn_blocking(move || {
                        let _p = permit;
                        crate::media::make_clip(&o, &v, start, end, &m)
                    })
                    .await?;
                    if let Err(e) = clip {
                        media_audit.push(json!({"url": url, "status": "clip-failed", "mode": mode, "start_seconds": start, "error": format!("{e:#}")}));
                        continue;
                    }
                    media.push((key.clone(), view.clone()));
                    units.push(json!({"unit_id": stable_id(&[json!(article.article_id), json!(mode), json!(key)]), "kind": mode,
                        "text": "", "media_path": view.to_string_lossy(), "media_key": key, "source_url": url,
                        "source_sha256": info.sha256, "locator": {"start_seconds": start, "end_seconds": end},
                        "has_soundtrack": has_audio, "audio_included": mode == "audio"}));
                    guard(dir)?;
                }
            }
            let _ = fs::remove_file(&original);
        }
        Ok::<(), anyhow::Error>(())
    };
    let supp_fut = async {
        if cfg.supplements == "none" {
            return Ok(SuppOut::default());
        }
        supplement_views(&article, &urls, dir, cfg, http).await
    };
    let (media_res, supp_res) = tokio::join!(media_fut, supp_fut);
    media_res?;
    let supp = supp_res?;
    units.extend(supp.units);
    done.assets.extend(supp.assets);
    let supp_status = supp.status;

    article.json.insert("xml_sha256".into(), json!(xml_info.sha256));
    article.json.insert("media_audit".into(), Value::Array(media_audit));
    article.json.insert("media_inventory".into(), Value::Array(inventory.clone()));
    let aj = &article.json;
    let mut base = Map::new();
    for k in ["article_id", "pmcid", "pmid", "doi", "version", "title", "license", "is_retracted"] {
        base.insert(k.into(), aj[k].clone());
    }
    base.insert("license_code".into(), metadata["license_code"].clone());
    base.insert("redistributable".into(), json!(redistributable));
    // Mixed licences are publishable (user decision 2026-11-12); filter downstream on `redistributable`.
    base.insert("export_approved".into(), json!(true));
    base.insert("source_etag".into(), json!(etag));
    base.insert("snapshot".into(), source["snapshot"].clone());
    base.insert("xml_sha256".into(), json!(xml_info.sha256));
    let aid = article.article_id.clone();
    let canonical_member = format!("{aid}/article.json");
    let xml_member = format!("{aid}/source.xml");
    done.canonical = Some((canonical_member.clone(), py_default_json(&Value::Object(article.json.clone())), xml_member.clone(), xml_path));
    let count = |k: &str| inventory.iter().filter(|x| x["kind"] == k).count();
    let mut arow = base.clone();
    arow.insert("unit_id".into(), json!(stable_id(&[json!(aid), json!(xml_info.sha256), json!("canonical")])));
    arow.insert("kind".into(), json!("article"));
    arow.insert("canonical_member".into(), json!(canonical_member));
    arow.insert("xml_member".into(), json!(xml_member));
    arow.insert("media_counts".into(), json!({"image": count("image"), "video": count("video"), "audio": count("audio"), "other": count("other")}));

    // One `media` row per file the article ships (article XML/PDF/plain text + every media_urls entry),
    // classified by modality, plus a modality summary on the article row. No extra downloads.
    let mut files: Vec<(String, &str)> = vec![];
    for (field, role) in [("xml_url", "article_xml"), ("pdf_url", "article_pdf"), ("text_url", "article_text")] {
        if let Some(u) = metadata[field].as_str() {
            files.push((u.to_string(), role));
        }
    }
    files.extend(urls.iter().map(|u| (u.clone(), "media_file")));
    let viewed: std::collections::HashSet<&str> = units.iter().filter(|u| u["kind"] == "figure").filter_map(|u| u["source_url"].as_str()).collect();
    let mut modality_counts = Map::new();
    let mut media_rows = Vec::new();
    for (u, role) in &files {
        let m = if *role == "article_xml" { "structured_text" } else { crate::media::modality(u) };
        if *role == "media_file" {
            let c = modality_counts.entry(m.to_string()).or_insert(json!(0));
            *c = json!(c.as_u64().unwrap_or(0) + 1);
        }
        let mut r = base.clone();
        r.insert("unit_id".into(), json!(stable_id(&[json!(aid), json!("media"), json!(u)])));
        r.insert("kind".into(), json!("media"));
        r.insert("role".into(), json!(role));
        r.insert("modality".into(), json!(m));
        r.insert("extension".into(), json!(crate::media::extension(u)));
        r.insert("file_name".into(), json!(crate::jats::url_basename(u)));
        r.insert("source_url".into(), json!(u));
        r.insert("source_md5".into(), json!(crate::net::expected_md5(&crate::net::https_url(u).unwrap_or_default()).ok().flatten()));
        r.insert("embedded_as_view".into(), json!(viewed.contains(u.as_str())));
        if let Some(st) = supp_status.get(u.as_str()) {
            r.insert("embedded_as_text".into(), json!(st["status"] == "ok" && st["units"].as_u64().unwrap_or(0) > 0));
            r.insert("supplement".into(), st.clone());
        }
        media_rows.push(r);
    }
    let n = |k: &str| units.iter().filter(|u| u["kind"] == k).count();
    let mut modalities: Vec<String> = vec![];
    for (name, present) in [("text", n("text") > 0), ("table", n("table_text") > 0), ("figure", !article.figures.is_empty()),
                            ("pdf", metadata["pdf_url"].is_string()),
                            ("supplement_text", n("supplement_text") > 0), ("table_card", n("table_card") > 0)] {
        if present {
            modalities.push(name.into());
        }
    }
    for k in modality_counts.keys() {
        if !modalities.contains(k) {
            modalities.push(k.clone());
        }
    }
    arow.insert("modalities".into(), json!(modalities));
    arow.insert("modality_counts".into(), Value::Object(modality_counts));
    arow.insert("unit_counts".into(), json!({"text": n("text"), "table_text": n("table_text"), "caption": n("caption"),
        "figure": n("figure"), "page": n("page"),
        "supplement_text": n("supplement_text"), "table_card": n("table_card"), "figures_in_xml": article.figures.len(), "media_files": urls.len()}));
    done.rows.push(arow);
    done.rows.extend(media_rows);
    for u in units {
        let mut row = base.clone();
        for (k, v) in obj(u) {
            row.insert(k, v);
        }
        if cfg.archive_media {
            if let Some(key) = row.get("media_key").and_then(|k| k.as_str()).map(String::from) {
                let p = media.iter().find(|(k, _)| *k == key).map(|(_, p)| p.clone()).context("missing media view")?;
                let member = format!("{aid}/media/{key}{}", p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default());
                row.insert("asset_member".into(), json!(member));
                done.assets.push((member, p));
            }
        }
        done.rows.push(row);
    }
    Ok(())
}

fn tesseract_available() -> bool {
    std::process::Command::new("tesseract").arg("--version").stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// Process-wide slots for CPU-bound conversions: $PIGMED_CPU_SLOTS, else the CPUs this process may use.
/// Set it when several ingest processes share a node (e.g. cores / processes).
fn cpu_gate() -> Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| {
        let n = std::env::var("PIGMED_CPU_SLOTS").ok().and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0)
            .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));
        Arc::new(tokio::sync::Semaphore::new(n))
    })
    .clone()
}

/// Result of one supplementary file: status for its media row, units, optional archived Markdown.
type SuppResult = (String, Value, Vec<Value>, Option<(String, PathBuf)>);

/// Download + convert one supplementary file. Content problems become a status; only fatal network
/// errors (retry budget exhausted, 5xx) are returned as Err so the partition restarts.
#[allow(clippy::too_many_arguments)]
async fn one_supplement(article_id: String, url: String, ext: String, mut class: crate::supplements::Class,
                        label: String, caption: String, dir: PathBuf, cfg: &Cfg, http: &Http) -> Result<Option<SuppResult>> {
    use crate::supplements::{looks_tabular, Class};
    let key = stable_id(&[json!(url)]);
    let path = dir.join(format!("supp-{key}{ext}"));
    let t0 = Instant::now();
    // PMC's published MD5 is stale for some supplement files (e.g. every `*_index.html` in s1000):
    // two identical mismatches mean the object changed, not a corrupt transfer. Keep the bytes we
    // actually got and record the mismatch rather than dropping the file.
    let mut md5_mismatch = false;
    let first = http.download(&url, &path, cfg.supplement_max_bytes, None).await;
    let first = match first {
        Err(e) if format!("{e:#}").contains("MD5 mismatch") => {
            md5_mismatch = true;
            http.download(&url, &path, cfg.supplement_max_bytes, Some("")).await
        }
        r => r,
    };
    let info = match first {
        Ok(i) => i,
        Err(e) => {
            let too_large = e.downcast_ref::<TooLarge>().is_some();
            return match classify(e) {
                Fail::Fatal(e) => Err(e),
                Fail::Content(m) => Ok(Some((url, json!({"status": if too_large {"too-large"} else {"download-failed"},
                    "error": m.chars().take(300).collect::<String>()}), vec![], None))),
            };
        }
    };
    let download_ms = t0.elapsed().as_secs_f64() * 1000.0;
    // Conversion is CPU-bound: at most one per core process-wide, so many articles in flight never
    // oversubscribe the node. The permit lives inside the blocking job, so it is held until the
    // converter really returns (even past the timeout below).
    let tq = Instant::now();
    let permit = cpu_gate().acquire_owned().await.map_err(|e| anyhow!("cpu gate closed: {e}"))?;
    let queue_ms = tq.elapsed().as_secs_f64() * 1000.0;
    let t1 = Instant::now();
    let (chunk, overlap, card, maxw, archive_cap, tables) = (cfg.chunk_tokens, cfg.overlap_tokens, cfg.table_card_words,
        cfg.supplement_max_words, cfg.supplement_archive_bytes, cfg.supplements == "prose+tables");
    let opts = crate::convert::Opts { ocr_pages: cfg.supplement_ocr_pages };
    let (p, e2, u2, sha, aid, k2, d2) = (path.clone(), ext.clone(), url.clone(), info.sha256.clone(), article_id.clone(), key.clone(), dir.clone());
    // Conversion, chunking and the Markdown write all run off the async threads.
    let job = tokio::task::spawn_blocking(move || -> std::result::Result<(Value, Vec<Value>, Option<(String, PathBuf)>), Value> {
        let _permit = permit;
        let conv = crate::convert::convert(&p, &e2, opts)
            .map_err(|m| json!({"status": "convert-failed", "error": m.chars().take(300).collect::<String>()}))?;
        let _ = fs::remove_file(&p);
        let (md, pdf_type, tool) = (conv.md, conv.pdf_type, conv.tool);
        let detail = json!({"detected_format": conv.detected, "pages": conv.pages, "ocr_pages": conv.ocr_pages,
                            "pages_without_text": conv.pages_without_text});
        if md.split_whitespace().next().is_none() {
            let note = if conv.pages_without_text > 0 { "image-only pages; OCR off or capped" } else { "no extractable text" };
            let mut st = json!({"status": "no-text", "tool": tool, "pdf_type": pdf_type, "note": note});
            for (k, v) in detail.as_object().unwrap() { st[k] = v.clone(); }
            return Err(st);
        }
        if e2 == ".txt" && looks_tabular(&md) {
            class = Class::Table;
            if !tables {
                return Err(json!({"status": "skipped-class", "class": "table"}));
            }
        }
        let (mut units, mut st) = crate::supplements::units(&aid, &u2, &sha, class, &md, &label, &caption, chunk, overlap, card, maxw);
        let md_asset = if (md.len() as u64) <= archive_cap {
            // not supp-{key}.md: a Markdown supplement is downloaded to exactly that path and removed afterwards
            let mdp = d2.join(format!("supp-{k2}.archive.md"));
            fs::write(&mdp, md.as_bytes()).map_err(|e| json!({"status": "convert-failed", "error": e.to_string()}))?;
            Some((format!("{aid}/supplements/{k2}.md"), mdp))
        } else {
            None
        };
        let member = md_asset.as_ref().map(|(m, _)| m.clone());
        for u in units.iter_mut() {
            u.as_object_mut().unwrap().insert("markdown_member".into(), json!(member));
        }
        let o = st.as_object_mut().unwrap();
        o.insert("status".into(), json!("ok"));
        o.insert("tool".into(), json!(tool));
        o.insert("pdf_type".into(), json!(pdf_type));
        o.insert("markdown_bytes".into(), json!(md.len()));
        o.insert("markdown_member".into(), json!(member));
        for (k, v) in detail.as_object().unwrap() {
            o.insert(k.clone(), v.clone());
        }
        Ok((st, units, md_asset))
    });
    let res = tokio::time::timeout(std::time::Duration::from_secs(120), job).await;
    let _ = fs::remove_file(&path);
    let convert_ms = t1.elapsed().as_secs_f64() * 1000.0;
    let (mut st, units, md_asset) = match res {
        Ok(Ok(Ok(x))) => x,
        Ok(Ok(Err(st))) => (st, vec![], None),
        _ => (json!({"status": "convert-failed", "error": "conversion timed out or crashed"}), vec![], None),
    };
    let o = st.as_object_mut().unwrap();
    o.insert("bytes".into(), json!(info.bytes));
    o.insert("download_ms".into(), json!(download_ms.round()));
    o.insert("convert_ms".into(), json!(convert_ms.round()));
    o.insert("cpu_queue_ms".into(), json!(queue_ms.round()));
    if md5_mismatch {
        o.insert("md5_mismatch".into(), json!(true));
    }
    Ok(Some((url, st, units, md_asset)))
}

/// Download + convert supplementary files into `supplement_text` / `table_card` units, up to
/// `supplement_concurrency` files at once per article (the process-wide HTTP limit still applies).
/// Results are applied in `media_urls` order so bundles stay deterministic. Every file considered gets a
/// status (ok / too-large / download-failed / convert-failed / no-text / skipped-class) on its `media` row.
#[derive(Default)]
struct SuppOut {
    units: Vec<Value>,
    status: std::collections::HashMap<String, Value>,
    assets: Vec<(String, PathBuf)>,
}

async fn supplement_views(article: &crate::jats::Article, urls: &[String], dir: &Path, cfg: &Cfg, http: &Http) -> Result<SuppOut> {
    use crate::supplements::{caption_for, class_of, Class};
    let mut out = SuppOut::default();
    let mut jobs = vec![];
    for url in urls {
        let ext = crate::media::extension(url);
        let Some(class) = class_of(&ext) else { continue };
        if cfg.supplement_skip_pdf && ext.trim_start_matches('.').eq_ignore_ascii_case("pdf") {
            out.status.insert(url.clone(), json!({"status": "skipped-pdf"}));
            continue;
        }
        if class == Class::Table && cfg.supplements != "prose+tables" {
            out.status.insert(url.clone(), json!({"status": "skipped-class", "class": "table"}));
            continue;
        }
        let (label, caption) = caption_for(url, &article.supplements);
        jobs.push(one_supplement(article.article_id.clone(), url.clone(), ext, class, label, caption, dir.to_path_buf(), cfg, http));
    }
    let results: Vec<Result<Option<SuppResult>>> = stream::iter(jobs).buffered(cfg.supplement_concurrency.max(1)).collect().await;
    for r in results {
        if let Some((url, st, new_units, md_asset)) = r? {
            out.status.insert(url, st);
            out.units.extend(new_units);
            out.assets.extend(md_asset);
        }
    }
    Ok(out)
}

fn recover_cursor(worker: &Path, root: &Path, run_id: &str, manifest_hash: &str) -> Result<u64> {
    let mut cands: Vec<Value> = vec![];
    let state = worker.join("state.json");
    if state.exists() {
        cands.push(serde_json::from_slice(&fs::read(&state)?)?);
    }
    if let Ok(rd) = fs::read_dir(worker.join("ready")) {
        for e in rd.flatten() {
            let m = e.path().join("manifest.json");
            if m.exists() {
                let v: Value = serde_json::from_slice(&fs::read(&m)?)?;
                cands.push(v["checkpoint"].clone());
            }
        }
    }
    if let Ok(rd) = fs::read_dir(root.join("receipts").join(run_id)) {
        for e in rd.flatten() {
            if e.path().extension().map_or(false, |x| x == "json") {
                let v: Value = serde_json::from_slice(&fs::read(e.path())?)?;
                cands.push(v["checkpoint"].clone());
            }
        }
    }
    Ok(cands
        .iter()
        .filter(|c| c["manifest_sha256"] == manifest_hash && c["run_id"] == run_id)
        .filter_map(|c| c["next_index"].as_u64())
        .max()
        .unwrap_or(0))
}

pub async fn run(manifest: &Path, root: &Path, cfg: Cfg, concurrency: usize, http_conc: usize, interval_ms: u64, strict: bool) -> Result<Value> {
    // OCR is best-effort per page, so a missing binary would otherwise silently drop all scanned-supplement text.
    if cfg.supplement_ocr_pages > 0 && !tesseract_available() {
        bail!("supplement_ocr_pages > 0 but `tesseract` is not runnable on PATH; install/load it or set supplement_ocr_pages to 0");
    }
    fs::create_dir_all(root)?;
    let mut semantic = cfg.raw.clone();
    for k in ["device", "batch_size", "contact", "outbox_limit_bytes"] {
        semantic.shift_remove(k);
    }
    let manifest_hash = digest_file(manifest)?;
    let run_id = stable_id(&[json!(manifest_hash), Value::Object(semantic)]);
    let worker = root.join("runs").join(&run_id);
    let ready = worker.join("ready");
    fs::create_dir_all(&ready)?;
    let budget = Budget::new(root, cfg.outbox_limit_bytes)?;
    let http = Http::new(cfg.raw["contact"].as_str().unwrap_or(""), http_conc, interval_ms)?;
    let _lock = ExclusiveFile::acquire(&worker.join("worker.lock"))?;
    for e in fs::read_dir(&ready)?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if let Some(k) = n.strip_suffix(".partial") {
            budget.release(k)?;
            fs::remove_dir_all(e.path())?;
        } else if e.path().is_dir() {
            budget.settle(&n, &e.path())?; // bundles committed by an older writer still hold worst-case reservations
        }
    }
    let cursor = recover_cursor(&worker, root, &run_id, &manifest_hash)? as usize;
    atomic_json(&worker.join("run.json"), &json!({"run_id": run_id, "config": cfg.raw, "embedding": null,
        "manifest_sha256": manifest_hash, "tokenizer": crate::jats::COUNTER_NAME, "writer": "pigmed-rs"}))?;
    let rows = read_jsonl(manifest)?;
    if rows.first().map_or(false, |r| r["source"] == "pubmed") {
        return pubmed::run(&rows, &worker, &run_id, &manifest_hash, cursor, &cfg, &budget, &http).await;
    }
    let t0 = Instant::now();
    let cfg = Arc::new(cfg);
    let mut writer: Option<BundleWriter> = None;
    let mut start_cursor = cursor;
    let (mut processed, mut errors, mut article_seconds) = (0usize, 0usize, 0f64);
    let finish = |writer: &mut Option<BundleWriter>, start: &mut usize, next: usize| -> Result<()> {
        if let Some(mut w) = writer.take() {
            let cp = json!({"run_id": run_id, "manifest_sha256": manifest_hash, "next_index": next, "first_index": *start, "bundle": w.name});
            let committed = w.finish(cp.clone())?;
            budget.settle(&w.name, &committed)?;
            atomic_json(&worker.join("state.json"), &cp)?;
            *start = next;
        }
        Ok(())
    };
    // Pipelined: every article is its own task, so a slow one (a huge PDF supplement, a retrying
    // download) no longer stalls the others. Finished articles wait in `pending` and are committed
    // strictly in manifest order, so bundles and checkpoints are identical to the sequential order.
    // `window` bounds how far work may run ahead of the oldest unfinished article (bounds scratch disk).
    let conc = concurrency.max(1);
    let window = conc * 4;
    let mut tasks: tokio::task::JoinSet<(usize, Result<Done>)> = tokio::task::JoinSet::new();
    let mut pending: std::collections::BTreeMap<usize, Result<Done>> = std::collections::BTreeMap::new();
    let (mut next_sched, mut next_emit) = (cursor, cursor);
    let result: Result<()> = async {
        loop {
            while tasks.len() < conc && next_sched < rows.len() && next_sched < next_emit + window {
                let (i, r, cfg, http) = (next_sched, rows[next_sched].clone(), cfg.clone(), http.clone());
                tasks.spawn(async move { (i, process(r, cfg, http, strict).await) });
                next_sched += 1;
            }
            let Some(res) = pending.remove(&next_emit) else {
                match tasks.join_next().await {
                    Some(j) => {
                        let (i, r) = j.map_err(|e| anyhow!("article task failed: {e}"))?;
                        pending.insert(i, r);
                        continue;
                    }
                    None => break,
                }
            };
            let index = next_emit;
            next_emit += 1;
            let done = res.with_context(|| format!("manifest row {index}"))?;
            if writer.is_none() {
                let name = format!("{run_id}-{index:09}");
                budget.reserve(&name, cfg.target_shard_bytes + cfg.max_article_bytes)?;
                writer = Some(BundleWriter::new(&ready, &name)?);
            }
            let w = writer.as_mut().unwrap();
            if let Some((cm, bytes, xm, xp)) = &done.canonical {
                w.add_bytes(cm, bytes)?;
                w.add_file(xm, xp)?;
            }
            for (member, path) in &done.assets {
                w.add_file(member, path)?;
            }
            errors += done.rows.iter().filter(|r| r.get("status").and_then(|s| s.as_str()) == Some("ingest-error")).count();
            for row in done.rows {
                w.add(row)?;
            }
            article_seconds += done.seconds;
            processed += 1;
            let size = w.size()?;
            if size > cfg.target_shard_bytes + cfg.max_article_bytes {
                bail!("Bundle reservation exceeded; increase max_article_bytes or reduce views");
            }
            if index + 1 - start_cursor >= cfg.articles_per_shard || size >= cfg.target_shard_bytes {
                finish(&mut writer, &mut start_cursor, index + 1)?;
            }
        }
        finish(&mut writer, &mut start_cursor, rows.len())?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        if let Some(mut w) = writer.take() {
            let name = w.name.clone();
            let committed = w.final_path.exists();
            w.abort();
            if !committed {
                let _ = budget.release(&name);
            }
        }
        return Err(e);
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let r = json!({"complete": true, "processed_this_run": processed, "input_records": rows.len(),
        "elapsed_seconds": elapsed, "run_id": run_id, "content_errors": errors,
        "articles_per_second": processed as f64 / elapsed.max(1e-9), "mean_article_seconds": article_seconds / (processed.max(1) as f64),
        "concurrency": concurrency, "http_concurrency": http_conc, "request_interval_ms": interval_ms, "writer": "pigmed-rs",
        "bytes_downloaded": crate::net::BYTES_RECEIVED.load(std::sync::atomic::Ordering::Relaxed)});
    atomic_json(&worker.join("complete.json"), &r)?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_vs_fatal_errors() {
        let content = |m: &str| matches!(classify(anyhow!(m.to_string())), Fail::Content(_));
        assert!(content("XML parse error at 3:4"));
        assert!(content("image 9000x9000 exceeds max_pixels"));
        assert!(content("HTTP 404 for https://x"));
        assert!(content("Source MD5 mismatch (object differs from expected checksum)"));
        assert!(matches!(classify(TooLarge("big".into()).into()), Fail::Content(_)));
        assert!(!content("Retry budget exhausted for https://x: timeout"));
        assert!(!content("HTTP 503 for https://x"));
    }
    #[test]
    fn supplement_config_is_optional() {
        let d = load_config(None, &Map::new()).unwrap();
        assert_eq!(d.supplements, "none");
        assert!(!d.raw.contains_key("supplements")); // run_id unchanged for configs without the extension
        let s = load_config(None, &json!({"supplements": "prose+tables", "table_card_words": 500}).as_object().unwrap().clone()).unwrap();
        assert_eq!((s.supplements.as_str(), s.table_card_words), ("prose+tables", 500));
        assert!(load_config(None, &json!({"supplements": "yes"}).as_object().unwrap().clone()).is_err());
    }
    #[test]
    fn license_policy_config() {
        let all = load_config(None, &json!({"license_policy": "all"}).as_object().unwrap().clone()).unwrap();
        assert_eq!(all.license_policy, "all");
        assert_eq!(load_config(None, &Map::new()).unwrap().license_policy, "redistributable");
        assert!(load_config(None, &json!({"license_policy": "commercial"}).as_object().unwrap().clone()).is_ok());
        assert!(load_config(None, &json!({"license_policy": "nc"}).as_object().unwrap().clone()).is_err());
        assert!(!load_config(None, &Map::new()).unwrap().supplement_skip_pdf);
        assert!(load_config(None, &json!({"supplement_skip_pdf": true}).as_object().unwrap().clone()).unwrap().supplement_skip_pdf);
        assert!(load_config(None, &json!({"supplement_skip_pdf": "yes"}).as_object().unwrap().clone()).is_err());
    }
}
