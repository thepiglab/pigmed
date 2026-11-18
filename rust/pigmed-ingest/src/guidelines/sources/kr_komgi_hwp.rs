//! Hangul word-processor text for the Korean guideline registry (kr_komgi): HWP 5 (OLE compound file, zlib-deflated
//! `BodyText/SectionN` record streams) and HWPX (OWPML: ZIP of `Contents/sectionN.xml`), to Markdown.
//!
//! Paragraph text is kept in source order; tables become Markdown tables at their position (cell addresses from the
//! cell list headers; merged cells keep their text in the top-left cell). Not reproduced: heading levels (HWP outline
//! styles live in DocInfo and are not mapped), character formatting, pictures and drawing objects, and the exact
//! placement of footnotes/headers (their text is kept inline where the record stream stores it).
use anyhow::{bail, Context, Result};
use std::io::Read;

const PARA_TEXT: u16 = 0x43;
const CTRL_HEADER: u16 = 0x47;
const LIST_HEADER: u16 = 0x48;
const TABLE: u16 = 0x4D;

/// What kind of Hangul document `body` is: `Some("hwp")` (HWP 5 compound file), `Some("hwpx")` (OWPML package).
pub fn sniff(body: &[u8]) -> Option<&'static str> {
    if body.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        let mut c = cfb::CompoundFile::open(std::io::Cursor::new(body)).ok()?;
        let mut h = [0u8; 32];
        c.open_stream("/FileHeader").ok()?.read_exact(&mut h).ok()?;
        return h.starts_with(b"HWP Document File").then_some("hwp");
    }
    if body.starts_with(b"PK\x03\x04") {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(body)).ok()?;
        let mut m = String::new();
        z.by_name("mimetype").ok()?.take(64).read_to_string(&mut m).ok()?;
        return (m.trim() == "application/hwp+zip").then_some("hwpx");
    }
    None
}

struct Rec {
    tag: u16,
    level: u16,
    data: Vec<u8>,
}

fn records(stream: &[u8]) -> Result<Vec<Rec>> {
    let mut out = vec![];
    let mut i = 0usize;
    while i + 4 <= stream.len() {
        let h = u32::from_le_bytes(stream[i..i + 4].try_into().unwrap());
        i += 4;
        let tag = (h & 0x3FF) as u16;
        let level = ((h >> 10) & 0x3FF) as u16;
        let mut size = (h >> 20) as usize;
        if size == 0xFFF {
            if i + 4 > stream.len() {
                bail!("HWP record header truncated");
            }
            size = u32::from_le_bytes(stream[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
        }
        if i + size > stream.len() {
            bail!("HWP record of {size} bytes runs past the section stream");
        }
        out.push(Rec { tag, level, data: stream[i..i + size].to_vec() });
        i += size;
    }
    Ok(out)
}

/// PARA_TEXT payload (UTF-16LE with HWP control characters) to plain text.
fn para_text(data: &[u8]) -> String {
    let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        match u {
            // inline (4-9, 19-20) and extended (1-3, 11-12, 14-18, 21-23) controls occupy 8 code units
            1..=9 | 11 | 12 | 14..=23 => {
                if u == 9 {
                    out.push(b'\t' as u16);
                }
                i += 8;
                continue;
            }
            10 => out.push(b'\n' as u16),
            13 | 0 => {}
            24 => out.push(b'-' as u16),
            25..=31 => out.push(b' ' as u16),
            _ => out.push(u),
        }
        i += 1;
    }
    String::from_utf16_lossy(&out).trim_end().to_string()
}

fn u16_at(d: &[u8], o: usize) -> Option<usize> {
    d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
}

fn md_cell(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").replace('|', "\\|")
}

fn grid_markdown(grid: &[Vec<String>]) -> String {
    let cols = grid.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return String::new();
    }
    let mut md = String::new();
    for (ri, row) in grid.iter().enumerate() {
        md.push('|');
        for c in 0..cols {
            md.push_str(&format!(" {} |", md_cell(row.get(c).map(String::as_str).unwrap_or(""))));
        }
        md.push('\n');
        if ri == 0 {
            md.push('|');
            md.push_str(&" --- |".repeat(cols));
            md.push('\n');
        }
    }
    md
}

/// One table control: records `recs[start..end]` (TABLE, cell LIST_HEADERs and their paragraphs) at `level`+1.
fn table_markdown(recs: &[Rec], level: u16, warnings: &mut Vec<String>) -> String {
    let Some(t) = recs.iter().find(|r| r.tag == TABLE && r.level == level + 1) else {
        warnings.push("HWP table control without a table record; its cell text is kept as paragraphs".into());
        return recs.iter().filter(|r| r.tag == PARA_TEXT).map(|r| para_text(&r.data)).collect::<Vec<_>>().join("\n\n");
    };
    let (rows, cols) = (u16_at(&t.data, 4).unwrap_or(0), u16_at(&t.data, 6).unwrap_or(0));
    // cells in source order: (row, col, text)
    let mut cells: Vec<(Option<(usize, usize)>, String)> = vec![];
    for r in recs {
        if r.tag == LIST_HEADER && r.level == level + 1 {
            // list header (u16 paragraph count, u16, u32 flags) then cell column / row address
            let addr = match (u16_at(&r.data, 8), u16_at(&r.data, 10)) {
                (Some(c), Some(rw)) if rw < rows && c < cols => Some((rw, c)),
                _ => None,
            };
            cells.push((addr, String::new()));
        } else if r.tag == PARA_TEXT {
            if let Some((_, text)) = cells.last_mut() {
                let t = para_text(&r.data);
                if !t.is_empty() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&t);
                }
            }
        }
    }
    if rows == 0 || cols == 0 || cells.is_empty() {
        warnings.push("HWP table without rows/columns or cells; its text is kept as paragraphs".into());
        return cells.into_iter().map(|c| c.1).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n");
    }
    let mut grid = vec![vec![String::new(); cols]; rows];
    if cells.iter().all(|c| c.0.is_some()) {
        for (addr, text) in cells {
            let (r, c) = addr.unwrap();
            grid[r][c] = text;
        }
    } else {
        warnings.push("HWP table cell addresses unreadable; cells laid out in source order".into());
        for (i, (_, text)) in cells.into_iter().enumerate() {
            if i < rows * cols {
                grid[i / cols][i % cols] = text;
            }
        }
    }
    grid_markdown(&grid)
}

/// Section record stream to Markdown blocks (paragraphs and tables in order).
fn section_markdown(recs: &[Rec], warnings: &mut Vec<String>) -> String {
    let mut parts: Vec<String> = vec![];
    let mut i = 0;
    while i < recs.len() {
        let r = &recs[i];
        if r.tag == CTRL_HEADER && r.data.get(..4) == Some(b" lbt") {
            let mut end = i + 1;
            while end < recs.len() && recs[end].level > r.level {
                end += 1;
            }
            let t = table_markdown(&recs[i + 1..end], r.level, warnings);
            if !t.trim().is_empty() {
                parts.push(t);
            }
            i = end;
            continue;
        }
        if r.tag == PARA_TEXT {
            let t = para_text(&r.data);
            if !t.trim().is_empty() {
                parts.push(t);
            }
        }
        i += 1;
    }
    parts.join("\n\n")
}

/// HWP 5.0 document bytes to Markdown. Password-protected and distribution (view-only, encrypted) documents fail.
pub fn hwp5_markdown(body: &[u8]) -> Result<(String, Vec<String>)> {
    let mut c = cfb::CompoundFile::open(std::io::Cursor::new(body)).context("HWP: not an OLE compound file")?;
    let mut header = vec![];
    c.open_stream("/FileHeader").context("HWP FileHeader stream missing")?.read_to_end(&mut header)?;
    if !header.starts_with(b"HWP Document File") || header.len() < 40 {
        bail!("HWP FileHeader signature missing");
    }
    let flags = u32::from_le_bytes(header[36..40].try_into().unwrap());
    if flags & 0b10 != 0 {
        bail!("HWP document is password-protected");
    }
    if flags & 0b100 != 0 {
        bail!("HWP distribution document: body text is encrypted (ViewText), not extractable");
    }
    let compressed = flags & 1 != 0;
    let mut names: Vec<(usize, String)> = c
        .read_storage("/BodyText")
        .context("HWP BodyText storage missing")?
        .filter(|e| e.is_stream())
        .filter_map(|e| e.name().strip_prefix("Section").and_then(|n| n.parse().ok()).map(|n| (n, e.path().to_string_lossy().into_owned())))
        .collect();
    names.sort();
    if names.is_empty() {
        bail!("HWP BodyText has no sections");
    }
    let mut warnings = vec![];
    let mut md = vec![];
    for (_, path) in names {
        let mut raw = vec![];
        c.open_stream(&path)?.read_to_end(&mut raw)?;
        let data = if compressed {
            let mut out = vec![];
            flate2::read::DeflateDecoder::new(raw.as_slice()).take(512 << 20).read_to_end(&mut out).with_context(|| format!("HWP {path}: inflate failed"))?;
            out
        } else {
            raw
        };
        md.push(section_markdown(&records(&data)?, &mut warnings));
    }
    warnings.sort();
    warnings.dedup();
    warnings.push("HWP text extracted from the record stream: paragraphs and table cells in source order; heading levels, pictures, drawing objects and formatting are not reproduced, and footnote/header text stays where the file stores it.".into());
    Ok((md.join("\n\n"), warnings))
}

fn local(n: roxmltree::Node) -> &'static str {
    match n.tag_name().name() {
        "p" => "p",
        "t" => "t",
        "tbl" => "tbl",
        "tr" => "tr",
        "tc" => "tc",
        "tab" => "tab",
        "lineBreak" => "lineBreak",
        "cellAddr" => "cellAddr",
        _ => "",
    }
}

/// Text of a paragraph element, excluding nested tables (emitted separately).
fn hwpx_para_text(p: roxmltree::Node, out: &mut String, tables: &mut Vec<roxmltree::NodeId>) {
    for ch in p.children() {
        match local(ch) {
            "tbl" => tables.push(ch.id()),
            "t" => {
                for t in ch.children() {
                    if t.is_text() {
                        out.push_str(t.text().unwrap_or(""));
                    } else {
                        match local(t) {
                            "tab" => out.push('\t'),
                            "lineBreak" => out.push('\n'),
                            _ => {}
                        }
                    }
                }
            }
            _ if ch.is_element() => hwpx_para_text(ch, out, tables),
            _ => {}
        }
    }
}

fn hwpx_table(tbl: roxmltree::Node, warnings: &mut Vec<String>) -> String {
    let rows: usize = tbl.attribute("rowCnt").and_then(|v| v.parse().ok()).unwrap_or(0);
    let cols: usize = tbl.attribute("colCnt").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut grid = vec![vec![String::new(); cols.max(1)]; rows.max(1)];
    let mut sequential = 0usize;
    for tr in tbl.children().filter(|n| local(*n) == "tr") {
        for tc in tr.children().filter(|n| local(*n) == "tc") {
            let mut text = String::new();
            for p in tc.descendants().filter(|n| local(*n) == "p") {
                // only paragraphs whose nearest table ancestor is this one
                if p.ancestors().find(|a| local(*a) == "tbl").map(|a| a.id()) != Some(tbl.id()) {
                    continue;
                }
                let mut s = String::new();
                hwpx_para_text(p, &mut s, &mut vec![]);
                if !s.trim().is_empty() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(s.trim());
                }
            }
            let addr = tc.children().find(|n| local(*n) == "cellAddr").and_then(|a| {
                Some((a.attribute("rowAddr")?.parse::<usize>().ok()?, a.attribute("colAddr")?.parse::<usize>().ok()?))
            });
            match addr {
                Some((r, c)) if r < grid.len() && c < grid[0].len() => grid[r][c] = text,
                _ => {
                    warnings.push("HWPX table cell without a usable address; laid out in source order".into());
                    let w = grid[0].len();
                    if sequential < grid.len() * w {
                        grid[sequential / w][sequential % w] = text;
                    }
                }
            }
            sequential += 1;
        }
    }
    grid_markdown(&grid)
}

fn hwpx_block(p: roxmltree::Node, doc: &roxmltree::Document, parts: &mut Vec<String>, warnings: &mut Vec<String>) {
    let mut text = String::new();
    let mut tables = vec![];
    hwpx_para_text(p, &mut text, &mut tables);
    if !text.trim().is_empty() {
        parts.push(text.trim_end().to_string());
    }
    for id in tables {
        if let Some(t) = doc.get_node(id) {
            let md = hwpx_table(t, warnings);
            if !md.trim().is_empty() {
                parts.push(md);
            }
        }
    }
}

/// HWPX (OWPML) package bytes to Markdown.
pub fn hwpx_markdown(body: &[u8]) -> Result<(String, Vec<String>)> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(body)).context("HWPX: not a ZIP package")?;
    let mut sections: Vec<(usize, String)> = (0..z.len())
        .filter_map(|i| {
            let n = z.by_index(i).ok()?.name().to_string();
            let num = n.strip_prefix("Contents/section")?.strip_suffix(".xml")?.parse().ok()?;
            Some((num, n))
        })
        .collect();
    sections.sort();
    if sections.is_empty() {
        bail!("HWPX package has no Contents/sectionN.xml");
    }
    let mut parts = vec![];
    let mut warnings = vec![];
    for (_, name) in sections {
        let mut xml = String::new();
        z.by_name(&name)?.take(256 << 20).read_to_string(&mut xml)?;
        let doc = roxmltree::Document::parse_with_options(&xml, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() })
            .with_context(|| format!("HWPX {name}: XML invalid"))?;
        // top-level paragraphs: p elements with no p ancestor
        for p in doc.descendants().filter(|n| local(*n) == "p") {
            if p.ancestors().skip(1).any(|a| local(a) == "p") {
                continue;
            }
            hwpx_block(p, &doc, &mut parts, &mut warnings);
        }
    }
    warnings.sort();
    warnings.dedup();
    warnings.push("HWPX text extracted from the section XML: paragraphs and table cells in source order; heading levels, pictures, drawing objects and formatting are not reproduced.".into());
    Ok((parts.join("\n\n"), warnings))
}

/// Dispatch on the sniffed kind.
pub fn markdown(body: &[u8]) -> Result<(String, Vec<String>)> {
    match sniff(body) {
        Some("hwp") => hwp5_markdown(body),
        Some("hwpx") => hwpx_markdown(body),
        _ => bail!("not an HWP 5 or HWPX document"),
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    /// Real registry attachment: self-assessment checklist for the KAMS endoscopy-in-infected-patients guideline.
    const HWP: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/kr_komgi/checklist.hwp"));

    #[test]
    fn para_text_controls() {
        // "A", tab (inline, 8 units), "B", line break, "C", para break
        let mut u: Vec<u16> = vec![b'A' as u16, 9, 0, 0, 0, 0, 0, 0, 9, b'B' as u16, 10, b'C' as u16];
        u.extend([11, 0x7462, 0x6c20, 0, 0, 0, 0, 11]); // table extended control
        u.push(13);
        let bytes: Vec<u8> = u.iter().flat_map(|x| x.to_le_bytes()).collect();
        assert_eq!(para_text(&bytes), "A\tB\nC");
    }

    #[test]
    fn real_hwp5_checklist_converts_with_tables() {
        assert_eq!(sniff(HWP), Some("hwp"));
        let (md, warnings) = markdown(HWP).unwrap();
        assert!(md.chars().filter(|c| ('\u{AC00}'..='\u{D7A3}').contains(c)).count() > 100, "md: {md}");
        assert!(md.contains("| --- |"), "no table found: {md}");
        assert!(warnings.iter().any(|w| w.contains("heading levels")));
        let blocks = crate::guidelines::html::blocks_from_markdown(&md);
        assert!(blocks.iter().any(|b| matches!(b.kind, crate::guidelines::doc::BlockKind::Table { .. })));
    }

    #[test]
    fn hwpx_package_paragraphs_and_table() {
        use std::io::Write;
        let sec = r#"<?xml version="1.0" encoding="UTF-8"?><hs:sec xmlns:hs="http://www.hancom.co.kr/hwpml/2011/section" xmlns:hp="http://www.hancom.co.kr/hwpml/2011/paragraph"><hp:p><hp:run><hp:t>권고 1.<hp:tab/>내시경 전 선별검사</hp:t></hp:run></hp:p><hp:p><hp:run><hp:tbl rowCnt="2" colCnt="2"><hp:tr><hp:tc><hp:subList><hp:p><hp:run><hp:t>권고</hp:t></hp:run></hp:p></hp:subList><hp:cellAddr colAddr="0" rowAddr="0"/></hp:tc><hp:tc><hp:subList><hp:p><hp:run><hp:t>등급</hp:t></hp:run></hp:p></hp:subList><hp:cellAddr colAddr="1" rowAddr="0"/></hp:tc></hp:tr><hp:tr><hp:tc><hp:subList><hp:p><hp:run><hp:t>A</hp:t></hp:run></hp:p></hp:subList><hp:cellAddr colAddr="0" rowAddr="1"/></hp:tc><hp:tc><hp:subList><hp:p><hp:run><hp:t>강함</hp:t></hp:run></hp:p></hp:subList><hp:cellAddr colAddr="1" rowAddr="1"/></hp:tc></hp:tr></hp:tbl></hp:run></hp:p></hs:sec>"#;
        let mut buf = std::io::Cursor::new(vec![]);
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            w.start_file("mimetype", o).unwrap();
            w.write_all(b"application/hwp+zip").unwrap();
            w.start_file("Contents/section0.xml", o).unwrap();
            w.write_all(sec.as_bytes()).unwrap();
            w.finish().unwrap();
        }
        let bytes = buf.into_inner();
        assert_eq!(sniff(&bytes), Some("hwpx"));
        let (md, _) = markdown(&bytes).unwrap();
        assert!(md.starts_with("권고 1.\t내시경 전 선별검사"), "{md}");
        assert!(md.contains("| 권고 | 등급 |") && md.contains("| A | 강함 |"), "{md}");
    }
}
