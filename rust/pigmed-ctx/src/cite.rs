//! Citation pairs (citation context -> cited article), resolved across the whole corpus, as an S1-style source:
//!   queries.jsonl.gz {id, text}            context sentence, reference markers and figure call-outs removed
//!   docs.jsonl.gz    {id, text}            cited article: "title\n\nabstract" (raw text; the trainer adds prompts)
//!   pairs.jsonl.gz   {q, pos, gold, group, cited_group, citing_article_id}
//!   report.json
//! Both ends must be kept articles (train split, commercial, not retracted, not held out) in different groups.
//! Streaming: cite_ctx rows arrive grouped by citing article, so per-article caps need no global state.

use crate::shard::{read_ids, read_keep};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

pub struct CiteArgs {
    pub shards: PathBuf,
    pub records: PathBuf,
    pub keep: PathBuf,
    pub exclude: Vec<PathBuf>,
    /// contexts per (citing, cited) pair
    pub per_pair: usize,
    /// contexts per citing article
    pub per_article: usize,
    pub out: PathBuf,
}

fn shard_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut d: Vec<PathBuf> = std::fs::read_dir(root)?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("stats.json").exists()).collect();
    d.sort();
    Ok(d)
}

pub fn run(a: &CiteArgs) -> Result<Value> {
    let exclude = read_ids(&a.exclude)?;
    let keep = read_keep(&a.keep, &exclude)?;
    // corpus id -> kept article id (cited articles must be kept; others never resolve)
    let (mut by_pmid, mut by_pmcid, mut by_doi) = (HashMap::new(), HashMap::new(), HashMap::new());
    for line in crate::io::reader(&a.records)?.lines() {
        let line = line?;
        let Some(id) = line.get(15..).and_then(|x| x.split('"').next()) else { continue };
        if !keep.contains_key(id) {
            continue;
        }
        let r: Value = serde_json::from_str(&line)?;
        let aid = id.to_string();
        if let Some(p) = r["pmid"].as_str().filter(|x| !x.is_empty()) {
            by_pmid.insert(p.to_string(), aid.clone());
        }
        by_pmcid.insert(id.split('.').next().unwrap_or(id).to_string(), aid.clone());
        if let Some(d) = r["doi"].as_str().filter(|x| !x.is_empty()) {
            by_doi.insert(d.to_lowercase(), aid);
        }
    }
    std::fs::create_dir_all(&a.out)?;
    let mut qw = crate::io::JsonlOut::create(&a.out.join("queries.jsonl.gz"))?;
    let mut pw = crate::io::JsonlOut::create(&a.out.join("pairs.jsonl.gz"))?;
    let mut needed: HashSet<String> = HashSet::new();
    let mut why: BTreeMap<&str, usize> = BTreeMap::new();
    let dirs = shard_dirs(&a.shards)?;
    for d in &dirs {
        let f = d.join("cite_ctx.jsonl.gz");
        if !f.exists() {
            continue;
        }
        let mut cur = String::new();
        let mut per_pair: HashMap<String, usize> = HashMap::new();
        let mut n_art = 0usize;
        let mut seen_q: HashSet<String> = HashSet::new();
        for line in crate::io::reader(&f)?.lines() {
            let r: Value = serde_json::from_str(&line?)?;
            *why.entry("contexts").or_default() += 1;
            let citing = r["citing_article_id"].as_str().unwrap_or("").to_string();
            if citing != cur {
                cur = citing.clone();
                per_pair.clear();
                seen_q.clear();
                n_art = 0;
            }
            let s = |k: &str| r[k].as_str().filter(|x| !x.is_empty());
            let cited = s("pmcid").and_then(|p| by_pmcid.get(p.split('.').next().unwrap_or(p)))
                .or_else(|| s("pmid").and_then(|p| by_pmid.get(p)))
                .or_else(|| s("doi").and_then(|d| by_doi.get(&d.to_lowercase())));
            let Some(cited) = cited else { *why.entry("unresolved_or_not_kept").or_default() += 1; continue };
            let (cg, ig) = (&keep[cited].group, r["citing_group"].as_str().unwrap_or(""));
            if *cited == citing || cg == ig {
                *why.entry("same_article_or_group").or_default() += 1;
                continue;
            }
            let c = per_pair.entry(cited.clone()).or_default();
            if *c >= a.per_pair || n_art >= a.per_article {
                *why.entry("capped").or_default() += 1;
                continue;
            }
            *c += 1;
            n_art += 1;
            let qid = r["qid"].as_str().unwrap_or("").to_string();
            if seen_q.insert(qid.clone()) {
                qw.put(&json!({"id": qid, "text": r["query"]}))?;
            }
            pw.put(&json!({"q": qid, "pos": [cited], "gold": [cited], "group": ig, "cited_group": cg, "citing_article_id": citing}))?;
            needed.insert(cited.clone());
            *why.entry("pairs").or_default() += 1;
        }
    }
    let (nq, np) = (qw.finish()?, pw.finish()?);
    let mut dw = crate::io::JsonlOut::create(&a.out.join("docs.jsonl.gz"))?;
    for d in &dirs {
        for line in crate::io::reader(&d.join("articles.jsonl.gz"))?.lines() {
            let line = line?;
            let Some(id) = line.get(15..).and_then(|x| x.split('"').next()) else { continue };
            if !needed.remove(id) {
                continue;
            }
            let r: Value = serde_json::from_str(&line)?;
            let text = format!("{}\n\n{}", r["title"].as_str().unwrap_or(""), r["abstract"].as_str().unwrap_or("")).trim().to_string();
            dw.put(&json!({"id": id, "text": text, "group": r["group"], "pmcid": r["pmcid"], "doi": r["doi"], "license_code": r["license_code"]}))?;
        }
    }
    let nd = dw.finish()?;
    let rep = json!({"shards": dirs.len(), "queries": nq, "pairs": np, "docs": nd, "cited_without_doc": needed.len(),
        "counts": why, "per_pair": a.per_pair, "per_article": a.per_article});
    std::fs::write(a.out.join("report.json"), serde_json::to_string_pretty(&rep)?)?;
    std::fs::write(a.out.join("source.json"), serde_json::to_string_pretty(&json!({"symmetric": false,
        "origin": "PMC Open Access JATS: citation-context sentence (markers removed) -> cited article title + abstract; both ends train split, commercial-use licence, not retracted, different near-duplicate groups (pigmed-ctx cite-resolve)",
        "licence": "per article: docs.jsonl license_code (CC0 / CC BY / CC BY-SA / CC BY-ND, commercial use)",
        "group_fields": {"pairs.group": "citing article group", "docs.group": "cited article group"}}))?)?;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_filters_and_caps() {
        let d = tempfile::tempdir().unwrap();
        let sh = d.path().join("shards/s1");
        std::fs::create_dir_all(&sh).unwrap();
        std::fs::write(sh.join("stats.json"), "{}").unwrap();
        let ctx = |q: &str, citing: &str, g: &str, id: (&str, &str)| json!({"qid": format!("q-{q}-{}", id.1), "query": q, "citing_article_id": citing,
            "citing_group": g, id.0: id.1});
        let rows = [ctx("ctx a", "PMC1.1", "g1", ("pmid", "222")), ctx("ctx b", "PMC1.1", "g1", ("pmid", "222")),
            ctx("ctx c", "PMC1.1", "g1", ("pmid", "222")), ctx("ctx d", "PMC1.1", "g1", ("pmcid", "PMC3")),
            ctx("ctx e", "PMC1.1", "g1", ("doi", "10.1/TEST")), ctx("ctx f", "PMC1.1", "g1", ("pmid", "999"))];
        let mut w = crate::io::JsonlOut::create(&sh.join("cite_ctx.jsonl.gz")).unwrap();
        for r in &rows {
            w.put(r).unwrap();
        }
        w.finish().unwrap();
        let mut w = crate::io::JsonlOut::create(&sh.join("articles.jsonl.gz")).unwrap();
        for (id, g) in [("PMC1.1", "g1"), ("PMC2.1", "g2"), ("PMC3.1", "g1"), ("PMC4.1", "g4")] {
            w.put(&json!({"article_id": id, "group": g, "title": format!("T{id}"), "abstract": "A.", "chunks": []})).unwrap();
        }
        w.finish().unwrap();
        std::fs::write(d.path().join("records.jsonl"), [
            r#"{"article_id":"PMC1.1","pmid":"111","doi":""}"#, r#"{"article_id":"PMC2.1","pmid":"222","doi":""}"#,
            r#"{"article_id":"PMC3.1","pmid":"333","doi":""}"#, r#"{"article_id":"PMC4.1","pmid":"444","doi":"10.1/test"}"#,
            r#"{"article_id":"PMC9.1","pmid":"999","doi":""}"#].join("\n")).unwrap();
        // PMC9.1 is not kept (e.g. test split)
        std::fs::write(d.path().join("keep.tsv"), "PMC1.1\tg1\tJ\t1\nPMC2.1\tg2\tJ\t1\nPMC3.1\tg1\tJ\t1\nPMC4.1\tg4\tJ\t1\n").unwrap();
        let rep = run(&CiteArgs { shards: d.path().join("shards"), records: d.path().join("records.jsonl"), keep: d.path().join("keep.tsv"),
            exclude: vec![], per_pair: 2, per_article: 10, out: d.path().join("out") }).unwrap();
        // a, b -> PMC2 (c capped at 2 per pair); d -> PMC3 same group g1: dropped; e -> PMC4 via doi (case-folded); f -> not kept
        assert_eq!(rep["pairs"], 3);
        assert_eq!(rep["counts"]["capped"], 1);
        assert_eq!(rep["counts"]["same_article_or_group"], 1);
        assert_eq!(rep["counts"]["unresolved_or_not_kept"], 1);
        assert_eq!(rep["docs"], 2);
        let docs: Vec<Value> = crate::io::reader(&d.path().join("out/docs.jsonl.gz")).unwrap().lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
        assert!(docs.iter().any(|x| x["id"] == "PMC2.1" && x["text"] == "TPMC2.1\n\nA."));
    }
}
