//! JATS parsing + text units. Semantics follow `src/pigmed/documents.py` so unit IDs match.
use crate::common::{collapse_ws, normal_license, py_space, stable_id};
use anyhow::{Context, Result};
use percent_encoding::percent_decode_str;
use regex::Regex;
use roxmltree::{Document, Node, ParsingOptions};
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

fn name<'a>(n: &Node<'a, '_>) -> &'a str {
    n.tag_name().name()
}

/// Elements whose boundaries are word boundaries (Python's `"".join(itertext())` glued
/// "Smith"+"John" and table cells together; audit finding #3).
const BREAKS: &[&str] = &[
    "p", "title", "label", "caption", "list-item", "list", "td", "th", "tr", "thead", "tbody", "name", "surname",
    "given-names", "prefix", "suffix", "sec", "fn", "def-item", "term", "def", "break", "attrib", "disp-quote",
    "mml:mtd", "mtd", "mtr", "statement", "boxed-text", "verse-line", "contrib", "aff", "address",
];
/// Subtrees that are their own evidence units and must not leak into prose (audit #1).
const PROSE_SKIP: &[&str] = &[
    "fig", "fig-group", "table-wrap", "table-wrap-group", "disp-formula", "disp-formula-group", "supplementary-material",
];

fn has_mathml_sibling(n: Node) -> bool {
    n.parent().map_or(false, |p| p.children().any(|c| c.is_element() && name(&c) == "math"))
}

/// LaTeX in PMC `tex-math` is a full document; keep only the body (audit #2).
fn tex_body(s: &str) -> String {
    let body = match (s.find("\\begin{document}"), s.rfind("\\end{document}")) {
        (Some(a), Some(b)) if b > a => &s[a + "\\begin{document}".len()..b],
        _ => s,
    };
    body.trim().trim_matches('$').trim().to_string()
}

fn walk(n: Node, skip: &[&str], out: &mut String) {
    for c in n.children() {
        if c.is_text() {
            out.push_str(c.text().unwrap_or(""));
        } else if c.is_element() {
            let t = name(&c);
            if skip.contains(&t) {
                continue;
            }
            if t == "tex-math" {
                // Prefer the MathML alternative when present; otherwise the LaTeX body only.
                if !has_mathml_sibling(c) {
                    out.push(' ');
                    out.push_str(&tex_body(&c.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect::<String>()));
                    out.push(' ');
                }
                continue;
            }
            let brk = BREAKS.contains(&t);
            if brk {
                out.push(' ');
            }
            walk(c, skip, out);
            if brk {
                out.push(' ');
            }
        }
    }
}

fn text_of(n: Node, skip: &[&str]) -> String {
    let mut s = String::new();
    walk(n, skip, &mut s);
    collapse_ws(&s)
}

pub fn clean_text(n: Option<Node>) -> String {
    n.map(|n| text_of(n, &[])).unwrap_or_default()
}

/// Every element boundary is a word boundary (tables, names).
fn spaced_text(n: Node) -> String {
    fn all(n: Node, out: &mut String) {
        for c in n.children() {
            if c.is_text() {
                out.push_str(c.text().unwrap_or(""));
            } else if c.is_element() {
                if name(&c) == "tex-math" && has_mathml_sibling(c) {
                    continue;
                }
                out.push(' ');
                all(c, out);
                out.push(' ');
            }
        }
    }
    let mut s = String::new();
    all(n, &mut s);
    collapse_ws(&s)
}

fn child<'a, 'i>(n: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && name(c) == tag)
}

fn descendant<'a, 'i>(n: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
    n.descendants().skip(1).find(|c| c.is_element() && name(c) == tag)
}

fn descendants<'a, 'i>(n: Node<'a, 'i>, tag: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.descendants().skip(1).filter(move |c| c.is_element() && name(c) == tag)
}

fn path<'a, 'i>(root: Node<'a, 'i>, steps: &[&str]) -> Option<Node<'a, 'i>> {
    let mut cur = root;
    for s in steps {
        cur = child(cur, s)?;
    }
    Some(cur)
}

/// libxml2 `xmlGetNodePath` for local names: index only when same-named siblings exist.
fn getpath(n: Node) -> String {
    let mut parts = Vec::new();
    let mut cur = Some(n);
    while let Some(e) = cur {
        if !e.is_element() {
            break;
        }
        let nm = name(&e);
        let same: Vec<Node> = e.parent().map(|p| p.children().filter(|c| c.is_element() && name(c) == nm).collect()).unwrap_or_default();
        if same.len() > 1 {
            let idx = same.iter().position(|s| *s == e).unwrap() + 1;
            parts.push(format!("{nm}[{idx}]"));
        } else {
            parts.push(nm.to_string());
        }
        cur = e.parent().filter(|p| p.is_element());
    }
    parts.reverse();
    format!("/{}", parts.join("/"))
}

/// Minimal XML serializer for table/formula originals (namespace prefixes dropped, like the
/// Python code after it rewrites tags to local names; xlink attributes keep their prefix).
fn serialize(n: Node, out: &mut String) {
    fn esc(s: &str, attr: bool) -> String {
        let mut o = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => o.push_str("&amp;"),
                '<' => o.push_str("&lt;"),
                '>' => o.push_str("&gt;"),
                '"' if attr => o.push_str("&quot;"),
                '\r' => o.push_str("&#13;"),
                c => o.push(c),
            }
        }
        o
    }
    if n.is_text() {
        out.push_str(&esc(n.text().unwrap_or(""), false));
        return;
    }
    if !n.is_element() {
        return;
    }
    out.push('<');
    out.push_str(name(&n));
    for a in n.attributes() {
        out.push(' ');
        if a.namespace() == Some(XLINK_NS) {
            out.push_str("xlink:");
        } else if a.namespace() == Some("http://www.w3.org/XML/1998/namespace") {
            out.push_str("xml:");
        }
        out.push_str(a.name());
        out.push_str("=\"");
        out.push_str(&esc(a.value(), true));
        out.push('"');
    }
    if n.has_children() {
        out.push('>');
        for c in n.children() {
            serialize(c, out);
        }
        out.push_str("</");
        out.push_str(name(&n));
        out.push('>');
    } else {
        out.push_str("/>");
    }
}

fn attr<'a>(n: Node<'a, '_>, a: &str) -> Option<&'a str> {
    n.attributes().find(|x| x.name() == a && x.namespace().is_none()).map(|x| x.value())
}

fn findtext(n: Node, tag: &str) -> Value {
    match child(n, tag) {
        Some(c) => json!(c.children().filter(|t| t.is_text()).next().and_then(|t| t.text()).unwrap_or("")),
        None => Value::Null,
    }
}

pub struct Figure {
    pub figure_id: String,
    pub caption: String,
    pub hrefs: Vec<String>,
    pub rights_review_required: bool,
}

/// `<supplementary-material>` entry: hrefs (own + nested media) with its label/caption. Not part of
/// the canonical article JSON (kept identical to the Python writer); used to caption supplement units.
pub struct Supplement {
    pub hrefs: Vec<String>,
    pub label: String,
    pub caption: String,
}

pub struct Block {
    pub block_id: String,
    pub section: Vec<String>,
    pub section_id: String,
    pub kind: String,
    pub text: String,
}

pub struct Article {
    pub json: Map<String, Value>,
    pub blocks: Vec<Block>,
    pub figures: Vec<Figure>,
    pub supplements: Vec<Supplement>,
    pub article_id: String,
    pub title: String,
}

/// Replace named entities that are not XML built-ins with the matching Unicode char where known.
/// PMC XML normally uses numeric references; this only rescues rare stragglers.
fn rescue_entities(xml: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"&([A-Za-z][A-Za-z0-9]*);").unwrap());
    re.replace_all(xml, |c: &regex::Captures| match &c[1] {
        "amp" | "lt" | "gt" | "quot" | "apos" => c[0].to_string(),
        "nbsp" => "\u{a0}".into(),
        "ndash" => "\u{2013}".into(),
        "mdash" => "\u{2014}".into(),
        "thinsp" => "\u{2009}".into(),
        other => format!("&amp;{other};"),
    })
    .into_owned()
}

pub fn parse_jats(xml_bytes: &[u8], metadata: &Value) -> Result<Article> {
    let raw = std::str::from_utf8(xml_bytes).context("JATS is not UTF-8")?;
    let opts = ParsingOptions { allow_dtd: true, nodes_limit: 50_000_000 };
    let rescued;
    let doc = match Document::parse_with_options(raw, opts) {
        Ok(d) => d,
        Err(roxmltree::Error::UnknownEntityReference(..)) => {
            rescued = rescue_entities(raw);
            Document::parse_with_options(&rescued, opts)?
        }
        Err(e) => return Err(e.into()),
    };
    let root = doc.root_element();
    let mut title = clean_text(descendant(root, "article-title"));
    if title.is_empty() {
        title = metadata["title"].as_str().unwrap_or("").to_string();
    }
    let mut pmcid = match &metadata["pmcid"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    };
    if !pmcid.starts_with("PMC") {
        pmcid = format!("PMC{pmcid}");
    }
    let version = metadata["version"].as_i64().unwrap_or(1);
    let article_id = format!("{pmcid}.{version}");

    // Candidate order = document order of the XPath union.
    let abstract_scope: Vec<Node> = path(root, &["front", "article-meta"]).map(|am| am.children().filter(|c| c.is_element() && name(c) == "abstract").collect()).unwrap_or_default();
    // Body and back matter (appendices, data availability, extended tables); never the reference list.
    let scopes: Vec<Node> = ["body", "back"].iter().filter_map(|t| child(root, t)).collect();
    let in_abstract = |n: &Node| n.ancestors().skip(1).any(|a| abstract_scope.contains(&a));
    let in_scope = |n: &Node| {
        let anc: Vec<Node> = n.ancestors().skip(1).collect();
        anc.iter().any(|a| scopes.contains(a)) && !anc.iter().any(|a| matches!(name(a), "ref-list" | "ref"))
    };
    let mut candidates = Vec::new();
    for n in root.descendants() {
        if !n.is_element() {
            continue;
        }
        let t = name(&n);
        let ok = (t == "p" && (in_abstract(&n) || in_scope(&n)))
            || ((t == "table-wrap" || t == "disp-formula") && in_scope(&n));
        if ok {
            candidates.push(n);
        }
    }
    let mut blocks = Vec::new();
    let mut blocks_json = Vec::new();
    for (i, el) in candidates.iter().enumerate() {
        let t = name(el);
        let ancestors: Vec<Node> = el.ancestors().skip(1).filter(|a| a.is_element()).collect();
        // A <p> inside a fig/table/formula/supplement belongs to that object; a <p> inside another
        // <p> (lists) is already part of the outer paragraph (audit #4).
        if t == "p" && ancestors.iter().any(|a| matches!(name(a), "fig" | "table-wrap" | "supplementary-material" | "disp-formula" | "p")) {
            continue;
        }
        // A formula nested in a table is part of the table's text.
        if t == "disp-formula" && ancestors.iter().any(|a| matches!(name(a), "table-wrap" | "fig")) {
            continue;
        }
        let sections: Vec<Node> = ancestors.iter().rev().filter(|a| matches!(name(a), "sec" | "abstract" | "app" | "ack")).cloned().collect();
        let path_names: Vec<String> = sections
            .iter()
            .map(|s| {
                let c = clean_text(child(*s, "title"));
                if !c.is_empty() {
                    c
                } else {
                    match name(s) {
                        "abstract" => "Abstract".into(),
                        "app" => "Appendix".into(),
                        "ack" => "Acknowledgments".into(),
                        _ => "Untitled".into(),
                    }
                }
            })
            .collect();
        let section_ids: Vec<String> = sections.iter().map(|s| attr(*s, "id").map(String::from).unwrap_or_else(|| getpath(*s))).collect();
        let text = match t {
            "table-wrap" => spaced_text(*el),
            "disp-formula" => text_of(*el, &["label"]),
            _ => text_of(*el, PROSE_SKIP),
        };
        if text.is_empty() {
            continue;
        }
        let block_id = attr(*el, "id").map(String::from).unwrap_or_else(|| format!("b{i}"));
        let xml = if t != "p" {
            let mut s = String::new();
            serialize(*el, &mut s);
            json!(s)
        } else {
            Value::Null
        };
        let refs: Vec<&str> = descendants(*el, "xref").filter_map(|r| attr(r, "rid")).collect();
        blocks_json.push(json!({"block_id": block_id, "section": path_names, "section_id": section_ids.join("/"),
            "kind": t, "text": text, "xml": xml, "references": refs}));
        blocks.push(Block { block_id, section: path_names, section_id: section_ids.join("/"), kind: t.to_string(), text });
    }

    let mut figures = Vec::new();
    let mut figures_json = Vec::new();
    for (i, fig) in descendants(root, "fig").enumerate() {
        let hrefs: Vec<String> = descendants(fig, "graphic")
            .filter_map(|g| {
                g.attributes()
                    .find(|a| a.name() == "href" && a.namespace() == Some(XLINK_NS))
                    .or_else(|| g.attributes().find(|a| a.name() == "href" && a.namespace().is_none()))
                    .map(|a| a.value().to_string())
            })
            .filter(|h| !h.is_empty())
            .collect();
        let rights: Vec<String> = fig
            .descendants()
            .skip(1)
            .filter(|d| d.is_element() && matches!(name(d), "permissions" | "copyright-statement" | "credit"))
            .map(|d| clean_text(Some(d)))
            .collect();
        let rights_text = rights.join(" ");
        let figure_id = attr(fig, "id").map(String::from).unwrap_or_else(|| format!("fig{i}"));
        let caption = clean_text(child(fig, "caption"));
        figures_json.push(json!({"figure_id": figure_id, "label": clean_text(child(fig, "label")), "caption": caption,
            "hrefs": hrefs, "rights_notice": rights_text, "rights_review_required": !rights_text.is_empty()}));
        figures.push(Figure { figure_id, caption, hrefs, rights_review_required: !rights_text.is_empty() });
    }

    let href_of = |g: Node| {
        g.attributes()
            .find(|a| a.name() == "href" && a.namespace() == Some(XLINK_NS))
            .or_else(|| g.attributes().find(|a| a.name() == "href" && a.namespace().is_none()))
            .map(|a| a.value().to_string())
    };
    let supplements: Vec<Supplement> = descendants(root, "supplementary-material")
        .map(|sm| Supplement {
            hrefs: sm.descendants().filter(|d| d.is_element()).filter_map(href_of).filter(|h| !h.is_empty()).collect(),
            label: clean_text(child(sm, "label")),
            caption: clean_text(child(sm, "caption")),
        })
        .collect();

    let authors: Vec<String> = path(root, &["front", "article-meta"])
        .map(|am| {
            am.children()
                .filter(|c| c.is_element() && name(c) == "contrib-group")
                .flat_map(|g| g.children().filter(|c| c.is_element() && name(c) == "contrib").collect::<Vec<_>>())
                .flat_map(|c| c.children().filter(|x| x.is_element() && name(x) == "name").collect::<Vec<_>>())
                .map(spaced_text)
                .collect()
        })
        .unwrap_or_default();
    let dates: Vec<Value> = path(root, &["front", "article-meta"])
        .map(|am| {
            am.children()
                .filter(|c| c.is_element() && name(c) == "pub-date")
                .map(|d| {
                    let ty = attr(d, "pub-type").or_else(|| attr(d, "date-type"));
                    json!({"type": ty, "year": findtext(d, "year"), "month": findtext(d, "month"), "day": findtext(d, "day")})
                })
                .collect()
        })
        .unwrap_or_default();
    let pmid = match &metadata["pmid"] {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let doi = match &metadata["doi"] {
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let mut m = Map::new();
    m.insert("article_id".into(), json!(article_id));
    m.insert("pmcid".into(), json!(pmcid));
    m.insert("pmid".into(), json!(pmid));
    m.insert("doi".into(), json!(doi));
    m.insert("version".into(), json!(version));
    m.insert("title".into(), json!(title));
    m.insert("license".into(), json!(normal_license(metadata["license_code"].as_str())));
    m.insert("license_statement".into(), json!(clean_text(descendant(root, "permissions"))));
    m.insert("authors".into(), json!(authors));
    m.insert("is_retracted".into(), json!(metadata["is_retracted"].as_bool().unwrap_or(false)));
    m.insert("publication_dates".into(), json!(dates));
    m.insert("blocks".into(), Value::Array(blocks_json));
    m.insert("figures".into(), Value::Array(figures_json));
    m.insert("source_metadata".into(), metadata.clone());
    Ok(Article { json: m, blocks, figures, supplements, article_id, title })
}

pub fn url_basename(u: &str) -> String {
    let path = u.split(['?', '#']).next().unwrap_or("");
    let path = path.split_once("://").map(|(_, r)| r.split_once('/').map(|(_, p)| p).unwrap_or("")).unwrap_or(path);
    let decoded = percent_decode_str(path).decode_utf8_lossy().to_string();
    decoded.rsplit('/').next().unwrap_or("").to_string()
}

fn stem(name: &str) -> &str {
    // Python Path.stem: strip the last suffix unless the name starts with '.' and has no other dot.
    match name.rfind('.') {
        Some(0) | None => name,
        Some(i) => &name[..i],
    }
}

/// Match XML image references to inventory names; never guess an extension.
pub fn resolve_asset<'a>(href: &str, urls: &'a [String]) -> Option<&'a String> {
    let target = url_basename(href);
    let exact: Vec<&String> = urls.iter().filter(|u| url_basename(u) == target).collect();
    if exact.len() == 1 {
        return Some(exact[0]);
    }
    // Extension-less href such as "pone.0269007.g001" -> "pone.0269007.g001.jpg" (audit #12).
    let bare: Vec<&String> = urls.iter().filter(|u| stem(&url_basename(u)) == target).collect();
    if bare.len() == 1 {
        return Some(bare[0]);
    }
    let st = stem(&target).to_string();
    let m: Vec<&String> = urls.iter().filter(|u| stem(&url_basename(u)) == st).collect();
    if m.len() == 1 { Some(m[0]) } else { None }
}

// ---------------------------------------------------------------- chunking

pub const COUNTER_NAME: &str = "regex-word-v1";

/// `re.finditer(r"\S+", text)` offsets in **code points**.
pub fn word_offsets(chars: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in chars.iter().enumerate() {
        if py_space(*c) {
            if let Some(s) = start.take() {
                out.push((s, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((s, chars.len()));
    }
    out
}

fn sentence_end(tok: &[char]) -> bool {
    // r"[.!?][\"')\]]?$"
    let n = tok.len();
    let is_p = |c: char| matches!(c, '.' | '!' | '?');
    let is_q = |c: char| matches!(c, '"' | '\'' | ')' | ']');
    (n >= 1 && is_p(tok[n - 1])) || (n >= 2 && is_q(tok[n - 1]) && is_p(tok[n - 2]))
}

/// Returns (text, char_start, char_end, token_count).
pub fn split_text(text: &str, budget: usize, overlap: usize) -> Vec<(String, usize, usize, usize)> {
    assert!(budget >= 1 && overlap < budget);
    let chars: Vec<char> = text.chars().collect();
    let tokens = word_offsets(&chars);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        let mut end = (i + budget).min(tokens.len());
        if end < tokens.len() {
            let lo = (i + overlap + 1).max(i + budget * 3 / 4);
            let mut j = end;
            while j > lo {
                let (a, b) = tokens[j - 1];
                if sentence_end(&chars[a..b]) {
                    end = j;
                    break;
                }
                j -= 1;
            }
        }
        let (s, e) = (tokens[i].0, tokens[end - 1].1);
        out.push((chars[s..e].iter().collect(), s, e, end - i));
        if end == tokens.len() {
            break;
        }
        i = (i + 1).max(end - overlap);
    }
    out
}

pub fn text_units(a: &Article, budget: usize, overlap: usize) -> Vec<Value> {
    // OrderedDict keyed by (section_id, prose|kind, block_id|"").
    let mut keys: Vec<(String, String, String)> = Vec::new();
    let mut groups: Vec<Vec<&Block>> = Vec::new();
    for b in &a.blocks {
        let key = (
            b.section_id.clone(),
            if b.kind != "p" { b.kind.clone() } else { "prose".into() },
            if b.kind != "p" { b.block_id.clone() } else { String::new() },
        );
        match keys.iter().position(|k| *k == key) {
            Some(i) => groups[i].push(b),
            None => {
                keys.push(key);
                groups.push(vec![b]);
            }
        }
    }
    let mut out = Vec::new();
    for (key, blocks) in keys.iter().zip(groups.iter()) {
        let text = blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n\n");
        let block_ids: Vec<&str> = blocks.iter().map(|b| b.block_id.as_str()).collect();
        for (part, s, e, count) in split_text(&text, budget, overlap) {
            let uid = stable_id(&[json!(a.article_id), json!([key.0, key.1, key.2]), json!(s), json!(e), json!(part),
                                  json!(COUNTER_NAME), json!(budget), json!(overlap)]);
            out.push(json!({"unit_id": uid, "kind": if key.1 == "table-wrap" {"table_text"} else {"text"},
                "text": part, "locator": {"section_id": key.0, "section": blocks[0].section, "block_ids": block_ids,
                                          "char_start": s, "char_end": e},
                "token_count": count, "tokenizer": COUNTER_NAME,
                "chunk_recipe": {"budget": budget, "overlap": overlap}, "media_path": null}));
        }
    }
    for f in &a.figures {
        if !f.caption.is_empty() {
            for (part, s, e, count) in split_text(&f.caption, budget, overlap) {
                let uid = stable_id(&[json!(a.article_id), json!(f.figure_id), json!("caption"), json!(s), json!(part)]);
                out.push(json!({"unit_id": uid, "kind": "caption", "text": part,
                    "locator": {"figure_id": f.figure_id, "char_start": s, "char_end": e},
                    "token_count": count, "tokenizer": COUNTER_NAME, "media_path": null}));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_matches_python_rules() {
        let t = "One two three. Four five six seven. Eight nine";
        // Python: range(end=4, max(i+overlap+1, i+3)=3, -1) only inspects token 4 ("Four").
        let parts = split_text(t, 4, 1);
        assert_eq!(parts[0].0, "One two three. Four");
        // budget 8: window inspects tokens 8..7 -> "seven." ends a sentence.
        assert_eq!(split_text(t, 8, 2)[0].0, "One two three. Four five six seven.");
        assert!(parts.iter().all(|p| p.3 <= 4));
        let u = "é ü x";
        assert_eq!(split_text(u, 512, 64)[0].2, 5); // code points, not bytes
    }
    #[test]
    fn resolve() {
        let urls = vec!["s3://pmc-oa-opendata/PMC1.1/nihms-1-f0001.jpg?md5=0".to_string(), "s3://pmc-oa-opendata/PMC1.1/x.pdf".to_string()];
        assert_eq!(resolve_asset("nihms-1-f0001", &urls), Some(&urls[0]));
        assert_eq!(resolve_asset("nihms-1-f0001.jpg", &urls), Some(&urls[0]));
        assert_eq!(resolve_asset("nope.tif", &urls), None);
        let plos = vec!["s3://pmc-oa-opendata/PMC2.1/pone.0269007.g001.jpg".to_string(), "s3://pmc-oa-opendata/PMC2.1/pone.0269007.g002.jpg".to_string()];
        assert_eq!(resolve_asset("pone.0269007.g001", &plos), Some(&plos[0]));
    }

    #[test]
    fn nested_fig_table_formula_and_names() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE article PUBLIC "-//NLM//DTD JATS (Z39.96) Journal Archiving and Interchange DTD v1.3 20210610//EN" "JATS-archivearticle1-3.dtd">
<article xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:mml="http://www.w3.org/1998/Math/MathML"><front><article-meta>
<title-group><article-title>T</article-title></title-group>
<contrib-group><contrib><name><surname>Henri</surname><given-names>Samuel</given-names></name></contrib></contrib-group>
</article-meta></front><body><sec id="s1"><title>Results</title>
<p>Intro text<fig id="f1"><label>Figure 1</label><caption><title>Cap title.</title><p>Cap body</p></caption><graphic xlink:href="x.g001"/></fig> after fig.</p>
<p>Formula <inline-formula><alternatives><tex-math>\documentclass[12pt]{minimal}\begin{document}$$x^2$$\end{document}</tex-math><mml:math><mml:mi>x</mml:mi></mml:math></alternatives></inline-formula> here.</p>
<p>List <list><list-item><p>one</p></list-item><list-item><p>two</p></list-item></list></p>
<table-wrap id="t1"><table><tr><td>Polyclonal</td><td>BioLegend</td></tr></table></table-wrap>
</sec></body><back><app-group><app id="a1"><title>Extra</title><p>Appendix text.</p></app></app-group>
<ref-list><ref><mixed-citation><p>Ref text</p></mixed-citation></ref></ref-list></back></article>"#;
        let a = parse_jats(xml.as_bytes(), &json!({"pmcid": "PMC9", "version": 1, "license_code": "CC BY"})).unwrap();
        let texts: Vec<&str> = a.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts[0], "Intro text after fig.");
        assert_eq!(texts[1], "Formula x here.");
        assert_eq!(texts[2], "List one two");
        assert_eq!(texts[3], "Polyclonal BioLegend");
        assert_eq!(texts[4], "Appendix text.");
        assert_eq!(texts.len(), 5);
        assert_eq!(a.blocks[4].section, vec!["Extra".to_string()]);
        assert_eq!(a.figures[0].caption, "Cap title. Cap body");
        assert_eq!(a.json["authors"], json!(["Henri Samuel"]));
        assert_eq!(a.json["license"], json!("CC-BY"));
    }
    #[test]
    fn getpath_like_lxml() {
        let d = Document::parse("<article><body><sec><p/></sec><sec><sec/></sec></body></article>").unwrap();
        let secs: Vec<Node> = d.descendants().filter(|n| n.has_tag_name("sec")).collect();
        assert_eq!(getpath(secs[0]), "/article/body/sec[1]");
        assert_eq!(getpath(secs[2]), "/article/body/sec[2]/sec");
    }
}
