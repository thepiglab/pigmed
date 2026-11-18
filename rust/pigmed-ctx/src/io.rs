//! Plain / gzip line readers and JSONL writers.
use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

pub fn reader(path: &Path) -> Result<Box<dyn BufRead>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(if path.extension().is_some_and(|e| e == "gz") {
        Box::new(BufReader::new(MultiGzDecoder::new(f)))
    } else {
        Box::new(BufReader::new(f))
    })
}

/// JSONL (or .jsonl.gz) writer, written to `<path>.tmp` and renamed by `finish`.
pub struct JsonlOut {
    w: Box<dyn Write>,
    tmp: std::path::PathBuf,
    path: std::path::PathBuf,
    pub rows: usize,
}

impl JsonlOut {
    pub fn create(path: &Path) -> Result<Self> {
        let tmp = path.with_extension(format!("{}.tmp", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
        let f = BufWriter::new(File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?);
        let w: Box<dyn Write> = if path.extension().is_some_and(|e| e == "gz") {
            Box::new(flate2::write::GzEncoder::new(f, flate2::Compression::new(6)))
        } else {
            Box::new(f)
        };
        Ok(JsonlOut { w, tmp, path: path.to_path_buf(), rows: 0 })
    }
    pub fn put(&mut self, v: &serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.w, v)?;
        self.w.write_all(b"\n")?;
        self.rows += 1;
        Ok(())
    }
    pub fn finish(mut self) -> Result<usize> {
        self.w.flush()?;
        drop(self.w);
        std::fs::rename(&self.tmp, &self.path)?;
        Ok(self.rows)
    }
}
