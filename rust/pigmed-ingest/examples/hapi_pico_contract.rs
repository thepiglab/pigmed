//! One source-emitted HAPI PICO child contract, development/test access only.
//! Uses the unchanged polite Fetcher. No recursion, query variants or raw clinical output.
//! Raw source bytes remain only in an explicitly private local cache.
use scraper::{Html, Selector};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use url::Url;

#[allow(dead_code)]
mod doc {
    use serde::{Deserialize, Serialize};
    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    pub struct FetchRecord {
        pub url: String,
        pub final_url: String,
        pub redirects: Vec<String>,
        pub status: u16,
        pub content_type: Option<String>,
        pub bytes: u64,
        pub sha256: String,
        pub fetched_at: String,
        pub from_cache: bool,
    }
    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum BlockKind {
        Heading {
            level: u8,
        },
        Paragraph,
        ListItem,
        Recommendation {
            label: Option<String>,
            strength: Option<String>,
            certainty: Option<String>,
            population: Option<String>,
            grading_system: Option<String>,
            qualifier: Option<String>,
        },
        Table {
            caption: Option<String>,
        },
        Figure {
            src_url: Option<String>,
            caption: Option<String>,
        },
        Footnote,
    }
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Block {
        pub block_id: String,
        pub kind: BlockKind,
        pub text: String,
        pub heading_path: Vec<String>,
        pub page: Option<u32>,
    }
}
mod common {
    use sha2::{Digest, Sha256};
    pub fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }
}
#[allow(dead_code)]
#[path = "../src/guidelines/fetch.rs"]
mod fetch;
#[allow(dead_code)]
#[path = "../src/guidelines/html.rs"]
mod html;

const SOURCE:&str="https://api.helsedirektoratet.no/innhold/picoer/0006-0004-66b569fb-28da-4082-a57b-311d58c11c63";
const CHILD_ID: &str = "0006-0004-66b569fb-28da-4082-a57b-311d58c11c63";
const KEY_ENV: &str = "HAPI_PRODUCTION_API_KEY";
const MAX_BYTES: u64 = 8 * 1024 * 1024;
struct Args {
    cache: PathBuf,
    out: PathBuf,
}
fn args() -> Result<Args, ()> {
    let mut iter = std::env::args_os().skip(1);
    let (mut cache, mut out) = (None, None);
    while let Some(flag) = iter.next() {
        let value = iter.next().ok_or(())?;
        match flag.to_str() {
            Some("--cache") if cache.is_none() => cache = Some(PathBuf::from(value)),
            Some("--out") if out.is_none() => out = Some(PathBuf::from(value)),
            _ => return Err(()),
        }
    }
    Ok(Args {
        cache: cache.ok_or(())?,
        out: out.ok_or(())?,
    })
}
fn fixed_url(raw: &str) -> bool {
    if raw != SOURCE {
        return false;
    }
    Url::parse(raw).ok().is_some_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("api.helsedirektoratet.no")
            && u.port_or_known_default() == Some(443)
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    })
}
fn identifier(raw: &str) -> String {
    if !raw.is_empty()
        && raw.len() <= 100
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        raw.into()
    } else {
        "<omitted>".into()
    }
}
fn url_path(raw: &str) -> Value {
    match Url::parse(SOURCE).ok().and_then(|b| b.join(raw).ok()) {
        Some(u)
            if matches!(u.scheme(), "https" | "http")
                && u.username().is_empty()
                && u.password().is_none() =>
        {
            json!({"origin":u.origin().ascii_serialization(),"path":u.path()})
        }
        _ => json!({"omitted":"invalid_or_non_http_url"}),
    }
}
fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
fn fields(value: &Value) -> Value {
    match value.as_object() {
        Some(map) => json!(map
            .iter()
            .map(|(k, v)| json!({"field":identifier(k),"type":value_type(v)}))
            .collect::<Vec<_>>()),
        None => Value::Null,
    }
}
fn block_kind(kind: &doc::BlockKind) -> &'static str {
    use doc::BlockKind::*;
    match kind {
        Heading { .. } => "heading",
        Paragraph => "paragraph",
        ListItem => "list_item",
        Recommendation { .. } => "recommendation",
        Table { .. } => "table",
        Figure { .. } => "figure",
        Footnote => "footnote",
    }
}
fn component_shape(path: &str, raw: &str) -> Value {
    let dom = Html::parse_fragment(raw);
    let mut tags: BTreeMap<String, usize> = BTreeMap::new();
    for e in dom.select(&Selector::parse("*").unwrap()) {
        *tags.entry(e.value().name().into()).or_default() += 1;
    }
    let wrapped = format!("<div id=\"pico-component\">{raw}</div>");
    let (blocks, warnings) = html::blocks_from_html(
        &wrapped,
        Some(&Url::parse(SOURCE).unwrap()),
        Some("#pico-component"),
    );
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for b in &blocks {
        *counts.entry(block_kind(&b.kind).into()).or_default() += 1;
    }
    json!({"field_path":path,"source_html_sha256":common::sha256_hex(raw.as_bytes()),"source_html_bytes":raw.len(),"block_count":blocks.len(),"block_kind_counts":counts,"tag_counts":tags,"structural_warning_count":warnings.len(),"clinical_mapping":"unknown; shape inspection only"})
}
fn clinical_html_field(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "tekst"
            | "intro"
            | "populasjon"
            | "population"
            | "intervensjon"
            | "intervention"
            | "tiltak"
            | "sammenligning"
            | "comparison"
            | "utfall"
            | "outcome"
            | "beskrivelse"
            | "description"
            | "praktisk"
            | "rasjonale"
            | "forskningsgrunnlag"
            | "evidens"
            | "begrunnelse"
    )
}
fn inspect_fields(
    value: &Value,
    path: &str,
    depth: usize,
    components: &mut Vec<Value>,
    links: &mut Vec<Value>,
) {
    if depth > 8 || components.len() > 128 || links.len() > 128 {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let name = identifier(key);
                let next = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}.{name}")
                };
                if key == "links" {
                    if let Some(items) = value.as_array() {
                        for item in items.iter().take(128) {
                            if let Some(href) = item.get("href").and_then(Value::as_str) {
                                links.push(json!({"field_path":format!("{next}[].href"),"url":url_path(href),"rel":item.get("rel").and_then(Value::as_str).map(identifier),"type":item.get("type").and_then(Value::as_str).map(identifier),"fetched":false}));
                            }
                        }
                    }
                }
                if clinical_html_field(key) {
                    if let Some(raw) = value
                        .as_str()
                        .filter(|s| s.contains('<') && s.contains('>'))
                    {
                        components.push(component_shape(&next, raw));
                    }
                }
                // Do not inspect or fingerprint personal/owner/contact fields or arbitrary scalars.
                if matches!(
                    key.as_str(),
                    "data"
                        | "utfall"
                        | "outcomes"
                        | "pico"
                        | "populasjon"
                        | "intervensjon"
                        | "tiltak"
                        | "sammenligning"
                ) {
                    inspect_fields(value, &next, depth + 1, components, links);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter().take(128) {
                inspect_fields(item, &format!("{path}[]"), depth + 1, components, links)
            }
        }
        _ => {}
    }
}
fn mime(raw: Option<&str>) -> Option<String> {
    let s = raw?.split(';').next()?.trim().to_ascii_lowercase();
    (s.len() <= 100
        && s.contains('/')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/!#$&^_.+-".contains(c)))
    .then_some(s)
}
fn response_shape(f: &fetch::Fetched) -> Value {
    let content_type = mime(f.record.content_type.as_deref());
    let origin_ok =
        fixed_url(&f.record.final_url) && f.record.redirects.iter().all(|u| fixed_url(u));
    let status_ok = f.record.status == 200;
    let mime_ok = matches!(
        content_type.as_deref(),
        Some("application/json" | "text/json")
    );
    let sha_ok = f.record.sha256 == common::sha256_hex(&f.body);
    let size_ok = f.body.len() as u64 <= MAX_BYTES && f.record.bytes == f.body.len() as u64;
    let mut receipt = json!({"mode":"authorized_development_test_pico_contract","source":url_path(SOURCE),"http_status":f.record.status,"mime":content_type,"bytes":f.record.bytes,"sha256":f.record.sha256,"from_cache":f.record.from_cache,
        "response_validation":{"fixed_https_source":origin_ok,"http_200":status_ok,"json_mime":mime_ok,"body_sha256_equal":sha_ok,"body_size_valid":size_ok},
        "logical_child_gets":1,"transport_attempt_count":null,"transport_attempt_note":"Shared Fetcher bounded retry/backoff policy; individual transport attempt count is not exposed","recursive_child_gets":0,"clinical_completeness":"unknown","status":"rejected"});
    if !(origin_ok && status_ok && mime_ok && sha_ok && size_ok) {
        return receipt;
    }
    let value: Value = match serde_json::from_slice(&f.body) {
        Ok(v) => v,
        Err(_) => {
            receipt["code"] = json!("invalid_json");
            return receipt;
        }
    };
    if !value.is_object() {
        receipt["code"] = json!("unknown_nonobject_schema");
        return receipt;
    }
    let child_match = value
        .get("id")
        .and_then(Value::as_str)
        .map(|id| id == CHILD_ID);
    let mut components = vec![];
    let mut links = vec![];
    inspect_fields(&value, "", 0, &mut components, &mut links);
    receipt["child_id_matches"] = json!(child_match);
    receipt["root_field_types"] = fields(&value);
    receipt["data_field_types"] = fields(&value["data"]);
    receipt["components"] = json!(components);
    receipt["source_emitted_links"] = json!(links);
    receipt["component_scope"]=json!("Only recognized clinical HTML fields are shaped; missing/unmapped fields and downstream child schemas remain unknown. No clinical text, personal values, key, request headers or raw URLs with query/fragment are exported.");
    receipt["status"] = json!(match child_match {
        Some(true) => "ok",
        Some(false) => "rejected",
        None => "schema_unknown",
    });
    receipt
}
#[cfg(unix)]
fn private_dir(path: &Path, create: bool) -> Result<(), ()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if !path.is_absolute() {
        return Err(());
    }
    if create && !path.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| ())?;
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ())?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(());
    }
    Ok(())
}
#[cfg(not(unix))]
fn private_dir(_: &Path, _: bool) -> Result<(), ()> {
    Err(())
}
fn write_receipt(path: &Path, value: &Value) -> Result<(), ()> {
    use std::io::Write;
    private_dir(path.parent().ok_or(())?, false)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| ())?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(|_| ())?)
        .map_err(|_| ())?;
    file.write_all(b"\n").map_err(|_| ())?;
    Ok(())
}
async fn run(args: &Args) -> Result<Value, &'static str> {
    if !fixed_url(SOURCE) {
        return Err("invalid_fixed_source");
    }
    private_dir(&args.cache, true).map_err(|_| "invalid_private_cache")?;
    let key = std::env::var(KEY_ENV).map_err(|_| "missing_runtime_key")?;
    if key.trim().is_empty() {
        return Err("missing_runtime_key");
    }
    let f = fetch::Fetcher::new(&args.cache, false, 2000, MAX_BYTES, "")
        .map_err(|_| "fetch_setup_failed")?;
    let request = fetch::Req {
        headers: vec![
            ("Ocp-Apim-Subscription-Key".into(), key),
            ("Accept".into(), "application/json".into()),
        ],
        cache_max_age_secs: Some(0),
        ..fetch::Req::get(SOURCE)
    };
    // Exactly one logical source-emitted URL. No parent request, endpoint variants,
    // child follow-up or auth/session mutation. Shared robots/retries are unchanged.
    let fetched = f
        .request(&request)
        .await
        .map_err(|_| "fetch_or_robots_failed")?;
    Ok(response_shape(&fetched))
}
#[tokio::main]
async fn main() {
    let args = match args() {
        Ok(a) => a,
        Err(_) => {
            println!("{{\"status\":\"error\",\"code\":\"invalid_arguments\"}}");
            std::process::exit(2);
        }
    };
    if !args.out.is_absolute()
        || args.out.exists()
        || args
            .out
            .parent()
            .is_none_or(|p| private_dir(p, false).is_err())
    {
        println!("{{\"status\":\"error\",\"code\":\"invalid_private_output\"}}");
        std::process::exit(2);
    }
    let receipt = match run(&args).await {
        Ok(v) => v,
        Err(code) => {
            json!({"status":"error","code":code,"logical_child_get_budget":1,"transport_attempt_count":null,"recursive_child_gets":0})
        }
    };
    if write_receipt(&args.out, &receipt).is_err() {
        println!("{{\"status\":\"error\",\"code\":\"receipt_write_failed\"}}");
        std::process::exit(2);
    }
    let ok = matches!(receipt["status"].as_str(), Some("ok" | "schema_unknown"));
    println!(
        "{}",
        json!({"status":receipt["status"],"receipt_written":true,"raw_clinical_output":false})
    );
    if !ok {
        std::process::exit(1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fetched(value: Value) -> fetch::Fetched {
        let body = serde_json::to_vec(&value).unwrap();
        fetch::Fetched {
            record: doc::FetchRecord {
                url: SOURCE.into(),
                final_url: SOURCE.into(),
                status: 200,
                content_type: Some("application/json; charset=utf-8".into()),
                bytes: body.len() as u64,
                sha256: common::sha256_hex(&body),
                ..Default::default()
            },
            body,
        }
    }
    #[test]
    fn fixed_origin_identity_mime_and_hash_are_required() {
        assert!(fixed_url(SOURCE));
        for bad in [
            SOURCE.replace("https:", "http:"),
            format!("{SOURCE}?full=true"),
            SOURCE.replace(
                "api.helsedirektoratet.no",
                "api.helsedirektoratet.no.attacker.example",
            ),
            SOURCE.replace("https://", "https://user:secret@"),
            format!("{SOURCE}#secret"),
        ] {
            assert!(!fixed_url(&bad));
        }
        let mut f = fetched(json!({"id":CHILD_ID,"data":{}}));
        assert_eq!(response_shape(&f)["status"], "ok");
        f.record.final_url = "https://other.example/".into();
        assert_eq!(response_shape(&f)["status"], "rejected");
        f.record.final_url = SOURCE.into();
        f.record.content_type = Some("text/html".into());
        assert_eq!(response_shape(&f)["status"], "rejected");
        f.record.content_type = Some("application/json".into());
        f.record.sha256 = "bad".into();
        assert_eq!(response_shape(&f)["status"], "rejected");
        assert_eq!(
            response_shape(&fetched(json!({"id":"different-child"})))["child_id_matches"],
            false
        );
        assert_eq!(
            response_shape(&fetched(json!({"unknownField":[]})))["status"],
            "schema_unknown"
        );
    }
    #[test]
    fn schema_receipt_omits_clinical_personal_and_query_values() {
        let f = fetched(
            json!({"id":CHILD_ID,"tittel":"PRIVATE-CLINICAL-TITLE","eier":{"name":"PRIVATE-PERSON"},"tekst":"<p>PRIVATE-CLINICAL-STATEMENT</p>","data":{"populasjon":"<p>PRIVATE-POPULATION</p>","kontakt":"PRIVATE-CONTACT"},"links":[{"rel":"barn","type":"pico","href":"https://api.helsedirektoratet.no/path?key=PRIVATE-KEY#PRIVATE-FRAGMENT"}]}),
        );
        let r = response_shape(&f);
        let s = r.to_string();
        assert!(!s.contains("PRIVATE"));
        assert!(!s.contains("Ocp-Apim"));
        assert_eq!(r["components"].as_array().unwrap().len(), 2);
        assert_eq!(r["components"][0]["block_count"], 1);
        assert_eq!(r["source_emitted_links"][0]["url"]["path"], "/path");
        assert_eq!(r["recursive_child_gets"], 0);
        assert!(r["transport_attempt_count"].is_null());
    }
}
