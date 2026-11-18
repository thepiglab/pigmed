//! Fix round 2 (group R2b): PMC crosswalk adoption for aasm_sleep, asam_addiction, idsa, aasld and ash_hematology.
//!
//! Thin layer over [`pmc_crosswalk`](super::pmc_crosswalk) (which is not modified):
//! - [`Session`]: per-discovery dedup of source-stated ids (DOI/PMCID) and of PMC records, so one PMC article is
//!   planned once per source even when several society pages link it; every attempt yields a [`Cross`] with the
//!   full-text row (Open) or a relation-only link (Closed, in PMC without distributable XML) and an evidence record.
//! - [`extract`]: `pmc_crosswalk::extract` plus (a) the user decision of 2026-11-17 that PMC copies without a licence
//!   are kept with `rights.status = restricted` (never upgraded to open), and (b) a relation back to the society's own
//!   document (`guideline:<source>:<doc_key>`) or the society page that links the article.
//! - [`doi_for_citation`]: a society link that prints only journal/volume/issue/first page (ASH Blood Advances, OUP
//!   AJRCCM) is resolved through Europe PMC search by ISSN + volume (+ issue) and accepted only when exactly one hit
//!   has that first page. [`doi_for_slug`]: an LWW link that prints only journal, year and a title slug (ASAM) is
//!   resolved through Europe PMC by ISSN + year + the society's topic title and accepted only when exactly one hit's
//!   normalised title starts with the printed slug. Both go through `pmc_crosswalk::epmc_search` (its 10 s gate for
//!   www.ebi.ac.uk); the cached answer is re-read for the page/volume fields the shared hit type does not carry.
//!   Nothing is guessed: no hit, several hits or a mismatch leaves the item unresolved with the reason.
use super::pmc_crosswalk::{self as cw, Ids, Lookup, Planned, SourceRef};
use super::PlanRow;
use crate::guidelines::{
    doc::{GuidelineDoc, Relation},
    fetch::Fetcher,
    registry::SourceEntry,
};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Outcome of one source-stated id.
pub struct Cross {
    /// Full-text row (PMC OAI JATS) to add to the plan.
    pub row: Option<PlanRow>,
    /// Relation for the society's own document (or row): `related → guideline:<src>:<pmc doc_key>` for a planned
    /// full text, `related → pmc:PMCxxxx` (relation only, no text) for a closed PMC record.
    pub relation: Option<Relation>,
    /// "full_text" | "relation_only" | "not_in_pmc" | "duplicate" | "lookup_failed".
    pub status: &'static str,
    pub reason: Option<String>,
    /// JSON record for the plan row (`extra.pmc_crosswalk_results[]`), for completeness accounting.
    pub evidence: Value,
}

#[derive(Clone)]
struct SeenRec {
    pmcid: Option<String>,
    doc_key: Option<String>,
    status: &'static str,
    reason: Option<String>,
}

/// Per-discovery state: ids and PMC records already tried for this source.
#[derive(Default)]
pub struct Session {
    seen: HashMap<String, SeenRec>,
}

fn lookup_key(l: &Lookup) -> (String, String) {
    match l {
        Lookup::Doi(d) => ("doi".into(), d.to_ascii_lowercase()),
        Lookup::Pmid(p) => ("pmid".into(), p.clone()),
        Lookup::Pmcid(p) => ("pmcid".into(), p.to_ascii_uppercase()),
    }
}

fn open_relation(src: &SourceEntry, doc_key: &str, pmcid: &str, evidence_url: Option<String>) -> Relation {
    Relation {
        rel: "related".into(),
        target: format!("guideline:{}:{doc_key}", src.id),
        scope: Some(format!("full text of the linked journal article, acquired separately from PMC Open Access JATS ({pmcid})")),
        evidence_url,
    }
}

impl Session {
    /// Try one id the source states. `source_doc` = `guideline:<src>:<doc_key>` of the society's own document when
    /// there is one (it becomes the PMC document's back-relation); `extra` is carried on the full-text row.
    pub async fn try_id(&mut self, src: &SourceEntry, f: &Fetcher, lookup: Lookup, sref: SourceRef, mut extra: Value, source_doc: Option<String>) -> Cross {
        let (kind, id) = lookup_key(&lookup);
        let key = format!("{kind}:{id}");
        let route = sref.id_route.clone();
        let ev = |status: &str, pmcid: &Option<String>, doc_key: &Option<String>, reason: &Option<String>| {
            json!({"id_type": kind, "id": id, "status": status, "pmcid": pmcid, "doc_key": doc_key, "reason": reason, "id_route": route})
        };
        if let Some(s) = self.seen.get(&key).cloned() {
            return self.duplicate(src, &s, ev("duplicate", &s.pmcid, &s.doc_key, &s.reason));
        }
        let resolved = cw::resolve(f, &lookup).await;
        let (ids, pmcid) = match (&lookup, resolved) {
            (_, Ok(ids)) => {
                let p = ids.pmcid.clone().or_else(|| match &lookup { Lookup::Pmcid(p) => Some(p.clone()), _ => None });
                (Some(ids), p)
            }
            // A PMCID the source itself states is still tried directly against PMC OAI (as cw::plan_ids does).
            (Lookup::Pmcid(p), Err(_)) => (None, Some(p.clone())),
            (_, Err(e)) => {
                let reason = Some(format!("PMC ID Converter lookup failed: {e:#}"));
                let rec = SeenRec { pmcid: None, doc_key: None, status: "lookup_failed", reason: reason.clone() };
                self.seen.insert(key, rec);
                return Cross { row: None, relation: None, status: "lookup_failed", evidence: ev("lookup_failed", &None, &None, &reason), reason };
            }
        };
        let Some(pmcid) = pmcid else {
            let why = ids.as_ref().and_then(|i| i.note.clone()).unwrap_or_else(|| "no PMCID returned".into());
            let reason = Some(format!("no PMC copy ({why})"));
            self.seen.insert(key, SeenRec { pmcid: None, doc_key: None, status: "not_in_pmc", reason: reason.clone() });
            return Cross { row: None, relation: None, status: "not_in_pmc", evidence: ev("not_in_pmc", &None, &None, &reason), reason };
        };
        let pkey = format!("pmcid:{pmcid}");
        if let Some(s) = self.seen.get(&pkey).cloned() {
            self.seen.insert(key, s.clone());
            return self.duplicate(src, &s, ev("duplicate", &s.pmcid, &s.doc_key, &s.reason));
        }
        if !extra.is_object() {
            extra = json!({});
        }
        if let Some(d) = &source_doc {
            extra["source_doc"] = json!(d);
        }
        let evidence_url = sref.evidence_url.clone();
        let planned = cw::plan_pmcid(src, f, &pmcid, ids.clone(), sref, extra).await;
        let (status, row, relation, reason) = match planned {
            Ok(Planned::Open(row)) => {
                let rel = open_relation(src, &row.doc_key, &pmcid, evidence_url);
                ("full_text", Some(row), Some(rel), None)
            }
            // plan_pmcid also answers Closed when the OAI request itself failed or the JATS could not be mapped:
            // those are failures, not "in PMC without distributable XML".
            Ok(Planned::Closed { reason, .. }) if !reason.contains("does not distribute its full text") => ("lookup_failed", None, None, Some(reason)),
            Ok(Planned::Closed { ids: cids, reason }) => {
                let ids = cids.or(ids).unwrap_or_else(|| Ids { requested: pmcid.clone(), pmcid: Some(pmcid.clone()), evidence_url: cw::oai_url(&pmcid).unwrap_or_default(), ..Default::default() });
                let ids = Ids { pmcid: ids.pmcid.clone().or(Some(pmcid.clone())), ..ids };
                ("relation_only", None, cw::pmc_relation(&ids, &reason), Some(reason))
            }
            Err(e) => ("lookup_failed", None, None, Some(format!("PMC planning failed: {e:#}"))),
        };
        let doc_key = row.as_ref().map(|r| r.doc_key.clone());
        let rec = SeenRec { pmcid: Some(pmcid.clone()), doc_key: doc_key.clone(), status, reason: reason.clone() };
        self.seen.insert(key, rec.clone());
        self.seen.insert(pkey, rec);
        Cross { row, relation, status, evidence: ev(status, &Some(pmcid), &doc_key, &reason), reason }
    }

    fn duplicate(&self, src: &SourceEntry, s: &SeenRec, evidence: Value) -> Cross {
        let relation = match (&s.doc_key, &s.pmcid) {
            (Some(k), Some(p)) => Some(open_relation(src, k, p, None)),
            (None, Some(p)) if s.status == "relation_only" => cw::pmc_relation(
                &Ids { requested: p.clone(), pmcid: Some(p.clone()), evidence_url: cw::oai_url(p).unwrap_or_default(), ..Default::default() },
                s.reason.as_deref().unwrap_or("relation only"),
            ),
            _ => None,
        };
        let reason = Some(format!("same id or PMC record as an earlier item of this source ({})", s.status));
        Cross { row: None, relation, status: "duplicate", reason, evidence }
    }
}

/// Relations a discovery stored on a source row (`extra.pmc_relations`), skipping targets already present.
pub fn relations_from_extra(extra: &Value, existing: &[Relation]) -> Vec<Relation> {
    let mut out: Vec<Relation> = vec![];
    for r in extra["pmc_relations"].as_array().into_iter().flatten() {
        if let Ok(r) = serde_json::from_value::<Relation>(r.clone()) {
            if !existing.iter().chain(out.iter()).any(|x| x.target == r.target) {
                out.push(r);
            }
        }
    }
    out
}

/// Append a relation and an evidence record to a source row's `extra` (`pmc_relations[]`, `pmc_crosswalk_results[]`).
pub fn record_on(row: &mut PlanRow, c: &Cross) {
    if !row.extra.is_object() {
        row.extra = json!({});
    }
    if let Some(r) = &c.relation {
        let v = serde_json::to_value(r).unwrap_or(Value::Null);
        match row.extra["pmc_relations"].as_array_mut() {
            Some(a) => {
                if !a.iter().any(|x| x["target"] == v["target"]) {
                    a.push(v)
                }
            }
            None => row.extra["pmc_relations"] = json!([v]),
        }
    }
    match row.extra["pmc_crosswalk_results"].as_array_mut() {
        Some(a) => a.push(c.evidence.clone()),
        None => row.extra["pmc_crosswalk_results"] = json!([c.evidence]),
    }
}

/// `pmc_crosswalk::extract` + restricted status for licence-less PMC copies + back-relation to the society item.
pub async fn extract(src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
    let mut d = cw::extract(src, f, row).await?;
    finish(&mut d, row);
    Ok(d)
}

fn finish(d: &mut GuidelineDoc, row: &PlanRow) {
    if d.rights.status == "unknown" {
        d.rights.status = "restricted".into();
        d.rights.restrictions.push(
            "No licence in the PMC JATS <permissions>: free-to-read PMC copy, not open access. Kept for the private corpus as restricted (project decision 2026-11-17); not for redistribution.".into(),
        );
    }
    let sref = &row.extra["pmc_crosswalk"]["source"];
    let evidence = sref["evidence_url"].as_str().map(String::from);
    if let Some(t) = row.extra["source_doc"].as_str() {
        d.relations.push(Relation { rel: "related".into(), target: t.into(), scope: Some("society document that links this journal article".into()), evidence_url: evidence });
    } else if let Some(e) = evidence.filter(|e| Some(e.as_str()) != sref["source_url"].as_str() && !e.contains("ebi.ac.uk")) {
        d.relations.push(Relation { rel: "related".into(), target: e.clone(), scope: Some("society page that links this journal article".into()), evidence_url: Some(e) });
    }
    if let Some(r) = row.extra["citation_resolution"].as_object() {
        d.warnings.push(format!(
            "The society link states no DOI; the article was identified by {} (query {}).",
            r.get("method").and_then(|v| v.as_str()).unwrap_or("citation"),
            r.get("query").and_then(|v| v.as_str()).unwrap_or("")
        ));
    }
}

// ---------------------------------------------------------------------------------------------------------------
// citation / slug resolution through Europe PMC (no DOI printed by the society)

/// The bibliographic fields of one Europe PMC search result (core or lite JSON).
#[derive(Clone, Debug, PartialEq)]
pub struct EpmcRec {
    pub doi: Option<String>,
    pub pmcid: Option<String>,
    pub title: String,
    pub volume: Option<String>,
    pub issue: Option<String>,
    pub page_info: Option<String>,
    pub pub_year: Option<String>,
}

pub fn epmc_records(v: &Value) -> Vec<EpmcRec> {
    let s = |x: &Value| match x {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    v["resultList"]["result"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| EpmcRec {
                    doi: s(&r["doi"]).and_then(|d| cw::normalize_doi(&d)),
                    pmcid: s(&r["pmcid"]).filter(|p| p.starts_with("PMC")),
                    title: s(&r["title"]).unwrap_or_default(),
                    volume: s(&r["journalInfo"]["volume"]).or_else(|| s(&r["journalVolume"])),
                    issue: s(&r["journalInfo"]["issue"]).or_else(|| s(&r["issue"])),
                    page_info: s(&r["pageInfo"]),
                    pub_year: s(&r["pubYear"]).or_else(|| s(&r["journalInfo"]["yearOfPublication"])),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A citation printed by the society link (no DOI).
#[derive(Clone, Debug, PartialEq)]
pub struct Citation {
    pub issn: String,
    pub volume: String,
    pub issue: Option<String>,
    pub first_page: String,
}

pub fn citation_query(c: &Citation) -> String {
    let mut q = format!("ISSN:\"{}\" AND VOLUME:\"{}\"", c.issn, c.volume);
    if let Some(i) = c.issue.as_ref().filter(|i| i.bytes().all(|b| b.is_ascii_digit())) {
        q.push_str(&format!(" AND ISSUE:\"{i}\""));
    }
    q
}

fn first_page(page_info: &str) -> &str {
    page_info.split(['-', '–']).next().unwrap_or("").trim()
}

/// Exactly one record with the citation's volume and first page (and issue when printed) → that record.
pub fn match_citation<'a>(recs: &'a [EpmcRec], c: &Citation) -> Result<&'a EpmcRec, String> {
    let hits: Vec<&EpmcRec> = recs
        .iter()
        .filter(|r| r.volume.as_deref() == Some(c.volume.as_str()))
        .filter(|r| c.issue.is_none() || r.issue.as_deref().map(|i| i.split_whitespace().next().unwrap_or("")) == c.issue.as_deref())
        .filter(|r| r.page_info.as_deref().is_some_and(|p| first_page(p).eq_ignore_ascii_case(&c.first_page)))
        .collect();
    match hits.as_slice() {
        [one] if one.doi.is_some() || one.pmcid.is_some() => Ok(*one),
        [_] => Err("the matching Europe PMC record has no DOI or PMCID".into()),
        [] => Err("no Europe PMC record with this volume/issue/first page".into()),
        _ => Err(format!("{} Europe PMC records share this volume/issue/first page; not resolved", hits.len())),
    }
}

/// Exactly one record from that year whose normalised title starts with the printed slug → that record.
pub fn match_slug<'a>(recs: &'a [EpmcRec], year: &str, slug: &str) -> Result<&'a EpmcRec, String> {
    let want = cw::norm_title(&slug.replace(['_', '-'], " "));
    if want.split_whitespace().count() < 4 {
        return Err(format!("title slug {slug:?} is too short to identify an article"));
    }
    let hits: Vec<&EpmcRec> = recs
        .iter()
        .filter(|r| r.pub_year.as_deref() == Some(year))
        .filter(|r| cw::norm_title(&r.title).starts_with(&want))
        .collect();
    match hits.as_slice() {
        [one] if one.doi.is_some() || one.pmcid.is_some() => Ok(*one),
        [_] => Err("the matching Europe PMC record has no DOI or PMCID".into()),
        [] => Err("no Europe PMC record of that year starts with the printed title slug".into()),
        _ => Err(format!("{} Europe PMC records match the printed title slug; not resolved", hits.len())),
    }
}

/// Europe PMC search (shared 10 s gate), then the cached answers re-read for the full bibliographic fields.
async fn epmc_recs(f: &Fetcher, query: &str) -> Result<(u64, Vec<EpmcRec>, Vec<String>), String> {
    let page = cw::epmc_search(f, query, 100).await.map_err(|e| format!("Europe PMC search failed: {e:#}"))?;
    if page.hit_count > 100 {
        return Err(format!("Europe PMC query has {} hits (>100); too broad to verify", page.hit_count));
    }
    let mut recs = vec![];
    for u in &page.urls {
        let p = f.get(u).await.map_err(|e| format!("Europe PMC answer not re-readable: {e:#}"))?;
        let v: Value = serde_json::from_slice(&p.body).map_err(|e| format!("Europe PMC answer not JSON: {e}"))?;
        recs.extend(epmc_records(&v));
    }
    Ok((page.hit_count, recs, page.urls))
}

/// (lookup, evidence) for a printed citation, or the reason it stays unresolved.
pub async fn doi_for_citation(f: &Fetcher, c: &Citation) -> Result<(Lookup, Value), String> {
    let q = citation_query(c);
    let (n, recs, urls) = epmc_recs(f, &q).await?;
    let r = match_citation(&recs, c).map_err(|e| format!("{e} (query {q}, {n} hits)"))?;
    let ev = json!({"method": "journal ISSN + volume + issue + first page through Europe PMC (unique match)", "api": "Europe PMC REST search", "query": q, "hit_count": n,
        "urls": urls, "citation": {"issn": c.issn, "volume": c.volume, "issue": c.issue, "first_page": c.first_page},
        "matched": {"doi": r.doi, "pmcid": r.pmcid, "title": r.title, "page_info": r.page_info}});
    Ok((r.doi.clone().map(Lookup::Doi).unwrap_or_else(|| Lookup::Pmcid(r.pmcid.clone().unwrap_or_default())), ev))
}

/// (lookup, evidence) for a link that prints journal, year and a title slug; `topic` narrows the query and is the
/// society's own title for the item.
pub async fn doi_for_slug(f: &Fetcher, issn: &str, year: &str, slug: &str, topic: &str) -> Result<(Lookup, Value), String> {
    let topic = topic.replace('"', " ");
    let q = format!("ISSN:\"{issn}\" AND PUB_YEAR:{year} AND TITLE:\"{}\"", topic.trim());
    let (n, recs, urls) = epmc_recs(f, &q).await?;
    let r = match_slug(&recs, year, slug).map_err(|e| format!("{e} (query {q}, {n} hits)"))?;
    let ev = json!({"method": "journal ISSN + year + printed title slug through Europe PMC (unique match)", "api": "Europe PMC REST search", "query": q, "hit_count": n,
        "urls": urls, "slug": slug, "matched": {"doi": r.doi, "pmcid": r.pmcid, "title": r.title, "pub_year": r.pub_year}});
    Ok((r.doi.clone().map(Lookup::Doi).unwrap_or_else(|| Lookup::Pmcid(r.pmcid.clone().unwrap_or_default())), ev))
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;

    fn fx(name: &str) -> Vec<u8> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/r2b_crosswalk/").to_string() + name).unwrap()
    }

    #[test]
    fn citation_and_slug_matching_is_exact() {
        let v: Value = serde_json::from_slice(&fx("epmc-bloodadv-3-23.json")).unwrap();
        let recs = epmc_records(&v);
        assert!(recs.len() >= 3);
        let c = Citation { issn: "2473-9529".into(), volume: "3".into(), issue: Some("23".into()), first_page: "3898".into() };
        assert_eq!(citation_query(&c), "ISSN:\"2473-9529\" AND VOLUME:\"3\" AND ISSUE:\"23\"");
        let r = match_citation(&recs, &c).unwrap();
        assert_eq!(r.doi.as_deref(), Some("10.1182/bloodadvances.2019000975"));
        assert_eq!(r.pmcid.as_deref(), Some("PMC6963238"));
        // a page inside another article's range is not a first page
        assert!(match_citation(&recs, &Citation { first_page: "3899".into(), ..c.clone() }).is_err());
        assert!(match_citation(&recs, &Citation { volume: "4".into(), ..c.clone() }).is_err());
        // supplements: non-numeric issue is left out of the query
        assert_eq!(citation_query(&Citation { issue: Some("Supplement_3".into()), ..c.clone() }), "ISSN:\"2473-9529\" AND VOLUME:\"3\"");

        let v: Value = serde_json::from_slice(&fx("epmc-jam-stimulant.json")).unwrap();
        let recs = epmc_records(&v);
        let r = match_slug(&recs, "2024", "the_asam_aaap_clinical_practice_guideline_on_the").unwrap();
        assert_eq!(r.doi.as_deref(), Some("10.1097/adm.0000000000001299"));
        assert!(match_slug(&recs, "2023", "the_asam_aaap_clinical_practice_guideline_on_the").is_err());
        assert!(match_slug(&recs, "2024", "the_asam_aaap_guideline_for_alcohol").is_err());
        assert!(match_slug(&recs, "2024", "the_asam").is_err());
    }

    #[test]
    fn licence_less_copy_becomes_restricted_and_links_back() {
        let mut d = GuidelineDoc { rights: crate::guidelines::doc::Rights { status: "unknown".into(), ..Default::default() }, ..Default::default() };
        let row = PlanRow { source_id: "x".into(), doc_key: "k".into(), url: "u".into(), title: None, rendition: None, version: None,
            extra: json!({"source_doc": "guideline:x:site-1", "pmc_crosswalk": {"source": {"evidence_url": "https://soc.example/page", "source_url": "https://doi.org/10.1/x"}}}) };
        finish(&mut d, &row);
        assert_eq!(d.rights.status, "restricted");
        assert!(d.rights.restrictions[0].contains("not open access"));
        assert!(d.relations.iter().any(|r| r.target == "guideline:x:site-1"));
        // an open licence is never touched; without a society doc the society page is linked
        let mut d = GuidelineDoc { rights: crate::guidelines::doc::Rights { status: "open".into(), license: Some("CC-BY-NC-4.0".into()), ..Default::default() }, ..Default::default() };
        let row = PlanRow { extra: json!({"pmc_crosswalk": {"source": {"evidence_url": "https://soc.example/page", "source_url": "https://doi.org/10.1/x"}}}), ..row };
        finish(&mut d, &row);
        assert_eq!(d.rights.status, "open");
        assert!(d.rights.restrictions.is_empty());
        assert!(d.relations.iter().any(|r| r.target == "https://soc.example/page"));
    }

    #[test]
    fn relations_are_recorded_once() {
        let mut row = PlanRow { source_id: "x".into(), doc_key: "k".into(), url: "u".into(), title: None, rendition: None, version: None, extra: json!({}) };
        let rel = Relation { rel: "related".into(), target: "pmc:PMC1".into(), scope: None, evidence_url: None };
        let c = Cross { row: None, relation: Some(rel.clone()), status: "relation_only", reason: None, evidence: json!({"id": "PMC1"}) };
        record_on(&mut row, &c);
        record_on(&mut row, &c);
        assert_eq!(row.extra["pmc_relations"].as_array().unwrap().len(), 1);
        assert_eq!(row.extra["pmc_crosswalk_results"].as_array().unwrap().len(), 2);
        assert_eq!(relations_from_extra(&row.extra, &[]).len(), 1);
        assert!(relations_from_extra(&row.extra, &[rel]).is_empty());
    }
}
