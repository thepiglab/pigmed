//! Figure views and PDF page renders. Sizes follow Pillow's `thumbnail` exactly so figure
//! unit IDs (which embed the transform) match the Python pipeline.
use anyhow::{anyhow, bail, Context, Result};
use fast_image_resize as fr;
use image::{codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder}, DynamicImage, ImageDecoder, ImageEncoder, ImageReader, RgbImage};
use pdfium_render::prelude::*;
use serde_json::{json, Value};
use std::io::{BufWriter, Cursor};
use std::path::Path;
use std::sync::OnceLock;

pub fn media_kind(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let file = path.rsplit('/').next().unwrap_or("");
    let ext = match file.rfind('.') {
        Some(i) if i > 0 => file[i..].to_lowercase(),
        _ => String::new(),
    };
    match ext.as_str() {
        ".mp4" | ".avi" | ".mov" | ".flv" | ".webm" | ".m4v" | ".mpg" | ".mpeg" | ".wmv" => "video",
        ".mp3" | ".wav" | ".flac" | ".ogg" | ".m4a" | ".aac" | ".aif" | ".aiff" => "audio",
        ".jpg" | ".jpeg" | ".png" | ".tif" | ".tiff" | ".webp" | ".gif" => "image",
        _ => "other",
    }
}

/// File extension (lower-case, with dot) of a URL path, or "" if none.
pub fn extension(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let file = path.rsplit('/').next().unwrap_or("");
    match file.rfind('.') {
        Some(i) if i > 0 => file[i..].to_lowercase(),
        _ => String::new(),
    }
}

/// Finer modality class for the per-file `media` rows (media_kind stays the 4-way v0.1 class).
pub fn modality(url: &str) -> &'static str {
    match extension(url).as_str() {
        ".jpg" | ".jpeg" | ".png" | ".tif" | ".tiff" | ".webp" | ".gif" | ".bmp" | ".eps" | ".svg" => "image",
        ".mp4" | ".avi" | ".mov" | ".flv" | ".webm" | ".m4v" | ".mpg" | ".mpeg" | ".wmv" | ".mkv" | ".3gp" => "video",
        ".mp3" | ".wav" | ".flac" | ".ogg" | ".m4a" | ".aac" | ".aif" | ".aiff" | ".wma" => "audio",
        ".pdf" => "pdf",
        ".doc" | ".docx" | ".odt" | ".rtf" | ".txt" | ".md" | ".tex" | ".htm" | ".html" => "document",
        ".xls" | ".xlsx" | ".ods" | ".csv" | ".tsv" => "spreadsheet",
        ".ppt" | ".pptx" | ".odp" | ".key" => "presentation",
        ".xml" | ".nxml" | ".json" => "structured_text",
        ".zip" | ".gz" | ".tgz" | ".tar" | ".7z" | ".rar" | ".bz2" | ".xz" => "archive",
        ".fasta" | ".fa" | ".fastq" | ".pdb" | ".cif" | ".sdf" | ".mol" | ".mol2" | ".nii" | ".dcm" | ".vcf"
        | ".bed" | ".gff" | ".gtf" | ".h5" | ".hdf5" | ".mat" | ".sav" | ".dta" | ".rds" | ".rdata" | ".npy"
        | ".r" | ".py" | ".m" | ".ipynb" | ".sql" => "data_or_code",
        _ => "other",
    }
}

/// Pillow `Image.thumbnail((longest, longest))` target size (no upscaling).
pub fn thumbnail_size(w: u32, h: u32, longest: u32) -> (u32, u32) {
    let (mut x, mut y) = (longest as f64, longest as f64);
    let aspect = w as f64 / h as f64;
    let round_aspect = |n: f64, key: &dyn Fn(f64) -> f64| -> f64 {
        let (a, b) = (n.floor(), n.ceil());
        let best = if key(b) < key(a) { b } else { a }; // min() keeps the first on ties
        best.max(1.0)
    };
    if x / y >= aspect {
        let yy = y;
        x = round_aspect(y * aspect, &|n| (aspect - n / yy).abs());
    } else {
        let xx = x;
        y = round_aspect(x / aspect, &|n| if n == 0.0 { 0.0 } else { (aspect - xx / n).abs() });
    }
    if x >= w as f64 && y >= h as f64 {
        return (w, h);
    }
    (x as u32, y as u32)
}

/// PNG deflate effort for page/figure views ($PIGMED_PNG_COMPRESSION = default | fast | best).
/// Always lossless: only file size and CPU change. Encoding was ~40% of full-media CPU at `default`.
fn png_quality() -> (CompressionType, PngFilter) {
    static Q: OnceLock<(CompressionType, PngFilter)> = OnceLock::new();
    *Q.get_or_init(|| match std::env::var("PIGMED_PNG_COMPRESSION").as_deref() {
        // Measured on 9,321 articles (full media, 32 procs): fast = -14% wall, -20% CPU but +63% bytes
        // (85 -> 139 GB); fast-nofilter is ~2.5x larger. Storage/upload dominate, so default stays.
        Ok("fast") => (CompressionType::Fast, PngFilter::Adaptive),
        Ok("best") => (CompressionType::Best, PngFilter::Adaptive),
        _ => (CompressionType::Default, PngFilter::Adaptive),
    })
}

fn encode_png(img: &RgbImage, dst: &Path) -> Result<()> {
    let f = std::fs::File::create(dst)?;
    let (c, flt) = png_quality();
    let enc = PngEncoder::new_with_quality(BufWriter::new(f), c, flt);
    enc.write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8)?;
    Ok(())
}

fn resize_rgb(img: RgbImage, w: u32, h: u32) -> Result<RgbImage> {
    if (img.width(), img.height()) == (w, h) {
        return Ok(img);
    }
    let src = fr::images::Image::from_vec_u8(img.width(), img.height(), img.into_raw(), fr::PixelType::U8x3)?;
    let mut dst = fr::images::Image::new(w, h, fr::PixelType::U8x3);
    let mut r = fr::Resizer::new();
    r.resize(&src, &mut dst, &fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3)))?;
    RgbImage::from_raw(w, h, dst.into_vec()).ok_or_else(|| anyhow!("resize buffer"))
}

/// Decode (EXIF-orient, RGB), thumbnail to `longest`, write PNG. Returns the transform record.
pub fn image_view(src: &Path, dst: &Path, longest: u32, max_pixels: u64) -> Result<Value> {
    let bytes = std::fs::read(src)?;
    let reader = ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
    let mut dec = reader.into_decoder().context("unsupported or corrupt image")?;
    let (w, h) = dec.dimensions();
    if (w as u64) * (h as u64) > max_pixels {
        bail!("image {w}x{h} exceeds max_pixels={max_pixels}");
    }
    let orientation = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut im = DynamicImage::from_decoder(dec)?;
    im.apply_orientation(orientation);
    // Transparent line art must land on white, not black (Pillow's convert("RGB") drops alpha; audit #5).
    // 16-bit and CMYK sources are scaled/converted by the decoder.
    let rgb = if im.color().has_alpha() {
        let rgba = im.into_rgba8();
        let (w, h) = rgba.dimensions();
        let mut out = RgbImage::new(w, h);
        for (o, p) in out.pixels_mut().zip(rgba.pixels()) {
            let a = p[3] as u32;
            for c in 0..3 {
                o[c] = ((p[c] as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
            }
        }
        out
    } else {
        im.into_rgb8()
    };
    let original = [rgb.width(), rgb.height()];
    let (tw, th) = thumbnail_size(original[0], original[1], longest);
    let out = resize_rgb(rgb, tw, th)?;
    encode_png(&out, dst)?;
    Ok(json!({"original_size": original, "rendered_size": [tw, th], "longest_side": longest}))
}

static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

/// Bind pdfium from $PIGMED_PDFIUM (path to libpdfium.so) or the pypdfium2 wheel in $PIGMED_VENV.
pub fn pdfium() -> Result<&'static Pdfium> {
    PDFIUM
        .get_or_init(|| {
            let mut candidates = Vec::new();
            if let Ok(p) = std::env::var("PIGMED_PDFIUM") {
                candidates.push(p);
            }
            if let Ok(v) = std::env::var("UV_PROJECT_ENVIRONMENT").or_else(|_| std::env::var("PIGMED_VENV")) {
                if let Ok(rd) = std::fs::read_dir(format!("{v}/lib")) {
                    for py in rd.flatten() {
                        candidates.push(format!("{}/site-packages/pypdfium2_raw/libpdfium.so", py.path().display()));
                    }
                }
            }
            for c in &candidates {
                if Path::new(c).exists() {
                    return Pdfium::bind_to_library(c).map(Pdfium::new).map_err(|e| format!("{c}: {e}"));
                }
            }
            Pdfium::bind_to_system_library().map(Pdfium::new).map_err(|e| format!("no libpdfium found ({candidates:?}): {e}"))
        })
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

pub struct Page {
    pub path: std::path::PathBuf,
    pub locator: Value,
}

/// Render every page so its longest side is `longest` px. Refuses (no silent truncation)
/// when the document has more than `max_pages`.
/// pdfium is not re-entrant: documents opened on different threads interleave global state even when
/// each FFI call is individually locked. One document at a time per process; PNG encoding of the
/// rendered pages still overlaps with other articles' downloads/decodes.
pub(crate) static PDFIUM_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn render_pdf(pdf: &Path, out: &Path, longest: u32, max_pages: usize) -> Result<Vec<Page>> {
    std::fs::create_dir_all(out)?;
    let raw = {
        let _gate = PDFIUM_GATE.lock().unwrap_or_else(|e| e.into_inner());
        render_raw(pdf, longest, max_pages)?
    };
    // Encode outside the gate (the expensive part in the Python profile: 59% of pilot time).
    let mut res = Vec::with_capacity(raw.len());
    for (i, (rgb, locator)) in raw.into_iter().enumerate() {
        let dst = out.join(format!("page-{:04}.png", i + 1));
        encode_png(&rgb, &dst)?;
        res.push(Page { path: dst, locator });
    }
    Ok(res)
}

fn render_raw(pdf: &Path, longest: u32, max_pages: usize) -> Result<Vec<(RgbImage, Value)>> {
    let lib = pdfium()?;
    let doc = lib.load_pdf_from_file(pdf, None).map_err(|e| anyhow!("pdfium load: {e}"))?;
    let n = doc.pages().len() as usize;
    if n > max_pages {
        bail!("PDF has {n} pages, exceeding configured max_pages={max_pages}; not silently truncated");
    }
    let mut res = Vec::with_capacity(n);
    for (i, page) in doc.pages().iter().enumerate() {
        let (w, h) = (page.width().value, page.height().value);
        // pypdfium2: scale = longest / max(w, h) in double precision, bitmap = ceil(side * scale).
        let (wd, hd) = (w as f64, h as f64);
        let scale = longest as f64 / wd.max(hd);
        let cfg = PdfRenderConfig::new()
            .set_target_width(((wd * scale).ceil() as i32).max(1))
            .set_target_height(((hd * scale).ceil() as i32).max(1))
            .render_form_data(true)
            .render_annotations(true);
        let bmp = page.render_with_config(&cfg).map_err(|e| anyhow!("pdfium render p{}: {e}", i + 1))?;
        let rgb = bmp.as_image().into_rgb8();
        res.push((rgb, json!({"page": i + 1, "page_size_pt": [w as f64, h as f64], "longest_side": longest})));
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pillow_thumbnail_sizes() {
        // Values checked against Pillow 13: Image.new("RGB",(w,h)).thumbnail((2048,2048))
        assert_eq!(thumbnail_size(4000, 3000, 2048), (2048, 1536));
        assert_eq!(thumbnail_size(3000, 4000, 2048), (1536, 2048));
        assert_eq!(thumbnail_size(800, 600, 2048), (800, 600));
        assert_eq!(thumbnail_size(5000, 7, 2048), (2048, 3));
        assert_eq!(thumbnail_size(2049, 2048, 2048), (2048, 2047));
    }
    #[test]
    fn kinds() {
        assert_eq!(media_kind("s3://b/PMC1.1/a.MP4?md5=1"), "video");
        assert_eq!(media_kind("s3://b/PMC1.1/f1.jpg"), "image");
        assert_eq!(media_kind("s3://b/PMC1.1/data.xlsx"), "other");
    }
}

#[cfg(test)]
mod alpha_tests {
    use super::*;
    #[test]
    fn transparent_pixels_become_white_and_modalities() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.png");
        let mut im = image::RgbaImage::new(2, 1);
        im.put_pixel(0, 0, image::Rgba([0, 0, 0, 0])); // transparent black (line-art background)
        im.put_pixel(1, 0, image::Rgba([200, 10, 10, 255])); // opaque red
        im.save(&src).unwrap();
        let dst = dir.path().join("b.png");
        let t = image_view(&src, &dst, 2048, 1 << 30).unwrap();
        assert_eq!(t["rendered_size"], json!([2, 1]));
        let out = image::open(&dst).unwrap().into_rgb8();
        assert_eq!(out.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(out.get_pixel(1, 0).0, [200, 10, 10]);
        // 16-bit greyscale must not come out white.
        let g16 = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_pixel(1, 1, image::Luma([32768]));
        let s16 = dir.path().join("g.png");
        g16.save(&s16).unwrap();
        image_view(&s16, &dst, 2048, 1 << 30).unwrap();
        let v = image::open(&dst).unwrap().into_rgb8().get_pixel(0, 0).0[0];
        assert!((126..=130).contains(&v), "16-bit mid-grey mapped to {v}");
        assert_eq!(modality("s3://b/PMC1.1/mmc1.DOCX?md5=1"), "document");
        assert_eq!(modality("s3://b/PMC1.1/data.xlsx"), "spreadsheet");
        assert_eq!(modality("s3://b/PMC1.1/movie.mov"), "video");
        assert_eq!(modality("s3://b/PMC1.1/x.zip"), "archive");
    }
}

/// Run an external tool with a wall-clock limit; returns stdout. Port of `subprocess.run(..., timeout=)`.
pub(crate) fn run_tool(args: &[String], timeout_s: u64) -> Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(&args[0]).args(&args[1..]).env("OMP_THREAD_LIMIT", "1").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().with_context(|| format!("spawn {} (is it on PATH? e.g. `module load ffmpeg`)", args[0]))?;
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let out_t = std::thread::spawn(move || { let mut b = vec![]; let _ = std::io::Read::read_to_end(&mut so, &mut b); b });
    let err_t = std::thread::spawn(move || { let mut b = vec![]; let _ = std::io::Read::read_to_end(&mut se, &mut b); b });
    let t0 = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if t0.elapsed().as_secs() >= timeout_s {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} timed out after {timeout_s}s", args[0]);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let (out, err) = (out_t.join().unwrap_or_default(), err_t.join().unwrap_or_default());
    if !status.success() {
        bail!("{} failed ({status}): {}", args[0], String::from_utf8_lossy(&err).chars().take(300).collect::<String>());
    }
    Ok(out)
}

/// Port of `media.probe_media`: duration (s), has_audio, has_video via ffprobe.
pub fn probe_media(path: &Path) -> Result<(f64, bool, bool)> {
    let out = run_tool(&["ffprobe", "-v", "error", "-show_format", "-show_streams", "-of", "json"].iter().map(|s| s.to_string())
        .chain([path.to_string_lossy().into_owned()]).collect::<Vec<_>>(), 120)?;
    let info: Value = serde_json::from_slice(&out)?;
    let duration = match &info["format"]["duration"] {
        Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    };
    let streams = info["streams"].as_array().cloned().unwrap_or_default();
    let has = |t: &str| streams.iter().any(|x| x["codec_type"] == t);
    Ok((duration, has("audio"), has("video")))
}

/// Port of `media.time_windows`: [start, end) windows of `seconds` with `overlap`.
pub fn time_windows(duration: f64, seconds: f64, overlap: f64) -> Result<Vec<(f64, f64)>> {
    if seconds <= 0.0 || !(0.0..seconds).contains(&overlap) {
        bail!("Invalid clip length/overlap");
    }
    let (mut start, mut out) = (0.0f64, vec![]);
    while start < duration {
        let end = duration.min(start + seconds);
        out.push((start, end));
        if end == duration {
            break;
        }
        start = end - overlap;
    }
    Ok(out)
}

/// Python `str(float)` (shortest repr, always with a '.' or exponent) for ffmpeg arguments.
fn py_float(x: f64) -> String {
    format!("{x:?}")
}

/// Port of `media.make_clip`: 16 kHz mono PCM WAV for audio, visual-only H.264 (<=1280 wide) for video.
pub fn make_clip(path: &Path, dest: &Path, start: f64, end: f64, kind: &str) -> Result<()> {
    let mut args: Vec<String> = ["ffmpeg", "-nostdin", "-loglevel", "error", "-y", "-ss"].iter().map(|s| s.to_string()).collect();
    args.push(py_float(start));
    args.push("-i".into());
    args.push(path.to_string_lossy().into_owned());
    args.push("-t".into());
    args.push(py_float(end - start));
    if kind == "audio" {
        args.extend(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"].iter().map(|s| s.to_string()));
    } else {
        args.extend(["-an", "-vf", "scale='min(1280,iw)':-2", "-c:v", "libx264", "-preset", "fast", "-crf", "23"].iter().map(|s| s.to_string()));
    }
    args.push(dest.to_string_lossy().into_owned());
    run_tool(&args, 600).map(|_| ())
}
