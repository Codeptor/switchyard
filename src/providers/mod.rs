//! Provider registry and upstream adapters.
//!
//! # Ownership
//! This crate owns the provider side of the Switchyard boundary.
//! Codex owns `src/gateway/`. Keep the interface minimal and typed:
//! provider identity, model identity, request forwarding, streaming events,
//! and normalized errors.
//!
//! # Architecture
//! Hexagonal / ports-and-adapters: the core [`ProviderRegistry`] depends only
//! on the [`ProviderAdapter`] port. The generic [`AnthropicAdapter`] is an
//! outbound adapter behind that port. Transport, auth, and provider quirks stay
//! inside the adapter.

pub mod adapter;
pub mod anthropic;
pub mod config;
pub mod credentials;
pub mod error;
pub mod registry;
pub mod stream;
pub mod types;

// Re-exports for Codex boundary.
pub use adapter::{ProviderAdapter, ResolvedModel};
pub use anthropic::AnthropicAdapter;
pub use config::{AuthConfig, ModelCapabilities, ModelConfig, ProviderConfig, RetryConfig};
pub use credentials::{load_credential, redact_headers};
pub use error::{ProviderError, UpstreamErrorDetails};
pub use registry::ProviderRegistry;
pub use stream::{SseEvent, StreamEvent};
pub use types::{
    AnthropicErrorBody, ContentBlock, MessagesRequest, MessagesResponse, StopReason, Tool,
    ToolChoice, Usage,
};
