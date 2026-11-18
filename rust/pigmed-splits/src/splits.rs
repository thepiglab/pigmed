//! Leakage-safe paper-group splits (docs/RETRIEVER_DATA_PLAN.md, "PMC splits").
//!
//! 1. identity graph: union-find over PMCID / PMID / DOI nodes plus JATS related-article identity links (a
//!    high-degree endpoint is a mass notice, not a version, and is skipped);
//! 2. near-duplicate merge: article links from e-means E3 and MinHash (notices never join by text);
//! 3. assignment: eval_reserved > test_temporal > test_journal > hash test/dev/train;
//! 4. pair rule: training needs both endpoints in train; an eval pair takes its query's split;
//! 5. audit: an independent re-check of a manifest.

use crate::ids::{article_group, commercial_use, id_node, stable_id, unit_hash};
use crate::record::Record;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// related-article types that mean "the same paper" (versions) or "a notice about it", matched as substrings of
/// the lowercased type. Others (commentary, reply, companion, peer review, series) are different papers.
const IDENTITY_MARKERS: [&str; 15] = ["correct", "retract", "concern", "addend", "errat", "withdraw", "preprint",
    "published", "update", "republish", "version", "translat", "alt-language", "duplicate", "expression"];

/// Notices are short template text; they join their targets through identity links only.
pub const NOTICE_TYPES: [&str; 6] = ["retraction", "correction", "expression-of-concern", "addendum", "removal", "erratum"];

const SPLIT_NAMES: [&str; 6] = ["eval_reserved", "test_temporal", "test_journal", "test", "dev", "train"];

pub fn is_identity_link(kind: Option<&str>) -> bool {
    let t = kind.unwrap_or("").to_lowercase();
    IDENTITY_MARKERS.iter().any(|m| t.contains(m))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SplitConfig {
    pub seed: String,
    pub train_fraction: f64,
    pub dev_fraction: f64,
    pub temporal_cutoff: String,
    pub test_journals: usize,
    pub test_journal_share: f64,
    pub min_journal_articles: usize,
    pub max_journal_share: f64,
    pub journal_seed: String,
    pub max_group_articles: usize,
    pub max_link_degree: usize,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            seed: "pigmed-v1".into(),
            train_fraction: 0.96,
            dev_fraction: 0.02,
            temporal_cutoff: "2026-07-01".into(),
            test_journals: 50,
            test_journal_share: 0.02,
            min_journal_articles: 10,
            max_journal_share: 0.005,
            journal_seed: "pigmed-journal-v1".into(),
            max_group_articles: 200,
            max_link_degree: 5,
        }
    }
}

/// Same hash as evaluation.assign_split, so a singleton article keeps its earlier split.
pub fn hash_split(key: &str, c: &SplitConfig) -> &'static str {
    let v = unit_hash(&[&c.seed, key]);
    if v < c.train_fraction {
        "train"
    } else if v < c.train_fraction + c.dev_fraction {
        "dev"
    } else {
        "test"
    }
}

/// Held-out strength, strongest first; used when a frozen previous manifest pins a group.
fn heldout_rank(s: &str) -> u8 {
    match s {
        "eval_reserved" => 0,
        "test_temporal" => 1,
        "test_journal" => 2,
        "test" => 3,
        "dev" => 4,
        _ => 5,
    }
}

fn hash_rank(s: &str) -> u8 {
    match s {
        "test" => 0,
        "dev" => 1,
        _ => 2,
    }
}

// ------------------------------------------------------------------------------------------------ union-find

#[derive(Default)]
pub struct UnionFind {
    index: HashMap<String, usize>,
    names: Vec<String>,
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl UnionFind {
    pub fn add(&mut self, x: &str) -> usize {
        if let Some(&i) = self.index.get(x) {
            return i;
        }
        let i = self.names.len();
        self.index.insert(x.to_string(), i);
        self.names.push(x.to_string());
        self.parent.push(i);
        self.size.push(1);
        i
    }

    pub fn find_idx(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    pub fn union(&mut self, a: &str, b: &str) {
        let (ia, ib) = (self.add(a), self.add(b));
        let (mut ra, mut rb) = (self.find_idx(ia), self.find_idx(ib));
        if ra == rb {
            return;
        }
        if self.size[ra] < self.size[rb] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb] = ra;
        self.size[ra] += self.size[rb];
    }

    pub fn components(&mut self) -> Vec<Vec<String>> {
        let mut by_root: HashMap<usize, Vec<String>> = HashMap::new();
        for i in 0..self.names.len() {
            let r = self.find_idx(i);
            by_root.entry(r).or_default().push(self.names[i].clone());
        }
        by_root.into_values().collect()
    }
}

// ------------------------------------------------------------------------------------------------ journals

fn collapse_lower(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// article_id -> journal key: ISSNs and the NLM TA of one article are unioned, so a journal listed by eISSN in
/// one article and pISSN in another is one journal. Key = smallest member node.
pub fn journal_keys(records: &[Record]) -> HashMap<String, String> {
    let mut uf = UnionFind::default();
    let mut per: Vec<(String, Vec<String>)> = vec![];
    for r in records {
        let mut nodes: Vec<String> = r.issns.iter().filter(|x| !x.is_empty()).map(|x| format!("issn:{}", x.to_uppercase())).collect();
        if !r.nlm_ta.trim().is_empty() {
            nodes.push(format!("nlm:{}", collapse_lower(&r.nlm_ta)));
        }
        if nodes.is_empty() && !r.journal.trim().is_empty() {
            nodes.push(format!("name:{}", collapse_lower(&r.journal)));
        }
        for n in &nodes {
            uf.add(n);
        }
        for n in nodes.iter().skip(1) {
            uf.union(&nodes[0], n);
        }
        per.push((r.article_id.clone(), nodes));
    }
    let mut root_key: HashMap<usize, String> = HashMap::new();
    for comp in uf.components() {
        let key = comp.iter().min().unwrap().clone();
        let root = { let i = uf.add(&comp[0]); uf.find_idx(i) };
        root_key.insert(root, key);
    }
    per.into_iter()
        .filter(|(_, n)| !n.is_empty())
        .map(|(a, n)| {
            let i = uf.add(&n[0]);
            let r = uf.find_idx(i);
            (a, root_key[&r].clone())
        })
        .collect()
}

/// Hash-ordered journals, round-robin across strata (subject if given, else log2 size bucket), until `n`
/// journals or `target_share` of the corpus is held out.
pub fn select_test_journals(counts: &HashMap<String, usize>, n: usize, min_articles: usize, max_share: f64, seed: &str,
                            strata: Option<&HashMap<String, String>>, target_share: f64) -> BTreeSet<String> {
    let total = counts.values().sum::<usize>().max(1) as f64;
    let mut eligible: Vec<(&String, String)> = counts
        .iter()
        .filter(|(_, &c)| c >= min_articles && c as f64 / total <= max_share)
        .map(|(j, _)| (j, stable_id(&[seed, j])))
        .collect();
    eligible.sort_by(|a, b| a.1.cmp(&b.1));
    let mut by_stratum: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for (j, _) in eligible {
        let stratum = strata
            .and_then(|s| s.get(j).cloned())
            .unwrap_or_else(|| format!("size-2^{}", (counts[j] as f64).log2().floor() as i64));
        by_stratum.entry(stratum).or_default().push(j);
    }
    let mut queues: Vec<std::collections::VecDeque<&String>> = by_stratum.into_values().map(Into::into).collect();
    let (mut chosen, mut held) = (BTreeSet::new(), 0usize);
    while chosen.len() < n && (held as f64) < target_share * total && queues.iter().any(|q| !q.is_empty()) {
        for q in queues.iter_mut() {
            if chosen.len() < n && (held as f64) < target_share * total {
                if let Some(j) = q.pop_front() {
                    held += counts[j];
                    chosen.insert(j.clone());
                }
            }
        }
    }
    chosen
}

// ------------------------------------------------------------------------------------------------ build

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ArticleRow {
    pub article_id: String,
    pub group: String,
    pub split: String,
    pub journal: Option<String>,
    pub group_size: usize,
    pub retracted: bool,
    #[serde(default)]
    pub article_type: String,
    pub commercial: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GroupRow {
    pub split: String,
    pub articles: Vec<String>,
    pub n_ids: usize,
    pub earliest: Option<String>,
    pub retracted: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub blocked_by: Vec<String>,
}

pub struct Built {
    pub config: SplitConfig,
    pub articles: BTreeMap<String, ArticleRow>,
    pub nodes: HashMap<String, String>,
    pub groups: BTreeMap<String, GroupRow>,
    pub test_journals: BTreeSet<String>,
    pub oversized_groups: Vec<(usize, String)>,
    pub related_link_types: BTreeMap<String, usize>,
    pub stats: BTreeMap<String, usize>,
}

impl Built {
    pub fn split_of(&self, x: &str) -> Option<&str> {
        let gid = self.nodes.get(x).or_else(|| self.nodes.get(&format!("article:{x}")))?;
        self.groups.get(gid).map(|g| g.split.as_str())
    }

    pub fn split_counts(&self) -> BTreeMap<String, BTreeMap<String, usize>> {
        let mut out: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        for a in self.articles.values() {
            let c = out.entry(a.split.clone()).or_default();
            *c.entry("articles".into()).or_default() += 1;
            *c.entry("commercial".into()).or_default() += a.commercial as usize;
            *c.entry("retracted".into()).or_default() += a.retracted as usize;
        }
        for g in self.groups.values().filter(|g| !g.articles.is_empty()) {
            *out.entry(g.split.clone()).or_default().entry("groups".into()).or_default() += 1;
        }
        out
    }
}

pub type Link = (String, String, String);

/// `previous`: article_id -> split of an earlier, already-used manifest. Held-out is sticky: a group containing
/// an article that was held out before keeps the strongest such split, so merges and new links can never make
/// it trainable.
pub fn build_splits(records: &[Record], config: SplitConfig, dup_links: &[Link], blocklist: &[String],
                    external_nodes: &HashSet<String>, journal_strata: Option<&HashMap<String, String>>,
                    previous: Option<&HashMap<String, String>>) -> Result<Built> {
    let mut uf = UnionFind::default();
    let mut stats: BTreeMap<String, usize> = BTreeMap::new();
    let mut link_types: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_id: HashMap<&str, &Record> = HashMap::new();
    let mut identity_edges: Vec<(String, String)> = vec![];
    let bump = |stats: &mut BTreeMap<String, usize>, k: &str| *stats.entry(k.to_string()).or_default() += 1;
    for r in records {
        if by_id.insert(&r.article_id, r).is_some() {
            bail!("duplicate article_id {}", r.article_id);
        }
        let anode = format!("article:{}", r.article_id);
        uf.add(&anode);
        for n in r.nodes() {
            uf.union(&anode, &n);
        }
        for l in &r.related {
            *link_types.entry(l.kind.clone().unwrap_or_else(|| "?".into())).or_default() += 1;
            let node = id_node(&l.id_type, &l.id);
            if node.is_empty() {
                bump(&mut stats, "related_unresolvable");
            } else if is_identity_link(l.kind.as_deref()) {
                identity_edges.push((anode.clone(), node));
            } else {
                bump(&mut stats, "related_soft_links");
            }
        }
    }
    // A mass retraction/correction notice links dozens of unrelated papers (and each links back to it).
    let own: HashMap<String, String> =
        records.iter().flat_map(|r| r.nodes().into_iter().map(move |n| (n, format!("article:{}", r.article_id)))).collect();
    let target = |n: &String| own.get(n).cloned().unwrap_or_else(|| n.clone());
    let mut degree: HashMap<String, HashSet<String>> = HashMap::new();
    for (a, n) in &identity_edges {
        let t = target(n);
        degree.entry(a.clone()).or_default().insert(t.clone());
        degree.entry(t).or_default().insert(a.clone());
    }
    for (a, n) in &identity_edges {
        if degree[a].len() > config.max_link_degree || degree[&target(n)].len() > config.max_link_degree {
            bump(&mut stats, "related_mass_notice_links");
            continue;
        }
        uf.union(a, n);
        bump(&mut stats, "related_identity_links");
    }
    for (a, b, source) in dup_links {
        let (Some(ra), Some(rb)) = (by_id.get(a.as_str()), by_id.get(b.as_str())) else { continue };
        if a == b {
            continue;
        }
        if NOTICE_TYPES.contains(&ra.article_type.as_str()) || NOTICE_TYPES.contains(&rb.article_type.as_str()) {
            bump(&mut stats, "dup_links_skipped_notice");
            continue;
        }
        uf.union(&format!("article:{a}"), &format!("article:{b}"));
        bump(&mut stats, &format!("dup_links_{source}"));
    }
    for n in external_nodes.iter().filter(|n| !n.is_empty()) {
        uf.add(n);
    }
    let blocked: HashSet<&String> = blocklist.iter().filter(|n| !n.is_empty()).collect();
    for n in &blocked {
        uf.add(n);
    }

    let jkeys = journal_keys(records);
    let mut jcounts: HashMap<String, usize> = HashMap::new();
    for k in jkeys.values() {
        *jcounts.entry(k.clone()).or_default() += 1;
    }
    let test_journals = select_test_journals(&jcounts, config.test_journals, config.min_journal_articles,
        config.max_journal_share, &config.journal_seed, journal_strata, config.test_journal_share);

    let mut groups = BTreeMap::new();
    let mut nodes = HashMap::new();
    for members in uf.components() {
        let mut arts: Vec<String> = members.iter().filter_map(|m| m.strip_prefix("article:").map(str::to_string)).collect();
        let mut ids: Vec<String> = members.iter().filter(|m| !m.starts_with("article:")).cloned().collect();
        arts.sort();
        ids.sort();
        let key = arts.first().or(ids.first()).unwrap().clone();
        let gid = format!("g:{key}");
        let earliest = arts.iter().flat_map(|a| by_id[a.as_str()].dates.iter()).filter(|d| !d.is_empty()).min().cloned();
        let mut blocked_by: Vec<String> = ids.iter().filter(|i| blocked.contains(i)).cloned().collect();
        blocked_by.sort();
        let split = if !blocked_by.is_empty() {
            "eval_reserved"
        } else if earliest.as_deref().is_some_and(|e| e >= config.temporal_cutoff.as_str()) {
            "test_temporal"
        } else if arts.iter().any(|a| jkeys.get(a).is_some_and(|j| test_journals.contains(j))) {
            "test_journal"
        } else {
            let keys: Vec<String> = if arts.is_empty() {
                vec![ids[0].clone()]
            } else {
                arts.iter().map(|a| { let r = by_id[a.as_str()]; article_group(&r.pmcid, &r.doi, &r.pmid, &r.article_id) }).collect()
            };
            keys.iter().map(|k| hash_split(k, &config)).min_by_key(|s| hash_rank(s)).unwrap()
        };
        let pinned = previous.and_then(|p| arts.iter().filter_map(|a| p.get(a)).map(String::as_str)
            .filter(|s| *s != "train").min_by_key(|s| heldout_rank(s)));
        let split = match pinned {
            Some(p) if heldout_rank(p) < heldout_rank(split) => {
                *stats.entry("sticky_heldout_overrides".into()).or_default() += 1;
                SPLIT_NAMES.iter().copied().find(|s| *s == p).unwrap_or("test")
            }
            _ => split,
        };
        let retracted = arts.iter().any(|a| by_id[a.as_str()].is_retracted_or_notice());
        for m in &members {
            nodes.insert(m.clone(), gid.clone());
        }
        groups.insert(gid, GroupRow { split: split.into(), articles: arts, n_ids: ids.len(), earliest, retracted, blocked_by });
    }
    let mut articles = BTreeMap::new();
    for r in records {
        let gid = nodes[&format!("article:{}", r.article_id)].clone();
        let g = &groups[&gid];
        articles.insert(r.article_id.clone(), ArticleRow {
            article_id: r.article_id.clone(), group: gid.clone(), split: g.split.clone(),
            journal: jkeys.get(&r.article_id).cloned(), group_size: g.articles.len(), retracted: g.retracted,
            article_type: r.article_type.clone(), commercial: commercial_use(r.license_code.as_deref()),
        });
    }
    let mut sizes: BTreeMap<usize, usize> = BTreeMap::new();
    for g in groups.values().filter(|g| !g.articles.is_empty()) {
        *sizes.entry(g.articles.len()).or_default() += 1;
    }
    let mut oversized: Vec<(usize, String)> = groups.iter()
        .filter(|(_, g)| g.articles.len() > config.max_group_articles)
        .map(|(k, g)| (g.articles.len(), k.clone()))
        .collect();
    oversized.sort_by(|a, b| b.cmp(a));
    oversized.truncate(20);
    stats.insert("articles".into(), records.len());
    stats.insert("groups_with_articles".into(), sizes.values().sum());
    stats.insert("multi_article_groups".into(), sizes.iter().filter(|(s, _)| **s > 1).map(|(_, c)| c).sum());
    stats.insert("largest_group".into(), sizes.keys().max().copied().unwrap_or(0));
    stats.insert("journals".into(), jcounts.len());
    stats.insert("test_journals".into(), test_journals.len());
    Ok(Built { config, articles, nodes, groups, test_journals, oversized_groups: oversized, related_link_types: link_types, stats })
}

// ------------------------------------------------------------------------------------------------ pairs

/// Training needs both endpoints in train. An eval pair takes its query's split (the eval corpus holds every
/// split, so a positive may be a train document). eval_reserved queries are never used by us.
pub fn pair_split<'a>(q: Option<&'a str>, p: Option<&str>) -> Option<&'a str> {
    let (q, p) = (q?, p?);
    match q {
        "train" => (p == "train").then_some("train"),
        "eval_reserved" => None,
        other => Some(other),
    }
}

/// (query, positive) endpoints (article ids or typed nodes) -> split label. Unknown endpoints get the hash
/// split of their own node, which is what they would get as singletons.
pub fn label_pair(built: &Built, q: &str, p: &str) -> Option<String> {
    let sq = built.split_of(q).unwrap_or_else(|| hash_split(q, &built.config)).to_string();
    let sp = built.split_of(p).unwrap_or_else(|| hash_split(p, &built.config)).to_string();
    pair_split(Some(&sq), Some(&sp)).map(str::to_string)
}

// ------------------------------------------------------------------------------------------------ audit

#[derive(Serialize, Debug, Default)]
pub struct AuditReport {
    pub ok: bool,
    pub failures: BTreeMap<String, usize>,
    pub examples: BTreeMap<String, Vec<Value>>,
    pub warnings: BTreeMap<String, Value>,
    pub pairs: BTreeMap<String, usize>,
    pub moved_since_previous: BTreeMap<String, usize>,
    pub split_counts: BTreeMap<String, BTreeMap<String, usize>>,
}

impl AuditReport {
    pub fn fail(&mut self, kind: &str, example: Value) {
        *self.failures.entry(kind.into()).or_default() += 1;
        let e = self.examples.entry(kind.into()).or_default();
        if e.len() < 5 {
            e.push(example);
        }
    }
}

/// Count one labelled (query, positive) pair; a train pair must not touch a held-out endpoint.
pub fn audit_pair(rep: &mut AuditReport, built: &Built, q: &str, p: &str, s: Option<&str>) {
    *rep.pairs.entry(s.unwrap_or("dropped").to_string()).or_default() += 1;
    if s == Some("train") {
        for end in [q, p] {
            if built.split_of(end).is_some_and(|x| x != "train") {
                rep.fail("train_pair_touches_heldout", json!([q, p]));
            }
        }
    }
}

/// pairs: (query, positive, label). loose_links: candidate near-dup links that were not used for grouping; their
/// train/held-out crossings are a warning (the residual-leakage estimate).
pub fn audit(built: &Built, pairs: &[(String, String, Option<String>)], dup_links: &[Link], blocklist: &[String],
             previous: Option<&HashMap<String, String>>, loose_links: &[Link]) -> AuditReport {
    let mut rep = AuditReport::default();
    let mut seen: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for (aid, a) in &built.articles {
        seen.entry(&a.group).or_default().insert(&a.split);
        if built.groups.get(&a.group).map(|g| g.split.as_str()) != Some(a.split.as_str()) {
            rep.fail("article_split_differs_from_group", json!(aid));
        }
    }
    for (gid, s) in &seen {
        if s.len() > 1 {
            rep.fail("group_spans_splits", json!([gid, s]));
        }
    }
    for (q, p, s) in pairs {
        audit_pair(&mut rep, built, q, p, s.as_deref());
    }
    for n in blocklist {
        if let Some(s) = built.split_of(n) {
            if s != "eval_reserved" {
                rep.fail("blocklisted_not_reserved", json!([n, s]));
            }
        }
    }
    let notice: HashSet<&str> = built.articles.iter()
        .filter(|(_, a)| NOTICE_TYPES.contains(&a.article_type.as_str()))
        .map(|(k, _)| k.as_str())
        .collect();
    for (a, b, source) in dup_links {
        if notice.contains(a.as_str()) || notice.contains(b.as_str()) {
            *rep.pairs.entry("dup_links_notice_ignored".into()).or_default() += 1;
            continue;
        }
        if let (Some(sa), Some(sb)) = (built.split_of(a), built.split_of(b)) {
            if sa != sb {
                rep.fail("dup_link_crosses_splits", json!([a, b, source, sa, sb]));
            }
        }
    }
    if let Some(prev) = previous {
        for (aid, old) in prev {
            if let Some(new) = built.articles.get(aid).map(|a| a.split.as_str()) {
                if new != old {
                    *rep.moved_since_previous.entry(format!("{old}->{new}")).or_default() += 1;
                    if old != "train" && new == "train" {
                        rep.fail("heldout_became_train", json!([aid, old]));
                    }
                }
            }
        }
    }
    let (mut loose_pairs, mut loose_cross) = (0usize, 0usize);
    for (a, b, _) in loose_links {
        if let (Some(sa), Some(sb)) = (built.split_of(a), built.split_of(b)) {
            loose_pairs += 1;
            loose_cross += ((sa == "train") != (sb == "train")) as usize;
        }
    }
    if !built.oversized_groups.is_empty() {
        rep.warnings.insert("oversized_groups".into(), json!(built.oversized_groups));
    }
    if loose_pairs > 0 {
        rep.warnings.insert("loose_links".into(), json!({"pairs": loose_pairs, "train_vs_heldout": loose_cross}));
    }
    rep.ok = rep.failures.is_empty();
    rep.split_counts = built.split_counts();
    rep
}

// ------------------------------------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Related;

    fn rec(aid: &str) -> Record {
        Record { article_id: aid.into(), pmcid: aid.split('.').next().unwrap().into(), ..Default::default() }
    }
    fn cfg0() -> SplitConfig {
        SplitConfig { test_journals: 0, temporal_cutoff: "9999-01-01".into(), ..Default::default() }
    }
    fn link(kind: &str, t: &str, id: &str) -> Related {
        Related { kind: Some(kind.into()), id_type: t.into(), id: id.into() }
    }
    fn first_with(split: &str, c: &SplitConfig) -> String {
        (1..5000).map(|i| format!("PMC{i}")).find(|k| hash_split(k, c) == split).unwrap()
    }
    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn hash_split_matches_python_assign_split() {
        let c = SplitConfig::default();
        for line in include_str!("../tests/py_hash_splits.tsv").lines() {
            let (k, s) = line.split_once('\t').unwrap();
            assert_eq!(hash_split(k, &c), s, "{k}");
        }
    }

    #[test]
    fn identity_graph_merges_ids() {
        let mut a = rec("PMC1.1");
        a.doi = "10.1000/a".into();
        a.pmid = "11".into();
        let b = Record { article_id: "X2".into(), pmid: "11".into(), ..Default::default() };
        let mut c = rec("PMC3.1");
        c.doi = "10.1000/c".into();
        let built = build_splits(&[a, b, c], cfg0(), &[], &[], &none(), None, None).unwrap();
        assert_eq!(built.articles["PMC1.1"].group, built.articles["X2"].group);
        assert_ne!(built.articles["PMC3.1"].group, built.articles["PMC1.1"].group);
        assert_eq!(built.split_of("doi:10.1000/a"), built.split_of("pmid:11"));
    }

    #[test]
    fn related_identity_vs_soft_links_and_mass_notices() {
        let mut a = rec("PMC1.1");
        a.doi = "10.1000/a".into();
        let mut corr = rec("PMC2.1");
        corr.related = vec![link("corrected-article", "doi", "10.1000/A")];
        let mut comm = rec("PMC3.1");
        comm.related = vec![link("commentary-article", "doi", "10.1000/a")];
        let mut pre = rec("PMC4.1");
        pre.related = vec![link("preprint", "pmcid", "PMC1")];
        let mut rows = vec![a, corr, comm, pre];
        let mut notice = rec("PMC100.1");
        notice.doi = "10.1000/notice".into();
        notice.article_type = "retraction".into();
        notice.related = (10..17).map(|i| link("retracted-article", "pmcid", &format!("PMC{i}"))).collect();
        rows.push(notice);
        for i in 10..17 {
            let mut t = rec(&format!("PMC{i}.1"));
            t.related = vec![link("retraction-forward", "doi", "10.1000/notice")];
            rows.push(t);
        }
        let b = build_splits(&rows, cfg0(), &[], &[], &none(), None, None).unwrap();
        let g = &b.articles["PMC1.1"].group;
        assert_eq!(&b.articles["PMC2.1"].group, g);
        assert_eq!(&b.articles["PMC4.1"].group, g);
        assert_ne!(&b.articles["PMC3.1"].group, g);
        let targets: HashSet<&String> = (10..17).map(|i| &b.articles[&format!("PMC{i}.1")].group).collect();
        assert_eq!(targets.len(), 7);
        assert_eq!(b.stats["related_identity_links"], 2);
        assert_eq!(b.stats["related_mass_notice_links"], 14);
        assert_eq!(b.stats["related_soft_links"], 1);
    }

    #[test]
    fn singletons_keep_legacy_split_and_merges_take_most_heldout() {
        let c = cfg0();
        let rows: Vec<Record> = (1..300).map(|i| rec(&format!("PMC{i}.1"))).collect();
        let b = build_splits(&rows, c.clone(), &[], &[], &none(), None, None).unwrap();
        assert!(rows.iter().all(|r| b.articles[&r.article_id].split == hash_split(&r.pmcid, &c)));
        let (te, tr) = (first_with("test", &c), first_with("train", &c));
        let rows = vec![rec(&format!("{te}.1")), rec(&format!("{tr}.1"))];
        let l = vec![(format!("{te}.1"), format!("{tr}.1"), "minhash".to_string())];
        let b = build_splits(&rows, c, &l, &[], &none(), None, None).unwrap();
        assert!(b.articles.values().all(|a| a.split == "test"));
    }

    #[test]
    fn previous_heldout_is_sticky() {
        let c = cfg0();
        let (te, tr) = (format!("{}.1", first_with("test", &c)), format!("{}.1", first_with("train", &c)));
        let rows = vec![rec(&te), rec(&tr)];
        let prev: HashMap<String, String> = [(tr.clone(), "test_temporal".to_string())].into();
        let b = build_splits(&rows, c.clone(), &[], &[], &none(), None, Some(&prev)).unwrap();
        assert_eq!(b.articles[&tr].split, "test_temporal");
        assert_eq!(b.articles[&te].split, "test");
        assert_eq!(b.stats["sticky_heldout_overrides"], 1);
        let l = vec![(te.clone(), tr.clone(), "e3".to_string())];
        let b = build_splits(&rows, c, &l, &[], &none(), None, Some(&prev)).unwrap();
        assert!(b.articles.values().all(|a| a.split == "test_temporal"));
    }

    #[test]
    fn notices_do_not_join_by_text() {
        let mut a = rec("PMC1.1");
        a.article_type = "retraction".into();
        let mut b = rec("PMC2.1");
        b.article_type = "retraction".into();
        let l = vec![("PMC1.1".into(), "PMC2.1".into(), "minhash".into()), ("PMC2.1".into(), "PMC3.1".into(), "minhash".into())];
        let built = build_splits(&[a, b, rec("PMC3.1")], cfg0(), &l, &[], &none(), None, None).unwrap();
        assert_eq!(built.articles.values().map(|a| &a.group).collect::<HashSet<_>>().len(), 3);
        assert_eq!(built.stats["dup_links_skipped_notice"], 2);
        let rep = audit(&built, &[], &l, &[], None, &[]);
        assert!(rep.ok);
        assert_eq!(rep.pairs["dup_links_notice_ignored"], 2);
    }

    #[test]
    fn assignment_priority() {
        let c = SplitConfig { test_journals: 1, min_journal_articles: 2, max_journal_share: 1.0, test_journal_share: 1.0, ..Default::default() };
        let mk = |aid: &str, pmid: &str, date: &str, issns: &[&str]| {
            let mut r = rec(aid);
            r.pmid = pmid.into();
            r.dates = vec![date.into()];
            r.issns = issns.iter().map(|s| s.to_string()).collect();
            r
        };
        let rows = vec![mk("PMC1.1", "1", "2020-01-01", &[]), mk("PMC2.1", "", "2026-08-01", &[]),
            mk("PMC3.1", "3", "2026-08-01", &[]), mk("PMC4.1", "", "2019-01-01", &["1234-5678"]),
            mk("PMC5.1", "", "2019-01-01", &["1234-5678", "8765-4321"]), mk("PMC6.1", "", "2019-01-01", &["8765-4321"])];
        let b = build_splits(&rows, c, &[], &["pmid:1".into(), "pmid:3".into()], &none(), None, None).unwrap();
        let s = |a: &str| b.articles[a].split.as_str();
        assert_eq!((s("PMC1.1"), s("PMC3.1"), s("PMC2.1")), ("eval_reserved", "eval_reserved", "test_temporal"));
        assert_eq!((s("PMC4.1"), s("PMC5.1"), s("PMC6.1")), ("test_journal", "test_journal", "test_journal"));
    }

    #[test]
    fn journal_selection() {
        let counts: HashMap<String, usize> = [5, 50, 60, 70, 80, 900, 3000, 12, 15, 40].iter().enumerate()
            .map(|(i, c)| (format!("j{i}"), *c)).collect();
        let chosen = select_test_journals(&counts, 4, 10, 0.2, "pigmed-journal-v1", None, 1.0);
        assert_eq!(chosen.len(), 4);
        assert!(!chosen.contains("j0") && !chosen.contains("j6"));
        let even: HashMap<String, usize> = (0..100).map(|i| (format!("j{i}"), 100)).collect();
        assert_eq!(select_test_journals(&even, 50, 10, 0.5, "s", None, 0.05).len(), 5);
    }

    #[test]
    fn pair_rule_and_audit_catches_leaks() {
        assert_eq!(pair_split(Some("train"), Some("train")), Some("train"));
        assert_eq!(pair_split(Some("train"), Some("test")), None);
        assert_eq!(pair_split(Some("test"), Some("train")), Some("test"));
        assert_eq!(pair_split(Some("eval_reserved"), Some("train")), None);
        let c = cfg0();
        let (tr, te) = (format!("{}.1", first_with("train", &c)), format!("{}.1", first_with("test", &c)));
        let mut a = rec(&tr);
        a.pmid = "7".into();
        let b = build_splits(&[a, rec(&te)], c, &[], &[], &none(), None, None).unwrap();
        let prev: HashMap<String, String> = [(tr.clone(), "test".to_string()), (te.clone(), "test".to_string())].into();
        let rep = audit(&b, &[(tr.clone(), te.clone(), Some("train".into()))], &[(tr.clone(), te.clone(), "e3".into())],
                        &["pmid:7".into()], Some(&prev), &[(tr.clone(), te.clone(), "loose".into())]);
        assert!(!rep.ok);
        let kinds: BTreeSet<&str> = rep.failures.keys().map(String::as_str).collect();
        assert_eq!(kinds, ["blocklisted_not_reserved", "dup_link_crosses_splits", "heldout_became_train", "train_pair_touches_heldout"].into());
        assert_eq!(rep.warnings["loose_links"], json!({"pairs": 1, "train_vs_heldout": 1}));
    }
}
