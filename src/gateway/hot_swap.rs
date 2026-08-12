//! Hot-swappable backend using `arc_swap`.
//!
//! [`HotBackend`] implements [`Backend`] by delegating to the current value of
//! an [`ArcSwap`]. Callers can atomically swap in a new backend without
//! disrupting in-flight requests.

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::Value;

use super::backend::{Backend, BackendFuture, BackendRequest, BackendStream, ModelInfo};

/// A backend that delegates to a swappable inner implementation.
pub struct HotBackend<B: Backend> {
    inner: Arc<ArcSwap<B>>,
}

impl<B: Backend> HotBackend<B> {
    pub fn new(backend: B) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(backend)),
        }
    }

    /// Swap in a new backend atomically.
    pub fn swap(&self, new: B) {
        self.inner.store(Arc::new(new));
    }

    /// Get a shared reference to the ArcSwap for external swapping.
    pub fn arc_swap(&self) -> &Arc<ArcSwap<B>> {
        &self.inner
    }
}

impl<B: Backend> Clone for HotBackend<B> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<B: Backend> Backend for HotBackend<B> {
    fn models(&self) -> Vec<ModelInfo> {
        self.inner.load().models()
    }

    fn complete(&self, request: BackendRequest) -> BackendFuture<'_, Value> {
        let backend = self.inner.load_full();
        Box::pin(async move { backend.complete(request).await })
    }

    fn stream(&self, request: BackendRequest) -> BackendFuture<'_, BackendStream> {
        let backend = self.inner.load_full();
        Box::pin(async move { backend.stream(request).await })
    }
}
