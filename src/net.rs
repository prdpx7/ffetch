use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};
use url::Url;

#[cfg(test)]
#[path = "net_http2_tests.rs"]
mod http2_tests;

tokio::task_local! { static DEADLINE: Instant; }

// Header negotiation only. This does not emulate browser TLS or execute JS.
const SEARCH_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
#[derive(Clone, Copy)]
enum HeaderProfile {
    Standard,
    GitHub { search: bool },
    BrowserUserAgent,
}

/// Nested provider stages can narrow the operation deadline. The scoped future
/// restores its context between polls, including when sources run concurrently.
pub async fn within_budget<F: std::future::Future>(
    budget: Duration,
    future: F,
) -> std::result::Result<F::Output, tokio::time::error::Elapsed> {
    let own = Instant::now() + budget;
    let deadline = DEADLINE.try_with(|outer| own.min(*outer)).unwrap_or(own);
    DEADLINE
        .scope(deadline, tokio::time::timeout_at(deadline, future))
        .await
}

#[derive(Clone)]
pub struct Http {
    client: Client,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    origins: Arc<Mutex<HashMap<String, Arc<Origin>>>>,
    timeout: Duration,
    concurrency: usize,
    origin_concurrency: usize,
    pub max_bytes: usize,
}

struct Origin {
    permits: Arc<Semaphore>,
    cooldown: Mutex<Option<(Instant, Duration)>>,
    requests: AtomicU64,
    retries: AtomicU64,
    limited: AtomicU64,
    redirects: AtomicU64,
    http1: AtomicU64,
    http2: AtomicU64,
    other_protocol: AtomicU64,
}

impl Origin {
    fn remaining(&self) -> Duration {
        self.cooldown
            .lock()
            .unwrap()
            .as_ref()
            .map(|(start, delay)| delay.saturating_sub(start.elapsed()))
            .unwrap_or_default()
    }
    fn defer(&self, delay: Duration) {
        let mut cooldown = self.cooldown.lock().unwrap();
        let remaining = cooldown
            .as_ref()
            .map(|(start, delay)| delay.saturating_sub(start.elapsed()))
            .unwrap_or_default();
        if delay > remaining {
            *cooldown = Some((Instant::now(), delay));
        }
    }
}

#[derive(Serialize)]
pub struct HttpStats {
    pub concurrency: usize,
    pub origin_concurrency: usize,
    pub requests_started: u64,
    pub retries_started: u64,
    pub backoff_responses: u64,
    pub redirects_followed: u64,
    pub responses_by_protocol: ProtocolStats,
    pub origins: Vec<OriginStats>,
}

#[derive(Serialize)]
pub struct OriginStats {
    pub origin: String,
    pub requests_started: u64,
    pub retries_started: u64,
    pub backoff_responses: u64,
    pub redirects_followed: u64,
    pub cooldown_remaining_ms: u64,
    pub responses_by_protocol: ProtocolStats,
}

/// Negotiated responses received by this client. A proxy may use a different
/// protocol upstream; these counters do not claim completed response bodies.
#[derive(Serialize, Default)]
pub struct ProtocolStats {
    pub http1: u64,
    pub http2: u64,
    pub other: u64,
}

fn http_date(
    value: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::Datelike;
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(value) {
        return Some(date.with_timezone(&chrono::Utc));
    }
    // Legacy HTTP dates remain valid inputs. Interpret a two-digit year more
    // than 50 years ahead as the previous century (RFC 9110 section 5.6.7).
    if let Some((_, rest)) = value.split_once(", ")
        && let Ok(mut date) = chrono::NaiveDateTime::parse_from_str(rest, "%d-%b-%y %H:%M:%S GMT")
    {
        if date.year() > now.year() + 50 {
            date = date.with_year(date.year() - 100)?;
        }
        return Some(date.and_utc());
    }
    chrono::NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
        .ok()
        .map(|d| d.and_utc())
}

fn retry_delay(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        // An overflowing delay must not turn into an immediate retry.
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let date = http_date(value, now)?;
    Some(date.signed_duration_since(now).to_std().unwrap_or_default())
}

pub struct Response {
    pub url: Url,
    pub status: StatusCode,
    pub content_type: String,
    pub retry_after: Option<String>,
    pub bytes: Vec<u8>,
}

pub fn normalize(input: &str) -> Result<Url> {
    let value = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let url = Url::parse(&value).context("invalid URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "only HTTP(S) URLs are supported"
    );
    ensure!(url.host_str().is_some(), "URL needs a host");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "use a session for credentials, not URL userinfo"
    );
    Ok(url)
}

impl Http {
    pub fn new(timeout: Duration, max_bytes: usize, concurrency: usize) -> Result<Self> {
        Self::with_limits(timeout, max_bytes, concurrency, 4)
    }

    pub fn with_limits(
        timeout: Duration,
        max_bytes: usize,
        concurrency: usize,
        origin_concurrency: usize,
    ) -> Result<Self> {
        ensure!(
            concurrency > 0 && origin_concurrency > 0,
            "HTTP concurrency limits must be positive"
        );
        ensure!(
            concurrency <= Semaphore::MAX_PERMITS,
            "HTTP concurrency exceeds semaphore capacity"
        );
        ensure!(
            Instant::now().checked_add(timeout).is_some(),
            "HTTP timeout exceeds monotonic clock range"
        );
        let client = Client::builder()
            .user_agent(concat!(
                "ffetch/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/prdpx7/ffetch)"
            ))
            .timeout(timeout.min(Duration::from_secs(15)))
            .connect_timeout(Duration::from_secs(10).min(timeout))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            max_bytes,
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency)),
            origins: Arc::new(Mutex::new(HashMap::new())),
            timeout,
            concurrency,
            origin_concurrency: origin_concurrency.min(concurrency),
        })
    }

    fn origin(&self, url: &Url) -> Arc<Origin> {
        self.origins
            .lock()
            .unwrap()
            .entry(url.origin().ascii_serialization())
            .or_insert_with(|| {
                Arc::new(Origin {
                    permits: Arc::new(Semaphore::new(self.origin_concurrency)),
                    cooldown: Mutex::new(None),
                    requests: AtomicU64::new(0),
                    retries: AtomicU64::new(0),
                    limited: AtomicU64::new(0),
                    redirects: AtomicU64::new(0),
                    http1: AtomicU64::new(0),
                    http2: AtomicU64::new(0),
                    other_protocol: AtomicU64::new(0),
                })
            })
            .clone()
    }

    pub fn stats(&self) -> HttpStats {
        let mut origins: Vec<_> = self
            .origins
            .lock()
            .unwrap()
            .iter()
            .map(|(name, state)| OriginStats {
                origin: name.clone(),
                requests_started: state.requests.load(Ordering::Relaxed),
                retries_started: state.retries.load(Ordering::Relaxed),
                backoff_responses: state.limited.load(Ordering::Relaxed),
                redirects_followed: state.redirects.load(Ordering::Relaxed),
                cooldown_remaining_ms: state.remaining().as_millis().min(u64::MAX as u128) as u64,
                responses_by_protocol: ProtocolStats {
                    http1: state.http1.load(Ordering::Relaxed),
                    http2: state.http2.load(Ordering::Relaxed),
                    other: state.other_protocol.load(Ordering::Relaxed),
                },
            })
            .collect();
        origins.sort_by(|a, b| a.origin.cmp(&b.origin));
        HttpStats {
            concurrency: self.concurrency,
            origin_concurrency: self.origin_concurrency,
            requests_started: origins.iter().map(|o| o.requests_started).sum(),
            retries_started: origins.iter().map(|o| o.retries_started).sum(),
            backoff_responses: origins.iter().map(|o| o.backoff_responses).sum(),
            redirects_followed: origins.iter().map(|o| o.redirects_followed).sum(),
            responses_by_protocol: ProtocolStats {
                http1: origins.iter().map(|o| o.responses_by_protocol.http1).sum(),
                http2: origins.iter().map(|o| o.responses_by_protocol.http2).sum(),
                other: origins.iter().map(|o| o.responses_by_protocol.other).sum(),
            },
            origins,
        }
    }

    async fn acquire(
        &self,
        origin: &Origin,
        deadline: Instant,
    ) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        loop {
            let delay = origin.remaining();
            ensure!(
                delay < deadline.saturating_duration_since(Instant::now()),
                "origin Retry-After exceeds HTTP request deadline; retry later or select another origin"
            );
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            // Never occupy a global request slot while queued behind one origin.
            let local = origin.permits.clone().acquire_owned().await?;
            let global = self.permits.clone().acquire_owned().await?;
            ensure!(
                Instant::now() < deadline,
                "HTTP request deadline exceeded while queued for capacity"
            );
            if origin.remaining().is_zero() {
                return Ok((local, global));
            }
            // Another in-flight response may have established a cooldown while
            // these permits were queued. Release both before waiting again.
        }
    }
    pub async fn get(&self, url: Url) -> Result<Response> {
        self.get_authorized(url, None, true).await
    }
    /// For adapters with stricter source-specific spacing, such as arXiv.
    /// Shared cooldowns and redirect scheduling still apply.
    pub async fn get_once(&self, url: Url) -> Result<Response> {
        self.get_authorized(url, None, false).await
    }
    pub async fn reddit_get(&self, url: Url, token: Option<&str>) -> Result<Response> {
        if token.is_some() {
            ensure!(
                url.scheme() == "https"
                    && url.host_str() == Some("oauth.reddit.com")
                    && url.port().is_none()
                    && url.username().is_empty()
                    && url.password().is_none(),
                "Reddit bearer credentials require the official HTTPS OAuth origin"
            );
        }
        self.get_authorized(url, token, true).await
    }
    pub async fn github_get(
        &self,
        url: Url,
        token: Option<&str>,
        search: bool,
    ) -> Result<Response> {
        if token.is_some() {
            ensure!(
                url.scheme() == "https"
                    && url.host_str() == Some("api.github.com")
                    && url.port().is_none()
                    && url.username().is_empty()
                    && url.password().is_none(),
                "GitHub bearer credentials require the official HTTPS API origin"
            );
        }
        self.get_profile(url, token, true, HeaderProfile::GitHub { search })
            .await
    }
    /// A browser User-Agent on this origin, retaining the same TLS, connection
    /// pool, bounds and cooldowns. This never starts a browser process.
    pub async fn get_with_browser_user_agent(
        &self,
        url: Url,
        allow_retry: bool,
    ) -> Result<Response> {
        self.get_profile(url, None, allow_retry, HeaderProfile::BrowserUserAgent)
            .await
    }
    async fn get_authorized(
        &self,
        url: Url,
        token: Option<&str>,
        allow_retry: bool,
    ) -> Result<Response> {
        self.get_profile(url, token, allow_retry, HeaderProfile::Standard)
            .await
    }
    async fn get_profile(
        &self,
        url: Url,
        token: Option<&str>,
        allow_retry: bool,
        profile: HeaderProfile,
    ) -> Result<Response> {
        let own = Instant::now()
            .checked_add(self.timeout)
            .context("HTTP timeout exceeds clock range")?;
        let inherited = DEADLINE.try_with(|outer| *outer).ok();
        let deadline = inherited.map(|outer| own.min(outer)).unwrap_or(own);
        let mut last_status = None;
        let result = tokio::time::timeout_at(
            deadline,
            self.get_loop(url, token, deadline, allow_retry, &mut last_status, profile),
        )
        .await;
        // Let the enclosing timeout cancel unfinished work at its deadline.
        // A child error racing that timeout would instead count a failed item.
        if inherited.is_some_and(|outer| outer <= own) && Instant::now() >= deadline {
            return std::future::pending().await;
        }
        match result {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "HTTP{} request deadline exceeded",
                last_status
                    .map(|s: StatusCode| format!(" {}", s.as_u16()))
                    .unwrap_or_default()
            )),
        }
    }

    async fn get_loop(
        &self,
        mut url: Url,
        mut token: Option<&str>,
        deadline: Instant,
        allow_retry: bool,
        last_status: &mut Option<StatusCode>,
        mut profile: HeaderProfile,
    ) -> Result<Response> {
        let mut redirects = 0;
        let mut retried = false;
        let mut retrying = false;
        loop {
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none(),
                "HTTP redirects require an HTTP(S) URL without userinfo"
            );
            let origin = self.origin(&url);
            let (_local, _global) = self.acquire(&origin, deadline).await?;
            let mut request = self.client.get(url.clone());
            match profile {
                HeaderProfile::Standard => (),
                HeaderProfile::BrowserUserAgent => {
                    request = request.header("User-Agent", SEARCH_USER_AGENT)
                }
                HeaderProfile::GitHub { search } => {
                    request = request
                        .header(
                            "Accept",
                            if search {
                                "application/vnd.github.text-match+json"
                            } else {
                                "application/vnd.github+json"
                            },
                        )
                        .header("X-GitHub-Api-Version", "2022-11-28");
                }
            }
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            origin.requests.fetch_add(1, Ordering::Relaxed);
            if retrying {
                origin.retries.fetch_add(1, Ordering::Relaxed);
                retrying = false;
            }
            let response = request
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("request failed: {}", e.without_url()))?;
            let status = response.status();
            match response.version() {
                reqwest::Version::HTTP_09
                | reqwest::Version::HTTP_10
                | reqwest::Version::HTTP_11 => &origin.http1,
                reqwest::Version::HTTP_2 => &origin.http2,
                _ => &origin.other_protocol,
            }
            .fetch_add(1, Ordering::Relaxed);
            *last_status = Some(status);
            let delay = response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| retry_delay(s, chrono::Utc::now()));
            if matches!(
                status,
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
            ) {
                origin.limited.fetch_add(1, Ordering::Relaxed);
                if let Some(delay) = delay {
                    origin.defer(delay);
                    if allow_retry
                        && !retried
                        && delay < deadline.saturating_duration_since(Instant::now())
                    {
                        retried = true;
                        retrying = true;
                        continue;
                    }
                }
            }
            if matches!(
                status,
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::SEE_OTHER
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) && let Some(location) = response.headers().get("location")
            {
                ensure!(redirects < 10, "HTTP redirect limit exceeded");
                let next = url
                    .join(location.to_str().context("invalid redirect Location")?)
                    .context("invalid redirect URL")?;
                ensure!(
                    matches!(next.scheme(), "http" | "https")
                        && next.host_str().is_some()
                        && next.username().is_empty()
                        && next.password().is_none(),
                    "HTTP redirects require an HTTP(S) URL without userinfo"
                );
                if url.origin() != next.origin() {
                    token = None;
                    profile = HeaderProfile::Standard;
                }
                if let Some(delay) = delay {
                    // Retry-After on 3xx delays this redirect, not unrelated work at
                    // its destination. Drop network permits before that wait.
                    drop(response);
                    drop(_local);
                    drop(_global);
                    ensure!(
                        delay < deadline.saturating_duration_since(Instant::now()),
                        "redirect Retry-After exceeds HTTP request deadline"
                    );
                    tokio::time::sleep(delay).await;
                }
                redirects += 1;
                origin.redirects.fetch_add(1, Ordering::Relaxed);
                url = next;
                continue;
            }
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_owned();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned);
            if let Some(len) = response.content_length() {
                ensure!(
                    len <= self.max_bytes as u64,
                    "HTTP {} body exceeds --max-bytes ({})",
                    status.as_u16(),
                    self.max_bytes
                );
            }
            // Hints may describe compressed or hostile bodies: cap eager allocation.
            let capacity = response
                .content_length()
                .unwrap_or(0)
                .min(self.max_bytes as u64)
                .min(64 * 1024) as usize;
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::with_capacity(capacity);
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| {
                    anyhow::anyhow!(
                        "HTTP {} body read failed: {}",
                        status.as_u16(),
                        e.without_url()
                    )
                })?;
                ensure!(
                    bytes.len().saturating_add(chunk.len()) <= self.max_bytes,
                    "HTTP {} body exceeds --max-bytes ({})",
                    status.as_u16(),
                    self.max_bytes
                );
                bytes.extend_from_slice(&chunk);
            }
            return Ok(Response {
                url,
                status,
                content_type,
                retry_after,
                bytes,
            });
        }
    }
    pub async fn json(&self, url: Url) -> Result<serde_json::Value> {
        let response = self.get(url).await?;
        ensure!(
            response.status.is_success(),
            "HTTP {}{}",
            response.status.as_u16(),
            response
                .retry_after
                .as_deref()
                .map(|s| format!("; retry after {s}"))
                .unwrap_or_default()
        );
        serde_json::from_slice(&response.bytes).context("provider returned invalid JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn browser_user_agent_shares_retry_limits_and_does_not_change_regular_requests() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", server.local_addr().unwrap())).unwrap();
        let task = std::thread::spawn(move || {
            for index in 0..3 {
                let (mut socket, _) = server.accept().unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let mut bytes = [0; 4096];
                    let n = socket.read(&mut bytes).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&bytes[..n]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert_eq!(request.contains("chrome/152."), index < 2);
                assert_eq!(request.contains("user-agent: ffetch/"), index == 2);
                assert!(!request.contains("authorization:"));
                assert!(!request.contains("x-github-api-version:"));
                let response = if index == 0 {
                    "HTTP/1.1 429 Limited\r\nRetry-After: 0\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                };
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        let http = Http::new(Duration::from_secs(2), 4096, 1).unwrap();
        http.get_with_browser_user_agent(url.clone(), true)
            .await
            .unwrap();
        http.get(url).await.unwrap();
        let stats = http.stats();
        assert_eq!(stats.requests_started, 3);
        assert_eq!(stats.retries_started, 1);
        assert_eq!(stats.responses_by_protocol.http1, 3);
        assert_eq!(stats.origins[0].responses_by_protocol.http1, 3);
        task.join().unwrap();
    }
    #[tokio::test]
    async fn browser_user_agent_is_retained_on_same_origin_and_reset_on_cross_origin_redirects() {
        use std::io::{Read, Write};
        fn headers(socket: &mut std::net::TcpStream) -> String {
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut data = Vec::new();
            while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut bytes = [0; 4096];
                let n = socket.read(&mut bytes).unwrap();
                assert!(n > 0 && data.len() + n < 64 * 1024);
                data.extend_from_slice(&bytes[..n]);
            }
            String::from_utf8(data).unwrap().to_ascii_lowercase()
        }
        let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/start", source.local_addr().unwrap())).unwrap();
        let destination = format!("http://{}/final", target.local_addr().unwrap());
        let sender = std::thread::spawn(move || {
            for index in 0..2 {
                let (mut socket, _) = source.accept().unwrap();
                let request = headers(&mut socket);
                assert!(request.contains("chrome/152."));
                assert!(!request.contains("authorization:"));
                let location = if index == 0 { "/same" } else { &destination };
                write!(socket,"HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        let receiver = std::thread::spawn(move || {
            let (mut socket, _) = target.accept().unwrap();
            let request = headers(&mut socket);
            assert!(request.contains("user-agent: ffetch/"));
            assert!(!request.contains("chrome/152."));
            assert!(!request.contains("x-github-api-version:"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let http = Http::new(Duration::from_secs(3), 4096, 2).unwrap();
        assert_eq!(
            http.get_with_browser_user_agent(url, false)
                .await
                .unwrap()
                .bytes,
            b"ok"
        );
        sender.join().unwrap();
        receiver.join().unwrap();
        assert_eq!(http.stats().redirects_followed, 2);
        assert_eq!(http.stats().responses_by_protocol.http1, 3);
    }
    #[tokio::test]
    async fn github_requests_pin_the_api_and_select_text_match_media_only_for_search() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", server.local_addr().unwrap());
        let task = std::thread::spawn(move || {
            for search in [false, true] {
                let (mut socket, _) = server.accept().unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let mut bytes = [0; 4096];
                    let n = socket.read(&mut bytes).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&bytes[..n]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.contains("x-github-api-version: 2022-11-28"));
                assert!(request.contains(if search {
                    "accept: application/vnd.github.text-match+json"
                } else {
                    "accept: application/vnd.github+json"
                }));
                assert!(!request.contains("authorization:"));
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
            }
        });
        let http = Http::new(Duration::from_secs(2), 4096, 2).unwrap();
        for search in [false, true] {
            http.github_get(Url::parse(&base).unwrap(), None, search)
                .await
                .unwrap();
        }
        task.join().unwrap();
    }
    #[test]
    fn retry_after_accepts_http_dates_and_rejects_malformed_delays() {
        let now = chrono::DateTime::parse_from_rfc2822("Sun, 06 Nov 1994 08:49:36 GMT")
            .unwrap()
            .with_timezone(&chrono::Utc);
        for value in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "1",
        ] {
            assert_eq!(
                retry_delay(value, now),
                Some(Duration::from_secs(1)),
                "{value}"
            );
        }
        for value in ["-1", "1.5", "not a date", ""] {
            assert!(retry_delay(value, now).is_none());
        }
        assert_eq!(retry_delay("0", now), Some(Duration::ZERO));
        assert_eq!(
            retry_delay("999999999999999999999999", now),
            Some(Duration::from_secs(u64::MAX))
        );
        let modern = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            retry_delay("Tuesday, 01-Jan-80 00:00:00 GMT", modern),
            Some(Duration::ZERO)
        );
    }

    #[tokio::test]
    async fn redirects_keep_bearer_only_on_the_original_origin() {
        use std::io::{Read, Write};
        fn read_headers(socket: &mut std::net::TcpStream) -> String {
            let mut data = Vec::new();
            let mut bytes = [0; 4096];
            while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut bytes).unwrap();
                assert!(n > 0 && data.len() + n <= 64 * 1024);
                data.extend_from_slice(&bytes[..n]);
            }
            String::from_utf8(data).unwrap().to_ascii_lowercase()
        }
        let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let start = Url::parse(&format!("http://{}/start", source.local_addr().unwrap())).unwrap();
        let destination = format!("http://{}/", target.local_addr().unwrap());
        let sender = std::thread::spawn(move || {
            for n in 0..2 {
                let (mut socket, _) = source.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                assert!(read_headers(&mut socket).contains("authorization: bearer fixture-token"));
                let location = if n == 0 { "/same" } else { &destination };
                write!(socket, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        let receiver = std::thread::spawn(move || {
            let (mut socket, _) = target.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            assert!(!read_headers(&mut socket).contains("authorization:"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let http = Http::new(Duration::from_secs(3), 4096, 2).unwrap();
        assert_eq!(
            http.get_authorized(start, Some("fixture-token"), true)
                .await
                .unwrap()
                .bytes,
            b"ok"
        );
        sender.join().unwrap();
        receiver.join().unwrap();
        assert_eq!(http.stats().redirects_followed, 2);
    }
    #[test]
    fn normalize_is_conservative() {
        assert_eq!(
            normalize("example.org/a?q=1#frag").unwrap().as_str(),
            "https://example.org/a?q=1#frag"
        );
        assert!(normalize("file:///etc/passwd").is_err());
        assert!(normalize("https://user:secret@example.org/").is_err());
    }
}
