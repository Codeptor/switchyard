//! Mock-based integration tests for the generic Anthropic upstream adapter.
//!
//! Provider-agnostic: no Kimi/Muse/Qwen hardcoding. Tests use a local Axum
//! mock that speaks the Anthropic `/v1/messages` wire protocol.
//! Covers malformed config, missing credentials, streaming chunks, tool calls,
//! upstream non-2xx normalization, timeouts, usage/stop-reason, and redaction.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::StreamExt;
use tokio::net::TcpListener;

use switchyard::providers::{
    AnthropicAdapter, AuthConfig, ModelCapabilities, ModelConfig, ProviderAdapter, ProviderConfig,
    ProviderError, ProviderRegistry, RetryConfig,
    types::{ContentBlock, Message, MessageContent, MessagesRequest, StopReason},
};

// ---------------------------------------------------------------------------
// Mock helpers
// ---------------------------------------------------------------------------

struct MockServer {
    addr: SocketAddr,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

async fn spawn_mock<F, R>(handler: F) -> MockServer
where
    F: Fn(HeaderMap, String) -> R + Clone + Send + Sync + 'static,
    R: IntoResponse + Send + 'static,
{
    let app = Router::new().route(
        "/v1/messages",
        post(move |headers: HeaderMap, body: String| {
            let h = handler.clone();
            async move { h(headers, body).into_response() }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    // Give the OS a moment to start listening.
    tokio::time::sleep(Duration::from_millis(20)).await;
    MockServer {
        addr,
        _handle: handle,
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn sample_request(model: &str, stream: bool) -> MessagesRequest {
    MessagesRequest {
        model: model.to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: MessageContent::Text("hello".to_string()),
            extra: Default::default(),
        }],
        max_tokens: 256,
        system: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stream: Some(stream),
        stop_sequences: None,
        tools: None,
        tool_choice: None,
        metadata: None,
        extra: Default::default(),
        forward_headers: vec![],
    }
}

fn sample_tool_request(model: &str) -> MessagesRequest {
    let mut req = sample_request(model, false);
    req.tools = Some(vec![switchyard::providers::types::Tool {
        name: "lookup".to_string(),
        description: Some("lookup data".to_string()),
        input_schema: serde_json::json!({"type":"object","properties":{"query":{"type":"string"}}}),
        extra: Default::default(),
    }]);
    req
}

// ---------------------------------------------------------------------------
// Config validation (malformed)
// ---------------------------------------------------------------------------

#[test]
fn malformed_provider_id_rejected() {
    let cfg = ProviderConfig {
        id: "bad id with spaces".to_string(),
        base_url: "https://example.test".parse().expect("url"),
        auth: AuthConfig::None,
        models: vec![],
        connect_timeout_ms: None,
        read_timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
        retry: None,
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn malformed_base_url_rejected() {
    let cfg = ProviderConfig {
        id: "prov-1".to_string(),
        base_url: "ftp://example.test"
            .parse()
            .expect("url parses but invalid scheme"),
        auth: AuthConfig::None,
        models: vec![],
        connect_timeout_ms: None,
        read_timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
        retry: None,
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn duplicate_model_id_rejected() {
    let cfg = ProviderConfig {
        id: "prov-1".to_string(),
        base_url: "https://example.test".parse().expect("url"),
        auth: AuthConfig::None,
        models: vec![
            ModelConfig {
                id: "m1".to_string(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
                capabilities: ModelCapabilities::default(),
            },
            ModelConfig {
                id: "m1".to_string(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
                capabilities: ModelCapabilities::default(),
            },
        ],
        connect_timeout_ms: None,
        read_timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
        retry: None,
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn registry_rejects_duplicate_provider() {
    let mut reg = ProviderRegistry::new();
    let cfg1 = ProviderConfig {
        id: "dup".to_string(),
        base_url: "https://example.test".parse().expect("url"),
        auth: AuthConfig::None,
        models: vec![],
        connect_timeout_ms: None,
        read_timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
        retry: None,
    };
    let cfg2 = cfg1.clone();
    reg.register_anthropic(cfg1).expect("first");
    let err = reg
        .register_anthropic(cfg2)
        .expect_err("duplicate should fail");
    assert!(err.to_string().contains("duplicate"));
}

// ---------------------------------------------------------------------------
// Missing credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_credential_returns_auth_error() {
    let server = spawn_mock(|_, _| {
        (
            StatusCode::OK,
            serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type":"text","text":"hi"}],
                "model": "m1",
                "stop_reason": "end_turn",
                "usage": {"input_tokens":1,"output_tokens":1}
            })
            .to_string(),
        )
    })
    .await;

    let cfg = ProviderConfig {
        id: "prov-auth".to_string(),
        base_url: server.base_url().parse().expect("url"),
        auth: AuthConfig::Header {
            header: "x-api-key".to_string(),
            env_var: "SWITCHYARD_TEST_MISSING_TOKEN_XYZ_UNIQUE_2".to_string(),
            prefix: None,
        },
        models: vec![ModelConfig {
            id: "m1".to_string(),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        }],
        connect_timeout_ms: None,
        read_timeout_ms: Some(2000),
        default_model: Some("m1".to_string()),
        extra_headers: vec![],
        retry: None,
    };

    // Ensure env var is not set.
    // SAFETY: tests run in same process but with unique var name.
    unsafe { std::env::remove_var("SWITCHYARD_TEST_MISSING_TOKEN_XYZ_UNIQUE_2") };

    let adapter = AnthropicAdapter::from_config(&cfg).expect("adapter");
    let req = sample_request("m1", false);
    let res = adapter.complete(req).await;
    assert!(res.is_err());
    let err = res.expect_err("should err");
    assert!(matches!(err, ProviderError::AuthMissing { .. }));
    // Redaction: display must not contain secret.
    let msg = err.to_string();
    assert!(!msg.contains("sk-"));
}

#[tokio::test]
async fn credential_with_prefix_builds_bearer_header() {
    // Use mock that echoes Authorization header.
    let server = spawn_mock(|headers: HeaderMap, _body| {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Check that the mock received Bearer prefix.
        if auth.starts_with("Bearer test-secret-") {
            (
                StatusCode::OK,
                serde_json::json!({
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type":"text","text":"ok"}],
                    "model": "m1",
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens":1,"output_tokens":1}
                })
                .to_string(),
            )
        } else {
            (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({"type":"error","error":{"type":"authentication_error","message":"bad auth"}})
                    .to_string(),
            )
        }
    })
    .await;

    // SAFETY: set env for this test.
    unsafe { std::env::set_var("SWITCHYARD_TEST_BEARER_TOKEN_3", "test-secret-abc123") };
    let cfg = ProviderConfig {
        id: "prov-bearer".to_string(),
        base_url: server.base_url().parse().expect("url"),
        auth: AuthConfig::Header {
            header: "Authorization".to_string(),
            env_var: "SWITCHYARD_TEST_BEARER_TOKEN_3".to_string(),
            prefix: Some("Bearer ".to_string()),
        },
        models: vec![ModelConfig {
            id: "m1".to_string(),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        }],
        connect_timeout_ms: None,
        read_timeout_ms: Some(2000),
        default_model: None,
        extra_headers: vec![],
        retry: None,
    };
    let adapter = AnthropicAdapter::from_config(&cfg).expect("adapter");
    let req = sample_request("m1", false);
    let res = adapter.complete(req).await;
    assert!(res.is_ok(), "bearer auth should succeed: {res:?}");
    unsafe { std::env::remove_var("SWITCHYARD_TEST_BEARER_TOKEN_3") };
}

// ---------------------------------------------------------------------------
// Non-2xx normalization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upstream_401_maps_to_authentication_error() {
    let server = spawn_mock(|_, _| {
        (
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"type":"error","error":{"type":"authentication_error","message":"invalid key"}}).to_string(),
        )
    })
    .await;
    let adapter = AnthropicAdapter::new(
        "prov-err",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(2000),
    )
    .expect("adapter");
    let req = sample_request("m1", false);
    let err = adapter.complete(req).await.expect_err("should err");
    match err {
        ProviderError::Upstream { status, code, .. } => {
            assert_eq!(status, 401);
            assert_eq!(code, "authentication_error");
        }
        other => panic!("unexpected error type: {other:?}"),
    }
}

#[tokio::test]
async fn upstream_429_maps_to_rate_limit() {
    let server = spawn_mock(|_, _| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            serde_json::json!({"type":"error","error":{"type":"rate_limit_error","message":"too many"}}).to_string(),
        )
    })
    .await;
    let adapter = AnthropicAdapter::new(
        "prov-429",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(2000),
    )
    .expect("adapter");
    let err = adapter
        .complete(sample_request("m1", false))
        .await
        .expect_err("should err");
    match err {
        ProviderError::Upstream { status, code, .. } => {
            assert_eq!(status, 429);
            assert_eq!(code, "rate_limit_error");
        }
        other => panic!("wrong error: {other:?}"),
    }
}

#[tokio::test]
async fn streaming_upstream_error_normalized() {
    let server = spawn_mock(|_, _| {
        (
            StatusCode::BAD_REQUEST,
            serde_json::json!({"type":"error","error":{"type":"invalid_request_error","message":"bad messages"}}).to_string(),
        )
    })
    .await;
    let adapter = AnthropicAdapter::new(
        "prov-stream-err",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(2000),
    )
    .expect("adapter");
    let err = match adapter.stream(sample_request("m1", true)).await {
        Ok(_) => panic!("stream should err on non-2xx"),
        Err(e) => e,
    };
    assert!(matches!(err, ProviderError::Upstream { .. }));
}

// ---------------------------------------------------------------------------
// Streaming + tool-use + usage + stop reason
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_tool_use_and_usage_normalized() {
    // SSE payload with text delta, then tool_use start/delta/stop, then message_delta with tool_use stop reason.
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"mock-model\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"lookup\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\":\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"hi\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":12}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    let body_clone = sse_body.to_string();
    let server = spawn_mock(move |_, _| {
        let body = body_clone.clone();
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(body))
            .expect("response")
    })
    .await;

    let adapter = AnthropicAdapter::new(
        "prov-stream",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(5000),
    )
    .expect("adapter");

    let mut stream = adapter
        .stream(sample_request("mock-model", true))
        .await
        .expect("stream ok");

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("stream event ok"));
    }

    // Verify normalized events:
    // - MessageStart with id/model/usage
    // - ContentBlockDelta text
    // - InputJson deltas
    // - MessageDelta with ToolUse stop reason and usage
    assert!(events.iter().any(|e| matches!(
        e,
        switchyard::providers::StreamEvent::MessageStart { id, .. } if id == "msg_1"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        switchyard::providers::StreamEvent::ContentBlockDelta { delta: switchyard::providers::stream::Delta::TextDelta { text }, .. } if text == "hello "
    )));
    // Tool input deltas reassembled.
    let tool_deltas: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            switchyard::providers::StreamEvent::ContentBlockDelta {
                delta: switchyard::providers::stream::Delta::InputJsonDelta { partial_json },
                ..
            } => Some(partial_json.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_deltas.len(), 2);
    let combined = tool_deltas.concat();
    let parsed: serde_json::Value = serde_json::from_str(&combined).expect("tool input valid json");
    assert_eq!(parsed["query"], "hi");

    let has_tool_stop = events.iter().any(|e| {
        matches!(
            e,
            switchyard::providers::StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::ToolUse),
                ..
            }
        )
    });
    assert!(has_tool_stop, "expected ToolUse stop reason");

    // Usage from message_delta.
    let usage_ok = events.iter().any(|e| match e {
        switchyard::providers::StreamEvent::MessageDelta { usage, .. } => {
            usage.output_tokens == 12 && usage.input_tokens == 10
        }
        _ => false,
    });
    assert!(usage_ok);

    assert!(
        events
            .iter()
            .any(|e| matches!(e, switchyard::providers::StreamEvent::MessageStop))
    );
}

// ---------------------------------------------------------------------------
// Non-streaming tool-use and usage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_with_tool_use_normalized() {
    let server = spawn_mock(|_, _| {
        (
            StatusCode::OK,
            serde_json::json!({
                "id": "msg_123",
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type":"text","text":"calling tool"},
                    {"type":"tool_use","id":"toolu_99","name":"lookup","input":{"query":"hi"}}
                ],
                "model": "mock-model",
                "stop_reason": "tool_use",
                "usage": {"input_tokens":5,"output_tokens":10}
            })
            .to_string(),
        )
    })
    .await;

    let adapter = AnthropicAdapter::new(
        "prov-complete-tool",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(2000),
    )
    .expect("adapter");

    let resp = adapter
        .complete(sample_tool_request("mock-model"))
        .await
        .expect("complete ok");
    assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(resp.usage.input_tokens, 5);
    assert_eq!(resp.usage.output_tokens, 10);
    assert!(
        resp.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "lookup"))
    );
}

#[tokio::test]
async fn complete_preserves_thinking_fields_for_anthropic_compatible_providers() {
    let server = spawn_mock(|_, body: String| {
        let body: serde_json::Value = serde_json::from_str(&body).expect("request json");
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "enabled", "budget_tokens": 4096})
        );
        assert_eq!(body["reasoning_effort"], "high");
        (
            StatusCode::OK,
            serde_json::json!({
                "id": "msg-thinking",
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type":"thinking","thinking":"inspect the tool result","signature":"sig"},
                    {"type":"text","text":"done"}
                ],
                "model": "qwen3.8-max",
                "stop_reason": "end_turn",
                "usage": {"input_tokens":5,"output_tokens":10,"cache_read_input_tokens":2}
            })
            .to_string(),
        )
    })
    .await;

    let adapter = AnthropicAdapter::new(
        "prov-thinking",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(2000),
    )
    .expect("adapter");
    let mut request = sample_request("qwen3.8-max", false);
    request.extra.insert(
        "thinking".to_string(),
        serde_json::json!({"type": "enabled", "budget_tokens": 4096}),
    );
    request
        .extra
        .insert("reasoning_effort".to_string(), serde_json::json!("high"));

    let response = adapter.complete(request).await.expect("complete");
    assert!(response.content.iter().any(|block| matches!(
        block,
        ContentBlock::Thinking { thinking, signature: Some(signature), .. }
            if thinking == "inspect the tool result" && signature == "sig"
    )));
    assert_eq!(response.usage.extra["cache_read_input_tokens"], 2);
}

// ---------------------------------------------------------------------------
// Timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_produces_timeout_error() {
    let server = spawn_mock(|_, _| {
        // This handler is not reached before timeout; but even if it sleeps,
        // reqwest timeout will fire.
        std::thread::sleep(Duration::from_millis(500));
        (
            StatusCode::OK,
            serde_json::json!({
                "id":"msg_1","type":"message","role":"assistant",
                "content":[{"type":"text","text":"late"}],
                "model":"m1","stop_reason":"end_turn",
                "usage":{"input_tokens":1,"output_tokens":1}
            })
            .to_string(),
        )
    })
    .await;

    // Very short timeout.
    let adapter = AnthropicAdapter::new(
        "prov-timeout",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(80),
    )
    .expect("adapter");

    let err = adapter
        .complete(sample_request("m1", false))
        .await
        .expect_err("should timeout");
    assert!(matches!(err, ProviderError::Timeout { .. }));
    let msg = err.to_string();
    assert!(!msg.contains("sk-"));
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

#[test]
fn redact_headers_removes_secrets() {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer secret123".to_string());
    headers.insert("x-api-key".to_string(), "sk-xyz".to_string());
    headers.insert("content-type".to_string(), "application/json".to_string());
    let redacted = switchyard::providers::credentials::redact_headers(&headers);
    assert_eq!(
        redacted.get("Authorization").map(|s| s.as_str()),
        Some("[REDACTED]")
    );
    assert_eq!(
        redacted.get("x-api-key").map(|s| s.as_str()),
        Some("[REDACTED]")
    );
    assert_eq!(
        redacted.get("content-type").map(|s| s.as_str()),
        Some("application/json")
    );
}

// ---------------------------------------------------------------------------
// Provider-agnostic: registry works with manual model ids, no discovery
// ---------------------------------------------------------------------------

#[test]
fn no_discovery_needed_any_model_id_accepted_when_empty() {
    let mut reg = ProviderRegistry::new();
    let cfg = ProviderConfig {
        id: "generic-prov".to_string(),
        base_url: "https://example.test".parse().expect("url"),
        auth: AuthConfig::None,
        models: vec![], // no models pre-configured
        connect_timeout_ms: None,
        read_timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
        retry: None,
    };
    reg.register_anthropic(cfg).expect("register");
    // Any model id should resolve.
    let h = reg
        .resolve("generic-prov", Some("arbitrary-model-xyz"))
        .expect("resolve any");
    assert_eq!(h.model_id, "arbitrary-model-xyz");
}

// ---------------------------------------------------------------------------
// F1: connect + read timeout tests
// ---------------------------------------------------------------------------

#[test]
fn config_rejects_zero_connect_timeout() {
    let cfg = ProviderConfig {
        id: "p1".to_string(),
        base_url: "https://example.test".parse().expect("url"),
        auth: AuthConfig::None,
        models: vec![],
        connect_timeout_ms: Some(0),
        read_timeout_ms: Some(5000),
        default_model: None,
        extra_headers: vec![],
        retry: None,
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn config_rejects_zero_read_timeout() {
    let cfg = ProviderConfig {
        id: "p1".to_string(),
        base_url: "https://example.test".parse().expect("url"),
        auth: AuthConfig::None,
        models: vec![],
        connect_timeout_ms: Some(5000),
        read_timeout_ms: Some(0),
        default_model: None,
        extra_headers: vec![],
        retry: None,
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn config_accepts_valid_connect_and_read_timeouts() {
    let cfg = ProviderConfig {
        id: "p1".to_string(),
        base_url: "https://example.test".parse().expect("url"),
        auth: AuthConfig::None,
        models: vec![],
        connect_timeout_ms: Some(5000),
        read_timeout_ms: Some(120000),
        default_model: None,
        extra_headers: vec![],
        retry: None,
    };
    assert!(cfg.validate().is_ok());
}

#[tokio::test]
async fn stream_completes_when_chunk_gaps_below_read_timeout() {
    // The stream takes ~600ms total (3 chunks, 100ms gaps) but each individual
    // gap is well under the 400ms read_timeout, so it should succeed.
    use axum::body::Body;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let chunks: Vec<(Duration, String)> = vec![
        (
            Duration::from_millis(0),
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n"
                .to_string(),
        ),
        (
            Duration::from_millis(100),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n"
                .to_string(),
        ),
        (
            Duration::from_millis(100),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
        ),
    ];

    let chunks = Arc::new(Mutex::new(chunks));

    let app = axum::Router::new().route(
        "/v1/messages",
        post(move |_: axum::http::HeaderMap, _: String| {
            let chunks = chunks.clone();
            async move {
                let stream = futures_util::stream::unfold(chunks, |chunks| async move {
                    let mut guard = chunks.lock().await;
                    if guard.is_empty() {
                        return None;
                    }
                    let (delay, data) = guard.remove(0);
                    drop(guard);
                    tokio::time::sleep(delay).await;
                    Some((Ok::<_, std::convert::Infallible>(data), chunks))
                });
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .expect("response")
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // read_timeout = 400ms, gaps = 100ms each (well under), total ~200ms
    let adapter = AnthropicAdapter::with_timeouts(
        "prov-read-timeout-ok",
        format!("http://{addr}"),
        AuthConfig::None,
        Duration::from_secs(5),
        Duration::from_millis(400),
    )
    .expect("adapter");

    let mut stream = adapter
        .stream(sample_request("m1", true))
        .await
        .expect("stream should start");

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event should be ok"));
    }

    assert!(
        events
            .iter()
            .any(|e| matches!(e, switchyard::providers::StreamEvent::MessageStop))
    );

    handle.abort();
}

#[tokio::test]
async fn stream_stall_exceeds_read_timeout_produces_timeout_error() {
    use axum::body::Body;

    let app = axum::Router::new().route(
        "/v1/messages",
        post(|_: axum::http::HeaderMap, _: String| async {
            // Send one chunk, then stall for 600ms (exceeds 200ms read timeout).
            let stream = futures_util::stream::unfold(0u32, |state| async move {
                if state == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Some((
                        Ok::<_, std::convert::Infallible>(
                            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n"
                                .to_string(),
                        ),
                        state + 1,
                    ))
                } else if state == 1 {
                    // Stall longer than read_timeout.
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    Some((
                        Ok("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string()),
                        state + 1,
                    ))
                } else {
                    None
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .expect("response")
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let adapter = AnthropicAdapter::with_timeouts(
        "prov-read-timeout-stall",
        format!("http://{addr}"),
        AuthConfig::None,
        Duration::from_secs(5),
        Duration::from_millis(200),
    )
    .expect("adapter");

    let mut stream = adapter
        .stream(sample_request("m1", true))
        .await
        .expect("stream should start");

    let mut got_timeout = false;
    while let Some(ev) = stream.next().await {
        if let Err(ProviderError::Timeout { .. }) = ev {
            got_timeout = true;
            break;
        }
    }

    assert!(got_timeout, "expected timeout error from read stall");
    handle.abort();
}

// ---------------------------------------------------------------------------
// F4: retry tests
// ---------------------------------------------------------------------------

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

async fn spawn_counting_mock<F, R>(counter: Arc<AtomicU32>, handler: F) -> MockServer
where
    F: Fn(HeaderMap, String) -> R + Clone + Send + Sync + 'static,
    R: IntoResponse + Send + 'static,
{
    let app = Router::new().route(
        "/v1/messages",
        post(move |headers: HeaderMap, body: String| {
            let h = handler.clone();
            let c = counter.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                h(headers, body).into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    let _handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    MockServer { addr, _handle }
}

#[tokio::test]
async fn retry_429_then_200_succeeds_with_retry_after() {
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();

    let app = Router::new().route(
        "/v1/messages",
        post(move |_headers: HeaderMap, _body: String| {
            let c = c.clone();
            async move {
                let count = c.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    Response::builder()
                        .status(StatusCode::TOO_MANY_REQUESTS)
                        .header("retry-after", "1")
                        .body(Body::from(
                            serde_json::json!({"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}).to_string()
                        ))
                        .expect("response")
                } else {
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(
                            serde_json::json!({
                                "id":"msg_1","type":"message","role":"assistant",
                                "content":[{"type":"text","text":"ok"}],
                                "model":"m1","stop_reason":"end_turn",
                                "usage":{"input_tokens":1,"output_tokens":1}
                            }).to_string()
                        ))
                        .expect("response")
                }
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let _handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let adapter = AnthropicAdapter::new(
        "prov-retry-429",
        format!("http://{addr}"),
        AuthConfig::None,
        Duration::from_millis(5000),
    )
    .expect("adapter")
    .with_retry(RetryConfig {
        max_retries: 2,
        base_delay_ms: 50,
        max_delay_ms: 200,
    });

    let res = adapter.complete(sample_request("m1", false)).await;
    assert!(res.is_ok(), "should succeed after retry: {res:?}");

    // Exactly 2 upstream hits (1 initial + 1 retry).
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retry_persistent_500_fails_after_max_retries() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = counter.clone();

    let server = spawn_counting_mock(counter_clone, |_: HeaderMap, _: String| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"type":"error","error":{"type":"api_error","message":"server error"}})
                .to_string(),
        )
    })
    .await;

    let adapter = AnthropicAdapter::new(
        "prov-retry-500",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(5000),
    )
    .expect("adapter")
    .with_retry(RetryConfig {
        max_retries: 2,
        base_delay_ms: 50,
        max_delay_ms: 200,
    });

    let err = adapter
        .complete(sample_request("m1", false))
        .await
        .expect_err("should fail");

    match err {
        ProviderError::Upstream { status, .. } => assert_eq!(status, 500),
        other => panic!("expected Upstream error, got: {other:?}"),
    }

    // max_retries + 1 = 3 total attempts.
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn retry_401_does_not_retry() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = counter.clone();

    let server = spawn_counting_mock(counter_clone, |_: HeaderMap, _: String| {
        (
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"type":"error","error":{"type":"authentication_error","message":"bad key"}})
                .to_string(),
        )
    })
    .await;

    let adapter = AnthropicAdapter::new(
        "prov-no-retry-401",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(5000),
    )
    .expect("adapter")
    .with_retry(RetryConfig {
        max_retries: 3,
        base_delay_ms: 50,
        max_delay_ms: 200,
    });

    let err = adapter
        .complete(sample_request("m1", false))
        .await
        .expect_err("should fail");

    assert!(matches!(err, ProviderError::Upstream { status: 401, .. }));
    // Exactly 1 attempt — no retries for 401.
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_stream_path_retries_pre_stream_failure() {
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();

    let app = Router::new().route(
        "/v1/messages",
        post(move |_headers: HeaderMap, _body: String| {
            let c = c.clone();
            async move {
                let count = c.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .body(Body::from(
                            serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}).to_string()
                        ))
                        .expect("response")
                } else {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(
                            concat!(
                                "event: message_start\n",
                                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_r\",\"model\":\"m1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                                "event: message_stop\n",
                                "data: {\"type\":\"message_stop\"}\n\n",
                            )
                            .to_string()
                        ))
                        .expect("response")
                }
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let _handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let adapter = AnthropicAdapter::new(
        "prov-retry-stream",
        format!("http://{addr}"),
        AuthConfig::None,
        Duration::from_millis(5000),
    )
    .expect("adapter")
    .with_retry(RetryConfig {
        max_retries: 2,
        base_delay_ms: 50,
        max_delay_ms: 200,
    });

    let mut stream = adapter
        .stream(sample_request("m1", true))
        .await
        .expect("stream should succeed after retry");

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event ok"));
    }

    assert!(
        events
            .iter()
            .any(|e| matches!(e, switchyard::providers::StreamEvent::MessageStart { .. }))
    );

    // 2 attempts (1 initial 503 + 1 successful retry).
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// F14: anthropic-beta header passthrough
// ---------------------------------------------------------------------------

#[tokio::test]
async fn adapter_forwards_anthropic_beta_header_to_upstream() {
    let server = spawn_mock(|headers: HeaderMap, body: String| {
        let beta = headers
            .get("anthropic-beta")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(beta, "prompt-caching-2024-07-31");
        // Also verify the body does not contain forward_headers.
        assert!(
            !body.contains("forward_headers"),
            "forward_headers leaked into body: {body}"
        );
        (
            StatusCode::OK,
            serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type":"text","text":"ok"}],
                "model": "m1",
                "stop_reason": "end_turn",
                "usage": {"input_tokens":1,"output_tokens":1}
            })
            .to_string(),
        )
    })
    .await;

    let adapter = AnthropicAdapter::new(
        "prov-fwd-beta",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(2000),
    )
    .expect("adapter");

    let mut req = sample_request("m1", false);
    req.forward_headers = vec![(
        "anthropic-beta".to_string(),
        "prompt-caching-2024-07-31".to_string(),
    )];
    let res = adapter.complete(req).await;
    assert!(
        res.is_ok(),
        "forwarded header should arrive upstream: {res:?}"
    );
}

#[tokio::test]
async fn extra_headers_override_same_name_forwarded_header() {
    let server = spawn_mock(|headers: HeaderMap, _body: String| {
        let beta = headers
            .get("anthropic-beta")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Config extra_headers should win over forwarded header.
        assert_eq!(beta, "config-wins");
        (
            StatusCode::OK,
            serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type":"text","text":"ok"}],
                "model": "m1",
                "stop_reason": "end_turn",
                "usage": {"input_tokens":1,"output_tokens":1}
            })
            .to_string(),
        )
    })
    .await;

    let cfg = ProviderConfig {
        id: "prov-override".to_string(),
        base_url: server.base_url().parse().expect("url"),
        auth: AuthConfig::None,
        models: vec![],
        connect_timeout_ms: None,
        read_timeout_ms: Some(2000),
        default_model: None,
        extra_headers: vec![("anthropic-beta".to_string(), "config-wins".to_string())],
        retry: None,
    };
    cfg.validate().expect("valid config");
    let adapter = AnthropicAdapter::from_config(&cfg).expect("adapter");

    let mut req = sample_request("m1", false);
    req.forward_headers = vec![("anthropic-beta".to_string(), "client-value".to_string())];
    let res = adapter.complete(req).await;
    assert!(
        res.is_ok(),
        "extra_headers should override forwarded header: {res:?}"
    );
}

#[tokio::test]
async fn forward_headers_do_not_leak_into_request_body_json() {
    let server = spawn_mock(|_headers: HeaderMap, body: String| {
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json body");
        assert!(
            parsed.get("forward_headers").is_none(),
            "forward_headers must not appear in JSON body: {body}"
        );
        (
            StatusCode::OK,
            serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type":"text","text":"ok"}],
                "model": "m1",
                "stop_reason": "end_turn",
                "usage": {"input_tokens":1,"output_tokens":1}
            })
            .to_string(),
        )
    })
    .await;

    let adapter = AnthropicAdapter::new(
        "prov-no-leak",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(2000),
    )
    .expect("adapter");

    let mut req = sample_request("m1", false);
    req.forward_headers = vec![("anthropic-beta".to_string(), "some-beta".to_string())];
    let res = adapter.complete(req).await;
    assert!(res.is_ok(), "request should succeed: {res:?}");
}

#[tokio::test]
async fn stream_also_forwards_anthropic_beta_header() {
    let server = spawn_mock(|headers: HeaderMap, _body: String| {
        let beta = headers
            .get("anthropic-beta")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(beta, "interleaved-thinking-2025-05-14");
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(
                concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"model\":\"m1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n",
                )
                .to_string(),
            ))
            .expect("response")
    })
    .await;

    let adapter = AnthropicAdapter::new(
        "prov-stream-beta",
        server.base_url(),
        AuthConfig::None,
        Duration::from_millis(2000),
    )
    .expect("adapter");

    let mut req = sample_request("m1", true);
    req.forward_headers = vec![(
        "anthropic-beta".to_string(),
        "interleaved-thinking-2025-05-14".to_string(),
    )];
    let mut stream = adapter.stream(req).await.expect("stream should start");
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event ok"));
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, switchyard::providers::StreamEvent::MessageStop))
    );
}
