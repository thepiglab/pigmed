//! Concurrent client for an OpenAI-compatible chat server (vLLM): one request per chunk, four query types back as
//! JSON (schema-constrained), with per-request latency and token usage for throughput accounting.

use crate::sample::{Article, SampleChunk};
use std::collections::HashMap;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const QUERY_TYPES: [&str; 4] = ["keyword", "clinician", "patient", "contextual"];

pub const SYSTEM: &str = "You write search queries that people would type to find one specific passage of a \
biomedical article. Answer with JSON only.";

pub fn prompt(c: &SampleChunk) -> String {
    format!(
        "Article title: {title}\nAbstract (context only): {abs}\nSection: {sec}\n\nPassage:\n<<<\n{text}\n>>>\n\n\
Write four different search queries that this passage answers:\n\
- keyword: 3 to 8 keywords a researcher would type into a literature search engine.\n\
- clinician: a specific question a clinician or researcher would ask, answered by this passage.\n\
- patient: a question a patient or caregiver would ask in plain, non-technical words, answered by this passage.\n\
- contextual: a specific question answered by this passage that names the article's main subject (for example the \
drug, disease, population or method from the title or abstract) even though the passage itself does not name it.\n\
Every query must be answerable from the passage, must not copy a sentence from it, and must be under 30 words.",
        title = c.title, abs = c.abstract_text, sec = if c.section.is_empty() { "(none)" } else { &c.section }, text = c.text
    )
}

const RULES: &str = "Write four different search queries that the passage answers:\n\
- keyword: 3 to 8 keywords a researcher would type into a literature search engine.\n\
- clinician: a specific question a clinician or researcher would ask, answered by the passage.\n\
- patient: a question a patient or caregiver would ask in plain, non-technical words, answered by the passage.\n\
- contextual: a specific question answered by the passage that names the article's main subject (for example the \
drug, disease, population or method) even though the passage itself does not name it.\n\
Every query must be answerable from the passage, must not copy a sentence from it, and must be under 30 words.";

/// Layout B: the whole article (passages marked) is the system prompt, shared by all its chunks' requests, so the
/// server's prefix cache computes it once.
pub fn system_with_article(a: &Article) -> String {
    format!("{SYSTEM}\n\nThe article (target passages are marked [[P1]] ... [[/P1]] and so on):\n\n{}", a.text)
}

pub fn user_for_marker(marker: &str) -> String {
    format!("The passage is [[{marker}]] in the article above. {RULES}")
}

/// Layout C: one call per article, queries for every marked passage.
pub fn prompt_article(a: &Article) -> String {
    format!("Article (target passages are marked [[P1]] ... [[/P1]] and so on):\n\n{}\n\nFor EACH marked passage ({}), \
{} Return one JSON object with one key per passage label.", a.text, a.passages.join(", "), RULES.replacen("the passage", "that passage", 1))
}

pub fn schema_article(passages: &[String]) -> Value {
    let props: serde_json::Map<String, Value> = passages.iter().map(|p| (p.clone(), schema())).collect();
    json!({"type": "object", "properties": props, "required": passages, "additionalProperties": false})
}

pub fn schema() -> Value {
    let props: serde_json::Map<String, Value> = QUERY_TYPES.iter().map(|t| (t.to_string(), json!({"type": "string"}))).collect();
    json!({"type": "object", "properties": props, "required": QUERY_TYPES, "additionalProperties": false})
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Generation {
    pub chunk_id: String,
    pub article_id: String,
    pub arm: String,
    pub ok: bool,
    pub error: Option<String>,
    pub queries: serde_json::Map<String, Value>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_s: f64,
    /// thinking tokens (usage.completion_tokens_details.reasoning_tokens) when the server reports them
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// length of message.reasoning_content (or .reasoning) in characters
    #[serde(default)]
    pub reasoning_chars: u64,
    /// recipe v2: per-slot specs, answers, flags and the article's personas (see recipe.rs)
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

/// One parsed chat completion.
pub struct Reply {
    pub content: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub reasoning_chars: u64,
}

impl Generation {
    fn take(&mut self, r: &Reply, k: u64) {
        self.prompt_tokens = r.prompt_tokens / k;
        self.completion_tokens = r.completion_tokens / k;
        self.reasoning_tokens = r.reasoning_tokens / k;
        self.reasoning_chars = r.reasoning_chars / k;
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Layout {
    /// one call per chunk, title + abstract + passage
    Abstract,
    /// one call per chunk, full article as a shared (prefix-cached) system prompt
    Shared,
    /// one call per article for all its passages
    PerArticle,
    /// one call per figure: image + caption + citing text + title/abstract
    Figure,
    /// recipe v2 text: one call per article, scheduled slot specs + personas + answers
    TextV2,
    /// recipe v2 blind figure call: image + title + abstract, caption and citing text withheld
    FigBlind,
    /// recipe v2 grounded figure call: image + caption + citing text, plus one composed (image + question) query
    FigGrounded,
    /// recipe v3: intent cards, then persona-matched surface forms (intent-first)
    TextV3If,
    /// recipe v3, persona-first: cards are written for a given persona
    TextV3Pf,
    /// recipe v3 without personas (style schedule only)
    TextV3Np,
}

impl std::str::FromStr for Layout {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s { "a" | "abstract" => Layout::Abstract, "b" | "shared" => Layout::Shared,
                     "c" | "article" => Layout::PerArticle, "fig" | "figure" => Layout::Figure,
                     "c2" | "text2" => Layout::TextV2, "figb" | "blind" => Layout::FigBlind,
                     "figg" | "grounded" => Layout::FigGrounded,
                     "c3if" => Layout::TextV3If, "c3pf" => Layout::TextV3Pf, "c3np" => Layout::TextV3Np,
                     _ => anyhow::bail!("layout a|b|c|fig|c2|figb|figg|c3if|c3pf|c3np") })
    }
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
    pub layout: Layout,
    pub gpus: usize,
}

/// Parse the model's content: strip any reasoning block or code fence, take the outermost JSON object.
pub fn parse_queries(content: &str) -> Option<serde_json::Map<String, Value>> {
    parse_typed(content, &QUERY_TYPES)
}

pub fn parse_typed(content: &str, types: &[&str]) -> Option<serde_json::Map<String, Value>> {
    let c = content.rsplit("</think>").next().unwrap_or(content);
    let (s, e) = (c.find('{')?, c.rfind('}')?);
    let v: Value = serde_json::from_str(&c[s..=e]).ok()?;
    let m = v.as_object()?.clone();
    let vals: Vec<String> = types.iter().filter_map(|t| m.get(*t).and_then(|x| x.as_str()).map(|x| x.trim().to_lowercase())).collect();
    if vals.len() != types.len() || vals.iter().any(|v| v.is_empty()) {
        return None;
    }
    // degenerate outputs seen from quantised models under JSON-constrained decoding: a field's value is its own
    // key ("keyword": "keyword") or every field holds the same text
    let degenerate = types.iter().zip(&vals).any(|(t, v)| {
        let k = t.replace('_', " ");
        *v == k || *v == format!("{k}s") || *v == *t
    }) || (vals.len() > 1 && vals.iter().all(|v| *v == vals[0]));
    (!degenerate).then_some(m)
}

pub fn schema_typed(types: &[&str]) -> Value {
    let props: serde_json::Map<String, Value> = types.iter().map(|t| (t.to_string(), json!({"type": "string"}))).collect();
    json!({"type": "object", "properties": props, "required": types, "additionalProperties": false})
}

pub const FIG_SYSTEM: &str = "You write search queries that people would type to find one specific figure (image) from a \
biomedical article. Answer with JSON only.";

pub fn figure_prompt(f: &crate::figure::FigureSample) -> String {
    format!("Article title: {}\nAbstract (context only): {}\nFigure caption: {}\nText that cites the figure: {}\n\n\
The image above is the figure. Write four different search queries for which this figure is the answer:\n\
- visual: a \"find the figure showing ...\" request describing what the figure visibly shows.\n\
- clinician: a specific question a clinician or researcher would ask that this figure answers.\n\
- image_only: a query that can only be answered by looking at the image itself (a visible trend, structure, value, \
location or appearance), not from the caption alone.\n\
- keyword: 3 to 8 keywords a researcher would type into a literature search engine.\n\
Do not copy the caption. Every query must be under 30 words.",
        f.title, f.abstract_text, f.text, f.citing.join(" ... "))
}

pub(crate) fn image_data_url(path: &str) -> Result<String> {
    use base64::Engine;
    Ok(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(std::fs::read(path)?)))
}

/// Hosted OpenAI-compatible APIs: the key is read once from the file named by PIGMED_API_KEY_FILE
/// (never from the command line or the environment value itself), and a client-side spend cap applies:
/// PIGMED_PRICE_IN / PIGMED_PRICE_OUT ($ per 1M tokens) and PIGMED_CAP_USD. Once the running estimate reaches the cap,
/// no further request is sent. 429s are retried with backoff and counted.
pub struct ApiState {
    pub key: Option<String>,
    pub price_in: f64,
    pub price_out: f64,
    pub cap: f64,
    pub tok_in: std::sync::atomic::AtomicU64,
    pub tok_out: std::sync::atomic::AtomicU64,
    pub rate_limited: AtomicUsize,
    pub refused_cap: AtomicUsize,
}

pub fn api() -> &'static ApiState {
    static S: std::sync::OnceLock<ApiState> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let f = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok());
        let key = std::env::var("PIGMED_API_KEY_FILE").ok().and_then(|p| std::fs::read_to_string(p).ok()).map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
        ApiState { key, price_in: f("PIGMED_PRICE_IN").unwrap_or(0.0), price_out: f("PIGMED_PRICE_OUT").unwrap_or(0.0),
            cap: f("PIGMED_CAP_USD").unwrap_or(f64::INFINITY), tok_in: 0.into(), tok_out: 0.into(), rate_limited: 0.into(), refused_cap: 0.into() }
    })
}

impl ApiState {
    pub fn spent(&self) -> f64 {
        (self.tok_in.load(Ordering::Relaxed) as f64 * self.price_in + self.tok_out.load(Ordering::Relaxed) as f64 * self.price_out) / 1e6
    }
    pub fn report(&self) -> Value {
        json!({"hosted": self.key.is_some(), "prompt_tokens": self.tok_in.load(Ordering::Relaxed), "completion_tokens": self.tok_out.load(Ordering::Relaxed),
            "est_usd": self.spent(), "cap_usd": if self.cap.is_finite() { json!(self.cap) } else { Value::Null },
            "rate_limited_429": self.rate_limited.load(Ordering::Relaxed), "refused_at_cap": self.refused_cap.load(Ordering::Relaxed)})
    }
}

pub(crate) fn post(agent: &ureq::Agent, a: &GenArgs, system: &str, user: Value, schema: Value) -> (Result<Reply, String>, f64) {
    let st = api();
    if st.spent() >= st.cap {
        st.refused_cap.fetch_add(1, Ordering::Relaxed);
        return (Err("spend cap reached".into()), 0.0);
    }
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
    let mut attempt = 0u64;
    while attempt < 3 {
        let mut req = agent.post(&format!("{}/chat/completions", a.endpoint));
        if let Some(k) = &st.key {
            req = req.set("Authorization", &format!("Bearer {k}"));
        }
        match req.send_json(&body) {
            Err(ureq::Error::Status(429, r)) => {
                st.rate_limited.fetch_add(1, Ordering::Relaxed);
                let wait = r.header("retry-after").and_then(|x| x.parse::<u64>().ok()).unwrap_or(5 * (attempt + 1)).min(60);
                last_err = "429 rate limited".into();
                std::thread::sleep(Duration::from_secs(wait));
                // 429s do not use up the attempt budget, but give up after ~10 minutes of them
                if t.elapsed().as_secs() > 600 {
                    break;
                }
                continue;
            }
            Ok(resp) => match resp.into_json::<Value>() {
                Ok(v) => {
                    let m = &v["choices"][0]["message"];
                    let reasoning = m["reasoning_content"].as_str().or_else(|| m["reasoning"].as_str()).unwrap_or("");
                    let u = &v["usage"];
                    st.tok_in.fetch_add(u["prompt_tokens"].as_u64().unwrap_or(0), Ordering::Relaxed);
                    st.tok_out.fetch_add(u["completion_tokens"].as_u64().unwrap_or(0), Ordering::Relaxed);
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
        attempt += 1;
    }
    (Err(last_err), t.elapsed().as_secs_f64())
}

fn new_gen(c: &SampleChunk, a: &GenArgs) -> Generation {
    Generation { chunk_id: c.chunk_id.clone(), article_id: c.article_id.clone(), arm: a.arm.clone(), ..Default::default() }
}

fn one_chunk(agent: &ureq::Agent, a: &GenArgs, c: &SampleChunk, article: Option<&Article>) -> Generation {
    let mut g = new_gen(c, a);
    let (res, lat) = match (a.layout, article) {
        (Layout::Shared, Some(art)) => post(agent, a, &system_with_article(art), json!(user_for_marker(&c.marker)), schema()),
        _ => post(agent, a, SYSTEM, json!(prompt(c)), schema()),
    };
    g.latency_s = lat;
    match res {
        Ok(r) => {
            g.take(&r, 1);
            match parse_queries(&r.content) {
                Some(q) => { g.ok = true; g.queries = q; }
                None => g.error = Some(format!("unparsed: {}", r.content.chars().take(200).collect::<String>())),
            }
        }
        Err(e) => g.error = Some(e),
    }
    g
}

/// Split a per-article response into per-chunk generations (tokens shared equally).
pub fn split_article(content: &str, chunks: &[&SampleChunk]) -> Vec<Option<serde_json::Map<String, Value>>> {
    let c = content.rsplit("</think>").next().unwrap_or(content);
    let v: Option<Value> = c.find('{').and_then(|s| c.rfind('}').and_then(|e| serde_json::from_str(&c[s..=e]).ok()));
    chunks.iter().map(|ch| {
        let obj = v.as_ref()?.get(&ch.marker)?;
        parse_queries(&obj.to_string())
    }).collect()
}

fn one_article(agent: &ureq::Agent, a: &GenArgs, art: &Article, chunks: &[&SampleChunk]) -> Vec<Generation> {
    let (res, lat) = post(agent, a, SYSTEM, json!(prompt_article(art)), schema_article(&art.passages));
    let k = chunks.len().max(1) as u64;
    let mut out: Vec<Generation> = chunks.iter().map(|c| { let mut g = new_gen(c, a); g.latency_s = lat; g }).collect();
    match res {
        Ok(r) => {
            let parsed = split_article(&r.content, chunks);
            for (g, q) in out.iter_mut().zip(parsed) {
                g.take(&r, k);
                match q {
                    Some(q) => { g.ok = true; g.queries = q; }
                    None => g.error = Some(format!("unparsed: {}", r.content.chars().take(200).collect::<String>())),
                }
            }
        }
        Err(e) => out.iter_mut().for_each(|g| g.error = Some(e.clone())),
    }
    out
}

/// Sum of every server metric whose name contains one of the patterns (vLLM /metrics, Prometheus text).
pub fn scrape(agent: &ureq::Agent, endpoint: &str) -> HashMap<String, f64> {
    let base = endpoint.trim_end_matches('/').trim_end_matches("/v1");
    let mut m = HashMap::new();
    let Ok(resp) = agent.get(&format!("{base}/metrics")).call() else { return m };
    let Ok(text) = resp.into_string() else { return m };
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (name, val) = match line.rsplit_once(' ') { Some(x) => x, None => continue };
        let name = name.split('{').next().unwrap_or(name);
        if ["prefix_cache", "prompt_tokens", "generation_tokens", "spec_decode", "num_preemptions"].iter().any(|p| name.contains(p)) {
            if let Ok(v) = val.parse::<f64>() {
                *m.entry(name.to_string()).or_default() += v;
            }
        }
    }
    m
}

pub fn run(a: &GenArgs, chunks: &[SampleChunk], articles: &[Article]) -> Result<(Vec<Generation>, Value)> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(1800)).max_idle_connections(a.concurrency * 2).build();
    let by_article: HashMap<&str, &Article> = articles.iter().map(|x| (x.article_id.as_str(), x)).collect();
    // work units: single chunks (layout A) or whole articles (B: its chunks in sequence; C: one call)
    let mut groups: Vec<Vec<&SampleChunk>> = vec![];
    if a.layout == Layout::Abstract {
        groups = chunks.iter().map(|c| vec![c]).collect();
    } else {
        let mut idx: HashMap<&str, usize> = HashMap::new();
        for c in chunks {
            let i = *idx.entry(c.article_id.as_str()).or_insert_with(|| { groups.push(vec![]); groups.len() - 1 });
            groups[i].push(c);
        }
    }
    let before = scrape(&agent, &a.endpoint);
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Generation>> = Mutex::new(Vec::with_capacity(chunks.len()));
    let calls = AtomicUsize::new(0);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..a.concurrency {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= groups.len() {
                    break;
                }
                let g = &groups[i];
                let art = by_article.get(g[0].article_id.as_str()).copied();
                let gens: Vec<Generation> = match (a.layout, art) {
                    (Layout::PerArticle, Some(art)) => { calls.fetch_add(1, Ordering::Relaxed); one_article(&agent, a, art, g) }
                    _ => g.iter().map(|c| { calls.fetch_add(1, Ordering::Relaxed); one_chunk(&agent, a, c, art) }).collect(),
                };
                let mut o = out.lock().unwrap();
                let before = o.len();
                o.extend(gens);
                if o.len() / 500 != before / 500 {
                    eprintln!("{}/{} {:.0}s", o.len(), chunks.len(), t0.elapsed().as_secs_f64());
                }
            });
        }
    });
    let wall = t0.elapsed().as_secs_f64();
    let after = scrape(&agent, &a.endpoint);
    let server: serde_json::Map<String, Value> = after.iter().map(|(k, v)| (k.clone(), json!(v - before.get(k).copied().unwrap_or(0.0)))).collect();
    let gens = out.into_inner().unwrap();
    let ok = gens.iter().filter(|g| g.ok).count();
    let (pt, ct): (u64, u64) = gens.iter().fold((0, 0), |(p, c), g| (p + g.prompt_tokens, c + g.completion_tokens));
    let mut lat: Vec<f64> = gens.iter().map(|g| g.latency_s).collect();
    lat.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let q = |p: f64| lat.get(((lat.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0.0);
    let n_calls = calls.load(Ordering::Relaxed);
    let qph = (ok * QUERY_TYPES.len()) as f64 / wall * 3600.0;
    let summary = json!({
        "arm": a.arm, "model": a.model, "layout": format!("{:?}", a.layout), "gpus": a.gpus,
        "chunks": gens.len(), "calls": n_calls, "ok_chunks": ok, "json_valid_rate": ok as f64 / gens.len().max(1) as f64,
        "wall_s": wall, "concurrency": a.concurrency, "prompt_tokens": pt, "completion_tokens": ct,
        "calls_per_s": n_calls as f64 / wall, "input_tokens_per_s": pt as f64 / wall, "output_tokens_per_s": ct as f64 / wall,
        "queries_per_hour": qph, "queries_per_gpu_hour": qph / a.gpus.max(1) as f64,
        "gpu_hours_per_million_queries": 1e6 / (qph / a.gpus.max(1) as f64).max(1e-9),
        "prompt_tokens_per_chunk": pt as f64 / gens.len().max(1) as f64, "completion_tokens_per_chunk": ct as f64 / gens.len().max(1) as f64,
        "reasoning_tokens_per_chunk": gens.iter().map(|g| g.reasoning_tokens).sum::<u64>() as f64 / gens.len().max(1) as f64,
        "reasoning_chars_per_chunk": gens.iter().map(|g| g.reasoning_chars).sum::<u64>() as f64 / gens.len().max(1) as f64,
        "latency_s": {"p50": q(0.5), "p90": q(0.9), "p99": q(0.99)}, "server_metrics_delta": server,
    });
    Ok((gens, summary))
}

fn one_figure(agent: &ureq::Agent, a: &GenArgs, f: &crate::figure::FigureSample) -> Generation {
    let mut g = Generation { chunk_id: f.chunk_id.clone(), article_id: f.article_id.clone(), arm: a.arm.clone(), ..Default::default() };
    let url = match image_data_url(&f.image) { Ok(u) => u, Err(e) => { g.error = Some(format!("image: {e}")); return g; } };
    let user = json!([{"type": "image_url", "image_url": {"url": url}}, {"type": "text", "text": figure_prompt(f)}]);
    let types = crate::figure::FIG_QUERY_TYPES;
    let (res, lat) = post(agent, a, FIG_SYSTEM, user, schema_typed(&types));
    g.latency_s = lat;
    match res {
        Ok(r) => {
            g.take(&r, 1);
            match parse_typed(&r.content, &types) {
                Some(q) => { g.ok = true; g.queries = q; }
                None => g.error = Some(format!("unparsed: {}", r.content.chars().take(200).collect::<String>())),
            }
        }
        Err(e) => g.error = Some(e),
    }
    g
}

/// Figure track: one call per figure, same accounting as `run`.
pub fn run_figures(a: &GenArgs, figs: &[crate::figure::FigureSample]) -> Result<(Vec<Generation>, Value)> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(1800)).max_idle_connections(a.concurrency * 2).build();
    let before = scrape(&agent, &a.endpoint);
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Generation>> = Mutex::new(Vec::with_capacity(figs.len()));
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..a.concurrency {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= figs.len() {
                    break;
                }
                let g = one_figure(&agent, a, &figs[i]);
                out.lock().unwrap().push(g);
            });
        }
    });
    let wall = t0.elapsed().as_secs_f64();
    let after = scrape(&agent, &a.endpoint);
    let server: serde_json::Map<String, Value> = after.iter().map(|(k, v)| (k.clone(), json!(v - before.get(k).copied().unwrap_or(0.0)))).collect();
    let gens = out.into_inner().unwrap();
    let ok = gens.iter().filter(|g| g.ok).count();
    let (pt, ct): (u64, u64) = gens.iter().fold((0, 0), |(p, c), g| (p + g.prompt_tokens, c + g.completion_tokens));
    let qph = (ok * crate::figure::FIG_QUERY_TYPES.len()) as f64 / wall * 3600.0;
    let summary = json!({
        "arm": a.arm, "model": a.model, "layout": "Figure", "gpus": a.gpus, "chunks": gens.len(), "calls": gens.len(),
        "ok_chunks": ok, "json_valid_rate": ok as f64 / gens.len().max(1) as f64, "wall_s": wall, "concurrency": a.concurrency,
        "prompt_tokens": pt, "completion_tokens": ct, "input_tokens_per_s": pt as f64 / wall, "output_tokens_per_s": ct as f64 / wall,
        "queries_per_hour": qph, "queries_per_gpu_hour": qph / a.gpus.max(1) as f64,
        "gpu_hours_per_million_queries": 1e6 / (qph / a.gpus.max(1) as f64).max(1e-9),
        "prompt_tokens_per_chunk": pt as f64 / gens.len().max(1) as f64, "completion_tokens_per_chunk": ct as f64 / gens.len().max(1) as f64,
        "reasoning_tokens_per_chunk": gens.iter().map(|g| g.reasoning_tokens).sum::<u64>() as f64 / gens.len().max(1) as f64,
        "reasoning_chars_per_chunk": gens.iter().map(|g| g.reasoning_chars).sum::<u64>() as f64 / gens.len().max(1) as f64,
        "server_metrics_delta": server,
    });
    Ok((gens, summary))
}

/// No-LLM reference queries copied from the chunk.
pub fn copy_baseline(c: &SampleChunk) -> serde_json::Map<String, Value> {
    let words: Vec<String> = c.text.split_whitespace().map(|w| w.trim_matches(|x: char| !x.is_alphanumeric()).to_lowercase())
        .filter(|w| w.len() > 4).collect();
    let mut freq: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for w in &words {
        *freq.entry(w.as_str()).or_default() += 1;
    }
    let mut top: Vec<(&str, usize)> = freq.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let keyword = top.iter().take(6).map(|(w, _)| *w).collect::<Vec<_>>().join(" ");
    let first: String = c.text.split(". ").next().unwrap_or("").split_whitespace().take(25).collect::<Vec<_>>().join(" ");
    let mut m = serde_json::Map::new();
    m.insert("keyword".into(), json!(keyword));
    m.insert("clinician".into(), json!(first));
    m.insert("patient".into(), json!(first));
    m.insert("contextual".into(), json!(c.title));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_with_reasoning_and_fences() {
        let s = r#"<think>hmm</think>```json
{"keyword": "a b", "clinician": "c?", "patient": "d?", "contextual": "e?"}
```"#;
        assert!(parse_queries(s).is_some());
        assert!(parse_queries(r#"{"keyword": "a", "clinician": "", "patient": "d", "contextual": "e"}"#).is_none());
        assert!(parse_queries("no json").is_none());
        assert!(parse_queries(r#"{"keyword": "keyword", "clinician": "clinician", "patient": "patient", "contextual": "contextual"}"#).is_none());
        assert!(parse_queries(r#"{"keyword": "same", "clinician": "same", "patient": "same", "contextual": "same"}"#).is_none());
        assert!(parse_typed(r#"{"visual": "a", "clinician": "b", "image_only": "image only", "keyword": "c"}"#, &crate::figure::FIG_QUERY_TYPES).is_none());
        assert_eq!(schema()["required"].as_array().unwrap().len(), 4);
        let mk = |m: &str| SampleChunk { chunk_id: m.into(), article_id: "A".into(), cluster: String::new(), title: String::new(),
            abstract_text: String::new(), section: String::new(), text: String::new(), words: 0, marker: m.into() };
        let (p1, p2) = (mk("P1"), mk("P2"));
        let q = r#"{"keyword": "a", "clinician": "b?", "patient": "c?", "contextual": "d?"}"#;
        let parsed = split_article(&format!(r#"{{"P1": {q}, "P2": {{"keyword": ""}}}}"#), &[&p1, &p2]);
        assert!(parsed[0].is_some() && parsed[1].is_none());
        assert!("b".parse::<Layout>().unwrap() == Layout::Shared);
    }
}
