use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use std::time::Duration;
use url::Url;

#[derive(Clone)]
pub struct Http {
    client: Client,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    pub max_bytes: usize,
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
        let client = Client::builder()
            .user_agent(concat!(
                "ffetch/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/prdpx7/ffetch)"
            ))
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(10).min(timeout))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        Ok(Self {
            client,
            max_bytes,
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency)),
        })
    }
    pub async fn get(&self, url: Url) -> Result<Response> {
        self.get_authorized(url, None).await
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
        self.get_authorized(url, token).await
    }
    async fn get_authorized(&self, url: Url, token: Option<&str>) -> Result<Response> {
        let _permit = self.permits.acquire().await?;
        let mut request = self.client.get(url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("request failed: {}", e.without_url()))?;
        let status = response.status();
        let url = response.url().clone();
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
                "body exceeds --max-bytes ({})",
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
            let chunk =
                chunk.map_err(|e| anyhow::anyhow!("body read failed: {}", e.without_url()))?;
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= self.max_bytes,
                "body exceeds --max-bytes ({})",
                self.max_bytes
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(Response {
            url,
            status,
            content_type,
            retry_after,
            bytes,
        })
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
