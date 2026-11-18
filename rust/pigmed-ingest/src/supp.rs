//! `probe-supplements`: is it worth converting + embedding PMC supplementary files?
//! Reads `kind: media` rows from existing bundles, summarises every supplement by format, then
//! downloads a deterministic stratified sample and converts it (anydoc for office/CSV/EPUB/RTF,
//! pdf-inspector for PDF, plain text read directly). Writes per-file JSONL + a summary JSON.
use crate::common::{read_jsonl, stable_id, write_jsonl};
use crate::net::Http;
use anyhow::Result;
use futures::{stream, StreamExt};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const ANYDOC: &[&str] = &[".doc", ".docx", ".odt", ".rtf", ".xls", ".xlsx", ".ods", ".csv", ".tsv", ".ppt", ".pptx", ".odp", ".epub", ".htm", ".html"];
pub const PLAIN: &[&str] = &[".txt", ".md", ".tex"];

pub fn bundle_paths(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(runs) = std::fs::read_dir(root.join("runs")) {
        for r in runs.flatten() {
            if let Ok(ready) = std::fs::read_dir(r.path().join("ready")) {
                for b in ready.flatten() {
                    let p = b.path().join("items.jsonl.gz");
                    if p.exists() && !b.file_name().to_string_lossy().ends_with(".partial") {
                        out.push(p);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

/// Supplement -> Markdown. Returns (markdown, pdf type, tool). See `crate::convert` (no OCR here).
pub fn to_markdown(path: &Path, ext: &str) -> std::result::Result<(String, Option<String>, &'static str), String> {
    crate::convert::convert(path, ext, crate::convert::Opts { ocr_pages: 0 }).map(|c| (c.md, c.pdf_type, c.tool))
}

fn convert(path: &Path, ext: &str) -> Value {
    let t0 = Instant::now();
    let res = to_markdown(path, ext);
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    match res {
        Ok((md, kind, tool)) => {
            let w = words(&md);
            json!({"ok": w > 0, "tool": tool, "pdf_type": kind, "chars": md.chars().count(), "words": w,
                   "convert_ms": ms, "preview": md.chars().take(300).collect::<String>()})
        }
        Err(e) => json!({"ok": false, "error": e.chars().take(300).collect::<String>(), "convert_ms": ms}),
    }
}

pub async fn probe(root: &Path, out: &Path, per_ext: usize, max_bytes: u64, conc: usize) -> Result<Value> {
    std::fs::create_dir_all(out)?;
    // Inventory: every media row that is not the article's own XML/PDF/text, nor an image/video/audio.
    let (mut articles, mut main_words): (HashSet<String>, HashMap<String, usize>) = (HashSet::new(), HashMap::new());
    let mut supp: Vec<Value> = vec![];
    let mut by_modality: BTreeMap<String, u64> = BTreeMap::new();
    let mut with_supp: HashSet<String> = HashSet::new();
    for b in bundle_paths(root) {
        for r in read_jsonl(&b)? {
            let aid = r["article_id"].as_str().unwrap_or("").to_string();
            match r["kind"].as_str() {
                Some("article") => {
                    articles.insert(aid);
                }
                Some("text") | Some("table_text") | Some("caption") => {
                    *main_words.entry(aid).or_default() += words(r["text"].as_str().unwrap_or(""));
                }
                Some("media") if r["role"] == "media_file" => {
                    let m = r["modality"].as_str().unwrap_or("other").to_string();
                    *by_modality.entry(m.clone()).or_default() += 1;
                    if !matches!(m.as_str(), "image" | "video" | "audio") {
                        with_supp.insert(aid);
                        supp.push(r);
                    }
                }
                _ => {}
            }
        }
    }
    let mut ext_counts: BTreeMap<String, u64> = BTreeMap::new();
    for r in &supp {
        *ext_counts.entry(r["extension"].as_str().unwrap_or("").to_string()).or_default() += 1;
    }
    // Deterministic stratified sample: lowest stable hash per extension.
    let mut groups: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    for r in supp.iter() {
        let url = r["source_url"].as_str().unwrap_or("");
        groups.entry(r["extension"].as_str().unwrap_or("").to_string()).or_default().push((stable_id(&[json!(url)]), r.clone()));
    }
    let mut sample = vec![];
    for (_, mut v) in groups {
        v.sort_by(|a, b| a.0.cmp(&b.0));
        sample.extend(v.into_iter().take(per_ext).map(|x| x.1));
    }
    let http = Http::new("", conc, 20)?;
    let scratch = tempfile::Builder::new().prefix("pigmed-supp-").tempdir()?;
    let results: Vec<Value> = stream::iter(sample.into_iter().enumerate().map(|(i, r)| {
        let http = http.clone();
        let dir = scratch.path().to_path_buf();
        async move {
            let url = r["source_url"].as_str().unwrap_or("").to_string();
            let ext = r["extension"].as_str().unwrap_or("").to_string();
            let path = dir.join(format!("f{i}{ext}"));
            let t0 = Instant::now();
            let mut row = Map::new();
            for k in ["article_id", "license", "redistributable", "modality", "extension", "file_name", "source_url"] {
                row.insert(k.into(), r[k].clone());
            }
            match http.download(&url, &path, max_bytes, None).await {
                Ok(d) => {
                    row.insert("bytes".into(), json!(d.bytes));
                    row.insert("download_ms".into(), json!(t0.elapsed().as_secs_f64() * 1000.0));
                    let (p2, e2) = (path.clone(), ext.clone());
                    let conv = tokio::time::timeout(Duration::from_secs(120), tokio::task::spawn_blocking(move || convert(&p2, &e2))).await;
                    match conv {
                        Ok(Ok(v)) => {
                            for (k, x) in v.as_object().unwrap() {
                                row.insert(k.clone(), x.clone());
                            }
                        }
                        _ => {
                            row.insert("ok".into(), json!(false));
                            row.insert("error".into(), json!("conversion timed out (>120 s)"));
                        }
                    }
                }
                Err(e) => {
                    row.insert("ok".into(), json!(false));
                    row.insert("error".into(), json!(format!("download: {e:#}").chars().take(300).collect::<String>()));
                }
            }
            let _ = std::fs::remove_file(&path);
            Value::Object(row)
        }
    }))
    .buffer_unordered(conc.max(1))
    .collect()
    .await;
    write_jsonl(&out.join("files.jsonl"), &results)?;

    // Summary per extension.
    let mut per: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for r in &results {
        per.entry(r["extension"].as_str().unwrap_or("").to_string()).or_default().push(r);
    }
    let median = |mut v: Vec<f64>| -> f64 {
        if v.is_empty() { return 0.0; }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let mut ext_summary = Map::new();
    for (ext, rows) in &per {
        let ok: Vec<&&Value> = rows.iter().filter(|r| r["ok"] == true).collect();
        let errs: BTreeMap<String, usize> = rows.iter().filter(|r| r["ok"] != true).fold(BTreeMap::new(), |mut m, r| {
            let e = r["error"].as_str().unwrap_or("empty output").chars().take(80).collect::<String>();
            *m.entry(e).or_default() += 1;
            m
        });
        let pdf_types: BTreeMap<String, usize> = ok.iter().filter_map(|r| r["pdf_type"].as_str()).fold(BTreeMap::new(), |mut m, t| {
            *m.entry(t.to_string()).or_default() += 1;
            m
        });
        ext_summary.insert(ext.clone(), json!({
            "inventory_files": ext_counts.get(ext).copied().unwrap_or(0), "sampled": rows.len(), "converted_ok": ok.len(),
            "median_words": median(ok.iter().map(|r| r["words"].as_f64().unwrap_or(0.0)).collect()),
            "mean_words": ok.iter().map(|r| r["words"].as_f64().unwrap_or(0.0)).sum::<f64>() / ok.len().max(1) as f64,
            "median_mb": median(rows.iter().filter_map(|r| r["bytes"].as_f64()).map(|b| b / 1e6).collect()),
            "median_convert_ms": median(ok.iter().map(|r| r["convert_ms"].as_f64().unwrap_or(0.0)).collect()),
            "pdf_types": pdf_types, "failures": errs }));
    }
    // Extrapolated supplement words per article vs main text, using per-extension mean words on success.
    let mut est_supp_words = 0.0;
    for (ext, n) in &ext_counts {
        if let Some(s) = ext_summary.get(ext) {
            let rate = s["converted_ok"].as_f64().unwrap_or(0.0) / s["sampled"].as_f64().unwrap_or(1.0).max(1.0);
            est_supp_words += *n as f64 * rate * s["mean_words"].as_f64().unwrap_or(0.0);
        }
    }
    let main_total: usize = main_words.values().sum();
    let summary = json!({
        "articles": articles.len(), "articles_with_non_media_supplements": with_supp.len(),
        "media_files_by_modality": by_modality, "supplement_files": supp.len(), "supplement_files_by_extension": ext_counts,
        "sampled_files": results.len(), "per_extension": ext_summary,
        "main_text_words_total": main_total, "estimated_supplement_words_total": est_supp_words.round(),
        "estimated_supplement_to_main_text_ratio": est_supp_words / (main_total.max(1) as f64),
        "note": "Main text = text/table/caption units. Supplement estimate = inventory count x sampled success rate x sampled mean words, per extension; archives/data/code are not converted."
    });
    crate::common::atomic_json(&out.join("summary.json"), &summary)?;
    Ok(summary)
}
