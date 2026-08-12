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
use crate::providers::config::{AuthConfig, ProviderConfig};
use crate::providers::credentials::{build_auth_header_value, load_credential, redact_headers};
use crate::providers::error::ProviderError;
use crate::providers::stream::{SseParser, StreamEvent, normalize_sse_event};
use crate::providers::types::{MessagesRequest, MessagesResponse};

/// Generic Anthropic-compatible upstream adapter.
///
/// Configuration is validated on construction. Secrets are loaded from the
/// environment at request time, never stored.
#[derive(Debug, Clone)]
pub struct AnthropicAdapter {
    provider_id: String,
    base_url: String,
    auth: AuthConfig,
    timeout: Duration,
    extra_headers: Vec<(String, String)>,
    client: reqwest::Client,
}

impl AnthropicAdapter {
    /// Create an adapter from a validated [`ProviderConfig`].
    pub fn from_config(config: &ProviderConfig) -> Result<Self, ProviderError> {
        config
            .validate()
            .map_err(|e| ProviderError::Config(e.to_string()))?;

        let timeout = Duration::from_millis(config.timeout_ms.unwrap_or(60_000));

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Config(format!("failed to build http client: {e}")))?;

        Ok(Self {
            provider_id: config.id.clone(),
            base_url: config.normalized_base_url(),
            auth: config.auth.clone(),
            timeout,
            extra_headers: config.extra_headers.clone(),
            client,
        })
    }

    /// Direct constructor for tests (bypasses config parsing).
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        auth: AuthConfig,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        // Validate URL.
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
            .timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Config(format!("failed to build http client: {e}")))?;

        Ok(Self {
            provider_id: provider_id.into(),
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            timeout,
            extra_headers: vec![],
            client,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    fn build_headers(&self) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );

        if let Some(token) = load_credential(&self.auth)? {
            if let Some((name, value)) = build_auth_header_value(&self.auth, &token) {
                let header_name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| ProviderError::Config(format!("invalid header name: {name}")))?;
                let header_value = HeaderValue::from_str(&value)
                    .map_err(|_| ProviderError::Config("invalid header value".to_string()))?;
                headers.insert(header_name, header_value);
            }
        } else if !matches!(self.auth, AuthConfig::None) {
            // load_credential would have errored for missing env; this path
            // is reachable only if auth is None.
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
            // Ensure stream is false for non-streaming path.
            request.stream = Some(false);

            let headers = self.build_headers()?;
            self.redact_for_log(&headers);

            let body = serde_json::to_vec(&request)
                .map_err(|e| ProviderError::Parse(format!("failed to serialize request: {e}")))?;

            let resp = self
                .client
                .post(self.endpoint())
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        ProviderError::Timeout {
                            provider: self.provider_id.clone(),
                            timeout_ms: self.timeout.as_millis() as u64,
                        }
                    } else {
                        ProviderError::Transport {
                            provider: self.provider_id.clone(),
                            message: sanitize_error_message(&e.to_string()),
                        }
                    }
                })?;

            let status = resp.status().as_u16();
            let bytes = resp.bytes().await.map_err(|e| ProviderError::Transport {
                provider: self.provider_id.clone(),
                message: sanitize_error_message(&e.to_string()),
            })?;

            if !(200..300).contains(&status) {
                return Err(ProviderError::upstream_from_body(
                    Some(self.provider_id.clone()),
                    status,
                    &bytes,
                    format!("upstream returned status {status}"),
                ));
            }

            let parsed: MessagesResponse = serde_json::from_slice(&bytes).map_err(|e| {
                ProviderError::Parse(format!("failed to parse upstream response: {e}"))
            })?;

            Ok(parsed)
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

            let headers = self.build_headers()?;
            self.redact_for_log(&headers);

            let body = serde_json::to_vec(&request)
                .map_err(|e| ProviderError::Parse(format!("failed to serialize request: {e}")))?;

            let resp = self
                .client
                .post(self.endpoint())
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        ProviderError::Timeout {
                            provider: self.provider_id.clone(),
                            timeout_ms: self.timeout.as_millis() as u64,
                        }
                    } else {
                        ProviderError::Transport {
                            provider: self.provider_id.clone(),
                            message: sanitize_error_message(&e.to_string()),
                        }
                    }
                })?;

            let status = resp.status().as_u16();
            if !(200..300).contains(&status) {
                let bytes = resp.bytes().await.unwrap_or_else(|_| Bytes::from(""));
                return Err(ProviderError::upstream_from_body(
                    Some(self.provider_id.clone()),
                    status,
                    &bytes,
                    format!("upstream returned status {status}"),
                ));
            }

            let provider_id = self.provider_id.clone();
            let byte_stream = resp.bytes_stream();

            // Convert bytes_stream into a StreamEvent stream via SSE parsing.
            let sse_stream = futures_util::stream::try_unfold(
                (byte_stream, SseParser::new(), provider_id.clone()),
                |(mut inner, mut parser, pid)| async move {
                    loop {
                        match inner.next().await {
                            Some(Ok(chunk)) => {
                                let raw_events = parser.feed(&chunk);
                                if raw_events.is_empty() {
                                    continue;
                                }
                                // Return one batch of normalized events at a time.
                                // For simplicity, unfold yields a Vec batch; flatten via stream.
                                let normalized: Vec<Result<StreamEvent, ProviderError>> = raw_events
                                .iter()
                                .filter_map(|raw| {
                                    if let Some(ev) = normalize_sse_event(raw) {
                                        Some(Ok(ev))
                                    } else {
                                        // Ignore unrecognized frames, but log at debug.
                                        debug!(provider = %pid, raw = ?raw, "unrecognized SSE frame");
                                        None
                                    }
                                })
                                .collect();
                                if normalized.is_empty() {
                                    continue;
                                }
                                // Use a channel-like unfold by returning the batch with state.
                                // We return the first event now and buffer the rest via a follow-up poll.
                                // Simpler: return a stream of events flattened externally.
                                // Here, we yield the batch as a single item; caller flattens.
                                return Ok(Some((normalized, (inner, parser, pid))));
                            }
                            Some(Err(e)) => {
                                if is_timeout_error(&e) {
                                    return Err(ProviderError::Timeout {
                                        provider: pid,
                                        timeout_ms: 0,
                                    });
                                }
                                return Err(ProviderError::Stream {
                                    provider: pid,
                                    message: sanitize_error_message(&e.to_string()),
                                });
                            }
                            None => {
                                // Flush any trailing buffer.
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
                                return Ok(Some((normalized, (inner, parser, pid))));
                            }
                        }
                    }
                },
            );

            // Flatten Vec batches into individual events.
            let flat = sse_stream.map_ok(futures_util::stream::iter).try_flatten();

            // If the upstream sends a non-SSE error body with 200, normalize it.
            // The gateway will observe StreamEvent::Error variants.

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
    // Redact any accidental secret leakage in error strings.
    let mut out = msg.to_string();
    // Simple heuristic: replace long base64-ish tokens.
    // We do not attempt to parse headers here; just avoid obvious leaks.
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
