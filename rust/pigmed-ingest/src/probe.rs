//! Measurement probes for the multimodal backfill plan (held-out videos/audio and PDFs of the v2 run).
//!
//! `probe-media`: per file (JSONL rows with `url`, optional `md5`), download (md5-checked), ffprobe (duration, streams)
//! and optionally cut one 32 s clip the way ingest does (`media::make_clip`), timing every step.
//! `probe-pdf`: per PDF, download, run the supplement converter (`convert::convert`: pdfium text, OCR of image-only pages
//! up to `ocr_pages`) and optionally render up to `render_pages` pages (`media::render_pdf`), timing every step.
//! Input rows are echoed into the output (minus nothing), so results join back to the sample. Files are deleted at once.
use crate::net::Http;
use anyhow::{Context, Result};
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn read_rows(p: &Path) -> Result<Vec<Map<String, Value>>> {
    let text = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
    Ok(text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str::<Value>(l).ok()?.as_object().cloned()).collect())
}

fn dir_bytes(d: &Path) -> u64 {
    std::fs::read_dir(d).map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0)
}

async fn fetch(http: &Http, row: &Map<String, Value>, dir: &Path, i: usize, max_bytes: u64) -> Result<(PathBuf, f64, u64)> {
    let url = row.get("url").and_then(|v| v.as_str()).context("row without url")?;
    let ext = crate::media::extension(url);
    let path = dir.join(format!("f{i}{ext}"));
    let md5 = row.get("md5").and_then(|v| v.as_str());
    let t = Instant::now();
    let d = http.download(url, &path, max_bytes, md5).await?;
    Ok((path, t.elapsed().as_secs_f64(), d.bytes))
}

pub async fn probe_media(input: &Path, out: &Path, concurrency: usize, clip_test: bool, scratch: &Path) -> Result<Value> {
    let rows = read_rows(input)?;
    let http = Http::new("", concurrency * 2, 0)?;
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    let t0 = Instant::now();
    let mut results = futures::stream::iter(rows.into_iter().enumerate().map(|(i, row)| {
        let http = http.clone();
        let dir = scratch.join(format!("m{i}"));
        async move {
            let mut o = row.clone();
            std::fs::create_dir_all(&dir).ok();
            match fetch(&http, &row, &dir, i, 2 << 30).await {
                Err(e) => {
                    o.insert("error".into(), json!(format!("download: {e:#}")));
                }
                Ok((path, dl_s, bytes)) => {
                    o.insert("download_s".into(), json!(dl_s));
                    o.insert("bytes_downloaded".into(), json!(bytes));
                    let p = path.clone();
                    let t = Instant::now();
                    match tokio::task::spawn_blocking(move || crate::media::probe_media(&p)).await {
                        Ok(Ok((dur, has_audio, has_video))) => {
                            o.insert("probe_s".into(), json!(t.elapsed().as_secs_f64()));
                            o.insert("duration_s".into(), json!(dur));
                            o.insert("has_audio".into(), json!(has_audio));
                            o.insert("has_video".into(), json!(has_video));
                            if clip_test && has_video && dur > 0.0 {
                                let (p, d) = (path.clone(), dir.join("clip.mp4"));
                                let end = dur.min(32.0);
                                let t = Instant::now();
                                let r = tokio::task::spawn_blocking(move || crate::media::make_clip(&p, &d, 0.0, end, "video")).await;
                                o.insert("clip32_s".into(), json!(t.elapsed().as_secs_f64()));
                                o.insert("clip32_ok".into(), json!(matches!(r, Ok(Ok(())))));
                                o.insert("clip32_bytes".into(), json!(dir_bytes(&dir).saturating_sub(bytes)));
                            }
                        }
                        Ok(Err(e)) => {
                            o.insert("error".into(), json!(format!("probe: {e:#}")));
                        }
                        Err(e) => {
                            o.insert("error".into(), json!(format!("probe task: {e}")));
                        }
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            Value::Object(o)
        }
    }))
    .buffer_unordered(concurrency.max(1));
    let (mut n, mut errors) = (0usize, 0usize);
    while let Some(v) = results.next().await {
        n += 1;
        if v.get("error").is_some() {
            errors += 1;
        }
        writeln!(w, "{v}")?;
    }
    w.flush()?;
    Ok(json!({"files": n, "errors": errors, "wall_s": t0.elapsed().as_secs_f64()}))
}

#[allow(clippy::too_many_arguments)]
pub async fn probe_pdf(input: &Path, out: &Path, concurrency: usize, ocr_pages: usize, render_pages: usize, longest: u32,
                       scratch: &Path) -> Result<Value> {
    let rows = read_rows(input)?;
    let http = Http::new("", concurrency * 2, 0)?;
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    let t0 = Instant::now();
    let mut results = futures::stream::iter(rows.into_iter().enumerate().map(|(i, row)| {
        let http = http.clone();
        let dir = scratch.join(format!("p{i}"));
        async move {
            let mut o = row.clone();
            std::fs::create_dir_all(&dir).ok();
            match fetch(&http, &row, &dir, i, 512 << 20).await {
                Err(e) => {
                    o.insert("error".into(), json!(format!("download: {e:#}")));
                }
                Ok((path, dl_s, bytes)) => {
                    o.insert("download_s".into(), json!(dl_s));
                    o.insert("bytes_downloaded".into(), json!(bytes));
                    let p = path.clone();
                    let t = Instant::now();
                    let conv = tokio::task::spawn_blocking(move || crate::convert::convert(&p, ".pdf", crate::convert::Opts { ocr_pages })).await;
                    o.insert("convert_s".into(), json!(t.elapsed().as_secs_f64()));
                    match conv {
                        Ok(Ok(c)) => {
                            o.insert("tool".into(), json!(c.tool));
                            o.insert("pdf_type".into(), json!(c.pdf_type));
                            o.insert("pages".into(), json!(c.pages));
                            o.insert("ocr_pages".into(), json!(c.ocr_pages));
                            o.insert("pages_without_text".into(), json!(c.pages_without_text));
                            o.insert("md_chars".into(), json!(c.md.chars().count()));
                            o.insert("md_words".into(), json!(c.md.split_whitespace().count()));
                        }
                        Ok(Err(e)) => {
                            o.insert("convert_error".into(), json!(e));
                        }
                        Err(e) => {
                            o.insert("convert_error".into(), json!(format!("task: {e}")));
                        }
                    }
                    if render_pages > 0 {
                        let (p, d) = (path.clone(), dir.join("pages"));
                        let t = Instant::now();
                        let r = tokio::task::spawn_blocking(move || crate::media::render_pdf(&p, &d, longest, render_pages)).await;
                        o.insert("render_s".into(), json!(t.elapsed().as_secs_f64()));
                        match r {
                            Ok(Ok(pages)) => {
                                o.insert("rendered_pages".into(), json!(pages.len()));
                                o.insert("render_png_bytes".into(), json!(dir_bytes(&dir.join("pages"))));
                            }
                            Ok(Err(e)) => {
                                o.insert("render_error".into(), json!(format!("{e:#}")));
                            }
                            Err(e) => {
                                o.insert("render_error".into(), json!(format!("task: {e}")));
                            }
                        }
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            Value::Object(o)
        }
    }))
    .buffer_unordered(concurrency.max(1));
    let (mut n, mut errors) = (0usize, 0usize);
    while let Some(v) = results.next().await {
        n += 1;
        if v.get("error").is_some() || v.get("convert_error").is_some() {
            errors += 1;
        }
        writeln!(w, "{v}")?;
    }
    w.flush()?;
    Ok(json!({"files": n, "errors": errors, "wall_s": t0.elapsed().as_secs_f64()}))
}
