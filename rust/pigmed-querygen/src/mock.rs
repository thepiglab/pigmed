//! Mock OpenAI-compatible server for parity tests and dry runs: answers /v1/chat/completions with JSON that fits the
//! request's json_schema, deterministically from the request (so two clients that send the same request get the same
//! reply), and records every request. Stage-B queries come from a bank of good and deliberately bad queries so the
//! reject chain is exercised. No model, no GPU, no network beyond localhost.

use crate::util::uh;
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const NEEDS: [&str; 10] = [
    "Whether low-dose colchicine reduces recurrent stroke within two years after a lacunar infarct in adults",
    "How metformin changes blood lactate levels in patients admitted with septic shock",
    "Which staining method best shows fungal hyphae in skin biopsies from immunocompromised patients",
    "What this study found about statins and memory",
    "future research directions for gut microbiome therapy in inflammatory bowel disease",
    "too short",
    "Survival of patients with advanced melanoma treated with nivolumab plus ipilimumab compared with nivolumab alone",
    "What the hippocampus looks like on MRI in early Alzheimer's disease compared with healthy older adults",
    "How phosphorylated ERK levels change in colorectal cancer cells after MEK inhibitor treatment",
    "Mean lesion size in the crude ES group at day 14",
];

const QUERIES: [&str; 26] = [
    "How metformin changes blood lactate levels in patients admitted with septic shock",
    "diabetes",
    "colchicine lacunar stroke recurrence",
    "does metformin raise lactate in septic shock?",
    "what did this study find about statins",
    "You need to find the aspirin dose for kids",
    "slot-03 colchicine stroke",
    "{\"query\": \"spilled\"}",
    "$\\alpha$7 receptor knockout mice memory",
    "how did they measure cortisol in the saliva samples",
    "Rider B heart rate after the jump",
    "future research directions for CRISPR delivery to the liver",
    "nivolumab ipilimumab melanoma survival curve combination versus monotherapy long term",
    "my mum has early alzheimers what does her brain scan show",
    "figure showing kaplan meier survival for nivolumab in melanoma",
    "what does this curve mean for patients on the combination",
    "western blot p-ERK MEK inhibitor colorectal",
    "the special cell stain for fungi",
    "B510 nitrogen fixation rice",
    "picture of a torn ACL on MRI",
    "that graph where the survival lines cross after a year",
    "diabetes vaccine colonoscopy alzheimer's metformin seizures",
    "is low dose colchicine good for preventing another stroke",
    "which drug better for stroke prevention colchicine or aspirin",
    "hippocampal atrophy MRI early AD vs controls",
    "immunofluorescence images of tau tangles spreading between neurons",
];

const ROLES: [&str; 5] = ["", "researcher", "veterinary surgeon", "agronomist", "hospital pharmacist"];

fn text_for(name: &str, x: u64) -> String {
    match name {
        "need" => NEEDS[(x % NEEDS.len() as u64) as usize].to_string(),
        "query" => QUERIES[(x % QUERIES.len() as u64) as usize].to_string(),
        "expert_role" => ROLES[(x % ROLES.len() as u64) as usize].to_string(),
        "answer" => "no significant reduction at two years (HR 0.93)".into(),
        "narrative" => "Relevant documents report recurrence after lacunar stroke with colchicine; cardiac outcomes alone do not count.".into(),
        "visible" => "two diverging survival curves with numbers at risk".into(),
        _ => format!("mock {name}"),
    }
}

/// A value that fits `schema`. `idx` = position inside an array of objects (card / slot ids are assigned in order).
pub fn fake(schema: &Value, name: &str, idx: Option<usize>, seed: &str) -> Value {
    let x = uh(&[seed, name, &idx.map(|i| i.to_string()).unwrap_or_default()]);
    if let Some(e) = schema["enum"].as_array() {
        if e.is_empty() {
            return Value::Null;
        }
        return e[(x % e.len() as u64) as usize].clone();
    }
    match schema["type"].as_str().unwrap_or("") {
        "object" => {
            let mut m = Map::new();
            for (k, s) in schema["properties"].as_object().into_iter().flatten() {
                let v = match (s["enum"].as_array(), idx) {
                    (Some(e), Some(i)) if (k == "card" || k == "slot") && !e.is_empty() => e[i % e.len()].clone(),
                    _ => fake(s, k, None, &format!("{seed}/{}", idx.unwrap_or(0))),
                };
                m.insert(k.clone(), v);
            }
            Value::Object(m)
        }
        "array" => {
            let lo = schema["minItems"].as_u64().unwrap_or(1) as usize;
            let hi = schema["maxItems"].as_u64().map(|x| x as usize).unwrap_or(lo.max(1));
            let n = lo.max(1).min(hi.max(lo));
            let n = if name == "cards" || name == "slots" { lo } else { n };
            Value::Array((0..n).map(|i| fake(&schema["items"], name, Some(i), seed)).collect())
        }
        "boolean" => json!(match name {
            "skip" => x % 10 == 0,
            "biomedical" => x % 13 != 0,
            "legible" => x % 20 != 0,
            _ => x % 2 == 0,
        }),
        "integer" => json!(schema["minimum"].as_i64().unwrap_or(0)),
        "string" => json!(text_for(name, x)),
        _ => Value::Null,
    }
}

fn reply(body: &Value) -> Value {
    let user = &body["messages"][1]["content"];
    let text = match user {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    let seed = crate::util::hash(&[&text]);
    let schema = &body["response_format"]["json_schema"]["schema"];
    let content = fake(schema, "", None, &seed).to_string();
    json!({"id": "mock", "object": "chat.completion", "model": body["model"],
        "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": text.len() / 4, "completion_tokens": content.len() / 4, "total_tokens": (text.len() + content.len()) / 4}})
}

fn handle(mut s: TcpStream, rec: Arc<Mutex<Option<std::fs::File>>>, model: String) {
    let mut rd = BufReader::new(s.try_clone().unwrap());
    loop {
        let mut line = String::new();
        if rd.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let (method, path) = (parts.first().copied().unwrap_or(""), parts.get(1).copied().unwrap_or("").to_string());
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            if rd.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; len];
        if rd.read_exact(&mut body).is_err() {
            return;
        }
        let (ctype, out) = if method == "POST" && path.ends_with("/chat/completions") {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if let Some(f) = rec.lock().unwrap().as_mut() {
                let row = json!({"system": v["messages"][0]["content"], "user": v["messages"][1]["content"],
                    "schema": v["response_format"]["json_schema"]["schema"], "max_tokens": v["max_tokens"], "temperature": v["temperature"],
                    "top_p": v["top_p"], "extra": v.as_object().map(|o| o.iter().filter(|(k, _)| !["model", "messages", "response_format", "max_tokens", "temperature", "top_p"].contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect::<Map<String, Value>>())});
                let _ = writeln!(f, "{row}");
            }
            ("application/json", reply(&v).to_string())
        } else if path.ends_with("/models") {
            ("application/json", json!({"object": "list", "data": [{"id": model, "object": "model"}]}).to_string())
        } else {
            ("text/plain", String::new())
        };
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n{out}", out.len());
        if s.write_all(resp.as_bytes()).is_err() {
            return;
        }
    }
}

/// Serve until killed. `record`: append one JSON line per chat request (system, user, schema, sampling, extra).
pub fn serve(port: u16, record: Option<PathBuf>, model: &str) -> anyhow::Result<()> {
    let l = TcpListener::bind(("127.0.0.1", port))?;
    let rec = Arc::new(Mutex::new(match record {
        Some(p) => Some(std::fs::OpenOptions::new().create(true).append(true).open(p)?),
        None => None,
    }));
    eprintln!("mock server on 127.0.0.1:{} (model {model})", l.local_addr()?.port());
    for s in l.incoming().flatten() {
        let rec = rec.clone();
        let m = model.to_string();
        std::thread::spawn(move || handle(s, rec, m));
    }
    Ok(())
}
