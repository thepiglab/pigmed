//! Input preprocessing that mirrors the Hugging Face processors exactly (transformers 5.22.2):
//! tokenization (same `tokenizers` Rust library HF uses), Gemma 4 aspect-ratio resize + 16 px patches,
//! NeoMME pad-to-32 px patches with the `<doc><img>(<img>*W<row>)*H` layout and 2-axis positions.

use anyhow::{Context, Result, anyhow, bail};
use crate::pil_resize::{Filter, resize_rgb8};
use image::RgbImage;
use tokenizers::{Tokenizer, TruncationParams};

/// Decode an image file to RGB8 with the same C decoders the HF/torchvision path uses: JPEG via libjpeg-turbo
/// (mozjpeg fork: integer IDCT, fancy upsampling), WebP via libwebp; other formats via the `image` crate (no
/// decode-size limits). Alpha is dropped (PIL `convert("RGB")` semantics).
pub fn load_rgb(path: &std::path::Path) -> Result<RgbImage> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        let r = std::panic::catch_unwind(|| -> std::io::Result<RgbImage> {
            let d = mozjpeg::Decompress::new_mem(&data)?;
            let mut s = d.rgb()?;
            let (w, h) = (s.width() as u32, s.height() as u32);
            let px: Vec<[u8; 3]> = s.read_scanlines()?;
            s.finish()?;
            Ok(RgbImage::from_raw(w, h, px.into_iter().flatten().collect()).unwrap())
        });
        if let Ok(Ok(im)) = r {
            return Ok(im);
        } // e.g. CMYK JPEG: fall through to the image crate
    }
    if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        if let Some(w) = webp::Decoder::new(&data).decode() {
            let (wd, ht) = (w.width(), w.height());
            let raw: &[u8] = &w;
            let rgb: Vec<u8> = if w.is_alpha() { raw.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect() } else { raw.to_vec() };
            return RgbImage::from_raw(wd, ht, rgb).ok_or_else(|| anyhow!("webp buffer"));
        }
    }
    let mut r = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    r.no_limits();
    match r.decode() {
        Ok(im) => Ok(im.to_rgb8()),
        // e.g. palette TIFFs: the image crate's TIFF decoder rejects photometric "RGB palette"; FFmpeg reads them
        Err(e) => crate::media::decode_image_ffmpeg(path).with_context(|| format!("image crate: {e}; ffmpeg fallback")),
    }
}

pub fn load_tokenizer(path: &std::path::Path, max_len: Option<usize>) -> Result<Tokenizer> {
    let mut tok = Tokenizer::from_file(path).map_err(|e| anyhow!("tokenizer {}: {e}", path.display()))?;
    tok.with_padding(None);
    if let Some(m) = max_len {
        tok.with_truncation(Some(TruncationParams { max_length: m, ..Default::default() }))
            .map_err(|e| anyhow!("{e}"))?;
    } else {
        tok.with_truncation(None).map_err(|e| anyhow!("{e}"))?;
    }
    Ok(tok)
}

pub fn token_id(tok: &Tokenizer, s: &str) -> Result<i64> {
    tok.token_to_id(s).map(|x| x as i64).with_context(|| format!("token {s} not in vocab"))
}

/// Tokenize each text (sequentially; TOKENIZERS_PARALLELISM is off on the Python side too).
pub fn encode(tok: &Tokenizer, texts: &[String], add_special: bool) -> Result<Vec<Vec<i64>>> {
    texts
        .iter()
        .map(|t| {
            let e = tok.encode(t.as_str(), add_special).map_err(|e| anyhow!("{e}"))?;
            Ok(e.get_ids().iter().map(|&x| x as i64).collect())
        })
        .collect()
}

/// Right-pad to the longest row. Returns (ids, mask, seq_len).
pub fn pad(rows: &[Vec<i64>], pad_id: i64) -> (Vec<i64>, Vec<i64>, usize) {
    let s = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut ids = Vec::with_capacity(rows.len() * s);
    let mut mask = Vec::with_capacity(rows.len() * s);
    for r in rows {
        ids.extend_from_slice(r);
        ids.extend(std::iter::repeat_n(pad_id, s - r.len()));
        mask.extend(std::iter::repeat_n(1i64, r.len()));
        mask.extend(std::iter::repeat_n(0i64, s - r.len()));
    }
    (ids, mask, s)
}

fn resize_rgb(img: &RgbImage, w: u32, h: u32, filter: Filter) -> Result<RgbImage> {
    if img.width() == w && img.height() == h {
        return Ok(img.clone());
    }
    let out = resize_rgb8(img.as_raw(), img.width() as usize, img.height() as usize, w as usize, h as usize, filter);
    RgbImage::from_raw(w, h, out).ok_or_else(|| anyhow!("resize buffer"))
}

// ------------------------------------------------------------------------------------------- Gemma 4
pub const G_PATCH: usize = 16;
pub const G_POOL: usize = 3;
pub const G_SOFT: usize = 280;
pub const G_MAX_PATCHES: usize = G_SOFT * G_POOL * G_POOL; // 2520
pub const G_PATCH_DIM: usize = 3 * G_PATCH * G_PATCH; // 768

/// transformers.models.gemma4.image_processing_gemma4.get_aspect_ratio_preserving_size
pub fn gemma_target_size(h: usize, w: usize, max_patches: usize) -> Result<(usize, usize)> {
    let total = (h * w) as f64;
    let target_px = (max_patches * G_PATCH * G_PATCH) as f64;
    let f = (target_px / total).sqrt();
    let side = G_POOL * G_PATCH;
    let mut th = ((f * h as f64) / side as f64).floor() as usize * side;
    let mut tw = ((f * w as f64) / side as f64).floor() as usize * side;
    if th == 0 && tw == 0 {
        bail!("0x0 resize");
    }
    let max_side = (max_patches / (G_POOL * G_POOL)) * side;
    if th == 0 {
        th = side;
        tw = ((w / h) * side).min(max_side);
    } else if tw == 0 {
        tw = side;
        th = ((h / w) * side).min(max_side);
    }
    if (th * tw) as f64 > target_px {
        bail!("resize exceeds patch budget");
    }
    Ok((th, tw))
}

pub struct GemmaImage {
    pub patches: Vec<f32>,   // [2520 * 768], padded with zeros
    pub positions: Vec<i64>, // [2520 * 2] (x, y), padded with -1
    pub n_soft: usize,
}

pub fn gemma_image(img: &RgbImage) -> Result<GemmaImage> {
    gemma_image_budget(img, G_SOFT)
}

/// Same as the HF Gemma 4 image/video processors with `max_soft_tokens = soft` (70/140/280/560/1120).
pub fn gemma_image_budget(img: &RgbImage, soft: usize) -> Result<GemmaImage> {
    let max_patches = soft * G_POOL * G_POOL;
    if std::env::var("PIGMED_PIL_WEIGHTS").is_ok() {
        crate::pil_resize::TORCH_INT16_WEIGHTS.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    let (th, tw) = gemma_target_size(img.height() as usize, img.width() as usize, max_patches)?;
    // Pillow's antialiased bicubic (a = -0.5) = torchvision's uint8 antialias bicubic used by the HF processor.
    let im = resize_rgb(img, tw as u32, th as u32, Filter::Bicubic)?;
    let (ph, pw) = (th / G_PATCH, tw / G_PATCH);
    let n = ph * pw;
    let mut patches = vec![0f32; max_patches * G_PATCH_DIM];
    let mut positions = vec![-1i64; max_patches * 2];
    let raw = im.as_raw();
    let scale = 1.0f32 / 255.0;
    for r in 0..ph {
        for c in 0..pw {
            let p = r * pw + c;
            let dst = &mut patches[p * G_PATCH_DIM..(p + 1) * G_PATCH_DIM];
            let mut k = 0;
            for py in 0..G_PATCH {
                let row = (r * G_PATCH + py) * tw * 3;
                for px in 0..G_PATCH {
                    let o = row + (c * G_PATCH + px) * 3;
                    dst[k] = raw[o] as f32 * scale;
                    dst[k + 1] = raw[o + 1] as f32 * scale;
                    dst[k + 2] = raw[o + 2] as f32 * scale;
                    k += 3;
                }
            }
            positions[p * 2] = c as i64;
            positions[p * 2 + 1] = r as i64;
        }
    }
    if let Ok(dir) = std::env::var("PIGMED_DUMP_PREP") {
        // debug: dump the real (unpadded) patches for diffing against the HF processor
        static K: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let k = K.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let a = ndarray::Array2::from_shape_vec((n, G_PATCH_DIM), patches[..n * G_PATCH_DIM].to_vec())?;
        ndarray_npy::write_npy(std::path::Path::new(&dir).join(format!("gemma-prep-{k:03}.npy")), &a)?;
    }
    Ok(GemmaImage { patches, positions, n_soft: n / (G_POOL * G_POOL) })
}

// ------------------------------------------------------------------------------------------- NeoMME
pub const N_PATCH: usize = 32;
pub const N_PATCH_DIM: usize = 3 * N_PATCH * N_PATCH; // 3072
pub const N_MAX_SIDE: usize = 2048;

pub struct NeoImage {
    pub gh: usize,
    pub gw: usize,
    pub patches: Vec<f32>, // [gh*gw * 3072]
}

/// NeoMMEImageProcessor: optional longest-side cap (bilinear, antialias), pad bottom/right with 0 to a
/// multiple of 32, rescale 1/255, normalize mean 0.5 std 0.5, row-major (py, px, c) patches.
pub fn neomme_image(img: &RgbImage) -> Result<NeoImage> {
    let (h, w) = (img.height() as usize, img.width() as usize);
    let im = if h.max(w) > N_MAX_SIDE {
        // get_resize_output_size with max_side only
        let s = N_MAX_SIDE as f64 / h.max(w) as f64;
        let (mut rh, mut rw) = (((h as f64) * s).round().max(1.0) as usize, ((w as f64) * s).round().max(1.0) as usize);
        if rh.max(rw) > N_MAX_SIDE {
            rh = ((h as f64) * s).floor().max(1.0) as usize;
            rw = ((w as f64) * s).floor().max(1.0) as usize;
        }
        resize_rgb(img, rw as u32, rh as u32, Filter::Bilinear)?
    } else {
        img.clone()
    };
    let (h, w) = (im.height() as usize, im.width() as usize);
    let (gh, gw) = (h.div_ceil(N_PATCH), w.div_ceil(N_PATCH));
    let raw = im.as_raw();
    let mut patches = vec![0f32; gh * gw * N_PATCH_DIM];
    let pad_val = (0.0f32 - 0.5) / 0.5;
    for r in 0..gh {
        for c in 0..gw {
            let dst = &mut patches[(r * gw + c) * N_PATCH_DIM..(r * gw + c + 1) * N_PATCH_DIM];
            let mut k = 0;
            for py in 0..N_PATCH {
                let y = r * N_PATCH + py;
                for px in 0..N_PATCH {
                    let x = c * N_PATCH + px;
                    if y < h && x < w {
                        let o = (y * w + x) * 3;
                        for ch in 0..3 {
                            dst[k + ch] = (raw[o + ch] as f32 * (1.0 / 255.0) - 0.5) / 0.5;
                        }
                    } else {
                        dst[k] = pad_val;
                        dst[k + 1] = pad_val;
                        dst[k + 2] = pad_val;
                    }
                    k += 3;
                }
            }
        }
    }
    Ok(NeoImage { gh, gw, patches })
}

/// Token ids, 2-axis positions ([seq] x 2, (row, col)) and patch flags for one NeoMME page image.
pub fn neomme_image_layout(gh: usize, gw: usize, doc: i64, img: i64, row: i64) -> (Vec<i64>, Vec<[i64; 2]>, Vec<bool>) {
    let s = 2 + gh * (gw + 1);
    let mut ids = Vec::with_capacity(s);
    let mut pos = Vec::with_capacity(s);
    let mut is_patch = Vec::with_capacity(s);
    ids.extend([doc, img]);
    pos.extend([[0, 0], [1, 1]]);
    is_patch.extend([false, false]);
    for r in 0..gh {
        for c in 0..=gw {
            ids.push(if c < gw { img } else { row });
            pos.push([2 + r as i64, 2 + c as i64]);
            is_patch.push(c < gw);
        }
    }
    (ids, pos, is_patch)
}
