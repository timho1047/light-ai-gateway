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
    /// True when this line carried an in-stream error frame.
    pub error: bool,
}

/// Aggregate result for one streamed response.
#[derive(Debug, Clone, Default)]
pub struct SseResult {
    pub usage: TokenUsage,
    pub saw_done: bool,
    pub content_chunks: u64,
    /// Set when the stream carried an error frame.
    ///
    /// Some providers answer with HTTP 200 and only report the failure inside
    /// the stream, for example:
    ///   data: {"error":{"message":"A server error occurred.","type":"server_error"}}
    /// Relaying those bytes is correct, but recording them as a success is not:
    /// it inflates request counts and hides a provider that is failing.
    pub error: Option<String>,
}

impl SseResult {
    /// Whether the stream ended with a terminal marker.
    ///
    /// A missing marker means the stream was cut short -- the client
    /// disconnected, or the upstream died mid-answer -- and proxy.rs needs to
    /// see that to flag the request instead of recording a truncated stream as
    /// a complete one. The field stays public so existing construction is
    /// unchanged.
    pub fn saw_done(&self) -> bool {
        self.saw_done
    }
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

        // An error delivered inside the stream, after a 200. Recorded so the
        // request is not counted as a success, while the bytes are still relayed
        // to the client unchanged.
        if let Some(err) = stream_error(&value) {
            if self.inner.error.is_none() {
                self.inner.error = Some(err);
            }
            obs.error = true;
        }

        // Usage may arrive on any frame. The final OpenAI usage frame carries an
        // empty choices array, so nothing here indexes choices[0].
        if let Some(usage) = usage_from_json(&value) {
            self.merge_usage(&usage);
            obs.usage = Some(usage);
        }

        let is_first_content = self.inner.content_chunks == 0;
        if let Some(len) = content_len(&value) {
            self.inner.content_chunks += 1;
            self.content_chars = self.content_chars.saturating_add(len);
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
        // OR, not assignment: an all-zero frame is demoted to unmeasured (see
        // usage_from_json), and it must neither erase a real reading that
        // arrived earlier in the stream nor suppress one that follows it.
        cur.measured = cur.measured || u.measured;
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
                        && let Ok(value) = serde_json::from_slice::<Value>(&self.json_buf)
                    {
                        if let Some(usage) = usage_from_json(&value) {
                            self.sse.merge_usage(&usage);
                        }
                        // Keep the content-length hint for the local
                        // estimate when no usage frame was present.
                        if let Some(len) = content_len(&value) {
                            self.sse.inner.content_chunks += 1;
                            self.sse.content_chars = self.sse.content_chars.saturating_add(len);
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
        let mut consumed = 0;
        // The scan resumes where the last line ended instead of restarting at 0,
        // and the buffer is compacted once per push instead of once per line:
        // each drain moved every byte after the newline and allocated a Vec and
        // a String, which is quadratic in the lines a chunk carries -- and this
        // runs inside poll_next, so it is relay latency.
        while let Some(offset) = self.line_buf[consumed..].iter().position(|b| *b == b'\n') {
            let end = consumed + offset;
            let text = String::from_utf8_lossy(&self.line_buf[consumed..end]).into_owned();
            if self.sse.feed_line(&text).first_content {
                first = true;
            }
            consumed = end + 1;
        }
        if consumed > 0 {
            self.line_buf.drain(..consumed);
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

/// A top-level error object carried inside an otherwise successful stream.
///
/// Returns a short, machine-readable description. The human message is
/// deliberately not kept: providers reword it freely and it is not ours to
/// interpret.
fn stream_error(value: &Value) -> Option<String> {
    let err = value.get("error")?;
    let ty = err.get("type").and_then(Value::as_str);
    let code = err.get("code").and_then(Value::as_str);
    let label = code.or(ty).unwrap_or("error");
    Some(label.chars().take(64).collect())
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
    let old = obj
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    obj.insert("model".into(), serde_json::Value::String(model.to_string()));
    old
}

/// Extract usage from an OpenAI-shaped or Anthropic-shaped JSON value.
///
/// Accepts the usage object at "usage" (OpenAI chunks and responses) or at
/// "message.usage" (the Anthropic message_start event). Returns None when no
/// recognised token field is present, so an unrelated object can never be
/// reported as a real zero-token measurement.
///
/// Three cache dialects are accepted because all three are live on this
/// gateway: OpenAI's prompt_tokens_details.cached_tokens, Anthropic's
/// cache_read_input_tokens, and DeepSeek's prompt_cache_hit_tokens /
/// prompt_cache_miss_tokens. Before the DeepSeek pair was read here, every
/// DeepSeek request metered cached = 0: the cache-hit ratio was structurally
/// zero and the prompt tokens it discounts were billed at the full uncached
/// rate, which the pricing panel then reported as price drift.
///
/// A frame whose recognised counters are ALL zero comes back with
/// measured = false. Providers emit those as pacing/keepalive noise, and
/// taking one as a reading would hand the hard gate cost = 0 and push a 0 into
/// the pool's recent-cost reserve -- the zero-accounting failure mode that makes
/// the whole local-ledger scheme fail. A genuinely
/// tiny request still carries at least one non-zero counter, so only the
/// all-zero frame is demoted.
pub fn usage_from_json(v: &Value) -> Option<TokenUsage> {
    let u = v
        .get("usage")
        .filter(|x| x.is_object())
        .or_else(|| v.get("message").and_then(|m| m.get("usage")))
        .filter(|x| x.is_object())?;

    let mut usage = TokenUsage::default();
    let mut any = false;

    // DeepSeek splits the prompt into cache hits and misses instead of using
    // prompt_tokens_details. Both are read so a frame carrying only that pair
    // still yields a prompt count rather than a zero.
    let cache_hit = u.get("prompt_cache_hit_tokens").and_then(Value::as_u64);
    let cache_miss = u.get("prompt_cache_miss_tokens").and_then(Value::as_u64);

    // OpenAI names first; Anthropic names as the fallback.
    if let Some(n) = u.get("prompt_tokens").and_then(Value::as_u64) {
        usage.prompt_tokens = n;
        any = true;
    } else if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
        usage.prompt_tokens = n;
        any = true;
    } else if cache_hit.is_some() || cache_miss.is_some() {
        usage.prompt_tokens = cache_hit
            .unwrap_or(0)
            .saturating_add(cache_miss.unwrap_or(0));
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
    } else if let Some(n) = cache_hit {
        // DeepSeek's hit count IS the cached portion of prompt_tokens, the same
        // quantity OpenAI nests under prompt_tokens_details.
        usage.cached_tokens = n;
        any = true;
    }

    // Reasoning is nested under completion_tokens_details by OpenAI and
    // DeepSeek, and reported flat by some OpenAI-compatible providers.
    if let Some(n) = u
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| u.get("reasoning_tokens").and_then(Value::as_u64))
    {
        usage.reasoning_tokens = n;
        any = true;
    }

    if !any {
        return None;
    }
    usage.measured = usage.prompt_tokens > 0
        || usage.completion_tokens > 0
        || usage.cached_tokens > 0
        || usage.reasoning_tokens > 0;
    Some(usage)
}

/// Number of visible content characters carried by one frame, if any.
///
/// Role-only deltas, empty deltas and structural frames deliberately return
/// None: TTFT is measured to the first actual content token, so a frame that
/// carries no token must not stamp it.
///
/// Tool-call deltas ARE content. A stream that answers with a tool call carries
/// no delta.content at all -- the payload arrives as
/// delta.tool_calls[].function.name and then as arguments increments, or in the
/// Anthropic dialect as a tool_use content_block plus input_json_delta
/// fragments -- so reading only content left every tool-calling request with a
/// NULL TTFT and, when the upstream sent no usage frame, a completion estimate
/// of content_chars / 4 = 0. That is the under-estimating direction: the hard
/// gate then prices a tool payload of several hundred tokens as free.
fn content_len(v: &Value) -> Option<u64> {
    // Anthropic: content_block_delta / content_block_start, and the tool-use
    // arguments that arrive as input_json_delta fragments.
    if let Some(t) = v
        .get("delta")
        .and_then(|d| d.get("text"))
        .and_then(Value::as_str)
    {
        return non_empty_len(t);
    }
    if let Some(t) = v
        .get("delta")
        .and_then(|d| d.get("partial_json"))
        .and_then(Value::as_str)
    {
        return non_empty_len(t);
    }
    if let Some(t) = v
        .get("content_block")
        .and_then(|b| b.get("text"))
        .and_then(Value::as_str)
    {
        return non_empty_len(t);
    }
    // Anthropic tool call: the opening block carries the function name, and a
    // tool call with no arguments has no delta frame after it at all.
    if let Some(n) = v
        .get("content_block")
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .and_then(|b| b.get("name"))
        .and_then(Value::as_str)
        .and_then(non_empty_len)
    {
        return Some(n);
    }
    // OpenAI: choices[].delta.content (or the non-streaming message.content).
    let choices = v.get("choices")?.as_array()?;
    for c in choices {
        if let Some(t) = c.get("delta").and_then(|d| d.get("content"))
            && let Some(n) = text_len(t)
        {
            return Some(n);
        }
        if let Some(n) = tool_calls_len_of(c.get("delta")) {
            return Some(n);
        }
        if let Some(t) = c.get("message").and_then(|m| m.get("content"))
            && let Some(n) = text_len(t)
        {
            return Some(n);
        }
        if let Some(n) = tool_calls_len_of(c.get("message")) {
            return Some(n);
        }
        if let Some(t) = c.get("text")
            && let Some(n) = text_len(t)
        {
            return Some(n);
        }
    }
    None
}

/// Content characters in the tool_calls array of a delta or a message.
fn tool_calls_len_of(container: Option<&Value>) -> Option<u64> {
    tool_calls_len(container?.get("tool_calls")?)
}

/// Content characters in a tool_calls array.
///
/// The function NAME counts alongside the arguments: it is generated output the
/// provider bills for, and it is what arrives on the first tool-call frame,
/// where arguments is still the empty string -- the frame that should define
/// TTFT. A provider that resends the name with every argument chunk over-counts
/// slightly, which is the safe direction for a completion estimate.
fn tool_calls_len(tool_calls: &Value) -> Option<u64> {
    let mut chars = 0u64;
    for call in tool_calls.as_array()? {
        let function = call.get("function");
        for part in [
            function.and_then(|f| f.get("name")),
            function.and_then(|f| f.get("arguments")),
        ] {
            if let Some(s) = part.and_then(Value::as_str) {
                chars = chars.saturating_add(s.chars().count() as u64);
            }
        }
    }
    (chars > 0).then_some(chars)
}

/// Content characters in a content field, which may be a bare string or an
/// array of typed parts.
fn text_len(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => non_empty_len(s),
        Value::Array(parts) => parts
            .iter()
            .find_map(|p| p.get("text").and_then(Value::as_str))
            .and_then(non_empty_len),
        _ => None,
    }
}

/// Character count of a non-empty string; None for the empty string.
fn non_empty_len(s: &str) -> Option<u64> {
    let n = s.chars().count() as u64;
    (n > 0).then_some(n)
}

/// True when the frame signals the end of generation.
fn stream_finished(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) == Some("message_stop") {
        return true;
    }
    if v.get("delta")
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
        assert!(
            !second.first_content,
            "only the first content chunk reports it"
        );

        let r = m.finish();
        assert_eq!(r.content_chunks, 2);
        assert_eq!(m.content_chars(), 11);
    }

    /// A tool-calling stream carries no delta.content at all, so before this the
    /// first tool-call frame did not stamp TTFT and a stream that lost its usage
    /// frame estimated completion from 0 characters -- pricing a real tool
    /// payload as free, the under-estimating direction.
    #[test]
    fn tool_call_deltas_start_ttft_and_count_their_characters() {
        let mut m = SseMeter::new();
        // The first tool-call frame names the function; arguments are still empty.
        let first = m.feed_line(
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
        );
        assert!(
            first.first_content,
            "the tool call is the first generated content"
        );
        assert_eq!(m.content_chars(), 11, "the 11 characters of get_weather");

        let args = m.feed_line(
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":\"Paris\"}"}}]},"finish_reason":null}]}"#,
        );
        assert!(
            !args.first_content,
            "only the first content frame reports it"
        );

        // An empty array and a structural frame are not content.
        let empty = m.feed_line(
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[]},"finish_reason":null}]}"#,
        );
        assert!(!empty.first_content);

        let r = m.finish();
        // 11 (name) + 16 ({"city":"Paris"}) characters.
        assert_eq!(r.content_chunks, 2);
        assert_eq!(m.content_chars(), 27);

        // The non-streaming shape benefits the same way: a tool-call body with
        // no usage frame must not read as zero content characters either.
        let mut json = StreamMeter::json();
        json.push(
            br#"{"id":"x","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        );
        let r = json.finish();
        assert!(!r.usage.measured);
        assert_eq!(json.content_chars(), 27);
    }

    /// The same hole in the Anthropic dialect: a tool_use block announces the
    /// function on content_block_start and streams its arguments as
    /// input_json_delta fragments, and neither is delta.text.
    #[test]
    fn anthropic_tool_use_frames_count_as_content() {
        let mut m = SseMeter::new();
        let open = m.feed_line(
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
        );
        assert!(
            open.first_content,
            "the tool_use block is generated content"
        );
        assert_eq!(m.content_chars(), 11, "the 11 characters of get_weather");

        m.feed_line(
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
        );
        assert_eq!(m.content_chars(), 19, "plus the 8-character city fragment");

        // An empty fragment is not content.
        let empty = m.feed_line(
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}"#,
        );
        assert!(!empty.first_content);
        assert_eq!(m.finish().content_chunks, 2);
    }

    /// Regression, seen against a live provider: HTTP 200 with the failure
    /// reported inside the stream. The bytes must still be relayed, but the
    /// request must not be counted as a success.
    #[test]
    fn an_error_frame_inside_a_successful_stream_is_detected() {
        let mut m = SseMeter::new();
        let obs = m.feed_line(
            r#"data: {"error":{"message":"A server error occurred.","type":"server_error","code":"internal_error"}}"#,
        );
        assert!(obs.error, "the error frame must be reported");
        assert_eq!(m.finish().error.as_deref(), Some("internal_error"));

        // A normal stream must not be marked as errored.
        let mut ok = SseMeter::new();
        ok.feed_line(
            r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
        );
        ok.feed_line("data: [DONE]");
        assert_eq!(ok.finish().error, None);
        assert!(ok.finish().saw_done);
    }

    #[test]
    fn done_marker_is_recognized() {
        let mut m = SseMeter::new();
        let obs = m.feed_line("data: [DONE]");
        assert!(obs.done);
        assert!(m.finish().saw_done);
    }

    /// The proxy marks a stream truncated when it never saw a terminal marker,
    /// and this accessor is what it reads. Both ways a stream can end must be
    /// visible through it, and a stream cut off mid-answer must stay false.
    #[test]
    fn saw_done_is_readable_through_an_accessor() {
        let mut cut = SseMeter::new();
        cut.feed_line(
            r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
        );
        assert!(
            !cut.finish().saw_done(),
            "a stream cut off mid-answer never saw a terminal marker"
        );

        let mut done = SseMeter::new();
        done.feed_line("data: [DONE]");
        assert!(done.finish().saw_done());

        let mut finished = SseMeter::new();
        finished.feed_line(r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#);
        assert!(
            finished.finish().saw_done(),
            "finish_reason also ends the stream"
        );
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
        assert_eq!(
            r.usage.prompt_tokens, 25,
            "message_start keeps the prompt count"
        );
        assert_eq!(
            r.usage.completion_tokens, 15,
            "message_delta carries the output count"
        );
        assert_eq!(r.usage.cached_tokens, 10);
        assert!(r.usage.measured);
    }

    /// DeepSeek -- one of the providers this gateway routes -- reports prompt
    /// cache hits as prompt_cache_hit_tokens instead of the OpenAI
    /// prompt_tokens_details.cached_tokens. Reading only the OpenAI field made
    /// every DeepSeek request meter cached = 0, so the cache-hit metric was
    /// structurally zero and the discounted tokens were billed at the full
    /// uncached rate.
    #[test]
    fn deepseek_prompt_cache_hit_tokens_are_metered_as_cached() {
        let mut m = SseMeter::new();
        let obs = m.feed_line(
            r#"data: {"id":"c1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_cache_hit_tokens":64,"prompt_cache_miss_tokens":36,"completion_tokens_details":{"reasoning_tokens":8}}}"#,
        );
        let usage = obs.usage.expect("a DeepSeek usage frame must be parsed");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(
            usage.cached_tokens, 64,
            "prompt_cache_hit_tokens is the cached part of the prompt"
        );
        assert_eq!(usage.reasoning_tokens, 8);
        assert_eq!(usage.cache_hit_ratio(), Some(0.64));
        assert!(usage.measured);

        // A frame carrying only the hit/miss pair still yields a prompt count,
        // and a flat reasoning_tokens field is accepted too.
        let mut bare = SseMeter::new();
        let obs = bare.feed_line(
            r#"data: {"usage":{"prompt_cache_hit_tokens":64,"prompt_cache_miss_tokens":36,"reasoning_tokens":8}}"#,
        );
        let usage = obs.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.cached_tokens, 64);
        assert_eq!(usage.reasoning_tokens, 8);
    }

    /// Providers emit all-zero usage frames as pacing/keepalive noise. Taking
    /// one as a measurement hands the hard gate cost = 0 and pushes a 0 into the
    /// pool's recent-cost reserve, which pulls the safety margin down -- exactly
    /// the zero-accounting failure mode the reserve exists to prevent.
    #[test]
    fn an_all_zero_usage_frame_is_not_a_measurement() {
        let mut m = SseMeter::new();
        let obs = m.feed_line(
            r#"data: {"id":"c1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0,"prompt_cache_hit_tokens":0,"completion_tokens_details":{"reasoning_tokens":0}}}"#,
        );
        let usage = obs
            .usage
            .expect("the frame is still recognised as a usage frame");
        assert!(!usage.measured, "an all-zero frame measured nothing");
        assert!(
            !m.finish().usage.measured,
            "and the stream must stay unmeasured so the conservative estimate takes over"
        );

        // A genuinely tiny request carries at least one non-zero counter and
        // must still count, or every short answer would be estimated instead.
        let mut tiny = SseMeter::new();
        tiny.feed_line(r#"data: {"usage":{"prompt_tokens":3,"completion_tokens":1}}"#);
        assert!(tiny.finish().usage.measured);

        // An all-zero frame must not erase a real reading that preceded it.
        let mut mixed = SseMeter::new();
        mixed.feed_line(r#"data: {"usage":{"prompt_tokens":11,"completion_tokens":7}}"#);
        mixed.feed_line(r#"data: {"usage":{"prompt_tokens":0,"completion_tokens":0}}"#);
        let r = mixed.finish();
        assert!(r.usage.measured);
        assert_eq!(r.usage.prompt_tokens, 11);
        assert_eq!(r.usage.completion_tokens, 7);

        // ...nor suppress one that follows it.
        let mut late = SseMeter::new();
        late.feed_line(r#"data: {"usage":{"prompt_tokens":0,"completion_tokens":0}}"#);
        late.feed_line(r#"data: {"usage":{"prompt_tokens":11,"completion_tokens":7}}"#);
        let r = late.finish();
        assert!(r.usage.measured);
        assert_eq!(r.usage.prompt_tokens, 11);
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
