//! Supplementary file -> text/Markdown, aiming for complete content at bounded cost.
//!
//! * The format is sniffed from the bytes, not trusted from the extension (PMC has `.xls` files that
//!   are HTML or TSV, `.docx` that are legacy `.doc`, ...).
//! * PDF: pdf-inspector Markdown for ordinary files; pdfium plain text for object-heavy files (vector
//!   plots; pdf-inspector is superlinear in drawing objects) and when pdf-inspector finds no text;
//!   tesseract OCR for image-only pages (scans, figure sheets) when `ocr_pages > 0`.
//! * Spreadsheets: anydoc, falling back to calamine (streaming for .xlsx) when the workbook exceeds
//!   anydoc's fixed resource limits or is very large.
//! * HTML: tag-stripping text conversion (anydoc has no HTML reader).
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    /// OCR at most this many image-only pages per PDF (0 = OCR off).
    pub ocr_pages: usize,
}

#[derive(Debug)]
pub struct Converted {
    pub md: String,
    pub pdf_type: Option<String>,
    pub tool: &'static str,
    /// Format actually detected from content (e.g. "html" for a `.xls` that is an HTML table).
    pub detected: &'static str,
    pub ocr_pages: usize,
    pub pages: Option<u32>,
    /// Image-only pages left without text (OCR off, or beyond the OCR page cap).
    pub pages_without_text: usize,
}

impl Converted {
    fn new(md: String, tool: &'static str, detected: &'static str) -> Self {
        Self { md, pdf_type: None, tool, detected, ocr_pages: 0, pages: None, pages_without_text: 0 }
    }
}

type R<T> = std::result::Result<T, String>;

/// Drawing objects above which pdf-inspector's layout/table analysis is skipped for pdfium text.
/// Measured on PMC supplements (s1000): ordinary PDFs convert in ~23 ms median; files with 140k-1.6M
/// objects took 18-112 s in pdf-inspector vs 0.6-5 s for pdfium plain text.
pub const PDF_OBJECT_CAP: usize = 100_000;
/// Workbooks above this go straight to calamine (anydoc builds a full document model).
const EXCEL_ANYDOC_MAX: u64 = 16 << 20;
/// Markdown kept from a streamed workbook (counts continue past it).
const EXCEL_MD_CAP: usize = 4 << 20;

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum Kind {
    Pdf,
    Excel,
    Doc(anydoc::Format),
    Html,
    Text,
}

fn sniff(bytes: &[u8], ext: &str) -> Kind {
    let head = &bytes[..bytes.len().min(1024)];
    if head.windows(5).any(|w| w == b"%PDF-") {
        return Kind::Pdf;
    }
    match anydoc::Format::from_bytes(bytes) {
        Some(anydoc::Format::Pdf) => return Kind::Pdf,
        Some(anydoc::Format::Excel) => return Kind::Excel,
        Some(f) => return Kind::Doc(f),
        None => {}
    }
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_lowercase();
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with('<') && ["<html", "<!doctype html", "<table", "<body", "<div", "<p>", "<head"].iter().any(|k| t.contains(k)) {
        return Kind::Html;
    }
    // A ZIP or OLE container anydoc could not identify from these bytes (we only pass the head of
    // large files, and ZIP packages are identified by entries that may lie past it): let the
    // extension pick the parser.
    // A ZIP package is always the modern (OOXML/ODF) member of the extension's family, even when
    // named with the legacy extension (PMC has `.doc` files that are really `.docx`).
    if bytes.starts_with(b"PK\x03\x04") {
        use anydoc::Format as F;
        match ext {
            ".doc" | ".docx" | ".docm" | ".rtf" => return Kind::Doc(F::Docx),
            ".xls" | ".xlsx" | ".xlsm" | ".xlsb" => return Kind::Excel,
            ".ppt" | ".pptx" | ".pptm" => return Kind::Doc(F::Pptx),
            ".odt" => return Kind::Doc(F::Odt),
            ".ods" => return Kind::Doc(F::Ods),
            ".odp" => return Kind::Doc(F::Odp),
            ".epub" => return Kind::Doc(F::Epub),
            _ => {}
        }
    }
    // An OLE container is the legacy member of the family.
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        use anydoc::Format as F;
        match ext {
            ".doc" | ".docx" | ".dot" => return Kind::Doc(F::Doc),
            ".xls" | ".xlsx" => return Kind::Excel,
            ".ppt" | ".pptx" | ".pps" => return Kind::Doc(F::Ppt),
            _ => {}
        }
    }
    Kind::Text
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Pdf => "pdf",
        Kind::Excel => "excel",
        Kind::Html => "html",
        Kind::Text => "text",
        Kind::Doc(f) => match f {
            anydoc::Format::Doc => "doc",
            anydoc::Format::Docx => "docx",
            anydoc::Format::Odt => "odt",
            anydoc::Format::Ppt => "ppt",
            anydoc::Format::Pptx => "pptx",
            anydoc::Format::Rtf => "rtf",
            anydoc::Format::Epub => "epub",
            anydoc::Format::Ods => "ods",
            anydoc::Format::Odp => "odp",
            anydoc::Format::Csv => "csv",
            _ => "other",
        },
    }
}

pub fn convert(path: &Path, ext: &str, opts: Opts) -> R<Converted> {
    std::panic::catch_unwind(|| convert_inner(path, ext, opts)).unwrap_or_else(|_| Err("converter panicked".into()))
}

fn convert_inner(path: &Path, ext: &str, opts: Opts) -> R<Converted> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    // Large files: sniff from the head only, never read whole multi-hundred-MB files twice.
    let head = {
        use std::io::Read;
        let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut b = vec![0u8; 64 << 10];
        let n = f.read(&mut b).map_err(|e| e.to_string())?;
        b.truncate(n);
        b
    };
    let mut kind = sniff(&head, ext);
    // anydoc identifies ZIP/OLE packages by their internal entries; with only the head we may miss
    // them, so re-sniff on the full bytes for small files that came out as plain text.
    if kind == Kind::Text && head.len() < size as usize && size <= 64 << 20 && (head.starts_with(b"PK") || head.starts_with(&[0xD0, 0xCF])) {
        let all = std::fs::read(path).map_err(|e| e.to_string())?;
        kind = sniff(&all, ext);
    }
    let detected = kind_name(kind);
    match kind {
        Kind::Pdf => pdf(path, opts).map(|mut c| {
            c.detected = detected;
            c
        }),
        Kind::Excel => excel(path, ext, size).map(|mut c| {
            c.detected = detected;
            c
        }),
        Kind::Doc(f) => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            match anydoc::to_markdown_bytes(&bytes, f) {
                Ok(md) => Ok(Converted::new(md, "anydoc", detected)),
                // anydoc's limits are fixed (e.g. total embedded-image bytes): fall back to the raw
                // text runs of an OOXML package so the words are not lost.
                Err(e) => ooxml_text(path, f).map(|t| Converted::new(t, "ooxml-text", detected)).ok_or_else(|| format!("{e}")),
            }
        }
        Kind::Html => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            Ok(Converted::new(html_to_text(&String::from_utf8_lossy(&bytes)), "html-text", detected))
        }
        Kind::Text => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            if matches!(ext, ".csv" | ".tsv") {
                if let Ok(md) = anydoc::to_markdown_bytes(&bytes, anydoc::Format::Csv) {
                    return Ok(Converted::new(md, "anydoc", "csv"));
                }
            }
            if bytes.iter().take(8192).filter(|b| **b == 0).count() > 8 {
                return Err(format!("binary content not recognised (extension {ext})"));
            }
            Ok(Converted::new(String::from_utf8_lossy(&bytes).into_owned(), "plain", detected))
        }
    }
}

// ---------------------------------------------------------------- PDF

fn pdf(path: &Path, opts: Opts) -> R<Converted> {
    use pdfium_render::prelude::*;
    // One pdfium pass: object count (routing) and, for object-heavy files, the text itself.
    let probe = (|| -> Option<(usize, u32)> {
        let lib = crate::media::pdfium().ok()?;
        let _g = crate::media::PDFIUM_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let doc = lib.load_pdf_from_file(path, None).ok()?;
        let mut n = 0;
        for p in doc.pages().iter() {
            n += p.objects().len();
            if n >= PDF_OBJECT_CAP {
                break;
            }
        }
        Some((n, doc.pages().len() as u32))
    })();
    if let Some((n, pages)) = probe {
        if n >= PDF_OBJECT_CAP {
            let (md, empty) = pdfium_pages_text(path)?;
            let mut c = Converted::new(md, "pdfium-text", "pdf");
            c.pdf_type = Some("ObjectHeavy".into());
            c.pages = Some(pages);
            return finish_ocr(path, c, empty, opts);
        }
    }
    let r = pdf_inspector::process_pdf(path).map_err(|e| format!("{e}"))?;
    let mut c = Converted::new(r.markdown.clone().unwrap_or_default(), "pdf-inspector", "pdf");
    c.pdf_type = Some(format!("{:?}", r.pdf_type));
    c.pages = Some(r.page_count);
    // pdf-inspector found nothing: pdfium's text layer may still have it.
    if c.md.split_whitespace().next().is_none() {
        let (t, empty) = pdfium_pages_text(path)?;
        if t.split_whitespace().next().is_some() {
            c.md = t;
            c.tool = "pdfium-text";
            return finish_ocr(path, c, empty, opts);
        }
        let all: Vec<u32> = (0..r.page_count).collect();
        return finish_ocr(path, c, all, opts);
    }
    // Text PDF with some image-only pages (pdf-inspector reports them 0-indexed or 1-indexed by
    // version; normalise against the page count).
    let mut need: Vec<u32> = r.pages_needing_ocr.clone();
    if need.iter().any(|p| *p >= r.page_count) || (need.contains(&r.page_count) && !need.contains(&0)) {
        need = need.into_iter().map(|p| p.saturating_sub(1)).collect();
    }
    finish_ocr(path, c, need, opts)
}

/// pdfium text for every page; returns text and the 0-indexed pages that had no text.
fn pdfium_pages_text(path: &Path) -> R<(String, Vec<u32>)> {
    use pdfium_render::prelude::*;
    let lib = crate::media::pdfium().map_err(|e| e.to_string())?;
    let _g = crate::media::PDFIUM_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let doc = lib.load_pdf_from_file(path, None).map_err(|e| format!("pdfium load: {e}"))?;
    let (mut out, mut empty) = (String::new(), vec![]);
    for (i, p) in doc.pages().iter().enumerate() {
        let t = p.text().map(|t| t.all()).unwrap_or_default();
        if t.split_whitespace().next().is_none() {
            empty.push(i as u32);
        }
        out.push_str(&t);
        out.push_str("\n\n");
    }
    Ok((out, empty))
}

/// OCR the given 0-indexed pages (up to opts.ocr_pages) and append their text with page markers.
fn finish_ocr(path: &Path, mut c: Converted, pages: Vec<u32>, opts: Opts) -> R<Converted> {
    if pages.is_empty() {
        return Ok(c);
    }
    let todo: Vec<u32> = pages.iter().copied().take(opts.ocr_pages).collect();
    c.pages_without_text = pages.len() - todo.len();
    if todo.is_empty() {
        return Ok(c);
    }
    let texts = ocr_pdf_pages(path, &todo)?;
    let was_empty = c.md.split_whitespace().next().is_none();
    let mut added = 0;
    for (p, t) in todo.iter().zip(texts) {
        if t.split_whitespace().next().is_some() {
            c.md.push_str(&format!("\n\n[page {} (OCR)]\n{}", p + 1, t.trim()));
            added += 1;
        } else {
            c.pages_without_text += 1;
        }
    }
    c.ocr_pages = added;
    if added > 0 && was_empty {
        c.tool = "tesseract";
    } else if added > 0 {
        c.tool = match c.tool {
            "pdf-inspector" => "pdf-inspector+ocr",
            "pdfium-text" => "pdfium-text+ocr",
            t => t,
        };
    }
    Ok(c)
}

/// Render pages at OCR_DPI (grayscale) with pdfium, OCR each with tesseract (plus a rotated read when
/// OSD proposes one), single-threaded per page; parallelism comes from several pages and many files.
/// Engine choice: benchmarked on real PMC supplement pages against OnnxTR (fast/accurate), ocrs and
/// granite-docling; tesseract had the best word F1 / CER at the lowest CPU (~1.7 s/page).
fn ocr_pdf_pages(path: &Path, pages: &[u32]) -> R<Vec<String>> {
    use pdfium_render::prelude::*;
    let dir = tempfile::Builder::new().prefix("pigmed-ocr-").tempdir().map_err(|e| e.to_string())?;
    let mut pngs = vec![];
    {
        let lib = crate::media::pdfium().map_err(|e| e.to_string())?;
        let _g = crate::media::PDFIUM_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let doc = lib.load_pdf_from_file(path, None).map_err(|e| format!("pdfium load: {e}"))?;
        for &i in pages {
            let Ok(page) = doc.pages().get(i as u16) else { continue };
            let (w, h) = (page.width().value as f64, page.height().value as f64);
            // Never above ~4400 px on the long side (huge poster pages).
            let scale = (OCR_DPI / 72.0f64).min(4400.0 / w.max(h).max(1.0));
            let cfg = PdfRenderConfig::new()
                .set_target_width(((w * scale).ceil() as i32).max(1))
                .set_target_height(((h * scale).ceil() as i32).max(1))
                .render_form_data(true);
            let Ok(bmp) = page.render_with_config(&cfg) else { continue };
            let img = bmp.as_image().into_luma8();
            let p = dir.path().join(format!("p{i}.png"));
            if img.save(&p).is_ok() {
                pngs.push((i, p));
            }
        }
    }
    // Up to OCR_PAGE_THREADS tesseract processes per file, so a long scan is not one serial tail.
    const OCR_PAGE_THREADS: usize = 4;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(vec![String::new(); pages.len()]);
    std::thread::scope(|sc| {
        for _ in 0..OCR_PAGE_THREADS.min(pngs.len()) {
            sc.spawn(|| loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some((i, png)) = pngs.get(k) else { break };
                // One tesseract run writes both the text and a TSV with per-word confidences.
                let ocr = |f: &Path| -> Option<(String, usize)> {
                    let base = f.with_extension("ocr");
                    let args: Vec<String> = vec!["tesseract".into(), f.to_string_lossy().into_owned(), base.to_string_lossy().into_owned(),
                        "-l".into(), "eng".into(), "--psm".into(), "3".into(), "txt".into(), "tsv".into()];
                    crate::media::run_tool(&args, 120).ok()?;
                    let text = std::fs::read_to_string(base.with_extension("ocr.txt")).ok()?;
                    let q = std::fs::read_to_string(base.with_extension("ocr.tsv")).map(|t| ocr_quality(&t)).unwrap_or(0);
                    Some((text, q))
                };
                let mut best = ocr(png);
                if let Some(rp) = rotated_candidate(png) {
                    if let Some((t, q)) = ocr(&rp) {
                        if q > best.as_ref().map_or(0, |b| b.1) {
                            best = Some((t, q));
                        }
                    }
                }
                if let Some((t, _)) = best {
                    if let Some(slot) = pages.iter().position(|p| p == i) {
                        results.lock().unwrap()[slot] = t;
                    }
                }
            });
        }
    });
    Ok(results.into_inner().unwrap())
}

// 300 dpi. On born-digital pages rendered then OCR'd, 200 dpi scored slightly better (F1 0.93 vs 0.90),
// but OCR only runs on image-only pages, and on real scanned PMC supplements 300 dpi yields 27% to 3x
// more confident text (fine detail of the embedded scan survives). ~4.8 s convert time per OCR page
// in the supplement audit; the rotated second read (only when OSD proposes one) adds ~1%.
const OCR_DPI: f64 = 300.0;

/// Parse tesseract `--psm 0` output: the clockwise rotation that makes the page upright, if confident.
fn osd_rotation(out: &str) -> Option<u32> {
    let field = |k: &str| out.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
    let rot: u32 = field("Rotate:")?.parse().ok()?;
    let conf: f64 = field("Orientation confidence:")?.parse().ok()?;
    (matches!(rot, 90 | 180 | 270) && conf >= 1.0).then_some(rot)
}

/// Rotated pages (landscape tables in portrait PDFs, sideways scans) OCR as garbage. Tesseract OSD
/// proposes a rotation, but it is unreliable on number-heavy tables and charts (wrong 180s at
/// confidence 2-6 on upright pages; right 270s at 6). So a proposal only adds a rotated candidate;
/// the caller OCRs both and keeps whichever reads as more real words.
fn rotated_candidate(png: &Path) -> Option<std::path::PathBuf> {
    let args: Vec<String> = vec!["tesseract".into(), png.to_string_lossy().into_owned(), "stdout".into(), "--psm".into(), "0".into()];
    let b = crate::media::run_tool(&args, 60).ok()?;
    let rot = osd_rotation(&String::from_utf8_lossy(&b))?;
    let img = image::open(png).ok()?;
    let img = match rot {
        90 => img.rotate90(),
        180 => img.rotate180(),
        _ => img.rotate270(),
    };
    let out = png.with_extension(format!("r{rot}.png"));
    img.save(&out).ok()?;
    Some(out)
}

/// Confidence-weighted size of an OCR result from tesseract's TSV: characters in words read with
/// confidence >= 60. Text OCR'd at the wrong orientation still yields letters, but at low confidence.
fn ocr_quality(tsv: &str) -> usize {
    tsv.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let conf: f64 = f.get(10)?.parse().ok()?;
            let word = f.get(11)?.trim();
            (conf >= 60.0 && !word.is_empty()).then(|| word.chars().count())
        })
        .sum()
}

/// Paragraph text of a .docx / .pptx straight from the package XML (no images, no layout).
fn ooxml_text(path: &Path, f: anydoc::Format) -> Option<String> {
    use std::io::Read;
    let (prefix, para) = match f {
        anydoc::Format::Docx => ("word/", "p"),
        anydoc::Format::Pptx => ("ppt/slides/slide", "p"),
        _ => return None,
    };
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    let mut names: Vec<String> = z.file_names().filter(|n| n.starts_with(prefix) && n.ends_with(".xml")
        && (f != anydoc::Format::Docx || matches!(*n, "word/document.xml" | "word/footnotes.xml" | "word/endnotes.xml"))).map(String::from).collect();
    names.sort_by_key(|n| n.trim_start_matches(prefix).trim_end_matches(".xml").parse::<u32>().unwrap_or(0));
    let mut out = String::new();
    for n in names {
        let mut xml = String::new();
        z.by_name(&n).ok()?.read_to_string(&mut xml).ok()?;
        let Ok(doc) = roxmltree::Document::parse(&xml) else { continue };
        for p in doc.descendants().filter(|x| x.tag_name().name() == para) {
            let t: String = p.descendants().filter(|x| x.tag_name().name() == "t").filter_map(|x| x.text()).collect();
            if !t.trim().is_empty() {
                out.push_str(t.trim());
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out.split_whitespace().next().is_some().then_some(out)
}

// ---------------------------------------------------------------- spreadsheets

/// Legacy (BIFF/OLE, non-ZIP) workbooks: calamine's .xls reader allocates without bound on some PMC files (e.g. the
/// supplements of PMC2220035, PMC2215774, PMC2225505: 20-100 KB files that reach >180 GB RSS on both the anydoc and the
/// calamine path). They are converted in a child process (this binary's `convert-file`) under RLIMIT_AS and a timeout, so a
/// bad file becomes a failed supplement instead of an OOM-killed ingest.
pub const ISOLATE_MEM_BYTES: u64 = 8 << 30;
pub const ISOLATE_SECS: u64 = 120;
const ISOLATE_ENV: &str = "PIGMED_CONVERT_ISOLATED";

fn excel_isolated(path: &Path, ext: &str) -> R<Converted> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().map_err(|e| format!("isolate: current_exe: {e}"))?;
    let md_out = path.with_extension("isolated.md");
    let mut cmd = Command::new(exe);
    cmd.arg("convert-file").arg(path).arg("--ext").arg(ext).arg("--md-out").arg(&md_out)
        .env(ISOLATE_ENV, "1").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            let lim = libc::rlimit { rlim_cur: ISOLATE_MEM_BYTES as libc::rlim_t, rlim_max: ISOLATE_MEM_BYTES as libc::rlim_t };
            if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // An aborting child must not write a multi-GB core file into the ingest working directory.
            let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if libc::setrlimit(libc::RLIMIT_CORE, &no_core) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("isolate: spawn: {e}"))?;
    let t0 = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if t0.elapsed().as_secs() >= ISOLATE_SECS => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&md_out);
                return Err(format!("isolated spreadsheet conversion timed out after {ISOLATE_SECS} s"));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => return Err(format!("isolate: wait: {e}")),
        }
    };
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        use std::io::Read;
        let _ = so.read_to_string(&mut out);
    }
    let md = std::fs::read_to_string(&md_out).ok();
    let _ = std::fs::remove_file(&md_out);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or(serde_json::Value::Null);
    match (status.success(), v["ok"].as_bool(), md) {
        (true, Some(true), Some(md)) => Ok(Converted::new(md, if v["tool"] == "anydoc" { "anydoc" } else { "calamine" }, "excel")),
        (true, Some(false), _) => Err(v["error"].as_str().unwrap_or("isolated conversion failed").to_string()),
        _ => Err(format!("isolated spreadsheet conversion failed ({}; memory cap {} GiB)", status, ISOLATE_MEM_BYTES >> 30)),
    }
}

fn excel(path: &Path, ext: &str, size: u64) -> R<Converted> {
    let is_zip = {
        use std::io::Read;
        let mut b = [0u8; 2];
        std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut b)).map(|_| &b == b"PK").unwrap_or(false)
    };
    if !is_zip && std::env::var_os(ISOLATE_ENV).is_none() {
        return excel_isolated(path, ext);
    }
    excel_inline(path, ext, size)
}

fn excel_inline(path: &Path, ext: &str, size: u64) -> R<Converted> {
    if size <= EXCEL_ANYDOC_MAX {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        match anydoc::to_markdown_bytes(&bytes, anydoc::Format::Excel) {
            Ok(md) => return Ok(Converted::new(md, "anydoc", "excel")),
            Err(e) => {
                let msg = format!("{e}");
                return excel_calamine(path, ext).map_err(|c| format!("anydoc: {msg}; calamine: {c}"));
            }
        }
    }
    excel_calamine(path, ext)
}

fn cell_text(v: &calamine::DataRef<'_>) -> String {
    use calamine::DataRef as D;
    match v {
        D::Empty => String::new(),
        D::String(s) | D::DateTimeIso(s) | D::DurationIso(s) => s.clone(),
        D::SharedString(s) => s.to_string(),
        D::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 { format!("{}", *f as i64) } else { format!("{f}") }
        }
        D::Int(i) => i.to_string(),
        D::Bool(b) => b.to_string(),
        D::DateTime(d) => format!("{d}"),
        D::Error(e) => format!("#{e:?}"),
    }
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Debug entry (`pigmed-rs convert-file --calamine-only`).
pub fn excel_calamine_pub(path: &Path, ext: &str) -> R<Converted> {
    excel_calamine(path, ext)
}

/// calamine fallback. `.xlsx`/`.xlsm` stream cell by cell (bounded memory); others load per sheet.
/// Markdown is kept up to EXCEL_MD_CAP; the trailing note records total rows so cards stay honest.
fn excel_calamine(path: &Path, ext: &str) -> R<Converted> {
    use calamine::{open_workbook, open_workbook_auto, Reader, Xlsx};
    let mut md = String::new();
    let (mut rows_total, mut capped) = (0usize, false);
    let mut push_row = |md: &mut String, cells: &[String], first: bool, capped: &mut bool| {
        if *capped {
            return;
        }
        let line = format!("| {} |\n", cells.iter().map(|c| md_cell(c)).collect::<Vec<_>>().join(" | "));
        md.push_str(&line);
        if first {
            md.push_str(&format!("|{}\n", " --- |".repeat(cells.len().max(1))));
        }
        if md.len() > EXCEL_MD_CAP {
            *capped = true;
        }
    };
    let streamable = matches!(ext, ".xlsx" | ".xlsm") || std::fs::read(path).map(|b| b.starts_with(b"PK")).unwrap_or(false);
    if streamable {
        let mut wb: Xlsx<_> = open_workbook(path).map_err(|e| format!("calamine: {e}"))?;
        for name in wb.sheet_names().to_owned() {
            md.push_str(&format!("\n## {name}\n\n"));
            let mut r = wb.worksheet_cells_reader(&name).map_err(|e| format!("calamine sheet {name}: {e}"))?;
            let (mut cur, mut cells, mut first) = (None::<u32>, Vec::<String>::new(), true);
            while let Some(cell) = r.next_cell().map_err(|e| format!("calamine cell: {e}"))? {
                let (row, col) = cell.get_position();
                if cur != Some(row) {
                    if cur.is_some() && cells.iter().any(|c| !c.is_empty()) {
                        push_row(&mut md, &cells, first, &mut capped);
                        first = false;
                        rows_total += 1;
                    }
                    cur = Some(row);
                    cells.clear();
                }
                let t = cell_text(cell.get_value());
                if col as usize >= cells.len() {
                    if col > 2000 {
                        continue; // pathological sparse columns
                    }
                    cells.resize(col as usize + 1, String::new());
                }
                cells[col as usize] = t;
            }
            if cells.iter().any(|c| !c.is_empty()) {
                push_row(&mut md, &cells, first, &mut capped);
                rows_total += 1;
            }
        }
    } else {
        let mut wb = open_workbook_auto(path).map_err(|e| format!("calamine: {e}"))?;
        for name in wb.sheet_names().to_owned() {
            md.push_str(&format!("\n## {name}\n\n"));
            let range = wb.worksheet_range(&name).map_err(|e| format!("calamine sheet {name}: {e}"))?;
            let mut first = true;
            for row in range.rows() {
                let cells: Vec<String> = row.iter().map(|d| d.to_string()).collect();
                if cells.iter().any(|c| !c.is_empty()) {
                    push_row(&mut md, &cells, first, &mut capped);
                    first = false;
                    rows_total += 1;
                }
            }
        }
    }
    if capped {
        md.push_str(&format!("\n[workbook truncated in this rendering: {rows_total} non-empty rows in total]\n"));
    }
    Ok(Converted::new(md, "calamine", "excel"))
}

// ---------------------------------------------------------------- HTML

/// Readable text from HTML: drops script/style, keeps block structure as lines and table cells as
/// `|`-separated columns, decodes entities.
pub fn html_to_text(s: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<[Regex; 6]> = OnceLock::new();
    let re = RE.get_or_init(|| {
        [
            Regex::new(r"(?is)<(script|style|noscript|template)\b.*?</(script|style|noscript|template)\s*>").unwrap(),
            Regex::new(r"(?s)<!--.*?-->").unwrap(),
            Regex::new(r"(?i)</?(p|div|br|tr|li|h[1-6]|table|thead|tbody|section|article|ul|ol|dl|dt|dd|title|header|footer|blockquote|pre|hr)\b[^>]*>").unwrap(),
            Regex::new(r"(?i)</t[dh]\s*>").unwrap(),
            Regex::new(r"(?s)<[^>]*>").unwrap(),
            Regex::new(r"&(#x[0-9a-fA-F]+|#[0-9]+|[a-zA-Z]+);").unwrap(),
        ]
    });
    let t = re[0].replace_all(s, " ");
    let t = re[1].replace_all(&t, " ");
    let t = re[2].replace_all(&t, "\n");
    let t = re[3].replace_all(&t, " | ");
    let t = re[4].replace_all(&t, "");
    let t = re[5].replace_all(&t, |c: &regex::Captures| {
        let e = &c[1];
        let ch = if let Some(h) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
            u32::from_str_radix(h, 16).ok().and_then(char::from_u32)
        } else if let Some(d) = e.strip_prefix('#') {
            d.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            match e {
                "amp" => Some('&'), "lt" => Some('<'), "gt" => Some('>'), "quot" => Some('"'), "apos" => Some('\''),
                "nbsp" => Some(' '), "ndash" => Some('–'), "mdash" => Some('—'), "deg" => Some('°'), "plusmn" => Some('±'),
                "micro" => Some('µ'), "times" => Some('×'), "alpha" => Some('α'), "beta" => Some('β'), "gamma" => Some('γ'),
                _ => None,
            }
        };
        ch.map(|c| c.to_string()).unwrap_or_else(|| c[0].to_string())
    });
    let mut out = String::new();
    let mut blank = false;
    for line in t.lines() {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let l = l.trim_matches(|c: char| c == '|' || c == ' ');
        if l.is_empty() {
            if !blank && !out.is_empty() {
                out.push('\n');
            }
            blank = true;
        } else {
            out.push_str(l);
            out.push('\n');
            blank = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn osd_parse() {
        let o = "Page number: 0\nOrientation in degrees: 270\nRotate: 90\nOrientation confidence: 15.27\nScript: Latin\n";
        assert_eq!(osd_rotation(o), Some(90));
        assert_eq!(osd_rotation("Rotate: 0\nOrientation confidence: 17.5\n"), None);
        assert_eq!(osd_rotation("Rotate: 180\nOrientation confidence: 0.4\n"), None);
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t0\t0\t9\t9\t96.1\tTable\n5\t1\t1\t1\t1\t2\t0\t0\t9\t9\t31.0\toluasoujed\n4\t1\t1\t1\t1\t0\t0\t0\t9\t9\t-1\t\n";
        assert_eq!(ocr_quality(tsv), 5);
        assert_eq!(osd_rotation("Too few characters. Skipping this page"), None);
    }

    #[test]
    fn html_text() {
        let h = "<html><head><style>p{}</style><title>T</title></head><body><p>A &amp; B&nbsp;&#945;</p><table><tr><td>x</td><td>1</td></tr></table><script>no()</script></body></html>";
        let t = html_to_text(h);
        assert!(t.contains("A & B α"), "{t}");
        assert!(t.contains("x | 1"), "{t}");
        assert!(!t.contains("no()") && !t.contains("p{}"));
    }
    #[test]
    fn sniffing() {
        assert_eq!(sniff(b"%PDF-1.7\n...", ".doc"), Kind::Pdf);
        assert_eq!(sniff(b"<html><body><table><tr><td>1</td></tr></table>", ".xls"), Kind::Html);
        assert_eq!(sniff(b"gene\tp\nA\t0.1\n", ".xls"), Kind::Text);
    }
}
