//! Hash draws, template filling and JSON-lines IO (formats shared with rust/pigmed-synth).

use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

/// sha256 of the parts joined by U+001F, hex (same as pigmed-synth `sample::hash`).
pub fn hash(parts: &[&str]) -> String {
    hex::encode(Sha256::digest(parts.join("\u{1f}").as_bytes()))
}

/// First 12 hex digits of `hash` as an integer.
pub fn uh(parts: &[&str]) -> u64 {
    u64::from_str_radix(&hash(parts)[..12], 16).unwrap_or(0)
}

/// Weighted draw, items in the order given.
pub fn pick<'a>(x: u64, items: &'a [(String, u32)]) -> &'a str {
    let tot: u32 = items.iter().map(|(_, w)| *w).sum();
    let mut r = (x % tot.max(1) as u64) as u32;
    for (v, w) in items {
        if r < *w {
            return v;
        }
        r -= w;
    }
    &items[items.len() - 1].0
}

pub fn chance(x: u64, p: f64) -> bool {
    (x % 1_000_000) as f64 / 1e6 < p
}

/// Fill {{name}} placeholders. Unknown names are left as they are, so a template typo shows up in the prompt.
pub fn fill(t: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(t.len() + 256);
    let mut rest = t;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find("}}") {
            Some(j) => {
                let name = &after[..j];
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push_str("{{");
                        out.push_str(name);
                        out.push_str("}}");
                    }
                }
                rest = &after[j + 2..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Token-set Jaccard similarity (lowercase alphanumeric tokens).
pub fn jaccard(a: &str, b: &str) -> f64 {
    let t = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect::<HashSet<_>>();
    let (x, y) = (t(a), t(b));
    let u = x.union(&y).count();
    if u == 0 { 0.0 } else { x.intersection(&y).count() as f64 / u as f64 }
}

pub fn first_words(s: &str, n: usize) -> String {
    s.split_whitespace().take(n).collect::<Vec<_>>().join(" ")
}

pub fn reader(path: &Path) -> Result<Box<dyn BufRead>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(if path.extension().is_some_and(|e| e == "gz") { Box::new(BufReader::new(MultiGzDecoder::new(f))) } else { Box::new(BufReader::new(f)) })
}

pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let mut out = vec![];
    for (i, line) in reader(path)?.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(&line).with_context(|| format!("{}:{}", path.display(), i + 1))?);
    }
    Ok(out)
}

pub fn write_jsonl<'a, T: Serialize + 'a>(path: &Path, rows: impl IntoIterator<Item = &'a T>) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::new(File::create(&tmp)?);
        for r in rows {
            serde_json::to_writer(&mut w, r)?;
            w.write_all(b"\n")?;
        }
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_and_pick() {
        assert_eq!(fill("a {{x}} b {{y}} {{z}}", &[("x", "1"), ("y", "{{x}}")]), "a 1 b {{x}} {{z}}");
        let items = vec![("a".to_string(), 1), ("b".to_string(), 3)];
        assert_eq!(pick(0, &items), "a");
        assert_eq!(pick(3, &items), "b");
        assert!(jaccard("a b c", "a b c") == 1.0 && jaccard("", "") == 0.0);
    }
}
