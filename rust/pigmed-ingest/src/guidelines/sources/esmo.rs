//! ESMO independently evidenced Nuxt guideline record; source HTML describes, not duplicates, the article.
use super::{common, Adapter, PlanRow};
use super::{pmc_crosswalk as cw, r2a_crosswalk as r2a};
use crate::guidelines::doc::{Block, BlockKind, GuidelineDoc, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use url::Url;
pub struct Esmo;
const TOPIC: &str =
    "https://www.esmo.org/guidelines/esmo-clinical-practice-guideline-early-breast-cancer";
const CATEGORY: &str =
    "https://www.esmo.org/guidelines/esmo-clinical-practice-guidelines-breast-cancer";
const ROUTES:&str="https://kontent.cdn.aws.esmo.org/rest/routes?codenames=esmo_orgbreast_cancer_a84d54e,esmo_orggastrointestinal_cancers_274bf32";
const DATAVIEW:&str="https://kontent.cdn.aws.esmo.org/rest/items?includeTotalCount&system.workflow_step%5Bnin%5D=archived&limit=2000&depth=4&elements.tumour_sites%5Bany%5D=breast_cancer&elements.guideline_product%5Bany%5D=esmo_clinical_practice_guideline,consensus_recommendations&order=elements.teaser_title";
const SOURCE_QUERY:&str="elements.tumour_sites[any]=breast_cancer&elements.guideline_product[any]=esmo_clinical_practice_guideline,consensus_recommendations&order=elements.teaser_title";
const API_BASE: &str = "https://kontent.cdn.aws.esmo.org/rest/";
const MAX_CATEGORIES: usize = 100;
const MAX_PAGES: usize = 100;
fn category_route(raw: &str) -> Result<String> {
    let v: Value = serde_json::from_str(raw)?;
    let r = &v["esmo_orgbreast_cancer_a84d54e"];
    if r["name"] != "breast_cancer_a84d54e" {
        bail!("ESMO source route codename mismatch")
    }
    let path = r["path"]
        .as_str()
        .context("ESMO source category route missing")?;
    let url = Url::parse("https://www.esmo.org/")?.join(path)?;
    if url.origin() != Url::parse("https://www.esmo.org/")?.origin()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().starts_with("/guidelines/")
    {
        bail!("ESMO source category route invalid")
    }
    Ok(url.to_string())
}
fn other_category_route(raw: &str) -> Result<String> {
    let v: Value = serde_json::from_str(raw)?;
    let r = &v["esmo_orggastrointestinal_cancers_274bf32"];
    if r["name"] != "gastrointestinal_cancers_274bf32" {
        bail!("ESMO second category route codename mismatch")
    }
    let path = r["path"]
        .as_str()
        .context("ESMO second category route missing")?;
    let u = Url::parse("https://www.esmo.org/")?.join(path)?;
    if u.origin() != Url::parse("https://www.esmo.org/")?.origin()
        || u.query().is_some()
        || u.fragment().is_some()
        || !u.path().starts_with("/guidelines/")
    {
        bail!("ESMO second category route invalid")
    }
    Ok(u.to_string())
}
fn data_page(raw: &str) -> Result<(Vec<Value>, usize, usize, Option<String>)> {
    let v: Value = serde_json::from_str(raw)?;
    let entries = v["items"]
        .as_array()
        .context("ESMO dataview items missing")?;
    let p = &v["pagination"];
    let skip = p["skip"].as_u64().context("ESMO dataview skip missing")? as usize;
    let count = p["count"].as_u64().context("ESMO dataview count missing")? as usize;
    let total = p["total_count"]
        .as_u64()
        .context("ESMO dataview total missing")? as usize;
    let next = p["next_page"]
        .as_str()
        .context("ESMO dataview next page missing")?;
    if count != entries.len()
        || skip.checked_add(count).map_or(true, |x| x > total)
        || count == 0 && skip < total
    {
        bail!("ESMO dataview range or count inconsistent")
    }
    if (skip + count < total) != (!next.is_empty()) {
        bail!("ESMO dataview next-page state inconsistent")
    }
    let mut seen = HashSet::new();
    for e in entries {
        let id = e["system"]["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("ESMO source item ID missing")?;
        if !seen.insert(id)
            || e["system"]["type"] != "esmo_guideline"
            || e["elements"]["title"]["value"]
                .as_str()
                .filter(|s| !s.is_empty())
                .is_none()
        {
            bail!("ESMO dataview duplicate or unsupported source item")
        }
    }
    Ok((
        entries.clone(),
        skip,
        total,
        (!next.is_empty()).then(|| next.to_string()),
    ))
}
fn dataview(raw: &str) -> Result<Vec<Value>> {
    let (items, _, _, _) = data_page(raw)?;
    Ok(items)
}
fn dataview_item(
    item: &Value,
    base: &Url,
) -> Result<(String, String, Vec<Block>, Vec<Value>, Value)> {
    let e = &item["elements"];
    let title = e["title"]["value"]
        .as_str()
        .context("ESMO item title missing")?
        .to_string();
    let html = e["content"]["value"]
        .as_str()
        .context("ESMO item source description missing")?;
    let citation = e["citation"]["value"].as_str().unwrap_or("");
    if html.is_empty() {
        bail!("ESMO item description empty")
    }
    let original = serde_json::to_string(item)?;
    let hash = crate::common::sha256_hex(original.as_bytes());
    let wrapped = format!("<article id='esmo-record'>{html}{citation}</article>");
    let (blocks, _) = blocks_from_html(&wrapped, Some(base), Some("#esmo-record"));
    let mut attachments = vec![];
    for (key, role) in [
        ("article_url", "publisher_article"),
        ("article_pdf", "publisher_pdf"),
    ] {
        if let Some(href) = e[key]["value"].as_str().filter(|s| !s.is_empty()) {
            let url = common::link(base, href).context("ESMO item publisher URL invalid")?;
            attachments.push(json!({"url":url,"role":role,"status":"unresolved; independent publisher access and rights","rights":"unknown"}));
        }
    }
    Ok((
        title,
        hash,
        blocks,
        attachments,
        json!({"source_item":item,"source_id":item["system"]["id"],"source_codename":item["system"]["codename"],"source_product_roles":e["guideline_product"]["value"],"source_creation_date":e["publish_date"]["value"],"source_sorting_date":e["last_update"]["value"],"source_citation_html":citation,"source_description_html":html}),
    ))
}
// --- Journal identifiers (2026-11-17 fix, group F) -------------------------------------------
// ESMO items link their full guideline as a journal article: either a doi.org URL (stated DOI)
// or an Elsevier article URL carrying only a PII. A PII is resolved to its DOI only through
// Crossref's official works API (`alternative-id` filter, exact match); nothing is derived.
const CROSSREF_WORKS: &str = "https://api.crossref.org/works";
const CROSSREF_BATCH: usize = 40;
fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}
fn doi_from_url(href: &str) -> Option<String> {
    let u = Url::parse(href.trim()).ok()?;
    if !matches!(u.host_str(), Some("doi.org" | "dx.doi.org" | "www.doi.org")) {
        return None;
    }
    let path = pct_decode(u.path().trim_start_matches('/'))?;
    regex::Regex::new(r"^10\.\d{4,9}/\S+$")
        .unwrap()
        .is_match(&path)
        .then_some(path)
}
fn compact_pii(p: &str) -> String {
    p.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase()
}
fn publisher_ids(item: &Value) -> Value {
    let e = &item["elements"];
    let urls = ["article_url", "article_pdf"]
        .iter()
        .filter_map(|k| e[*k]["value"].as_str().filter(|s| !s.is_empty()))
        .collect::<Vec<_>>();
    let doi = urls.iter().find_map(|u| doi_from_url(u));
    let re = regex::Regex::new(r"S\d{4}-\d{3}[\dXx]\(\d{2}\)\d{5}-[\dXx]").unwrap();
    let pii = urls.iter().find_map(|u| {
        let d = pct_decode(u).unwrap_or_else(|| u.to_string());
        re.find(&d).map(|m| m.as_str().to_uppercase())
    });
    let host = e["article_url"]["value"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())
        .and_then(|u| u.host_str().map(String::from));
    json!({"doi":doi,"doi_source":doi.as_ref().map(|_|"source article_url (doi.org link)"),"pii":pii,
           "pii_compact":pii.as_deref().map(compact_pii),"article_host":host,
           "article_url":e["article_url"]["value"],"article_pdf":e["article_pdf"]["value"]})
}
fn crossref_url(compact: &[String]) -> Result<String> {
    let filter = compact
        .iter()
        .map(|p| format!("alternative-id:{p}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(Url::parse_with_params(
        CROSSREF_WORKS,
        &[
            ("filter", filter.as_str()),
            ("rows", "100"),
            ("select", "DOI,alternative-id,container-title,license,title"),
        ],
    )?
    .to_string())
}
/// Crossref works response -> compact PII -> {doi, ...}. A PII listed under two different DOIs is
/// kept as ambiguous and never resolved.
fn crossref_dois(raw: &str) -> Result<HashMap<String, Value>> {
    let v: Value = serde_json::from_str(raw)?;
    if v["status"] != "ok" {
        bail!("Crossref works response status is not ok")
    }
    let items = v["message"]["items"]
        .as_array()
        .context("Crossref works items missing")?;
    if v["message"]["total-results"]
        .as_u64()
        .is_some_and(|t| t as usize > items.len())
    {
        bail!("Crossref works response truncated")
    }
    let mut out: HashMap<String, Value> = HashMap::new();
    for it in items {
        let doi = it["DOI"].as_str().context("Crossref DOI missing")?;
        let lic = it["license"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|l| l["content-version"] == "vor")
            .filter_map(|l| l["URL"].as_str())
            .collect::<Vec<_>>();
        for alt in it["alternative-id"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            let key = compact_pii(alt);
            let same = out.get(&key).map(|prev| prev["doi"].as_str() == Some(doi));
            match same {
                Some(false) => {
                    out.insert(key, json!({"ambiguous":true}));
                }
                Some(true) => {}
                None => {
                    out.insert(key, json!({"doi":doi,"container_title":it["container-title"][0],"title":it["title"][0],"vor_license_urls":lic}));
                }
            }
        }
    }
    Ok(out)
}
/// Fills `doi` for PII-only items of one dataview page (batched Crossref lookups). Failures are
/// recorded per item, never fatal.
async fn resolve_piis(f: &Fetcher, ids: &mut HashMap<String, Value>) {
    let mut todo = ids
        .values()
        .filter(|v| v["doi"].is_null())
        .filter_map(|v| v["pii_compact"].as_str().map(String::from))
        .collect::<Vec<_>>();
    todo.sort();
    todo.dedup();
    for chunk in todo.chunks(CROSSREF_BATCH) {
        let result = async {
            let url = crossref_url(chunk)?;
            let r = f.get_ok(&url).await?;
            Ok::<_, anyhow::Error>((crossref_dois(&r.text())?, r.record))
        }
        .await;
        for v in ids.values_mut() {
            let Some(p) = v["pii_compact"].as_str().map(String::from) else { continue };
            if !v["doi"].is_null() || !chunk.contains(&p) {
                continue;
            }
            match &result {
                Ok((map, record)) => {
                    v["crossref_fetch"] = json!(record);
                    match map.get(&p) {
                        Some(hit) if hit["doi"].is_string() => {
                            v["doi"] = hit["doi"].clone();
                            v["doi_source"] = json!("Crossref works API alternative-id exact match for the source PII");
                            v["crossref_record"] = hit.clone();
                        }
                        Some(_) => v["doi_lookup_error"] = json!("Crossref lists this PII under more than one DOI"),
                        None => v["doi_lookup_error"] = json!("Crossref returned no work for this PII"),
                    }
                }
                Err(e) => v["doi_lookup_error"] = json!(format!("Crossref lookup unavailable: {e:#}")),
            }
        }
    }
}
fn issue(src: &SourceEntry, url: &str, msg: String) -> PlanRow {
    PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("esmo-{}-unresolved", common::short_hash(url)),
        url: url.into(),
        title: None,
        rendition: None,
        version: None,
        extra: json!({"discovery_error":msg}),
    }
}
fn idx<'a>(a: &'a [Value], v: &Value) -> Result<&'a Value> {
    let n = v.as_u64().context("Nuxt reference is not an index")? as usize;
    a.get(n).context("Nuxt reference out of range")
}
fn field<'a>(a: &'a [Value], o: &Value, key: &str) -> Result<&'a Value> {
    idx(
        a,
        o.get(key)
            .with_context(|| format!("Nuxt field {key} missing"))?,
    )
}
fn scalar<'a>(a: &'a [Value], elements: &Value, key: &str) -> Result<&'a str> {
    let obj = field(a, elements, key)?;
    field(a, obj, "value")?
        .as_str()
        .with_context(|| format!("Nuxt {key} value is not a string"))
}
fn state(html: &str) -> Result<Value> {
    let d = Html::parse_document(html);
    let node = d
        .select(&sel("script#__NUXT_DATA__[type='application/json']"))
        .next()
        .context("ESMO Nuxt source state missing")?;
    Ok(serde_json::from_str(&node.text().collect::<String>())?)
}
fn source_categories(html: &str) -> Result<Vec<Value>> {
    let state = state(html)?;
    let a = state
        .as_array()
        .context("ESMO source state is not an array")?;
    let mut grids = a.iter().filter(|item| {
        item.get("system")
            .and_then(|v| idx(a, v).ok())
            .and_then(|s| s.get("codename"))
            .and_then(|v| idx(a, v).ok())
            .and_then(Value::as_str)
            == Some("n5fd7c0f7_98c1_0117_2dd4_d4a5a0b32f77")
    });
    let grid = grids.next().context("ESMO source topic grid missing")?;
    if grids.next().is_some() {
        bail!("ESMO source topic grid ambiguous")
    }
    let elements = field(a, grid, "elements")?;
    let items = field(a, elements, "items")?;
    let linked = field(a, items, "linkedItems")?
        .as_array()
        .context("ESMO topic grid items missing")?;
    let mut out = vec![];
    let mut seen = HashSet::new();
    for link in linked {
        let item = idx(a, link)?;
        let el = field(a, item, "elements")?;
        let system = field(a, item, "system")?;
        let get = |key: &str| {
            system
                .get(key)
                .and_then(|v| idx(a, v).ok())
                .and_then(Value::as_str)
        };
        let id = get("codename").context("ESMO category codename missing")?;
        let collection = get("collection").unwrap_or("");
        let title = scalar(a, el, "title")?;
        let path = scalar(a, el, "migration_url").unwrap_or("");
        let slug = scalar(a, el, "url").unwrap_or("");
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            bail!("ESMO unsupported category codename")
        }
        if seen.insert((collection.to_string(), id.to_string())) {
            out.push(json!({"codename":id,"collection":collection,"title":title,"source_migration_path":path,"source_slug":slug}));
        }
    }
    if out.is_empty() {
        bail!("ESMO source categories missing")
    }
    if out.len() > MAX_CATEGORIES {
        bail!("ESMO source category bound exceeded")
    }
    Ok(out)
}
fn source_queries(html: &str) -> Result<Vec<Value>> {
    let state = state(html)?;
    let a = state
        .as_array()
        .context("ESMO category state is not an array")?;
    let mut out = vec![];
    let mut seen = HashSet::new();
    for item in a {
        let Some(system) = item.get("system").and_then(|v| idx(a, v).ok()) else {
            continue;
        };
        let kind = system
            .get("type")
            .and_then(|v| idx(a, v).ok())
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind != "dataview" {
            continue;
        }
        let Some(elements) = item.get("elements").and_then(|v| idx(a, v).ok()) else {
            continue;
        };
        let Ok(query) = scalar(a, elements, "query") else {
            continue;
        };
        if query.trim().is_empty() {
            continue;
        }
        let codename = system
            .get("codename")
            .and_then(|v| idx(a, v).ok())
            .and_then(Value::as_str)
            .unwrap_or("");
        if seen.insert((codename.to_string(), query.to_string())) {
            out.push(json!({"codename":codename,"query":query}));
        }
    }
    if out.is_empty() {
        bail!("ESMO category has no source-exposed dataview query")
    }
    Ok(out)
}
fn api_url(path: &str) -> Result<Url> {
    let u = Url::parse(path)?;
    let base = Url::parse(API_BASE)?;
    if u.scheme() != "https"
        || u.origin() != base.origin()
        || u.username() != ""
        || u.password().is_some()
        || !u.path().starts_with("/rest/")
        || u.fragment().is_some()
    {
        bail!("ESMO source API URL outside verified origin")
    }
    Ok(u)
}
fn route_url(codes: &[String]) -> Result<String> {
    if codes.is_empty()
        || codes.len() > 50
        || codes.iter().any(|x| {
            x.is_empty()
                || !x
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
    {
        bail!("ESMO route batch has invalid source codenames")
    }
    Ok(format!(
        "{API_BASE}routes?codenames={}",
        codes
            .iter()
            .map(|c| format!("esmo_org{c}"))
            .collect::<Vec<_>>()
            .join(",")
    ))
}
fn resolved_route(data: &Value, codename: &str) -> Result<String> {
    let key = format!("esmo_org{codename}");
    let r = &data[&key];
    if r["name"] != codename {
        bail!("ESMO route lookup did not resolve source codename")
    }
    let path = r["path"]
        .as_str()
        .context("ESMO source route path missing")?;
    let u = Url::parse("https://www.esmo.org/")?.join(path)?;
    if u.scheme() != "https"
        || u.origin() != Url::parse("https://www.esmo.org/")?.origin()
        || !u.path().starts_with("/guidelines/")
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        bail!("ESMO route lookup returned unsupported route")
    }
    Ok(u.to_string())
}
fn query_url(query: &str) -> Result<String> {
    if query.len() > 4096 || query.is_empty() {
        bail!("ESMO source query size unsupported")
    }
    for (k, _) in url::form_urlencoded::parse(query.as_bytes()) {
        if !(k.starts_with("elements.") || k == "order" || k == "system.type")
            || k.contains("token")
            || k.contains("password")
        {
            bail!("ESMO source dataview query field unsupported: {k}")
        }
    }
    let u=format!("{API_BASE}items?includeTotalCount&system.workflow_step%5Bnin%5D=archived&limit=2000&depth=4&{query}");
    api_url(&u)?;
    Ok(u)
}
fn next_page_url(first: &str, current: &str, next: &str) -> Result<String> {
    let first = api_url(first)?;
    let current = api_url(current)?;
    let u = current.join(next)?;
    api_url(u.as_str())?;
    if u.path() != "/rest/items" {
        bail!("ESMO next page is not the source item endpoint")
    }
    let filters = |x: &Url| {
        let mut v = x
            .query_pairs()
            .filter(|(k, _)| k != "skip")
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Vec<_>>();
        v.sort();
        v
    };
    if filters(&first) != filters(&u) {
        bail!("ESMO next page changed source filters")
    }
    Ok(u.to_string())
}
fn catalog_inventory(html: &str) -> Result<(String, Value)> {
    let d = Html::parse_document(html);
    let node = d
        .select(&sel("script#__NUXT_DATA__[type='application/json']"))
        .next()
        .context("ESMO source catalog state missing")?;
    let raw = node.text().collect::<String>();
    let v: Value = serde_json::from_str(&raw)?;
    let a = v
        .as_array()
        .context("ESMO source catalog state is not an array")?;
    let mut items = vec![];
    for (i, item) in a.iter().enumerate() {
        let Some(element_ref) = item.get("elements") else {
            continue;
        };
        let Ok(elements) = idx(a, element_ref) else {
            continue;
        };
        let (Ok(title), Ok(path)) = (
            scalar(a, elements, "title"),
            scalar(a, elements, "migration_url"),
        ) else {
            continue;
        };
        if title.is_empty() || !path.starts_with("esmo/guidelines/") {
            continue;
        }
        let system = item.get("system").and_then(|s| idx(a, s).ok());
        let kind = system
            .and_then(|s| s.get("type"))
            .and_then(|s| idx(a, s).ok())
            .and_then(Value::as_str)
            .unwrap_or("");
        let modified = system
            .and_then(|s| s.get("lastModified"))
            .and_then(|s| idx(a, s).ok())
            .and_then(Value::as_str)
            .unwrap_or("");
        items.push(json!({"source_state_index":i,"title":title,"source_cms_path":path,"source_url_slug":scalar(a,elements,"url").unwrap_or(""),"source_path_role":"migration URL, not a verified public route","source_item_type":kind,"source_last_modified":modified,"status":"unresolved; CMS path is not a verified public route"}));
    }
    if items.is_empty() {
        bail!("ESMO source catalog exposes no mapped CMS inventory")
    }
    let categories = source_categories(html)?;
    Ok((
        crate::common::sha256_hex(raw.as_bytes()),
        json!({"source_items":items,"source_categories":categories,"source_nuxt_data":raw,"scope":"embedded catalog source categories and CMS migration paths; public routes require source lookup"}),
    ))
}
fn topic(html: &str, base: &Url) -> Result<(String, String, Vec<Block>, Vec<Value>, Value)> {
    let d = Html::parse_document(html);
    let node = d
        .select(&sel("script#__NUXT_DATA__[type='application/json']"))
        .next()
        .context("ESMO Nuxt state missing")?;
    let raw = node.text().collect::<String>();
    let v: Value = serde_json::from_str(&raw)?;
    let a = v.as_array().context("ESMO Nuxt state not an array")?;
    let root = a.get(1).context("ESMO Nuxt root missing")?;
    let pinia = field(a, root, "pinia")?;
    let app = field(a, pinia, "app")?;
    let page = field(a, app, "page")?;
    let item = field(a, page, "item")?;
    let elements = field(a, item, "elements")?;
    let title = scalar(a, elements, "title")?.to_string();
    let content = scalar(a, elements, "content")?;
    let citation = scalar(a, elements, "citation")?;
    if title.is_empty() || content.is_empty() {
        bail!("ESMO topic content missing")
    }
    let mut assets = vec![];
    for (key, role) in [
        ("article_url", "publisher_article"),
        ("article_pdf", "publisher_pdf"),
    ] {
        let href = scalar(a, elements, key)?;
        if !href.is_empty() {
            let u = common::link(base, href).context("ESMO publisher link invalid")?;
            assets.push(json!({"url":u,"role":role,"rights":"unknown; independent publisher"}));
        }
    }
    let hash = crate::common::sha256_hex(raw.as_bytes());
    let wrapped = format!("<article id='esmo-source'>{content}{citation}</article>");
    let (blocks, _) = blocks_from_html(&wrapped, Some(base), Some("#esmo-source"));
    let creation = scalar(a, elements, "publish_date").unwrap_or("");
    let sorting = scalar(a, elements, "last_update").unwrap_or("");
    Ok((
        title,
        hash,
        blocks,
        assets,
        json!({"source_content_html":content,"source_citation_html":citation,"source_creation_date":creation,"source_sorting_date":sorting,"source_nuxt_data":raw}),
    ))
}
// Public living pages carry clinical HTML in a reference-indexed JSON field, not SSR DOM.
// Resolve only schema-defined references; an integer at the target is a scalar.
const LIVING_PREFIX: &str = "/guidelines/living-guidelines";
fn living_url(value: &str, base: &Url) -> Result<Url> {
    let u = base.join(value)?;
    if u.scheme() != "https"
        || u.host_str() != Some("www.esmo.org")
        || u.port_or_known_default() != Some(443)
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || !(u.path() == LIVING_PREFIX || u.path().starts_with(&format!("{LIVING_PREFIX}/")))
    {
        bail!("ESMO living route outside verified public path");
    }
    Ok(u)
}
fn living_ref<'a>(a: &'a [Value], reference: &Value) -> Result<&'a Value> {
    let index = reference
        .as_u64()
        .context("ESMO living reference is not an index")?;
    let mut value = a
        .get(usize::try_from(index)?)
        .context("ESMO living reference out of range")?;
    for _ in 0..8 {
        if let Some(wrapper) = value.as_array().filter(|v| {
            v.len() == 2
                && matches!(
                    v[0].as_str(),
                    Some("Reactive" | "ShallowReactive" | "Ref" | "ShallowRef")
                )
        }) {
            value = a
                .get(usize::try_from(
                    wrapper[1]
                        .as_u64()
                        .context("ESMO living wrapper index invalid")?,
                )?)
                .context("ESMO living wrapper out of range")?;
        } else {
            return Ok(value);
        }
    }
    bail!("ESMO living wrapper depth exceeded")
}
fn living_field<'a>(a: &'a [Value], object: &Value, key: &str) -> Result<&'a Value> {
    living_ref(
        a,
        object
            .get(key)
            .context("ESMO living source field missing")?,
    )
}
fn living_scalar<'a>(a: &'a [Value], elements: &Value, key: &str) -> Result<&'a str> {
    let field = living_field(a, elements, key)?;
    living_field(a, field, "value")?
        .as_str()
        .context("ESMO living source value is not a string")
}
fn living_state(html: &str) -> Result<Value> {
    if html.len() > 8 * 1024 * 1024 {
        bail!("ESMO living HTML size bound exceeded")
    }
    let d = Html::parse_document(html);
    let script_selector = sel("script#__NUXT_DATA__[type='application/json']");
    let mut scripts = d.select(&script_selector);
    let script = scripts.next().context("ESMO living JSON state missing")?;
    if scripts.next().is_some() {
        bail!("ESMO living JSON state ambiguous")
    }
    let raw = script.text().collect::<String>();
    if raw.len() > 8 * 1024 * 1024 {
        bail!("ESMO living JSON size bound exceeded")
    }
    let state: Value = serde_json::from_str(&raw)?;
    let array = state
        .as_array()
        .context("ESMO living state is not an array")?;
    if array.len() > 20000 {
        bail!("ESMO living reference count bound exceeded")
    }
    Ok(state)
}
fn living_item<'a>(a: &'a [Value], kind: &str) -> Result<&'a Value> {
    let mut items = a.iter().filter(|item| {
        item.get("system")
            .and_then(|r| living_ref(a, r).ok())
            .and_then(|system| system.get("type"))
            .and_then(|r| living_ref(a, r).ok())
            .and_then(Value::as_str)
            == Some(kind)
    });
    let item = items.next().context("ESMO living source item missing")?;
    if items.next().is_some() {
        bail!("ESMO living source item ambiguous")
    }
    Ok(item)
}
fn native_occurrences(text: &str) -> Vec<Value> {
    let bracket = regex::Regex::new(r"\[[^\[\]]{1,2000}\]").unwrap();
    let pair = regex::Regex::new(r"^\[\s*(I|II|III|IV|V)\s*,\s*([A-E])(?:\s*;|\s*\])").unwrap();
    bracket.find_iter(text).map(|m| {
        let capture=pair.captures(m.as_str());
        json!({"bracket":m.as_str(),"utf8_byte_start":m.start(),"utf8_byte_end":m.end(),
            "unicode_scalar_start":text[..m.start()].chars().count(),"unicode_scalar_end":text[..m.end()].chars().count(),
            "native_evidence_level":capture.as_ref().map(|c|c[1].to_string()),
            "native_recommendation_grade":capture.as_ref().map(|c|c[2].to_string()),
            "escat_present":m.as_str().contains("ESCAT"),"esmo_mcbs_present":m.as_str().contains("ESMO-MCBS"),
            "coordinate_scope":"normalized own list-item text; nested lists excluded"})
    }).collect()
}
struct LivingContent {
    blocks: Vec<Block>,
    lists: Vec<Value>,
    warnings: Vec<String>,
    next: usize,
    headings: Vec<String>,
    ids: HashSet<String>,
}
impl LivingContent {
    fn walk(
        &mut self,
        element: scraper::ElementRef<'_>,
        base: &Url,
        parent: Option<&str>,
        depth: usize,
    ) -> Result<()> {
        if depth > 64 || self.next > 10000 {
            bail!("ESMO living clinical DOM bound exceeded")
        }
        let tag = element.value().name();
        if matches!(tag, "ul" | "ol") {
            let start = element
                .value()
                .attr("start")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(1);
            let mut ordinal = 0;
            for item in element
                .children()
                .filter_map(scraper::ElementRef::wrap)
                .filter(|e| e.value().name() == "li")
            {
                ordinal += 1;
                self.next += 1;
                if self.next > 10000 {
                    bail!("ESMO living clinical item bound exceeded")
                }
                let source_ordinal = if tag == "ol" {
                    Some(
                        start
                            .checked_add(ordinal - 1)
                            .context("ESMO living list ordinal overflow")?,
                    )
                } else {
                    None
                };
                let mut id = item
                    .value()
                    .attr("id")
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("esmo-living-item-{}", self.next));
                while !self.ids.insert(id.clone()) {
                    id.push('-');
                }
                let nested = item
                    .select(&sel("ul,ol"))
                    .filter(|e| {
                        !e.ancestors()
                            .filter_map(scraper::ElementRef::wrap)
                            .filter(|a| !std::ptr::eq(a.value(), e.value()))
                            .take_while(|a| !std::ptr::eq(a.value(), item.value()))
                            .any(|a| matches!(a.value().name(), "ul" | "ol"))
                    })
                    .collect::<Vec<_>>();
                let mut own = item.html();
                for list in &nested {
                    own = own.replacen(&list.html(), "", 1)
                }
                let fragment = Html::parse_fragment(&own);
                let own_text = crate::guidelines::html::text_of(&fragment.root_element());
                let mut path = self.headings.clone();
                if let Some(parent) = parent {
                    path.push(format!("Nested clinical clause of {parent}"));
                }
                let source_sequence=item.children().enumerate().map(|(position,node)| {
                    if let Some(element)=scraper::ElementRef::wrap(node) {json!({"child_position":position,"kind":if matches!(element.value().name(),"ul"|"ol"){"nested_list"}else{"own_component"},"tag":element.value().name(),"html":element.html()})}
                    else {json!({"child_position":position,"kind":"own_text","text":match node.value(){scraper::node::Node::Text(t)=>Some(t.to_string()),_=>None}})}
                }).collect::<Vec<_>>();
                let kind = if parent.is_none() {
                    BlockKind::Recommendation {
                        label: None,
                        strength: None,
                        certainty: None,
                        population: None,
                        grading_system: None,
                        qualifier: None,
                    }
                } else {
                    BlockKind::ListItem
                };
                self.blocks.push(Block {
                    block_id: id.clone(),
                    kind,
                    text: own_text.clone(),
                    heading_path: path,
                    page: None,
                });
                self.lists.push(json!({"block_id":id,"parent_block_id":parent,"list_kind":tag,"sibling_ordinal":ordinal,
                    "source_list_start":element.value().attr("start"),"source_ordinal":source_ordinal,
                    "source_child_sequence":source_sequence,"source_html":item.html(),"own_html":own,"native_occurrences":native_occurrences(&own_text),
                    "formal_heading_observed":false,"canonical_strength_and_certainty":"unknown; native scales retained at their exact source scopes"}));
                // Preserve embedded clinical tables/figures, independently of recommendation grades.
                let (extras, notes) = blocks_from_html(&own, Some(base), None);
                self.warnings.extend(notes);
                for mut block in extras.into_iter().filter(|b| {
                    matches!(
                        b.kind,
                        BlockKind::Table { .. } | BlockKind::Figure { .. } | BlockKind::Footnote
                    )
                }) {
                    block.block_id = format!("{id}-{}", block.block_id);
                    self.blocks.push(block);
                }
                for list in nested {
                    self.walk(list, base, Some(&id), depth + 1)?;
                }
            }
        } else if matches!(tag, "html" | "body" | "div" | "article" | "section") {
            for child in element.children().filter_map(scraper::ElementRef::wrap) {
                self.walk(child, base, parent, depth + 1)?;
            }
        } else {
            let (mut blocks, warnings) = blocks_from_html(&element.html(), Some(base), None);
            for block in &mut blocks {
                if let BlockKind::Heading { level } = block.kind {
                    self.headings.truncate(usize::from(level).saturating_sub(1));
                    self.headings.push(block.text.clone());
                }
                block.heading_path = self.headings.clone();
                self.next += 1;
                block.block_id = format!("esmo-living-context-{}-{}", self.next, block.block_id);
            }
            self.blocks.extend(blocks);
            self.warnings.extend(warnings);
        }
        Ok(())
    }
}
struct LivingLeaf {
    title: String,
    id: String,
    version: Option<String>,
    hash: String,
    blocks: Vec<Block>,
    metadata: Value,
    assets: Vec<Value>,
    warnings: Vec<String>,
    item_type: &'static str,
}
fn living_leaf(html: &str, base: &Url) -> Result<LivingLeaf> {
    living_url(base.as_str(), base)?;
    let state = living_state(html)?;
    let a = state.as_array().unwrap();
    let item = living_item(a, "living_guideline_section")?;
    let system = living_field(a, item, "system")?;
    let elements = living_field(a, item, "elements")?;
    let id = living_field(a, system, "id")?
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 200)
        .context("ESMO living native ID missing")?
        .to_string();
    let title = living_scalar(a, elements, "title")?.to_string();
    let content = living_scalar(a, elements, "content")?;
    if title.trim().is_empty() || content.len() > 2 * 1024 * 1024 {
        bail!("ESMO living title/content bound invalid")
    }
    let parsed = Html::parse_fragment(content);
    if parsed.select(&sel("ul > li, ol > li")).next().is_none() {
        bail!("ESMO living clinical list missing; other templates unresolved")
    }
    let mut clinical = LivingContent {
        blocks: vec![],
        lists: vec![],
        warnings: vec![],
        next: 0,
        headings: vec![],
        ids: HashSet::new(),
    };
    clinical.walk(parsed.root_element(), base, None, 0)?;
    let footnotes = living_scalar(a, elements, "footnotes").unwrap_or("");
    if !footnotes.trim().is_empty() {
        let (mut blocks, notes) = blocks_from_html(footnotes, Some(base), None);
        for block in &mut blocks {
            block.block_id = format!("esmo-living-footnote-{}", block.block_id);
        }
        clinical.blocks.extend(blocks);
        clinical.warnings.extend(notes);
    }
    let mut assets = vec![];
    let mut seen = HashSet::new();
    let linked_html = Html::parse_fragment(&format!("{content}{footnotes}"));
    for anchor in linked_html.select(&sel("a[href]")) {
        if let Some(url) = common::link(base, anchor.value().attr("href").unwrap_or_default()) {
            if seen.insert(url.clone()) {
                assets.push(json!({"url":url,"label":crate::guidelines::html::text_of(&anchor),"role":"source_linked_supporting_asset","bytes_downloaded":false,"mime_verified":false,"evidence_url":base.as_str()}));
            }
        }
    }
    // A leaf's known schema has no version/date fields. Missing fields stay unknown;
    // display context is added only when its source reference path is established.
    let version = living_scalar(a, elements, "version")
        .ok()
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    let metadata = json!({"source_id":id,"source_type":"living_guideline_section","source_codename":living_field(a,system,"codename").ok().and_then(Value::as_str),"source_url":base.as_str(),"source_content_html":content,
        "source_footnotes_html":footnotes,"source_list_tree":clinical.lists,"native_version":version,
        "source_creation_date":living_scalar(a,elements,"publish_date").ok(),"source_sorting_date":living_scalar(a,elements,"last_update").ok(),
        "source_cms_modified":living_field(a,system,"lastModified").ok().and_then(Value::as_str),
        "source_cms_workflow_step":living_field(a,system,"workflowStep").ok().and_then(Value::as_str),"source_cms_workflow":living_field(a,system,"workflow").ok().and_then(Value::as_str),"source_cms_language":living_field(a,system,"language").ok().and_then(Value::as_str),
        "native_grade_policy":"ESMO evidence level/recommendation grade, ESCAT and ESMO-MCBS remain source-specific scoped occurrences; canonical certainty is unknown",
        "inventory_complete":false,"clinical_content_acquired":true});
    let hash = crate::common::sha256_hex(&serde_json::to_vec(
        &json!({"title":title,"metadata":metadata,"assets":assets,"blocks":clinical.blocks}),
    )?);
    clinical.warnings.push("Public Nuxt living clinical leaf only; no complete living-index enumeration or publisher-original/full-PDF acquisition is claimed. Linked supporting assets are descriptors, not downloaded bytes. Native grade occurrences are scoped source fields, not inferred GRADE certainty. Leaf-owned version/date fields remain unknown when absent; any acquired related-guideline version context is separately scoped.".into());
    Ok(LivingLeaf {
        title,
        id,
        version,
        hash,
        blocks: clinical.blocks,
        metadata,
        assets,
        warnings: clinical.warnings,
        item_type: "living_guideline_section",
    })
}
// An index/parent record is useful source metadata, but is never clinical full text.
fn living_index(html: &str, base: &Url) -> Result<LivingLeaf> {
    living_url(base.as_str(), base)?;
    let state = living_state(html)?;
    let a = state.as_array().unwrap();
    let (item, source_type, role) = if let Ok(item) = living_item(a, "living_guideline") {
        (item, "living_guideline", "living_guideline_index")
    } else if base.path() == LIVING_PREFIX {
        (
            living_item(a, "page")?,
            "page",
            "living_guideline_catalog_index",
        )
    } else {
        bail!("ESMO living hierarchy/group template unresolved")
    };
    let system = living_field(a, item, "system")?;
    let elements = living_field(a, item, "elements")?;
    let id = living_field(a, system, "id")?
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 200)
        .context("ESMO living index native ID missing")?
        .to_string();
    let title = living_scalar(a, elements, "title")?.to_string();
    let version = living_scalar(a, elements, "version")
        .ok()
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    let mut fields = serde_json::Map::new();
    let mut blocks = vec![];
    let mut warnings = vec![];
    let mut assets = vec![];
    let mut seen = HashSet::new();
    for key in ["introduction", "citation"] {
        if let Ok(html) = living_scalar(a, elements, key) {
            fields.insert(key.into(), json!(html));
            let (mut part, notes) = blocks_from_html(html, Some(base), None);
            for block in &mut part {
                block.block_id = format!("esmo-living-index-{key}-{}", block.block_id);
            }
            blocks.extend(part);
            warnings.extend(notes);
            let fragment = Html::parse_fragment(html);
            for link in fragment.select(&sel("a[href]")) {
                if let Some(url) = common::link(base, link.value().attr("href").unwrap_or_default())
                {
                    if seen.insert(url.clone()) {
                        assets.push(json!({"url":url,"role":"source_index_supporting_link","label":crate::guidelines::html::text_of(&link),"bytes_downloaded":false,"mime_verified":false,"evidence_url":base.as_str()}));
                    }
                }
            }
        }
    }
    if let Ok(field) = living_field(a, elements, "related_tools") {
        if let Ok(items) = living_field(a, field, "linkedItems") {
            let items = items
                .as_array()
                .context("ESMO related tool links are not an array")?;
            if items.len() > 64 {
                bail!("ESMO related tool bound exceeded")
            }
            let mut tools = vec![];
            for reference in items {
                let tool = living_ref(a, reference)?;
                let system = living_field(a, tool, "system")?;
                let el = living_field(a, tool, "elements")?;
                let mut native = serde_json::Map::new();
                for key in ["title", "label", "content", "url"] {
                    if let Ok(value) = living_scalar(a, el, key) {
                        native.insert(key.into(), json!(value));
                        if key == "content" {
                            let html = Html::parse_fragment(value);
                            for link in html.select(&sel("a[href]")) {
                                if let Some(url) = common::link(
                                    base,
                                    link.value().attr("href").unwrap_or_default(),
                                ) {
                                    if seen.insert(url.clone()) {
                                        assets.push(json!({"url":url,"role":"source_related_tool_link","bytes_downloaded":false,"mime_verified":false,"evidence_url":base.as_str()}));
                                    }
                                }
                            }
                        }
                    }
                }
                tools.push(json!({"source_id":living_field(a,system,"id").ok().and_then(Value::as_str),"source_type":living_field(a,system,"type").ok().and_then(Value::as_str),"source_fields":native}));
            }
            fields.insert("related_tools".into(), json!(tools));
        }
    }
    let metadata = json!({"source_id":id,"source_type":source_type,"source_codename":living_field(a,system,"codename").ok().and_then(Value::as_str),"source_url":base.as_str(),"native_version":version,"clinical_content_acquired":false,
        "source_creation_date":living_scalar(a,elements,"publish_date").ok(),"source_sorting_date":living_scalar(a,elements,"last_update").ok(),
        "source_cms_modified":living_field(a,system,"lastModified").ok().and_then(Value::as_str),
        "source_cms_workflow_step":living_field(a,system,"workflowStep").ok().and_then(Value::as_str),"source_cms_workflow":living_field(a,system,"workflow").ok().and_then(Value::as_str),"source_cms_language":living_field(a,system,"language").ok().and_then(Value::as_str),"source_index_fields":fields,
        "sitemap_role":"Observed parent sitemap field is taxonomy, not a verified child-link inventory","inventory_complete":false});
    let hash = crate::common::sha256_hex(&serde_json::to_vec(
        &json!({"title":title,"metadata":metadata,"blocks":blocks,"assets":assets}),
    )?);
    warnings.push("Living hierarchy/index metadata only; no clinical full text claimed. Index dataview items are empty in the saved response; parent sitemap is taxonomy. Child route/store and pagination contracts remain unresolved; no API or child path was synthesized.".into());
    Ok(LivingLeaf {
        title,
        id,
        version,
        hash,
        blocks,
        metadata,
        assets,
        warnings,
        item_type: role,
    })
}
fn living_resource(html: &str, base: &Url) -> Result<LivingLeaf> {
    let state = living_state(html)?;
    let a = state.as_array().unwrap();
    let sections = a
        .iter()
        .filter(|item| {
            item.get("system")
                .and_then(|r| living_ref(a, r).ok())
                .and_then(|system| system.get("type"))
                .and_then(|r| living_ref(a, r).ok())
                .and_then(Value::as_str)
                == Some("living_guideline_section")
        })
        .count();
    if sections > 0 {
        living_leaf(html, base)
    } else {
        living_index(html, base)
    }
}
fn living_transport(page: &crate::guidelines::fetch::Fetched, xml: bool) -> Result<()> {
    if page.body.len() > 8 * 1024 * 1024 {
        bail!("ESMO living response size bound exceeded")
    }
    let requested = Url::parse(&page.record.url)?;
    let actual = Url::parse(&page.record.final_url)?;
    if requested.origin() != actual.origin()
        || actual.scheme() != "https"
        || actual.host_str() != Some("www.esmo.org")
    {
        bail!("ESMO living response origin changed")
    }
    let mime = page
        .record
        .content_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if if xml {
        !matches!(mime.as_str(), "application/xml" | "text/xml")
    } else {
        !matches!(mime.as_str(), "text/html" | "application/xhtml+xml")
    } {
        bail!("ESMO living response MIME unsupported")
    }
    Ok(())
}
// Verified public-cache shape: routes is ["Map", keyRef, routeRef, ...]. Keys and
// values are required references. Repeated aliases of the identical target record do
// not create a second route; distinct records with one path remain ambiguous.
fn living_route_values<'a>(a: &'a [Value], routes: &Value) -> Result<Vec<&'a Value>> {
    let array = routes
        .as_array()
        .context("ESMO source routes are not an array")?;
    if array.len() > 2000 {
        bail!("ESMO source route count exceeded")
    }
    let mut resolved: Vec<&Value> = vec![];
    let references = if array.first().and_then(Value::as_str) == Some("Map") {
        if (array.len() - 1) % 2 != 0 {
            bail!("ESMO source route Map has incomplete key/value pairs")
        }
        let mut values = vec![];
        for pair in array[1..].chunks_exact(2) {
            living_ref(a, &pair[0])?
                .as_str()
                .context("ESMO source route Map key is not a string")?;
            values.push(&pair[1]);
        }
        values
    } else {
        array.iter().collect::<Vec<_>>()
    };
    for reference in references {
        let route = living_ref(a, reference)?;
        if !route.is_object() {
            bail!("ESMO source route value is not an object")
        }
        if !resolved.iter().any(|old| std::ptr::eq(*old, route)) {
            resolved.push(route)
        }
    }
    Ok(resolved)
}
fn living_route_context(html: &str, base: &Url) -> Result<Option<Value>> {
    let state = living_state(html)?;
    let a = state.as_array().unwrap();
    let Some(root) = a.get(1) else {
        return Ok(None);
    };
    let Ok(pinia) = living_field(a, root, "pinia") else {
        return Ok(None);
    };
    let app = living_field(a, pinia, "app")?;
    let routes = living_field(a, app, "routes")?;
    let resolved = living_route_values(a, routes)?;
    let get = |object: &Value, key: &str| -> Option<String> {
        living_field(a, object, key)
            .ok()
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let mut current = resolved.iter().filter(|route| {
        get(route, "path").is_some_and(|path| living_url(&path, base).ok().as_ref() == Some(base))
    });
    let Some(route) = current.next() else {
        return Ok(None);
    };
    if current.next().is_some() {
        bail!("ESMO current source route ambiguous")
    }
    let Some(smr) = get(route, "smr").filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let mut contexts = resolved.iter().filter(|candidate| {
        get(candidate, "name").as_deref() == Some(smr.as_str())
            && get(candidate, "type").as_deref() == Some("living_guideline")
    });
    let Some(context) = contexts.next() else {
        return Ok(None);
    };
    if contexts.next().is_some() {
        bail!("ESMO source smr route context ambiguous")
    }
    let path = get(context, "path").context("ESMO source smr route path missing")?;
    let url = living_url(&path, base)?;
    if url == *base {
        return Ok(None);
    }
    Ok(Some(
        json!({"url":url.as_str(),"source_route_name":get(route,"name"),"source_smr":smr,"related_route_name":get(context,"name"),"related_route_type":"living_guideline","evidence_url":base.as_str(),"association":"pinia.app.routes: current route smr equals related route name"}),
    ))
}
async fn living_load(
    f: &Fetcher,
    page: &crate::guidelines::fetch::Fetched,
    base: &Url,
) -> Result<(LivingLeaf, Vec<crate::guidelines::doc::FetchRecord>)> {
    let mut resource = living_resource(&page.text(), base)?;
    let mut records = vec![];
    if resource.item_type == "living_guideline_section" {
        if let Some(mut context) = living_route_context(&page.text(), base)? {
            let url = context["url"]
                .as_str()
                .context("ESMO source context URL missing")?
                .to_string();
            let related = f.get_ok(&url).await?;
            living_transport(&related, false)?;
            let actual = living_url(&related.record.final_url, base)?;
            let parent = living_index(&related.text(), &actual)?;
            if parent.item_type != "living_guideline_index"
                || parent.metadata["source_codename"] != context["related_route_name"]
            {
                bail!("ESMO source smr context type/codename does not match emitted route")
            }
            context["source_record"] = parent.metadata.clone();
            context["source_record_sha256"] = json!(parent.hash);
            // Browser and source route association establish the displayed living-guideline
            // version's scope. The leaf's own absent native_version remains null in metadata.
            context["version_scope"]=json!("related living_guideline record selected by source smr association; not a leaf-owned version field");
            if resource.version.is_none() {
                resource.version = parent.version.clone();
            }
            resource.metadata["related_guideline_context"] = context;
            resource.metadata["displayed_guideline_version"] = json!(resource.version);
            resource.hash = crate::common::sha256_hex(&serde_json::to_vec(
                &json!({"title":resource.title,"metadata":resource.metadata,"version":resource.version,"assets":resource.assets,"blocks":resource.blocks}),
            )?);
            resource.warnings.push("Displayed living-guideline version is scoped to the source smr-associated living_guideline record. Leaf-owned version/date fields remain unknown; related creation/sorting dates are not leaf publication dates.".into());
            records.push(related.record);
        }
    }
    Ok((resource, records))
}

const LIVING_SITEMAP: &str = "https://www.esmo.org/sitemap_index.xml"; // Explicit robots declaration observed at source.
fn living_sitemap(xml: &str, index: bool) -> Result<Vec<(String, Option<String>)>> {
    if xml.len() > 8 * 1024 * 1024 {
        bail!("ESMO sitemap byte bound exceeded")
    }
    let document = roxmltree::Document::parse(xml).context("ESMO sitemap XML invalid")?;
    let root = document.root_element();
    if root.tag_name().name() != if index { "sitemapindex" } else { "urlset" } {
        bail!("ESMO sitemap root unsupported")
    }
    let mut out = vec![];
    let mut seen = HashSet::new();
    for entry in root.children().filter(|n| n.is_element()) {
        if entry.tag_name().name() != if index { "sitemap" } else { "url" } {
            continue;
        }
        let raw = entry
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "loc")
            .and_then(|n| n.text())
            .context("ESMO sitemap location missing")?
            .trim();
        let url = Url::parse(raw)?;
        if url.scheme() != "https"
            || url.host_str() != Some("www.esmo.org")
            || url.port_or_known_default() != Some(443)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("ESMO sitemap location outside verified origin")
        }
        if index && url.path() != "/__sitemap__/en.xml" {
            bail!("ESMO unverified sitemap shard; additional shard coverage unresolved")
        }
        if !index && living_url(raw, &url).is_err() {
            continue;
        }
        if !seen.insert(url.to_string()) {
            continue;
        }
        let modified = entry
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "lastmod")
            .and_then(|n| n.text())
            .map(|v| v.trim().to_string());
        out.push((url.to_string(), modified));
        if out.len() > if index { 8 } else { 2000 } {
            bail!("ESMO sitemap entry bound exceeded")
        }
    }
    if out.is_empty() {
        bail!("ESMO sitemap contains no recognized locations")
    }
    Ok(out)
}
fn living_plan(src: &SourceEntry, url: &Url, resource: LivingLeaf, scope: Value) -> PlanRow {
    let snapshot = crate::common::sha256_hex(
        &serde_json::to_vec(&json!({"clinical_sha256":resource.hash,"source_discovery":scope}))
            .expect("JSON-only source snapshot"),
    );
    PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("living-{}-{}", common::slug(&resource.id), &snapshot[..12]),
        url: url.to_string(),
        title: Some(resource.title),
        rendition: Some("html".into()),
        version: resource.version,
        extra: json!({"kind":if resource.item_type=="living_guideline_section" {"living_clinical_leaf"}else{"living_index"},"source_id":resource.id,"clinical_sha256":resource.hash,"discovery_scope":scope}),
    }
}

async fn living_discover(
    src: &SourceEntry,
    f: &Fetcher,
    limit: Option<usize>,
) -> Result<Vec<PlanRow>> {
    if limit == Some(0) {
        return Ok(vec![]);
    }
    let requested = src
        .catalog_url
        .as_deref()
        .context("ESMO living starting URL missing")?;
    let base = living_url(requested, &Url::parse("https://www.esmo.org/")?)?;
    let page = f.get_ok(base.as_str()).await?;
    living_transport(&page, false)?;
    let final_url = living_url(&page.record.final_url, &base)?;
    let (resource, _) = living_load(f, &page, &final_url).await?;
    let first = living_plan(
        src,
        &final_url,
        resource,
        json!({"role":"explicit source-linked starting page","inventory_complete":false}),
    );
    if final_url.path() != LIVING_PREFIX {
        return Ok(vec![first]);
    }
    let include_indexes = src.config["include_living_indexes"]
        .as_bool()
        .unwrap_or(false);
    let mut rows = if include_indexes { vec![first] } else { vec![] };
    let index = f.get_ok(LIVING_SITEMAP).await?;
    living_transport(&index, true)?;
    let mut candidates = vec![];
    let mut seen = HashSet::new();
    for (shard, _) in living_sitemap(&index.text(), true)? {
        let fetched = f.get_ok(&shard).await?;
        living_transport(&fetched, true)?;
        for (url, lastmod) in living_sitemap(&fetched.text(), false)? {
            if url != final_url.as_str() && seen.insert(url.clone()) {
                candidates.push((url, lastmod, fetched.record.clone()));
            }
        }
    }
    let total = candidates.len();
    let offset = src.config["living_sitemap_start"]
        .as_u64()
        .unwrap_or(0)
        .min(2000) as usize;
    // The normal no-limit crawl can visit the whole finite sitemap. A scoped smoke
    // explicitly supplies a small cap; requesting N means N clinical leaves, not N metadata pages.
    let cap = src.config["living_max_candidates"]
        .as_u64()
        .unwrap_or(if limit.is_some() { 32 } else { 2000 })
        .clamp(1, 2000) as usize;
    let mut visited = 0usize;
    let mut clinical = 0usize;
    let mut metadata_pages = 0usize;
    let mut unresolved = vec![];
    for (url, lastmod, shard_fetch) in candidates.into_iter().skip(offset).take(cap) {
        visited += 1;
        let result:Result<Option<PlanRow>>=async {
            let fetched=f.get_ok(&url).await?;living_transport(&fetched,false)?;
            let actual=living_url(&fetched.record.final_url,&base)?;
            let (resource,_)=living_load(f,&fetched,&actual).await?;
            let is_clinical=resource.item_type=="living_guideline_section";
            if is_clinical {clinical+=1}else{metadata_pages+=1;}
            if !is_clinical && !include_indexes {return Ok(None)}
            let mut row=living_plan(src,&actual,resource,json!({"role":"source-declared sitemap route; actual source item type classifies role","sitemap_index":LIVING_SITEMAP,"sitemap_lastmod":lastmod,"lastmod_scope":"sitemap metadata only, not clinical version/publication date","inventory_complete":false}));
            row.extra["living_index_fetch"]=json!(page.record);row.extra["sitemap_index_fetch"]=json!(index.record);row.extra["sitemap_shard_fetch"]=json!(shard_fetch);
            Ok(Some(row))
        }.await;
        match result {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => {}
            Err(error) => unresolved
                .push(json!({"url":url,"state":"unresolved","error":format!("{error:#}")})),
        }
        if limit.is_some_and(|n| clinical >= n) {
            break;
        }
    }
    let next_offset = offset.min(total) + visited;
    let coverage = json!({"total":total,"visited":visited,"next_offset":next_offset.min(total),"unvisited":total.saturating_sub(next_offset),"skipped_before_start":offset.min(total),"candidate_budget":cap,"budget_termination":next_offset<total && visited>=cap,"clinical_limit_reached":limit.is_some_and(|n|clinical>=n),"clinical_leaves":clinical,"metadata_pages":metadata_pages,"unresolved_routes":unresolved,"sitemap_membership_visited_all":offset==0 && next_offset>=total,"inventory_complete":false,"enumeration_scope":"finite source-declared living sitemap membership; classification and clinical coverage remain separately audited"});
    if rows.is_empty() {
        bail!(
            "ESMO bounded living discovery acquired no clinical leaves; coverage={}",
            serde_json::to_string(&coverage)?
        )
    }
    for row in &mut rows {
        row.extra["bounded_sitemap_coverage"] = coverage.clone();
    }
    Ok(rows)
}

// R2a-crosswalk: the site scrape (unchanged) lives in `discover_site`/`extract_site`; the trait impl at the end
// of this block adds the PMC open-access crosswalk for each item's journal DOI (`extra.doi`).
impl Esmo {
    async fn discover_site(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        if src
            .catalog_url
            .as_deref()
            .and_then(|u| Url::parse(u).ok())
            .is_some_and(|u| {
                u.path() == LIVING_PREFIX || u.path().starts_with(&format!("{LIVING_PREFIX}/"))
            })
        {
            return living_discover(src, f, limit).await;
        }
        let cat = f
            .get_ok(src.catalog_url.as_deref().context("ESMO catalog missing")?)
            .await?;
        let (catalog_hash, _) = catalog_inventory(&cat.text())?;
        let categories = source_categories(&cat.text())?;
        // The catalog page is an inventory, not a guideline: it is recorded on every item row
        // (`catalog_sha256`, `catalog_fetch`) instead of being emitted as its own document.
        let mut rows: Vec<PlanRow> = vec![];
        let mut row_of_item: HashMap<String, usize> = HashMap::new();
        for batch in categories.chunks(50) {
            let mut codes = vec![];
            for c in batch {
                if c["collection"] != "default" {
                    rows.push(issue(
                        src,
                        cat.record.url.as_str(),
                        format!("ESMO unsupported source collection for {}", c["codename"]),
                    ));
                    if limit.is_some_and(|n| rows.len() >= n) {
                        return Ok(rows);
                    }
                    continue;
                }
                codes.push(
                    c["codename"]
                        .as_str()
                        .context("ESMO category codename missing")?
                        .to_string(),
                );
            }
            if codes.is_empty() {
                continue;
            }
            let ru = route_url(&codes)?;
            let routes = match f.get_ok(&ru).await {
                Ok(x) => x,
                Err(e) => {
                    rows.push(issue(
                        src,
                        &ru,
                        format!("ESMO category route lookup unavailable: {e:#}"),
                    ));
                    if limit.is_some_and(|n| rows.len() >= n) {
                        return Ok(rows);
                    }
                    continue;
                }
            };
            let rv: Value = match serde_json::from_str(&routes.text()) {
                Ok(v) => v,
                Err(e) => {
                    rows.push(issue(
                        src,
                        &ru,
                        format!("ESMO category route JSON invalid: {e}"),
                    ));
                    if limit.is_some_and(|n| rows.len() >= n) {
                        return Ok(rows);
                    }
                    continue;
                }
            };
            for c in batch {
                if c["collection"] != "default" {
                    continue;
                }
                let code = c["codename"]
                    .as_str()
                    .context("ESMO category codename missing")?;
                let category = match resolved_route(&rv, code) {
                    Ok(u) => u,
                    Err(e) => {
                        rows.push(issue(
                            src,
                            &format!("{ru}#{code}"),
                            format!("ESMO source category route unresolved: {e:#}"),
                        ));
                        if limit.is_some_and(|n| rows.len() >= n) {
                            return Ok(rows);
                        }
                        continue;
                    }
                };
                let cp = match f.get_ok(&category).await {
                    Ok(x) => x,
                    Err(e) => {
                        rows.push(issue(
                            src,
                            &category,
                            format!("ESMO source category unavailable: {e:#}"),
                        ));
                        if limit.is_some_and(|n| rows.len() >= n) {
                            return Ok(rows);
                        }
                        continue;
                    }
                };
                if Url::parse(&cp.record.final_url)?.origin() != Url::parse(&category)?.origin() {
                    rows.push(issue(
                        src,
                        &category,
                        "ESMO category redirected off origin".into(),
                    ));
                    if limit.is_some_and(|n| rows.len() >= n) {
                        return Ok(rows);
                    }
                    continue;
                }
                let queries = match source_queries(&cp.text()) {
                    Ok(q) => q,
                    Err(e) => {
                        rows.push(issue(
                            src,
                            &category,
                            format!("ESMO source category query unresolved: {e:#}"),
                        ));
                        if limit.is_some_and(|n| rows.len() >= n) {
                            return Ok(rows);
                        }
                        continue;
                    }
                };
                for q in queries {
                    let query = q["query"].as_str().context("ESMO source query missing")?;
                    let first = match query_url(query) {
                        Ok(u) => u,
                        Err(e) => {
                            rows.push(issue(
                                src,
                                &category,
                                format!("ESMO source query unsupported: {e:#}"),
                            ));
                            if limit.is_some_and(|n| rows.len() >= n) {
                                return Ok(rows);
                            }
                            continue;
                        }
                    };
                    let mut next = Some(first.clone());
                    let mut seen_pages = HashSet::new();
                    let mut seen_items = HashSet::new();
                    let mut expected_skip = 0usize;
                    let mut expected_total = None;
                    while let Some(url) = next.take() {
                        if seen_pages.len() >= MAX_PAGES || !seen_pages.insert(url.clone()) {
                            rows.push(issue(
                                src,
                                &url,
                                "ESMO source dataview page repeated or exceeded page bound".into(),
                            ));
                            if limit.is_some_and(|n| rows.len() >= n) {
                                return Ok(rows);
                            }
                            break;
                        }
                        let data = match f.get_ok(&url).await {
                            Ok(x) => x,
                            Err(e) => {
                                rows.push(issue(
                                    src,
                                    &url,
                                    format!("ESMO source dataview unavailable: {e:#}"),
                                ));
                                if limit.is_some_and(|n| rows.len() >= n) {
                                    return Ok(rows);
                                }
                                break;
                            }
                        };
                        let (items, skip, total, source_next) = match data_page(&data.text()) {
                            Ok(x) => x,
                            Err(e) => {
                                rows.push(issue(
                                    src,
                                    &url,
                                    format!("ESMO source dataview unsupported: {e:#}"),
                                ));
                                if limit.is_some_and(|n| rows.len() >= n) {
                                    return Ok(rows);
                                }
                                break;
                            }
                        };
                        if skip != expected_skip || expected_total.is_some_and(|t| t != total) {
                            rows.push(issue(
                                src,
                                &url,
                                "ESMO dataview skip or total changed".into(),
                            ));
                            if limit.is_some_and(|n| rows.len() >= n) {
                                return Ok(rows);
                            }
                            break;
                        }
                        expected_total = Some(total);
                        expected_skip = skip + items.len();
                        let mut item_codes = vec![];
                        for x in &items {
                            let code = x["system"]["codename"]
                                .as_str()
                                .context("ESMO item codename missing")?;
                            if x["system"]["collection"] != "default"
                                || !code.bytes().all(|b| {
                                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
                                })
                            {
                                continue;
                            }
                            item_codes.push(code.to_string());
                        }
                        let mut item_routes = HashMap::new();
                        let mut route_error = None;
                        for chunk in item_codes.chunks(50) {
                            let item_ru = route_url(chunk)?;
                            match f.get_ok(&item_ru).await {
                                Ok(r) => match serde_json::from_str::<Value>(&r.text()) {
                                    Ok(v) => {
                                        for code in chunk {
                                            match resolved_route(&v, code) {
                                                Ok(u) => {
                                                    item_routes.insert(
                                                        code.clone(),
                                                        (u, r.record.clone()),
                                                    );
                                                }
                                                Err(e) => {
                                                    route_error = Some(format!(
                                                        "item route unresolved for {code}: {e:#}"
                                                    ))
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        route_error =
                                            Some(format!("item route response invalid: {e}"))
                                    }
                                },
                                Err(e) => {
                                    route_error =
                                        Some(format!("item route lookup unavailable: {e:#}"))
                                }
                            }
                        }
                        let mut ids: HashMap<String, Value> = items
                            .iter()
                            .filter_map(|x| Some((x["system"]["id"].as_str()?.to_string(), publisher_ids(x))))
                            .collect();
                        ids.retain(|id, _| !row_of_item.contains_key(id));
                        resolve_piis(f, &mut ids).await;
                        for item in items {
                            let id = item["system"]["id"]
                                .as_str()
                                .context("ESMO item ID missing")?;
                            if let Some(&i) = row_of_item.get(id) {
                                // Same source item listed under another category/query: one row.
                                if let Some(a) = rows[i].extra["also_listed_in"].as_array_mut() {
                                    a.push(json!({"category":category,"query":query}));
                                }
                                seen_items.insert(id.to_string());
                                continue;
                            }
                            if !seen_items.insert(id.to_string()) {
                                rows.push(issue(
                                    src,
                                    &url,
                                    "ESMO dataview repeated source item".into(),
                                ));
                                if limit.is_some_and(|n| rows.len() >= n) {
                                    return Ok(rows);
                                }
                                continue;
                            }
                            let (title, hash, _, _, _) =
                                match dataview_item(&item, &Url::parse(&category)?) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        rows.push(issue(
                                            src,
                                            &format!("{url}#item-{id}"),
                                            format!("ESMO item template unsupported: {e:#}"),
                                        ));
                                        if limit.is_some_and(|n| rows.len() >= n) {
                                            return Ok(rows);
                                        }
                                        continue;
                                    }
                                };
                            let codename = item["system"]["codename"].as_str().unwrap_or("");
                            let resolved = item_routes.get(codename);
                            let public = resolved.map(|(u, _)| u.clone());
                            let item_route_record = resolved.map(|(_, r)| r.clone());
                            let mut source_locator = Url::parse(&url)?;
                            source_locator.set_fragment(Some(&format!("item-{id}")));
                            let item_url =
                                public.clone().unwrap_or_else(|| source_locator.to_string());
                            let key = common::short_hash(&format!("{code}:{id}"));
                            let pids = ids.remove(id).unwrap_or_else(|| publisher_ids(&item));
                            row_of_item.insert(id.to_string(), rows.len());
                            rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("esmo-{key}-{}",&hash[..12]),url:item_url,title:Some(title),rendition:Some("json".into()),version:None,extra:json!({"kind":"dataview_description","source_id":id,"clinical_sha256":hash,"source_data_url":url,"source_category_url":category,"source_query":query,"public_route_verified":public.is_some(),"catalog_fetch":cat.record,"catalog_sha256":catalog_hash,"routes_fetch":routes.record,"category_fetch":cp.record,"item_routes_fetch":item_route_record,"doi":pids["doi"],"publisher_ids":pids,"also_listed_in":[]})});
                            if limit.is_some_and(|n| rows.len() >= n) {
                                return Ok(rows);
                            }
                        }
                        if let Some(e) = route_error {
                            rows.push(issue(
                                src,
                                &url,
                                format!("ESMO source item routes unresolved: {e}"),
                            ));
                            if limit.is_some_and(|n| rows.len() >= n) {
                                return Ok(rows);
                            }
                        }
                        if let Some(n) = source_next {
                            match next_page_url(&first, &url, &n) {
                                Ok(u) => next = Some(u),
                                Err(e) => {
                                    rows.push(issue(
                                        src,
                                        &url,
                                        format!("ESMO source next page unsupported: {e:#}"),
                                    ));
                                    if limit.is_some_and(|n| rows.len() >= n) {
                                        return Ok(rows);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(rows)
    }
    async fn extract_site(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(e) = row.extra["discovery_error"].as_str() {
            bail!("ESMO discovery unresolved: {e}")
        }
        let fetch_url = if row.extra["kind"] == "dataview_description" {
            row.extra["source_data_url"]
                .as_str()
                .context("ESMO source data URL missing")?
        } else {
            &row.url
        };
        let p = f.get_ok(fetch_url).await?;
        let b = Url::parse(&p.record.final_url)?;
        if b.origin() != Url::parse(fetch_url)?.origin() {
            bail!("ESMO source origin changed")
        }
        if row.extra["kind"] == "living_clinical_leaf" || row.extra["kind"] == "living_index" {
            living_transport(&p, false)?;
            let (leaf, context_fetches) = living_load(f, &p, &b).await?;
            if row.extra["clinical_sha256"].as_str() != Some(leaf.hash.as_str())
                || row.version != leaf.version
                || row.extra["source_id"].as_str() != Some(leaf.id.as_str())
            {
                bail!("ESMO living semantic snapshot changed since planning")
            }
            let mut dates = vec![];
            for (label, key) in [
                ("Source creation date", "source_creation_date"),
                ("Source sorting date", "source_sorting_date"),
                ("CMS last modified", "source_cms_modified"),
            ] {
                if let Some(value) = leaf.metadata[key].as_str().filter(|v| !v.is_empty()) {
                    dates.push(common::date(label, value, &row.url));
                }
            }
            let mut metadata = leaf.metadata;
            metadata["source_discovery"] = row.extra["discovery_scope"].clone();
            metadata["bounded_sitemap_coverage"] = row.extra["bounded_sitemap_coverage"].clone();
            let mut relations = vec![];
            if row.extra["sitemap_index_fetch"].is_object() {
                relations.push(crate::guidelines::doc::Relation{rel:"related".into(),target:LIVING_SITEMAP.into(),scope:Some("source-declared sitemap inventory membership; not a parent/child clinical hierarchy assertion".into()),evidence_url:Some(LIVING_SITEMAP.into())});
            }
            if let Some(url) = metadata["related_guideline_context"]["url"].as_str() {
                relations.push(crate::guidelines::doc::Relation{rel:"related".into(),target:url.into(),scope:Some("source app.routes current smr matches living_guideline route name; native version context".into()),evidence_url:Some(row.url.clone())});
            }
            return Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:Some(format!("esmo-living-{}",leaf.id)),title:leaf.title,url:row.url.clone(),item_type:Some(leaf.item_type.into()),version:leaf.version,status:"unknown".into(),date_observations:dates,
        rights:Rights{status:"unknown".into(),note:Some("ESMO reserves rights; living page and linked supporting assets require independent reuse assessment.".into()),..Default::default()},
        blocks:leaf.blocks,attachments:leaf.assets,search_metadata:metadata,warnings:leaf.warnings,fetches:{let mut records=vec![];for key in ["living_index_fetch","sitemap_index_fetch","sitemap_shard_fetch"]{if let Ok(record)=serde_json::from_value(row.extra[key].clone()){records.push(record)}}records.extend(context_fetches);records.push(p.record);records},relations,..Default::default()});
        }
        if row.extra["kind"] == "source_index" {
            let (hash, metadata) = catalog_inventory(&p.text())?;
            if row.extra["clinical_sha256"]
                .as_str()
                .is_some_and(|s| s != hash)
            {
                bail!("ESMO catalog state changed since discovery")
            }
            let blocks = metadata["source_categories"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, x)| Block {
                    block_id: format!("esmo-source-item-{i}"),
                    kind: BlockKind::ListItem,
                    text: x["title"].as_str().unwrap_or("").into(),
                    heading_path: vec!["ESMO source catalog".into()],
                    page: None,
                })
                .collect();
            return Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some("guideline_catalog_index".into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),..Default::default()},search_metadata:metadata,blocks,fetches:vec![p.record],warnings:vec!["Catalog CMS paths are source inventory fields, not verified public routes; other category routing, publisher terminals and complete enumeration remain unresolved. No publication-terms acknowledgment was accepted.".into()],..Default::default()});
        }
        if row.extra["kind"] == "dataview_description" {
            let id = row.extra["source_id"]
                .as_str()
                .context("ESMO source item ID missing")?;
            let items = dataview(&p.text())?;
            let item = items
                .iter()
                .find(|x| x["system"]["id"] == id)
                .context("ESMO source item disappeared")?;
            let (title, hash, blocks, attachments, mut metadata) = dataview_item(
                item,
                &Url::parse(
                    row.extra["source_category_url"]
                        .as_str()
                        .context("ESMO category URL missing")?,
                )?,
            )?;
            metadata["source_category_url"] = row.extra["source_category_url"].clone();
            metadata["source_query"] = row.extra["source_query"].clone();
            metadata["public_route_verified"] = row.extra["public_route_verified"].clone();
            if row.extra["clinical_sha256"]
                .as_str()
                .is_some_and(|h| h != hash)
            {
                bail!("ESMO source item changed since discovery")
            }
            let mut dates = vec![];
            for (label, key) in [
                ("Creation date", "source_creation_date"),
                ("Sorting date", "source_sorting_date"),
            ] {
                if let Some(value) = metadata[key].as_str().filter(|s| !s.is_empty()) {
                    dates.push(common::date(label, value, fetch_url));
                }
            }
            let mut fetches = vec![];
            for key in [
                "catalog_fetch",
                "routes_fetch",
                "category_fetch",
                "item_routes_fetch",
            ] {
                if let Ok(r) = serde_json::from_value::<crate::guidelines::doc::FetchRecord>(
                    row.extra[key].clone(),
                ) {
                    fetches.push(r)
                }
            }
            fetches.push(p.record);
            let mut warnings=vec!["This is the source dataview description for one item, not its full clinical guideline. The source topic displays an unaccepted publication notice; no acceptance or publisher-access claim is made. Publisher article access and reuse rights remain independently unresolved.".into()];
            if row.extra["public_route_verified"] != true {
                warnings.push("Source item route unresolved; URL identifies the item in its source API response.".into());
            }
            // Journal identity of the full guideline (stated doi.org link, or Crossref-resolved PII).
            let ids = row.extra["publisher_ids"].clone();
            let doi = ids["doi"].as_str().map(String::from);
            let mut relations = vec![];
            if let Some(d) = &doi {
                relations.push(crate::guidelines::doc::Relation{rel:"related".into(),target:format!("doi:{d}"),
                    scope:Some("journal publication of this ESMO guideline (full text); this record holds ESMO's description and citation only".into()),
                    evidence_url:ids["article_url"].as_str().map(String::from)});
                warnings.push(format!("Full guideline text is the journal article doi:{d} ({}); open-access full text, where it exists, is resolved separately (PMC crosswalk).",ids["doi_source"].as_str().unwrap_or("source")));
            } else if ids.is_object() {
                warnings.push(format!("Journal DOI unresolved: {}",ids["doi_lookup_error"].as_str().unwrap_or("source links carry no DOI or PII")));
            }
            if let Ok(r) = serde_json::from_value::<crate::guidelines::doc::FetchRecord>(ids["crossref_fetch"].clone()) {
                fetches.insert(fetches.len() - 1, r);
            }
            if ids.is_object() {
                metadata["publisher_ids"] = ids;
            }
            if row.extra["also_listed_in"].as_array().is_some_and(|a| !a.is_empty()) {
                metadata["also_listed_in"] = row.extra["also_listed_in"].clone();
            }
            return Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:Some(format!("esmo-{id}")),title,url:row.url.clone(),item_type:Some("guideline_description".into()),status:"unknown".into(),date_observations:dates,rights:Rights{status:"unknown".into(),note:Some("Source record and linked publisher article have independent unverified rights.".into()),..Default::default()},search_metadata:metadata,blocks,attachments,fetches,warnings,doi,relations,..Default::default()});
        }
        let (title, hash, blocks, attachments, metadata) = topic(&p.text(), &b)?;
        if row.extra["clinical_sha256"]
            .as_str()
            .is_some_and(|x| x != hash)
        {
            bail!("ESMO state changed since discovery")
        }
        let mut fetches = vec![];
        if let Ok(r) = serde_json::from_value::<crate::guidelines::doc::FetchRecord>(
            row.extra["catalog_fetch"].clone(),
        ) {
            fetches.push(r)
        }
        fetches.push(p.record);
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),title,url:row.url.clone(),item_type:Some("guideline_description".into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),note:Some("ESMO record, publisher article and related resources require independent rights assessment.".into()),..Default::default()},search_metadata:metadata,blocks,attachments,fetches,warnings:vec!["This independently evidenced topic is a source description, not its full clinical article. The rendered topic displays an unaccepted publication-terms acknowledgment; its implications and publisher access remain unverified. The catalog topic hierarchy, related adaptation and full enumeration remain unverified.".into()],..Default::default()})
    }
}

/// R2a-crosswalk: one site row plus the PMC crosswalk row for its journal DOI (`extra.doi`).
async fn crosswalk_group(src: &SourceEntry, f: &Fetcher, mut row: PlanRow, seen: &mut HashSet<String>) -> (PlanRow, Vec<PlanRow>) {
    let (doi, pmcid) = cw::ids_from_extra(&row.extra);
    let lookups = r2a::single(doi.as_deref(), pmcid.as_deref());
    if row.extra["kind"] != "dataview_description" || row.extra["discovery_error"].is_string() || lookups.is_empty() {
        return (row, vec![]);
    }
    let ids = &row.extra["publisher_ids"];
    // The article link ESMO printed (doi.org or the Elsevier PII URL) is kept as the publisher link.
    let linked = ids["article_url"].as_str().filter(|s| !s.is_empty()).map(String::from)
        .or_else(|| doi.as_ref().map(|d| format!("https://doi.org/{d}"))).unwrap_or_default();
    let route = if ids["doi_source"].is_string() { "source_doi" } else { "source_pii_crossref_doi" };
    // No source_title: ESMO's item title is its own label, not the article's link text (the DOI is the identity).
    let sref = cw::SourceRef { source_url: linked, evidence_url: Some(row.url.clone()), source_title: None,
        item_type: Some("guideline".into()), key_prefix: r2a::key_prefix(src), id_route: route.into(), notes: vec![] };
    let site = r2a::SiteRef { doc_key: Some(row.doc_key.clone()), url: row.url.clone() };
    let o = r2a::adopt(src, f, &lookups, &sref, &site, &json!({"esmo_item_id": row.extra["source_id"]}), seen).await;
    o.record(&mut row.extra);
    (row, o.rows)
}

#[async_trait::async_trait]
impl Adapter for Esmo {
    async fn discover(&self, src: &SourceEntry, f: &Fetcher, limit: Option<usize>) -> Result<Vec<PlanRow>> {
        let rows = self.discover_site(src, f, limit).await?;
        let mut seen = HashSet::new();
        let mut groups = vec![];
        for row in rows {
            groups.push(crosswalk_group(src, f, row, &mut seen).await);
        }
        Ok(r2a::interleave(groups, limit))
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if cw::is_crosswalk_row(row) {
            return r2a::extract(src, f, row).await;
        }
        let mut d = self.extract_site(src, f, row).await?;
        r2a::annotate(&mut d, src, &row.extra);
        Ok(d)
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    #[test]
    fn genuine_nuxt_record_is_description_with_separate_publisher_links() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/early-breast.html"
        ));
        let u = Url::parse(TOPIC).unwrap();
        let (title, _, blocks, links, m) = topic(raw, &u).unwrap();
        assert!(title.contains("Early Breast Cancer"));
        assert!(blocks
            .iter()
            .any(|b| b.text.contains("key recommendations and algorithms")));
        assert!(links.iter().any(
            |x| x["role"] == "publisher_pdf" && x["url"].as_str().unwrap().contains("showPdf")
        ));
        assert!(links
            .iter()
            .all(|x| !x["url"].as_str().unwrap().contains("data.esmo.org")));
        assert_eq!(m["source_creation_date"], "2023-12-13T00:00:00Z");
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod catalog_inventory_tests {
    use super::*;
    #[test]
    fn real_embedded_catalog_preserves_internal_paths_without_guessing_public_routes() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/catalog.html"
        ));
        let (_, m) = catalog_inventory(raw).unwrap();
        let items = m["source_items"].as_array().unwrap();
        assert!(items.iter().any(|i|i["title"]=="Breast Cancer"&&i["source_cms_path"]=="esmo/guidelines/guidelines-by-topic/esmo-clinical-practice-guidelines-breast-cancer"));
        assert!(items.iter().all(|i| i.get("url").is_none()));
    }
    #[tokio::test]
    async fn catalog_inventory_and_topic_obey_limits() {
        let cat = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/catalog.html"
        ));
        let topic = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/early-breast.html"
        ));
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 5 << 20, "").unwrap();
        let u = "https://www.esmo.org/guidelines/";
        f.seed_cache_for_tests(u, cat.as_bytes(), "text/html");
        f.seed_cache_for_tests(TOPIC, topic.as_bytes(), "text/html");
        let src: SourceEntry = serde_json::from_value(
            json!({"id":"esmo","access":"public","enabled":true,"catalog_url":u}),
        )
        .unwrap();
        // The catalog inventory is no longer its own document; with no category routes cached the
        // only row is the recorded route-lookup issue.
        for (n, want) in [(0, 0), (1, 1), (2, 1), (3, 1)] {
            assert_eq!(Esmo.discover(&src, &f, Some(n)).await.unwrap().len(), want);
        }
        let rows = Esmo.discover(&src, &f, None).await.unwrap();
        assert!(rows.iter().all(|r| r.extra["kind"] != "source_index"));
        assert!(rows[0].extra["discovery_error"].as_str().unwrap().contains("route lookup unavailable"));
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod genuine_dataview_tests {
    use super::*;
    use std::io::Read;
    fn source_data() -> String {
        let mut s = String::new();
        flate2::read::GzDecoder::new(
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/guidelines/esmo/breast-dataview.json.gz"
            ))
            .as_slice(),
        )
        .read_to_string(&mut s)
        .unwrap();
        s
    }
    #[test]
    fn exact_route_and_four_source_items_keep_native_metadata() {
        let routes = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/category-routes.json"
        ));
        let category = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/breast-category.html"
        ));
        assert_eq!(category_route(routes).unwrap(), CATEGORY);
        assert_eq!(other_category_route(routes).unwrap(),"https://www.esmo.org/guidelines/esmo-clinical-practice-guidelines-gastrointestinal-cancers");
        assert!(category.contains(SOURCE_QUERY));
        let items = dataview(&source_data()).unwrap();
        assert_eq!(items.len(), 4);
        let early = items
            .iter()
            .find(|x| x["system"]["codename"] == "early_breast_cancer")
            .unwrap();
        assert_eq!(
            early["system"]["id"],
            "f9fbaea0-f0e3-5d65-a509-5e4bb88dfd0b"
        );
        let (title, _, _, links, m) = dataview_item(early, &Url::parse(CATEGORY).unwrap()).unwrap();
        assert!(title.contains("Early Breast Cancer"));
        assert!(m["source_citation_html"]
            .as_str()
            .unwrap()
            .contains("Ann Oncol"));
        assert!(m["source_creation_date"]
            .as_str()
            .unwrap()
            .starts_with("2023-12-13"));
        assert!(m["source_product_roles"]
            .to_string()
            .contains("ESMO Clinical Practice Guideline"));
        assert!(links.iter().any(
            |a| a["role"] == "publisher_pdf" && a["url"].as_str().unwrap().contains("showPdf")
        ));
        assert!(items
            .iter()
            .any(|x| x["system"]["codename"] == "metastatic_breast_cancer"));
    }
    #[test]
    fn source_inventory_contains_all_fifteen_categories() {
        let cat = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/catalog.html"
        ));
        let categories = source_categories(cat).unwrap();
        assert_eq!(categories.len(), 15);
        assert!(categories
            .iter()
            .any(|x| x["codename"] == "_gl_topic_covid_19_and_cancer"));
        let category = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/breast-category.html"
        ));
        assert!(source_queries(category)
            .unwrap()
            .iter()
            .any(|q| q["query"] == SOURCE_QUERY));
    }
    #[tokio::test]
    async fn genuine_dataview_extract_preserves_item_identity_and_revision() {
        let data = source_data();
        let item = dataview(&data)
            .unwrap()
            .into_iter()
            .find(|x| x["system"]["codename"] == "early_breast_cancer")
            .unwrap();
        let (title, hash, _, _, _) = dataview_item(&item, &Url::parse(CATEGORY).unwrap()).unwrap();
        let url = query_url(SOURCE_QUERY).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 5 << 20, "").unwrap();
        f.seed_cache_for_tests(&url, data.as_bytes(), "application/json");
        let src:SourceEntry=serde_json::from_value(json!({"id":"esmo","access":"public","enabled":true,"catalog_url":"https://www.esmo.org/guidelines/"})).unwrap();
        let mut row = PlanRow {
            source_id: "esmo".into(),
            doc_key: "source-item".into(),
            url: format!("{url}#item-f9fbaea0-f0e3-5d65-a509-5e4bb88dfd0b"),
            title: Some(title),
            rendition: Some("json".into()),
            version: None,
            extra: json!({"kind":"dataview_description","source_id":"f9fbaea0-f0e3-5d65-a509-5e4bb88dfd0b","clinical_sha256":hash,"source_data_url":url,"source_category_url":CATEGORY,"source_query":SOURCE_QUERY,"public_route_verified":false}),
        };
        let doc = Esmo.extract(&src, &f, &row).await.unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("guideline_description"));
        assert_eq!(
            doc.search_metadata["source_id"],
            "f9fbaea0-f0e3-5d65-a509-5e4bb88dfd0b"
        );
        assert!(doc.warnings.iter().any(|x| x.contains("route unresolved")));
        row.extra["clinical_sha256"] = json!("different-revision");
        assert!(Esmo
            .extract(&src, &f, &row)
            .await
            .unwrap_err()
            .to_string()
            .contains("changed since discovery"));
    }

    #[tokio::test]
    async fn genuine_catalog_to_item_routes_and_extraction_obey_limits() {
        let cat = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/catalog.html"
        ));
        let all_routes = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/all-category-routes.json"
        ));
        let item_routes = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/breast-item-routes.json"
        ));
        let breast = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/breast-category.html"
        ));
        let gi = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/esmo/gi-category.html"
        ));
        let categories = source_categories(cat).unwrap();
        assert_eq!(categories.len(), 15);
        let codes = categories
            .iter()
            .map(|x| x["codename"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        let category_url = route_url(&codes).unwrap();
        let routes: Value = serde_json::from_str(all_routes).unwrap();
        assert_eq!(resolved_route(&routes, &codes[0]).unwrap(), CATEGORY);
        let gi_url = resolved_route(&routes, "gastrointestinal_cancers_274bf32").unwrap();
        assert_eq!(gi_url,"https://www.esmo.org/guidelines/esmo-clinical-practice-guidelines-gastrointestinal-cancers");
        assert!(!source_queries(gi).unwrap().is_empty());
        let queries = source_queries(breast).unwrap();
        assert_eq!(queries[0]["query"], SOURCE_QUERY);
        let data_url = query_url(SOURCE_QUERY).unwrap();
        let data = source_data();
        let items = dataview(&data).unwrap();
        let item_codes = items
            .iter()
            .map(|x| x["system"]["codename"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        let item_route_url = route_url(&item_codes).unwrap();
        let ir: Value = serde_json::from_str(item_routes).unwrap();
        assert_eq!(resolved_route(&ir, "early_breast_cancer").unwrap(), TOPIC);
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 5 << 20, "").unwrap();
        let u = "https://www.esmo.org/guidelines/";
        f.seed_cache_for_tests(u, cat.as_bytes(), "text/html");
        f.seed_cache_for_tests(&category_url, all_routes.as_bytes(), "application/json");
        f.seed_cache_for_tests(CATEGORY, breast.as_bytes(), "text/html");
        f.seed_cache_for_tests(&data_url, data.as_bytes(), "application/json");
        f.seed_cache_for_tests(&item_route_url, item_routes.as_bytes(), "application/json");
        f.seed_cache_for_tests(&gi_url, gi.as_bytes(), "text/html");
        let src: SourceEntry = serde_json::from_value(
            json!({"id":"esmo","access":"public","enabled":true,"catalog_url":u}),
        )
        .unwrap();
        for n in [0, 1, 3, 5] {
            assert_eq!(Esmo.discover(&src, &f, Some(n)).await.unwrap().len(), n);
        }
        let rows = Esmo.discover(&src, &f, Some(5)).await.unwrap();
        let mut row = rows
            .into_iter()
            .find(|r| r.extra["source_id"] == "f9fbaea0-f0e3-5d65-a509-5e4bb88dfd0b")
            .unwrap();
        assert_eq!(row.url, TOPIC);
        assert_eq!(row.extra["public_route_verified"], true);
        let doc = Esmo.extract(&src, &f, &row).await.unwrap();
        assert_eq!(doc.url, TOPIC);
        assert_eq!(doc.item_type.as_deref(), Some("guideline_description"));
        assert_eq!(doc.fetches.len(), 5);
        assert_eq!(
            doc.search_metadata["source_id"],
            "f9fbaea0-f0e3-5d65-a509-5e4bb88dfd0b"
        );
        assert!(doc.attachments.iter().any(|x| x["role"] == "publisher_pdf"));
        row.extra["clinical_sha256"] = json!("changed");
        assert!(Esmo
            .extract(&src, &f, &row)
            .await
            .unwrap_err()
            .to_string()
            .contains("changed since discovery"));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod journal_id_tests {
    use super::*;
    use std::io::Read;
    const CROSSREF: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/esmo/crossref-breast-piis.json"));
    fn source_data() -> String {
        let mut s = String::new();
        flate2::read::GzDecoder::new(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/esmo/breast-dataview.json.gz")).as_slice())
            .read_to_string(&mut s).unwrap();
        s
    }
    #[test]
    fn stated_doi_links_and_piis_are_read_never_derived() {
        assert_eq!(doi_from_url("https://doi.org/10.1093/annonc/mdy152").as_deref(), Some("10.1093/annonc/mdy152"));
        assert_eq!(doi_from_url("https://doi.org/10.1016/j.annonc.2020.07.019").as_deref(), Some("10.1016/j.annonc.2020.07.019"));
        assert!(doi_from_url("https://www.annalsofoncology.org/article/S0923-7534(23)05104-9/fulltext").is_none());
        assert!(doi_from_url("https://example.org/10.1000/x").is_none());
        let item = json!({"elements":{"article_url":{"value":"https://www.esmoopen.com/article/S2059-7029(24)01892-1/fulltext"},"article_pdf":{"value":"https://www.esmoopen.com/action/showPdf?pii=S2059-7029%2824%2901892-1"}}});
        let ids = publisher_ids(&item);
        assert_eq!(ids["pii"], "S2059-7029(24)01892-1");
        assert_eq!(ids["pii_compact"], "S2059702924018921");
        assert!(ids["doi"].is_null());
        let pdf_only = json!({"elements":{"article_url":{"value":"https://doi.org/10.1093/annonc/mdx216"},"article_pdf":{"value":"https://www.annalsofoncology.org/action/showPdf?pii=S0923-7534%2819%2942146-7"}}});
        let ids = publisher_ids(&pdf_only);
        assert_eq!(ids["doi"], "10.1093/annonc/mdx216");
        assert_eq!(ids["pii"], "S0923-7534(19)42146-7");
    }
    #[test]
    fn crossref_exact_match_only_and_ambiguity_is_unresolved() {
        let map = crossref_dois(CROSSREF).unwrap();
        assert_eq!(map["S0923753423051049"]["doi"], "10.1016/j.annonc.2023.11.016");
        assert_eq!(map["S092375342204193X"]["doi"], "10.1016/j.annonc.2022.10.004");
        assert!(map.get("S0923753423051040").is_none());
        let mut v: Value = serde_json::from_str(CROSSREF).unwrap();
        v["message"]["items"][1]["alternative-id"] = v["message"]["items"][0]["alternative-id"].clone();
        let amb = crossref_dois(&v.to_string()).unwrap();
        assert!(amb.values().any(|x| x["ambiguous"] == true));
        v["message"]["total-results"] = json!(50);
        assert!(crossref_dois(&v.to_string()).is_err());
    }
    #[tokio::test]
    async fn dataview_rows_carry_crossref_dois_and_extract_relates_the_article() {
        let cat = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/esmo/catalog.html"));
        let all_routes = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/esmo/all-category-routes.json"));
        let item_routes = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/esmo/breast-item-routes.json"));
        let breast = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/esmo/breast-category.html"));
        let codes = source_categories(cat).unwrap().iter().map(|x| x["codename"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let data = source_data();
        let items = dataview(&data).unwrap();
        let item_codes = items.iter().map(|x| x["system"]["codename"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let mut piis = items.iter().filter_map(|x| publisher_ids(x)["pii_compact"].as_str().map(String::from)).collect::<Vec<_>>();
        piis.sort();
        piis.dedup();
        assert_eq!(piis.len(), 4);
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 5 << 20, "").unwrap();
        let u = "https://www.esmo.org/guidelines/";
        f.seed_cache_for_tests(u, cat.as_bytes(), "text/html");
        f.seed_cache_for_tests(&route_url(&codes).unwrap(), all_routes.as_bytes(), "application/json");
        f.seed_cache_for_tests(CATEGORY, breast.as_bytes(), "text/html");
        f.seed_cache_for_tests(&query_url(SOURCE_QUERY).unwrap(), data.as_bytes(), "application/json");
        f.seed_cache_for_tests(&route_url(&item_codes).unwrap(), item_routes.as_bytes(), "application/json");
        f.seed_cache_for_tests(&crossref_url(&piis).unwrap(), CROSSREF.as_bytes(), "application/json");
        let src: SourceEntry = serde_json::from_value(json!({"id":"esmo","access":"public","enabled":true,"catalog_url":u})).unwrap();
        let rows = Esmo.discover(&src, &f, Some(4)).await.unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.extra["kind"] == "dataview_description" && r.extra["catalog_sha256"].is_string()));
        let early = rows.iter().find(|r| r.extra["source_id"] == "f9fbaea0-f0e3-5d65-a509-5e4bb88dfd0b").unwrap();
        assert_eq!(early.extra["doi"], "10.1016/j.annonc.2023.11.016");
        assert_eq!(early.extra["publisher_ids"]["pii"], "S0923-7534(23)05104-9");
        assert!(early.extra["publisher_ids"]["doi_source"].as_str().unwrap().contains("Crossref"));
        assert!(rows.iter().all(|r| r.extra["doi"].as_str().is_some_and(|d| d.starts_with("10.1016/j.annonc."))));
        let doc = Esmo.extract(&src, &f, early).await.unwrap();
        assert_eq!(doc.doi.as_deref(), Some("10.1016/j.annonc.2023.11.016"));
        assert!(doc.relations.iter().any(|r| r.rel == "related" && r.target == "doi:10.1016/j.annonc.2023.11.016"));
        assert!(doc.fetches.iter().any(|x| x.url.starts_with("https://api.crossref.org/works?")));
        assert_eq!(doc.search_metadata["publisher_ids"]["crossref_record"]["vor_license_urls"][0], "http://www.elsevier.com/open-access/userlicense/1.0/");
        assert!(doc.warnings.iter().any(|w| w.contains("doi:10.1016/j.annonc.2023.11.016")));
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod living_tests {
    use super::*;
    const DIAGNOSTIC: &str =
        include_str!("../../../tests/fixtures/guidelines/esmo/living-diagnostic-synthetic.html");
    const TREATMENT: &str =
        include_str!("../../../tests/fixtures/guidelines/esmo/living-treatment-synthetic.html");
    fn leaf_url() -> Url {
        Url::parse(
            "https://www.esmo.org/guidelines/living-guidelines/synthetic-parent/synthetic-section",
        )
        .unwrap()
    }
    fn pairs(metadata: &Value) -> usize {
        metadata["source_list_tree"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|item| item["native_occurrences"].as_array().unwrap())
            .filter(|grade| grade["native_evidence_level"].is_string())
            .count()
    }

    fn modified_source(content: Option<&str>, parent: bool, version: Option<&str>) -> String {
        let mut state = living_state(DIAGNOSTIC).unwrap();
        let a = state.as_array_mut().unwrap();
        let item = a
            .iter()
            .position(|item| {
                item.get("system")
                    .and_then(|r| living_ref(a, r).ok())
                    .and_then(|s| s.get("type"))
                    .and_then(|r| living_ref(a, r).ok())
                    .and_then(Value::as_str)
                    == Some("living_guideline_section")
            })
            .unwrap();
        let elements = a[item]["elements"].as_u64().unwrap() as usize;
        if let Some(html) = content {
            let field = a[elements]["content"].as_u64().unwrap() as usize;
            let value = a[field]["value"].as_u64().unwrap() as usize;
            a[value] = json!(html);
        }
        if parent {
            let system = a[item]["system"].as_u64().unwrap() as usize;
            let kind = a[system]["type"].as_u64().unwrap() as usize;
            a[kind] = json!("living_guideline");
        }
        if let Some(version) = version {
            let value = a.len();
            a.push(json!(version));
            let field = a.len();
            a.push(json!({"value":value}));
            a[elements]["version"] = json!(field);
        }
        format!(
            "<script id='__NUXT_DATA__' type='application/json'>{}</script>",
            serde_json::to_string(a).unwrap()
        )
    }

    fn with_source_routes() -> String {
        let mut state = living_state(DIAGNOSTIC).unwrap();
        let a = state.as_array_mut().unwrap();
        let system = a
            .iter()
            .position(|v| v.get("type").and_then(Value::as_u64) == Some(1))
            .unwrap();
        let saved_type = a.len();
        a.push(a[1].clone());
        a[system]["type"] = json!(saved_type);
        fn add(a: &mut Vec<Value>, v: Value) -> usize {
            let i = a.len();
            a.push(v);
            i
        }
        let path = add(a, json!(leaf_url().path()));
        let leaf_name = add(a, json!("synthetic-leaf-route"));
        let parent_name = add(a, json!("synthetic_section"));
        let parent_path = add(
            a,
            json!("/guidelines/living-guidelines/exact-emitted-context"),
        );
        let parent_kind = add(a, json!("living_guideline"));
        let leaf_kind = add(a, json!("living_guideline_section"));
        let leaf = add(
            a,
            json!({"path":path,"name":leaf_name,"type":leaf_kind,"smr":parent_name}),
        );
        let parent = add(
            a,
            json!({"path":parent_path,"name":parent_name,"type":parent_kind,"smr":parent_name}),
        );
        let routes = add(a, json!([leaf, parent]));
        let app = add(a, json!({"routes":routes}));
        let pinia = add(a, json!({"app":app}));
        a[1] = json!({"pinia":pinia});
        format!(
            "<script id='__NUXT_DATA__' type='application/json'>{}</script>",
            serde_json::to_string(a).unwrap()
        )
    }
    #[tokio::test]
    async fn exact_emitted_smr_context_preserves_scoped_version_and_detects_parent_drift() {
        let child = with_source_routes();
        let context = living_route_context(&child, &leaf_url()).unwrap().unwrap();
        assert_eq!(
            context["url"],
            "https://www.esmo.org/guidelines/living-guidelines/exact-emitted-context"
        );
        let parent = modified_source(None, true, Some("v2.0 - August 2026"))
            .replace("synthetic-diagnostic-id", "synthetic-parent-id");
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, 8 << 20, "").unwrap();
        fetcher.seed_cache_for_tests(leaf_url().as_str(), child.as_bytes(), "text/html");
        fetcher.seed_cache_for_tests(
            context["url"].as_str().unwrap(),
            parent.as_bytes(),
            "text/html",
        );
        let source: SourceEntry = serde_json::from_value(
            json!({"id":"esmo","access":"public","enabled":true,"catalog_url":leaf_url().as_str()}),
        )
        .unwrap();
        let rows = Esmo.discover(&source, &fetcher, Some(1)).await.unwrap();
        assert_eq!(rows[0].version.as_deref(), Some("v2.0 - August 2026"));
        let doc = Esmo.extract(&source, &fetcher, &rows[0]).await.unwrap();
        assert_eq!(doc.fetches.len(), 2);
        assert!(doc.search_metadata["native_version"].is_null());
        assert_eq!(
            doc.search_metadata["related_guideline_context"]["source_record"]["native_version"],
            "v2.0 - August 2026"
        );
        assert!(doc
            .relations
            .iter()
            .any(|r| r.target == context["url"].as_str().unwrap()
                && r.scope.as_deref().unwrap().contains("smr")));
        assert_eq!(doc.status, "unknown");
        fetcher.seed_cache_for_tests(
            context["url"].as_str().unwrap(),
            parent
                .replace("v2.0 - August 2026", "v2.1 - September 2026")
                .as_bytes(),
            "text/html",
        );
        assert!(Esmo
            .extract(&source, &fetcher, &rows[0])
            .await
            .unwrap_err()
            .to_string()
            .contains("snapshot changed"));
    }

    #[tokio::test]
    async fn no_limit_crawl_reaches_clinical_leaf_after_more_than_32_metadata_routes() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, 8 << 20, "").unwrap();
        let root = "https://www.esmo.org/guidelines/living-guidelines";
        let parent = modified_source(None, true, Some("v2.0 - August 2026"));
        fetcher.seed_cache_for_tests(root, parent.as_bytes(), "text/html");
        fetcher.seed_cache_for_tests(LIVING_SITEMAP,b"<sitemapindex><sitemap><loc>https://www.esmo.org/__sitemap__/en.xml</loc></sitemap></sitemapindex>","application/xml");
        let mut xml = String::from("<urlset>");
        for number in 0..40 {
            let url = format!("{root}/synthetic-metadata-{number}");
            xml.push_str(&format!("<url><loc>{url}</loc></url>"));
            fetcher.seed_cache_for_tests(&url, parent.as_bytes(), "text/html");
        }
        xml.push_str(&format!("<url><loc>{}</loc></url></urlset>", leaf_url()));
        fetcher.seed_cache_for_tests(
            "https://www.esmo.org/__sitemap__/en.xml",
            xml.as_bytes(),
            "application/xml",
        );
        fetcher.seed_cache_for_tests(leaf_url().as_str(), DIAGNOSTIC.as_bytes(), "text/html");
        let mut source: SourceEntry = serde_json::from_value(
            json!({"id":"esmo","access":"public","enabled":true,"catalog_url":root}),
        )
        .unwrap();
        let rows = Esmo.discover(&source, &fetcher, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].extra["kind"], "living_clinical_leaf");
        let coverage = &rows[0].extra["bounded_sitemap_coverage"];
        assert_eq!(coverage["visited"], 41);
        assert_eq!(coverage["metadata_pages"], 40);
        assert_eq!(coverage["unvisited"], 0);
        assert_eq!(coverage["sitemap_membership_visited_all"], true);
        assert_eq!(coverage["inventory_complete"], false);
        source.config = json!({"living_max_candidates":64});
        let limited = Esmo.discover(&source, &fetcher, Some(1)).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].extra["bounded_sitemap_coverage"]["visited"], 41);
        source.config = json!({"living_max_candidates":8});
        let error = Esmo
            .discover(&source, &fetcher, Some(1))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("budget_termination") && error.contains("no clinical leaves"));
    }

    fn map_routes_source() -> String {
        let mut state = living_state(&with_source_routes()).unwrap();
        let a = state.as_array_mut().unwrap();
        let pinia = a[1]["pinia"].as_u64().unwrap() as usize;
        let app = a[pinia]["app"].as_u64().unwrap() as usize;
        let routes = a[app]["routes"].as_u64().unwrap() as usize;
        let old = a[routes].as_array().unwrap().clone();
        fn add(a: &mut Vec<Value>, value: Value) -> usize {
            let n = a.len();
            a.push(value);
            n
        }
        let path = add(a, json!("/"));
        let name = add(a, json!("homepage"));
        let kind = add(a, json!("page"));
        let home = add(a, json!({"path":path,"name":name,"type":kind,"smr":-1}));
        let other = add(a, json!({"path":path,"name":name,"type":kind,"smr":-1}));
        let keys = (0..5)
            .map(|n| add(a, json!(format!("source-route-key-{n}"))))
            .collect::<Vec<_>>();
        a[routes] = json!([
            "Map", keys[0], home, keys[1], other, keys[2], old[0], keys[3], old[0], keys[4], old[1]
        ]);
        format!(
            "<script id='__NUXT_DATA__' type='application/json'>{}</script>",
            serde_json::to_string(a).unwrap()
        )
    }
    #[test]
    fn verified_map_routes_skip_header_validate_pairs_and_deduplicate_exact_aliases() {
        let html = map_routes_source();
        let context = living_route_context(&html, &leaf_url()).unwrap().unwrap();
        assert_eq!(
            context["url"],
            "https://www.esmo.org/guidelines/living-guidelines/exact-emitted-context"
        );
        let state = living_state(&html).unwrap();
        let a = state.as_array().unwrap();
        let pinia = living_field(a, &a[1], "pinia").unwrap();
        let app = living_field(a, pinia, "app").unwrap();
        let routes = living_field(a, app, "routes").unwrap();
        let values = living_route_values(a, routes).unwrap();
        assert_eq!(values.len(), 4);
        assert!(values
            .iter()
            .filter(|v| v.get("smr") == Some(&json!(-1)))
            .all(|v| living_field(a, v, "smr").is_err()));
        assert!(living_ref(a, &json!(-1)).is_err()); // Required negative references remain invalid.
        assert!(living_ref(a, &json!("Map")).is_err()); // No global literal/reference relaxation.
    }
    #[test]
    fn malformed_map_and_distinct_duplicate_current_records_remain_errors() {
        let a = vec![json!("key"), json!({"path":0}), json!(7)];
        assert!(living_route_values(&a, &json!(["Map", 0])).is_err());
        assert!(living_route_values(&a, &json!(["Map", -1, 1])).is_err());
        assert!(living_route_values(&a, &json!(["Map", 0, 99])).is_err());
        assert!(living_route_values(&a, &json!(["Map", 0, 2])).is_err());
        let mut state = living_state(&map_routes_source()).unwrap();
        let a = state.as_array_mut().unwrap();
        let pinia = a[1]["pinia"].as_u64().unwrap() as usize;
        let app = a[pinia]["app"].as_u64().unwrap() as usize;
        let routes = a[app]["routes"].as_u64().unwrap() as usize;
        let current = a[routes][6].as_u64().unwrap() as usize;
        let distinct = a.len();
        a.push(a[current].clone());
        a[routes][8] = json!(distinct);
        let html = format!(
            "<script id='__NUXT_DATA__' type='application/json'>{}</script>",
            serde_json::to_string(a).unwrap()
        );
        assert!(living_route_context(&html, &leaf_url())
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
    }
    #[test]
    fn source_context_never_comes_from_url_truncation_or_unrelated_routes() {
        let source = with_source_routes();
        assert!(living_route_context(DIAGNOSTIC, &leaf_url())
            .unwrap()
            .is_none());
        assert!(living_route_context(
            &source.replace("exact-emitted-context", "different-literal-context"),
            &leaf_url()
        )
        .unwrap()
        .unwrap()["url"]
            .as_str()
            .unwrap()
            .ends_with("/different-literal-context"));
        let changed = source.replace("\"smr\":", "\"unrelated_property\":");
        assert!(living_route_context(&changed, &leaf_url())
            .unwrap()
            .is_none());
        assert!(living_route_context(
            &source.replace(
                "/guidelines/living-guidelines/exact-emitted-context",
                "https://example.org/foreign"
            ),
            &leaf_url()
        )
        .is_err());
    }
    #[test]
    fn declared_sitemap_uses_literal_source_routes_and_preserves_discovery_dates_only() {
        let index="<sitemapindex xmlns='http://www.sitemaps.org/schemas/sitemap/0.9'><sitemap><loc>https://www.esmo.org/__sitemap__/en.xml</loc></sitemap></sitemapindex>";
        assert_eq!(living_sitemap(index, true).unwrap().len(), 1);
        let shard="<urlset><url><loc>https://www.esmo.org/guidelines/living-guidelines/synthetic-parent</loc><lastmod>2026-10-01</lastmod></url><url><loc>https://www.esmo.org/guidelines/living-guidelines/synthetic-parent/synthetic-section</loc><lastmod>2026-10-02</lastmod></url><url><loc>https://www.esmo.org/guidelines/other</loc></url></urlset>";
        let routes = living_sitemap(shard, false).unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[1].1.as_deref(), Some("2026-10-02"));
        assert!(living_sitemap(
            &index.replace("/__sitemap__/en.xml", "/__sitemap__/invented.xml"),
            true
        )
        .is_err());
        assert!(living_sitemap(&shard.replace("www.esmo.org", "example.org"), false).is_err());
        let source: SourceEntry =
            serde_json::from_value(json!({"id":"esmo","access":"public","enabled":true})).unwrap();
        let first = living_plan(
            &source,
            &leaf_url(),
            living_leaf(DIAGNOSTIC, &leaf_url()).unwrap(),
            json!({"sitemap_lastmod":"2026-10-01"}),
        );
        let changed = living_plan(
            &source,
            &leaf_url(),
            living_leaf(DIAGNOSTIC, &leaf_url()).unwrap(),
            json!({"sitemap_lastmod":"2026-10-02"}),
        );
        assert_ne!(first.doc_key, changed.doc_key);
        assert_eq!(first.version, None);
    }
    #[tokio::test]
    async fn sitemap_discovery_is_bounded_and_classifies_actual_source_type() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, 8 << 20, "").unwrap();
        let root = "https://www.esmo.org/guidelines/living-guidelines";
        let parent = modified_source(None, true, Some("v2.0 - August 2026"));
        fetcher.seed_cache_for_tests(root, parent.as_bytes(), "text/html");
        fetcher.seed_cache_for_tests(LIVING_SITEMAP,b"<sitemapindex><sitemap><loc>https://www.esmo.org/__sitemap__/en.xml</loc></sitemap></sitemapindex>","application/xml");
        let shard=format!("<urlset><url><loc>{}</loc><lastmod>2026-10-02</lastmod></url><url><loc>https://www.esmo.org/guidelines/living-guidelines/unvisited</loc></url></urlset>",leaf_url());
        fetcher.seed_cache_for_tests(
            "https://www.esmo.org/__sitemap__/en.xml",
            shard.as_bytes(),
            "application/xml",
        );
        fetcher.seed_cache_for_tests(leaf_url().as_str(), DIAGNOSTIC.as_bytes(), "text/html");
        let source:SourceEntry=serde_json::from_value(json!({"id":"esmo","access":"public","enabled":true,"catalog_url":root,"config":{"living_max_candidates":1,"include_living_indexes":true}})).unwrap();
        let rows = Esmo.discover(&source, &fetcher, Some(3)).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].extra["kind"], "living_index");
        assert_eq!(rows[1].extra["kind"], "living_clinical_leaf");
        let doc = Esmo.extract(&source, &fetcher, &rows[1]).await.unwrap();
        assert_eq!(doc.fetches.len(), 4);
        assert_eq!(
            doc.search_metadata["source_discovery"]["sitemap_lastmod"],
            "2026-10-02"
        );
        assert!(doc.version.is_none());
        assert!(doc
            .date_observations
            .iter()
            .all(|date| date.label != "sitemap_lastmod"));
    }
    #[test]
    fn nonbreast_synthetic_boundaries_keep_multiple_lists_and_paragraph_context() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/guidelines/esmo/living-nonbreast-synthetic.json"
        ))
        .unwrap();
        let colorectal = living_leaf(
            &modified_source(fixture["colorectal"].as_str(), false, None),
            &leaf_url(),
        )
        .unwrap();
        assert_eq!(
            colorectal
                .blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            13
        );
        assert_eq!(pairs(&colorectal.metadata), 18);
        let renal = living_leaf(
            &modified_source(fixture["renal"].as_str(), false, None),
            &leaf_url(),
        )
        .unwrap();
        assert_eq!(
            renal
                .blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            7
        );
        assert_eq!(pairs(&renal.metadata), 7);
        assert_eq!(
            renal.blocks[0].text,
            "Initial clinical heading context synthetic."
        );
        assert_eq!(
            renal.blocks[6].text,
            "Later clinical heading context synthetic."
        );
        assert!(matches!(renal.blocks[0].kind, BlockKind::Paragraph));
        assert!(matches!(renal.blocks[6].kind, BlockKind::Paragraph));
    }
    #[test]
    fn parent_index_native_version_is_scoped_to_index_and_never_clinical_content() {
        let raw = modified_source(None, true, Some("v2.0 - August 2026"));
        let parent = living_resource(&raw, &leaf_url()).unwrap();
        assert_eq!(parent.item_type, "living_guideline_index");
        assert_eq!(parent.version.as_deref(), Some("v2.0 - August 2026"));
        assert_eq!(parent.metadata["clinical_content_acquired"], false);
        assert!(!parent
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Recommendation { .. })));
        assert_ne!(
            parent.hash,
            living_resource(
                &raw.replace("v2.0 - August 2026", "v2.1 - September 2026"),
                &leaf_url()
            )
            .unwrap()
            .hash
        );
        assert!(living_leaf(DIAGNOSTIC, &leaf_url())
            .unwrap()
            .version
            .is_none());
    }
    #[test]
    fn diagnostic_native_pairs_keep_multiple_scopes_and_non_grade_brackets() {
        let leaf = living_leaf(DIAGNOSTIC, &leaf_url()).unwrap();
        let recommendations = leaf
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recommendations.len(), 9);
        assert_eq!(pairs(&leaf.metadata), 10);
        assert!(recommendations.iter().all(|b| matches!(
            b.kind,
            BlockKind::Recommendation {
                strength: None,
                certainty: None,
                ..
            }
        )));
        let items = leaf.metadata["source_list_tree"].as_array().unwrap();
        assert_eq!(
            items[3]["native_occurrences"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|g| g["native_evidence_level"].is_string())
                .count(),
            2
        );
        assert!(items[0]["native_occurrences"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["bracket"] == "[ER, PgR, HER2]" && g["native_evidence_level"].is_null()));
        assert!(items[3]["native_occurrences"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["escat_present"] == true && g["native_evidence_level"].is_null()));
        for (index, item) in items.iter().enumerate() {
            for grade in item["native_occurrences"].as_array().unwrap() {
                let start = grade["utf8_byte_start"].as_u64().unwrap() as usize;
                let end = grade["utf8_byte_end"].as_u64().unwrap() as usize;
                assert_eq!(
                    &recommendations[index].text[start..end],
                    grade["bracket"].as_str().unwrap()
                );
            }
        }
        assert!(leaf.version.is_none());
        assert_eq!(leaf.metadata["source_cms_modified"], "2026-08-20T00:00:00Z");
    }
    #[test]
    fn treatment_ungraded_item_nested_clause_and_combined_scores_are_separate() {
        let leaf = living_leaf(TREATMENT, &leaf_url()).unwrap();
        assert_eq!(
            leaf.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            6
        );
        assert_eq!(pairs(&leaf.metadata), 7);
        assert!(leaf.blocks.iter().any(|b| b.block_id == "treatment-5"
            && b.text == "Synthetic ungraded clinical recommendation."));
        let parent = leaf
            .blocks
            .iter()
            .find(|b| b.block_id == "treatment-4")
            .unwrap();
        assert!(!parent.text.contains("Synthetic nested clause"));
        assert!(parent.text.contains("Parent continuation synthetic."));
        let nested = leaf
            .blocks
            .iter()
            .find(|b| b.block_id == "nested-4-1")
            .unwrap();
        assert!(matches!(nested.kind, BlockKind::ListItem));
        let tree = leaf.metadata["source_list_tree"].as_array().unwrap();
        assert_eq!(
            tree.iter().find(|i| i["block_id"] == "nested-4-1").unwrap()["parent_block_id"],
            "treatment-4"
        );
        assert_eq!(
            tree.iter()
                .find(|i| i["block_id"] == "treatment-3")
                .unwrap()["native_occurrences"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|g| g["esmo_mcbs_present"] == true && g["native_evidence_level"] == "I")
                .count(),
            2
        );
        assert!(leaf
            .blocks
            .first()
            .unwrap()
            .text
            .contains("Clinical context before"));
        assert!(leaf
            .blocks
            .iter()
            .any(|b| b.text.contains("Clinical context after")));
        assert_eq!(leaf.assets.len(), 1);
        assert_eq!(leaf.assets[0]["bytes_downloaded"], false);
        assert_ne!(
            leaf.hash,
            living_leaf(
                &TREATMENT.replace("synthetic-scorecard", "changed-scorecard"),
                &leaf_url()
            )
            .unwrap()
            .hash
        );
        assert_eq!(
            leaf.hash,
            living_leaf(
                &TREATMENT.replace("SYNTHETIC-UNRELATED-STATE", "CHANGED-UNRELATED-STATE"),
                &leaf_url()
            )
            .unwrap()
            .hash
        );
    }
    #[test]
    fn reference_indices_are_bounded_and_target_numeric_scalars_are_not_followed() {
        let a = vec![json!(1), json!("not numeric"), json!(["Ref", 0])];
        assert_eq!(living_ref(&a, &json!(0)).unwrap(), &json!(1));
        assert_eq!(living_ref(&a, &json!(2)).unwrap(), &json!(1));
        assert!(living_ref(&a, &json!(99)).is_err());
        let looped = vec![json!(["Ref", 0])];
        assert!(living_ref(&looped, &json!(0)).is_err());
        assert!(living_leaf("<h1>Clinical shell</h1>", &leaf_url()).is_err());
        assert!(living_leaf(
            DIAGNOSTIC,
            &Url::parse("https://example.org/guidelines/living-guidelines/x").unwrap()
        )
        .is_err());
        let mutated = DIAGNOSTIC.replace("living_guideline_section", "esmo_guideline");
        assert!(living_leaf(&mutated, &leaf_url()).is_err());
        let unicode = "É scope [I, A; ESMO-MCBS 4]";
        let occurrences = native_occurrences(unicode);
        assert!(
            occurrences[0]["utf8_byte_start"].as_u64().unwrap()
                > occurrences[0]["unicode_scalar_start"].as_u64().unwrap()
        );
    }
    #[tokio::test]
    async fn explicit_living_leaf_plan_extract_pins_semantics_and_preserves_legacy_role() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, 8 << 20, "").unwrap();
        let url = leaf_url();
        fetcher.seed_cache_for_tests(url.as_str(), TREATMENT.as_bytes(), "text/html");
        let source: SourceEntry = serde_json::from_value(
            json!({"id":"esmo","access":"public","enabled":true,"catalog_url":url.as_str()}),
        )
        .unwrap();
        let rows = Esmo.discover(&source, &fetcher, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].extra["kind"], "living_clinical_leaf");
        let doc = Esmo.extract(&source, &fetcher, &rows[0]).await.unwrap();
        assert_eq!(doc.item_type.as_deref(), Some("living_guideline_section"));
        assert_eq!(doc.status, "unknown");
        assert!(doc.version.is_none());
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            6
        );
        fetcher.seed_cache_for_tests(
            url.as_str(),
            TREATMENT
                .replace("Synthetic treatment six", "Changed treatment six")
                .as_bytes(),
            "text/html",
        );
        assert!(Esmo
            .extract(&source, &fetcher, &rows[0])
            .await
            .unwrap_err()
            .to_string()
            .contains("snapshot changed"));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod r2a_tests {
    use super::*;
    use crate::guidelines::sources::pmc_crosswalk::{self as cw, Lookup};
    fn fx(n: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/guidelines/r2a_crosswalk/{n}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }
    // A description row shaped as `discover_site` emits it (DOI 10.1136/esmoopen-2020-000933, stated as a doi.org
    // link), with the real ID Converter and PMC OAI answers (ESMO Open, CC BY-NC 4.0; refs trimmed).
    #[tokio::test]
    async fn description_row_gains_its_open_access_pmc_copy() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, 8 << 20, "").unwrap();
        let doi = "10.1136/esmoopen-2020-000933";
        f.seed_cache_for_tests(&cw::idconv_url(&Lookup::Doi(doi.into())), &fx("idconv-esmoopen-2020-000933.json"), "application/json");
        f.seed_cache_for_tests(&cw::oai_url("PMC7674109").unwrap(), &fx("oai-PMC7674109.xml"), "application/xml");
        let src: SourceEntry = serde_json::from_value(json!({"id":"esmo","access":"public","enabled":true})).unwrap();
        let row = PlanRow { source_id: "esmo".into(), doc_key: "esmo-abc-0123456789ab".into(), url: "https://www.esmo.org/guidelines/x".into(),
            title: Some("ESMO item".into()), rendition: Some("json".into()), version: None,
            extra: json!({"kind":"dataview_description","source_id":"x","doi":doi,
                "publisher_ids":{"doi":doi,"doi_source":"source article_url (doi.org link)","article_url":format!("https://doi.org/{doi}")}}) };
        let mut seen = HashSet::new();
        let (site, open) = crosswalk_group(&src, &f, row, &mut seen).await;
        assert_eq!(open.len(), 1);
        assert!(open[0].doc_key.starts_with("esmo-pmc-pmc7674109-"));
        assert_eq!(site.extra[r2a::OPEN][0]["pmcid"], "PMC7674109");
        let d = Esmo.extract(&src, &f, &open[0]).await.unwrap();
        assert_eq!(d.rights.license.as_deref(), Some("CC-BY-NC-4.0"));
        assert!(d.relations.iter().any(|r| r.target == "guideline:esmo:esmo-abc-0123456789ab"));
        assert!(d.relations.iter().any(|r| r.target == format!("https://doi.org/{doi}")));
        assert!(d.blocks.len() > 20);
        let mut doc = GuidelineDoc::default();
        r2a::annotate(&mut doc, &src, &site.extra);
        assert_eq!(doc.relations[0].target, format!("guideline:esmo:{}", open[0].doc_key));
        // Rows without a DOI, or not description rows, are untouched.
        let bare = PlanRow { extra: json!({"kind":"dataview_description"}), ..open[0].clone() };
        assert!(crosswalk_group(&src, &f, bare, &mut seen).await.1.is_empty());
    }
}
