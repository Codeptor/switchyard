use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use switchyard::gateway::{
    Backend, BackendError, BackendFuture, BackendRequest, BackendStream, Gateway, ModelInfo,
};
use tokio::sync::broadcast;
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

    fn complete(&self, request: BackendRequest) -> BackendFuture<'_, Value> {
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

    fn stream(&self, request: BackendRequest) -> BackendFuture<'_, BackendStream> {
        let calls = Arc::clone(&self.calls);
        let events = self.events.clone();
        Box::pin(async move {
            calls.lock().expect("test mutex").push(request);
            Ok(Box::pin(stream::iter(events)) as BackendStream)
        })
    }
}

// ── existing tests ──────────────────────────────────────────────────

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
async fn invalid_stream_type_returns_bad_request_without_calling_backend() {
    let backend = MockBackend::default();
    let calls = Arc::clone(&backend.calls);
    let app = Gateway::new(backend).router();
    let request_body = json!({
        "model": "kimi/kimi-k3[1m]",
        "max_tokens": 64,
        "stream": "true",
        "messages": [{"role":"user","content":"hello"}]
    });

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

// ── F2: body limit ──────────────────────────────────────────────────

#[tokio::test]
async fn large_request_body_within_64mb_limit_is_accepted() {
    let backend = MockBackend::default();
    let calls = Arc::clone(&backend.calls);
    let app = Gateway::new(backend).router();

    // ~3 MB string — well above the old 2 MB default, below the new 64 MB limit.
    let large_string = "A".repeat(3 * 1024 * 1024);
    let request_body = json!({
        "model": "kimi/kimi-k3[1m]",
        "messages": [{"role":"user","content": large_string}]
    });

    let response = app
        .oneshot(json_request("/v1/messages", &request_body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let calls = calls.lock().expect("test mutex");
    assert_eq!(calls.len(), 1);
}

// ── F3: graceful shutdown ───────────────────────────────────────────

#[derive(Clone)]
struct DelayedStreamBackend {
    calls: Arc<Mutex<Vec<BackendRequest>>>,
    started: Arc<tokio::sync::Notify>,
}

impl Backend for DelayedStreamBackend {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo::new("test/model")]
    }

    fn complete(&self, _request: BackendRequest) -> BackendFuture<'_, Value> {
        Box::pin(async { Ok(Value::Null) })
    }

    fn stream(&self, request: BackendRequest) -> BackendFuture<'_, BackendStream> {
        let calls = Arc::clone(&self.calls);
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            calls.lock().expect("test mutex").push(request);
            started.notify_one();
            let chunks = stream::iter(0..5usize).then(|i| async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<_, BackendError>(json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": format!("chunk{i}")}
                }))
            });
            let stop = stream::once(async { Ok(json!({"type":"message_stop"})) });
            Ok(Box::pin(chunks.chain(stop)) as BackendStream)
        })
    }
}

#[tokio::test]
async fn graceful_shutdown_drains_inflight_streaming_request() {
    let started = Arc::new(tokio::sync::Notify::new());
    let backend = DelayedStreamBackend {
        calls: Arc::new(Mutex::new(Vec::new())),
        started: Arc::clone(&started),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");

    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);
    let shutdown = async move {
        let mut rx = shutdown_rx;
        let _ = rx.recv().await;
    };

    let server_handle = tokio::spawn(async move {
        Gateway::new(backend)
            .serve_with_shutdown(listener, shutdown)
            .await
            .expect("serve");
    });

    // Wait for the server to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send a streaming request in the background.
    let request_body = json!({
        "model": "test/model",
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    });
    let response_handle = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{addr}/v1/messages"))
            .json(&request_body)
            .send()
            .await
            .expect("request sent");
        assert_eq!(response.status(), 200);
        response.text().await.expect("body")
    });

    // Wait until the backend starts processing (request is in-flight).
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("backend started processing");

    // Fire shutdown while the stream is still active.
    let _ = shutdown_tx.send(());

    // The in-flight response must complete fully.
    let body = tokio::time::timeout(Duration::from_secs(10), response_handle)
        .await
        .expect("response not stuck")
        .expect("join");
    assert!(body.contains("message_stop"), "full stream drained");

    // Server must stop after draining.
    tokio::time::timeout(Duration::from_secs(5), server_handle)
        .await
        .expect("server stopped")
        .expect("join");
}

// ── F7: request IDs ─────────────────────────────────────────────────

#[tokio::test]
async fn request_id_is_generated_when_absent() {
    let app = Gateway::new(MockBackend::default()).router();
    let response = app
        .oneshot(json_request(
            "/v1/messages",
            &json!({"model":"kimi/kimi-k3[1m]","messages":[{"role":"user","content":"hi"}]}),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let id = response
        .headers()
        .get("x-request-id")
        .expect("x-request-id header");
    assert!(!id.to_str().expect("ascii").is_empty());
}

#[tokio::test]
async fn request_id_is_echoed_when_provided() {
    let app = Gateway::new(MockBackend::default()).router();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-request-id", "my-custom-id-123")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "kimi/kimi-k3[1m]",
                        "messages": [{"role":"user","content":"hi"}]
                    }))
                    .expect("json"),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-request-id"], "my-custom-id-123");
}

#[tokio::test]
async fn error_responses_carry_request_id_in_body_and_header() {
    let app = Gateway::new(MockBackend::default()).router();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-request-id", "err-req-42")
                .body(Body::from(
                    serde_json::to_vec(&json!({"max_tokens": 64, "messages": []})).expect("json"),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()["x-request-id"], "err-req-42");
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("error json");
    assert_eq!(body["request_id"], "err-req-42");
    assert_eq!(body["type"], "error");
}

#[tokio::test]
async fn sse_responses_carry_request_id_header() {
    let backend = MockBackend::with_events(vec![Ok(json!({"type":"message_stop"}))]);
    let app = Gateway::new(backend).router();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-request-id", "stream-req-99")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "qwen/qwen3.8-max",
                        "stream": true,
                        "messages": [{"role":"user","content":"hi"}]
                    }))
                    .expect("json"),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["x-request-id"], "stream-req-99");
}

#[tokio::test]
async fn invalid_request_id_is_replaced_with_generated_one() {
    let app = Gateway::new(MockBackend::default()).router();
    // A 200-char valid ASCII string — exceeds the 128-char max, so rejected.
    let long_id = "a".repeat(200);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-request-id", &long_id)
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "kimi/kimi-k3[1m]",
                        "messages": [{"role":"user","content":"hi"}]
                    }))
                    .expect("json"),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let id = response.headers()["x-request-id"]
        .to_str()
        .expect("ascii")
        .to_string();
    // Should be a UUID, not the overly-long value.
    assert_ne!(id, long_id);
    assert!(id.contains('-'), "looks like a uuid: {id}");
}

// ── F14: anthropic-beta header passthrough ─────────────────────────

#[tokio::test]
async fn anthropic_beta_header_is_captured_in_forward_headers() {
    let backend = MockBackend::default();
    let calls = Arc::clone(&backend.calls);
    let app = Gateway::new(backend).router();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-beta", "prompt-caching-2024-07-31")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "kimi/kimi-k3[1m]",
                        "messages": [{"role":"user","content":"hi"}]
                    }))
                    .expect("json"),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let calls = calls.lock().expect("test mutex");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].forward_headers,
        vec![(
            "anthropic-beta".to_string(),
            "prompt-caching-2024-07-31".to_string()
        )]
    );
}

#[tokio::test]
async fn non_allowlisted_headers_are_not_forwarded() {
    let backend = MockBackend::default();
    let calls = Arc::clone(&backend.calls);
    let app = Gateway::new(backend).router();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-custom-header", "should-not-forward")
                .header("authorization", "Bearer should-not-forward")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "kimi/kimi-k3[1m]",
                        "messages": [{"role":"user","content":"hi"}]
                    }))
                    .expect("json"),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let calls = calls.lock().expect("test mutex");
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].forward_headers.is_empty(),
        "non-allowlisted headers must not be forwarded"
    );
}
