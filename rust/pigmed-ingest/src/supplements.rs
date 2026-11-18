//! Supplementary-file text for embedding (Rust-only extension; off unless `supplements` is set).
//! Prose supplements (PDF, Word, slides, HTML, plain text) become `supplement_text` chunks with the
//! same splitter as the body. Tables (Excel, CSV/TSV, tabular .txt) become one capped `table_card`
//! each: a header naming the file and its JATS caption, then the first rows. Full Markdown is archived
//! in the bundle (up to `supplement_archive_bytes`) so a later run can re-chunk without re-downloading.
use crate::common::stable_id;
use crate::jats::{split_text, url_basename, Supplement, COUNTER_NAME};
use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Prose,
    Table,
}

pub fn class_of(ext: &str) -> Option<Class> {
    match ext {
        ".pdf" | ".doc" | ".docx" | ".odt" | ".rtf" | ".ppt" | ".pptx" | ".odp" | ".htm" | ".html" | ".md" | ".tex" | ".epub" => Some(Class::Prose),
        ".xls" | ".xlsx" | ".ods" | ".csv" | ".tsv" => Some(Class::Table),
        ".txt" => Some(Class::Prose), // re-classified by `looks_tabular` after reading
        _ => None,
    }
}

/// Plain text that is really a delimited table (most `.txt` supplements are data exports).
pub fn looks_tabular(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(200).collect();
    if lines.len() < 5 {
        return false;
    }
    let delimited = lines.iter().filter(|l| l.contains('\t') || l.matches(',').count() >= 3).count();
    delimited * 2 > lines.len()
}

/// Caption for a supplement file from `<supplementary-material>` (matched by basename or stem).
pub fn caption_for(url: &str, supps: &[Supplement]) -> (String, String) {
    let file = url_basename(url);
    let stem = |s: &str| s.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or_else(|| s.to_string());
    for s in supps {
        for h in &s.hrefs {
            let hb = url_basename(h);
            if hb == file || stem(&hb) == stem(&file) {
                return (s.label.clone(), s.caption.clone());
            }
        }
    }
    (String::new(), String::new())
}

/// First `cap` words of a Markdown table rendering, cut at a line boundary, with a descriptive header.
/// Returns (card text, total words in the table, truncated?).
pub fn table_card(file_name: &str, label: &str, caption: &str, md: &str, cap: usize) -> (String, usize, bool) {
    let total = md.split_whitespace().count();
    let lines: Vec<&str> = md.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut head = format!("Supplementary table: {file_name}");
    let lc = [label, caption].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" ");
    if !lc.is_empty() {
        head.push_str(&format!("\n{lc}"));
    }
    let rows = lines.iter().filter(|l| l.trim_start().starts_with('|') && !l.contains("---")).count();
    head.push_str(&format!("\n({rows} table lines, {total} words in full)\n"));
    let mut used = head.split_whitespace().count();
    let mut body = Vec::new();
    for l in &lines {
        let w = l.split_whitespace().count();
        if used + w > cap && !body.is_empty() {
            break;
        }
        used += w;
        body.push(*l);
    }
    let truncated = body.len() < lines.len();
    (format!("{head}{}", body.join("\n")), total, truncated)
}

/// Units for one converted supplement. `md` is the converter output.
#[allow(clippy::too_many_arguments)]
pub fn units(article_id: &str, url: &str, sha256: &str, class: Class, md: &str, label: &str, caption: &str,
             chunk: usize, overlap: usize, card_words: usize, max_words: usize) -> (Vec<Value>, Value) {
    let file_name = url_basename(url);
    let key = stable_id(&[json!(url)]);
    let base = |kind: &str| json!({"kind": kind, "source_url": url, "source_sha256": sha256, "media_key": null,
        "supplement_file": file_name, "supplement_label": label, "supplement_caption": caption,
        "tokenizer": COUNTER_NAME, "media_path": null});
    let mut out = Vec::new();
    match class {
        Class::Table => {
            let (text, total, truncated) = table_card(&file_name, label, caption, md, card_words);
            let mut u = base("table_card");
            let o = u.as_object_mut().unwrap();
            o.insert("unit_id".into(), json!(stable_id(&[json!(article_id), json!("table_card"), json!(key), json!(sha256), json!(card_words)])));
            o.insert("token_count".into(), json!(text.split_whitespace().count()));
            o.insert("text".into(), json!(text));
            o.insert("locator".into(), json!({"file_name": file_name, "table_words": total, "truncated": truncated}));
            o.insert("chunk_recipe".into(), json!({"card_words": card_words}));
            out.push(u);
            (out, json!({"class": "table", "words": total, "units": 1, "truncated": truncated}))
        }
        Class::Prose => {
            // Cap absurdly long "prose" (e.g. a gene list exported to .docx) at max_words.
            let total = md.split_whitespace().count();
            let text: String = if total > max_words {
                let mut n = 0;
                let mut end = md.len();
                for (i, _) in md.match_indices(char::is_whitespace) {
                    n += 1;
                    if n >= max_words {
                        end = i;
                        break;
                    }
                }
                md[..end].to_string()
            } else {
                md.to_string()
            };
            for (part, s, e, count) in split_text(&text, chunk, overlap) {
                let mut u = base("supplement_text");
                let o = u.as_object_mut().unwrap();
                o.insert("unit_id".into(), json!(stable_id(&[json!(article_id), json!("supplement"), json!(key), json!(sha256),
                    json!(s), json!(e), json!(part), json!(COUNTER_NAME), json!(chunk), json!(overlap)])));
                o.insert("text".into(), json!(part));
                o.insert("token_count".into(), json!(count));
                o.insert("locator".into(), json!({"file_name": file_name, "char_start": s, "char_end": e}));
                o.insert("chunk_recipe".into(), json!({"budget": chunk, "overlap": overlap}));
                out.push(u);
            }
            let n = out.len();
            (out, json!({"class": "prose", "words": total, "units": n, "truncated": total > max_words}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tabular_txt_and_cards() {
        assert!(looks_tabular("a\tb\tc\n1\t2\t3\n4\t5\t6\n7\t8\t9\n1\t1\t1\n"));
        assert!(!looks_tabular("Methods.\nWe did things.\nThen more.\nAnd more.\nEnd.\n"));
        let md = "| gene | p |\n|---|---|\n| A | 0.1 |\n| B | 0.2 |\n| C | 0.3 |\n";
        let (card, total, trunc) = table_card("t1.xlsx", "Table S1", "P values.", md, 30);
        assert!(card.starts_with("Supplementary table: t1.xlsx\nTable S1 P values.\n(4 table lines"));
        assert!(total > 0 && trunc);
        let (all, _, t2) = table_card("t1.xlsx", "", "", md, 10_000);
        assert!(!t2 && all.contains("| C | 0.3 |"));
    }
    #[test]
    fn prose_units_capped() {
        let md = "word ".repeat(1000);
        let (u, info) = units("PMC1.1", "s3://b/PMC1.1/x.docx?md5=0", "h", Class::Prose, &md, "", "", 512, 64, 2000, 600);
        assert_eq!(info["truncated"], true);
        assert_eq!(info["words"], 1000);
        assert_eq!(u.len(), 2);
        assert!(u.iter().all(|x| x["kind"] == "supplement_text"));
    }
}
