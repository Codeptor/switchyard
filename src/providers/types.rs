//! Anthropic Messages typed data structures.
//!
//! Preserves Claude Code semantics for messages, streaming, tool use,
//! stop reasons, and usage. Provider quirks are not represented here;
//! they are normalized at the adapter boundary.

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    // Image blocks are passed through opaquely to keep the boundary generic.
    #[serde(rename = "image")]
    Image {
        source: Value,
    },
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

/// Anthropic Messages request. Uses typed fields; unknown fields are denied
/// at the gateway layer, forwarded opaquely here if needed via `extra`.
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
        };
        assert!(req.validate().is_ok());
    }
}
