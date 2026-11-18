//! Anonymous ACEP SSO entry-point inspection, using the production polite fetcher.
//! No login, form submission, cookie jar, scripts, credentials, or raw HTML output.
//! Run with umask 077 and absolute, owned private --cache and --out paths.
//! The entry GET is uncached. The shared fetcher may cache public robots.txt only.
//! Example tests are pure/offline; the included fetcher also has its own tests.

use scraper::{Html, Selector};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use url::Url;

// Match the actual src/guidelines/doc.rs receipt type, without pulling in the
// production ingest graph. Never serialize this raw record in this example.
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

const CLINICAL_PATH: &str = "/patient-care/clinical-policies/acute-blunt-trauma";
const DEFAULT_URL: &str = "https://www.acep.org/api/services/sso/login?ReturnUrl=%2Fpatient-care%2Fclinical-policies%2Facute-blunt-trauma";

struct Args {
    url: String,
    cache: PathBuf,
    out: PathBuf,
}

fn parse_args() -> Result<Args, ()> {
    let mut args = std::env::args_os().skip(1);
    let mut url = None;
    let mut cache = None;
    let mut out = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(())?;
        match flag.to_str() {
            Some("--url") if url.is_none() => url = Some(value.into_string().map_err(|_| ())?),
            Some("--cache") if cache.is_none() => cache = Some(PathBuf::from(value)),
            Some("--out") if out.is_none() => out = Some(PathBuf::from(value)),
            _ => return Err(()),
        }
    }
    Ok(Args { url: url.unwrap_or_else(|| DEFAULT_URL.into()), cache: cache.ok_or(())?, out: out.ok_or(())? })
}

fn validate_source(raw: &str) -> Result<Url, ()> {
    // URL parsing normalizes away an empty userinfo marker, so reject that
    // spelling before parsing as well as checking parsed username/password.
    let authority = raw.split_once("://").ok_or(())?.1
        .split(['/', '?', '#']).next().ok_or(())?;
    if authority.contains('@') { return Err(()); }
    let url = Url::parse(raw).map_err(|_| ())?;
    if url.scheme() != "https"
        || url.host_str() != Some("www.acep.org")
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.path() != "/api/services/sso/login"
    {
        return Err(());
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    if url.query().is_some() && (pairs.len() != 1 || pairs[0].0 != "ReturnUrl") {
        return Err(());
    }
    if let Some((_, value)) = pairs.first() {
        // Decode ReturnUrl once, and require the known first-party clinical
        // destination. No open redirect, nested URL, fragment or query allowed.
        if value.as_ref() != CLINICAL_PATH {
            return Err(());
        }
        let destination = url.join(value).map_err(|_| ())?;
        if destination.origin() != url.origin() || destination.path() != CLINICAL_PATH
            || destination.query().is_some() || destination.fragment().is_some()
        {
            return Err(());
        }
    }
    Ok(url)
}

// Only query parameter names survive. Never return a raw URL or URL parse error.
fn url_shape(url: &Url) -> Value {
    if !matches!(url.scheme(), "https" | "http") {
        return json!({"omitted": "non_http_url"});
    }
    let names: BTreeSet<String> = url.query_pairs().map(|(key, _)| identifier(&key)).collect();
    json!({"origin": url.origin().ascii_serialization(), "path": url.path(), "query_names": names})
}

fn reference_shape(base: &Url, raw: &str) -> Value {
    match base.join(raw) {
        Ok(url) => url_shape(&url),
        Err(_) => json!({"omitted": "invalid_url"}),
    }
}

// Attribute values from name/id/type are structural identifiers only. Arbitrary
// data, URL-like identifiers and unusually long strings are not retained.
fn identifier(raw: &str) -> String {
    if !raw.is_empty() && raw.len() <= 128
        && raw.chars().all(|c| c.is_ascii_alphanumeric() || "_-.$:[]".contains(c))
        && !raw.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| part.len() > 48)
    {
        raw.into()
    } else {
        "<omitted>".into()
    }
}

fn short_text(raw: &str) -> Option<String> {
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() || text.contains(['=', '?', '@', '%'])
        || text.contains("://")
        || text.split_whitespace().any(|word| word.len() > 32)
    {
        return None;
    }
    Some(text.chars().take(120).collect())
}

fn selector(raw: &str) -> Selector {
    // All callers use fixed selectors, never untrusted strings.
    Selector::parse(raw).expect("fixed selector")
}

fn dom_metadata(body: &[u8], document_url: &Url) -> Value {
    let dom = Html::parse_document(&String::from_utf8_lossy(body));
    let base = dom.select(&selector("base[href]")).next()
        .and_then(|el| el.value().attr("href"))
        .and_then(|raw| document_url.join(raw).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .unwrap_or_else(|| document_url.clone());
    let heading = |tag: &str| dom.select(&selector(tag)).next()
        .and_then(|el| short_text(&el.text().collect::<Vec<_>>().join(" ")));
    let forms: Vec<Value> = dom.select(&selector("form")).take(16).map(|form| {
        let method = match form.value().attr("method").unwrap_or("get").to_ascii_lowercase().as_str() {
            "get" => "GET", "post" => "POST", "dialog" => "DIALOG", _ => "OTHER",
        };
        let action = match form.value().attr("action").filter(|raw| !raw.is_empty()) {
            Some(raw) => reference_shape(&base, raw),
            None => url_shape(document_url),
        };
        let inputs: Vec<Value> = form.select(&selector("input")).take(128).map(|input| {
            let attr = |name| input.value().attr(name).map(identifier);
            json!({"name": attr("name"), "type": attr("type").unwrap_or_else(|| "text".into()), "id": attr("id")})
        }).collect();
        json!({"method": method, "action": action, "inputs": inputs})
    }).collect();
    let references = |tag: &str, attr: &str, limit: usize| -> Vec<Value> {
        dom.select(&selector(tag)).take(limit).filter_map(|el| el.value().attr(attr))
            .map(|raw| reference_shape(&base, raw)).collect()
    };
    json!({
        "title": heading("title"), "h1": heading("h1"), "forms": forms,
        "iframes": references("iframe[src]", "src", 32),
        "scripts": references("script[src]", "src", 128),
        "links": references("a[href]", "href", 64),
        "limits": {"forms":16,"inputs_per_form":128,"iframes":32,"scripts":128,"links":64,"heading_chars":120}
    })
}

fn mime(raw: Option<&str>) -> Option<String> {
    let value = raw?.split(';').next()?.trim().to_ascii_lowercase();
    (value.len() <= 100 && value.contains('/')
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "/!#$&^_.+-".contains(c)))
        .then_some(value)
}

#[cfg(unix)]
fn private_directory(path: &Path, create: bool) -> Result<(), ()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if !path.is_absolute() { return Err(()); }
    if create && !path.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(path).map_err(|_| ())?;
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ())?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let current_uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != current_uid || metadata.mode() & 0o077 != 0 {
        return Err(());
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_directory(_: &Path, _: bool) -> Result<(), ()> { Err(()) }

fn write_receipt(path: &Path, receipt: &Value) -> Result<(), ()> {
    use std::io::Write;
    if !path.is_absolute() { return Err(()); }
    private_directory(path.parent().ok_or(())?, false)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| ())?;
    let bytes = serde_json::to_vec_pretty(receipt).map_err(|_| ())?;
    file.write_all(&bytes).map_err(|_| ())?;
    file.write_all(b"\n").map_err(|_| ())?;
    Ok(())
}

async fn inspect(args: &Args) -> Result<Value, &'static str> {
    let url = validate_source(&args.url).map_err(|_| "invalid_source")?;
    private_directory(&args.cache, true).map_err(|_| "invalid_private_cache")?;
    // Plain Fetcher::new means there is no session or cookie jar. Its existing
    // robots, user agent, pacing, retry and redirect policies apply unchanged.
    let fetcher = fetch::Fetcher::new(&args.cache, false, 500, 2 * 1024 * 1024, "")
        .map_err(|_| "fetch_setup_failed")?;
    let mut request = fetch::Req::get(url.as_str());
    request.uncached = true;
    let fetched = fetcher.request(&request).await.map_err(|_| "fetch_failed")?;
    let final_url = Url::parse(&fetched.record.final_url).map_err(|_| "invalid_response_metadata")?;
    let chain: Vec<Value> = fetched.record.redirects.iter()
        .chain(std::iter::once(&fetched.record.final_url))
        .map(|raw| match Url::parse(raw) {
            Ok(url) => url_shape(&url),
            Err(_) => json!({"omitted": "invalid_url"}),
        }).collect();
    let content_type = mime(fetched.record.content_type.as_deref());
    let dom = if fetched.is_html() { dom_metadata(&fetched.body, &final_url) } else { Value::Null };
    Ok(json!({
        "status": "ok", "mode": "anonymous_entry_contract", "source": url_shape(&url),
        "redirect_chain": chain, "http_status": fetched.record.status,
        "mime": content_type, "bytes": fetched.record.bytes, "sha256": fetched.record.sha256,
        "fetched_at": fetched.record.fetched_at, "dom": dom,
        "entry_response_cached": false, "cookie_jar": false,
        "cache_scope": "shared_fetcher_public_robots_only",
        "scope": "anonymous GET only; DOM metadata does not establish a working login contract"
    }))
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(()) => {
            println!("{{\"status\":\"error\",\"code\":\"invalid_arguments\",\"options\":[\"--url\",\"--cache\",\"--out\"]}}");
            std::process::exit(2);
        }
    };
    // Validate receipt storage before performing a network request. Existing
    // receipts are never overwritten, and neither URLs nor errors are echoed.
    if !args.out.is_absolute() || args.out.exists()
        || args.out.parent().is_none_or(|parent| private_directory(parent, false).is_err())
    {
        println!("{{\"status\":\"error\",\"code\":\"invalid_private_output\"}}");
        std::process::exit(2);
    }
    let receipt = match inspect(&args).await {
        Ok(receipt) => receipt,
        Err(code) => json!({"status": "error", "code": code}),
    };
    if write_receipt(&args.out, &receipt).is_err() {
        println!("{{\"status\":\"error\",\"code\":\"receipt_write_failed\"}}");
        std::process::exit(2);
    }
    let ok = receipt["status"] == "ok";
    println!("{}", json!({"status": if ok { "ok" } else { "error" }, "receipt_written": true}));
    if !ok { std::process::exit(1); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_fragment_and_userinfo_are_omitted() {
        let url = Url::parse("https://name:private-password@identity.example/callback?code=private-code&state=private-state&code=again#private-fragment").unwrap();
        let shape = url_shape(&url);
        assert_eq!(shape, json!({"origin":"https://identity.example","path":"/callback","query_names":["code","state"]}));
        assert!(!shape.to_string().contains("private"));
        assert_eq!(reference_shape(&url, "javascript:private-script"), json!({"omitted":"non_http_url"}));
    }

    #[test]
    fn dom_retains_input_structure_without_values_or_inline_scripts() {
        let html = br#"<title>Sign in</title><h1>ACEP Sign in</h1>
            <form method="post" action="/submit?state=private-state#private-fragment">
            <input name="__RequestVerificationToken" type="hidden" id="csrf" value="private-csrf">
            <input name="password" type="password" value="private-password"></form>
            <script>private-inline-script</script><script src="https://cdn.example/login.js?token=private-script"></script>
            <iframe src="/frame?code=private-code"></iframe>"#;
        let dom = dom_metadata(html, &Url::parse(DEFAULT_URL).unwrap());
        let encoded = dom.to_string();
        assert!(!encoded.contains("private"));
        assert!(!encoded.contains("\"value\""));
        assert_eq!(dom["forms"][0]["method"], "POST");
        assert_eq!(dom["forms"][0]["inputs"][0]["name"], "__RequestVerificationToken");
        assert_eq!(dom["forms"][0]["inputs"][0]["id"], "csrf");
        assert_eq!(dom["forms"][0]["action"]["query_names"], json!(["state"]));
        assert_eq!(dom["scripts"][0]["origin"], "https://cdn.example");
    }

    #[test]
    fn source_is_fixed_to_first_party_entry_and_known_relative_return() {
        assert!(validate_source(DEFAULT_URL).is_ok());
        assert!(validate_source("https://www.acep.org/api/services/sso/login").is_ok());
        for raw in [
            "http://www.acep.org/api/services/sso/login",
            "https://www.acep.org:8443/api/services/sso/login",
            "https://www.acep.org.attacker.example/api/services/sso/login",
            "https://user:password@www.acep.org/api/services/sso/login",
            "https://@www.acep.org/api/services/sso/login",
            "https://www.acep.org/other",
            "https://www.acep.org/api/services/sso/login#fragment",
            "https://www.acep.org/api/services/sso/login?ReturnUrl=https%3A%2F%2Fother.example%2F",
            "https://www.acep.org/api/services/sso/login?ReturnUrl=%2F%2Fother.example%2F",
            "https://www.acep.org/api/services/sso/login?ReturnUrl=%252Fpatient-care%252Fclinical-policies%252Facute-blunt-trauma",
            "https://www.acep.org/api/services/sso/login?ReturnUrl=%2Fpatient-care%2Fclinical-policies%2Facute-blunt-trauma&token=secret",
        ] {
            assert!(validate_source(raw).is_err(), "unexpected accepted source");
        }
    }
}
