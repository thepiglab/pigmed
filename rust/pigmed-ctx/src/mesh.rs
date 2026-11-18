//! Per-article MeSH table for recipe v3.1e (pigmed-querygen `--article-meta`): MeSH descriptor UIs of each article's
//! PubMed record (e-means meta-v2 PubMed rows) mapped to tree numbers with NLM's descriptor file (descYYYY.xml).
//! Output row: {"article_id", "pmid", "mesh_trees": ["C04.557.470", ...], "journal", "nlm_id", "pub_types"}.

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::Path;

/// DescriptorUI -> tree numbers, from NLM descYYYY.xml (streamed line by line; the file is large).
pub fn read_desc(path: &Path) -> Result<HashMap<String, Vec<String>>> {
    let ui = Regex::new(r"<DescriptorUI>(D\d+)</DescriptorUI>").unwrap();
    let tn = Regex::new(r"<TreeNumber>([A-Z]\d{2}(?:\.\d{3})*)</TreeNumber>").unwrap();
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let (mut cur, mut in_record) = (String::new(), false);
    for line in crate::io::reader(path)?.lines() {
        let line = line?;
        if line.contains("<DescriptorRecord ") || line.contains("<DescriptorRecord>") {
            in_record = true;
            cur.clear();
        } else if line.contains("</DescriptorRecord>") {
            in_record = false;
        }
        if !in_record {
            continue;
        }
        // the record's own UI is its first DescriptorUI (later ones belong to cross-references)
        if cur.is_empty() {
            if let Some(c) = ui.captures(&line) {
                cur = c[1].to_string();
                continue;
            }
        }
        if let Some(c) = tn.captures(&line) {
            out.entry(cur.clone()).or_default().push(c[1].to_string());
        }
    }
    Ok(out)
}

/// Several descriptor years, newest first: UI -> (tree numbers, retired). A UI absent from the newest file (a retired
/// descriptor) takes its trees from the newest older year that still has it, and is flagged.
pub struct Trees {
    pub map: HashMap<String, (Vec<String>, bool)>,
    pub retired: usize,
}

pub fn read_descs(paths: &[std::path::PathBuf]) -> Result<Trees> {
    let mut map: HashMap<String, (Vec<String>, bool)> = HashMap::new();
    let mut retired = 0;
    for (k, p) in paths.iter().enumerate() {
        for (ui, t) in read_desc(p)? {
            if !map.contains_key(&ui) {
                retired += (k > 0) as usize;
                map.insert(ui, (t, k > 0));
            }
        }
    }
    Ok(Trees { map, retired })
}

/// pmid -> (descriptor UIs, journal, pub_types) for the wanted PMIDs, from PubMed JSONL(.gz) parts.
pub fn read_pubmed(dir: &Path, want: &HashSet<String>) -> Result<HashMap<String, (Vec<String>, String, Vec<String>)>> {
    let ui = Regex::new(r"'ui':\s*'(D\d+)'|\x22ui\x22:\s*\x22(D\d+)\x22").unwrap();
    let mut parts: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".jsonl.gz") || p.to_string_lossy().ends_with(".jsonl")).collect();
    parts.sort();
    let mut out = HashMap::new();
    for p in parts {
        for line in crate::io::reader(&p)?.lines() {
            let line = line?;
            let r: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
            let pmid = match &r["pmid"] { Value::Number(n) => n.to_string(), Value::String(s) => s.clone(), _ => continue };
            if !want.contains(&pmid) {
                continue;
            }
            let mesh = match &r["mesh"] { Value::String(s) => s.clone(), v => v.to_string() };
            let uis = ui.captures_iter(&mesh).filter_map(|c| c.get(1).or(c.get(2)).map(|m| m.as_str().to_string())).collect();
            let pt = r["pub_types"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
            // later rows (update files) replace earlier ones
            out.insert(pmid, (uis, r["journal"].as_str().unwrap_or("").to_string(), pt));
        }
    }
    Ok(out)
}

/// querygen v3.1e [domain]: any tree under A B C D N G03..G12, or E together with another top-level tree.
pub fn biomedical_trees(t: &[String]) -> bool {
    let top = |x: &str| x.chars().next().unwrap_or(' ');
    let any = t.iter().any(|x| matches!(top(x), 'A' | 'B' | 'C' | 'D' | 'N')
        || (x.starts_with('G') && x.get(1..3).and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| (3..=12).contains(&n))));
    any || (t.iter().any(|x| x.starts_with('E')) && t.iter().any(|x| !x.starts_with('E')))
}

/// Stricter rule for the journal list: A B C N or G03..G12; D (chemicals) and E (techniques) count only together with
/// one of those, so chemistry journals whose papers carry only chemical descriptors stay out.
pub fn biomedical_trees_strict(t: &[String]) -> bool {
    let top = |x: &str| x.chars().next().unwrap_or(' ');
    t.iter().any(|x| matches!(top(x), 'A' | 'B' | 'C' | 'N')
        || (x.starts_with('G') && x.get(1..3).and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| (3..=12).contains(&n))))
}

/// Multidisciplinary mega-journals: never on the journal list (their articles are decided by MeSH, not by the venue).
pub const MEGA_JOURNALS: [&str; 18] = ["scientific reports", "plos one", "nature communications", "proceedings of the national academy of sciences",
    "peerj", "heliyon", "royal society open science", "acs omega", "rsc advances", "science advances", "iscience", "nature", "science",
    "f1000research", "sage open", "plos biology", "cell reports", "patterns"];

fn mega(title: &str) -> bool {
    let t = title.to_lowercase();
    t.starts_with("proceedings of the national academy of sciences")
        || MEGA_JOURNALS.iter().any(|m| t == *m || t.starts_with(&format!("{m} (")) || t.starts_with(&format!("{m}.")) || t.starts_with(&format!("{m} :")))
}

/// querygen v3.1e [lay]: (C or D tree and a clinical publication type) or an N02 / N05 tree.
pub fn patient_relevant(t: &[String], pub_types: &[String]) -> bool {
    const CLIN: [&str; 6] = ["clinical trial", "randomized controlled trial", "review", "systematic review", "case reports", "practice guideline"];
    let cd = t.iter().any(|x| x.starts_with('C') || x.starts_with('D'));
    let clin = pub_types.iter().any(|p| CLIN.contains(&p.to_lowercase().as_str()));
    (cd && clin) || t.iter().any(|x| x.starts_with("N02") || x.starts_with("N05"))
}

/// Biomedical journal list from the data itself: journals with >= min_n PubMed articles of which >= min_share have
/// MeSH under the biomedical trees. One full title per line (as PubMed spells it), for querygen's journal_list.
pub fn journal_list(pubmed: &Path, desc: &[std::path::PathBuf], records: Option<&Path>, min_n: usize, min_share: f64, out: &Path) -> Result<Value> {
    let trees = read_descs(desc)?.map;
    // PubMed spells journals as MEDLINE abbreviations (nlm_ta); the corpus records carry both the abbreviation and the
    // full title, so the list can be written in full titles (what --article-meta's `journal` holds)
    let mut full: HashMap<String, HashMap<String, usize>> = HashMap::new();
    if let Some(r) = records {
        for line in crate::io::reader(r)?.lines() {
            let line = line?;
            let v: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
            let (ta, j) = (v["nlm_ta"].as_str().unwrap_or("").trim(), v["journal"].as_str().unwrap_or("").trim());
            if !ta.is_empty() && !j.is_empty() {
                *full.entry(ta.to_lowercase()).or_default().entry(j.to_string()).or_default() += 1;
            }
        }
    }
    let title = |abbrev: &str| full.get(&abbrev.to_lowercase()).and_then(|m| m.iter().max_by_key(|(_, n)| **n).map(|(t, _)| t.clone()));
    let ui = Regex::new(r"'ui':\s*'(D\d+)'|\x22ui\x22:\s*\x22(D\d+)\x22").unwrap();
    let mut parts: Vec<_> = std::fs::read_dir(pubmed)?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().contains(".jsonl")).collect();
    parts.sort();
    let mut by: HashMap<String, (usize, usize)> = HashMap::new(); // journal -> (with mesh, biomedical)
    for p in parts {
        for line in crate::io::reader(&p)?.lines() {
            let line = line?;
            let r: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
            let j = r["journal"].as_str().unwrap_or("").trim();
            if j.is_empty() {
                continue;
            }
            let mesh = match &r["mesh"] { Value::String(s) => s.clone(), v => v.to_string() };
            let t: Vec<String> = ui.captures_iter(&mesh).filter_map(|c| c.get(1).or(c.get(2)))
                .flat_map(|m| trees.get(m.as_str()).map(|x| x.0.clone()).unwrap_or_default()).collect();
            if t.is_empty() {
                continue;
            }
            let e = by.entry(j.to_string()).or_default();
            e.0 += 1;
            e.1 += biomedical_trees_strict(&t) as usize;
        }
    }
    let mut keep: Vec<(&String, &(usize, usize))> = by.iter().filter(|(_, (n, b))| *n >= min_n && *b as f64 >= min_share * *n as f64).collect();
    keep.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let mut text = format!("# Biomedical journals for querygen [domain].journal_list, derived from PubMed: journals with >= {min_n} \
MeSH-indexed articles of which >= {:.0}% carry MeSH under A B C N G03-G12 (D chemicals and E techniques count only with one of those); multidisciplinary mega-journals excluded.\n# Full titles as PubMed \
spells them; matched case-insensitively.\n", min_share * 100.0);
    let mut titled = 0usize;
    let mut lines: Vec<String> = keep.iter().map(|(j, _)| match title(j) { Some(t) => { titled += 1; t } None => j.to_string() })
        .filter(|t| !mega(t)).collect();
    lines.sort_by_key(|x| x.to_lowercase());
    lines.dedup_by_key(|x| x.to_lowercase());
    for l in &lines {
        text.push_str(l);
        text.push('\n');
    }
    std::fs::write(out, text)?;
    Ok(json!({"journals_seen": by.len(), "journals_listed": lines.len(), "full_titles": titled, "min_n": min_n, "min_share": min_share}))
}

pub fn build(articles: &Path, records: &Path, pubmed: &Path, desc: &[std::path::PathBuf], out: &Path) -> Result<Value> {
    // wanted articles (one per line: JSON with article_id, or a bare id)
    let mut want_art: Vec<String> = vec![];
    for line in crate::io::reader(articles)?.lines() {
        let line = line?;
        // JSONL with article_id, or a TSV / plain list whose first field is the id (keep.tsv)
        let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v["article_id"].as_str().map(str::to_string))
            .unwrap_or_else(|| line.split('\t').next().unwrap_or("").trim().to_string());
        if !id.is_empty() {
            want_art.push(id);
        }
    }
    let wa: HashSet<&str> = want_art.iter().map(String::as_str).collect();
    // article -> (pmid, journal, record pub_types) from the corpus records
    let mut rec: HashMap<String, (String, String, Vec<String>)> = HashMap::new();
    for line in crate::io::reader(records).context("records")?.lines() {
        let line = line?;
        let Some(id) = line.get(15..).and_then(|x| x.split('"').next()) else { continue };
        if !wa.contains(id) {
            continue;
        }
        let r: Value = serde_json::from_str(&line)?;
        rec.insert(id.to_string(), (r["pmid"].as_str().unwrap_or("").to_string(), r["journal"].as_str().unwrap_or("").to_string(),
            r["pub_types"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()));
    }
    let pmids: HashSet<String> = rec.values().map(|x| x.0.clone()).filter(|p| !p.is_empty()).collect();
    let pm = read_pubmed(pubmed, &pmids)?;
    let td = read_descs(desc)?;
    let trees = &td.map;
    let mut w = crate::io::JsonlOut::create(out)?;
    let (mut with_mesh, mut no_pmid, mut unknown_ui, mut patient, mut bio) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut retired_hits, mut rows_retired) = (0usize, 0usize);
    for a in &want_art {
        let (pmid, jr, ptr) = rec.get(a).cloned().unwrap_or_default();
        if pmid.is_empty() {
            no_pmid += 1;
        }
        let (uis, jp, ptp) = pm.get(&pmid).cloned().unwrap_or_default();
        let mut t: Vec<String> = vec![];
        let mut rt: Vec<String> = vec![];
        for u in &uis {
            match trees.get(u) {
                Some((x, old)) => {
                    t.extend(x.iter().cloned());
                    if *old {
                        retired_hits += 1;
                        rt.extend(x.iter().cloned());
                    }
                }
                None => unknown_ui += 1,
            }
        }
        t.sort();
        t.dedup();
        rt.sort();
        rt.dedup();
        rows_retired += (!rt.is_empty()) as usize;
        with_mesh += (!t.is_empty()) as usize;
        let pub_types = if ptp.is_empty() { ptr } else { ptp };
        patient += patient_relevant(&t, &pub_types) as usize;
        bio += biomedical_trees(&t) as usize;
        w.put(&json!({"article_id": a, "pmid": pmid, "mesh_trees": t, "journal": if jr.is_empty() { jp } else { jr },
            "nlm_id": Value::Null, "pub_types": pub_types, "retired_trees": rt.len()}))?;
    }
    let n = w.finish()?;
    let share = |x: usize| (x as f64 / n.max(1) as f64 * 1000.0).round() / 1000.0;
    Ok(json!({"articles": n, "with_mesh_trees": with_mesh, "share_with_mesh": share(with_mesh), "biomedical_by_mesh": share(bio),
        "patient_relevant": patient, "share_patient_relevant": share(patient), "no_pmid": no_pmid,
        "pubmed_rows_found": pm.len(), "unknown_descriptor_uis": unknown_ui, "retired_descriptor_hits": retired_hits,
        "rows_with_retired_trees": rows_retired, "descriptors": trees.len(), "retired_descriptors_mapped": td.retired,
        "desc_files": desc.iter().map(|p| p.display().to_string().rsplit('/').next().unwrap_or("").to_string()).collect::<Vec<_>>()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_pubmed_mesh_to_trees() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("desc.xml"), "<DescriptorRecordSet>\n<DescriptorRecord DescriptorClass=\"1\">\n<DescriptorUI>D000818</DescriptorUI>\n<DescriptorName><String>Animals</String></DescriptorName>\n<TreeNumberList>\n<TreeNumber>B01.050</TreeNumber>\n</TreeNumberList>\n<SeeRelatedList><SeeRelatedDescriptor><DescriptorReferredTo><DescriptorUI>D999999</DescriptorUI></DescriptorReferredTo></SeeRelatedDescriptor></SeeRelatedList>\n</DescriptorRecord>\n<DescriptorRecord DescriptorClass=\"1\">\n<DescriptorUI>D009369</DescriptorUI>\n<TreeNumberList>\n<TreeNumber>C04</TreeNumber>\n<TreeNumber>C04.557</TreeNumber>\n</TreeNumberList>\n</DescriptorRecord>\n</DescriptorRecordSet>\n").unwrap();
        let pm = d.path().join("pubmed");
        std::fs::create_dir_all(&pm).unwrap();
        std::fs::write(pm.join("part-00001.jsonl"), "{\"pmid\": 111, \"journal\": \"J Onc\", \"mesh\": \"[{'major': False, 'name': 'Animals', 'ui': 'D000818'}, {'major': True, 'name': 'Neoplasms', 'ui': 'D009369'}]\", \"pub_types\": [\"Review\"]}\n{\"pmid\": 222, \"mesh\": \"[]\", \"pub_types\": []}\n").unwrap();
        std::fs::write(d.path().join("records.jsonl"), "{\"article_id\":\"PMC1.1\",\"pmid\":\"111\",\"journal\":\"Journal of Oncology\",\"pub_types\":[]}\n{\"article_id\":\"PMC2.1\",\"pmid\":\"\",\"journal\":\"Physics Letters\",\"pub_types\":[\"Journal Article\"]}\n").unwrap();
        std::fs::write(d.path().join("arts.jsonl"), "{\"article_id\":\"PMC1.1\"}\n{\"article_id\":\"PMC2.1\"}\n").unwrap();
        let out = d.path().join("meta.jsonl");
        // an older year with a retired descriptor D111111 (N05.300) that the newest file no longer has
        std::fs::write(d.path().join("old.xml"), "<DescriptorRecordSet>\n<DescriptorRecord DescriptorClass=\"1\">\n<DescriptorUI>D111111</DescriptorUI>\n<TreeNumberList>\n<TreeNumber>N05.300</TreeNumber>\n</TreeNumberList>\n</DescriptorRecord>\n<DescriptorRecord DescriptorClass=\"1\">\n<DescriptorUI>D009369</DescriptorUI>\n<TreeNumberList>\n<TreeNumber>C99</TreeNumber>\n</TreeNumberList>\n</DescriptorRecord>\n</DescriptorRecordSet>\n").unwrap();
        let descs = vec![d.path().join("desc.xml"), d.path().join("old.xml")];
        let rep = build(&d.path().join("arts.jsonl"), &d.path().join("records.jsonl"), &pm, &descs, &out).unwrap();
        assert_eq!((rep["with_mesh_trees"].as_u64(), rep["no_pmid"].as_u64(), rep["descriptors"].as_u64()), (Some(1), Some(1), Some(3)));
        assert_eq!(rep["retired_descriptors_mapped"], 1, "only the UI missing from the newest year");
        let rows: Vec<Value> = crate::io::reader(&out).unwrap().lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
        assert_eq!(rows[0]["mesh_trees"], json!(["B01.050", "C04", "C04.557"]), "newest year wins: C99 from the old file is ignored");
        assert_eq!(rows[0]["retired_trees"], 0);
        assert_eq!(rows[0]["pub_types"], json!(["Review"]));
        assert_eq!(rows[0]["journal"], "Journal of Oncology");
        assert_eq!((rows[1]["mesh_trees"].as_array().unwrap().len(), rows[1]["pub_types"].clone()), (0, json!(["Journal Article"])));
        assert_eq!((rep["patient_relevant"].as_u64(), rep["share_with_mesh"].as_f64()), (Some(1), Some(0.5)), "C04 + Review");
        let jl = d.path().join("j.txt");
        std::fs::write(d.path().join("rec2.jsonl"), "{\"article_id\":\"PMC1.1\",\"nlm_ta\":\"J Onc\",\"journal\":\"Journal of Oncology\"}\n").unwrap();
        let jr = journal_list(&pm, &descs, Some(&d.path().join("rec2.jsonl")), 1, 0.6, &jl).unwrap();
        assert_eq!((jr["journals_listed"].as_u64(), jr["full_titles"].as_u64()), (Some(1), Some(1)));
        assert!(std::fs::read_to_string(&jl).unwrap().lines().any(|l| l == "Journal of Oncology"));
        assert!(biomedical_trees(&["G05.360".into()]) && !biomedical_trees(&["E01.370".into()]) && biomedical_trees(&["E01".into(), "B01".into()]));
        assert!(!biomedical_trees_strict(&["D02.455".into()]) && biomedical_trees_strict(&["D02.455".into(), "C04".into()]));
        assert!(mega("Scientific Reports") && mega("PLoS ONE") && mega("Proceedings of the National Academy of Sciences of the United States of America")
            && mega("Proceedings of the National Academy of Sciences (PNAS)") && !mega("Science Translational Medicine") && !mega("Nature Medicine"));
    }
}
