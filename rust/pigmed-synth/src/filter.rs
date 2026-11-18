//! EG2 round-trip filter: rank of each query's source chunk (and source article) among every embedded text unit
//! of the corpus, by brute-force cosine; plus near-duplicate queries.

use crate::io::read_jsonl;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Little-endian 2-D .npy of f2 or f4 -> (rows, cols, f32 data).
pub fn read_npy(path: &Path) -> Result<(usize, usize, Vec<f32>)> {
    let b = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if b.len() < 10 || &b[1..6] != b"NUMPY" {
        bail!("not npy: {}", path.display());
    }
    let (hlen, off) = if b[6] == 1 { (u16::from_le_bytes([b[8], b[9]]) as usize, 10) } else {
        (u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize, 12)
    };
    let header = std::str::from_utf8(&b[off..off + hlen])?;
    if header.contains("'fortran_order': True") {
        bail!("fortran order unsupported");
    }
    let shape = header.split("'shape': (").nth(1).and_then(|s| s.split(')').next()).context("shape")?;
    let dims: Vec<usize> = shape.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    let (rows, cols) = match dims.as_slice() { [r, c] => (*r, *c), [r] => (*r, 1), _ => bail!("shape {shape}") };
    let data = &b[off + hlen..];
    let v: Vec<f32> = if header.contains("'<f2'") {
        data.chunks_exact(2).take(rows * cols).map(|x| half::f16::from_le_bytes([x[0], x[1]]).to_f32()).collect()
    } else if header.contains("'<f4'") {
        data.chunks_exact(4).take(rows * cols).map(|x| f32::from_le_bytes([x[0], x[1], x[2], x[3]])).collect()
    } else {
        bail!("dtype in {header}")
    };
    if v.len() != rows * cols {
        bail!("short npy {}", path.display());
    }
    Ok((rows, cols, v))
}

pub fn normalize(v: &mut [f32], dim: usize) {
    v.par_chunks_mut(dim).for_each(|r| {
        let n = r.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        r.iter_mut().for_each(|x| *x /= n);
    });
}

#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    for (x, y) in a.chunks_exact(8).zip(b.chunks_exact(8)) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    acc.iter().sum::<f32>() + a.chunks_exact(8).remainder().iter().zip(b.chunks_exact(8).remainder()).map(|(x, y)| x * y).sum::<f32>()
}

pub struct Corpus {
    pub dim: usize,
    pub vecs: Vec<f32>,
    pub unit_ids: Vec<String>,
    pub article: Vec<u32>,
    pub articles: Vec<String>,
    pub index: HashMap<String, usize>,
}

/// Every text-modality unit of VECTORS/<model>/<bundle>/{dense.npy, ids.json}, with articles from VECTORS/units.
pub fn load_corpus(vectors: &Path, model: &str, modality_want: &str) -> Result<Corpus> {
    let mut unit_article: HashMap<String, String> = HashMap::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(vectors.join("units"))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    for f in files {
        for r in read_jsonl::<Value>(&f)? {
            if let (Some(u), Some(a)) = (r["unit_id"].as_str(), r["article_id"].as_str()) {
                unit_article.insert(u.to_string(), a.to_string());
            }
        }
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(vectors.join(model))?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("meta.json").exists()).collect();
    dirs.sort();
    let parts: Vec<(Vec<String>, Vec<f32>, usize)> = dirs.par_iter().map(|d| -> Result<_> {
        let ids: Value = serde_json::from_str(&std::fs::read_to_string(d.join("ids.json"))?)?;
        let (rows, cols, v) = read_npy(&d.join("dense.npy"))?;
        let uid = ids["unit_id"].as_array().context("unit_id")?;
        let modality = ids["modality"].as_array().context("modality")?;
        if uid.len() != rows {
            bail!("ids/rows mismatch in {}", d.display());
        }
        let (mut keep_ids, mut keep_v) = (vec![], vec![]);
        for i in 0..rows {
            if modality[i] == modality_want {
                keep_ids.push(uid[i].as_str().unwrap_or("").to_string());
                keep_v.extend_from_slice(&v[i * cols..(i + 1) * cols]);
            }
        }
        Ok((keep_ids, keep_v, cols))
    }).collect::<Result<_>>()?;
    let dim = parts.iter().map(|p| p.2).max().unwrap_or(768);
    let (mut vecs, mut unit_ids, mut article, mut articles, mut aidx) = (vec![], vec![], vec![], vec![], HashMap::new());
    for (ids, v, _) in parts {
        vecs.extend(v);
        for u in ids {
            let a = unit_article.get(&u).cloned().unwrap_or_default();
            let n = aidx.len() as u32;
            let i = *aidx.entry(a.clone()).or_insert_with(|| { articles.push(a); n });
            article.push(i);
            unit_ids.push(u);
        }
    }
    normalize(&mut vecs, dim);
    let index = unit_ids.iter().enumerate().map(|(i, u)| (u.clone(), i)).collect();
    Ok(Corpus { dim, vecs, unit_ids, article, articles, index })
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QueryMeta {
    pub qid: String,
    pub arm: String,
    pub qtype: String,
    pub chunk_id: String,
    pub article_id: String,
    pub text: String,
    /// recipe v2 task (T1, T2, F1, F2, I1); empty for the original four-type arms
    #[serde(default)]
    pub task: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Ranks {
    pub qid: String,
    /// 1-based rank of the source chunk among all corpus text units
    pub chunk_rank: usize,
    /// 1-based rank of the source article (by its best unit) among all articles, capped at cap+1
    pub article_rank: usize,
    /// 1-based rank of the source chunk among its own article's units
    pub within_rank: usize,
    pub score: f32,
}

/// Ranks for each query vector (normalised rows of `q`, aligned with `metas`). Queries whose chunk is missing from
/// the corpus get rank 0.
pub fn ranks(c: &Corpus, q: &[f32], metas: &[QueryMeta], cap: usize) -> Vec<Ranks> {
    let d = c.dim;
    let src: Vec<Option<usize>> = metas.iter().map(|m| c.index.get(&m.chunk_id).copied()).collect();
    // per query: source chunk score and best score inside the source article
    let mut members: HashMap<u32, Vec<usize>> = HashMap::new();
    for s in src.iter().flatten() {
        members.entry(c.article[*s]).or_default();
    }
    for (i, a) in c.article.iter().enumerate() {
        if let Some(v) = members.get_mut(a) {
            v.push(i);
        }
    }
    let pre: Vec<Option<(f32, f32, usize)>> = (0..metas.len()).into_par_iter().map(|k| {
        let s = src[k]?;
        let qv = &q[k * d..(k + 1) * d];
        let sc = dot(qv, &c.vecs[s * d..(s + 1) * d]);
        let mut best = f32::MIN;
        let mut within = 1;
        for &u in &members[&c.article[s]] {
            let x = dot(qv, &c.vecs[u * d..(u + 1) * d]);
            best = best.max(x);
            if u != s && x > sc {
                within += 1;
            }
        }
        Some((sc, best, within))
    }).collect();
    // corpus scan: parallel over blocks of queries; each block streams the corpus in tiles that stay in cache
    let n = c.unit_ids.len();
    const QB: usize = 64;
    const TILE: usize = 1024;
    let blocks: Vec<Vec<Ranks>> = (0..metas.len().div_ceil(QB)).into_par_iter().map(|b| {
        let ks: Vec<usize> = (b * QB..((b + 1) * QB).min(metas.len())).collect();
        let mut cnt = vec![0u32; ks.len()];
        let mut above: Vec<Vec<u32>> = vec![vec![]; ks.len()];
        for lo in (0..n).step_by(TILE) {
            let hi = (lo + TILE).min(n);
            for (j, &k) in ks.iter().enumerate() {
                let Some((sc, best, _)) = pre[k] else { continue };
                let qv = &q[k * d..(k + 1) * d];
                for u in lo..hi {
                    let x = dot(qv, &c.vecs[u * d..(u + 1) * d]);
                    if x > sc {
                        cnt[j] += 1;
                    }
                    if x > best && above[j].len() <= cap * 4 {
                        above[j].push(c.article[u]);
                    }
                }
            }
        }
        ks.iter().enumerate().map(|(j, &k)| {
            let Some((sc, _, within)) = pre[k] else { return Ranks { qid: metas[k].qid.clone(), ..Default::default() } };
            let own = c.article[src[k].unwrap()];
            let mut arts = std::mem::take(&mut above[j]);
            arts.sort_unstable();
            arts.dedup();
            arts.retain(|a| *a != own);
            Ranks { qid: metas[k].qid.clone(), chunk_rank: cnt[j] as usize + 1, article_rank: (arts.len() + 1).min(cap + 1),
                    within_rank: within, score: sc }
        }).collect()
    }).collect();
    blocks.into_iter().flatten().collect()
}

/// Query pairs of the same arm and type with cosine >= threshold from different chunks (near-duplicates).
pub fn duplicate_queries(q: &[f32], d: usize, metas: &[QueryMeta], threshold: f32) -> Vec<(usize, usize, f32)> {
    (0..metas.len()).into_par_iter().flat_map_iter(|i| {
        let qi = &q[i * d..(i + 1) * d];
        ((i + 1)..metas.len()).filter_map(move |j| {
            let (a, b) = (&metas[i], &metas[j]);
            if a.arm != b.arm || a.qtype != b.qtype || a.chunk_id == b.chunk_id {
                return None;
            }
            let x = dot(qi, &q[j * d..(j + 1) * d]);
            (x >= threshold).then_some((i, j, x))
        }).collect::<Vec<_>>()
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npy_roundtrip_and_ranks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.npy");
        let header = "{'descr': '<f4', 'fortran_order': False, 'shape': (2, 3), }";
        let mut h = header.to_string();
        while (10 + h.len() + 1) % 64 != 0 { h.push(' '); }
        h.push('\n');
        let mut b = vec![0x93u8];
        b.extend(b"NUMPY");
        b.extend([1u8, 0]);
        b.extend((h.len() as u16).to_le_bytes());
        b.extend(h.as_bytes());
        for x in [1f32, 0., 0., 0., 1., 0.] { b.extend(x.to_le_bytes()); }
        std::fs::write(&p, b).unwrap();
        let (r, c, v) = read_npy(&p).unwrap();
        assert_eq!((r, c), (2, 3));
        assert_eq!(v[4], 1.0);

        // corpus: 4 units in 2 articles, dim 2
        let vecs = vec![1.0, 0.0, 0.8, 0.6, 0.0, 1.0, 0.6, 0.8];
        let unit_ids: Vec<String> = ["u0", "u1", "u2", "u3"].iter().map(|s| s.to_string()).collect();
        let corpus = Corpus { dim: 2, vecs, index: unit_ids.iter().enumerate().map(|(i, u)| (u.clone(), i)).collect(),
            unit_ids, article: vec![0, 0, 1, 1], articles: vec!["A".into(), "B".into()] };
        let m = |c: &str| QueryMeta { qid: c.into(), arm: "x".into(), qtype: "k".into(), chunk_id: c.into(), article_id: String::new(), text: String::new(), task: String::new() };
        let q = vec![1.0, 0.0, 0.0, 1.0];
        let r = ranks(&corpus, &q, &[m("u0"), m("u3")], 10);
        assert_eq!((r[0].chunk_rank, r[0].within_rank, r[0].article_rank), (1, 1, 1));
        // query [0,1] for u3: u2 (score 1.0) beats u3 (0.8) -> chunk rank 2, within 2, article still 1
        assert_eq!((r[1].chunk_rank, r[1].within_rank, r[1].article_rank), (2, 2, 1));
        let dups = duplicate_queries(&[1.0, 0.0, 0.99, 0.141], 2, &[m("u0"), m("u1")], 0.95);
        assert_eq!(dups.len(), 1);
    }
}
