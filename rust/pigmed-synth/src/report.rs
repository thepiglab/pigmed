//! A/B report: keep-rates per arm and query type under the round-trip rule, ranks, lexical overlap, contextual
//! naming, near-duplicates, and a side-by-side review sample.

use crate::filter::{QueryMeta, Ranks};
use crate::sample::{hash, SampleChunk};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

const STOP: [&str; 40] = ["the", "and", "for", "with", "that", "this", "from", "are", "was", "were", "what", "which",
    "how", "does", "did", "can", "who", "why", "when", "where", "is", "of", "in", "to", "a", "an", "on", "by", "or",
    "be", "it", "as", "at", "do", "my", "i", "there", "their", "its", "than"];

fn tokens(s: &str) -> Vec<String> {
    let re = Regex::new(r"[a-z0-9]+").unwrap();
    re.find_iter(&s.to_lowercase()).map(|m| m.as_str().to_string()).filter(|t| t.len() > 2 && !STOP.contains(&t.as_str())).collect()
}

/// Share of the query's content tokens that appear in the passage.
pub fn overlap(query: &str, passage: &HashSet<String>) -> f64 {
    let t = tokens(query);
    if t.is_empty() { 0.0 } else { t.iter().filter(|x| passage.contains(*x)).count() as f64 / t.len() as f64 }
}

/// Does the query contain a title content word that the passage lacks (the contextual-query goal)?
pub fn names_missing_context(query: &str, title: &str, passage: &HashSet<String>) -> bool {
    let q: HashSet<String> = tokens(query).into_iter().collect();
    tokens(title).iter().any(|t| t.len() > 3 && !passage.contains(t) && q.contains(t))
}

/// Does the query copy 8 consecutive passage words?
pub fn copies(query: &str, passage_words: &[String]) -> bool {
    let q: Vec<String> = query.to_lowercase().split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string()).collect();
    if q.len() < 8 { return false; }
    let grams: HashSet<String> = passage_words.windows(8).map(|w| w.join(" ")).collect();
    q.windows(8).any(|w| grams.contains(&w.join(" ")))
}

pub fn keep(r: &Ranks) -> bool {
    r.chunk_rank > 0 && r.within_rank <= 3 && r.article_rank <= 3
}

/// distinct word bigrams / all bigrams over a set of queries (diversity)
pub fn distinct2(qs: &[&str]) -> f64 {
    let (mut all, mut uniq) = (0usize, HashSet::new());
    for q in qs {
        let w: Vec<String> = q.to_lowercase().split_whitespace().map(String::from).collect();
        for b in w.windows(2) {
            all += 1;
            uniq.insert(b.join(" "));
        }
    }
    if all == 0 { 0.0 } else { uniq.len() as f64 / all as f64 }
}

pub fn build(chunks: &[SampleChunk], metas: &[QueryMeta], ranks: &[Ranks], dups: &[(usize, usize, f32)],
             gen_summaries: &[Value], review_n: usize) -> (Value, String, String, Value) {
    let by_chunk: HashMap<&str, &SampleChunk> = chunks.iter().map(|c| (c.chunk_id.as_str(), c)).collect();
    let rank_of: HashMap<&str, &Ranks> = ranks.iter().map(|r| (r.qid.as_str(), r)).collect();
    let mut dup_count: HashMap<(String, String), usize> = HashMap::new();
    for (i, _, _) in dups {
        *dup_count.entry((metas[*i].arm.clone(), metas[*i].qtype.clone())).or_default() += 1;
    }
    let mut stats: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let arms: Vec<String> = { let mut a: Vec<String> = metas.iter().map(|m| m.arm.clone()).collect(); a.sort(); a.dedup(); a };
    for arm in &arms {
        let mut types: Vec<String> = metas.iter().filter(|m| &m.arm == arm).map(|m| m.qtype.clone()).collect();
        types.sort();
        types.dedup();
        for qt in types.iter().map(String::as_str).chain(["all"]) {
            let qs: Vec<&QueryMeta> = metas.iter().filter(|m| &m.arm == arm && (qt == "all" || m.qtype == qt)).collect();
            let rs: Vec<&Ranks> = qs.iter().filter_map(|m| rank_of.get(m.qid.as_str()).copied()).filter(|r| r.chunk_rank > 0).collect();
            let n = rs.len().max(1) as f64;
            let share = |f: &dyn Fn(&Ranks) -> bool| rs.iter().filter(|r| f(r)).count() as f64 / n;
            let (mut ov, mut ctx, mut cp, mut words) = (0.0, 0usize, 0usize, 0usize);
            for m in &qs {
                let Some(c) = by_chunk.get(m.chunk_id.as_str()) else { continue };
                let pw: Vec<String> = c.text.to_lowercase().split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string()).collect();
                let ps: HashSet<String> = tokens(&c.text).into_iter().collect();
                ov += overlap(&m.text, &ps);
                ctx += names_missing_context(&m.text, &c.title, &ps) as usize;
                cp += copies(&m.text, &pw) as usize;
                words += m.text.split_whitespace().count();
            }
            let nq = qs.len().max(1) as f64;
            let dups = if qt == "all" { dup_count.iter().filter(|((a, _), _)| a == arm).map(|(_, v)| *v).sum() } else {
                dup_count.get(&(arm.clone(), qt.to_string())).copied().unwrap_or(0) };
            let texts: Vec<&str> = qs.iter().map(|m| m.text.as_str()).collect();
            let mut med: Vec<usize> = rs.iter().map(|r| r.chunk_rank).collect();
            med.sort_unstable();
            stats.entry(arm.clone()).or_default().insert(qt.to_string(), json!({
                "queries": qs.len(), "ranked": rs.len(),
                "keep_round_trip_top3": share(&|r| keep(r)),
                "within_article_top3": share(&|r| r.within_rank <= 3),
                "article_top1": share(&|r| r.article_rank <= 1), "article_top3": share(&|r| r.article_rank <= 3),
                "chunk_top3_corpus": share(&|r| r.chunk_rank <= 3), "chunk_top10_corpus": share(&|r| r.chunk_rank <= 10),
                "chunk_rank_median": med.get(med.len() / 2).copied().unwrap_or(0),
                "mean_words": words as f64 / nq, "mean_passage_overlap": ov / nq,
                "names_context_missing_from_passage": ctx as f64 / nq, "copies_8gram": cp as f64 / nq,
                "near_duplicates_cos95": dups, "near_duplicate_rate": dups as f64 / nq, "distinct_bigrams": distinct2(&texts),
            }));
        }
    }
    let jv: BTreeMap<String, Value> = gen_summaries.iter().filter_map(|g| Some((g["arm"].as_str()?.to_string(), g["json_valid_rate"].clone()))).collect();
    let report = json!({"generation": gen_summaries, "json_valid_rate": jv, "filter": stats,
        "rule": "keep = source chunk in top 3 of its own article AND source article in top 3 of the corpus (EG2 cosine)"});
    // side-by-side review on the same chunks
    let mut by_chunk_arm: HashMap<(&str, &str), Vec<&QueryMeta>> = HashMap::new();
    for m in metas {
        by_chunk_arm.entry((m.chunk_id.as_str(), m.arm.as_str())).or_default().push(m);
    }
    let mut picked: Vec<&SampleChunk> = chunks.iter().filter(|c| arms.iter().all(|a| by_chunk_arm.contains_key(&(c.chunk_id.as_str(), a.as_str())))).collect();
    picked.sort_by_key(|c| hash(&["review", &c.chunk_id]));
    picked.truncate(review_n);
    let mut md = String::from("# Synthetic-query A/B: review sample\n\nRanks: chunk rank in corpus / chunk rank in article / article rank. ✓ = kept.\n");
    for c in picked {
        let excerpt: String = c.text.split_whitespace().take(120).collect::<Vec<_>>().join(" ");
        let _ = write!(md, "\n## {} — {}\n\n*{}* · {}\n\n> {} …\n\n", c.article_id, c.title, c.section, c.cluster, excerpt);
        for arm in &arms {
            let _ = writeln!(md, "**{arm}**\n");
            let mut qs = by_chunk_arm[&(c.chunk_id.as_str(), arm.as_str())].clone();
            qs.sort_by_key(|m| m.qtype.clone());
            for m in qs {
                let r = rank_of.get(m.qid.as_str());
                let tag = r.map(|r| format!("{}/{}/{} {}", r.chunk_rank, r.within_rank, r.article_rank, if keep(r) { "✓" } else { "✗" })).unwrap_or_default();
                let _ = writeln!(md, "- {}: {} ({})", m.qtype, m.text, tag);
            }
            md.push('\n');
        }
    }
    // blind copy: arms relabelled by a hash order, key kept separately
    let mut order = arms.clone();
    order.sort_by_key(|a| hash(&["blind", a]));
    let label: HashMap<String, String> = order.iter().enumerate().map(|(i, a)| (a.clone(), format!("System {}", (b'A' + i as u8) as char))).collect();
    let mut blind = md.clone();
    let mut by_len = arms.clone();
    by_len.sort_by_key(|a| std::cmp::Reverse(a.len()));
    for a in &by_len {
        blind = blind.replace(&format!("**{a}**"), &format!("**{}**", label[a]));
    }
    // drop ranks from the blind copy so the reader judges the text only
    let rank_re = Regex::new(r" \(\d+/\d+/\d+ [✓✗]\)").unwrap();
    let blind = rank_re.replace_all(&blind, "").replace("Ranks: chunk rank in corpus / chunk rank in article / article rank. ✓ = kept.\n", "");
    let key = json!(label);
    (report, md, blind.to_string(), key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_helpers() {
        let p: HashSet<String> = tokens("The dose of drug X was 5 mg daily in adults").into_iter().collect();
        assert!(overlap("dose drug daily", &p) > 0.99);
        assert!(names_missing_context("What dose of metformin is used daily?", "Metformin in obese adults", &p));
        assert!(!names_missing_context("What dose is used daily?", "Metformin in obese adults", &p));
        let w: Vec<String> = "a b c d e f g h i".split(' ').map(String::from).collect();
        assert!(copies("x a b c d e f g h", &w));
        assert!(!copies("a b c", &w));
        let r = Ranks { qid: "q".into(), chunk_rank: 50, article_rank: 2, within_rank: 1, score: 0.5 };
        assert!(keep(&r));
    }
}
