//! Claude Code-facing Anthropic Messages HTTP surface.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Extension, Json, State, rejection::JsonRejection};
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::get, routing::post};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tracing::{info, warn};

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
            .layer(middleware::from_fn(request_id_middleware))
            .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
            .with_state(AppState {
                backend: self.backend,
            })
    }

    /// Serve the gateway with a caller-supplied shutdown signal.
    ///
    /// After the shutdown future resolves, in-flight requests are given 30 s
    /// to drain. If they have not finished by then, the server is force-dropped
    /// and a warning is logged.
    pub async fn serve_with_shutdown(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> io::Result<()> {
        use std::future::IntoFuture;

        let router = self.router();

        let serve_fut = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                shutdown.await;
                info!("shutdown signal received, draining in-flight requests");
            })
            .into_future();
        tokio::pin!(serve_fut);

        let deadline = tokio::time::sleep(std::time::Duration::from_secs(30));
        tokio::pin!(deadline);

        tokio::select! {
            result = &mut serve_fut => result,
            () = &mut deadline => {
                warn!("shutdown drain deadline exceeded, forcing server stop");
                Ok(())
            }
        }
    }

    /// Serve the gateway on an already-bound TCP listener (runs until the
    /// process is killed).
    pub async fn serve(self, listener: TcpListener) -> io::Result<()> {
        self.serve_with_shutdown(listener, std::future::pending())
            .await
    }

    /// Bind and serve on the supplied address.
    pub async fn bind_and_serve(self, address: SocketAddr) -> io::Result<()> {
        let listener = TcpListener::bind(address).await?;
        self.serve(listener).await
    }
}

fn is_valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
}

async fn request_id_middleware(mut request: Request<Body>, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| is_valid_request_id(v))
        .map(|v| v.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    request.extensions_mut().insert(request_id.clone());

    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
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
    request_id: String,
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
    Extension(request_id): Extension<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let start = Instant::now();
    let id: &str = &request_id;

    let body = match payload {
        Ok(Json(body)) => body,
        Err(rejection) => {
            let err = BackendError::InvalidRequest(format!(
                "invalid JSON request: {}",
                rejection.body_text()
            ));
            let resp = error_response(&err, id);
            info!(
                request_id = id,
                status = err.status_code(),
                "request rejected"
            );
            return resp;
        }
    };

    let (model, stream) = match validate_request(&body) {
        Ok(request) => request,
        Err(error) => {
            let resp = error_response(&error, id);
            info!(
                request_id = id,
                status = error.status_code(),
                "request rejected"
            );
            return resp;
        }
    };

    info!(request_id = id, model = %model, stream = stream, "request started");

    let request = BackendRequest { model, body };
    if stream {
        match state.backend.stream(request).await {
            Ok(events) => {
                info!(
                    request_id = id,
                    status = 200u16,
                    latency_ms = start.elapsed().as_millis() as u64,
                    "request completed"
                );
                stream_response(events)
            }
            Err(error) => {
                let status = error.status_code();
                let resp = error_response(&error, id);
                info!(
                    request_id = id,
                    status,
                    latency_ms = start.elapsed().as_millis() as u64,
                    "request completed"
                );
                resp
            }
        }
    } else {
        match state.backend.complete(request).await {
            Ok(body) => {
                info!(
                    request_id = id,
                    status = 200u16,
                    latency_ms = start.elapsed().as_millis() as u64,
                    "request completed"
                );
                axum::Json(body).into_response()
            }
            Err(error) => {
                let status = error.status_code();
                let resp = error_response(&error, id);
                info!(
                    request_id = id,
                    status,
                    latency_ms = start.elapsed().as_millis() as u64,
                    "request completed"
                );
                resp
            }
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

fn error_response(error: &BackendError, request_id: &str) -> Response {
    let status = StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = ErrorResponse {
        kind: "error",
        request_id: request_id.to_string(),
        error: ErrorDetail {
            kind: error.anthropic_type().to_string(),
            message: error.public_message(),
        },
    };
    let mut response = axum::Json(body).into_response();
    *response.status_mut() = status;
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}
