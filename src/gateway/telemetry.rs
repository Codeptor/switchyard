//! Usage accounting and Prometheus metrics for the gateway.
//!
//! [`Telemetry<B>`] wraps any [`Backend`] and records per-request counters
//! (requests, errors, tokens) and latency histograms keyed by provider, model,
//! and UTC day. Exposes `/usage` (JSON) and `/metrics` (Prometheus text).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::backend::{
    Backend, BackendError, BackendFuture, BackendRequest, BackendStream, ModelInfo,
};

/// Per-(provider, model, day) usage counters.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageRow {
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy)]
struct Histogram {
    b01: u64,
    b05: u64,
    b1: u64,
    b2: u64,
    b5: u64,
    b10: u64,
    b30: u64,
    b60: u64,
    b120: u64,
    b300: u64,
    count: u64,
    sum: f64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            b01: 0,
            b05: 0,
            b1: 0,
            b2: 0,
            b5: 0,
            b10: 0,
            b30: 0,
            b60: 0,
            b120: 0,
            b300: 0,
            count: 0,
            sum: 0.0,
        }
    }
}

impl Histogram {
    fn observe(&mut self, v: f64) {
        if v <= 0.1 {
            self.b01 += 1;
        }
        if v <= 0.5 {
            self.b05 += 1;
        }
        if v <= 1.0 {
            self.b1 += 1;
        }
        if v <= 2.0 {
            self.b2 += 1;
        }
        if v <= 5.0 {
            self.b5 += 1;
        }
        if v <= 10.0 {
            self.b10 += 1;
        }
        if v <= 30.0 {
            self.b30 += 1;
        }
        if v <= 60.0 {
            self.b60 += 1;
        }
        if v <= 120.0 {
            self.b120 += 1;
        }
        if v <= 300.0 {
            self.b300 += 1;
        }
        self.count += 1;
        self.sum += v;
    }
}

/// Telemetry state shared between the backend wrapper and HTTP handlers.
#[derive(Debug, Default)]
pub struct TelemetryState {
    usage: Mutex<HashMap<(String, String, String), UsageRow>>,
    requests_ok: Mutex<HashMap<(String, String), u64>>,
    requests_err: Mutex<HashMap<(String, String), u64>>,
    tokens: Mutex<HashMap<(String, String, String), u64>>,
    duration: Mutex<HashMap<(String, String), Histogram>>,
}

impl TelemetryState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_complete(
        &self,
        provider: &str,
        model: &str,
        day: &str,
        ok: bool,
        input_tokens: u64,
        output_tokens: u64,
        duration_secs: Option<f64>,
    ) {
        // Usage row
        {
            let mut usage = self.usage.lock().expect("usage mutex");
            let row = usage
                .entry((provider.to_string(), model.to_string(), day.to_string()))
                .or_default();
            row.requests += 1;
            if !ok {
                row.errors += 1;
            }
            row.input_tokens += input_tokens;
            row.output_tokens += output_tokens;
        }
        // Request counter
        {
            let key = (provider.to_string(), model.to_string());
            if ok {
                let mut m = self.requests_ok.lock().expect("requests_ok mutex");
                *m.entry(key).or_insert(0) += 1;
            } else {
                let mut m = self.requests_err.lock().expect("requests_err mutex");
                *m.entry(key).or_insert(0) += 1;
            }
        }
        // Token counters
        if input_tokens > 0 {
            let mut m = self.tokens.lock().expect("tokens mutex");
            let key = (provider.to_string(), model.to_string(), "input".to_string());
            *m.entry(key).or_insert(0) += input_tokens;
        }
        if output_tokens > 0 {
            let mut m = self.tokens.lock().expect("tokens mutex");
            let key = (
                provider.to_string(),
                model.to_string(),
                "output".to_string(),
            );
            *m.entry(key).or_insert(0) += output_tokens;
        }
        // Duration histogram
        if let Some(secs) = duration_secs {
            let mut m = self.duration.lock().expect("duration mutex");
            let key = (provider.to_string(), model.to_string());
            m.entry(key).or_default().observe(secs);
        }
    }

    /// JSON snapshot for `/usage`.
    pub fn usage_snapshot(&self) -> Vec<UsageSnapshotRow> {
        let usage = self.usage.lock().expect("usage mutex");
        let mut rows: Vec<UsageSnapshotRow> = usage
            .iter()
            .map(|((provider, model, day), row)| UsageSnapshotRow {
                provider: provider.clone(),
                model: model.clone(),
                day: day.clone(),
                requests: row.requests,
                errors: row.errors,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
            })
            .collect();
        rows.sort_by(|a, b| {
            a.provider
                .cmp(&b.provider)
                .then(a.model.cmp(&b.model))
                .then(a.day.cmp(&b.day))
        });
        rows
    }

    /// Prometheus text exposition for `/metrics`.
    pub fn metrics_text(&self) -> String {
        let mut out = String::with_capacity(4096);

        // switchyard_requests_total
        let _ = writeln!(
            out,
            "# HELP switchyard_requests_total Total completed requests."
        );
        let _ = writeln!(out, "# TYPE switchyard_requests_total counter");
        {
            let ok_map = self.requests_ok.lock().expect("requests_ok mutex");
            for ((provider, model), count) in ok_map.iter() {
                let _ = writeln!(
                    out,
                    "switchyard_requests_total{{provider=\"{provider}\",model=\"{model}\",class=\"ok\"}} {count}"
                );
            }
        }
        {
            let err_map = self.requests_err.lock().expect("requests_err mutex");
            for ((provider, model), count) in err_map.iter() {
                let _ = writeln!(
                    out,
                    "switchyard_requests_total{{provider=\"{provider}\",model=\"{model}\",class=\"error\"}} {count}"
                );
            }
        }

        // switchyard_tokens_total
        let _ = writeln!(out, "# HELP switchyard_tokens_total Total tokens.");
        let _ = writeln!(out, "# TYPE switchyard_tokens_total counter");
        {
            let tok_map = self.tokens.lock().expect("tokens mutex");
            for ((provider, model, kind), count) in tok_map.iter() {
                let _ = writeln!(
                    out,
                    "switchyard_tokens_total{{provider=\"{provider}\",model=\"{model}\",kind=\"{kind}\"}} {count}"
                );
            }
        }

        // switchyard_request_duration_seconds
        let _ = writeln!(
            out,
            "# HELP switchyard_request_duration_seconds Request duration histogram."
        );
        let _ = writeln!(out, "# TYPE switchyard_request_duration_seconds histogram");
        {
            let dur_map = self.duration.lock().expect("duration mutex");
            for ((provider, model), h) in dur_map.iter() {
                let labels = format!("provider=\"{provider}\",model=\"{model}\"");
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"0.1\"}} {}",
                    h.b01
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"0.5\"}} {}",
                    h.b05
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"1\"}} {}",
                    h.b1
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"2\"}} {}",
                    h.b2
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"5\"}} {}",
                    h.b5
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"10\"}} {}",
                    h.b10
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"30\"}} {}",
                    h.b30
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"60\"}} {}",
                    h.b60
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"120\"}} {}",
                    h.b120
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"300\"}} {}",
                    h.b300
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}",
                    h.count
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_count{{{labels}}} {}",
                    h.count
                );
                let _ = writeln!(
                    out,
                    "switchyard_request_duration_seconds_sum{{{labels}}} {}",
                    h.sum
                );
            }
        }

        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshotRow {
    pub provider: String,
    pub model: String,
    pub day: String,
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Telemetry-wrapping backend. Records usage and metrics for every request.
pub struct Telemetry<B> {
    inner: B,
    state: Arc<TelemetryState>,
}

impl<B: Backend> Telemetry<B> {
    pub fn new(inner: B, state: Arc<TelemetryState>) -> Self {
        Self { inner, state }
    }

    pub fn state(&self) -> &Arc<TelemetryState> {
        &self.state
    }
}

impl<B: Backend> Backend for Telemetry<B> {
    fn models(&self) -> Vec<ModelInfo> {
        self.inner.models()
    }

    fn complete(&self, request: BackendRequest) -> BackendFuture<'_, Value> {
        let (provider, model) = parse_route(&request.model);
        let start = Instant::now();
        let state = Arc::clone(&self.state);
        let fut = self.inner.complete(request);
        Box::pin(async move {
            let result = fut.await;
            let elapsed = start.elapsed().as_secs_f64();
            let day = utc_day();
            match &result {
                Ok(value) => {
                    let (input_tokens, output_tokens) = extract_complete_usage(value);
                    state.record_complete(
                        &provider,
                        &model,
                        &day,
                        true,
                        input_tokens,
                        output_tokens,
                        Some(elapsed),
                    );
                }
                Err(_) => {
                    state.record_complete(&provider, &model, &day, false, 0, 0, Some(elapsed));
                }
            }
            result
        })
    }

    fn stream(&self, request: BackendRequest) -> BackendFuture<'_, BackendStream> {
        let (provider, model) = parse_route(&request.model);
        let start = Instant::now();
        let state = Arc::clone(&self.state);
        let fut = self.inner.stream(request);
        Box::pin(async move {
            let result = fut.await;
            match result {
                Ok(stream) => {
                    let provider = provider.clone();
                    let model = model.clone();
                    let state = Arc::clone(&state);
                    let wrapped = TelemetryStream {
                        inner: stream,
                        provider,
                        model,
                        state,
                        start,
                        first_event: true,
                        input_tokens: 0,
                        output_tokens: 0,
                        had_error: false,
                    };
                    Ok(Box::pin(wrapped) as BackendStream)
                }
                Err(_) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    let day = utc_day();
                    state.record_complete(&provider, &model, &day, false, 0, 0, Some(elapsed));
                    Err(BackendError::Unavailable(
                        "stream creation failed".to_string(),
                    ))
                }
            }
        })
    }
}

fn parse_route(model: &str) -> (String, String) {
    if let Some((provider, rest)) = model.split_once('/') {
        (provider.to_string(), rest.to_string())
    } else {
        ("unknown".to_string(), model.to_string())
    }
}

fn extract_complete_usage(value: &Value) -> (u64, u64) {
    let usage = value.get("usage");
    let input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (input, output)
}

/// Wrapping stream that captures usage from message_start/message_delta events.
struct TelemetryStream {
    inner: BackendStream,
    provider: String,
    model: String,
    state: Arc<TelemetryState>,
    start: Instant,
    first_event: bool,
    input_tokens: u64,
    output_tokens: u64,
    had_error: bool,
}

impl Stream for TelemetryStream {
    type Item = Result<Value, BackendError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        match this.inner.poll_next_unpin(cx) {
            std::task::Poll::Ready(Some(Ok(event))) => {
                if this.first_event {
                    this.first_event = false;
                    // Record time-to-first-event for histogram
                    let elapsed = this.start.elapsed().as_secs_f64();
                    let mut m = this.state.duration.lock().expect("duration mutex");
                    let key = (this.provider.clone(), this.model.clone());
                    m.entry(key).or_default().observe(elapsed);
                }
                // Extract usage from message_start
                if let Some(msg) = event.get("message")
                    && let Some(usage) = msg.get("usage")
                    && let Some(v) = usage.get("input_tokens").and_then(Value::as_u64)
                {
                    this.input_tokens += v;
                }
                // Extract usage from message_delta
                if event.get("type").and_then(Value::as_str) == Some("message_delta")
                    && let Some(usage) = event.get("usage")
                    && let Some(v) = usage.get("output_tokens").and_then(Value::as_u64)
                {
                    this.output_tokens = v;
                }
                std::task::Poll::Ready(Some(Ok(event)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                this.had_error = true;
                std::task::Poll::Ready(Some(Err(error)))
            }
            std::task::Poll::Ready(None) => {
                // Stream ended — commit counters (histogram already recorded
                // at first event, so pass None for duration).
                let day = utc_day();
                this.state.record_complete(
                    &this.provider,
                    &this.model,
                    &day,
                    !this.had_error,
                    this.input_tokens,
                    this.output_tokens,
                    None,
                );
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Compute a UTC day string (YYYY-MM-DD) from current system time.
fn utc_day() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    day_from_secs(secs)
}

/// Convert unix seconds to YYYY-MM-DD UTC. Dep-free civil date calculation.
fn day_from_secs(secs: u64) -> String {
    let days = secs / 86400;
    // Algorithm from Howard Hinnant's date library (civil_from_days)
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // year of era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // month index [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // day [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // month [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_from_known_epoch() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        assert_eq!(day_from_secs(1704067200), "2024-01-01");
        // 2024-06-15 12:00:00 UTC = 1718452800
        assert_eq!(day_from_secs(1718452800), "2024-06-15");
        // Unix epoch
        assert_eq!(day_from_secs(0), "1970-01-01");
    }

    #[test]
    fn parse_route_splits_provider_model() {
        assert_eq!(
            parse_route("kimi/kimi-k3[1m]"),
            ("kimi".to_string(), "kimi-k3[1m]".to_string())
        );
        assert_eq!(
            parse_route("noslash"),
            ("unknown".to_string(), "noslash".to_string())
        );
    }

    #[test]
    fn extract_usage_from_response() {
        let value = serde_json::json!({
            "type": "message",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        assert_eq!(extract_complete_usage(&value), (100, 50));
    }

    #[test]
    fn extract_usage_missing_fields() {
        let value = serde_json::json!({"type": "message"});
        assert_eq!(extract_complete_usage(&value), (0, 0));
    }

    #[test]
    fn histogram_buckets_correct() {
        let mut h = Histogram::default();
        h.observe(0.05); // ≤0.1
        h.observe(0.3); // ≤0.5
        h.observe(0.8); // ≤1
        h.observe(1.5); // ≤2
        assert_eq!(h.b01, 1);
        assert_eq!(h.b05, 2); // 0.05 + 0.3
        assert_eq!(h.b1, 3); // + 0.8
        assert_eq!(h.b2, 4); // + 1.5
        assert_eq!(h.count, 4);
    }
}
