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
                }],
                model: "model-one".to_string(),
                stop_reason: Some(StopReason::EndTurn),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 2,
                    output_tokens: 3,
                },
            },
            events: vec![
                StreamEvent::MessageStart {
                    id: "msg_stub".to_string(),
                    model: "model-one".to_string(),
                    usage: Usage {
                        input_tokens: 2,
                        output_tokens: 0,
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

    let backend = ProviderBackend::new(Arc::new(registry));
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

    let app = Gateway::new(ProviderBackend::new(Arc::new(registry))).router();
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

    let app = Gateway::new(ProviderBackend::new(Arc::new(registry))).router();
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
