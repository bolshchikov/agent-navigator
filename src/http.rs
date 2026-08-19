use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, LOCATION, USER_AGENT};
use reqwest::{Method, StatusCode};
use url::Url;

use crate::config::{ClientConfig, UrlPolicy};
use crate::error::{Error, Result};
use crate::session::Session;

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HashMap<String, String>,
    pub body: Option<RequestBody>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub enum RequestBody {
    Bytes { content_type: String, data: Vec<u8> },
    Form(HashMap<String, String>),
}

fn request_body_summary(body: &Option<RequestBody>) -> String {
    match body {
        None => "none".into(),
        Some(RequestBody::Form(map)) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            format!("form fields={keys:?}")
        }
        Some(RequestBody::Bytes { content_type, data }) => {
            format!("{content_type} {} bytes", data.len())
        }
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub final_url: Url,
    pub redirect_chain: Vec<String>,
    pub elapsed: Duration,
    pub warnings: Vec<String>,
}

impl HttpResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn content_type(&self) -> Option<String> {
        self.headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    pub fn last_modified(&self) -> Option<String> {
        self.headers
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    pub fn is_html(&self) -> bool {
        self.content_type()
            .map(|ct| {
                let ct = ct.to_ascii_lowercase();
                ct.contains("text/html") || ct.contains("application/xhtml")
            })
            .unwrap_or_else(|| {
                let t = self.text();
                let head = t
                    .trim_start()
                    .get(..64)
                    .unwrap_or(t.trim_start())
                    .to_ascii_lowercase();
                head.starts_with("<!doctype html") || head.starts_with("<html")
            })
    }

    pub fn is_json(&self) -> bool {
        self.content_type()
            .as_deref()
            .map(is_json_mime)
            .unwrap_or(false)
    }
}

fn is_json_mime(ct: &str) -> bool {
    let mime = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime == "application/json"
        || mime == "text/json"
        || mime == "application/problem+json"
        || (mime.ends_with("+json") && !mime.ends_with("ld+json"))
}

pub struct FetchContext<'a> {
    pub robots: &'a RobotsCache,
    pub probe: &'a reqwest::Client,
    pub ignore_robots: bool,
}

pub async fn send(
    session: &Session,
    cfg: &ClientConfig,
    req: HttpRequest,
    ctx: &FetchContext<'_>,
) -> Result<HttpResponse> {
    let started = Instant::now();
    let timeout = req.timeout.unwrap_or(cfg.timeout);
    let mut last_err: Option<Error> = None;

    tracing::info!(
        method = %req.method,
        url = %req.url,
        body = %request_body_summary(&req.body),
        "HTTP request"
    );
    for attempt in 0..=cfg.max_retries {
        if attempt > 0 {
            let backoff = Duration::from_millis(100 * 4u64.pow(attempt - 1));
            tracing::debug!(attempt, ?backoff, "retrying request");
            tokio::time::sleep(backoff).await;
        }
        match send_follow_redirects(session, cfg, &req, timeout, ctx).await {
            Ok(resp) => {
                if should_retry_status(resp.status) && attempt < cfg.max_retries {
                    last_err = Some(Error::Other(format!(
                        "transient HTTP {}",
                        resp.status.as_u16()
                    )));
                    continue;
                }
                let mut resp = resp;
                resp.elapsed = started.elapsed();
                tracing::info!(
                    method = %req.method,
                    url = %resp.final_url,
                    status = resp.status.as_u16(),
                    bytes = resp.body.len(),
                    elapsed_ms = resp.elapsed.as_millis() as u64,
                    redirects = resp.redirect_chain.len(),
                    "HTTP response"
                );
                return Ok(resp);
            }
            Err(err) => {
                if is_retryable(&err) && attempt < cfg.max_retries {
                    last_err = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::Other("retry loop exhausted".into())))
}

async fn send_follow_redirects(
    session: &Session,
    cfg: &ClientConfig,
    req: &HttpRequest,
    timeout: Duration,
    ctx: &FetchContext<'_>,
) -> Result<HttpResponse> {
    let mut url = req.url.clone();
    let mut method = req.method.clone();
    let mut body = req.body.clone();
    let mut chain = Vec::new();
    let mut warnings = Vec::new();

    for hop in 0..=cfg.redirect_cap {
        ensure_fetchable_url(&url, cfg.url_policy)?;
        let (allowed, _, _) = ctx
            .robots
            .allowed(ctx.probe, cfg, &url, ctx.ignore_robots)
            .await?;
        if !allowed {
            return Err(Error::RobotsDisallowed {
                url: url.to_string(),
            });
        }

        let mut builder = session
            .http
            .request(method.clone(), url.clone())
            .timeout(timeout)
            .header(USER_AGENT, &cfg.user_agent);

        for (k, v) in &req.headers {
            if k.eq_ignore_ascii_case("user-agent") {
                tracing::warn!(
                    "ignoring caller User-Agent override; AgentNavigator self-identifies"
                );
                continue;
            }
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                builder = builder.header(name, value);
            }
        }

        builder = match &body {
            Some(RequestBody::Bytes { content_type, data }) => builder
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(data.clone()),
            Some(RequestBody::Form(map)) => builder.form(map),
            None => builder,
        };

        let response = builder.send().await?;
        let status = response.status();

        if status.is_redirection() {
            let loc = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::MissingRedirectLocation {
                    from: url.to_string(),
                })?;
            let next = url.join(loc)?;
            chain.push(url.to_string());
            if hop == cfg.redirect_cap {
                return Err(Error::TooManyRedirects {
                    url: req.url.to_string(),
                    cap: cfg.redirect_cap,
                });
            }
            if url.scheme() == "https" && next.scheme() == "http" {
                warnings.push(format!("HTTPS to HTTP redirect: {} → {}", url, next));
            }
            // 301/302/303: switch POST-like methods to GET and drop the body.
            if matches!(status.as_u16(), 301..=303) && !matches!(method, Method::GET | Method::HEAD)
            {
                method = Method::GET;
                body = None;
            }
            url = next;
            continue;
        }

        if let Some(len) = response.content_length() {
            if len > cfg.max_body_bytes {
                return Err(Error::BodyTooLarge {
                    url: url.to_string(),
                    size: len,
                    limit: cfg.max_body_bytes,
                });
            }
        }
        let headers = response.headers().clone();
        let body = response.bytes().await?;
        if body.len() as u64 > cfg.max_body_bytes {
            return Err(Error::BodyTooLarge {
                url: url.to_string(),
                size: body.len() as u64,
                limit: cfg.max_body_bytes,
            });
        }
        return Ok(HttpResponse {
            status,
            headers,
            body,
            final_url: url,
            redirect_chain: chain,
            elapsed: Duration::ZERO,
            warnings,
        });
    }

    Err(Error::TooManyRedirects {
        url: req.url.to_string(),
        cap: cfg.redirect_cap,
    })
}

fn should_retry_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn is_retryable(err: &Error) -> bool {
    match err {
        Error::Http(e) => e.is_timeout() || e.is_connect(),
        _ => false,
    }
}

/// Shared client used for robots.txt / llms.txt probes (no cookies).
/// Cross-origin and blocked-address redirects are not followed.
pub fn probe_client(cfg: &ClientConfig) -> Result<reqwest::Client> {
    let policy = cfg.url_policy;
    Ok(reqwest::Client::builder()
        .user_agent(&cfg.user_agent)
        .timeout(cfg.timeout)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > 5 {
                return attempt.error("too many probe redirects");
            }
            let Some(start) = attempt.previous().first() else {
                return attempt.stop();
            };
            if origin_of(start) != origin_of(attempt.url()) {
                return attempt.stop();
            }
            if ensure_fetchable_url(attempt.url(), policy).is_err() {
                return attempt.error("blocked probe redirect target");
            }
            attempt.follow()
        }))
        .build()?)
}

pub fn origin_of(url: &Url) -> String {
    let origin = url.origin();
    if origin.is_tuple() {
        return origin.ascii_serialization();
    }
    match url.host() {
        Some(url::Host::Ipv6(addr)) => match url.port() {
            Some(port) => format!("{}://[{}]:{}", url.scheme(), addr, port),
            None => format!("{}://[{}]", url.scheme(), addr),
        },
        Some(host) => match url.port() {
            Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
            None => format!("{}://{}", url.scheme(), host),
        },
        None => format!("{}://localhost", url.scheme()),
    }
}

pub fn ensure_fetchable_url(url: &Url, policy: UrlPolicy) -> Result<()> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(Error::ForbiddenUrl {
            url: url.to_string(),
            reason: format!(
                "scheme '{}' is not allowed; only http and https",
                url.scheme()
            ),
        });
    }
    if is_link_local_or_metadata(url) {
        return Err(Error::ForbiddenUrl {
            url: url.to_string(),
            reason: "refusing link-local / cloud-metadata address".into(),
        });
    }
    if policy == UrlPolicy::PublicOnly && is_private_or_loopback(url) {
        return Err(Error::ForbiddenUrl {
            url: url.to_string(),
            reason: "refusing loopback / private-network address".into(),
        });
    }
    Ok(())
}

fn is_private_or_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(addr)) => ipv4_blocked_public(addr),
        Some(url::Host::Ipv6(addr)) => ipv6_blocked_public(addr),
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost") || d == "localhost."
        }
        None => true,
    }
}

fn ipv4_blocked_public(addr: Ipv4Addr) -> bool {
    addr.is_loopback()
        || addr.is_private()
        || addr.is_unspecified()
        || addr.is_broadcast()
        || addr.is_link_local()
        || is_cgnat_v4(addr)
        || is_aws_metadata_v4(addr)
}

fn is_cgnat_v4(addr: Ipv4Addr) -> bool {
    let o = addr.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

fn ipv6_blocked_public(addr: Ipv6Addr) -> bool {
    if addr.is_loopback() || addr.is_unspecified() {
        return true;
    }
    if let Some(v4) = addr.to_ipv4_mapped() {
        return ipv4_blocked_public(v4);
    }
    // Unique local fc00::/7
    (addr.segments()[0] & 0xfe00) == 0xfc00
}

fn is_link_local_or_metadata(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(addr)) => addr.is_link_local() || is_aws_metadata_v4(addr),
        Some(url::Host::Ipv6(addr)) => {
            if let Some(v4) = addr.to_ipv4_mapped() {
                return v4.is_link_local() || is_aws_metadata_v4(v4);
            }
            is_link_local_v6(addr) || is_aws_metadata_v6(addr)
        }
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            d == "metadata.google.internal" || d.ends_with(".metadata.google.internal")
        }
        None => false,
    }
}

fn is_aws_metadata_v4(addr: Ipv4Addr) -> bool {
    addr.octets() == [169, 254, 169, 254]
}

fn is_link_local_v6(addr: Ipv6Addr) -> bool {
    (addr.segments()[0] & 0xffc0) == 0xfe80
}

fn is_aws_metadata_v6(addr: Ipv6Addr) -> bool {
    addr.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
}

pub struct RobotsCache {
    inner: std::sync::Mutex<HashMap<String, CachedRobots>>,
}

struct CachedRobots {
    body: Option<String>,
    expires: Instant,
}

impl RobotsCache {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn allowed(
        &self,
        client: &reqwest::Client,
        cfg: &ClientConfig,
        url: &Url,
        ignore: bool,
    ) -> Result<(bool, bool, Option<String>)> {
        if ignore {
            return Ok((true, false, None));
        }
        let origin = origin_of(url);
        let body = self.body_for(client, cfg, &origin).await?;
        let Some(body) = body else {
            return Ok((true, true, None));
        };
        let mut matcher = robotstxt::DefaultMatcher::default();
        let allowed = matcher.one_agent_allowed_by_robots(&body, "AgentNavigator", url.as_str());
        Ok((allowed, true, Some(body)))
    }

    async fn body_for(
        &self,
        client: &reqwest::Client,
        cfg: &ClientConfig,
        origin: &str,
    ) -> Result<Option<String>> {
        {
            let guard = self.inner.lock().expect("robots cache poisoned");
            if let Some(entry) = guard.get(origin) {
                if Instant::now() < entry.expires {
                    return Ok(entry.body.clone());
                }
            }
        }
        let robots_url = format!("{origin}/robots.txt");
        let body = match client.get(&robots_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await.unwrap_or_default();
                if text.trim().is_empty() || looks_like_html(&text) {
                    None
                } else {
                    Some(text)
                }
            }
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(error = %err, origin, "robots.txt fetch failed; failing open");
                None
            }
        };
        let mut guard = self.inner.lock().expect("robots cache poisoned");
        guard.insert(
            origin.to_string(),
            CachedRobots {
                body: body.clone(),
                expires: Instant::now() + cfg.discovery_ttl,
            },
        );
        Ok(body)
    }
}

fn looks_like_html(text: &str) -> bool {
    let t = text.trim_start().to_ascii_lowercase();
    t.starts_with("<!doctype html") || t.starts_with("<html")
}

impl Default for RobotsCache {
    fn default() -> Self {
        Self::new()
    }
}

pub struct DiscoveryCache {
    inner: std::sync::Mutex<HashMap<String, CachedDiscovery>>,
}

#[derive(Clone)]
pub struct CachedDiscovery {
    pub llms_txt: Option<crate::envelope::LlmsTxtDocument>,
    pub llms_full_txt: Option<crate::envelope::LlmsTxtDocument>,
    pub warnings: Vec<String>,
    pub(crate) expires: Instant,
}

impl CachedDiscovery {
    pub fn fresh(
        llms_txt: Option<crate::envelope::LlmsTxtDocument>,
        llms_full_txt: Option<crate::envelope::LlmsTxtDocument>,
        warnings: Vec<String>,
    ) -> Self {
        Self {
            llms_txt,
            llms_full_txt,
            warnings,
            expires: Instant::now(),
        }
    }
}

impl DiscoveryCache {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, origin: &str) -> Option<CachedDiscovery> {
        let guard = self.inner.lock().expect("discovery cache poisoned");
        guard
            .get(origin)
            .filter(|e| Instant::now() < e.expires)
            .cloned()
    }

    pub fn insert(&self, origin: String, mut entry: CachedDiscovery, ttl: Duration) {
        entry.expires = Instant::now() + ttl;
        self.inner
            .lock()
            .expect("discovery cache poisoned")
            .insert(origin, entry);
    }
}

impl Default for DiscoveryCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_of_brackets_ipv6() {
        let url = Url::parse("http://[::1]:8080/path").unwrap();
        assert_eq!(origin_of(&url), "http://[::1]:8080");
        let url = Url::parse("https://[2001:db8::1]/").unwrap();
        assert_eq!(origin_of(&url), "https://[2001:db8::1]");
    }

    #[test]
    fn origin_of_hostname() {
        let url = Url::parse("https://example.com:8443/x").unwrap();
        assert_eq!(origin_of(&url), "https://example.com:8443");
        let url = Url::parse("https://example.com/x").unwrap();
        assert_eq!(origin_of(&url), "https://example.com");
    }

    #[test]
    fn rejects_non_http_schemes() {
        let url = Url::parse("file:///etc/passwd").unwrap();
        assert!(ensure_fetchable_url(&url, UrlPolicy::AllowLoopback).is_err());
        let url = Url::parse("ftp://example.com/a").unwrap();
        assert!(ensure_fetchable_url(&url, UrlPolicy::AllowLoopback).is_err());
    }

    #[test]
    fn rejects_link_local_metadata() {
        let url = Url::parse("http://169.254.169.254/latest/meta-data/").unwrap();
        assert!(ensure_fetchable_url(&url, UrlPolicy::AllowLoopback).is_err());
        let url = Url::parse("http://metadata.google.internal/").unwrap();
        assert!(ensure_fetchable_url(&url, UrlPolicy::AllowLoopback).is_err());
        let url = Url::parse("http://127.0.0.1:3000/").unwrap();
        assert!(ensure_fetchable_url(&url, UrlPolicy::AllowLoopback).is_ok());
    }

    #[test]
    fn public_only_rejects_loopback_and_rfc1918() {
        let policy = UrlPolicy::PublicOnly;
        assert!(
            ensure_fetchable_url(&Url::parse("http://127.0.0.1:3000/").unwrap(), policy).is_err()
        );
        assert!(ensure_fetchable_url(&Url::parse("http://localhost/").unwrap(), policy).is_err());
        assert!(ensure_fetchable_url(&Url::parse("http://10.0.0.1/").unwrap(), policy).is_err());
        assert!(ensure_fetchable_url(&Url::parse("http://192.168.1.1/").unwrap(), policy).is_err());
        assert!(ensure_fetchable_url(&Url::parse("http://172.16.0.1/").unwrap(), policy).is_err());
        assert!(ensure_fetchable_url(&Url::parse("http://100.64.0.1/").unwrap(), policy).is_err());
        assert!(ensure_fetchable_url(&Url::parse("http://[::1]/").unwrap(), policy).is_err());
        assert!(ensure_fetchable_url(&Url::parse("https://example.com/").unwrap(), policy).is_ok());
        assert!(ensure_fetchable_url(
            &Url::parse("http://[::ffff:169.254.169.254]/").unwrap(),
            policy
        )
        .is_err());
        assert!(
            ensure_fetchable_url(&Url::parse("http://[::ffff:127.0.0.1]/").unwrap(), policy)
                .is_err()
        );
    }

    #[test]
    fn json_mime_does_not_match_html_or_ld_json() {
        assert!(is_json_mime("application/json; charset=utf-8"));
        assert!(is_json_mime("application/problem+json"));
        assert!(!is_json_mime("application/ld+json"));
        assert!(!is_json_mime("text/html"));
        assert!(!is_json_mime("application/javascript"));
    }
}
