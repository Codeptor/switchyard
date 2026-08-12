//! Generic Anthropic Messages upstream adapter.
//!
//! This is the only concrete adapter in v1. It speaks the Anthropic Messages
//! wire protocol and is therefore usable with any provider that exposes an
//! Anthropic-compatible `/v1/messages` endpoint. Provider-specific quirks are
//! handled here, not in the core boundary.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tracing::{debug, warn};

use crate::providers::adapter::{ProviderAdapter, ProviderStream};
use crate::providers::config::{AuthConfig, ProviderConfig, RetryConfig};
use crate::providers::credentials::{build_auth_header_value, load_credential, redact_headers};
use crate::providers::error::ProviderError;
use crate::providers::stream::{SseParser, StreamEvent, normalize_sse_event};
use crate::providers::types::{MessagesRequest, MessagesResponse};

/// Default connect timeout: 10 seconds.
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
/// Default read timeout: 120 seconds (per socket read, not total request).
const DEFAULT_READ_TIMEOUT_MS: u64 = 120_000;

/// Status codes that are retryable.
const RETRYABLE_STATUSES: &[u16] = &[429, 500, 502, 503, 529];

/// Generic Anthropic-compatible upstream adapter.
///
/// Configuration is validated on construction. Secrets are loaded from the
/// environment at request time, never stored.
#[derive(Debug, Clone)]
pub struct AnthropicAdapter {
    provider_id: String,
    base_url: String,
    auth: AuthConfig,
    read_timeout: Duration,
    retry: RetryConfig,
    extra_headers: Vec<(String, String)>,
    client: reqwest::Client,
}

impl AnthropicAdapter {
    /// Create an adapter from a validated [`ProviderConfig`].
    pub fn from_config(config: &ProviderConfig) -> Result<Self, ProviderError> {
        config
            .validate()
            .map_err(|e| ProviderError::Config(e.to_string()))?;

        let connect_timeout = Duration::from_millis(
            config
                .connect_timeout_ms
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        );
        let read_timeout =
            Duration::from_millis(config.read_timeout_ms.unwrap_or(DEFAULT_READ_TIMEOUT_MS));
        let retry = config.retry.clone().unwrap_or_default();

        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .build()
            .map_err(|e| ProviderError::Config(format!("failed to build http client: {e}")))?;

        Ok(Self {
            provider_id: config.id.clone(),
            base_url: config.normalized_base_url(),
            auth: config.auth.clone(),
            read_timeout,
            retry,
            extra_headers: config.extra_headers.clone(),
            client,
        })
    }

    /// Direct constructor for tests (bypasses config parsing).
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        auth: AuthConfig,
        read_timeout: Duration,
    ) -> Result<Self, ProviderError> {
        Self::with_timeouts(
            provider_id,
            base_url,
            auth,
            Duration::from_secs(10),
            read_timeout,
        )
    }

    /// Direct constructor with explicit connect and read timeouts.
    pub fn with_timeouts(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        auth: AuthConfig,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        let parsed = url::Url::parse(&base_url)
            .map_err(|e| ProviderError::Config(format!("invalid base_url: {e}")))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Err(ProviderError::Config(format!(
                    "unsupported scheme '{other}'"
                )));
            }
        }
        auth.validate()
            .map_err(|e| ProviderError::Config(e.to_string()))?;

        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .build()
            .map_err(|e| ProviderError::Config(format!("failed to build http client: {e}")))?;

        Ok(Self {
            provider_id: provider_id.into(),
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            read_timeout,
            retry: RetryConfig::default(),
            extra_headers: vec![],
            client,
        })
    }

    /// Set a custom retry config (builder pattern, used in tests).
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    fn build_headers(
        &self,
        forward_headers: &[(String, String)],
    ) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );

        if let Some(token) = load_credential(&self.auth)?
            && let Some((name, value)) = build_auth_header_value(&self.auth, &token)
        {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ProviderError::Config(format!("invalid header name: {name}")))?;
            let header_value = HeaderValue::from_str(&value)
                .map_err(|_| ProviderError::Config("invalid header value".to_string()))?;
            headers.insert(header_name, header_value);
        }

        // Forwarded client headers (allowlisted) — applied before extra_headers
        // so configured extra_headers win on name conflicts.
        for (k, v) in forward_headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| ProviderError::Config(format!("invalid header name: {k}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|_| ProviderError::Config(format!("invalid header value for {k}")))?;
            headers.insert(name, value);
        }

        for (k, v) in &self.extra_headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| ProviderError::Config(format!("invalid extra header name: {k}")))?;
            let value = HeaderValue::from_str(v).map_err(|_| {
                ProviderError::Config(format!("invalid extra header value for {k}"))
            })?;
            headers.insert(name, value);
        }

        Ok(headers)
    }

    fn redact_for_log(&self, headers: &HeaderMap) {
        let map: HashMap<String, String> = headers
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    v.to_str().unwrap_or("[non-utf8]").to_string(),
                )
            })
            .collect();
        let redacted = redact_headers(&map);
        debug!(
            provider = %self.provider_id,
            endpoint = %self.endpoint(),
            headers = ?redacted,
            "upstream request headers (redacted)"
        );
    }

    /// Send a POST request and return the raw response, mapping transport errors.
    async fn send_request(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<reqwest::Response, ProviderError> {
        self.client
            .post(self.endpoint())
            .headers(headers.clone())
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout {
                        provider: self.provider_id.clone(),
                        timeout_ms: self.read_timeout.as_millis() as u64,
                    }
                } else {
                    ProviderError::Transport {
                        provider: self.provider_id.clone(),
                        message: sanitize_error_message(&e.to_string()),
                    }
                }
            })
    }

    /// Check if an error is retryable.
    fn is_retryable(err: &ProviderError) -> bool {
        match err {
            ProviderError::Transport { .. } | ProviderError::Timeout { .. } => true,
            ProviderError::Upstream { status, .. } => RETRYABLE_STATUSES.contains(status),
            _ => false,
        }
    }

    /// Compute the delay for a retry attempt, honoring Retry-After if present.
    fn compute_retry_delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(ra) = retry_after {
            return ra.min(Duration::from_secs(10));
        }
        let base = self.retry.base_delay_ms;
        let max = self.retry.max_delay_ms;
        let exp = base.saturating_mul(1u64 << attempt.min(20));
        let capped = exp.min(max);
        let jitter_range = capped / 4;
        let jitter = if jitter_range > 0 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            (nanos as u64 % (jitter_range * 2)).saturating_sub(jitter_range)
        } else {
            0
        };
        let delay_ms = capped as i64 + jitter as i64;
        Duration::from_millis(delay_ms.max(0) as u64)
    }

    /// Extract Retry-After from a reqwest Response before consuming it.
    fn extract_retry_after(resp: &reqwest::Response) -> Option<Duration> {
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
    }

    /// Extract Retry-After from a ProviderError if it was stashed.
    fn extract_retry_after_from_error(err: &ProviderError) -> Option<Duration> {
        match err {
            ProviderError::Upstream { message, .. } => parse_retry_after_sentinel(message),
            _ => None,
        }
    }
}

/// Parse the `[retry-after:N]` sentinel prefix from a message string.
fn parse_retry_after_sentinel(message: &str) -> Option<Duration> {
    let rest = message.strip_prefix("[retry-after:")?;
    let end = rest.find(']')?;
    rest[..end].parse::<u64>().ok().map(Duration::from_secs)
}

/// Attach a Retry-After sentinel to an upstream error message.
fn stash_retry_after(err: ProviderError, retry_after: Option<Duration>) -> ProviderError {
    match (err, retry_after) {
        (
            ProviderError::Upstream {
                status,
                code,
                message,
                provider_id,
            },
            Some(d),
        ) => ProviderError::Upstream {
            status,
            code,
            message: format!("[retry-after:{}] {message}", d.as_secs()),
            provider_id,
        },
        (err, _) => err,
    }
}

/// Strip the retry-after sentinel from an error before returning it to callers.
fn strip_retry_after_sentinel(err: ProviderError) -> ProviderError {
    match err {
        ProviderError::Upstream {
            status,
            code,
            message,
            provider_id,
        } => {
            let clean_msg = if let Some(rest) = message.strip_prefix("[retry-after:") {
                if let Some(end) = rest.find(']') {
                    rest[end + 1..].trim_start().to_string()
                } else {
                    message
                }
            } else {
                message
            };
            ProviderError::Upstream {
                status,
                code,
                message: clean_msg,
                provider_id,
            }
        }
        err => err,
    }
}

impl ProviderAdapter for AnthropicAdapter {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn complete(
        &self,
        mut request: MessagesRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<MessagesResponse, ProviderError>> + Send + '_>,
    > {
        Box::pin(async move {
            self.validate_request(&request)?;
            request.stream = Some(false);

            let headers = self.build_headers(&request.forward_headers)?;
            self.redact_for_log(&headers);

            let body = serde_json::to_vec(&request)
                .map_err(|e| ProviderError::Parse(format!("failed to serialize request: {e}")))?;

            let max_attempts = self.retry.max_retries + 1;
            let mut last_err: Option<ProviderError> = None;

            for attempt in 0..max_attempts {
                if attempt > 0 {
                    let retry_after = last_err
                        .as_ref()
                        .and_then(Self::extract_retry_after_from_error);
                    let delay = self.compute_retry_delay(attempt - 1, retry_after);
                    warn!(
                        provider = %self.provider_id,
                        attempt = attempt,
                        cause = %last_err.as_ref().map(|e| e.to_string()).unwrap_or_default(),
                        "retrying upstream request"
                    );
                    tokio::time::sleep(delay).await;
                }

                match self.send_request(&headers, &body).await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let retry_after_header = Self::extract_retry_after(&resp);
                        let bytes = resp.bytes().await.map_err(|e| ProviderError::Transport {
                            provider: self.provider_id.clone(),
                            message: sanitize_error_message(&e.to_string()),
                        })?;

                        if (200..300).contains(&status) {
                            let parsed: MessagesResponse =
                                serde_json::from_slice(&bytes).map_err(|e| {
                                    ProviderError::Parse(format!(
                                        "failed to parse upstream response: {e}"
                                    ))
                                })?;
                            return Ok(parsed);
                        }

                        let err = ProviderError::upstream_from_body(
                            Some(self.provider_id.clone()),
                            status,
                            &bytes,
                            format!("upstream returned status {status}"),
                        );

                        if attempt + 1 < max_attempts && Self::is_retryable(&err) {
                            last_err = Some(stash_retry_after(err, retry_after_header));
                            continue;
                        }
                        return Err(strip_retry_after_sentinel(stash_retry_after(
                            err,
                            retry_after_header,
                        )));
                    }
                    Err(e) => {
                        if attempt + 1 < max_attempts && Self::is_retryable(&e) {
                            last_err = Some(e);
                            continue;
                        }
                        return Err(e);
                    }
                }
            }

            Err(last_err.map(strip_retry_after_sentinel).unwrap_or_else(|| {
                ProviderError::Transport {
                    provider: self.provider_id.clone(),
                    message: "request failed after all retries".to_string(),
                }
            }))
        })
    }

    fn stream(
        &self,
        mut request: MessagesRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderStream, ProviderError>> + Send + '_>,
    > {
        Box::pin(async move {
            self.validate_request(&request)?;
            request.stream = Some(true);

            let headers = self.build_headers(&request.forward_headers)?;
            self.redact_for_log(&headers);

            let body = serde_json::to_vec(&request)
                .map_err(|e| ProviderError::Parse(format!("failed to serialize request: {e}")))?;

            // Retry only the pre-stream connection phase.
            let max_attempts = self.retry.max_retries + 1;
            let mut last_err: Option<ProviderError> = None;

            let resp = {
                let mut attempt = 0u32;
                loop {
                    if attempt > 0 {
                        let retry_after = last_err
                            .as_ref()
                            .and_then(Self::extract_retry_after_from_error);
                        let delay = self.compute_retry_delay(attempt - 1, retry_after);
                        warn!(
                            provider = %self.provider_id,
                            attempt = attempt,
                            cause = %last_err.as_ref().map(|e| e.to_string()).unwrap_or_default(),
                            "retrying upstream stream request"
                        );
                        tokio::time::sleep(delay).await;
                    }

                    match self.send_request(&headers, &body).await {
                        Ok(resp) => {
                            let status = resp.status().as_u16();
                            if (200..300).contains(&status) {
                                break Ok(resp);
                            }

                            let retry_after_header = Self::extract_retry_after(&resp);
                            let bytes = resp.bytes().await.unwrap_or_else(|_| Bytes::from(""));
                            let err = ProviderError::upstream_from_body(
                                Some(self.provider_id.clone()),
                                status,
                                &bytes,
                                format!("upstream returned status {status}"),
                            );

                            if attempt + 1 < max_attempts && Self::is_retryable(&err) {
                                last_err = Some(stash_retry_after(err, retry_after_header));
                                attempt += 1;
                                continue;
                            }
                            break Err(strip_retry_after_sentinel(stash_retry_after(
                                err,
                                retry_after_header,
                            )));
                        }
                        Err(e) => {
                            if attempt + 1 < max_attempts && Self::is_retryable(&e) {
                                last_err = Some(e);
                                attempt += 1;
                                continue;
                            }
                            break Err(e);
                        }
                    }
                }
            }?;

            let provider_id = self.provider_id.clone();
            let read_timeout_ms = self.read_timeout.as_millis() as u64;
            let byte_stream = resp.bytes_stream();

            let sse_stream = futures_util::stream::try_unfold(
                (
                    byte_stream,
                    SseParser::new(),
                    provider_id.clone(),
                    read_timeout_ms,
                ),
                |(mut inner, mut parser, pid, rt_ms)| async move {
                    loop {
                        match inner.next().await {
                            Some(Ok(chunk)) => {
                                let raw_events = parser.feed(&chunk);
                                if raw_events.is_empty() {
                                    continue;
                                }
                                let normalized: Vec<Result<StreamEvent, ProviderError>> =
                                    raw_events
                                        .iter()
                                        .filter_map(|raw| {
                                            if let Some(ev) = normalize_sse_event(raw) {
                                                Some(Ok(ev))
                                            } else {
                                                debug!(provider = %pid, raw = ?raw, "unrecognized SSE frame");
                                                None
                                            }
                                        })
                                        .collect();
                                if normalized.is_empty() {
                                    continue;
                                }
                                return Ok(Some((normalized, (inner, parser, pid, rt_ms))));
                            }
                            Some(Err(e)) => {
                                if is_timeout_error(&e) {
                                    return Err(ProviderError::Timeout {
                                        provider: pid,
                                        timeout_ms: rt_ms,
                                    });
                                }
                                return Err(ProviderError::Stream {
                                    provider: pid,
                                    message: sanitize_error_message(&e.to_string()),
                                });
                            }
                            None => {
                                let trailing = parser.flush();
                                if trailing.is_empty() {
                                    return Ok(None);
                                }
                                let normalized: Vec<Result<StreamEvent, ProviderError>> = trailing
                                    .iter()
                                    .filter_map(|raw| normalize_sse_event(raw).map(Ok))
                                    .collect();
                                if normalized.is_empty() {
                                    return Ok(None);
                                }
                                return Ok(Some((normalized, (inner, parser, pid, rt_ms))));
                            }
                        }
                    }
                },
            );

            let flat = sse_stream.map_ok(futures_util::stream::iter).try_flatten();

            let boxed: ProviderStream = Box::pin(flat.map(|res| match res {
                Ok(ev) => Ok(ev),
                Err(e) => {
                    warn!(error = %e, "stream error");
                    Err(e)
                }
            }));
            Ok(boxed)
        })
    }
}

fn sanitize_error_message(msg: &str) -> String {
    let mut out = msg.to_string();
    if out.to_ascii_lowercase().contains("bearer") {
        out = out
            .lines()
            .map(|l| {
                if l.to_ascii_lowercase().contains("bearer") {
                    "[REDACTED bearer]".to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    out
}

fn is_timeout_error(e: &reqwest::Error) -> bool {
    e.is_timeout()
}
