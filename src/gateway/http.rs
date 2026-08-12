//! Claude Code-facing Anthropic Messages HTTP surface.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Json, State, rejection::JsonRejection};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::get, routing::post};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::backend::{Backend, BackendError, BackendRequest, BackendStream, ModelInfo};

struct AppState<B> {
    backend: Arc<B>,
}

impl<B> Clone for AppState<B> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
        }
    }
}

/// Claude Code-facing gateway.
pub struct Gateway<B> {
    backend: Arc<B>,
}

impl<B> Clone for Gateway<B> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
        }
    }
}

impl<B: Backend> Gateway<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/v1/models", get(models::<B>))
            .route("/v1/messages", post(messages::<B>))
            .with_state(AppState {
                backend: self.backend,
            })
    }

    /// Serve the gateway on an already-bound WSL/Linux TCP listener.
    pub async fn serve(self, listener: TcpListener) -> io::Result<()> {
        axum::serve(listener, self.router()).await
    }

    /// Bind and serve on the supplied address.
    pub async fn bind_and_serve(self, address: SocketAddr) -> io::Result<()> {
        let listener = TcpListener::bind(address).await?;
        self.serve(listener).await
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct ModelList {
    object: &'static str,
    data: Vec<ModelInfo>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    #[serde(rename = "type")]
    kind: &'static str,
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

async fn health() -> impl IntoResponse {
    axum::Json(HealthResponse { status: "ok" })
}

async fn models<B: Backend>(State(state): State<AppState<B>>) -> impl IntoResponse {
    axum::Json(ModelList {
        object: "list",
        data: state.backend.models(),
    })
}

async fn messages<B: Backend>(
    State(state): State<AppState<B>>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body = match payload {
        Ok(Json(body)) => body,
        Err(rejection) => {
            return error_response(BackendError::InvalidRequest(format!(
                "invalid JSON request: {}",
                rejection.body_text()
            )));
        }
    };

    let (model, stream) = match validate_request(&body) {
        Ok(request) => request,
        Err(error) => return error_response(error),
    };

    let request = BackendRequest { model, body };
    if stream {
        match state.backend.stream(request).await {
            Ok(events) => stream_response(events),
            Err(error) => error_response(error),
        }
    } else {
        match state.backend.complete(request).await {
            Ok(body) => axum::Json(body).into_response(),
            Err(error) => error_response(error),
        }
    }
}

fn validate_request(body: &Value) -> Result<(String, bool), BackendError> {
    let object = body.as_object().ok_or_else(|| {
        BackendError::InvalidRequest("request body must be a JSON object".to_string())
    })?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| BackendError::InvalidRequest("model is required".to_string()))?;
    let stream = match object.get("stream") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| BackendError::InvalidRequest("stream must be a boolean".to_string()))?,
    };
    Ok((model.to_string(), stream))
}

fn stream_response(events: BackendStream) -> Response {
    let body_stream = events.map(|event| {
        let bytes = match event {
            Ok(event) => encode_event(event),
            Err(error) => encode_event(json!({
                "type": "error",
                "error": {
                    "type": error.anthropic_type(),
                    "message": error.public_message(),
                }
            })),
        };
        Ok::<Bytes, Infallible>(bytes)
    });

    let mut response = Response::new(Body::from_stream(body_stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response
}

fn encode_event(event: Value) -> Bytes {
    let event_name = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    let data = match serde_json::to_string(&event) {
        Ok(data) => data,
        Err(_) => r#"{"type":"error","error":{"type":"api_error","message":"failed to encode stream event"}}"#.to_string(),
    };
    Bytes::from(format!("event: {event_name}\ndata: {data}\n\n"))
}

fn error_response(error: BackendError) -> Response {
    let status = StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = ErrorResponse {
        kind: "error",
        error: ErrorDetail {
            kind: error.anthropic_type().to_string(),
            message: error.public_message(),
        },
    };
    let mut response = axum::Json(body).into_response();
    *response.status_mut() = status;
    response
}
