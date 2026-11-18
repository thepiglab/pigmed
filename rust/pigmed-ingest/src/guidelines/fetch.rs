//! Polite HTTP fetcher for guideline sites.
//!
//! Unlike `net::Http` (bulk PMC/S3, which refuses redirects), guideline sites redirect constantly, so this client
//! follows up to 10 redirects itself and records the chain. It also:
//! - paces requests per host: the larger of `min_interval_ms` from the source registry (never faster than
//!   1 request / 500 ms) and the robots.txt `Crawl-delay` of the group we obey (ours, else `*`; capped at
//!   [`MAX_CRAWL_DELAY`]),
//! - fetches and obeys robots.txt (`User-agent: *` and our agent token), refusing disallowed paths,
//! - caps body size, retries 429/5xx with backoff (honours Retry-After, seconds or HTTP date),
//! - decodes gzip/deflate bodies a server sends although we never ask for compression (`Content-Encoding`, or
//!   gzip magic on a textual content type), capped at `max_bytes` after decoding,
//! - keeps status and key headers (Server, Retry-After, ...) of non-200 answers: in the error text of
//!   [`Fetcher::get_ok`]-style calls and as a side record `status/<xx>/<sha>.json` in the cache dir,
//! - caches every response body by URL under `cache_dir` (content-addressed body + meta JSON), so development reruns
//!   and tests do not hit the site again; `offline = true` serves only from cache (for tests / reruns),
//! - flags likely login walls ([`looks_like_login_wall`]) so adapters can stop instead of scraping a sign-in page.
//! Explicitly authorized API adapters may supply runtime credential headers. Those headers
//! stay on the original origin and are redacted from request diagnostics.
use super::doc::FetchRecord;
use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use reqwest::cookie::CookieStore;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::sleep;

pub const AGENT_TOKEN: &str = "PigMed-guidelines";
/// Upper bound on an honoured robots.txt `Crawl-delay` (the largest seen across the guideline sources is 10 s);
/// a larger value is clamped so one misconfigured file cannot stall a run for hours.
pub const MAX_CRAWL_DELAY: Duration = Duration::from_secs(120);


pub struct Fetched {
    pub record: FetchRecord,
    pub body: Vec<u8>,
}

impl Fetched {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    /// True only for real PDF bytes (magic within the first 1 KB). A `.pdf` URL or a PDF content type that serves an
    /// HTML viewer/wrapper is not a PDF; see [`Fetched::declared_pdf_but_not`].
    pub fn is_pdf(&self) -> bool {
        self.body.iter().take(1024).collect::<Vec<_>>().windows(4).any(|w| w == [&b'%', &b'P', &b'D', &b'F'])
    }
    /// Content type or URL says PDF, bytes are not: a viewer page, download wrapper or error shell.
    pub fn declared_pdf_but_not(&self) -> bool {
        let ct = self.record.content_type.as_deref().is_some_and(|c| c.contains("pdf"));
        let ext = self.record.final_url.split(['?', '#']).next().is_some_and(|u| u.to_ascii_lowercase().ends_with(".pdf"));
        (ct || ext) && !self.is_pdf()
    }
    pub fn is_html(&self) -> bool {
        self.record.content_type.as_deref().is_some_and(|c| c.contains("html"))
            || self.body.iter().take(512).copied().collect::<Vec<u8>>().to_ascii_lowercase().windows(5).any(|w| w == b"<html")
    }
}

#[derive(Default)]
struct Robots {
    /// (allow, path prefix) rules for our group, longest-match wins.
    rules: Vec<(bool, String)>,
    /// robots.txt could not be read as rules (HTML shell, 5xx, network error): nothing on the origin is fetched,
    /// per RFC 9309 ("unreachable" = complete disallow). The reason is reported to the adapter.
    unresolved: Option<String>,
    /// `Crawl-delay` of the selected group (non-standard but widely used; seconds, fractions allowed), clamped to
    /// [`MAX_CRAWL_DELAY`]. Several values in one group: the largest is kept.
    crawl_delay: Option<Duration>,
}

fn parse_crawl_delay(v: &str) -> Option<Duration> {
    let s = v.split_whitespace().next()?.trim_end_matches(['s', 'S']);
    let secs: f64 = s.parse().ok()?;
    (secs.is_finite() && secs > 0.0).then(|| Duration::from_secs_f64(secs).min(MAX_CRAWL_DELAY))
}

impl Robots {
    /// RFC 9309 group selection: the group naming our token wins over `*`; rules of all groups naming the chosen
    /// agent are merged. `Crawl-delay` lines belong to the group they appear in, like rules.
    fn parse(txt: &str) -> Self {
        let mut ours: Vec<(bool, String)> = vec![];
        let mut star: Vec<(bool, String)> = vec![];
        let (mut delay_ours, mut delay_star): (Option<Duration>, Option<Duration>) = (None, None);
        let mut saw_ours = false;
        let (mut g_ours, mut g_star, mut saw_rule) = (false, false, false);
        let token = AGENT_TOKEN.to_ascii_lowercase();
        for line in txt.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "user-agent" => {
                    if saw_rule {
                        (g_ours, g_star, saw_rule) = (false, false, false);
                    }
                    let ua = v.to_ascii_lowercase();
                    if ua == "*" {
                        g_star = true;
                    } else if !ua.is_empty() && token.starts_with(&ua) {
                        g_ours = true;
                        saw_ours = true;
                    }
                }
                "allow" | "disallow" => {
                    saw_rule = true;
                    if !v.is_empty() {
                        if g_ours {
                            ours.push((k == "allow", v.to_string()));
                        }
                        if g_star {
                            star.push((k == "allow", v.to_string()));
                        }
                    }
                }
                "crawl-delay" => {
                    saw_rule = true;
                    if let Some(d) = parse_crawl_delay(v) {
                        if g_ours {
                            delay_ours = delay_ours.max(Some(d));
                        }
                        if g_star {
                            delay_star = delay_star.max(Some(d));
                        }
                    }
                }
                _ => {}
            }
        }
        if saw_ours {
            Self { rules: ours, unresolved: None, crawl_delay: delay_ours }
        } else {
            Self { rules: star, unresolved: None, crawl_delay: delay_star }
        }
    }
    fn allowed(&self, path: &str) -> bool {
        if self.unresolved.is_some() {
            return false;
        }
        Self::allowed_by(&self.rules, path)
    }
    fn allowed_by(rules: &[(bool, String)], path: &str) -> bool {
        let m = |pat: &str| {
            let p = pat.trim_end_matches('$');
            if p.contains('*') {
                let re = regex::escape(p).replace("\\*", ".*");
                regex::Regex::new(&format!("^{re}")).map(|r| r.is_match(path)).unwrap_or(false)
            } else {
                path.starts_with(p)
            }
        };
        rules.iter().filter(|(_, p)| m(p)).max_by_key(|(a, p)| (p.len(), *a)).map_or(true, |(a, _)| *a)
    }
}

pub struct Fetcher {
    client: reqwest::Client,
    user_agent: String,
    session: Option<OriginSession>,
    cache_dir: PathBuf,
    offline: bool,
    min_interval: Duration,
    hosts: Arc<Mutex<HashMap<String, Instant>>>,
    robots: Arc<Mutex<HashMap<String, Arc<Robots>>>>,
    /// Status line and key headers of the last non-200 answer per request URL (for error texts).
    status_notes: Arc<std::sync::Mutex<HashMap<String, String>>>,
    pub max_bytes: u64,
}

struct OriginSession {
    origin: url::Url,
    jar: Arc<reqwest::cookie::Jar>,
}

fn now_iso() -> String {
    httpdate::fmt_http_date(std::time::SystemTime::now())
}

impl Fetcher {
    pub fn new(cache_dir: &Path, offline: bool, min_interval_ms: u64, max_bytes: u64, contact: &str) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)?;
        let ua = if contact.is_empty() {
            format!("{AGENT_TOKEN}/{} (research corpus; respects robots.txt)", env!("CARGO_PKG_VERSION"))
        } else {
            format!("{AGENT_TOKEN}/{} (research corpus; respects robots.txt; {contact})", env!("CARGO_PKG_VERSION"))
        };
        let client = reqwest::Client::builder()
            .user_agent(&ua)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            client,
            user_agent: ua,
            session: None,
            cache_dir: cache_dir.to_path_buf(),
            offline,
            min_interval: Duration::from_millis(min_interval_ms.max(500)),
            hosts: Arc::new(Mutex::new(HashMap::new())),
            robots: Arc::new(Mutex::new(HashMap::new())),
            status_notes: Arc::new(std::sync::Mutex::new(HashMap::new())),
            max_bytes,
        })
    }

    /// A memory-only account session, bound to one HTTPS origin. It shares pacing and robots
    /// with the parent. Account responses are always fresh: no login or authenticated-body replay.
    pub fn with_origin_session(&self, origin: &str, account_identity: &str) -> Result<Self> {
        let origin = url::Url::parse(origin).context("invalid authenticated origin")?;
        if origin.scheme() != "https" || origin.host_str().is_none() || !origin.username().is_empty()
            || origin.password().is_some() || origin.query().is_some() || origin.fragment().is_some()
            || origin.path() != "/" || account_identity.is_empty() {
            bail!("authenticated sessions require an explicit HTTPS origin and account identity");
        }
        let namespace = crate::common::sha256_hex(format!("{}\n{}", origin.origin().ascii_serialization(), account_identity).as_bytes());
        let cache_dir = self.cache_dir.join(format!("session-{namespace}"));
        std::fs::create_dir_all(&cache_dir)?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let client = reqwest::Client::builder().user_agent(&self.user_agent)
            .redirect(reqwest::redirect::Policy::none()).cookie_provider(jar.clone())
            .connect_timeout(Duration::from_secs(20)).read_timeout(Duration::from_secs(120)).build()?;
        Ok(Self { client, user_agent: self.user_agent.clone(), session: Some(OriginSession { origin, jar }),
            cache_dir, offline: self.offline, min_interval: self.min_interval,
            hosts: self.hosts.clone(), robots: self.robots.clone(), status_notes: self.status_notes.clone(), max_bytes: self.max_bytes })
    }

    /// Raw value of a session cookie at the bound origin. Never print it or put it in metadata.
    pub fn session_cookie_value(&self, name: &str) -> Option<String> {
        let session = self.session.as_ref()?;
        self.session_cookie_value_for(session.origin.as_str(), name)
    }

    /// Read a path-scoped cookie only for a URL on the already bound HTTPS origin.
    pub fn session_cookie_value_for(&self, target: &str, name: &str) -> Option<String> {
        let session = self.session.as_ref()?;
        let target = url::Url::parse(target).ok()?;
        if !same_origin(session.origin.as_str(), target.as_str()) || !target.username().is_empty() || target.password().is_some() { return None; }
        let cookies = session.jar.cookies(&target)?;
        cookies.to_str().ok()?.split(';').find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
    }

    fn session_request(&self, req: &Req) -> Result<Req> {
        let mut effective = req.clone();
        if let Some(session) = &self.session {
            let target = url::Url::parse(&req.url).context("invalid authenticated request URL")?;
            if !same_origin(session.origin.as_str(), &req.url) || !target.username().is_empty() || target.password().is_some() {
                bail!("authenticated request leaves its bound HTTPS origin");
            }
            // An explicit cookie also makes the cache key reflect the actual authorization state.
            // reqwest's jar validates Set-Cookie domain/path/expiry; it is never exported.
            effective.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
            if let Some(cookies) = session.jar.cookies(&url::Url::parse(&req.url)?) {
                effective.headers.push(("Cookie".into(), cookies.to_str().context("invalid session cookie header")?.into()));
            }
        }
        Ok(effective)
    }

    fn cache_paths(&self, url: &str) -> (PathBuf, PathBuf) {
        let k = crate::common::sha256_hex(url.as_bytes());
        let d = self.cache_dir.join(&k[..2]);
        (d.join(format!("{k}.body")), d.join(format!("{k}.json")))
    }

    /// True for cache-only fetchers (tests, offline reruns): nothing goes to the network, so callers that add
    /// their own pacing on top of the Fetcher should skip it.
    pub fn is_offline(&self) -> bool {
        self.offline
    }

    /// The robots.txt `Crawl-delay` this fetcher honours for `url`'s origin, once robots.txt has been read
    /// (None before that, or when the selected group states none).
    pub async fn crawl_delay(&self, url: &str) -> Option<Duration> {
        let u = url::Url::parse(url).ok()?;
        self.robots.lock().await.get(&robots_origin(&u)).and_then(|r| r.crawl_delay)
    }

    /// Per-host interval for the next request to `u`: max(registry `min_interval_ms`, robots Crawl-delay).
    async fn host_interval(&self, u: &url::Url) -> Duration {
        let cd = self.robots.lock().await.get(&robots_origin(u)).and_then(|r| r.crawl_delay);
        effective_interval(self.min_interval, cd)
    }

    async fn pace(&self, host: &str, interval: Duration) {
        loop {
            let wait = {
                let mut h = self.hosts.lock().await;
                let now = Instant::now();
                match h.get(host) {
                    Some(t) if *t + interval > now => Some(*t + interval - now),
                    _ => {
                        h.insert(host.to_string(), now);
                        None
                    }
                }
            };
            match wait {
                Some(d) => sleep(d).await,
                None => return,
            }
        }
    }

    async fn robots_for(&self, u: &url::Url) -> Arc<Robots> {
        let origin = robots_origin(u);
        if let Some(r) = self.robots.lock().await.get(&origin) {
            return r.clone();
        }
        // boxed: raw() -> robots_for() -> raw() is async recursion
        let got = Box::pin(self.raw(&Req::get(&format!("{origin}/robots.txt")), false)).await;
        let r = match got {
            Ok(f) if f.record.status == 200 => {
                let t = f.text();
                let head = t.trim_start().chars().take(200).collect::<String>().to_ascii_lowercase();
                if head.starts_with("<!doctype") || head.starts_with("<html") || head.contains("<head") {
                    Robots { unresolved: Some("robots.txt returned an HTML page, not rules".into()), ..Default::default() }
                } else {
                    Robots::parse(&t)
                }
            }
            // 4xx: no robots.txt -> no restrictions (RFC 9309)
            Ok(f) if (400..500).contains(&f.record.status) => Robots::default(),
            Ok(f) => Robots { unresolved: Some(format!("robots.txt HTTP {}", f.record.status)), ..Default::default() },
            // offline runs only serve cached bodies, which were fetched under the robots rules of their time
            Err(_) if self.offline => Robots::default(),
            Err(e) => Robots { unresolved: Some(format!("robots.txt unreachable: {e:#}")), ..Default::default() },
        };
        let r = Arc::new(r);
        self.robots.lock().await.insert(origin, r.clone());
        r
    }

    async fn check_robots(&self, u: &url::Url) -> Result<()> {
        let r = self.robots_for(u).await;
        if let Some(why) = &r.unresolved {
            bail!("robots policy unresolved for {}://{} ({why}); not fetching", u.scheme(), u.host_str().unwrap_or(""));
        }
        let pq = match u.query() { Some(q) => format!("{}?{q}", u.path()), None => u.path().to_string() };
        if !r.allowed(&pq) {
            bail!("robots.txt disallows {u}");
        }
        Ok(())
    }

    /// GET `url` following redirects (each hop must be allowed by robots.txt), with caching.
    pub async fn get(&self, url: &str) -> Result<Fetched> {
        self.request(&Req::get(url)).await
    }

    /// Any request (GET with extra headers, POST of a form or JSON body to a public search/listing endpoint).
    /// Same robots, pacing, redirect, size and cache rules as [`Fetcher::get`]; the cache key covers method, headers
    /// and body. Credential-bearing requests require a source-specific, authorized access contract;
    /// adapters must keep credentials out of URLs, errors, plans and content metadata.
    pub async fn request(&self, req: &Req) -> Result<Fetched> {
        self.session_request(req)?;
        let u = url::Url::parse(&req.url).context("invalid request URL")?;
        if self.session.is_some() {
            self.check_robots(&u).await.map_err(|_|anyhow!("authenticated request is not allowed by applicable robots policy"))?;
        } else { self.check_robots(&u).await?; }
        self.raw(req, true).await
    }

    /// POST a URL-encoded form (e.g. an ASP.NET postback or a public search form) and require HTTP 200 / no login wall.
    pub async fn post_form_ok(&self, url: &str, fields: &[(&str, &str)]) -> Result<Fetched> {
        let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish();
        self.ok(self.request(&Req { method: "POST".into(), url: url.into(),
            headers: vec![("content-type".into(), "application/x-www-form-urlencoded".into())], body: Some(body.into_bytes()), ..Default::default() }).await?)
    }

    /// POST a JSON body to a public API and require HTTP 200.
    pub async fn post_json_ok(&self, url: &str, body: &serde_json::Value) -> Result<Fetched> {
        self.ok(self.request(&Req { method: "POST".into(), url: url.into(),
            headers: vec![("content-type".into(), "application/json".into()), ("accept".into(), "application/json".into())],
            body: Some(serde_json::to_vec(body)?), ..Default::default() }).await?)
    }

    /// GET with an `Accept: application/json` header, require HTTP 200, parse JSON.
    pub async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        let f = self.ok(self.request(&Req { headers: vec![("accept".into(), "application/json".into())], ..Req::get(url) }).await?)?;
        serde_json::from_slice(&f.body).with_context(|| format!("not JSON: {url}"))
    }

    async fn raw(&self, req: &Req, check_robots: bool) -> Result<Fetched> {
        let effective = self.session_request(req)?;
        let req = &effective;
        if self.session.is_some() {
            return self.raw_session(req, check_robots).await;
        }
        let url = req.url.as_str();
        let (body_p, meta_p) = self.cache_paths(&req.cache_key());
        if let (false, Ok(b), Ok(m)) = (req.uncached, std::fs::read(&body_p), std::fs::read(&meta_p)) {
            let mut record: FetchRecord = serde_json::from_slice(&m)?;
            if cache_record_fresh(&record.fetched_at, req.cache_max_age_secs, self.offline) {
                record.from_cache = true;
                return Ok(Fetched { record, body: b });
            }
        }
        if self.offline {
            bail!("offline and not cached: {url}");
        }
        let mut cur = url.to_string();
        let mut redirects = vec![];
        let mut last_err = None;
        for attempt in 0..5u32 {
            if attempt > 0 {
                sleep(Duration::from_secs(2u64.pow(attempt).min(60))).await;
            }
            let u = url::Url::parse(&cur)?;
            let interval = self.host_interval(&u).await;
            self.pace(u.host_str().unwrap_or(""), interval).await;
            // a redirected POST becomes a GET (303, and 301/302 in practice); 307/308 keep the method
            let method = if redirects.is_empty() || req.keep_method_on_redirect { req.method.as_str() } else { "GET" };
            let mut rb = self.client.request(reqwest::Method::from_bytes(method.as_bytes())?, cur.as_str());
            for (k, v) in &req.headers {
                // Let the jar choose fresh cookies for each hop. A login response may rotate
                // the session before a same-origin redirect; the initial cookie is only a cache key.
                if self.session.is_some() && k.eq_ignore_ascii_case("cookie") {
                    continue;
                }
                // Explicit API credentials are scoped to the original origin. A publisher/CDN
                // redirect must never receive the source's key, authorization, or cookie header.
                if !same_origin(&req.url, &cur) && !matches!(k.to_ascii_lowercase().as_str(), "accept" | "accept-language" | "content-type" | "range") {
                    continue;
                }
                rb = rb.header(k.as_str(), v.as_str());
            }
            if method != "GET" {
                if let Some(b) = &req.body {
                    rb = rb.body(b.clone());
                }
            }
            let r = match rb.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(anyhow!(e));
                    continue;
                }
            };
            let st = r.status().as_u16();
            if (300..400).contains(&st) {
                let loc = r.headers().get("location").and_then(|v| v.to_str().ok()).context("redirect without Location")?;
                let next = u.join(loc)?.to_string();
                if req.body.is_some() && req.keep_method_on_redirect && !same_origin(url, &next) {
                    bail!("refusing to forward a request body across origins");
                }
                if self.session.is_some() && !same_origin(url, &next) {
                    bail!("authenticated redirect leaves its bound HTTPS origin");
                }
                if redirects.len() >= 10 {
                    bail!("too many redirects from {url}");
                }
                redirects.push(cur.clone());
                if check_robots {
                    let nu = url::Url::parse(&next)?;
                    self.check_robots(&nu).await.with_context(|| format!("redirect target of {url}"))?;
                }
                cur = next;
                continue;
            }
            if matches!(st, 429 | 500 | 502 | 503 | 504) {
                let ra = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(retry_after);
                let note = self.note_status(&req.cache_key(), url, &cur, st, r.headers());
                last_err = Some(anyhow!("HTTP {st} for {cur}{note} after {} attempt(s)", attempt + 1));
                if let Some(d) = ra {
                    sleep(d.min(Duration::from_secs(300))).await;
                }
                continue;
            }
            let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).map(String::from);
            let ce = r.headers().get("content-encoding").and_then(|v| v.to_str().ok()).map(String::from);
            if st != 200 {
                self.note_status(&req.cache_key(), url, &cur, st, r.headers());
            }
            let declared = r.content_length();
            let mut body = Vec::new();
            let mut s = r.bytes_stream();
            while let Some(c) = s.next().await {
                let c = c?;
                body.extend_from_slice(&c);
                if body.len() as u64 > self.max_bytes {
                    bail!("response exceeds max_bytes ({}) for {url}", self.max_bytes);
                }
            }
            if let Some(n) = declared {
                // a short read is a partial body, never a document (servers also send 206 for ignored ranges)
                if (body.len() as u64) < n && st == 200 {
                    last_err = Some(anyhow!("truncated body for {url}: {} of {n} bytes", body.len()));
                    continue;
                }
            }
            let body = decode_body(body, ce.as_deref(), ct.as_deref(), self.max_bytes).with_context(|| format!("response body of {cur}"))?;
            let record = FetchRecord {
                url: url.to_string(), final_url: cur.clone(), redirects: redirects.clone(), status: st, content_type: ct,
                bytes: body.len() as u64, sha256: crate::common::sha256_hex(&body), fetched_at: now_iso(), from_cache: false,
            };
            if st == 200 && !req.uncached {
                std::fs::create_dir_all(body_p.parent().unwrap())?;
                // A refresh may replace a URL in place. Retain the previous body and receipt by
                // content hash, so an earlier immutable plan still has its original evidence.
                if req.cache_max_age_secs.is_some() {
                    if let (Ok(old_body), Ok(old_meta)) = (std::fs::read(&body_p), std::fs::read(&meta_p)) {
                        let old_hash = crate::common::sha256_hex(&old_body);
                        if old_hash != record.sha256 {
                            let versions = self.cache_dir.join("versions");
                            std::fs::create_dir_all(&versions)?;
                            let previous = versions.join(format!("{old_hash}.body"));
                            if !previous.exists() {
                                std::fs::write(previous, old_body)?;
                                std::fs::write(versions.join(format!("{old_hash}.json")), old_meta)?;
                            }
                        }
                    }
                }
                std::fs::write(&body_p, &body)?;
                std::fs::write(&meta_p, serde_json::to_vec(&record)?)?;
            }
            return Ok(Fetched { record, body });
        }
        Err(last_err.unwrap_or_else(|| anyhow!("retry budget exhausted for {url}")))
    }

    /// Account sessions deliberately do not read/write the response cache or retry submissions.
    /// This avoids replaying CSRF bootstrap pages, cookie-rotation bodies or revoked entitlements.
    async fn raw_session(&self, req: &Req, check_robots: bool) -> Result<Fetched> {
        if self.offline {
            bail!("authenticated sessions require a fresh response; cached account pages are not replayed");
        }
        let session = self.session.as_ref().context("account session missing")?;
        let mut current = url::Url::parse(&req.url).context("invalid authenticated URL")?;
        let mut method = req.method.clone();
        let mut redirects = vec![];
        loop {
            if !same_origin(session.origin.as_str(), current.as_str()) || !current.username().is_empty() || current.password().is_some() {
                bail!("authenticated request leaves its bound HTTPS origin");
            }
            let interval = self.host_interval(&current).await;
            self.pace(current.host_str().unwrap_or(""), interval).await;
            let parsed_method = reqwest::Method::from_bytes(method.as_bytes()).context("invalid authenticated request method")?;
            let mut request = self.client.request(parsed_method, current.as_str());
            // Jar cookies are selected afresh at every hop, including after session rotation.
            for (key, value) in &req.headers {
                if !key.eq_ignore_ascii_case("cookie") { request = request.header(key.as_str(), value.as_str()); }
            }
            if !matches!(method.as_str(), "GET" | "HEAD") {
                if let Some(body) = &req.body { request = request.body(body.clone()); }
            }
            let response = request.send().await.map_err(|_| anyhow!("authenticated HTTP request failed; submission was not retried"))?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if redirects.len() >= 10 { bail!("authenticated redirect limit exceeded"); }
                let location = response.headers().get("location").and_then(|v|v.to_str().ok()).context("authenticated redirect missing Location")?;
                let next = current.join(location).context("invalid authenticated redirect")?;
                if !same_origin(session.origin.as_str(), next.as_str()) || !next.username().is_empty() || next.password().is_some() {
                    bail!("authenticated redirect leaves its bound HTTPS origin");
                }
                if check_robots { self.check_robots(&next).await.map_err(|_|anyhow!("authenticated redirect is not allowed by applicable robots policy"))?; }
                method = redirect_method(&method, status, req.keep_method_on_redirect).to_string();
                redirects.push(session_record_url(current.as_str()));
                current = next;
                continue;
            }
            let content_type = response.headers().get("content-type").and_then(|v|v.to_str().ok()).map(String::from);
            let response_encoding = response.headers().get("content-encoding").and_then(|v|v.to_str().ok()).map(String::from);
            let declared = response.content_length();
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                body.extend_from_slice(&chunk.map_err(|_|anyhow!("authenticated response body incomplete"))?);
                if body.len() as u64 > self.max_bytes { bail!("authenticated response exceeds max_bytes"); }
            }
            if status == 200 && declared.is_some_and(|size| (body.len() as u64) < size) { bail!("authenticated response body truncated"); }
            let encoding = response_encoding.as_deref();
            let body = decode_body(body, encoding, content_type.as_deref(), self.max_bytes).map_err(|_| anyhow!("authenticated response body could not be decoded"))?;
            let record = FetchRecord { url: session_record_url(&req.url), final_url: session_record_url(current.as_str()),
                redirects, status, content_type, bytes: body.len() as u64, sha256: crate::common::sha256_hex(&body),
                fetched_at: now_iso(), from_cache: false };
            return Ok(Fetched { record, body });
        }
    }

    /// Store a response in the cache as if it had been fetched (offline tests and saved fixtures).
    pub fn seed_cache_for_tests(&self, url: &str, body: &[u8], content_type: &str) {
        self.seed_cache_request_for_tests(&Req::get(url), body, content_type)
    }

    /// Like [`Fetcher::seed_cache_for_tests`] for a POST / header-bearing request.
    pub fn seed_cache_request_for_tests(&self, req: &Req, body: &[u8], content_type: &str) {
        let effective = self.session_request(req).unwrap();
        let req = &effective;
        let url = req.url.as_str();
        let (b, m) = self.cache_paths(&req.cache_key());
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&b, body).unwrap();
        let rec = FetchRecord { url: url.into(), final_url: url.into(), status: 200, content_type: Some(content_type.into()),
                                bytes: body.len() as u64, sha256: crate::common::sha256_hex(body), ..Default::default() };
        std::fs::write(&m, serde_json::to_vec(&rec).unwrap()).unwrap();
    }

    /// GET and require HTTP 200 and no login wall.
    pub async fn get_ok(&self, url: &str) -> Result<Fetched> {
        self.ok(self.get(url).await?)
    }

    fn ok(&self, f: Fetched) -> Result<Fetched> {
        if f.record.status != 200 {
            let note = self.status_notes.lock().ok().and_then(|n| n.get(&f.record.url).cloned()).unwrap_or_default();
            bail!("HTTP {} for {}{note}", f.record.status, f.record.url);
        }
        if f.is_html() && looks_like_login_wall(&f.text()) {
            bail!("login wall at {} (source/item needs auth; skip it)", f.record.final_url);
        }
        Ok(f)
    }
}

/// Heuristic: a page whose main content is a sign-in form. Adapters should treat this as "needs auth" and stop.
pub fn looks_like_login_wall(html: &str) -> bool {
    let l = html.to_ascii_lowercase();
    let pw = l.contains("type=\"password\"") || l.contains("type='password'") || l.contains("type=password");
    let words = ["sign in", "log in", "login", "member access", "subscribe to read", "institutional access"];
    pw && words.iter().any(|w| l.contains(w)) && l.len() < 400_000
}

/// One HTTP request. Build GETs with [`Req::get`]; see [`Fetcher::request`].
#[derive(Clone, Default)]
pub struct Req {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Login/token responses must create a fresh session and must not enter the disk cache.
    pub uncached: bool,
    /// Optional freshness bound for public/API response caches. Zero refreshes online while
    /// retaining response evidence. Explicit offline mode may replay the saved response.
    pub cache_max_age_secs: Option<u64>,
    /// keep POST on 301/302 redirects (default: switch to GET, as browsers do)
    pub keep_method_on_redirect: bool,
}

/// Headers that confer account/service access must not cross an origin boundary.
fn credential_header(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "ocp-apim-subscription-key"
        | "api-key" | "x-api-key" | "x-auth-token" | "x-xsrf-token" | "x-csrf-token")
}

fn same_origin(original: &str, target: &str) -> bool {
    match (url::Url::parse(original), url::Url::parse(target)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

fn redirect_method(method: &str, status: u16, keep_post_on_301_302: bool) -> &str {
    match status {
        303 if method != "HEAD" => "GET",
        301 | 302 if method == "POST" && !keep_post_on_301_302 => "GET",
        _ => method,
    }
}

fn cache_record_fresh(fetched_at: &str, max_age: Option<u64>, offline: bool) -> bool {
    if offline || max_age.is_none() { return true; }
    let Some(limit) = max_age.filter(|limit|*limit > 0) else { return false; };
    let Ok(time) = httpdate::parse_http_date(fetched_at) else { return false; };
    std::time::SystemTime::now().duration_since(time).is_ok_and(|age| age < Duration::from_secs(limit))
}

// Authentication callbacks may include one-time codes/state supplied by the server.
// Their path remains useful provenance; query values and fragments never enter records.
fn session_record_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut url) => { url.set_query(None); url.set_fragment(None); let _=url.set_username(""); let _=url.set_password(None); url.to_string() },
        Err(_) => "<invalid-authenticated-url>".into(),
    }
}

// Request diagnostics must not disclose login bodies or credential header values.
impl std::fmt::Debug for Req {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self.headers.iter().map(|(k, v)|
            (k.as_str(), if matches!(k.to_ascii_lowercase().as_str(), "accept" | "content-type" | "x-requested-with") { v.as_str() } else { "<redacted>" })
        ).collect();
        f.debug_struct("Req").field("method", &self.method).field("url", &session_record_url(&self.url))
            .field("headers", &headers).field("body_bytes", &self.body.as_ref().map(Vec::len))
            .field("uncached", &self.uncached)
            .field("cache_max_age_secs", &self.cache_max_age_secs)
            .field("keep_method_on_redirect", &self.keep_method_on_redirect).finish()
    }
}

impl Req {
    pub fn get(url: &str) -> Self {
        Self { method: "GET".into(), url: url.into(), ..Default::default() }
    }
    /// Plain GETs keep the historical key (the URL) so existing caches and fixtures stay valid.
    fn cache_key(&self) -> String {
        if self.method == "GET" && self.headers.is_empty() {
            return self.url.clone();
        }
        let mut k = format!("{} {}\n", self.method, self.url);
        for (h, v) in &self.headers {
            k.push_str(&format!("{}: {v}\n", h.to_ascii_lowercase()));
        }
        if let Some(b) = &self.body {
            k.push_str(&crate::common::sha256_hex(b));
        }
        k
    }
}

pub fn record_json(f: &Fetched) -> Value {
    json!(f.record)
}

/// robots.txt scope: scheme, host and (non-default) port (RFC 9309 2.3).
fn robots_origin(u: &url::Url) -> String {
    match u.port() {
        Some(p) => format!("{}://{}:{p}", u.scheme(), u.host_str().unwrap_or("")),
        None => format!("{}://{}", u.scheme(), u.host_str().unwrap_or("")),
    }
}

/// Per-host request interval: the registry's `min_interval` (already >= 500 ms), raised to the robots.txt
/// Crawl-delay when the site asks for more.
fn effective_interval(min_interval: Duration, crawl_delay: Option<Duration>) -> Duration {
    crawl_delay.map_or(min_interval, |d| d.max(min_interval))
}

/// `Retry-After` as delta-seconds or an HTTP date (RFC 9110 10.2.3); a date in the past means "now".
fn retry_after(v: &str) -> Option<Duration> {
    let v = v.trim();
    if let Ok(s) = v.parse::<u64>() {
        return Some(Duration::from_secs(s));
    }
    let t = httpdate::parse_http_date(v).ok()?;
    Some(t.duration_since(std::time::SystemTime::now()).unwrap_or(Duration::ZERO))
}

/// Response headers worth keeping with a non-200 answer: who answered (Server, CDN ids), what it asks of us
/// (Retry-After, WWW-Authenticate, Location) and what it sent. Never cookies or other credentials.
const KEPT_HEADERS: &[&str] = &["server", "retry-after", "content-type", "content-length", "location", "www-authenticate",
    "cf-ray", "cf-mitigated", "x-cache", "via", "x-amz-cf-id", "x-served-by", "x-robots-tag"];

fn kept_headers(h: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    KEPT_HEADERS.iter().filter_map(|k| {
        let v = h.get(*k)?.to_str().ok()?.trim();
        (!v.is_empty()).then(|| (k.to_string(), v.chars().take(200).collect()))
    }).collect()
}

/// " (server: cloudflare; retry-after: 120)" or "" when nothing worth keeping was sent.
fn status_note(headers: &[(String, String)]) -> String {
    let keep: Vec<String> = headers.iter().filter(|(k, _)| !matches!(k.as_str(), "content-type" | "content-length"))
        .map(|(k, v)| format!("{k}: {v}")).collect();
    if keep.is_empty() { String::new() } else { format!(" ({})", keep.join("; ")) }
}

impl Fetcher {
    /// Remember a non-200 answer: in memory for error texts, and as `status/<xx>/<sha>.json` next to the cache
    /// (never served as a cached response). Returns the note appended to error texts.
    fn note_status(&self, cache_key: &str, url: &str, final_url: &str, status: u16, h: &reqwest::header::HeaderMap) -> String {
        let headers = kept_headers(h);
        let note = status_note(&headers);
        if let Ok(mut n) = self.status_notes.lock() {
            n.insert(url.to_string(), note.clone());
        }
        if self.session.is_none() {
            let k = crate::common::sha256_hex(cache_key.as_bytes());
            let d = self.cache_dir.join("status").join(&k[..2]);
            let rec = json!({"url": url, "final_url": final_url, "status": status, "fetched_at": now_iso(),
                "headers": headers.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<serde_json::Map<String, Value>>()});
            let _ = std::fs::create_dir_all(&d).and_then(|_| std::fs::write(d.join(format!("{k}.json")), serde_json::to_vec(&rec).unwrap_or_default()));
        }
        note
    }
}

/// Undo a transfer encoding the server applied although we never send `Accept-Encoding`: `Content-Encoding: gzip`
/// / `x-gzip` / `deflate`, or (no header) gzip magic on a textual content type (JSON/XML/HTML/text; one HAPI server
/// did this). Real `.gz` downloads (`application/gzip`, octet-stream) are left untouched. The decoded size is capped
/// at `max_bytes`; a truncated or corrupt stream is an error, never a partial document.
pub fn decode_body(body: Vec<u8>, content_encoding: Option<&str>, content_type: Option<&str>, max_bytes: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let enc = content_encoding.unwrap_or("").trim().to_ascii_lowercase();
    let gz_magic = body.len() >= 3 && body[0] == 0x1f && body[1] == 0x8b && body[2] == 0x08;
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let textual = ["json", "xml", "html", "text/", "javascript", "csv"].iter().any(|t| ct.contains(t)) && !ct.contains("gzip");
    let mode = match enc.as_str() {
        "gzip" | "x-gzip" if gz_magic => "gzip",
        // a header that lies (plain body) is ignored: the bytes are what they are
        "gzip" | "x-gzip" => return Ok(body),
        "deflate" => "deflate",
        "br" | "compress" | "x-compress" | "zstd" => bail!("unsupported Content-Encoding {enc} (not requested)"),
        _ if gz_magic && textual => "gzip",
        _ => return Ok(body),
    };
    let mut out = Vec::new();
    let read = match mode {
        "gzip" => flate2::read::MultiGzDecoder::new(&body[..]).take(max_bytes + 1).read_to_end(&mut out),
        _ => {
            let r = flate2::read::ZlibDecoder::new(&body[..]).take(max_bytes + 1).read_to_end(&mut out);
            if r.is_err() {
                out.clear();
                flate2::read::DeflateDecoder::new(&body[..]).take(max_bytes + 1).read_to_end(&mut out)
            } else {
                r
            }
        }
    };
    read.map_err(|e| anyhow!("{mode} body could not be decoded ({e})"))?;
    if out.len() as u64 > max_bytes {
        bail!("decoded {mode} body exceeds max_bytes ({max_bytes})");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn account_sessions_partition_caches_and_refuse_other_origins() {
        let dir = tempfile::tempdir().unwrap();
        let public = Fetcher::new(dir.path(), true, 500, 1 << 20, "").unwrap();
        let account = public.with_origin_session("https://example.org/", "synthetic-account").unwrap();
        let other = public.with_origin_session("https://example.org/", "other-account").unwrap();
        assert_ne!(public.cache_dir, account.cache_dir);
        assert_ne!(account.cache_dir, other.cache_dir);
        let request = Req::get("https://example.org/guideline");
        let before = account.session_request(&request).unwrap().cache_key();
        account.session.as_ref().unwrap().jar.add_cookie_str("session=synthetic-private-cookie; Secure; Path=/", &url::Url::parse("https://example.org/").unwrap());
        let after = account.session_request(&request).unwrap().cache_key();
        assert_ne!(before, after);
        assert_eq!(account.session_cookie_value("session").as_deref(), Some("synthetic-private-cookie"));
        account.session.as_ref().unwrap().jar.add_cookie_str("xsrf=synthetic-path-cookie; Secure; Path=/login", &url::Url::parse("https://example.org/login/").unwrap());
        assert!(account.session_cookie_value("xsrf").is_none());
        assert_eq!(account.session_cookie_value_for("https://example.org/login/", "xsrf").as_deref(), Some("synthetic-path-cookie"));
        assert!(account.session_cookie_value_for("https://other.example.org/login/", "xsrf").is_none());
        account.seed_cache_for_tests(&request.url, b"authorized body", "text/html");
        assert!(account.get(&request.url).await.is_err()); // A fresh jar must never replay saved account content.
        assert!(public.get(&request.url).await.is_err());
        assert!(account.session_request(&Req::get("https://other.example.org/")).is_err());
        assert!(account.session_request(&Req::get("http://example.org/")).is_err());
        assert!(account.session_request(&Req::get("https://example.org:8443/")).is_err());
        let login = Req { uncached: true, ..request };
        assert!(account.raw(&login, false).await.is_err());
    }

    #[test]
    fn account_redirect_methods_and_callback_provenance() {
        assert_eq!(redirect_method("POST", 307, false), "POST");
        assert_eq!(redirect_method("POST", 308, false), "POST");
        assert_eq!(redirect_method("POST", 303, true), "GET");
        assert_eq!(redirect_method("POST", 302, false), "GET");
        assert_eq!(redirect_method("POST", 302, true), "POST");
        assert_eq!(redirect_method("HEAD", 303, false), "HEAD");
        assert_eq!(session_record_url("https://example.org/callback?code=synthetic-secret&state=private#token"), "https://example.org/callback");
        let request=Req::get("https://example.org/callback?code=synthetic-secret#private");
        assert!(!format!("{request:?}").contains("synthetic-secret"));
    }

    #[test]
    fn explicit_cache_freshness_does_not_replay_stale_api_responses() {
        let current = now_iso();
        let stale = httpdate::fmt_http_date(std::time::SystemTime::now() - Duration::from_secs(172800));
        assert!(cache_record_fresh(&current, Some(3600), false));
        assert!(!cache_record_fresh(&current, Some(0), false));
        assert!(!cache_record_fresh(&stale, Some(60), false));
        assert!(!cache_record_fresh("invalid", Some(60), false));
        assert!(cache_record_fresh(&stale, Some(0), true));
        assert!(cache_record_fresh(&stale, None, false));
    }

    #[test]
    fn credential_headers_are_origin_scoped_and_diagnostics_are_redacted() {
        for header in ["Authorization", "Cookie", "Ocp-Apim-Subscription-Key", "X-API-Key"] {
            assert!(credential_header(header));
        }
        assert!(!credential_header("Accept"));
        assert!(same_origin("https://api.example.test/a", "https://api.example.test:443/b"));
        for target in ["https://cdn.example.test/b", "http://api.example.test/b", "https://api.example.test:8443/b", "not a URL"] {
            assert!(!same_origin("https://api.example.test/a", target));
        }
        let request = Req { method: "POST".into(), url: "https://api.example.test/login".into(),
            headers: vec![("Ocp-Apim-Subscription-Key".into(), "unit-test-secret-key".into()),
                          ("Accept".into(), "application/json".into())],
            body: Some(b"unit-test-secret-password".to_vec()), ..Default::default() };
        let debug = format!("{request:?}");
        assert!(!debug.contains("unit-test-secret"));
        assert!(debug.contains("<redacted>") && debug.contains("application/json"));
        assert!(!request.cache_key().is_empty());
    }

    #[test]
    fn robots_longest_match() {
        let r = Robots::parse("User-agent: *\nDisallow: /private\nAllow: /private/ok\nDisallow: /*.cgi$\n\nUser-agent: other\nDisallow: /\n");
        assert!(r.allowed("/guidelines/x"));
        assert!(!r.allowed("/private/a"));
        assert!(r.allowed("/private/ok/b"));
        assert!(!r.allowed("/bin/run.cgi"));
        assert!(Robots::parse("").allowed("/anything"));
        // a group naming our agent replaces the * group
        let r = Robots::parse("User-agent: *\nDisallow: /\n\nUser-agent: PigMed-guidelines\nDisallow: /private\n");
        assert!(r.allowed("/guidelines") && !r.allowed("/private/x"));
        // query strings are matched too
        assert!(!Robots::parse("User-agent: *\nDisallow: /*?page=\n").allowed("/list?page=2"));
        let u = Robots { unresolved: Some("html".into()), ..Default::default() };
        assert!(!u.allowed("/"));
    }

    #[test]
    fn pdf_magic_not_suffix() {
        let mk = |body: &[u8], ct: &str, url: &str| Fetched { body: body.to_vec(), record: FetchRecord {
            final_url: url.into(), content_type: Some(ct.into()), ..Default::default() } };
        assert!(mk(b"%PDF-1.7 ...", "application/octet-stream", "https://x/a").is_pdf());
        let shell = mk(b"<html><body>viewer</body></html>", "application/pdf", "https://x/a.pdf");
        assert!(!shell.is_pdf() && shell.declared_pdf_but_not());
    }

    #[tokio::test]
    async fn post_requests_have_their_own_cache_key() {
        let d = tempfile::tempdir().unwrap();
        let f = Fetcher::new(d.path(), true, 500, 1 << 20, "").unwrap();
        let get = Req::get("https://example.org/search");
        let post = Req { method: "POST".into(), body: Some(b"page=2".to_vec()), ..Req::get("https://example.org/search") };
        f.seed_cache_request_for_tests(&post, b"<html>page 2</html>", "text/html");
        assert!(f.raw(&get, false).await.is_err());
        assert!(f.raw(&post, false).await.unwrap().text().contains("page 2"));
    }

    #[test]
    fn login_wall_heuristic() {
        assert!(looks_like_login_wall("<html><form><h1>Sign in</h1><input type=\"password\"></form></html>"));
        assert!(!looks_like_login_wall("<html><h1>Recommendations</h1><p>Offer X.</p></html>"));
    }

    #[test]
    fn robots_crawl_delay_of_the_selected_group() {
        let r = Robots::parse("User-agent: *\nCrawl-delay: 10\nDisallow: /x\n\nUser-agent: bingbot\nCrawl-delay: 1\n");
        assert_eq!(r.crawl_delay, Some(Duration::from_secs(10)));
        // our own group wins, with its own delay (or none)
        let r = Robots::parse("User-agent: *\nCrawl-delay: 10\n\nUser-agent: PigMed-guidelines\nCrawl-Delay: 2.5\nAllow: /\n");
        assert_eq!(r.crawl_delay, Some(Duration::from_millis(2500)));
        assert_eq!(Robots::parse("User-agent: *\nCrawl-delay: 10\n\nUser-agent: PigMed-guidelines\nDisallow: /p\n").crawl_delay, None);
        // grouped user-agents share the record; comments, junk, zero and negative values are ignored
        let r = Robots::parse("User-agent: foo\nUser-agent: *\nCrawl-delay: 5 # per wikipedia\nCrawl-delay: 7\n");
        assert_eq!(r.crawl_delay, Some(Duration::from_secs(7)), "largest value in the group");
        for junk in ["Crawl-delay: soon", "Crawl-delay: 0", "Crawl-delay: -3", "Crawl-delay:"] {
            assert_eq!(Robots::parse(&format!("User-agent: *\n{junk}\n")).crawl_delay, None, "{junk}");
        }
        assert_eq!(Robots::parse("User-agent: *\nCrawl-delay: 86400\n").crawl_delay, Some(MAX_CRAWL_DELAY));
        // a delay after a rule line of another group does not leak into the * group
        let r = Robots::parse("User-agent: *\nDisallow: /a\nUser-agent: other\nCrawl-delay: 30\n");
        assert_eq!(r.crawl_delay, None);
        assert!(!r.allowed("/a/b"));
    }

    #[test]
    fn effective_interval_and_retry_after() {
        let min = Duration::from_millis(2000);
        assert_eq!(effective_interval(min, None), min);
        assert_eq!(effective_interval(min, Some(Duration::from_secs(1))), min, "registry pacing already slower");
        assert_eq!(effective_interval(min, Some(Duration::from_secs(10))), Duration::from_secs(10));
        assert_eq!(retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), Some(Duration::ZERO), "a past date means now");
        let soon = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(90));
        assert!(retry_after(&soon).is_some_and(|d| d > Duration::from_secs(80) && d <= Duration::from_secs(90)));
        assert_eq!(retry_after("later"), None);
    }

    fn gz(b: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn unrequested_compression_is_decoded_with_a_size_cap() {
        let json = br#"{"resourceType":"Bundle","entry":[]}"#;
        // header says gzip
        assert_eq!(decode_body(gz(json), Some("gzip"), Some("application/json"), 1 << 20).unwrap(), json);
        // no header, gzip magic on a JSON content type (the HAPI case)
        assert_eq!(decode_body(gz(json), None, Some("application/json; charset=utf-8"), 1 << 20).unwrap(), json);
        // a real .gz download is data, not transfer encoding
        let tgz = gz(b"archive member");
        assert_eq!(decode_body(tgz.clone(), None, Some("application/gzip"), 1 << 20).unwrap(), tgz);
        assert_eq!(decode_body(tgz.clone(), None, Some("application/octet-stream"), 1 << 20).unwrap(), tgz);
        // a lying header on a plain body leaves the bytes alone
        assert_eq!(decode_body(json.to_vec(), Some("gzip"), Some("application/json"), 1 << 20).unwrap(), json);
        // deflate (zlib-wrapped)
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut z, json).unwrap();
        assert_eq!(decode_body(z.finish().unwrap(), Some("deflate"), Some("application/json"), 1 << 20).unwrap(), json);
        // truncated stream and decompression bomb are errors, never partial documents
        let g = gz(&vec![b'a'; 100_000]);
        assert!(decode_body(g[..g.len() / 2].to_vec(), Some("gzip"), Some("text/html"), 1 << 20).is_err());
        assert!(decode_body(g, Some("gzip"), Some("text/html"), 1000).unwrap_err().to_string().contains("max_bytes"));
        assert!(decode_body(b"x".to_vec(), Some("br"), Some("text/html"), 1 << 20).is_err());
        assert_eq!(decode_body(b"plain".to_vec(), Some("identity"), None, 1 << 20).unwrap(), b"plain");
    }

    #[test]
    fn non_200_notes_keep_server_and_retry_after_only() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("server", "cloudflare".parse().unwrap());
        h.insert("retry-after", "120".parse().unwrap());
        h.insert("set-cookie", "__cf_bm=secret".parse().unwrap());
        h.insert("content-type", "text/html".parse().unwrap());
        let kept = kept_headers(&h);
        assert!(kept.iter().all(|(k, _)| k != "set-cookie"));
        assert_eq!(status_note(&kept), " (server: cloudflare; retry-after: 120)");
        assert_eq!(status_note(&[]), "");
    }

    /// Minimal HTTP/1.1 server on 127.0.0.1 (std only): robots.txt with a Crawl-delay, two pages, a gzip body the
    /// client did not ask for, and a 403 with Server/Retry-After headers.
    fn serve() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(mut c) = c else { continue };
                use std::io::{Read, Write};
                let mut buf = vec![];
                let mut b = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut b) { Ok(0) | Err(_) => break, Ok(n) => buf.extend_from_slice(&b[..n]) }
                }
                let req = String::from_utf8_lossy(&buf);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, extra, body): (&str, &str, Vec<u8>) = match path.as_str() {
                    "/robots.txt" => ("200 OK", "Content-Type: text/plain\r\n", b"User-agent: *\nCrawl-delay: 0.8\nDisallow: /private\n".to_vec()),
                    "/gz" => ("200 OK", "Content-Type: application/json\r\nContent-Encoding: gzip\r\n", gz(br#"{"ok":true}"#)),
                    "/denied" => ("403 Forbidden", "Content-Type: text/html\r\nServer: unit-test-server\r\nRetry-After: 7\r\n", b"<html>Just a moment...</html>".to_vec()),
                    p => ("200 OK", "Content-Type: text/html\r\n", format!("<html>{p}</html>").into_bytes()),
                };
                let head = format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = c.write_all(head.as_bytes()).and_then(|_| c.write_all(&body));
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn online_fetch_honours_crawl_delay_decodes_gzip_and_keeps_403_headers() {
        let base = serve();
        let d = tempfile::tempdir().unwrap();
        let f = Fetcher::new(d.path(), false, 500, 1 << 20, "").unwrap();
        let a = f.get(&format!("{base}/a")).await.unwrap();
        assert!(a.text().contains("/a"));
        assert_eq!(f.crawl_delay(&format!("{base}/x")).await, Some(Duration::from_millis(800)));
        let t = Instant::now();
        f.get(&format!("{base}/b")).await.unwrap();
        assert!(t.elapsed() >= Duration::from_millis(750), "crawl-delay 0.8 s > registry 0.5 s: {:?}", t.elapsed());
        let g = f.get_ok(&format!("{base}/gz")).await.unwrap();
        assert_eq!(g.body, br#"{"ok":true}"#);
        assert_eq!(g.record.bytes, g.body.len() as u64);
        let e = f.get_ok(&format!("{base}/denied")).await.err().expect("403 must be an error").to_string();
        assert!(e.contains("HTTP 403") && e.contains("server: unit-test-server") && e.contains("retry-after: 7"), "{e}");
        let side: Vec<_> = walk(&d.path().join("status"));
        assert_eq!(side.len(), 1);
        let v: Value = serde_json::from_slice(&std::fs::read(&side[0]).unwrap()).unwrap();
        assert_eq!(v["status"], 403);
        assert_eq!(v["headers"]["server"], "unit-test-server");
        assert!(f.get(&format!("{base}/private/x")).await.err().expect("robots disallow").to_string().contains("robots.txt disallows"));
        // the 403 was not cached as a response
        let off = Fetcher::new(d.path(), true, 500, 1 << 20, "").unwrap();
        assert!(off.get(&format!("{base}/denied")).await.is_err());
        assert!(off.get(&format!("{base}/a")).await.unwrap().record.from_cache);
    }

    fn walk(p: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
            if e.path().is_dir() { out.extend(walk(&e.path())); } else { out.push(e.path()); }
        }
        out
    }

    #[tokio::test]
    async fn offline_robots_from_cache_still_report_crawl_delay() {
        let d = tempfile::tempdir().unwrap();
        let f = Fetcher::new(d.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests("https://example.org/robots.txt", b"User-agent: *\nCrawl-delay: 10\n", "text/plain");
        f.seed_cache_for_tests("https://example.org/x", b"<html>x</html>", "text/html");
        assert!(f.is_offline());
        assert!(f.get("https://example.org/x").await.unwrap().record.from_cache);
        assert_eq!(f.crawl_delay("https://example.org/y").await, Some(Duration::from_secs(10)));
        assert_eq!(f.host_interval(&url::Url::parse("https://example.org/y").unwrap()).await, Duration::from_secs(10));
        assert_eq!(f.host_interval(&url::Url::parse("https://other.example.org/y").unwrap()).await, Duration::from_millis(500));
    }

    #[tokio::test]
    async fn offline_serves_cache_only() {
        let d = tempfile::tempdir().unwrap();
        let f = Fetcher::new(d.path(), true, 500, 1 << 20, "").unwrap();
        assert!(f.raw(&Req::get("https://example.org/x"), false).await.is_err());
        let (b, m) = f.cache_paths("https://example.org/x");
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&b, b"<html>hi</html>").unwrap();
        let rec = FetchRecord { url: "https://example.org/x".into(), final_url: "https://example.org/x".into(), status: 200, ..Default::default() };
        std::fs::write(&m, serde_json::to_vec(&rec).unwrap()).unwrap();
        let got = f.raw(&Req::get("https://example.org/x"), false).await.unwrap();
        assert!(got.record.from_cache && got.text().contains("hi"));
    }
}
