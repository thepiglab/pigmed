//! `pdf-bench`: why are some supplementary PDFs slow to convert? Picks the slowest PDFs (by recorded
//! `supplement.convert_ms`) plus a deterministic sample of ordinary ones from existing bundles,
//! downloads them, then times pdf-inspector stages (detect / analyze / full), per-page extraction for
//! slow files, and pdfium plain-text extraction as an alternative. One file at a time, so timings are clean.
use crate::common::{read_jsonl, stable_id};
use crate::net::Http;
use anyhow::Result;
use pdf_inspector::{PdfOptions, ProcessMode};
use pdfium_render::prelude::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn ms(t: Instant) -> f64 {
    (t.elapsed().as_secs_f64() * 1000.0).round()
}

fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

fn inspector(path: &Path, opts: PdfOptions) -> (f64, Value) {
    let t = Instant::now();
    let r = std::panic::catch_unwind(|| pdf_inspector::process_pdf_with_options(path, opts));
    let el = ms(t);
    match r {
        Ok(Ok(r)) => (el, json!({"pages": r.page_count, "type": format!("{:?}", r.pdf_type),
            "words": r.markdown.as_deref().map(words), "ocr_pages": r.pages_needing_ocr.len(),
            "layout": format!("{:?}", r.layout), "producer": r.producer, "creator": r.creator})),
        Ok(Err(e)) => (el, json!({"error": e.to_string()})),
        Err(_) => (el, json!({"error": "panic"})),
    }
}

fn pdfium_text(path: &Path) -> (f64, Value) {
    let t = Instant::now();
    let r = (|| -> Result<(usize, usize, usize)> {
        let lib = crate::media::pdfium()?;
        let _g = crate::media::PDFIUM_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let doc = lib.load_pdf_from_file(path, None).map_err(|e| anyhow::anyhow!("{e}"))?;
        let (mut w, mut objs) = (0, 0);
        for p in doc.pages().iter() {
            objs += p.objects().len();
            if let Ok(t) = p.text() {
                w += words(&t.all());
            }
        }
        Ok((doc.pages().len() as usize, w, objs))
    })();
    let el = ms(t);
    match r {
        Ok((n, w, o)) => (el, json!({"pages": n, "words": w, "page_objects": o})),
        Err(e) => (el, json!({"error": e.to_string()})),
    }
}

pub async fn run(root: &Path, slow: usize, sample: usize, max_pages: u32) -> Result<Value> {
    let mut rows: Vec<Value> = vec![];
    for b in crate::supp::bundle_paths(root) {
        for r in read_jsonl(&b)? {
            if r["kind"] == "media" && r["extension"] == ".pdf" && r["supplement"].is_object() && r["supplement"]["bytes"].is_u64() {
                rows.push(r);
            }
        }
    }
    let cm = |r: &Value| r["supplement"]["convert_ms"].as_f64().unwrap_or(0.0);
    rows.sort_by(|a, b| cm(b).partial_cmp(&cm(a)).unwrap());
    let mut pick: Vec<(String, Value)> = rows.iter().take(slow).map(|r| ("slow".to_string(), r.clone())).collect();
    let mut rest: Vec<&Value> = rows.iter().skip(slow).collect();
    rest.sort_by_key(|r| stable_id(&[r["source_url"].clone()]));
    pick.extend(rest.into_iter().take(sample).map(|r| ("sample".to_string(), r.clone())));
    let http = Http::new("", 8, 10)?;
    let dir = tempfile::Builder::new().prefix("pigmed-pdfbench-").tempdir()?;
    let mut out = vec![];
    for (i, (group, r)) in pick.into_iter().enumerate() {
        let url = r["source_url"].as_str().unwrap_or("").to_string();
        let path: PathBuf = dir.path().join(format!("f{i}.pdf"));
        if let Err(e) = http.download(&url, &path, 256 << 20, None).await {
            out.push(json!({"group": group, "article_id": r["article_id"], "url": url, "download_error": format!("{e:#}")}));
            continue;
        }
        let bytes = std::fs::metadata(&path)?.len();
        let p = path.clone();
        let res = tokio::task::spawn_blocking(move || {
            let (detect_ms, d) = inspector(&p, PdfOptions::detect_only());
            let (analyze_ms, _) = inspector(&p, PdfOptions::new().mode(ProcessMode::Analyze));
            let (full_ms, f) = inspector(&p, PdfOptions::new());
            let (pdfium_ms, pf) = pdfium_text(&p);
            // Per-page cost only where the whole file was slow: which pages carry the time?
            let mut per_page = vec![];
            if full_ms > 3000.0 {
                let n = d["pages"].as_u64().unwrap_or(0) as u32;
                for pg in 1..=n.min(max_pages) {
                    let (t, v) = inspector(&p, PdfOptions::new().pages([pg]));
                    per_page.push(json!({"page": pg, "ms": t, "words": v["words"]}));
                }
            }
            json!({"detect_ms": detect_ms, "analyze_ms": analyze_ms, "full_ms": full_ms, "inspector": f,
                   "pdfium_text_ms": pdfium_ms, "pdfium": pf, "per_page": per_page})
        })
        .await?;
        let mut row = json!({"group": group, "article_id": r["article_id"], "file": r["file_name"], "bytes": bytes,
            "recorded_convert_ms": r["supplement"]["convert_ms"], "recorded_status": r["supplement"]["status"]});
        for (k, v) in res.as_object().unwrap() {
            row[k] = v.clone();
        }
        println!("{}", serde_json::to_string(&row)?);
        out.push(row);
        let _ = std::fs::remove_file(&path);
    }
    Ok(json!({"files": out.len()}))
}
