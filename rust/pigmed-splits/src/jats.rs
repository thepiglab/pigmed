//! Compact per-article metadata from PMC JATS: identifiers, journal, dates, related-article links, references,
//! citation contexts, question-form title, length, and the MinHash text (abstract + first body section).

use crate::record::{PubDate, Record, Ref, Related};
use anyhow::{Context, Result};
use regex::Regex;
use roxmltree::{Document, Node, ParsingOptions};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;

const XLINK: &str = "http://www.w3.org/1999/xlink";
/// pub-date types that are not a publication of the paper (PMC embargo release, history dates).
const NOT_PUBLICATION: [&str; 6] = ["pmc-release", "nihms-submitted", "accepted", "received", "rev-recd", "rev-request"];

pub fn template_section() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)conflict|competing|author.{0,12}contribution|funding|acknowledg|data (?:availability|sharing)|",
            r"ethic|consent|abbreviation|supplementary|peer review|publisher|declaration|disclosure|orcid|copyright|licen[cs]e"
        ))
        .unwrap()
    })
}

fn ws() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s+").unwrap())
}

/// Whitespace-collapsed text of a subtree (lxml `itertext` equivalent).
fn text(n: Option<Node>) -> String {
    let Some(n) = n else { return String::new() };
    let raw: String = n.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect();
    ws().replace_all(&raw, " ").trim().to_string()
}

fn name<'a>(n: &Node<'a, 'a>) -> &'a str {
    n.tag_name().name()
}

fn child<'a, 'input>(n: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == tag)
}

fn children<'a, 'input>(n: Node<'a, 'input>, tag: &'a str) -> impl Iterator<Item = Node<'a, 'input>> + 'a
where
    'input: 'a,
{
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == tag)
}

fn descendants<'a, 'input>(n: Node<'a, 'input>, tag: &'a str) -> impl Iterator<Item = Node<'a, 'input>> + 'a
where
    'input: 'a,
{
    n.descendants().filter(move |c| c.is_element() && c.tag_name().name() == tag)
}

fn href<'a>(n: &Node<'a, 'a>) -> Option<&'a str> {
    n.attribute((XLINK, "href")).or_else(|| n.attribute("href"))
}

fn ref_ids(r: Node) -> Ref {
    static DOI: OnceLock<Regex> = OnceLock::new();
    let doi_re = DOI.get_or_init(|| Regex::new(r"10\.\d{4,9}/\S+").unwrap());
    let mut out = Ref::default();
    for pid in descendants(r, "pub-id") {
        let t = pid.attribute("pub-id-type").unwrap_or("").to_lowercase();
        let v = text(Some(pid));
        if v.is_empty() {
            continue;
        }
        match t.as_str() {
            "pmid" if out.pmid.is_none() => out.pmid = Some(v),
            "pmcid" | "pmc" if out.pmcid.is_none() => out.pmcid = Some(v),
            "doi" if out.doi.is_none() => out.doi = Some(v),
            _ => {}
        }
    }
    if out.doi.is_none() {
        for ext in descendants(r, "ext-link") {
            let h = href(&ext).map(str::to_string).unwrap_or_else(|| text(Some(ext)));
            let is_doi = ext.attribute("ext-link-type").unwrap_or("").eq_ignore_ascii_case("doi") || h.contains("doi.org");
            if is_doi {
                if let Some(m) = doi_re.find(&h) {
                    out.doi = Some(m.as_str().to_string());
                    break;
                }
            }
        }
    }
    out
}

fn related(root: Node) -> Vec<Related> {
    let mut links = vec![];
    for el in descendants(root, "related-article") {
        let h = href(&el).unwrap_or("").to_string();
        let mut id_type = el.attribute("ext-link-type").unwrap_or("").to_lowercase();
        if id_type.is_empty() && (h.starts_with("10.") || h.contains("doi.org")) {
            id_type = "doi".into();
        }
        let id_type = match id_type.as_str() {
            "pubmed" => "pmid".to_string(),
            "pmc" | "pmcid" => "pmcid".to_string(),
            _ => id_type,
        };
        links.push(Related { kind: el.attribute("related-article-type").map(str::to_string), id_type, id: h });
    }
    for el in descendants(root, "related-object") {
        let kind = el.attribute("object-type").or_else(|| el.attribute("link-type")).map(str::to_string);
        let id_type = el.attribute("source-id-type").or_else(|| el.attribute("object-id-type")).unwrap_or("").to_lowercase();
        let id = el.attribute("source-id").or_else(|| el.attribute("object-id")).or_else(|| href(&el)).unwrap_or("").to_string();
        links.push(Related { kind, id_type, id });
    }
    links
}

/// Count tokens with an optional tokenizer (EG2). Returned as None when no tokenizer is given.
pub type TokenCount<'t> = Option<&'t (dyn Fn(&str) -> usize + Sync)>;

/// Parse one JATS document. Returns the record (without corpus-level fields) and the MinHash text.
pub fn extract(xml: &str, count_tokens: TokenCount) -> Result<(Record, String)> {
    let opts = ParsingOptions { allow_dtd: true, nodes_limit: u32::MAX };
    let doc = Document::parse_with_options(xml, opts).context("JATS parse")?;
    let root = doc.root_element();
    let front = child(root, "front");
    let meta = front.and_then(|f| child(f, "article-meta"));
    let jmeta = front.and_then(|f| child(f, "journal-meta"));
    let mut r = Record { article_type: root.attribute("article-type").unwrap_or("").to_string(), ..Default::default() };
    if let Some(m) = meta {
        for aid in children(m, "article-id") {
            let t = aid.attribute("pub-id-type").unwrap_or("").to_lowercase();
            let v = text(Some(aid));
            if v.is_empty() {
                continue;
            }
            let slot = match t.as_str() {
                "pmid" => &mut r.pmid,
                "pmc" | "pmcid" => &mut r.pmcid,
                "doi" => &mut r.doi,
                _ => continue,
            };
            if slot.is_empty() {
                *slot = v;
            }
        }
    }
    if let Some(j) = jmeta {
        static ISSN: OnceLock<Regex> = OnceLock::new();
        let issn_re = ISSN.get_or_init(|| Regex::new(r"^\d{4}-?\d{3}[\dXx]$").unwrap());
        let mut issns = BTreeSet::new();
        for i in children(j, "issn") {
            let v: String = text(Some(i)).split_whitespace().collect();
            if issn_re.is_match(&v) {
                let v = v.to_uppercase();
                issns.insert(if v.contains('-') { v } else { format!("{}-{}", &v[..4], &v[4..]) });
            }
        }
        r.issns = issns.into_iter().collect();
        for jid in children(j, "journal-id") {
            if jid.attribute("journal-id-type") == Some("nlm-ta") {
                let v = text(Some(jid));
                if !v.is_empty() {
                    r.nlm_ta = v;
                }
            }
        }
        r.journal = text(descendants(j, "journal-title").next());
    }
    let mut abstract_text = String::new();
    if let Some(m) = meta {
        for d in children(m, "pub-date") {
            let t = d.attribute("pub-type").or_else(|| d.attribute("date-type")).unwrap_or("").to_lowercase();
            if NOT_PUBLICATION.contains(&t.as_str()) {
                continue;
            }
            let part = |tag: &str| child(d, tag).map(|x| text(Some(x)));
            r.pub_dates.push(PubDate { kind: t.clone(), year: part("year"), month: part("month"), day: part("day") });
        }
        r.title = text(child(m, "title-group").and_then(|t| child(t, "article-title")));
        abstract_text = children(m, "abstract")
            .filter(|a| !matches!(a.attribute("abstract-type").unwrap_or(""), "graphical" | "teaser" | "toc"))
            .map(|a| text(Some(a)))
            .collect::<Vec<_>>()
            .join(" ");
    }
    let body = child(root, "body");
    let mut blocks = vec![];
    let mut first_section = String::new();
    if let Some(b) = body {
        const CONTAINERS: [&str; 4] = ["fig", "table-wrap", "supplementary-material", "disp-formula"];
        for el in b.descendants().filter(|n| n.is_element() && matches!(name(n), "p" | "table-wrap" | "disp-formula")) {
            if name(&el) == "p" && el.ancestors().skip(1).any(|a| a.is_element() && CONTAINERS.contains(&name(&a))) {
                continue;
            }
            let t = text(Some(el));
            if !t.is_empty() {
                blocks.push(t);
            }
        }
        for sec in children(b, "sec") {
            if !template_section().is_match(&text(child(sec, "title"))) {
                first_section = descendants(sec, "p").map(|p| text(Some(p))).collect::<Vec<_>>().join(" ");
                break;
            }
        }
        if first_section.is_empty() && !blocks.is_empty() {
            first_section = blocks[..blocks.len().min(5)].join(" ");
        }
    }
    let mut by_rid: HashMap<&str, bool> = HashMap::new();
    for rf in descendants(root, "ref") {
        let ids = ref_ids(rf);
        if let Some(id) = rf.attribute("id") {
            by_rid.insert(id, !ids.is_empty());
        }
        r.references.push(ids);
    }
    if let Some(b) = body {
        for p in descendants(b, "p") {
            let rids: HashSet<&str> = descendants(p, "xref")
                .filter(|x| x.attribute("ref-type") == Some("bibr"))
                .flat_map(|x| x.attribute("rid").unwrap_or("").split_whitespace())
                .collect();
            let ok = rids.iter().filter(|rid| by_rid.get(*rid).copied().unwrap_or(false)).count();
            if ok > 0 {
                r.citation_paragraphs += 1;
                r.citation_context_pairs += ok;
            }
        }
    }
    r.fig_mentions = descendants(root, "xref").filter(|x| x.attribute("ref-type") == Some("fig")).count();
    r.n_figs = descendants(root, "fig").count();
    let full: Vec<&str> = [r.title.as_str(), abstract_text.as_str()]
        .into_iter()
        .chain(blocks.iter().map(String::as_str))
        .filter(|x| !x.is_empty())
        .collect();
    let full = full.join("\n");
    r.question_title = r.title.trim_end().ends_with('?');
    r.has_abstract = !abstract_text.is_empty();
    r.abstract_words = abstract_text.split_whitespace().count();
    r.words = full.split_whitespace().count();
    r.eg2_tokens = count_tokens.map(|f| f(&full));
    r.related = related(root);
    r.n_refs = r.references.len();
    r.n_refs_resolvable = r.references.iter().filter(|x| !x.is_empty()).count();
    let mh = format!("{abstract_text} {first_section}").trim().to_string();
    Ok((r, mh))
}

#[cfg(test)]
pub const FIXTURE: &str = r#"<?xml version="1.0"?>
<!DOCTYPE article PUBLIC "-//NLM//DTD JATS (Z39.96) Journal Archiving and Interchange DTD v1.3 20210610//EN" "JATS-archivearticle1-3.dtd">
<article xmlns:xlink="http://www.w3.org/1999/xlink" article-type="research-article">
<front><journal-meta><journal-id journal-id-type="nlm-ta">J Test</journal-id>
<journal-title-group><journal-title>Journal of Tests</journal-title></journal-title-group>
<issn pub-type="ppub">1234-5678</issn><issn pub-type="epub">87654321</issn></journal-meta>
<article-meta><article-id pub-id-type="pmid">111</article-id><article-id pub-id-type="pmc">PMC222</article-id>
<article-id pub-id-type="doi">10.1/xyz</article-id>
<title-group><article-title>Does aspirin prevent stroke?</article-title></title-group>
<pub-date pub-type="epub"><day>05</day><month>03</month><year>2024</year></pub-date>
<pub-date pub-type="pmc-release"><day>05</day><month>03</month><year>2025</year></pub-date>
<abstract><p>Aspirin was tested in a cohort.</p></abstract>
<related-article related-article-type="corrected-article" ext-link-type="doi" xlink:href="10.1/orig"/>
</article-meta></front>
<body><sec><title>Introduction</title><p>Stroke is common [<xref ref-type="bibr" rid="r1">1</xref>,
<xref ref-type="bibr" rid="r2">2</xref>]. See <xref ref-type="fig" rid="f1">Fig. 1</xref>.</p>
<fig id="f1"><caption><p>A figure.</p></caption></fig></sec>
<sec><title>Funding</title><p>None.</p></sec></body>
<back><ref-list><ref id="r1"><element-citation><pub-id pub-id-type="pmid">999</pub-id></element-citation></ref>
<ref id="r2"><mixed-citation>Some book.</mixed-citation></ref>
<ref id="r3"><element-citation><ext-link ext-link-type="doi" xlink:href="https://doi.org/10.5555/abc">x</ext-link>
</element-citation></ref></ref-list></back></article>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_fixture() {
        let words = |s: &str| s.split_whitespace().count();
        let (r, mh) = extract(FIXTURE, Some(&words)).unwrap();
        assert_eq!((r.pmid.as_str(), r.pmcid.as_str(), r.doi.as_str()), ("111", "PMC222", "10.1/xyz"));
        assert_eq!(r.issns, vec!["1234-5678", "8765-4321"]);
        assert_eq!(r.nlm_ta, "J Test");
        assert_eq!(r.journal, "Journal of Tests");
        assert_eq!(r.pub_dates.len(), 1);
        assert_eq!(r.pub_dates[0].kind, "epub");
        assert!(r.question_title && r.has_abstract);
        assert_eq!(r.related[0].kind.as_deref(), Some("corrected-article"));
        assert_eq!(r.related[0].id, "10.1/orig");
        assert_eq!(r.references[0].pmid.as_deref(), Some("999"));
        assert!(r.references[1].is_empty());
        assert_eq!(r.references[2].doi.as_deref(), Some("10.5555/abc"));
        assert_eq!((r.n_refs, r.n_refs_resolvable), (3, 2));
        assert_eq!((r.citation_context_pairs, r.fig_mentions, r.n_figs), (1, 1, 1));
        assert!(mh.contains("Stroke is common") && !mh.contains("None."));
        assert_eq!(r.eg2_tokens, Some(r.words));
        let cited: Vec<_> = r.citation_ids().collect();
        assert_eq!(cited, vec![("pmid", "999".to_string()), ("doi", "10.5555/abc".to_string())]);
    }
}
