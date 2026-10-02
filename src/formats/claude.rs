//! Anthropic Messages API.

use serde_json::{Value, json};

use super::{Frame, StreamParser, StreamRenderer, text_of};
use crate::ir::*;
use crate::sse::SseEvent;

/// Foreign reasoning state rides in the thinking `signature` so Claude clients
/// (Claude Code) send it back on the next turn.
const CODEX_SIG: &str = "cpx-codex:";
const GEMINI_SIG: &str = "cpx-gemini:";

fn sig_from_signature(s: &str) -> Sig {
    if let Some(enc) = s.strip_prefix(CODEX_SIG) {
        Sig::Codex { id: None, encrypted: enc.to_string() }
    } else if let Some(g) = s.strip_prefix(GEMINI_SIG) {
        Sig::Gemini(g.to_string())
    } else {
        Sig::Claude(s.to_string())
    }
}

fn sig_to_signature(sig: &Sig) -> String {
    match sig {
        Sig::Claude(s) => s.clone(),
        Sig::Codex { encrypted, .. } => format!("{CODEX_SIG}{encrypted}"),
        Sig::Gemini(g) => format!("{GEMINI_SIG}{g}"),
    }
}

// ------------------------------------------------------------------ request in

pub fn parse_request(v: &Value) -> Result<Request, String> {
    let mut req = Request {
        max_tokens: v["max_tokens"].as_u64(),
        temperature: v["temperature"].as_f64(),
        top_p: v["top_p"].as_f64(),
        ..Default::default()
    };
    req.stop = v["stop_sequences"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();

    match &v["system"] {
        Value::String(s) if !s.is_empty() => req.system.push(s.clone()),
        Value::Array(blocks) => {
            for b in blocks {
                if let Some(t) = b["text"].as_str().filter(|t| !t.is_empty()) {
                    req.system.push(t.to_string());
                }
            }
        }
        _ => {}
    }

    let effort = v["output_config"]["effort"].as_str().map(String::from);
    req.reasoning = match v["thinking"]["type"].as_str() {
        Some("enabled") => Some(Reasoning { budget: v["thinking"]["budget_tokens"].as_u64(), effort, disabled: false }),
        Some("adaptive") => Some(Reasoning { effort: effort.or(Some("medium".into())), ..Default::default() }),
        Some("disabled") => Some(Reasoning { disabled: true, ..Default::default() }),
        _ => effort.map(|e| Reasoning { effort: Some(e), ..Default::default() }),
    };

    let messages = v["messages"].as_array().ok_or("`messages` must be an array")?;
    for m in messages {
        let role = match m["role"].as_str() {
            Some("assistant") => Role::Assistant,
            Some("system") => {
                let t = text_of(&m["content"]);
                if !t.is_empty() {
                    req.system.push(t);
                }
                continue;
            }
            _ => Role::User,
        };
        req.messages.push(Message { role, parts: blocks_to_parts(&m["content"]) });
    }
    req.messages = merge_adjacent(req.messages);

    for t in v["tools"].as_array().into_iter().flatten() {
        // Anthropic-defined server/client tools (web_search_*, bash_*, ...) have no schema.
        let Some(name) = t["name"].as_str() else { continue };
        if t.get("input_schema").is_none() {
            continue;
        }
        req.tools.push(Tool {
            name: name.to_string(),
            description: t["description"].as_str().unwrap_or_default().to_string(),
            parameters: super::chat::params_or_empty(&t["input_schema"]),
        });
    }
    req.tool_choice = match v["tool_choice"]["type"].as_str() {
        Some("any") => ToolChoice::Required,
        Some("none") => ToolChoice::None,
        Some("tool") => ToolChoice::Tool(v["tool_choice"]["name"].as_str().unwrap_or_default().to_string()),
        _ => ToolChoice::Auto,
    };
    if v["tool_choice"]["disable_parallel_tool_use"].as_bool() == Some(true) {
        req.parallel_tool_calls = Some(false);
    }
    Ok(req)
}

fn blocks_to_parts(content: &Value) -> Vec<Part> {
    let blocks = match content {
        Value::String(s) => return vec![Part::Text(s.clone())],
        Value::Array(b) => b,
        _ => return vec![],
    };
    let mut parts = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("text") => {
                if let Some(t) = b["text"].as_str() {
                    parts.push(Part::Text(t.to_string()));
                }
            }
            Some("image") => {
                let src = &b["source"];
                match src["type"].as_str() {
                    Some("base64") => parts.push(Part::Image(Image::Base64 {
                        mime: src["media_type"].as_str().unwrap_or("image/png").to_string(),
                        data: src["data"].as_str().unwrap_or_default().to_string(),
                    })),
                    Some("url") => {
                        parts.push(Part::Image(Image::Url(src["url"].as_str().unwrap_or_default().to_string())))
                    }
                    _ => {}
                }
            }
            Some("thinking") => parts.push(Part::Reasoning {
                text: b["thinking"].as_str().unwrap_or_default().to_string(),
                sig: b["signature"].as_str().filter(|s| !s.is_empty()).map(sig_from_signature),
            }),
            Some("redacted_thinking") => {
                parts.push(Part::RedactedReasoning(b["data"].as_str().unwrap_or_default().to_string()))
            }
            Some("tool_use") => parts.push(Part::ToolCall {
                id: b["id"].as_str().unwrap_or_default().to_string(),
                name: b["name"].as_str().unwrap_or_default().to_string(),
                args: b["input"].to_string(),
                sig: None,
            }),
            Some("tool_result") => parts.push(Part::ToolResult {
                id: b["tool_use_id"].as_str().unwrap_or_default().to_string(),
                name: None,
                content: blocks_to_parts(&b["content"]),
                is_error: b["is_error"].as_bool().unwrap_or(false),
            }),
            Some("document") => {
                if let Some(t) = b["source"]["data"].as_str().filter(|_| b["source"]["type"] == "text") {
                    parts.push(Part::Text(t.to_string()));
                }
            }
            _ => {}
        }
    }
    parts
}

// ----------------------------------------------------------------- request out

/// Older Claude models that need `budget_tokens` instead of adaptive thinking.
pub fn uses_budget_thinking(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("claude-3")
        || m.contains("haiku-4-5")
        || m.contains("sonnet-4-5")
        || m.contains("opus-4-5")
        || m.contains("opus-4-1")
        || m.starts_with("claude-opus-4-2025")
        || m.starts_with("claude-sonnet-4-2025")
        || m == "claude-opus-4"
        || m == "claude-sonnet-4"
}

pub fn default_max_tokens(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    if m.contains("claude-3") {
        8192
    } else if uses_budget_thinking(&m) {
        32_000
    } else {
        64_000
    }
}

pub fn sanitize_tool_id(id: &str) -> String {
    let s: String =
        id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if s.is_empty() { new_id("toolu_") } else { s }
}

fn image_block(img: &Image) -> Value {
    match img {
        Image::Base64 { mime, data } => {
            json!({ "type": "image", "source": { "type": "base64", "media_type": mime, "data": data } })
        }
        Image::Url(u) => json!({ "type": "image", "source": { "type": "url", "url": u } }),
    }
}

pub fn build_request(req: &Request, model: &str) -> Value {
    let mut thinking_on = false;
    let mut out = json!({ "model": model });
    let o = out.as_object_mut().unwrap();

    let mut max_tokens = req.max_tokens.unwrap_or_else(|| default_max_tokens(model));
    if let Some(r) = &req.reasoning {
        if r.disabled {
            o.insert("thinking".into(), json!({ "type": "disabled" }));
        } else if uses_budget_thinking(model) {
            if let Some(b) = r.budget_tokens() {
                if max_tokens <= 1024 {
                    max_tokens = default_max_tokens(model);
                }
                let budget = b.clamp(1024, max_tokens - 1024);
                o.insert("thinking".into(), json!({ "type": "enabled", "budget_tokens": budget }));
                thinking_on = true;
            }
        } else {
            let effort = match r.effort_level().as_deref() {
                Some("minimal") | Some("low") => "low",
                Some("medium") => "medium",
                Some("xhigh") if model.contains("4-6") => "max",
                Some("xhigh") => "xhigh",
                Some("max") => "max",
                _ => "high",
            };
            o.insert("thinking".into(), json!({ "type": "adaptive" }));
            o.insert("output_config".into(), json!({ "effort": effort }));
            thinking_on = true;
        }
    }

    // Anthropic rejects a trailing tool_use turn without a signed thinking block when thinking is on.
    if thinking_on {
        let last_assistant = req.messages.iter().rev().find(|m| m.role == Role::Assistant);
        if let Some(m) = last_assistant {
            let has_tool = m.parts.iter().any(|p| matches!(p, Part::ToolCall { .. }));
            let signed = m
                .parts
                .iter()
                .any(|p| matches!(p, Part::Reasoning { sig: Some(Sig::Claude(_)), .. } | Part::RedactedReasoning(_)));
            if has_tool && !signed {
                o.insert("thinking".into(), json!({ "type": "disabled" }));
                o.remove("output_config");
                thinking_on = false;
            }
        }
    }
    o.insert("max_tokens".into(), max_tokens.into());

    if !req.system.is_empty() {
        o.insert("system".into(), req.system.iter().map(|t| json!({ "type": "text", "text": t })).collect());
    }

    let mut messages = Vec::new();
    for m in &req.messages {
        let mut blocks = Vec::new();
        let mut results = Vec::new();
        for p in &m.parts {
            match p {
                Part::Text(t) if !t.trim().is_empty() => blocks.push(json!({ "type": "text", "text": t })),
                Part::Image(i) => blocks.push(image_block(i)),
                Part::Reasoning { text, sig: Some(Sig::Claude(s)) } if m.role == Role::Assistant => {
                    blocks.push(json!({ "type": "thinking", "thinking": text, "signature": s }))
                }
                Part::RedactedReasoning(d) if m.role == Role::Assistant => {
                    blocks.push(json!({ "type": "redacted_thinking", "data": d }))
                }
                Part::ToolCall { id, name, args, .. } => blocks.push(json!({
                    "type": "tool_use", "id": sanitize_tool_id(id), "name": name, "input": parse_args(args)
                })),
                Part::ToolResult { id, content, is_error, .. } => {
                    let inner: Vec<Value> = content
                        .iter()
                        .filter_map(|c| match c {
                            Part::Text(t) if !t.is_empty() => Some(json!({ "type": "text", "text": t })),
                            Part::Image(i) => Some(image_block(i)),
                            _ => None,
                        })
                        .collect();
                    let mut r = json!({ "type": "tool_result", "tool_use_id": sanitize_tool_id(id), "content": inner });
                    if *is_error {
                        r["is_error"] = true.into();
                    }
                    results.push(r);
                }
                _ => {}
            }
        }
        // tool_result blocks must lead the user turn.
        results.extend(blocks);
        if results.is_empty() {
            continue;
        }
        let role = if m.role == Role::Assistant { "assistant" } else { "user" };
        messages.push(json!({ "role": role, "content": results }));
    }
    o.insert("messages".into(), messages.into());

    if !req.tools.is_empty() && req.tool_choice != ToolChoice::None {
        o.insert(
            "tools".into(),
            req.tools
                .iter()
                .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.parameters }))
                .collect(),
        );
        let mut tc = match &req.tool_choice {
            ToolChoice::Required => json!({ "type": "any" }),
            ToolChoice::Tool(n) => json!({ "type": "tool", "name": n }),
            _ => json!({ "type": "auto" }),
        };
        if req.parallel_tool_calls == Some(false) {
            tc["disable_parallel_tool_use"] = true.into();
        }
        // Forced tool use is incompatible with extended thinking.
        if !(thinking_on && tc["type"] != "auto") {
            o.insert("tool_choice".into(), tc);
        }
    }
    if !thinking_on {
        if let Some(t) = req.temperature {
            o.insert("temperature".into(), t.clamp(0.0, 1.0).into());
        } else if let Some(p) = req.top_p {
            o.insert("top_p".into(), p.into());
        }
    }
    if !req.stop.is_empty() {
        o.insert("stop_sequences".into(), req.stop.clone().into());
    }
    if let Some(ResponseFormat::JsonSchema { schema, .. }) = &req.response_format {
        let oc = o.entry("output_config").or_insert_with(|| json!({}));
        oc["format"] = json!({ "type": "json_schema", "schema": schema });
    }
    o.insert("stream".into(), true.into());
    out
}

// --------------------------------------------------------------- stream parser

#[derive(Default)]
pub struct Parser {
    started: bool,
}

pub fn finish_of(s: &str) -> Finish {
    match s {
        "max_tokens" | "model_context_window_exceeded" => Finish::Length,
        "tool_use" => Finish::ToolCalls,
        "refusal" => Finish::Filter,
        _ => Finish::Stop,
    }
}

pub fn usage_of(u: &Value) -> Usage {
    Usage {
        input: u["input_tokens"].as_u64().unwrap_or(0),
        output: u["output_tokens"].as_u64().unwrap_or(0),
        cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
        cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        reasoning: 0,
    }
}

fn status_of_error(kind: &str) -> u16 {
    match kind {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "rate_limit_error" => 429,
        "overloaded_error" => 529,
        _ => 500,
    }
}

impl StreamParser for Parser {
    fn feed(&mut self, ev: &SseEvent, out: &mut Vec<Event>) {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return };
        let kind = v["type"].as_str().or(ev.event.as_deref()).unwrap_or_default();
        match kind {
            "message_start" => {
                let m = &v["message"];
                if !self.started {
                    self.started = true;
                    out.push(Event::Start {
                        id: m["id"].as_str().map(String::from),
                        model: m["model"].as_str().map(String::from),
                    });
                }
                out.push(Event::Usage(usage_of(&m["usage"])));
            }
            "content_block_start" => {
                let key = v["index"].as_u64().unwrap_or(0) as usize;
                let b = &v["content_block"];
                match b["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = b["text"].as_str().filter(|t| !t.is_empty()) {
                            out.push(Event::Text(t.to_string()));
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = b["thinking"].as_str().filter(|t| !t.is_empty()) {
                            out.push(Event::Reasoning(t.to_string()));
                        }
                    }
                    Some("redacted_thinking") => {
                        out.push(Event::RedactedReasoning(b["data"].as_str().unwrap_or_default().to_string()))
                    }
                    Some("tool_use") => {
                        out.push(Event::ToolStart {
                            key,
                            id: b["id"].as_str().unwrap_or_default().to_string(),
                            name: b["name"].as_str().unwrap_or_default().to_string(),
                        });
                        if let Some(input) = b["input"].as_object().filter(|o| !o.is_empty()) {
                            out.push(Event::ToolArgs { key, delta: Value::Object(input.clone()).to_string() });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let key = v["index"].as_u64().unwrap_or(0) as usize;
                let d = &v["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => out.push(Event::Text(d["text"].as_str().unwrap_or_default().to_string())),
                    Some("thinking_delta") => {
                        out.push(Event::Reasoning(d["thinking"].as_str().unwrap_or_default().to_string()))
                    }
                    Some("signature_delta") => out.push(Event::ReasoningSig(Sig::Claude(
                        d["signature"].as_str().unwrap_or_default().to_string(),
                    ))),
                    Some("input_json_delta") => {
                        let p = d["partial_json"].as_str().unwrap_or_default();
                        if !p.is_empty() {
                            out.push(Event::ToolArgs { key, delta: p.to_string() });
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    out.push(Event::Finish(finish_of(r)));
                }
                if v["usage"].is_object() {
                    out.push(Event::Usage(usage_of(&v["usage"])));
                }
            }
            "error" => {
                let e = &v["error"];
                out.push(Event::Error {
                    status: status_of_error(e["type"].as_str().unwrap_or_default()),
                    message: e["message"].as_str().unwrap_or("upstream error").to_string(),
                });
            }
            _ => {}
        }
    }
}

pub fn full_to_events(v: &Value) -> Vec<Event> {
    let mut out =
        vec![Event::Start { id: v["id"].as_str().map(String::from), model: v["model"].as_str().map(String::from) }];
    for (i, b) in v["content"].as_array().into_iter().flatten().enumerate() {
        match b["type"].as_str() {
            Some("text") => out.push(Event::Text(b["text"].as_str().unwrap_or_default().to_string())),
            Some("thinking") => {
                out.push(Event::Reasoning(b["thinking"].as_str().unwrap_or_default().to_string()));
                if let Some(s) = b["signature"].as_str().filter(|s| !s.is_empty()) {
                    out.push(Event::ReasoningSig(Sig::Claude(s.to_string())));
                }
            }
            Some("redacted_thinking") => {
                out.push(Event::RedactedReasoning(b["data"].as_str().unwrap_or_default().to_string()))
            }
            Some("tool_use") => {
                out.push(Event::ToolStart {
                    key: i,
                    id: b["id"].as_str().unwrap_or_default().to_string(),
                    name: b["name"].as_str().unwrap_or_default().to_string(),
                });
                out.push(Event::ToolArgs { key: i, delta: b["input"].to_string() });
            }
            _ => {}
        }
    }
    out.push(Event::Usage(usage_of(&v["usage"])));
    out.push(Event::Finish(finish_of(v["stop_reason"].as_str().unwrap_or("end_turn"))));
    out
}

// ------------------------------------------------------------- stream renderer

#[derive(PartialEq)]
enum Block {
    None,
    Text,
    Thinking,
    Tool(usize),
}

pub struct Renderer {
    id: String,
    model: String,
    started: bool,
    block: Block,
    index: usize,
    usage: Usage,
    finish: Option<Finish>,
    any_tool: bool,
    errored: bool,
}

impl Renderer {
    pub fn new(model: &str) -> Self {
        Self {
            id: new_id("msg_"),
            model: model.to_string(),
            started: false,
            block: Block::None,
            index: 0,
            usage: Usage::default(),
            finish: None,
            any_tool: false,
            errored: false,
        }
    }

    fn ensure_started(&mut self, out: &mut Vec<Frame>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(Frame::named(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": self.id, "type": "message", "role": "assistant", "model": self.model,
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": {
                        "input_tokens": self.usage.input, "output_tokens": 0,
                        "cache_read_input_tokens": self.usage.cache_read,
                        "cache_creation_input_tokens": self.usage.cache_write
                    }
                }
            })
            .to_string(),
        ));
    }

    fn close(&mut self, out: &mut Vec<Frame>) {
        if self.block != Block::None {
            out.push(Frame::named(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": self.index }).to_string(),
            ));
            self.index += 1;
            self.block = Block::None;
        }
    }

    fn open(&mut self, block: Block, content: Value, out: &mut Vec<Frame>) {
        self.close(out);
        out.push(Frame::named(
            "content_block_start",
            json!({ "type": "content_block_start", "index": self.index, "content_block": content }).to_string(),
        ));
        self.block = block;
    }

    fn delta(&self, d: Value, out: &mut Vec<Frame>) {
        out.push(Frame::named(
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": self.index, "delta": d }).to_string(),
        ));
    }
}

fn stop_reason(f: Finish) -> &'static str {
    match f {
        Finish::Stop => "end_turn",
        Finish::Length => "max_tokens",
        Finish::ToolCalls => "tool_use",
        Finish::Filter => "refusal",
    }
}

impl StreamRenderer for Renderer {
    fn push(&mut self, ev: &Event, out: &mut Vec<Frame>) {
        if let Event::Usage(u) = ev {
            self.usage.merge(u);
            return;
        }
        if let Event::Start { .. } = ev {
            return;
        }
        self.ensure_started(out);
        match ev {
            Event::Text(t) => {
                if self.block != Block::Text {
                    self.open(Block::Text, json!({ "type": "text", "text": "" }), out);
                }
                self.delta(json!({ "type": "text_delta", "text": t }), out);
            }
            Event::Reasoning(t) => {
                if self.block != Block::Thinking {
                    self.open(Block::Thinking, json!({ "type": "thinking", "thinking": "", "signature": "" }), out);
                }
                self.delta(json!({ "type": "thinking_delta", "thinking": t }), out);
            }
            Event::ReasoningSig(sig) => {
                if self.block != Block::Thinking {
                    self.open(Block::Thinking, json!({ "type": "thinking", "thinking": "", "signature": "" }), out);
                }
                self.delta(json!({ "type": "signature_delta", "signature": sig_to_signature(sig) }), out);
                // A signature ends the thinking block.
                self.close(out);
            }
            Event::RedactedReasoning(d) => {
                self.open(Block::Thinking, json!({ "type": "redacted_thinking", "data": d }), out);
                self.close(out);
            }
            Event::ToolStart { key, id, name } => {
                self.any_tool = true;
                self.open(
                    Block::Tool(*key),
                    json!({ "type": "tool_use", "id": sanitize_tool_id(id), "name": name, "input": {} }),
                    out,
                );
            }
            Event::ToolArgs { key, delta } => {
                if self.block == Block::Tool(*key) {
                    self.delta(json!({ "type": "input_json_delta", "partial_json": delta }), out);
                }
            }
            Event::Finish(f) => self.finish = Some(*f),
            Event::Error { status, message } => {
                self.errored = true;
                out.push(Frame::named("error", super::error_body(Format::Claude, *status, message).to_string()));
            }
            _ => {}
        }
    }

    fn finish(&mut self, out: &mut Vec<Frame>) {
        if self.errored {
            return;
        }
        self.ensure_started(out);
        self.close(out);
        let mut f = self.finish.unwrap_or(Finish::Stop);
        if self.any_tool && f == Finish::Stop {
            f = Finish::ToolCalls;
        }
        out.push(Frame::named(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason(f), "stop_sequence": null },
                "usage": {
                    "input_tokens": self.usage.input, "output_tokens": self.usage.output,
                    "cache_read_input_tokens": self.usage.cache_read,
                    "cache_creation_input_tokens": self.usage.cache_write
                }
            })
            .to_string(),
        ));
        out.push(Frame::named("message_stop", json!({ "type": "message_stop" }).to_string()));
    }
}

pub fn render_full(agg: &Aggregate, model: &str) -> Value {
    let mut content = Vec::new();
    for p in &agg.parts {
        match p {
            Part::Text(t) => content.push(json!({ "type": "text", "text": t })),
            Part::Reasoning { text, sig } => {
                let s = sig.as_ref().map(sig_to_signature).unwrap_or_default();
                content.push(json!({ "type": "thinking", "thinking": text, "signature": s }));
            }
            Part::RedactedReasoning(d) => content.push(json!({ "type": "redacted_thinking", "data": d })),
            Part::ToolCall { id, name, args, .. } => content.push(json!({
                "type": "tool_use", "id": sanitize_tool_id(id), "name": name, "input": parse_args(args)
            })),
            _ => {}
        }
    }
    json!({
        "id": new_id("msg_"),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason(agg.finish_reason()),
        "stop_sequence": null,
        "usage": {
            "input_tokens": agg.usage.input,
            "output_tokens": agg.usage.output,
            "cache_read_input_tokens": agg.usage.cache_read,
            "cache_creation_input_tokens": agg.usage.cache_write
        }
    })
}
