//! Anthropic Messages typed data structures.
//!
//! Preserves Claude Code semantics for messages, streaming, tool use,
//! stop reasons, and usage. Provider quirks are not represented here;
//! they are normalized at the adapter boundary.

use serde::de::{Deserializer, Error as _};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Stop reason
// ---------------------------------------------------------------------------

/// Normalized stop reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    #[serde(other)]
    Unknown,
}

impl StopReason {
    /// Parse an upstream string (`end_turn`, `max_tokens`, `stop_sequence`, `tool_use`)
    /// into the normalized variant. Unknown values map to `Unknown` rather than erroring,
    /// keeping the boundary forward-compatible.
    pub fn parse(s: &str) -> Self {
        match s {
            "end_turn" => Self::EndTurn,
            "max_tokens" => Self::MaxTokens,
            "stop_sequence" => Self::StopSequence,
            "tool_use" => Self::ToolUse,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTokens => "max_tokens",
            Self::StopSequence => "stop_sequence",
            Self::ToolUse => "tool_use",
            Self::Unknown => "end_turn",
        }
    }
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    /// Provider-specific usage counters, such as cached input tokens.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text {
        text: String,
        #[allow(dead_code)]
        extra: BTreeMap<String, Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        extra: BTreeMap<String, Value>,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        is_error: bool,
        extra: BTreeMap<String, Value>,
    },
    // Image blocks are passed through opaquely to keep the boundary generic.
    Image {
        source: Value,
        extra: BTreeMap<String, Value>,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
        extra: BTreeMap<String, Value>,
    },
    RedactedThinking {
        data: String,
        extra: BTreeMap<String, Value>,
    },
    /// Preserve content block types introduced by a provider or newer API.
    Unknown(Value),
}

impl Serialize for ContentBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = match self {
            Self::Text { text, extra } => {
                object_with_type("text", extra, [("text", serde_json::json!(text))])
            }
            Self::ToolUse {
                id,
                name,
                input,
                extra,
            } => object_with_type(
                "tool_use",
                extra,
                [
                    ("id", serde_json::json!(id)),
                    ("name", serde_json::json!(name)),
                    ("input", input.clone()),
                ],
            ),
            Self::ToolResult {
                tool_use_id,
                content,
                is_error,
                extra,
            } => object_with_type(
                "tool_result",
                extra,
                [
                    ("tool_use_id", serde_json::json!(tool_use_id)),
                    ("content", content.clone()),
                    ("is_error", serde_json::json!(is_error)),
                ],
            ),
            Self::Image { source, extra } => {
                object_with_type("image", extra, [("source", source.clone())])
            }
            Self::Thinking {
                thinking,
                signature,
                extra,
            } => {
                let mut value = object_with_type(
                    "thinking",
                    extra,
                    [("thinking", serde_json::json!(thinking))],
                );
                if let Some(signature) = signature {
                    value["signature"] = serde_json::json!(signature);
                }
                value
            }
            Self::RedactedThinking { data, extra } => object_with_type(
                "redacted_thinking",
                extra,
                [("data", serde_json::json!(data))],
            ),
            Self::Unknown(value) => value.clone(),
        };
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let Some(kind) = value.get("type").and_then(Value::as_str).map(str::to_owned) else {
            return Ok(Self::Unknown(value));
        };
        let Value::Object(mut object) = value else {
            return Ok(Self::Unknown(value));
        };
        object.remove("type");

        let required = |name: &str, object: &mut serde_json::Map<String, Value>| {
            object.remove(name).ok_or_else(|| {
                D::Error::custom(format!("content block {kind:?} is missing {name:?}"))
            })
        };
        let parse = |name: &str, object: &mut serde_json::Map<String, Value>| {
            serde_json::from_value(required(name, object)?).map_err(D::Error::custom)
        };

        match kind.as_str() {
            "text" => Ok(Self::Text {
                text: parse("text", &mut object)?,
                extra: object.into_iter().collect(),
            }),
            "tool_use" => Ok(Self::ToolUse {
                id: parse("id", &mut object)?,
                name: parse("name", &mut object)?,
                input: required("input", &mut object)?,
                extra: object.into_iter().collect(),
            }),
            "tool_result" => Ok(Self::ToolResult {
                tool_use_id: parse("tool_use_id", &mut object)?,
                content: required("content", &mut object)?,
                is_error: object
                    .remove("is_error")
                    .map(|value| serde_json::from_value(value).map_err(D::Error::custom))
                    .transpose()?
                    .unwrap_or(false),
                extra: object.into_iter().collect(),
            }),
            "image" => Ok(Self::Image {
                source: required("source", &mut object)?,
                extra: object.into_iter().collect(),
            }),
            "thinking" => Ok(Self::Thinking {
                thinking: parse("thinking", &mut object)?,
                signature: object
                    .remove("signature")
                    .map(|value| serde_json::from_value(value).map_err(D::Error::custom))
                    .transpose()?,
                extra: object.into_iter().collect(),
            }),
            "redacted_thinking" => Ok(Self::RedactedThinking {
                data: parse("data", &mut object)?,
                extra: object.into_iter().collect(),
            }),
            _ => {
                object.insert("type".to_string(), Value::String(kind));
                Ok(Self::Unknown(Value::Object(object)))
            }
        }
    }
}

fn object_with_type<const N: usize>(
    kind: &str,
    extra: &BTreeMap<String, Value>,
    fields: [(&str, Value); N],
) -> Value {
    let mut object = extra.clone();
    object.insert("type".to_string(), Value::String(kind.to_string()));
    object.extend(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_string(), value)),
    );
    Value::Object(object.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

/// Anthropic Messages request. Common fields are typed while provider-specific
/// fields are retained and forwarded unchanged through `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl MessagesRequest {
    /// Validate invariants that the gateway relies on.
    pub fn validate(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("model is required".to_string());
        }
        if self.messages.is_empty() {
            return Err("messages must not be empty".to_string());
        }
        if self.max_tokens == 0 {
            return Err("max_tokens must be > 0".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    pub usage: Usage,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    pub r#type: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicErrorBody {
    #[serde(rename = "type")]
    pub r#type: String,
    pub error: AnthropicErrorDetail,
}

// ---------------------------------------------------------------------------
// Helpers for tool-use accumulation
// ---------------------------------------------------------------------------

/// Accumulator for incremental `input_json_delta` chunks during streaming.
#[derive(Debug, Default)]
pub struct ToolInputAccumulator {
    buffer: String,
}

impl ToolInputAccumulator {
    pub fn push(&mut self, delta: &str) {
        self.buffer.push_str(delta);
    }

    /// Attempt to parse the accumulated JSON. Returns `None` if incomplete.
    pub fn try_parse(&self) -> Option<Value> {
        if self.buffer.is_empty() {
            return None;
        }
        serde_json::from_str(&self.buffer).ok()
    }

    pub fn raw(&self) -> &str {
        &self.buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_parse() {
        assert_eq!(StopReason::parse("end_turn"), StopReason::EndTurn);
        assert_eq!(StopReason::parse("tool_use"), StopReason::ToolUse);
        assert_eq!(StopReason::parse("unknown_xyz"), StopReason::Unknown);
    }

    #[test]
    fn request_validate() {
        let req = MessagesRequest {
            model: "m1".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: MessageContent::Text("hi".to_string()),
                extra: BTreeMap::new(),
            }],
            max_tokens: 100,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stream: None,
            stop_sequences: None,
            tools: None,
            tool_choice: None,
            metadata: None,
            extra: BTreeMap::new(),
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn request_round_trips_provider_specific_fields() {
        let input = serde_json::json!({
            "model": "qwen3.8-max",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 256,
            "tool_choice": {"type": "none"},
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "reasoning_effort": "high",
            "provider_extension": {"priority": "latency"}
        });

        let request: MessagesRequest = serde_json::from_value(input.clone()).expect("request");
        let output = serde_json::to_value(request).expect("wire request");

        assert_eq!(output["thinking"], input["thinking"]);
        assert_eq!(output["tool_choice"], input["tool_choice"]);
        assert_eq!(output["reasoning_effort"], input["reasoning_effort"]);
        assert_eq!(output["provider_extension"], input["provider_extension"]);
    }

    #[test]
    fn response_accepts_thinking_content_blocks() {
        let input = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "check the tool result", "signature": "sig"},
                {"type": "text", "text": "done"}
            ],
            "model": "qwen3.8-max",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });

        let response: MessagesResponse = serde_json::from_value(input.clone()).expect("response");
        let output = serde_json::to_value(response).expect("wire response");
        assert_eq!(output["content"][0]["type"], "thinking");
        assert_eq!(output["content"][0]["thinking"], "check the tool result");
        assert_eq!(output["content"][1]["text"], "done");
    }
}
