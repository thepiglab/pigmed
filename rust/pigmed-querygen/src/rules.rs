//! Filters (card drops, the query reject chain), true-by-construction instruction constraints and typing noise, all
//! compiled from the config. Behaviour matches pigmed-synth v3.1d with querygen/v3.1d.toml (tests/parity.rs).

use crate::config::{Config, Noise, Style};
use crate::persona::Persona;
use crate::util::{hash, jaccard};
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Article as written by pigmed-synth `sample` / `annotate-articles` (passages marked [[Pk]] ... [[/Pk]]).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Article {
    pub article_id: String,
    pub title: String,
    pub text: String,
    pub words: usize,
    pub passages: Vec<String>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub article_type: String,
    #[serde(default)]
    pub pub_types: Vec<String>,
    #[serde(default)]
    pub license_code: String,
    #[serde(default)]
    pub commercial: Option<bool>,
}

/// `null` (or a missing field) reads as the type's default.
fn nullable<'de, D: serde::Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Per-article metadata for v3.1e (domain, patient relevance, field -> role): MeSH tree numbers and journal, as built
/// by pigmed-ctx `article-mesh`: {article_id, pmid, mesh_trees, journal, nlm_id, pub_types}; nulls allowed, other
/// fields (pmid) kept for provenance only.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ArticleMeta {
    pub article_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub pmid: String,
    #[serde(default, deserialize_with = "nullable")]
    pub mesh_trees: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub journal: String,
    #[serde(default, deserialize_with = "nullable")]
    pub nlm_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub pub_types: Vec<String>,
    /// distinct tree numbers that came from retired MeSH descriptors (older descYYYY years); provenance only
    #[serde(default, deserialize_with = "nullable")]
    pub retired_trees: u32,
}

fn tree_hit(trees: &[String], prefixes: &[String]) -> bool {
    trees.iter().any(|t| prefixes.iter().any(|p| t.starts_with(p.as_str())))
}

/// v3.1e domain rule. MeSH decides when the article has MeSH (journal list overrides to biomedical); without MeSH the
/// LLM flag OR the journal list ("llm_or_journal"), or the journal list alone ("journal"). No [domain]: the LLM flag.
pub fn biomedical(c: &Config, meta: Option<&ArticleMeta>, llm: bool) -> bool {
    let Some(d) = &c.domain else { return llm };
    let journal = meta.is_some_and(|m| (!m.journal.is_empty() && c.journals.contains(&m.journal.to_lowercase()))
        || (!m.nlm_id.is_empty() && c.journals.contains(&m.nlm_id.to_lowercase())));
    match meta.filter(|m| !m.mesh_trees.is_empty()) {
        Some(m) => {
            let top = |t: &String| t.chars().next().unwrap_or(' ');
            let other = |p: &String| m.mesh_trees.iter().any(|t| !t.starts_with(p.as_str()) && d.trees_with_other.iter().all(|q| top(t) != q.chars().next().unwrap_or(' ')));
            journal || tree_hit(&m.mesh_trees, &d.trees_any)
                || d.trees_with_other.iter().any(|p| m.mesh_trees.iter().any(|t| t.starts_with(p.as_str())) && other(p))
        }
        None => if d.no_mesh == "journal" { journal } else { llm || journal },
    }
}

/// v3.1e lay pass: patient-relevant articles (MeSH C/D + a clinical publication type, or N02/N05).
pub fn patient_relevant(c: &Config, meta: Option<&ArticleMeta>, art: &Article) -> bool {
    let (Some(l), Some(m)) = (&c.lay, meta) else { return false };
    if !l.enabled || m.mesh_trees.is_empty() {
        return false;
    }
    let pts: Vec<String> = if m.pub_types.is_empty() { &art.pub_types } else { &m.pub_types }.iter().map(|x| x.to_lowercase()).collect();
    (tree_hit(&m.mesh_trees, &l.patient_trees) && pts.iter().any(|p| l.clinical_pub_types.iter().any(|c| p == c)))
        || tree_hit(&m.mesh_trees, &l.trees_alone)
}

/// v3.1e field -> role: journal substring first, then the longest matching MeSH tree prefix.
pub fn field_role(c: &Config, meta: Option<&ArticleMeta>) -> Option<String> {
    let (f, m) = (c.fallback.as_ref()?, meta?);
    let j = m.journal.to_lowercase();
    if let Some((_, r)) = f.journal_roles.iter().find(|(k, _)| !j.is_empty() && j.contains(k.as_str())) {
        return Some(r.clone());
    }
    f.mesh_roles.iter().filter(|(p, _)| m.mesh_trees.iter().any(|t| t.starts_with(p.as_str()))).max_by_key(|(p, _)| p.len()).map(|(_, r)| r.clone())
}

pub struct Rules {
    leak: Regex,
    doc_ref: Regex,
    doc_ref_fig: Regex,
    doc_ref_group: Regex,
    exam: Regex,
    tex: Regex,
    agenda: Regex,
    part: Regex,
    code: Regex,
    they: Regex,
    this: Regex,
    the: Regex,
    titles: Vec<(String, Regex)>,
    pub cfg_min_words: usize,
}

fn re(s: &str, name: &str) -> Result<Regex> {
    Regex::new(s).with_context(|| format!("regex {name}"))
}

/// What a query is checked against.
pub struct QueryCtx<'a> {
    pub need: &'a str,
    pub style: &'a Style,
    /// figure arm: the caption (caption_copy) and the figure-specific doc_ref
    pub caption: Option<&'a str>,
}

impl Rules {
    pub fn new(c: &Config) -> Result<Rules> {
        let r = &c.filters.regex;
        let fig = c.filters.figure.as_ref().map(|f| f.doc_ref.as_str()).unwrap_or(&r.doc_ref);
        Ok(Rules {
            leak: re(&r.leak, "leak")?, doc_ref: re(&r.doc_ref, "doc_ref")?, doc_ref_fig: re(fig, "figure.doc_ref")?,
            doc_ref_group: re(&r.doc_ref_group, "doc_ref_group")?, exam: re(&r.exam, "exam")?, tex: re(&r.tex, "tex")?,
            agenda: re(&r.research_agenda, "research_agenda")?, part: re(&r.local_label_part, "local_label_part")?,
            code: re(&r.local_label_code, "local_label_code")?, they: re(&r.unresolved_they, "unresolved_they")?,
            this: re(&r.unresolved_this, "unresolved_this")?, the: re(&r.unresolved_the, "unresolved_the")?,
            titles: c.constraints.title.iter().map(|t| Ok((t.name.clone(), re(&t.regex, &t.name)?))).collect::<Result<_>>()?,
            cfg_min_words: c.filters.min_words,
        })
    }

    pub fn doc_ref_with(&self, c: &Config, q: &str, figure: bool) -> bool {
        if (if figure { &self.doc_ref_fig } else { &self.doc_ref }).is_match(q) {
            return true;
        }
        let ok = &c.filters.words.doc_ref_group_ok;
        self.doc_ref_group.captures_iter(q).any(|m| {
            let last = m[1].rsplit(' ').next().unwrap_or("").to_lowercase();
            !ok.iter().any(|w| *w == last)
        })
    }

    pub fn doc_ref(&self, c: &Config, q: &str) -> bool {
        self.doc_ref_with(c, q, false)
    }

    pub fn exam(&self, q: &str) -> bool {
        self.exam.is_match(q)
    }

    pub fn tex(&self, q: &str) -> bool {
        self.tex.is_match(q)
    }

    pub fn research_agenda(&self, s: &str) -> bool {
        self.agenda.is_match(s)
    }

    pub fn local_label(&self, q: &str) -> bool {
        self.part.is_match(q) || self.code.captures_iter(q).any(|c| c.get(1).is_none())
    }

    pub fn unresolved(&self, c: &Config, q: &str) -> bool {
        let w = &c.filters.words;
        let has = |l: &[String], x: &str| l.iter().any(|y| y == x);
        self.they.is_match(q) || self.this.is_match(q) || self.the.captures_iter(q).any(|m| {
            let mods: Vec<&str> = m[1].split_whitespace().collect();
            let head = m[2].to_lowercase();
            (mods.is_empty() && has(&w.unresolved_bare_heads, &head))
                || (!mods.is_empty() && mods.iter().all(|x| has(&w.unresolved_generic_mods, &x.to_lowercase())))
                || mods.first().is_some_and(|x| has(&w.unresolved_back_mods, &x.to_lowercase()))
        })
    }

    /// One named check; true = the query fails it.
    fn check(&self, c: &Config, name: &str, q: &str, x: &QueryCtx) -> bool {
        let words = q.split_whitespace().count();
        match name {
            "empty" => words < self.cfg_min_words,
            "leak" => self.leak.is_match(q),
            "spill" => c.filters.spill_chars.iter().any(|s| q.contains(s.as_str())),
            "banned" => self.doc_ref_with(c, q, x.caption.is_some()) || self.exam(q),
            "tex" => self.tex(q),
            "unresolved" => self.unresolved(c, q),
            "local_label" => self.local_label(q),
            "generic" => self.research_agenda(q),
            "copy" => jaccard(q, x.need) >= c.filters.copy_jaccard,
            "caption_copy" => x.caption.is_some_and(|cap| {
                let t = c.figure.as_ref().map(|f| f.caption_copy_jaccard).unwrap_or(0.5);
                jaccard(q, cap) >= t
            }),
            "style" => x.style.max_words.is_some_and(|m| words > m) || (x.style.no_question_mark && q.contains('?')),
            _ => false,
        }
    }

    /// First failed check of the configured reject chain ("" = kept). `duplicate` and `skip` depend on the call's
    /// other queries and the model's flag, so the caller applies them.
    pub fn reject(&self, c: &Config, chain: &[String], q: &str, x: &QueryCtx) -> &'static str {
        for name in chain {
            if x.style.skip_checks.iter().any(|s| s == name) {
                continue;
            }
            if self.check(c, name, q, x) {
                return intern(name);
            }
        }
        ""
    }

    /// Instruction constraints that are true of the article (metadata rules, year forms, title rules), in order.
    pub fn constraints(&self, c: &Config, art: &Article) -> Vec<String> {
        let pt: Vec<String> = art.pub_types.iter().map(|x| x.to_lowercase()).collect();
        let has = |s: &str| pt.iter().any(|x| x.contains(s));
        let at = art.article_type.as_str();
        let mut v: Vec<String> = vec![];
        let mut fired_excl: Vec<&str> = vec![];
        for r in &c.constraints.meta {
            if !r.exclusive.is_empty() && fired_excl.contains(&r.exclusive.as_str()) {
                continue;
            }
            let any_given = !r.pub_types_any.is_empty() || !r.article_types_any.is_empty();
            let any = r.pub_types_any.iter().any(|s| has(s)) || r.article_types_any.iter().any(|s| s == at);
            let fires = (!any_given || any) && (r.article_type_is.is_empty() || r.article_type_is == at)
                && !r.pub_types_none.iter().any(|s| has(s));
            if fires {
                v.extend(r.add.iter().cloned());
                if !r.exclusive.is_empty() {
                    fired_excl.push(&r.exclusive);
                }
            }
        }
        let y = &c.constraints.year;
        if let Some(yr) = art.year.filter(|x| (y.min..=y.max).contains(x)) {
            v.extend(y.forms.iter().map(|(p, off)| format!("{p} {}", yr + off)));
        }
        let mut fired: Vec<&str> = vec![];
        for (t, (name, rx)) in c.constraints.title.iter().zip(&self.titles) {
            let hit = rx.is_match(&art.title) || t.also_if.iter().any(|n| fired.contains(&n.as_str()));
            if hit && !t.unless.iter().any(|n| fired.contains(&n.as_str())) {
                v.push(t.add.clone());
            }
            if rx.is_match(&art.title) {
                fired.push(name);
            }
        }
        v
    }
}

/// Reject names as &'static str (they are written into every row).
pub fn intern(name: &str) -> &'static str {
    const NAMES: [&str; 16] = ["empty", "leak", "spill", "banned", "tex", "unresolved", "local_label", "generic", "copy",
        "caption_copy", "style", "duplicate", "skip", "missing", "off_domain", "doc_ref"];
    const MORE: [&str; 1] = ["no_reader"];
    if let Some(n) = MORE.iter().find(|n| **n == name) { return n; }
    NAMES.iter().find(|n| **n == name).copied().unwrap_or("other")
}

// ---------------------------------------------------------------------------------------------------------- noise

fn neighbour(rows: &[String], c: char, x: u64) -> char {
    for (r, row) in rows.iter().enumerate() {
        if let Some(i) = row.find(c) {
            let mut opts: Vec<char> = vec![];
            if i > 0 { opts.push(row.as_bytes()[i - 1] as char); }
            if i + 1 < row.len() { opts.push(row.as_bytes()[i + 1] as char); }
            if r > 0 { if let Some(&b) = rows[r - 1].as_bytes().get(i) { opts.push(b as char); } }
            return opts[(x as usize) % opts.len().max(1)];
        }
    }
    c
}

fn r(key: &str, k: usize) -> u64 {
    u64::from_str_radix(&hash(&[key, "noise", &k.to_string()])[..12], 16).unwrap_or(0)
}

/// One typo in a word of >= 4 letters: adjacent-key substitution, dropped, doubled or transposed letter.
fn typo(rows: &[String], word: &str, x: u64) -> String {
    let cs: Vec<char> = word.chars().collect();
    if cs.len() < 4 || !cs.iter().all(|c| c.is_ascii_alphabetic()) {
        return word.to_string();
    }
    let i = 1 + (x as usize / 7) % (cs.len() - 2);
    let mut out = cs.clone();
    match x % 4 {
        0 => { let lc = cs[i].to_ascii_lowercase(); out[i] = neighbour(rows, lc, x / 13); }
        1 => { out.remove(i); }
        2 => { out.insert(i, cs[i]); }
        _ => { out.swap(i, i + 1); }
    }
    out.into_iter().collect()
}

/// Noisy form of `q` for persona `p` (stage C). Order: known misspellings, random typos, then case and punctuation.
pub fn noise(n: &Noise, q: &str, p: &Persona, key: &str) -> String {
    let mut words: Vec<String> = q.split_whitespace().map(str::to_string).collect();
    let mut budget = p.typos as usize;
    if budget > 0 {
        for w in words.iter_mut() {
            let core = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_lowercase();
            if let Some((_, m)) = n.misspellings.iter().find(|(a, b)| *a == core && a != b) {
                if r(key, 99) % 2 == 0 {
                    *w = w.to_lowercase().replace(&core, m);
                    budget -= 1;
                    break;
                }
            }
        }
    }
    for k in 0..budget {
        let cands: Vec<usize> = (0..words.len()).filter(|&i| words[i].chars().filter(|c| c.is_ascii_alphabetic()).count() >= 4).collect();
        if cands.is_empty() {
            break;
        }
        let x = r(key, k);
        let i = cands[(x as usize) % cands.len()];
        let (lead, rest): (String, String) = { let w = &words[i]; let m = w.len() - w.trim_start_matches(|c: char| !c.is_alphanumeric()).len(); (w[..m].to_string(), w[m..].to_string()) };
        let end = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
        words[i] = format!("{lead}{}{}", typo(&n.keyboard_rows, &rest[..end], x / 3), &rest[end..]);
    }
    let mut s = words.join(" ");
    if p.lowercase {
        s = s.to_lowercase();
    }
    if p.no_punct {
        s = s.chars().filter(|c| !n.punct_removed.contains(*c)).collect();
    }
    s
}
