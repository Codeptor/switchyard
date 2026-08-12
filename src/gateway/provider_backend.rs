//! Bridge from the provider registry to the Claude Code gateway port.

use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::Value;

use crate::providers::{MessagesRequest, ProviderError, ProviderRegistry};

use super::backend::{
    Backend, BackendError, BackendFuture, BackendRequest, BackendStream, ModelInfo,
};

/// Gateway backend backed by the provider registry.
#[derive(Clone)]
pub struct ProviderBackend {
    registry: Arc<ProviderRegistry>,
}

impl ProviderBackend {
    pub fn new(registry: Arc<ProviderRegistry>) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &Arc<ProviderRegistry> {
        &self.registry
    }
}

impl Backend for ProviderBackend {
    fn models(&self) -> Vec<ModelInfo> {
        self.registry
            .provider_ids()
            .into_iter()
            .flat_map(|provider_id| {
                self.registry
                    .model_ids(&provider_id)
                    .unwrap_or_default()
                    .iter()
                    .map(move |model_id| ModelInfo::new(format!("{provider_id}/{model_id}")))
            })
            .collect()
    }

    fn complete(&self, request: BackendRequest) -> BackendFuture<'_, Value> {
        Box::pin(async move {
            let (provider_id, model_id) = split_model_id(&request.model)?;
            let handle = self
                .registry
                .resolve(provider_id, Some(model_id))
                .map_err(map_provider_error)?;
            let typed_request = parse_request(request.body, model_id)?;
            let response = handle
                .adapter
                .complete(typed_request)
                .await
                .map_err(map_provider_error)?;
            serde_json::to_value(response).map_err(|error| {
                BackendError::Internal(format!("failed to encode response: {error}"))
            })
        })
    }

    fn stream(&self, request: BackendRequest) -> BackendFuture<'_, BackendStream> {
        Box::pin(async move {
            let (provider_id, model_id) = split_model_id(&request.model)?;
            let handle = self
                .registry
                .resolve(provider_id, Some(model_id))
                .map_err(map_provider_error)?;
            let typed_request = parse_request(request.body, model_id)?;
            let provider_stream = handle
                .adapter
                .stream(typed_request)
                .await
                .map_err(map_provider_error)?;
            let normalized = provider_stream.map(|event| {
                event.map_err(map_provider_error).and_then(|event| {
                    serde_json::to_value(event).map_err(|error| {
                        BackendError::Internal(format!("failed to encode stream event: {error}"))
                    })
                })
            });
            Ok(Box::pin(normalized) as BackendStream)
        })
    }
}

fn split_model_id(model: &str) -> Result<(&str, &str), BackendError> {
    let (provider_id, model_id) = model.split_once('/').ok_or_else(|| {
        BackendError::ModelUnavailable(format!(
            "model '{model}' must use the provider/model format"
        ))
    })?;
    if provider_id.is_empty() || model_id.is_empty() {
        return Err(BackendError::ModelUnavailable(format!(
            "model '{model}' must use the provider/model format"
        )));
    }
    Ok((provider_id, model_id))
}

fn parse_request(body: Value, model_id: &str) -> Result<MessagesRequest, BackendError> {
    let mut object = body.as_object().cloned().ok_or_else(|| {
        BackendError::InvalidRequest("request body must be a JSON object".to_string())
    })?;
    object.insert("model".to_string(), Value::String(model_id.to_string()));
    serde_json::from_value(Value::Object(object)).map_err(|error| {
        BackendError::InvalidRequest(format!("invalid Anthropic Messages request: {error}"))
    })
}

fn map_provider_error(error: ProviderError) -> BackendError {
    match error {
        ProviderError::Config(message) => BackendError::InvalidRequest(message),
        ProviderError::AuthMissing { reason, .. } => {
            BackendError::upstream(401u16, "authentication_error", reason)
        }
        ProviderError::ProviderNotFound(provider) => BackendError::ModelUnavailable(provider),
        ProviderError::ModelNotFound { provider, model } => {
            BackendError::ModelUnavailable(format!("provider={provider} model={model}"))
        }
        ProviderError::Timeout { provider, .. } => {
            BackendError::Unavailable(format!("provider {provider} timed out"))
        }
        ProviderError::Transport { provider, message } => {
            BackendError::Unavailable(format!("provider {provider}: {message}"))
        }
        ProviderError::Parse(message) => BackendError::Internal(message),
        ProviderError::Upstream {
            status,
            code,
            message,
            ..
        } => BackendError::upstream(status, code, message),
        ProviderError::Stream { provider, message } => {
            BackendError::Unavailable(format!("provider {provider}: {message}"))
        }
    }
}
