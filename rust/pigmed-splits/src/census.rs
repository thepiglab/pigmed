//! Corpus census: fetch each article's JATS from the public PMC Open Data bucket (cached), extract records,
//! MinHash near-duplicate links, and summary statistics (length tail in EG2 tokens, licences, free supervision).

use crate::ids::{commercial_use, id_node, iso_date, normal_license};
use crate::io::{gunzip, gzip, read_jsonl, write_json, write_jsonl};
use crate::jats::{extract, TokenCount};
use crate::minhash;
use crate::record::Record;
use anyhow::{anyhow, bail, Result};
use rayon::prelude::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const BASE: &str = "https://pmc-oa-opendata.s3.amazonaws.com/";

pub struct CensusArgs {
    pub units: PathBuf,
    pub pmc_meta: Option<PathBuf>,
    pub pubmed_meta: Option<PathBuf>,
    pub tokenizer: Option<PathBuf>,
    pub xml_cache: PathBuf,
    pub threads: usize,
    pub limit: Option<usize>,
    pub out: PathBuf,
}

fn fetch(agent: &ureq::Agent, cache: &Path, article_id: &str) -> Result<String> {
    let path = cache.join(format!("{article_id}.xml.gz"));
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(String::from_utf8_lossy(&gunzip(&bytes)?).into_owned());
    }
    let url = format!("{BASE}{article_id}/{article_id}.xml");
    let mut last = String::new();
    for attempt in 0..6u32 {
        match agent.get(&url).call() {
            Ok(resp) => {
                let mut body = vec![];
                std::io::Read::read_to_end(&mut resp.into_reader(), &mut body)?;
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, gzip(&body)?)?;
                std::fs::rename(&tmp, &path)?;
                return Ok(String::from_utf8_lossy(&body).into_owned());
            }
            Err(ureq::Error::Status(code @ (403 | 404), _)) => bail!("NotFound {code} {url}"),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(2u64.pow(attempt).min(30)));
    }
    Err(anyhow!("failed {url}: {last}"))
}

fn str_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

pub fn run(a: &CensusArgs) -> Result<()> {
    std::fs::create_dir_all(&a.xml_cache)?;
    std::fs::create_dir_all(&a.out)?;
    // article rows of the ingest units (article_id -> licence)
    let mut arts: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&a.units)?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".jsonl.gz")).collect();
    files.sort();
    for f in files {
        for r in read_jsonl::<Value>(&f)? {
            if r["kind"] == "article" {
                if let Some(id) = r["article_id"].as_str() {
                    arts.insert(id.to_string(), str_of(&r["license"]));
                }
            }
        }
    }
    let pmc: HashMap<String, Value> = match &a.pmc_meta {
        Some(p) => read_jsonl::<Value>(p)?.into_iter().filter_map(|r| Some((r["article_id"].as_str()?.to_string(), r))).collect(),
        None => HashMap::new(),
    };
    let pubmed: HashMap<String, Value> = match &a.pubmed_meta {
        Some(p) => read_jsonl::<Value>(p)?.into_iter().filter_map(|r| Some((str_of(&r["pmid"])?, r))).collect(),
        None => HashMap::new(),
    };
    let ids: Vec<String> = arts.keys().take(a.limit.unwrap_or(usize::MAX)).cloned().collect();
    eprintln!("articles {}; pmc_meta {}; pubmed {}", ids.len(), pmc.len(), pubmed.len());

    #[cfg(feature = "tokens")]
    let tok = match &a.tokenizer {
        Some(p) => Some(tokenizers::Tokenizer::from_file(p).map_err(|e| anyhow!("tokenizer: {e}"))?),
        None => None,
    };
    #[cfg(feature = "tokens")]
    let count = |s: &str| tok.as_ref().map(|t| t.encode(s, false).map(|e| e.len()).unwrap_or(0)).unwrap_or(0);
    #[cfg(not(feature = "tokens"))]
    let count = |_: &str| 0usize;
    #[cfg(not(feature = "tokens"))]
    if a.tokenizer.is_some() {
        bail!("built without the `tokens` feature");
    }
    let counter: TokenCount = if a.tokenizer.is_some() { Some(&count) } else { None };

    let pool = rayon::ThreadPoolBuilder::new().num_threads(a.threads).build()?;
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).user_agent("PigMed/0.1 (retriever data census)").build();
    let done = AtomicUsize::new(0);
    let t0 = Instant::now();
    let results: Vec<(String, Result<(Record, String)>)> = pool.install(|| {
        ids.par_iter()
            .map(|aid| {
                let r = fetch(&agent, &a.xml_cache, aid).and_then(|xml| extract(&xml, counter));
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if n % 5000 == 0 {
                    eprintln!("{n}/{} {:.0}s", ids.len(), t0.elapsed().as_secs_f64());
                }
                (aid.clone(), r)
            })
            .collect()
    });
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    let mut records = vec![];
    let mut mh_texts = vec![];
    for (aid, res) in results {
        let (mut rec, mh) = match res {
            Ok(x) => x,
            Err(e) => {
                let msg = format!("{e:#}");
                let kind = msg.split_whitespace().next().unwrap_or("error").to_string();
                if errors.values().sum::<usize>() < 20 {
                    eprintln!("ERR {aid} {msg}");
                }
                *errors.entry(kind).or_default() += 1;
                continue;
            }
        };
        let m = pmc.get(&aid).cloned().unwrap_or(Value::Null);
        rec.article_id = aid.clone();
        if rec.pmcid.is_empty() {
            rec.pmcid = aid.split('.').next().unwrap_or("").to_string();
        }
        if rec.pmid.is_empty() {
            rec.pmid = str_of(&m["pmid"]).unwrap_or_default();
        }
        if rec.doi.is_empty() {
            rec.doi = str_of(&m["doi"]).unwrap_or_default();
        }
        rec.license_code = str_of(&m["license_code"]).or_else(|| arts[&aid].clone());
        rec.commercial = commercial_use(rec.license_code.as_deref());
        rec.is_retracted = m["is_retracted"].as_bool().unwrap_or(false);
        let pm = pubmed.get(&rec.pmid).cloned().unwrap_or(Value::Null);
        rec.pub_types = pm["pub_types"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        rec.retraction_notice = rec.pub_types.iter().any(|t| t == "Retraction of Publication") || rec.article_type == "retraction";
        rec.is_retracted |= rec.pub_types.iter().any(|t| t == "Retracted Publication");
        let mut dates: Vec<String> = rec.pub_dates.iter()
            .filter_map(|d| iso_date(d.year.as_deref(), d.month.as_deref(), d.day.as_deref())).collect();
        if dates.is_empty() {
            if let Some(y) = str_of(&pm["year"]) {
                dates.extend(iso_date(Some(&y), None, None));
            }
        }
        dates.sort();
        rec.dates = dates;
        records.push(rec);
        mh_texts.push(mh);
    }
    let fetch_s = t0.elapsed().as_secs_f64();
    write_jsonl(&a.out.join("records.jsonl.gz"), &records)?;
    // MinHash links
    let hashes: Vec<Vec<u64>> = pool.install(|| mh_texts.par_iter().map(|t| minhash::shingle_hashes(t, 5)).collect());
    let sigs: Vec<Vec<u32>> = pool.install(|| hashes.par_iter().map(|h| minhash::signature(h)).collect());
    let valid: Vec<bool> = hashes.iter().map(|h| h.len() >= 50).collect();
    let (pairs, st) = minhash::duplicates(&sigs, &valid, 0.8, 16, 200);
    let links: Vec<Value> = pairs.iter().map(|&(i, j, e)| json!({"a": records[i].article_id, "b": records[j].article_id,
        "source": "minhash", "jaccard": (e * 1000.0).round() / 1000.0})).collect();
    write_jsonl(&a.out.join("minhash_links.jsonl.gz"), &links)?;
    let mut c = summarize(&records);
    c["fetch"] = json!({"seconds": fetch_s, "errors": errors, "threads": a.threads});
    c["minhash"] = json!({"candidates": st.candidates, "skipped_buckets": st.skipped_buckets, "pairs": pairs.len(),
        "threshold": 0.8, "bands": 16, "num_perm": minhash::NUM_PERM, "min_shingles": 50,
        "articles_with_signature": valid.iter().filter(|v| **v).count()});
    write_json(&a.out.join("census.json"), &c)?;
    println!("{}", serde_json::to_string_pretty(&c)?);
    Ok(())
}

fn percentiles(v: &mut [usize]) -> Value {
    if v.is_empty() {
        return json!({});
    }
    v.sort_unstable();
    // numpy's default linear interpolation
    let q = |p: f64| {
        let pos = p / 100.0 * (v.len() - 1) as f64;
        let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
        (v[lo] as f64 + (v[hi] as f64 - v[lo] as f64) * (pos - lo as f64)) as i64
    };
    let mean = v.iter().sum::<usize>() as f64 / v.len() as f64;
    json!({"p50": q(50.0), "p75": q(75.0), "p90": q(90.0), "p95": q(95.0), "p99": q(99.0), "mean": mean, "max": v[v.len() - 1]})
}

pub fn summarize(records: &[Record]) -> Value {
    let mut out = json!({"articles": records.len()});
    let mut lic: BTreeMap<String, usize> = BTreeMap::new();
    for r in records {
        *lic.entry(normal_license(r.license_code.as_deref())).or_default() += 1;
    }
    out["licences"] = json!(lic);
    let com: Vec<&Record> = records.iter().filter(|r| r.commercial).collect();
    out["commercial_use"] = json!({"articles": com.len(), "share": com.len() as f64 / records.len().max(1) as f64});
    let all: Vec<&Record> = records.iter().collect();
    for (name, rows) in [("all", &all), ("commercial", &com)] {
        let mut tok: Vec<usize> = rows.iter().filter_map(|r| r.eg2_tokens).collect();
        let over = |lim: usize| tok.iter().filter(|t| **t > lim).count() as f64 / tok.len().max(1) as f64;
        let (o8, o16, o32) = (over(8192), over(16384), over(32768));
        let mut with_id: BTreeMap<&str, usize> = BTreeMap::new();
        let mut cited: HashSet<String> = HashSet::new();
        for r in rows.iter() {
            for rf in &r.references {
                for (t, v) in [("pmid", &rf.pmid), ("pmcid", &rf.pmcid), ("doi", &rf.doi)] {
                    if v.as_deref().is_some_and(|v| !id_node(t, v).is_empty()) {
                        *with_id.entry(t).or_default() += 1;
                    }
                }
            }
            for (t, v) in r.citation_ids() {
                cited.insert(id_node(t, &v));
            }
        }
        let sum = |f: &dyn Fn(&Record) -> usize| rows.iter().map(|r| f(r)).sum::<usize>();
        out[name] = json!({
            "eg2_tokens": percentiles(&mut tok),
            "share_over_8192": o8, "share_over_16384": o16, "share_over_32768": o32,
            "articles_with_body": rows.iter().filter(|r| r.words > 300).count(),
            "question_titles": rows.iter().filter(|r| r.question_title).count(),
            "question_titles_with_abstract": rows.iter().filter(|r| r.question_title && r.has_abstract).count(),
            "references": {"total": sum(&|r| r.n_refs), "resolvable_any": sum(&|r| r.n_refs_resolvable),
                "with_id": with_id, "unique_cited_ids": cited.len(),
                "citation_context_pairs": sum(&|r| r.citation_context_pairs),
                "citing_paragraphs": sum(&|r| r.citation_paragraphs)},
            "figures": {"figs": sum(&|r| r.n_figs), "fig_mentions": sum(&|r| r.fig_mentions)},
        });
    }
    let count_by = |f: &dyn Fn(&Record) -> Vec<String>| {
        let mut m: BTreeMap<String, usize> = BTreeMap::new();
        for r in records {
            for k in f(r) {
                *m.entry(k).or_default() += 1;
            }
        }
        m
    };
    out["article_types"] = json!(count_by(&|r| vec![if r.article_type.is_empty() { "?".into() } else { r.article_type.clone() }]));
    out["related_link_types"] = json!(count_by(&|r| r.related.iter().map(|l| l.kind.clone().unwrap_or_else(|| "?".into())).collect()));
    out["retracted"] = json!(records.iter().filter(|r| r.is_retracted).count());
    out["retraction_notices"] = json!(records.iter().filter(|r| r.retraction_notice).count());
    out["pub_years"] = json!(count_by(&|r| vec![r.dates.first().map(|d| d[..4.min(d.len())].to_string()).unwrap_or_else(|| "?".into())]));
    out
}
