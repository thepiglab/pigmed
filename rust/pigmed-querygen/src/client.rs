//! OpenAI-compatible chat client (vLLM / SGLang), the Generation row format and the run summary. Request bodies and
//! rows match pigmed-synth, so its queries / filter / metrics / judge3 / export subcommands read our output unchanged.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Generation {
    pub chunk_id: String,
    pub article_id: String,
    pub arm: String,
    pub ok: bool,
    pub error: Option<String>,
    pub queries: Map<String, Value>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_s: f64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub reasoning_chars: u64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

#[derive(Clone)]
pub struct GenArgs {
    pub endpoint: String,
    pub model: String,
    pub arm: String,
    pub concurrency: usize,
    pub max_tokens: u32,
    pub temperature: f64,
    pub top_p: f64,
    pub extra: Value,
    pub gpus: usize,
}

pub struct Reply {
    pub content: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub reasoning_chars: u64,
}

pub fn agent(conc: usize) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(3600)).max_idle_connections(conc * 2).build()
}

/// One schema-constrained chat call; 3 attempts on errors.
pub fn post(agent: &ureq::Agent, a: &GenArgs, system: &str, user: Value, schema: Value) -> (Result<Reply, String>, f64) {
    let mut body = json!({
        "model": a.model, "max_tokens": a.max_tokens, "temperature": a.temperature, "top_p": a.top_p,
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
        "response_format": {"type": "json_schema", "json_schema": {"name": "queries", "schema": schema, "strict": true}},
    });
    if let (Some(b), Some(x)) = (body.as_object_mut(), a.extra.as_object()) {
        for (k, v) in x {
            b.insert(k.clone(), v.clone());
        }
    }
    let t = Instant::now();
    let mut last_err = String::new();
    for attempt in 0..3u64 {
        match agent.post(&format!("{}/chat/completions", a.endpoint)).send_json(&body) {
            Ok(resp) => match resp.into_json::<Value>() {
                Ok(v) => {
                    let m = &v["choices"][0]["message"];
                    let reasoning = m["reasoning_content"].as_str().or_else(|| m["reasoning"].as_str()).unwrap_or("");
                    let u = &v["usage"];
                    return (Ok(Reply {
                        content: m["content"].as_str().unwrap_or("").to_string(),
                        prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                        completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                        reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
                        reasoning_chars: reasoning.chars().count() as u64,
                    }), t.elapsed().as_secs_f64());
                }
                Err(e) => last_err = e.to_string(),
            },
            Err(e) => {
                last_err = e.to_string();
                std::thread::sleep(Duration::from_secs(2 * (attempt + 1)));
            }
        }
    }
    (Err(last_err), t.elapsed().as_secs_f64())
}

/// The model's JSON: strip any reasoning block, take the outermost object.
pub fn json_body(content: &str) -> Option<Value> {
    let c = content.rsplit("</think>").next().unwrap_or(content);
    let (s, e) = (c.find('{')?, c.rfind('}')?);
    serde_json::from_str(&c[s..=e]).ok()
}

/// Run `f` over `n` work items with `conc` threads; results in completion order. `on_done` sees every finished
/// item (for append-only progress files).
pub fn par<R: Send>(n: usize, conc: usize, f: impl Fn(usize) -> R + Sync, on_done: impl Fn(&R) + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<R>> = Mutex::new(vec![]);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..conc.max(1) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let r = f(i);
                on_done(&r);
                let mut o = out.lock().unwrap();
                o.push(r);
                if o.len() % 50 == 0 {
                    eprintln!("{}/{n} {:.0}s", o.len(), t0.elapsed().as_secs_f64());
                }
            });
        }
    });
    out.into_inner().unwrap()
}

/// Sum of server metrics (vLLM / SGLang Prometheus text) whose names contain one of the patterns.
pub fn scrape(agent: &ureq::Agent, endpoint: &str) -> HashMap<String, f64> {
    let base = endpoint.trim_end_matches('/').trim_end_matches("/v1");
    let mut m = HashMap::new();
    let Ok(resp) = agent.get(&format!("{base}/metrics")).call() else { return m };
    let Ok(text) = resp.into_string() else { return m };
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let Some((name, val)) = line.rsplit_once(' ') else { continue };
        let name = name.split('{').next().unwrap_or(name);
        if ["prefix_cache", "prompt_tokens", "generation_tokens", "spec_decode", "num_preemptions", "cache_hit"].iter().any(|p| name.contains(p)) {
            if let Ok(v) = val.parse::<f64>() {
                *m.entry(name.to_string()).or_default() += v;
            }
        }
    }
    m
}

pub fn delta(before: &HashMap<String, f64>, after: &HashMap<String, f64>) -> Map<String, Value> {
    after.iter().map(|(k, v)| (k.clone(), json!(v - before.get(k).copied().unwrap_or(0.0)))).collect()
}

/// Same fields as pigmed-synth's summary, plus kept/GPU-hour.
pub fn summary(a: &GenArgs, layout: &str, gens: &[Generation], calls: usize, wall: f64, server: Map<String, Value>) -> Value {
    let ok = gens.iter().filter(|g| g.ok).count();
    let nq: usize = gens.iter().map(|g| g.queries.len()).sum();
    let slots: usize = gens.iter().map(|g| g.meta["slots"].as_object().map(|m| m.len()).unwrap_or(0)).sum();
    let banned = gens.iter().flat_map(|g| g.meta["slots"].as_object().into_iter().flatten()).filter(|(_, m)| m["banned"] == json!(true)).count();
    let (pt, ct): (u64, u64) = gens.iter().fold((0, 0), |(p, c), g| (p + g.prompt_tokens, c + g.completion_tokens));
    let n = gens.len().max(1) as f64;
    let wall = wall.max(1e-9);
    let qph = nq as f64 / wall * 3600.0;
    let sph = slots as f64 / wall * 3600.0;
    json!({
        "arm": a.arm, "model": a.model, "layout": layout, "gpus": a.gpus, "chunks": gens.len(), "calls": calls,
        "ok_chunks": ok, "json_valid_rate": ok as f64 / n, "slots": slots, "queries": nq,
        "accepted_rate": nq as f64 / slots.max(1) as f64, "banned_rate": banned as f64 / slots.max(1) as f64,
        "wall_s": wall, "concurrency": a.concurrency, "prompt_tokens": pt, "completion_tokens": ct,
        "input_tokens_per_s": pt as f64 / wall, "output_tokens_per_s": ct as f64 / wall,
        "queries_per_hour": qph, "queries_per_gpu_hour": qph / a.gpus.max(1) as f64,
        "generated_per_gpu_hour": sph / a.gpus.max(1) as f64,
        "prompt_tokens_per_chunk": pt as f64 / n, "completion_tokens_per_chunk": ct as f64 / n,
        "reasoning_tokens_per_chunk": gens.iter().map(|g| g.reasoning_tokens).sum::<u64>() as f64 / n,
        "reasoning_chars_per_chunk": gens.iter().map(|g| g.reasoning_chars).sum::<u64>() as f64 / n,
        "server_metrics_delta": server,
    })
}
