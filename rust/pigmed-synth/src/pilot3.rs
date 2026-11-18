//! v2-vs-v3 pilot analysis (docs/RETRIEVER_QUERY_RECIPE_V3.md §7). Per arm:
//!   keep rates: v2 gate (source chunk top-3 in its article AND article top-3) and the v3 style-aware gate (lay,
//!     half-remembered and non-native styles: source article in the corpus top-10);
//!   diversity: distinct-2/3, top two-word opener share, mean words; embedding dispersion (mean pairwise cosine
//!     distance) and Vendi-2 (inverse participation ratio of the query Gram spectrum, = n^2 / sum_ij cos^2);
//!   register mix: judge3 register, v3 style and persona group shares;
//!   judge3 rates on kept queries: standalone, generic, exam, natural;
//!   realism vs real query sets (eval only): coverage = mean over real queries of the max cosine to the arm's
//!     queries; precision = mean over the arm's queries of the max cosine to the real set.
//! Also writes 40 random kept v3 queries with persona and intent, and a blinded hand-read table.

use crate::filter::{read_npy, normalize, QueryMeta, Ranks};
use crate::generate::Generation;
use crate::io::read_jsonl;
use crate::recipe::words;
use crate::report::keep;
use crate::sample::hash;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const LAY_STYLES: [&str; 3] = ["lay_paraphrase", "half_remembered", "non_native"];

pub fn keep_v3(r: &Ranks, style: &str) -> bool {
    if LAY_STYLES.contains(&style) { r.chunk_rank > 0 && r.article_rank <= 10 } else { keep(r) }
}

pub struct Emb {
    pub ids: Vec<String>,
    pub dim: usize,
    pub v: Vec<f32>,
}

pub fn load_emb(dir: &Path) -> Result<Emb> {
    let (rows, cols, mut v) = read_npy(&dir.join("embeddings.dense.npy"))?;
    let ids: Vec<String> = serde_json::from_str(&std::fs::read_to_string(dir.join("embeddings.ids.json")).context("ids")?)?;
    anyhow::ensure!(rows == ids.len(), "{} rows vs {} ids", rows, ids.len());
    normalize(&mut v, cols);
    Ok(Emb { ids, dim: cols, v })
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// (mean pairwise cosine distance, Vendi-2) of unit vectors `rows`.
pub fn dispersion(rows: &[&[f32]], dim: usize) -> (f64, f64) {
    let n = rows.len();
    if n < 2 {
        return (0.0, n as f64);
    }
    // sum_ij cos^2 = ||X^T X||_F^2 (d x d), sum_ij cos = ||sum x||^2
    let mut g = vec![0f64; dim * dim];
    let mut s = vec![0f64; dim];
    for r in rows {
        for i in 0..dim {
            let xi = r[i] as f64;
            s[i] += xi;
            if xi != 0.0 {
                let gi = &mut g[i * dim..(i + 1) * dim];
                for j in 0..dim {
                    gi[j] += xi * r[j] as f64;
                }
            }
        }
    }
    let sum_sq: f64 = g.iter().map(|x| x * x).sum();
    let sum_cos: f64 = s.iter().map(|x| x * x).sum();
    let nf = n as f64;
    let mean_off = (sum_cos - nf) / (nf * (nf - 1.0));
    (1.0 - mean_off, nf * nf / sum_sq)
}

fn max_cos(q: &[f32], set: &[&[f32]]) -> f32 {
    set.iter().map(|r| dot(q, r)).fold(f32::MIN, f32::max)
}

fn sample_idx(n: usize, cap: usize, seed: &str) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by_key(|i| hash(&[seed, &i.to_string()]));
    idx.truncate(cap);
    idx.sort();
    idx
}

fn distinct(qs: &[Vec<String>], n: usize) -> f64 {
    let (mut all, mut set) = (0usize, HashSet::new());
    for q in qs {
        for w in q.windows(n) {
            all += 1;
            set.insert(w.join(" "));
        }
    }
    if all == 0 { 0.0 } else { set.len() as f64 / all as f64 }
}

pub struct Inputs<'a> {
    pub metas: &'a [QueryMeta],
    pub ranks: &'a [Ranks],
    pub qemb: &'a Emb,
    pub gens: &'a [Generation],
    pub judge: &'a HashMap<String, Value>,
    pub refs: &'a [(String, Emb)],
}

pub fn analyse(inp: &Inputs) -> (Value, String, String, Value) {
    let rank: HashMap<&str, &Ranks> = inp.ranks.iter().map(|r| (r.qid.as_str(), r)).collect();
    let pos: HashMap<&str, usize> = inp.qemb.ids.iter().enumerate().map(|(i, x)| (x.as_str(), i)).collect();
    let d = inp.qemb.dim;
    let vec_of = |qid: &str| pos.get(qid).map(|&i| &inp.qemb.v[i * d..(i + 1) * d]);
    // slot meta by qid
    let mut slot: HashMap<String, Value> = HashMap::new();
    for g in inp.gens {
        for (k, m) in g.meta["slots"].as_object().into_iter().flatten() {
            slot.insert(format!("{}-{}-{}", g.arm, k, g.chunk_id), m.clone());
        }
    }
    let mut by_arm: BTreeMap<&str, Vec<&QueryMeta>> = BTreeMap::new();
    for m in inp.metas {
        by_arm.entry(m.arm.as_str()).or_default().push(m);
    }
    let mut out = Map::new();
    for (arm, ms) in &by_arm {
        let style = |m: &QueryMeta| slot.get(&m.qid).and_then(|s| s["style"].as_str()).unwrap_or("v2").to_string();
        let ranked: Vec<(&QueryMeta, &Ranks)> = ms.iter().filter_map(|m| rank.get(m.qid.as_str()).map(|r| (*m, *r))).filter(|(_, r)| r.chunk_rank > 0).collect();
        let n = ranked.len().max(1) as f64;
        let k2 = ranked.iter().filter(|(_, r)| keep(r)).count() as f64 / n;
        let kept: Vec<&QueryMeta> = ranked.iter().filter(|(m, r)| keep_v3(r, &style(m))).map(|(m, _)| *m).collect();
        let k3 = kept.len() as f64 / n;
        let mut by_style: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
        for (m, r) in &ranked {
            let e = by_style.entry(style(m)).or_default();
            e.0 += 1;
            e.1 += keep(r) as usize;
            e.2 += keep_v3(r, &style(m)) as usize;
        }
        let style_rows: Map<String, Value> = by_style.iter().map(|(s, (a, b, c))| (s.clone(), json!({"queries": a,
            "share": *a as f64 / n, "keep_v2": *b as f64 / (*a).max(1) as f64, "keep_v3": *c as f64 / (*a).max(1) as f64}))).collect();
        // text diversity, over all generated (pre-filter) and kept
        let div = |set: &[&QueryMeta]| {
            let ws: Vec<Vec<String>> = set.iter().map(|m| words(&m.text)).collect();
            let mut op: HashMap<String, usize> = HashMap::new();
            for w in &ws {
                *op.entry(w.iter().take(2).cloned().collect::<Vec<_>>().join(" ")).or_default() += 1;
            }
            let top = op.iter().max_by_key(|x| *x.1).map(|(k, v)| (k.clone(), *v)).unwrap_or_default();
            let nn = set.len().max(1) as f64;
            let idx = sample_idx(set.len(), 4000, arm);
            let rows: Vec<&[f32]> = idx.iter().filter_map(|&i| vec_of(&set[i].qid)).collect();
            let (mpd, vendi) = dispersion(&rows, d);
            json!({"queries": set.len(), "mean_words": ws.iter().map(|w| w.len()).sum::<usize>() as f64 / nn,
                "distinct2": distinct(&ws, 2), "distinct3": distinct(&ws, 3), "top_opener": top.0, "top_opener_share": top.1 as f64 / nn,
                "emb_sample": rows.len(), "mean_pairwise_cos_distance": mpd, "vendi2": vendi, "vendi2_per_query": vendi / rows.len().max(1) as f64})
        };
        let all: Vec<&QueryMeta> = ms.clone();
        // judge3 on kept queries
        let js: Vec<&Value> = kept.iter().filter_map(|m| inp.judge.get(&m.qid)).collect();
        let jn = js.len().max(1) as f64;
        let jshare = |f: &dyn Fn(&Value) -> bool| js.iter().filter(|x| f(x)).count() as f64 / jn;
        let mut reg: BTreeMap<String, usize> = BTreeMap::new();
        for x in &js {
            *reg.entry(x["register"].as_str().unwrap_or("?").to_string()).or_default() += 1;
        }
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();
        let mut l2 = 0usize;
        for m in &kept {
            let s = slot.get(&m.qid);
            *groups.entry(s.and_then(|s| s["persona_group"].as_str()).unwrap_or("none").to_string()).or_default() += 1;
            l2 += s.is_some_and(|s| s["english"].as_str().is_some_and(|e| e != "native")) as usize;
        }
        // diversity within an intent card (v3: the 2 surface forms of one (card, persona) pair): mean cosine distance
        let mut by_card: HashMap<String, Vec<&[f32]>> = HashMap::new();
        for m in &all {
            if let (Some(s), Some(v)) = (slot.get(&m.qid), vec_of(&m.qid)) {
                if s["task"] == json!("T3") {
                    let k = m.qid.get(m.arm.len() + 1..m.qid.len().saturating_sub(m.chunk_id.len() + 1)).unwrap_or("");
                    let key = format!("{}|{}|{}", m.arm, m.chunk_id, k.split(".q").next().unwrap_or(k));
                    by_card.entry(key).or_default().push(v);
                }
            }
        }
        let (mut wsum, mut wn) = (0f64, 0usize);
        for vs in by_card.values() {
            for i in 0..vs.len() {
                for j in i + 1..vs.len() {
                    wsum += 1.0 - dot(vs[i], vs[j]) as f64;
                    wn += 1;
                }
            }
        }
        let within_card = if wn > 0 { json!({"pairs": wn, "mean_cos_distance": wsum / wn as f64}) } else { Value::Null };
        // realism vs real query sets, on kept queries
        let kidx = sample_idx(kept.len(), 4000, &format!("{arm}-real"));
        let krows: Vec<&[f32]> = kidx.iter().filter_map(|&i| vec_of(&kept[i].qid)).collect();
        let mut real = Map::new();
        for (name, e) in inp.refs {
            let rrows: Vec<&[f32]> = (0..e.ids.len()).map(|i| &e.v[i * e.dim..(i + 1) * e.dim]).collect();
            if krows.is_empty() || rrows.is_empty() || e.dim != d {
                continue;
            }
            let cov = rrows.iter().map(|r| max_cos(r, &krows) as f64).sum::<f64>() / rrows.len() as f64;
            let prec = krows.iter().map(|q| max_cos(q, &rrows) as f64).sum::<f64>() / krows.len() as f64;
            real.insert(name.clone(), json!({"coverage_mean_max_cos": cov, "precision_mean_max_cos": prec, "real_queries": rrows.len()}));
        }
        let st: Vec<&&Value> = js.iter().filter(|x| x["style_ok"].is_boolean()).collect();
        let style_ok = st.iter().filter(|x| x["style_ok"] == json!(true)).count() as f64 / st.len().max(1) as f64;
        out.insert(arm.to_string(), json!({
            "generated": ms.len(), "ranked": ranked.len(), "keep_v2_gate": k2, "keep_v3_gate": k3, "kept_v3": kept.len(),
            "by_style": style_rows, "diversity_all": div(&all), "within_card": within_card, "diversity_kept": div(&kept),
            "judge_kept": {"judged": js.len(), "standalone": jshare(&|x| x["standalone"] == json!(true)),
                "doc_referential": jshare(&|x| x["standalone"] == json!(false)), "generic_many": jshare(&|x| x["specificity"] == json!("many")),
                "exam": jshare(&|x| x["exam"] == json!(true)),
                "natural_mean": js.iter().filter_map(|x| x["natural"].as_u64()).sum::<u64>() as f64 / jn,
                "register": reg,
                "style_ok": style_ok},
            "persona_groups_kept": groups, "l2_english_share_kept": l2 as f64 / kept.len().max(1) as f64,
            "realism": real,
        }));
    }
    // 40 random kept v3 queries (persona-first arm first: the adopted v3 arm; then any v3 arm)
    let mut v3: Vec<(&QueryMeta, &Value)> = inp.metas.iter().filter_map(|m| {
        let s = slot.get(&m.qid)?;
        let r = rank.get(m.qid.as_str())?;
        (s["task"] == json!("T3") && keep_v3(r, s["style"].as_str().unwrap_or(""))).then_some((m, s))
    }).collect();
    v3.sort_by_key(|(m, _)| (!m.arm.ends_with("c3pf"), hash(&["sample40", &m.qid])));
    let if_n = v3.iter().filter(|(m, _)| m.arm.ends_with("c3pf")).count();
    let take: Vec<&(&QueryMeta, &Value)> = v3.iter().take(40).collect();
    let mut md = format!("# 40 random kept v3 queries ({} from the persona-first arm)", if_n.min(40)) + "\n\n| # | query | style | persona | intent | need |\n|---|---|---|---|---|---|\n";
    for (i, (m, s)) in take.iter().enumerate() {
        let esc = |x: &str| x.replace('|', "/");
        md += &format!("| {} | {} | {} | {} | {} | {} |\n", i + 1, esc(&m.text), s["style"].as_str().unwrap_or(""),
            esc(s["persona"].as_str().unwrap_or("none")), s["intent"].as_str().unwrap_or(""), esc(s["need"].as_str().unwrap_or("")));
    }
    // blinded hand-read: 16 articles present in every arm, 3 random queries per article per arm
    let arms: Vec<&str> = by_arm.keys().copied().collect();
    let mut per: HashMap<(&str, &str), Vec<&QueryMeta>> = HashMap::new();
    for m in inp.metas {
        per.entry((m.arm.as_str(), m.article_id.as_str())).or_default().push(m);
    }
    let mut common: Vec<&str> = inp.metas.iter().map(|m| m.article_id.as_str()).collect::<HashSet<_>>().into_iter()
        .filter(|a| arms.iter().all(|arm| per.contains_key(&(*arm, *a)))).collect();
    common.sort_by_key(|a| hash(&["handread", a]));
    common.truncate(16);
    let mut rows: Vec<(String, &QueryMeta)> = vec![];
    for a in &common {
        for arm in &arms {
            let mut qs = per[&(*arm, *a)].clone();
            qs.sort_by_key(|m| hash(&["hr", &m.qid]));
            for m in qs.into_iter().take(3) {
                rows.push((hash(&["hrrow", &m.qid])[..8].to_string(), m));
            }
        }
    }
    rows.sort_by(|x, y| (x.1.article_id.as_str(), &x.0).cmp(&(y.1.article_id.as_str(), &y.0)));
    let mut blind = String::from("# Hand-read (blinded): score standalone 0/1, specific 0/1, natural 1-3, persona-plausible 1-3, answered 0/1\n\n\
Queries are grouped by article; arms are hidden (key in handread_key.json).\n\n| row | article | query | standalone | specific | natural | persona | answered |\n|---|---|---|---|---|---|---|---|\n");
    let mut key = Map::new();
    for (id, m) in &rows {
        blind += &format!("| {id} | {} | {} | | | | | |\n", m.article_id, m.text.replace('|', "/"));
        let s = slot.get(&m.qid);
        key.insert(id.clone(), json!({"arm": m.arm, "qid": m.qid, "persona": s.and_then(|s| s["persona"].as_str()),
            "style": s.and_then(|s| s["style"].as_str()), "need": s.and_then(|s| s["need"].as_str())}));
    }
    (Value::Object(out), md, blind, Value::Object(key))
}

pub fn load_judge(dir: &Path) -> Result<HashMap<String, Value>> {
    let mut m = HashMap::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            for v in read_jsonl::<Value>(&p)? {
                if let Some(q) = v["qid"].as_str() {
                    m.insert(q.to_string(), v.clone());
                }
            }
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispersion_and_gates() {
        let a = [1f32, 0.0];
        let b = [0f32, 1.0];
        let (mpd, v) = dispersion(&[&a, &b], 2);
        assert!((mpd - 1.0).abs() < 1e-9 && (v - 2.0).abs() < 1e-9);
        let (mpd, v) = dispersion(&[&a, &a, &a], 2);
        assert!(mpd.abs() < 1e-9 && (v - 1.0).abs() < 1e-9);
        let r = Ranks { qid: "q".into(), chunk_rank: 40, article_rank: 7, within_rank: 5, score: 0.3 };
        assert!(!keep_v3(&r, "keyword") && keep_v3(&r, "lay_paraphrase"));
    }
}
