use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures_util::stream;
use serde_json::{Value, json};
use switchyard::gateway::{Backend, Gateway, ProviderBackend};
use switchyard::providers::adapter::ProviderStream;
use switchyard::providers::{
    ContentBlock, MessagesRequest, MessagesResponse, ProviderAdapter, ProviderError,
    ProviderRegistry, StopReason, StreamEvent, Usage,
};
use tower::ServiceExt;

#[derive(Clone)]
struct StubAdapter {
    id: String,
    requests: Arc<Mutex<Vec<MessagesRequest>>>,
    response: MessagesResponse,
    events: Vec<StreamEvent>,
}

impl StubAdapter {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            requests: Arc::new(Mutex::new(Vec::new())),
            response: MessagesResponse {
                id: "msg_stub".to_string(),
                kind: "message".to_string(),
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: "stub response".to_string(),
                    extra: Default::default(),
                }],
                model: "model-one".to_string(),
                stop_reason: Some(StopReason::EndTurn),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 2,
                    output_tokens: 3,
                    extra: Default::default(),
                },
                extra: Default::default(),
            },
            events: vec![
                StreamEvent::MessageStart {
                    id: "msg_stub".to_string(),
                    model: "model-one".to_string(),
                    usage: Usage {
                        input_tokens: 2,
                        output_tokens: 0,
                        extra: Default::default(),
                    },
                },
                StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: switchyard::providers::stream::Delta::TextDelta {
                        text: "stub stream".to_string(),
                    },
                },
                StreamEvent::MessageStop,
            ],
        }
    }
}

impl ProviderAdapter for StubAdapter {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn complete(
        &self,
        request: MessagesRequest,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<MessagesResponse, ProviderError>> + Send + '_>,
    > {
        let requests = Arc::clone(&self.requests);
        let response = self.response.clone();
        Box::pin(async move {
            requests.lock().expect("test mutex").push(request);
            Ok(response)
        })
    }

    fn stream(
        &self,
        request: MessagesRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ProviderStream, ProviderError>> + Send + '_>>
    {
        let requests = Arc::clone(&self.requests);
        let events = self.events.iter().cloned().map(Ok).collect::<Vec<_>>();
        Box::pin(async move {
            requests.lock().expect("test mutex").push(request);
            Ok(Box::pin(stream::iter(events)) as ProviderStream)
        })
    }
}

#[tokio::test]
async fn provider_backend_resolves_provider_model_and_rewrites_upstream_model() {
    let adapter = StubAdapter::new("alpha");
    let requests = Arc::clone(&adapter.requests);
    let mut registry = ProviderRegistry::new();
    registry
        .register_adapter(
            Arc::new(adapter),
            vec!["model-one".to_string()],
            Some("model-one".to_string()),
        )
        .expect("register adapter");

    let backend = ProviderBackend::new(Arc::new(registry), BTreeMap::new());
    assert_eq!(backend.models()[0].id, "alpha/model-one");
    let app = Gateway::new(backend).router();
    let body = json!({
        "model": "alpha/model-one",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    });

    let response = app.oneshot(json_request(&body)).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let response_body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("response json");
    assert_eq!(response_body["model"], "model-one");
    assert_eq!(requests.lock().expect("test mutex")[0].model, "model-one");
}

#[tokio::test]
async fn provider_backend_strips_context_suffix_before_upstream_forwarding() {
    let adapter = StubAdapter::new("alpha");
    let requests = Arc::clone(&adapter.requests);
    let mut registry = ProviderRegistry::new();
    registry
        .register_adapter(
            Arc::new(adapter),
            vec!["model-one[1m]".to_string()],
            Some("model-one[1m]".to_string()),
        )
        .expect("register adapter");

    let app = Gateway::new(ProviderBackend::new(Arc::new(registry), BTreeMap::new())).router();
    let body = json!({
        "model": "alpha/model-one",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    });

    let response = app.oneshot(json_request(&body)).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(requests.lock().expect("test mutex")[0].model, "model-one");
}

#[tokio::test]
async fn provider_backend_forwards_normalized_stream_events() {
    let adapter = StubAdapter::new("alpha");
    let mut registry = ProviderRegistry::new();
    registry
        .register_adapter(
            Arc::new(adapter),
            vec!["model-one".to_string()],
            Some("model-one".to_string()),
        )
        .expect("register adapter");

    let app = Gateway::new(ProviderBackend::new(Arc::new(registry), BTreeMap::new())).router();
    let body = json!({
        "model": "alpha/model-one",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    });

    let response = app.oneshot(json_request(&body)).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8 body");
    assert!(text.contains("event: message_start\n"));
    assert!(text.contains("event: content_block_delta\n"));
    assert!(text.contains("stub stream"));
    assert!(text.contains("event: message_stop\n"));
}

#[tokio::test]
async fn provider_backend_rejects_unknown_provider_model() {
    let mut registry = ProviderRegistry::new();
    registry
        .register_adapter(
            Arc::new(StubAdapter::new("alpha")),
            vec!["model-one".to_string()],
            Some("model-one".to_string()),
        )
        .expect("register adapter");

    let app = Gateway::new(ProviderBackend::new(Arc::new(registry), BTreeMap::new())).router();
    let body = json!({
        "model": "missing/model",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    });

    let response = app.oneshot(json_request(&body)).await.expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

fn json_request(body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).expect("request json")))
        .expect("request")
}

fn build_test_registry() -> (ProviderRegistry, Arc<Mutex<Vec<MessagesRequest>>>) {
    let adapter = StubAdapter::new("alpha");
    let requests = Arc::clone(&adapter.requests);
    let mut registry = ProviderRegistry::new();
    registry
        .register_adapter(
            Arc::new(adapter),
            vec!["model-one".to_string()],
            Some("model-one".to_string()),
        )
        .expect("register adapter");
    (registry, requests)
}

// ── F13: model aliases ─────────────────────────────────────────────

#[tokio::test]
async fn alias_resolves_to_target_route_for_complete() {
    let (registry, requests) = build_test_registry();
    let mut aliases = BTreeMap::new();
    aliases.insert("quick".to_string(), "alpha/model-one".to_string());
    let backend = ProviderBackend::new(Arc::new(registry), aliases);
    let app = Gateway::new(backend).router();

    let body = json!({
        "model": "quick",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    });
    let response = app.oneshot(json_request(&body)).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let response_body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("response json");
    assert_eq!(response_body["model"], "model-one");
    assert_eq!(requests.lock().expect("test mutex")[0].model, "model-one");
}

#[tokio::test]
async fn alias_resolves_to_target_route_for_stream() {
    let (registry, _requests) = build_test_registry();
    let mut aliases = BTreeMap::new();
    aliases.insert("quick".to_string(), "alpha/model-one".to_string());
    let backend = ProviderBackend::new(Arc::new(registry), aliases);
    let app = Gateway::new(backend).router();

    let body = json!({
        "model": "quick",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    });
    let response = app.oneshot(json_request(&body)).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8 body");
    assert!(text.contains("event: message_start\n"));
    assert!(text.contains("event: message_stop\n"));
}

#[tokio::test]
async fn models_endpoint_lists_aliases_alongside_real_routes() {
    let (registry, _requests) = build_test_registry();
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".to_string(), "alpha/model-one".to_string());
    aliases.insert("slow".to_string(), "alpha/model-one".to_string());
    let backend = ProviderBackend::new(Arc::new(registry), aliases);
    let app = Gateway::new(backend).router();

    let response = app
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
    let ids: Vec<&str> = body["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|m| m["id"].as_str().expect("id string"))
        .collect();
    assert!(ids.contains(&"alpha/model-one"));
    assert!(ids.contains(&"fast"));
    assert!(ids.contains(&"slow"));
}

#[tokio::test]
async fn unknown_model_still_returns_404_with_aliases_configured() {
    let (registry, _requests) = build_test_registry();
    let mut aliases = BTreeMap::new();
    aliases.insert("quick".to_string(), "alpha/model-one".to_string());
    let backend = ProviderBackend::new(Arc::new(registry), aliases);
    let app = Gateway::new(backend).router();

    let body = json!({
        "model": "nonexistent/model",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    });
    let response = app.oneshot(json_request(&body)).await.expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn alias_does_not_chain_resolve() {
    let (registry, _requests) = build_test_registry();
    let mut aliases = BTreeMap::new();
    // "a" → "b", "b" → "alpha/model-one"
    // Using "a" should resolve to "b" (not chain to "alpha/model-one"),
    // which then fails because "b" is not a valid provider/model.
    aliases.insert("a".to_string(), "b".to_string());
    aliases.insert("b".to_string(), "alpha/model-one".to_string());
    let backend = ProviderBackend::new(Arc::new(registry), aliases);
    let app = Gateway::new(backend).router();

    let body = json!({
        "model": "a",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    });
    let response = app.oneshot(json_request(&body)).await.expect("response");
    // "a" resolves to "b" (single substitution), then split_model_id fails
    // because "b" has no "/" — returns 404.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── F14: anthropic-beta header passthrough (end-to-end) ────────────

#[tokio::test]
async fn anthropic_beta_header_reaches_adapter() {
    let (registry, requests) = build_test_registry();
    let backend = ProviderBackend::new(Arc::new(registry), BTreeMap::new());
    let app = Gateway::new(backend).router();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-beta", "interleaved-thinking-2025-05-14")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "alpha/model-one",
                        "max_tokens": 64,
                        "messages": [{"role":"user","content":"hi"}]
                    }))
                    .expect("json"),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let reqs = requests.lock().expect("test mutex");
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].forward_headers,
        vec![(
            "anthropic-beta".to_string(),
            "interleaved-thinking-2025-05-14".to_string()
        )]
    );
}
