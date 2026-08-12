//! Provider-independent gateway backend boundary.

use std::future::Future;
use std::pin::Pin;

use futures_util::Stream;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

/// A request passed from the Claude Code-facing HTTP layer to the provider
/// backend. The JSON body remains opaque so provider adapters can preserve
/// fields added by newer Claude Code versions.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendRequest {
    pub model: String,
    pub body: Value,
    /// Client headers from the allowlist that must be forwarded upstream
    /// (currently only `anthropic-beta`).
    pub forward_headers: Vec<(String, String)>,
}

/// A model exposed by Switchyard's local catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: &'static str,
}

impl ModelInfo {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            object: "model",
            created: 0,
            owned_by: "switchyard",
        }
    }
}

/// Errors returned by the provider backend.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum BackendError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("upstream error: status={status} code={code} message={message}")]
    Upstream {
        status: u16,
        code: String,
        message: String,
    },
    #[error("upstream unavailable: {0}")]
    Unavailable(String),
    #[error("internal backend error: {0}")]
    Internal(String),
}

impl BackendError {
    pub fn upstream(
        status: impl Into<u16>,
        code: impl Into<String>,
        message: impl AsRef<str>,
    ) -> Self {
        Self::Upstream {
            status: status.into(),
            code: code.into(),
            message: sanitize_message(message.as_ref()),
        }
    }

    pub fn status_code(&self) -> u16 {
        match self {
            Self::InvalidRequest(_) => 400,
            Self::ModelUnavailable(_) => 404,
            Self::Upstream { status, .. } => *status,
            Self::Unavailable(_) => 502,
            Self::Internal(_) => 500,
        }
    }

    pub fn anthropic_type(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request_error",
            Self::ModelUnavailable(_) => "not_found_error",
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
            Self::Unavailable(_) | Self::Internal(_) => "api_error",
        }
    }

    pub fn public_message(&self) -> String {
        match self {
            Self::InvalidRequest(message) | Self::ModelUnavailable(message) => message.clone(),
            Self::Upstream { message, .. } => sanitize_message(message),
            Self::Unavailable(_) => "upstream provider unavailable".to_string(),
            Self::Internal(_) => "internal gateway error".to_string(),
        }
    }
}

/// Future returned by a backend operation.
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, BackendError>> + Send + 'a>>;

/// Normalized streaming events. Each value must be an Anthropic event object
/// containing a string `type` field.
pub type BackendStream = Pin<Box<dyn Stream<Item = Result<Value, BackendError>> + Send>>;

/// Backend port consumed by the Claude Code gateway.
pub trait Backend: Send + Sync + 'static {
    fn models(&self) -> Vec<ModelInfo>;
    fn complete(&self, request: BackendRequest) -> BackendFuture<'_, Value>;
    fn stream(&self, request: BackendRequest) -> BackendFuture<'_, BackendStream>;
}

fn sanitize_message(message: &str) -> String {
    message
        .split_whitespace()
        .map(|part| {
            let lower = part.to_ascii_lowercase();
            if lower.starts_with("sk-")
                || lower.starts_with("token=")
                || lower.starts_with("api_key=")
                || lower.starts_with("apikey=")
                || lower.starts_with("bearer=")
            {
                "[REDACTED]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
