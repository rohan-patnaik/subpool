//! OpenAI Responses API (also what the ChatGPT Codex backend speaks).

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::{Frame, StreamParser, StreamRenderer, args_string, text_of};
use crate::ir::*;
use crate::sse::SseEvent;

/// Foreign reasoning signatures are tunnelled through `encrypted_content`
/// so they survive a round trip through Responses clients such as Codex.
const CLAUDE_SIG: &str = "cpx-claude:";
const GEMINI_SIG: &str = "cpx-gemini:";

fn sig_from_encrypted(id: Option<String>, enc: &str) -> Sig {
    if let Some(s) = enc.strip_prefix(CLAUDE_SIG) {
        Sig::Claude(s.to_string())
    } else if let Some(s) = enc.strip_prefix(GEMINI_SIG) {
        Sig::Gemini(s.to_string())
    } else {
        Sig::Codex { id, encrypted: enc.to_string() }
    }
}

fn sig_to_encrypted(sig: &Sig) -> String {
    match sig {
        Sig::Claude(s) => format!("{CLAUDE_SIG}{s}"),
        Sig::Gemini(s) => format!("{GEMINI_SIG}{s}"),
        Sig::Codex { encrypted, .. } => encrypted.clone(),
    }
}

// ------------------------------------------------------------------ request in

pub fn parse_request(v: &Value) -> Result<Request, String> {
    let mut req = Request {
        max_tokens: v["max_output_tokens"].as_u64(),
        temperature: v["temperature"].as_f64(),
        top_p: v["top_p"].as_f64(),
        parallel_tool_calls: v["parallel_tool_calls"].as_bool(),
        ..Default::default()
    };
    if let Some(i) = v["instructions"].as_str().filter(|s| !s.is_empty()) {
        req.system.push(i.to_string());
    }
    if let Some(e) = v["reasoning"]["effort"].as_str() {
        req.reasoning = Some(Reasoning { effort: Some(e.to_string()), disabled: e == "none", ..Default::default() });
    }
    req.response_format = match v["text"]["format"]["type"].as_str() {
        Some("json_object") => Some(ResponseFormat::JsonObject),
        Some("json_schema") => {
            let f = &v["text"]["format"];
            Some(ResponseFormat::JsonSchema {
                name: f["name"].as_str().unwrap_or("response").to_string(),
                schema: f["schema"].clone(),
                strict: f["strict"].as_bool().unwrap_or(false),
            })
        }
        _ => None,
    };

    for t in v["tools"].as_array().into_iter().flatten() {
        match t["type"].as_str() {
            Some("function") => req.tools.push(Tool {
                name: t["name"].as_str().unwrap_or_default().to_string(),
                description: t["description"].as_str().unwrap_or_default().to_string(),
                parameters: super::chat::params_or_empty(&t["parameters"]),
            }),
            Some("custom") => {
                let name = t["name"].as_str().unwrap_or_default().to_string();
                req.custom_tools.push(name.clone());
                let mut description = t["description"].as_str().unwrap_or_default().to_string();
                if let Some(def) = t["format"]["definition"].as_str() {
                    description.push_str("\n\nThe `input` argument must follow this grammar:\n");
                    description.push_str(def);
                }
                req.tools.push(Tool {
                    name,
                    description,
                    parameters: json!({
                        "type": "object",
                        "properties": { "input": { "type": "string", "description": "Raw tool input" } },
                        "required": ["input"]
                    }),
                });
            }
            _ => {}
        }
    }
    req.tool_choice = match &v["tool_choice"] {
        Value::String(s) if s == "none" => ToolChoice::None,
        Value::String(s) if s == "required" => ToolChoice::Required,
        Value::Object(o) => {
            o.get("name").and_then(Value::as_str).map(|n| ToolChoice::Tool(n.to_string())).unwrap_or_default()
        }
        _ => ToolChoice::Auto,
    };

    match &v["input"] {
        Value::String(s) => req.messages.push(Message { role: Role::User, parts: vec![Part::Text(s.clone())] }),
        Value::Array(items) => parse_items(items, &mut req),
        _ => {}
    }
    req.messages = merge_adjacent(req.messages);
    Ok(req)
}

pub fn parse_items(items: &[Value], req: &mut Request) {
    for it in items {
        let kind = it["type"].as_str().unwrap_or(if it.get("role").is_some() { "message" } else { "" });
        match kind {
            "message" => {
                let role = it["role"].as_str().unwrap_or("user");
                if role == "system" || role == "developer" {
                    let t = text_of(&it["content"]);
                    if !t.is_empty() {
                        req.system.push(t);
                    }
                    continue;
                }
                let parts = content_parts(&it["content"]);
                let role = if role == "assistant" { Role::Assistant } else { Role::User };
                req.messages.push(Message { role, parts });
            }
            "function_call" => req.messages.push(Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall {
                    id: it["call_id"].as_str().unwrap_or_default().to_string(),
                    name: it["name"].as_str().unwrap_or_default().to_string(),
                    args: args_string(&it["arguments"]),
                    sig: None,
                }],
            }),
            "custom_tool_call" => req.messages.push(Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall {
                    id: it["call_id"].as_str().unwrap_or_default().to_string(),
                    name: it["name"].as_str().unwrap_or_default().to_string(),
                    args: json!({ "input": it["input"].as_str().unwrap_or_default() }).to_string(),
                    sig: None,
                }],
            }),
            "function_call_output" | "custom_tool_call_output" => {
                let content = match &it["output"] {
                    Value::String(s) => vec![Part::Text(s.clone())],
                    Value::Array(_) => content_parts(&it["output"]),
                    other => vec![Part::Text(other.to_string())],
                };
                req.messages.push(Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        id: it["call_id"].as_str().unwrap_or_default().to_string(),
                        name: None,
                        content,
                        is_error: false,
                    }],
                });
            }
            "reasoning" => {
                let text: Vec<&str> =
                    it["summary"].as_array().into_iter().flatten().filter_map(|s| s["text"].as_str()).collect();
                let sig = it["encrypted_content"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|e| sig_from_encrypted(it["id"].as_str().map(String::from), e));
                req.messages.push(Message {
                    role: Role::Assistant,
                    parts: vec![Part::Reasoning { text: text.join("\n\n"), sig }],
                });
            }
            _ => {}
        }
    }
}

fn content_parts(v: &Value) -> Vec<Part> {
    match v {
        Value::String(s) => vec![Part::Text(s.clone())],
        Value::Array(items) => items
            .iter()
            .filter_map(|c| match c["type"].as_str() {
                Some("input_text") | Some("output_text") | Some("text") => {
                    c["text"].as_str().map(|t| Part::Text(t.to_string()))
                }
                Some("refusal") => c["refusal"].as_str().map(|t| Part::Text(t.to_string())),
                Some("input_image") => c["image_url"].as_str().map(|u| Part::Image(Image::from_url(u))),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

// ----------------------------------------------------------------- request out

pub struct BuildOpts {
    /// ChatGPT Codex backend rejects sampling / length parameters.
    pub chatgpt_backend: bool,
}

pub fn build_request(req: &Request, model: &str, opts: &BuildOpts) -> Value {
    let mut input = Vec::new();
    if !req.system.is_empty() {
        input.push(json!({
            "type": "message", "role": "developer",
            "content": [{ "type": "input_text", "text": req.system.join("\n\n") }]
        }));
    }
    let custom: HashSet<&str> = req.custom_tools.iter().map(String::as_str).collect();
    let mut custom_calls: HashSet<String> = HashSet::new();
    for m in &req.messages {
        let mut content = Vec::new();
        let flush = |content: &mut Vec<Value>, input: &mut Vec<Value>| {
            if !content.is_empty() {
                let role = if m.role == Role::Assistant { "assistant" } else { "user" };
                input.push(json!({ "type": "message", "role": role, "content": std::mem::take(content) }));
            }
        };
        for p in &m.parts {
            match p {
                Part::Text(t) => {
                    let ty = if m.role == Role::Assistant { "output_text" } else { "input_text" };
                    content.push(json!({ "type": ty, "text": t }));
                }
                Part::Image(i) if m.role == Role::User => {
                    content.push(json!({ "type": "input_image", "image_url": i.to_url() }));
                }
                Part::Reasoning { text, sig: Some(sig @ Sig::Codex { .. }) } => {
                    flush(&mut content, &mut input);
                    let summary: Vec<Value> =
                        if text.is_empty() { vec![] } else { vec![json!({ "type": "summary_text", "text": text })] };
                    input.push(
                        json!({ "type": "reasoning", "summary": summary, "encrypted_content": sig_to_encrypted(sig) }),
                    );
                }
                Part::ToolCall { id, name, args, .. } => {
                    flush(&mut content, &mut input);
                    if custom.contains(name.as_str()) {
                        custom_calls.insert(id.clone());
                        let raw = parse_args(args);
                        let text = raw["input"].as_str().map(String::from).unwrap_or_else(|| args.clone());
                        input.push(json!({ "type": "custom_tool_call", "call_id": id, "name": name, "input": text }));
                    } else {
                        input.push(json!({
                            "type": "function_call", "call_id": id, "name": name,
                            "arguments": if args.is_empty() { "{}" } else { args }
                        }));
                    }
                }
                Part::ToolResult { id, content: c, .. } => {
                    flush(&mut content, &mut input);
                    let ty = if custom_calls.contains(id) { "custom_tool_call_output" } else { "function_call_output" };
                    let images: Vec<&Image> =
                        c.iter().filter_map(|p| if let Part::Image(i) = p { Some(i) } else { None }).collect();
                    let output = if images.is_empty() {
                        Value::String(parts_text(c))
                    } else {
                        let mut arr = vec![json!({ "type": "input_text", "text": parts_text(c) })];
                        arr.extend(images.iter().map(|i| json!({ "type": "input_image", "image_url": i.to_url() })));
                        Value::Array(arr)
                    };
                    input.push(json!({ "type": ty, "call_id": id, "output": output }));
                }
                _ => {}
            }
        }
        flush(&mut content, &mut input);
    }

    let mut out = json!({
        "model": model,
        "instructions": "",
        "input": input,
        "stream": true,
        "store": false,
        "parallel_tool_calls": req.parallel_tool_calls.unwrap_or(true),
        "include": ["reasoning.encrypted_content"],
    });
    let o = out.as_object_mut().unwrap();
    let effort = req.reasoning.as_ref().and_then(|r| r.effort_level()).unwrap_or_else(|| "medium".into());
    let effort = match effort.as_str() {
        "max" => "xhigh".to_string(),
        other => other.to_string(),
    };
    o.insert("reasoning".into(), json!({ "effort": effort, "summary": "auto" }));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                if custom.contains(t.name.as_str()) {
                    json!({ "type": "custom", "name": t.name, "description": t.description })
                } else {
                    json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false })
                }
            })
            .collect();
        o.insert("tools".into(), tools.into());
        o.insert(
            "tool_choice".into(),
            match &req.tool_choice {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Tool(n) => json!({ "type": "function", "name": n }),
            },
        );
    }
    match &req.response_format {
        Some(ResponseFormat::JsonObject) => {
            o.insert("text".into(), json!({ "format": { "type": "json_object" } }));
        }
        Some(ResponseFormat::JsonSchema { name, schema, strict }) => {
            o.insert(
                "text".into(),
                json!({ "format": { "type": "json_schema", "name": name, "schema": schema, "strict": strict } }),
            );
        }
        None => {}
    }
    if !opts.chatgpt_backend {
        if let Some(m) = req.max_tokens {
            o.insert("max_output_tokens".into(), m.into());
        }
        if let Some(t) = req.temperature {
            o.insert("temperature".into(), t.into());
        }
        if let Some(t) = req.top_p {
            o.insert("top_p".into(), t.into());
        }
    }
    out
}

// --------------------------------------------------------------- stream parser

#[derive(Default)]
pub struct Parser {
    started: bool,
    had_args: HashSet<usize>,
    custom: HashSet<usize>,
    reasoning_parts: HashMap<usize, u64>,
    text_seen: HashSet<usize>,
}

pub fn usage_of(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let input = u["input_tokens"].as_u64().unwrap_or(0);
    let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    Some(Usage {
        input: input.saturating_sub(cached),
        cache_read: cached,
        output: u["output_tokens"].as_u64().unwrap_or(0),
        reasoning: u["output_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
        cache_write: 0,
    })
}

fn status_of_code(code: &str) -> u16 {
    match code {
        "rate_limit_exceeded" | "usage_limit_reached" | "insufficient_quota" => 429,
        "invalid_prompt" | "invalid_request_error" | "context_length_exceeded" => 400,
        "server_is_overloaded" | "slow_down" => 503,
        _ => 500,
    }
}

fn finish_of(resp: &Value) -> Finish {
    match resp["incomplete_details"]["reason"].as_str() {
        Some("max_output_tokens") => Finish::Length,
        Some("content_filter") => Finish::Filter,
        _ => Finish::Stop,
    }
}

impl StreamParser for Parser {
    fn feed(&mut self, ev: &SseEvent, out: &mut Vec<Event>) {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return };
        let kind = v["type"].as_str().or(ev.event.as_deref()).unwrap_or_default();
        let idx = v["output_index"].as_u64().unwrap_or(0) as usize;
        match kind {
            "response.created" | "response.in_progress" => {
                if !self.started {
                    self.started = true;
                    let r = &v["response"];
                    out.push(Event::Start {
                        id: r["id"].as_str().map(String::from),
                        model: r["model"].as_str().map(String::from),
                    });
                }
            }
            "response.output_item.added" => {
                let item = &v["item"];
                match item["type"].as_str() {
                    Some("function_call") | Some("custom_tool_call") => {
                        if item["type"] == "custom_tool_call" {
                            self.custom.insert(idx);
                        }
                        out.push(Event::ToolStart {
                            key: idx,
                            id: item["call_id"].as_str().unwrap_or_default().to_string(),
                            name: item["name"].as_str().unwrap_or_default().to_string(),
                        });
                        if let Some(a) = item["arguments"].as_str().filter(|a| !a.is_empty()) {
                            self.had_args.insert(idx);
                            out.push(Event::ToolArgs { key: idx, delta: a.to_string() });
                        }
                    }
                    _ => {}
                }
            }
            "response.output_text.delta" => {
                self.text_seen.insert(idx);
                if let Some(d) = v["delta"].as_str().filter(|d| !d.is_empty()) {
                    out.push(Event::Text(d.to_string()));
                }
            }
            "response.reasoning_summary_part.added" => {
                let n = self.reasoning_parts.entry(idx).or_insert(0);
                if *n > 0 {
                    out.push(Event::Reasoning("\n\n".into()));
                }
                *n += 1;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = v["delta"].as_str().filter(|d| !d.is_empty()) {
                    out.push(Event::Reasoning(d.to_string()));
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(d) = v["delta"].as_str().filter(|d| !d.is_empty()) {
                    self.had_args.insert(idx);
                    out.push(Event::ToolArgs { key: idx, delta: d.to_string() });
                }
            }
            "response.output_item.done" => {
                let item = &v["item"];
                match item["type"].as_str() {
                    Some("reasoning") => {
                        if let Some(enc) = item["encrypted_content"].as_str().filter(|s| !s.is_empty()) {
                            out.push(Event::ReasoningSig(sig_from_encrypted(
                                item["id"].as_str().map(String::from),
                                enc,
                            )));
                        }
                    }
                    Some("function_call") if !self.had_args.contains(&idx) => {
                        out.push(Event::ToolArgs { key: idx, delta: args_string(&item["arguments"]) });
                    }
                    Some("custom_tool_call") => {
                        let input = item["input"].as_str().unwrap_or_default();
                        out.push(Event::ToolArgs { key: idx, delta: json!({ "input": input }).to_string() });
                    }
                    Some("message") if !self.text_seen.contains(&idx) => {
                        for c in item["content"].as_array().into_iter().flatten() {
                            if let Some(t) = c["text"].as_str().filter(|t| !t.is_empty()) {
                                out.push(Event::Text(t.to_string()));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                let r = &v["response"];
                if let Some(u) = usage_of(&r["usage"]) {
                    out.push(Event::Usage(u));
                }
                out.push(Event::Finish(finish_of(r)));
            }
            "response.failed" => {
                let e = &v["response"]["error"];
                out.push(Event::Error {
                    status: status_of_code(e["code"].as_str().unwrap_or_default()),
                    message: e["message"].as_str().unwrap_or("response failed").to_string(),
                });
            }
            "error" => {
                let e = if v["error"].is_object() { &v["error"] } else { &v };
                out.push(Event::Error {
                    status: v["status"].as_u64().map(|s| s as u16).unwrap_or_else(|| {
                        status_of_code(e["code"].as_str().or(e["type"].as_str()).unwrap_or_default())
                    }),
                    message: e["message"].as_str().unwrap_or("upstream error").to_string(),
                });
            }
            _ => {}
        }
    }
}

pub fn full_to_events(v: &Value) -> Vec<Event> {
    let r = if v["response"].is_object() { &v["response"] } else { v };
    let mut out =
        vec![Event::Start { id: r["id"].as_str().map(String::from), model: r["model"].as_str().map(String::from) }];
    for (i, item) in r["output"].as_array().into_iter().flatten().enumerate() {
        match item["type"].as_str() {
            Some("reasoning") => {
                let text: Vec<&str> =
                    item["summary"].as_array().into_iter().flatten().filter_map(|s| s["text"].as_str()).collect();
                if !text.is_empty() {
                    out.push(Event::Reasoning(text.join("\n\n")));
                }
                if let Some(enc) = item["encrypted_content"].as_str().filter(|s| !s.is_empty()) {
                    out.push(Event::ReasoningSig(sig_from_encrypted(item["id"].as_str().map(String::from), enc)));
                }
            }
            Some("message") => {
                for c in item["content"].as_array().into_iter().flatten() {
                    if let Some(t) = c["text"].as_str() {
                        out.push(Event::Text(t.to_string()));
                    }
                }
            }
            Some("function_call") => {
                out.push(Event::ToolStart {
                    key: i,
                    id: item["call_id"].as_str().unwrap_or_default().to_string(),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                });
                out.push(Event::ToolArgs { key: i, delta: args_string(&item["arguments"]) });
            }
            Some("custom_tool_call") => {
                out.push(Event::ToolStart {
                    key: i,
                    id: item["call_id"].as_str().unwrap_or_default().to_string(),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                });
                out.push(Event::ToolArgs { key: i, delta: json!({ "input": item["input"] }).to_string() });
            }
            _ => {}
        }
    }
    if let Some(u) = usage_of(&r["usage"]) {
        out.push(Event::Usage(u));
    }
    out.push(Event::Finish(finish_of(r)));
    out
}

// ------------------------------------------------------------- stream renderer

enum Item {
    None,
    Reasoning { id: String, text: String, sig: Option<Sig> },
    Message { id: String, text: String },
    Tool { key: usize, id: String, call_id: String, name: String, args: String, custom: bool },
}

pub struct Renderer {
    id: String,
    model: String,
    created: i64,
    seq: u64,
    started: bool,
    item: Item,
    output: Vec<Value>,
    usage: Usage,
    finish: Option<Finish>,
    custom: HashSet<String>,
    echo: Value,
    done: bool,
}

impl Renderer {
    pub fn new(model: &str, req: &Request) -> Self {
        Self {
            id: new_id("resp_"),
            model: model.to_string(),
            created: now_secs(),
            seq: 0,
            started: false,
            item: Item::None,
            output: Vec::new(),
            usage: Usage::default(),
            finish: None,
            custom: req.custom_tools.iter().cloned().collect(),
            echo: request_echo(req),
            done: false,
        }
    }

    fn emit(&mut self, kind: &'static str, mut data: Value, out: &mut Vec<Frame>) {
        data["type"] = kind.into();
        data["sequence_number"] = self.seq.into();
        self.seq += 1;
        out.push(Frame::named(kind, data.to_string()));
    }

    fn response(&self, status: &str) -> Value {
        let mut r = json!({
            "id": self.id, "object": "response", "created_at": self.created, "status": status,
            "model": self.model, "output": self.output, "error": null, "incomplete_details": null,
        });
        merge(&mut r, &self.echo);
        r
    }

    fn ensure_started(&mut self, out: &mut Vec<Frame>) {
        if self.started {
            return;
        }
        self.started = true;
        let r = self.response("in_progress");
        self.emit("response.created", json!({ "response": r }), out);
        let r = self.response("in_progress");
        self.emit("response.in_progress", json!({ "response": r }), out);
    }

    fn index(&self) -> usize {
        self.output.len()
    }

    fn close(&mut self, out: &mut Vec<Frame>) {
        let idx = self.index();
        match std::mem::replace(&mut self.item, Item::None) {
            Item::None => {}
            Item::Reasoning { id, text, sig } => {
                let mut summary = vec![];
                if !text.is_empty() {
                    let part = json!({ "type": "summary_text", "text": text });
                    self.emit(
                        "response.reasoning_summary_text.done",
                        json!({ "item_id": id, "output_index": idx, "summary_index": 0, "text": text }),
                        out,
                    );
                    self.emit(
                        "response.reasoning_summary_part.done",
                        json!({ "item_id": id, "output_index": idx, "summary_index": 0, "part": part }),
                        out,
                    );
                    summary.push(part);
                }
                let mut item = json!({ "id": id, "type": "reasoning", "summary": summary });
                if let Some(s) = &sig {
                    item["encrypted_content"] = sig_to_encrypted(s).into();
                }
                self.emit("response.output_item.done", json!({ "output_index": idx, "item": item }), out);
                self.output.push(item);
            }
            Item::Message { id, text } => {
                let part = json!({ "type": "output_text", "text": text, "annotations": [] });
                self.emit(
                    "response.output_text.done",
                    json!({ "item_id": id, "output_index": idx, "content_index": 0, "text": text }),
                    out,
                );
                self.emit(
                    "response.content_part.done",
                    json!({ "item_id": id, "output_index": idx, "content_index": 0, "part": part }),
                    out,
                );
                let item = json!({ "id": id, "type": "message", "status": "completed", "role": "assistant", "content": [part] });
                self.emit("response.output_item.done", json!({ "output_index": idx, "item": item }), out);
                self.output.push(item);
            }
            Item::Tool { id, call_id, name, args, custom, .. } => {
                let args = if args.is_empty() { "{}".to_string() } else { args };
                let item = if custom {
                    let input = parse_args(&args)["input"].as_str().map(String::from).unwrap_or(args);
                    self.emit(
                        "response.custom_tool_call_input.delta",
                        json!({ "item_id": id, "output_index": idx, "delta": input }),
                        out,
                    );
                    self.emit(
                        "response.custom_tool_call_input.done",
                        json!({ "item_id": id, "output_index": idx, "input": input }),
                        out,
                    );
                    json!({ "id": id, "type": "custom_tool_call", "status": "completed", "call_id": call_id, "name": name, "input": input })
                } else {
                    self.emit(
                        "response.function_call_arguments.done",
                        json!({ "item_id": id, "output_index": idx, "arguments": args }),
                        out,
                    );
                    json!({ "id": id, "type": "function_call", "status": "completed", "call_id": call_id, "name": name, "arguments": args })
                };
                self.emit("response.output_item.done", json!({ "output_index": idx, "item": item }), out);
                self.output.push(item);
            }
        }
    }

    fn open_reasoning(&mut self, out: &mut Vec<Frame>) {
        self.close(out);
        let id = new_id("rs_");
        let idx = self.index();
        self.emit(
            "response.output_item.added",
            json!({ "output_index": idx, "item": { "id": id, "type": "reasoning", "summary": [] } }),
            out,
        );
        self.item = Item::Reasoning { id, text: String::new(), sig: None };
    }
}

fn merge(dst: &mut Value, src: &Value) {
    if let (Some(d), Some(s)) = (dst.as_object_mut(), src.as_object()) {
        for (k, v) in s {
            d.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

/// Request parameters echoed back on the response object.
fn request_echo(req: &Request) -> Value {
    json!({
        "parallel_tool_calls": req.parallel_tool_calls.unwrap_or(true),
        "tool_choice": match &req.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Tool(n) => json!({ "type": "function", "name": n }),
        },
        "tools": [],
        "store": false,
        "reasoning": { "effort": req.reasoning.as_ref().and_then(|r| r.effort_level()), "summary": "auto" },
    })
}

fn error_code(status: u16) -> &'static str {
    match status {
        429 => "rate_limit_exceeded",
        400 | 404 | 413 => "invalid_request_error",
        401 | 403 => "invalid_api_key",
        503 | 529 => "server_is_overloaded",
        _ => "server_error",
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.prompt_total(),
        "input_tokens_details": { "cached_tokens": u.cache_read },
        "output_tokens": u.output,
        "output_tokens_details": { "reasoning_tokens": u.reasoning },
        "total_tokens": u.prompt_total() + u.output,
    })
}

impl StreamRenderer for Renderer {
    fn push(&mut self, ev: &Event, out: &mut Vec<Frame>) {
        match ev {
            Event::Usage(u) => return self.usage.merge(u),
            Event::Finish(f) => return self.finish = Some(*f),
            Event::Start { .. } => return,
            _ => {}
        }
        self.ensure_started(out);
        match ev {
            Event::Reasoning(t) => {
                if !matches!(self.item, Item::Reasoning { sig: None, .. }) {
                    self.open_reasoning(out);
                }
                let idx = self.index();
                if let Item::Reasoning { id, text, .. } = &mut self.item {
                    let id = id.clone();
                    let first = text.is_empty();
                    text.push_str(t);
                    if first {
                        self.emit(
                            "response.reasoning_summary_part.added",
                            json!({ "item_id": id, "output_index": idx, "summary_index": 0, "part": { "type": "summary_text", "text": "" } }),
                            out,
                        );
                    }
                    self.emit(
                        "response.reasoning_summary_text.delta",
                        json!({ "item_id": id, "output_index": idx, "summary_index": 0, "delta": t }),
                        out,
                    );
                }
            }
            Event::ReasoningSig(s) => {
                if !matches!(self.item, Item::Reasoning { sig: None, .. }) {
                    self.open_reasoning(out);
                }
                if let Item::Reasoning { sig, .. } = &mut self.item {
                    *sig = Some(s.clone());
                }
            }
            Event::Text(t) => {
                if !matches!(self.item, Item::Message { .. }) {
                    self.close(out);
                    let id = new_id("msg_");
                    let idx = self.index();
                    self.emit(
                        "response.output_item.added",
                        json!({ "output_index": idx, "item": { "id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": [] } }),
                        out,
                    );
                    self.emit(
                        "response.content_part.added",
                        json!({ "item_id": id, "output_index": idx, "content_index": 0, "part": { "type": "output_text", "text": "", "annotations": [] } }),
                        out,
                    );
                    self.item = Item::Message { id, text: String::new() };
                }
                let idx = self.index();
                if let Item::Message { id, text } = &mut self.item {
                    text.push_str(t);
                    let id = id.clone();
                    self.emit(
                        "response.output_text.delta",
                        json!({ "item_id": id, "output_index": idx, "content_index": 0, "delta": t }),
                        out,
                    );
                }
            }
            Event::ToolStart { key, id: call_id, name } => {
                self.close(out);
                let custom = self.custom.contains(name);
                let id = new_id(if custom { "ctc_" } else { "fc_" });
                let idx = self.index();
                let item = if custom {
                    json!({ "id": id, "type": "custom_tool_call", "status": "in_progress", "call_id": call_id, "name": name, "input": "" })
                } else {
                    json!({ "id": id, "type": "function_call", "status": "in_progress", "call_id": call_id, "name": name, "arguments": "" })
                };
                self.emit("response.output_item.added", json!({ "output_index": idx, "item": item }), out);
                self.item = Item::Tool {
                    key: *key,
                    id,
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args: String::new(),
                    custom,
                };
            }
            Event::ToolArgs { key, delta } => {
                let idx = self.index();
                if let Item::Tool { key: k, id, args, custom, .. } = &mut self.item
                    && k == key
                {
                    args.push_str(delta);
                    if !*custom {
                        let id = id.clone();
                        self.emit(
                            "response.function_call_arguments.delta",
                            json!({ "item_id": id, "output_index": idx, "delta": delta }),
                            out,
                        );
                    }
                }
            }
            Event::Error { status, message } => {
                self.close(out);
                self.done = true;
                let mut r = self.response("failed");
                r["error"] = json!({ "code": error_code(*status), "message": message });
                self.emit("response.failed", json!({ "response": r }), out);
            }
            _ => {}
        }
    }

    fn finish(&mut self, out: &mut Vec<Frame>) {
        if self.done {
            return;
        }
        self.done = true;
        self.ensure_started(out);
        self.close(out);
        let (status, kind) = match self.finish {
            Some(Finish::Length) | Some(Finish::Filter) => ("incomplete", "response.incomplete"),
            _ => ("completed", "response.completed"),
        };
        let mut r = self.response(status);
        r["usage"] = usage_json(&self.usage);
        match self.finish {
            Some(Finish::Length) => r["incomplete_details"] = json!({ "reason": "max_output_tokens" }),
            Some(Finish::Filter) => r["incomplete_details"] = json!({ "reason": "content_filter" }),
            _ => {}
        }
        self.emit(kind, json!({ "response": r }), out);
    }
}

pub fn render_full(agg: &Aggregate, model: &str, req: &Request) -> Value {
    let mut r = Renderer::new(model, req);
    let mut sink = Vec::new();
    for p in &agg.parts {
        match p {
            Part::Text(t) => r.push(&Event::Text(t.clone()), &mut sink),
            Part::Reasoning { text, sig } => {
                if !text.is_empty() {
                    r.push(&Event::Reasoning(text.clone()), &mut sink);
                }
                if let Some(s) = sig {
                    r.push(&Event::ReasoningSig(s.clone()), &mut sink);
                }
            }
            Part::ToolCall { id, name, args, .. } => {
                r.push(&Event::ToolStart { key: usize::MAX, id: id.clone(), name: name.clone() }, &mut sink);
                r.push(&Event::ToolArgs { key: usize::MAX, delta: args.clone() }, &mut sink);
            }
            _ => {}
        }
    }
    r.usage = agg.usage.clone();
    r.finish = agg.finish;
    r.finish(&mut sink);
    let last = sink.pop().map(|f| f.data).unwrap_or_default();
    let v: Value = serde_json::from_str(&last).unwrap_or_default();
    v["response"].clone()
}
