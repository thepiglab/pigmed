//! Lay-query eval set from plain-language summaries (docs/RETRIEVER_QUERY_RECIPE_V3.md §7): authors' own lay
//! sentences (PLOS "Author summary", eLife digest, Cochrane / journal "Plain language summary", "Lay summary", ...)
//! of held-out articles become queries; gold = every other text unit of the same article. Written by the authors,
//! not by a generator, so it is a non-circular lay set. Held-out splits only (dev, test, test_temporal,
//! test_journal); never used for training.

use crate::pairs::sentences;
use crate::sample::hash;
use anyhow::{anyhow, Context, Result};
use arrow_array::{Array, ListArray, StringArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ProjectionMask;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::Path;

pub const PLS_RE: &str = r"(?i)\b(author summary|plain[- ]language summary|plain english summary|lay summary|lay abstract|non-?technical summary|digest|summary for (patients|the public|lay))\b";
pub const PER_ARTICLE: usize = 3;

/// article_id -> split for every held-out article of the manifest.
pub fn heldout(manifest: &Path) -> Result<HashMap<String, String>> {
    let mut m = HashMap::new();
    for line in crate::io::reader(manifest)?.lines() {
        let v: Value = serde_json::from_str(&line?)?;
        let split = v["split"].as_str().unwrap_or("");
        if !split.is_empty() && split != "train" {
            if let Some(a) = v["article_id"].as_str() {
                m.insert(a.to_string(), split.to_string());
            }
        }
    }
    Ok(m)
}

#[derive(Default)]
struct Art {
    pls: Vec<(String, String)>, // (section, text)
    gold: Vec<String>,
}

pub fn shard(parquet: &Path, held: &HashMap<String, String>, seed: &str) -> Result<Vec<Value>> {
    let file = std::fs::File::open(parquet).with_context(|| format!("open {}", parquet.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let mask = ProjectionMask::columns(builder.parquet_schema(), ["unit_id", "article_id", "kind", "text", "section"]);
    let reader = builder.with_projection(mask).with_batch_size(8192).build()?;
    let re = Regex::new(PLS_RE).unwrap();
    let mut order = vec![];
    let mut arts: HashMap<String, Art> = HashMap::new();
    for batch in reader {
        let b = batch?;
        let s = |n: &str| b.column_by_name(n).and_then(|c| c.as_any().downcast_ref::<StringArray>()).ok_or_else(|| anyhow!("column {n}"));
        let (uid, aid, kind, text) = (s("unit_id")?, s("article_id")?, s("kind")?, s("text")?);
        let sec = b.column_by_name("section").and_then(|c| c.as_any().downcast_ref::<ListArray>()).ok_or_else(|| anyhow!("section"))?;
        for i in 0..b.num_rows() {
            let a = aid.value(i);
            if !held.contains_key(a) || kind.value(i) != "text" {
                continue;
            }
            let section = if sec.is_null(i) { String::new() } else {
                let v = sec.value(i);
                let x = v.as_any().downcast_ref::<StringArray>().expect("list<string>");
                (0..x.len()).filter(|&k| !x.is_null(k)).map(|k| x.value(k)).collect::<Vec<_>>().join(" > ")
            };
            let e = arts.entry(a.to_string()).or_insert_with(|| { order.push(a.to_string()); Art::default() });
            if re.is_match(&section) {
                e.pls.push((section, text.value(i).to_string()));
            } else {
                e.gold.push(uid.value(i).to_string());
            }
        }
    }
    let mut out = vec![];
    for aid in order {
        let a = &arts[&aid];
        if a.pls.is_empty() || a.gold.is_empty() {
            continue;
        }
        let mut seen = HashSet::new();
        let mut cands: Vec<(String, String)> = a.pls.iter().flat_map(|(sec, t)| sentences(t).into_iter().map(move |x| (sec.clone(), x)))
            .filter(|(_, x)| (8..=40).contains(&x.split_whitespace().count()) && seen.insert(x.clone())).collect();
        cands.sort_by_key(|(_, x)| hash(&[seed, &aid, x]));
        for (sec, q) in cands.into_iter().take(PER_ARTICLE) {
            out.push(json!({"qid": format!("pls-{}", &hash(&[&aid, &q])[..16]), "query": q, "gold": a.gold, "article_id": aid,
                "split": held[&aid], "section": sec}));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pls_section_names() {
        let re = Regex::new(PLS_RE).unwrap();
        for s in ["Author summary", "Plain language summary", "eLife digest", "Lay Summary", "Plain-language summary"] {
            assert!(re.is_match(s), "{s}");
        }
        for s in ["Abstract", "Summary statistics", "Results > Summary of findings"] {
            assert!(!re.is_match(s), "{s}");
        }
    }
}
