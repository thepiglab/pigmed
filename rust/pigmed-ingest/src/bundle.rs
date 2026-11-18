//! Immutable bundles + outbox reservations, format-compatible with `src/pigmed/shards.py`
//! and `src/pigmed/budget.py` (Python publish/verify/reembed read Rust output unchanged).
use crate::common::{atomic_json, compact_json, digest_file, safe_member};
use anyhow::{bail, Context, Result};
use flate2::{write::GzEncoder, Compression};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub struct BundleWriter {
    pub name: String,
    pub partial: PathBuf,
    pub final_path: PathBuf,
    rows: Option<GzEncoder<BufWriter<File>>>,
    tar: Option<tar::Builder<BufWriter<File>>>,
    names: HashSet<String>,
    count: u64,
    all_approved: bool,
    closed: bool,
}

impl BundleWriter {
    pub fn new(out: &Path, name: &str) -> Result<Self> {
        let partial = out.join(format!("{name}.partial"));
        let final_path = out.join(name);
        if partial.exists() || final_path.exists() {
            bail!("Bundle already exists: {name}");
        }
        fs::create_dir_all(&partial)?;
        let rows = GzEncoder::new(BufWriter::new(File::create(partial.join("items.jsonl.gz"))?), Compression::new(6));
        let mut tar = tar::Builder::new(BufWriter::new(File::create(partial.join("assets.tar"))?));
        tar.mode(tar::HeaderMode::Deterministic);
        Ok(Self { name: name.into(), partial, final_path, rows: Some(rows), tar: Some(tar), names: HashSet::new(),
                  count: 0, all_approved: true, closed: false })
    }

    fn header(size: u64) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_size(size);
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_entry_type(tar::EntryType::Regular);
        h
    }

    pub fn add_bytes(&mut self, member: &str, data: &[u8]) -> Result<String> {
        let member = safe_member(member)?;
        if self.names.insert(member.clone()) {
            let mut h = Self::header(data.len() as u64);
            self.tar.as_mut().unwrap().append_data(&mut h, &member, data)?;
        }
        Ok(member)
    }

    pub fn add_file(&mut self, member: &str, path: &Path) -> Result<String> {
        let member = safe_member(member)?;
        if self.names.insert(member.clone()) {
            let f = File::open(path)?;
            let mut h = Self::header(f.metadata()?.len());
            self.tar.as_mut().unwrap().append_data(&mut h, &member, f)?;
        }
        Ok(member)
    }

    /// Row without vectors (model-agnostic ingest). `media_path` never leaves the worker.
    pub fn add(&mut self, mut row: Map<String, Value>) -> Result<()> {
        row.shift_remove("media_path");
        self.all_approved &= row.get("export_approved").and_then(|v| v.as_bool()).unwrap_or(false);
        let w = self.rows.as_mut().unwrap();
        w.write_all(compact_json(&Value::Object(row)).as_bytes())?;
        w.write_all(b"\n")?;
        self.count += 1;
        Ok(())
    }

    pub fn size(&mut self) -> Result<u64> {
        self.rows.as_mut().unwrap().flush()?;
        self.tar.as_mut().unwrap().get_mut().flush()?;
        let mut total = 0;
        for e in fs::read_dir(&self.partial)? {
            let e = e?;
            if e.file_type()?.is_file() {
                total += e.metadata()?.len();
            }
        }
        Ok(total)
    }

    pub fn finish(&mut self, checkpoint: Value) -> Result<PathBuf> {
        if self.closed {
            bail!("Already finalized");
        }
        let rows = self.rows.take().unwrap().finish()?;
        let f = rows.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        let tw = self.tar.take().unwrap().into_inner()?;
        let f = tw.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        let mut entries: Vec<_> = fs::read_dir(&self.partial)?.flatten().map(|e| e.path()).collect();
        entries.sort();
        let mut files = Vec::new();
        for p in entries {
            let len = p.metadata()?.len();
            if len == 0 {
                fs::remove_file(&p)?;
            } else {
                files.push(json!({"path": p.file_name().unwrap().to_string_lossy(), "bytes": len, "sha256": digest_file(&p)?}));
            }
        }
        let manifest = json!({"schema_version": 1, "bundle": self.name, "rows": self.count, "embedding": null,
            "export_approved": self.all_approved, "checkpoint": checkpoint, "files": files,
            "late_compression": "per-vector symmetric int8 + float32 scales; no pooling", "writer": "pigmed-rs"});
        atomic_json(&self.partial.join("manifest.json"), &manifest)?;
        fs::rename(&self.partial, &self.final_path)?;
        if let Some(parent) = self.final_path.parent() {
            File::open(parent)?.sync_all()?; // durable commit rename
        }
        self.closed = true;
        Ok(self.final_path.clone())
    }

    pub fn abort(&mut self) {
        self.rows.take();
        self.tar.take();
        self.closed = true;
        let _ = fs::remove_dir_all(&self.partial);
    }
}

pub fn verify_bundle(path: &Path) -> Result<Value> {
    let manifest: Value = serde_json::from_slice(&fs::read(path.join("manifest.json"))?)?;
    for item in manifest["files"].as_array().context("files")? {
        let p = path.join(safe_member(item["path"].as_str().context("path")?)?);
        let ok = p.is_file()
            && p.metadata()?.len() == item["bytes"].as_u64().unwrap_or(u64::MAX)
            && digest_file(&p)? == item["sha256"].as_str().unwrap_or("");
        if !ok {
            bail!("Bundle checksum/size mismatch: {}", p.display());
        }
    }
    Ok(manifest)
}

#[derive(Debug)]
pub struct Backpressure(pub String);
impl std::fmt::Display for Backpressure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for Backpressure {}

/// Cross-process outbox reservations via flock on `budget/ledger.lock` (same as Python).
pub struct Budget {
    root: PathBuf,
    limit: u64,
}

struct Flock(File);
impl Drop for Flock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl Budget {
    pub fn new(root: &Path, limit: u64) -> Result<Self> {
        let root = root.join("budget");
        fs::create_dir_all(&root)?;
        Ok(Self { root, limit })
    }

    fn lock(&self) -> Result<Flock> {
        let f = OpenOptions::new().create(true).append(true).read(true).open(self.root.join("ledger.lock"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("flock failed: {}", std::io::Error::last_os_error());
        }
        Ok(Flock(f))
    }

    pub fn reserve(&self, key: &str, size: u64) -> Result<()> {
        let _g = self.lock()?;
        let record = self.root.join(format!("{key}.json"));
        if record.exists() {
            return Ok(());
        }
        let mut used = 0u64;
        for e in fs::read_dir(&self.root)?.flatten() {
            let p = e.path();
            if p.extension().map_or(false, |x| x == "json") {
                let v: Value = serde_json::from_slice(&fs::read(&p)?)?;
                used += v["bytes"].as_u64().unwrap_or(0);
            }
        }
        if used + size > self.limit {
            let g = |b: u64| b as f64 / (1u64 << 30) as f64;
            return Err(Backpressure(format!(
                "Outbox reserved {:.1} GiB; cannot reserve {:.1} GiB within {:.1} GiB. Publish/drain and resume.",
                g(used), g(size), g(self.limit)
            ))
            .into());
        }
        atomic_json(&record, &json!({"key": key, "bytes": size}))
    }

    /// After a bundle commits, shrink its worst-case reservation (target shard + max article) to the
    /// bytes actually on disk, so the outbox limit counts real data instead of headroom: otherwise
    /// ~90 committed 250 MB bundles already "reserve" 200 GiB and the next reservation backpressures.
    pub fn settle(&self, key: &str, committed: &Path) -> Result<()> {
        let bytes: u64 = fs::read_dir(committed)?.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
        let _g = self.lock()?;
        let record = self.root.join(format!("{key}.json"));
        if record.exists() {
            let v: Value = serde_json::from_slice(&fs::read(&record)?)?;
            if bytes < v["bytes"].as_u64().unwrap_or(u64::MAX) {
                atomic_json(&record, &json!({"key": key, "bytes": bytes}))?;
            }
        }
        Ok(())
    }

    pub fn release(&self, key: &str) -> Result<()> {
        let _g = self.lock()?;
        let _ = fs::remove_file(self.root.join(format!("{key}.json")));
        Ok(())
    }
}

/// Single-writer lock held with flock(2). A live holder blocks other writers (on any node, on a filesystem mounted
/// with flock); a holder that dies (kill -9, preemption, node loss) loses the lock with its descriptor, so restarts
/// are never blocked by a stale file. The file stays on release (unlinking a flock file races with a new opener).
pub struct ExclusiveFile(File);
impl ExclusiveFile {
    pub fn acquire(path: &Path) -> Result<Self> {
        use std::io::{Seek, SeekFrom};
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let mut f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let holder = fs::read_to_string(path).unwrap_or_default();
            bail!("Lock held by a live process: {} ({})", path.display(), holder.trim());
        }
        let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let info = json!({"pid": std::process::id(), "host": host.trim(), "job": std::env::var("SLURM_JOB_ID").ok(),
                          "time": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64(),
                          "writer": "pigmed-rs"});
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(info.to_string().as_bytes())?;
        f.flush()?;
        Ok(Self(f))
    }
}
impl Drop for ExclusiveFile {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_file_blocks_live_holder_not_stale_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("worker.lock");
        fs::write(&p, b"{\"pid\": 1, \"note\": \"left by a killed job\"}").unwrap(); // stale file, no lock
        let held = ExclusiveFile::acquire(&p).expect("a stale lock file must not block a restart");
        let e = ExclusiveFile::acquire(&p).err().expect("a live holder must block").to_string();
        assert!(e.contains("live process"), "{e}");
        drop(held);
        ExclusiveFile::acquire(&p).expect("released on drop");
    }

    #[test]
    fn settle_shrinks_reservation_to_committed_bytes() {
        let d = tempfile::tempdir().unwrap();
        let b = Budget::new(d.path(), 10_000).unwrap();
        b.reserve("a", 6_000).unwrap();
        assert!(b.reserve("b", 6_000).is_err(), "worst-case reservations exceed the limit");
        let bundle = d.path().join("a");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("rows.jsonl.gz"), vec![0u8; 500]).unwrap();
        b.settle("a", &bundle).unwrap();
        b.reserve("b", 6_000).unwrap(); // 500 + 6000 fits once "a" is settled
        b.settle("missing", &bundle).unwrap(); // no record: no-op
    }
}
