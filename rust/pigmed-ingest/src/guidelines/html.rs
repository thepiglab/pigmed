//! Generic extractors that turn a fetched page or converted file into ordered [`Block`]s.
//!
//! - [`blocks_from_html`]: tolerant HTML (html5ever via `scraper`). It walks the chosen content root in source
//!   order. h1–h6 become headings and maintain `heading_path`; p/div text becomes paragraphs; li becomes list items;
//!   table becomes a Markdown table; figure/img becomes a figure with an absolute `src_url` and caption; aside/footnote
//!   becomes a footnote. Navigation, scripts, styles, headers and footers are skipped.
//! - [`blocks_from_markdown`]: Markdown from `convert::convert` (PDF/DOCX/...). `#` headings, paragraphs, `- ` items,
//!   `|` tables and `[page N]` markers become blocks.
//! Source-specific logic (recommendation boxes, grade labels, which root selector to use) belongs in the adapter.
//! It post-processes these blocks or uses its own selectors.
use super::doc::{Block, BlockKind};
use scraper::{ElementRef, Html, Node, Selector};

const SKIP: &[&str] = &["script", "style", "noscript", "nav", "header", "footer", "form", "button", "svg", "template", "iframe"];

struct Walker<'a> {
    base: Option<&'a url::Url>,
    out: Vec<Block>,
    path: Vec<(u8, String)>,
    n: usize,
    used_ids: std::collections::HashSet<String>,
    warnings: Vec<String>,
}

/// Block-level content that must not be flattened into an inline element's text or skipped with an aria-hidden
/// wrapper (images inside links, tables inside figures, hidden tab panels with real text).
const BLOCK_DESC: &str = "img, table, figure, p, div, ul, ol, dl, section, article, h1, h2, h3, h4, h5, h6";

fn has_block_desc(el: &ElementRef) -> bool {
    el.select(&sel(BLOCK_DESC)).next().is_some()
}

impl<'a> Walker<'a> {
    /// Source element id when present and not already used (pages repeat ids), else a positional id.
    fn id(&mut self, el: &ElementRef) -> String {
        self.n += 1;
        let mut id = el.value().id().map(String::from).unwrap_or_else(|| format!("b{:05}", self.n));
        if !self.used_ids.insert(id.clone()) {
            id = format!("{id}~b{:05}", self.n);
            self.used_ids.insert(id.clone());
        }
        id
    }
    fn heading_path(&self) -> Vec<String> {
        self.path.iter().map(|(_, t)| t.clone()).collect()
    }
    fn push(&mut self, el: &ElementRef, kind: BlockKind, text: String) {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let empty_ok = matches!(kind, BlockKind::Figure { .. });
        if text.is_empty() && !empty_ok {
            return;
        }
        let block_id = self.id(el);
        let heading_path = self.heading_path();
        self.out.push(Block { block_id, kind, text, heading_path, page: None });
    }
    fn abs(&self, src: &str) -> String {
        self.base.and_then(|b| b.join(src).ok()).map(|u| u.to_string()).unwrap_or_else(|| src.to_string())
    }

    fn walk(&mut self, el: ElementRef) {
        let tag = el.value().name();
        if SKIP.contains(&tag) {
            return;
        }
        // aria-hidden wrappers are usually icons/duplicates, but some sites hide inactive tab panels this way; keep
        // anything with block content (hidden-by-CSS panels are kept too: raw HTML is the source of truth)
        if el.value().attr("aria-hidden") == Some("true") && !has_block_desc(&el) {
            return;
        }
        match tag {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = tag[1..].parse::<u8>().unwrap_or(6);
                let text = text_of(&el);
                self.path.retain(|(l, _)| *l < level);
                self.push(&el, BlockKind::Heading { level }, text.clone());
                if !text.trim().is_empty() {
                    self.path.push((level, text.split_whitespace().collect::<Vec<_>>().join(" ")));
                }
            }
            // a paragraph holding an image or table (e.g. <p><a><img></a></p>) is walked like a container
            "p" | "blockquote" | "dd" | "dt" | "pre" if has_block_desc(&el) => self.container(el),
            "p" | "blockquote" | "dd" | "dt" | "pre" => {
                let t = text_of(&el);
                self.push(&el, BlockKind::Paragraph, t);
            }
            "li" => {
                // a list item that contains nested block structure is walked; a plain one is one item
                if has_block_desc(&el) {
                    self.container(el);
                } else {
                    let t = text_of(&el);
                    self.push(&el, BlockKind::ListItem, t);
                }
            }
            "table" => {
                let caption = el.select(&sel("caption")).next().map(|c| text_of(&c)).filter(|c| !c.trim().is_empty());
                let md = table_markdown(&el);
                self.push(&el, BlockKind::Table { caption }, md);
            }
            "figure" => {
                let caption = el.select(&sel("figcaption")).next().map(|c| text_of(&c)).filter(|c| !c.is_empty());
                let tables: Vec<ElementRef> = el.select(&sel("table")).collect();
                let imgs: Vec<ElementRef> = el.select(&sel("img")).collect();
                // a figure can wrap a table (keep it as a table) and/or several images (one figure block each)
                for t in &tables {
                    let cap = t.select(&sel("caption")).next().map(|c| text_of(&c)).filter(|c| !c.is_empty()).or(caption.clone());
                    let md = table_markdown(t);
                    self.push(t, BlockKind::Table { caption: cap }, md);
                }
                for i in &imgs {
                    let src = img_src(i).map(|s| self.abs(&s));
                    self.push(i, BlockKind::Figure { src_url: src, caption: caption.clone() }, String::new());
                }
                if tables.is_empty() && imgs.is_empty() {
                    // inline SVG / canvas / object: keep the caption and say the image was not captured
                    let what = el.select(&sel("svg, object, embed, canvas, picture")).next().map(|e| e.value().name().to_string());
                    self.push(&el, BlockKind::Figure { src_url: None, caption: caption.clone() }, String::new());
                    if let Some(w) = what {
                        self.warnings.push(format!("figure {:?} has a <{w}> without an image URL", caption.as_deref().unwrap_or("")));
                    }
                }
            }
            "img" => {
                let src = img_src(&el).map(|s| self.abs(&s));
                let alt = el.value().attr("alt").map(String::from);
                // decorative images (icons, logos, spacers) are not figures
                let decorative = src.as_deref().is_some_and(|s| s.ends_with(".svg") || s.contains("logo") || s.contains("icon"));
                if !decorative {
                    self.push(&el, BlockKind::Figure { src_url: src, caption: alt }, String::new());
                }
            }
            "aside" => {
                let t = text_of(&el);
                self.push(&el, BlockKind::Footnote, t);
            }
            _ => self.container(el),
        }
    }

    /// Descend; loose text directly inside a div-like container becomes a paragraph.
    fn container(&mut self, el: ElementRef) {
        let mut loose = String::new();
        for c in el.children() {
            match c.value() {
                Node::Text(t) => loose.push_str(t),
                Node::Element(_) => {
                    if let Some(ce) = ElementRef::wrap(c) {
                        // an inline wrapper around block content (e.g. <a><img></a>, <span><p>..</p></span>)
                        // is walked, not flattened, so the image/table inside is not lost
                        if ce.value().attr("aria-hidden") == Some("true") && !has_block_desc(&ce) {
                            // decorative inline (icon glyphs, duplicated labels): skip
                        } else if is_inline(ce.value().name()) && !has_block_desc(&ce) {
                            loose.push_str(&text_of(&ce));
                            loose.push(' ');
                        } else {
                            if !loose.trim().is_empty() {
                                let t = std::mem::take(&mut loose);
                                self.push(&el, BlockKind::Paragraph, t);
                            }
                            self.walk(ce);
                        }
                    }
                }
                _ => {}
            }
        }
        if !loose.trim().is_empty() {
            self.push(&el, BlockKind::Paragraph, loose);
        }
    }
}

/// Image URL: `src`, else lazy-load attributes, else the first `srcset` candidate. Data URIs are ignored.
fn img_src(el: &ElementRef) -> Option<String> {
    let v = el.value();
    let first_srcset = |a: &str| v.attr(a).and_then(|s| s.split(',').next()).and_then(|c| c.split_whitespace().next()).map(String::from);
    ["src", "data-src", "data-original", "data-lazy-src"].iter().filter_map(|a| v.attr(a).map(String::from))
        .chain(first_srcset("srcset")).chain(first_srcset("data-srcset"))
        .find(|s| !s.trim().is_empty() && !s.starts_with("data:"))
}

fn is_inline(tag: &str) -> bool {
    matches!(tag, "a" | "span" | "em" | "strong" | "b" | "i" | "u" | "sup" | "sub" | "abbr" | "code" | "small" | "mark" | "cite" | "br" | "q" | "time")
}

pub fn sel(s: &str) -> Selector {
    Selector::parse(s).unwrap_or_else(|e| panic!("bad selector {s}: {e:?}"))
}

pub fn text_of(el: &ElementRef) -> String {
    let mut s = String::new();
    for t in el.text() {
        s.push_str(t);
        s.push(' ');
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn table_markdown(t: &ElementRef) -> String {
    let rows: Vec<Vec<String>> = t
        .select(&sel("tr"))
        .map(|tr| tr.select(&sel("th, td")).map(|c| text_of(&c).replace('|', "\\|")).collect::<Vec<_>>())
        .filter(|r: &Vec<String>| !r.is_empty())
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    let w = rows.iter().map(|r| r.len()).max().unwrap_or(1);
    let line = |r: &Vec<String>| {
        let mut cells = r.clone();
        cells.resize(w, String::new());
        format!("| {} |", cells.join(" | "))
    };
    let mut out = vec![line(&rows[0]), format!("|{}|", vec!["---"; w].join("|"))];
    out.extend(rows[1..].iter().map(line));
    out.join("\n")
}

/// Blocks from HTML. `root` is a CSS selector for the main content (e.g. "main", "article", "#content"); when it
/// matches nothing, the whole body is used and a warning is returned.
pub fn blocks_from_html(html: &str, base: Option<&url::Url>, root: Option<&str>) -> (Vec<Block>, Vec<String>) {
    let doc = Html::parse_document(html);
    let mut warnings = vec![];
    let start = root
        .and_then(|r| doc.select(&sel(r)).next())
        .or_else(|| {
            if root.is_some() {
                warnings.push(format!("root selector {:?} matched nothing; used <body>", root.unwrap()));
            }
            doc.select(&sel("body")).next()
        })
        .unwrap_or_else(|| doc.root_element());
    let mut w = Walker { base, out: vec![], path: vec![], n: 0, used_ids: Default::default(), warnings: vec![] };
    w.walk(start);
    warnings.extend(w.warnings);
    (w.out, warnings)
}

/// Blocks from Markdown produced by `convert::convert` (PDF, DOCX, ...). Page markers like `[page 12]` or
/// `[page 12 (OCR)]` set `page` on following blocks.
pub fn blocks_from_markdown(md: &str) -> Vec<Block> {
    let mut out: Vec<Block> = vec![];
    let mut path: Vec<(u8, String)> = vec![];
    let mut page: Option<u32> = None;
    let mut para: Vec<String> = vec![];
    let mut table: Vec<String> = vec![];
    let mut n = 0usize;
    let page_re = regex::Regex::new(r"^\[page (\d+)").unwrap();
    let flush = |out: &mut Vec<Block>, n: &mut usize, kind: BlockKind, text: String, path: &[(u8, String)], page: Option<u32>| {
        if text.trim().is_empty() {
            return;
        }
        *n += 1;
        out.push(Block { block_id: format!("m{:05}", n), kind, text: text.trim().to_string(),
                         heading_path: path.iter().map(|(_, t)| t.clone()).collect(), page });
    };
    for line in md.lines().chain(std::iter::once("")) {
        let l = line.trim_end();
        let is_table = l.trim_start().starts_with('|');
        if !is_table && !table.is_empty() {
            let t = std::mem::take(&mut table).join("\n");
            flush(&mut out, &mut n, BlockKind::Table { caption: None }, t, &path, page);
        }
        if is_table {
            if !para.is_empty() {
                let p = std::mem::take(&mut para).join(" ");
                flush(&mut out, &mut n, BlockKind::Paragraph, p, &path, page);
            }
            table.push(l.trim().to_string());
            continue;
        }
        if let Some(c) = page_re.captures(l.trim()) {
            page = c[1].parse().ok();
            continue;
        }
        let hashes = l.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && l.chars().nth(hashes) == Some(' ') {
            if !para.is_empty() {
                let p = std::mem::take(&mut para).join(" ");
                flush(&mut out, &mut n, BlockKind::Paragraph, p, &path, page);
            }
            let text = l[hashes..].trim().to_string();
            let level = hashes as u8;
            path.retain(|(lv, _)| *lv < level);
            flush(&mut out, &mut n, BlockKind::Heading { level }, text.clone(), &path, page);
            path.push((level, text));
            continue;
        }
        let item = l.trim_start();
        // strip_prefix, not byte slicing: "•" and "▪" are multi-byte
        if let Some(rest) = ["- ", "* ", "• ", "▪ ", "◦ ", "– "].iter().find_map(|m| item.strip_prefix(m)) {
            if !para.is_empty() {
                let p = std::mem::take(&mut para).join(" ");
                flush(&mut out, &mut n, BlockKind::Paragraph, p, &path, page);
            }
            flush(&mut out, &mut n, BlockKind::ListItem, rest.to_string(), &path, page);
            continue;
        }
        if l.trim().is_empty() {
            if !para.is_empty() {
                let p = std::mem::take(&mut para).join(" ");
                flush(&mut out, &mut n, BlockKind::Paragraph, p, &path, page);
            }
        } else {
            para.push(l.trim().to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_blocks_in_order_with_heading_paths() {
        let html = r##"<html><body><nav>skip me</nav><main>
            <h1>Hypertension</h1><p>Intro <a href="#">text</a>.</p>
            <h2 id="recs">Recommendations</h2><ul><li>Offer A.</li><li>Consider B.</li></ul>
            <table><caption>Table 1</caption><tr><th>Drug</th><th>Dose</th></tr><tr><td>X</td><td>5 mg</td></tr></table>
            <figure><img src="/img/alg.png"><figcaption>Algorithm 1</figcaption></figure>
            <h2>Background</h2><div>Loose text in a div.</div></main><footer>skip</footer></body></html>"##;
        let base = url::Url::parse("https://example.org/g/htn").unwrap();
        let (b, w) = blocks_from_html(html, Some(&base), Some("main"));
        assert!(w.is_empty());
        let kinds: Vec<String> = b.iter().map(|x| format!("{:?}", x.kind).split([' ', '{', '(']).next().unwrap().to_string()).collect();
        assert_eq!(kinds, ["Heading", "Paragraph", "Heading", "ListItem", "ListItem", "Table", "Figure", "Heading", "Paragraph"]);
        assert_eq!(b[3].heading_path, ["Hypertension", "Recommendations"]);
        assert_eq!(b[2].block_id, "recs");
        assert!(b[5].text.contains("| X | 5 mg |"));
        match &b[6].kind {
            BlockKind::Figure { src_url, caption } => {
                assert_eq!(src_url.as_deref(), Some("https://example.org/img/alg.png"));
                assert_eq!(caption.as_deref(), Some("Algorithm 1"));
            }
            k => panic!("{k:?}"),
        }
        assert_eq!(b[8].heading_path, ["Hypertension", "Background"]);
        assert!(!b.iter().any(|x| x.text.contains("skip")));
    }

    #[test]
    fn linked_images_figure_tables_duplicate_ids_hidden_panels() {
        let html = r##"<html><body><main>
            <h2 id="x">Algorithm</h2><p><a href="/big.png"><img src="/small.png" alt="Flowchart"></a></p>
            <figure><table><tr><th>A</th></tr><tr><td>1</td></tr></table><figcaption>Table 2 Doses</figcaption></figure>
            <figure><img src="/a.png"><img data-src="/b.png"><figcaption>Two panels</figcaption></figure>
            <div id="x"><p>Same id again.</p></div>
            <div role="tabpanel" aria-hidden="true"><p>Hidden tab with recommendations.</p></div>
            <span aria-hidden="true">icon</span></main></body></html>"##;
        let base = url::Url::parse("https://example.org/g/").unwrap();
        let (b, _) = blocks_from_html(html, Some(&base), Some("main"));
        let figs: Vec<&str> = b.iter().filter_map(|x| match &x.kind { BlockKind::Figure { src_url, .. } => src_url.as_deref(), _ => None }).collect();
        assert_eq!(figs, ["https://example.org/small.png", "https://example.org/a.png", "https://example.org/b.png"]);
        assert!(b.iter().any(|x| matches!(&x.kind, BlockKind::Table { caption: Some(c) } if c == "Table 2 Doses") && x.text.contains("| 1 |")));
        let mut ids = std::collections::HashSet::new();
        assert!(b.iter().all(|x| ids.insert(x.block_id.clone())), "duplicate block ids");
        assert!(b.iter().any(|x| x.text == "Hidden tab with recommendations."));
        assert!(!b.iter().any(|x| x.text == "icon"));
    }

    #[test]
    fn markdown_multibyte_bullets() {
        let b = blocks_from_markdown("# H\n• first\n▪ second\n- third");
        let items: Vec<&str> = b.iter().filter(|x| matches!(x.kind, BlockKind::ListItem)).map(|x| x.text.as_str()).collect();
        assert_eq!(items, ["first", "second", "third"]);
    }

    #[test]
    fn markdown_blocks_pages_tables() {
        let md = "[page 1]\n# Guideline\nIntro line one\nline two.\n\n## Recs\n- Do X\n- Do Y\n\n| a | b |\n|---|---|\n| 1 | 2 |\n[page 2]\nTail.";
        let b = blocks_from_markdown(md);
        assert_eq!(b[1].text, "Intro line one line two.");
        assert_eq!(b[3].heading_path, ["Guideline", "Recs"]);
        assert!(matches!(b[5].kind, BlockKind::Table { .. }) && b[5].page == Some(1));
        assert_eq!(b.last().unwrap().page, Some(2));
    }
}
