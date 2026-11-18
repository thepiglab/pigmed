//! Authorized HAPI development/test API. Production access is a separate product scope.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{
    Block, BlockKind, DateObservation, FetchRecord, GuidelineDoc, Relation, Rights,
};
use crate::guidelines::fetch::{Fetched, Fetcher, Req};
use crate::guidelines::html::blocks_from_html;
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::sync::OnceCell;
use url::Url;

#[derive(Default)]
pub struct NoHelsedirektoratet {
    // One immutable collection receipt per adapter run; the 35 MB collection is not refetched per row.
    collection: OnceCell<Collection>,
}
struct Collection {
    items: Vec<Value>,
    fetch: FetchRecord,
}
const ENDPOINT: &str = "https://api.helsedirektoratet.no/innhold/anbefalinger";
const KEY_ENV: &str = "HAPI_PRODUCTION_API_KEY";
const HEADER: &str = "Ocp-Apim-Subscription-Key";
const MAX_ITEMS: usize = 1000;

fn config(src: &SourceEntry) -> Result<()> {
    if src.config["access_scope"].as_str() != Some("development_test") {
        bail!("HAPI requires explicit development_test access_scope");
    }
    if src.catalog_url.as_deref() != Some(ENDPOINT) {
        bail!("HAPI catalog must be the documented HTTPS recommendations endpoint");
    }
    Ok(())
}
fn key() -> Result<String> {
    let value = std::env::var(KEY_ENV)
        .map_err(|_| anyhow::anyhow!("HAPI requires runtime environment variable {KEY_ENV}"))?;
    if value.trim().is_empty() {
        bail!("HAPI requires nonempty runtime environment variable {KEY_ENV}");
    }
    Ok(value)
}
fn request(key: &str, url: &str) -> Req {
    Req {
        headers: vec![
            (HEADER.into(), key.into()),
            ("Accept".into(), "application/json".into()),
        ],
        cache_max_age_secs: Some(0),
        ..Req::get(url)
    }
}
fn validate_id(id: &str) -> Result<()> {
    // Native IDs are opaque ASCII identifiers, never path segments supplied by an arbitrary URL.
    if id.is_empty()
        || id.len() > 200
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        bail!("HAPI source ID has an unexpected format");
    }
    Ok(())
}
fn detail_url(id: &str, full: bool) -> Result<String> {
    validate_id(id)?;
    let mut url = Url::parse(ENDPOINT)?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid HAPI detail URL"))?
        .push(id);
    if full {
        url.query_pairs_mut().append_pair("full", "true");
    }
    Ok(url.to_string())
}
fn checked_json(page: &Fetched) -> Result<Value> {
    if page.record.status != 200 {
        bail!("HAPI returned HTTP {}", page.record.status);
    }
    let url = Url::parse(&page.record.final_url)?;
    if url.scheme() != "https"
        || url.host_str() != Some("api.helsedirektoratet.no")
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("HAPI response left the permitted HTTPS origin");
    }
    let mime = page
        .record
        .content_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if !mime.eq_ignore_ascii_case("application/json") && !mime.eq_ignore_ascii_case("text/json") {
        bail!("HAPI response is not JSON");
    }
    serde_json::from_slice(&json_body(&page.body)?).context("HAPI response JSON invalid")
}
/// HAPI intermittently serves a gzip-encoded body (gzip magic bytes, `application/json` content type) although
/// the request sends no Accept-Encoding and the fetcher does not decode (observed 2026-11-17 on the 50th
/// `?full=true` detail of the full plan). Decode exactly that case, bounded; any other bytes pass unchanged.
const MAX_JSON_BYTES: u64 = 256 << 20;
fn json_body(body: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    if !body.starts_with(&[0x1f, 0x8b]) {
        return Ok(std::borrow::Cow::Borrowed(body));
    }
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(body)
        .take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut out)
        .context("HAPI gzip-encoded response could not be decoded")?;
    if out.len() as u64 > MAX_JSON_BYTES {
        bail!("HAPI gzip-encoded response exceeds the decoded size cap");
    }
    Ok(std::borrow::Cow::Owned(out))
}
fn response(page: &Fetched) -> Result<Vec<Value>> {
    let value = checked_json(page)?;
    let items = value
        .as_array()
        .context("HAPI recommendation collection is not a JSON array")?;
    let mut seen = HashSet::new();
    // Check the whole received collection, including records outside the requested probe limit.
    for item in items {
        let (id, _) = identity(item)?;
        if !seen.insert(id) {
            bail!("HAPI collection contains duplicate source IDs");
        }
    }
    Ok(items.clone())
}
fn safe_url(raw: &str) -> Option<(String, bool)> {
    let base = Url::parse(ENDPOINT).ok()?;
    let mut url = base.join(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let pairs = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let sensitive = |key: &str| {
        let key = key.to_ascii_lowercase();
        key.contains("token")
            || key.contains("signature")
            || key.contains("session")
            || key == "key"
            || key == "api_key"
            || key == "apikey"
            || key == "sig"
            || key.starts_with("x-amz-")
            || key.starts_with("x-goog-")
    };
    let removed = pairs.iter().any(|(key, _)| sensitive(key));
    if removed {
        url.set_query(None);
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in pairs.iter().filter(|(key, _)| !sensitive(key)) {
                query.append_pair(key, value);
            }
        }
        if url.query() == Some("") {
            url.set_query(None);
        }
    }
    Some((url.to_string(), removed))
}
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
fn identity(item: &Value) -> Result<(String, String)> {
    let id = field(item, "id").context("HAPI Infobit missing source id")?;
    validate_id(id)?;
    Ok((
        id.into(),
        format!("{}-{}", common::slug(id), common::short_hash(id)),
    ))
}
fn snapshot(value: &Value) -> Result<String> {
    Ok(crate::common::sha256_hex(&serde_json::to_vec(value)?))
}
fn links(item: &Value) -> (Vec<Relation>, Vec<Value>, Vec<Value>, Vec<String>) {
    let (mut relations, mut attachments, mut native, mut warnings) =
        (vec![], vec![], vec![], vec![]);
    if let Some(items) = item["links"].as_array() {
        for link in items {
            let href = field(link, "href");
            let parsed = href.and_then(safe_url);
            if href.is_some() && parsed.is_none() {
                warnings
                    .push("Source link has invalid or unsafe URL; inspect source record".into());
            }
            let refresh = parsed.as_ref().is_some_and(|(_, removed)| *removed);
            if refresh {
                warnings.push("Source link has credential/signature parameters; redacted URL requires refreshed authorization".into());
            }
            let target = parsed.map(|(url, _)| url);
            native.push(json!({"rel":link.get("rel"),"type":link.get("type"),"strukturId":link.get("strukturId"),
            "href":target,"access_url_requires_refresh":refresh}));
            if let Some(target) = target {
                let rel = match field(link, "rel") {
                    Some("forelder" | "root" | "publikasjon") => "part_of",
                    Some("barn") => "has_part",
                    _ => "related",
                };
                relations.push(Relation {
                    rel: rel.into(),
                    target,
                    scope: Some(format!(
                        "source rel={} type={} strukturId={}",
                        field(link, "rel").unwrap_or(""),
                        field(link, "type").unwrap_or(""),
                        field(link, "strukturId").unwrap_or("")
                    )),
                    evidence_url: Some(ENDPOINT.into()),
                });
            }
        }
    }
    if let Some(items) = item["attachments"].as_array() {
        for attachment in items {
            let raw = field(attachment, "fileUri");
            // Observed /guillotine paths have no verified asset origin. Never guess the API host.
            let absolute = raw.and_then(|uri| Url::parse(uri).ok()).is_some();
            let parsed = raw.filter(|_| absolute).and_then(safe_url);
            let unresolved = raw.is_some() && !absolute;
            let refresh = parsed.as_ref().is_some_and(|(_, removed)| *removed);
            if unresolved {
                warnings.push("Relative HAPI attachment origin is unresolved; descriptor retained without a download URL".into());
            }
            if absolute && parsed.is_none() {
                warnings.push(
                    "Source attachment has invalid or unsafe URL; inspect source record".into(),
                );
            }
            if refresh {
                warnings.push("Source attachment has credential/signature parameters; redacted URL requires refreshed authorization".into());
            }
            attachments.push(json!({"url":parsed.map(|(url,_)|url),"filename":field(attachment,"fileName"),"type":field(attachment,"fileType"),
            "relative_file_uri":raw.filter(|uri| unresolved && !uri.contains('?') && !uri.contains('#')),
            "origin_unresolved":unresolved,"access_url_requires_refresh":refresh,"bytes_downloaded":false}));
        }
    }
    (relations, attachments, native, warnings)
}
const MAX_PICO_CHILDREN: usize = 8;
const MAX_PICO_BYTES: usize = 8 << 20;
const PICO_PREFIX: &str = "/innhold/picoer/";
const PICO_FIELDS: &[&str] = &[
    "populasjon",
    "populasjonskode",
    "intervensjon",
    "intervensjonskode",
    "komperator",
    "komperatorkode",
    "kontroll",
    "utfall",
    "utfallskode",
    "utfallswidget",
];
struct PicoLink {
    id: String,
    url: String,
    occurrences: Vec<Value>,
}
struct PicoChild {
    import_observation: Option<DateObservation>,
    blocks: Vec<Block>,
    metadata: Value,
    fetch: FetchRecord,
    warnings: Vec<String>,
}
fn pico_url(raw: &str) -> Result<(String, String)> {
    let u = Url::parse(raw).context("HAPI PICO link must be absolute")?;
    if u.scheme() != "https"
        || u.host_str() != Some("api.helsedirektoratet.no")
        || u.port_or_known_default() != Some(443)
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        bail!("HAPI typed PICO link leaves the exact permitted HTTPS route");
    }
    let id = u
        .path()
        .strip_prefix(PICO_PREFIX)
        .context("HAPI typed PICO path invalid")?;
    validate_id(id)?;
    Ok((id.into(), u.to_string()))
}
fn pico_links(item: &Value) -> Result<Vec<PicoLink>> {
    if item
        .get("links")
        .is_some_and(|v| !v.is_null() && !v.is_array())
    {
        bail!("HAPI parent links schema is unmapped; typed child coverage unresolved");
    }
    let mut out: Vec<PicoLink> = vec![];
    for link in item["links"].as_array().into_iter().flatten() {
        if field(link, "rel") != Some("barn") || field(link, "type") != Some("pico") {
            continue;
        }
        let (id, url) = pico_url(field(link, "href").context("HAPI typed PICO href missing")?)?;
        let occurrence =
            json!({"rel":"barn","type":"pico","href":url,"strukturId":link.get("strukturId")});
        if let Some(old) = out.iter_mut().find(|old| old.id == id) {
            if old.url != url {
                bail!("HAPI PICO ID has conflicting source URLs");
            }
            old.occurrences.push(occurrence);
        } else {
            out.push(PicoLink {
                id,
                url,
                occurrences: vec![occurrence],
            });
        }
        if out.len() > MAX_PICO_CHILDREN {
            bail!("HAPI direct typed PICO child bound exceeded; coverage unresolved");
        }
    }
    Ok(out)
}
fn semantic_item(item: &Value) -> Value {
    let mut value = item.clone();
    // Import-only timestamp is preserved in raw cache and native metadata, not clinical identity.
    if let Some(object) = value.as_object_mut() {
        object.remove("sistImportertTilHapi");
    }
    value
}
fn clinical_snapshot(parent: &Value, children: &[PicoChild]) -> Result<String> {
    snapshot(
        &json!({"parent":semantic_item(parent),"typed_pico_children":children.iter().map(|c|&c.metadata).collect::<Vec<_>>()}),
    )
}
fn pico_component(
    id: &str,
    name: &str,
    text: &str,
    base: &Url,
) -> Result<(Vec<Block>, Vec<String>)> {
    if text.trim().is_empty() {
        return Ok((vec![], vec![]));
    }
    let known_html = regex::Regex::new(r"(?i)</?(?:p|div|ul|ol|li|h[1-6]|table|thead|tbody|tr|td|th|a|strong|em|span|br|sup|sub|figure|img|aside|section)\b")?.is_match(text);
    let fragment = if known_html {
        text.to_string()
    } else {
        format!(
            "<p>{}</p>",
            text.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        )
    };
    let html = format!("<div id=\"hapi-pico-field\">{fragment}</div>");
    let (mut blocks, mut warnings) = blocks_from_html(&html, Some(base), Some("#hapi-pico-field"));
    if blocks.is_empty() {
        bail!("HAPI nonempty PICO component has no preserved context blocks");
    }
    let heading = format!("PICO {id}");
    for block in &mut blocks {
        block.block_id = format!("hapi-pico-{id}-{name}-{}", block.block_id);
        block
            .heading_path
            .splice(0..0, [heading.clone(), name.into()]);
        // No child asset acquisition was authorized/verified. Preserve its exact source in metadata.
        if let BlockKind::Figure { src_url, .. } = &mut block.kind {
            *src_url = None;
            warnings.push("PICO visual remains a source-HTML descriptor; child assets are not automatically fetched".into());
        }
    }
    Ok((blocks, warnings))
}
fn pico_child(link: &PicoLink, parent_id: &str, page: Fetched) -> Result<PicoChild> {
    if page.body.len() > MAX_PICO_BYTES {
        bail!("HAPI PICO child exceeds8MiB acceptance/parser cap");
    }
    let (final_id, final_url) = pico_url(&page.record.final_url)?;
    if final_id != link.id || final_url != link.url {
        bail!("HAPI PICO child response path differs from emitted route");
    }
    if page.record.bytes != page.body.len() as u64
        || page.record.sha256 != crate::common::sha256_hex(&page.body)
    {
        bail!("HAPI PICO response byte/hash receipt mismatch");
    }
    let item = checked_json(&page)?;
    if field(&item, "id") != Some(link.id.as_str()) || link.id == parent_id {
        bail!("HAPI PICO child identity mismatch");
    }
    let data = item["data"]
        .as_object()
        .context("HAPI PICO data object missing; unknown schema")?;
    for required in ["populasjon", "intervensjon", "komperator", "utfall"] {
        if !data.get(required).is_some_and(Value::is_string) {
            bail!("HAPI PICO observed clinical string field missing/unknown schema");
        }
    }
    for (name, value) in data {
        if !PICO_FIELDS.contains(&name.as_str()) || !value.is_string() {
            bail!("HAPI PICO data field schema unmapped; child coverage unresolved");
        }
    }
    if data
        .get("utfallswidget")
        .and_then(Value::as_str)
        .is_some_and(|v| !v.trim().is_empty())
    {
        bail!(
            "HAPI nonempty PICO utfallswidget is unmapped; no outcome/evidence completeness claim"
        );
    }
    let base = Url::parse(&link.url)?;
    let (mut blocks, mut warnings) = (vec![], vec![]);
    let mut components = vec![];
    for (scope, name) in [
        ("root", "intro"),
        ("root", "tekst"),
        ("data", "populasjon"),
        ("data", "intervensjon"),
        ("data", "komperator"),
        ("data", "kontroll"),
        ("data", "utfall"),
    ] {
        let value = if scope == "root" {
            item.get(name)
        } else {
            data.get(name)
        };
        if value.is_some_and(|v| !v.is_null() && !v.is_string()) {
            bail!("HAPI PICO clinical component has unmapped type");
        }
        let raw = value.and_then(Value::as_str).unwrap_or("");
        let (component, notes) = pico_component(&link.id, name, raw, &base)?;
        components.push(json!({"field":format!("{scope}.{name}"),"present":value.is_some(),"source_value":value,"block_ids":component.iter().map(|b|&b.block_id).collect::<Vec<_>>(),"role":"PICO context, not a recommendation or outcome-effect/certainty table"}));
        blocks.extend(component);
        warnings.extend(notes);
    }
    let dates = [
        "opprettet",
        "forstPublisert",
        "sistOppdatert",
        "sistFagligOppdatert",
    ]
    .into_iter()
    .filter_map(|name| field(&item, name).map(|raw| common::date(name, raw, &link.url)))
    .collect::<Vec<_>>();
    let (_, attachments, native_links, notes) = links(&item);
    warnings.extend(notes);
    let mut native = semantic_item(&item);
    native["links"] = json!(native_links);
    native["attachments"] = json!(attachments);
    if let Some(raw) = field(&item, "url") {
        native["url"] = json!(safe_url(raw).map(|v| v.0));
    }
    let metadata = json!({"id":link.id,"source_url":link.url,"source_link_occurrences":link.occurrences,"source_status":item.get("status"),"source_type":item.get("dokumentType"),"own_dates":dates,"native_fields":native,"components":components,"native_blocks":blocks,"coverage":{"typed_direct_child":"parsed_observed_string_fields","recursive_links_fetched":false,"external_evidence_fetched":false,"outcome_effects_or_certainty_verified":false,"clinical_completeness":"unknown"}});
    Ok(PicoChild {
        import_observation: field(&item, "sistImportertTilHapi")
            .map(|raw| common::date("sistImportertTilHapi", raw, &link.url)),
        blocks,
        metadata,
        fetch: page.record,
        warnings,
    })
}
fn append_picos(doc: &mut GuidelineDoc, children: Vec<PicoChild>) {
    let count = children.len();
    let mut native = vec![];
    let mut imports = vec![];
    for child in children {
        if let Some(date) = child.import_observation {
            imports.push(json!({"child_id":child.metadata["id"],"observation":date,"scope":"child import timestamp only; excluded from clinical semantic identity"}));
        }
        doc.blocks.extend(child.blocks);
        doc.fetches.push(child.fetch);
        doc.warnings.extend(child.warnings);
        native.push(child.metadata);
    }
    doc.search_metadata["native_pico_children"] = json!(native);
    doc.search_metadata["pico_import_observations"] = json!(imports);
    doc.search_metadata["pico_coverage"] = json!({"typed_direct_children":count,"mapped_direct_children":count,"child_bound":MAX_PICO_CHILDREN,"child_acceptance_parser_max_bytes":MAX_PICO_BYTES,"failed_or_unmapped_child_policy":"fail_closed","recursive_links_fetched":false,"external_evidence_fetched":false,"clinical_completeness":"unknown"});
    if count > 0 {
        doc.warnings.push("Typed PICO child plain/HTML fields are additional context only. Their source status/dates are child-owned; no new recommendation, outcome effect or GRADE certainty is inferred. Downstream links/evidence/attachments remain unfetched, so clinical completeness is unknown.".into());
    }
}

fn document(
    src: &SourceEntry,
    row: &PlanRow,
    item: &Value,
    fetch: FetchRecord,
) -> Result<GuidelineDoc> {
    let (id, work) = identity(item)?;
    if row.extra["source_id"].as_str() != Some(id.as_str()) {
        bail!("HAPI source identity changed");
    }
    let title = field(item, "tittel").context("HAPI Infobit title missing")?;
    let body = field(item, "tekst").context("HAPI Infobit clinical text missing")?;
    let page_url = field(item, "url")
        .and_then(safe_url)
        .map(|(url, _)| url)
        .unwrap_or_else(|| ENDPOINT.into());
    let base = Url::parse(&page_url)?;
    let parse_component = |name: &str, fragment: &str| {
        let html = format!("<div id=\"hapi-component\">{fragment}</div>");
        let (mut blocks, warnings) = blocks_from_html(&html, Some(&base), Some("#hapi-component"));
        // Components are parsed independently so their heading paths cannot bleed into each other.
        for block in &mut blocks {
            block.block_id = format!("hapi-{name}-{}", block.block_id);
        }
        (blocks, warnings)
    };
    let (mut blocks, mut warnings) = parse_component("intro", field(item, "intro").unwrap_or(""));
    let (statement_blocks, statement_warnings) = parse_component("tekst", body);
    warnings.extend(statement_warnings);
    let statement_prose = statement_blocks
        .iter()
        .filter(|block| matches!(block.kind, BlockKind::Paragraph | BlockKind::ListItem))
        .collect::<Vec<_>>();
    let statement_text = statement_prose
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let statement_source_ids = statement_prose
        .iter()
        .map(|block| block.block_id.clone())
        .collect::<Vec<_>>();
    let mut grouped = false;
    // The API tekst field is one source recommendation component. A visible two-item list
    // belongs to that one statement; paragraph/list boundaries never create recommendations.
    for mut block in statement_blocks.iter().cloned() {
        if matches!(block.kind, BlockKind::Paragraph | BlockKind::ListItem) {
            if grouped {
                continue;
            }
            block.block_id = "hapi-recommendation".into();
            block.text = statement_text.clone();
            block.kind = BlockKind::Recommendation {
                label: None,
                strength: field(&item["data"], "styrke").map(String::from),
                certainty: None,
                population: None,
                grading_system: None,
                qualifier: None,
            };
            grouped = true;
        }
        blocks.push(block);
    }
    if !grouped {
        warnings.push("Infobit tekst has no prose/list text to group as a recommendation; native structure is retained".into());
    }
    // Native per-block order below retains the original statement structure even when prose
    // spans a table/figure. Main blocks keep all headings, tables, figures and footnotes.
    // Practical information and rationale are separate context, never part of the statement.
    for key in ["praktisk", "rasjonale"] {
        if let Some(value) = field(&item["data"], key) {
            let (context, context_warnings) =
                parse_component(key, &format!("<h2>{key}</h2>{value}"));
            blocks.extend(context);
            warnings.extend(context_warnings);
        }
    }
    if blocks.is_empty() {
        bail!("HAPI clinical text has no parseable blocks");
    }
    let mut dates = vec![];
    for key in [
        "opprettet",
        "forstPublisert",
        "sistOppdatert",
        "sistFagligOppdatert",
        "sistImportertTilHapi",
    ] {
        if let Some(value) = field(item, key) {
            dates.push(common::date(key, value, ENDPOINT));
        }
    }
    let (relations, attachments, native_links, link_warnings) = links(item);
    warnings.extend(link_warnings);
    if let Some(data) = item["data"].as_object() {
        let unknown = data
            .keys()
            .filter(|key| {
                ![
                    "nokkelInfo",
                    "referanserEndNote",
                    "styrke",
                    "praktisk",
                    "rasjonale",
                ]
                .contains(&key.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            warnings.push(format!(
                "Unmapped HAPI data fields require clinical-fidelity review: {}",
                unknown.join(", ")
            ));
        }
        if data
            .get("nokkelInfo")
            .and_then(Value::as_object)
            .is_some_and(|value| !value.is_empty())
        {
            warnings.push("Nonempty native nokkelInfo is preserved as source metadata; its clinical block mapping remains unverified".into());
        }
    }
    warnings.push("Development/test subscription only. Collection has no verified pagination/full-inventory contract. Linked attachments are descriptors, not downloaded clinical content.".into());
    Ok(GuidelineDoc { source_id:src.id.clone(), doc_key:row.doc_key.clone(),work_key:Some(work),title:title.into(),url:page_url,
        item_type:Some("recommendation".into()),status:"unknown".into(),language:field(item,"spraak").map(String::from),
        date_observations:dates,relations,attachments,
        rights:Rights { status:"restricted".into(),license:Some("Norsk lisens for offentlige data (NLOD)".into()),
            terms_url:Some("https://utvikler.helsedirektoratet.no/product#product=59c3b0c684302f0154ebbe28".into()),
            attribution:Some("Helsedirektoratet".into()),restrictions:vec!["Clearly identify Helsedirektoratet as source adjacent to the content; do not alter clinical meaning or imply official origin.".into()],
            evidence_urls:vec!["https://utvikler.helsedirektoratet.no/product#product=59c3b0c684302f0154ebbe28".into()],
            note:Some("Version of NLOD not specified in subscription terms; verify per asset and publication use.".into()),..Default::default() },
        search_metadata:json!({"source_id":id,"source_status":item.get("status"),"source_type":item.get("dokumentType"),
            "tema":item.get("tema"),"maalgruppe":item.get("maalgruppe"),"source_url":ENDPOINT,"native_links":native_links,
            "source_owner":item.get("eier"),"native_strength":item["data"].get("styrke"),"native_nokkelInfo":item["data"].get("nokkelInfo"),
            "native_references":item["data"].get("referanserEndNote"),"native_technical_data":item.get("tekniskeData"),
            "native_statement_blocks":statement_blocks,"recommendation_grouping":{"component":"tekst","recommendation_block_id":grouped.then_some("hapi-recommendation"),"source_prose_block_ids":statement_source_ids,"source_order":"native_statement_blocks"},
            "data_keys":item.get("data").and_then(Value::as_object).map(|value|value.keys().cloned().collect::<Vec<_>>()),"access_scope":"development_test",
            "metadata_provenance":{"tema":ENDPOINT,"maalgruppe":ENDPOINT,"source_owner":ENDPOINT,"native_strength":ENDPOINT}}),
        blocks,warnings,fetches:vec![fetch],..Default::default() })
}
impl NoHelsedirektoratet {
    async fn collection_with_key(&self, fetcher: &Fetcher, api_key: &str) -> Result<&Collection> {
        self.collection
            .get_or_try_init(|| async {
                let page = fetcher.request(&request(api_key, ENDPOINT)).await?;
                Ok::<_, anyhow::Error>(Collection {
                    items: response(&page)?,
                    fetch: page.record,
                })
            })
            .await
    }
    async fn full_detail(
        &self,
        fetcher: &Fetcher,
        api_key: &str,
        id: &str,
        full: bool,
    ) -> Result<(Value, FetchRecord)> {
        let page = fetcher
            .request(&request(api_key, &detail_url(id, full)?))
            .await?;
        let value = checked_json(&page)?;
        if field(&value, "id") != Some(id) {
            bail!("HAPI detail identity or object shape differs from planned identity");
        }
        Ok((value, page.record))
    }
    async fn picos(
        &self,
        fetcher: &Fetcher,
        api_key: &str,
        parent: &Value,
    ) -> Result<Vec<PicoChild>> {
        let parent_id = field(parent, "id").context("HAPI parent ID missing")?;
        let mut out = vec![];
        for link in pico_links(parent)? {
            // Validate the exact source-emitted URL before attaching the runtime API header.
            let page = fetcher
                .request(&request(api_key, &link.url))
                .await
                .context("HAPI typed PICO child fetch failed; coverage unresolved")?;
            out.push(pico_child(&link, parent_id, page)?);
        }
        Ok(out)
    }
    async fn discover_with_key(
        &self,
        src: &SourceEntry,
        fetcher: &Fetcher,
        limit: Option<usize>,
        api_key: &str,
    ) -> Result<Vec<PlanRow>> {
        config(src)?;
        let limit = limit.unwrap_or(50);
        if limit > MAX_ITEMS {
            bail!("HAPI development/test discovery limit exceeds {MAX_ITEMS}");
        }
        if limit == 0 {
            return Ok(vec![]);
        }
        let collection = self.collection_with_key(fetcher, api_key).await?;
        let mut rows = vec![];
        for item in collection.items.iter().take(limit) {
            let (id, work) = identity(item)?;
            let (full, full_fetch) = self.full_detail(fetcher, api_key, &id, true).await?;
            let children = self.picos(fetcher, api_key, &full).await?;
            let digest = clinical_snapshot(&full, &children)?;
            rows.push(PlanRow { source_id:src.id.clone(),doc_key:format!("{work}-{}",&digest[..12]),url:ENDPOINT.into(),
                title:Some(field(&full,"tittel").context("HAPI Infobit title missing")?.into()),rendition:Some("api".into()),version:None,
                extra:json!({"source_id":id,"item_sha256":snapshot(item)?,"item_semantic_sha256":snapshot(&semantic_item(item))?,"full_detail_sha256":snapshot(&full)?,"clinical_snapshot_sha256":digest,
                    "typed_pico_children":children.iter().map(|c|&c.metadata).collect::<Vec<_>>(),"planned_pico_fetches":children.iter().map(|c|&c.fetch).collect::<Vec<_>>(),"planned_full_fetch":full_fetch,"collection_count":collection.items.len(),"access_scope":"development_test"}) });
        }
        Ok(rows)
    }
    async fn extract_with_key(
        &self,
        src: &SourceEntry,
        fetcher: &Fetcher,
        row: &PlanRow,
        api_key: &str,
    ) -> Result<GuidelineDoc> {
        config(src)?;
        if row.source_id != src.id
            || row.url != ENDPOINT
            || row.extra["access_scope"] != "development_test"
        {
            bail!("HAPI plan scope or endpoint invalid");
        }
        let id = row.extra["source_id"]
            .as_str()
            .context("HAPI plan source ID missing")?;
        validate_id(id)?;
        let collection = self.collection_with_key(fetcher, api_key).await?;
        let item = collection
            .items
            .iter()
            .find(|item| field(item, "id") == Some(id))
            .context("HAPI planned Infobit absent")?;
        if row.extra["item_semantic_sha256"].as_str()
            != Some(snapshot(&semantic_item(item))?.as_str())
        {
            bail!("HAPI collection Infobit changed since planning; replan");
        }
        let (normal, normal_fetch) = self.full_detail(fetcher, api_key, id, false).await?;
        let (full, full_fetch) = self.full_detail(fetcher, api_key, id, true).await?;
        let children = self.picos(fetcher, api_key, &full).await?;
        let digest = clinical_snapshot(&full, &children)?;
        let (_, work) = identity(&full)?;
        if row.extra["clinical_snapshot_sha256"].as_str() != Some(digest.as_str())
            || row.doc_key != format!("{work}-{}", &digest[..12])
        {
            bail!("HAPI full clinical detail changed since planning; replan");
        }
        let mut doc = document(src, row, &full, collection.fetch.clone())?;
        doc.fetches.extend([normal_fetch, full_fetch]);
        doc.search_metadata["collection_count"] = json!(collection.items.len());
        doc.search_metadata["collection_detail_equal"] = json!(item == &normal);
        doc.search_metadata["detail_full_equal"] = json!(normal == full);
        doc.search_metadata["full_detail_sha256"] = json!(snapshot(&full)?);
        doc.search_metadata["clinical_snapshot_sha256"] = json!(digest);
        doc.search_metadata["pico_source_transport_max_bytes"] = json!(fetcher.max_bytes);
        append_picos(&mut doc, children);
        doc.warnings.push("Collection/detail/full equality is structural evidence, not proof of clinical completeness. Individual JSON receipts remain in the private Fetcher cache.".into());
        Ok(doc)
    }
}
#[async_trait::async_trait]
impl Adapter for NoHelsedirektoratet {
    async fn discover(
        &self,
        src: &SourceEntry,
        fetcher: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        config(src)?;
        if limit == Some(0) {
            return Ok(vec![]);
        }
        self.discover_with_key(src, fetcher, limit, &key()?).await
    }
    async fn extract(
        &self,
        src: &SourceEntry,
        fetcher: &Fetcher,
        row: &PlanRow,
    ) -> Result<GuidelineDoc> {
        self.extract_with_key(src, fetcher, row, &key()?).await
    }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/guidelines/no_helsedirektoratet/infobit.json"
        ))
        .unwrap()
    }
    fn source() -> SourceEntry {
        serde_json::from_value(json!({"id":"no_helsedirektoratet","access":"mixed","enabled":true,"catalog_url":ENDPOINT,"config":{"access_scope":"development_test"}})).unwrap()
    }
    #[test]
    fn native_infobit_preserves_statement_grade_rationale_and_unresolved_assets() {
        let item = fixture();
        let row = PlanRow {
            source_id: source().id,
            doc_key: "fixture-snapshot".into(),
            url: ENDPOINT.into(),
            title: None,
            rendition: None,
            version: None,
            extra: json!({"source_id":"sample-1"}),
        };
        let doc = document(&source(), &row, &item, Default::default()).unwrap();
        assert_eq!(doc.language.as_deref(), Some("no"));
        assert_eq!(doc.status, "unknown");
        assert_eq!(
            doc.date_observations[0].normalized.as_deref(),
            Some("2024-01-02")
        );
        let recommendations = doc
            .blocks
            .iter()
            .filter(|block| matches!(block.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recommendations.len(), 1);
        let recommendation = recommendations[0];
        assert_eq!(recommendation.block_id, "hapi-recommendation");
        assert_eq!(
            recommendation.text,
            "Synthetic first recommendation bullet.\n\nSynthetic second recommendation bullet."
        );
        assert_eq!(recommendation.heading_path, vec!["Context"]);
        assert!(
            matches!(&recommendation.kind,BlockKind::Recommendation{strength:Some(strength),certainty:None,..} if strength=="sterk")
        );
        assert!(!recommendation
            .text
            .contains("Synthetic introductory context"));
        assert!(!recommendation.text.contains("Synthetic statement note"));
        assert!(!recommendation.text.contains("Synthetic practical guidance"));
        assert!(!recommendation.text.contains("Synthetic rationale"));
        let note = doc
            .blocks
            .iter()
            .position(|block| {
                matches!(block.kind, BlockKind::Footnote)
                    && block.text == "Synthetic statement note."
            })
            .unwrap();
        let rec_position = doc
            .blocks
            .iter()
            .position(|block| block.block_id == "hapi-recommendation")
            .unwrap();
        let table = doc
            .blocks
            .iter()
            .position(|block| matches!(block.kind, BlockKind::Table { .. }))
            .unwrap();
        let figure = doc
            .blocks
            .iter()
            .position(|block| matches!(block.kind, BlockKind::Figure { .. }))
            .unwrap();
        let practical = doc
            .blocks
            .iter()
            .position(|block| block.text == "Synthetic practical guidance.")
            .unwrap();
        let rationale = doc
            .blocks
            .iter()
            .position(|block| block.text == "Synthetic rationale.")
            .unwrap();
        assert!(
            rec_position < note
                && note < table
                && table < figure
                && figure < practical
                && practical < rationale
        );
        assert_eq!(doc.blocks[0].text, "Synthetic introductory context.");
        let native = doc.search_metadata["native_statement_blocks"]
            .as_array()
            .unwrap();
        let list_items = native
            .iter()
            .filter(|block| block["kind"]["type"] == "list_item")
            .collect::<Vec<_>>();
        assert_eq!(list_items.len(), 2);
        assert_eq!(
            list_items[0]["text"],
            "Synthetic first recommendation bullet."
        );
        assert_eq!(
            list_items[1]["text"],
            "Synthetic second recommendation bullet."
        );
        assert_eq!(
            doc.search_metadata["recommendation_grouping"]["source_prose_block_ids"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            doc.blocks
                .iter()
                .map(|block| &block.block_id)
                .collect::<HashSet<_>>()
                .len(),
            doc.blocks.len()
        );
        assert!(doc
            .blocks
            .iter()
            .any(|block| block.text == "Synthetic practical guidance."
                && block
                    .heading_path
                    .iter()
                    .any(|heading| heading == "praktisk")));
        assert!(doc
            .blocks
            .iter()
            .any(|block| block.text == "Synthetic rationale."
                && block
                    .heading_path
                    .iter()
                    .any(|heading| heading == "rasjonale")));
        assert!(doc
            .blocks
            .iter()
            .any(|block| matches!(block.kind, BlockKind::Table { .. })));
        assert!(doc.blocks.iter().any(|block|matches!(&block.kind,BlockKind::Figure{src_url:Some(url),..} if url=="https://www.helsedirektoratet.no/image.png")));
        assert_eq!(doc.search_metadata["native_strength"], "sterk");
        assert_eq!(
            doc.search_metadata["native_references"][0]["link"],
            "Synthetic reference"
        );
        assert_eq!(
            doc.search_metadata["native_technical_data"]["subtype"],
            "Synthetic subtype"
        );
        assert_eq!(doc.relations[0].rel, "part_of");
        assert_eq!(doc.search_metadata["native_links"][0]["rel"], "forelder");
        assert!(doc.attachments[0]["url"].is_null());
        assert_eq!(doc.attachments[0]["origin_unresolved"], true);
        assert_eq!(
            doc.attachments[0]["relative_file_uri"],
            "/guillotine/fixture.xml"
        );
        assert!(doc
            .warnings
            .iter()
            .any(|warning| warning.contains("descriptors")));
    }
    #[test]
    fn statement_prose_spanning_structures_is_one_recommendation_with_native_order() {
        let mut item = fixture();
        item["tekst"]=json!("<p>First statement paragraph.</p><table><tr><td>Embedded evidence</td></tr></table><p>Second statement paragraph.</p><aside>Statement footnote.</aside>");
        let row = PlanRow {
            source_id: source().id,
            doc_key: "fixture-snapshot".into(),
            url: ENDPOINT.into(),
            title: None,
            rendition: None,
            version: None,
            extra: json!({"source_id":"sample-1"}),
        };
        let doc = document(&source(), &row, &item, Default::default()).unwrap();
        let recommendations = doc
            .blocks
            .iter()
            .filter(|block| matches!(block.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recommendations.len(), 1);
        assert_eq!(
            recommendations[0].text,
            "First statement paragraph.\n\nSecond statement paragraph."
        );
        let native = doc.search_metadata["native_statement_blocks"]
            .as_array()
            .unwrap();
        assert_eq!(
            native
                .iter()
                .map(|block| block["kind"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["paragraph", "table", "paragraph", "footnote"]
        );
        assert!(doc
            .blocks
            .iter()
            .any(|block| matches!(block.kind, BlockKind::Table { .. })));
        assert!(doc
            .blocks
            .iter()
            .any(|block| matches!(block.kind, BlockKind::Footnote)
                && block.text == "Statement footnote."));
    }
    #[tokio::test]
    async fn offline_discovery_memoizes_collection_and_pins_full_detail_including_assets() {
        let temp = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(temp.path(), true, 500, 1 << 20, "").unwrap();
        let api_key = "synthetic-api-key";
        let item = fixture();
        let bytes = serde_json::to_vec(&item).unwrap();
        fetcher.seed_cache_request_for_tests(
            &request(api_key, ENDPOINT),
            &serde_json::to_vec(&vec![&item]).unwrap(),
            "application/json",
        );
        for full in [false, true] {
            fetcher.seed_cache_request_for_tests(
                &request(api_key, &detail_url("sample-1", full).unwrap()),
                &bytes,
                "application/json",
            );
        }
        let adapter = NoHelsedirektoratet::default();
        let rows = adapter
            .discover_with_key(&source(), &fetcher, Some(1), api_key)
            .await
            .unwrap();
        // A changed cache cannot replace this adapter instance's already loaded collection snapshot.
        fetcher.seed_cache_request_for_tests(
            &request(api_key, ENDPOINT),
            b"[]",
            "application/json",
        );
        let doc = adapter
            .extract_with_key(&source(), &fetcher, &rows[0], api_key)
            .await
            .unwrap();
        assert_eq!(doc.fetches.len(), 3);
        assert_eq!(doc.search_metadata["collection_count"], 1);
        assert!(doc.fetches.iter().all(|receipt| receipt.from_cache));
        assert!(!serde_json::to_string(&doc).unwrap().contains(api_key));
        let mut changed = item.clone();
        changed["attachments"][0]["fileName"] = json!("changed.xml");
        fetcher.seed_cache_request_for_tests(
            &request(api_key, &detail_url("sample-1", true).unwrap()),
            &serde_json::to_vec(&changed).unwrap(),
            "application/json",
        );
        let error = adapter
            .extract_with_key(&source(), &fetcher, &rows[0], api_key)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("full clinical detail changed"));
        let fresh = NoHelsedirektoratet::default();
        fetcher.seed_cache_request_for_tests(
            &request(api_key, ENDPOINT),
            &serde_json::to_vec(&vec![&item]).unwrap(),
            "application/json",
        );
        let updated = fresh
            .discover_with_key(&source(), &fetcher, Some(1), api_key)
            .await
            .unwrap();
        assert_ne!(rows[0].doc_key, updated[0].doc_key);
    }
    #[test]
    fn strict_ids_and_json_contract_reject_unsafe_or_ambiguous_responses() {
        for id in ["", "a/b", "..", "a?full=true", "https://evil.test"] {
            assert!(detail_url(id, true).is_err());
        }
        assert!(detail_url("sample-1", true)
            .unwrap()
            .ends_with("sample-1?full=true"));
        let mut page = Fetched {
            record: FetchRecord {
                status: 200,
                final_url: ENDPOINT.into(),
                content_type: Some("application/json; charset=utf-8".into()),
                ..Default::default()
            },
            body: b"[]".to_vec(),
        };
        assert!(response(&page).unwrap().is_empty());
        let item = fixture();
        page.body = serde_json::to_vec(&vec![&item, &item]).unwrap();
        assert!(response(&page).is_err());
        page.record.final_url = "https://example.org/".into();
        assert!(response(&page).is_err());
        page.record.final_url = ENDPOINT.into();
        page.record.content_type = Some("text/html".into());
        assert!(response(&page).is_err());
        let (relations, _, _, warnings) = links(
            &json!({"links":[{"href":"/a?token=synthetic-secret&full=true","rel":"forelder"}]}),
        );
        assert!(!relations[0].target.contains("synthetic-secret"));
        assert!(!warnings.is_empty());
        assert_eq!(
            request("synthetic-api-key", ENDPOINT).cache_max_age_secs,
            Some(0)
        );
    }
    fn pico_fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/guidelines/no_helsedirektoratet/pico-context-synthetic.json"
        ))
        .unwrap()
    }
    fn typed_link() -> Value {
        json!({"rel":"barn","type":"pico","strukturId":"synthetic-structure","href":"https://api.helsedirektoratet.no/innhold/picoer/pico-synthetic-1"})
    }
    fn typed_parent() -> Value {
        let mut parent = fixture();
        parent["links"].as_array_mut().unwrap().push(typed_link());
        parent
    }
    fn child_page(item: &Value) -> Fetched {
        let body = serde_json::to_vec(item).unwrap();
        Fetched {
            record: FetchRecord {
                url: typed_link()["href"].as_str().unwrap().into(),
                final_url: typed_link()["href"].as_str().unwrap().into(),
                status: 200,
                content_type: Some("application/json".into()),
                bytes: body.len() as u64,
                sha256: crate::common::sha256_hex(&body),
                ..Default::default()
            },
            body,
        }
    }
    fn parsed_child(item: &Value) -> PicoChild {
        pico_child(
            &pico_links(&typed_parent()).unwrap().remove(0),
            "sample-1",
            child_page(item),
        )
        .unwrap()
    }
    #[test]
    fn pico_plain_fields_codes_dates_and_status_are_context_with_separate_provenance() {
        let child = parsed_child(&pico_fixture());
        assert_eq!(child.blocks.len(), 4);
        assert_eq!(child.blocks[0].text, "Synthetic population < 5 & distinct.");
        assert!(child
            .blocks
            .iter()
            .all(|b| matches!(b.kind, BlockKind::Paragraph)));
        assert_eq!(child.metadata["source_status"], "Synthetic child state");
        assert_eq!(
            child.metadata["native_fields"]["data"]["populasjonskode"],
            "synthetic-pop-code"
        );
        assert_eq!(child.metadata["own_dates"].as_array().unwrap().len(), 4);
        assert_eq!(
            child.metadata["own_dates"][0]["source_url"],
            typed_link()["href"]
        );
        assert_eq!(
            child.metadata["coverage"]["outcome_effects_or_certainty_verified"],
            false
        );
        let row = PlanRow {
            source_id: source().id,
            doc_key: "fixture".into(),
            url: ENDPOINT.into(),
            title: None,
            rendition: None,
            version: None,
            extra: json!({"source_id":"sample-1"}),
        };
        let mut doc = document(&source(), &row, &typed_parent(), Default::default()).unwrap();
        let original_recs = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .cloned()
            .collect::<Vec<_>>();
        let parent_dates = serde_json::to_value(&doc.date_observations).unwrap();
        append_picos(&mut doc, vec![child]);
        let recs = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recs.len(), 1);
        assert_eq!(
            serde_json::to_value(&recs).unwrap(),
            serde_json::to_value(&original_recs).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&doc.date_observations).unwrap(),
            parent_dates
        );
        assert_eq!(doc.status, "unknown");
        assert_eq!(doc.fetches.len(), 2);
        assert_eq!(
            doc.search_metadata["pico_import_observations"][0]["child_id"],
            "pico-synthetic-1"
        );
        assert!(
            doc.blocks
                .iter()
                .map(|b| &b.block_id)
                .collect::<HashSet<_>>()
                .len()
                == doc.blocks.len()
        );
    }
    #[test]
    fn pico_html_and_explicit_empty_fields_keep_native_structure_without_new_recommendations() {
        let mut item = pico_fixture();
        item["data"]["intervensjon"]=json!("<p id='context'>Synthetic action.</p><ul><li>Clause A.</li><li>Clause B.</li></ul><table><tr><th>Measure</th></tr><tr><td>Value</td></tr></table><aside>Context note.</aside><figure><img src='https://www.fhi.no/synthetic.png'><figcaption>Descriptor only</figcaption></figure>");
        let child = parsed_child(&item);
        assert_eq!(
            child
                .blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::ListItem))
                .count(),
            2
        );
        assert_eq!(
            child
                .blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
                .count(),
            1
        );
        assert!(child
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Footnote) && b.text == "Context note."));
        assert!(child
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Figure { src_url: None, .. })));
        assert_eq!(
            child
                .blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            0
        );
        assert!(child.metadata["native_fields"]["data"]["intervensjon"]
            .as_str()
            .unwrap()
            .contains("https://www.fhi.no/synthetic.png"));
        for name in ["populasjon", "intervensjon", "komperator", "utfall"] {
            item["data"][name] = json!("");
        }
        let empty = parsed_child(&item);
        assert!(empty.blocks.is_empty());
        assert_eq!(empty.metadata["components"][2]["source_value"], "");
        assert_eq!(
            empty.metadata["coverage"]["clinical_completeness"],
            "unknown"
        );
    }
    #[test]
    fn typed_pico_links_are_exact_origin_deduplicated_bounded_and_never_recursive() {
        let mut parent = typed_parent();
        parent["links"].as_array_mut().unwrap().push(typed_link());
        let links = pico_links(&parent).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].occurrences.len(), 2);
        for raw in [
            "http://api.helsedirektoratet.no/innhold/picoer/a",
            "https://evil.test/innhold/picoer/a",
            "https://api.helsedirektoratet.no:444/innhold/picoer/a",
            "https://u:p@api.helsedirektoratet.no/innhold/picoer/a",
            "/innhold/picoer/a",
            "https://api.helsedirektoratet.no/innhold/picoer/a?full=true",
            "https://api.helsedirektoratet.no/innhold/picoer/a#part",
            "https://api.helsedirektoratet.no/innhold/picoer/a/b",
            "https://api.helsedirektoratet.no/innhold/anbefalinger/a",
        ] {
            assert!(pico_url(raw).is_err(), "unsafe route accepted");
        }
        parent["links"] = json!([{"rel":"related","type":"pico","href":"https://evil.test/"},{"rel":"barn","type":"rapport","href":"https://www.fhi.no/"}]);
        assert!(pico_links(&parent).unwrap().is_empty());
        parent["links"]=json!((0..=MAX_PICO_CHILDREN).map(|n|json!({"rel":"barn","type":"pico","href":format!("https://api.helsedirektoratet.no/innhold/picoer/pico-{n}")})).collect::<Vec<_>>());
        assert!(pico_links(&parent).is_err());
    }
    #[test]
    fn pico_response_schema_identity_mime_and_size_fail_closed() {
        let item = pico_fixture();
        let link = pico_links(&typed_parent()).unwrap().remove(0);
        for modification in 0..7 {
            let mut page = child_page(&item);
            match modification {
                0 => page.record.status = 403,
                1 => page.record.content_type = Some("text/html".into()),
                2 => {
                    page.record.final_url =
                        "https://evil.test/innhold/picoer/pico-synthetic-1".into()
                }
                3 => {
                    page.record.final_url =
                        "https://api.helsedirektoratet.no/innhold/picoer/other-id".into()
                }
                4 => page.body = vec![b' '; MAX_PICO_BYTES + 1],
                5 => {
                    let mut v = item.clone();
                    v["id"] = json!("other-id");
                    page = child_page(&v)
                }
                _ => page = child_page(&json!([])),
            }
            assert!(pico_child(&link, "sample-1", page).is_err());
        }
        for modification in 0..5 {
            let mut changed = item.clone();
            match modification {
                0 => changed["data"]["utfallswidget"] = json!("unmapped nonempty widget"),
                1 => changed["data"]["utfall"] = json!({"unknown":"shape"}),
                2 => changed["data"] = json!({}),
                3 => changed["data"]["unknownEvidence"] = json!("unmapped clinical field"),
                _ => changed["intro"] = json!(["unexpected root schema"]),
            }
            assert!(pico_child(&link, "sample-1", child_page(&changed)).is_err());
        }
    }
    #[tokio::test]
    async fn pico_child_only_drift_forces_replan_but_import_and_fetch_time_do_not() {
        let temp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(temp.path(), true, 500, 16 << 20, "").unwrap();
        let api_key = "synthetic-api-key";
        let parent = typed_parent();
        let child = pico_fixture();
        f.seed_cache_request_for_tests(
            &request(api_key, ENDPOINT),
            &serde_json::to_vec(&vec![&parent]).unwrap(),
            "application/json",
        );
        for full in [false, true] {
            f.seed_cache_request_for_tests(
                &request(api_key, &detail_url("sample-1", full).unwrap()),
                &serde_json::to_vec(&parent).unwrap(),
                "application/json",
            );
        }
        let child_req = request(api_key, typed_link()["href"].as_str().unwrap());
        f.seed_cache_request_for_tests(
            &child_req,
            &serde_json::to_vec(&child).unwrap(),
            "application/json",
        );
        let adapter = NoHelsedirektoratet::default();
        let rows = adapter
            .discover_with_key(&source(), &f, Some(1), api_key)
            .await
            .unwrap();
        let doc = adapter
            .extract_with_key(&source(), &f, &rows[0], api_key)
            .await
            .unwrap();
        assert_eq!(doc.fetches.len(), 4);
        assert_eq!(
            doc.search_metadata["pico_coverage"]["typed_direct_children"],
            1
        );
        assert!(!serde_json::to_string(&doc).unwrap().contains(api_key));
        let mut changed = child.clone();
        changed["sistImportertTilHapi"] = json!("2030-01-01");
        f.seed_cache_request_for_tests(
            &child_req,
            &serde_json::to_vec(&changed).unwrap(),
            "application/json",
        );
        assert!(adapter
            .extract_with_key(&source(), &f, &rows[0], api_key)
            .await
            .is_ok());
        changed["data"]["utfall"] = json!("Changed synthetic outcome description.");
        f.seed_cache_request_for_tests(
            &child_req,
            &serde_json::to_vec(&changed).unwrap(),
            "application/json",
        );
        let error = adapter
            .extract_with_key(&source(), &f, &rows[0], api_key)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("replan"));
        let replanned = adapter
            .discover_with_key(&source(), &f, Some(1), api_key)
            .await
            .unwrap();
        assert_ne!(rows[0].doc_key, replanned[0].doc_key);
        let mut one = parsed_child(&child);
        let before = clinical_snapshot(&parent, &[parsed_child(&child)]).unwrap();
        one.fetch.fetched_at = "2031-01-01".into();
        assert_eq!(before, clinical_snapshot(&parent, &[one]).unwrap());
        let mut parent_import = parent.clone();
        parent_import["sistImportertTilHapi"] = json!("2032-01-01");
        assert_eq!(
            before,
            clinical_snapshot(&parent_import, &[parsed_child(&child)]).unwrap()
        );
        changed["data"]["utfallswidget"] = json!("unknown widget");
        f.seed_cache_request_for_tests(
            &child_req,
            &serde_json::to_vec(&changed).unwrap(),
            "application/json",
        );
        assert!(adapter
            .discover_with_key(&source(), &f, Some(1), api_key)
            .await
            .is_err());
        // Child includes a grandchild and an FHI link, neither seeded: offline success above proves no recursive fetch.
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod gzip_body_test {
    //! Full plan 2026-11-17: "HAPI response JSON invalid: expected value at line 1 column 1". The cached response
    //! for one `?full=true` detail was status 200, `application/json; charset=utf-8`, 3220 bytes starting 1f 8b
    //! (gzip) and decodes to the expected Infobit JSON.
    use super::*;
    use std::io::Write;
    const INFOBIT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/no_helsedirektoratet/infobit.json"));
    fn page(body: Vec<u8>) -> Fetched {
        Fetched {
            record: FetchRecord { status: 200, final_url: format!("{ENDPOINT}/sample-1?full=true"), content_type: Some("application/json; charset=utf-8".into()), ..Default::default() },
            body,
        }
    }
    #[test]
    fn gzip_magic_body_is_decoded_and_plain_json_is_unchanged() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(INFOBIT.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(&gz[..2], &[0x1f, 0x8b]);
        let plain: Value = serde_json::from_str(INFOBIT).unwrap();
        assert_eq!(checked_json(&page(gz.clone())).unwrap(), plain);
        assert_eq!(checked_json(&page(INFOBIT.as_bytes().to_vec())).unwrap(), plain);
        // Truncated gzip is an error, never partial JSON.
        assert!(checked_json(&page(gz[..gz.len() / 2].to_vec())).is_err());
        // Non-JSON, non-gzip bytes still fail as before.
        assert!(checked_json(&page(b"<html>".to_vec())).is_err());
    }
}
