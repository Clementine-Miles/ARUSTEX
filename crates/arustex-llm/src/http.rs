//! Shared HTTP transport: rate limiting, retries with backoff, timeouts, cancellation.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::sse::{SseEvent, SseParser};
use crate::{LlmError, LlmResult};

/// Transport settings derived from operator configuration.
#[derive(Debug, Clone)]
pub struct HttpSettings {
    pub base_url: String,
    /// Upper bound for a whole request including the streamed body.
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_retries: u32,
    pub requests_per_minute: u32,
    pub max_event_bytes: usize,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(10),
            max_retries: 3,
            requests_per_minute: 60,
            max_event_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Spaces requests evenly to stay under a per-minute rate.
#[derive(Debug)]
pub struct RateLimiter {
    interval: Duration,
    next: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub fn per_minute(rpm: u32) -> Self {
        let interval = if rpm == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(60) / rpm
        };
        Self {
            interval,
            next: Mutex::new(None),
        }
    }

    /// Waits for a slot; returns `Cancelled` if the token fires first.
    pub async fn acquire(&self, cancel: &CancellationToken) -> LlmResult<()> {
        let wait = {
            let mut next = self.next.lock().await;
            let now = Instant::now();
            let slot = next.map_or(now, |n| n.max(now));
            *next = Some(slot + self.interval);
            slot - now
        };
        if wait.is_zero() {
            return Ok(());
        }
        tokio::select! {
            () = cancel.cancelled() => Err(LlmError::Cancelled),
            () = tokio::time::sleep(wait) => Ok(()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Transport {
    client: reqwest::Client,
    pub(crate) settings: HttpSettings,
    limiter: RateLimiter,
}

impl Transport {
    pub(crate) fn new(settings: HttpSettings) -> LlmResult<Self> {
        let url = reqwest::Url::parse(&settings.base_url)
            .map_err(|e| LlmError::Config(format!("invalid provider base URL: {e}")))?;
        let loopback = matches!(
            url.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
        );
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(LlmError::Config(
                "provider endpoints must use https (http only on loopback)".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(settings.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("arustex/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| LlmError::Config(format!("http client: {e}")))?;
        let limiter = RateLimiter::per_minute(settings.requests_per_minute);
        Ok(Self {
            client,
            settings,
            limiter,
        })
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.settings.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// Sends a POST, retrying connection failures and retryable statuses before any
    /// response body is consumed.
    pub(crate) async fn post(
        &self,
        path: &str,
        headers: &[(&'static str, String)],
        body: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> LlmResult<reqwest::Response> {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            let mut value = HeaderValue::from_str(v)
                .map_err(|_| LlmError::Config(format!("invalid header value for {k}")))?;
            if k.eq_ignore_ascii_case("authorization") || k.eq_ignore_ascii_case("x-api-key") {
                value.set_sensitive(true);
            }
            map.insert(HeaderName::from_static(k), value);
        }
        let url = self.url(path);
        let mut attempt = 0u32;
        loop {
            self.limiter.acquire(cancel).await?;
            let send = self
                .client
                .post(&url)
                .headers(map.clone())
                .json(body)
                .send();
            let result = tokio::select! {
                () = cancel.cancelled() => return Err(LlmError::Cancelled),
                r = send => r,
            };
            let err = match result {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => error_from_response(resp).await,
                Err(e) if e.is_timeout() => LlmError::Timeout(self.settings.connect_timeout),
                Err(e) => LlmError::Transport(without_url(&e)),
            };
            if !err.is_retryable() || attempt >= self.settings.max_retries {
                return Err(err);
            }
            let delay = backoff(attempt, &err);
            tracing::warn!(attempt, ?delay, error = %err, "retrying model request");
            attempt += 1;
            tokio::select! {
                () = cancel.cancelled() => return Err(LlmError::Cancelled),
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// Runs a whole request under the configured timeout and the cancellation token.
    pub(crate) async fn run<T>(
        &self,
        cancel: &CancellationToken,
        fut: impl Future<Output = LlmResult<T>>,
    ) -> LlmResult<T> {
        let timeout = self.settings.timeout;
        tokio::select! {
            () = cancel.cancelled() => Err(LlmError::Cancelled),
            r = tokio::time::timeout(timeout, fut) => r.map_err(|_| LlmError::Timeout(timeout))?,
        }
    }

    /// Reads a streamed body as SSE, calling `f` per event until it returns `false`.
    pub(crate) async fn for_each_event(
        &self,
        mut resp: reqwest::Response,
        mut f: impl FnMut(SseEvent) -> LlmResult<bool>,
    ) -> LlmResult<()> {
        let mut parser = SseParser::new(self.settings.max_event_bytes);
        loop {
            let chunk = resp
                .chunk()
                .await
                .map_err(|e| LlmError::Transport(without_url(&e)))?;
            let Some(chunk) = chunk else { break };
            for event in parser.feed(&chunk).map_err(LlmError::Protocol)? {
                if !f(event)? {
                    return Ok(());
                }
            }
        }
        if let Some(event) = parser.finish() {
            f(event)?;
        }
        Ok(())
    }
}

fn without_url(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    if let Some(url) = e.url() {
        s = s.replace(url.as_str(), "<endpoint>");
    }
    s
}

async fn error_from_response(resp: reqwest::Response) -> LlmError {
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let text = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| text.chars().take(300).collect());
    LlmError::Http {
        status,
        message: message.chars().take(500).collect(),
        retry_after,
    }
}

fn backoff(attempt: u32, err: &LlmError) -> Duration {
    if let LlmError::Http {
        retry_after: Some(d),
        ..
    } = err
    {
        return (*d).min(Duration::from_secs(60));
    }
    let base = Duration::from_millis(500) * 2u32.saturating_pow(attempt.min(6));
    let jitter = Duration::from_millis(u64::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_millis())
            % 250,
    ));
    (base + jitter).min(Duration::from_secs(20))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_plain_http_off_loopback() {
        let s = |u: &str| HttpSettings {
            base_url: u.into(),
            ..Default::default()
        };
        assert!(Transport::new(s("http://example.com/v1")).is_err());
        assert!(Transport::new(s("http://127.0.0.1:8080/v1")).is_ok());
        assert!(Transport::new(s("https://example.com/v1")).is_ok());
        assert!(Transport::new(s("ftp://example.com")).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limiter_spaces_requests() {
        let rl = RateLimiter::per_minute(60);
        let cancel = CancellationToken::new();
        let start = tokio::time::Instant::now();
        for _ in 0..3 {
            rl.acquire(&cancel).await.unwrap();
        }
        assert!(start.elapsed() >= Duration::from_secs(2));
    }
}
