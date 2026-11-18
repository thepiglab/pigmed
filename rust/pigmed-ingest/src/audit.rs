//! `supp-audit`: extraction fidelity for supplementary files. Re-converts every file that previously
//! failed (no-text / convert-failed / download-failed / too-large) plus a deterministic sample of
//! successful ones, and compares the words we extract against an independent reference:
//!   PDF  -> pdfium text layer (OCR'd pages excluded from the ratio, reported separately)
//!   DOCX -> raw `<w:t>` runs of word/document.xml
//!   XLSX/XLS/ODS -> every non-empty cell via calamine (full read)
//! coverage = our words / reference words. Prints one JSON line per file; the caller summarises.
use crate::common::{read_jsonl, stable_id};
use crate::net::Http;
use anyhow::Result;
use futures::{stream, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

/// Content words only: tokens with a letter or digit (Markdown table pipes/rules don't count).
fn words(s: &str) -> usize {
    s.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphanumeric())).count()
}

fn pdfium_plain(path: &Path) -> Option<String> {
    use pdfium_render::prelude::*;
    let lib = crate::media::pdfium().ok()?;
    let _g = crate::media::PDFIUM_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let doc = lib.load_pdf_from_file(path, None).ok()?;
    let mut s = String::new();
    for p in doc.pages().iter() {
        s.push_str(&p.text().map(|t| t.all()).unwrap_or_default());
        s.push_str("\n\n");
    }
    Some(s)
}

/// Lower-cased alphanumeric tokens (table pipes, punctuation and Markdown syntax do not count).
fn tokens(s: &str) -> std::collections::HashMap<String, usize> {
    let mut m = std::collections::HashMap::new();
    for t in s.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
        *m.entry(t.to_lowercase()).or_insert(0) += 1;
    }
    m
}

/// Fraction of the reference's tokens (with multiplicity) that our text also contains.
fn recall(ours: &str, reference: &str) -> (f64, usize) {
    let (o, r) = (tokens(ours), tokens(reference));
    let total: usize = r.values().sum();
    let hit: usize = r.iter().map(|(k, n)| (*n).min(*o.get(k).unwrap_or(&0))).sum();
    (if total > 0 { hit as f64 / total as f64 } else { 1.0 }, total)
}

fn reference_text(path: &Path, detected: &str) -> Option<(String, &'static str)> {
    match detected {
        "pdf" => pdfium_plain(path).map(|t| (t, "pdfium-text-layer")),
        "docx" => {
            let f = std::fs::File::open(path).ok()?;
            let mut z = zip::ZipArchive::new(f).ok()?;
            let mut xml = String::new();
            z.by_name("word/document.xml").ok()?.read_to_string(&mut xml).ok()?;
            let doc = roxmltree::Document::parse_with_options(&xml, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() }).ok()?;
            let mut out = String::new();
            for p in doc.descendants().filter(|n| n.tag_name().name() == "p") {
                let t: String = p.descendants().filter(|n| n.tag_name().name() == "t").filter_map(|n| n.text()).collect();
                out.push_str(&t);
                out.push('\n');
            }
            Some((out, "docx-w:t"))
        }
        "excel" => {
            use calamine::{open_workbook_auto, Reader};
            let mut wb = open_workbook_auto(path).ok()?;
            let mut out = String::new();
            for name in wb.sheet_names().to_owned() {
                if let Ok(r) = wb.worksheet_range(&name) {
                    for c in r.used_cells() {
                        out.push_str(&c.2.to_string());
                        out.push(' ');
                    }
                }
            }
            Some((out, "calamine-cells"))
        }
        _ => None,
    }
}

pub async fn run(root: &Path, per_ext_ok: usize, ocr_pages: usize, conc: usize, dump: Option<std::path::PathBuf>) -> Result<Value> {
    if let Some(d) = &dump {
        std::fs::create_dir_all(d)?;
    }
    let mut rows: Vec<Value> = vec![];
    for b in crate::supp::bundle_paths(root) {
        for r in read_jsonl(&b)? {
            if r["kind"] == "media" && r["supplement"].is_object() {
                rows.push(r);
            }
        }
    }
    let mut pick = vec![];
    let mut ok_by_ext: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    for r in rows {
        if r["supplement"]["status"] == "ok" {
            ok_by_ext.entry(r["extension"].as_str().unwrap_or("").to_string()).or_default().push((stable_id(&[r["source_url"].clone(), json!("audit")]), r));
        } else if r["supplement"]["status"] != "skipped-class" {
            pick.push(("failed".to_string(), r));
        }
    }
    for (_, mut v) in ok_by_ext {
        v.sort_by(|a, b| a.0.cmp(&b.0));
        pick.extend(v.into_iter().take(per_ext_ok).map(|x| ("ok".to_string(), x.1)));
    }
    let http = Http::new("", conc.max(1), 0)?;
    let dir = tempfile::Builder::new().prefix("pigmed-audit-").tempdir()?;
    let n = pick.len();
    let mut s = stream::iter(pick.into_iter().enumerate().map(|(i, (group, r))| {
        let (http, d, dump) = (http.clone(), dir.path().to_path_buf(), dump.clone());
        async move {
            let url = r["source_url"].as_str().unwrap_or("").to_string();
            let ext = r["extension"].as_str().unwrap_or("").to_string();
            let path = d.join(format!("f{i}{ext}"));
            let mut row = json!({"group": group, "article_id": r["article_id"], "file": r["file_name"], "ext": ext,
                                 "before": r["supplement"]["status"], "before_words": r["supplement"]["words"]});
            let mut dl = http.download(&url, &path, 256 << 20, None).await;
            if matches!(&dl, Err(e) if format!("{e:#}").contains("MD5 mismatch")) {
                row["md5_mismatch"] = json!(true);
                dl = http.download(&url, &path, 256 << 20, Some("")).await;
            }
            if let Err(e) = dl {
                row["download_error"] = json!(format!("{e:#}").chars().take(200).collect::<String>());
                return row;
            }
            let p = path.clone();
            let res = tokio::task::spawn_blocking(move || {
                let t = Instant::now();
                let c = crate::convert::convert(&p, &ext, crate::convert::Opts { ocr_pages });
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let mut out = json!({"convert_ms": ms.round()});
                match c {
                    Ok(c) => {
                        let ours = words(&c.md);
                        // Words from OCR'd pages are not in the reference text layer: exclude them from coverage.
                        let ocr_words: usize = c.md.split("\n\n[page ").skip(1).filter(|s| s.contains("(OCR)]")).map(words).sum();
                        out["after"] = json!(if ours > 0 { "ok" } else { "no-text" });
                        out["tool"] = json!(c.tool);
                        out["detected"] = json!(c.detected);
                        out["pdf_type"] = json!(c.pdf_type);
                        out["pages"] = json!(c.pages);
                        out["ocr_pages"] = json!(c.ocr_pages);
                        out["pages_without_text"] = json!(c.pages_without_text);
                        out["words"] = json!(ours);
                        out["ocr_words"] = json!(ocr_words);
                        let t2 = Instant::now();
                        if let Some((rt, how)) = reference_text(&p, c.detected) {
                            // OCR'd pages are not in the reference text layer: score the non-OCR part only.
                            let body = c.md.split("\n\n[page ").next().unwrap_or("");
                            let (rec, rw) = recall(if ocr_words > 0 { body } else { &c.md }, &rt);
                            out["ref_words"] = json!(rw);
                            out["ref"] = json!(how);
                            out["ref_ms"] = json!((t2.elapsed().as_secs_f64() * 1000.0).round());
                            if rw > 0 {
                                let cov = rec;
                                out["coverage"] = json!((cov * 1000.0).round() / 1000.0);
                                // Low coverage: keep both texts (and the file) for inspection.
                                if let (Some(dd), true) = (&dump, cov < 0.8 && c.detected == "pdf") {
                                    let stem = format!("{i:04}");
                                    let _ = std::fs::write(dd.join(format!("{stem}.ours.md")), &c.md);
                                    if let Some(t) = pdfium_plain(&p) {
                                        let _ = std::fs::write(dd.join(format!("{stem}.ref.txt")), t);
                                    }
                                    let _ = std::fs::copy(&p, dd.join(format!("{stem}.pdf")));
                                    out["dump"] = json!(stem);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        out["after"] = json!("convert-failed");
                        out["error"] = json!(e.chars().take(200).collect::<String>());
                    }
                }
                out
            })
            .await
            .unwrap_or_else(|e| json!({"after": "panic", "error": e.to_string()}));
            for (k, v) in res.as_object().unwrap() {
                row[k] = v.clone();
            }
            let _ = std::fs::remove_file(&path);
            row
        }
    }))
    .buffer_unordered(conc.max(1));
    let mut done = 0;
    while let Some(row) = s.next().await {
        println!("{}", serde_json::to_string(&row)?);
        done += 1;
    }
    Ok(json!({"files": n, "printed": done}))
}
