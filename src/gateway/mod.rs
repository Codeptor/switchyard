//! Claude Code-facing gateway implementation.

mod backend;
mod http;
mod runtime;

pub use backend::{Backend, BackendError, BackendFuture, BackendRequest, BackendStream, ModelInfo};
pub use http::Gateway;
pub use runtime::ListenConfig;
