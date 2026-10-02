//! Google Gemini generateContent / streamGenerateContent.

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};

use super::{Frame, StreamParser, StreamRenderer};
use crate::ir::*;
use crate::sse::SseEvent;

/// Accepted by Gemini in place of a real thought signature on replayed calls.
const SKIP_SIG: &str = "skip_thought_signature_validator";

fn g<'a>(v: &'a Value, camel: &str, snake: &str) -> &'a Value {
    match v.get(camel) {
        Some(x) if !x.is_null() => x,
        _ => v.get(snake).unwrap_or(&Value::Null),
    }
}

// ------------------------------------------------------------------ request in

pub fn parse_request(v: &Value) -> Result<Request, String> {
    let mut req = Request::default();
    for p in g(v, "systemInstruction", "system_instruction")["parts"].as_array().into_iter().flatten() {
        if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
            req.system.push(t.to_string());
        }
    }

    let gc = g(v, "generationConfig", "generation_config");
    req.max_tokens = g(gc, "maxOutputTokens", "max_output_tokens").as_u64();
    req.temperature = gc["temperature"].as_f64();
    req.top_p = g(gc, "topP", "top_p").as_f64();
    req.stop = g(gc, "stopSequences", "stop_sequences")
        .as_array()
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let tc = g(gc, "thinkingConfig", "thinking_config");
    if tc.is_object() {
        let budget = g(tc, "thinkingBudget", "thinking_budget").as_i64();
        let level = g(tc, "thinkingLevel", "thinking_level").as_str().map(|s| s.to_ascii_lowercase());
        req.reasoning = Some(Reasoning {
            effort: level,
            budget: budget.filter(|b| *b > 0).map(|b| b as u64),
            disabled: budget == Some(0),
        });
    }
    if g(gc, "responseMimeType", "response_mime_type").as_str() == Some("application/json") {
        let schema = g(gc, "responseJsonSchema", "response_json_schema");
        let schema = if schema.is_null() { g(gc, "responseSchema", "response_schema") } else { schema };
        req.response_format = Some(if schema.is_object() {
            ResponseFormat::JsonSchema { name: "response".into(), schema: schema.clone(), strict: false }
        } else {
            ResponseFormat::JsonObject
        });
    }

    let mut pending: HashMap<String, VecDeque<String>> = HashMap::new();
    let mut counter = 0usize;
    for c in v["contents"].as_array().into_iter().flatten() {
        let role = if c["role"] == "model" { Role::Assistant } else { Role::User };
        let mut parts = Vec::new();
        for p in c["parts"].as_array().into_iter().flatten() {
            let sig =
                g(p, "thoughtSignature", "thought_signature").as_str().filter(|s| !s.is_empty() && *s != SKIP_SIG);
            let sig = sig.map(|s| Sig::Gemini(s.to_string()));
            if let Some(t) = p["text"].as_str() {
                if p["thought"].as_bool() == Some(true) {
                    parts.push(Part::Reasoning { text: t.to_string(), sig });
                } else {
                    parts.push(Part::Text(t.to_string()));
                }
                continue;
            }
            let inline = g(p, "inlineData", "inline_data");
            if inline.is_object() {
                parts.push(Part::Image(Image::Base64 {
                    mime: g(inline, "mimeType", "mime_type").as_str().unwrap_or("image/png").to_string(),
                    data: inline["data"].as_str().unwrap_or_default().to_string(),
                }));
                continue;
            }
            let file = g(p, "fileData", "file_data");
            if let Some(uri) = g(file, "fileUri", "file_uri").as_str() {
                parts.push(Part::Image(Image::Url(uri.to_string())));
                continue;
            }
            let fc = g(p, "functionCall", "function_call");
            if fc.is_object() {
                let name = fc["name"].as_str().unwrap_or_default().to_string();
                let id = fc["id"].as_str().map(String::from).unwrap_or_else(|| {
                    counter += 1;
                    format!("call_{name}_{counter}")
                });
                pending.entry(name.clone()).or_default().push_back(id.clone());
                parts.push(Part::ToolCall { id, name, args: fc["args"].to_string(), sig });
                continue;
            }
            let fr = g(p, "functionResponse", "function_response");
            if fr.is_object() {
                let name = fr["name"].as_str().unwrap_or_default().to_string();
                let id = fr["id"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| pending.get_mut(&name).and_then(|q| q.pop_front()))
                    .unwrap_or_else(|| format!("call_{name}"));
                let resp = &fr["response"];
                let text = match resp.get("result").or_else(|| resp.get("output")).or_else(|| resp.get("content")) {
                    Some(Value::String(s)) => s.clone(),
                    _ => resp.to_string(),
                };
                parts.push(Part::ToolResult {
                    id,
                    name: Some(name),
                    content: vec![Part::Text(text)],
                    is_error: resp.get("error").is_some(),
                });
            }
        }
        req.messages.push(Message { role, parts });
    }
    req.messages = merge_adjacent(req.messages);

    for t in v["tools"].as_array().into_iter().flatten() {
        for f in g(t, "functionDeclarations", "function_declarations").as_array().into_iter().flatten() {
            let params = g(f, "parametersJsonSchema", "parameters_json_schema");
            let params = if params.is_object() { params.clone() } else { lower_schema_types(&f["parameters"]) };
            req.tools.push(Tool {
                name: f["name"].as_str().unwrap_or_default().to_string(),
                description: f["description"].as_str().unwrap_or_default().to_string(),
                parameters: super::chat::params_or_empty(&params),
            });
        }
    }
    let fcc = &g(v, "toolConfig", "tool_config");
    let fcc = g(fcc, "functionCallingConfig", "function_calling_config");
    req.tool_choice = match fcc["mode"].as_str() {
        Some("NONE") => ToolChoice::None,
        Some("ANY") => {
            match g(fcc, "allowedFunctionNames", "allowed_function_names").as_array().map(|a| a.as_slice()) {
                Some([one]) => ToolChoice::Tool(one.as_str().unwrap_or_default().to_string()),
                _ => ToolChoice::Required,
            }
        }
        _ => ToolChoice::Auto,
    };
    Ok(req)
}

/// Gemini's OpenAPI-style schemas use upper-case type names ("OBJECT").
fn lower_schema_types(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, val)| {
                    let val = if k == "type" {
                        val.as_str().map(|s| Value::String(s.to_ascii_lowercase())).unwrap_or_else(|| val.clone())
                    } else {
                        lower_schema_types(val)
                    };
                    (k.clone(), val)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(lower_schema_types).collect()),
        other => other.clone(),
    }
}

// ----------------------------------------------------------------- request out

fn strip_schema(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter().filter(|(k, _)| k.as_str() != "$schema").map(|(k, val)| (k.clone(), strip_schema(val))).collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(strip_schema).collect()),
        other => other.clone(),
    }
}

pub fn build_request(req: &Request, model: &str) -> Value {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut contents = Vec::new();
    for m in &req.messages {
        let mut parts = Vec::new();
        for p in &m.parts {
            match p {
                Part::Text(t) if !t.is_empty() => parts.push(json!({ "text": t })),
                Part::Image(Image::Base64 { mime, data }) => {
                    parts.push(json!({ "inlineData": { "mimeType": mime, "data": data } }))
                }
                Part::Image(Image::Url(u)) => {
                    parts.push(json!({ "fileData": { "mimeType": "image/*", "fileUri": u } }))
                }
                Part::Reasoning { text, sig: Some(Sig::Gemini(s)) } if m.role == Role::Assistant => {
                    parts.push(json!({ "text": text, "thought": true, "thoughtSignature": s }))
                }
                Part::ToolCall { id, name, args, sig } => {
                    names.insert(id.clone(), name.clone());
                    let s = match sig {
                        Some(Sig::Gemini(s)) => s.as_str(),
                        _ => SKIP_SIG,
                    };
                    parts.push(
                        json!({ "functionCall": { "name": name, "args": parse_args(args) }, "thoughtSignature": s }),
                    );
                }
                Part::ToolResult { id, name, content, is_error } => {
                    let name = name.clone().or_else(|| names.get(id).cloned()).unwrap_or_else(|| id.clone());
                    let text = parts_text(content);
                    let response = if *is_error { json!({ "error": text }) } else { json!({ "result": text }) };
                    parts.push(json!({ "functionResponse": { "name": name, "response": response } }));
                }
                _ => {}
            }
        }
        if parts.is_empty() {
            continue;
        }
        let role = if m.role == Role::Assistant { "model" } else { "user" };
        contents.push(json!({ "role": role, "parts": parts }));
    }

    let mut out = json!({ "contents": contents });
    let o = out.as_object_mut().unwrap();
    if !req.system.is_empty() {
        o.insert("systemInstruction".into(), json!({ "parts": [{ "text": req.system.join("\n\n") }] }));
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "parametersJsonSchema": strip_schema(&t.parameters) }))
            .collect();
        o.insert("tools".into(), json!([{ "functionDeclarations": decls }]));
        let cfg = match &req.tool_choice {
            ToolChoice::Auto => json!({ "mode": "AUTO" }),
            ToolChoice::None => json!({ "mode": "NONE" }),
            ToolChoice::Required => json!({ "mode": "ANY" }),
            ToolChoice::Tool(n) => json!({ "mode": "ANY", "allowedFunctionNames": [n] }),
        };
        o.insert("toolConfig".into(), json!({ "functionCallingConfig": cfg }));
    }

    let mut gc = serde_json::Map::new();
    if let Some(m) = req.max_tokens {
        gc.insert("maxOutputTokens".into(), m.into());
    }
    if let Some(t) = req.temperature {
        gc.insert("temperature".into(), t.into());
    }
    if let Some(t) = req.top_p {
        gc.insert("topP".into(), t.into());
    }
    if !req.stop.is_empty() {
        gc.insert("stopSequences".into(), req.stop.clone().into());
    }
    let gemini3 = model.starts_with("gemini-3");
    let thinking = match &req.reasoning {
        Some(r) if r.disabled => {
            if gemini3 {
                json!({ "thinkingLevel": "low" })
            } else {
                json!({ "thinkingBudget": 0 })
            }
        }
        Some(r) if gemini3 => {
            let level = match r.effort_level().as_deref() {
                Some("minimal") | Some("low") => "low",
                Some("medium") => "medium",
                _ => "high",
            };
            json!({ "includeThoughts": true, "thinkingLevel": level })
        }
        Some(r) => match r.budget_tokens() {
            Some(b) => json!({ "includeThoughts": true, "thinkingBudget": b.min(32_768) }),
            None => json!({ "includeThoughts": true }),
        },
        None => json!({ "includeThoughts": true }),
    };
    gc.insert("thinkingConfig".into(), thinking);
    match &req.response_format {
        Some(ResponseFormat::JsonObject) => {
            gc.insert("responseMimeType".into(), "application/json".into());
        }
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            gc.insert("responseMimeType".into(), "application/json".into());
            gc.insert("responseJsonSchema".into(), strip_schema(schema));
        }
        None => {}
    }
    o.insert("generationConfig".into(), Value::Object(gc));
    out
}

// --------------------------------------------------------------- stream parser

#[derive(Default)]
pub struct Parser {
    started: bool,
    tools: usize,
}

fn finish_of(s: &str) -> Finish {
    match s {
        "MAX_TOKENS" => Finish::Length,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => Finish::Filter,
        _ => Finish::Stop,
    }
}

fn usage_of(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let prompt = u["promptTokenCount"].as_u64().unwrap_or(0);
    let cached = u["cachedContentTokenCount"].as_u64().unwrap_or(0);
    let thoughts = u["thoughtsTokenCount"].as_u64().unwrap_or(0);
    Some(Usage {
        input: prompt.saturating_sub(cached),
        cache_read: cached,
        output: u["candidatesTokenCount"].as_u64().unwrap_or(0) + thoughts,
        reasoning: thoughts,
        cache_write: 0,
    })
}

impl Parser {
    fn chunk(&mut self, v: &Value, out: &mut Vec<Event>) {
        if let Some(e) = v.get("error") {
            out.push(Event::Error {
                status: e["code"].as_u64().unwrap_or(500) as u16,
                message: e["message"].as_str().unwrap_or("upstream error").to_string(),
            });
            return;
        }
        if !self.started {
            self.started = true;
            out.push(Event::Start {
                id: v["responseId"].as_str().map(String::from),
                model: v["modelVersion"].as_str().map(String::from),
            });
        }
        let cand = &v["candidates"][0];
        for p in cand["content"]["parts"].as_array().into_iter().flatten() {
            let sig = p["thoughtSignature"].as_str().filter(|s| !s.is_empty()).map(|s| Sig::Gemini(s.to_string()));
            if let Some(fc) = p.get("functionCall") {
                let key = self.tools;
                self.tools += 1;
                out.push(Event::ToolStart {
                    key,
                    id: fc["id"].as_str().map(String::from).unwrap_or_else(|| new_id("call_")),
                    name: fc["name"].as_str().unwrap_or_default().to_string(),
                });
                out.push(Event::ToolArgs { key, delta: fc["args"].to_string() });
                if let Some(s) = sig {
                    out.push(Event::ToolSig { key, sig: s });
                }
            } else if let Some(t) = p["text"].as_str() {
                if p["thought"].as_bool() == Some(true) {
                    if !t.is_empty() {
                        out.push(Event::Reasoning(t.to_string()));
                    }
                    if let Some(s) = sig {
                        out.push(Event::ReasoningSig(s));
                    }
                } else if !t.is_empty() {
                    out.push(Event::Text(t.to_string()));
                }
            }
        }
        if let Some(u) = usage_of(&v["usageMetadata"]) {
            out.push(Event::Usage(u));
        }
        if let Some(f) = cand["finishReason"].as_str() {
            out.push(Event::Finish(finish_of(f)));
        }
    }
}

impl StreamParser for Parser {
    fn feed(&mut self, ev: &SseEvent, out: &mut Vec<Event>) {
        if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
            self.chunk(&v, out);
        }
    }
}

pub fn full_to_events(v: &Value) -> Vec<Event> {
    let mut p = Parser::default();
    let mut out = Vec::new();
    match v {
        Value::Array(chunks) => chunks.iter().for_each(|c| p.chunk(c, &mut out)),
        _ => p.chunk(v, &mut out),
    }
    out
}

// ------------------------------------------------------------- stream renderer

pub struct Renderer {
    id: String,
    model: String,
    tool: Option<(usize, String, String, String, Option<String>)>,
    usage: Usage,
    finish: Option<Finish>,
    errored: bool,
}

impl Renderer {
    pub fn new(model: &str) -> Self {
        Self {
            id: new_id(""),
            model: model.to_string(),
            tool: None,
            usage: Usage::default(),
            finish: None,
            errored: false,
        }
    }

    fn chunk(&self, parts: Vec<Value>) -> Frame {
        Frame::data(
            json!({
                "candidates": [{ "content": { "role": "model", "parts": parts }, "index": 0 }],
                "modelVersion": self.model, "responseId": self.id
            })
            .to_string(),
        )
    }

    fn flush_tool(&mut self, out: &mut Vec<Frame>) {
        if let Some((_, id, name, args, sig)) = self.tool.take() {
            let mut part = json!({ "functionCall": { "id": id, "name": name, "args": parse_args(&args) } });
            if let Some(s) = sig {
                part["thoughtSignature"] = s.into();
            }
            out.push(self.chunk(vec![part]));
        }
    }
}

fn finish_str(f: Finish) -> &'static str {
    match f {
        Finish::Length => "MAX_TOKENS",
        Finish::Filter => "SAFETY",
        _ => "STOP",
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "promptTokenCount": u.prompt_total(),
        "candidatesTokenCount": u.output.saturating_sub(u.reasoning),
        "thoughtsTokenCount": u.reasoning,
        "cachedContentTokenCount": u.cache_read,
        "totalTokenCount": u.prompt_total() + u.output,
    })
}

impl StreamRenderer for Renderer {
    fn push(&mut self, ev: &Event, out: &mut Vec<Frame>) {
        match ev {
            Event::ToolArgs { key, delta } => {
                if let Some((k, _, _, args, _)) = &mut self.tool
                    && k == key
                {
                    args.push_str(delta);
                }
                return;
            }
            Event::ToolSig { key, sig: Sig::Gemini(s) } => {
                if let Some((k, _, _, _, sig)) = &mut self.tool
                    && k == key
                {
                    *sig = Some(s.clone());
                }
                return;
            }
            Event::Usage(u) => return self.usage.merge(u),
            Event::Finish(f) => return self.finish = Some(*f),
            _ => {}
        }
        self.flush_tool(out);
        match ev {
            Event::Text(t) => out.push(self.chunk(vec![json!({ "text": t })])),
            Event::Reasoning(t) => out.push(self.chunk(vec![json!({ "text": t, "thought": true })])),
            Event::ReasoningSig(Sig::Gemini(s)) => {
                out.push(self.chunk(vec![json!({ "text": "", "thought": true, "thoughtSignature": s })]))
            }
            Event::ToolStart { key, id, name } => {
                self.tool = Some((*key, id.clone(), name.clone(), String::new(), None))
            }
            Event::Error { status, message } => {
                self.errored = true;
                out.push(Frame::data(super::error_body(Format::Gemini, *status, message).to_string()))
            }
            _ => {}
        }
    }

    fn finish(&mut self, out: &mut Vec<Frame>) {
        if self.errored {
            return;
        }
        self.flush_tool(out);
        out.push(Frame::data(
            json!({
                "candidates": [{
                    "content": { "role": "model", "parts": [{ "text": "" }] },
                    "finishReason": finish_str(self.finish.unwrap_or_default()), "index": 0
                }],
                "usageMetadata": usage_json(&self.usage),
                "modelVersion": self.model, "responseId": self.id
            })
            .to_string(),
        ));
    }
}

pub fn render_full(agg: &Aggregate, model: &str) -> Value {
    let mut parts = Vec::new();
    for p in &agg.parts {
        match p {
            Part::Text(t) => parts.push(json!({ "text": t })),
            Part::Reasoning { text, sig } => {
                let mut part = json!({ "text": text, "thought": true });
                if let Some(Sig::Gemini(s)) = sig {
                    part["thoughtSignature"] = s.as_str().into();
                }
                parts.push(part);
            }
            Part::ToolCall { id, name, args, sig } => {
                let mut part = json!({ "functionCall": { "id": id, "name": name, "args": parse_args(args) } });
                if let Some(Sig::Gemini(s)) = sig {
                    part["thoughtSignature"] = s.as_str().into();
                }
                parts.push(part);
            }
            _ => {}
        }
    }
    json!({
        "candidates": [{
            "content": { "role": "model", "parts": parts },
            "finishReason": finish_str(agg.finish.unwrap_or_default()), "index": 0
        }],
        "usageMetadata": usage_json(&agg.usage),
        "modelVersion": model,
        "responseId": new_id(""),
    })
}
