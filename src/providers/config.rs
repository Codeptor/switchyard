//! Configuration schema, validation, and normalization for providers and models.
//!
//! Provider-agnostic: no hardcoded provider names, URLs, or auth schemes.
//! Manual model IDs are sufficient; no `/v1/models` discovery is required.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

/// Errors produced during configuration validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("provider id is empty or invalid: {0}")]
    InvalidProviderId(String),
    #[error("model id is empty or invalid: {0}")]
    InvalidModelId(String),
    #[error("base_url is invalid: {0}")]
    InvalidBaseUrl(String),
    #[error("env_var is invalid: {0}")]
    InvalidEnvVar(String),
    #[error("header name is empty")]
    InvalidHeaderName,
    #[error("duplicate provider id: {0}")]
    DuplicateProviderId(String),
    #[error("duplicate model id in provider {provider}: {model}")]
    DuplicateModelId { provider: String, model: String },
    #[error("timeout_ms must be > 0, got {0}")]
    InvalidTimeout(u64),
    #[error("default_model '{0}' not found in provider models")]
    DefaultModelNotFound(String),
}

/// Authentication configuration. Generic over providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    /// No authentication (useful for local mocks).
    #[default]
    None,
    /// Header-based auth where the token is read from an environment variable.
    ///
    /// `header` is the HTTP header name (e.g. `Authorization`, `x-api-key`,
    /// or provider-specific). `env_var` is the environment variable that
    /// holds the secret. `prefix` is prepended to the token value if set,
    /// e.g. `Some("Bearer ")` yields `Authorization: Bearer <token>`.
    Header {
        header: String,
        env_var: String,
        #[serde(default)]
        prefix: Option<String>,
    },
}

impl AuthConfig {
    /// Validate auth config without reading secrets.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::None => Ok(()),
            Self::Header {
                header,
                env_var,
                prefix: _,
            } => {
                if header.trim().is_empty() {
                    return Err(ConfigError::InvalidHeaderName);
                }
                validate_env_var(env_var)?;
                validate_header_name(header)?;
                Ok(())
            }
        }
    }
}

/// Capabilities advertised by a model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    #[serde(default = "default_true")]
    pub supports_tools: bool,
    #[serde(default = "default_true")]
    pub supports_streaming: bool,
    #[serde(default)]
    pub supports_vision: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_tools: true,
            supports_streaming: true,
            supports_vision: false,
        }
    }
}

/// A single model entry under a provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelConfig {
    /// Model identifier as exposed to Claude Code (e.g. `my-model-v1`).
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
}

impl ModelConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_model_id(&self.id)?;
        Ok(())
    }
}

/// Provider-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    /// Provider identity (e.g. `example`). Must match `^[a-zA-Z0-9_.-]+$`.
    pub id: String,
    /// Base URL for the upstream Anthropic-compatible endpoint (no trailing path required).
    /// The adapter appends `/v1/messages`.
    pub base_url: Url,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
}

impl ProviderConfig {
    /// Validate the config.
    ///
    /// Normalization:
    /// - rejects surrounding whitespace in ids,
    /// - rejects empty ids,
    /// - validates base_url scheme,
    /// - validates auth and models.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_provider_id(&self.id)?;

        // Base URL validation: require http or https.
        match self.base_url.scheme() {
            "http" | "https" => {}
            other => {
                return Err(ConfigError::InvalidBaseUrl(format!(
                    "unsupported scheme '{other}' in {}",
                    self.base_url
                )));
            }
        }
        if self.base_url.host_str().is_none() {
            return Err(ConfigError::InvalidBaseUrl(format!(
                "missing host in {}",
                self.base_url
            )));
        }

        self.auth.validate()?;

        if let Some(ms) = self.timeout_ms
            && ms == 0
        {
            return Err(ConfigError::InvalidTimeout(ms));
        }

        let mut seen = std::collections::HashSet::new();
        for m in &self.models {
            m.validate()?;
            if !seen.insert(m.id.clone()) {
                return Err(ConfigError::DuplicateModelId {
                    provider: self.id.clone(),
                    model: m.id.clone(),
                });
            }
        }

        if let Some(default) = &self.default_model
            && !self.models.iter().any(|m| &m.id == default)
        {
            return Err(ConfigError::DefaultModelNotFound(default.clone()));
        }

        for (k, _) in &self.extra_headers {
            validate_header_name(k)?;
        }

        Ok(())
    }

    /// Normalized base URL string without trailing slash.
    pub fn normalized_base_url(&self) -> String {
        let s = self.base_url.to_string();
        s.trim_end_matches('/').to_string()
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_provider_id(id: &str) -> Result<(), ConfigError> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed != id {
        return Err(ConfigError::InvalidProviderId(id.to_string()));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(ConfigError::InvalidProviderId(id.to_string()));
    }
    Ok(())
}

fn validate_model_id(id: &str) -> Result<(), ConfigError> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed != id {
        return Err(ConfigError::InvalidModelId(id.to_string()));
    }
    // Model IDs are provider-defined. Reject only whitespace/control characters
    // so suffixes such as Qwen/Kimi's `[1m]` remain valid.
    if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ConfigError::InvalidModelId(id.to_string()));
    }
    Ok(())
}

fn validate_env_var(env_var: &str) -> Result<(), ConfigError> {
    let trimmed = env_var.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidEnvVar(env_var.to_string()));
    }
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return Err(ConfigError::InvalidEnvVar(env_var.to_string())),
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(ConfigError::InvalidEnvVar(env_var.to_string()));
    }
    Ok(())
}

fn validate_header_name(header: &str) -> Result<(), ConfigError> {
    let trimmed = header.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidHeaderName);
    }
    // RFC 7230 token.
    if !trimmed.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '!' | '#'
                    | '$'
                    | '%'
                    | '&'
                    | '\''
                    | '*'
                    | '+'
                    | '-'
                    | '.'
                    | '^'
                    | '_'
                    | '`'
                    | '|'
                    | '~'
            )
    }) {
        return Err(ConfigError::InvalidHeaderName);
    }
    // Also forbid header injection.
    if trimmed.contains(':') || trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(ConfigError::InvalidHeaderName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("valid url")
    }

    #[test]
    fn auth_none_valid() {
        assert!(AuthConfig::None.validate().is_ok());
    }

    #[test]
    fn header_auth_valid() {
        let a = AuthConfig::Header {
            header: "Authorization".to_string(),
            env_var: "MY_TOKEN".to_string(),
            prefix: Some("Bearer ".to_string()),
        };
        assert!(a.validate().is_ok());
    }

    #[test]
    fn rejects_empty_provider_id() {
        let cfg = ProviderConfig {
            id: "".to_string(),
            base_url: url("https://example.com"),
            auth: AuthConfig::None,
            models: vec![],
            timeout_ms: None,
            default_model: None,
            extra_headers: vec![],
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_surrounding_whitespace_in_provider_and_model_ids() {
        let provider = ProviderConfig {
            id: " p1 ".to_string(),
            base_url: url("https://example.com"),
            auth: AuthConfig::None,
            models: vec![],
            timeout_ms: None,
            default_model: None,
            extra_headers: vec![],
        };
        assert!(provider.validate().is_err());

        let model = ProviderConfig {
            id: "p1".to_string(),
            base_url: url("https://example.com"),
            auth: AuthConfig::None,
            models: vec![ModelConfig {
                id: " m1 ".to_string(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
                capabilities: ModelCapabilities::default(),
            }],
            timeout_ms: None,
            default_model: None,
            extra_headers: vec![],
        };
        assert!(model.validate().is_err());
    }

    #[test]
    fn normalizes_base_url() {
        let cfg = ProviderConfig {
            id: "p1".to_string(),
            base_url: url("https://example.com/"),
            auth: AuthConfig::None,
            models: vec![],
            timeout_ms: None,
            default_model: None,
            extra_headers: vec![],
        };
        assert_eq!(cfg.normalized_base_url(), "https://example.com");
    }
}
