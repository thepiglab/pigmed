//! Evaluation-only judge with Claude Sonnet 5.5 through the Message Batches API (raw HTTP: there is no official
//! Rust SDK). Used to grade *samples* of kept queries against their source passage (and, for recipe v3, their
//! intent card). Never used to make training labels.
//!
//! Spend control: a ledger file holds every batch's actual cost (from the returned usage, batch prices). Before a
//! batch is submitted its cost is estimated with a margin; if ledger + estimate exceeds --cap-usd nothing is sent.
//! The API key is read from a file and never printed.

use crate::generate::Generation;
use crate::io::read_jsonl;
use crate::pilot3::keep_v3;
use crate::filter::{QueryMeta, Ranks};
use crate::sample::{hash, SampleChunk};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

pub const MODEL: &str = "claude-sonnet-5-5";
const API: &str = "https://api.anthropic.com/v1/messages/batches";
const VERSION: &str = "2026-10-21";
/// Sonnet 5.5 list price $2 / $10 per MTok; batches are billed at 50%.
const IN_PER_TOK: f64 = 1.0e-6;
const OUT_PER_TOK: f64 = 5.0e-6;
const PER_REQUEST: usize = 5;

const SYSTEM: &str = "You are a careful evaluator of search queries for a biomedical literature search engine. You \
judge each query against the passage it was written for and, when given, the information need it was meant to \
express. Be strict and consistent. Output JSON only.";

const RULES: &str = "For each query give:\n\
- standalone: true if a stranger understands what is searched for without seeing this article (false for \"this \
study\", \"the authors\", or labels only this paper uses such as \"group B\" or \"the crude ES group\").\n\
- specificity: how many published papers would answer it well: unique (one or two), few (a handful), many (hundreds).\n\
- natural: 1 = no real person would type this; 2 = plausible but stilted or exam-like; 3 = a real person could well type this.\n\
- answered: does the passage answer the query? yes, partial, no.\n\
- need_match: if a NEED is given for the query, does the query express that need (yes/no)? If no need is given, na.\n\
- register: expert, student, lay.";

pub struct Item {
    pub qid: String,
    pub arm: String,
    pub chunk_id: String,
    pub query: String,
    pub need: String,
    pub style: String,
}

/// Up to `per_arm` kept queries per arm (v3 style-aware gate), drawn by stable hash.
pub fn select(metas: &[QueryMeta], ranks: &[Ranks], gens: &[Generation], per_arm: usize) -> Vec<Item> {
    let rank: HashMap<&str, &Ranks> = ranks.iter().map(|r| (r.qid.as_str(), r)).collect();
    let mut slot: HashMap<String, Value> = HashMap::new();
    for g in gens {
        for (k, m) in g.meta["slots"].as_object().into_iter().flatten() {
            slot.insert(format!("{}-{}-{}", g.arm, k, g.chunk_id), m.clone());
        }
    }
    let mut by_arm: BTreeMap<&str, Vec<Item>> = BTreeMap::new();
    for m in metas {
        let s = slot.get(&m.qid);
        let style = s.and_then(|s| s["style"].as_str()).unwrap_or("v2").to_string();
        let Some(r) = rank.get(m.qid.as_str()) else { continue };
        if r.chunk_rank == 0 || !keep_v3(r, &style) {
            continue;
        }
        by_arm.entry(m.arm.as_str()).or_default().push(Item { qid: m.qid.clone(), arm: m.arm.clone(), chunk_id: m.chunk_id.clone(),
            query: m.text.clone(), need: s.and_then(|s| s["need"].as_str()).unwrap_or("").to_string(), style });
    }
    let mut out = vec![];
    for (_, mut v) in by_arm {
        v.sort_by_key(|x| hash(&["claude-judge", &x.qid]));
        v.truncate(per_arm);
        out.extend(v);
    }
    out
}

fn schema(n: usize) -> Value {
    let ids: Vec<String> = (1..=n).map(|k| format!("q{k}")).collect();
    json!({"type": "object", "additionalProperties": false, "required": ["ratings"], "properties": {"ratings": {
        "type": "array", "items": {"type": "object", "additionalProperties": false,
        "required": ["id", "standalone", "specificity", "natural", "answered", "need_match", "register"],
        "properties": {"id": {"type": "string", "enum": ids}, "standalone": {"type": "boolean"},
            "specificity": {"type": "string", "enum": ["unique", "few", "many"]}, "natural": {"type": "integer", "enum": [1, 2, 3]},
            "answered": {"type": "string", "enum": ["yes", "partial", "no"]}, "need_match": {"type": "string", "enum": ["yes", "no", "na"]},
            "register": {"type": "string", "enum": ["expert", "student", "lay"]}}}}}})
}

/// Batch requests: one per (arm, passage) group of up to PER_REQUEST queries. Returns (requests, custom_id -> qids).
pub fn requests(items: &[Item], chunks: &HashMap<String, SampleChunk>) -> (Vec<Value>, HashMap<String, Vec<String>>) {
    let mut groups: BTreeMap<(String, String), Vec<&Item>> = BTreeMap::new();
    for it in items {
        groups.entry((it.arm.clone(), it.chunk_id.clone())).or_default().push(it);
    }
    let (mut reqs, mut map) = (vec![], HashMap::new());
    for ((arm, chunk), its) in groups {
        let Some(c) = chunks.get(&chunk) else { continue };
        for (gi, part) in its.chunks(PER_REQUEST).enumerate() {
            let cid = format!("r{}", &hash(&[&arm, &chunk, &gi.to_string()])[..20]);
            let list: Vec<String> = part.iter().enumerate().map(|(k, it)| {
                if it.need.is_empty() { format!("q{}: {}", k + 1, it.query) } else { format!("q{}: {}\n    NEED: {}", k + 1, it.query, it.need) }
            }).collect();
            let user = format!("ARTICLE TITLE: {}\nPASSAGE:\n{}\n\nQUERIES:\n{}\n\n{RULES}", c.title, c.text, list.join("\n"));
            reqs.push(json!({"custom_id": cid, "params": {"model": MODEL, "max_tokens": 1024, "system": SYSTEM,
                "thinking": {"type": "between_tools"},
                "output_config": {"effort": "low", "format": {"type": "json_schema", "schema": schema(part.len())}},
                "messages": [{"role": "user", "content": user}]}}));
            map.insert(cid, part.iter().map(|it| it.qid.clone()).collect());
        }
    }
    (reqs, map)
}

/// Cost estimate with margin: input ~ chars/3.2 tokens + schema/system overhead, output ~ 70 tokens per query + 40.
pub fn estimate(reqs: &[Value], map: &HashMap<String, Vec<String>>) -> f64 {
    reqs.iter().map(|r| {
        let chars = r["params"]["messages"][0]["content"].as_str().map(|s| s.len()).unwrap_or(0);
        let n = map.get(r["custom_id"].as_str().unwrap_or("")).map(|v| v.len()).unwrap_or(PER_REQUEST);
        (chars as f64 / 3.2 + 700.0) * IN_PER_TOK + (70.0 * n as f64 + 40.0) * OUT_PER_TOK
    }).sum::<f64>() * 1.3
}

pub fn ledger_spent(ledger: &Path) -> Result<f64> {
    if !ledger.exists() {
        return Ok(0.0);
    }
    let v: Value = serde_json::from_str(&std::fs::read_to_string(ledger)?)?;
    Ok(v["batches"].as_array().into_iter().flatten().map(|b| b["cost_usd"].as_f64().or(b["estimate_usd"].as_f64()).unwrap_or(0.0)).sum())
}

fn ledger_write(ledger: &Path, f: impl FnOnce(&mut Value)) -> Result<()> {
    let mut v: Value = if ledger.exists() { serde_json::from_str(&std::fs::read_to_string(ledger)?)? } else { json!({"model": MODEL, "batches": []}) };
    f(&mut v);
    let tmp = ledger.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&v)?)?;
    std::fs::rename(tmp, ledger)?;
    Ok(())
}

fn key(path: &Path) -> Result<String> {
    let k = std::fs::read_to_string(path).context("read API key file")?.trim().to_string();
    if k.is_empty() {
        bail!("empty API key file");
    }
    Ok(k)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(300)).build()
}

fn call(req: ureq::Request, k: &str) -> ureq::Request {
    req.set("x-api-key", k).set("anthropic-version", VERSION).set("content-type", "application/json")
}

/// Submit (ledger-capped), poll until ended, fetch results. Returns (rows per qid, actual cost).
pub fn run(reqs: Vec<Value>, map: &HashMap<String, Vec<String>>, key_path: &Path, ledger: &Path, cap: f64, label: &str,
           poll_s: u64, max_wait_s: u64) -> Result<(Vec<Value>, f64)> {
    let est = estimate(&reqs, map);
    let spent = ledger_spent(ledger)?;
    eprintln!("claude judge: {} requests, estimate ${est:.3}, ledger ${spent:.3}, cap ${cap:.2}", reqs.len());
    if spent + est > cap {
        bail!("refusing: ledger ${spent:.3} + estimate ${est:.3} > cap ${cap:.2}");
    }
    let k = key(key_path)?;
    let ag = agent();
    let resp: Value = call(ag.post(API), &k).send_json(json!({"requests": reqs})).map_err(|e| match e {
        ureq::Error::Status(c, r) => anyhow::anyhow!("batch create {c}: {}", r.into_string().unwrap_or_default().chars().take(500).collect::<String>()),
        e => anyhow::anyhow!("batch create: {e}"),
    })?.into_json()?;
    let id = resp["id"].as_str().context("batch id")?.to_string();
    // reserve the estimate in the ledger until the actual cost is known
    ledger_write(ledger, |v| v["batches"].as_array_mut().unwrap().push(json!({"id": id, "label": label, "requests": reqs.len(),
        "estimate_usd": est, "created": resp["created_at"]})))?;
    eprintln!("batch {id} submitted");
    let t0 = std::time::Instant::now();
    let status = loop {
        std::thread::sleep(Duration::from_secs(poll_s));
        let s: Value = call(ag.get(&format!("{API}/{id}")), &k).call()?.into_json()?;
        if s["processing_status"] == json!("ended") {
            break s;
        }
        if t0.elapsed().as_secs() > max_wait_s {
            bail!("batch {id} not ended after {max_wait_s}s (left running; collect later)");
        }
    };
    let url = status["results_url"].as_str().context("results_url")?;
    let body = call(ag.get(url), &k).call()?.into_string()?;
    let (mut rows, mut tin, mut tout, mut ok, mut bad) = (vec![], 0u64, 0u64, 0usize, 0usize);
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let r: Value = serde_json::from_str(line)?;
        let cid = r["custom_id"].as_str().unwrap_or("");
        if r["result"]["type"] != json!("succeeded") {
            bad += 1;
            continue;
        }
        let m = &r["result"]["message"];
        let u = &m["usage"];
        tin += u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0) + u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        tout += u["output_tokens"].as_u64().unwrap_or(0);
        let text = m["content"].as_array().into_iter().flatten().find(|b| b["type"] == json!("text")).and_then(|b| b["text"].as_str()).unwrap_or("");
        let Ok(v) = serde_json::from_str::<Value>(text) else { bad += 1; continue };
        ok += 1;
        let qids = map.get(cid).cloned().unwrap_or_default();
        for x in v["ratings"].as_array().into_iter().flatten() {
            let idx: usize = x["id"].as_str().and_then(|s| s.trim_start_matches('q').parse().ok()).unwrap_or(0);
            if let Some(q) = idx.checked_sub(1).and_then(|i| qids.get(i)) {
                let mut x = x.clone();
                x["qid"] = json!(q);
                rows.push(x);
            }
        }
    }
    let cost = tin as f64 * IN_PER_TOK + tout as f64 * OUT_PER_TOK;
    ledger_write(ledger, |v| {
        for b in v["batches"].as_array_mut().unwrap() {
            if b["id"] == json!(id) {
                b["cost_usd"] = json!(cost);
                b["input_tokens"] = json!(tin);
                b["output_tokens"] = json!(tout);
                b["succeeded"] = json!(ok);
                b["failed"] = json!(bad);
            }
        }
    })?;
    eprintln!("batch {id}: {ok} ok, {bad} failed, {tin} in / {tout} out tokens, ${cost:.4}");
    Ok((rows, cost))
}

/// Per-arm rates from Claude ratings joined to items.
pub fn summarise(items: &[Item], rows: &[Value]) -> Value {
    let by: HashMap<&str, &Value> = rows.iter().filter_map(|r| Some((r["qid"].as_str()?, r))).collect();
    let mut arms: BTreeMap<&str, Vec<(&Item, &Value)>> = BTreeMap::new();
    for it in items {
        if let Some(r) = by.get(it.qid.as_str()) {
            arms.entry(it.arm.as_str()).or_default().push((it, r));
        }
    }
    let mut out = serde_json::Map::new();
    for (arm, xs) in arms {
        let n = xs.len().max(1) as f64;
        let share = |f: &dyn Fn(&Value) -> bool| xs.iter().filter(|(_, r)| f(r)).count() as f64 / n;
        let need: Vec<&&Value> = xs.iter().map(|(_, r)| r).filter(|r| r["need_match"] != json!("na")).collect();
        let mut reg: BTreeMap<String, usize> = BTreeMap::new();
        for (_, r) in &xs {
            *reg.entry(r["register"].as_str().unwrap_or("?").to_string()).or_default() += 1;
        }
        let mut st: BTreeMap<&str, (usize, u64, usize)> = BTreeMap::new();
        for (it, r) in &xs {
            let e = st.entry(it.style.as_str()).or_default();
            e.0 += 1;
            e.1 += r["natural"].as_u64().unwrap_or(0);
            e.2 += (r["standalone"] == json!(true)) as usize;
        }
        let by_style: serde_json::Map<String, Value> = st.into_iter()
            .map(|(k, (n, nat, sa))| (k.to_string(), json!({"n": n, "natural_mean": nat as f64 / n as f64, "standalone": sa as f64 / n as f64}))).collect();
        out.insert(arm.to_string(), json!({"judged": xs.len(), "standalone": share(&|r| r["standalone"] == json!(true)),
            "generic_many": share(&|r| r["specificity"] == json!("many")), "natural_mean": xs.iter().filter_map(|(_, r)| r["natural"].as_u64()).sum::<u64>() as f64 / n,
            "natural_3": share(&|r| r["natural"] == json!(3)), "answered_yes": share(&|r| r["answered"] == json!("yes")),
            "answered_no": share(&|r| r["answered"] == json!("no")),
            "need_match": if need.is_empty() { Value::Null } else { json!(need.iter().filter(|r| r["need_match"] == json!("yes")).count() as f64 / need.len() as f64) },
            "register": reg, "by_style": by_style}));
    }
    Value::Object(out)
}

pub fn load_chunks(sample: &Path) -> Result<HashMap<String, SampleChunk>> {
    Ok(read_jsonl::<SampleChunk>(sample)?.into_iter().map(|c| (c.chunk_id.clone(), c)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_group_and_estimate() {
        let c = SampleChunk { chunk_id: "u1".into(), article_id: "A".into(), title: "T".into(), text: "x ".repeat(300), ..Default::default() };
        let chunks: HashMap<String, SampleChunk> = [("u1".to_string(), c)].into();
        let items: Vec<Item> = (0..7).map(|i| Item { qid: format!("q{i}"), arm: "a".into(), chunk_id: "u1".into(), query: "statins".into(),
            need: if i % 2 == 0 { "need".into() } else { String::new() }, style: "keyword".into() }).collect();
        let (reqs, map) = requests(&items, &chunks);
        assert_eq!(reqs.len(), 2);
        assert_eq!(map.values().map(|v| v.len()).sum::<usize>(), 7);
        let p = &reqs[0]["params"];
        assert_eq!(p["model"], json!(MODEL));
        assert!(p.get("temperature").is_none());
        let e = estimate(&reqs, &map);
        assert!(e > 0.0 && e < 0.01, "{e}");
        let d = tempfile::tempdir().unwrap();
        let l = d.path().join("ledger.json");
        assert_eq!(ledger_spent(&l).unwrap(), 0.0);
        ledger_write(&l, |v| v["batches"].as_array_mut().unwrap().push(json!({"id": "b", "estimate_usd": 0.5}))).unwrap();
        assert!((ledger_spent(&l).unwrap() - 0.5).abs() < 1e-9);
        ledger_write(&l, |v| v["batches"][0]["cost_usd"] = json!(0.2)).unwrap();
        assert!((ledger_spent(&l).unwrap() - 0.2).abs() < 1e-9);
    }
}
