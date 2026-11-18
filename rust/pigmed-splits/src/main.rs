//! pigmed-splits: retriever training-data census, leakage-safe splits and audit (docs/RETRIEVER_DATA_PLAN.md).
//!
//!   pigmed-splits census    --units DIR [--pmc-meta F] [--pubmed-meta F] [--tokenizer F] --xml-cache DIR --out DIR
//!   pigmed-splits blocklist --out F
//!   pigmed-splits build     --records F [--dup-links F..] [--blocklist F..] --out DIR
//!   pigmed-splits audit     --manifest DIR [--records F] [--dup-links F..] [--blocklist F..] [--previous DIR]
//!                           [--loose-clusters F] [--citation-shards DIR] [--out F]   (exit 1 on any failure)
//!   full corpus: census-shard / minhash-links / census-merge (src/full.rs), then build + audit as above

mod census;
mod full;
mod ids;
mod io;
mod jats;
mod minhash;
mod record;
mod splits;

use anyhow::Result;
use clap::{Parser, Subcommand};
use ids::id_node;
use io::{read_jsonl, write_json, write_jsonl};
use record::Record;
use serde_json::{json, Value};
use splits::{audit, build_splits, label_pair, ArticleRow, Built, GroupRow, Link, SplitConfig};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fetch JATS (cached), extract records, MinHash links, census.json
    Census {
        #[arg(long)] units: PathBuf,
        #[arg(long)] pmc_meta: Option<PathBuf>,
        #[arg(long)] pubmed_meta: Option<PathBuf>,
        #[arg(long)] tokenizer: Option<PathBuf>,
        #[arg(long)] xml_cache: PathBuf,
        #[arg(long, default_value_t = 32)] threads: usize,
        #[arg(long)] limit: Option<usize>,
        #[arg(long)] out: PathBuf,
    },
    /// Full corpus, one shard: S3 metadata JSON + JATS per plan row (no XML cache), signatures, citations
    CensusShard {
        #[arg(long)] plan: PathBuf,
        #[arg(long)] shard: usize,
        #[arg(long)] nshards: usize,
        #[arg(long, default_value_t = 64)] threads: usize,
        #[arg(long)] limit: Option<usize>,
        #[arg(long)] out: PathBuf,
    },
    /// Full corpus: global MinHash LSH over all shard signatures
    MinhashLinks {
        #[arg(long)] shards: PathBuf,
        #[arg(long)] out: PathBuf,
        #[arg(long, default_value_t = 0.8)] threshold: f64,
        #[arg(long, default_value_t = 16)] bands: usize,
        #[arg(long, default_value_t = 200)] max_bucket: usize,
    },
    /// Full corpus: merge shard records (+ e-means article_meta pub types/year) -> records.jsonl.gz, census.json
    CensusMerge {
        #[arg(long)] shards: PathBuf,
        #[arg(long)] article_meta: Option<PathBuf>,
        #[arg(long)] out: PathBuf,
    },
    /// Benchmark-positive blocklist from public qrels (TREC CDS 2014-16, TREC PM 2017/2019 abstracts)
    Blocklist {
        #[arg(long)] out: PathBuf,
        #[arg(long, default_value_t = 1)] min_grade: i64,
    },
    /// records (+ near-dup links, blocklist) -> split manifest
    Build {
        #[arg(long)] records: PathBuf,
        #[arg(long, num_args = 0..)] dup_links: Vec<PathBuf>,
        #[arg(long, num_args = 0..)] blocklist: Vec<PathBuf>,
        #[arg(long)] temporal_cutoff: Option<String>,
        #[arg(long)] test_journals: Option<usize>,
        /// earlier, already-used manifest dir: its held-out articles stay held out
        #[arg(long)] previous: Option<PathBuf>,
        #[arg(long)] out: PathBuf,
    },
    /// Re-check a manifest; exit 1 on any failure
    Audit {
        #[arg(long)] manifest: PathBuf,
        #[arg(long)] records: Option<PathBuf>,
        #[arg(long, num_args = 0..)] dup_links: Vec<PathBuf>,
        #[arg(long, num_args = 0..)] blocklist: Vec<PathBuf>,
        #[arg(long)] previous: Option<PathBuf>,
        #[arg(long)] loose_clusters: Option<PathBuf>,
        /// full corpus: stream citation pairs from these census-shard outputs
        #[arg(long)] citation_shards: Option<PathBuf>,
        #[arg(long)] out: Option<PathBuf>,
    },
}

fn read_links(paths: &[PathBuf]) -> Result<Vec<Link>> {
    let mut out = vec![];
    for p in paths {
        let stem = p.file_name().unwrap().to_string_lossy().split('.').next().unwrap_or("links").to_string();
        for r in read_jsonl::<Value>(p)? {
            let (Some(a), Some(b)) = (r["a"].as_str(), r["b"].as_str()) else { continue };
            let source = r["source"].as_str().map(str::to_string)
                .or_else(|| r["modality"].as_str().map(|m| format!("e3-{m}")))
                .unwrap_or_else(|| stem.clone());
            out.push((a.to_string(), b.to_string(), source));
        }
    }
    Ok(out)
}

fn read_blocklist(paths: &[PathBuf]) -> Result<Vec<String>> {
    let mut out = vec![];
    for p in paths {
        for r in read_jsonl::<Value>(p)? {
            let n = id_node(r["id_type"].as_str().unwrap_or(""), r["id"].as_str().unwrap_or(""));
            if !n.is_empty() {
                out.push(n);
            }
        }
    }
    Ok(out)
}

fn write_manifest(b: &Built, out: &Path) -> Result<Value> {
    write_jsonl(&out.join("articles.jsonl.gz"), b.articles.values())?;
    let nodes: Vec<Value> = b.nodes.iter().map(|(n, g)| json!({"node": n, "group": g})).collect();
    write_jsonl(&out.join("nodes.jsonl.gz"), &nodes)?;
    let groups: Vec<Value> = b.groups.iter().map(|(gid, g)| {
        let mut v = serde_json::to_value(g).unwrap();
        v.as_object_mut().unwrap().insert("group".into(), json!(gid));
        v
    }).collect();
    write_jsonl(&out.join("groups.jsonl.gz"), &groups)?;
    let summary = json!({"config": b.config, "stats": b.stats, "test_journals": b.test_journals,
        "oversized_groups": b.oversized_groups, "related_link_types": b.related_link_types,
        "split_counts": b.split_counts()});
    write_json(&out.join("summary.json"), &summary)?;
    Ok(summary)
}

fn load_manifest(dir: &Path) -> Result<Built> {
    let summary: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("summary.json"))?)?;
    let config: SplitConfig = serde_json::from_value(summary["config"].clone())?;
    let articles: BTreeMap<String, ArticleRow> =
        read_jsonl::<ArticleRow>(&dir.join("articles.jsonl.gz"))?.into_iter().map(|a| (a.article_id.clone(), a)).collect();
    let nodes: HashMap<String, String> = read_jsonl::<Value>(&dir.join("nodes.jsonl.gz"))?.into_iter()
        .filter_map(|r| Some((r["node"].as_str()?.to_string(), r["group"].as_str()?.to_string()))).collect();
    let mut groups = BTreeMap::new();
    for mut r in read_jsonl::<Value>(&dir.join("groups.jsonl.gz"))? {
        let gid = r["group"].as_str().unwrap_or_default().to_string();
        r.as_object_mut().unwrap().remove("group");
        groups.insert(gid, serde_json::from_value::<GroupRow>(r)?);
    }
    let oversized: Vec<(usize, String)> = serde_json::from_value(summary["oversized_groups"].clone()).unwrap_or_default();
    Ok(Built { config, articles, nodes, groups, test_journals: BTreeSet::new(), oversized_groups: oversized,
               related_link_types: BTreeMap::new(), stats: BTreeMap::new() })
}

fn blocklist(out: &Path, min_grade: i64) -> Result<()> {
    const QRELS: [(&str, &str, &str); 5] = [
        ("trec-cds-2014", "https://trec.nist.gov/data/clinical/qrels-treceval-2014.txt", "pmcid"),
        ("trec-cds-2015", "https://trec.nist.gov/data/clinical/qrels-treceval-2015.txt", "pmcid"),
        ("trec-cds-2016", "https://trec.nist.gov/data/clinical/qrels-treceval-2016.txt", "pmcid"),
        ("trec-pm-2017-abstracts", "https://trec.nist.gov/data/precmed/qrels-final-abstracts.txt", "pmid"),
        ("trec-pm-2019-abstracts", "https://trec.nist.gov/data/precmed/qrels-treceval-abstracts.2019.txt", "pmid"),
    ];
    let agent = ureq::AgentBuilder::new().user_agent("PigMed/0.1 (benchmark blocklist)").build();
    let (mut rows, mut counts) = (vec![], BTreeMap::new());
    for (name, url, id_type) in QRELS {
        let text = agent.get(url).call()?.into_string()?;
        let mut uniq = HashSet::new();
        let mut n = 0;
        for line in text.lines() {
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() < 4 {
                continue;
            }
            let grade = p[3].parse::<f64>().unwrap_or(0.0) as i64;
            if grade < min_grade {
                continue;
            }
            let id = if id_type == "pmcid" && p[2].bytes().all(|b| b.is_ascii_digit()) { format!("PMC{}", p[2]) } else { p[2].to_string() };
            uniq.insert(id.clone());
            n += 1;
            rows.push(json!({"benchmark": name, "id_type": id_type, "id": id, "grade": grade}));
        }
        counts.insert(name, json!({"positives": n, "unique_ids": uniq.len(), "url": url}));
    }
    write_jsonl(out, &rows)?;
    println!("{}", serde_json::to_string_pretty(&counts)?);
    Ok(())
}

fn read_previous(dir: &Path) -> Result<HashMap<String, String>> {
    Ok(read_jsonl::<ArticleRow>(&dir.join("articles.jsonl.gz"))?.into_iter().map(|a| (a.article_id, a.split)).collect())
}

/// Re-key an earlier manifest onto the current article ids by PMCID, so a new article version (PMC1.1 -> PMC1.2)
/// inherits its old split. Exact article ids win over version matches.
fn remap_previous<'a>(prev: HashMap<String, String>, current: impl Iterator<Item = &'a str>) -> HashMap<String, String> {
    let base = |a: &str| a.split('.').next().unwrap_or(a).to_string();
    let mut by_base: HashMap<String, &String> = HashMap::new();
    for (a, s) in &prev {
        let e = by_base.entry(base(a)).or_insert(s);
        if *e == "train" {
            *e = s; // any held-out version wins over train
        }
    }
    let mut out = HashMap::new();
    for a in current {
        if let Some(s) = prev.get(a).or_else(|| by_base.get(&base(a)).copied()) {
            out.insert(a.to_string(), s.clone());
        }
    }
    out
}

fn citation_pairs(records: &[Record]) -> impl Iterator<Item = (String, String)> + '_ {
    records.iter().flat_map(|r| r.citation_ids().map(move |(t, v)| (r.article_id.clone(), id_node(t, &v))))
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Census { units, pmc_meta, pubmed_meta, tokenizer, xml_cache, threads, limit, out } => {
            census::run(&census::CensusArgs { units, pmc_meta, pubmed_meta, tokenizer, xml_cache, threads, limit, out })
        }
        Cmd::CensusShard { plan, shard, nshards, threads, limit, out } => {
            full::run_shard(&full::ShardArgs { plan, shard, nshards, threads, limit, out })
        }
        Cmd::MinhashLinks { shards, out, threshold, bands, max_bucket } => full::minhash_links(&shards, &out, threshold, bands, max_bucket),
        Cmd::CensusMerge { shards, article_meta, out } => full::merge(&shards, article_meta.as_deref(), &out),
        Cmd::Blocklist { out, min_grade } => blocklist(&out, min_grade),
        Cmd::Build { records, dup_links, blocklist, temporal_cutoff, test_journals, previous, out } => {
            let records: Vec<Record> = read_jsonl(&records)?;
            let external: HashSet<String> = citation_pairs(&records).map(|(_, n)| n).collect();
            let mut cfg = SplitConfig::default();
            if let Some(c) = temporal_cutoff { cfg.temporal_cutoff = c; }
            if let Some(n) = test_journals { cfg.test_journals = n; }
            let prev = previous.map(|d| read_previous(&d)).transpose()?
                .map(|p| remap_previous(p, records.iter().map(|r| r.article_id.as_str())));
            let built = build_splits(&records, cfg, &read_links(&dup_links)?, &read_blocklist(&blocklist)?, &external, None, prev.as_ref())?;
            println!("{}", serde_json::to_string_pretty(&write_manifest(&built, &out)?)?);
            Ok(())
        }
        Cmd::Audit { manifest, records, dup_links, blocklist, previous, loose_clusters, citation_shards, out } => {
            let built = load_manifest(&manifest)?;
            let pairs: Vec<(String, String, Option<String>)> = match records {
                Some(p) => {
                    let recs: Vec<Record> = read_jsonl(&p)?;
                    citation_pairs(&recs).map(|(q, p)| { let s = label_pair(&built, &q, &p); (q, p, s) }).collect()
                }
                None => vec![],
            };
            let prev = previous.map(|d| read_previous(&d)).transpose()?
                .map(|p| remap_previous(p, built.articles.keys().map(String::as_str)));
            let mut loose = vec![];
            if let Some(p) = loose_clusters {
                let m: BTreeMap<String, String> = serde_json::from_str(&std::fs::read_to_string(p)?)?;
                let mut members: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for (aid, c) in m {
                    members.entry(c).or_default().push(aid);
                }
                for v in members.values().filter(|v| v.len() <= 200) {
                    for i in 0..v.len() {
                        for j in i + 1..v.len() {
                            loose.push((v[i].clone(), v[j].clone(), "loose".to_string()));
                        }
                    }
                }
            }
            let mut rep = audit(&built, &pairs, &read_links(&dup_links)?, &read_blocklist(&blocklist)?, prev.as_ref(), &loose);
            if let Some(dir) = citation_shards {
                full::for_each_citation(&dir, |q, p| {
                    let s = label_pair(&built, q, p);
                    splits::audit_pair(&mut rep, &built, q, p, s.as_deref());
                })?;
                rep.ok = rep.failures.is_empty();
            }
            let text = serde_json::to_string_pretty(&rep)?;
            if let Some(o) = out {
                std::fs::write(o, &text)?;
            }
            println!("{text}");
            std::process::exit(if rep.ok { 0 } else { 1 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_remaps_by_pmcid() {
        let prev: HashMap<String, String> = [("PMC1.1", "test"), ("PMC2.1", "dev"), ("PMC2.2", "train")]
            .into_iter().map(|(a, s)| (a.to_string(), s.to_string())).collect();
        let m = remap_previous(prev, ["PMC1.3", "PMC2.2", "PMC2.3", "PMC9.1"].into_iter());
        assert_eq!(m.get("PMC2.3").map(String::as_str), Some("dev"));
        assert_eq!(m.get("PMC1.3").map(String::as_str), Some("test"));
        assert_eq!(m.get("PMC2.2").map(String::as_str), Some("train"));
        assert!(!m.contains_key("PMC9.1"));
    }

    #[test]
    fn manifest_roundtrip_and_audit() {
        let dir = tempfile::tempdir().unwrap();
        let records: Vec<Record> = (1..60).map(|i| Record {
            article_id: format!("PMC{i}.1"), pmcid: format!("PMC{i}"), pmid: i.to_string(),
            references: vec![record::Ref { pmid: Some((i + 1).to_string()), ..Default::default() }],
            ..Default::default()
        }).collect();
        let external: HashSet<String> = citation_pairs(&records).map(|(_, n)| n).collect();
        let cfg = SplitConfig { test_journals: 0, ..Default::default() };
        let built = build_splits(&records, cfg, &[], &["pmid:5".into()], &external, None, None).unwrap();
        let summary = write_manifest(&built, dir.path()).unwrap();
        assert_eq!(summary["split_counts"]["eval_reserved"]["articles"], 1);
        let loaded = load_manifest(dir.path()).unwrap();
        assert_eq!(loaded.articles.len(), 59);
        let pairs: Vec<_> = citation_pairs(&records).map(|(q, p)| { let s = label_pair(&loaded, &q, &p); (q, p, s) }).collect();
        let prev: HashMap<String, String> = built.articles.values().map(|a| (a.article_id.clone(), a.split.clone())).collect();
        let rep = audit(&loaded, &pairs, &[], &["pmid:5".into()], Some(&prev), &[]);
        assert!(rep.ok, "{:?}", rep.failures);
        assert_eq!(rep.pairs.values().sum::<usize>(), 59);
    }
}
