//! Chunk recipes applied to an article's body-text units.
//!
//! `balanced-c` is an exact port of `scripts/chunk_variants.py` recipe C (`load_articles` + `pack`): each section's
//! text is rebuilt from its stored text units (char offsets index the section text), consecutive sections under the
//! same top-level heading (and abstract-ness) are packed into one stream with inline sub-headings, and a stream longer
//! than `max` words is split evenly (budget = ceil(words / ceil(words / target)) + overlap, capped at `max`) with
//! `split_text`'s sentence snapping; a tail shorter than `min` words (after overlap) is folded into its neighbour.
//! Only `kind: "text"` rows are re-chunked; tables, captions, figures, supplements and audit rows pass through.
//! Unit ids match the Python reference: sha256(json.dumps([aid, "packed", k, t[:64], target, min, max, overlap],
//! sort_keys=True))[:32].
use crate::common::py_space;
use anyhow::{bail, Context, Result};
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
pub struct Recipe {
    pub target: usize,
    pub min: usize,
    pub max: usize,
    pub overlap: usize,
    /// Put the chunk's top-level heading in the model input: `embed_text` = "title: none | text: <heading>\n<chunk>".
    pub heading_prefix: bool,
}

impl Default for Recipe {
    fn default() -> Self {
        Recipe { target: 384, min: 100, max: 512, overlap: 48, heading_prefix: false }
    }
}

impl Recipe {
    pub fn tag(&self) -> Value {
        let mut v = json!({"name": "balanced-c", "target": self.target, "min": self.min, "max": self.max, "overlap": self.overlap});
        if self.heading_prefix {
            v["embed_input"] = json!("title: none | text: <section[0]>\n<text>");
        }
        v
    }
}

/// Word tokens (Python `re.finditer(r"\S+")`) as char-index spans [start, end).
fn tokens(chars: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if py_space(chars[i]) {
            i += 1;
            continue;
        }
        let s = i;
        while i < chars.len() && !py_space(chars[i]) {
            i += 1;
        }
        out.push((s, i));
    }
    out
}

fn n_words(chars: &[char]) -> usize {
    tokens(chars).len()
}

/// `[.!?]["')\]]?$` on one token.
fn sentence_end(tok: &[char]) -> bool {
    let n = tok.len();
    if n >= 1 && matches!(tok[n - 1], '.' | '!' | '?') {
        return true;
    }
    n >= 2 && matches!(tok[n - 1], '"' | '\'' | ')' | ']') && matches!(tok[n - 2], '.' | '!' | '?')
}

/// Port of `split_text`: (char start, char end, words) per piece.
fn split_text(chars: &[char], budget: usize, overlap: usize) -> Vec<(usize, usize, usize)> {
    let toks = tokens(chars);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let mut end = (i + budget).min(toks.len());
        if end < toks.len() {
            let lo = (i + overlap + 1).max(i + budget * 3 / 4);
            let mut j = end;
            while j > lo {
                let (a, b) = toks[j - 1];
                if sentence_end(&chars[a..b]) {
                    end = j;
                    break;
                }
                j -= 1;
            }
        }
        out.push((toks[i].0, toks[end - 1].1, end - i));
        if end == toks.len() {
            break;
        }
        i = (i + 1).max(end.saturating_sub(overlap));
    }
    out
}

struct Section {
    sid: String,
    path: Vec<String>,
    text: Vec<char>,
    abstract_: bool,
}

pub struct Packed {
    pub text: String,
    pub path: Vec<String>,
    pub sids: Vec<String>,
    pub abstract_: bool,
    pub char_start: usize,
    pub char_end: usize,
    pub words: usize,
}

fn is_abstract(sid: &str, path: &[String]) -> bool {
    sid.to_lowercase().starts_with("abs") || path.join(" ").to_lowercase().contains("abstract")
}

fn strip_py(chars: &[char]) -> Vec<char> {
    let mut a = 0;
    let mut b = chars.len();
    while a < b && py_space(chars[a]) {
        a += 1;
    }
    while b > a && py_space(chars[b - 1]) {
        b -= 1;
    }
    chars[a..b].to_vec()
}

fn pack(sections: &[Section], r: Recipe) -> Vec<Packed> {
    // groups of consecutive sections sharing (top heading or sid, abstract)
    let mut groups: Vec<Vec<&Section>> = Vec::new();
    let mut cur_key: Option<(String, bool)> = None;
    for s in sections {
        let key = (s.path.first().cloned().unwrap_or_else(|| s.sid.clone()), s.abstract_);
        if cur_key.as_ref() != Some(&key) {
            groups.push(Vec::new());
            cur_key = Some(key);
        }
        groups.last_mut().unwrap().push(s);
    }
    let mut out = Vec::new();
    for g in groups {
        let path: Vec<String> = g[0].path.iter().take(1).cloned().collect();
        let sids: Vec<String> = g.iter().map(|s| s.sid.clone()).collect();
        let ab = g[0].abstract_;
        let mut stream: Vec<char> = Vec::new();
        for (n, s) in g.iter().enumerate() {
            if n > 0 {
                stream.extend(['\n', '\n']);
            }
            let sub: Vec<char> = if s.path.len() > 1 { s.path.last().unwrap().chars().collect() } else { vec![] };
            if !sub.is_empty() && !s.text.starts_with(&sub) {
                stream.extend(sub.iter());
                stream.extend(['.', ' ']);
            }
            stream.extend(s.text.iter());
        }
        let words = n_words(&stream);
        if words <= r.max {
            out.push(Packed { text: stream.iter().collect(), path: path.clone(), sids, abstract_: ab, char_start: 0,
                              char_end: stream.len(), words });
            continue;
        }
        let n = words.div_ceil(r.target);
        let budget = r.max.min(words.div_ceil(n) + r.overlap);
        let mut pieces = split_text(&stream, budget, r.overlap);
        if pieces.len() > 1 {
            let last = pieces[pieces.len() - 1];
            if (last.2 as i64) - (r.overlap as i64) < r.min as i64 {
                let prev = pieces[pieces.len() - 2];
                let merged = (prev.0, last.1, n_words(&stream[prev.0..last.1]));
                pieces.truncate(pieces.len() - 2);
                pieces.push(merged);
            }
        }
        for (a, b, w) in pieces {
            out.push(Packed { text: stream[a..b].iter().collect(), path: path.clone(), sids: sids.clone(), abstract_: ab,
                              char_start: a, char_end: b, words: w });
        }
    }
    out
}

/// Python `json.dumps(obj)` (ensure_ascii, ", " separators) for strings.
fn py_str_ascii(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn packed_uid(aid: &str, k: usize, text: &str, r: Recipe) -> String {
    let mut s = String::from("[");
    py_str_ascii(aid, &mut s);
    s.push_str(", \"packed\", ");
    s.push_str(&k.to_string());
    s.push_str(", ");
    let head: String = text.chars().take(64).collect();
    py_str_ascii(&head, &mut s);
    s.push_str(&format!(", {}, {}, {}, {}]", r.target, r.min, r.max, r.overlap));
    hex::encode(Sha256::digest(s.as_bytes()))[..32].to_string()
}

/// Rebuild one article's sections from its text rows (Python `load_articles`) in first-seen order.
fn sections_of(text_rows: &[&Map<String, Value>]) -> Vec<Section> {
    let mut order: Vec<String> = Vec::new();
    let mut secs: HashMap<String, (Vec<String>, Vec<(usize, usize, Vec<char>)>)> = HashMap::new();
    for r in text_rows {
        let loc = r.get("locator").and_then(|v| v.as_object());
        let sid = loc.and_then(|l| l.get("section_id")).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let path: Vec<String> = loc
            .and_then(|l| l.get("section"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect())
            .unwrap_or_default();
        let cs = loc.and_then(|l| l.get("char_start")).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let ce = loc.and_then(|l| l.get("char_end")).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let t: Vec<char> = r.get("text").and_then(|v| v.as_str()).unwrap_or("").chars().collect();
        let e = secs.entry(sid.clone()).or_insert_with(|| {
            order.push(sid.clone());
            (path, Vec::new())
        });
        e.1.push((cs, ce, t));
    }
    order
        .into_iter()
        .map(|sid| {
            let (path, pieces) = secs.remove(&sid).unwrap();
            let n = pieces.iter().map(|p| p.1).max().unwrap_or(0);
            let mut buf = vec![' '; n];
            for (cs, ce, t) in &pieces {
                if ce >= cs && ce - cs == t.len() {
                    buf[*cs..*ce].copy_from_slice(t);
                }
            }
            let abstract_ = is_abstract(&sid, &path);
            Section { text: strip_py(&buf), sid, path, abstract_ }
        })
        .collect()
}

const DROP: [&str; 6] = ["unit_id", "text", "locator", "token_count", "tokenizer", "chunk_recipe"];

/// Re-chunk the `kind: "text"` rows of a row list (any number of articles). Each article's packed rows are placed
/// where its first text row was; all other rows keep their order. Returns (rows, text rows in, packed rows out).
pub fn rechunk_rows(rows: Vec<Map<String, Value>>, r: Recipe) -> (Vec<Map<String, Value>>, usize, usize) {
    let is_text = |m: &Map<String, Value>| {
        m.get("kind").and_then(|v| v.as_str()) == Some("text")
            && m.get("text").and_then(|v| v.as_str()).map(|t| !t.is_empty()).unwrap_or(false)
    };
    let aid_of = |m: &Map<String, Value>| m.get("article_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut by_article: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, m) in rows.iter().enumerate() {
        if is_text(m) {
            by_article.entry(aid_of(m)).or_default().push(i);
        }
    }
    let mut packed_at: HashMap<usize, Vec<Map<String, Value>>> = HashMap::new();
    let mut n_in = 0;
    let mut n_out = 0;
    for (aid, idx) in &by_article {
        let trs: Vec<&Map<String, Value>> = idx.iter().map(|&i| &rows[i]).collect();
        n_in += trs.len();
        let secs = sections_of(&trs);
        let mut base = trs[0].clone();
        for k in DROP {
            base.remove(k);
        }
        let mut new_rows = Vec::new();
        for (k, p) in pack(&secs, r).into_iter().enumerate() {
            let mut m = base.clone();
            m.insert("unit_id".into(), json!(packed_uid(aid, k, &p.text, r)));
            m.insert("kind".into(), json!("text"));
            m.insert("text".into(), json!(p.text));
            m.insert("is_abstract".into(), json!(p.abstract_));
            m.insert("locator".into(), json!({"section_ids": p.sids, "section": p.path,
                "char_start": p.char_start, "char_end": p.char_end, "offsets": "packed-stream"}));
            m.insert("token_count".into(), json!(p.words));
            m.insert("tokenizer".into(), json!("regex-word-v1"));
            m.insert("chunk_recipe".into(), r.tag());
            if r.heading_prefix {
                let head = p.path.first().filter(|h| !h.is_empty()).map(|h| format!("{h}\n")).unwrap_or_default();
                m.insert("embed_text".into(), json!(format!("title: none | text: {head}{}", p.text)));
            }
            new_rows.push(m);
        }
        n_out += new_rows.len();
        packed_at.insert(idx[0], new_rows);
    }
    let mut out = Vec::with_capacity(rows.len());
    for (i, m) in rows.into_iter().enumerate() {
        if let Some(p) = packed_at.remove(&i) {
            out.extend(p);
        } else if !is_text(&m) {
            out.push(m);
        }
    }
    (out, n_in, n_out)
}

/// Late-chunking documents (port of the tested v2 Python repack construction, `--late-out`): one
/// `late_doc` row per article from its packed body rows in order. Each chunk is preceded by its heading path
/// (" > "-joined non-empty headings + "\n" when the path is non-empty) and chunks are joined by "\n\n"; the document
/// is prefixed "title: <whitespace-collapsed title or none> | text: ". `spans` are code-point offsets into `text`
/// (prefix included) whose ids are the packed chunk unit ids, so late vectors pair with naive ones.
pub fn late_rows(rows: &[Map<String, Value>]) -> Vec<Map<String, Value>> {
    let mut order: Vec<String> = Vec::new();
    let mut docs: HashMap<String, (Option<String>, String, usize, Vec<Value>)> = HashMap::new();
    for m in rows {
        if m.get("kind").and_then(|v| v.as_str()) != Some("text") || m.get("chunk_recipe").and_then(|v| v.get("name")).is_none() {
            continue;
        }
        let aid = m.get("article_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let e = docs.entry(aid.clone()).or_insert_with(|| {
            order.push(aid.clone());
            (None, String::new(), 0, Vec::new())
        });
        if e.0.is_none() {
            e.0 = m.get("title").and_then(|v| v.as_str()).map(String::from);
        }
        let path: Vec<&str> = m.get("locator").and_then(|l| l.get("section")).and_then(|v| v.as_array())
            .map(|a| a.iter().map(|x| x.as_str().unwrap_or("")).collect()).unwrap_or_default();
        let head = if path.is_empty() { String::new() } else { path.iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" > ") + "\n" };
        let tx = m.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if !e.1.is_empty() {
            e.1.push_str("\n\n");
            e.2 += 2;
        }
        e.1.push_str(&head);
        e.2 += head.chars().count();
        let st = e.2;
        e.1.push_str(tx);
        e.2 += tx.chars().count();
        e.3.push(json!({"id": m.get("unit_id").cloned().unwrap_or(Value::Null), "start": st, "end": e.2}));
    }
    let ws = regex::Regex::new(r"\s+").unwrap();
    order.into_iter().map(|aid| {
        let (title, doc, _, spans) = docs.remove(&aid).unwrap();
        let tt = ws.replace_all(title.as_deref().unwrap_or("none"), " ").trim().to_string();
        let tt = if tt.is_empty() { "none".to_string() } else { tt };
        let prefix = format!("title: {tt} | text: ");
        let pc = prefix.chars().count();
        let spans: Vec<Value> = spans.into_iter().map(|s| json!({"id": s["id"], "start": s["start"].as_u64().unwrap() as usize + pc,
            "end": s["end"].as_u64().unwrap() as usize + pc})).collect();
        let mut uid = String::from("[");
        py_str_ascii(&aid, &mut uid);
        uid.push_str(", \"late_doc\"]");
        json!({"article_id": aid, "unit_id": hex::encode(Sha256::digest(uid.as_bytes()))[..32].to_string(), "kind": "late_doc",
               "text": prefix + &doc, "prefix_chars": pc, "spans": spans}).as_object().unwrap().clone()
    }).collect()
}

fn write_rows_bundle(ready: &Path, name: &str, rows: &[Map<String, Value>], man_extra: Value) -> Result<()> {
    fs::create_dir_all(ready)?;
    let tmp: PathBuf = ready.join(format!("{name}.partial"));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    for m in rows {
        serde_json::to_writer(&mut gz, m)?;
        gz.write_all(b"\n")?;
    }
    let gzb = gz.finish()?;
    fs::write(tmp.join("items.jsonl.gz"), &gzb)?;
    let mut man = json!({"bundle": name, "rows": rows.len(), "schema_version": 1, "writer": "pigmed-rs rechunk",
        "files": [{"path": "items.jsonl.gz", "bytes": gzb.len(), "sha256": hex::encode(Sha256::digest(&gzb))}]});
    if let (Some(o), Value::Object(x)) = (man.as_object_mut(), man_extra) {
        o.extend(x);
    }
    fs::write(tmp.join("manifest.json"), serde_json::to_vec_pretty(&man)?)?;
    let dest = ready.join(name);
    let _ = fs::remove_dir_all(&dest);
    fs::rename(&tmp, &dest)?;
    Ok(())
}

/// Re-chunk one committed bundle into `out_root/runs/<run>/ready/<bundle name>` (atomic rename; skipped when
/// the output exists and records the same source manifest sha256).
pub fn rechunk_bundle(bundle: &Path, out_root: &Path, run: &str, r: Recipe, body_only: bool, late_out: Option<&Path>) -> Result<Value> {
    let name = bundle.file_name().context("bundle name")?.to_string_lossy().to_string();
    let src_manifest = fs::read(bundle.join("manifest.json")).with_context(|| format!("{}: manifest", bundle.display()))?;
    let src_sha = hex::encode(Sha256::digest(&src_manifest));
    let ready = out_root.join("runs").join(run).join("ready");
    let dest = ready.join(&name);
    if let Ok(m) = fs::read(dest.join("manifest.json")) {
        let v: Value = serde_json::from_slice(&m)?;
        if v["source_manifest_sha256"].as_str() == Some(src_sha.as_str()) {
            return Ok(json!({"bundle": name, "skipped": "exists"}));
        }
    }
    let src: Value = serde_json::from_slice(&src_manifest)?;
    let items = bundle.join("items.jsonl.gz");
    let want = src["files"].as_array().and_then(|f| f.iter().find(|x| x["path"] == "items.jsonl.gz")).map(|x| x["sha256"].clone());
    let data = fs::read(&items)?;
    if let Some(Value::String(w)) = want {
        if hex::encode(Sha256::digest(&data)) != w {
            bail!("{}: items.jsonl.gz sha256 mismatch vs manifest", bundle.display());
        }
    }
    let mut rows = Vec::new();
    for line in BufReader::new(MultiGzDecoder::new(&data[..])).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line)? {
            Value::Object(m) => rows.push(m),
            _ => bail!("non-object row"),
        }
    }
    let n_rows_in = rows.len();
    let (mut rows, n_in, n_out) = rechunk_rows(rows, r);
    if body_only {  // only the re-chunked body text (e.g. a text-only re-embed; other units keep their vectors)
        rows.retain(|m| m.get("kind").and_then(|v| v.as_str()) == Some("text") && m.contains_key("chunk_recipe")
            && m["chunk_recipe"].get("name").and_then(|v| v.as_str()) == Some("balanced-c"));
    }
    // Late docs are written BEFORE the naive bundle appears, so a consumer that sees the naive bundle also sees its twin.
    let n_late = match late_out {
        Some(lo) => {
            let lr = late_rows(&rows);
            if !lr.is_empty() {
                write_rows_bundle(&lo.join("runs").join(run).join("ready"), &name, &lr,
                    json!({"kind": "late_docs", "chunk_recipe": r.tag(), "source_manifest_sha256": src_sha}))?;
            }
            lr.len()
        }
        None => 0,
    };
    fs::create_dir_all(&ready)?;
    let tmp: PathBuf = ready.join(format!("{name}.partial"));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    for m in &rows {
        serde_json::to_writer(&mut gz, m)?;
        gz.write_all(b"\n")?;
    }
    let gzb = gz.finish()?;
    fs::write(tmp.join("items.jsonl.gz"), &gzb)?;
    let man = json!({"bundle": name, "rows": rows.len(), "schema_version": 1, "writer": "pigmed-rs rechunk", "body_only": body_only,
        "export_approved": src["export_approved"], "chunk_recipe": r.tag(), "source_manifest_sha256": src_sha,
        "source_checkpoint": src["checkpoint"],
        "files": [{"path": "items.jsonl.gz", "bytes": gzb.len(), "sha256": hex::encode(Sha256::digest(&gzb))}]});
    fs::write(tmp.join("manifest.json"), serde_json::to_vec_pretty(&man)?)?;
    let _ = fs::remove_dir_all(&dest);
    fs::rename(&tmp, &dest)?;
    Ok(json!({"bundle": name, "rows_in": n_rows_in, "rows_out": rows.len(), "text_in": n_in, "packed_out": n_out, "late_docs": n_late}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trow(aid: &str, sid: &str, path: &[&str], cs: usize, text: &str) -> Map<String, Value> {
        let n = text.chars().count();
        json!({"article_id": aid, "kind": "text", "unit_id": format!("{aid}-{sid}-{cs}"), "text": text, "license": "CC-BY",
               "locator": {"section_id": sid, "section": path, "char_start": cs, "char_end": cs + n}})
        .as_object().unwrap().clone()
    }

    fn words(prefix: &str, n: usize) -> String {
        (0..n).map(|i| if i % 13 == 12 { format!("{prefix}{i}.") } else { format!("{prefix}{i}") }).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn uid_matches_python_json_dumps() {
        // python: hashlib.sha256(json.dumps(["PMC1.1","packed",0,"Café \"x\"\n\U0001F600",384,100,512,48],
        //          sort_keys=True).encode()).hexdigest()[:32]
        // python -I: chunk_variants.uid("PMC1.1", "packed", 0, "Café \"x\"\n😀", 384, 100, 512, 48)
        assert_eq!(packed_uid("PMC1.1", 0, "Café \"x\"\n😀", Recipe::default()), "68f81e0f46d8b7b0d7ec6fdb8508d9e9");
        let mut s = String::new();
        py_str_ascii("Caf\u{e9} \"x\"\n\u{1F600}\u{7f}", &mut s);
        assert_eq!(s, concat!("\"Caf", "\\", "u00e9 ", "\\", "\"x", "\\", "\"", "\\", "n", "\\", "ud83d", "\\", "ude00", "\\", "u007f\""));
    }

    #[test]
    fn short_group_is_one_chunk_with_inline_subheadings() {
        let rows = vec![
            trow("A.1", "abs", &["Abstract"], 0, "Short abstract."),
            trow("A.1", "s1", &["Methods"], 0, "Intro to methods."),
            trow("A.1", "s1/s1.1", &["Methods", "Culture"], 0, "Cells were grown."),
            trow("A.1", "s1/s1.2", &["Methods", "Stats"], 0, "Stats were done."),
            trow("A.1", "s2", &["Results"], 0, "It worked."),
        ];
        let (out, n_in, n_out) = rechunk_rows(rows, Recipe::default());
        assert_eq!((n_in, n_out), (5, 3));
        let texts: Vec<&str> = out.iter().map(|m| m["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["Short abstract.", "Intro to methods.\n\nCulture. Cells were grown.\n\nStats were done.", "It worked."]);
        assert_eq!(out[0]["is_abstract"], json!(true));
        assert_eq!(out[1]["locator"]["section"], json!(["Methods"]));
        assert_eq!(out[1]["locator"]["section_ids"], json!(["s1", "s1/s1.1", "s1/s1.2"]));
        assert_eq!(out[1]["license"], json!("CC-BY"));
    }

    #[test]
    fn overlapping_units_rebuild_the_section() {
        // two units of one section overlapping by one word reconstruct the original text exactly
        let full = words("w", 30);
        let ch: Vec<char> = full.chars().collect();
        let toks = tokens(&ch);
        let (a0, a1) = (toks[0].0, toks[19].1);
        let (b0, b1) = (toks[18].0, toks[29].1);
        let u1: String = ch[a0..a1].iter().collect();
        let u2: String = ch[b0..b1].iter().collect();
        let rows = vec![trow("B.1", "s1", &["Body"], a0, &u1), trow("B.1", "s1", &["Body"], b0, &u2)];
        let (out, _, n_out) = rechunk_rows(rows, Recipe::default());
        assert_eq!(n_out, 1);
        assert_eq!(out[0]["text"].as_str().unwrap(), full);
    }

    #[test]
    fn long_group_splits_evenly_and_folds_tail() {
        let r = Recipe::default();
        let text = words("x", 1200);
        let rows = vec![trow("C.1", "s1", &["Discussion"], 0, &text)];
        let (out, _, _) = rechunk_rows(rows, r);
        // expected from scripts/chunk_variants.py pack() + uid() on the same input
        let want = [(338, "fa00549435d070c44ea917c2e6836ba0", "x337."), (347, "c80ccfe7e6239744685e0b1ee03f385b", "x636."),
                    (347, "7158fe9c7600f77cb8c6172eab82ab52", "x935."), (312, "3d51c916384023606b6f5bf4c8d2490d", "x1199")];
        assert_eq!(out.len(), want.len());
        for (m, (w, u, tail)) in out.iter().zip(want) {
            assert_eq!(m["token_count"].as_u64().unwrap() as usize, w);
            assert_eq!(m["unit_id"].as_str().unwrap(), u);
            assert!(m["text"].as_str().unwrap().ends_with(tail));
        }
        assert!(out.iter().all(|m| m["chunk_recipe"]["name"] == "balanced-c"));
    }

    #[test]
    fn late_docs_follow_repack_v2() {
        let r = Recipe { target: 256, min: 64, max: 384, overlap: 32, heading_prefix: true };
        let mut rows = vec![trow("PMC1.1", "s1", &["Intro"], 0, "Alpha beta."), trow("PMC1.1", "s2", &["Methods", "Sub"], 0, "Gamma delta."),
                            trow("PMC2.1", "s1", &[], 0, "Solo text \u{e9}.")];
        rows[0].insert("title".into(), json!("A  title\n x"));
        let (out, _, _) = rechunk_rows(rows, r);
        let late = late_rows(&out);
        assert_eq!(late.len(), 2);
        let d = &late[0];
        let text = d["text"].as_str().unwrap();
        assert!(text.starts_with("title: A title x | text: Intro\nAlpha beta."), "{text}");
        let chars: Vec<char> = text.chars().collect();
        let packed: Vec<&Map<String, Value>> = out.iter().filter(|m| m["article_id"] == "PMC1.1" && m["kind"] == "text").collect();
        let spans = d["spans"].as_array().unwrap();
        assert_eq!(spans.len(), packed.len());
        for (s, p) in spans.iter().zip(&packed) {
            assert_eq!(s["id"], p["unit_id"]);
            let got: String = chars[s["start"].as_u64().unwrap() as usize..s["end"].as_u64().unwrap() as usize].iter().collect();
            assert_eq!(got, p["text"].as_str().unwrap());
        }
        assert_eq!(d["prefix_chars"], json!("title: A title x | text: ".chars().count()));
        let t2 = late[1]["text"].as_str().unwrap();
        assert!(t2.starts_with("title: none | text: Solo"), "{t2}");
    }

    #[test]
    fn heading_prefix_sets_embed_text() {
        let r = Recipe { heading_prefix: true, ..Recipe::default() };
        let rows = vec![trow("H.1", "s1", &["Methods"], 0, "We did it."), trow("H.1", "s2", &[], 0, "No heading.")];
        let (out, _, _) = rechunk_rows(rows, r);
        assert_eq!(out[0]["embed_text"], json!("title: none | text: Methods\nWe did it."));
        assert_eq!(out[1]["embed_text"], json!("title: none | text: No heading."));
        assert_eq!(out[0]["chunk_recipe"]["embed_input"], json!("title: none | text: <section[0]>\n<text>"));
    }

    #[test]
    fn sentence_snap() {
        assert!(sentence_end(&['a', '.']) && sentence_end(&['.', ')']) && !sentence_end(&['a', ')']) && !sentence_end(&[')']));
    }
}
