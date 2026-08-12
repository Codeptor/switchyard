//! Claude Code-facing gateway implementation.

mod backend;
mod http;
mod provider_backend;
mod runtime;

pub use backend::{Backend, BackendError, BackendFuture, BackendRequest, BackendStream, ModelInfo};
pub use http::Gateway;
pub use provider_backend::ProviderBackend;
pub use runtime::ListenConfig;
