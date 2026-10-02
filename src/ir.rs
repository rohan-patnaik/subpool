//! Provider-neutral request / response model.
//!
//! Every client format parses into [`Request`] and every provider stream is
//! decoded into [`Event`]s, so N formats need 2N translators instead of N².

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// OpenAI /v1/chat/completions
    Chat,
    /// OpenAI /v1/responses (Codex)
    Responses,
    /// Anthropic /v1/messages
    Claude,
    /// Google generateContent
    Gemini,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Chat => "openai",
            Format::Responses => "responses",
            Format::Claude => "claude",
            Format::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub tool_choice: ToolChoice,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    pub reasoning: Option<Reasoning>,
    pub response_format: Option<ResponseFormat>,
    pub parallel_tool_calls: Option<bool>,
    /// Chat clients that asked for usage in the final stream chunk.
    pub include_usage: bool,
    /// Responses API freeform tools (their single argument is `input`).
    pub custom_tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sig {
    Claude(String),
    /// Codex reasoning item id + encrypted_content.
    Codex {
        id: Option<String>,
        encrypted: String,
    },
    Gemini(String),
}

#[derive(Debug, Clone)]
pub enum Part {
    Text(String),
    Image(Image),
    Reasoning { text: String, sig: Option<Sig> },
    RedactedReasoning(String),
    ToolCall { id: String, name: String, args: String, sig: Option<Sig> },
    ToolResult { id: String, name: Option<String>, content: Vec<Part>, is_error: bool },
}

#[derive(Debug, Clone)]
pub enum Image {
    Base64 { mime: String, data: String },
    Url(String),
}

impl Image {
    pub fn from_url(url: &str) -> Image {
        if let Some(rest) = url.strip_prefix("data:")
            && let Some((meta, data)) = rest.split_once(',')
        {
            let mime = meta.trim_end_matches(";base64").to_string();
            return Image::Base64 { mime, data: data.to_string() };
        }
        Image::Url(url.to_string())
    }

    pub fn to_url(&self) -> String {
        match self {
            Image::Base64 { mime, data } => format!("data:{mime};base64,{data}"),
            Image::Url(u) => u.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Tool(String),
}

#[derive(Debug, Clone, Default)]
pub struct Reasoning {
    /// minimal | low | medium | high | xhigh | max
    pub effort: Option<String>,
    pub budget: Option<u64>,
    /// Explicitly disabled by the client.
    pub disabled: bool,
}

impl Reasoning {
    pub fn budget_tokens(&self) -> Option<u64> {
        if self.disabled {
            return None;
        }
        self.budget.or_else(|| {
            Some(match self.effort.as_deref()? {
                "none" => return None,
                "minimal" => 1024,
                "low" => 4096,
                "medium" => 10_000,
                "high" => 24_000,
                _ => 32_000,
            })
        })
    }

    pub fn effort_level(&self) -> Option<String> {
        if self.disabled {
            return Some("none".into());
        }
        if let Some(e) = &self.effort {
            return Some(e.clone());
        }
        let b = self.budget?;
        Some(
            match b {
                0 => "none",
                1..=2048 => "low",
                2049..=12_000 => "medium",
                _ => "high",
            }
            .into(),
        )
    }
}

#[derive(Debug, Clone)]
pub enum ResponseFormat {
    JsonObject,
    JsonSchema { name: String, schema: Value, strict: bool },
}

// ---------------------------------------------------------------- stream events

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Finish {
    #[default]
    Stop,
    Length,
    ToolCalls,
    Filter,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl Usage {
    pub fn merge(&mut self, o: &Usage) {
        self.input = self.input.max(o.input);
        self.output = self.output.max(o.output);
        self.cache_read = self.cache_read.max(o.cache_read);
        self.cache_write = self.cache_write.max(o.cache_write);
        self.reasoning = self.reasoning.max(o.reasoning);
    }
    /// Prompt tokens including cached ones (OpenAI semantics).
    pub fn prompt_total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Start { id: Option<String>, model: Option<String> },
    Text(String),
    Reasoning(String),
    ReasoningSig(Sig),
    RedactedReasoning(String),
    ToolStart { key: usize, id: String, name: String },
    ToolArgs { key: usize, delta: String },
    ToolSig { key: usize, sig: Sig },
    Usage(Usage),
    Finish(Finish),
    Error { status: u16, message: String },
}

/// Collects a stream of events into a complete assistant turn.
#[derive(Debug, Default, Clone)]
pub struct Aggregate {
    pub id: Option<String>,
    pub model: Option<String>,
    pub parts: Vec<Part>,
    pub usage: Usage,
    pub finish: Option<Finish>,
    pub error: Option<(u16, String)>,
    tool_index: Vec<(usize, usize)>,
}

impl Aggregate {
    pub fn push(&mut self, ev: &Event) {
        match ev {
            Event::Start { id, model } => {
                if self.id.is_none() {
                    self.id = id.clone();
                }
                if self.model.is_none() {
                    self.model = model.clone();
                }
            }
            Event::Text(t) => match self.parts.last_mut() {
                Some(Part::Text(s)) => s.push_str(t),
                _ => self.parts.push(Part::Text(t.clone())),
            },
            Event::Reasoning(t) => match self.parts.last_mut() {
                Some(Part::Reasoning { text, sig: None }) => text.push_str(t),
                _ => self.parts.push(Part::Reasoning { text: t.clone(), sig: None }),
            },
            Event::ReasoningSig(s) => match self.parts.last_mut() {
                Some(Part::Reasoning { sig, .. }) if sig.is_none() => *sig = Some(s.clone()),
                _ => self.parts.push(Part::Reasoning { text: String::new(), sig: Some(s.clone()) }),
            },
            Event::RedactedReasoning(d) => self.parts.push(Part::RedactedReasoning(d.clone())),
            Event::ToolStart { key, id, name } => {
                self.tool_index.push((*key, self.parts.len()));
                self.parts.push(Part::ToolCall { id: id.clone(), name: name.clone(), args: String::new(), sig: None });
            }
            Event::ToolArgs { key, delta } => {
                if let Some(Part::ToolCall { args, .. }) = self.tool_part(*key) {
                    args.push_str(delta);
                }
            }
            Event::ToolSig { key, sig: s } => {
                if let Some(Part::ToolCall { sig, .. }) = self.tool_part(*key) {
                    *sig = Some(s.clone());
                }
            }
            Event::Usage(u) => self.usage.merge(u),
            Event::Finish(f) => self.finish = Some(*f),
            Event::Error { status, message } => self.error = Some((*status, message.clone())),
        }
    }

    fn tool_part(&mut self, key: usize) -> Option<&mut Part> {
        let idx = self.tool_index.iter().rev().find(|(k, _)| *k == key).map(|(_, i)| *i)?;
        self.parts.get_mut(idx)
    }

    pub fn finish_reason(&self) -> Finish {
        match self.finish {
            Some(Finish::Stop) | None if self.has_tool_calls() => Finish::ToolCalls,
            Some(f) => f,
            None => Finish::Stop,
        }
    }

    pub fn has_tool_calls(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, Part::ToolCall { .. }))
    }

    pub fn text(&self) -> String {
        self.parts.iter().filter_map(|p| if let Part::Text(t) = p { Some(t.as_str()) } else { None }).collect()
    }

    pub fn reasoning_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| if let Part::Reasoning { text, .. } = p { Some(text.as_str()) } else { None })
            .collect()
    }
}

// ---------------------------------------------------------------- helpers

pub fn new_id(prefix: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}{}", &id[..24])
}

pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Parses tool arguments, falling back to an empty object for invalid JSON.
pub fn parse_args(args: &str) -> Value {
    if args.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({ "_raw": args }))
}

/// Flattens the text parts of tool result content.
pub fn parts_text(parts: &[Part]) -> String {
    let mut out = String::new();
    for p in parts {
        if let Part::Text(t) = p {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

/// Merges adjacent messages with the same role (required by Claude and Gemini).
pub fn merge_adjacent(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        if m.parts.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.role == m.role => last.parts.extend(m.parts),
            _ => out.push(m),
        }
    }
    out
}

/// Strip a trailing `(effort)` / `(budget)` suffix: `gpt-6-astra(high)` or `claude-opus-5-5(16000)`.
pub fn split_model_suffix(model: &str) -> (String, Option<Reasoning>) {
    let m = model.trim();
    if let Some(open) = m.rfind('(')
        && m.ends_with(')')
        && open > 0
    {
        let inner = &m[open + 1..m.len() - 1];
        let base = m[..open].to_string();
        let r = if let Ok(n) = inner.parse::<u64>() {
            Reasoning { budget: Some(n), disabled: n == 0, ..Default::default() }
        } else {
            let e = inner.to_ascii_lowercase();
            Reasoning { disabled: e == "none", effort: Some(e), ..Default::default() }
        };
        return (base, Some(r));
    }
    (m.to_string(), None)
}
