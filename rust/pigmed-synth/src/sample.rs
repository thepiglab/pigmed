//! Topic-balanced chunk sample for the generator A/B.
//!
//! Articles: train split, commercial-use licence, research/review/case/brief articles with an abstract and a real
//! body, drawn round-robin over e-means level-1 clusters (hash order inside each cluster), so no field swamps the
//! sample. Chunks: body text units outside template sections, spread evenly through the article.

use crate::io::read_jsonl;
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

pub const TEMPLATE: &str = concat!(
    r"(?i)conflict|competing|author.{0,12}contribution|funding|acknowledg|data (?:availability|sharing)|ethic|consent|",
    r"abbreviation|supplementary|peer review|publisher|declaration|disclosure|orcid|copyright|licen[cs]e|references"
);
const ARTICLE_TYPES: [&str; 4] = ["research-article", "review-article", "case-report", "brief-report"];

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SampleChunk {
    pub chunk_id: String,
    pub article_id: String,
    pub cluster: String,
    pub title: String,
    pub abstract_text: String,
    pub section: String,
    pub text: String,
    pub words: usize,
    /// the passage's label in the article text ([[P1]] ... [[/P1]])
    #[serde(default)]
    pub marker: String,
}

pub fn hash(parts: &[&str]) -> String {
    hex::encode(Sha256::digest(parts.join("\u{1f}").as_bytes()))
}

fn find_items(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(root)? {
        let p = e?.path();
        if p.is_dir() {
            find_items(&p, out)?;
        } else if p.file_name().is_some_and(|n| n == "items.jsonl.gz") {
            out.push(p);
        }
    }
    Ok(())
}

fn section_of(r: &Value) -> String {
    match &r["locator"] {
        Value::Object(o) => o.get("section").and_then(|s| s.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" > ")).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Pick `k` evenly spaced items of `v` (all if fewer).
pub fn spread<T: Clone>(v: &[T], k: usize) -> Vec<T> {
    if v.len() <= k {
        return v.to_vec();
    }
    (0..k).map(|i| v[(i * 2 + 1) * v.len() / (2 * k)].clone()).collect()
}

/// Round-robin over clusters (sorted by key), each cluster's articles in hash order.
pub fn round_robin(by_cluster: BTreeMap<String, Vec<String>>, seed: &str, n: usize) -> Vec<(String, String)> {
    let mut queues: Vec<(String, VecDeque<String>)> = by_cluster.into_iter().map(|(c, mut v)| {
        v.sort_by_key(|a| hash(&[seed, a]));
        (c, v.into())
    }).collect();
    queues.sort_by_key(|(c, _)| hash(&[seed, c]));
    let mut out = vec![];
    while out.len() < n && queues.iter().any(|(_, q)| !q.is_empty()) {
        for (c, q) in queues.iter_mut() {
            if out.len() >= n {
                break;
            }
            if let Some(a) = q.pop_front() {
                out.push((a, c.clone()));
            }
        }
    }
    out
}

pub struct SampleArgs<'a> {
    pub manifest: &'a Path,
    pub records: &'a Path,
    pub atlas: &'a Path,
    pub items_root: &'a Path,
    pub articles: usize,
    pub chunks_per_article: usize,
    pub min_words: usize,
    pub max_article_words: usize,
    pub seed: &'a str,
}

pub fn run(a: &SampleArgs) -> Result<(Vec<SampleChunk>, Vec<Article>)> {
    let split: HashMap<String, String> = read_jsonl::<Value>(a.manifest)?.into_iter()
        .filter_map(|r| Some((r["article_id"].as_str()?.to_string(), r["split"].as_str()?.to_string()))).collect();
    let atlas: HashMap<String, Vec<i64>> = serde_json::from_str(&std::fs::read_to_string(a.atlas)?).context("atlas")?;
    let mut by_cluster: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in read_jsonl::<Value>(a.records)? {
        let Some(aid) = r["article_id"].as_str() else { continue };
        let ok = split.get(aid).is_some_and(|s| s == "train")
            && r["commercial"].as_bool() == Some(true)
            && r["has_abstract"].as_bool() == Some(true)
            && r["words"].as_u64().unwrap_or(0) as usize >= a.min_words
            && ARTICLE_TYPES.contains(&r["article_type"].as_str().unwrap_or(""));
        if let (true, Some(c)) = (ok, atlas.get(aid).and_then(|p| p.get(1))) {
            by_cluster.entry(format!("L1-{c}")).or_default().push(aid.to_string());
        }
    }
    // draw extra candidates; some articles lack enough usable chunks
    let picked = round_robin(by_cluster, a.seed, a.articles * 3 / 2);
    let wanted: HashMap<String, String> = picked.iter().cloned().collect();
    let order: Vec<String> = picked.iter().map(|(x, _)| x.clone()).collect();
    let template = Regex::new(TEMPLATE).unwrap();
    let mut files = vec![];
    find_items(a.items_root, &mut files)?;
    files.sort();
    let mut per_article: HashMap<String, Collected> = HashMap::new();
    for f in files {
        for r in read_jsonl::<Value>(&f)? {
            let Some(aid) = r["article_id"].as_str() else { continue };
            let kind = r["kind"].as_str().unwrap_or("");
            if !wanted.contains_key(aid) || !(kind == "text" || kind == "table_text") {
                continue;
            }
            let e = per_article.entry(aid.to_string()).or_insert_with(|| Collected {
                title: r["title"].as_str().unwrap_or("").to_string(), ..Default::default() });
            let sec = section_of(&r);
            let text = r["text"].as_str().unwrap_or("").to_string();
            let uid = r["unit_id"].as_str().unwrap_or("").to_string();
            if sec.to_lowercase().starts_with("abstract") {
                e.abstract_parts.push(text);
                continue;
            }
            if template.is_match(&sec) {
                continue;
            }
            let loc = &r["locator"];
            e.units.push(Unit { unit_id: uid.clone(), section: sec.clone(),
                section_id: loc["section_id"].as_str().unwrap_or("").to_string(),
                char_start: loc["char_start"].as_u64().unwrap_or(0) as usize, char_end: loc["char_end"].as_u64().unwrap_or(0) as usize,
                text: text.clone() });
            let words = text.split_whitespace().count();
            if kind == "text" && words >= 80 {
                e.candidates.push(SampleChunk {
                    chunk_id: uid, article_id: aid.to_string(), cluster: wanted[aid].clone(), title: String::new(),
                    abstract_text: String::new(), section: sec, text, words, marker: String::new(),
                });
            }
        }
    }
    let mut out = vec![];
    let mut articles = vec![];
    let mut used = HashSet::new();
    for aid in order {
        if used.len() >= a.articles {
            break;
        }
        let Some(col) = per_article.remove(&aid) else { continue };
        if col.candidates.len() < a.chunks_per_article.min(3) || col.abstract_parts.is_empty() {
            continue;
        }
        let abstract_text: String = col.abstract_parts.join(" ").split_whitespace().take(250).collect::<Vec<_>>().join(" ");
        let picked = spread(&col.candidates, a.chunks_per_article);
        let markers: HashMap<String, String> = picked.iter().enumerate().map(|(i, c)| (c.chunk_id.clone(), format!("P{}", i + 1))).collect();
        let (body, words) = article_text(&col.units, &markers, a.max_article_words);
        let full = format!("Title: {}\n\nAbstract: {}\n\n{}", col.title, col.abstract_parts.join(" "), body);
        articles.push(Article { article_id: aid.clone(), title: col.title.clone(), text: full, words,
            passages: picked.iter().map(|c| markers[&c.chunk_id].clone()).collect(), ..Default::default() });
        for mut c in picked {
            c.title = col.title.clone();
            c.abstract_text = abstract_text.clone();
            c.marker = markers[&c.chunk_id].clone();
            out.push(c);
        }
        used.insert(aid);
    }
    Ok((out, articles))
}

#[derive(Default)]
struct Collected {
    title: String,
    abstract_parts: Vec<String>,
    units: Vec<Unit>,
    candidates: Vec<SampleChunk>,
}

#[derive(Clone, Debug)]
pub struct Unit {
    pub unit_id: String,
    pub section: String,
    pub section_id: String,
    pub char_start: usize,
    pub char_end: usize,
    pub text: String,
}

/// The article body in reading order: section headings when the section changes, consecutive units of one section
/// de-overlapped by their character offsets, target passages wrapped in [[P1]] ... [[/P1]] and always kept whole.
/// Non-target text is cut once `max_words` is reached (targets still included).
pub fn article_text(units: &[Unit], markers: &HashMap<String, String>, max_words: usize) -> (String, usize) {
    let mut out = String::new();
    let (mut words, mut prev_sec, mut prev_end) = (0usize, String::new(), 0usize);
    let mut truncated = false;
    for u in units {
        let marker = markers.get(&u.unit_id);
        let mut text = u.text.as_str();
        if u.section_id == prev_sec && u.char_start < prev_end && marker.is_none() {
            // drop the characters already emitted by the previous unit of this section
            let skip = (prev_end - u.char_start).min(text.len());
            let mut cut = skip;
            while cut < text.len() && !text.is_char_boundary(cut) {
                cut += 1;
            }
            text = text[cut..].trim_start();
        }
        let n = text.split_whitespace().count();
        if marker.is_none() && words + n > max_words {
            truncated = true;
            continue;
        }
        if u.section_id != prev_sec && !u.section.is_empty() {
            out.push_str(&format!("\n## {}\n", u.section));
        }
        match marker {
            Some(m) => out.push_str(&format!("[[{m}]] {} [[/{m}]]\n", u.text)),
            None if !text.is_empty() => { out.push_str(text); out.push('\n'); }
            None => {}
        }
        words += n;
        prev_sec = u.section_id.clone();
        prev_end = u.char_end;
    }
    if truncated {
        out.push_str("\n[... article truncated ...]\n");
    }
    (out, words)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Article {
    pub article_id: String,
    pub title: String,
    pub text: String,
    pub words: usize,
    pub passages: Vec<String>,
    /// From the corpus records (`annotate-articles`); empty when not annotated. Used for true-by-construction
    /// instruction constraints (recipe v3.1) and release provenance.
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub article_type: String,
    #[serde(default)]
    pub pub_types: Vec<String>,
    #[serde(default)]
    pub license_code: String,
    #[serde(default)]
    pub commercial: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_and_round_robin() {
        let v: Vec<usize> = (0..10).collect();
        assert_eq!(spread(&v, 3), vec![1, 5, 8]);
        assert_eq!(spread(&v[..2], 5), vec![0, 1]);
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), vec!["a1".to_string(), "a2".into(), "a3".into()]);
        m.insert("b".to_string(), vec!["b1".to_string()]);
        let r = round_robin(m, "s", 3);
        assert_eq!(r.len(), 3);
        assert_eq!(r.iter().filter(|(_, c)| c == "b").count(), 1);
    }

    #[test]
    fn article_text_dedups_overlap_and_marks_targets() {
        let u = |id: &str, sid: &str, s: usize, e: usize, t: &str| Unit { unit_id: id.into(), section: "Methods".into(),
            section_id: sid.into(), char_start: s, char_end: e, text: t.into() };
        // section text "aaa bbb ccc ddd eee": unit1 = [0,11) "aaa bbb ccc", unit2 = [8,19) "ccc ddd eee"
        let units = vec![u("1", "S1", 0, 11, "aaa bbb ccc"), u("2", "S1", 8, 19, "ccc ddd eee"), u("3", "S2", 0, 5, "fff g")];
        let m: HashMap<String, String> = [("3".to_string(), "P1".to_string())].into();
        let (t, _) = article_text(&units, &m, 1000);
        assert!(t.contains("aaa bbb ccc\nddd eee"), "{t}");
        assert!(t.contains("[[P1]] fff g [[/P1]]"));
        let (t, w) = article_text(&units, &m, 2);
        assert!(t.contains("[[P1]]") && t.contains("truncated") && w <= 4, "{t}");
    }
}
