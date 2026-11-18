//! Bookshelf's documented OAI service, restricted to reviewed clinical title sets.
//! Preserve the source BITS tree alongside canonical blocks; a title set is not one chapter.
use super::{Adapter, PlanRow};
use crate::{
    common::sha256_hex,
    guidelines::{
        doc::{Block, BlockKind, DateObservation, GuidelineDoc, Relation, Rights},
        fetch::{Fetched, Fetcher, Req},
        registry::SourceEntry,
    },
};
use anyhow::{anyhow, bail, Context, Result};
use roxmltree::{Document, Node};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use url::Url;

pub struct NcbiBookshelf;
const BASE: &str = "https://api.ncbi.nlm.nih.gov/lit/oai/books/";
const OAI: &str = "http://www.openarchives.org/OAI/2.0/";
// The service forbids concurrent requests. Hold the lock over an adapter operation,
// including its discovery+record fetch sequence. Separate processes must also be serialized.
static SERVICE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
fn name(n: Node<'_, '_>, s: &str) -> bool {
    n.is_element() && n.tag_name().name() == s
}
fn child<'a>(n: Node<'a, 'a>, s: &str) -> Option<Node<'a, 'a>> {
    n.children().find(|c| name(*c, s))
}
fn descendant<'a>(n: Node<'a, 'a>, s: &str) -> Option<Node<'a, 'a>> {
    n.descendants().find(|c| name(*c, s))
}
fn text(n: Node<'_, '_>) -> String {
    n.descendants()
        .filter(|x| x.is_text())
        .filter_map(|x| x.text())
        .collect::<Vec<_>>()
        .join("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn val(n: Node<'_, '_>, s: &str) -> Option<String> {
    descendant(n, s).map(text).filter(|s| !s.is_empty())
}
fn href(n: Node<'_, '_>) -> Option<String> {
    n.attributes()
        .find(|a| a.name() == "href")
        .map(|a| a.value().to_string())
}
fn url(args: &[(&str, &str)]) -> String {
    let mut u = Url::parse(BASE).expect("constant OAI URL");
    u.query_pairs_mut().extend_pairs(args);
    u.into()
}
fn checked<'a>(xml: &'a str) -> Result<Document<'a>> {
    let d = Document::parse(xml).context("invalid OAI XML")?;
    let root = d.root_element();
    if !name(root, "OAI-PMH") || root.tag_name().namespace() != Some(OAI) {
        bail!("not an OAI-PMH response");
    }
    if let Some(e) = child(root, "error") {
        bail!(
            "OAI error {}: {}",
            e.attribute("code").unwrap_or("unknown"),
            text(e)
        );
    }
    Ok(d)
}
fn numeric_id(id: &str) -> Result<&str> {
    let x = id
        .strip_prefix("oai:books.ncbi.nlm.nih.gov:")
        .ok_or_else(|| anyhow!("unexpected OAI identifier {id}"))?;
    if x.is_empty() || !x.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid OAI identifier {id}");
    }
    Ok(x)
}
fn nbk_id(s: &str) -> Option<String> {
    let t = s.trim();
    t.strip_prefix("NBK")
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map(|_| t.to_string())
}
fn sets(h: Node<'_, '_>) -> Vec<String> {
    h.children()
        .filter(|n| name(*n, "setSpec"))
        .map(text)
        .collect()
}
fn reviewed_sets(src: &SourceEntry) -> Result<Vec<String>> {
    let values = src
        .config
        .get("reviewed_sets")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("reviewed_sets required"))?;
    if values.is_empty() {
        bail!("reviewed_sets must not be empty")
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    for v in values {
        let s = v
            .as_str()
            .ok_or_else(|| anyhow!("reviewed set must be a string"))?;
        if s == "book-open"
            || s.is_empty()
            || !s
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            bail!("expected an explicitly reviewed title set")
        }
        if seen.insert(s.to_string()) {
            out.push(s.to_string())
        }
    }
    Ok(out)
}
fn list_page(xml: &str, set: &str) -> Result<(Vec<(String, String)>, Option<String>)> {
    let d = checked(xml)?;
    let list = child(d.root_element(), "ListIdentifiers")
        .ok_or_else(|| anyhow!("missing ListIdentifiers"))?;
    let mut rows = vec![];
    for h in list.children().filter(|n| name(*n, "header")) {
        if h.attribute("status") == Some("deleted") {
            bail!("deleted OAI record in {set}; reconcile prior records before proceeding");
        }
        let id = val(h, "identifier").ok_or_else(|| anyhow!("missing identifier"))?;
        numeric_id(&id)?;
        let membership = sets(h);
        if !membership.iter().any(|s| s == set) || !membership.iter().any(|s| s == "book-open") {
            bail!("record {id} lacks requested title or book-open membership");
        }
        let date = val(h, "datestamp").ok_or_else(|| anyhow!("missing OAI datestamp"))?;
        rows.push((id, date));
    }
    let token = child(list, "resumptionToken")
        .map(text)
        .filter(|s| !s.is_empty());
    Ok((rows, token))
}
// Ordered mixed-content XML tree: namespaces, attributes, links, spans, figures and
// source recommendation labels remain inspectable even where canonical blocks are narrower.
fn tree(n: Node<'_, '_>) -> Value {
    if n.is_text() {
        return json!({"text":n.text().unwrap_or("")});
    }
    json!({"element":n.tag_name().name(),"namespace":n.tag_name().namespace(),
  "attributes":n.attributes().map(|a|json!({"name":a.name(),"namespace":a.namespace(),"value":a.value()})).collect::<Vec<_>>(),
  "children":n.children().filter(|n|n.is_element() || n.is_text()).map(tree).collect::<Vec<_>>()})
}
fn payload_hash(wrapper: Node<'_, '_>) -> Result<String> {
    Ok(sha256_hex(&serde_json::to_vec(&tree(wrapper))?))
}
fn inline(n: Node<'_, '_>) -> String {
    if n.is_text() {
        return n.text().unwrap_or("").to_string();
    }
    let body = n.children().map(inline).collect::<String>();
    match n.tag_name().name() {
        "xref" => {
            let target = n.attribute("rid").unwrap_or("");
            if target.is_empty() {
                body
            } else {
                format!("[{body}](#{target})")
            }
        }
        "ext-link" | "uri" => href(n).map(|u| format!("[{body}]({u})")).unwrap_or(body),
        "break" => "\n".into(),
        "p" | "title" | "list-item" => format!("{body}\n"),
        "sup" => format!("<sup>{body}</sup>"),
        "sub" => format!("<sub>{body}</sub>"),
        _ => body,
    }
}
struct Bits {
    blocks: Vec<Block>,
    path: Vec<String>,
    used: HashSet<String>,
    warnings: Vec<String>,
}
impl Bits {
    fn new() -> Self {
        Self {
            blocks: vec![],
            path: vec![],
            used: HashSet::new(),
            warnings: vec![],
        }
    }
    fn push(&mut self, n: Node<'_, '_>, kind: BlockKind, t: String) {
        let text = t.trim().to_string();
        if text.is_empty() && !matches!(kind, BlockKind::Figure { .. }) {
            return;
        }
        let mut id = n
            .attribute("id")
            .map(String::from)
            .unwrap_or_else(|| format!("b{:05}", self.blocks.len() + 1));
        if !self.used.insert(id.clone()) {
            id = format!("{id}~{}", self.blocks.len() + 1);
            self.used.insert(id.clone());
        }
        self.blocks.push(Block {
            block_id: id,
            kind,
            text,
            heading_path: self.path.clone(),
            page: None,
        });
    }
    fn table(&mut self, n: Node<'_, '_>, caption: Option<String>) -> Result<()> {
        if n.descendants().filter(|n| name(*n, "table")).count() > 1 {
            bail!("nested BITS table requires explicit handling")
        }
        let mut grid: Vec<Vec<String>> = vec![];
        let mut occupied: HashMap<(usize, usize), String> = HashMap::new();
        for (r, tr) in n.descendants().filter(|n| name(*n, "tr")).enumerate() {
            let mut c = 0;
            for cell in tr.children().filter(|n| name(*n, "th") || name(*n, "td")) {
                while occupied.contains_key(&(r, c)) {
                    c += 1
                }
                let span = |key: &str| -> Result<usize> {
                    let v = cell
                        .attribute(key)
                        .unwrap_or("1")
                        .parse::<usize>()
                        .context("invalid BITS table span")?;
                    if !(1..=256).contains(&v) {
                        bail!("unbounded BITS table span")
                    };
                    Ok(v)
                };
                let rs = span("rowspan")?;
                let cs = span("colspan")?;
                if r + rs > 4096 || c + cs > 4096 {
                    bail!("unbounded BITS table geometry")
                }
                let value = inline(cell)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .replace('|', "\\|");
                for rr in r..r + rs {
                    for cc in c..c + cs {
                        if occupied.insert((rr, cc), value.clone()).is_some() {
                            bail!("overlapping BITS table spans")
                        }
                    }
                }
                c += cs;
                if rs > 1 || cs > 1 {
                    self.warnings.push("Table spans expanded into repeated cells in Markdown; exact geometry retained in BITS attachment".into())
                }
            }
        }
        let height = occupied.keys().map(|(r, _)| r + 1).max().unwrap_or(0);
        let width = occupied.keys().map(|(_, c)| c + 1).max().unwrap_or(0);
        if height == 0 || width == 0 {
            bail!("BITS table has no parseable cells")
        }
        for r in 0..height {
            grid.push(
                (0..width)
                    .map(|c| occupied.remove(&(r, c)).unwrap_or_default())
                    .collect(),
            );
        }
        let mut lines = vec![
            format!("| {} |", grid[0].join(" | ")),
            format!("|{}|", vec!["---"; width].join("|")),
        ];
        lines.extend(grid[1..].iter().map(|r| format!("| {} |", r.join(" | "))));
        self.push(n, BlockKind::Table { caption }, lines.join("\n"));
        Ok(())
    }
    fn walk(&mut self, n: Node<'_, '_>) -> Result<()> {
        if n.is_text() {
            if !n.text().unwrap_or("").trim().is_empty() {
                self.push(n, BlockKind::Paragraph, n.text().unwrap_or("").into());
            }
            return Ok(());
        }
        if !n.is_element() {
            return Ok(());
        }
        let tag = n.tag_name().name();
        match tag {
            "sec" | "app" | "ref-list" => {
                let saved=self.path.clone();
                if let Some(t)=child(n,"title") {
                    let title=text(t);
                    self.push(n,BlockKind::Heading{level:(self.path.len()+1).min(6) as u8},title.clone());
                    self.path.push(title);
                }
                for c in n.children().filter(|c|!name(*c,"title")){self.walk(c)?;}
                self.path=saved;
            }
            "p" => {
                let block=|c:Node<'_, '_>|c.is_element() && matches!(c.tag_name().name(),"list"|"table-wrap"|"fig"|"fn"|"graphic"|"inline-graphic");
                if n.children().any(block) {
                    let mut loose=String::new();
                    for c in n.children() {
                        if block(c) {
                            if !loose.trim().is_empty(){self.push(n,BlockKind::Paragraph,std::mem::take(&mut loose));}
                            self.walk(c)?;
                        } else {loose.push_str(&inline(c));}
                    }
                    if !loose.trim().is_empty(){self.push(n,BlockKind::Paragraph,loose);}
                } else {self.push(n,BlockKind::Paragraph,inline(n));}
            }
            "list" => {
                let ordered=matches!(n.attribute("list-type"),Some("order"|"ordered"|"alpha-lower"|"alpha-upper"|"roman-lower"|"roman-upper"));
                let mut i=0;
                for c in n.children().filter(|n|n.is_element()) {
                    if !name(c,"list-item"){self.walk(c)?;continue;}
                    i+=1;
                    let prefix=child(c,"label").map(text).unwrap_or_else(||if ordered{format!("{i}.")}else{String::new()});
                    let mut prose=String::new();
                    for k in c.children() {
                        if name(k,"list") || name(k,"table-wrap") || name(k,"fig") {
                            if !prose.trim().is_empty(){self.push(c,BlockKind::ListItem,format!("{prefix} {}",prose.trim()));prose.clear();}
                            self.walk(k)?;
                        } else if !name(k,"label"){prose.push_str(&inline(k));prose.push(' ');}
                    }
                    if !prose.trim().is_empty(){self.push(c,BlockKind::ListItem,format!("{prefix} {}",prose.trim()));}
                }
            }
            "table-wrap" => {
                let caption=child(n,"caption").map(text);
                let table=child(n,"table").ok_or_else(||anyhow!("table-wrap without table; image-only table needs review"))?;
                self.table(table,caption)?;
                for c in n.children().filter(|c|!name(*c,"table") && !name(*c,"caption") && !name(*c,"label")){self.walk(c)?;}
            }
            "table" => self.table(n,None)?,
            "fig" => {
                let caption=child(n,"caption").map(text);
                let graphics=n.descendants().filter(|n|name(*n,"graphic") || name(*n,"inline-graphic")).collect::<Vec<_>>();
                if graphics.is_empty(){
                    self.push(n,BlockKind::Figure{src_url:None,caption},String::new());
                    self.warnings.push("Figure has no graphic URL; BITS source retained".into());
                }else{for g in graphics {self.figure(g,caption.clone());}}
            }
            "graphic" | "inline-graphic" => self.figure(n,None),
            "fn" => self.push(n,BlockKind::Footnote,inline(n)),
            "ref" => self.push(n,BlockKind::Paragraph,inline(n)),
            "book-part" => self.warnings.push("Nested book-part retained as has_part relationship and source tree; its body is excluded here to prevent parent/child overlap".into()),
            "tex-math" | "math" | "chem-struct" | "media" => bail!("unsupported BITS {tag}; source requires explicit rendering"),
            "body" | "named-book-part-body" | "back" | "front-matter" | "fn-group" | "table-wrap-foot" | "boxed-text" | "disp-quote" | "def-list" | "def-item" | "def" | "glossary" | "ack" | "notes" | "supplementary-material" => {
                for c in n.children(){self.walk(c)?;}
            }
            "title" => self.push(n,BlockKind::Heading{level:(self.path.len()+1).min(6) as u8},text(n)),
            _ => {
                self.warnings.push(format!("BITS <{tag}> rendered with source structure retained for review"));
                if n.children().any(|c|c.is_element() && matches!(c.tag_name().name(),"p"|"sec"|"list"|"table-wrap"|"table"|"fig"|"graphic"|"fn"|"body"|"named-book-part-body")) {
                    for c in n.children(){self.walk(c)?;}
                } else if !text(n).is_empty(){self.push(n,BlockKind::Paragraph,inline(n));}
            }
        }
        Ok(())
    }
    fn figure(&mut self, n: Node<'_, '_>, caption: Option<String>) {
        let raw = href(n);
        let absolute = raw
            .as_ref()
            .filter(|s| Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https")))
            .cloned();
        if raw.is_some() && absolute.is_none() {
            self.warnings.push(format!(
                "Unresolved source graphic {} retained in BITS attachment; no fetch URL guessed",
                raw.as_deref().unwrap_or("")
            ))
        }
        self.push(
            n,
            BlockKind::Figure {
                src_url: absolute,
                caption,
            },
            String::new(),
        )
    }
}
fn date_value(n: Node<'_, '_>) -> Option<String> {
    let y = val(n, "year")?;
    if y.len() != 4 || !y.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let Some(month) = val(n, "month") else {
        return Some(y);
    };
    let m = month.parse::<u8>().ok().filter(|m| (1..=12).contains(m))?;
    let Some(day) = val(n, "day") else {
        return Some(format!("{y}-{m:02}"));
    };
    let d = day.parse::<u8>().ok().filter(|d| (1..=31).contains(d))?;
    Some(format!("{y}-{m:02}-{d:02}"))
}
fn rights(book: Node<'_, '_>, part: Option<Node<'_, '_>>, evidence: &str) -> Rights {
    let permissions = part
        .and_then(|p| child(p, "book-part-meta"))
        .and_then(|m| child(m, "permissions"))
        .or_else(|| child(book, "permissions"));
    let licenses = permissions
        .map(|p| {
            p.descendants()
                .filter(|n| name(*n, "license"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let links = licenses
        .iter()
        .flat_map(|n| n.descendants().filter(|d| d.is_element()).filter_map(href))
        .collect::<Vec<_>>();
    let statement = permissions.map(text);
    let mut status = "unknown";
    let mut restrictions = vec![];
    if !links.is_empty()
        && links.iter().all(|u| {
            u.trim_end_matches('/') == "https://creativecommons.org/publicdomain/zero/1.0"
                || u.trim_end_matches('/') == "http://creativecommons.org/publicdomain/zero/1.0"
        })
    {
        status = "open"
    } else if links
        .iter()
        .any(|u| u.contains("creativecommons.org/licenses/by-nc-sa/3.0/igo"))
    {
        status = "restricted";
        restrictions.push("Noncommercial; attribution and share alike; IGO terms apply. Review third-party material and the complete source notice.".into())
    } else if links
        .iter()
        .any(|u| u.contains("creativecommons.org/licenses/by/"))
    {
        status = "open";
        restrictions.push(
            "Attribution required; inspect complete source notice and third-party exclusions."
                .into(),
        )
    }
    Rights {
        status: status.into(),
        license: links.first().cloned(),
        terms_url: links.first().cloned(),
        note: statement,
        rights_holders: permissions
            .map(|p| {
                p.descendants()
                    .filter(|n| name(*n, "copyright-holder") || name(*n, "copyright-statement"))
                    .map(text)
                    .collect()
            })
            .unwrap_or_default(),
        restrictions,
        evidence_urls: vec![evidence.into()],
        ..Default::default()
    }
}
fn extract_xml(src: &SourceEntry, row: &PlanRow, xml: &str) -> Result<GuidelineDoc> {
    let d = checked(xml)?;
    let rec = child(d.root_element(), "GetRecord")
        .and_then(|n| child(n, "record"))
        .ok_or_else(|| anyhow!("missing GetRecord record"))?;
    let h = child(rec, "header").ok_or_else(|| anyhow!("missing record header"))?;
    if h.attribute("status") == Some("deleted") {
        bail!("deleted OAI record")
    }
    let id = val(h, "identifier").ok_or_else(|| anyhow!("missing identifier"))?;
    let num = numeric_id(&id)?;
    if row.doc_key != format!("nbk{num}") {
        bail!("OAI record identity mismatch")
    }
    let set = row
        .extra
        .get("oai_set")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing reviewed title set"))?;
    if !reviewed_sets(src)?.iter().any(|s| s == set) {
        bail!("plan title set is not configured")
    }
    let membership = sets(h);
    if !membership.iter().any(|s| s == set) || !membership.iter().any(|s| s == "book-open") {
        bail!("record lacks reviewed title or book-open membership")
    }
    let meta = child(rec, "metadata").ok_or_else(|| anyhow!("missing OAI metadata"))?;
    let wrapper = meta
        .children()
        .find(|n| n.is_element())
        .ok_or_else(|| anyhow!("empty OAI metadata"))?;
    if !name(wrapper, "book-part-wrapper")
        || !matches!(
            wrapper.tag_name().namespace(),
            Some("https://jats.nlm.nih.gov/ns/extensions/bits/2.0/") | None
        )
    {
        bail!("unsupported Bookshelf payload root/namespace")
    }
    if let Some(n) = wrapper.descendants().find(|n| {
        n.is_element()
            && matches!(
                n.tag_name().name(),
                "tex-math" | "math" | "chem-struct" | "media"
            )
    }) {
        bail!(
            "unsupported inline or block BITS {}; explicit rendering required",
            n.tag_name().name()
        )
    }
    let book = child(wrapper, "book-meta").ok_or_else(|| anyhow!("missing book-meta"))?;
    let part = child(wrapper, "book-part").or_else(|| child(wrapper, "front-matter-part"));
    let toc = child(wrapper, "toc");
    let partmeta = part.and_then(|p| child(p, "book-part-meta"));
    let booktitle = val(book, "book-title");
    let title = partmeta
        .and_then(|p| val(p, "title"))
        .or_else(|| toc.and_then(|t| child(t, "title")).map(text))
        .or_else(|| booktitle.clone())
        .ok_or_else(|| anyhow!("missing title"))?;
    let book_ids = book
        .children()
        .filter(|n| name(*n, "book-id"))
        .map(|n| json!({"type":n.attribute("book-id-type"),"value":text(n)}))
        .collect::<Vec<_>>();
    let part_ids = partmeta
        .map(|m| {
            m.children()
                .filter(|n| name(*n, "book-part-id"))
                .map(|n| json!({"type":n.attribute("book-part-id-type"),"value":text(n)}))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(p) = partmeta {
        if let Some(art) = p.children().find(|n| {
            name(*n, "book-part-id") && n.attribute("book-part-id-type") == Some("art-access-id")
        }) {
            if text(art) != format!("NBK{num}") {
                bail!("BITS part identifier disagrees with OAI header")
            }
        }
    }
    let mut bits = Bits::new();
    let role = if toc.is_some() {
        "table_of_contents"
    } else if wrapper.attribute("from-where") == Some("front-matter")
        || part.is_some_and(|p| name(p, "front-matter-part"))
    {
        if title.to_ascii_lowercase().contains("executive summary") {
            "executive_summary"
        } else {
            "front_matter"
        }
    } else if let Some(p) = part {
        match wrapper
            .attribute("content-type")
            .or_else(|| p.attribute("content-type"))
            .or_else(|| p.attribute("book-part-type"))
            .unwrap_or("unknown")
        {
            "chapter" => "chapter",
            "front-matter" | "preface" | "foreword" => "front_matter",
            "appendix" | "app" => "appendix",
            _ => "book_part",
        }
    } else {
        "book_metadata"
    };
    if let Some(p) = part {
        for c in p
            .children()
            .filter(|n| n.is_element() && !name(*n, "book-part-meta"))
        {
            bits.walk(c)?
        }
    }
    let mut relations = vec![Relation {
        rel: "part_of".into(),
        target: format!("urn:ncbi:bookshelf:title-set:{set}"),
        scope: booktitle.clone(),
        evidence_url: Some(row.url.clone()),
    }];
    let mut targets = HashSet::new();
    for n in wrapper.descendants().filter(|n| {
        name(*n, "related-object") || name(*n, "parent-part-link") || name(*n, "book-part-id")
    }) {
        let local = if name(n, "book-part-id")
            && n.attribute("book-part-id-type") == Some("art-access-id")
        {
            Some(text(n))
        } else {
            n.attribute("document-id").map(String::from)
        };
        if let Some(raw) = local {
            let source = n.attribute("source-id").unwrap_or(set);
            let target = if let Some(t) = nbk_id(&raw) {
                format!("guideline:{}:{}", src.id, t.to_ascii_lowercase())
            } else if source == set {
                row.extra
                    .get("part_targets")
                    .and_then(|v| v.get(&raw))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| format!("urn:ncbi:bookshelf:title-set:{set}:part:{raw}"))
            } else {
                format!("urn:ncbi:bookshelf:title-set:{source}:part:{raw}")
            };
            let rel = if name(n, "parent-part-link") {
                "part_of"
            } else if toc.is_some() || name(n, "book-part-id") {
                "has_part"
            } else {
                "related"
            };
            if target != format!("guideline:{}:{}", src.id, row.doc_key)
                && targets.insert((rel, target.clone()))
            {
                relations.push(Relation {
                    rel: rel.into(),
                    target,
                    scope: Some(format!(
                        "BITS {}; source-id={source}; document-id={raw}",
                        n.tag_name().name()
                    )),
                    evidence_url: Some(row.url.clone()),
                })
            }
        }
    }
    for r in &relations {
        if r.target.starts_with("urn:ncbi:bookshelf:title-set:") && r.target.contains(":part:") {
            bits.warnings.push(format!("Unresolved title-part relationship {} retained with source ID; compare complete inventory and TOC",r.target));
        }
    }
    let mut dates = vec![];
    for pd in book.children().filter(|n| name(*n, "pub-date")) {
        let normalized = date_value(pd);
        dates.push(DateObservation {
            label: format!(
                "Book publication date ({})",
                pd.attribute("publication-format")
                    .or_else(|| pd.attribute("pub-type"))
                    .unwrap_or("unspecified")
            ),
            raw: text(pd),
            normalized,
            source_url: Some(row.url.clone()),
        })
    }
    let published = dates.first().and_then(|d| d.normalized.clone());
    for n in wrapper
        .descendants()
        .filter(|n| name(*n, "date") || name(*n, "copyright-year"))
    {
        dates.push(DateObservation {
            label: if name(n, "copyright-year") {
                "Copyright year".into()
            } else {
                format!(
                    "Source date ({})",
                    n.attribute("date-type").unwrap_or("unspecified")
                )
            },
            raw: text(n),
            normalized: if name(n, "copyright-year") {
                None
            } else {
                date_value(n)
            },
            source_url: Some(row.url.clone()),
        });
    }
    if let Some(ds) = val(h, "datestamp") {
        dates.push(DateObservation {
            label: "OAI repository datestamp (not clinical revision date)".into(),
            raw: ds.clone(),
            normalized: Some(ds),
            source_url: Some(row.url.clone()),
        })
    }
    if bits.blocks.is_empty() {
        bits.warnings.push(format!("Record role {role} has no direct body blocks; retained for title coverage and typed relationships"))
    }
    bits.warnings.push("Clinical currentness and complete title coverage are not established by OAI membership or repository datestamp".into());
    let digest = payload_hash(wrapper)?;
    Ok(GuidelineDoc {
        source_id: src.id.clone(),
        doc_key: row.doc_key.clone(),
        work_key: Some(format!("bookshelf-title-set:{set}")),
        item_type: Some(
            match role {
                "chapter" => "guideline_chapter",
                "executive_summary" => "executive_summary",
                "front_matter" => "book_front_matter",
                "appendix" => "book_appendix",
                "table_of_contents" => "book_table_of_contents",
                _ => "book_part",
            }
            .into(),
        ),
        title,
        url: row.url.clone(),
        publisher: val(book, "publisher-name"),
        version: Some(format!("sha256:{digest}")),
        status: "unknown".into(),
        published,
        date_observations: dates,
        relations,
        rights: rights(book, part, &row.url),
        search_metadata: json!({"book_title":booktitle,"book_subtitle":val(book,"subtitle"),"book_subtitles":child(book,"book-title-group").map(|g|g.children().filter(|n|name(*n,"subtitle")).map(text).collect::<Vec<_>>()).unwrap_or_default(),"book_ids":book_ids,"part_ids":part_ids,"wrapper_content_type":wrapper.attribute("content-type"),"wrapper_from_where":wrapper.attribute("from-where"),"record_role":role,"oai_identifier":id,"oai_set":set,"content_sha256":digest}),
        blocks: bits.blocks,
        attachments: vec![
            json!({"type":"source_bits_xml","source_url":row.url,"sha256":digest,"tree":tree(wrapper)}),
        ],
        warnings: bits.warnings,
        ..Default::default()
    })
}
// Compression is requested as documented. FetchRecord hashes the wire response;
// immutable plan identity hashes only the decoded BITS payload, excluding responseDate.
fn request(u: &str) -> Req {
    Req {
        headers: vec![("Accept-Encoding".into(), "gzip, deflate".into())],
        ..Req::get(u)
    }
}
fn decode(body: &[u8], max: u64) -> Result<String> {
    fn read(reader: impl Read, max: u64) -> Result<Vec<u8>> {
        let mut out = vec![];
        reader.take(max.saturating_add(1)).read_to_end(&mut out)?;
        if out.len() as u64 > max {
            bail!("decoded Bookshelf XML exceeds max_bytes")
        };
        Ok(out)
    }
    let bytes = if body.starts_with(&[0x1f, 0x8b]) {
        read(flate2::read::GzDecoder::new(body), max)?
    } else if body
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| *b == b'<')
        || body.starts_with(&[0xef, 0xbb, 0xbf])
    {
        if body.len() as u64 > max {
            bail!("Bookshelf XML exceeds max_bytes")
        };
        body.to_vec()
    } else {
        match read(flate2::read::ZlibDecoder::new(body), max) {
            Ok(v) => v,
            Err(_) => read(flate2::read::DeflateDecoder::new(body), max)
                .context("invalid XML or compressed Bookshelf response")?,
        }
    };
    String::from_utf8(bytes).context("Bookshelf XML is not UTF-8")
}
async fn get(f: &Fetcher, u: &str, max: u64, budget: &mut usize) -> Result<(Fetched, String)> {
    if *budget >= 100 {
        bail!("Bookshelf operation exceeds 100 requests; split into reviewed off-peak harvests rather than truncate silently")
    };
    *budget += 1;
    let fetched = f.request(&request(u)).await?;
    if fetched.record.status != 200 {
        bail!("Bookshelf HTTP {}", fetched.record.status)
    }
    let xml = decode(&fetched.body, max)?;
    Ok((fetched, xml))
}
fn finish_plan(rows: &mut [PlanRow]) {
    let mut targets = serde_json::Map::new();
    for row in rows.iter() {
        if let Some(ids) = row.extra.get("part_ids").and_then(Value::as_array) {
            for id in ids {
                if let Some(local) = id.get("value").and_then(Value::as_str) {
                    targets.insert(
                        format!("{}:{local}", row.extra["oai_set"].as_str().unwrap_or("")),
                        json!(format!("guideline:{}:{}", row.source_id, row.doc_key)),
                    );
                }
            }
        }
    }
    for row in rows {
        let set = row.extra["oai_set"].as_str().unwrap_or("");
        let prefix = format!("{set}:");
        row.extra["part_targets"] = Value::Object(
            targets
                .iter()
                .filter_map(|(k, v)| {
                    k.strip_prefix(&prefix)
                        .map(|key| (key.to_string(), v.clone()))
                })
                .collect(),
        );
    }
}
#[async_trait::async_trait]
impl Adapter for NcbiBookshelf {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        let _guard = SERVICE_LOCK.lock().await;
        let reviewed = reviewed_sets(src)?;
        if src.min_interval_ms < 2000 {
            bail!("Bookshelf source requires min_interval_ms >= 2000")
        };
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let mut rows = vec![];
        let mut ids = HashSet::new();
        let mut budget = 0;
        for set in reviewed {
            let mut token: Option<String> = None;
            let mut seen = HashSet::new();
            loop {
                let u = match &token {
                    Some(t) => url(&[("verb", "ListIdentifiers"), ("resumptionToken", t)]),
                    None => url(&[
                        ("verb", "ListIdentifiers"),
                        ("set", &set),
                        ("metadataPrefix", "nbk_ftext"),
                    ]),
                };
                let (listing, xml) = get(f, &u, src.max_bytes, &mut budget).await?;
                let (page, next) = list_page(&xml, &set)?;
                for (id, date) in page {
                    if !ids.insert(id.clone()) {
                        continue;
                    }
                    let num = numeric_id(&id)?;
                    let u = url(&[
                        ("verb", "GetRecord"),
                        ("identifier", &id),
                        ("metadataPrefix", "nbk_ftext"),
                    ]);
                    let (fetched, xml) = get(f, &u, src.max_bytes, &mut budget).await?;
                    let mut row = PlanRow {
                        source_id: src.id.clone(),
                        doc_key: format!("nbk{num}"),
                        url: u,
                        title: None,
                        rendition: Some("api".into()),
                        version: Some(format!("sha256:{}", fetched.record.sha256)),
                        extra: json!({"oai_set":set,"discovery_datestamp":date,"content_sha256":fetched.record.sha256,"coverage":"reviewed title set; limited plans are partial; preserve TOC/front matter and reconcile relationships"}),
                    };
                    let doc = extract_xml(src, &row, &xml)?;
                    row.version = doc.version.clone();
                    row.extra["content_sha256"] = doc.search_metadata["content_sha256"].clone();
                    row.extra["part_ids"] = doc.search_metadata["part_ids"].clone();
                    row.title = Some(doc.title);
                    row.extra["record_role"] = doc.search_metadata["record_role"].clone();
                    row.extra["discovery_fetch"] = json!(fetched.record);
                    row.extra["listing_fetch"] = json!(listing.record);
                    rows.push(row);
                    if limit.is_some_and(|x| rows.len() >= x) {
                        finish_plan(&mut rows);
                        return Ok(rows);
                    }
                }
                match next {
                    Some(t) => {
                        if !seen.insert(t.clone()) {
                            bail!("repeated OAI resumption token")
                        };
                        token = Some(t)
                    }
                    None => break,
                }
            }
        }
        finish_plan(&mut rows);
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        let _guard = SERVICE_LOCK.lock().await;
        if src.min_interval_ms < 2000 {
            bail!("Bookshelf source requires min_interval_ms >= 2000")
        }
        let id = format!(
            "oai:books.ncbi.nlm.nih.gov:{}",
            row.doc_key
                .strip_prefix("nbk")
                .ok_or_else(|| anyhow!("invalid Bookshelf plan key"))?
        );
        numeric_id(&id)?;
        let expected = url(&[
            ("verb", "GetRecord"),
            ("identifier", &id),
            ("metadataPrefix", "nbk_ftext"),
        ]);
        if row.url != expected || row.source_id != src.id {
            bail!("Bookshelf plan source/URL mismatch")
        }
        let digest = row
            .extra
            .get("content_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Bookshelf plan must pin content_sha256; rediscover"))?;
        let mut budget = 0;
        let (fetched, xml) = get(f, &row.url, src.max_bytes, &mut budget).await?;
        let mut doc = extract_xml(src, row, &xml)?;
        if doc.search_metadata["content_sha256"].as_str() != Some(digest)
            || row.version != doc.version
        {
            bail!("Bookshelf payload changed since discovery; rediscover and review")
        };
        for key in ["listing_fetch", "discovery_fetch"] {
            if let Ok(record) = serde_json::from_value(row.extra[key].clone()) {
                doc.fetches.push(record)
            }
        }
        doc.fetches.push(fetched.record);
        Ok(doc)
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const PART: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/ncbi_bookshelf/part.xml"
    ));
    const LIST: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/ncbi_bookshelf/list.xml"
    ));
    fn src() -> SourceEntry {
        serde_json::from_value(json!({"id":"ncbi_bookshelf","access":"public","enabled":true,"adapter":"ncbi_bookshelf","config":{"reviewed_sets":["who344424"]}})).unwrap()
    }
    fn row(xml: &str) -> PlanRow {
        let id = "oai:books.ncbi.nlm.nih.gov:573627";
        let d = checked(xml).unwrap();
        let meta = descendant(d.root_element(), "metadata").unwrap();
        let digest = payload_hash(meta.children().find(|n| n.is_element()).unwrap()).unwrap();
        PlanRow {
            source_id: "ncbi_bookshelf".into(),
            doc_key: "nbk573627".into(),
            url: url(&[
                ("verb", "GetRecord"),
                ("identifier", id),
                ("metadataPrefix", "nbk_ftext"),
            ]),
            title: None,
            rendition: Some("api".into()),
            version: Some(format!("sha256:{digest}")),
            extra: json!({"oai_set":"who344424","content_sha256":digest}),
        }
    }
    #[test]
    fn pagination_errors_deleted_and_namespace() {
        let (rows, t) = list_page(LIST, "who344424").unwrap();
        assert_eq!(rows[0].0, "oai:books.ncbi.nlm.nih.gov:573627");
        assert_eq!(t.as_deref(), Some("next+token"));
        for xml in ["<OAI-PMH xmlns='http://www.openarchives.org/OAI/2.0/'><error code='badResumptionToken'>expired</error></OAI-PMH>","<OAI-PMH xmlns='http://www.openarchives.org/OAI/2.0/'><ListIdentifiers><header status='deleted'/></ListIdentifiers></OAI-PMH>","<OAI-PMH><ListIdentifiers/></OAI-PMH>"]{assert!(list_page(xml,"who344424").is_err())}
    }
    #[test]
    fn canonical_roundtrip_preserves_metadata_blocks_and_rights() {
        let doc = extract_xml(&src(), &row(PART), PART).unwrap();
        assert_eq!(doc.title, "Recommendations");
        assert_eq!(doc.publisher.as_deref(), Some("WHO"));
        assert_eq!(doc.rights.status, "restricted");
        assert!(doc.rights.restrictions[0].contains("Noncommercial"));
        assert!(doc.blocks.iter().any(
            |b| matches!(b.kind, BlockKind::Table { .. }) && b.text.contains("Offer treatment")
        ));
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::ListItem)
                && b.text.contains("Implementation detail")));
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Footnote) && b.text.contains("Important note")));
        assert!(doc
            .date_observations
            .iter()
            .any(|d| d.label.contains("not clinical revision")));
        let round: GuidelineDoc =
            serde_json::from_value(serde_json::to_value(&doc).unwrap()).unwrap();
        assert_eq!(round.attachments[0]["tree"], doc.attachments[0]["tree"]);
        assert_eq!(round.blocks.len(), doc.blocks.len());
        assert_eq!(
            doc.work_key.as_deref(),
            Some("bookshelf-title-set:who344424")
        );
    }
    #[test]
    fn links_figures_spans_and_toc_are_explicit() {
        let xml=PART.replace("<p>Opening guidance.</p>","<p>See <xref rid='t1'>Table 1</xref> and <ext-link xmlns:xlink='http://www.w3.org/1999/xlink' xlink:href='https://who.int/'>WHO</ext-link>.</p><fig id='fig1'><caption><p>Algorithm.</p></caption><graphic xmlns:xlink='http://www.w3.org/1999/xlink' xlink:href='local.png'/></fig>").replace("<th>Recommendation</th>","<th colspan='2'>Recommendation</th>");
        let doc = extract_xml(&src(), &row(&xml), &xml).unwrap();
        assert!(doc.blocks.iter().any(|b| b.text.contains("[Table 1](#t1)")));
        assert!(doc.blocks.iter().any(
            |b| matches!(&b.kind,BlockKind::Figure{src_url:None,caption:Some(c)} if c=="Algorithm.")
        ));
        assert!(doc.warnings.iter().any(|w| w.contains("spans expanded")));
        assert!(serde_json::to_string(&doc.attachments)
            .unwrap()
            .contains("local.png"));
        let toc = PART
            .replace("<book-part>", "<toc>")
            .replace("</book-part>", "</toc>");
        let doc = extract_xml(&src(), &row(&toc), &toc).unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("book_table_of_contents"));
        assert!(doc.blocks.is_empty());
    }
    #[test]
    fn front_matter_toc_identity_and_envelope_hash() {
        let xml = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/ncbi_bookshelf/front_matter.xml"
        ));
        let mut source = src();
        source.config = json!({"reviewed_sets":["tip63v2"]});
        let mut plan = row(PART);
        plan.doc_key = "nbk574916".into();
        plan.extra["oai_set"] = json!("tip63v2");
        let doc = extract_xml(&source, &plan, xml).unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("executive_summary"));
        assert_eq!(doc.published.as_deref(), Some("2018"));
        assert_eq!(
            doc.search_metadata["book_subtitles"],
            json!(["Synthetic series subtitle", "Updated 2021"])
        );
        assert_eq!(doc.search_metadata["part_ids"][1]["value"], "fm");
        assert!(doc
            .blocks
            .iter()
            .any(|b| b.text.contains("Boxed summary guidance")));
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Figure { src_url: None, .. })));
        assert!(doc.warnings.iter().any(|w| w.contains("summary_graphic")));
        assert_eq!(doc.rights.status, "open");
        assert!(doc
            .blocks
            .iter()
            .any(|b| b.text.contains("Clinical summary preserved")));
        assert!(doc.blocks.iter().any(|b| b.text == "1. First action."));
        assert!(doc.blocks.iter().any(|b| b.text == "2. Second action."));
        let changed = xml.replace("2026-11-14T12:00:00Z", "2026-11-15T12:00:00Z");
        assert_eq!(
            doc.version,
            extract_xml(&source, &plan, &changed).unwrap().version
        );
        let xml = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/ncbi_bookshelf/toc.xml"
        ));
        let mut plan = row(PART);
        plan.doc_key = "nbk573631".into();
        plan.extra["part_targets"] = json!({"ch3":"guideline:ncbi_bookshelf:nbk573627"});
        let doc = extract_xml(&src(), &plan, xml).unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("book_table_of_contents"));
        assert!(doc.blocks.is_empty());
        assert!(doc
            .relations
            .iter()
            .any(|r| r.rel == "has_part" && r.target == "guideline:ncbi_bookshelf:nbk573627"));
        assert!(doc.warnings.iter().any(|w| w.contains("external_annex")));
    }
    #[test]
    fn compressed_payload_and_restrictive_licenses() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
        gz.write_all(PART.as_bytes()).unwrap();
        let bytes = gz.finish().unwrap();
        assert_eq!(decode(&bytes, 1 << 20).unwrap(), PART);
        assert!(decode(&bytes, 10).is_err());
        let mut z = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::default());
        z.write_all(PART.as_bytes()).unwrap();
        assert_eq!(decode(&z.finish().unwrap(), 1 << 20).unwrap(), PART);
        let xml = PART.replace(
            "https://creativecommons.org/licenses/by-nc-sa/3.0/igo",
            "https://publisher.example/custom-terms",
        );
        assert_eq!(
            extract_xml(&src(), &row(&xml), &xml).unwrap().rights.status,
            "unknown"
        );
        let xml = PART.replace(
            "<p>Opening guidance.</p>",
            "<p>Opening guidance.</p><tex-math>x^2</tex-math>",
        );
        assert!(extract_xml(&src(), &row(&xml), &xml).is_err());
    }
    #[tokio::test]
    async fn offline_discovery_pins_bytes_and_enforces_zero_limit() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        assert!(NcbiBookshelf
            .discover(&src(), &f, Some(0))
            .await
            .unwrap()
            .is_empty());
        let first = url(&[
            ("verb", "ListIdentifiers"),
            ("set", "who344424"),
            ("metadataPrefix", "nbk_ftext"),
        ]);
        f.seed_cache_request_for_tests(&request(&first), LIST.as_bytes(), "text/xml");
        let planned = row(PART);
        f.seed_cache_request_for_tests(&request(&planned.url), PART.as_bytes(), "text/xml");
        let rows = NcbiBookshelf.discover(&src(), &f, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].extra["content_sha256"],
            planned.extra["content_sha256"]
        );
        let doc = NcbiBookshelf.extract(&src(), &f, &rows[0]).await.unwrap();
        assert_eq!(doc.title, "Recommendations");
        f.seed_cache_request_for_tests(
            &request(&planned.url),
            PART.replace("Offer treatment", "Changed treatment")
                .as_bytes(),
            "text/xml",
        );
        assert!(NcbiBookshelf.extract(&src(), &f, &rows[0]).await.is_err());
    }
}
