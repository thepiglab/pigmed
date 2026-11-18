//! Bounded bulk HTTP for the public PMC bucket and the NCBI FTP-over-HTTPS mirror.
//! No E-utilities, proxies, IP rotation or scraping. Mirrors `src/pigmed/net.py` policy:
//! host allowlist, no redirects, retry on 429/5xx with Retry-After, MD5 + SHA-256 checks.
use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use md5::Md5;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{sleep, Instant};
use url::Url;

pub const PMC_BUCKET: &str = "pmc-oa-opendata";
pub const PMC_BASE: &str = "https://pmc-oa-opendata.s3.amazonaws.com/";
const ALLOWED_HOSTS: &[&str] = &[
    "pmc-oa-opendata.s3.amazonaws.com",
    "pmc-oa-opendata.s3.us-east-1.amazonaws.com",
    "ftp.ncbi.nlm.nih.gov",
];

/// Python `quote(key, safe="/")`: keep unreserved `A-Za-z0-9_.-~` and `/`.
const S3_KEY: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~').remove(b'/');

pub fn s3_url(key: &str) -> String {
    format!("{PMC_BASE}{}", utf8_percent_encode(key, S3_KEY))
}

pub fn https_url(raw: &str) -> Result<String> {
    let u = Url::parse(raw).with_context(|| format!("bad url {raw:?}"))?;
    if u.scheme() == "s3" && u.host_str() == Some(PMC_BUCKET) {
        let mut out = format!("https://{PMC_BUCKET}.s3.amazonaws.com{}", u.path());
        if let Some(q) = u.query() {
            out.push('?');
            out.push_str(q);
        }
        return Ok(out);
    }
    if u.scheme() != "https" || !u.host_str().map_or(false, |h| ALLOWED_HOSTS.contains(&h)) {
        bail!("Not an authorized bulk-source URL: {raw:?}");
    }
    Ok(raw.to_string())
}

pub fn expected_md5(raw: &str) -> Result<Option<String>> {
    let u = Url::parse(raw)?;
    for (k, v) in u.query_pairs() {
        if k == "md5" {
            if v.len() != 32 || !v.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("Malformed source MD5");
            }
            return Ok(Some(v.to_lowercase()));
        }
    }
    Ok(None)
}

fn retry_delay(header: Option<&str>, attempt: u32) -> Duration {
    if let Some(v) = header {
        if let Ok(s) = v.trim().parse::<f64>() {
            return Duration::from_secs_f64(s.clamp(0.0, 600.0));
        }
        if let Ok(t) = httpdate::parse_http_date(v) {
            let d = t.duration_since(std::time::SystemTime::now()).unwrap_or_default();
            return d.min(Duration::from_secs(600));
        }
    }
    let base = 2f64.powi(attempt as i32).min(60.0);
    Duration::from_secs_f64(base + rand::thread_rng().gen::<f64>())
}

/// Bytes received by `Http::download` in this process (all attempts), for run accounting.
pub static BYTES_RECEIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Downloaded {
    pub bytes: u64,
    pub sha256: String,
    pub md5: String,
    pub url: String,
}

/// Errors that a restart may fix (network) vs. content problems (size, checksum after retries).
#[derive(Debug)]
pub struct TooLarge(pub String);
impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for TooLarge {}

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    retries: u32,
    /// Global cap on simultaneous requests from this process.
    permits: Arc<Semaphore>,
    /// Minimum spacing between request starts (process-wide).
    interval: Duration,
    last: Arc<Mutex<Instant>>,
}

impl Http {
    pub fn new(contact: &str, concurrency: usize, interval_ms: u64) -> Result<Self> {
        let ua = if contact.is_empty() {
            format!("PigMed/{}", env!("CARGO_PKG_VERSION"))
        } else {
            format!("PigMed/{} (mailto:{contact})", env!("CARGO_PKG_VERSION"))
        };
        let client = reqwest::Client::builder()
            .user_agent(ua)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(concurrency.max(1))
            .build()?;
        Ok(Self {
            client,
            retries: 5,
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            interval: Duration::from_millis(interval_ms),
            last: Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1))),
        })
    }

    async fn pace(&self) {
        let mut last = self.last.lock().await;
        let next = *last + self.interval;
        let now = Instant::now();
        if next > now {
            sleep(next - now).await;
        }
        *last = Instant::now();
    }

    /// GET with retries; returns the response for streaming (caller holds the permit).
    async fn send(&self, url: &str) -> Result<reqwest::Response> {
        let url = https_url(url)?;
        let mut last_err = None;
        for attempt in 0..self.retries {
            self.pace().await;
            match self.client.get(&url).send().await {
                Ok(r) => {
                    let st = r.status().as_u16();
                    if matches!(st, 429 | 500 | 502 | 503 | 504) {
                        let d = retry_delay(r.headers().get("retry-after").and_then(|v| v.to_str().ok()), attempt);
                        last_err = Some(anyhow!("HTTP {st}"));
                        sleep(d).await;
                        continue;
                    }
                    if (300..400).contains(&st) {
                        bail!("Bulk source redirected; inspect the source adapter before continuing: {url}");
                    }
                    if !r.status().is_success() {
                        bail!("HTTP {st} for {url}");
                    }
                    return Ok(r);
                }
                Err(e) => {
                    last_err = Some(e.into());
                    sleep(retry_delay(None, attempt)).await;
                }
            }
        }
        Err(anyhow!("Retry budget exhausted for {url}: {:?}", last_err))
    }

    pub async fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let _p = self.permits.acquire().await?;
        let mut last_err = None;
        for attempt in 0..self.retries {
            let r = self.send(url).await?;
            let mut out = Vec::new();
            let mut stream = r.bytes_stream();
            let mut failed = false;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(c) => {
                        out.extend_from_slice(&c);
                        if out.len() as u64 > max_bytes {
                            return Err(TooLarge(format!("Response exceeds configured memory bound: {url}")).into());
                        }
                    }
                    Err(e) => {
                        last_err = Some(e);
                        failed = true;
                        break;
                    }
                }
            }
            if !failed {
                return Ok(out);
            }
            sleep(retry_delay(None, attempt)).await;
        }
        Err(anyhow!("Body read retry budget exhausted for {url}: {:?}", last_err))
    }

    pub async fn json(&self, url: &str) -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(&self.get(url, 16 << 20).await?)?)
    }

    /// Stream to `target` with whole-object restart on failure; verifies size, MD5 and SHA-256.
    pub async fn download(&self, raw_url: &str, target: &Path, max_bytes: u64, md5: Option<&str>) -> Result<Downloaded> {
        let url = https_url(raw_url)?;
        // Some("") = skip checksum verification (caller records why); None = use the URL's ?md5=.
        let md5 = match md5 {
            Some("") => None,
            Some(m) => Some(m.to_lowercase()),
            None => expected_md5(&url)?,
        };
        if let Some(p) = target.parent() {
            tokio::fs::create_dir_all(p).await?;
        }
        let tmp = target.with_file_name(format!("{}.partial", target.file_name().unwrap().to_string_lossy()));
        let _p = self.permits.acquire().await?;
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..self.retries {
            let res: Result<Downloaded> = async {
                let r = self.send(&url).await?;
                let length = r.content_length();
                if let Some(l) = length {
                    if l > max_bytes {
                        return Err(TooLarge(format!("Object exceeds {max_bytes} bytes: {url}")).into());
                    }
                }
                let mut f = tokio::fs::File::create(&tmp).await?;
                let (mut h5, mut h256, mut size) = (Md5::new(), Sha256::new(), 0u64);
                let mut stream = r.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let c = chunk?;
                    size += c.len() as u64;
                    BYTES_RECEIVED.fetch_add(c.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    if size > max_bytes {
                        return Err(TooLarge(format!("Object exceeds {max_bytes} bytes: {url}")).into());
                    }
                    h5.update(&c);
                    h256.update(&c);
                    f.write_all(&c).await?;
                }
                f.flush().await?;
                drop(f);
                if let Some(l) = length {
                    if l != size {
                        bail!("Truncated HTTP body");
                    }
                }
                let got5 = hex::encode(h5.finalize());
                if let Some(m) = &md5 {
                    if &got5 != m {
                        bail!("Source MD5 mismatch");
                    }
                }
                tokio::fs::rename(&tmp, target).await?;
                Ok(Downloaded { bytes: size, sha256: hex::encode(h256.finalize()), md5: got5, url: url.clone() })
            }
            .await;
            match res {
                Ok(d) => return Ok(d),
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    if e.downcast_ref::<TooLarge>().is_some() || e.to_string().contains("Not an authorized") || e.to_string().contains("redirected") {
                        return Err(e);
                    }
                    // Two identical mismatches mean the object changed, not a corrupt transfer.
                    if e.to_string().contains("MD5 mismatch") && attempt >= 1 {
                        return Err(anyhow!("Source MD5 mismatch (object differs from expected checksum): {url}"));
                    }
                    last_err = Some(e);
                    sleep(retry_delay(None, attempt)).await;
                }
            }
        }
        Err(anyhow!("Download retry budget exhausted: {url}: {:?}", last_err))
    }

    /// S3 ListObjectsV2 over the public bucket (anonymous).
    pub async fn list_s3(&self, prefix: &str, delimiter: Option<&str>) -> Result<Vec<(Option<String>, Option<String>)>> {
        self.list_s3_sized(prefix, delimiter, None).await
    }

    /// `list_s3`, also collecting each object's Size (one entry per `Contents`, in order) when `sizes` is given.
    pub async fn list_s3_sized(&self, prefix: &str, delimiter: Option<&str>, mut sizes: Option<&mut Vec<u64>>)
        -> Result<Vec<(Option<String>, Option<String>)>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut u = Url::parse(PMC_BASE)?;
            {
                let mut q = u.query_pairs_mut();
                q.append_pair("list-type", "2").append_pair("prefix", prefix).append_pair("max-keys", "1000");
                if let Some(d) = delimiter {
                    q.append_pair("delimiter", d);
                }
                if let Some(t) = &token {
                    q.append_pair("continuation-token", t);
                }
            }
            let body = self.get(u.as_str(), 64 << 20).await?;
            let text = String::from_utf8(body)?;
            let doc = roxmltree::Document::parse(&text)?;
            token = None;
            for n in doc.root_element().children().filter(|n| n.is_element()) {
                match n.tag_name().name() {
                    "Contents" => {
                        let key = n.children().find(|c| c.has_tag_name("Key")).and_then(|c| c.text()).map(String::from);
                        if let Some(sizes) = sizes.as_deref_mut() {
                            let sz = n.children().find(|c| c.has_tag_name("Size")).and_then(|c| c.text()).and_then(|t| t.parse().ok());
                            sizes.push(sz.unwrap_or(0));
                        }
                        out.push((key, None));
                    }
                    "CommonPrefixes" => {
                        let p = n.children().find(|c| c.has_tag_name("Prefix")).and_then(|c| c.text()).map(String::from);
                        out.push((None, p));
                    }
                    "NextContinuationToken" => token = n.text().map(String::from),
                    _ => {}
                }
            }
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls() {
        assert_eq!(s3_url("metadata/PMC1.1.json"), "https://pmc-oa-opendata.s3.amazonaws.com/metadata/PMC1.1.json");
        assert_eq!(s3_url("a b+c"), "https://pmc-oa-opendata.s3.amazonaws.com/a%20b%2Bc");
        assert_eq!(
            https_url("s3://pmc-oa-opendata/PMC1.1/x.jpg?md5=83ffa86a228d7eca6e9e44e6243412ca").unwrap(),
            "https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/x.jpg?md5=83ffa86a228d7eca6e9e44e6243412ca"
        );
        assert!(https_url("https://example.com/x").is_err());
        assert!(https_url("http://ftp.ncbi.nlm.nih.gov/x").is_err());
        assert_eq!(
            expected_md5("https://pmc-oa-opendata.s3.amazonaws.com/x?md5=83FFA86A228D7ECA6E9E44E6243412CA").unwrap().unwrap(),
            "83ffa86a228d7eca6e9e44e6243412ca"
        );
    }
}
