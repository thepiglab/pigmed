//! Resumable S3 figure fetcher for one `figures.jsonl.gz` shard. For each figure: GET the public PMC Open Data object,
//! check md5 against the published `media.md5`, decode, convert to RGB, resize so the longest side <= max_side
//! (Lanczos3; never upscaled), encode JPEG at `quality`, write `<out>/<id>.jpg`. Every outcome is appended to
//! `<out>.done.tsv` (id, status, bytes_in, w, h) and flushed, so a restart skips finished figures; failures are not
//! retried within a run. At the end `<out>.figures.jsonl.gz` = the shard's rows with `image` set for fetched figures.

use crate::shard::S3_BASE;
use anyhow::{bail, Result};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, DynamicImage, GenericImageView};
use md5::{Digest, Md5};
use rayon::prelude::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub struct FetchArgs {
    pub figures: PathBuf,
    pub out: PathBuf,
    pub max_side: u32,
    pub quality: u8,
    pub threads: usize,
    /// at most this many figures from the shard (stable hash order of the figure id), for pilots
    pub limit: Option<usize>,
    /// read objects from this directory (<s3_key>) instead of S3 (tests, local mirrors)
    pub local: Option<PathBuf>,
}

/// Decode, RGB, downscale (longest side <= max_side), JPEG. Returns (bytes, w, h).
pub fn transcode(bytes: &[u8], max_side: u32, quality: u8) -> Result<(Vec<u8>, u32, u32)> {
    let mut lim = image::Limits::default();
    lim.max_alloc = Some(1 << 30);
    let mut rd = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    rd.limits(lim);
    let img = rd.decode()?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        bail!("empty image");
    }
    let img = if w.max(h) > max_side {
        let s = max_side as f64 / w.max(h) as f64;
        img.resize_exact(((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1), FilterType::Lanczos3)
    } else {
        img
    };
    let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
    let mut out = vec![];
    JpegEncoder::new_with_quality(&mut out, quality).encode_image(&rgb)?;
    Ok((out, rgb.width(), rgb.height()))
}

fn get(agent: &ureq::Agent, key: &str, local: Option<&Path>) -> Result<Vec<u8>> {
    if let Some(d) = local {
        return Ok(std::fs::read(d.join(key))?);
    }
    let url = format!("{S3_BASE}{key}");
    let mut last = String::new();
    for attempt in 0..4u32 {
        match agent.get(&url).call() {
            Ok(r) => {
                let mut b = vec![];
                r.into_reader().take(200 << 20).read_to_end(&mut b)?;
                return Ok(b);
            }
            Err(ureq::Error::Status(c @ (403 | 404), _)) => bail!("http{c}"),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(2u64.pow(attempt).min(15)));
    }
    bail!("net: {last}")
}

pub fn run(a: &FetchArgs) -> Result<Value> {
    std::fs::create_dir_all(&a.out)?;
    let done_path = a.out.with_extension("done.tsv");
    let mut done: HashMap<String, (String, u32, u32)> = HashMap::new();
    if done_path.exists() {
        for line in crate::io::reader(&done_path)?.lines() {
            let line = line?;
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() >= 5 {
                done.insert(f[0].to_string(), (f[1].to_string(), f[3].parse().unwrap_or(0), f[4].parse().unwrap_or(0)));
            }
        }
    }
    let mut rows: Vec<Value> = crate::io::reader(&a.figures)?.lines().map(|l| Ok(serde_json::from_str(&l?)?)).collect::<Result<_>>()?;
    if let Some(n) = a.limit {
        rows.sort_by_cached_key(|r| hex::encode(Md5::digest(format!("pilot|{}", r["id"].as_str().unwrap_or("")).as_bytes())));
        rows.truncate(n);
    }
    let todo: Vec<&Value> = rows.iter().filter(|r| !done.contains_key(r["id"].as_str().unwrap_or(""))).collect();
    let log = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&done_path)?);
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(120)).build();
    let pool = rayon::ThreadPoolBuilder::new().num_threads(a.threads).build()?;
    let fresh: Vec<(String, String, u32, u32)> = pool.install(|| todo.par_iter().map(|r| {
        let id = r["id"].as_str().unwrap_or("").to_string();
        let key = r["s3_key"].as_str().unwrap_or("");
        let want = r["md5"].as_str().unwrap_or("").to_lowercase();
        let (status, n, w, h) = match get(&agent, key, a.local.as_deref()) {
            Err(e) => (e.to_string().split(':').next().unwrap_or("error").to_string(), 0, 0, 0),
            Ok(b) => {
                let got = hex::encode(Md5::digest(&b));
                if !want.is_empty() && got != want {
                    ("md5_mismatch".to_string(), b.len(), 0, 0)
                } else {
                    match transcode(&b, a.max_side, a.quality) {
                        Err(_) => ("decode_error".to_string(), b.len(), 0, 0),
                        Ok((jpg, w, h)) => {
                            let p = a.out.join(format!("{id}.jpg"));
                            let tmp = p.with_extension("jpg.tmp");
                            match std::fs::write(&tmp, &jpg).and_then(|_| std::fs::rename(&tmp, &p)) {
                                Ok(_) => ("ok".to_string(), b.len(), w, h),
                                Err(_) => ("write_error".to_string(), b.len(), 0, 0),
                            }
                        }
                    }
                }
            }
        };
        if let Ok(mut f) = log.lock() {
            let _ = writeln!(f, "{id}\t{status}\t{n}\t{w}\t{h}");
            let _ = f.flush();
        }
        (id, status, w, h)
    }).collect());
    for (id, s, w, h) in fresh {
        done.insert(id, (s, w, h));
    }
    let part = a.out.file_name().and_then(|x| x.to_str()).unwrap_or("").to_string();
    let mut fw = crate::io::JsonlOut::create(&a.out.with_extension("figures.jsonl.gz"))?;
    let mut status: std::collections::BTreeMap<String, usize> = Default::default();
    for r in &rows {
        let id = r["id"].as_str().unwrap_or("");
        let Some((s, w, h)) = done.get(id) else { continue };
        *status.entry(s.clone()).or_default() += 1;
        if s == "ok" {
            let mut v = r.clone();
            v["image"] = json!(format!("{part}/{id}.jpg"));
            v["width"] = json!(w);
            v["height"] = json!(h);
            fw.put(&v)?;
        }
    }
    let n = fw.finish()?;
    Ok(json!({"figures": rows.len(), "fetched_this_run": todo.len(), "status": status, "with_image": n,
        "max_side": a.max_side, "quality": a.quality}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 255) as u8, (y % 255) as u8, 90, 255]));
        let mut b = vec![];
        DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut b), image::ImageFormat::Png).unwrap();
        b
    }

    #[test]
    fn transcode_downscales_to_rgb_jpeg() {
        let (j, w, h) = transcode(&png(2000, 1000), 896, 90).unwrap();
        assert_eq!((w, h), (896, 448));
        let back = image::load_from_memory(&j).unwrap();
        assert_eq!(back.color(), image::ColorType::Rgb8);
        let (_, w, h) = transcode(&png(300, 200), 896, 90).unwrap();
        assert_eq!((w, h), (300, 200), "never upscaled");
    }

    #[test]
    fn fetch_is_resumable_and_checks_md5() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("s3");
        std::fs::create_dir_all(src.join("PMC1.1")).unwrap();
        let (a, b) = (png(1200, 900), png(64, 64));
        std::fs::write(src.join("PMC1.1/a.png"), &a).unwrap();
        std::fs::write(src.join("PMC1.1/b.png"), &b).unwrap();
        std::fs::write(src.join("PMC1.1/c.jpg"), b"not an image").unwrap();
        let rows = [json!({"id": "m1", "s3_key": "PMC1.1/a.png", "md5": hex::encode(Md5::digest(&a))}),
            json!({"id": "m2", "s3_key": "PMC1.1/b.png", "md5": "00000000000000000000000000000000"}),
            json!({"id": "m3", "s3_key": "PMC1.1/c.jpg", "md5": ""}),
            json!({"id": "m4", "s3_key": "PMC1.1/missing.jpg", "md5": ""})];
        let f = d.path().join("figures.jsonl.gz");
        let mut w = crate::io::JsonlOut::create(&f).unwrap();
        for r in &rows {
            w.put(r).unwrap();
        }
        w.finish().unwrap();
        let args = FetchArgs { figures: f, out: d.path().join("img/part-00000"), max_side: 896, quality: 90, threads: 2, limit: None, local: Some(src) };
        let r1 = run(&args).unwrap();
        assert_eq!(r1["status"]["ok"], 1);
        assert_eq!(r1["status"]["md5_mismatch"], 1);
        assert_eq!(r1["status"]["decode_error"], 1);
        assert_eq!(r1["with_image"], 1);
        assert!(d.path().join("img/part-00000/m1.jpg").exists());
        let r2 = run(&args).unwrap();
        assert_eq!(r2["fetched_this_run"], 0, "resumed: nothing refetched");
        assert_eq!(r2["with_image"], 1);
        let out: Vec<Value> = crate::io::reader(&d.path().join("img/part-00000.figures.jsonl.gz")).unwrap().lines()
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
        assert_eq!((out[0]["image"].as_str().unwrap(), out[0]["width"].as_u64().unwrap()), ("part-00000/m1.jpg", 896));
    }
}
