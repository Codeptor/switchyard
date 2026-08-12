//! Fallback routing backend.
//!
//! Wraps another [`Backend`] and, on eligible errors, retries the request
//! against an ordered list of fallback routes. Eligibility is limited to
//! transient upstream failures — client errors and model-not-found responses
//! pass through untouched.

use std::collections::BTreeMap;

use serde_json::Value;
use tracing::warn;

use super::backend::{
    Backend, BackendError, BackendFuture, BackendRequest, BackendStream, ModelInfo,
};

/// Backend that retries eligible failures against configured fallback routes.
pub struct FallbackBackend<B> {
    inner: B,
    fallbacks: BTreeMap<String, Vec<String>>,
}

impl<B: Backend> FallbackBackend<B> {
    pub fn new(inner: B, fallbacks: BTreeMap<String, Vec<String>>) -> Self {
        Self { inner, fallbacks }
    }

    pub fn inner(&self) -> &B {
        &self.inner
    }
}

impl<B: Backend> Backend for FallbackBackend<B> {
    fn models(&self) -> Vec<ModelInfo> {
        self.inner.models()
    }

    fn complete(&self, request: BackendRequest) -> BackendFuture<'_, Value> {
        let targets = self.fallbacks.get(&request.model).cloned();
        Box::pin(async move {
            let result = self.inner.complete(request.clone()).await;
            match result {
                Ok(value) => Ok(value),
                Err(error) => {
                    if !is_fallback_eligible(&error) {
                        return Err(error);
                    }
                    let Some(targets) = targets else {
                        return Err(error);
                    };
                    try_fallbacks(&self.inner, &request, &targets, error).await
                }
            }
        })
    }

    fn stream(&self, request: BackendRequest) -> BackendFuture<'_, BackendStream> {
        let targets = self.fallbacks.get(&request.model).cloned();
        Box::pin(async move {
            let result = self.inner.stream(request.clone()).await;
            match result {
                Ok(stream) => Ok(stream),
                Err(error) => {
                    if !is_fallback_eligible(&error) {
                        return Err(error);
                    }
                    let Some(targets) = targets else {
                        return Err(error);
                    };
                    try_fallback_streams(&self.inner, &request, &targets, error).await
                }
            }
        })
    }
}

fn is_fallback_eligible(error: &BackendError) -> bool {
    match error {
        BackendError::Unavailable(_) => true,
        BackendError::Upstream { status, .. } => matches!(status, 429 | 500 | 502 | 503 | 529),
        _ => false,
    }
}

async fn try_fallbacks<B: Backend>(
    inner: &B,
    original: &BackendRequest,
    targets: &[String],
    mut last_error: BackendError,
) -> Result<Value, BackendError> {
    for target in targets {
        warn!(
            original_route = %original.model,
            fallback_route = %target,
            cause = %last_error,
            "fallback: retrying on alternate route"
        );
        let mut req = original.clone();
        req.model = target.clone();
        match inner.complete(req).await {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = error;
            }
        }
    }
    Err(last_error)
}

async fn try_fallback_streams<B: Backend>(
    inner: &B,
    original: &BackendRequest,
    targets: &[String],
    mut last_error: BackendError,
) -> Result<BackendStream, BackendError> {
    for target in targets {
        warn!(
            original_route = %original.model,
            fallback_route = %target,
            cause = %last_error,
            "fallback: retrying on alternate route"
        );
        let mut req = original.clone();
        req.model = target.clone();
        match inner.stream(req).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                last_error = error;
            }
        }
    }
    Err(last_error)
}
