//! Helpers that must stay bit-compatible with `src/pigmed/common.py`.
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// Python `json.dumps(v, sort_keys=True, ensure_ascii=False, separators=(",", ":"))`.
pub fn py_json_sorted_compact(v: &Value) -> String {
    let mut out = String::new();
    write_sorted(v, &mut out);
    out
}

fn write_sorted(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            // Python sorts str keys by code point; Rust String ordering is byte-wise UTF-8,
            // which is identical to code-point order.
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push(':');
                write_sorted(&m[*k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_sorted(x, out);
            }
            out.push(']');
        }
        Value::Number(n) => out.push_str(&py_number(n)),
        other => out.push_str(&serde_json::to_string(other).unwrap()),
    }
}

/// Python float repr always carries ".0" or an exponent; serde_json matches for finite values
/// except it can print "1e16" where Python prints "1e+16".
fn py_number(n: &serde_json::Number) -> String {
    if n.is_f64() {
        let f = n.as_f64().unwrap();
        let s = serde_json::to_string(&f).unwrap();
        if let Some(pos) = s.find('e') {
            let (mant, exp) = s.split_at(pos);
            let exp = &exp[1..];
            let (sign, digits) = if let Some(d) = exp.strip_prefix('-') {
                ("-", d)
            } else {
                ("+", exp.strip_prefix('+').unwrap_or(exp))
            };
            return format!("{mant}e{sign}{digits:0>2}");
        }
        s
    } else {
        n.to_string()
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// `stable_id(*parts)`: first 32 hex chars of sha256 over the sorted compact JSON array.
pub fn stable_id(parts: &[Value]) -> String {
    let s = py_json_sorted_compact(&Value::Array(parts.to_vec()));
    sha256_hex(s.as_bytes())[..32].to_string()
}

pub fn digest_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Python `json.dump(value, ensure_ascii=False, sort_keys=True, indent=2)` + fsync + rename.
pub fn atomic_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!("{}.tmp", path.file_name().unwrap().to_string_lossy()));
    let sorted = sort_keys_deep(value);
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"  ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    sorted.serialize(&mut ser)?;
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn sort_keys_deep(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sort_keys_deep(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_keys_deep).collect()),
        x => x.clone(),
    }
}

/// Python `json.dumps(row, ensure_ascii=False, separators=(",", ":"))` (insertion order kept).
pub fn compact_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap()
}

/// Python default `json.dumps(v, ensure_ascii=False)`: separators ", " and ": ".
pub fn py_default_json(v: &Value) -> Vec<u8> {
    struct PyFmt;
    impl serde_json::ser::Formatter for PyFmt {
        fn begin_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_key<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
            w.write_all(b": ")
        }
    }
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PyFmt);
    v.serialize(&mut ser).unwrap();
    buf
}

pub fn read_jsonl(path: &Path) -> Result<Vec<Value>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader: Box<dyn BufRead> = if path.to_string_lossy().ends_with(".gz") {
        Box::new(BufReader::new(flate2::read::GzDecoder::new(f)))
    } else {
        Box::new(BufReader::new(f))
    };
    let mut rows = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if !line.trim().is_empty() {
            rows.push(serde_json::from_str(&line)?);
        }
    }
    Ok(rows)
}

pub fn write_jsonl(path: &Path, rows: &[Value]) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let mut f = io::BufWriter::new(File::create(path)?);
    for r in rows {
        f.write_all(compact_json(r).as_bytes())?;
        f.write_all(b"\n")?;
    }
    f.flush()?;
    Ok(())
}

pub fn safe_member(name: &str) -> Result<String> {
    let p = Path::new(name);
    if name.is_empty() || p.is_absolute() || name.contains('\\') || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("Unsafe relative name: {name:?}");
    }
    Ok(name.to_string())
}

pub fn tree_bytes(path: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        let Ok(rd) = fs::read_dir(p) else { return 0 };
        rd.flatten()
            .map(|e| {
                let ft = e.file_type();
                match ft {
                    Ok(t) if t.is_dir() => walk(&e.path()),
                    Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                    _ => 0,
                }
            })
            .sum()
    }
    walk(path)
}

/// Python `str.isspace()` / `re` `\s`: Unicode White_Space plus U+001C..U+001F.
pub fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `re.sub(r"\s+", " ", s).strip()`
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for c in s.chars() {
        if py_space(c) {
            pending = true;
        } else {
            if pending && !out.is_empty() {
                out.push(' ');
            }
            pending = false;
            out.push(c);
        }
    }
    out
}

pub fn normal_license(value: Option<&str>) -> String {
    let v = value.unwrap_or("UNKNOWN").trim().to_uppercase().replace('_', "-");
    let ws = regex::Regex::new(r"\s+").unwrap();
    let mut v = ws.replace_all(&v, "-").to_string();
    let ver = regex::Regex::new(r"-(?:[1-4](?:\.0)?)$").unwrap();
    v = ver.replace(&v, "").to_string();
    match v.as_str() {
        "CC-0" | "CC0" | "PUBLIC-DOMAIN" => "CC0".into(),
        "BY" => "CC-BY".into(),
        _ => v,
    }
}

pub fn allowed_license(value: Option<&str>) -> bool {
    matches!(normal_license(value).as_str(), "CC0" | "CC-BY")
}

/// Commercial-use subset of PMC OA: CC0, CC BY, CC BY-SA, CC BY-ND (no NC licences, no "other").
pub fn commercial_license(value: Option<&str>) -> bool {
    matches!(normal_license(value).as_str(), "CC0" | "CC-BY" | "CC-BY-SA" | "CC-BY-ND")
}

pub fn ensure_dir(p: &Path) -> Result<PathBuf> {
    fs::create_dir_all(p)?;
    Ok(p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stable_id_matches_python() {
        // python -c 'from pigmed.common import stable_id; print(stable_id("PMC1"), stable_id("a", 1, [2.5, None], {"b": True, "a": "é"}))'
        // Python parity is checked end-to-end by scripts/rust_parity.py; here we only check format.
        let id = stable_id(&[json!("PMC1")]);
        assert_eq!(id.len(), 32);
        assert_eq!(py_json_sorted_compact(&json!(["a", 1, [2.5, null], {"b": true, "a": "é"}])),
                   r#"["a",1,[2.5,null],{"a":"é","b":true}]"#);
        assert_eq!(py_json_sorted_compact(&json!([1e16_f64, 612.0_f64, 0.1_f64])), "[1e+16,612.0,0.1]");
    }

    #[test]
    fn whitespace_and_license() {
        assert_eq!(collapse_ws("  a\u{a0}\n b\u{1c}c  "), "a b c");
        assert_eq!(normal_license(Some("CC BY")), "CC-BY");
        assert_eq!(normal_license(Some("cc by 4.0")), "CC-BY 4.0".replace(' ', "-").replace("-4.0", ""));
        assert!(allowed_license(Some("CC0")));
        assert!(!allowed_license(Some("CC BY-NC")));
        assert!(!allowed_license(None));
        assert!(commercial_license(Some("CC BY-SA")) && commercial_license(Some("cc by-nd 4.0")) && commercial_license(Some("CC0")));
        assert!(!commercial_license(Some("CC BY-NC")) && !commercial_license(Some("CC BY-NC-SA")) && !commercial_license(None));
    }
}
