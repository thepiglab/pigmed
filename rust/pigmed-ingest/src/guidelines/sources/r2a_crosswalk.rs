//! Fix group R2a-crosswalk: adoption of the shared PMC open-access crosswalk ([`super::pmc_crosswalk`]) by
//! `acp_im`, `ada`, `esmo`, `wses_guidelines`, `rcog` and `bsg`, on top of each source's own site scrape.
//!
//! - Discovery: every identifier the SOURCE states on a row (`extra` keys per adapter) goes through the crosswalk
//!   ([`adopt`]). `Planned::Open` becomes a crosswalk row (doc_key `<source>-pmc-pmcNNN-<sha12>`, which cannot
//!   collide with any site key) placed right after its site row ([`interleave`]); it carries a pointer back to
//!   the site record (`extra.pmc_crosswalk_source_record`). `Planned::Closed` is kept on the site row
//!   (`extra.pmc_crosswalk_closed`): the id, the API answer, the reason and, when PMC holds a record without
//!   distributable XML, a relation-only `related pmc:PMCxxxx` link (no text taken).
//! - Extraction: crosswalk rows go to [`extract`] (= `pmc_crosswalk::extract` plus a `related` link back to the
//!   site document `guideline:<source>:<doc_key>`, or to the source page when the site row has no document).
//!   Site documents get [`annotate`]: `related` links to their open PMC copies, the relation-only PMC links, and
//!   the per-id outcome in `search_metadata.pmc_crosswalk`.
//! - A PMCID is planned once per source (`seen`); a later row stating it gets a relation, not a second document.
//! - BSG links BMJ articles without printing a DOI. [`bmj_lookups`] resolves those links through Europe PMC search
//!   and accepts a hit only when the API's own answer verifies it (DOI ends in the stated article id and journal
//!   ISSN matches; or, for volume/issue/page links, the OA JATS record's volume, issue, first page and ISSN).
//!   No DOI is ever built from a BMJ id.
use super::pmc_crosswalk::{self as cw, Lookup, OaJats, Planned, SourceRef};
use super::PlanRow;
use crate::guidelines::{
    doc::{GuidelineDoc, Relation},
    fetch::Fetcher,
    registry::SourceEntry,
};
use anyhow::Result;
use roxmltree::{Document, ParsingOptions};
use serde_json::{json, Value};
use std::collections::HashSet;

/// Site-row key: open PMC copies planned for this row (`[{requested, pmcid, doc_key}]`).
pub const OPEN: &str = "pmc_crosswalk_open";
/// Site-row key: ids with no open PMC copy (`[{requested, ids, reason, relation}]`).
pub const CLOSED: &str = "pmc_crosswalk_closed";
/// Crosswalk-row key: the site record that stated the id (`{requested, doc_key, url}`).
pub const BACKREF: &str = "pmc_crosswalk_source_record";

pub fn key_prefix(src: &SourceEntry) -> String {
    format!("{}-pmc", src.id)
}

/// Registry-internal relation target (see `doc::Relation`).
pub fn target(src: &SourceEntry, doc_key: &str) -> String {
    format!("guideline:{}:{doc_key}", src.id)
}

pub fn label(l: &Lookup) -> String {
    match l {
        Lookup::Doi(d) => format!("doi:{d}"),
        Lookup::Pmid(p) => format!("pmid:{p}"),
        Lookup::Pmcid(p) => format!("pmc:{p}"),
    }
}

/// One lookup for a single-identity item: the PMCID when the source states one, else its DOI.
pub fn single(doi: Option<&str>, pmcid: Option<&str>) -> Vec<Lookup> {
    match (pmcid.filter(|p| p.starts_with("PMC")), doi.and_then(cw::normalize_doi)) {
        (Some(p), _) => vec![Lookup::Pmcid(p.to_string())],
        (None, Some(d)) => vec![Lookup::Doi(d)],
        _ => vec![],
    }
}

/// The site record the ids were read from: its planned doc_key (None when the site row yields no document of
/// its own, e.g. a publisher row that is replaced or always refused) and the source page URL.
#[derive(Clone, Debug)]
pub struct SiteRef {
    pub doc_key: Option<String>,
    pub url: String,
}

#[derive(Default, Debug)]
pub struct Outcome {
    pub rows: Vec<PlanRow>,
    pub open: Vec<Value>,
    pub closed: Vec<Value>,
}

impl Outcome {
    /// Append this outcome's open/closed records to a site row's `extra`.
    pub fn record(&self, extra: &mut Value) {
        if !extra.is_object() {
            *extra = json!({});
        }
        for (k, v) in [(OPEN, &self.open), (CLOSED, &self.closed)] {
            if v.is_empty() {
                continue;
            }
            match extra[k].as_array_mut() {
                Some(a) => a.extend(v.iter().cloned()),
                None => extra[k] = json!(v),
            }
        }
    }
    pub fn merge(&mut self, o: Outcome) {
        self.rows.extend(o.rows);
        self.open.extend(o.open);
        self.closed.extend(o.closed);
    }
    /// One line per closed id, for refusal messages of rows that have no other content route.
    pub fn closed_note(&self) -> Option<String> {
        (!self.closed.is_empty()).then(|| {
            self.closed
                .iter()
                .map(|c| format!("{}: {}", c["requested"].as_str().unwrap_or("?"), c["reason"].as_str().unwrap_or("?")))
                .collect::<Vec<_>>()
                .join("; ")
        })
    }
}

/// Plan each lookup separately (one failing id never aborts the source) and sort the results into rows / records.
pub async fn adopt(src: &SourceEntry, f: &Fetcher, lookups: &[Lookup], sref: &SourceRef, site: &SiteRef, extra: &Value, seen: &mut HashSet<String>) -> Outcome {
    let mut o = Outcome::default();
    for l in lookups {
        let requested = label(l);
        let planned = match cw::plan_all(src, f, std::slice::from_ref(l), sref, extra).await {
            Ok(v) => v,
            Err(e) => {
                o.closed.push(json!({"requested": requested, "ids": null, "reason": format!("crosswalk error: {e:#}"), "relation": null}));
                continue;
            }
        };
        for (_, p) in planned {
            match p {
                Planned::Open(mut row) => {
                    let pmcid = cw::row_pmcid(&row).unwrap_or_default();
                    if !seen.insert(pmcid.clone()) {
                        let rel = Relation {
                            rel: "related".into(),
                            target: format!("pmc:{pmcid}"),
                            scope: Some("open-access PMC copy of the same article, planned as a document from an earlier record of this source".into()),
                            evidence_url: row.extra["pmc_crosswalk"]["oai_url"].as_str().map(String::from),
                        };
                        o.closed.push(json!({"requested": requested, "ids": row.extra["pmc_crosswalk"]["ids"], "pmcid": pmcid,
                            "reason": "same PMC record already planned from an earlier record of this source", "relation": rel}));
                        continue;
                    }
                    row.extra[BACKREF] = json!({"requested": requested, "doc_key": site.doc_key, "url": site.url});
                    o.open.push(json!({"requested": requested, "pmcid": pmcid, "doc_key": row.doc_key, "title": row.title}));
                    o.rows.push(row);
                }
                Planned::Closed { ids, reason } => {
                    let rel = ids.as_ref().and_then(|i| cw::pmc_relation(i, &reason));
                    o.closed.push(json!({"requested": requested, "ids": ids, "reason": reason, "relation": rel}));
                }
            }
        }
    }
    o
}

/// Site rows each followed by their crosswalk rows; a limit counts every row.
pub fn interleave(groups: Vec<(PlanRow, Vec<PlanRow>)>, limit: Option<usize>) -> Vec<PlanRow> {
    let mut out = vec![];
    for (site, cws) in groups {
        out.push(site);
        out.extend(cws);
    }
    if let Some(n) = limit {
        out.truncate(n);
    }
    out
}

/// Relations a site document gains from its row's crosswalk records.
pub fn site_relations(src: &SourceEntry, extra: &Value) -> Vec<Relation> {
    let mut out = vec![];
    for o in extra[OPEN].as_array().into_iter().flatten() {
        if let Some(k) = o["doc_key"].as_str() {
            out.push(Relation {
                rel: "related".into(),
                target: target(src, k),
                scope: Some(format!(
                    "open-access full text of {} from PubMed Central ({}), a separate document of this source",
                    o["requested"].as_str().unwrap_or("this item"),
                    o["pmcid"].as_str().unwrap_or("?")
                )),
                evidence_url: None,
            });
        }
    }
    for c in extra[CLOSED].as_array().into_iter().flatten() {
        if let Ok(r) = serde_json::from_value::<Relation>(c["relation"].clone()) {
            out.push(r);
        }
    }
    out
}

/// Add the crosswalk relations, a warning and the per-id outcome to a site document.
pub fn annotate(doc: &mut GuidelineDoc, src: &SourceEntry, extra: &Value) {
    let open = extra[OPEN].as_array().cloned().unwrap_or_default();
    let closed = extra[CLOSED].as_array().cloned().unwrap_or_default();
    if open.is_empty() && closed.is_empty() {
        return;
    }
    for r in site_relations(src, extra) {
        if !doc.relations.iter().any(|x| x.target == r.target && x.rel == r.rel) {
            doc.relations.push(r);
        }
    }
    let rel_only = closed.iter().filter(|c| !c["relation"].is_null()).count();
    if !closed.is_empty() {
        doc.warnings.push(format!(
            "PMC crosswalk: {} source-stated identifier(s) have no open-access PMC full text ({} in PMC as relation-only links, no text taken); per-id reasons in search_metadata.pmc_crosswalk.",
            closed.len(),
            rel_only
        ));
    }
    if !doc.search_metadata.is_object() {
        doc.search_metadata = json!({});
    }
    doc.search_metadata["pmc_crosswalk"] = json!({"open": open, "closed": closed});
}

/// Extract a crosswalk row: the shared mapping plus a link back to the record that stated the id.
pub async fn extract(src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
    let mut d = cw::extract(src, f, row).await?;
    let b = &row.extra[BACKREF];
    let back = b["doc_key"].as_str().map(|k| target(src, k)).or_else(|| b["url"].as_str().map(String::from));
    if let Some(t) = back {
        if !d.relations.iter().any(|r| r.target == t) {
            d.relations.push(Relation {
                rel: "related".into(),
                target: t,
                scope: Some(format!("{} record that states this identifier ({})", src.id, b["requested"].as_str().unwrap_or("?"))),
                evidence_url: b["url"].as_str().map(String::from),
            });
        }
    }
    Ok(d)
}

// ---------------------------------------------------------------------------------------------------------------
// BSG: BMJ links without a printed DOI

/// BMJ journal hosts BSG links, with the ISSNs (print, electronic) used to query and verify Europe PMC answers.
const BMJ_JOURNALS: &[(&str, &str, [&str; 2])] = &[
    ("gut.bmj.com", "gutjnl", ["0017-5749", "1468-3288"]),
    ("fg.bmj.com", "flgastro", ["1759-8478", "1759-8486"]),
];

/// A BMJ link stated by the source: article id (`gutjnl-2025-335023`, "early" URLs) or a citation path
/// (`gut.bmj.com/71/8/1459`, from `/content/71/8/1459`).
#[derive(Clone, Debug, PartialEq)]
pub enum BmjRef {
    ArticleId(String),
    Citation { host: String, volume: String, issue: String, page: String },
}

impl BmjRef {
    pub fn label(&self) -> String {
        match self {
            BmjRef::ArticleId(id) => format!("bmj:{id}"),
            BmjRef::Citation { host, volume, issue, page } => format!("bmj:{host}/{volume}/{issue}/{page}"),
        }
    }
    pub fn parse_citation(s: &str) -> Option<BmjRef> {
        let p: Vec<&str> = s.split('/').collect();
        (p.len() == 4 && p.iter().all(|x| !x.is_empty())).then(|| BmjRef::Citation {
            host: p[0].into(),
            volume: p[1].into(),
            issue: p[2].into(),
            page: p[3].into(),
        })
    }
}

fn alnum(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// Volume / issue / first page / ISSNs printed in a JATS record's front matter.
pub fn jats_citation(xml: &str) -> Option<(String, String, String, Vec<String>)> {
    let doc = Document::parse_with_options(xml, ParsingOptions { allow_dtd: true, nodes_limit: 50_000_000 }).ok()?;
    let front = doc.descendants().find(|n| n.is_element() && n.tag_name().name() == "front")?;
    let meta = front.children().find(|n| n.is_element() && n.tag_name().name() == "article-meta")?;
    let t = |name: &str| meta.children().find(|n| n.is_element() && n.tag_name().name() == name).map(|n| n.text().unwrap_or("").trim().to_string());
    let issns = front.descendants().filter(|n| n.is_element() && n.tag_name().name() == "issn").filter_map(|n| n.text()).map(|s| s.trim().to_string()).collect();
    Some((t("volume")?, t("issue").unwrap_or_default(), t("fpage")?, issns))
}

/// Does a JATS record carry exactly the stated citation (volume, issue, first page) in the stated journal?
pub fn jats_matches(xml: &str, volume: &str, issue: &str, page: &str, issns: &[&str]) -> bool {
    jats_citation(xml).is_some_and(|(v, i, p, is)| {
        alnum(&v) == alnum(volume) && alnum(&i) == alnum(issue) && alnum(&p) == alnum(page) && is.iter().any(|x| issns.contains(&x.as_str()))
    })
}

/// Europe PMC query for a BMJ link. Article ids are searched as a quoted phrase; citations by journal, volume,
/// issue (numeric issues only; supplement labels vary) and the first page as a phrase.
pub fn bmj_query(r: &BmjRef) -> Option<(String, [&'static str; 2])> {
    match r {
        BmjRef::ArticleId(id) => {
            let issns = BMJ_JOURNALS.iter().find(|(_, pre, _)| id.starts_with(&format!("{pre}-")))?.2;
            Some((format!("\"{id}\""), issns))
        }
        BmjRef::Citation { host, volume, issue, page } => {
            let issns = BMJ_JOURNALS.iter().find(|(h, _, _)| h == host)?.2;
            let issue_q = if issue.bytes().all(|b| b.is_ascii_digit()) { format!(" AND ISSUE:\"{issue}\"") } else { String::new() };
            Some((format!("ISSN:\"{}\" AND VOLUME:\"{volume}\"{issue_q} AND \"{page}\"", issns[0]), issns))
        }
    }
}

/// Resolve one BMJ link to a PMC lookup through Europe PMC (verified as described in the module docs).
/// Ok(Lookup) = verified id to plan; Err(record) = closed record (no relation: unverified candidates are listed only).
pub async fn bmj_lookup(f: &Fetcher, r: &BmjRef) -> std::result::Result<Lookup, Value> {
    let requested = r.label();
    let closed = |reason: String, extra: Value| Err(json!({"requested": requested, "ids": null, "reason": reason, "relation": null, "candidates": extra}));
    let Some((q, issns)) = bmj_query(r) else {
        return closed("BMJ link of a journal this adapter has no ISSN for; not resolved".into(), Value::Null);
    };
    let page = match cw::epmc_search(f, &q, 10).await {
        Ok(p) => p,
        Err(e) => return closed(format!("Europe PMC search failed: {e:#}"), Value::Null),
    };
    let evidence = json!({"query": q, "hit_count": page.hit_count, "urls": page.urls,
        "hits": page.hits.iter().map(|h| json!({"pmcid": h.pmcid, "doi": h.doi, "title": h.title})).collect::<Vec<_>>()});
    let journal_ok = |h: &cw::EpmcHit| h.journal_issn.iter().any(|i| issns.contains(&i.as_str()));
    match r {
        BmjRef::ArticleId(id) => {
            let suffix = format!("/{}", id.to_ascii_lowercase());
            let hits: Vec<&cw::EpmcHit> = page.hits.iter().filter(|h| journal_ok(h) && h.doi.as_deref().is_some_and(|d| d.ends_with(&suffix))).collect();
            match hits.as_slice() {
                [h] => match (&h.pmcid, &h.doi) {
                    (Some(p), _) => Ok(Lookup::Pmcid(p.clone())),
                    (None, Some(d)) => Ok(Lookup::Doi(d.clone())),
                    _ => closed("Europe PMC hit has no PMCID or DOI".into(), evidence),
                },
                [] => closed("Europe PMC returned no record whose DOI ends in this article id in this journal".into(), evidence),
                _ => closed("Europe PMC returned several records for this article id; not resolved".into(), evidence),
            }
        }
        BmjRef::Citation { volume, issue, page: fpage, .. } => {
            let mut verified = vec![];
            for h in page.hits.iter().filter(|h| journal_ok(h)).take(5) {
                let Some(p) = &h.pmcid else { continue };
                if let Ok(OaJats::Open { article_xml, .. }) = cw::open_access_jats(f, p).await {
                    if jats_matches(&article_xml, volume, issue, fpage, &issns) {
                        verified.push(p.clone());
                    }
                }
            }
            match verified.as_slice() {
                [p] => Ok(Lookup::Pmcid(p.clone())),
                [] => closed("no Europe PMC hit verified against an open-access JATS record (volume, issue, first page, ISSN); in PMC without OA XML or not in PMC".into(), evidence),
                _ => closed("several open-access JATS records match this citation; not resolved".into(), evidence),
            }
        }
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    fn fx(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/guidelines/r2a_crosswalk/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }
    fn src(id: &str) -> SourceEntry {
        serde_json::from_value(json!({"id": id, "access": "public", "enabled": true})).unwrap()
    }
    fn sref(id: &str, route: &str) -> SourceRef {
        SourceRef { source_url: "https://example.org/linked".into(), evidence_url: Some("https://example.org/page".into()), source_title: None,
            item_type: Some("guideline".into()), key_prefix: format!("{id}-pmc"), id_route: route.into(), notes: vec![] }
    }

    #[test]
    fn bmj_queries_and_jats_citation_check() {
        let id = BmjRef::ArticleId("gutjnl-2025-335023".into());
        assert_eq!(bmj_query(&id).unwrap().0, "\"gutjnl-2025-335023\"");
        let c = BmjRef::parse_citation("gut.bmj.com/71/8/1459").unwrap();
        assert_eq!(bmj_query(&c).unwrap().0, "ISSN:\"0017-5749\" AND VOLUME:\"71\" AND ISSUE:\"8\" AND \"1459\"");
        let s = BmjRef::parse_citation("gut.bmj.com/74/Suppl_2/s1").unwrap();
        assert_eq!(bmj_query(&s).unwrap().0, "ISSN:\"0017-5749\" AND VOLUME:\"74\" AND \"s1\"");
        assert!(bmj_query(&BmjRef::ArticleId("bmj-2025-1".into())).is_none());
        let xml = String::from_utf8(fx("oai-PMC9279848.xml")).unwrap();
        let (art, _) = cw::split_oai(&xml).unwrap().unwrap();
        assert!(jats_matches(&art, "71", "8", "1459", &["0017-5749", "1468-3288"]));
        assert!(!jats_matches(&art, "71", "8", "1460", &["0017-5749"]));
        assert!(!jats_matches(&art, "71", "8", "1459", &["1759-8478"]));
    }

    // Real Europe PMC / ID Converter / OAI answers (2026-11-17): the BMJ article id in a BSG "early" link and a
    // Gut volume/issue/page link both resolve to their OA PMC records without building a DOI.
    #[tokio::test]
    async fn bmj_links_resolve_only_through_verified_api_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
        let id = BmjRef::ArticleId("gutjnl-2025-335023".into());
        f.seed_cache_for_tests(&cw::epmc_search_url(&bmj_query(&id).unwrap().0, 10, "*"), &fx("epmc-gutjnl-2025-335023.json"), "application/json");
        assert!(matches!(bmj_lookup(&f, &id).await, Ok(Lookup::Pmcid(p)) if p == "PMC13018778"));
        let c = BmjRef::parse_citation("gut.bmj.com/71/8/1459").unwrap();
        f.seed_cache_for_tests(&cw::epmc_search_url(&bmj_query(&c).unwrap().0, 10, "*"), &fx("epmc-gut-71-8-1459.json"), "application/json");
        f.seed_cache_for_tests(&cw::oai_url("PMC9279848").unwrap(), &fx("oai-PMC9279848.xml"), "application/xml");
        assert!(matches!(bmj_lookup(&f, &c).await, Ok(Lookup::Pmcid(p)) if p == "PMC9279848"));
        // Same answer, different stated page: nothing verifies, nothing is guessed.
        let wrong = BmjRef::parse_citation("gut.bmj.com/71/8/1460").unwrap();
        f.seed_cache_for_tests(&cw::epmc_search_url(&bmj_query(&wrong).unwrap().0, 10, "*"), &fx("epmc-gut-71-8-1459.json"), "application/json");
        let e = bmj_lookup(&f, &wrong).await.unwrap_err();
        assert!(e["reason"].as_str().unwrap().contains("no Europe PMC hit verified") && e["relation"].is_null());
    }

    // Open copy → crosswalk row with a back-pointer; closed-in-PMC → relation-only; a repeated PMCID → relation.
    #[tokio::test]
    async fn adopt_sorts_open_closed_and_repeated_records() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
        let s = src("rcog");
        let accreta = "10.1111/1471-0528.70239";
        let missing = "10.1111/1471-0528.17844";
        f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Doi(accreta.into())), &fx("idconv-rcog-70239.json"), "application/json");
        f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Doi(missing.into())), &fx("idconv-rcog-17844.json"), "application/json");
        f.seed_cache_for_tests(&cw::oai_url("PMC13485456").unwrap(), &fx("oai-PMC13485456.xml"), "application/xml");
        let site = SiteRef { doc_key: Some("rcog-abc-123".into()), url: "https://example.org/page".into() };
        let mut seen = HashSet::new();
        let o = adopt(&s, &f, &[Lookup::Doi(accreta.into()), Lookup::Doi(missing.into())], &sref("rcog", "source_doi"), &site, &json!({}), &mut seen).await;
        assert!(o.rows.is_empty() && o.open.is_empty());
        assert_eq!(o.closed.len(), 2);
        assert_eq!(o.closed[0]["relation"]["target"], "pmc:PMC13485456");
        assert!(o.closed[0]["reason"].as_str().unwrap().contains("cannotDisseminateFormat"));
        assert!(o.closed[1]["relation"].is_null() && o.closed[1]["reason"].as_str().unwrap().contains("Identifier not found in PMC"));
        let mut extra = json!({});
        o.record(&mut extra);
        let mut doc = GuidelineDoc { search_metadata: json!({"a": 1}), ..Default::default() };
        annotate(&mut doc, &s, &extra);
        assert_eq!(doc.relations.len(), 1);
        assert_eq!(doc.search_metadata["pmc_crosswalk"]["closed"].as_array().unwrap().len(), 2);
        assert!(doc.warnings[0].contains("2 source-stated identifier(s)") && doc.warnings[0].contains("1 in PMC"));

        // Open record (WSES, CC BY): planned once; the second statement of the same PMCID is a relation.
        let w = src("wses_guidelines");
        f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Pmcid("PMC8352154".into())), &fx("idconv-PMC8352154.json"), "application/json");
        f.seed_cache_for_tests(&cw::oai_url("PMC8352154").unwrap(), &fx("oai-PMC8352154.xml"), "application/xml");
        let mut seen = HashSet::new();
        let site = SiteRef { doc_key: None, url: "https://www.wses.org.uk/scientific-resources/guidelines".into() };
        let o = adopt(&w, &f, &single(Some("10.1186/s13017-021-00380-1"), Some("PMC8352154")), &sref("wses_guidelines", "exact_title"), &site, &json!({}), &mut seen).await;
        assert_eq!(o.rows.len(), 1);
        let row = &o.rows[0];
        assert!(row.doc_key.starts_with("wses_guidelines-pmc-pmc8352154-"));
        assert_eq!(row.extra[BACKREF]["url"], site.url);
        let again = adopt(&w, &f, &single(None, Some("PMC8352154")), &sref("wses_guidelines", "exact_title"), &site, &json!({}), &mut seen).await;
        assert!(again.rows.is_empty() && again.closed[0]["relation"]["target"] == "pmc:PMC8352154");
        let d = extract(&w, &f, row).await.unwrap();
        assert!(d.relations.iter().any(|r| r.rel == "rendition_of" && r.target == "pmc:PMC8352154"));
        assert!(d.relations.iter().any(|r| r.rel == "related" && r.target == site.url));
        assert_eq!(d.rights.license.as_deref(), Some("CC-BY-4.0"));
        assert!(d.blocks.len() > 50);
    }

    #[test]
    fn interleave_keeps_site_then_copies_and_counts_limit() {
        let r = |k: &str| PlanRow { source_id: "x".into(), doc_key: k.into(), url: String::new(), title: None, rendition: None, version: None, extra: json!({}) };
        let g = vec![(r("a"), vec![r("a-pmc")]), (r("b"), vec![]), (r("c"), vec![r("c-pmc")])];
        let keys = |v: Vec<PlanRow>| v.into_iter().map(|x| x.doc_key).collect::<Vec<_>>();
        assert_eq!(keys(interleave(g.clone(), None)), ["a", "a-pmc", "b", "c", "c-pmc"]);
        assert_eq!(keys(interleave(g, Some(3))), ["a", "a-pmc", "b"]);
    }
}
