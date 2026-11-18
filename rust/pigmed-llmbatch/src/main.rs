//! Resumable batch client for an OpenAI-compatible chat server (vLLM / SGLang).
//!
//! Input: tasks JSONL, one object per line:
//!   {"id": str, "messages": [...], "max_tokens": int?, "schema": {json schema}?}
//! Output: results JSONL (appended), one object per finished task:
//!   {"id", "content", "prompt_tokens", "completion_tokens", "cached_tokens", "ms"} or {"id", "error"}.
//! Ids already present in the output (without "error") are skipped, so a rerun resumes.
//! Tasks are dispatched in input order, so tasks sharing a long prefix (one document, many chunks) stay adjacent and
//! hit the server's prefix cache. A summary JSON (wall time, token totals) is printed to stdout at the end.
use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    tasks: String,
    #[arg(long)]
    out: String,
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    model: String,
    #[arg(long, default_value_t = 256)]
    concurrency: usize,
    #[arg(long, default_value_t = 0.0)]
    temperature: f64,
    #[arg(long, default_value_t = 512)]
    max_tokens: u64,
    /// extra JSON object merged into every request body (e.g. chat_template_kwargs)
    #[arg(long)]
    extra: Option<String>,
    /// retries per task on transport errors / 5xx
    #[arg(long, default_value_t = 3)]
    retries: usize,
}

fn done_ids(path: &str) -> HashSet<String> {
    let mut s = HashSet::new();
    if let Ok(f) = File::open(path) {
        for line in BufReader::new(f).lines().map_while(|l| l.ok()) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v.get("error").is_none() {
                    if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                        s.insert(id.to_string());
                    }
                }
            }
        }
    }
    s
}

fn call(agent: &ureq::Agent, a: &Args, extra: &Value, t: &Value) -> Value {
    let id = t["id"].clone();
    let mut body = json!({
        "model": a.model, "messages": t["messages"], "temperature": a.temperature,
        "max_tokens": t.get("max_tokens").and_then(|x| x.as_u64()).unwrap_or(a.max_tokens),
    });
    if let Some(s) = t.get("schema") {
        body["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "out", "schema": s, "strict": true}});
    }
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    let url = format!("{}/chat/completions", a.endpoint);
    let mut last = String::new();
    for attempt in 0..=a.retries {
        let t0 = Instant::now();
        match agent.post(&url).send_json(&body) {
            Ok(r) => match r.into_json::<Value>() {
                Ok(v) => {
                    let u = &v["usage"];
                    return json!({
                        "id": id,
                        "content": v["choices"][0]["message"]["content"],
                        "finish_reason": v["choices"][0]["finish_reason"],
                        "prompt_tokens": u["prompt_tokens"], "completion_tokens": u["completion_tokens"],
                        "cached_tokens": u["prompt_tokens_details"]["cached_tokens"],
                        "ms": t0.elapsed().as_millis() as u64,
                    });
                }
                Err(e) => last = format!("decode: {e}"),
            },
            Err(ureq::Error::Status(code, r)) => {
                last = format!("http {code}: {}", r.into_string().unwrap_or_default().chars().take(300).collect::<String>());
                if code < 500 {
                    break; // client error: retrying will not help
                }
            }
            Err(e) => last = format!("transport: {e}"),
        }
        std::thread::sleep(Duration::from_secs(2 << attempt.min(4)));
    }
    json!({"id": id, "error": last})
}

fn main() -> Result<()> {
    let a = Args::parse();
    let extra: Value = match &a.extra {
        Some(s) => serde_json::from_str(s).context("--extra is not JSON")?,
        None => json!({}),
    };
    let done = done_ids(&a.out);
    let mut todo = Vec::new();
    for line in BufReader::new(File::open(&a.tasks).with_context(|| a.tasks.clone())?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let t: Value = serde_json::from_str(&line)?;
        if !done.contains(t["id"].as_str().unwrap_or_default()) {
            todo.push(t);
        }
    }
    eprintln!("{} tasks to do, {} already done", todo.len(), done.len());
    let out = Arc::new(Mutex::new(OpenOptions::new().create(true).append(true).open(&a.out)?));
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(1800)).max_idle_connections(a.concurrency * 2).build();
    let next = AtomicUsize::new(0);
    let tot = Mutex::new((0u64, 0u64, 0u64, 0usize, 0usize)); // prompt, completion, cached, ok, err
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..a.concurrency.max(1) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= todo.len() {
                    break;
                }
                let r = call(&agent, &a, &extra, &todo[i]);
                {
                    let mut t = tot.lock().unwrap();
                    if r.get("error").is_some() {
                        t.4 += 1;
                    } else {
                        t.0 += r["prompt_tokens"].as_u64().unwrap_or(0);
                        t.1 += r["completion_tokens"].as_u64().unwrap_or(0);
                        t.2 += r["cached_tokens"].as_u64().unwrap_or(0);
                        t.3 += 1;
                    }
                    let n = t.3 + t.4;
                    if n % 5000 == 0 {
                        eprintln!("{n}/{} {:.0}s", todo.len(), t0.elapsed().as_secs_f64());
                    }
                }
                let mut f = out.lock().unwrap();
                let _ = writeln!(f, "{r}");
            });
        }
    });
    let t = tot.into_inner().unwrap();
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "{}",
        json!({"tasks": todo.len(), "ok": t.3, "errors": t.4, "wall_s": wall, "prompt_tokens": t.0,
               "completion_tokens": t.1, "cached_tokens": t.2, "calls_per_s": t.3 as f64 / wall.max(1e-9)})
    );
    Ok(())
}
