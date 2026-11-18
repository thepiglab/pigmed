//! Free supervision pairs from the published v2 chunk shards, for retriever training (S1).
//!
//!   pairs-keep  --manifest splits/articles.jsonl.gz [--exclude-records F] -> keep.tsv (article_id, group, journal)
//!               train && commercial && !retracted; --exclude-records drops their PMCIDs from abstract->body only
//!   pairs-shard --keep keep.tsv --chunks shard.parquet --out DIR -> title.jsonl, abssent.jsonl, articles.tsv
//!   pairs-merge --shards DIR --cap 2000000 --corpus 300000 --out DIR -> title.jsonl.gz, abssent.jsonl.gz, candidates.tsv
//!
//! Row format: {"qid", "query", "pos": [unit_id], "gold": [unit_id], "article_id", "group"}.
//! title -> article: pos = gold = the article's abstract units.
//! abstract sentence (8-60 words) -> body: pos = the body chunk sharing the most content words, gold = all body chunks.

use crate::sample::{hash, TEMPLATE};
use anyhow::{anyhow, Context, Result};
use arrow_array::{Array, BooleanArray, ListArray, StringArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ProjectionMask;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufWriter, Write};
use std::path::Path;

/// abstract sentence -> body pairs per article (S1: up to 3)
pub const MAX_ABS_PER_ARTICLE: usize = 3;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Pair {
    pub qid: String,
    pub query: String,
    pub pos: Vec<String>,
    pub gold: Vec<String>,
    pub article_id: String,
    pub group: String,
    /// merge-time stratum; dropped from the final files
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub journal: String,
}

pub struct Keep {
    pub group: String,
    pub journal: String,
    /// abstract -> body allowed (not in the excluded eval corpus)
    pub abs_ok: bool,
}

pub fn keep_list(manifest: &Path, exclude_records: Option<&Path>) -> Result<Vec<(String, String, String, bool)>> {
    let mut excluded: HashSet<String> = HashSet::new();
    if let Some(p) = exclude_records {
        for line in crate::io::reader(p)?.lines() {
            let r: Value = serde_json::from_str(&line?)?;
            if let Some(a) = r["article_id"].as_str() {
                excluded.insert(a.split('.').next().unwrap_or(a).to_string());
            }
        }
    }
    let mut out = vec![];
    for line in crate::io::reader(manifest)?.lines() {
        let r: Value = serde_json::from_str(&line?)?;
        if r["split"] != "train" || r["commercial"].as_bool() != Some(true) || r["retracted"].as_bool() == Some(true) {
            continue;
        }
        let (Some(a), Some(g)) = (r["article_id"].as_str(), r["group"].as_str()) else { continue };
        let base = a.split('.').next().unwrap_or(a);
        out.push((a.to_string(), g.to_string(), r["journal"].as_str().unwrap_or("").to_string(), !excluded.contains(base)));
    }
    Ok(out)
}

pub fn read_keep(p: &Path) -> Result<HashMap<String, Keep>> {
    let mut m = HashMap::new();
    for line in crate::io::reader(p)?.lines() {
        let line = line?;
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 4 {
            continue;
        }
        m.insert(f[0].to_string(), Keep { group: f[1].into(), journal: f[2].into(), abs_ok: f[3] == "1" });
    }
    Ok(m)
}

/// Sentences: split after . ! ? when the next non-space character is uppercase or a digit.
pub fn sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let (mut out, mut cur) = (vec![], String::new());
    let mut i = 0;
    while i < chars.len() {
        cur.push(chars[i]);
        if matches!(chars[i], '.' | '!' | '?') && i + 1 < chars.len() && chars[i + 1].is_whitespace() {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j].is_uppercase() || chars[j].is_ascii_digit()) {
                out.push(cur.trim().to_string());
                cur.clear();
                i = j;
                continue;
            }
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn content_words(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 4).map(|w| w.to_lowercase()).collect()
}

#[derive(Default)]
struct Art {
    title: String,
    abs_units: Vec<String>,
    abs_text: Vec<String>,
    body: Vec<(String, String)>,
    body_gold: Vec<String>,
}

fn joined(list: &ListArray, i: usize) -> String {
    if list.is_null(i) {
        return String::new();
    }
    let v = list.value(i);
    let s = v.as_any().downcast_ref::<StringArray>().expect("list<string>");
    (0..s.len()).filter(|&k| !s.is_null(k)).map(|k| s.value(k)).collect::<Vec<_>>().join(" > ")
}

pub fn qid(kind: &str, aid: &str, q: &str) -> String {
    hash(&[kind, aid, q])[..20].to_string()
}

/// One chunk shard -> (title pairs, abstract-sentence pairs, per-article rows "article_id\tgroup\tjournal").
pub fn shard(parquet: &Path, keep: &HashMap<String, Keep>, seed: &str) -> Result<(Vec<Pair>, Vec<Pair>, Vec<String>)> {
    let file = std::fs::File::open(parquet).with_context(|| format!("open {}", parquet.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let cols = ["unit_id", "article_id", "title", "kind", "text", "section", "is_abstract"];
    let mask = ProjectionMask::columns(builder.parquet_schema(), cols);
    let reader = builder.with_projection(mask).with_batch_size(8192).build()?;
    let template = Regex::new(TEMPLATE).unwrap();
    let mut order: Vec<String> = vec![];
    let mut arts: HashMap<String, Art> = HashMap::new();
    for batch in reader {
        let b = batch?;
        let s = |n: &str| b.column_by_name(n).and_then(|c| c.as_any().downcast_ref::<StringArray>()).ok_or_else(|| anyhow!("column {n}"));
        let (uid, aid, title, kind, text) = (s("unit_id")?, s("article_id")?, s("title")?, s("kind")?, s("text")?);
        let sec = b.column_by_name("section").and_then(|c| c.as_any().downcast_ref::<ListArray>()).ok_or_else(|| anyhow!("section"))?;
        let is_abs = b.column_by_name("is_abstract").and_then(|c| c.as_any().downcast_ref::<BooleanArray>()).ok_or_else(|| anyhow!("is_abstract"))?;
        for i in 0..b.num_rows() {
            let a = aid.value(i);
            if !keep.contains_key(a) || kind.value(i) != "text" {
                continue;
            }
            let e = arts.entry(a.to_string()).or_insert_with(|| {
                order.push(a.to_string());
                Art { title: if title.is_null(i) { String::new() } else { title.value(i).to_string() }, ..Default::default() }
            });
            let section = joined(sec, i);
            let (u, t) = (uid.value(i).to_string(), text.value(i).to_string());
            if (!is_abs.is_null(i) && is_abs.value(i)) || section.to_lowercase().starts_with("abstract") {
                e.abs_units.push(u);
                e.abs_text.push(t);
            } else {
                e.body_gold.push(u.clone());
                if !template.is_match(&section) {
                    e.body.push((u, t));
                }
            }
        }
    }
    let (mut titles, mut abss, mut rows) = (vec![], vec![], vec![]);
    for aid in order {
        let a = &arts[&aid];
        let k = &keep[&aid];
        rows.push(format!("{aid}\t{}\t{}", k.group, k.journal));
        let tw = a.title.split_whitespace().count();
        if !a.abs_units.is_empty() && (3..=60).contains(&tw) {
            titles.push(Pair { qid: qid("title", &aid, &a.title), query: a.title.clone(), pos: a.abs_units.clone(),
                gold: a.abs_units.clone(), article_id: aid.clone(), group: k.group.clone(), journal: k.journal.clone() });
        }
        if !k.abs_ok || a.body.is_empty() {
            continue;
        }
        // abstract chunks overlap; dedupe sentences, keep 8-60 words, draw one by stable hash
        let mut seen = HashSet::new();
        let mut cands: Vec<String> = a.abs_text.iter().flat_map(|t| sentences(t))
            .filter(|s| (8..=60).contains(&s.split_whitespace().count()) && seen.insert(s.clone())).collect();
        cands.sort_by_key(|s| hash(&[seed, &aid, s]));
        let mut made = 0;
        for sent in cands.iter() {
            if made >= MAX_ABS_PER_ARTICLE {
                break;
            }
            let w = content_words(sent);
            let best = a.body.iter().map(|(u, t)| (content_words(t).intersection(&w).count(), u)).max_by_key(|(n, _)| *n);
            if let Some((n, u)) = best {
                if n >= 3 {
                    abss.push(Pair { qid: qid("abssent", &aid, sent), query: sent.clone(), pos: vec![u.clone()],
                        gold: a.body_gold.clone(), article_id: aid.clone(), group: k.group.clone(), journal: k.journal.clone() });
                    made += 1;
                }
            }
        }
    }
    Ok((titles, abss, rows))
}

/// One article per group (stable-hash pick; all of that article's pairs kept), at most `journal_share` of `cap`
/// pairs per journal, then article-hash order to `cap` pairs.
pub fn sample(pairs: Vec<Pair>, cap: usize, journal_share: f64, seed: &str) -> Vec<Pair> {
    let mut pick: HashMap<String, (String, String)> = HashMap::new(); // group -> (hash, article)
    for p in &pairs {
        let h = hash(&[seed, &p.article_id]);
        match pick.get(&p.group) {
            Some((q, _)) if *q <= h => {}
            _ => { pick.insert(p.group.clone(), (h, p.article_id.clone())); }
        }
    }
    let mut v: Vec<(String, Pair)> = pairs.into_iter().filter(|p| pick[&p.group].1 == p.article_id)
        .map(|p| (format!("{}{}", hash(&[seed, "order", &p.article_id]), p.qid), p)).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    let jcap = ((cap as f64 * journal_share).ceil() as usize).max(1);
    let mut per_j: HashMap<String, usize> = HashMap::new();
    let mut out = vec![];
    for (_, p) in v {
        if out.len() >= cap {
            break;
        }
        let c = per_j.entry(p.journal.clone()).or_default();
        if !p.journal.is_empty() && *c >= jcap {
            continue;
        }
        *c += 1;
        out.push(p);
    }
    out
}

/// Candidate corpus: every positive article, then a journal-proportional, hash-ordered fill up to max(target, union).
pub fn candidates(all: &[(String, String, String)], positive: &HashSet<String>, target: usize, seed: &str) -> Vec<(String, String)> {
    let n = all.len().max(1);
    let total = target.max(positive.len()).min(all.len());
    let mut jn: HashMap<&str, usize> = HashMap::new();
    for (_, _, j) in all {
        *jn.entry(j.as_str()).or_default() += 1;
    }
    let mut taken: HashMap<&str, usize> = HashMap::new();
    let mut out: Vec<(String, String)> = vec![];
    for (a, g, j) in all.iter().filter(|(a, _, _)| positive.contains(a)) {
        *taken.entry(j.as_str()).or_default() += 1;
        out.push((a.clone(), g.clone()));
    }
    let mut rest: Vec<&(String, String, String)> = all.iter().filter(|(a, _, _)| !positive.contains(a)).collect();
    rest.sort_by_key(|(a, _, _)| hash(&[seed, "cand", a]));
    for (a, g, j) in rest {
        if out.len() >= total {
            break;
        }
        let quota = (total as f64 * jn[j.as_str()] as f64 / n as f64).ceil() as usize;
        let t = taken.entry(j.as_str()).or_default();
        if *t < quota {
            *t += 1;
            out.push((a.clone(), g.clone()));
        }
    }
    out
}

pub fn write_gz(path: &Path, rows: &[Pair]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut w = flate2::write::GzEncoder::new(BufWriter::new(std::fs::File::create(&tmp)?), flate2::Compression::new(6));
        for p in rows {
            let mut p = p.clone();
            p.journal.clear();
            serde_json::to_writer(&mut w, &p)?;
            w.write_all(b"\n")?;
        }
        w.finish()?.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn merge(root: &Path, cap: usize, corpus: usize, seed: &str, out: &Path) -> Result<Value> {
    let mut dirs: Vec<_> = std::fs::read_dir(root)?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("articles.tsv").exists()).collect();
    dirs.sort();
    let (mut titles, mut abss, mut all) = (vec![], vec![], vec![]);
    for d in &dirs {
        titles.extend(crate::io::read_jsonl::<Pair>(&d.join("title.jsonl"))?);
        abss.extend(crate::io::read_jsonl::<Pair>(&d.join("abssent.jsonl"))?);
        for line in std::fs::read_to_string(d.join("articles.tsv"))?.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() >= 3 {
                all.push((f[0].to_string(), f[1].to_string(), f[2].to_string()));
            }
        }
    }
    let (nt, na) = (titles.len(), abss.len());
    let t = sample(titles, cap, 0.01, seed);
    let a = sample(abss, cap, 0.01, seed);
    std::fs::create_dir_all(out)?;
    write_gz(&out.join("title.jsonl.gz"), &t)?;
    write_gz(&out.join("abssent.jsonl.gz"), &a)?;
    let positive: HashSet<String> = t.iter().chain(a.iter()).map(|p| p.article_id.clone()).collect();
    let cand = candidates(&all, &positive, corpus, seed);
    let mut w = BufWriter::new(std::fs::File::create(out.join("candidates.tsv"))?);
    for (a, g) in &cand {
        writeln!(w, "{a}\t{g}")?;
    }
    w.flush()?;
    let mut journals: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &t {
        *journals.entry(p.journal.as_str()).or_default() += 1;
    }
    let rep = serde_json::json!({"shards": dirs.len(), "kept_articles": all.len(),
        "title": {"raw": nt, "sampled": t.len()}, "abssent": {"raw": na, "sampled": a.len()},
        "positive_articles": positive.len(), "candidates": cand.len(), "title_journals": journals.len(),
        "cap": cap, "journal_share": 0.01, "seed": seed});
    crate::io::write_json(&out.join("report.json"), &rep)?;
    Ok(rep)
}

/// Full-split merge, streaming (no cap, no journal quota): one article per near-duplicate group (same stable-hash
/// pick as `sample`), every pair of the picked article kept, provenance added per pair from the corpus records
/// (pmcid, doi, license_code). Memory is the group/article maps, not the pairs. Writes title.jsonl.gz,
/// abssent.jsonl.gz, candidates.tsv (all kept articles: article_id, group) and report.json.
pub fn merge_stream(root: &Path, records: &Path, seed: &str, out: &Path) -> Result<Value> {
    let mut dirs: Vec<_> = std::fs::read_dir(root)?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("articles.tsv").exists()).collect();
    dirs.sort();
    let mut pick: HashMap<String, (String, String)> = HashMap::new(); // group -> (hash, article)
    let mut all: Vec<(String, String)> = vec![];
    for d in &dirs {
        for line in std::fs::read_to_string(d.join("articles.tsv"))?.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 2 {
                continue;
            }
            let h = hash(&[seed, f[0]]);
            match pick.get(f[1]) {
                Some((q, _)) if *q <= h => {}
                _ => { pick.insert(f[1].to_string(), (h, f[0].to_string())); }
            }
            all.push((f[0].to_string(), f[1].to_string()));
        }
    }
    let picked: HashSet<String> = pick.into_values().map(|(_, a)| a).collect();
    let mut prov: HashMap<String, (String, String)> = HashMap::new(); // article -> (doi, license_code)
    for line in crate::io::reader(records)?.lines() {
        let line = line?;
        let Some(id) = line.get(15..).and_then(|x| x.split('"').next()) else { continue };
        if !picked.contains(id) {
            continue;
        }
        let r: Value = serde_json::from_str(&line)?;
        prov.insert(id.to_string(), (r["doi"].as_str().unwrap_or("").to_string(), r["license_code"].as_str().unwrap_or("").to_string()));
    }
    std::fs::create_dir_all(out)?;
    let mut rep = serde_json::Map::new();
    let mut positive: HashSet<String> = HashSet::new();
    for kind in ["title", "abssent"] {
        let path = out.join(format!("{kind}.jsonl.gz"));
        let tmp = path.with_extension("tmp");
        let (mut raw, mut kept, mut no_prov) = (0usize, 0usize, 0usize);
        let mut journals: HashSet<String> = HashSet::new();
        let mut licenses: BTreeMap<String, usize> = BTreeMap::new();
        {
            let mut w = flate2::write::GzEncoder::new(BufWriter::new(std::fs::File::create(&tmp)?), flate2::Compression::new(6));
            for d in &dirs {
                for line in crate::io::reader(&d.join(format!("{kind}.jsonl")))?.lines() {
                    let p: Pair = serde_json::from_str(&line?)?;
                    raw += 1;
                    if !picked.contains(&p.article_id) {
                        continue;
                    }
                    let (doi, lic) = prov.get(&p.article_id).cloned().unwrap_or_else(|| { no_prov += 1; Default::default() });
                    let mut v = serde_json::to_value(&p)?;
                    v["pmcid"] = serde_json::json!(p.article_id.split('.').next().unwrap_or(&p.article_id));
                    v["doi"] = serde_json::json!(doi);
                    v["license_code"] = serde_json::json!(lic);
                    *licenses.entry(lic).or_default() += 1;
                    journals.insert(p.journal.clone());
                    positive.insert(p.article_id.clone());
                    serde_json::to_writer(&mut w, &v)?;
                    w.write_all(b"\n")?;
                    kept += 1;
                }
            }
            w.finish()?.flush()?;
        }
        std::fs::rename(&tmp, &path)?;
        rep.insert(kind.into(), serde_json::json!({"raw": raw, "kept": kept, "journals": journals.len(), "missing_provenance": no_prov, "licenses": licenses}));
    }
    let mut w = BufWriter::new(std::fs::File::create(out.join("candidates.tsv"))?);
    for (a, g) in &all {
        writeln!(w, "{a}\t{g}")?;
    }
    w.flush()?;
    rep.insert("shards".into(), serde_json::json!(dirs.len()));
    rep.insert("kept_articles".into(), serde_json::json!(all.len()));
    rep.insert("picked_articles".into(), serde_json::json!(picked.len()));
    rep.insert("positive_articles".into(), serde_json::json!(positive.len()));
    rep.insert("seed".into(), serde_json::json!(seed));
    let rep = Value::Object(rep);
    crate::io::write_json(&out.join("report.json"), &rep)?;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_sentences() {
        let s = sentences("We studied 12 mice. Results were clear, e.g. in vitro. 3 groups differed! end");
        assert_eq!(s, vec!["We studied 12 mice.", "Results were clear, e.g. in vitro.", "3 groups differed! end"]);
    }

    fn p(q: &str, g: &str, j: &str) -> Pair {
        Pair { qid: q.into(), query: q.into(), pos: vec![], gold: vec![], article_id: q.into(), group: g.into(), journal: j.into() }
    }

    #[test]
    fn sample_one_per_group_and_journal_cap() {
        let mut v = vec![p("a", "g1", "J1"), p("b", "g1", "J1"), p("c", "g2", "J1"), p("d", "g3", "J2"), p("e", "g4", "J2")];
        let s = sample(v.clone(), 100, 0.01, "s");
        assert_eq!(s.iter().filter(|x| x.group == "g1").count(), 1);
        // several pairs of one article stay together
        let mut q = p("d2", "g3", "J2");
        q.article_id = "d".into();
        v.push(q);
        let s2 = sample(v, 100, 0.5, "s");
        assert_eq!(s2.iter().filter(|x| x.article_id == "d").count(), 2);
        assert!(s.iter().filter(|x| x.journal == "J1").count() <= 1);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn candidates_include_positives_and_fill() {
        let all: Vec<(String, String, String)> = (0..100).map(|i| (format!("A{i}"), format!("g{i}"), if i < 50 { "J1".into() } else { "J2".into() })).collect();
        let pos: HashSet<String> = ["A1".to_string(), "A99".to_string()].into();
        let c = candidates(&all, &pos, 20, "s");
        assert_eq!(c.len(), 20);
        assert!(c.iter().any(|x| x.0 == "A1") && c.iter().any(|x| x.0 == "A99"));
        assert!(c.iter().filter(|x| x.0.as_str() < "A5" && x.0.len() == 2).count() <= 20);
    }

    #[test]
    fn merge_stream_one_article_per_group_with_provenance() {
        let root = std::env::temp_dir().join(format!("pairs-ms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (sh, arts) in [("s1", vec![("PMC1.1", "g1"), ("PMC2.1", "g2")]), ("s2", vec![("PMC3.1", "g1")])] {
            let d = root.join("shards").join(sh);
            std::fs::create_dir_all(&d).unwrap();
            let tsv: String = arts.iter().map(|(a, g)| format!("{a}\t{g}\tJ\n")).collect();
            std::fs::write(d.join("articles.tsv"), tsv).unwrap();
            let rows: String = arts.iter().map(|(a, g)| serde_json::to_string(&Pair { qid: format!("q{a}"), query: "t".into(), pos: vec![],
                gold: vec![], article_id: a.to_string(), group: g.to_string(), journal: "J".into() }).unwrap() + "\n").collect();
            std::fs::write(d.join("title.jsonl"), &rows).unwrap();
            std::fs::write(d.join("abssent.jsonl"), "").unwrap();
        }
        let rec = root.join("records.jsonl");
        std::fs::write(&rec, "{\"article_id\":\"PMC1.1\",\"doi\":\"10.1/a\",\"license_code\":\"CC BY\"}\n{\"article_id\":\"PMC2.1\",\"doi\":\"10.1/b\",\"license_code\":\"CC0\"}\n{\"article_id\":\"PMC3.1\",\"doi\":\"10.1/c\",\"license_code\":\"CC BY\"}\n").unwrap();
        let rep = merge_stream(&root.join("shards"), &rec, "seed", &root.join("out")).unwrap();
        assert_eq!(rep["title"]["raw"], 3);
        assert_eq!(rep["title"]["kept"], 2, "g1 keeps one of PMC1/PMC3");
        assert_eq!(rep["title"]["missing_provenance"], 0);
        let rows: Vec<Value> = crate::io::read_jsonl(&root.join("out/title.jsonl.gz")).unwrap();
        assert!(rows.iter().all(|r| r["doi"].as_str().unwrap().starts_with("10.1/") && r["pmcid"].as_str().unwrap().starts_with("PMC")
            && !r["pmcid"].as_str().unwrap().contains('.')));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
