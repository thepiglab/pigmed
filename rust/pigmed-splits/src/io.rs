//! JSON-lines (optionally gzip) helpers.

use anyhow::{Context, Result};
use flate2::{read::MultiGzDecoder, write::GzEncoder, Compression};
use serde::{de::DeserializeOwned, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

pub fn reader(path: &Path) -> Result<Box<dyn BufRead>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(if path.extension().is_some_and(|e| e == "gz") {
        Box::new(BufReader::new(MultiGzDecoder::new(f)))
    } else {
        Box::new(BufReader::new(f))
    })
}

pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let mut out = vec![];
    for (i, line) in reader(path)?.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(&line).with_context(|| format!("{}:{}", path.display(), i + 1))?);
    }
    Ok(out)
}

pub fn write_jsonl<'a, T: Serialize + 'a>(path: &Path, rows: impl IntoIterator<Item = &'a T>) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let f = BufWriter::new(File::create(&tmp)?);
        let mut w: Box<dyn Write> = if path.extension().is_some_and(|e| e == "gz") {
            Box::new(GzEncoder::new(f, Compression::new(6)))
        } else {
            Box::new(f)
        };
        for r in rows {
            serde_json::to_writer(&mut w, r)?;
            w.write_all(b"\n")?;
        }
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

pub fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![];
    MultiGzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
}

pub fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut e = GzEncoder::new(vec![], Compression::new(6));
    e.write_all(bytes)?;
    Ok(e.finish()?)
}
