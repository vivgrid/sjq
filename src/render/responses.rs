//! OpenAI Responses API (`/v1/responses`) streams. Instead of chunks, these send typed events
//! such as `{"type":"response.output_text.delta","item_id":"msg_1","output_index":1,
//! "content_index":0,"delta":"Hi","sequence_number":12}`, and the `.done` events after the deltas
//! repeat the whole text once per level (text, part, item, response).

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::{
    CYAN, DIM, GREEN, MAGENTA, RED, Renderer, YELLOW, bare, is_empty, is_noise, produced, prune,
    text,
};

/// Fields of the `response` object that go on the event line; the rest is metadata.
const RESPONSE_BODY: [&str; 5] = ["status", "output", "usage", "error", "incomplete_details"];

/// Fields saying which output item and part an event is about. Nearly every event repeats them,
/// so they're only shown when they change.
const CONTEXT: [&str; 4] = ["output_index", "item_id", "content_index", "summary_index"];

/// Fields of `.done` events, parts and items that repeat the whole text their deltas streamed.
const FULL_TEXT: [&str; 5] = ["text", "refusal", "arguments", "input", "code"];

pub(super) fn is_event(ty: &str) -> bool {
    ty.starts_with("response.") || ty == "error"
}

/// What earlier events of the response in progress showed.
#[derive(Default)]
pub(super) struct State {
    /// The last value of each `CONTEXT` field.
    context: Map<String, Value>,
    /// How many output items were announced, which is the `output_index` the next one gets.
    items: u64,
    /// Output items as `response.output_item.added` announced them, by id.
    announced: HashMap<String, Value>,
    /// Output items as `response.output_item.done` finished them, which `response.completed`
    /// repeats.
    finished: Vec<Value>,
    /// The text deltas streamed so far, by `key()`.
    streamed: HashMap<String, String>,
    sequence: Option<u64>,
}

/// Where streamed text ends up: an item's field and, for a list of parts, the part's index.
fn key(item_id: &str, field: &str, index: u64) -> String {
    format!("{item_id}/{field}/{index}")
}

/// The item field holding the text that `response.<base>.*` events are about, and the event
/// field with the index of the part within it.
fn slot(base: &str) -> (&'static str, Option<&'static str>) {
    match base {
        "reasoning_summary_text" | "reasoning_summary_part" => ("summary", Some("summary_index")),
        "function_call_arguments" | "mcp_call_arguments" => ("arguments", None),
        "custom_tool_call_input" => ("input", None),
        "code_interpreter_call_code" => ("code", None),
        // output_text, refusal, reasoning_text, content_part
        _ => ("content", Some("content_index")),
    }
}

impl Renderer {
    /// Renders a Responses API event of type `ty` (received `now`), whose name is printed
    /// without the `response.` prefix.
    pub(super) fn response_event(
        &mut self,
        ev: &Map<String, Value>,
        ty: &str,
        now: f64,
    ) -> Vec<String> {
        let name = ty.strip_prefix("response.").unwrap_or(ty);
        if name == "created" {
            self.responses = State::default();
        }
        self.observe_event(ev, name, now);
        let (base, phase) = name.rsplit_once('.').unwrap_or((name, ""));
        let key = {
            let (field, index) = slot(base);
            let index = index.and_then(|i| ev.get(i)?.as_u64()).unwrap_or(0);
            let item_id = ev.get("item_id").and_then(Value::as_str);
            item_id.map(|id| key(id, field, index))
        };

        let mut out = Vec::new();
        let mut head = Vec::new();
        let mut tail = Vec::new();
        let delta = ev.get("delta").filter(|_| phase == "delta");
        match delta {
            Some(delta) => {
                if let (Some(key), Some(s)) = (&key, delta.as_str()) {
                    let streamed = self.responses.streamed.entry(key.clone()).or_default();
                    streamed.push_str(s);
                }
                head.push(self.delta_text(base, delta));
            }
            None => head.push(self.paint(if name == "error" { RED } else { CYAN }, name)),
        }
        let context = self.context(ev, name);
        for (k, v) in ev {
            match k.as_str() {
                "type" | "sequence_number" => {}
                "delta" if delta.is_some() => {}
                k if CONTEXT.contains(&k) => {}
                "response" if v.is_object() => {
                    let response = v.as_object().expect("checked by the guard");
                    out.extend(self.meta(response, &RESPONSE_BODY));
                    self.response(response, name, &mut head, &mut tail);
                }
                "item" => {
                    head.extend(self.item(v, name));
                    if name == "output_item.done" {
                        self.responses.finished.push(v.clone());
                    }
                }
                "part" => head.extend(self.part(v, key.as_deref())),
                k if phase == "done" && FULL_TEXT.contains(&k) => {
                    head.extend(self.full_text(k, v, key.as_deref()));
                }
                k if is_noise(k, v) => {}
                k => head.push(format!("{k}={}", bare(v))),
            }
        }
        head.extend(context);
        head.extend(self.sequence(ev));
        let line = [head.join(" ")].into_iter().chain(tail);
        out.push(self.data(&line.collect::<Vec<_>>().join(", ")));
        out
    }

    /// Records when the first generated token and the first answer text arrived, and the final
    /// output token count.
    fn observe_event(&mut self, ev: &Map<String, Value>, name: &str, now: f64) {
        let t = &mut self.timing;
        if name.ends_with(".delta") && ev.get("delta").is_some_and(produced) {
            t.first_token.get_or_insert(now);
            if name == "output_text.delta" {
                t.first_content.get_or_insert(now);
            }
        }
        let usage = ev.get("response").and_then(|r| r.get("usage"));
        if let Some(n) = usage.and_then(|u| u.get("output_tokens")?.as_u64()) {
            t.completion_tokens = Some(n);
        }
    }

    /// `'Hi'` for answer text; `reasoning_summary_text=' The'` and so on for everything else,
    /// styled like the chat completion fields.
    fn delta_text(&self, base: &str, delta: &Value) -> String {
        if base == "output_text" {
            return text(delta);
        }
        let field = format!("{base}={}", text(delta));
        match base {
            b if b.starts_with("reasoning") => self.paint(DIM, &field),
            "refusal" => self.paint(RED, &field),
            b if b.contains("call") => self.paint(MAGENTA, &field),
            _ => field,
        }
    }

    /// The `CONTEXT` fields of `ev` that changed since the previous event that had them.
    fn context(&mut self, ev: &Map<String, Value>, name: &str) -> Vec<String> {
        let state = &mut self.responses;
        if name == "output_item.added" {
            // A new item comes next, and its parts are counted from 0 again.
            state
                .context
                .insert("output_index".into(), state.items.into());
            state.context.remove("content_index");
            state.context.remove("summary_index");
            let index = ev.get("output_index").and_then(Value::as_u64);
            state.items = state.items.max(index.unwrap_or(state.items) + 1);
        }
        let mut changed = Vec::new();
        for k in CONTEXT {
            let Some(v) = ev.get(k).filter(|v| !v.is_null()) else {
                continue;
            };
            let last = state.context.get(k).cloned();
            if last
                .or_else(|| k.ends_with("_index").then(|| 0.into()))
                .as_ref()
                != Some(v)
            {
                changed.push(format!("{k}={}", bare(v)));
                state.context.insert(k.to_owned(), v.clone());
            }
        }
        changed
    }

    /// `sequence_number=N` when events went missing or arrived out of order.
    fn sequence(&mut self, ev: &Map<String, Value>) -> Option<String> {
        let n = ev.get("sequence_number")?.as_u64()?;
        let expected = self.responses.sequence.map(|last| last + 1);
        self.responses.sequence = Some(n);
        (expected.is_some_and(|e| e != n))
            .then(|| self.paint(YELLOW, &format!("sequence_number={n}")))
    }

    /// The `response` object of `created`, `completed`, `failed`, ... events, minus its metadata
    /// and the output items that were already shown as they finished.
    fn response(
        &self,
        response: &Map<String, Value>,
        name: &str,
        head: &mut Vec<String>,
        tail: &mut Vec<String>,
    ) {
        let status = response.get("status").filter(|s| !s.is_null());
        if let Some(status) = status.filter(|s| s.as_str() != Some(name)) {
            head.push(self.paint(YELLOW, &format!("status={}", bare(status))));
        }
        if let Some(details) = response.get("incomplete_details").filter(|v| !is_empty(v)) {
            head.push(self.paint(YELLOW, &format!("incomplete_details={details}")));
        }
        if let Some(output) = response.get("output").and_then(Value::as_array)
            && output
                .iter()
                .any(|item| !self.responses.finished.contains(item))
        {
            head.push(format!("output={}", Value::Array(output.clone())));
        }
        if let Some(usage) = response.get("usage").and_then(|u| prune(u, 0)) {
            tail.push(self.paint(GREEN, &format!("usage: {usage}")));
        }
        if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
            tail.push(self.paint(RED, &format!("error: {error}")));
        }
    }

    /// `message role=assistant status=in_progress id=msg_1`: an output item without what earlier
    /// events showed, such as fields it was announced with or the text its deltas streamed.
    fn item(&mut self, item: &Value, name: &str) -> Vec<String> {
        let Some(fields) = item.as_object() else {
            return vec![bare(item)];
        };
        let id = fields.get("id").and_then(Value::as_str).unwrap_or_default();
        let announced = self.responses.announced.get(id).cloned();
        let mut parts: Vec<String> = fields.get("type").map(bare).into_iter().collect();
        for (k, v) in fields {
            match k.as_str() {
                "type" => {}
                "id" => {
                    if self.responses.context.get("item_id") != Some(v) {
                        parts.push(format!("id={}", bare(v)));
                        self.responses.context.insert("item_id".into(), v.clone());
                    }
                }
                k if announced.as_ref().and_then(|a| a.get(k)) == Some(v) => {}
                "content" | "summary" if self.streamed_parts(id, k, v) => {}
                k if FULL_TEXT.contains(&k) => {
                    parts.extend(self.full_text(k, v, Some(&key(id, k, 0))));
                }
                k if is_noise(k, v) => {}
                k => parts.push(format!("{k}={}", bare(v))),
            }
        }
        if name == "output_item.added" {
            self.responses.announced.insert(id.to_owned(), item.clone());
        }
        parts
    }

    /// `output_text`: a content or summary part, with its text only if the deltas didn't
    /// stream exactly that.
    fn part(&self, part: &Value, key: Option<&str>) -> Vec<String> {
        let Some(fields) = part.as_object() else {
            return vec![bare(part)];
        };
        let mut parts: Vec<String> = fields.get("type").map(bare).into_iter().collect();
        for (k, v) in fields {
            match k.as_str() {
                "type" => {}
                k if FULL_TEXT.contains(&k) => parts.extend(self.full_text(k, v, key)),
                k if is_noise(k, v) => {}
                k => parts.push(format!("{k}={}", bare(v))),
            }
        }
        parts
    }

    /// Whether an item's `content` or `summary` parts hold nothing but the text streamed into
    /// them.
    fn streamed_parts(&self, id: &str, field: &str, parts: &Value) -> bool {
        let Some(parts) = parts.as_array() else {
            return false;
        };
        parts.iter().zip(0..).all(|(part, i)| {
            let streamed = self.responses.streamed.get(&key(id, field, i));
            part.as_object().is_some_and(|p| {
                p.iter().all(|(k, v)| match k.as_str() {
                    "type" => true,
                    k if FULL_TEXT.contains(&k) => v.as_str() == streamed.map(String::as_str),
                    k => is_noise(k, v),
                })
            })
        })
    }

    /// A field repeating a whole streamed text: hidden when it matches the deltas, red when it
    /// contradicts them.
    fn full_text(&self, k: &str, v: &Value, key: Option<&str>) -> Option<String> {
        let streamed = key.and_then(|key| self.responses.streamed.get(key));
        let field = format!("{k}={}", text(v));
        match streamed {
            Some(s) if v.as_str() == Some(s) => None,
            Some(_) => Some(self.paint(RED, &field)),
            None if is_empty(v) => None,
            None => Some(field),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(lines: &[&str]) -> Vec<String> {
        let mut r = Renderer::new(false);
        let mut out: Vec<String> = lines.iter().flat_map(|l| r.line(l, 0.0)).collect();
        out.extend(r.finish());
        out
    }

    #[test]
    fn reasoning_then_message() {
        let lines = [
            "event: response.created",
            r#"data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_1","object":"response","created_at":1700000000,"model":"test-model","status":"in_progress","output":[],"usage":null,"error":null,"reasoning":{"effort":"medium"},"service_tier":"auto"}}"#,
            "",
            "event: response.in_progress",
            r#"data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_1","object":"response","created_at":1700000000,"model":"test-model","status":"in_progress","output":[],"usage":null,"error":null,"reasoning":{"effort":"medium"},"service_tier":"auto"}}"#,
            "",
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"status":"in_progress"},"sequence_number":2}"#,
            r#"data: {"type":"response.reasoning_summary_part.added","item_id":"rs_1","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""},"sequence_number":3}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","summary_index":0,"delta":"Easy","item_id":"rs_1","output_index":0,"sequence_number":4}"#,
            r#"data: {"type":"response.reasoning_summary_text.done","summary_index":0,"item_id":"rs_1","output_index":0,"text":"Easy","sequence_number":5}"#,
            r#"data: {"type":"response.reasoning_summary_part.done","item_id":"rs_1","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"Easy"},"sequence_number":6}"#,
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"Easy"}],"status":"completed"},"sequence_number":7}"#,
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"message","role":"assistant","content":[],"status":"in_progress","id":"msg_1"},"sequence_number":8}"#,
            r#"data: {"type":"response.content_part.added","content_index":0,"item_id":"msg_1","output_index":1,"part":{"type":"output_text","text":"","annotations":[]},"sequence_number":9}"#,
            r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"Hi","item_id":"msg_1","output_index":1,"sequence_number":10,"logprobs":[],"obfuscation":"x1"}"#,
            r#"data: {"type":"response.output_text.delta","content_index":0,"delta":" it's","item_id":"msg_1","output_index":1,"sequence_number":11,"logprobs":[]}"#,
            r#"data: {"type":"response.output_text.done","content_index":0,"item_id":"msg_1","output_index":1,"text":"Hi it's","sequence_number":12,"logprobs":[]}"#,
            r#"data: {"type":"response.content_part.done","content_index":0,"item_id":"msg_1","output_index":1,"part":{"type":"output_text","text":"Hi it's","annotations":[]},"sequence_number":13}"#,
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi it's","annotations":[]}],"status":"completed","id":"msg_1"},"sequence_number":14}"#,
            r#"data: {"type":"response.completed","sequence_number":15,"response":{"id":"resp_1","object":"response","created_at":1700000000,"model":"test-model","status":"completed","output":[{"status":"completed","id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"Easy"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi it's","annotations":[]}],"status":"completed","id":"msg_1"}],"usage":{"input_tokens":18,"input_tokens_details":{"cached_tokens":0},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":1},"total_tokens":23},"error":null,"reasoning":{"effort":"medium"},"service_tier":"default"}}"#,
            "data: [DONE]",
        ];
        assert_eq!(
            render(&lines),
            [
                r#"meta: id=resp_1 created_at=1700000000 model=test-model reasoning={"effort":"medium"} service_tier=auto"#,
                "data: created status=in_progress",
                "data: in_progress",
                "data: output_item.added reasoning id=rs_1 status=in_progress",
                "data: reasoning_summary_part.added summary_text",
                "data: reasoning_summary_text='Easy'",
                "data: reasoning_summary_text.done",
                "data: reasoning_summary_part.done summary_text",
                "data: output_item.done reasoning status=completed",
                "data: output_item.added message role=assistant status=in_progress id=msg_1",
                "data: content_part.added output_text",
                "data: 'Hi'",
                r"data: ' it\'s'",
                "data: output_text.done",
                "data: content_part.done output_text",
                "data: output_item.done message status=completed",
                "meta: service_tier=default",
                r#"data: completed, usage: {"input_tokens":18,"output_tokens":5,"output_tokens_details":{"reasoning_tokens":1},"total_tokens":23}"#,
                "data: [DONE]",
            ]
        );
    }

    #[test]
    fn mismatches_gaps_and_unannounced_text() {
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]},"sequence_number":2}"#,
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hel","sequence_number":3}"#,
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"lo","sequence_number":5}"#,
            r#"data: {"type":"response.output_text.done","item_id":"msg_1","output_index":0,"content_index":0,"text":"Help","sequence_number":6}"#,
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"Help"}]},"sequence_number":7}"#,
            r#"data: {"type":"response.completed","sequence_number":8,"response":{"id":"resp_2","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"Help!"}]}]}}"#,
        ];
        assert_eq!(
            render(&lines),
            [
                "data: output_item.added message id=msg_1 role=assistant",
                "data: 'Hel'",
                "data: 'lo' sequence_number=5",
                "data: output_text.done text='Help'",
                r#"data: output_item.done message content=[{"type":"output_text","text":"Help"}]"#,
                "meta: id=resp_2",
                r#"data: completed status=incomplete incomplete_details={"reason":"max_output_tokens"} output=[{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"Help!"}]}]"#,
            ]
        );
    }

    #[test]
    fn parallel_function_calls() {
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"get_weather","arguments":"","status":"in_progress"}}"#,
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"get_time","arguments":"","status":"in_progress"}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"city\":\"Rome\"}"}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_2","output_index":1,"delta":"{}"}"#,
            r#"data: {"type":"response.function_call_arguments.done","item_id":"fc_2","output_index":1,"arguments":"{}"}"#,
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"get_time","arguments":"{}","status":"completed"}}"#,
        ];
        assert_eq!(
            render(&lines),
            [
                "data: output_item.added function_call id=fc_1 call_id=call_1 name=get_weather status=in_progress",
                "data: output_item.added function_call id=fc_2 call_id=call_2 name=get_time status=in_progress",
                r#"data: function_call_arguments='{"city":"Rome"}' output_index=0 item_id=fc_1"#,
                "data: function_call_arguments='{}' output_index=1 item_id=fc_2",
                "data: function_call_arguments.done",
                "data: output_item.done function_call status=completed",
            ]
        );
    }

    #[test]
    fn errors() {
        let lines = [
            r#"data: {"type":"error","code":"rate_limit_exceeded","message":"Slow down","param":null,"sequence_number":3}"#,
            r#"data: {"type":"response.failed","response":{"id":"resp_3","object":"response","status":"failed","error":{"code":"server_error","message":"boom"}}}"#,
        ];
        assert_eq!(
            render(&lines),
            [
                "data: error code=rate_limit_exceeded message='Slow down'",
                "meta: id=resp_3",
                r#"data: failed, error: {"code":"server_error","message":"boom"}"#,
            ]
        );
    }

    #[test]
    fn event_lines_are_kept_unless_repeated() {
        let lines = [
            "event: message",
            r#"data: {"choices":[{"delta":{"content":"x"}}]}"#,
            "event: response.in_progress",
            r#"data: {"type":"response.created"}"#,
            "event: ping",
            "",
            "event: end",
        ];
        assert_eq!(
            render(&lines),
            [
                "event: message",
                "data: 'x'",
                "event: response.in_progress",
                "data: created",
                "event: ping",
                "event: end",
            ]
        );
    }

    #[test]
    fn timing() {
        let mut r = Renderer::new(false);
        let stream = [
            (
                0.1,
                r#"data: {"type":"response.created","response":{"id":"r"}}"#,
            ),
            (
                0.4,
                r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs","delta":"Hm"}"#,
            ),
            (
                0.9,
                r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"Hi"}"#,
            ),
            (
                2.4,
                r#"data: {"type":"response.completed","response":{"id":"r","usage":{"output_tokens":30}}}"#,
            ),
        ];
        for (now, line) in stream {
            r.line(line, now);
        }
        assert_eq!(
            r.summary(None).unwrap(),
            "time: ttft=400.0ms first_content=900.0ms total=2.400s speed=15.0tok/s"
        );
    }
}
