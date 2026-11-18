//! PubMed baseline/update citation change log. Port of `documents.py::iter_pubmed` + `pipeline.py::run_pubmed`.
//! Fix vs Python: article IDs come only from `PubmedData/ArticleIdList`; Python read every
//! `.//ArticleId`, so IDs of *cited references* (later in the record) overwrote the article's own
//! PMC ID / DOI.
use crate::bundle::{Budget, BundleWriter};
use crate::common::{atomic_json, collapse_ws, stable_id};
use crate::net::Http;
use crate::pipeline::Cfg;
use anyhow::{bail, Context, Result};
use quick_xml::events::Event;
use roxmltree::{Document, Node};
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::Path;

fn text(n: Option<Node>) -> String {
    n.map(|n| collapse_ws(&n.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect::<String>()))
        .unwrap_or_default()
}
fn kid<'a, 'i>(n: Node<'a, 'i>, t: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == t)
}
fn desc<'a, 'i>(n: Node<'a, 'i>, t: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.descendants().skip(1).filter(move |c| c.is_element() && c.tag_name().name() == t)
}
fn findtext(n: Node, t: &str) -> Value {
    match kid(n, t) {
        Some(c) => json!(c.text().unwrap_or("")),
        None => Value::Null,
    }
}

/// Split the decompressed file into top-level records (byte ranges) with a streaming reader.
fn records(xml: &str) -> Result<Vec<(usize, usize)>> {
    let mut r = quick_xml::Reader::from_str(xml);
    r.config_mut().check_end_names = false;
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    loop {
        let pos = r.buffer_position() as usize;
        match r.read_event()? {
            Event::Start(e) => {
                depth += 1;
                if depth == 2 && matches!(e.name().as_ref(), b"PubmedArticle" | b"PubmedBookArticle" | b"DeleteCitation") {
                    start = Some(pos);
                }
            }
            Event::End(_) => {
                if depth == 2 {
                    if let Some(s) = start.take() {
                        out.push((s, r.buffer_position() as usize));
                    }
                }
                depth -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn rescue(s: &str) -> String {
    // Records are parsed without the external DTD; map a few named entities, escape the rest.
    let re = regex::Regex::new(r"&([A-Za-z][A-Za-z0-9]*);").unwrap();
    re.replace_all(s, |c: &regex::Captures| match &c[1] {
        "amp" | "lt" | "gt" | "quot" | "apos" => c[0].to_string(),
        "nbsp" => "\u{a0}".into(),
        o => format!("&amp;{o};"),
    })
    .into_owned()
}

pub fn iter_pubmed(xml: &str, source: &Value) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for (within, (a, b)) in records(xml)?.into_iter().enumerate() {
        let frag = rescue(&xml[a..b]);
        let doc = Document::parse(&frag).with_context(|| format!("record {within}"))?;
        let el = doc.root_element();
        let mut common = Map::new();
        common.insert("source".into(), json!("pubmed"));
        common.insert("source_file".into(), source["name"].clone());
        common.insert("source_sequence".into(), source["sequence"].clone());
        common.insert("phase".into(), source["phase"].clone());
        common.insert("within_file".into(), json!(within));
        common.insert("license".into(), json!("PUBMED-CITATION-TERMS"));
        common.insert("export_approved".into(), json!(false));
        if el.tag_name().name() == "DeleteCitation" {
            for p in el.children().filter(|c| c.is_element() && c.tag_name().name() == "PMID") {
                let mut r = common.clone();
                r.insert("op".into(), json!("delete"));
                r.insert("pmid".into(), json!(p.text().unwrap_or("")));
                rows.push(Value::Object(r));
            }
            continue;
        }
        let pmid = desc(el, "PMID").next().and_then(|p| p.text()).filter(|s| !s.is_empty()).context("PubMed record without PMID")?;
        let mut ids = Map::new();
        if let Some(list) = kid(el, "PubmedData").and_then(|d| kid(d, "ArticleIdList"))
            .or_else(|| kid(el, "BookDocument").and_then(|d| kid(d, "ArticleIdList")))
        {
            for x in list.children().filter(|c| c.is_element() && c.tag_name().name() == "ArticleId") {
                if let Some(t) = x.attribute("IdType") {
                    ids.insert(t.to_string(), json!(x.text()));
                }
            }
        }
        let mc = kid(el, "MedlineCitation");
        let mut r = common;
        r.insert("op".into(), json!("upsert"));
        r.insert("pmid".into(), json!(pmid));
        r.insert("pmcid".into(), ids.get("pmc").cloned().unwrap_or(Value::Null));
        r.insert("doi".into(), ids.get("doi").cloned().unwrap_or(Value::Null));
        r.insert("title".into(), json!(text(desc(el, "ArticleTitle").next())));
        r.insert("publication_dates".into(), json!(desc(el, "PubDate").map(|d| json!({"year": findtext(d, "Year"),
            "month": findtext(d, "Month"), "day": findtext(d, "Day"), "medline_date": findtext(d, "MedlineDate")})).collect::<Vec<_>>()));
        r.insert("abstract".into(), json!(desc(el, "AbstractText").map(|a| json!({"label": a.attribute("Label"), "text": text(Some(a))})).collect::<Vec<_>>()));
        r.insert("mesh".into(), json!(desc(el, "MeshHeading").filter_map(|m| kid(m, "DescriptorName")).map(|d| text(Some(d))).collect::<Vec<_>>()));
        r.insert("publication_types".into(), json!(desc(el, "PublicationType").map(|d| text(Some(d))).collect::<Vec<_>>()));
        r.insert("citation_status".into(), json!(mc.and_then(|m| m.attribute("Status"))));
        r.insert("retraction_links".into(), json!(desc(el, "CommentsCorrections").map(|c| json!({"type": c.attribute("RefType"), "pmid": kid(c, "PMID").and_then(|p| p.text())})).collect::<Vec<_>>()));
        rows.push(Value::Object(r));
    }
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
pub async fn run(sources: &[Value], worker: &Path, run_id: &str, manifest_hash: &str, cursor: usize, cfg: &Cfg, budget: &Budget, http: &Http) -> Result<Value> {
    if sources.len() != 1 {
        bail!("PubMed plans require one source archive per partition");
    }
    let source = &sources[0];
    let md5text = String::from_utf8(http.get(source["md5_url"].as_str().context("md5_url")?, 1 << 20).await?)?;
    let md5 = regex::Regex::new(r"\b[0-9a-fA-F]{32}\b").unwrap().find(&md5text).context("Cannot parse official PubMed MD5 file")?.as_str().to_lowercase();
    let tmp = tempfile::Builder::new().prefix("pigmed-pubmed-").tempdir()?;
    let path = tmp.path().join(source["name"].as_str().context("name")?);
    http.download(source["url"].as_str().context("url")?, &path, 2 << 30, Some(&md5)).await?;
    let p2 = path.clone();
    let src = source.clone();
    let rows = tokio::task::spawn_blocking(move || -> Result<Vec<Value>> {
        let mut s = String::new();
        flate2::read::MultiGzDecoder::new(std::fs::File::open(&p2)?).read_to_string(&mut s)?;
        iter_pubmed(&s, &src)
    })
    .await??;
    let ready = worker.join("ready");
    let mut writer: Option<BundleWriter> = None;
    let mut first = cursor;
    let count = rows.len();
    let name = source["name"].as_str().unwrap().to_string();
    let close = |w: &mut Option<BundleWriter>, first: &mut usize, next: usize| -> Result<()> {
        if let Some(mut bw) = w.take() {
            let cp = json!({"run_id": run_id, "manifest_sha256": manifest_hash, "next_index": next, "first_index": *first, "bundle": bw.name});
            let committed = bw.finish(cp.clone())?;
            budget.settle(&bw.name, &committed)?;
            atomic_json(&worker.join("state.json"), &cp)?;
            *first = next;
        }
        Ok(())
    };
    for (index, row) in rows.into_iter().enumerate().skip(cursor) {
        if writer.is_none() {
            let bname = format!("{run_id}-{index:09}");
            budget.reserve(&bname, cfg.target_shard_bytes + cfg.max_article_bytes)?;
            writer = Some(BundleWriter::new(&ready, &bname)?);
        }
        let mut r = row.as_object().unwrap().clone();
        let pmid = r["pmid"].clone();
        r.insert("unit_id".into(), json!(stable_id(&[json!(name), json!(index), pmid])));
        r.insert("kind".into(), json!("citation"));
        let w = writer.as_mut().unwrap();
        w.add(r)?;
        if index + 1 - first >= 10000 || w.size()? >= cfg.target_shard_bytes {
            close(&mut writer, &mut first, index + 1)?;
        }
    }
    close(&mut writer, &mut first, count)?;
    let r = json!({"complete": true, "source": name, "records": count,
        "export_policy": "PubMed abstract/citation redistribution requires explicit review", "run_id": run_id, "writer": "pigmed-rs"});
    atomic_json(&worker.join("complete.json"), &r)?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn own_ids_not_reference_ids() {
        let xml = r#"<?xml version="1.0"?><!DOCTYPE PubmedArticleSet PUBLIC "-//NLM//DTD PubMedArticle, 1st January 2025//EN" "x.dtd">
<PubmedArticleSet>
<PubmedArticle><MedlineCitation Status="MEDLINE"><PMID Version="1">111</PMID><Article><ArticleTitle>Own &amp; title&nbsp;x</ArticleTitle>
<Abstract><AbstractText Label="AIM">Text</AbstractText></Abstract></Article>
<CommentsCorrectionsList><CommentsCorrections RefType="RetractionIn"><PMID>999</PMID></CommentsCorrections></CommentsCorrectionsList></MedlineCitation>
<PubmedData><ArticleIdList><ArticleId IdType="pubmed">111</ArticleId><ArticleId IdType="pmc">PMC1000</ArticleId><ArticleId IdType="doi">10.1/own</ArticleId></ArticleIdList>
<ReferenceList><Reference><Citation>Cited</Citation><ArticleIdList><ArticleId IdType="pmc">PMC9999</ArticleId><ArticleId IdType="doi">10.1/cited</ArticleId></ArticleIdList></Reference></ReferenceList>
</PubmedData></PubmedArticle>
<DeleteCitation><PMID Version="1">222</PMID><PMID Version="1">333</PMID></DeleteCitation>
</PubmedArticleSet>"#;
        let rows = iter_pubmed(xml, &json!({"name": "pubmed26n0001.xml.gz", "sequence": 1, "phase": "baseline"})).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["pmcid"], "PMC1000");
        assert_eq!(rows[0]["doi"], "10.1/own");
        assert_eq!(rows[0]["title"], "Own & title\u{a0}x".replace('\u{a0}', " "));
        assert_eq!(rows[0]["retraction_links"][0]["type"], "RetractionIn");
        assert_eq!(rows[1]["op"], "delete");
        assert_eq!(rows[2]["pmid"], "333");
        assert_eq!(rows[2]["within_file"], 1);
    }
}
