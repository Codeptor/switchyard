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
    ProviderError, ProviderRegistry,
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
        timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
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
        timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
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
        timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
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
        timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
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
        timeout_ms: Some(2000),
        default_model: Some("m1".to_string()),
        extra_headers: vec![],
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
        timeout_ms: Some(2000),
        default_model: None,
        extra_headers: vec![],
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
        timeout_ms: None,
        default_model: None,
        extra_headers: vec![],
    };
    reg.register_anthropic(cfg).expect("register");
    // Any model id should resolve.
    let h = reg
        .resolve("generic-prov", Some("arbitrary-model-xyz"))
        .expect("resolve any");
    assert_eq!(h.model_id, "arbitrary-model-xyz");
}
