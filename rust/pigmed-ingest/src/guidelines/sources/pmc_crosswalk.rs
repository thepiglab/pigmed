//! Open-access crosswalk: a guideline the SOURCE itself identifies (DOI or PMID it prints or links) is resolved
//! to its PubMed Central record through official APIs, and, only when PMC distributes it as Open Access, its JATS
//! full text is mapped into guideline blocks. Reusable by any adapter whose publisher terminal is closed to us.
//!
//! Routes (all through the [`Fetcher`]: robots.txt, pacing, cache; plus a per-host gate for stated crawl-delays):
//! - NCBI PMC ID Converter `https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?ids=<doi|pmid>&format=json`
//!   (robots: `Allow: /tools/`, `Crawl-delay: 1`).
//! - PMC OAI-PMH `https://pmc.ncbi.nlm.nih.gov/api/oai/v1/mh/?verb=GetRecord&metadataPrefix=pmc&identifier=
//!   oai:pubmedcentral.nih.gov:<n>` (robots: `Allow: /api/`). It serves full JATS only for the PMC Open Access
//!   subset; any other article answers `cannotDisseminateFormat`, which we record as "free/not OA" and stop.
//!   Free-to-read PMC copies that are not OA are never scraped (PMC copyright terms reserve bulk retrieval to
//!   the OA services).
//! - Europe PMC REST `https://www.ebi.ac.uk/europepmc/webservices/rest/search` for enumeration and exact-title
//!   lookups (robots `*`: path allowed, `Crawl-Delay: 10`, honoured by the gate below).
//! - Crossref REST `https://api.crossref.org/works?filter=alternative-id:<PII>` only to turn a PII the source's own
//!   link states (Elsevier `/article/S...` URLs) into its DOI (robots.txt: 404, i.e. unrestricted).
//! - NCBI E-utilities is NOT used: eutils.ncbi.nlm.nih.gov/robots.txt is `Disallow: /`.
//!
//! API (for adapters; see also RESULTS.md of fix group A-journals):
//! - [`doi_from_url`] / [`normalize_doi`]: the DOI a source-emitted link states (doi.org, `/doi/...`,
//!   `article-lookup/doi/...`). Never derive a DOI from anything else.
//! - [`pii_from_url`] + [`doi_for_pii`]: PII stated in a source link → DOI via Crossref (exactly one match).
//! - [`resolve`] (`Lookup::Doi` / `Lookup::Pmid`) → [`Ids`]: PMID/PMCID exactly as the ID Converter returns them.
//! - [`open_access_jats`] → [`OaJats`]: `Open { article_xml, record }` or `NotOpenAccess { reason }`.
//! - [`plan`] → [`Planned`]: `Open(PlanRow)` for an OA copy (row.extra["pmc_crosswalk"] snapshot) or `Closed`
//!   with the ids and the reason; the adapter keeps its own unresolved row for `Closed`.
//! - [`ids_from_extra`] + [`plan_ids`]: adoption path for adapters that already carry `extra.doi` /
//!   `extra.oa_crosswalk.{doi,pmcid}`; [`pmc_relation`] gives a relation-only `pmc:` link for `Planned::Closed`
//!   records that are in PMC without distributable XML (no body taken).
//! - [`lookups_from_extra`] + [`plan_all`]: list form — every DOI/PMCID/PMID an adapter stored in `extra`
//!   (`doi`, `stated_dois`, `publisher_dois`, `stated_pmcids`, `oa_crosswalk.{doi,pmcid}`, ...) → per-id
//!   `Planned::Open(row)` (full text) or `Planned::Closed` (+ [`pmc_relation`] when a PMCID exists).
//! - [`is_crosswalk_row`] + [`extract`]: re-fetch (cache), check the planned snapshot, map JATS → [`GuidelineDoc`].
//! - [`epmc_search`]: Europe PMC enumeration (cursor paging, bounded) returning API-stated ids only.
//! - [`enumerate`]: registry-configured (`config.oa_enumeration`) Europe PMC enumeration → planned OA rows, with
//!   hit counts and the query recorded on every row for completeness accounting.
//! - [`resolve_exact_title`]: Europe PMC title lookup that accepts only one hit whose normalised title equals
//!   the source-printed title (and journal ISSN when given). Weaker than a DOI; recorded as such.
//! - [`jats_document`]: pure JATS → blocks/metadata/rights mapping (offline-testable).
//!
//! Document shape: `url` = the PMC article page; `doi`/`pmid` from the JATS article-ids (checked against the API);
//! relations `rendition_of → pmc:PMCxxxx` (text source) and `related → <publisher URL as the source linked it>`;
//! rights from the JATS `<permissions>` (licence URL/label, copyright statement, holder); figures point at PMC's
//! article-instance asset route `https://pmc.ncbi.nlm.nih.gov/articles/instance/<n>/bin/<href>`.
use super::{common, PlanRow};
use crate::guidelines::{
    doc::{Block, BlockKind, DateObservation, GuidelineDoc, Relation, Rights},
    fetch::{Fetched, Fetcher},
    registry::SourceEntry,
};
use anyhow::{bail, Context, Result};
use roxmltree::{Document, Node, ParsingOptions};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const IDCONV: &str = "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/";
pub const OAI: &str = "https://pmc.ncbi.nlm.nih.gov/api/oai/v1/mh/";
pub const EPMC_SEARCH: &str = "https://www.ebi.ac.uk/europepmc/webservices/rest/search";
const XLINK: &str = "http://www.w3.org/1999/xlink";

// ---------------------------------------------------------------------------------------------------------------
// polite access

/// Crawl-delays the API hosts state in robots.txt. Since round 3 the Fetcher itself honours robots `Crawl-delay`
/// per Fetcher (= per source); this process-wide gate keeps several sources that share these API hosts in one
/// process from multiplying the rate.
fn host_gap(host: &str) -> Duration {
    match host {
        "www.ebi.ac.uk" => Duration::from_secs(10),
        "pmc.ncbi.nlm.nih.gov" | "api.crossref.org" => Duration::from_secs(1),
        _ => Duration::ZERO,
    }
}

async fn polite_get(f: &Fetcher, url: &str) -> Result<Fetched> {
    // Offline (tests, cache replays) nothing reaches the network: no gate. Before this, every uncached offline
    // lookup stamped the gate and the next Europe PMC lookup slept 10 s, which made the full `cargo test` ~290 s.
    if f.is_offline() {
        return f.get(url).await;
    }
    static GATE: OnceLock<tokio::sync::Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let host = url::Url::parse(url)?.host_str().unwrap_or("").to_string();
    let gap = host_gap(&host);
    let gate = GATE.get_or_init(Default::default);
    let mut last = gate.lock().await;
    if let Some(t) = last.get(&host) {
        if t.elapsed() < gap {
            tokio::time::sleep(gap - t.elapsed()).await;
        }
    }
    let r = f.get(url).await;
    if !matches!(&r, Ok(p) if p.record.from_cache) {
        last.insert(host, Instant::now());
    }
    r
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ---------------------------------------------------------------------------------------------------------------
// identifiers

/// Lower-cased DOI if `s` is one (`10.<registrant>/<suffix>`), trimmed of trailing punctuation.
pub fn normalize_doi(s: &str) -> Option<String> {
    let s = percent_encoding::percent_decode_str(s.trim()).decode_utf8_lossy().to_string();
    let s = s.trim().trim_start_matches("doi:").trim();
    let s = s.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ')' | ']' | '"' | '\''));
    let re = regex::Regex::new(r"^10\.\d{4,9}/\S+$").unwrap();
    re.is_match(s).then(|| s.to_ascii_lowercase())
}

/// The DOI a source-emitted link states: `doi.org/<doi>`, `.../doi/(full|abs|epdf|pdf|reader)/<doi>`,
/// `.../doi/<doi>`, `article-lookup/doi/<doi>`. Query and fragment are ignored; nothing else is inferred.
pub fn doi_from_url(link: &str) -> Option<String> {
    let u = url::Url::parse(link.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    let path = u.path();
    let rest = if host == "doi.org" || host == "dx.doi.org" || host.ends_with(".doi.org") {
        path.trim_start_matches('/').to_string()
    } else if let Some(i) = path.find("/doi/") {
        let mut r = &path[i + 5..];
        for p in ["full/", "abs/", "epdf/", "pdf/", "reader/", "pdfdirect/", "suppl/"] {
            if let Some(x) = r.strip_prefix(p) {
                r = x;
                break;
            }
        }
        r.to_string()
    } else if let Some(i) = path.find("/doi.org/") {
        path[i + 9..].to_string()
    } else {
        return None;
    };
    // DOI suffixes may contain '/', so keep the whole remainder; only cut known file appendages (suppl_file/...).
    let rest = rest.split("/suppl_file/").next().unwrap_or("").trim_end_matches('/');
    normalize_doi(rest)
}

/// Elsevier-style PII a source-emitted article URL states (`/article/S0012-3692(26)06538-4/fulltext`), compacted
/// to Crossref's alternative-id form (`S0012369226065384`).
pub fn pii_from_url(link: &str) -> Option<String> {
    let u = url::Url::parse(link.trim()).ok()?;
    let seg = u.path_segments()?.skip_while(|s| *s != "article").nth(1)?;
    let seg = percent_encoding::percent_decode_str(seg).decode_utf8_lossy().to_string();
    let compact: String = seg.chars().filter(|c| !matches!(c, '-' | '(' | ')')).collect();
    let re = regex::Regex::new(r"^S[0-9]{15}[0-9X]$").unwrap();
    re.is_match(&compact).then_some(compact)
}

pub const CROSSREF_WORKS: &str = "https://api.crossref.org/works";

pub fn crossref_pii_url(pii: &str) -> String {
    format!("{CROSSREF_WORKS}?filter=alternative-id:{}&select=DOI,title,alternative-id,container-title,type&rows=5", enc(pii))
}

/// DOI for a PII through Crossref's `alternative-id` filter: exactly one work listing that PII, else None.
pub fn parse_crossref_pii(v: &Value, pii: &str) -> Option<String> {
    let items = v["message"]["items"].as_array()?;
    let hits: Vec<&Value> = items
        .iter()
        .filter(|i| i["alternative-id"].as_array().is_some_and(|a| a.iter().any(|x| x.as_str() == Some(pii))))
        .collect();
    (hits.len() == 1).then(|| hits[0]["DOI"].as_str().and_then(normalize_doi)).flatten()
}

pub async fn doi_for_pii(f: &Fetcher, pii: &str) -> Result<Option<String>> {
    let url = crossref_pii_url(pii);
    let p = polite_get(f, &url).await?;
    if p.record.status != 200 {
        bail!("Crossref HTTP {} for {url}", p.record.status);
    }
    let v: Value = serde_json::from_slice(&p.body).context("Crossref answer not JSON")?;
    Ok(parse_crossref_pii(&v, pii))
}

#[derive(Clone, Debug)]
pub enum Lookup {
    Doi(String),
    Pmid(String),
    Pmcid(String),
}

/// Identifiers exactly as the NCBI ID Converter returned them (`pmcid`/`pmid` None when not in PMC).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Ids {
    pub requested: String,
    pub doi: Option<String>,
    pub pmid: Option<String>,
    pub pmcid: Option<String>,
    /// The ID Converter request URL (evidence).
    pub evidence_url: String,
    /// Converter message when the id is not in PMC.
    pub note: Option<String>,
}

pub fn idconv_url(id: &Lookup) -> String {
    match id {
        Lookup::Doi(d) => format!("{IDCONV}?ids={}&format=json&tool=pigmed-guidelines", enc(d)),
        Lookup::Pmid(p) => format!("{IDCONV}?ids={}&idtype=pmid&format=json&tool=pigmed-guidelines", enc(p)),
        Lookup::Pmcid(p) => format!("{IDCONV}?ids={}&idtype=pmcid&format=json&tool=pigmed-guidelines", enc(p)),
    }
}

/// Parse an ID Converter JSON answer for the single requested id.
pub fn parse_idconv(v: &Value, requested: &str, evidence_url: &str) -> Result<Ids> {
    if v["status"] != "ok" {
        bail!("ID Converter status {}", v["status"]);
    }
    let recs = v["records"].as_array().context("ID Converter answer has no records")?;
    let rec = recs
        .iter()
        .find(|r| r["requested-id"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(requested)))
        .context("ID Converter answer lacks the requested id")?;
    let s = |k: &str| match &rec[k] {
        Value::String(x) if !x.is_empty() => Some(x.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    Ok(Ids {
        requested: requested.into(),
        doi: s("doi").map(|d| d.to_ascii_lowercase()),
        pmid: s("pmid"),
        pmcid: s("pmcid").filter(|p| p.starts_with("PMC")),
        evidence_url: evidence_url.into(),
        note: s("errmsg"),
    })
}

pub async fn resolve(f: &Fetcher, id: &Lookup) -> Result<Ids> {
    let url = idconv_url(id);
    let p = polite_get(f, &url).await?;
    if p.record.status != 200 {
        bail!("ID Converter HTTP {} for {url}", p.record.status);
    }
    let v: Value = serde_json::from_slice(&p.body).with_context(|| format!("ID Converter not JSON: {url}"))?;
    let requested = match id {
        Lookup::Doi(d) | Lookup::Pmid(d) | Lookup::Pmcid(d) => d.as_str(),
    };
    parse_idconv(&v, requested, &url)
}

// ---------------------------------------------------------------------------------------------------------------
// OA full text

pub fn oai_url(pmcid: &str) -> Result<String> {
    let n = pmcid.strip_prefix("PMC").filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let n = n.with_context(|| format!("not a PMCID: {pmcid}"))?;
    Ok(format!("{OAI}?verb=GetRecord&identifier=oai:pubmedcentral.nih.gov:{n}&metadataPrefix=pmc"))
}

pub enum OaJats {
    /// `article_xml` is the `<article>` element as served (the OAI envelope removed); `sets` its OAI setSpecs.
    Open { article_xml: String, sets: Vec<String>, record: Fetched, url: String },
    NotOpenAccess { reason: String, url: String },
}

/// Split an OAI-PMH GetRecord answer: Ok(Some((article xml, setSpecs))) / Ok(None) for cannotDisseminateFormat.
pub fn split_oai(body: &str) -> Result<Option<(String, Vec<String>)>> {
    let doc = Document::parse_with_options(body, ParsingOptions { allow_dtd: true, nodes_limit: 50_000_000 })
        .context("OAI answer is not XML")?;
    if let Some(e) = doc.root_element().children().find(|n| n.is_element() && n.tag_name().name() == "error") {
        let code = e.attribute("code").unwrap_or("");
        if code == "cannotDisseminateFormat" || code == "idDoesNotExist" {
            return Ok(None);
        }
        bail!("OAI error {code}: {}", e.text().unwrap_or("").trim());
    }
    let sets = doc.descendants().filter(|n| n.tag_name().name() == "setSpec").filter_map(|n| n.text()).map(|s| s.trim().to_string()).collect();
    let art = doc.descendants().find(|n| n.is_element() && n.tag_name().name() == "article").context("OAI record has no <article>")?;
    Ok(Some((body[art.range()].to_string(), sets)))
}

pub async fn open_access_jats(f: &Fetcher, pmcid: &str) -> Result<OaJats> {
    let url = oai_url(pmcid)?;
    let p = polite_get(f, &url).await?;
    if !(p.record.status == 200 || p.record.status == 400) {
        bail!("PMC OAI HTTP {} for {url}", p.record.status);
    }
    match split_oai(&p.text())? {
        Some((article_xml, sets)) => Ok(OaJats::Open { article_xml, sets, record: p, url }),
        None => Ok(OaJats::NotOpenAccess {
            reason: format!("{pmcid} is in PMC, but PMC does not distribute its full text as XML (OAI-PMH cannotDisseminateFormat; publisher does not allow XML download / not in the OA or author-manuscript sets); the free-to-read page is not scraped"),
            url,
        }),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// planning / extraction

/// What the source stated about the item (all optional except the URL it linked).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceRef {
    /// Publisher / article URL as the source linked it (kept as the `related` relation).
    pub source_url: String,
    /// Source page the link or id was read from.
    pub evidence_url: Option<String>,
    pub source_title: Option<String>,
    /// The source's own label for the item ("guideline", "clinical_practice_guideline", ...).
    pub item_type: Option<String>,
    /// doc_key prefix (usually the source id or its historic prefix).
    pub key_prefix: String,
    /// How the id was obtained: "source_doi", "source_pmid", "epmc_enumeration", "exact_title".
    pub id_route: String,
    /// Source-stated facts to carry as document warnings (e.g. a society rights notice on its landing page).
    #[serde(default)]
    pub notes: Vec<String>,
}

pub enum Planned {
    Open(PlanRow),
    Closed { ids: Option<Ids>, reason: String },
}

/// Resolve an id the source states and plan the OA copy if there is one.
pub async fn plan(src: &SourceEntry, f: &Fetcher, id: Lookup, sref: SourceRef, extra: Value) -> Result<Planned> {
    let ids = match resolve(f, &id).await {
        Ok(ids) => ids,
        Err(e) => return Ok(Planned::Closed { ids: None, reason: format!("PMC ID Converter lookup failed: {e:#}") }),
    };
    let Some(pmcid) = ids.pmcid.clone() else {
        let why = ids.note.clone().unwrap_or_else(|| "no PMCID returned".into());
        return Ok(Planned::Closed { ids: Some(ids), reason: format!("no PMC copy ({why})") });
    };
    plan_pmcid(src, f, &pmcid, Some(ids), sref, extra).await
}

/// Plan from a PMCID an official API returned (ID Converter or Europe PMC search).
pub async fn plan_pmcid(src: &SourceEntry, f: &Fetcher, pmcid: &str, ids: Option<Ids>, sref: SourceRef, extra: Value) -> Result<Planned> {
    match open_access_jats(f, pmcid).await {
        Err(e) => Ok(Planned::Closed { ids, reason: format!("PMC OAI fetch failed: {e:#}") }),
        Ok(OaJats::NotOpenAccess { reason, .. }) => Ok(Planned::Closed { ids, reason }),
        Ok(OaJats::Open { article_xml, sets, url, .. }) => {
            let sha = crate::common::sha256_hex(article_xml.as_bytes());
            let meta = match jats_document(&article_xml) {
                Ok(m) => m,
                Err(e) => return Ok(Planned::Closed { ids, reason: format!("{pmcid} OA JATS could not be mapped: {e:#}") }),
            };
            if meta.pmcid.as_deref().is_some_and(|p| p != pmcid) {
                bail!("{pmcid}: OAI record carries a different PMCID {:?}", meta.pmcid);
            }
            let title = meta.title.clone();
            let mut extra = if extra.is_object() { extra } else { json!({}) };
            extra["pmc_crosswalk"] = json!({
                "pmcid": pmcid, "ids": ids, "oai_url": url, "oai_sets": sets, "jats_sha256": sha,
                "source": serde_json::to_value(&sref)?, "jats_doi": meta.doi, "jats_pmid": meta.pmid,
            });
            Ok(Planned::Open(PlanRow {
                source_id: src.id.clone(),
                doc_key: format!("{}-{}-{}", sref.key_prefix, pmcid.to_ascii_lowercase(), &sha[..12]),
                url: format!("https://pmc.ncbi.nlm.nih.gov/articles/{pmcid}/"),
                title: Some(title),
                rendition: Some("jats".into()),
                version: None,
                extra,
            }))
        }
    }
}

/// For a `Planned::Closed` whose id IS in PMC (but not distributed as XML full text, e.g. ADA Standards of Care
/// chapters): a relation-only link to the PMC record, for the adapter's own document. No body is taken.
pub fn pmc_relation(ids: &Ids, reason: &str) -> Option<Relation> {
    let pmcid = ids.pmcid.as_ref()?;
    Some(Relation {
        rel: "related".into(),
        target: format!("pmc:{pmcid}"),
        scope: Some(format!("PMC record of the same article; relation only, no text taken: {reason}")),
        evidence_url: Some(ids.evidence_url.clone()),
    })
}

/// Convenience for adapters that hold a DOI and/or PMCID (e.g. `extra.doi`, `extra.oa_crosswalk.{doi,pmcid}`):
/// a PMCID is planned directly (after an ID Converter lookup for its DOI/PMID), else the DOI is resolved.
pub async fn plan_ids(src: &SourceEntry, f: &Fetcher, doi: Option<&str>, pmcid: Option<&str>, sref: SourceRef, extra: Value) -> Result<Planned> {
    match (pmcid.filter(|p| p.starts_with("PMC")), doi.and_then(normalize_doi)) {
        (Some(p), _) => {
            let ids = resolve(f, &Lookup::Pmcid(p.to_string())).await.ok();
            plan_pmcid(src, f, p, ids, sref, extra).await
        }
        (None, Some(d)) => plan(src, f, Lookup::Doi(d), sref, extra).await,
        (None, None) => Ok(Planned::Closed { ids: None, reason: "no DOI or PMCID".into() }),
    }
}

/// (doi, pmcid) from the PlanRow.extra keys other adapters already use: `doi`, `source_doi`,
/// `oa_crosswalk.{doi,pmcid}`, `pmcid`. Values are only read, never inferred.
pub fn ids_from_extra(extra: &Value) -> (Option<String>, Option<String>) {
    let s = |v: &Value| v.as_str().map(str::trim).filter(|x| !x.is_empty()).map(String::from);
    let doi = s(&extra["oa_crosswalk"]["doi"]).or_else(|| s(&extra["doi"])).or_else(|| s(&extra["source_doi"])).and_then(|d| normalize_doi(&d));
    let pmcid = s(&extra["oa_crosswalk"]["pmcid"]).or_else(|| s(&extra["pmcid"])).filter(|p| p.starts_with("PMC"));
    (doi, pmcid)
}

/// Every DOI / PMCID / PMID an adapter recorded in `extra`, deduplicated, in key order: `oa_crosswalk.{pmcid,doi}`,
/// `pmcid`, `stated_pmcids[]`, `doi`, `source_doi`, `stated_dois[]`, `publisher_dois[]`, `pmid`, `stated_pmids[]`.
/// Other ids (e.g. BMJ article ids) are not converted here: resolve them through their own API first.
pub fn lookups_from_extra(extra: &Value) -> Vec<Lookup> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    let strs = |v: &Value| -> Vec<String> {
        match v {
            Value::String(x) => vec![x.trim().to_string()],
            Value::Array(a) => a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).collect(),
            Value::Number(n) => vec![n.to_string()],
            _ => vec![],
        }
    };
    let mut add = |l: Lookup, key: String| {
        if seen.insert(key) {
            out.push(l);
        }
    };
    for v in [&extra["oa_crosswalk"]["pmcid"], &extra["pmcid"], &extra["stated_pmcids"]] {
        for p in strs(v).into_iter().filter(|p| p.starts_with("PMC") && p[3..].bytes().all(|b| b.is_ascii_digit()) && p.len() > 3) {
            add(Lookup::Pmcid(p.clone()), p);
        }
    }
    for v in [&extra["oa_crosswalk"]["doi"], &extra["doi"], &extra["source_doi"], &extra["stated_dois"], &extra["publisher_dois"]] {
        for d in strs(v).iter().filter_map(|d| normalize_doi(d)) {
            add(Lookup::Doi(d.clone()), d);
        }
    }
    for v in [&extra["pmid"], &extra["stated_pmids"]] {
        for p in strs(v).into_iter().filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
            add(Lookup::Pmid(p.clone()), format!("pmid:{p}"));
        }
    }
    out
}

/// Per-id results for a list of ids (DOIs, PMCIDs, PMIDs): `Planned::Open(row)` = OA/text-mining JATS full text,
/// `Planned::Closed { ids, reason }` = no full text (use [`pmc_relation`] for a relation-only `pmc:` link when
/// `ids.pmcid` is set). Two ids resolving to the same PMCID yield one Open row; the duplicate is Closed with a
/// "same PMC record as" reason. `sref.key_prefix`/`item_type` apply to every row.
pub async fn plan_all(src: &SourceEntry, f: &Fetcher, lookups: &[Lookup], sref: &SourceRef, extra: &Value) -> Result<Vec<(Lookup, Planned)>> {
    let mut out = vec![];
    let mut planned: HashSet<String> = HashSet::new();
    for l in lookups {
        let ids = match resolve(f, l).await {
            Ok(ids) => ids,
            Err(e) => {
                out.push((l.clone(), Planned::Closed { ids: None, reason: format!("PMC ID Converter lookup failed: {e:#}") }));
                continue;
            }
        };
        let Some(pmcid) = ids.pmcid.clone() else {
            let why = ids.note.clone().unwrap_or_else(|| "no PMCID returned".into());
            out.push((l.clone(), Planned::Closed { ids: Some(ids), reason: format!("no PMC copy ({why})") }));
            continue;
        };
        if !planned.insert(pmcid.clone()) {
            out.push((l.clone(), Planned::Closed { ids: Some(ids), reason: format!("same PMC record as an earlier id ({pmcid})") }));
            continue;
        }
        let r = plan_pmcid(src, f, &pmcid, Some(ids), sref.clone(), extra.clone()).await?;
        out.push((l.clone(), r));
    }
    Ok(out)
}

/// Restriction recorded on PMC copies whose JATS `<permissions>` carry no licence (same text as the round-2
/// `r2b_crosswalk::finish`, which therefore no longer fires on crosswalk docs).
pub const LICENCE_LESS: &str = "No licence in the PMC JATS <permissions>: free-to-read PMC copy, not open access. Kept for the private corpus as restricted (project decision 2026-11-17); not for redistribution.";

pub fn is_crosswalk_row(row: &PlanRow) -> bool {
    row.extra["pmc_crosswalk"]["pmcid"].is_string()
}

pub async fn extract(src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
    let cw = &row.extra["pmc_crosswalk"];
    let pmcid = cw["pmcid"].as_str().context("not a crosswalk row")?;
    let sref: SourceRef = serde_json::from_value(cw["source"].clone()).context("crosswalk source ref")?;
    let OaJats::Open { article_xml, sets, record, url } = open_access_jats(f, pmcid).await? else {
        bail!("{pmcid} left the PMC Open Access subset after planning; re-plan");
    };
    let sha = crate::common::sha256_hex(article_xml.as_bytes());
    if cw["jats_sha256"].as_str() != Some(sha.as_str()) {
        bail!("{pmcid} JATS changed after planning; re-plan");
    }
    let mut m = jats_document(&article_xml)?;
    let ids: Option<Ids> = serde_json::from_value(cw["ids"].clone()).ok().flatten();
    // Identity: JATS ids must agree with the converter's; a disagreement is reported, never resolved by guessing.
    if let Some(ids) = &ids {
        if ids.pmid.is_some() && m.pmid.is_some() && ids.pmid != m.pmid {
            m.warnings.push(format!("PMID differs: ID Converter {:?}, JATS {:?}", ids.pmid, m.pmid));
        }
        if ids.doi.is_some() && m.doi.is_some() && ids.doi != m.doi {
            m.warnings.push(format!("DOI differs: ID Converter {:?}, JATS {:?}", ids.doi, m.doi));
        }
    }
    let n = pmcid.trim_start_matches("PMC");
    for b in &mut m.blocks {
        if let BlockKind::Figure { src_url: Some(u), .. } = &mut b.kind {
            if !u.starts_with("http") {
                *u = format!("https://pmc.ncbi.nlm.nih.gov/articles/instance/{n}/bin/{u}");
            }
        }
    }
    let mut rights = m.rights.clone();
    rights.evidence_urls.push(url.clone());
    if rights.status == "unknown" {
        // User decision (2026-11-17): PMC copies without a licence in <permissions> are kept, as restricted.
        rights.status = "restricted".into();
        rights.restrictions.push(LICENCE_LESS.into());
    }
    if sets.iter().any(|s| s == "pmc-open") {
        rights.note = Some(format!(
            "{} Text taken from the PMC Open Access subset (OAI set pmc-open).",
            rights.note.clone().unwrap_or_default()
        ).trim().to_string());
    }
    let pmc_page = format!("https://pmc.ncbi.nlm.nih.gov/articles/{pmcid}/");
    let mut relations = vec![Relation {
        rel: "rendition_of".into(),
        target: format!("pmc:{pmcid}"),
        scope: Some("full text from the PMC Open Access JATS record (OAI-PMH metadataPrefix=pmc)".into()),
        evidence_url: Some(url.clone()),
    }];
    if !sref.source_url.is_empty() {
        relations.push(Relation {
            rel: "related".into(),
            target: sref.source_url.clone(),
            scope: Some(format!("publisher version as linked by the source ({})", sref.id_route)),
            evidence_url: sref.evidence_url.clone(),
        });
    }
    let mut attachments = vec![json!({"url": pmc_page, "label": format!("PubMed Central {pmcid}"), "role": "pmc_article_page", "rights": rights.license})];
    if let Some(d) = &m.doi {
        attachments.push(json!({"url": format!("https://doi.org/{d}"), "label": "Version of record (publisher)", "role": "publisher_version_of_record", "rights": "unknown"}));
    }
    for (u, label) in &m.supplements {
        let u = if u.starts_with("http") { u.clone() } else { format!("https://pmc.ncbi.nlm.nih.gov/articles/instance/{n}/bin/{u}") };
        attachments.push(json!({"url": u, "label": label, "role": "supplement", "rights": "unknown"}));
    }
    let mut warnings = m.warnings.clone();
    warnings.extend(sref.notes.iter().cloned());
    if sref.id_route == "exact_title" {
        warnings.push("PMC record matched by the source-printed title (exact, normalised) and journal, not by a source-stated DOI.".into());
    }
    if let Some(t) = &sref.source_title {
        if norm_title(t) != norm_title(&m.title) && sref.id_route != "epmc_enumeration" {
            warnings.push(format!("Source link text differs from the article title: {t:?}"));
        }
    }
    let published = ["epub", "ppub", "pub", "collection"]
        .iter()
        .find_map(|k| m.dates.iter().find(|d| d.label == format!("pub-date ({k})")).and_then(|d| d.normalized.clone()));
    let mut fetches = vec![record.record];
    if let Some(ids) = &ids {
        if let Ok(p) = polite_get(f, &ids.evidence_url).await {
            fetches.insert(0, p.record);
        }
    }
    Ok(GuidelineDoc {
        source_id: src.id.clone(),
        doc_key: row.doc_key.clone(),
        work_key: m.doi.as_deref().map(common::slug).or_else(|| Some(pmcid.to_ascii_lowercase())),
        item_type: sref.item_type.clone().or_else(|| Some("guideline".into())),
        title: m.title.clone(),
        url: pmc_page,
        publisher: m.publisher.clone(),
        issuers: vec![],
        version: None,
        status: "unknown".into(),
        published,
        updated: None,
        date_observations: m.dates.clone(),
        relations,
        language: m.language.clone(),
        country: None,
        doi: m.doi.clone(),
        pmid: m.pmid.clone(),
        rights,
        search_metadata: json!({
            "pmc_crosswalk": {"pmcid": pmcid, "id_route": sref.id_route, "source_url": sref.source_url,
                "source_evidence_url": sref.evidence_url, "oai_sets": sets, "jats_sha256": sha},
            "jats": {"article_type": m.article_type, "journal": m.journal, "subjects": m.subjects},
        }),
        blocks: m.blocks,
        attachments,
        fetches,
        warnings,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Europe PMC search (enumeration, exact-title lookup)

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpmcHit {
    pub id: String,
    pub source: String,
    pub pmid: Option<String>,
    pub pmcid: Option<String>,
    pub doi: Option<String>,
    pub title: String,
    pub journal_issn: Vec<String>,
    pub journal: Option<String>,
    pub first_publication_date: Option<String>,
    pub pub_types: Vec<String>,
    pub is_open_access: bool,
    pub in_pmc: bool,
    pub license: Option<String>,
    /// Bibliographic locators as Europe PMC states them (`journalInfo.volume` / `.issue`, `pageInfo`, `pubYear`),
    /// for citation checks (volume/issue/first page) without re-reading the cached answer.
    #[serde(default)]
    pub volume: Option<String>,
    #[serde(default)]
    pub issue: Option<String>,
    #[serde(default)]
    pub page_info: Option<String>,
    #[serde(default)]
    pub pub_year: Option<String>,
}

impl EpmcHit {
    /// First page of `pageInfo` ("3898-3911" → "3898", "e123" → "e123").
    pub fn first_page(&self) -> Option<&str> {
        self.page_info.as_deref().map(|p| p.split(['-', '–']).next().unwrap_or("").trim()).filter(|p| !p.is_empty())
    }
}

pub fn epmc_hits(v: &Value) -> Vec<EpmcHit> {
    let s = |x: &Value| x.as_str().filter(|s| !s.is_empty()).map(String::from);
    // volume/issue/year are strings in the API, but accept numbers too
    let sn = |x: &Value| match x {
        Value::String(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    v["resultList"]["result"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| EpmcHit {
                    id: s(&r["id"]).unwrap_or_default(),
                    source: s(&r["source"]).unwrap_or_default(),
                    pmid: s(&r["pmid"]),
                    pmcid: s(&r["pmcid"]).filter(|p| p.starts_with("PMC")),
                    doi: s(&r["doi"]).map(|d| d.to_ascii_lowercase()),
                    title: s(&r["title"]).unwrap_or_default(),
                    journal_issn: ["issn", "essn"].iter().filter_map(|k| s(&r["journalInfo"]["journal"][*k])).collect(),
                    journal: s(&r["journalInfo"]["journal"]["title"]),
                    first_publication_date: s(&r["firstPublicationDate"]),
                    pub_types: r["pubTypeList"]["pubType"].as_array().map(|a| a.iter().filter_map(s).collect()).unwrap_or_default(),
                    is_open_access: r["isOpenAccess"] == "Y",
                    in_pmc: r["inPMC"] == "Y",
                    license: s(&r["license"]),
                    volume: sn(&r["journalInfo"]["volume"]).or_else(|| sn(&r["journalVolume"])),
                    issue: sn(&r["journalInfo"]["issue"]).or_else(|| sn(&r["issue"])),
                    page_info: sn(&r["pageInfo"]),
                    pub_year: sn(&r["pubYear"]).or_else(|| sn(&r["journalInfo"]["yearOfPublication"])),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn epmc_search_url(query: &str, page_size: usize, cursor: &str) -> String {
    format!("{EPMC_SEARCH}?query={}&resultType=core&format=json&pageSize={page_size}&cursorMark={}", enc(query), enc(cursor))
}

pub struct EpmcPage {
    pub hit_count: u64,
    pub hits: Vec<EpmcHit>,
    pub urls: Vec<String>,
}

/// Europe PMC search, `resultType=core`, cursor paging up to `max` hits (the API's own order unless the
/// query has `sort`). `hit_count` is the API's total for the query, for completeness accounting.
pub async fn epmc_search(f: &Fetcher, query: &str, max: usize) -> Result<EpmcPage> {
    let mut cursor = "*".to_string();
    let mut out = EpmcPage { hit_count: 0, hits: vec![], urls: vec![] };
    for _ in 0..50 {
        let url = epmc_search_url(query, max.clamp(1, 100), &cursor);
        let p = polite_get(f, &url).await?;
        if p.record.status != 200 {
            bail!("Europe PMC search HTTP {} for {url}", p.record.status);
        }
        let v: Value = serde_json::from_slice(&p.body).context("Europe PMC search not JSON")?;
        out.hit_count = v["hitCount"].as_u64().unwrap_or(0);
        out.urls.push(url);
        let hits = epmc_hits(&v);
        let n = hits.len();
        out.hits.extend(hits);
        let next = v["nextCursorMark"].as_str().unwrap_or("").to_string();
        if n == 0 || out.hits.len() >= max || next.is_empty() || next == cursor {
            break;
        }
        cursor = next;
    }
    out.hits.truncate(max);
    Ok(out)
}

/// Registry-configured enumeration through Europe PMC for sources whose own catalog is closed or partial:
/// `config.oa_enumeration = {"query": "<Europe PMC query>", "coverage_query": "<same without OPEN_ACCESS:y>",
/// "max": 300, "item_type": "clinical_practice_guideline", "scope": "<what the query covers>"}`.
/// Hits are ordered newest first (first publication date, then id); each OA hit with a PMCID is planned through
/// PMC OAI. A hit Europe PMC calls open but PMC OAI does not serve, or one without a PMCID, becomes an
/// unresolved row (`discovery_error`). Every row carries the query, the API hit counts and the scope note, so
/// completeness is explicit. `skip_pmcids` lets an adapter avoid duplicating copies it planned another way.
pub async fn enumerate(src: &SourceEntry, f: &Fetcher, key_prefix: &str, limit: Option<usize>, skip_pmcids: &HashSet<String>) -> Result<Vec<PlanRow>> {
    let Some(cfg) = src.config.get("oa_enumeration") else { return Ok(vec![]) };
    let query = cfg["query"].as_str().context("oa_enumeration.query missing")?;
    let max = cfg["max"].as_u64().unwrap_or(300).min(1000) as usize;
    let page = epmc_search(f, query, max).await?;
    let coverage = match cfg["coverage_query"].as_str() {
        Some(q) => match epmc_search(f, q, 1).await {
            Ok(p) => json!({"query": q, "hit_count": p.hit_count, "url": p.urls.first()}),
            Err(e) => json!({"query": q, "error": format!("{e:#}")}),
        },
        None => Value::Null,
    };
    let enumeration = json!({"api": "Europe PMC REST search", "query": query, "hit_count": page.hit_count, "fetched": page.hits.len(),
        "max": max, "urls": page.urls, "coverage": coverage, "scope": cfg["scope"],
        "completeness": "Only items Europe PMC indexes for this query; items outside it (other article types, unindexed, not open access) are not enumerated here."});
    let mut hits = page.hits;
    hits.sort_by(|a, b| b.first_publication_date.cmp(&a.first_publication_date).then(a.id.cmp(&b.id)));
    let item_type = cfg["item_type"].as_str().map(String::from);
    let mut rows = vec![];
    let mut seen = HashSet::new();
    for h in hits {
        if limit.is_some_and(|n| rows.len() >= n) {
            break;
        }
        let key = h.pmcid.clone().unwrap_or_else(|| h.id.clone());
        if !seen.insert(key.clone()) || h.pmcid.as_ref().is_some_and(|p| skip_pmcids.contains(p)) {
            continue;
        }
        let source_url = h.doi.as_ref().map(|d| format!("https://doi.org/{d}")).unwrap_or_default();
        let extra = json!({"oa_enumeration": enumeration, "epmc_hit": h});
        let Some(pmcid) = h.pmcid.clone() else {
            rows.push(PlanRow { source_id: src.id.clone(), doc_key: format!("{key_prefix}-epmc-{}-unresolved", common::slug(&key)), url: source_url,
                title: Some(h.title.clone()), rendition: None, version: None,
                extra: json!({"discovery_error": "Europe PMC hit has no PMCID; no PMC Open Access JATS route", "oa_enumeration": enumeration, "epmc_hit": h}) });
            continue;
        };
        let ids = Ids { requested: h.id.clone(), doi: h.doi.clone(), pmid: h.pmid.clone(), pmcid: Some(pmcid.clone()), evidence_url: enumeration["urls"][0].as_str().unwrap_or("").into(), note: None };
        let sref = SourceRef { source_url: source_url.clone(), evidence_url: ids.evidence_url.clone().into(), source_title: Some(h.title.clone()), item_type: item_type.clone(),
            key_prefix: key_prefix.into(), id_route: "epmc_enumeration".into(), notes: vec![] };
        match plan_pmcid(src, f, &pmcid, Some(ids), sref, extra).await? {
            Planned::Open(row) => rows.push(row),
            Planned::Closed { reason, .. } => rows.push(PlanRow { source_id: src.id.clone(), doc_key: format!("{key_prefix}-{}-unresolved", pmcid.to_ascii_lowercase()),
                url: format!("https://pmc.ncbi.nlm.nih.gov/articles/{pmcid}/"), title: Some(h.title.clone()), rendition: None, version: None,
                extra: json!({"discovery_error": format!("Europe PMC enumeration hit: {reason}"), "oa_enumeration": enumeration, "epmc_hit": h}) }),
        }
    }
    Ok(rows)
}

pub fn row_pmcid(row: &PlanRow) -> Option<String> {
    row.extra["pmc_crosswalk"]["pmcid"].as_str().map(String::from)
}

pub fn norm_title(t: &str) -> String {
    let t: String = t.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' }).collect();
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One Europe PMC hit whose normalised title equals `title` (and ISSN in `issns` when non-empty), else None.
pub fn exact_title_hit<'a>(hits: &'a [EpmcHit], title: &str, issns: &[&str]) -> Option<&'a EpmcHit> {
    let want = norm_title(title);
    let ok: Vec<&EpmcHit> = hits
        .iter()
        .filter(|h| norm_title(&h.title) == want)
        .filter(|h| issns.is_empty() || h.journal_issn.iter().any(|i| issns.contains(&i.as_str())))
        .collect();
    (ok.len() == 1).then(|| ok[0])
}

pub async fn resolve_exact_title(f: &Fetcher, title: &str, issns: &[&str]) -> Result<Option<EpmcHit>> {
    let words = norm_title(title);
    if words.split(' ').count() < 4 {
        bail!("title too short for an exact-title lookup");
    }
    let mut q = format!("TITLE:\"{}\"", words.replace('"', ""));
    if !issns.is_empty() {
        q.push_str(&format!(" AND ({})", issns.iter().map(|i| format!("ISSN:\"{i}\"")).collect::<Vec<_>>().join(" OR ")));
    }
    let page = epmc_search(f, &q, 25).await?;
    Ok(exact_title_hit(&page.hits, title, issns).cloned())
}

// ---------------------------------------------------------------------------------------------------------------
// JATS → guideline blocks

pub struct JatsDoc {
    pub title: String,
    pub doi: Option<String>,
    pub pmid: Option<String>,
    pub pmcid: Option<String>,
    pub article_type: Option<String>,
    pub journal: Option<String>,
    pub publisher: Option<String>,
    pub language: Option<String>,
    pub subjects: Vec<String>,
    pub dates: Vec<DateObservation>,
    pub rights: Rights,
    pub blocks: Vec<Block>,
    /// (href, label/caption) of `<supplementary-material>`.
    pub supplements: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

fn nm<'a>(n: &Node<'a, '_>) -> &'a str {
    n.tag_name().name()
}
fn kids<'a, 'i>(n: Node<'a, 'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(|c| c.is_element())
}
fn kid<'a, 'i>(n: Node<'a, 'i>, t: &str) -> Option<Node<'a, 'i>> {
    kids(n).find(|c| nm(c) == t)
}
fn desc<'a, 'i>(n: Node<'a, 'i>, t: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.descendants().skip(1).filter(move |c| c.is_element() && nm(c) == t)
}
fn href(n: Node) -> Option<String> {
    n.attributes().find(|a| a.name() == "href" && (a.namespace() == Some(XLINK) || a.namespace().is_none())).map(|a| a.value().trim().to_string()).filter(|s| !s.is_empty())
}

const BLOCKY: &[&str] = &[
    "p", "title", "label", "caption", "list-item", "td", "th", "tr", "sec", "fn", "def-item", "term", "def", "break",
    "statement", "boxed-text", "disp-quote", "attrib", "disp-formula",
];

fn text_into(n: Node, skip: &[&str], out: &mut String) {
    for c in n.children() {
        if c.is_text() {
            out.push_str(c.text().unwrap_or(""));
        } else if c.is_element() {
            let t = nm(&c);
            if skip.contains(&t) {
                continue;
            }
            if t == "tex-math" && n.children().any(|s| s.is_element() && nm(&s) == "math") {
                continue;
            }
            let b = BLOCKY.contains(&t);
            if b {
                out.push(' ');
            }
            text_into(c, skip, out);
            if b {
                out.push(' ');
            }
        }
    }
}
fn txt(n: Node, skip: &[&str]) -> String {
    let mut s = String::new();
    text_into(n, skip, &mut s);
    crate::common::collapse_ws(&s)
}
/// Nested block elements inside a <p> / <list-item> that become their own blocks.
const NESTED: &[&str] = &["list", "table-wrap", "table-wrap-group", "fig", "fig-group", "boxed-text", "disp-quote", "supplementary-material", "def-list", "statement"];

fn label_title(n: Node) -> String {
    let l = kid(n, "label").map(|x| txt(x, &[])).unwrap_or_default();
    let t = kid(n, "title").or_else(|| kid(n, "caption").and_then(|c| kid(c, "title"))).map(|x| txt(x, &[])).unwrap_or_default();
    crate::common::collapse_ws(&format!("{l} {t}"))
}

fn table_markdown(t: Node) -> String {
    let rows: Vec<Vec<String>> = desc(t, "tr")
        .map(|tr| kids(tr).filter(|c| matches!(nm(c), "th" | "td")).map(|c| txt(c, &[]).replace('|', "\\|")).collect::<Vec<_>>())
        .filter(|r: &Vec<String>| !r.is_empty())
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    let w = rows.iter().map(|r| r.len()).max().unwrap_or(1);
    let line = |r: &Vec<String>| {
        let mut c = r.clone();
        c.resize(w, String::new());
        format!("| {} |", c.join(" | "))
    };
    let mut out = vec![line(&rows[0]), format!("|{}|", vec!["---"; w].join("|"))];
    out.extend(rows[1..].iter().map(line));
    out.join("\n")
}

struct Walker {
    out: Vec<Block>,
    path: Vec<String>,
    n: usize,
    ids: HashSet<String>,
    warnings: Vec<String>,
    supplements: Vec<(String, String)>,
    unmapped: HashMap<String, usize>,
}

fn rec_box_kind(title: &str) -> bool {
    let t = title.to_lowercase();
    t.contains("recommendation") || t.contains("consensus statement") || t.contains("good practice") || t.contains("expert opinion") || t.contains("best practice statement")
        || t.contains("guidance statement")
}

/// One printed statement inside a recommendation box (see [`Walker::boxed`]).
struct BoxStmt {
    text: String,
    strength: Option<String>,
    certainty: Option<String>,
    id: Option<String>,
    /// the list item's own printed label (AASLD "Guidance statements" number their items)
    label: Option<String>,
    item: bool,
}

impl Walker {
    fn push(&mut self, id: Option<&str>, kind: BlockKind, text: String) {
        if text.trim().is_empty() && !matches!(kind, BlockKind::Figure { .. }) {
            return;
        }
        self.n += 1;
        let mut bid = id.map(String::from).unwrap_or_else(|| format!("j{:05}", self.n));
        if !self.ids.insert(bid.clone()) {
            bid = format!("{bid}-{}", self.n);
            self.ids.insert(bid.clone());
        }
        self.out.push(Block { block_id: bid, kind, text, heading_path: self.path.clone(), page: None });
    }

    fn heading(&mut self, n: Node, title: String) {
        let level = (self.path.len() + 2).min(6) as u8;
        self.push(n.attribute("id"), BlockKind::Heading { level }, title.clone());
    }

    fn container(&mut self, n: Node) {
        for c in kids(n) {
            self.element(c);
        }
    }

    fn section(&mut self, n: Node, fallback: &str) {
        let mut title = label_title(n);
        if title.is_empty() {
            title = fallback.to_string();
        }
        if title.is_empty() {
            self.container(n);
            return;
        }
        self.heading(n, title.clone());
        self.path.push(title);
        self.container(n);
        self.path.pop();
    }

    fn element(&mut self, c: Node) {
        match nm(&c) {
            "title" | "label" | "sec-meta" | "object-id" => {}
            "sec" => self.section(c, ""),
            "app" => self.section(c, "Appendix"),
            "ack" => self.section(c, "Acknowledgments"),
            "glossary" => self.section(c, "Glossary"),
            "notes" => self.section(c, "Notes"),
            "app-group" | "floats-group" | "fn-group" | "body" | "back" => self.container(c),
            "ref-list" => {
                let k = desc(c, "ref").count();
                self.warnings.push(format!("reference list ({k} references) not emitted as blocks"));
            }
            "p" => self.para(c, BlockKind::Paragraph),
            "list" => self.list(c),
            "table-wrap" => self.table(c),
            "table-wrap-group" | "fig-group" => self.container(c),
            "fig" => self.fig(c),
            "boxed-text" => self.boxed(c),
            "fn" => {
                let t = txt(c, &[]);
                self.push(c.attribute("id"), BlockKind::Footnote, t);
            }
            "disp-quote" | "statement" | "verse-group" | "disp-formula" | "def-list" | "speech" | "preformat" | "code" => {
                let t = txt(c, &[]);
                self.push(c.attribute("id"), BlockKind::Paragraph, t);
            }
            "supplementary-material" => self.supplement(c),
            "graphic" => {
                if let Some(h) = href(c) {
                    self.push(c.attribute("id"), BlockKind::Figure { src_url: Some(h), caption: None }, String::new());
                }
            }
            "alternatives" => {
                if let Some(x) = kids(c).next() {
                    self.element(x);
                }
            }
            other => {
                let t = txt(c, &[]);
                if !t.is_empty() {
                    *self.unmapped.entry(other.to_string()).or_default() += 1;
                    self.push(c.attribute("id"), BlockKind::Paragraph, t);
                }
            }
        }
    }

    fn para(&mut self, p: Node, kind: BlockKind) {
        let t = txt(p, NESTED);
        self.push(p.attribute("id"), kind, t);
        for c in kids(p) {
            if NESTED.contains(&nm(&c)) {
                self.element(c);
            }
        }
    }

    fn list(&mut self, l: Node) {
        for it in kids(l) {
            match nm(&it) {
                "list-item" => {
                    let lab = kid(it, "label").map(|x| txt(x, &[])).unwrap_or_default();
                    let body = txt(it, &["label", "list", "table-wrap", "fig", "boxed-text"]);
                    let text = crate::common::collapse_ws(&format!("{lab} {body}"));
                    self.push(it.attribute("id"), BlockKind::ListItem, text);
                    for c in it.descendants().skip(1).filter(|d| d.is_element() && matches!(nm(d), "list" | "table-wrap" | "fig" | "boxed-text")) {
                        // only the outermost nested block (its own walker handles deeper ones)
                        if c.ancestors().skip(1).take_while(|a| *a != it).any(|a| matches!(nm(&a), "list" | "table-wrap" | "fig" | "boxed-text")) {
                            continue;
                        }
                        self.element(c);
                    }
                }
                "title" | "label" => {
                    let t = txt(it, &[]);
                    self.push(None, BlockKind::Paragraph, t);
                }
                _ => self.element(it),
            }
        }
    }

    fn caption(n: Node) -> String {
        let l = kid(n, "label").map(|x| txt(x, &[])).unwrap_or_default();
        let c = kid(n, "caption").map(|x| txt(x, &[])).unwrap_or_default();
        crate::common::collapse_ws(&format!("{l} {c}"))
    }

    fn table(&mut self, tw: Node) {
        let caption = Self::caption(tw);
        let tables: Vec<Node> = desc(tw, "table").collect();
        if tables.is_empty() {
            let gs: Vec<String> = desc(tw, "graphic").filter_map(href).collect();
            if gs.is_empty() {
                self.warnings.push(format!("table {:?} has neither a table nor an image", tw.attribute("id")));
            } else {
                self.warnings.push(format!("table {:?} is only available as an image", tw.attribute("id")));
            }
            for g in gs {
                self.push(tw.attribute("id"), BlockKind::Figure { src_url: Some(g), caption: Some(caption.clone()) }, caption.clone());
            }
        }
        for t in tables {
            let md = table_markdown(t);
            let inline: Vec<String> = desc(t, "inline-graphic").chain(desc(t, "graphic")).filter_map(href).collect();
            if !md.chars().any(|c| c.is_alphanumeric()) && !inline.is_empty() {
                // e.g. OUP ESJ synoptic recommendation tables: every cell is an image, so no text to keep.
                self.warnings.push(format!("table {:?} ({caption}) holds its content only as {} image(s); kept as figures, its text is not available", tw.attribute("id"), inline.len()));
                for g in inline {
                    self.push(tw.attribute("id"), BlockKind::Figure { src_url: Some(g), caption: (!caption.is_empty()).then(|| caption.clone()) }, caption.clone());
                }
                continue;
            }
            self.push(tw.attribute("id"), BlockKind::Table { caption: (!caption.is_empty()).then(|| caption.clone()) }, md);
        }
        for foot in desc(tw, "table-wrap-foot") {
            let t = txt(foot, &[]);
            self.push(None, BlockKind::Footnote, t);
        }
    }

    fn fig(&mut self, f: Node) {
        let caption = Self::caption(f);
        let gs: Vec<String> = desc(f, "graphic").filter_map(href).collect();
        if gs.is_empty() {
            self.push(f.attribute("id"), BlockKind::Figure { src_url: None, caption: Some(caption.clone()) }, caption);
            return;
        }
        for g in gs {
            self.push(f.attribute("id"), BlockKind::Figure { src_url: Some(g), caption: (!caption.is_empty()).then(|| caption.clone()) }, caption.clone());
        }
    }

    fn supplement(&mut self, s: Node) {
        let cap = Self::caption(s);
        for h in s.descendants().filter(|d| d.is_element()).filter_map(href) {
            self.supplements.push((h, cap.clone()));
        }
        if !cap.is_empty() {
            self.push(s.attribute("id"), BlockKind::Paragraph, format!("Supplementary material: {cap}"));
        }
    }

    fn boxed(&mut self, b: Node) {
        // Box title: own title/caption, else its first section's title.
        let first_sec = kids(b).find(|c| nm(c) == "sec");
        let mut title = label_title(b);
        if title.is_empty() {
            title = kid(b, "caption").map(|c| txt(c, &[])).unwrap_or_default();
        }
        let inner_title = first_sec.map(label_title).unwrap_or_default();
        let box_title = if title.is_empty() { inner_title.clone() } else { title.clone() };
        if !rec_box_kind(&box_title) {
            if box_title.is_empty() {
                self.container(b);
            } else {
                self.section(b, "");
            }
            return;
        }
        // Recommendation box: statement paragraphs, each followed by its own printed grade lines.
        let scope = if title.is_empty() { first_sec.unwrap_or(b) } else { b };
        let mut ps: Vec<Node> = vec![];
        // A <p> that only wraps a list (AASLD: <p><list><list-item>...) is a container, not a statement: its
        // items are the statements.
        let wrapper = |p: Node| nm(&p) == "p" && txt(p, NESTED).is_empty();
        for d in scope.descendants().skip(1).filter(|d| d.is_element() && matches!(nm(d), "p" | "list-item")) {
            if wrapper(d) {
                continue;
            }
            if d.ancestors().skip(1).take_while(|a| *a != scope).any(|a| matches!(nm(&a), "list-item" | "table-wrap" | "fig") || (nm(&a) == "p" && !wrapper(a))) {
                continue;
            }
            ps.push(d);
        }
        let lab = kid(b, "label").map(|x| txt(x, &[])).filter(|s| !s.is_empty());
        self.path.push(box_title.clone());
        let strength_re = regex::Regex::new(r"(?i)^\s*(?:strength of (?:the )?recommendation|strength|recommendation strength|grade of recommendation)\s*:\s*(.+)$").unwrap();
        let cert_re = regex::Regex::new(r"(?i)^\s*(?:quality of evidence|certainty of (?:the )?evidence|level of evidence|evidence quality|certainty)\s*:\s*(.+)$").unwrap();
        let mut cur: Option<BoxStmt> = None;
        let flush = |w: &mut Walker, cur: &mut Option<BoxStmt>| {
            if let Some(x) = cur.take() {
                w.push(x.id.as_deref(), BlockKind::Recommendation {
                    label: x.label.or_else(|| lab.clone()),
                    strength: x.strength,
                    certainty: x.certainty,
                    population: None,
                    grading_system: None,
                    qualifier: Some(box_title.clone()),
                }, x.text);
            }
        };
        let item_skip: Vec<&str> = std::iter::once("label").chain(NESTED.iter().copied()).collect();
        for p in ps {
            let item = nm(&p) == "list-item";
            let (item_label, t) = if item {
                (kid(p, "label").map(|x| txt(x, &[])).filter(|s| !s.is_empty()), txt(p, &item_skip))
            } else {
                (None, txt(p, NESTED))
            };
            if let Some(c) = strength_re.captures(&t) {
                match &mut cur {
                    Some(x) if x.strength.is_none() => x.strength = Some(c[1].trim().to_string()),
                    _ => self.push(p.attribute("id"), BlockKind::Paragraph, t.clone()),
                }
            } else if let Some(c) = cert_re.captures(&t) {
                match &mut cur {
                    Some(x) if x.certainty.is_none() => x.certainty = Some(c[1].trim().to_string()),
                    _ => self.push(p.attribute("id"), BlockKind::Paragraph, t.clone()),
                }
            } else if let Some(x) = cur.as_mut().filter(|x| x.strength.is_none() && x.certainty.is_none() && !x.item && !item) {
                // Ungraded consecutive paragraphs of one box are one printed statement (list items never merge:
                // each item is its own printed statement).
                x.text = format!("{} {}", x.text, t);
            } else {
                flush(self, &mut cur);
                cur = Some(BoxStmt { text: t, strength: None, certainty: None, id: p.attribute("id").map(String::from), label: item_label, item });
            }
        }
        flush(self, &mut cur);
        self.path.pop();
        for t in desc(b, "table-wrap").collect::<Vec<_>>() {
            self.table(t);
        }
    }
}

/// Paragraphs in a recommendations section that print their own grade parenthetically, e.g.
/// "We suggest X (conditional recommendation, very low certainty of evidence)", and paragraphs that start with
/// a printed label "Recommendation 3.1". Grades are copied verbatim; nothing is inherited from neighbours.
///
/// Also (round 3, I2): guideline-panel statements anywhere in the statement ("For older adults ..., the ASH
/// guideline panel suggests X (conditional recommendation based on low certainty in the evidence of effects
/// ⨁⨁◯◯)"), list items in recommendation sections that end with their printed grade ("... (Clinical consensus,
/// Strong Recommendation)."; ASAM), and the first statement under a heading that is itself a printed label
/// ("Recommendation 2a", label taken from that heading).
fn mark_inline_recommendations(blocks: &mut [Block]) {
    let label_re = regex::Regex::new(r"(?i)^(Recommendation\s+[0-9]+(?:\.[0-9]+)*[a-z]?)\b").unwrap();
    let heading_label_re = regex::Regex::new(r"(?i)^(?:recommendations?|statement|guidance statement)\s+[0-9]+[a-z]?(?:\.[0-9]+[a-z]?)*$").unwrap();
    let simple_strength = regex::Regex::new(r"(?i)\b(strong|conditional|weak) recommendation\b").unwrap();
    let cert_re = regex::Regex::new(r"(?i)\b(very[ -]low|low|moderate|high)(?:[ -]quality evidence|[ -]certainty(?: (?:of|in) (?:the )?evidence(?: of effects)?| evidence)?|[ -]quality of evidence)\b").unwrap();
    let lead_re = regex::Regex::new(r"^(?:[•▪◦\-–]\s*)?(\([0-9]+[a-z]?\)|[0-9]+[a-z]?\.)?\s*").unwrap();
    // "(... strong|conditional|weak recommendation ...)" printed as the statement's own grade
    let grade_paren = regex::Regex::new(r"(?i)\(([^()]*\b(?:strong|conditional|weak)\s+recommendation\b[^()]*)\)").unwrap();
    let tail_ok = regex::Regex::new(r"(?i)^[\s.;,:]*(?:(?:and|or)[\s.;,:]*)?$").unwrap();
    let verb_re = regex::Regex::new(r"(?i)\b(?:we|(?:the\s+)?(?:[\w/-]+\s+){0,3}(?:panel|committee|task\s+force|working\s+group|work\s+group|guideline\s+development\s+group))\s+(?:(?:strongly|conditionally)\s+)?(?:recommends?|suggests?)\b").unwrap();
    let mut seen_paths: HashSet<Vec<String>> = HashSet::new();
    for b in blocks.iter_mut() {
        if !matches!(b.kind, BlockKind::Paragraph | BlockKind::ListItem) {
            continue;
        }
        // Tables of contents repeat headings like "Recommendation 1"; they are navigation, not statements.
        if b.heading_path.iter().any(|h| matches!(h.trim().to_lowercase().as_str(), "contents" | "table of contents")) {
            continue;
        }
        let first_in_section = seen_paths.insert(b.heading_path.clone());
        let in_recs = b.heading_path.iter().any(|h| {
            let h = h.to_lowercase();
            h.contains("recommendation") || h.contains("guidance statement")
        });
        let mut label = label_re.captures(b.text.trim_start()).map(|c| c[1].to_string());
        if label.as_ref().is_some_and(|l| b.text.trim().len() < l.len() + 15) {
            label = None; // a bare label line is a heading-like marker, not the statement
        }
        let lead = lead_re.captures(b.text.trim_start());
        let body_start = lead.as_ref().map(|c| c.get(0).unwrap().end()).unwrap_or(0);
        let starts_like_statement = {
            let l = b.text.trim_start()[body_start..].to_lowercase();
            ["we recommend", "we suggest", "the panel recommends", "the panel suggests"].iter().any(|p| l.starts_with(p))
        };
        if label.is_none() && starts_like_statement {
            label = lead.and_then(|c| c.get(1)).map(|m| m.as_str().to_string());
        }
        // The statement's own printed grade: the last "(... recommendation ...)" group, at the end of the text.
        let grade = grade_paren.captures_iter(&b.text).last().filter(|c| tail_ok.is_match(&b.text[c.get(0).unwrap().end()..]));
        let has_verb = verb_re.is_match(&b.text);
        let heading_label = b.heading_path.last().filter(|h| heading_label_re.is_match(h.trim())).map(|h| h.trim().to_string());
        let by_heading = heading_label.is_some() && first_in_section && (has_verb || grade.is_some());
        let strength_anywhere = simple_strength.find(&b.text).map(|m| m.as_str().to_string());
        let is_rec = label.as_ref().is_some_and(|l| l.to_lowercase().starts_with("recommendation"))
            || (in_recs && starts_like_statement)
            || (strength_anywhere.is_some() && starts_like_statement)
            || (grade.is_some() && (has_verb || in_recs))
            || by_heading;
        if !is_rec {
            continue;
        }
        if by_heading && label.is_none() {
            label = heading_label;
        }
        let (strength, certainty) = match &grade {
            Some(c) => split_grade(&c[1]),
            None => (strength_anywhere, cert_re.find(&b.text).map(|m| m.as_str().to_string())),
        };
        b.kind = BlockKind::Recommendation { label, strength, certainty, population: None, grading_system: None, qualifier: None };
    }
}

/// Split a printed grade group verbatim into strength and certainty: "conditional recommendation, very low
/// certainty of evidence", "strong recommendation based on low certainty in the evidence of effects ⨁⨁◯◯",
/// "Clinical consensus, Strong Recommendation" (text before a ';' that precedes the strength is not grade text,
/// e.g. "eg, current DSM; Clinical consensus, Strong Recommendation").
fn split_grade(inner: &str) -> (Option<String>, Option<String>) {
    let strength_re = regex::Regex::new(r"(?i)\b(?:strong|conditional|weak)\s+recommendation\b").unwrap();
    let segs: Vec<&str> = inner.split(';').collect();
    let at = segs.iter().position(|s| strength_re.is_match(s)).unwrap_or(0);
    let g = crate::common::collapse_ws(&segs[at..].join(";"));
    if let Some(m) = regex::Regex::new(r"(?i)\sbased on\s").unwrap().find(&g) {
        let (a, b) = (g[..m.start()].trim(), g[m.end()..].trim());
        return (Some(a.to_string()).filter(|s| !s.is_empty()), Some(b.to_string()).filter(|s| !s.is_empty()));
    }
    let parts: Vec<&str> = g.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()).collect();
    let strength = parts.iter().find(|p| strength_re.is_match(p)).map(|s| s.to_string());
    let rest: Vec<&str> = parts.iter().filter(|p| !strength_re.is_match(p)).copied().collect();
    (strength, (!rest.is_empty()).then(|| rest.join(", ")))
}

fn license_label(url: &str, text: &str) -> Option<String> {
    let u = url.to_lowercase();
    let re = regex::Regex::new(r"creativecommons\.org/(licenses|publicdomain)/([a-z-]+)/(\d(?:\.\d)?)").unwrap();
    if let Some(c) = re.captures(&u) {
        return Some(if &c[1] == "publicdomain" { format!("CC0-{}", &c[3]) } else { format!("CC-{}-{}", c[2].to_uppercase(), &c[3]) });
    }
    let t = text.to_lowercase();
    let re2 = regex::Regex::new(r"creative commons (attribution(?:[- ](?:non-?commercial|noncommercial|no ?derivatives|noderivs|share ?alike))*)(?: (\d\.\d))? licen[cs]e").unwrap();
    re2.captures(&t).map(|c| {
        let mut l = String::from("CC-BY");
        let k = c[1].replace(' ', "-");
        if k.contains("commercial") {
            l.push_str("-NC");
        }
        if k.contains("deriv") {
            l.push_str("-ND");
        }
        if k.contains("share") {
            l.push_str("-SA");
        }
        if let Some(v) = c.get(2) {
            l.push('-');
            l.push_str(v.as_str());
        }
        l
    })
}

fn jats_rights(meta: Option<Node>) -> Rights {
    let mut r = Rights { status: "unknown".into(), ..Default::default() };
    let Some(perm) = meta.and_then(|m| kid(m, "permissions")) else {
        r.note = Some("JATS has no <permissions>; licence unknown".into());
        return r;
    };
    let stmt = kid(perm, "copyright-statement").map(|x| txt(x, &[])).filter(|s| !s.is_empty());
    let holder = kid(perm, "copyright-holder").map(|x| txt(x, &[])).filter(|s| !s.is_empty());
    let lic = kid(perm, "license");
    let lic_url = lic.and_then(|l| href(l).or_else(|| kids(l).find(|c| nm(c) == "license_ref").and_then(|x| x.text()).map(|s| s.trim().to_string())));
    let lic_text = lic.map(|l| txt(l, &[])).unwrap_or_default();
    r.license = license_label(lic_url.as_deref().unwrap_or(""), &lic_text).or_else(|| lic_url.clone());
    r.terms_url = lic_url.clone();
    r.attribution = stmt.clone();
    r.rights_holders = holder.into_iter().collect();
    if !lic_text.is_empty() {
        r.note = Some(lic_text.clone());
    }
    if let Some(l) = &r.license {
        if l.starts_with("CC") {
            r.status = "open".into();
            if l.contains("-NC") {
                r.restrictions.push("non-commercial (licence)".into());
            }
            if l.contains("-ND") {
                r.restrictions.push("no derivatives (licence)".into());
            }
            if l.contains("-SA") {
                r.restrictions.push("share-alike (licence)".into());
            }
        }
    }
    if r.license.is_none() && !lic_text.is_empty() {
        // e.g. PMC author manuscripts: "This file is available for text mining. It may also be used consistent with
        // the principles of fair use under the copyright law." Kept verbatim; never upgraded to an open licence.
        let first = lic_text.split_inclusive(". ").next().unwrap_or(&lic_text).trim().to_string();
        let short = if first.chars().count() > 200 { format!("{}…", first.chars().take(200).collect::<String>()) } else { first };
        r.license = Some(lic.and_then(|l| l.attribute("license-type")).map(String::from).unwrap_or(short));
        r.status = "restricted".into();
        r.restrictions.push(lic_text.clone());
    }
    if r.license.is_none() && lic_text.is_empty() {
        r.note = Some(format!("JATS <permissions> has no licence; statement: {}", stmt.unwrap_or_default()));
    }
    r
}

fn jats_dates(meta: Option<Node>) -> Vec<DateObservation> {
    let mut out = vec![];
    let Some(m) = meta else { return out };
    for d in kids(m).filter(|c| nm(c) == "pub-date") {
        let ty = d.attribute("pub-type").or_else(|| d.attribute("date-type")).unwrap_or("pub");
        let g = |t: &str| kid(d, t).and_then(|x| x.text()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let (y, mo, da) = (g("year"), g("month"), g("day"));
        let Some(y) = y else { continue };
        let raw = [Some(y.clone()), mo.clone(), da.clone()].into_iter().flatten().collect::<Vec<_>>().join("-");
        let pm = mo.as_deref().and_then(|m| m.parse::<u8>().ok()).filter(|m| (1..=12).contains(m));
        let pd = da.as_deref().and_then(|d| d.parse::<u8>().ok()).filter(|d| (1..=31).contains(d));
        let normalized = match (pm, pd) {
            (Some(m), Some(d)) => Some(format!("{y}-{m:02}-{d:02}")),
            (Some(m), None) if da.is_none() => Some(format!("{y}-{m:02}")),
            (None, None) if mo.is_none() => Some(y.clone()),
            _ => None,
        };
        out.push(DateObservation { label: format!("pub-date ({ty})"), raw, normalized, source_url: None });
    }
    if let Some(h) = kid(m, "history") {
        for d in kids(h).filter(|c| nm(c) == "date") {
            let ty = d.attribute("date-type").unwrap_or("history");
            let raw = ["year", "month", "day"].iter().filter_map(|t| kid(d, t).and_then(|x| x.text())).map(|s| s.trim()).collect::<Vec<_>>().join("-");
            if !raw.is_empty() {
                out.push(DateObservation { label: format!("history ({ty})"), raw, normalized: None, source_url: None });
            }
        }
    }
    out
}

/// Map one JATS `<article>` to guideline blocks and metadata. Pure; no network.
pub fn jats_document(xml: &str) -> Result<JatsDoc> {
    let doc = Document::parse_with_options(xml, ParsingOptions { allow_dtd: true, nodes_limit: 50_000_000 }).context("JATS parse")?;
    let art = doc.descendants().find(|n| n.is_element() && nm(n) == "article").context("no <article>")?;
    let front = kid(art, "front");
    let meta = front.and_then(|f| kid(f, "article-meta"));
    let jmeta = front.and_then(|f| kid(f, "journal-meta"));
    let id_of = |t: &str| meta.and_then(|m| kids(m).find(|c| nm(c) == "article-id" && c.attribute("pub-id-type") == Some(t))).and_then(|c| c.text()).map(|s| s.trim().to_string());
    let title = meta.and_then(|m| kid(m, "title-group")).and_then(|g| kid(g, "article-title")).map(|t| txt(t, &[])).unwrap_or_default();
    if title.is_empty() {
        bail!("JATS article-title missing");
    }
    let mut w = Walker { out: vec![], path: vec![], n: 0, ids: HashSet::new(), warnings: vec![], supplements: vec![], unmapped: HashMap::new() };
    if let Some(m) = meta {
        for a in kids(m).filter(|c| nm(c) == "abstract" || nm(c) == "trans-abstract") {
            let ty = a.attribute("abstract-type");
            let head = match (nm(&a), ty) {
                ("abstract", None) => "Abstract".to_string(),
                ("abstract", Some(t)) => format!("Abstract ({t})"),
                (_, _) => "Translated abstract".to_string(),
            };
            let t = label_title(a);
            w.section(a, if t.is_empty() { &head } else { "" });
        }
    }
    let body = kid(art, "body");
    match body {
        Some(b) => w.container(b),
        None => w.warnings.push("JATS has no <body>: abstract-level record only".into()),
    }
    if let Some(b) = kid(art, "back") {
        w.container(b);
    }
    if let Some(fl) = kid(art, "floats-group") {
        w.container(fl);
    }
    for (k, v) in &w.unmapped {
        w.warnings.push(format!("unmapped JATS element <{k}> x{v} kept as paragraph text"));
    }
    if desc(art, "sub-article").next().is_some() {
        w.warnings.push("JATS <sub-article> present (e.g. translation/commentary) and not emitted".into());
    }
    let mut blocks = w.out;
    mark_inline_recommendations(&mut blocks);
    let subjects = meta
        .and_then(|m| kid(m, "article-categories"))
        .map(|c| desc(c, "subject").map(|s| txt(s, &[])).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    Ok(JatsDoc {
        title,
        doi: id_of("doi").map(|d| d.to_ascii_lowercase()),
        pmid: id_of("pmid"),
        pmcid: id_of("pmcid").or_else(|| id_of("pmc").map(|p| if p.starts_with("PMC") { p } else { format!("PMC{p}") })),
        article_type: art.attribute("article-type").map(String::from),
        journal: jmeta.and_then(|j| desc(j, "journal-title").next()).map(|t| txt(t, &[])),
        publisher: jmeta.and_then(|j| desc(j, "publisher-name").next()).map(|t| txt(t, &[])),
        language: art.attributes().find(|a| a.name() == "lang").map(|a| a.value().to_string()),
        subjects,
        dates: jats_dates(meta),
        rights: jats_rights(meta),
        blocks,
        supplements: w.supplements,
        warnings: w.warnings,
    })
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const OAI_ESO: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/pmc_crosswalk/oai-PMC9134774.xml"));
    const IDCONV_ESO: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/pmc_crosswalk/idconv-eso.json"));
    const OAI_CLOSED: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/pmc_crosswalk/oai-not-oa.xml"));

    #[test]
    fn doi_from_source_links() {
        assert_eq!(doi_from_url("https://doi.org/10.1177/23969873221078696").as_deref(), Some("10.1177/23969873221078696"));
        assert_eq!(doi_from_url("http://doi.org/10.1093/esj/aakag043").as_deref(), Some("10.1093/esj/aakag043"));
        assert_eq!(doi_from_url("https://journals.sagepub.com/doi/full/10.1177/10507256261445624").as_deref(), Some("10.1177/10507256261445624"));
        assert_eq!(doi_from_url("https://journals.sagepub.com/doi/10.1089/thy.2020.0944").as_deref(), Some("10.1089/thy.2020.0944"));
        assert_eq!(doi_from_url("https://academic.oup.com/eurheartj/article-lookup/doi/10.1093/eurheartj/ehae176").as_deref(), Some("10.1093/eurheartj/ehae176"));
        assert_eq!(doi_from_url("https://www.atsjournals.org/doi/abs/10.1164/rccm.202306-1066ST?role=tab").as_deref(), Some("10.1164/rccm.202306-1066st"));
        assert_eq!(doi_from_url("https://www.atsjournals.org/doi/suppl/10.1164/rccm.202009-3608ST/suppl_file/x.pdf").as_deref(), Some("10.1164/rccm.202009-3608st"));
        assert_eq!(doi_from_url("https://journals.lww.com/doi.org/10.1097/AOG.0000000000006379").as_deref(), Some("10.1097/aog.0000000000006379"));
        assert_eq!(doi_from_url("https://academic.oup.com/esj/article/11/5/aakag043/8671382?preview=true"), None);
        assert_eq!(doi_from_url("https://journal.chestnet.org/article/S0012-3692(26)06538-4/fulltext"), None);
    }

    #[test]
    fn pii_links_resolve_only_through_a_unique_crossref_match() {
        assert_eq!(pii_from_url("https://journal.chestnet.org/article/S0012-3692(26)06538-4/fulltext").as_deref(), Some("S0012369226065384"));
        assert_eq!(pii_from_url("http://journal.chestnet.org/article/S0012-3692(15)38941-8/abstract?_ga=1").as_deref(), Some("S0012369215389418"));
        assert_eq!(pii_from_url("https://www.chestnet.org/guidelines/x"), None);
        let v = json!({"status":"ok","message":{"items":[{"DOI":"10.1016/j.chest.2026.08.038","alternative-id":["S0012369226065384"]}]}});
        assert_eq!(parse_crossref_pii(&v, "S0012369226065384").as_deref(), Some("10.1016/j.chest.2026.08.038"));
        assert_eq!(parse_crossref_pii(&v, "S0012369215389418"), None);
    }

    #[test]
    fn ids_from_existing_adapter_extras() {
        assert_eq!(ids_from_extra(&json!({"doi":"10.1016/J.ANNONC.2024.1"})), (Some("10.1016/j.annonc.2024.1".into()), None));
        assert_eq!(ids_from_extra(&json!({"doi":null,"oa_crosswalk":{"doi":"10.1000/x","pmcid":"PMC1"}})), (Some("10.1000/x".into()), Some("PMC1".into())));
        assert_eq!(ids_from_extra(&json!({"doi":"not a doi"})), (None, None));
    }

    #[test]
    fn lookups_from_all_adopter_keys() {
        let l = lookups_from_extra(&json!({"stated_pmcids":["PMC7034349","PMC7034349","BMJ123"],"stated_dois":["10.1136/gutjnl-2019-1"],"publisher_dois":["10.1111/1471-0528.1","10.1136/GUTJNL-2019-1"],"bmj_article_ids":["gutjnl-2019-1"],"pmid":"123"}));
        let k: Vec<String> = l.iter().map(|x| format!("{x:?}")).collect();
        assert_eq!(k, vec!["Pmcid(\"PMC7034349\")", "Doi(\"10.1136/gutjnl-2019-1\")", "Doi(\"10.1111/1471-0528.1\")", "Pmid(\"123\")"]);
    }

    #[test]
    fn idconv_parses_api_ids_only() {
        let v: Value = serde_json::from_str(IDCONV_ESO).unwrap();
        let ids = parse_idconv(&v, "10.1177/23969873221078696", "u").unwrap();
        assert_eq!(ids.pmcid.as_deref(), Some("PMC9134774"));
        assert_eq!(ids.pmid.as_deref(), Some("35647308"));
        let none = parse_idconv(&v, "10.1177/10507256261445624", "u").unwrap();
        assert_eq!(none.pmcid, None);
        assert_eq!(none.note.as_deref(), Some("Identifier not found in PMC"));
        assert!(parse_idconv(&v, "10.1/absent", "u").is_err());
    }

    #[test]
    fn oai_open_and_closed() {
        let (xml, sets) = split_oai(OAI_ESO).unwrap().unwrap();
        assert!(xml.starts_with("<article"));
        assert!(sets.contains(&"pmc-open".to_string()));
        assert!(split_oai(OAI_CLOSED).unwrap().is_none());
    }

    #[test]
    fn eso_jats_maps_headings_boxes_tables_figures_rights() {
        let (xml, _) = split_oai(OAI_ESO).unwrap().unwrap();
        let d = jats_document(&xml).unwrap();
        assert!(d.title.starts_with("European Stroke Organisation guidelines on stroke in women"));
        assert_eq!(d.doi.as_deref(), Some("10.1177/23969873221078696"));
        assert_eq!(d.pmid.as_deref(), Some("35647308"));
        assert_eq!(d.pmcid.as_deref(), Some("PMC9134774"));
        assert_eq!(d.rights.license.as_deref(), Some("CC-BY-4.0"));
        assert_eq!(d.rights.status, "open");
        assert!(d.rights.attribution.as_deref().unwrap().contains("European Stroke Organisation 2022"));
        assert!(d.dates.iter().any(|x| x.label == "pub-date (epub)" && x.normalized.as_deref() == Some("2022-03-29")));
        let heads: Vec<&Block> = d.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Heading { .. })).collect();
        assert_eq!(heads[0].text, "Abstract");
        assert!(heads.iter().any(|h| h.text == "Introduction"));
        let recs: Vec<&Block> = d.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Recommendation { .. })).collect();
        assert_eq!(recs.len(), 13, "one statement per ESO box");
        match &recs[0].kind {
            BlockKind::Recommendation { strength, certainty, qualifier, label, .. } => {
                assert_eq!(strength.as_deref(), Some("Weak against intervention ↓"));
                assert_eq!(certainty.as_deref(), Some("Very low ⊕"));
                assert_eq!(qualifier.as_deref(), Some("Evidence-based Recommendation"));
                assert_eq!(label, &None);
            }
            _ => unreachable!(),
        }
        assert!(recs[0].text.starts_with("In postmenopausal women, we suggest against the use of HRT"));
        assert!(recs[0].heading_path.iter().any(|h| h.starts_with("PICO 1.1")));
        // consensus statements have no grade and inherit none
        let cons = recs.iter().find(|r| r.text.starts_with("A majority of members suggests")).unwrap();
        assert!(matches!(&cons.kind, BlockKind::Recommendation { strength: None, certainty: None, qualifier: Some(q), .. } if q == "Expert consensus statement"));
        let tables: Vec<&Block> = d.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Table { .. })).collect();
        assert_eq!(tables.len(), 5);
        assert!(tables.iter().any(|t| matches!(&t.kind, BlockKind::Table { caption: Some(c) } if c.contains("Synoptic table")) && t.text.contains("| Topic/PICO question | Recommendation | Expert consensus statement |")));
        let figs: Vec<&Block> = d.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Figure { .. })).collect();
        assert!(figs.iter().any(|f| matches!(&f.kind, BlockKind::Figure { src_url: Some(u), caption: Some(c) } if u == "10.1177_23969873221078696-fig1.jpg" && c.starts_with("Figure 1."))));
        assert!(d.warnings.iter().any(|w| w.starts_with("reference list")));
        let mut ids = HashSet::new();
        assert!(d.blocks.iter().all(|b| ids.insert(b.block_id.clone())));
    }

    #[test]
    fn text_mining_only_licence_is_kept_verbatim_as_restricted() {
        let xml = r#"<article xmlns:xlink="http://www.w3.org/1999/xlink"><front><article-meta><title-group><article-title>Author manuscript</article-title></title-group>
          <permissions><license><license-p>This file is available for text mining. It may also be used consistent with the principles of fair use under the copyright law.</license-p></license></permissions></article-meta></front><body><p>x</p></body></article>"#;
        let d = jats_document(xml).unwrap();
        assert_eq!(d.rights.status, "restricted");
        assert!(d.rights.license.as_deref().unwrap().starts_with("This file is available for text mining"));
        assert!(d.rights.restrictions[0].contains("fair use"));
        let ids = Ids { pmcid: Some("PMC12690183".into()), evidence_url: "u".into(), ..Default::default() };
        let r = pmc_relation(&ids, "no XML").unwrap();
        assert_eq!(r.target, "pmc:PMC12690183");
        assert!(pmc_relation(&Ids::default(), "x").is_none());
    }

    #[test]
    fn inline_graded_paragraphs_and_labels() {
        let xml = r#"<article article-type="practice-guideline"><front><article-meta><title-group><article-title>T</article-title></title-group></article-meta></front><body>
          <sec><title>Recommendations</title><p>We suggest using X in adults (conditional recommendation, very low certainty of evidence).</p><p>Background without grade.</p>
          <list><list-item><p>We recommend Y (strong recommendation, moderate-quality evidence).</p></list-item></list></sec>
          <sec><title>Discussion</title><p>We recommend nothing here in prose outside recommendations.</p><p>Recommendation 2.1 Use Z for adults with Y.</p></sec>
          <sec><title>Contents</title><list><list-item><p>Recommendation 3</p></list-item></list></sec>
          <sec><title>Summary of Recommendations</title><list><list-item><p>(3a) We suggest chest CT screening in adults.</p></list-item></list>
          <table-wrap id="T9"><caption><p>Synoptic table</p></caption><table><tbody><tr><td><inline-graphic xlink:href="fx26-1.jpg" xmlns:xlink="http://www.w3.org/1999/xlink"/></td></tr></tbody></table></table-wrap></sec></body></article>"#;
        let d = jats_document(xml).unwrap();
        let recs: Vec<_> = d.blocks.iter().filter_map(|b| match &b.kind { BlockKind::Recommendation { label, strength, certainty, .. } => Some((label.clone(), strength.clone(), certainty.clone())), _ => None }).collect();
        assert_eq!(recs, vec![
            (None, Some("conditional recommendation".into()), Some("very low certainty of evidence".into())),
            (None, Some("strong recommendation".into()), Some("moderate-quality evidence".into())),
            (Some("Recommendation 2.1".into()), None, None),
            (Some("(3a)".into()), None, None),
        ]);
        assert!(d.blocks.iter().any(|b| matches!(&b.kind, BlockKind::Figure { src_url: Some(u), .. } if u == "fx26-1.jpg")));
        assert!(d.warnings.iter().any(|w| w.contains("only as 1 image")));
        assert_eq!(d.rights.status, "unknown");
        assert!(d.warnings.iter().all(|w| !w.contains("unmapped")));
    }

    fn rec_rows(d: &JatsDoc) -> Vec<(Option<String>, Option<String>, Option<String>, Option<String>, String)> {
        d.blocks.iter().filter_map(|b| match &b.kind {
            BlockKind::Recommendation { label, strength, certainty, qualifier, .. } => Some((label.clone(), strength.clone(), certainty.clone(), qualifier.clone(), b.text.clone())),
            _ => None,
        }).collect()
    }
    fn r2b_fixture(name: &str) -> JatsDoc {
        let raw = std::fs::read_to_string(format!("{}/tests/fixtures/guidelines/r2b_crosswalk/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let (xml, _) = split_oai(&raw).unwrap().unwrap();
        jats_document(&xml).unwrap()
    }

    #[test]
    fn grade_groups_split_verbatim() {
        assert_eq!(split_grade("conditional recommendation, very low certainty of evidence"), (Some("conditional recommendation".into()), Some("very low certainty of evidence".into())));
        assert_eq!(split_grade("strong recommendation based on low certainty in the evidence of effects ⨁⨁◯◯"), (Some("strong recommendation".into()), Some("low certainty in the evidence of effects ⨁⨁◯◯".into())));
        assert_eq!(split_grade("Clinical consensus, Strong Recommendation"), (Some("Strong Recommendation".into()), Some("Clinical consensus".into())));
        assert_eq!(split_grade("eg, current DSM; Clinical consensus, Strong Recommendation"), (Some("Strong Recommendation".into()), Some("Clinical consensus".into())));
        assert_eq!(split_grade("conditional recommendation; low-certainty evidence"), (Some("conditional recommendation".into()), Some("low-certainty evidence".into())));
        assert_eq!(split_grade("strong recommendation"), (Some("strong recommendation".into()), None));
    }

    #[test]
    fn panel_statements_graded_items_and_labelled_sections() {
        let xml = r#"<article><front><article-meta><title-group><article-title>T</article-title></title-group></article-meta></front><body>
          <sec><title>Recommendations</title>
            <sec><title>Recommendation 1</title><p>For older adults with AML who are candidates for ALT, the ASH guideline panel <italic>recommends</italic> offering ALT over best supportive care (strong recommendation based on low certainty in the evidence of effects &#10753;&#10753;&#9711;&#9711;).</p>
              <p>Remarks: <list><list-item><p>This recommendation is from the 2020 guideline.</p></list-item></list></p></sec>
            <sec><title>Recommendation 2a</title><p>The panel suggests X over Y (conditional recommendation based on moderate certainty in the evidence of effects).</p></sec></sec>
          <sec><title>Initial Assessment Recommendations</title><list list-type="order"><list-item><p>When assessing patients for StUD, clinicians should identify urgent needs (<italic>Clinical consensus, Strong Recommendation</italic>).</p></list-item>
            <list-item><p>Clinicians should conduct: <list><list-item><p>A full biopsychosocial assessment (<italic>Clinical consensus, Strong Recommendation</italic>); and</p></list-item></list></p></list-item></list></sec>
          <sec><title>Rating the Strength of Recommendations</title><p>Each statement was rated (eg, strong recommendation) by vote, see Table 2.</p></sec>
          <sec><title>Discussion</title><p>In one trial (strong recommendation, see above) outcomes improved.</p></sec>
          <sec><title>Hepatotoxicity</title><p>Text.</p><boxed-text><caption><title>Guidance statements</title></caption><p><list list-type="simple"><list-item><label>6</label><p>The estimated annual incidence of DILI is 14 to 19 per 100,000.</p></list-item><list-item><label>7</label><p>We recommend accessing LiverTox.</p></list-item></list></p></boxed-text></sec>
        </body></article>"#;
        let d = jats_document(xml).unwrap();
        let r = rec_rows(&d);
        let labels: Vec<Option<&str>> = r.iter().map(|x| x.0.as_deref()).collect();
        assert_eq!(labels, vec![Some("Recommendation 1"), Some("Recommendation 2a"), None, None, Some("6"), Some("7")], "{r:#?}");
        assert_eq!((r[0].1.as_deref(), r[0].2.as_deref()), (Some("strong recommendation"), Some("low certainty in the evidence of effects ⨁⨁◯◯")));
        assert_eq!((r[1].1.as_deref(), r[1].2.as_deref()), (Some("conditional recommendation"), Some("moderate certainty in the evidence of effects")));
        assert_eq!((r[2].1.as_deref(), r[2].2.as_deref()), (Some("Strong Recommendation"), Some("Clinical consensus")));
        assert!(r[3].4.starts_with("A full biopsychosocial assessment"));
        // box items: one statement each, the item's own label, the box title as qualifier, no grade invented
        assert_eq!((r[4].3.as_deref(), r[4].1.as_deref(), r[4].4.as_str()), (Some("Guidance statements"), None, "The estimated annual incidence of DILI is 14 to 19 per 100,000."));
        assert_eq!(r[5].4, "We recommend accessing LiverTox.");
        // remarks, methods text and mid-sentence parentheticals stay prose
        assert!(!r.iter().any(|x| x.4.contains("2020 guideline") || x.4.contains("by vote") || x.4.contains("one trial") || x.4.starts_with("Clinicians should conduct")));
    }

    /// Round-2 crosswalk fixtures (R2b): ASH AML 2026 (PMC12993901), ASAM/AAAP stimulant (PMC11105801),
    /// AASLD DILI guidance (PMC9936988). Before round 3: 2, 0 and 0 recommendations.
    #[test]
    fn r2b_guideline_statements_are_structured() {
        let ash = r2b_fixture("oai-PMC12993901-trimmed.xml");
        let r = rec_rows(&ash);
        let labelled: Vec<_> = r.iter().filter(|x| x.0.as_deref().is_some_and(|l| l.starts_with("Recommendation "))).collect();
        assert!(labelled.len() >= 10, "ASH: {} labelled of {}\n{r:#?}", labelled.len(), r.len());
        let one = labelled.iter().find(|x| x.0.as_deref() == Some("Recommendation 1")).expect("Recommendation 1");
        assert_eq!(one.1.as_deref(), Some("strong recommendation"));
        assert_eq!(one.2.as_deref(), Some("moderate certainty in the evidence of effects ⨁⨁⨁◯"), "{one:?}");
        assert!(r.iter().all(|x| !x.4.starts_with("This recommendation is from")), "remarks stay prose");

        let asam = r2b_fixture("oai-PMC11105801-trimmed.xml");
        let r = rec_rows(&asam);
        let graded = r.iter().filter(|x| x.1.as_deref().is_some_and(|s| s.to_lowercase().contains("recommendation"))).count();
        assert!(graded >= 20, "ASAM: {graded} graded of {}\n{r:#?}", r.len());
        assert!(r.iter().any(|x| x.1.as_deref() == Some("Strong Recommendation") && x.2.as_deref() == Some("Clinical consensus")), "{r:#?}");

        let aasld = r2b_fixture("oai-PMC9936988-trimmed.xml");
        let r = rec_rows(&aasld);
        let gs: Vec<_> = r.iter().filter(|x| x.3.as_deref() == Some("Guidance statements")).collect();
        assert!(gs.len() >= 15, "AASLD: {} guidance statements of {}\n{r:#?}", gs.len(), r.len());
        assert!(gs.iter().any(|x| x.0.as_deref() == Some("6") && x.4.starts_with("The estimated annual incidence of idiosyncratic DILI")), "{gs:#?}");
        assert!(gs.iter().all(|x| x.1.is_none()), "no grade invented");
        assert!(gs.iter().all(|x| x.4.len() < 1500), "one item per statement, not a whole list");
    }

    #[tokio::test]
    async fn offline_lookups_do_not_wait_on_the_api_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 20, "").unwrap();
        let t = Instant::now();
        for q in ["a", "b", "c"] {
            assert!(epmc_search(&f, &format!("TITLE:\"{q} uncached\""), 5).await.is_err());
            assert!(resolve(&f, &Lookup::Doi(format!("10.1/{q}"))).await.is_err());
        }
        assert!(t.elapsed() < Duration::from_secs(2), "offline lookups slept on the 10 s / 1 s gate: {:?}", t.elapsed());
    }

    #[tokio::test]
    async fn licence_less_pmc_copy_is_restricted_inside_extract() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
        let no_licence = regex::Regex::new(r"(?s)<permissions>.*?</permissions>").unwrap().replace_all(OAI_ESO, "").to_string();
        assert!(!no_licence.contains("<permissions>"));
        f.seed_cache_for_tests(&oai_url("PMC9134774").unwrap(), no_licence.as_bytes(), "application/xml");
        let src: SourceEntry = serde_json::from_value(json!({"id":"eso","enabled":true,"access":"public"})).unwrap();
        let sref = SourceRef { source_url: "https://doi.org/10.1177/23969873221078696".into(), key_prefix: "eso".into(), id_route: "source_doi".into(), ..Default::default() };
        let Planned::Open(row) = plan_pmcid(&src, &f, "PMC9134774", None, sref, json!({})).await.unwrap() else { panic!("expected a planned row") };
        let d = extract(&src, &f, &row).await.unwrap();
        assert_eq!(d.rights.status, "restricted");
        assert_eq!(d.rights.restrictions, vec![LICENCE_LESS.to_string()]);
        // the round-2 wrapper keeps working and does not add the restriction twice
        let d2 = super::super::r2b_crosswalk::extract(&src, &f, &row).await.unwrap();
        assert_eq!(d2.rights.status, "restricted");
        assert_eq!(d2.rights.restrictions.iter().filter(|r| r.contains("No licence in the PMC JATS")).count(), 1);
        // an open licence is untouched
        f.seed_cache_for_tests(&oai_url("PMC9134774").unwrap(), OAI_ESO.as_bytes(), "application/xml");
        let sref = SourceRef { source_url: "x".into(), key_prefix: "eso".into(), id_route: "source_doi".into(), ..Default::default() };
        let Planned::Open(row) = plan_pmcid(&src, &f, "PMC9134774", None, sref, json!({})).await.unwrap() else { panic!() };
        let d = extract(&src, &f, &row).await.unwrap();
        assert_eq!((d.rights.status.as_str(), d.rights.license.as_deref()), ("open", Some("CC-BY-4.0")));
        assert!(d.rights.restrictions.is_empty());
    }

    #[test]
    fn exact_title_requires_unique_equal_title_and_issn() {
        let mk = |t: &str, issn: &str| EpmcHit { title: t.into(), journal_issn: vec![issn.into()], ..Default::default() };
        let hits = vec![mk("Tobacco Treatment in the Inpatient Setting: CHEST Guideline.", "0012-3692"), mk("Tobacco treatment in the inpatient setting", "0012-3692"), mk("Tobacco Treatment in the Inpatient Setting", "1234-5678")];
        assert!(exact_title_hit(&hits, "Tobacco Treatment in the Inpatient Setting", &["0012-3692"]).is_some());
        assert!(exact_title_hit(&hits, "Tobacco Treatment in the Inpatient Setting", &[]).is_none(), "ambiguous without ISSN");
        assert!(exact_title_hit(&hits, "Tobacco Treatment", &["0012-3692"]).is_none());
    }

    #[test]
    fn epmc_hits_keep_api_fields() {
        let v = json!({"hitCount": 2, "resultList": {"result": [
            {"id":"1","source":"MED","pmid":"1","pmcid":"PMC9","doi":"10.1/A","title":"A","isOpenAccess":"Y","inPMC":"Y","license":"cc by","journalInfo":{"journal":{"title":"J","issn":"0000-0001","essn":"0000-0002"}},"pubTypeList":{"pubType":["Practice Guideline"]}},
            {"id":"2","source":"MED","title":"B","isOpenAccess":"N"}]}});
        let h = epmc_hits(&v);
        assert_eq!(h[0].doi.as_deref(), Some("10.1/a"));
        assert_eq!((h[1].volume.as_deref(), h[1].first_page()), (None, None));
        assert_eq!(h[0].journal_issn, vec!["0000-0001", "0000-0002"]);
        assert!(h[0].is_open_access && !h[1].is_open_access && h[1].pmcid.is_none());
    }

    #[test]
    fn epmc_hits_carry_volume_issue_pages_and_year() {
        let v = json!({"resultList": {"result": [
            {"id":"1","source":"MED","title":"A","pageInfo":"3898-3911","pubYear":"2019","journalInfo":{"volume":"3","issue":"23","yearOfPublication":2019,"journal":{"issn":"2473-9529"}}},
            {"id":"2","source":"MED","title":"B","pageInfo":"e123","journalInfo":{"volume":40,"yearOfPublication":2021}}]}});
        let h = epmc_hits(&v);
        assert_eq!((h[0].volume.as_deref(), h[0].issue.as_deref(), h[0].page_info.as_deref(), h[0].pub_year.as_deref()), (Some("3"), Some("23"), Some("3898-3911"), Some("2019")));
        assert_eq!(h[0].first_page(), Some("3898"));
        assert_eq!((h[1].volume.as_deref(), h[1].pub_year.as_deref(), h[1].first_page()), (Some("40"), Some("2021"), Some("e123")));
        // same fields as the round-2 reader (r2b_crosswalk::epmc_records) on a real Europe PMC answer
        let raw = std::fs::read_to_string(format!("{}/tests/fixtures/guidelines/r2b_crosswalk/epmc-bloodadv-3-23.json", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        let ours = epmc_hits(&v);
        let theirs = super::super::r2b_crosswalk::epmc_records(&v);
        assert!(!ours.is_empty() && ours.len() == theirs.len());
        for (a, b) in ours.iter().zip(&theirs) {
            assert_eq!((&a.volume, &a.issue, &a.page_info, &a.pub_year), (&b.volume, &b.issue, &b.page_info, &b.pub_year));
        }
        // rows planned before round 3 (no such fields) still deserialize
        let old: EpmcHit = serde_json::from_value(json!({"id":"1","source":"MED","pmid":null,"pmcid":null,"doi":null,"title":"t","journal_issn":[],"journal":null,
            "first_publication_date":null,"pub_types":[],"is_open_access":false,"in_pmc":false,"license":null})).unwrap();
        assert!(old.volume.is_none() && old.page_info.is_none());
    }

    #[tokio::test]
    async fn enumeration_plans_oa_hits_and_reports_unresolvable_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
        let q = "ISSN:\"2396-9873\" AND OPEN_ACCESS:y";
        let answer = json!({"hitCount": 2, "nextCursorMark": "*", "resultList": {"result": [
            {"id":"35647308","source":"MED","pmid":"35647308","pmcid":"PMC9134774","doi":"10.1177/23969873221078696","title":"European Stroke Organisation guidelines on stroke in women","isOpenAccess":"Y","inPMC":"Y","firstPublicationDate":"2022-03-29"},
            {"id":"1","source":"MED","title":"OA elsewhere","isOpenAccess":"Y","firstPublicationDate":"2023-01-01"}]}});
        f.seed_cache_for_tests(&epmc_search_url(q, 10, "*"), answer.to_string().as_bytes(), "application/json");
        f.seed_cache_for_tests(&oai_url("PMC9134774").unwrap(), OAI_ESO.as_bytes(), "application/xml");
        let src: SourceEntry = serde_json::from_value(json!({"id":"eso","enabled":true,"access":"public","config":{"oa_enumeration":{"query":q,"max":10,"item_type":"guideline","scope":"test"}}})).unwrap();
        let rows = enumerate(&src, &f, "eso", None, &HashSet::new()).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].extra["discovery_error"].as_str().unwrap().contains("no PMCID"), "newest first");
        assert_eq!(rows[1].extra["oa_enumeration"]["hit_count"], 2);
        let d = extract(&src, &f, &rows[1]).await.unwrap();
        assert_eq!(d.item_type.as_deref(), Some("guideline"));
        assert!(d.search_metadata["pmc_crosswalk"]["id_route"] == "epmc_enumeration");
        let skip: HashSet<String> = ["PMC9134774".to_string()].into();
        assert_eq!(enumerate(&src, &f, "eso", None, &skip).await.unwrap().len(), 1);
        let none: SourceEntry = serde_json::from_value(json!({"id":"x","enabled":true,"access":"public"})).unwrap();
        assert!(enumerate(&none, &f, "x", None, &HashSet::new()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn offline_plan_and_extract_from_seeded_api_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 1 << 24, "").unwrap();
        let doi = "10.1177/23969873221078696";
        f.seed_cache_for_tests(&idconv_url(&Lookup::Doi(doi.into())), IDCONV_ESO.as_bytes(), "application/json");
        f.seed_cache_for_tests(&oai_url("PMC9134774").unwrap(), OAI_ESO.as_bytes(), "application/xml");
        let src: SourceEntry = serde_json::from_value(json!({"id":"eso","enabled":true,"access":"public"})).unwrap();
        let sref = SourceRef { source_url: format!("https://doi.org/{doi}"), evidence_url: Some("https://eso-stroke.org/guidelines/eso-guideline-directory/".into()), source_title: Some("European Stroke Organisation (ESO) Guidelines on Stroke in Women: Management of menopause, pregnancy and postpartum".into()), item_type: Some("guideline".into()), key_prefix: "eso".into(), id_route: "source_doi".into(), notes: vec![] };
        let Planned::Open(row) = plan(&src, &f, Lookup::Doi(doi.into()), sref, json!({})).await.unwrap() else { panic!("expected OA plan") };
        assert!(row.doc_key.starts_with("eso-pmc9134774-"));
        assert!(is_crosswalk_row(&row));
        let d = extract(&src, &f, &row).await.unwrap();
        assert_eq!(d.url, "https://pmc.ncbi.nlm.nih.gov/articles/PMC9134774/");
        assert_eq!(d.doi.as_deref(), Some(doi));
        assert_eq!(d.published.as_deref(), Some("2022-03-29"));
        assert!(d.relations.iter().any(|r| r.rel == "rendition_of" && r.target == "pmc:PMC9134774"));
        assert!(d.relations.iter().any(|r| r.rel == "related" && r.target == format!("https://doi.org/{doi}")));
        assert!(d.blocks.iter().any(|b| matches!(&b.kind, BlockKind::Figure { src_url: Some(u), .. } if u == "https://pmc.ncbi.nlm.nih.gov/articles/instance/9134774/bin/10.1177_23969873221078696-fig1.jpg")));
        assert_eq!(d.rights.license.as_deref(), Some("CC-BY-4.0"));
        assert!(d.rights.evidence_urls.iter().any(|u| u.contains("/api/oai/v1/mh/")));
        assert!(d.fetches.len() >= 2);
        // not-in-PMC id stays closed with the converter's own message
        let closed = "10.1177/10507256261445624";
        f.seed_cache_for_tests(&idconv_url(&Lookup::Doi(closed.into())), IDCONV_ESO.as_bytes(), "application/json");
        let sref = SourceRef { source_url: "x".into(), key_prefix: "ata".into(), id_route: "source_doi".into(), ..Default::default() };
        match plan(&src, &f, Lookup::Doi(closed.into()), sref, json!({})).await.unwrap() {
            Planned::Closed { reason, ids } => {
                assert!(reason.contains("Identifier not found in PMC"));
                assert!(ids.unwrap().pmcid.is_none());
            }
            Planned::Open(_) => panic!("closed id planned as open"),
        }
    }
}
