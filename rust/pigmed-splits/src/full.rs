//! Full-corpus census (millions of articles): sharded fetch of the PMC S3 metadata JSON + JATS (no XML cache),
//! MinHash signatures computed in place, citations written as a side table (records carry counts only), then a
//! global MinHash LSH step and a merge step that adds PubMed metadata (publication types, year).
//!
//!   census-shard  --plan DIR --shard I --nshards N --out DIR    -> DIR/shard-IIII/{records.jsonl.gz,
//!                                                                  citations.tsv.gz, sig_ids.txt, sigs.u32, stats.json}
//!   minhash-links --shards DIR --out F
//!   census-merge  --shards DIR [--article-meta F] --out DIR      -> records.jsonl.gz, census.json

use crate::census::summarize;
use crate::ids::{commercial_use, id_node, iso_date};
use crate::io::{reader, write_json, write_jsonl};
use crate::jats::extract;
use crate::minhash;
use crate::record::Record;
use anyhow::{anyhow, bail, Context, Result};
use flate2::{write::GzEncoder, Compression};
use rayon::prelude::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const BASE: &str = "https://pmc-oa-opendata.s3.amazonaws.com/";

pub struct ShardArgs {
    pub plan: PathBuf,
    pub shard: usize,
    pub nshards: usize,
    pub threads: usize,
    pub limit: Option<usize>,
    pub out: PathBuf,
}

pub fn shard_dir(out: &Path, shard: usize) -> PathBuf {
    out.join(format!("shard-{shard:04}"))
}

/// Plan rows (pmcid, version) of the parts whose index % nshards == shard. Article id = "<pmcid>.<version>".
fn plan_rows(plan: &Path, shard: usize, nshards: usize) -> Result<Vec<(String, String)>> {
    let mut parts: Vec<PathBuf> = std::fs::read_dir(plan)?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("part-"))).collect();
    parts.sort();
    let mut out = vec![];
    for (i, p) in parts.iter().enumerate() {
        if i % nshards != shard {
            continue;
        }
        for line in reader(p)?.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let r: Value = serde_json::from_str(&line)?;
            let pmcid = r["pmcid"].as_str().ok_or_else(|| anyhow!("plan row without pmcid in {}", p.display()))?;
            let key = r["key"].as_str().map(str::to_string)
                .unwrap_or_else(|| format!("metadata/{pmcid}.{}.json", r["version"]));
            out.push((format!("{pmcid}.{}", r["version"]), key));
        }
    }
    Ok(out)
}

fn get(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let mut last = String::new();
    for attempt in 0..7u32 {
        match agent.get(url).call() {
            Ok(resp) => {
                let mut body = vec![];
                resp.into_reader().read_to_end(&mut body)?;
                return Ok(body);
            }
            Err(ureq::Error::Status(code @ (403 | 404), _)) => bail!("NotFound {code} {url}"),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(500 * 2u64.pow(attempt).min(60)));
    }
    Err(anyhow!("Failed {url}: {last}"))
}

fn s3_https(url: &str) -> Option<String> {
    let rest = url.strip_prefix("s3://pmc-oa-opendata/")?;
    Some(format!("{BASE}{}", rest.split('?').next().unwrap_or(rest)))
}

fn str_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// How one article was read.
enum Got {
    Jats(Record, Option<Vec<u32>>),
    /// no JATS (PDF/scan-only article): ids, licence, title from the metadata JSON
    MetaOnly(Record),
}

fn one(agent: &ureq::Agent, article_id: &str, key: &str) -> Result<Got> {
    let meta: Value = serde_json::from_slice(&get(agent, &format!("{BASE}{key}"))?).context("Metadata JSON")?;
    let xml_url = meta["xml_url"].as_str().and_then(s3_https);
    let mut got = match xml_url {
        Some(u) => {
            let xml = String::from_utf8_lossy(&get(agent, &u)?).into_owned();
            let (rec, mh) = extract(&xml, None)?;
            let hashes = minhash::shingle_hashes(&mh, 5);
            let sig = (hashes.len() >= 50).then(|| minhash::signature(&hashes));
            Got::Jats(rec, sig)
        }
        None => Got::MetaOnly(Record { title: str_of(&meta["title"]).unwrap_or_default(), ..Default::default() }),
    };
    let rec = match &mut got {
        Got::Jats(r, _) | Got::MetaOnly(r) => r,
    };
    rec.article_id = article_id.to_string();
    if rec.pmcid.is_empty() {
        rec.pmcid = article_id.split('.').next().unwrap_or("").to_string();
    }
    if rec.pmid.is_empty() {
        rec.pmid = str_of(&meta["pmid"]).unwrap_or_default();
    }
    if rec.doi.is_empty() {
        rec.doi = str_of(&meta["doi"]).unwrap_or_default();
    }
    rec.license_code = str_of(&meta["license_code"]);
    rec.commercial = commercial_use(rec.license_code.as_deref());
    rec.is_retracted = meta["is_retracted"].as_bool().unwrap_or(false);
    rec.dates = rec.pub_dates.iter().filter_map(|d| iso_date(d.year.as_deref(), d.month.as_deref(), d.day.as_deref())).collect();
    rec.dates.sort();
    Ok(got)
}

fn gz_writer(path: &Path) -> Result<GzEncoder<BufWriter<std::fs::File>>> {
    Ok(GzEncoder::new(BufWriter::new(std::fs::File::create(path)?), Compression::new(6)))
}

pub fn run_shard(a: &ShardArgs) -> Result<()> {
    let dir = shard_dir(&a.out, a.shard);
    std::fs::create_dir_all(&dir)?;
    let mut rows = plan_rows(&a.plan, a.shard, a.nshards)?;
    rows.truncate(a.limit.unwrap_or(usize::MAX));
    eprintln!("shard {}/{}: {} articles", a.shard, a.nshards, rows.len());
    let pool = rayon::ThreadPoolBuilder::new().num_threads(a.threads).build()?;
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(90)).max_idle_connections_per_host(a.threads)
        .user_agent("PigMed/0.1 (retriever data census)").build();
    let done = AtomicUsize::new(0);
    let t0 = Instant::now();
    let results: Vec<(String, Result<Got>)> = pool.install(|| {
        rows.par_iter().map(|(aid, key)| {
            let r = one(&agent, aid, key);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 20000 == 0 {
                eprintln!("{n}/{} {:.0}s", rows.len(), t0.elapsed().as_secs_f64());
            }
            (aid.clone(), r)
        }).collect()
    });
    let fetch_s = t0.elapsed().as_secs_f64();
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp)?;
    let mut recw = gz_writer(&tmp.join("records.jsonl.gz"))?;
    let mut citw = gz_writer(&tmp.join("citations.tsv.gz"))?;
    let mut idw = BufWriter::new(std::fs::File::create(tmp.join("sig_ids.txt"))?);
    let mut sigw = BufWriter::new(std::fs::File::create(tmp.join("sigs.u32"))?);
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    let mut errw = BufWriter::new(std::fs::File::create(tmp.join("errors.tsv"))?);
    let (mut n_jats, mut n_meta, mut n_sig, mut n_cit, mut n_refs) = (0usize, 0usize, 0usize, 0usize, 0usize);
    for (aid, res) in results {
        let mut rec = match res {
            Ok(Got::Jats(rec, sig)) => {
                n_jats += 1;
                if let Some(s) = sig {
                    n_sig += 1;
                    writeln!(idw, "{aid}")?;
                    for x in s {
                        sigw.write_all(&x.to_le_bytes())?;
                    }
                }
                rec
            }
            Ok(Got::MetaOnly(rec)) => {
                n_meta += 1;
                rec
            }
            Err(e) => {
                let msg = format!("{e:#}").replace(['\t', '\n'], " ");
                *errors.entry(msg.split_whitespace().next().unwrap_or("error").to_string()).or_default() += 1;
                writeln!(errw, "{aid}\t{msg}")?;
                continue;
            }
        };
        n_refs += rec.references.len();
        for (t, v) in rec.citation_ids() {
            writeln!(citw, "{}\t{}", rec.article_id, id_node(t, &v))?;
            n_cit += 1;
        }
        rec.references.clear();
        serde_json::to_writer(&mut recw, &rec)?;
        recw.write_all(b"\n")?;
    }
    recw.finish()?.flush()?;
    citw.finish()?.flush()?;
    idw.flush()?;
    sigw.flush()?;
    errw.flush()?;
    let stats = json!({"shard": a.shard, "nshards": a.nshards, "planned": rows.len(), "jats": n_jats,
        "meta_only": n_meta, "signatures": n_sig, "references": n_refs, "citation_pairs": n_cit,
        "errors": errors, "seconds": fetch_s, "threads": a.threads});
    write_json(&tmp.join("stats.json"), &stats)?;
    for f in ["records.jsonl.gz", "citations.tsv.gz", "sig_ids.txt", "sigs.u32", "errors.tsv", "stats.json"] {
        std::fs::rename(tmp.join(f), dir.join(f))?;
    }
    std::fs::remove_dir(&tmp)?;
    println!("{}", serde_json::to_string(&stats)?);
    Ok(())
}

pub fn shard_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("shard-")) && p.join("stats.json").exists())
        .collect();
    v.sort();
    Ok(v)
}

/// Global banded LSH over every shard's signatures -> minhash_links.jsonl.gz (same rule as `census`).
pub fn minhash_links(root: &Path, out: &Path, threshold: f64, bands: usize, max_bucket: usize) -> Result<()> {
    let (mut ids, mut sigs) = (vec![], vec![]);
    for d in shard_dirs(root)? {
        let shard_ids: Vec<String> = std::fs::read_to_string(d.join("sig_ids.txt"))?.lines().map(str::to_string).collect();
        let mut raw = vec![];
        std::fs::File::open(d.join("sigs.u32"))?.read_to_end(&mut raw)?;
        if raw.len() != shard_ids.len() * minhash::NUM_PERM * 4 {
            bail!("{}: {} ids but {} signature bytes", d.display(), shard_ids.len(), raw.len());
        }
        for c in raw.chunks_exact(minhash::NUM_PERM * 4) {
            sigs.push(c.chunks_exact(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect::<Vec<u32>>());
        }
        ids.extend(shard_ids);
    }
    let valid = vec![true; sigs.len()];
    let t0 = Instant::now();
    let (pairs, st) = minhash::duplicates(&sigs, &valid, threshold, bands, max_bucket);
    let links: Vec<Value> = pairs.iter().map(|&(i, j, e)| json!({"a": ids[i], "b": ids[j], "source": "minhash",
        "jaccard": (e * 1000.0).round() / 1000.0})).collect();
    write_jsonl(out, &links)?;
    let rep = json!({"signatures": sigs.len(), "candidates": st.candidates, "skipped_buckets": st.skipped_buckets,
        "pairs": pairs.len(), "threshold": threshold, "bands": bands, "max_bucket": max_bucket, "seconds": t0.elapsed().as_secs_f64()});
    write_json(&out.with_extension("").with_extension("json"), &rep)?;
    println!("{}", serde_json::to_string_pretty(&rep)?);
    Ok(())
}

struct Meta {
    pub_types: Vec<String>,
    year: Option<String>,
    pmid: Option<String>,
    doi: Option<String>,
}

/// e-means article_meta (pmcid -> pub_types, year, pmid, doi), compact.
fn read_article_meta(p: &Path) -> Result<HashMap<String, Meta>> {
    let mut m = HashMap::new();
    for line in reader(p)?.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let r: Value = serde_json::from_str(&line)?;
        let Some(pmcid) = r["pmcid"].as_str() else { continue };
        m.insert(pmcid.to_string(), Meta {
            pub_types: r["pub_types"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            year: str_of(&r["year"]),
            pmid: str_of(&r["pmid"]),
            doi: str_of(&r["doi"]),
        });
    }
    Ok(m)
}

/// Concatenate shard records, add PubMed publication types / year, write records.jsonl.gz + census.json.
pub fn merge(root: &Path, article_meta: Option<&Path>, out: &Path) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let meta = match article_meta {
        Some(p) => read_article_meta(p)?,
        None => HashMap::new(),
    };
    eprintln!("article_meta {}", meta.len());
    let dirs = shard_dirs(root)?;
    let mut records: Vec<Record> = vec![];
    let mut shard_stats = vec![];
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    let mut errors: BTreeMap<String, u64> = BTreeMap::new();
    let mut with_meta = 0usize;
    for d in &dirs {
        let st: Value = serde_json::from_str(&std::fs::read_to_string(d.join("stats.json"))?)?;
        for k in ["planned", "jats", "meta_only", "signatures", "references", "citation_pairs"] {
            *totals.entry(k.into()).or_default() += st[k].as_u64().unwrap_or(0);
        }
        if let Some(e) = st["errors"].as_object() {
            for (k, v) in e {
                *errors.entry(k.clone()).or_default() += v.as_u64().unwrap_or(0);
            }
        }
        for line in reader(&d.join("records.jsonl.gz"))?.lines() {
            let mut r: Record = serde_json::from_str(&line?)?;
            if let Some(m) = meta.get(&r.pmcid) {
                with_meta += 1;
                if r.pmid.is_empty() {
                    r.pmid = m.pmid.clone().unwrap_or_default();
                }
                if r.doi.is_empty() {
                    r.doi = m.doi.clone().unwrap_or_default();
                }
                r.pub_types = m.pub_types.clone();
                if r.dates.is_empty() {
                    r.dates.extend(iso_date(m.year.as_deref(), None, None));
                }
            }
            r.retraction_notice = r.pub_types.iter().any(|t| t == "Retraction of Publication") || r.article_type == "retraction";
            r.is_retracted |= r.pub_types.iter().any(|t| t == "Retracted Publication");
            records.push(r);
        }
        shard_stats.push(st);
    }
    let mut seen = std::collections::HashSet::new();
    let dups = records.iter().filter(|r| !seen.insert(r.article_id.as_str())).count();
    if dups > 0 {
        bail!("{dups} duplicate article ids across shards");
    }
    write_jsonl(&out.join("records.jsonl.gz"), &records)?;
    let mut c = summarize(&records);
    c["full_census"] = json!({"shards": dirs.len(), "totals": totals, "errors": errors, "with_article_meta": with_meta,
        "fetch_seconds_sum": shard_stats.iter().map(|s| s["seconds"].as_f64().unwrap_or(0.0)).sum::<f64>(),
        "note": "references are in shard citations.tsv.gz; per-record reference lists are empty, so the references block is zero"});
    write_json(&out.join("census.json"), &c)?;
    println!("{}", serde_json::to_string_pretty(&c)?);
    Ok(())
}

/// Citation pairs (citing article id, cited node) streamed from every shard's citations.tsv.gz.
pub fn for_each_citation(root: &Path, mut f: impl FnMut(&str, &str)) -> Result<()> {
    for d in shard_dirs(root)? {
        for line in reader(&d.join("citations.tsv.gz"))?.lines() {
            let line = line?;
            if let Some((a, n)) = line.split_once('\t') {
                f(a, n);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_urls_and_plan_rows() {
        assert_eq!(s3_https("s3://pmc-oa-opendata/PMC1.1/PMC1.1.xml?md5=ab").unwrap(),
                   "https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.xml");
        assert!(s3_https("s3://other/x").is_none());
        let d = tempfile::tempdir().unwrap();
        for (i, ids) in [(0, ["PMC1", "PMC2"]), (1, ["PMC3", "PMC4"]), (2, ["PMC5", "PMC6"])] {
            let body: String = ids.iter().map(|p| format!("{{\"pmcid\":\"{p}\",\"version\":2,\"key\":\"metadata/{p}.2.json\"}}\n")).collect();
            std::fs::write(d.path().join(format!("part-{i:05}.jsonl")), body).unwrap();
        }
        std::fs::write(d.path().join("plan.json"), "{}").unwrap();
        let s0 = plan_rows(d.path(), 0, 2).unwrap();
        let s1 = plan_rows(d.path(), 1, 2).unwrap();
        assert_eq!(s0.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["PMC1.2", "PMC2.2", "PMC5.2", "PMC6.2"]);
        assert_eq!(s1[0], ("PMC3.2".to_string(), "metadata/PMC3.2.json".to_string()));
    }

    #[test]
    fn minhash_links_over_shards() {
        let d = tempfile::tempdir().unwrap();
        let text = (0..300).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let sig = minhash::signature(&minhash::shingle_hashes(&text, 5));
        let other = minhash::signature(&minhash::shingle_hashes(&(0..300).map(|i| format!("z{i}")).collect::<Vec<_>>().join(" "), 5));
        for (s, ids, sigs) in [(0, vec!["A.1", "B.1"], vec![&sig, &other]), (1, vec!["C.1"], vec![&sig])] {
            let sd = shard_dir(d.path(), s);
            std::fs::create_dir_all(&sd).unwrap();
            std::fs::write(sd.join("sig_ids.txt"), ids.join("\n") + "\n").unwrap();
            let raw: Vec<u8> = sigs.iter().flat_map(|v| v.iter().flat_map(|x| x.to_le_bytes())).collect();
            std::fs::write(sd.join("sigs.u32"), raw).unwrap();
            std::fs::write(sd.join("stats.json"), "{}").unwrap();
        }
        let out = d.path().join("links.jsonl.gz");
        minhash_links(d.path(), &out, 0.8, 16, 200).unwrap();
        let links: Vec<Value> = crate::io::read_jsonl(&out).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!((links[0]["a"].as_str(), links[0]["b"].as_str()), (Some("A.1"), Some("C.1")));
    }
}
