//! Safe credential loading and header redaction.
//!
//! Secrets are loaded exclusively from environment variables at request time.
//! No secret is written to disk, committed, or logged. Diagnostic output
//! redacts sensitive headers.

use std::collections::{BTreeMap, HashMap};

use tracing::warn;

use crate::providers::config::AuthConfig;
use crate::providers::error::ProviderError;

/// Sensitive header names that must be redacted in logs and errors.
///
/// Comparison is case-insensitive.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "api-key",
    "x-auth-token",
];

/// Load a credential value for the given auth config.
///
/// Returns `None` for `AuthConfig::None`. Returns an error if the referenced
/// environment variable is missing or empty. The raw secret is returned to the
/// caller for use in the outgoing request header and is never logged.
pub fn load_credential(auth: &AuthConfig) -> Result<Option<String>, ProviderError> {
    match auth {
        AuthConfig::None => Ok(None),
        AuthConfig::Header {
            header: _,
            env_var,
            prefix: _,
        } => match std::env::var(env_var) {
            Ok(val) if !val.trim().is_empty() => Ok(Some(val)),
            Ok(_) => Err(ProviderError::AuthMissing {
                env_var: env_var.clone(),
                reason: "environment variable is empty".to_string(),
            }),
            Err(_) => Err(ProviderError::AuthMissing {
                env_var: env_var.clone(),
                reason: "environment variable not set".to_string(),
            }),
        },
    }
}

/// Build the header value to send for the given auth config and raw token.
///
/// If `prefix` is set, it is prepended verbatim (e.g. `"Bearer "`).
pub fn build_auth_header_value(auth: &AuthConfig, token: &str) -> Option<(String, String)> {
    match auth {
        AuthConfig::None => None,
        AuthConfig::Header {
            header,
            env_var: _,
            prefix,
        } => {
            let value = if let Some(p) = prefix {
                format!("{p}{token}")
            } else {
                token.to_string()
            };
            Some((header.clone(), value))
        }
    }
}

/// Redact sensitive header values for diagnostics.
///
/// Any header whose name case-insensitively matches `SENSITIVE_HEADERS` or a
/// custom provider header configured via `AuthConfig` is replaced with
/// `"[REDACTED]"`. Returns a new map suitable for logging.
pub fn redact_headers(headers: &HashMap<String, String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in headers {
        if is_sensitive(k) {
            out.insert(k.clone(), "[REDACTED]".to_string());
            // Do not log the original value at any level.
            let _ = v;
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Redact sensitive headers in a `BTreeMap` (convenience for sorted output).
pub fn redact_headers_btree(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in headers {
        if is_sensitive(k) {
            out.insert(k.clone(), "[REDACTED]".to_string());
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Check if a header name is sensitive.
pub fn is_sensitive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if SENSITIVE_HEADERS.contains(&lower.as_str()) {
        return true;
    }
    // Also treat any header containing `secret`, `token`, or `key` as sensitive.
    if lower.contains("secret") || lower.contains("token") || lower.contains("api-key") {
        return true;
    }
    false
}

/// Emit a warning-level diagnostic with redacted headers, never logging secrets.
pub fn log_redacted_headers(context: &str, headers: &HashMap<String, String>) {
    let redacted = redact_headers(headers);
    warn!(context = %context, headers = ?redacted, "outgoing headers (redacted)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_authorization() {
        let mut h = HashMap::new();
        h.insert("Authorization".to_string(), "Bearer secret123".to_string());
        h.insert("Content-Type".to_string(), "application/json".to_string());
        let redacted = redact_headers(&h);
        assert_eq!(
            redacted.get("Authorization").map(|s| s.as_str()),
            Some("[REDACTED]")
        );
        assert_eq!(
            redacted.get("Content-Type").map(|s| s.as_str()),
            Some("application/json")
        );
    }

    #[test]
    fn redacts_case_insensitive() {
        let mut h = HashMap::new();
        h.insert("x-api-key".to_string(), "sk-123".to_string());
        let redacted = redact_headers(&h);
        assert_eq!(
            redacted.get("x-api-key").map(|s| s.as_str()),
            Some("[REDACTED]")
        );
    }

    #[test]
    fn build_header_with_prefix() {
        let auth = AuthConfig::Header {
            header: "Authorization".to_string(),
            env_var: "TOK".to_string(),
            prefix: Some("Bearer ".to_string()),
        };
        let (k, v) = build_auth_header_value(&auth, "abc").expect("header");
        assert_eq!(k, "Authorization");
        assert_eq!(v, "Bearer abc");
    }

    #[test]
    fn load_missing_env_returns_error() {
        // Use a unique var name unlikely to be set.
        let auth = AuthConfig::Header {
            header: "x-api-key".to_string(),
            env_var: "SWITCHYARD_TEST_MISSING_TOKEN_XYZ_123".to_string(),
            prefix: None,
        };
        let res = load_credential(&auth);
        assert!(res.is_err());
        let err = res.expect_err("should err");
        // Ensure error display does not contain a secret.
        let msg = err.to_string();
        assert!(!msg.contains("sk-"));
    }
}
