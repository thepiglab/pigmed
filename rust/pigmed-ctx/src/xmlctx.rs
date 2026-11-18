//! One pass over an article's JATS: figures (id, label, caption, image hrefs, rights notices), the reference list
//! (rid -> pmid / pmcid / doi), and every body sentence that cites a reference (`xref ref-type="bibr"`) or a figure
//! (`xref ref-type="fig"`), with citation markers removed. Paragraphs that cite a figure are kept whole so the
//! figure -> citing chunk link can be resolved against the published chunks.
//!
//! Text follows the ingest rules (pigmed-ingest `jats.rs`): a `<p>` inside a figure, table, formula or supplement
//! belongs to that object, nested `<p>` (lists) are part of the outer paragraph, and the reference list is never body.

use anyhow::{Context, Result};
use regex::Regex;
use roxmltree::{Document, Node, ParsingOptions};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

const XLINK: &str = "http://www.w3.org/1999/xlink";
/// Containers whose text is not part of the surrounding prose.
const SKIP: [&str; 9] = ["fig", "fig-group", "table-wrap", "table-wrap-group", "disp-formula", "disp-formula-group",
    "supplementary-material", "tex-math", "ref-list"];

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Ref {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pmid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pmcid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
}

impl Ref {
    pub fn is_empty(&self) -> bool {
        self.pmid.is_none() && self.pmcid.is_none() && self.doi.is_none()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Fig {
    pub figure_id: String,
    pub label: String,
    pub caption: String,
    pub hrefs: Vec<String>,
    /// `<permissions>`, `<copyright-statement>` or `<credit>` inside the figure: third-party rights possible
    pub rights_notice: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Sent {
    /// index into `Ctx::paragraphs` when the paragraph cites a figure, else the body paragraph ordinal
    pub para: usize,
    pub section: String,
    /// sentence with reference markers removed (figure call-outs kept)
    pub text: String,
    /// sentence with reference markers and figure call-outs removed (query text)
    pub plain: String,
    /// reference ids cited by this sentence (rids, in order, deduplicated)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bibr: Vec<String>,
    /// figure ids cited by this sentence
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub figs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Para {
    pub para: usize,
    pub section: String,
    pub text: String,
    pub figs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Ctx {
    pub article_id: String,
    pub figures: Vec<Fig>,
    pub refs: BTreeMap<String, Ref>,
    pub sentences: Vec<Sent>,
    /// body paragraphs that cite at least one figure (whole text, markers removed)
    pub paragraphs: Vec<Para>,
}

fn name<'a>(n: &Node<'a, 'a>) -> &'a str {
    n.tag_name().name()
}

fn elems<'a, 'i>(n: Node<'a, 'i>, tag: &'a str) -> impl Iterator<Item = Node<'a, 'i>> + 'a
where
    'i: 'a,
{
    n.descendants().filter(move |c| c.is_element() && c.tag_name().name() == tag)
}

fn child<'a, 'i>(n: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == tag)
}

fn ws() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s+").unwrap())
}

/// Whitespace-collapsed text of a subtree.
pub fn text(n: Option<Node>) -> String {
    let Some(n) = n else { return String::new() };
    let raw: String = n.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect();
    ws().replace_all(&raw, " ").trim().to_string()
}

fn href(n: &Node) -> Option<String> {
    n.attribute((XLINK, "href")).or_else(|| n.attribute("href")).map(str::to_string)
}

fn ref_ids(r: Node) -> Ref {
    static DOI: OnceLock<Regex> = OnceLock::new();
    let doi_re = DOI.get_or_init(|| Regex::new(r"10\.\d{4,9}/\S+").unwrap());
    let mut out = Ref::default();
    for pid in elems(r, "pub-id") {
        let v = text(Some(pid));
        if v.is_empty() {
            continue;
        }
        match pid.attribute("pub-id-type").unwrap_or("").to_lowercase().as_str() {
            "pmid" if out.pmid.is_none() => out.pmid = Some(v),
            "pmcid" | "pmc" if out.pmcid.is_none() => out.pmcid = Some(if v.starts_with("PMC") { v } else { format!("PMC{v}") }),
            "doi" if out.doi.is_none() => out.doi = Some(v.to_lowercase()),
            _ => {}
        }
    }
    if out.doi.is_none() {
        for ext in elems(r, "ext-link") {
            let h = href(&ext).unwrap_or_else(|| text(Some(ext)));
            if ext.attribute("ext-link-type").unwrap_or("").eq_ignore_ascii_case("doi") || h.contains("doi.org") {
                if let Some(m) = doi_re.find(&h) {
                    out.doi = Some(m.as_str().trim_end_matches(['.', ',', ';']).to_lowercase());
                    break;
                }
            }
        }
    }
    out
}

/// A paragraph flattened to text with the char spans of its cross-references.
#[derive(Default, Debug)]
struct Flat {
    text: String,
    /// (start, end, ref-type, rids) in chars of `text`
    spans: Vec<(usize, usize, String, Vec<String>)>,
}

impl Flat {
    fn push_text(&mut self, s: &str) {
        for c in s.chars() {
            if c.is_whitespace() {
                if !self.text.is_empty() && !self.text.ends_with(' ') {
                    self.text.push(' ');
                }
            } else {
                self.text.push(c);
            }
        }
    }
    fn len(&self) -> usize {
        self.text.chars().count()
    }
}

fn flatten(n: Node, f: &mut Flat) {
    for c in n.children() {
        if c.is_text() {
            f.push_text(c.text().unwrap_or(""));
        } else if c.is_element() {
            let t = name(&c);
            if SKIP.contains(&t) {
                continue;
            }
            if t == "xref" {
                let start = f.len();
                f.push_text(&text(Some(c)));
                let rids = c.attribute("rid").unwrap_or("").split_whitespace().map(str::to_string).collect();
                f.spans.push((start, f.len(), c.attribute("ref-type").unwrap_or("").to_string(), rids));
            } else {
                flatten(c, f);
            }
        }
    }
}

const ABBR: [&str; 24] = ["fig", "figs", "al", "e.g", "i.e", "vs", "ref", "refs", "no", "nos", "approx", "ca", "cf", "dr",
    "eq", "eqs", "sp", "spp", "st", "suppl", "tab", "vol", "resp", "mr"];

/// Sentence ranges (char start, end) of flattened text: split after . ! ? followed by whitespace and an uppercase
/// letter, digit or "[", unless the word before is a common abbreviation or the point is inside a cross-reference.
fn sentence_ranges(chars: &[char], spans: &[(usize, usize, String, Vec<String>)]) -> Vec<(usize, usize)> {
    let inside = |i: usize| spans.iter().any(|(s, e, _, _)| *s < i && i < *e);
    let mut out = vec![];
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        if matches!(chars[i], '.' | '!' | '?') && i + 2 < chars.len() && chars[i + 1] == ' ' && !inside(i + 1) {
            let next = chars[i + 2];
            let word: String = chars[start..i].iter().rev().take_while(|c| !c.is_whitespace() && **c != '(').collect::<String>()
                .chars().rev().collect::<String>().to_lowercase();
            let abbr = ABBR.iter().any(|a| word == *a || word.ends_with(&format!(".{a}")));
            let single_initial = word.chars().count() == 1 && word.chars().all(|c| c.is_alphabetic());
            if (next.is_uppercase() || next.is_ascii_digit() || next == '[') && !abbr && !single_initial {
                out.push((start, i + 1));
                start = i + 2;
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    if start < chars.len() {
        out.push((start, chars.len()));
    }
    out
}

fn cleanup(s: &str) -> String {
    static EMPTY: OnceLock<Regex> = OnceLock::new();
    static SPACE_PUNCT: OnceLock<Regex> = OnceLock::new();
    let e = EMPTY.get_or_init(|| Regex::new(r"\s*[\[(]\s*(?:[,;:\u{2013}\u{2014}\-]|\s|and|&)*\s*[\])]").unwrap());
    let sp = SPACE_PUNCT.get_or_init(|| Regex::new(r"\s+([.,;:!?)\]])").unwrap());
    let mut t = e.replace_all(s, "").to_string();
    // repeat once: nested leftovers like "( [ ] )"
    t = e.replace_all(&t, "").to_string();
    t = sp.replace_all(&t, "$1").to_string();
    let t = ws().replace_all(&t, " ");
    t.trim().trim_start_matches([',', ';', ' ']).trim().to_string()
}

/// Text of chars [a, b) with every bibr span removed, then cleaned.
fn without_refs(chars: &[char], spans: &[(usize, usize, String, Vec<String>)], a: usize, b: usize) -> String {
    without(chars, spans, a, b, &["bibr"])
}

/// Text of chars [a, b) with the spans of the given ref-types removed, then cleaned.
fn without(chars: &[char], spans: &[(usize, usize, String, Vec<String>)], a: usize, b: usize, types: &[&str]) -> String {
    let mut s = String::new();
    let mut i = a;
    while i < b {
        if let Some((_, e, _, _)) = spans.iter().find(|(s0, e0, t, _)| types.contains(&t.as_str()) && *s0 == i && *e0 > i) {
            i = (*e).min(b).max(i + 1);
            s.push(' ');
            continue;
        }
        s.push(chars[i]);
        i += 1;
    }
    cleanup(&s)
}

/// Caption text: each block (title, p, ...) flattened with reference markers removed, blocks joined by a space.
fn caption_text(n: Option<Node>) -> String {
    let Some(n) = n else { return String::new() };
    let mut parts = vec![];
    let blocks: Vec<Node> = n.children().filter(|c| c.is_element()).collect();
    let blocks = if blocks.is_empty() { vec![n] } else { blocks };
    for b in blocks {
        let mut f = Flat::default();
        flatten(b, &mut f);
        let chars: Vec<char> = f.text.chars().collect();
        let t = without_refs(&chars, &f.spans, 0, chars.len());
        if !t.is_empty() {
            parts.push(t);
        }
    }
    parts.join(" ")
}

/// Query text of a sentence: reference markers already removed; parentheticals that only point at figures, tables
/// or supplements ("(Figure 1A)", "( and Figure S1)", "(see Table 2)") removed; call-outs that are part of the
/// sentence ("Figure 4B, C illustrates ...") kept, since removing them breaks the grammar.
pub fn plain_mention(s: &str) -> String {
    static P: OnceLock<Regex> = OnceLock::new();
    // a parenthetical made only of connectives and call-outs: "(Figure 1A)", "( and Figure S1)", "(see Table 2 and
    // Fig. 3B, C)"; anything with other words ("(e.g. in Fig. 1 of a review)") is real text and stays
    let re = P.get_or_init(|| Regex::new(concat!(
        r"(?i)\s*\(\s*(?:(?:see|also|and|or|cf\.|e\.g\.,?|i\.e\.,?|,|;|&)\s*)*",
        r"(?:(?:supplementary|suppl\.?|extended data|additional file)?\s*(?:fig(?:ure)?s?\.?|tables?|appendix|movie|video|panel)",
        r"\s*[a-z]?\d*[a-z]?(?:\s*[-,\u{2013}]\s*[a-z]?\d*[a-z]?)*\s*(?:(?:and|or|,|;|&)\s*)*)+\)")).unwrap());
    cleanup(&re.replace_all(s, ""))
}

fn section_path(n: Node) -> String {
    n.ancestors().filter(|a| a.is_element() && matches!(name(a), "sec" | "app")).collect::<Vec<_>>().into_iter().rev()
        .map(|s| { let t = text(child(s, "title")); if t.is_empty() { "Untitled".to_string() } else { t } })
        .collect::<Vec<_>>().join(" > ")
}

pub fn parse(xml: &str, article_id: &str) -> Result<Ctx> {
    let opts = ParsingOptions { allow_dtd: true, nodes_limit: 50_000_000 };
    let doc = Document::parse_with_options(xml, opts).context("JATS parse")?;
    let root = doc.root_element();
    let mut ctx = Ctx { article_id: article_id.to_string(), ..Default::default() };
    for (i, fig) in elems(root, "fig").enumerate() {
        let hrefs = elems(fig, "graphic").filter_map(|g| href(&g)).filter(|h| !h.is_empty()).collect();
        let rights = fig.descendants().skip(1).any(|d| d.is_element() && matches!(name(&d), "permissions" | "copyright-statement" | "credit"));
        ctx.figures.push(Fig { figure_id: fig.attribute("id").map(str::to_string).unwrap_or_else(|| format!("fig{i}")),
            label: text(child(fig, "label")), caption: caption_text(child(fig, "caption")), hrefs, rights_notice: rights });
    }
    for r in elems(root, "ref") {
        if let Some(id) = r.attribute("id") {
            ctx.refs.insert(id.to_string(), ref_ids(r));
        }
    }
    let Some(body) = child(root, "body") else { return Ok(ctx) };
    let paras = body.descendants().filter(|p| p.is_element() && name(p) == "p"
        && !p.ancestors().skip(1).any(|a| a.is_element() && (SKIP.contains(&name(&a)) || name(&a) == "p")));
    for (pi, p) in paras.enumerate() {
        let mut f = Flat::default();
        flatten(p, &mut f);
        if f.spans.is_empty() {
            continue;
        }
        let chars: Vec<char> = f.text.chars().collect();
        let section = section_path(p);
        let mut ranges = sentence_ranges(&chars, &f.spans);
        // a marker right after a sentence end ("... disease. [12] Next") belongs to the sentence before
        let mut owner: Vec<usize> = f.spans.iter().map(|(s, _, _, _)| ranges.iter().position(|(a, b)| a <= s && s < b).unwrap_or(0)).collect();
        // the leading run of a sentence made only of reference markers, brackets and separators
        let in_bibr = |i: usize| f.spans.iter().any(|(s, e, t, _)| t == "bibr" && *s <= i && i < *e);
        let lead: Vec<usize> = ranges.iter().map(|(a, b)| {
            let mut i = *a;
            while i < *b && (in_bibr(i) || chars[i].is_whitespace() || "[](),;\u{2013}\u{2014}-".contains(chars[i])) {
                i += 1;
            }
            i
        }).collect();
        for (k, (s, _, t, _)) in f.spans.iter().enumerate() {
            let o = owner[k];
            if t == "bibr" && o > 0 && *s < lead[o] {
                owner[k] = o - 1;
            }
        }
        // drop sentences that are only a moved marker
        let fig_para = f.spans.iter().any(|(_, _, t, _)| t == "fig");
        let pidx = if fig_para {
            ctx.paragraphs.push(Para { para: ctx.paragraphs.len(), section: section.clone(),
                text: without_refs(&chars, &f.spans, 0, chars.len()),
                figs: dedup(f.spans.iter().filter(|x| x.2 == "fig").flat_map(|x| x.3.clone())) });
            ctx.paragraphs.len() - 1
        } else {
            usize::MAX
        };
        for (si, (a, b)) in ranges.drain(..).enumerate() {
            let mine = |t: &str| dedup(f.spans.iter().zip(&owner).filter(|((_, _, tt, _), o)| tt == t && **o == si).flat_map(|((_, _, _, r), _)| r.clone()));
            let (bibr, figs) = (mine("bibr"), mine("fig"));
            if bibr.is_empty() && figs.is_empty() {
                continue;
            }
            let text = without_refs(&chars, &f.spans, a, b);
            if text.split_whitespace().count() < 3 {
                continue;
            }
            let plain = plain_mention(&without_refs(&chars, &f.spans, a, b));
            ctx.sentences.push(Sent { para: if pidx == usize::MAX { pi } else { pidx }, section: section.clone(), text, plain, bibr, figs });
        }
    }
    Ok(ctx)
}

fn dedup(it: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    it.filter(|x| seen.insert(x.clone())).collect()
}

#[cfg(test)]
pub const FIXTURE: &str = include_str!("../tests/fixtures/ctx_article.xml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figures_refs_and_sentences() {
        let c = parse(FIXTURE, "PMC100.1").unwrap();
        assert_eq!(c.figures.len(), 2);
        assert_eq!(c.figures[0].figure_id, "f1");
        assert_eq!(c.figures[0].label, "Figure 1");
        assert_eq!(c.figures[0].caption, "Infarct volume by group. Mean ± SD.");
        assert_eq!(c.figures[0].hrefs, vec!["pone.0001.g001"]);
        assert!(!c.figures[0].rights_notice && c.figures[1].rights_notice);
        assert_eq!(c.refs["r1"].pmid.as_deref(), Some("111"));
        assert_eq!(c.refs["r2"].pmcid.as_deref(), Some("PMC222"));
        assert_eq!(c.refs["r3"].doi.as_deref(), Some("10.5555/abc"));
        assert!(c.refs["r4"].is_empty());
        let cite: Vec<&Sent> = c.sentences.iter().filter(|s| !s.bibr.is_empty()).collect();
        // markers removed, brackets cleaned, the trailing marker after a full stop stays with its sentence
        assert_eq!(cite[0].text, "Colchicine lowers recurrent stroke risk after lacunar infarction.");
        assert_eq!(cite[0].bibr, vec!["r1", "r2"]);
        assert_eq!(cite[1].text, "Earlier trials (e.g. in Fig. 1 of a review) were small.");
        assert_eq!(cite[1].bibr, vec!["r3"]);
        assert!(c.sentences.iter().all(|s| !s.text.contains('[') && !s.text.contains("()")));
        let figs: Vec<&Sent> = c.sentences.iter().filter(|s| s.figs.contains(&"f1".to_string())).collect();
        assert_eq!(figs.len(), 1);
        assert_eq!(figs[0].text, "Infarct volume fell by 30% with colchicine (Figure 1A).");
        assert_eq!(figs[0].plain, "Infarct volume fell by 30% with colchicine.");
        assert_eq!(c.paragraphs.len(), 1);
        assert!(c.paragraphs[0].text.contains("Infarct volume fell") && c.paragraphs[0].figs == vec!["f1"]);
        // text inside a figure caption is never body prose
        assert!(c.sentences.iter().all(|s| !s.text.contains("Mean ± SD")));
    }

    #[test]
    fn plain_mentions() {
        assert_eq!(plain_mention("Seedling growth was inhibited ( and Figure S1)."), "Seedling growth was inhibited.");
        assert_eq!(plain_mention("Growth fell (see Table 2 and Fig. 3B) in rats."), "Growth fell in rats.");
        assert_eq!(plain_mention("Figure 4B, C illustrates the expression of DEGs."), "Figure 4B, C illustrates the expression of DEGs.");
        assert_eq!(plain_mention("Doses (10 mg/kg) were safe."), "Doses (10 mg/kg) were safe.");
    }

    #[test]
    fn splitter_keeps_abbreviations() {
        let t = "Rats (Smith et al. 2019) gained weight. See Fig. 2 for details. Dose was approx. 5 mg. Next one";
        let ch: Vec<char> = t.chars().collect();
        let r = sentence_ranges(&ch, &[]);
        let s: Vec<String> = r.iter().map(|(a, b)| ch[*a..*b].iter().collect()).collect();
        assert_eq!(s, vec!["Rats (Smith et al. 2019) gained weight.", "See Fig. 2 for details.", "Dose was approx. 5 mg.", "Next one"]);
    }
}
