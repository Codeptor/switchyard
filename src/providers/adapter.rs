//! Typed provider adapter boundary (port).
//!
//! The gateway depends only on this trait. Adapters are replaceable behind it.

use std::pin::Pin;

use futures_util::Stream;
use serde_json::Value;

use crate::providers::error::ProviderError;
use crate::providers::stream::StreamEvent;
use crate::providers::types::{MessagesRequest, MessagesResponse};

/// Resolved model handle returned by the registry.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
}

/// Stream of normalized [`StreamEvent`]s produced by an adapter.
///
/// The stream yields `Result<StreamEvent, ProviderError>` items and is
/// `Send` so the gateway can forward it directly to Claude Code as SSE.
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

/// Typed provider adapter port.
///
/// Each adapter is responsible for:
/// - translating [`MessagesRequest`] into its upstream wire format
/// - normalizing stop reasons, usage, and errors
/// - emitting normalized [`StreamEvent`]s
///
/// Implementations must be provider-agnostic at the trait level; provider
/// quirks stay inside the concrete adapter.
pub trait ProviderAdapter: Send + Sync {
    /// Provider identity this adapter serves.
    fn provider_id(&self) -> &str;

    /// Non-streaming completion.
    fn complete(
        &self,
        request: MessagesRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<MessagesResponse, ProviderError>> + Send + '_>,
    >;

    /// Streaming completion. Returns a stream of normalized events.
    fn stream(
        &self,
        request: MessagesRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderStream, ProviderError>> + Send + '_>,
    >;

    /// Optional health check / validation hook (e.g. test auth header).
    /// Default implementation does nothing.
    fn validate_request(&self, request: &MessagesRequest) -> Result<(), ProviderError> {
        request
            .validate()
            .map_err(|msg| ProviderError::Config(format!("invalid request: {msg}")))?;
        Ok(())
    }

    /// Raw-passthrough for debugging: serialize the normalized request to the
    /// upstream wire JSON (useful for tests). Default returns JSON of the
    /// typed request.
    fn wire_request_json(&self, request: &MessagesRequest) -> Result<Value, ProviderError> {
        serde_json::to_value(request).map_err(|e| ProviderError::Parse(e.to_string()))
    }
}
