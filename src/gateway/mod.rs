//! Claude Code-facing gateway implementation.

mod backend;
mod fallback;
mod hot_swap;
mod http;
mod provider_backend;
mod runtime;
mod telemetry;

pub use backend::{Backend, BackendError, BackendFuture, BackendRequest, BackendStream, ModelInfo};
pub use fallback::FallbackBackend;
pub use hot_swap::HotBackend;
pub use http::Gateway;
pub use provider_backend::ProviderBackend;
pub use runtime::ListenConfig;
pub use telemetry::{Telemetry, TelemetryState, UsageSnapshotRow};
