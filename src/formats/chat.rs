//! OpenAI Chat Completions.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{Frame, StreamParser, StreamRenderer, args_string, text_of};
use crate::ir::*;
use crate::sse::SseEvent;

// ------------------------------------------------------------------ request in

pub fn parse_request(v: &Value) -> Result<Request, String> {
    let mut req = Request {
        include_usage: v["stream_options"]["include_usage"].as_bool().unwrap_or(false),
        max_tokens: v["max_completion_tokens"].as_u64().or_else(|| v["max_tokens"].as_u64()),
        temperature: v["temperature"].as_f64(),
        top_p: v["top_p"].as_f64(),
        parallel_tool_calls: v["parallel_tool_calls"].as_bool(),
        ..Default::default()
    };
    match &v["stop"] {
        Value::String(s) => req.stop.push(s.clone()),
        Value::Array(a) => req.stop.extend(a.iter().filter_map(|s| s.as_str().map(String::from))),
        _ => {}
    }
    if let Some(effort) = v["reasoning_effort"].as_str() {
        req.reasoning =
            Some(Reasoning { effort: Some(effort.to_string()), disabled: effort == "none", ..Default::default() });
    }
    req.response_format = match v["response_format"]["type"].as_str() {
        Some("json_object") => Some(ResponseFormat::JsonObject),
        Some("json_schema") => {
            let js = &v["response_format"]["json_schema"];
            Some(ResponseFormat::JsonSchema {
                name: js["name"].as_str().unwrap_or("response").to_string(),
                schema: js["schema"].clone(),
                strict: js["strict"].as_bool().unwrap_or(false),
            })
        }
        _ => None,
    };

    let messages = v["messages"].as_array().ok_or("`messages` must be an array")?;
    for m in messages {
        match m["role"].as_str().unwrap_or("user") {
            "system" | "developer" => {
                let t = text_of(&m["content"]);
                if !t.is_empty() {
                    req.system.push(t);
                }
            }
            "assistant" => {
                let mut parts = Vec::new();
                for key in ["reasoning_content", "reasoning"] {
                    if let Some(r) = m[key].as_str().filter(|s| !s.is_empty()) {
                        parts.push(Part::Reasoning { text: r.to_string(), sig: None });
                        break;
                    }
                }
                let text = text_of(&m["content"]);
                if !text.is_empty() {
                    parts.push(Part::Text(text));
                }
                for tc in m["tool_calls"].as_array().into_iter().flatten() {
                    let f = &tc["function"];
                    parts.push(Part::ToolCall {
                        id: tc["id"].as_str().unwrap_or_default().to_string(),
                        name: f["name"].as_str().unwrap_or_default().to_string(),
                        args: args_string(&f["arguments"]),
                        sig: None,
                    });
                }
                req.messages.push(Message { role: Role::Assistant, parts });
            }
            "tool" | "function" => {
                let id = m["tool_call_id"].as_str().or_else(|| m["name"].as_str()).unwrap_or_default();
                req.messages.push(Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        id: id.to_string(),
                        name: m["name"].as_str().map(String::from),
                        content: content_parts(&m["content"]),
                        is_error: false,
                    }],
                });
            }
            _ => req.messages.push(Message { role: Role::User, parts: content_parts(&m["content"]) }),
        }
    }
    req.messages = merge_adjacent(req.messages);

    for t in v["tools"].as_array().into_iter().flatten() {
        let f = if t["type"] == "function" { &t["function"] } else { t };
        if let Some(name) = f["name"].as_str() {
            req.tools.push(Tool {
                name: name.to_string(),
                description: f["description"].as_str().unwrap_or_default().to_string(),
                parameters: params_or_empty(&f["parameters"]),
            });
        }
    }
    req.tool_choice = match &v["tool_choice"] {
        Value::String(s) if s == "none" => ToolChoice::None,
        Value::String(s) if s == "required" => ToolChoice::Required,
        Value::Object(o) => o
            .get("function")
            .and_then(|f| f["name"].as_str())
            .map(|n| ToolChoice::Tool(n.to_string()))
            .unwrap_or_default(),
        _ => ToolChoice::Auto,
    };
    Ok(req)
}

pub(crate) fn params_or_empty(v: &Value) -> Value {
    if v.is_object() { v.clone() } else { json!({ "type": "object", "properties": {} }) }
}

fn content_parts(v: &Value) -> Vec<Part> {
    match v {
        Value::String(s) => vec![Part::Text(s.clone())],
        Value::Array(items) => items
            .iter()
            .filter_map(|it| match it["type"].as_str() {
                Some("text") | Some("input_text") => it["text"].as_str().map(|t| Part::Text(t.to_string())),
                Some("image_url") => {
                    let url = it["image_url"]["url"].as_str().or_else(|| it["image_url"].as_str())?;
                    Some(Part::Image(Image::from_url(url)))
                }
                _ => it.as_str().map(|t| Part::Text(t.to_string())),
            })
            .collect(),
        Value::Null => vec![],
        other => vec![Part::Text(other.to_string())],
    }
}

// ----------------------------------------------------------------- request out

pub fn build_request(req: &Request, model: &str) -> Value {
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({ "role": "system", "content": req.system.join("\n\n") }));
    }
    for m in &req.messages {
        match m.role {
            Role::Assistant => {
                let text: String = m
                    .parts
                    .iter()
                    .filter_map(|p| if let Part::Text(t) = p { Some(t.as_str()) } else { None })
                    .collect();
                let calls: Vec<Value> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::ToolCall { id, name, args, .. } => Some(json!({
                            "id": id, "type": "function",
                            "function": { "name": name, "arguments": if args.is_empty() { "{}" } else { args } }
                        })),
                        _ => None,
                    })
                    .collect();
                let mut msg = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { Value::String(text) } });
                if !calls.is_empty() {
                    msg["tool_calls"] = Value::Array(calls);
                }
                messages.push(msg);
            }
            Role::User => {
                let mut content: Vec<Value> = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::ToolResult { id, content: c, .. } => {
                            messages.push(json!({ "role": "tool", "tool_call_id": id, "content": parts_text(c) }));
                            for img in c.iter().filter_map(|p| if let Part::Image(i) = p { Some(i) } else { None }) {
                                content.push(json!({ "type": "image_url", "image_url": { "url": img.to_url() } }));
                            }
                        }
                        Part::Text(t) => content.push(json!({ "type": "text", "text": t })),
                        Part::Image(i) => {
                            content.push(json!({ "type": "image_url", "image_url": { "url": i.to_url() } }))
                        }
                        _ => {}
                    }
                }
                if content.is_empty() {
                    continue;
                }
                let all_text = content.iter().all(|c| c["type"] == "text");
                let value = if all_text {
                    Value::String(content.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n"))
                } else {
                    Value::Array(content)
                };
                messages.push(json!({ "role": "user", "content": value }));
            }
        }
    }

    let mut out = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let o = out.as_object_mut().unwrap();
    if !req.tools.is_empty() {
        o.insert(
            "tools".into(),
            req.tools
                .iter()
                .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.parameters } }))
                .collect(),
        );
        o.insert(
            "tool_choice".into(),
            match &req.tool_choice {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Tool(n) => json!({ "type": "function", "function": { "name": n } }),
            },
        );
        if let Some(p) = req.parallel_tool_calls {
            o.insert("parallel_tool_calls".into(), p.into());
        }
    }
    if let Some(m) = req.max_tokens {
        o.insert("max_tokens".into(), m.into());
    }
    if let Some(t) = req.temperature {
        o.insert("temperature".into(), t.into());
    }
    if let Some(t) = req.top_p {
        o.insert("top_p".into(), t.into());
    }
    if !req.stop.is_empty() {
        o.insert("stop".into(), req.stop.clone().into());
    }
    if let Some(e) = req.reasoning.as_ref().and_then(|r| r.effort_level()) {
        o.insert("reasoning_effort".into(), e.into());
    }
    match &req.response_format {
        Some(ResponseFormat::JsonObject) => {
            o.insert("response_format".into(), json!({ "type": "json_object" }));
        }
        Some(ResponseFormat::JsonSchema { name, schema, strict }) => {
            o.insert(
                "response_format".into(),
                json!({ "type": "json_schema", "json_schema": { "name": name, "schema": schema, "strict": strict } }),
            );
        }
        None => {}
    }
    out
}

// --------------------------------------------------------------- stream parser

#[derive(Default)]
pub struct Parser {
    started: bool,
    tools: HashMap<u64, usize>,
    next_key: usize,
}

fn finish_of(s: &str) -> Finish {
    match s {
        "length" => Finish::Length,
        "tool_calls" | "function_call" => Finish::ToolCalls,
        "content_filter" => Finish::Filter,
        _ => Finish::Stop,
    }
}

fn usage_of(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    Some(Usage {
        input: prompt.saturating_sub(cached),
        cache_read: cached,
        output: u["completion_tokens"].as_u64().unwrap_or(0),
        reasoning: u["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
        cache_write: 0,
    })
}

impl StreamParser for Parser {
    fn feed(&mut self, ev: &SseEvent, out: &mut Vec<Event>) {
        if ev.data.trim() == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return };
        if let Some(err) = v.get("error") {
            out.push(Event::Error {
                status: err["code"].as_u64().unwrap_or(500) as u16,
                message: err["message"].as_str().map(String::from).unwrap_or_else(|| err.to_string()),
            });
            return;
        }
        if !self.started {
            self.started = true;
            out.push(Event::Start {
                id: v["id"].as_str().map(String::from),
                model: v["model"].as_str().map(String::from),
            });
        }
        if let Some(choice) = v["choices"].get(0) {
            let d = &choice["delta"];
            for key in ["reasoning_content", "reasoning"] {
                if let Some(r) = d[key].as_str().filter(|s| !s.is_empty()) {
                    out.push(Event::Reasoning(r.to_string()));
                    break;
                }
            }
            if let Some(t) = d["content"].as_str().filter(|s| !s.is_empty()) {
                out.push(Event::Text(t.to_string()));
            }
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                let idx = tc["index"].as_u64().unwrap_or(0);
                let key = match self.tools.get(&idx) {
                    Some(k) => *k,
                    None => {
                        let k = self.next_key;
                        self.next_key += 1;
                        self.tools.insert(idx, k);
                        out.push(Event::ToolStart {
                            key: k,
                            id: tc["id"].as_str().map(String::from).unwrap_or_else(|| new_id("call_")),
                            name: tc["function"]["name"].as_str().unwrap_or_default().to_string(),
                        });
                        k
                    }
                };
                if let Some(a) = tc["function"]["arguments"].as_str().filter(|s| !s.is_empty()) {
                    out.push(Event::ToolArgs { key, delta: a.to_string() });
                }
            }
            if let Some(f) = choice["finish_reason"].as_str() {
                out.push(Event::Finish(finish_of(f)));
            }
        }
        if let Some(u) = usage_of(&v["usage"]) {
            out.push(Event::Usage(u));
        }
    }
}

pub fn full_to_events(v: &Value) -> Vec<Event> {
    let mut out =
        vec![Event::Start { id: v["id"].as_str().map(String::from), model: v["model"].as_str().map(String::from) }];
    let choice = &v["choices"][0];
    let m = &choice["message"];
    for key in ["reasoning_content", "reasoning"] {
        if let Some(r) = m[key].as_str().filter(|s| !s.is_empty()) {
            out.push(Event::Reasoning(r.to_string()));
            break;
        }
    }
    if let Some(t) = m["content"].as_str().filter(|s| !s.is_empty()) {
        out.push(Event::Text(t.to_string()));
    }
    for (i, tc) in m["tool_calls"].as_array().into_iter().flatten().enumerate() {
        out.push(Event::ToolStart {
            key: i,
            id: tc["id"].as_str().unwrap_or_default().to_string(),
            name: tc["function"]["name"].as_str().unwrap_or_default().to_string(),
        });
        out.push(Event::ToolArgs { key: i, delta: args_string(&tc["function"]["arguments"]) });
    }
    if let Some(u) = usage_of(&v["usage"]) {
        out.push(Event::Usage(u));
    }
    out.push(Event::Finish(finish_of(choice["finish_reason"].as_str().unwrap_or("stop"))));
    out
}

// ------------------------------------------------------------- stream renderer

pub struct Renderer {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    sent_role: bool,
    tools: HashMap<usize, usize>,
    usage: Usage,
    finish: Option<Finish>,
    any_tool: bool,
    errored: bool,
}

impl Renderer {
    pub fn new(model: &str, include_usage: bool) -> Self {
        Self {
            id: new_id("chatcmpl-"),
            model: model.to_string(),
            created: now_secs(),
            include_usage,
            sent_role: false,
            tools: HashMap::new(),
            usage: Usage::default(),
            finish: None,
            any_tool: false,
            errored: false,
        }
    }

    fn chunk(&mut self, mut delta: Map<String, Value>, finish: Option<&str>) -> Frame {
        if !self.sent_role {
            self.sent_role = true;
            delta.insert("role".into(), "assistant".into());
        }
        Frame::data(
            json!({
                "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
            })
            .to_string(),
        )
    }
}

fn delta(k: &str, v: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(k.into(), v);
    m
}

fn finish_str(f: Finish) -> &'static str {
    match f {
        Finish::Stop => "stop",
        Finish::Length => "length",
        Finish::ToolCalls => "tool_calls",
        Finish::Filter => "content_filter",
    }
}

pub(crate) fn usage_json(u: &Usage) -> Value {
    json!({
        "prompt_tokens": u.prompt_total(),
        "completion_tokens": u.output,
        "total_tokens": u.prompt_total() + u.output,
        "prompt_tokens_details": { "cached_tokens": u.cache_read },
        "completion_tokens_details": { "reasoning_tokens": u.reasoning },
    })
}

impl StreamRenderer for Renderer {
    fn push(&mut self, ev: &Event, out: &mut Vec<Frame>) {
        match ev {
            Event::Text(t) => {
                let f = self.chunk(delta("content", t.as_str().into()), None);
                out.push(f);
            }
            Event::Reasoning(t) => {
                let f = self.chunk(delta("reasoning_content", t.as_str().into()), None);
                out.push(f);
            }
            Event::ToolStart { key, id, name } => {
                self.any_tool = true;
                let idx = self.tools.len();
                self.tools.insert(*key, idx);
                let f = self.chunk(
                    delta(
                        "tool_calls",
                        json!([{ "index": idx, "id": id, "type": "function", "function": { "name": name, "arguments": "" } }]),
                    ),
                    None,
                );
                out.push(f);
            }
            Event::ToolArgs { key, delta: d } => {
                if let Some(idx) = self.tools.get(key).copied() {
                    let f = self
                        .chunk(delta("tool_calls", json!([{ "index": idx, "function": { "arguments": d } }])), None);
                    out.push(f);
                }
            }
            Event::Usage(u) => self.usage.merge(u),
            Event::Finish(f) => self.finish = Some(*f),
            Event::Error { status, message } => {
                self.errored = true;
                out.push(Frame::data(super::error_body(crate::ir::Format::Chat, *status, message).to_string()));
            }
            Event::Start { model: Some(_), .. } | Event::Start { .. } => {}
            Event::ReasoningSig(_) | Event::RedactedReasoning(_) | Event::ToolSig { .. } => {}
        }
    }

    fn finish(&mut self, out: &mut Vec<Frame>) {
        if self.errored {
            out.push(Frame::data("[DONE]"));
            return;
        }
        let mut f = self.finish.unwrap_or(Finish::Stop);
        if self.any_tool && f == Finish::Stop {
            f = Finish::ToolCalls;
        }
        let frame = self.chunk(Map::new(), Some(finish_str(f)));
        out.push(frame);
        if self.include_usage {
            out.push(Frame::data(
                json!({
                    "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
                    "choices": [], "usage": usage_json(&self.usage)
                })
                .to_string(),
            ));
        }
        out.push(Frame::data("[DONE]"));
    }
}

pub fn render_full(agg: &Aggregate, model: &str) -> Value {
    let mut msg = json!({ "role": "assistant", "content": Value::Null });
    let text = agg.text();
    if !text.is_empty() {
        msg["content"] = text.into();
    }
    let reasoning = agg.reasoning_text();
    if !reasoning.is_empty() {
        msg["reasoning_content"] = reasoning.into();
    }
    let calls: Vec<Value> = agg
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall { id, name, args, .. } => Some(json!({
                "id": id, "type": "function", "function": { "name": name, "arguments": if args.is_empty() { "{}" } else { args } }
            })),
            _ => None,
        })
        .collect();
    if !calls.is_empty() {
        msg["tool_calls"] = Value::Array(calls);
    }
    json!({
        "id": new_id("chatcmpl-"),
        "object": "chat.completion",
        "created": now_secs(),
        "model": model,
        "choices": [{ "index": 0, "message": msg, "finish_reason": finish_str(agg.finish_reason()) }],
        "usage": usage_json(&agg.usage),
    })
}
