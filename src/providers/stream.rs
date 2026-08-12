//! Streaming SSE / event-source parsing for Anthropic-compatible streams.
//!
//! Normalizes upstream streaming chunks into typed [`StreamEvent`]s.
//! Also re-exports a helper to produce an Anthropic SSE byte stream.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::providers::types::{StopReason, Usage};

// ---------------------------------------------------------------------------
// Normalized stream events (what Codex consumes)
// ---------------------------------------------------------------------------

/// Normalized streaming events exposed to the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        id: String,
        model: String,
        usage: Usage,
    },
    ContentBlockStart {
        index: u32,
        content_block: Value,
    },
    ContentBlockDelta {
        index: u32,
        delta: Delta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        stop_reason: Option<StopReason>,
        stop_sequence: Option<String>,
        usage: Usage,
    },
    MessageStop,
    Ping,
    Error {
        error: Value,
    },
}

/// Delta payload inside `content_block_delta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    // Passthrough for future delta types.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Wire format (Anthropic SSE)
// ---------------------------------------------------------------------------

/// Raw SSE event parsed from the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Parse a raw SSE byte stream (as produced by `reqwest::bytes_stream`) into
/// `SseEvent`s. Handles `event:` / `data:` lines and blank-line delimiters.
///
/// This function operates on a complete text buffer; for incremental network
/// parsing see [`SseParser`].
pub fn parse_sse_buffer(text: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut cur_event: Option<String> = None;
    let mut cur_data = String::new();

    for line in text.lines() {
        if line.is_empty() {
            if !cur_data.is_empty() || cur_event.is_some() {
                events.push(SseEvent {
                    event: cur_event.take(),
                    data: cur_data.trim_end_matches('\n').to_string(),
                });
                cur_data.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            cur_event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            let chunk = rest.strip_prefix(' ').unwrap_or(rest);
            if !cur_data.is_empty() {
                cur_data.push('\n');
            }
            cur_data.push_str(chunk);
        } else if line.starts_with(':') {
            // SSE comment / ping — treat as ping event if no data.
            continue;
        } else if line.starts_with("data :") || line.starts_with("event :") {
            // tolerate space before colon
            continue;
        }
    }
    if !cur_data.is_empty() || cur_event.is_some() {
        events.push(SseEvent {
            event: cur_event,
            data: cur_data,
        });
    }
    events
}

/// Incremental SSE parser that handles chunked `bytes` delivery.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes (UTF-8 lossy) and extract any complete events.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let text = String::from_utf8_lossy(chunk);
        self.buf.push_str(&text);
        let mut out = Vec::new();
        // Events are delimited by double newline.
        while let Some(pos) = self.buf.find("\n\n") {
            let frame = self.buf[..pos].to_string();
            self.buf.drain(..pos + 2);
            out.extend(parse_sse_buffer(&(frame + "\n\n")));
        }
        // Also handle CRLF variants.
        while let Some(pos) = self.buf.find("\r\n\r\n") {
            let frame = self.buf[..pos].to_string();
            self.buf.drain(..pos + 4);
            out.extend(parse_sse_buffer(&(frame + "\n\n")));
        }
        out
    }

    /// Drain any trailing buffered data as a final event (non-standard but useful for tests).
    pub fn flush(&mut self) -> Vec<SseEvent> {
        if self.buf.trim().is_empty() {
            self.buf.clear();
            return vec![];
        }
        let taken = std::mem::take(&mut self.buf);
        parse_sse_buffer(&taken)
    }
}

/// Normalize a raw `SseEvent` into a typed [`StreamEvent`].
///
/// Returns `None` for unrecognized or incomplete frames; those are logged
/// at debug level by the caller.
pub fn normalize_sse_event(raw: &SseEvent) -> Option<StreamEvent> {
    // Prefer `event` field; fall back to `type` inside JSON.
    let data = raw.data.trim();
    if data.is_empty() {
        if raw.event.as_deref() == Some("ping") {
            return Some(StreamEvent::Ping);
        }
        return None;
    }
    // Anthropic sometimes sends `[DONE]` — treat as MessageStop.
    if data == "[DONE]" {
        return Some(StreamEvent::MessageStop);
    }
    let value: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => {
            // If the data itself is a ping comment.
            if raw.event.as_deref() == Some("ping") {
                return Some(StreamEvent::Ping);
            }
            return None;
        }
    };

    let type_str = raw
        .event
        .as_deref()
        .or_else(|| value.get("type").and_then(|v| v.as_str()))
        .unwrap_or("");

    match type_str {
        "message_start" => {
            let msg = value.get("message")?;
            let id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let model = msg
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let usage = msg
                .get("usage")
                .and_then(|v| serde_json::from_value::<Usage>(v.clone()).ok())
                .unwrap_or_default();
            Some(StreamEvent::MessageStart { id, model, usage })
        }
        "content_block_start" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let block = value.get("content_block").cloned().unwrap_or(Value::Null);
            Some(StreamEvent::ContentBlockStart {
                index,
                content_block: block,
            })
        }
        "content_block_delta" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let delta_val = value.get("delta")?.clone();
            let delta: Delta = serde_json::from_value(delta_val).unwrap_or(Delta::Unknown);
            Some(StreamEvent::ContentBlockDelta { index, delta })
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            Some(StreamEvent::ContentBlockStop { index })
        }
        "message_delta" => {
            let delta = value.get("delta")?;
            let stop_reason = delta
                .get("stop_reason")
                .and_then(|v| v.as_str())
                .map(StopReason::parse);
            let stop_sequence = delta
                .get("stop_sequence")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let usage = value
                .get("usage")
                .and_then(|v| serde_json::from_value::<Usage>(v.clone()).ok())
                .unwrap_or_default();
            Some(StreamEvent::MessageDelta {
                stop_reason,
                stop_sequence,
                usage,
            })
        }
        "message_stop" => Some(StreamEvent::MessageStop),
        "ping" => Some(StreamEvent::Ping),
        "error" => Some(StreamEvent::Error { error: value }),
        _ => {
            // Unknown type — expose as Error for forward compatibility if it carries `error`.
            if value.get("error").is_some() {
                return Some(StreamEvent::Error { error: value });
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta_stream() {
        let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let raw = parse_sse_buffer(sse);
        assert_eq!(raw.len(), 6);
        let norm: Vec<_> = raw.iter().filter_map(normalize_sse_event).collect();
        assert_eq!(norm.len(), 6);
        assert!(matches!(norm[0], StreamEvent::MessageStart { .. }));
        assert!(matches!(norm[2], StreamEvent::ContentBlockDelta { .. }));
        assert!(matches!(norm[5], StreamEvent::MessageStop));
    }

    #[test]
    fn parses_tool_input_deltas() {
        let sse = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"hi\\\"}\"}}\n\n";
        let raw = parse_sse_buffer(sse);
        let norm: Vec<_> = raw.iter().filter_map(normalize_sse_event).collect();
        assert_eq!(norm.len(), 2);
        for ev in &norm {
            if let StreamEvent::ContentBlockDelta {
                delta: Delta::InputJsonDelta { partial_json },
                ..
            } = ev
            {
                assert!(!partial_json.is_empty());
            } else {
                panic!("expected input_json delta");
            }
        }
    }

    #[test]
    fn incremental_parser() {
        let mut p = SseParser::new();
        let chunks = [
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hel",
            "lo\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];
        let mut events = Vec::new();
        for c in chunks {
            events.extend(p.feed(c.as_bytes()));
        }
        assert_eq!(events.len(), 2);
        let norm: Vec<_> = events.iter().filter_map(normalize_sse_event).collect();
        assert_eq!(norm.len(), 2);
    }
}
