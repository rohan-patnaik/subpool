use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    /// Interface to bind. Use 0.0.0.0 to expose on the network (set api-keys first).
    pub host: String,
    pub port: u16,
    /// Directory holding OAuth credential files. Uses the CLIProxyAPI file format,
    /// so pointing this at ~/.cli-proxy-api reuses existing logins.
    pub auth_dir: String,
    /// Keys clients must send (Authorization: Bearer, x-api-key or x-goog-api-key).
    /// Empty means no client authentication.
    pub api_keys: Vec<String>,
    /// Protects the dashboard and management API. Empty means localhost-only access.
    pub management_key: String,
    /// Optional upstream proxy (http://, https://, socks5://).
    pub proxy_url: String,
    /// How many different accounts to try before giving up on a request.
    pub request_retry: u32,
    /// round-robin or fill-first.
    pub routing: Routing,
    /// Keep an upstream websocket open to Codex when clients connect over websocket.
    pub codex_websockets: bool,
    /// Shape non-Claude-Code requests on Claude OAuth accounts the way Claude Code sends them.
    #[serde(alias = "claude-cloak")]
    pub claude_code_mode: bool,
    pub debug: bool,
    pub claude_api_key: Vec<KeyEntry>,
    pub codex_api_key: Vec<KeyEntry>,
    pub gemini_api_key: Vec<KeyEntry>,
    pub openai_compatibility: Vec<CompatEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Routing {
    #[default]
    RoundRobin,
    FillFirst,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct KeyEntry {
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Restrict (and optionally rename) models served by this key.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelAlias>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct CompatEntry {
    pub name: String,
    pub base_url: String,
    pub api_keys: Vec<String>,
    pub models: Vec<ModelAlias>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct ModelAlias {
    /// Model name sent upstream.
    pub name: String,
    /// Name clients use. Defaults to `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

impl ModelAlias {
    pub fn public(&self) -> &str {
        self.alias.as_deref().filter(|a| !a.is_empty()).unwrap_or(&self.name)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8317,
            auth_dir: "~/.subpool".into(),
            api_keys: vec![],
            management_key: String::new(),
            proxy_url: String::new(),
            request_retry: 3,
            routing: Routing::RoundRobin,
            codex_websockets: true,
            claude_code_mode: true,
            debug: false,
            claude_api_key: vec![],
            codex_api_key: vec![],
            gemini_api_key: vec![],
            openai_compatibility: vec![],
        }
    }
}

pub const TEMPLATE: &str = r#"# Subpool configuration. Changes are picked up automatically.

host: "127.0.0.1"          # use 0.0.0.0 to expose on your network (set api-keys first!)
port: 8317
auth-dir: "~/.subpool"     # OAuth credentials (point at ~/.cli-proxy-api to reuse CLIProxyAPI logins)

# Keys your clients must send. Leave empty to allow anyone who can reach the port.
api-keys: []

# Protects the dashboard + management API. Empty = only reachable from localhost.
management-key: ""

proxy-url: ""               # optional upstream proxy, e.g. socks5://127.0.0.1:1080
request-retry: 3            # accounts to try before failing a request
routing: round-robin        # round-robin | fill-first
codex-websockets: true      # native upstream websocket for Codex websocket clients
claude-code-mode: true      # send requests on Claude OAuth accounts in Claude Code's request shape
debug: false

# API keys (optional). OAuth accounts are added with `subpool login` or the dashboard.
claude-api-key: []
#  - api-key: "sk-ant-..."
#    base-url: "https://api.anthropic.com"   # optional

codex-api-key: []
#  - api-key: "sk-..."
#    base-url: "https://api.openai.com/v1"   # optional

gemini-api-key: []
#  - api-key: "AIza..."

openai-compatibility: []
#  - name: openrouter
#    base-url: "https://openrouter.ai/api/v1"
#    api-keys: ["sk-or-..."]
#    models:
#      - name: "moonshotai/kimi-k3"
#        alias: "kimi-k3"
"#;

pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| (p == "~").then_some(""))
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(path, TEMPLATE).with_context(|| format!("writing {}", path.display()))?;
            tracing::info!("created default config at {}", path.display());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let cfg: Config = serde_yaml::from_str(text).context("invalid config")?;
        Ok(cfg)
    }

    pub fn auth_dir(&self) -> PathBuf {
        expand_home(&self.auth_dir)
    }

    pub fn is_loopback(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "::1")
    }
}
