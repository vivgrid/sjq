//! Turns OpenAI-compatible `/v1/chat/completions` SSE lines into one compact line per event,
//! keeping only what a developer needs to see while a stream is flowing.

use serde_json::{Map, Value};

pub const DIM: &str = "2";
const RED: &str = "31";
const GREEN: &str = "32";
const YELLOW: &str = "33";
const BLUE: &str = "34";
const MAGENTA: &str = "35";
const CYAN: &str = "36";

/// Top-level chunk fields rendered on the event line; every other field is stream metadata.
const BODY_FIELDS: [&str; 3] = ["choices", "usage", "error"];

/// Wraps `s` in an ANSI color when `on` is set.
pub fn paint(on: bool, code: &str, s: &str) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub struct Renderer {
    color: bool,
    /// Metadata (id, model, ...) of the stream in progress, so only changes get printed.
    meta: Map<String, Value>,
    timing: Timing,
}

/// When things happened, in seconds since the request started.
#[derive(Default)]
struct Timing {
    first_token: Option<f64>,
    first_content: Option<f64>,
    last_line: Option<f64>,
    completion_tokens: Option<u64>,
}

/// Connection timings from curl's `-w`, in seconds since curl started; 0 means it didn't happen.
pub struct CurlTiming {
    pub connect: f64,
    pub tls: f64,
    pub ttfb: f64,
}

impl Renderer {
    pub fn new(color: bool) -> Self {
        Self {
            color,
            meta: Map::new(),
            timing: Timing::default(),
        }
    }

    /// Renders one input line (without its line terminator), received `now` seconds after the
    /// request started, into zero or more output lines.
    pub fn line(&mut self, line: &str, now: f64) -> Vec<String> {
        if line.trim().is_empty() {
            return Vec::new(); // SSE event separators carry no information
        }
        self.timing.last_line = Some(now);
        let Some(payload) = line.strip_prefix("data:") else {
            return vec![line.to_owned()]; // event:, comments, curl -w output, ...
        };
        let payload = payload.strip_prefix(' ').unwrap_or(payload);
        match serde_json::from_str(payload) {
            Ok(Value::Object(chunk)) if BODY_FIELDS.iter().any(|k| chunk.contains_key(*k)) => {
                self.observe(&chunk, now);
                self.chunk(&chunk)
            }
            _ => vec![self.data(payload)], // [DONE], or a payload that isn't a chat chunk
        }
    }

    /// The `time:` line: curl's connection timings (if any), then what sjq saw arrive itself.
    pub fn summary(&self, curl: Option<&CurlTiming>) -> Option<String> {
        let t = &self.timing;
        let total = t.last_line?;
        let mut parts = Vec::new();
        if let Some(c) = curl {
            for (name, secs) in [("connect", c.connect), ("tls", c.tls), ("ttfb", c.ttfb)] {
                if secs > 0.0 {
                    parts.push(format!("{name}={}", duration(secs)));
                }
            }
        }
        if let Some(ttft) = t.first_token {
            parts.push(format!("ttft={}", duration(ttft)));
        }
        if let Some(first) = t.first_content.filter(|&c| Some(c) != t.first_token) {
            parts.push(format!("first_content={}", duration(first)));
        }
        parts.push(format!("total={}", duration(total)));
        if let (Some(n), Some(ttft)) = (t.completion_tokens, t.first_token)
            && total > ttft
        {
            parts.push(format!("speed={:.1}tok/s", n as f64 / (total - ttft)));
        }
        Some(self.paint(BLUE, &format!("time: {}", parts.join(" "))))
    }

    /// Records when the first generated token (reasoning, content, tool call, ... but not the
    /// role) and the first answer content arrived, and the final completion token count.
    fn observe(&mut self, chunk: &Map<String, Value>, now: f64) {
        let t = &mut self.timing;
        let deltas = chunk
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("delta").or_else(|| c.get("message"))?.as_object());
        for (k, v) in deltas.flatten() {
            let produced = match v {
                Value::Null => false,
                Value::String(s) => !s.is_empty(),
                _ => true,
            };
            if produced && k != "role" {
                t.first_token.get_or_insert(now);
                if k == "content" {
                    t.first_content.get_or_insert(now);
                }
            }
        }
        let tokens = chunk.get("usage").and_then(|u| u.get("completion_tokens"));
        if let Some(n) = tokens.and_then(Value::as_u64) {
            t.completion_tokens = Some(n);
        }
    }

    fn chunk(&mut self, chunk: &Map<String, Value>) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(self.meta(chunk));

        let mut parts = Vec::new();
        for choice in chunk
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            parts.extend(self.choice(choice));
        }
        if let Some(usage) = chunk.get("usage").and_then(|u| prune(u, 0)) {
            parts.push(self.paint(GREEN, &format!("usage: {usage}")));
        }
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            parts.push(self.paint(RED, &format!("error: {error}")));
        }
        if parts.is_empty() {
            parts.push(self.paint(DIM, "(empty)"));
        }
        out.push(self.data(&parts.join(", ")));
        out
    }

    /// A `meta:` line with the top-level fields that are new or changed since the previous chunk.
    fn meta(&mut self, chunk: &Map<String, Value>) -> Option<String> {
        let id = |m: &Map<String, Value>| m.get("id").filter(|v| !v.is_null()).cloned();
        if id(chunk) != id(&self.meta) {
            self.meta.clear(); // a new stream: show everything again
        }
        let mut changed = Vec::new();
        for (k, v) in chunk {
            if BODY_FIELDS.contains(&k.as_str()) || is_noise(k, v) {
                continue;
            }
            let seen = self.meta.get(k);
            // Some servers stamp every chunk; one `created` per stream is enough.
            if seen == Some(v) || (k == "created" && seen.is_some()) {
                continue;
            }
            changed.push(format!("{k}={}", bare(v)));
            self.meta.insert(k.clone(), v.clone());
        }
        (!changed.is_empty()).then(|| self.paint(BLUE, &format!("meta: {}", changed.join(" "))))
    }

    fn choice(&self, choice: &Value) -> Vec<String> {
        let Some(choice) = choice.as_object() else {
            return vec![choice.to_string()];
        };
        let mut parts = Vec::new();
        for key in ["delta", "message"] {
            if let Some(delta) = choice.get(key).and_then(Value::as_object) {
                parts.extend(self.delta(delta));
            }
        }
        for (k, v) in choice {
            if v.is_null() || matches!(k.as_str(), "index" | "delta" | "message") {
                continue;
            }
            let field = format!("{k}={}", bare(v));
            parts.push(if k == "finish_reason" {
                self.paint(YELLOW, &field)
            } else {
                field
            });
        }
        if let Some(i) = choice
            .get("index")
            .and_then(Value::as_u64)
            .filter(|&i| i > 0)
            && let Some(first) = parts.first_mut()
        {
            *first = format!("[{i}] {first}");
        }
        parts
    }

    fn delta(&self, delta: &Map<String, Value>) -> Vec<String> {
        let mut parts = Vec::new();
        for (k, v) in delta {
            if v.is_null() {
                continue;
            }
            parts.push(match k.as_str() {
                "content" => text(v),
                "role" => self.paint(CYAN, &format!("role={}", bare(v))),
                "refusal" => self.paint(RED, &format!("refusal={}", text(v))),
                "tool_calls" => self.paint(MAGENTA, &tool_calls(v)),
                // reasoning_content, reasoning, reasoning_details, ... keep the provider's name
                k if k.starts_with("reasoning") => self.paint(DIM, &format!("{k}={}", text(v))),
                k => format!("{k}={}", bare(v)),
            });
        }
        parts
    }

    fn data(&self, body: &str) -> String {
        format!("{} {body}", self.paint(DIM, "data:"))
    }

    fn paint(&self, code: &str, s: &str) -> String {
        paint(self.color, code, s)
    }
}

/// Fields that repeat on every chunk without telling the reader anything.
fn is_noise(key: &str, v: &Value) -> bool {
    v.is_null() || key == "obfuscation" || (key == "object" && v == "chat.completion.chunk")
}

/// `tool_calls[0] id=call_1 name=get_weather args='{"city":'`; `type=function` is implied.
/// A missing `index` shows as `[?]` since it breaks most client SDKs.
fn tool_calls(v: &Value) -> String {
    let Some(calls) = v.as_array() else {
        return format!("tool_calls={}", bare(v));
    };
    let render = |call: &Value| {
        let Some(call) = call.as_object() else {
            return format!("tool_calls[?]={}", bare(call));
        };
        let index = call
            .get("index")
            .filter(|i| !i.is_null())
            .map_or("?".to_owned(), bare);
        let mut s = format!("tool_calls[{index}]");
        for (k, v) in call {
            match (k.as_str(), v) {
                (_, Value::Null) | ("index", _) => {}
                ("type", v) if v == "function" => {}
                ("function", Value::Object(f)) => {
                    for (fk, fv) in f {
                        match (fk.as_str(), fv) {
                            (_, Value::Null) => {}
                            ("arguments", Value::String(a)) if a.is_empty() => {}
                            ("arguments", a) => s += &format!(" args={}", text(a)),
                            (fk, fv) => s += &format!(" {fk}={}", bare(fv)),
                        }
                    }
                }
                (k, v) => s += &format!(" {k}={}", bare(v)),
            }
        }
        s
    };
    calls.iter().map(render).collect::<Vec<_>>().join(", ")
}

/// Drops nulls, plus zero counters and empty objects below the top level of `usage`,
/// e.g. `"prompt_tokens_details":{"audio_tokens":0,"cached_tokens":0}` disappears entirely.
fn prune(v: &Value, depth: usize) -> Option<Value> {
    match v {
        Value::Null => None,
        Value::Number(n) if depth > 1 && n.as_f64() == Some(0.0) => None,
        Value::Object(m) => {
            let m: Map<String, Value> = m
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), prune(v, depth + 1)?)))
                .collect();
            (depth == 0 || !m.is_empty()).then_some(Value::Object(m))
        }
        v => Some(v.clone()),
    }
}

/// `0.3ms`, `412.0ms`, `2.210s`
fn duration(secs: f64) -> String {
    if secs < 1.0 {
        format!("{:.1}ms", secs * 1000.0)
    } else {
        format!("{secs:.3}s")
    }
}

/// Generated text: always quoted, so whitespace and empty deltas stay visible.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => quote(s),
        v => v.to_string(),
    }
}

/// Identifiers print as-is (`stop`, `viv-fast-2609`); other strings are quoted, the rest is JSON.
fn bare(v: &Value) -> String {
    match v {
        Value::String(s)
            if !s.is_empty()
                && !s.chars().any(|c| {
                    c.is_whitespace() || c.is_control() || matches!(c, '\'' | '"' | '\\' | ',')
                }) =>
        {
            s.clone()
        }
        Value::String(s) => quote(s),
        v => v.to_string(),
    }
}

/// Single-quotes `s`, escaping newlines and control characters so each event stays on one line
/// and model output can't inject terminal escape sequences.
fn quote(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('\'');
    for c in s.chars() {
        match c {
            '\\' => q.push_str("\\\\"),
            '\'' => q.push_str("\\'"),
            '\n' => q.push_str("\\n"),
            '\r' => q.push_str("\\r"),
            '\t' => q.push_str("\\t"),
            c if c.is_control() => q.extend(c.escape_unicode()),
            c => q.push(c),
        }
    }
    q.push('\'');
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTENT: &str = r#"data: {"id":"46d5bd6d-02e1-4551-b8f3-861e65f316a4","created":1792034616,"model":"viv-fast-2609","object":"chat.completion.chunk","system_fingerprint":null,"obfuscation":null,"choices":[{"delta":{"content":" in","refusal":null},"index":0,"logprobs":null}]}"#;
    const FINISH: &str = r#"data: {"id":"46d5bd6d-02e1-4551-b8f3-861e65f316a4","created":1792034616,"model":"viv-fast-2609","object":"chat.completion.chunk","system_fingerprint":null,"obfuscation":null,"choices":[{"delta":{"refusal":null},"finish_reason":"stop","index":0,"logprobs":null}],"usage":{"prompt_tokens":158,"completion_tokens":139,"total_tokens":297,"prompt_tokens_details":{"audio_tokens":0,"cached_tokens":0},"completion_tokens_details":{"accepted_prediction_tokens":0,"audio_tokens":0,"reasoning_tokens":73,"rejected_prediction_tokens":0}}}"#;

    fn render(lines: &[&str]) -> Vec<String> {
        let mut r = Renderer::new(false);
        lines
            .iter()
            .flat_map(|l| r.line(l, 0.0))
            .collect::<Vec<_>>()
    }

    #[test]
    fn content_delta() {
        assert_eq!(
            render(&[CONTENT]),
            [
                "meta: id=46d5bd6d-02e1-4551-b8f3-861e65f316a4 created=1792034616 model=viv-fast-2609",
                "data: ' in'",
            ]
        );
    }

    #[test]
    fn finish_with_usage() {
        let out = render(&[CONTENT, FINISH]);
        assert_eq!(out.len(), 3, "meta is printed once per stream: {out:#?}");
        assert_eq!(
            out[2],
            r#"data: finish_reason=stop, usage: {"prompt_tokens":158,"completion_tokens":139,"total_tokens":297,"completion_tokens_details":{"reasoning_tokens":73}}"#
        );
    }

    #[test]
    fn top_level_usage_zeros_are_kept() {
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":0,"total_tokens":5}}"#;
        assert_eq!(
            render(&[line]),
            [r#"data: usage: {"prompt_tokens":5,"completion_tokens":0,"total_tokens":5}"#]
        );
    }

    #[test]
    fn role_reasoning_and_escapes() {
        let lines = [
            r#"data: {"choices":[{"delta":{"role":"assistant","reasoning_content":"it's","refusal":null}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"\n\tC:\\ \u001b[2J"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":""}}]}"#,
            r#"data: {"choices":[{"delta":{"refusal":null}}]}"#,
        ];
        assert_eq!(
            render(&lines),
            [
                r"data: role=assistant, reasoning_content='it\'s'",
                r"data: '\n\tC:\\ \u{1b}[2J'",
                "data: ''",
                "data: (empty)",
            ]
        );
    }

    #[test]
    fn tool_calls_stream() {
        let lines = [
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"1}"}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        assert_eq!(
            render(&lines),
            [
                "data: tool_calls[0] id=call_1 name=get_weather",
                r#"data: tool_calls[0] args='{"city":'"#,
                "data: tool_calls[?] args='1}'",
                "data: finish_reason=tool_calls",
            ]
        );
    }

    #[test]
    fn unknown_fields_are_surfaced() {
        let lines = [
            r#"data: {"id":"a","provider":"Acme","choices":[{"index":1,"delta":{"content":"x","audio":{"id":"au"}},"stop_reason":128009,"logprobs":null}]}"#,
            r#"data: {"id":"a","provider":"Acme","choices":[{"index":0,"delta":{"content":"y"}}]}"#,
            r#"data: {"id":"a","provider":"Other","choices":[{"index":0,"delta":{"content":"z"}}]}"#,
        ];
        assert_eq!(
            render(&lines),
            [
                "meta: id=a provider=Acme",
                r#"data: [1] 'x', audio={"id":"au"}, stop_reason=128009"#,
                "data: 'y'",
                "meta: provider=Other",
                "data: 'z'",
            ]
        );
    }

    #[test]
    fn new_stream_repeats_meta() {
        let next = CONTENT
            .replace("46d5bd6d", "99999999")
            .replace("1792034616", "1792034700");
        let out = render(&[CONTENT, CONTENT, &next]);
        assert_eq!(
            out[3],
            "meta: id=99999999-02e1-4551-b8f3-861e65f316a4 created=1792034700 model=viv-fast-2609"
        );
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn errors() {
        assert_eq!(
            render(&[r#"data: {"error":{"message":"rate limited","code":429}}"#]),
            [r#"data: error: {"message":"rate limited","code":429}"#]
        );
    }

    #[test]
    fn passthrough() {
        let lines = [
            "event: message",
            "",
            "\r",
            ": ping",
            "data: [DONE]",
            "data:[DONE]",
            r#"data: {"type":"x"}"#,
            "connect=0.1s",
        ];
        assert_eq!(
            render(&lines),
            [
                "event: message",
                ": ping",
                "data: [DONE]",
                "data: [DONE]",
                r#"data: {"type":"x"}"#,
                "connect=0.1s"
            ]
        );
    }

    #[test]
    fn timing_summary() {
        let mut r = Renderer::new(false);
        let stream = [
            (
                0.1,
                r#"data: {"choices":[{"delta":{"role":"assistant","content":"","refusal":null}}]}"#,
            ),
            (
                0.4,
                r#"data: {"choices":[{"delta":{"reasoning_content":"hm"}}]}"#,
            ),
            (1.6, r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#),
            (
                2.2,
                r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"completion_tokens":36}}"#,
            ),
            (2.2, ""),
            (2.3, "data: [DONE]"),
        ];
        for (now, line) in stream {
            r.line(line, now);
        }
        assert_eq!(
            r.summary(None).unwrap(),
            "time: ttft=400.0ms first_content=1.600s total=2.300s speed=18.9tok/s"
        );
        let curl = CurlTiming {
            connect: 0.000328,
            tls: 0.0,
            ttfb: 0.35,
        };
        assert_eq!(
            r.summary(Some(&curl)).unwrap(),
            "time: connect=0.3ms ttfb=350.0ms ttft=400.0ms first_content=1.600s total=2.300s speed=18.9tok/s"
        );
    }

    #[test]
    fn timing_without_tokens() {
        let mut r = Renderer::new(false);
        assert_eq!(r.summary(None), None, "nothing received yet");
        r.line(r#"{"error":{"message":"bad key"}}"#, 0.05);
        assert_eq!(r.summary(None).unwrap(), "time: total=50.0ms");
    }
}
