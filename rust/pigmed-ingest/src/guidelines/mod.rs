//! Clinical practice guidelines: acquisition, extraction and bundle writing (`pigmed-rs guidelines ...`).
//!
//! Same contract as the PMC path: discovery writes an immutable plan (JSONL), `ingest` turns a plan into immutable,
//! checksummed bundles under `ROOT/runs/<run_id>/ready/`, within the shared outbox budget. The existing embed
//! worker, verified HF publisher and retrieval eval then work unchanged (rows reuse the PMC `kind`s; see `doc.rs`).
//!
//!   pigmed-rs guidelines list                      sources, access class, adapter status
//!   pigmed-rs guidelines plan   --source ID --out plans/guidelines/ID.jsonl [--limit N]
//!   pigmed-rs guidelines probe  --source ID [--limit 3] [--report FILE]   discover + extract a few, no bundles
//!   pigmed-rs guidelines ingest --plan FILE --root ROOT                    bundles (resumable per document)
//!
//! Layout: `fetch.rs` (polite cached HTTP), `html.rs` (generic HTML/Markdown -> blocks), `doc.rs` (canonical model
//! -> rows), `registry.rs` (configs/guideline_sources.json), `sources/` (one adapter per source). See
//! docs/GUIDELINES.md.
pub mod doc;
pub mod fetch;
pub mod html;
pub mod registry;
pub mod sources;

use crate::bundle::{Budget, BundleWriter};
use crate::common::{atomic_json, digest_file, read_jsonl, stable_id};
use anyhow::{Context, Result};
use clap::Subcommand;
use doc::ChunkCfg;
use serde_json::{json, Value};
use sources::PlanRow;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Subcommand)]
pub enum Cmd {
    /// List sources with access class, enabled flag and adapter.
    List {
        #[arg(long, default_value = "configs/guideline_sources.json")]
        registry: PathBuf,
        /// only sources with an adapter
        #[arg(long)]
        implemented: bool,
    },
    /// Discover a source's documents and write a plan (JSONL of documents).
    Plan {
        #[arg(long, default_value = "configs/guideline_sources.json")]
        registry: PathBuf,
        #[arg(long)]
        source: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value = "cache/guidelines")]
        cache: PathBuf,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        allow_disabled: bool,
        #[arg(long, default_value = "")]
        contact: String,
    },
    /// Discover + extract a few documents and print a quality report (no bundles). The development loop.
    Probe {
        #[arg(long, default_value = "configs/guideline_sources.json")]
        registry: PathBuf,
        #[arg(long)]
        source: String,
        #[arg(long, default_value_t = 3)]
        limit: usize,
        #[arg(long, default_value = "cache/guidelines")]
        cache: PathBuf,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        allow_disabled: bool,
        /// also write each extracted document as JSON here (for fixtures / inspection)
        #[arg(long)]
        dump: Option<PathBuf>,
        #[arg(long)]
        report: Option<PathBuf>,
        #[arg(long, default_value = "")]
        contact: String,
    },
    /// Ingest a plan into bundles under ROOT (one bundle per plan; rerun resumes, finished plans are skipped).
    Ingest {
        #[arg(long, default_value = "configs/guideline_sources.json")]
        registry: PathBuf,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value = "cache/guidelines")]
        cache: PathBuf,
        #[arg(long)]
        offline: bool,
        #[arg(long, default_value_t = 512)]
        chunk_tokens: usize,
        #[arg(long, default_value_t = 64)]
        overlap_tokens: usize,
        #[arg(long, default_value_t = 2000)]
        table_words: usize,
        /// do not prefix embedded text with "<title> › <heading path>"
        #[arg(long)]
        no_context_prefix: bool,
        /// do not download figure images into assets.tar (rows then carry only `source_url`)
        #[arg(long)]
        no_figures: bool,
        /// per-image size cap for figure downloads
        #[arg(long, default_value_t = 20 << 20)]
        figure_max_bytes: u64,
        #[arg(long, default_value_t = 214748364800)]
        outbox_limit_bytes: u64,
        #[arg(long, default_value = "")]
        contact: String,
    },
}

fn fetcher(src: &registry::SourceEntry, cache: &Path, offline: bool, contact: &str) -> Result<fetch::Fetcher> {
    fetch::Fetcher::new(&cache.join(&src.id), offline, src.min_interval_ms, src.max_bytes, contact)
}

fn adapter(src: &registry::SourceEntry) -> Result<Box<dyn sources::Adapter>> {
    let name = src.adapter.as_deref().unwrap_or_default();
    sources::adapter_for(name).with_context(|| format!("adapter {name:?} for {} is not compiled in", src.id))
}

/// Quality summary of one extracted document: what a reviewer checks first.
pub fn doc_report(d: &doc::GuidelineDoc) -> Value {
    use doc::BlockKind::*;
    let n = |f: fn(&doc::BlockKind) -> bool| d.blocks.iter().filter(|b| f(&b.kind)).count();
    let words: usize = d.blocks.iter().map(|b| b.text.split_whitespace().count()).sum();
    json!({
        "doc_id": d.doc_id(), "title": d.title, "url": d.url, "status": d.status, "version": d.version,
        "published": d.published, "updated": d.updated, "license": d.rights.license, "rights_status": d.rights.status,
        "blocks": d.blocks.len(), "words": words,
        "headings": n(|k| matches!(k, Heading { .. })), "paragraphs": n(|k| matches!(k, Paragraph | ListItem)),
        "recommendations": n(|k| matches!(k, Recommendation { .. })), "tables": n(|k| matches!(k, Table { .. })),
        "figures": n(|k| matches!(k, Figure { .. })),
        "figures_with_url": d.blocks.iter().filter(|b| matches!(&b.kind, Figure { src_url: Some(_), .. })).count(),
        "item_type": d.item_type, "work_key": d.work_key, "dates": d.date_observations.len(), "relations": d.relations.len(),
        "recommendation_samples": d.blocks.iter().filter_map(|b| match &b.kind {
            Recommendation { label, strength, certainty, .. } => Some(json!({"label": label, "strength": strength, "certainty": certainty,
                "text": b.text.chars().take(160).collect::<String>()})),
            _ => None }).take(3).collect::<Vec<_>>(),
        "first_headings": d.blocks.iter().filter(|b| matches!(b.kind, Heading { .. })).take(8).map(|b| b.text.clone()).collect::<Vec<_>>(),
        "warnings": d.warnings, "fetches": d.fetches.len(),
    })
}

pub async fn run(cmd: Cmd) -> Result<Value> {
    match cmd {
        Cmd::List { registry: reg, implemented } => {
            let rows: Vec<Value> = registry::load(&reg)?.into_iter()
                .filter(|s| !implemented || s.adapter.is_some())
                .map(|s| json!({"id": s.id, "access": s.access, "enabled": s.enabled, "adapter": s.adapter, "name": s.name}))
                .collect();
            Ok(json!({"sources": rows.len(), "rows": rows}))
        }
        Cmd::Plan { registry: reg, source, out, limit, cache, offline, allow_disabled, contact } => {
            let src = registry::get(&reg, &source)?;
            registry::check_runnable(&src, allow_disabled)?;
            let f = fetcher(&src, &cache, offline, &contact)?;
            let rows = adapter(&src)?.discover(&src, &f, limit).await?;
            let mut seen = std::collections::HashSet::new();
            let rows: Vec<Value> = rows.into_iter().filter(|r| seen.insert(r.doc_key.clone()))
                .map(|r| serde_json::to_value(r).unwrap()).collect();
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            crate::common::write_jsonl(&out, &rows)?;
            Ok(json!({"source": source, "documents": rows.len(), "plan": out}))
        }
        Cmd::Probe { registry: reg, source, limit, cache, offline, allow_disabled, dump, report, contact } => {
            let src = registry::get(&reg, &source)?;
            registry::check_runnable(&src, allow_disabled)?;
            let f = fetcher(&src, &cache, offline, &contact)?;
            let a = adapter(&src)?;
            let t0 = Instant::now();
            let plan = a.discover(&src, &f, Some(limit)).await?;
            let mut docs = vec![];
            for row in &plan {
                let t = Instant::now();
                match a.extract(&src, &f, row).await {
                    Ok(d) => {
                        if let Some(dir) = &dump {
                            std::fs::create_dir_all(dir)?;
                            std::fs::write(dir.join(format!("{}.json", sanitize(&d.doc_key))), serde_json::to_vec_pretty(&d)?)?;
                        }
                        let mut r = doc_report(&d);
                        r["seconds"] = json!(t.elapsed().as_secs_f64());
                        docs.push(r);
                    }
                    Err(e) => docs.push(json!({"doc_key": row.doc_key, "url": row.url, "error": format!("{e:#}")})),
                }
            }
            let out = json!({"source": source, "discovered": plan.len(), "seconds": t0.elapsed().as_secs_f64(),
                             "ok": docs.iter().filter(|d| d.get("error").is_none()).count(), "documents": docs});
            if let Some(p) = report {
                atomic_json(&p, &out)?;
            }
            Ok(out)
        }
        Cmd::Ingest { registry: reg, plan, root, cache, offline, chunk_tokens, overlap_tokens, table_words, no_context_prefix,
                      no_figures, figure_max_bytes, outbox_limit_bytes, contact } => {
            let cfg = ChunkCfg { chunk_tokens, overlap_tokens, table_words, context_prefix: !no_context_prefix };
            let figs = if no_figures { None } else { Some(figure_max_bytes) };
            ingest(&reg, &plan, &root, &cache, offline, &cfg, figs, outbox_limit_bytes, &contact).await
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect()
}

#[allow(clippy::too_many_arguments)]
async fn ingest(reg: &Path, plan: &Path, root: &Path, cache: &Path, offline: bool, cfg: &ChunkCfg, figure_max: Option<u64>, limit: u64,
                contact: &str) -> Result<Value> {
    let rows: Vec<PlanRow> = read_jsonl(plan)?.into_iter().map(serde_json::from_value).collect::<std::result::Result<_, _>>()?;
    let source = rows.first().map(|r| r.source_id.clone()).context("empty plan")?;
    anyhow::ensure!(rows.iter().all(|r| r.source_id == source), "a plan must hold one source");
    let src = registry::get(reg, &source)?;
    registry::check_runnable(&src, false)?;
    let plan_hash = digest_file(plan)?;
    let recipe = json!({"chunk_tokens": cfg.chunk_tokens, "overlap_tokens": cfg.overlap_tokens, "table_words": cfg.table_words,
                        "context_prefix": cfg.context_prefix, "figures": figure_max.is_some(),
                        "adapter": src.adapter, "writer": format!("pigmed-rs {}", env!("CARGO_PKG_VERSION"))});
    let run_id = stable_id(&[json!("guidelines"), json!(plan_hash), recipe.clone()]);
    let worker = root.join("runs").join(&run_id);
    let ready = worker.join("ready");
    std::fs::create_dir_all(&ready)?;
    let name = format!("{run_id}-000000000");
    if ready.join(&name).join("manifest.json").exists() {
        return Ok(json!({"complete": true, "run_id": run_id, "skipped": "bundle already committed"}));
    }
    let _lock = crate::bundle::ExclusiveFile::acquire(&worker.join("worker.lock"))?;
    let partial = ready.join(format!("{name}.partial"));
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?; // a killed run restarts this (small) plan from scratch
    }
    let budget = Budget::new(root, limit)?;
    budget.reserve(&name, 1 << 30)?;
    let f = fetcher(&src, cache, offline, contact)?;
    let a = adapter(&src)?;
    let mut w = BundleWriter::new(&ready, &name)?;
    let (mut ok, mut failed, mut figs_ok, mut figs_failed) = (0usize, 0usize, 0usize, 0usize);
    let mut unit_ids = std::collections::HashSet::new();
    let t0 = Instant::now();
    for row in &rows {
        match a.extract(&src, &f, row).await {
            Ok(mut d) => {
                let dir = sanitize(&d.doc_id());
                // figure images go through the same polite fetcher (robots, pacing, cache) and into assets.tar, so
                // embedding never re-downloads from the guideline site
                let mut fig_assets = std::collections::HashMap::new();
                if let Some(cap) = figure_max {
                    for (k, url) in figure_urls(&d) {
                        match fetch_figure(&f, &url, cap).await {
                            Ok((bytes, ext)) => {
                                let m = w.add_bytes(&format!("{dir}/figures/{}{ext}", sanitize(&k)), &bytes)?;
                                fig_assets.insert(k, m);
                                figs_ok += 1;
                            }
                            Err(e) => {
                                d.warnings.push(format!("figure {url} not stored: {e:#}"));
                                figs_failed += 1;
                            }
                        }
                    }
                }
                let member = format!("{dir}/document.json");
                w.add_bytes(&member, &serde_json::to_vec(&d)?)?;
                for r in doc::to_rows(&d, cfg, true, &member, &fig_assets) {
                    let uid = r.get("unit_id").map(|v| v.to_string()).unwrap_or_default();
                    anyhow::ensure!(unit_ids.insert(uid.clone()), "duplicate unit_id {uid} in {} (adapter must emit unique block ids)", d.doc_id());
                    w.add(r)?;
                }
                ok += 1;
            }
            Err(e) => {
                // content problems are recorded, not fatal: the plan finishes and the row says why
                let mut r = serde_json::Map::new();
                r.insert("unit_id".into(), json!(stable_id(&[json!(row.source_id), json!(row.doc_key), json!("audit")])));
                r.insert("kind".into(), json!("audit"));
                r.insert("article_id".into(), json!(format!("guideline:{}:{}", row.source_id, row.doc_key)));
                r.insert("source_type".into(), json!("guideline"));
                r.insert("status".into(), json!("ingest-error"));
                r.insert("url".into(), json!(row.url));
                r.insert("error".into(), json!(format!("{e:#}").chars().take(500).collect::<String>()));
                r.insert("export_approved".into(), json!(true));
                r.insert("contains_article_content".into(), json!(false));
                w.add(r)?;
                failed += 1;
            }
        }
    }
    let cp = json!({"run_id": run_id, "manifest_sha256": plan_hash, "next_index": rows.len(), "first_index": 0, "bundle": name, "recipe": recipe});
    let committed = w.finish(cp.clone())?;
    budget.settle(&name, &committed)?;
    atomic_json(&worker.join("state.json"), &cp)?;
    Ok(json!({"complete": true, "run_id": run_id, "source": source, "documents": rows.len(), "ok": ok, "failed": failed,
              "figures_stored": figs_ok, "figures_failed": figs_failed, "seconds": t0.elapsed().as_secs_f64(), "bundle": committed}))
}

/// (block_id, url) of every figure with an http(s) image URL, in document order.
fn figure_urls(d: &doc::GuidelineDoc) -> Vec<(String, String)> {
    d.blocks.iter().filter_map(|b| match &b.kind {
        doc::BlockKind::Figure { src_url: Some(u), .. } if u.starts_with("http") => Some((b.block_id.clone(), u.clone())),
        _ => None,
    }).collect()
}

/// Fetch one figure image; accept only image bytes (magic sniffed) up to `cap`. Returns bytes and a file extension.
async fn fetch_figure(f: &fetch::Fetcher, url: &str, cap: u64) -> Result<(Vec<u8>, &'static str)> {
    let got = f.get_ok(url).await?;
    anyhow::ensure!((got.body.len() as u64) <= cap, "image larger than {cap} bytes");
    let b = &got.body;
    let ext = if b.starts_with(b"\x89PNG") { ".png" }
        else if b.starts_with(b"\xff\xd8\xff") { ".jpg" }
        else if b.starts_with(b"GIF8") { ".gif" }
        else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" { ".webp" }
        else if String::from_utf8_lossy(&b[..b.len().min(512)]).contains("<svg") { ".svg" }
        else { anyhow::bail!("not an image ({})", got.record.content_type.as_deref().unwrap_or("unknown type")) };
    Ok((got.body, ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End to end, offline: a static_list source whose two pages are pre-seeded in the fetch cache -> plan -> ingest
    /// -> a verified bundle whose rows carry the PMC kinds and the guideline identity.
    #[tokio::test]
    async fn static_list_plan_ingest_offline() {
        let d = tempfile::tempdir().unwrap();
        let reg = d.path().join("reg.json");
        std::fs::write(&reg, serde_json::to_vec(&json!({"sources": [{
            "id": "demo", "name": "Demo Guidelines", "access": "public", "enabled": true, "adapter": "static_list",
            "min_interval_ms": 500, "config": {"root": "main", "rights": {"license": "CC-BY-4.0", "status": "open"},
              "documents": [{"url": "https://example.org/g/a", "doc_key": "a-2026"},
                            {"url": "https://example.org/g/missing", "doc_key": "missing"}]}}]})).unwrap()).unwrap();
        // seed the cache exactly as the fetcher would have stored a live response
        let cache = d.path().join("cache");
        let f = fetch::Fetcher::new(&cache.join("demo"), true, 500, 1 << 20, "").unwrap();
        let html = "<html><head><title>A</title></head><body><main><h1>Asthma</h1><h2>Recommendations</h2><ul><li>Offer ICS.</li></ul>\
                    <p>Long enough background paragraph.</p><figure><img src=\"/img/alg.png\"><figcaption>Algorithm</figcaption></figure>\
                    <img src=\"https://example.org/missing.png\"></main></body></html>";
        f.seed_cache_for_tests("https://example.org/g/a", html.as_bytes(), "text/html");
        f.seed_cache_for_tests("https://example.org/img/alg.png", b"\x89PNG\r\n\x1a\nfake", "image/png");
        let plan = d.path().join("plan.jsonl");
        run(Cmd::Plan { registry: reg.clone(), source: "demo".into(), out: plan.clone(), limit: None, cache: cache.clone(),
                        offline: true, allow_disabled: false, contact: String::new() }).await.unwrap();
        let root = d.path().join("root");
        let r = run(Cmd::Ingest { registry: reg.clone(), plan: plan.clone(), root: root.clone(), cache: cache.clone(), offline: true,
                                  chunk_tokens: 512, overlap_tokens: 64, table_words: 2000, no_context_prefix: false,
                                  no_figures: false, figure_max_bytes: 1 << 20, outbox_limit_bytes: 1 << 40,
                                  contact: String::new() }).await.unwrap();
        assert_eq!((r["ok"].as_u64(), r["failed"].as_u64()), (Some(1), Some(1)));
        let bundle = PathBuf::from(r["bundle"].as_str().unwrap());
        crate::bundle::verify_bundle(&bundle).unwrap();
        let rows: Vec<Value> = {
            use std::io::BufRead;
            std::io::BufReader::new(flate2::read::GzDecoder::new(std::fs::File::open(bundle.join("items.jsonl.gz")).unwrap()))
                .lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect()
        };
        let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"article") && kinds.contains(&"text") && kinds.contains(&"audit"), "{kinds:?}");
        assert!(rows.iter().filter(|r| r["kind"] == "text").all(|r| r["license"] == "CC-BY-4.0" && r["source_type"] == "guideline"));
        assert!(rows.iter().filter(|r| r["kind"] == "text").all(|r| r["text"].as_str().unwrap().starts_with("Asthma")));
        // the cached figure is stored in assets.tar; the uncached one is a warning on the article row
        let figs: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "figure").collect();
        assert_eq!(figs.len(), 2);
        assert!(figs.iter().any(|r| r["asset_member"].as_str().is_some_and(|m| m.ends_with(".png"))));
        assert_eq!((r["figures_stored"].as_u64(), r["figures_failed"].as_u64()), (Some(1), Some(1)));
        let art = rows.iter().find(|r| r["kind"] == "article").unwrap();
        assert!(art["warnings"].to_string().contains("missing.png"));
        // rerun: already committed
        let again = run(Cmd::Ingest { registry: reg, plan, root, cache, offline: true, chunk_tokens: 512, overlap_tokens: 64,
                                      table_words: 2000, no_context_prefix: false, no_figures: false, figure_max_bytes: 1 << 20,
                                      outbox_limit_bytes: 1 << 40, contact: String::new() }).await.unwrap();
        assert_eq!(again["skipped"], "bundle already committed");
    }
}
