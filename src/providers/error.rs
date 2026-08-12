//! Normalized error types and upstream-error mapping.
//!
//! All provider errors are normalized into a typed [`ProviderError`]. The
//! gateway can translate these into Anthropic-compatible error envelopes
//! without inspecting provider-specific strings.

use thiserror::Error;

/// Details extracted from a non-2xx upstream response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamErrorDetails {
    /// HTTP status code.
    pub status: u16,
    /// Normalized error type (e.g. `invalid_request_error`, `authentication_error`).
    pub code: String,
    /// Human-readable message (sanitized, no headers/secrets).
    pub message: String,
    /// Provider that produced the error (for routing diagnostics).
    pub provider_id: Option<String>,
}

/// Normalized provider errors.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("missing credential: env_var={env_var}: {reason}")]
    AuthMissing { env_var: String, reason: String },

    #[error("provider not found: {0}")]
    ProviderNotFound(String),

    #[error("model not found: provider={provider} model={model}")]
    ModelNotFound { provider: String, model: String },

    #[error("timeout after {timeout_ms}ms for provider {provider}")]
    Timeout { provider: String, timeout_ms: u64 },

    #[error("transport error for provider {provider}: {message}")]
    Transport { provider: String, message: String },

    #[error("parse error: {0}")]
    Parse(String),

    #[error("upstream error: status={status} code={code} message={message}")]
    Upstream {
        status: u16,
        code: String,
        message: String,
        provider_id: Option<String>,
    },

    #[error("stream error for provider {provider}: {message}")]
    Stream { provider: String, message: String },
}

impl ProviderError {
    /// HTTP status that the gateway should return for this error.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Config(_) => 500,
            Self::AuthMissing { .. } => 500,
            Self::ProviderNotFound(_) => 404,
            Self::ModelNotFound { .. } => 404,
            Self::Timeout { .. } => 504,
            Self::Transport { .. } => 502,
            Self::Parse(_) => 502,
            Self::Upstream { status, .. } => *status,
            Self::Stream { .. } => 502,
        }
    }

    /// Normalized Anthropic error `type` string.
    pub fn anthropic_type(&self) -> &'static str {
        match self {
            Self::Config(_) => "api_error",
            Self::AuthMissing { .. } => "authentication_error",
            Self::ProviderNotFound(_) => "not_found_error",
            Self::ModelNotFound { .. } => "not_found_error",
            Self::Timeout { .. } => "api_error",
            Self::Transport { .. } => "api_error",
            Self::Parse(_) => "api_error",
            Self::Upstream { code, .. } => match code.as_str() {
                "invalid_request_error" => "invalid_request_error",
                "authentication_error" => "authentication_error",
                "permission_error" => "permission_error",
                "not_found_error" => "not_found_error",
                "rate_limit_error" => "rate_limit_error",
                "api_error" => "api_error",
                "overloaded_error" => "overloaded_error",
                _ => "api_error",
            },
            Self::Stream { .. } => "api_error",
        }
    }

    /// Convert into an Anthropic-compatible error body.
    pub fn into_anthropic_body(self) -> crate::providers::types::AnthropicErrorBody {
        let (type_str, message) = match &self {
            Self::Upstream { code, message, .. } => (code.clone(), message.clone()),
            other => (other.anthropic_type().to_string(), other.to_string()),
        };
        crate::providers::types::AnthropicErrorBody {
            r#type: "error".to_string(),
            error: crate::providers::types::AnthropicErrorDetail {
                r#type: type_str,
                message,
            },
        }
    }

    /// Create an upstream error from raw response bytes, attempting to parse
    /// an Anthropic error envelope before falling back to raw text.
    pub fn upstream_from_body(
        provider_id: Option<String>,
        status: u16,
        body: &[u8],
        fallback_message: String,
    ) -> Self {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) {
            if let Some(err) = parsed.get("error") {
                let code = err
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("api_error")
                    .to_string();
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&fallback_message)
                    .to_string();
                // Never include raw headers/body secrets; message is from upstream JSON.
                return Self::Upstream {
                    status,
                    code,
                    message,
                    provider_id,
                };
            }
            if let Some(msg) = parsed.get("message").and_then(|v| v.as_str()) {
                return Self::Upstream {
                    status,
                    code: "api_error".to_string(),
                    message: msg.to_string(),
                    provider_id,
                };
            }
        }
        let snippet = sanitize_body_snippet(body);
        let message = if snippet.is_empty() {
            fallback_message
        } else {
            snippet
        };
        Self::Upstream {
            status,
            code: map_status_to_code(status),
            message,
            provider_id,
        }
    }
}

fn map_status_to_code(status: u16) -> String {
    match status {
        400 => "invalid_request_error".to_string(),
        401 => "authentication_error".to_string(),
        403 => "permission_error".to_string(),
        404 => "not_found_error".to_string(),
        429 => "rate_limit_error".to_string(),
        529 => "overloaded_error".to_string(),
        _ => "api_error".to_string(),
    }
}

fn sanitize_body_snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let mut s = text.trim().to_string();
    if s.len() > 500 {
        s.truncate(500);
        s.push('…');
    }
    // Strip any accidental header-like lines.
    s.lines()
        .filter(|l| {
            let low = l.to_ascii_lowercase();
            !(low.contains("authorization") || low.contains("x-api-key") || low.contains("bearer"))
        })
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}
