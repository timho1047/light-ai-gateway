//! Server-sent-events metering.
//!
//! FILE OWNER: teammate proxy-dev. Replace stub bodies; keep public signatures.
//!
//! Two details from provider documentation drive this module:
//!   1. Without stream_options.include_usage the upstream sends NO usage frame
//!      at all and self-metering records zero forever. The gateway must inject
//!      the flag when the client omitted it.
//!   2. The usage frame arrives AFTER the chunk carrying finish_reason, as its
//!      own event with an EMPTY choices array. A parser that reads
//!      choices[0] unconditionally will fail exactly there.
//!
//! Everything here treats the upstream payload as untrusted: unknown shapes are
//! ignored, absent fields are never coerced to zero, and no input can panic.

use crate::domain::TokenUsage;
use serde_json::Value;

/// One observation from a single SSE line.
#[derive(Debug, Clone, Default)]
pub struct SseObservation {
    /// True for the first chunk that carried actual content.
    pub first_content: bool,
    /// Present when this line carried a usage frame.
    pub usage: Option<TokenUsage>,
    /// True when the stream signalled completion.
    pub done: bool,
}

/// Aggregate result for one streamed response.
#[derive(Debug, Clone, Default)]
pub struct SseResult {
    pub usage: TokenUsage,
    pub saw_done: bool,
    pub content_chunks: u64,
}

/// Incremental SSE meter. Feed it raw lines exactly as received.
#[derive(Debug, Default)]
pub struct SseMeter {
    inner: SseResult,
    /// Total visible content characters, used for a conservative local
    /// completion-token estimate when the upstream never reports usage.
    content_chars: u64,
}

impl SseMeter {
    pub fn new() -> Self {
        SseMeter::default()
    }

    /// Feed one line (without the trailing newline).
    ///
    /// Never panics: comments, empty payloads, [DONE], non-JSON bodies and
    /// JSON of an unexpected shape all produce an empty observation.
    pub fn feed_line(&mut self, line: &str) -> SseObservation {
        let mut obs = SseObservation::default();
        let trimmed = line.trim_end_matches(['\r', '\n']).trim_start();
        if trimmed.is_empty() || trimmed.starts_with(':') {
            return obs;
        }
        // An SSE field we do not consume (event:, id:, retry:).
        let Some(payload) = trimmed.strip_prefix("data:") else {
            return obs;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            return obs;
        }
        if payload == "[DONE]" {
            self.inner.saw_done = true;
            obs.done = true;
            return obs;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return obs;
        };

        // Usage may arrive on any frame. The final OpenAI usage frame carries an
        // empty choices array, so nothing here indexes choices[0].
        if let Some(usage) = usage_from_json(&value) {
            self.merge_usage(&usage);
            obs.usage = Some(usage);
        }

        let is_first_content = self.inner.content_chunks == 0;
        if let Some(text) = content_text(&value)
            && !text.is_empty() {
                self.inner.content_chunks += 1;
                self.content_chars = self
                    .content_chars
                    .saturating_add(text.chars().count() as u64);
                obs.first_content = is_first_content;
            }

        if stream_finished(&value) {
            self.inner.saw_done = true;
            obs.done = true;
        }
        obs
    }

    /// Result once the stream ends.
    pub fn finish(&self) -> SseResult {
        self.inner.clone()
    }

    /// Visible content characters observed so far. A rough stand-in for
    /// completion length when the upstream sent no usage frame.
    pub fn content_chars(&self) -> u64 {
        self.content_chars
    }

    /// Merge a usage reading into the running total.
    ///
    /// max() rather than assignment: providers such as Anthropic split one
    /// reading across frames (message_start carries input_tokens, message_delta
    /// carries the cumulative output_tokens), so a later frame must not reset
    /// fields it did not report.
    fn merge_usage(&mut self, u: &TokenUsage) {
        let cur = &mut self.inner.usage;
        cur.prompt_tokens = cur.prompt_tokens.max(u.prompt_tokens);
        cur.completion_tokens = cur.completion_tokens.max(u.completion_tokens);
        cur.cached_tokens = cur.cached_tokens.max(u.cached_tokens);
        cur.reasoning_tokens = cur.reasoning_tokens.max(u.reasoning_tokens);
        cur.measured = true;
    }
}

/// How a response body is metered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterMode {
    /// Server-sent events, parsed line by line.
    Sse,
    /// A single JSON body (non-streaming response), parsed at the end.
    Json,
}

/// Largest unparsed SSE line we will hold before discarding it.
const MAX_LINE_BYTES: usize = 1 << 20;
/// Largest non-streaming body we will buffer to look for usage.
const MAX_JSON_BYTES: usize = 4 << 20;

/// Byte-level meter: splits a byte stream into SSE lines (without altering the
/// bytes) or accumulates one JSON body, depending on the mode.
#[derive(Debug)]
pub struct StreamMeter {
    mode: MeterMode,
    sse: SseMeter,
    line_buf: Vec<u8>,
    json_buf: Vec<u8>,
    json_truncated: bool,
    finished: bool,
}

impl Default for StreamMeter {
    fn default() -> Self {
        StreamMeter::sse()
    }
}

impl StreamMeter {
    /// Meter a server-sent-event stream.
    pub fn sse() -> Self {
        StreamMeter {
            mode: MeterMode::Sse,
            sse: SseMeter::new(),
            line_buf: Vec::new(),
            json_buf: Vec::new(),
            json_truncated: false,
            finished: false,
        }
    }

    /// Meter a single JSON body.
    pub fn json() -> Self {
        StreamMeter {
            mode: MeterMode::Json,
            ..StreamMeter::sse()
        }
    }

    pub fn mode(&self) -> MeterMode {
        self.mode
    }

    /// Feed raw bytes exactly as they arrived. Returns true the first time
    /// visible content is observed, so the caller can stamp TTFT.
    ///
    /// The bytes are only observed; the caller forwards them unchanged.
    pub fn push(&mut self, chunk: &[u8]) -> bool {
        if self.finished {
            return false;
        }
        match self.mode {
            MeterMode::Json => {
                if self.json_buf.len() < MAX_JSON_BYTES {
                    let room = MAX_JSON_BYTES - self.json_buf.len();
                    let take = chunk.len().min(room);
                    self.json_buf.extend_from_slice(&chunk[..take]);
                    if take < chunk.len() {
                        self.json_truncated = true;
                    }
                } else {
                    self.json_truncated = true;
                }
                false
            }
            MeterMode::Sse => {
                self.line_buf.extend_from_slice(chunk);
                self.drain_lines(false)
            }
        }
    }

    /// Result once the stream ends. Idempotent.
    pub fn finish(&mut self) -> SseResult {
        if !self.finished {
            self.finished = true;
            match self.mode {
                MeterMode::Sse => {
                    self.drain_lines(true);
                }
                MeterMode::Json => {
                    if !self.json_truncated
                        && let Ok(value) = serde_json::from_slice::<Value>(&self.json_buf) {
                            if let Some(usage) = usage_from_json(&value) {
                                self.sse.merge_usage(&usage);
                            }
                            // Keep the content-length hint for the local
                            // estimate when no usage frame was present.
                            if let Some(text) = content_text(&value)
                                && !text.is_empty() {
                                    self.sse.inner.content_chunks += 1;
                                    self.sse.content_chars = self
                                        .sse
                                        .content_chars
                                        .saturating_add(text.chars().count() as u64);
                                }
                            self.sse.inner.saw_done = true;
                        }
                }
            }
        }
        self.sse.finish()
    }

    /// Visible content characters observed so far.
    pub fn content_chars(&self) -> u64 {
        self.sse.content_chars()
    }

    fn drain_lines(&mut self, at_end: bool) -> bool {
        let mut first = false;
        while let Some(pos) = self.line_buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.line_buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]).into_owned();
            if self.sse.feed_line(&text).first_content {
                first = true;
            }
        }
        if at_end && !self.line_buf.is_empty() {
            let text = String::from_utf8_lossy(&self.line_buf).into_owned();
            self.line_buf.clear();
            if self.sse.feed_line(&text).first_content {
                first = true;
            }
        }
        if self.line_buf.len() > MAX_LINE_BYTES {
            // A hostile or broken upstream must not grow our memory without
            // bound; drop the partial line and keep relaying.
            self.line_buf.clear();
        }
        first
    }
}

/// Ensure stream_options.include_usage is set. Returns true when it was added.
///
/// Without this the usage frame is never sent, which would leave the entire
/// local ledger at zero.
///
/// Idempotent, and an explicit false is preserved: a deliberate client choice
/// is respected, but the field must at least be present for the caller to see
/// what it asked for.
pub fn inject_include_usage(body: &mut serde_json::Value) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    match obj.get_mut("stream_options") {
        Some(Value::Object(opts)) => {
            if opts.contains_key("include_usage") {
                false
            } else {
                opts.insert("include_usage".into(), Value::Bool(true));
                true
            }
        }
        // Do not clobber a shape we do not understand.
        Some(_) => false,
        None => {
            obj.insert(
                "stream_options".into(),
                serde_json::json!({ "include_usage": true }),
            );
            true
        }
    }
}

/// Replace the model field, returning the previous value.
pub fn set_model(body: &mut serde_json::Value, model: &str) -> Option<String> {
    let obj = body.as_object_mut()?;
    let old = obj.get("model").and_then(|m| m.as_str()).map(str::to_string);
    obj.insert("model".into(), serde_json::Value::String(model.to_string()));
    old
}

/// Extract usage from an OpenAI-shaped or Anthropic-shaped JSON value.
///
/// Accepts the usage object at "usage" (OpenAI chunks and responses) or at
/// "message.usage" (the Anthropic message_start event). Returns None when no
/// recognised token field is present, so an unrelated object can never be
/// reported as a real zero-token measurement.
pub fn usage_from_json(v: &Value) -> Option<TokenUsage> {
    let u = v
        .get("usage")
        .filter(|x| x.is_object())
        .or_else(|| v.get("message").and_then(|m| m.get("usage")))
        .filter(|x| x.is_object())?;

    let mut usage = TokenUsage {
        measured: true,
        ..Default::default()
    };
    let mut any = false;

    // OpenAI names first; Anthropic names as the fallback.
    if let Some(n) = u.get("prompt_tokens").and_then(Value::as_u64) {
        usage.prompt_tokens = n;
        any = true;
    } else if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
        usage.prompt_tokens = n;
        any = true;
    }

    if let Some(n) = u.get("completion_tokens").and_then(Value::as_u64) {
        usage.completion_tokens = n;
        any = true;
    } else if let Some(n) = u.get("output_tokens").and_then(Value::as_u64) {
        usage.completion_tokens = n;
        any = true;
    }

    if let Some(n) = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
    {
        usage.cached_tokens = n;
        any = true;
    } else if let Some(n) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
        usage.cached_tokens = n;
        any = true;
    }

    if let Some(n) = u
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
    {
        usage.reasoning_tokens = n;
        any = true;
    }

    if any { Some(usage) } else { None }
}

/// Visible content carried by one frame, if any.
///
/// Role-only deltas, empty deltas and tool-call deltas deliberately return
/// None: TTFT is measured to the first actual content token.
fn content_text(v: &Value) -> Option<&str> {
    // Anthropic: content_block_delta / content_block_start.
    if let Some(t) = v.get("delta").and_then(|d| d.get("text")).and_then(Value::as_str) {
        return Some(t);
    }
    if let Some(t) = v
        .get("content_block")
        .and_then(|b| b.get("text"))
        .and_then(Value::as_str)
    {
        return Some(t);
    }
    // OpenAI: choices[].delta.content (or the non-streaming message.content).
    let choices = v.get("choices")?.as_array()?;
    for c in choices {
        if let Some(t) = c.get("delta").and_then(|d| d.get("content"))
            && let Some(s) = text_of(t) {
                return Some(s);
            }
        if let Some(t) = c.get("message").and_then(|m| m.get("content"))
            && let Some(s) = text_of(t) {
                return Some(s);
            }
        if let Some(t) = c.get("text")
            && let Some(s) = text_of(t) {
                return Some(s);
            }
    }
    None
}

/// Content may be a bare string or an array of typed parts.
fn text_of(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s.as_str()),
        Value::Array(parts) => parts
            .iter()
            .find_map(|p| p.get("text").and_then(Value::as_str)),
        _ => None,
    }
}

/// True when the frame signals the end of generation.
fn stream_finished(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) == Some("message_stop") {
        return true;
    }
    if v
        .get("delta")
        .and_then(|d| d.get("stop_reason"))
        .map(|s| !s.is_null())
        .unwrap_or(false)
    {
        return true;
    }
    if let Some(choices) = v.get("choices").and_then(Value::as_array)
        && choices
            .iter()
            .any(|c| !c.get("finish_reason").map(Value::is_null).unwrap_or(true))
        {
            return true;
        }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_frame_with_empty_choices_is_metered() {
        let mut m = SseMeter::new();
        let obs = m.feed_line(
            r#"data: {"id":"c1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}}"#,
        );
        let usage = obs.usage.expect("the usage frame must be parsed");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.cached_tokens, 4);
        assert_eq!(usage.reasoning_tokens, 2);
        assert!(usage.measured);
        assert!(!obs.first_content, "a usage frame carries no content");
        assert!(!obs.done, "the usage frame is not the terminal marker");

        let r = m.finish();
        assert_eq!(r.usage.prompt_tokens, 11);
        assert_eq!(r.usage.completion_tokens, 7);
        assert_eq!(r.usage.total_tokens(), 18);
        assert!(r.usage.measured);
    }

    #[test]
    fn first_content_is_reported_once_and_skips_role_only_chunks() {
        let mut m = SseMeter::new();
        let role = m.feed_line(
            r#"data: {"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        );
        assert!(!role.first_content);
        let empty = m.feed_line(r#"data: {"choices":[{"index":0,"delta":{}}]}"#);
        assert!(!empty.first_content);

        let first = m.feed_line(
            r#"data: {"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        );
        assert!(first.first_content, "the first content chunk defines TTFT");

        let second = m.feed_line(
            r#"data: {"choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
        );
        assert!(!second.first_content, "only the first content chunk reports it");

        let r = m.finish();
        assert_eq!(r.content_chunks, 2);
        assert_eq!(m.content_chars(), 11);
    }

    #[test]
    fn done_marker_is_recognized() {
        let mut m = SseMeter::new();
        let obs = m.feed_line("data: [DONE]");
        assert!(obs.done);
        assert!(m.finish().saw_done);
    }

    #[test]
    fn malformed_and_unknown_lines_do_not_panic() {
        let mut m = SseMeter::new();
        for line in [
            "",
            ": keep-alive comment",
            "event: message_start",
            "id: 42",
            "retry: 1000",
            "data:",
            "data: ",
            "data: {not json",
            "data: [1,2,3]",
            r#"data: {"choices":"not-an-array"}"#,
            r#"data: {"choices":[],"usage":"nope"}"#,
            "data: null",
            "data: \u{0}\u{1}",
        ] {
            let _ = m.feed_line(line);
        }
        // Float and negative counts are not integers: they must be ignored
        // rather than truncated or coerced to zero.
        let obs = m.feed_line(r#"data: {"usage":{"prompt_tokens":-5,"completion_tokens":1.5}}"#);
        assert!(obs.usage.is_none());
        let r = m.finish();
        assert_eq!(r.usage.prompt_tokens, 0);
        assert!(!r.usage.measured);
    }

    #[test]
    fn inject_include_usage_is_idempotent_and_preserves_explicit_false() {
        let mut body = json!({"model": "m", "stream": true});
        assert!(inject_include_usage(&mut body));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert!(
            !inject_include_usage(&mut body),
            "a second call must not report a change"
        );
        assert_eq!(body["stream_options"]["include_usage"], json!(true));

        let mut explicit = json!({"stream": true, "stream_options": {"include_usage": false}});
        assert!(!inject_include_usage(&mut explicit));
        assert_eq!(explicit["stream_options"]["include_usage"], json!(false));

        let mut other = json!({"stream_options": {"foo": 1}});
        assert!(inject_include_usage(&mut other));
        assert_eq!(other["stream_options"]["foo"], json!(1));
        assert_eq!(other["stream_options"]["include_usage"], json!(true));

        // A non-object body is left alone.
        let mut arr = json!([1, 2, 3]);
        assert!(!inject_include_usage(&mut arr));
    }

    #[test]
    fn anthropic_usage_merges_across_events() {
        let mut m = SseMeter::new();
        let start = m.feed_line(
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":25,"output_tokens":1,"cache_read_input_tokens":10}}}"#,
        );
        assert_eq!(start.usage.unwrap().prompt_tokens, 25);

        let text = m.feed_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        );
        assert!(text.first_content);

        let stop = m.feed_line(
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#,
        );
        assert!(stop.done, "stop_reason signals the end");

        let r = m.finish();
        assert_eq!(r.usage.prompt_tokens, 25, "message_start keeps the prompt count");
        assert_eq!(r.usage.completion_tokens, 15, "message_delta carries the output count");
        assert_eq!(r.usage.cached_tokens, 10);
        assert!(r.usage.measured);
    }

    #[test]
    fn stream_meter_reassembles_lines_split_across_chunks() {
        let mut meter = StreamMeter::sse();
        assert!(!meter.push(br#"data: {"choices":[{"delta":{"cont"#));
        assert!(!meter.push(br#"ent":"Hello"},"finish_reason":null}]}"#));
        assert!(
            meter.push(b"\n\n"),
            "the payload is only a line once its newline arrives"
        );
        assert!(!meter.push(b"data: [DONE]\n\n"));
        let r = meter.finish();
        assert_eq!(r.content_chunks, 1);
        assert!(r.saw_done);
        assert_eq!(meter.content_chars(), 5);
    }

    #[test]
    fn json_mode_parses_a_non_streaming_body() {
        let mut meter = StreamMeter::json();
        meter.push(br#"{"id":"x","choices":[{"index":0,"message":{"role":"assistant","#);
        meter.push(br#""content":"Hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2}}"#);
        let r = meter.finish();
        assert_eq!(r.usage.prompt_tokens, 9);
        assert_eq!(r.usage.completion_tokens, 2);
        assert!(r.usage.measured);
        // finish() is idempotent.
        let again = meter.finish();
        assert_eq!(again.usage.prompt_tokens, 9);
    }

    #[test]
    fn truncated_or_garbage_json_is_unmeasured() {
        let mut meter = StreamMeter::json();
        meter.push(b"{not json at all");
        let r = meter.finish();
        assert!(!r.usage.measured);
        assert_eq!(r.usage.total_tokens(), 0);
    }

    #[test]
    fn set_model_returns_the_previous_value() {
        let mut body = json!({"model": "old"});
        assert_eq!(set_model(&mut body, "new").as_deref(), Some("old"));
        assert_eq!(body["model"], json!("new"));
        let mut empty = json!({});
        assert_eq!(set_model(&mut empty, "new"), None);
    }
}
