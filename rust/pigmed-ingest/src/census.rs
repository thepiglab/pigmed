//! `media-census`: metadata-only scan of plan rows (no article downloads). Counts which media kinds
//! articles ship and writes an enrichment plan of the articles carrying a rare modality (e.g. audio),
//! so tests can cover formats a uniform sample may miss. Plan rows are copied verbatim.
use crate::common::read_jsonl;
use crate::net::{s3_url, Http};
use anyhow::Result;
use futures::{stream, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub async fn run(manifests: &[PathBuf], want: &str, limit: usize, out: &Path, conc: usize) -> Result<Value> {
    let mut rows = vec![];
    for m in manifests {
        rows.extend(read_jsonl(m)?);
    }
    let http = Http::new("", conc, 5)?;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let (mut scanned, mut failures, mut found) = (0u64, 0u64, vec![]);
    let mut s = stream::iter(rows.into_iter().map(|r| {
        let http = http.clone();
        async move {
            let meta = http.json(&s3_url(r["key"].as_str().unwrap_or(""))).await;
            (r, meta)
        }
    }))
    .buffer_unordered(conc.max(1));
    while let Some((r, meta)) = s.next().await {
        let Ok(meta) = meta else {
            failures += 1;
            continue;
        };
        scanned += 1;
        let urls: Vec<&str> = meta["media_urls"].as_array().map(|a| a.iter().filter_map(|u| u.as_str()).collect()).unwrap_or_default();
        let mods: std::collections::BTreeSet<&str> = urls.iter().map(|u| crate::media::modality(u)).collect();
        for m in &mods {
            *counts.entry(m.to_string()).or_default() += 1;
        }
        if mods.contains(want) && found.len() < limit {
            found.push(r);
        }
        if found.len() >= limit {
            break;
        }
    }
    let mut f = std::fs::File::create(out)?;
    for r in &found {
        writeln!(f, "{}", serde_json::to_string(r)?)?;
    }
    Ok(json!({"scanned": scanned, "metadata_failures": failures, "articles_by_modality": counts, "want": want,
              "found": found.len(), "out": out}))
}
