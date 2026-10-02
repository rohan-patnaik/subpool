//! Builds the HTTP request for each upstream provider.

use axum::http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::accounts::{Account, Credential};
use crate::config::Config;

pub const CLAUDE_API: &str = "https://api.anthropic.com";
pub const CODEX_BACKEND: &str = "https://chatgpt.com/backend-api/codex";
pub const OPENAI_API: &str = "https://api.openai.com/v1";
pub const GEMINI_API: &str = "https://generativelanguage.googleapis.com";

pub const CC_VERSION: &str = "2.1.280";
pub const CC_USER_AGENT: &str = "claude-cli/2.1.280 (external, cli)";
const CC_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const CC_FINGERPRINT_SALT: &str = "59cf53e54c78";
pub const CODEX_USER_AGENT: &str = "codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)";
pub const CODEX_ORIGINATOR: &str = "codex-tui";
pub const CODEX_WS_BETA: &str = "responses_websockets=2026-02-06";

pub struct Prepared {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

pub struct Target<'a> {
    pub acct: &'a Account,
    pub cfg: &'a Config,
    pub client_headers: &'a HeaderMap,
    pub model: &'a str,
    pub passthrough: bool,
    pub stream: bool,
    pub count_tokens: bool,
}

/// Headers never copied from the client to an upstream.
const HOP: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "content-length",
    "connection",
    "accept-encoding",
    "transfer-encoding",
    "cookie",
    "upgrade",
    "te",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "origin",
    "referer",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
];

fn header(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn is_claude_code(h: &HeaderMap) -> bool {
    header(h, "user-agent").is_some_and(|ua| ua.to_ascii_lowercase().starts_with("claude-cli/"))
}

fn creds(acct: &Account) -> (String, Option<String>, bool, Option<String>) {
    match &*acct.cred.read() {
        Credential::OAuth(o) => (
            o.access_token.clone(),
            o.base_url.as_ref().map(|b| b.trim_end_matches('/').to_string()),
            true,
            o.account_id.clone(),
        ),
        Credential::ApiKey { key, base_url } => (
            key.clone(),
            base_url.clone().filter(|b| !b.trim().is_empty()).map(|b| b.trim_end_matches('/').to_string()),
            false,
            None,
        ),
    }
}

fn push_custom(acct: &Account, headers: &mut Vec<(String, String)>) {
    for (k, v) in &acct.headers {
        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
        headers.push((k.clone(), v.clone()));
    }
}

pub fn prepare(t: &Target, body: Value) -> Prepared {
    use crate::accounts::Provider::*;
    let mut p = match t.acct.provider {
        Claude => claude(t, body),
        Codex => codex(t, body),
        Gemini => gemini(t, body),
        Compat => compat(t, body),
    };
    push_custom(t.acct, &mut p.headers);
    p
}

// ---------------------------------------------------------------------- claude

fn claude(t: &Target, mut body: Value) -> Prepared {
    let (token, base, oauth, account_uuid) = creds(t.acct);
    let base = base.unwrap_or_else(|| CLAUDE_API.into());
    let path = if t.count_tokens { "/v1/messages/count_tokens" } else { "/v1/messages" };
    let url = if oauth { format!("{base}{path}?beta=true") } else { format!("{base}{path}") };
    body["model"] = t.model.into();
    strip_foreign_thinking(&mut body);

    let native_cc = t.passthrough && is_claude_code(t.client_headers);
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut betas: Vec<String> = Vec::new();
    if let Some(b) = header(t.client_headers, "anthropic-beta") {
        betas.extend(b.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }

    if native_cc {
        // Claude Code already speaks the right dialect: forward it untouched.
        for (k, v) in t.client_headers {
            let name = k.as_str();
            if HOP.contains(&name) || name == "anthropic-beta" {
                continue;
            }
            if let Ok(v) = v.to_str() {
                headers.push((name.to_string(), v.to_string()));
            }
        }
    } else if oauth && t.cfg.claude_code_mode {
        let mut cc = vec![
            "claude-code-20250219",
            "oauth-2025-04-20",
            "interleaved-thinking-2025-05-14",
            "context-management-2025-06-27",
            "prompt-caching-scope-2026-01-05",
        ];
        if body["output_config"]["effort"].is_string() {
            cc.push("effort-2025-11-24");
        }
        if body["output_config"]["format"].is_object() {
            cc.push("structured-outputs-2025-12-15");
        }
        let caller = std::mem::take(&mut betas);
        betas = cc.into_iter().map(String::from).collect();
        betas.extend(caller);
        apply_claude_code_shape(&mut body, t.acct, account_uuid.as_deref(), t.count_tokens);
        headers.extend(
            [
                ("anthropic-version", "2023-06-01"),
                ("anthropic-dangerous-direct-browser-access", "true"),
                ("x-app", "cli"),
                ("user-agent", CC_USER_AGENT),
                ("x-stainless-lang", "js"),
                ("x-stainless-package-version", "0.112.1"),
                ("x-stainless-os", "MacOS"),
                ("x-stainless-arch", "arm64"),
                ("x-stainless-runtime", "node"),
                ("x-stainless-runtime-version", "v26.3.0"),
                ("x-stainless-retry-count", "0"),
                ("x-stainless-timeout", "600"),
                ("x-claude-code-session-id", t.acct.session_id.as_str()),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
    } else {
        let version = header(t.client_headers, "anthropic-version").unwrap_or_else(|| "2023-06-01".into());
        headers.push(("anthropic-version".into(), version));
        headers.push(("user-agent".into(), format!("Subpool/{}", env!("CARGO_PKG_VERSION"))));
        if body["output_config"]["effort"].is_string() && !betas.iter().any(|b| b.starts_with("effort-")) {
            betas.push("effort-2025-11-24".into());
        }
    }
    if oauth && !betas.iter().any(|b| b == "oauth-2025-04-20") {
        betas.insert(0, "oauth-2025-04-20".into());
    }
    let mut seen = std::collections::HashSet::new();
    betas.retain(|b| seen.insert(b.clone()));
    if !betas.is_empty() {
        headers.push(("anthropic-beta".into(), betas.join(",")));
    }
    if oauth {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    } else {
        headers.push(("x-api-key".into(), token));
    }
    headers.retain(|(k, _)| k != "content-type" && k != "accept");
    headers.push(("content-type".into(), "application/json".into()));
    headers.push(("accept".into(), if t.stream { "text/event-stream" } else { "application/json" }.into()));
    Prepared { url, headers, body }
}

/// Thinking blocks that carried another provider's reasoning through a Claude
/// client would fail Anthropic's signature check.
fn strip_foreign_thinking(body: &mut Value) {
    for m in body["messages"].as_array_mut().into_iter().flatten() {
        if let Some(blocks) = m["content"].as_array_mut() {
            blocks.retain(|b| {
                !(b["type"] == "thinking" && b["signature"].as_str().is_some_and(|s| s.starts_with("cpx-")))
            });
        }
    }
}

/// JavaScript-compatible 3-char build fingerprint Claude Code puts in its billing header.
fn cc_fingerprint(message: &str) -> String {
    let units: Vec<u16> = message.encode_utf16().collect();
    let sampled: Vec<u16> = [4usize, 7, 20].iter().map(|&i| units.get(i).copied().unwrap_or(b'0' as u16)).collect();
    let input = format!("{CC_FINGERPRINT_SALT}{}{CC_VERSION}", String::from_utf16_lossy(&sampled));
    hex::encode(Sha256::digest(input.as_bytes()))[..3].to_string()
}

fn first_user_text(body: &Value) -> (Option<usize>, String) {
    let Some(msgs) = body["messages"].as_array() else { return (None, String::new()) };
    let Some(idx) = msgs.iter().position(|m| m["role"] == "user") else { return (None, String::new()) };
    let text = match &msgs[idx]["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .find(|t| !t.starts_with("<system-reminder>"))
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    };
    (Some(idx), text)
}

/// Makes a third-party request look like Claude Code: Claude Code system
/// prompt on top, caller instructions moved into the first user turn, and a
/// Claude-Code-shaped metadata.user_id.
fn apply_claude_code_shape(body: &mut Value, acct: &Account, account_uuid: Option<&str>, count_tokens: bool) {
    let caller: Vec<String> = match &body["system"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str().map(String::from)).collect(),
        _ => vec![],
    }
    .into_iter()
    .filter(|t| !t.trim().is_empty() && t != CC_IDENTITY && !t.starts_with("x-anthropic-billing-header"))
    .collect();

    let (first_user, text) = first_user_text(body);
    let billing =
        format!("x-anthropic-billing-header: cc_version={CC_VERSION}.{}; cc_entrypoint=cli;", cc_fingerprint(&text));
    body["system"] = json!([
        { "type": "text", "text": billing },
        { "type": "text", "text": CC_IDENTITY, "cache_control": { "type": "ephemeral" } }
    ]);

    if let (Some(idx), false) = (first_user, caller.is_empty()) {
        let reminders: Vec<Value> = caller
            .iter()
            .map(|t| json!({ "type": "text", "text": format!("<system-reminder>\n{}\n</system-reminder>", t.trim_end()) }))
            .collect();
        let content = &mut body["messages"][idx]["content"];
        let mut blocks = match content.take() {
            Value::String(s) => vec![json!({ "type": "text", "text": s })],
            Value::Array(a) => a,
            _ => vec![],
        };
        let at = blocks.iter().take_while(|b| b["type"] == "tool_result").count();
        blocks.splice(at..at, reminders);
        *content = Value::Array(blocks);
    }

    if count_tokens {
        if let Some(o) = body.as_object_mut() {
            o.remove("metadata");
        }
        return;
    }
    let valid = body["metadata"]["user_id"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .is_some_and(|v| v["device_id"].as_str().is_some_and(|d| d.len() == 64));
    if !valid {
        let user_id = json!({
            "device_id": acct.device_id,
            "account_uuid": account_uuid.unwrap_or_default(),
            "session_id": acct.session_id,
        })
        .to_string();
        if !body["metadata"].is_object() {
            body["metadata"] = json!({});
        }
        body["metadata"]["user_id"] = user_id.into();
    }
}

// ----------------------------------------------------------------------- codex

/// Fields the ChatGPT Codex backend rejects.
const CODEX_STRIP: &[&str] = &[
    "max_output_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "safety_identifier",
    "prompt_cache_retention",
    "generate",
    "user",
    "truncation",
    "stream_options",
    "background",
];

pub fn sanitize_codex_body(body: &mut Value, model: &str, keep_previous: bool) {
    body["model"] = model.into();
    body["store"] = false.into();
    if body["instructions"].is_null() {
        body["instructions"] = "".into();
    }
    if let Some(o) = body.as_object_mut() {
        for k in CODEX_STRIP {
            o.remove(*k);
        }
        if !keep_previous {
            o.remove("previous_response_id");
        }
        if let Some(Value::Array(items)) = o.get_mut("input") {
            // Reasoning tunnelled from another provider can't be decrypted by OpenAI.
            items.retain(|it| {
                !(it["type"] == "reasoning" && it["encrypted_content"].as_str().is_some_and(|e| e.starts_with("cpx-")))
            });
            // Item ids reference server-side state that store=false never kept.
            for it in items.iter_mut() {
                if let Some(io) = it.as_object_mut()
                    && io.get("type").and_then(Value::as_str) != Some("item_reference")
                {
                    io.remove("id");
                }
            }
        }
    }
}

pub fn codex_headers(client: &HeaderMap, token: &str, account_id: Option<&str>, oauth: bool) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = vec![("authorization".into(), format!("Bearer {token}"))];
    for name in [
        "version",
        "session_id",
        "session-id",
        "thread-id",
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "x-codex-beta-features",
        "x-client-request-id",
        "x-codex-window-id",
        "x-openai-internal-codex-responses-lite",
    ] {
        if let Some(v) = header(client, name) {
            h.push((name.into(), v));
        }
    }
    if oauth {
        h.push(("user-agent".into(), CODEX_USER_AGENT.into()));
        h.push(("originator".into(), CODEX_ORIGINATOR.into()));
        if let Some(a) = account_id {
            h.push(("chatgpt-account-id".into(), a.into()));
        }
    } else {
        h.push(("user-agent".into(), format!("Subpool/{}", env!("CARGO_PKG_VERSION"))));
    }
    h
}

fn codex(t: &Target, mut body: Value) -> Prepared {
    let (token, base, oauth, account_id) = creds(t.acct);
    let base = base.unwrap_or_else(|| if oauth { CODEX_BACKEND.into() } else { OPENAI_API.into() });
    if oauth {
        sanitize_codex_body(&mut body, t.model, false);
        body["stream"] = true.into();
    } else {
        body["model"] = t.model.into();
        body["stream"] = t.stream.into();
    }
    let mut headers = codex_headers(t.client_headers, &token, account_id.as_deref(), oauth);
    if let Some(key) = body["prompt_cache_key"].as_str().filter(|_| oauth)
        && !headers.iter().any(|(k, _)| k == "session_id" || k == "session-id")
    {
        headers.push(("session_id".into(), key.to_string()));
    }
    headers.push(("content-type".into(), "application/json".into()));
    headers.push(("accept".into(), if oauth || t.stream { "text/event-stream" } else { "application/json" }.into()));
    Prepared { url: format!("{base}/responses"), headers, body }
}

pub fn codex_ws_url(acct: &Account) -> (String, Vec<(String, String)>) {
    let (token, base, oauth, account_id) = creds(acct);
    let base = base.unwrap_or_else(|| if oauth { CODEX_BACKEND.into() } else { OPENAI_API.into() });
    let ws = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let mut h = codex_headers(&HeaderMap::new(), &token, account_id.as_deref(), oauth);
    h.push(("openai-beta".into(), CODEX_WS_BETA.into()));
    (format!("{ws}/responses"), h)
}

// ---------------------------------------------------------------------- gemini

fn gemini(t: &Target, mut body: Value) -> Prepared {
    let (token, base, _, _) = creds(t.acct);
    let base = base.unwrap_or_else(|| GEMINI_API.into());
    let action = if t.count_tokens {
        "countTokens"
    } else if t.stream {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    if let Some(o) = body.as_object_mut() {
        o.remove("model");
    }
    let headers = vec![
        ("x-goog-api-key".into(), token),
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), format!("Subpool/{}", env!("CARGO_PKG_VERSION"))),
    ];
    Prepared { url: format!("{base}/v1beta/models/{}:{action}", t.model), headers, body }
}

// ---------------------------------------------------------------------- compat

fn compat(t: &Target, mut body: Value) -> Prepared {
    let (token, base, _, _) = creds(t.acct);
    let base = base.unwrap_or_else(|| OPENAI_API.into());
    body["model"] = t.model.into();
    if !t.passthrough {
        body["stream"] = true.into();
    }
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), format!("Subpool/{}", env!("CARGO_PKG_VERSION"))),
    ];
    if !token.is_empty() {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }
    Prepared { url: format!("{base}/chat/completions"), headers, body }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_three_hex_chars() {
        let f = cc_fingerprint("hello world, this is a test");
        assert_eq!(f.len(), 3);
        assert!(f.chars().all(|c| c.is_ascii_hexdigit()));
        // Short messages fall back to '0' for missing positions.
        assert_eq!(cc_fingerprint(""), cc_fingerprint("abc"));
    }
}
