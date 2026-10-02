//! Format translators. Each module knows how to:
//! * parse a client request into the IR (`parse_request`)
//! * build an upstream request from the IR (`build_request`)
//! * decode the upstream stream into IR events (`Parser`)
//! * render IR events back to its own wire format (`Renderer`, `render_full`)

pub mod chat;
pub mod claude;
pub mod gemini;
pub mod responses;

use std::borrow::Cow;

use serde_json::Value;

use crate::ir::{Aggregate, Event, Format, Request};
use crate::sse::SseEvent;

/// One outgoing stream frame. `event` is the SSE event name (also used as the
/// websocket message type for the Responses API).
#[derive(Debug, Clone)]
pub struct Frame {
    pub event: Option<Cow<'static, str>>,
    pub data: String,
}

impl Frame {
    pub fn data(data: impl Into<String>) -> Self {
        Self { event: None, data: data.into() }
    }
    pub fn named(event: &'static str, data: impl Into<String>) -> Self {
        Self { event: Some(Cow::Borrowed(event)), data: data.into() }
    }
    pub fn to_sse(&self) -> String {
        crate::sse::frame(self.event.as_deref(), &self.data)
    }
}

pub trait StreamParser: Send {
    fn feed(&mut self, ev: &SseEvent, out: &mut Vec<Event>);
}

pub trait StreamRenderer: Send {
    fn push(&mut self, ev: &Event, out: &mut Vec<Frame>);
    fn finish(&mut self, out: &mut Vec<Frame>);
}

pub fn parse_request(format: Format, body: &Value) -> Result<Request, String> {
    match format {
        Format::Chat => chat::parse_request(body),
        Format::Claude => claude::parse_request(body),
        Format::Responses => responses::parse_request(body),
        Format::Gemini => gemini::parse_request(body),
    }
}

pub fn parser(format: Format) -> Box<dyn StreamParser> {
    match format {
        Format::Chat => Box::new(chat::Parser::default()),
        Format::Claude => Box::new(claude::Parser::default()),
        Format::Responses => Box::new(responses::Parser::default()),
        Format::Gemini => Box::new(gemini::Parser::default()),
    }
}

pub fn renderer(format: Format, model: &str, req: &Request) -> Box<dyn StreamRenderer> {
    match format {
        Format::Chat => Box::new(chat::Renderer::new(model, req.include_usage)),
        Format::Claude => Box::new(claude::Renderer::new(model)),
        Format::Responses => Box::new(responses::Renderer::new(model, req)),
        Format::Gemini => Box::new(gemini::Renderer::new(model)),
    }
}

pub fn render_full(format: Format, agg: &Aggregate, model: &str, req: &Request) -> Value {
    match format {
        Format::Chat => chat::render_full(agg, model),
        Format::Claude => claude::render_full(agg, model),
        Format::Responses => responses::render_full(agg, model, req),
        Format::Gemini => gemini::render_full(agg, model),
    }
}

/// Converts a complete (non-streaming) upstream JSON body into events.
pub fn full_to_events(format: Format, body: &Value) -> Vec<Event> {
    match format {
        Format::Chat => chat::full_to_events(body),
        Format::Claude => claude::full_to_events(body),
        Format::Responses => responses::full_to_events(body),
        Format::Gemini => gemini::full_to_events(body),
    }
}

/// Error body in the client's dialect.
pub fn error_body(format: Format, status: u16, message: &str) -> Value {
    use serde_json::json;
    match format {
        Format::Claude => json!({
            "type": "error",
            "error": { "type": claude_error_type(status), "message": message }
        }),
        Format::Gemini => json!({
            "error": { "code": status, "message": message, "status": gemini_status(status) }
        }),
        Format::Chat | Format::Responses => json!({
            "error": { "message": message, "type": openai_error_type(status), "code": status }
        }),
    }
}

fn claude_error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

fn openai_error_type(status: u16) -> &'static str {
    match status {
        400 | 404 | 413 => "invalid_request_error",
        401 | 403 => "authentication_error",
        429 => "rate_limit_exceeded",
        _ => "server_error",
    }
}

fn gemini_status(status: u16) -> &'static str {
    match status {
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        503 => "UNAVAILABLE",
        _ => "INTERNAL",
    }
}

// ------------------------------------------------------------ small JSON helpers

pub(crate) fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut out = String::new();
            for it in items {
                let t = it.get("text").and_then(Value::as_str).or_else(|| it.as_str());
                if let Some(t) = t {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            out
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(crate) fn args_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "{}".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ir::{Event, Part, Sig};
    use crate::sse::SseEvent;

    fn sse(data: serde_json::Value) -> SseEvent {
        SseEvent { event: None, data: data.to_string() }
    }

    #[test]
    fn chat_to_claude_orders_tool_results_and_drops_unsigned_thinking() {
        let body = json!({
            "model": "claude-opus-5-5",
            "reasoning_effort": "high",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "call.1", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" } }
                ]},
                { "role": "tool", "tool_call_id": "call.1", "content": "sunny" },
                { "role": "user", "content": "thanks" }
            ]
        });
        let req = parse_request(Format::Chat, &body).unwrap();
        let out = claude::build_request(&req, "claude-opus-5-5");
        assert_eq!(out["system"][0]["text"], "be brief");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        // Tool ids are sanitized and the result leads the following user turn.
        assert_eq!(msgs[1]["content"][0]["id"], "call_1");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(msgs[2]["content"][1]["text"], "thanks");
        // An unsigned trailing tool_use turn forces thinking off.
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn effort_maps_to_adaptive_or_budget_thinking() {
        let body =
            json!({ "model": "x", "reasoning_effort": "low", "messages": [{ "role": "user", "content": "hi" }] });
        let req = parse_request(Format::Chat, &body).unwrap();
        let adaptive = claude::build_request(&req, "claude-sonnet-5-5");
        assert_eq!(adaptive["thinking"]["type"], "adaptive");
        assert_eq!(adaptive["output_config"]["effort"], "low");
        let budget = claude::build_request(&req, "claude-sonnet-4-5-20250929");
        assert_eq!(budget["thinking"]["type"], "enabled");
        assert_eq!(budget["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn codex_stream_aggregates_reasoning_text_and_tools() {
        let mut p = parser(Format::Responses);
        let mut evs = Vec::new();
        for d in [
            json!({ "type": "response.created", "response": { "id": "r1", "model": "gpt-6-astra" } }),
            json!({ "type": "response.reasoning_summary_part.added", "output_index": 0 }),
            json!({ "type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "think" }),
            json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "reasoning", "id": "rs", "encrypted_content": "ENC" } }),
            json!({ "type": "response.output_text.delta", "output_index": 1, "delta": "hi" }),
            json!({ "type": "response.output_item.added", "output_index": 2, "item": { "type": "function_call", "call_id": "c1", "name": "f" } }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"a\":1}" }),
            json!({ "type": "response.output_item.done", "output_index": 2, "item": { "type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"a\":1}" } }),
            json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 10, "input_tokens_details": { "cached_tokens": 4 }, "output_tokens": 5 } } }),
        ] {
            p.feed(&sse(d), &mut evs);
        }
        let mut agg = Aggregate::default();
        evs.iter().for_each(|e| agg.push(e));
        assert_eq!(agg.text(), "hi");
        assert_eq!(agg.reasoning_text(), "think");
        assert!(
            matches!(&agg.parts[0], Part::Reasoning { sig: Some(Sig::Codex { encrypted, .. }), .. } if encrypted == "ENC")
        );
        assert!(matches!(&agg.parts[2], Part::ToolCall { args, .. } if args == "{\"a\":1}"));
        assert_eq!((agg.usage.input, agg.usage.cache_read, agg.usage.output), (6, 4, 5));
        assert_eq!(agg.finish_reason(), crate::ir::Finish::ToolCalls);
    }

    #[test]
    fn codex_reasoning_round_trips_through_claude_clients() {
        // Render a Codex signature to a Claude client...
        let req = Request::default();
        let mut r = renderer(Format::Claude, "gpt-6-astra", &req);
        let mut frames = Vec::new();
        r.push(&Event::Reasoning("t".into()), &mut frames);
        r.push(&Event::ReasoningSig(Sig::Codex { id: None, encrypted: "ENC".into() }), &mut frames);
        r.finish(&mut frames);
        let sig = frames
            .iter()
            .filter_map(|f| serde_json::from_str::<serde_json::Value>(&f.data).ok())
            .find_map(|v| v["delta"]["signature"].as_str().map(String::from))
            .unwrap();
        // ...and parse it back when the client replays the conversation.
        let body = json!({ "model": "gpt-6-astra", "messages": [
            { "role": "user", "content": "q" },
            { "role": "assistant", "content": [{ "type": "thinking", "thinking": "t", "signature": sig }, { "type": "text", "text": "a" }] },
            { "role": "user", "content": "q2" }
        ]});
        let parsed = parse_request(Format::Claude, &body).unwrap();
        let out = responses::build_request(&parsed, "gpt-6-astra", &responses::BuildOpts { chatgpt_backend: true });
        let reasoning = out["input"].as_array().unwrap().iter().find(|i| i["type"] == "reasoning").unwrap();
        assert_eq!(reasoning["encrypted_content"], "ENC");
    }

    #[test]
    fn responses_renderer_emits_a_complete_sequence() {
        let req = Request::default();
        let mut r = renderer(Format::Responses, "m", &req);
        let mut frames = Vec::new();
        for ev in [
            Event::Text("hel".into()),
            Event::Text("lo".into()),
            Event::ToolStart { key: 0, id: "c1".into(), name: "f".into() },
            Event::ToolArgs { key: 0, delta: "{}".into() },
        ] {
            r.push(&ev, &mut frames);
        }
        r.finish(&mut frames);
        let kinds: Vec<_> = frames.iter().map(|f| f.event.clone().unwrap().into_owned()).collect();
        assert_eq!(kinds.first().unwrap(), "response.created");
        assert_eq!(kinds.last().unwrap(), "response.completed");
        let done: serde_json::Value = serde_json::from_str(&frames.last().unwrap().data).unwrap();
        let output = done["response"]["output"].as_array().unwrap();
        assert_eq!(output[0]["content"][0]["text"], "hello");
        assert_eq!(output[1]["call_id"], "c1");
        // Sequence numbers are strictly increasing.
        let seqs: Vec<u64> = frames
            .iter()
            .map(|f| serde_json::from_str::<serde_json::Value>(&f.data).unwrap()["sequence_number"].as_u64().unwrap())
            .collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn gemini_function_responses_get_names_and_signatures() {
        let body = json!({ "model": "x", "messages": [
            { "role": "user", "content": "q" },
            { "role": "assistant", "tool_calls": [{ "id": "c1", "type": "function", "function": { "name": "lookup", "arguments": "{}" } }] },
            { "role": "tool", "tool_call_id": "c1", "content": "42" }
        ]});
        let req = parse_request(Format::Chat, &body).unwrap();
        let out = gemini::build_request(&req, "gemini-3.8-flash");
        let contents = out["contents"].as_array().unwrap();
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "skip_thought_signature_validator");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "lookup");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["response"]["result"], "42");
    }

    #[test]
    fn model_suffix_parsing() {
        let (m, r) = crate::ir::split_model_suffix("gpt-6-astra(high)");
        assert_eq!(m, "gpt-6-astra");
        assert_eq!(r.unwrap().effort.as_deref(), Some("high"));
        let (m, r) = crate::ir::split_model_suffix("claude-opus-4-1(8000)");
        assert_eq!(m, "claude-opus-4-1");
        assert_eq!(r.unwrap().budget, Some(8000));
        assert!(crate::ir::split_model_suffix("plain").1.is_none());
    }
}
