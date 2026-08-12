use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures_util::stream;
use serde_json::{Value, json};
use switchyard::gateway::{
    Backend, BackendError, BackendFuture, BackendRequest, BackendStream, Gateway, ModelInfo,
};
use tower::ServiceExt;

#[derive(Clone, Default)]
struct MockBackend {
    calls: Arc<Mutex<Vec<BackendRequest>>>,
    completion: Option<Result<Value, BackendError>>,
    events: Vec<Result<Value, BackendError>>,
}

impl MockBackend {
    fn with_completion(completion: Result<Value, BackendError>) -> Self {
        Self {
            completion: Some(completion),
            ..Self::default()
        }
    }

    fn with_events(events: Vec<Result<Value, BackendError>>) -> Self {
        Self {
            events,
            ..Self::default()
        }
    }
}

impl Backend for MockBackend {
    fn models(&self) -> Vec<ModelInfo> {
        vec![
            ModelInfo::new("kimi/kimi-k3[1m]"),
            ModelInfo::new("qwen/qwen3.8-max"),
        ]
    }

    fn complete(&self, request: BackendRequest) -> BackendFuture<Value> {
        let calls = Arc::clone(&self.calls);
        let completion = self
            .completion
            .clone()
            .unwrap_or_else(|| Ok(json!({"id":"msg_test","type":"message","role":"assistant","content":[],"model":"kimi-k3[1m]","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":2}})));
        Box::pin(async move {
            calls.lock().expect("test mutex").push(request);
            completion
        })
    }

    fn stream(&self, request: BackendRequest) -> BackendFuture<BackendStream> {
        let calls = Arc::clone(&self.calls);
        let events = self.events.clone();
        Box::pin(async move {
            calls.lock().expect("test mutex").push(request);
            Ok(Box::pin(stream::iter(events)) as BackendStream)
        })
    }
}

#[tokio::test]
async fn health_and_models_are_exposed_without_upstream_calls() {
    let backend = MockBackend::default();
    let calls = Arc::clone(&backend.calls);
    let app = Gateway::new(backend).router();

    let response = app
        .clone()
        .oneshot(
            Request::get("/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
        r#"{"status":"ok"}"#
    );

    let response = app
        .clone()
        .oneshot(
            Request::get("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("models json");
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"][0]["id"], "kimi/kimi-k3[1m]");
    assert!(calls.lock().expect("test mutex").is_empty());
}

#[tokio::test]
async fn non_streaming_messages_are_forwarded_and_returned_as_json() {
    let backend = MockBackend::default();
    let calls = Arc::clone(&backend.calls);
    let app = Gateway::new(backend).router();
    let request_body = json!({
        "model": "kimi/kimi-k3[1m]",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    });

    let response = app
        .oneshot(json_request("/v1/messages", &request_body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("completion json");
    assert_eq!(body["type"], "message");
    let calls = calls.lock().expect("test mutex");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model, "kimi/kimi-k3[1m]");
    assert_eq!(calls[0].body, request_body);
}

#[tokio::test]
async fn streaming_messages_are_encoded_as_anthropic_sse() {
    let backend = MockBackend::with_events(vec![
        Ok(
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"qwen3.8-max","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":2,"output_tokens":0}}}),
        ),
        Ok(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
        ),
        Ok(
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}),
        ),
        Ok(json!({"type":"message_stop"})),
    ]);
    let app = Gateway::new(backend).router();
    let request_body = json!({
        "model": "qwen/qwen3.8-max",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    });

    let response = app
        .oneshot(json_request("/v1/messages", &request_body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8 body");
    assert!(body.contains("event: message_start\n"));
    assert!(body.contains("event: content_block_delta\n"));
    assert!(body.contains(r#""text":"hello""#));
    assert!(body.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
}

#[tokio::test]
async fn invalid_messages_return_anthropic_error_without_calling_backend() {
    let backend = MockBackend::default();
    let calls = Arc::clone(&backend.calls);
    let app = Gateway::new(backend).router();
    let request_body = json!({"max_tokens": 64, "messages": []});

    let response = app
        .oneshot(json_request("/v1/messages", &request_body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("error json");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(calls.lock().expect("test mutex").is_empty());
}

#[tokio::test]
async fn backend_errors_preserve_http_status_and_hide_credentials() {
    let backend = MockBackend::with_completion(Err(BackendError::upstream(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limit_error",
        "provider is busy; token=sk-secret-value",
    )));
    let app = Gateway::new(backend).router();
    let request_body = json!({
        "model": "kimi/kimi-k3[1m]",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    });

    let response = app
        .oneshot(json_request("/v1/messages", &request_body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8 body");
    assert!(body.contains("rate_limit_error"));
    assert!(!body.contains("sk-secret-value"));
}

#[tokio::test]
async fn malformed_json_returns_anthropic_invalid_request_error() {
    let app = Gateway::new(MockBackend::default()).router();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from("not-json"))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("error json");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn streaming_backend_errors_become_anthropic_error_events() {
    let backend = MockBackend::with_events(vec![Err(BackendError::upstream(
        529u16,
        "overloaded_error",
        "provider overloaded; api_key=sk-secret-value",
    ))]);
    let app = Gateway::new(backend).router();
    let request_body = json!({
        "model": "qwen/qwen3.8-max",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    });

    let response = app
        .oneshot(json_request("/v1/messages", &request_body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8 body");
    assert!(body.contains("event: error\n"));
    assert!(body.contains("overloaded_error"));
    assert!(!body.contains("sk-secret-value"));
}

fn json_request(path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).expect("json body")))
        .expect("request")
}
