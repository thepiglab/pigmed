//! Figure track: a typed sample of PMC figures (image + caption + citing paragraphs + title/abstract) for
//! multimodal query generation, and figure -> citing-chunk pairs.
//!
//! Figure types come from the caption (charts, microscopy, radiology/clinical images, diagrams; multi-panel when
//! the caption labels three or more panels). One figure per article, commercial-licence train articles only,
//! round-robin over e-means level-1 clusters inside each type. Images are fetched from the public PMC Open Data
//! bucket and resized so the longest side is at most `max_side` pixels (comparable visual prefill across models).

use crate::io::read_jsonl;
use crate::sample::{hash, round_robin};
use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const FIG_QUERY_TYPES: [&str; 4] = ["visual", "clinician", "image_only", "keyword"];
pub const FIG_TYPES: [&str; 5] = ["chart", "microscopy", "radiology_clinical", "diagram", "multipanel"];

/// SampleChunk-compatible (chunk_id = figure unit id, text = caption, section = figure type), plus figure fields.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct FigureSample {
    pub chunk_id: String,
    pub article_id: String,
    pub cluster: String,
    pub title: String,
    pub abstract_text: String,
    pub section: String,
    pub text: String,
    pub words: usize,
    #[serde(default)]
    pub marker: String,
    pub figure_id: String,
    pub image: String,
    pub citing: Vec<String>,
    pub citing_chunk: Option<String>,
}

fn re(cell: &'static OnceLock<Regex>, p: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(p).unwrap())
}

/// Figure type from its caption.
pub fn figure_type(caption: &str) -> Option<&'static str> {
    static PANEL: OnceLock<Regex> = OnceLock::new();
    static RAD: OnceLock<Regex> = OnceLock::new();
    static MIC: OnceLock<Regex> = OnceLock::new();
    static DIA: OnceLock<Regex> = OnceLock::new();
    static CHA: OnceLock<Regex> = OnceLock::new();
    let panels: HashSet<String> = re(&PANEL, r"\(([A-H])\)|\b([A-H])[).:]\s|\bPanel ([A-H])\b").captures_iter(caption)
        .filter_map(|c| c.get(1).or(c.get(2)).or(c.get(3)).map(|m| m.as_str().to_string())).collect();
    if panels.len() >= 3 {
        return Some("multipanel");
    }
    if re(&RAD, r"(?i)\b(CT|MRI|MR image|radiograph|X-ray|x ray|ultrasound|ultrasonograph|echocardiogra|angiogra|PET|endoscop|fundus|OCT|dermoscop|photograph|intraoperative|mammogra|clinical (?:photo|image|appearance))").is_match(caption) {
        return Some("radiology_clinical");
    }
    if re(&MIC, r"(?i)microscop|staining|stained|H&E|hematoxylin|haematoxylin|immunohisto|immunofluoresc|confocal|scale bar|magnification|histolog|\bTEM\b|\bSEM\b").is_match(caption) {
        return Some("microscopy");
    }
    if re(&DIA, r"(?i)schematic|diagram|flow ?chart|workflow|illustration|pathway|framework|study design|CONSORT|PRISMA|overview of|model of").is_match(caption) {
        return Some("diagram");
    }
    if re(&CHA, r"(?i)\bplot|graph|curve|\bbar|histogram|scatter|kaplan|survival|forest|\bROC\b|box ?plot|heat ?map|distribution|correlation|trend").is_match(caption) {
        return Some("chart");
    }
    None
}

fn find_items(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(root)? {
        let p = e?.path();
        if p.is_dir() {
            find_items(&p, out)?;
        } else if p.file_name().is_some_and(|n| n == "items.jsonl.gz") {
            out.push(p);
        }
    }
    Ok(())
}

fn ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Paragraphs of the JATS body that cite `figure_id` (xref ref-type="fig" with that rid).
pub fn citing_paragraphs(xml: &str, figure_id: &str, max: usize) -> Vec<String> {
    let opts = roxmltree::ParsingOptions { allow_dtd: true, nodes_limit: u32::MAX };
    let Ok(doc) = roxmltree::Document::parse_with_options(xml, opts) else { return vec![] };
    let mut out = vec![];
    for p in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "p") {
        if p.ancestors().skip(1).any(|a| a.is_element() && matches!(a.tag_name().name(), "fig" | "caption" | "table-wrap")) {
            continue;
        }
        let cites = p.descendants().any(|x| x.is_element() && x.tag_name().name() == "xref"
            && x.attribute("ref-type") == Some("fig")
            && x.attribute("rid").is_some_and(|r| r.split_whitespace().any(|id| id == figure_id)));
        if cites {
            let t: String = p.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect();
            let t = ws(&t);
            if !t.is_empty() {
                out.push(t.split_whitespace().take(200).collect::<Vec<_>>().join(" "));
            }
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

fn fetch_resize(agent: &ureq::Agent, url: &str, dest: &Path, max_side: u32) -> Result<()> {
    if dest.exists() {
        return Ok(());
    }
    let key = url.strip_prefix("s3://pmc-oa-opendata/").context("not a PMC bucket url")?;
    let key = key.split('?').next().unwrap_or(key);
    let https = format!("https://pmc-oa-opendata.s3.amazonaws.com/{key}");
    let mut body = vec![];
    std::io::Read::read_to_end(&mut agent.get(&https).call()?.into_reader(), &mut body)?;
    let img = image::load_from_memory(&body).context("decode")?;
    let img = if img.width().max(img.height()) > max_side { img.resize(max_side, max_side, image::imageops::FilterType::Lanczos3) } else { img };
    let tmp = dest.with_extension("tmp");
    img.to_rgb8().save_with_format(&tmp, image::ImageFormat::Jpeg)?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

pub struct FigArgs<'a> {
    pub manifest: &'a Path,
    pub records: &'a Path,
    pub atlas: &'a Path,
    pub items_root: &'a Path,
    pub xml_cache: &'a Path,
    pub image_dir: &'a Path,
    pub image_ids: &'a HashSet<String>,
    pub per_type: usize,
    pub max_side: u32,
    pub seed: &'a str,
}

/// Returns the figure sample and figure -> citing-chunk pairs.
pub fn run(a: &FigArgs) -> Result<(Vec<FigureSample>, Vec<Value>)> {
    std::fs::create_dir_all(a.image_dir)?;
    let split: HashMap<String, String> = read_jsonl::<Value>(a.manifest)?.into_iter()
        .filter_map(|r| Some((r["article_id"].as_str()?.to_string(), r["split"].as_str()?.to_string()))).collect();
    let atlas: HashMap<String, Vec<i64>> = serde_json::from_str(&std::fs::read_to_string(a.atlas)?)?;
    let ok_article: HashSet<String> = read_jsonl::<Value>(a.records)?.into_iter().filter_map(|r| {
        let aid = r["article_id"].as_str()?.to_string();
        (split.get(&aid).is_some_and(|s| s == "train") && r["commercial"].as_bool() == Some(true)
            && r["has_abstract"].as_bool() == Some(true)).then_some(aid)
    }).collect();
    let mut files = vec![];
    find_items(a.items_root, &mut files)?;
    files.sort();
    // pass 1: candidate figures per type; abstracts and text units per article (for context and citing chunks)
    let mut cands: BTreeMap<&str, BTreeMap<String, Vec<String>>> = FIG_TYPES.iter().map(|t| (*t, BTreeMap::new())).collect();
    let mut figs: HashMap<String, (String, String, String, String, String)> = HashMap::new(); // unit -> (article, title, caption, figure_id, url)
    let mut abstracts: HashMap<String, Vec<String>> = HashMap::new();
    let mut texts: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for f in &files {
        for r in read_jsonl::<Value>(f)? {
            let Some(aid) = r["article_id"].as_str() else { continue };
            if !ok_article.contains(aid) {
                continue;
            }
            match r["kind"].as_str().unwrap_or("") {
                "figure" => {
                    let uid = r["unit_id"].as_str().unwrap_or("").to_string();
                    let cap = r["caption"].as_str().unwrap_or("").to_string();
                    let url = r["source_url"].as_str().unwrap_or("").to_string();
                    let fid = r["locator"]["figure_id"].as_str().unwrap_or("").to_string();
                    let ext = url.split('?').next().unwrap_or("").rsplit('.').next().unwrap_or("").to_lowercase();
                    if !a.image_ids.contains(&uid) || cap.split_whitespace().count() < 8 || fid.is_empty()
                        || !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp") {
                        continue;
                    }
                    if let (Some(t), Some(c)) = (figure_type(&cap), atlas.get(aid).and_then(|p| p.get(1))) {
                        cands.get_mut(t).unwrap().entry(format!("L1-{c}")).or_default().push(uid.clone());
                        figs.insert(uid, (aid.to_string(), r["title"].as_str().unwrap_or("").to_string(), cap, fid, url));
                    }
                }
                "text" => {
                    let sec = r["locator"]["section"].as_array().map(|s| s.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" > ")).unwrap_or_default();
                    let t = r["text"].as_str().unwrap_or("").to_string();
                    if sec.to_lowercase().starts_with("abstract") {
                        abstracts.entry(aid.to_string()).or_default().push(t);
                    } else {
                        texts.entry(aid.to_string()).or_default().push((r["unit_id"].as_str().unwrap_or("").to_string(), t));
                    }
                }
                _ => {}
            }
        }
    }
    // pick per type, one figure per article overall
    let mut used_articles = HashSet::new();
    let mut picked: Vec<(String, &str, String)> = vec![];
    for t in FIG_TYPES {
        let order = round_robin(cands[t].clone(), &format!("{}-{t}", a.seed), a.per_type * 4);
        let mut n = 0;
        for (uid, cluster) in order {
            let art = &figs[&uid].0;
            if n >= a.per_type * 3 / 2 || used_articles.contains(art) {
                continue;
            }
            used_articles.insert(art.clone());
            picked.push((uid, t, cluster));
            n += 1;
        }
    }
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(60)).user_agent("PigMed/0.1 (figure sample)").build();
    let mut out = vec![];
    let mut pairs = vec![];
    let mut per_type: HashMap<&str, usize> = HashMap::new();
    for (uid, t, cluster) in picked {
        if per_type.get(t).copied().unwrap_or(0) >= a.per_type {
            continue;
        }
        let (aid, title, cap, fid, url) = figs[&uid].clone();
        let xml_path = a.xml_cache.join(format!("{aid}.xml.gz"));
        let citing = match std::fs::read(&xml_path) {
            Ok(b) => citing_paragraphs(&String::from_utf8_lossy(&crate::io::gunzip(&b)?), &fid, 2),
            Err(_) => vec![],
        };
        if citing.is_empty() {
            continue;
        }
        let dest = a.image_dir.join(format!("{uid}.jpg"));
        if let Err(e) = fetch_resize(&agent, &url, &dest, a.max_side) {
            eprintln!("image {uid}: {e:#}");
            continue;
        }
        // citing chunk: the article's text unit containing the start of the first citing paragraph
        let probe: String = citing[0].split_whitespace().take(10).collect::<Vec<_>>().join(" ");
        let citing_chunk = texts.get(&aid).and_then(|v| v.iter().find(|(_, t)| ws(t).contains(&probe)).map(|(u, _)| u.clone()));
        if let Some(c) = &citing_chunk {
            pairs.push(serde_json::json!({"query_figure": uid, "positive_chunk": c, "article_id": aid, "figure_type": t}));
        }
        let abstract_text: String = abstracts.get(&aid).map(|v| v.join(" ")).unwrap_or_default().split_whitespace().take(250).collect::<Vec<_>>().join(" ");
        out.push(FigureSample { chunk_id: uid, article_id: aid, cluster, title, abstract_text, section: t.to_string(),
            words: cap.split_whitespace().count(), text: cap, marker: String::new(), figure_id: fid,
            image: dest.to_string_lossy().to_string(), citing, citing_chunk });
        *per_type.entry(t).or_default() += 1;
    }
    out.sort_by_key(|f| hash(&[a.seed, &f.chunk_id]));
    if out.is_empty() {
        bail!("no figures sampled");
    }
    Ok((out, pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figure_types_from_captions() {
        assert_eq!(figure_type("(A) Survival. (B) Hazard. (C) Counts."), Some("multipanel"));
        assert_eq!(figure_type("Axial CT image showing a mass in the liver"), Some("radiology_clinical"));
        assert_eq!(figure_type("H&E staining of tumour tissue, scale bar 50 µm"), Some("microscopy"));
        assert_eq!(figure_type("Schematic of the study design"), Some("diagram"));
        assert_eq!(figure_type("Kaplan-Meier survival curves by group"), Some("chart"));
        assert_eq!(figure_type("Participants."), None);
    }

    #[test]
    fn finds_citing_paragraphs() {
        let xml = r#"<article><body><p>See <xref ref-type="fig" rid="F1">Fig. 1</xref> for details.</p>
<p>Unrelated <xref ref-type="fig" rid="F2">Fig. 2</xref>.</p><fig id="F1"><caption><p>cap <xref ref-type="fig" rid="F1"/></p></caption></fig>
<p>Both <xref ref-type="fig" rid="F1 F2">Figs. 1-2</xref>.</p></body></article>"#;
        let c = citing_paragraphs(xml, "F1", 5);
        assert_eq!(c, vec!["See Fig. 1 for details.".to_string(), "Both Figs. 1-2.".to_string()]);
    }
}
