//! Accounts: OAuth credential files + API keys from config, with selection,
//! cooldowns and usage counters.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::{Config, ModelAlias, Routing};
use crate::ir::Format;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Gemini,
    #[serde(rename = "openai-compat")]
    Compat,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Gemini => "gemini",
            Provider::Compat => "openai-compat",
        }
    }

    pub fn native(self) -> Format {
        match self {
            Provider::Claude => Format::Claude,
            Provider::Codex => Format::Responses,
            Provider::Gemini => Format::Gemini,
            Provider::Compat => Format::Chat,
        }
    }

    /// Built-in model families served by this provider.
    pub fn serves(self, model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        match self {
            Provider::Claude => m.starts_with("claude-"),
            Provider::Codex => {
                m.starts_with("gpt-")
                    || m.starts_with("codex-")
                    || (m.len() >= 2 && m.starts_with('o') && m.as_bytes()[1].is_ascii_digit())
            }
            Provider::Gemini => m.starts_with("gemini-") || m.starts_with("gemma-"),
            Provider::Compat => false,
        }
    }

    pub fn builtin_models(self) -> &'static [&'static str] {
        match self {
            Provider::Claude => &[
                "claude-fable-5-1",
                "claude-opus-5-5",
                "claude-sonnet-5-5",
                "claude-opus-5",
                "claude-sonnet-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "claude-haiku-4-5-20251001",
            ],
            Provider::Codex => &[
                "gpt-6-astra",
                "gpt-6.1-sol",
                "gpt-6-sol",
                "gpt-6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
            ],
            Provider::Gemini => &[
                "gemini-3.8-flash",
                "gemini-3.7-flash",
                "gemini-3.1-pro-preview",
                "gemini-3.5-flash-lite",
                "gemini-2.5-pro",
                "gemini-2.5-flash",
            ],
            Provider::Compat => &[],
        }
    }
}

#[derive(Debug, Clone)]
pub enum Credential {
    OAuth(OAuth),
    ApiKey { key: String, base_url: Option<String> },
}

#[derive(Debug, Clone, Default)]
pub struct OAuth {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub email: Option<String>,
    /// ChatGPT account id (Codex) or Anthropic account uuid (Claude).
    pub account_id: Option<String>,
    /// Optional API base override (e.g. a gateway), from `base_url` in the file.
    pub base_url: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Counters {
    pub requests: u64,
    pub failures: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
}

#[derive(Debug, Default)]
pub struct AccountState {
    pub disabled: bool,
    /// Cooldowns keyed by model ("*" = whole account).
    pub cooldowns: HashMap<String, DateTime<Utc>>,
    pub strikes: u32,
    pub last_error: Option<String>,
    pub last_used: Option<DateTime<Utc>>,
    pub counters: Counters,
}

pub struct Account {
    pub id: String,
    pub provider: Provider,
    pub label: String,
    pub path: Option<PathBuf>,
    /// Display name of the openai-compatibility group.
    pub group: Option<String>,
    /// Public model name -> upstream model name. Empty = provider default families.
    pub models: Vec<ModelAlias>,
    pub headers: BTreeMap<String, String>,
    pub proxy_url: Option<String>,
    pub cred: RwLock<Credential>,
    pub state: Mutex<AccountState>,
    pub refresh_lock: tokio::sync::Mutex<()>,
    /// Stable per-account device id for Claude Code request shaping.
    pub device_id: String,
    pub session_id: String,
}

impl Account {
    pub fn is_oauth(&self) -> bool {
        matches!(*self.cred.read(), Credential::OAuth(_))
    }

    /// Upstream model name if this account can serve `model`.
    pub fn resolve(&self, model: &str) -> Option<String> {
        if !self.models.is_empty() {
            return self.models.iter().find(|m| m.public().eq_ignore_ascii_case(model)).map(|m| m.name.clone());
        }
        self.provider.serves(model).then(|| model.to_string())
    }

    pub fn public_models(&self) -> Vec<String> {
        if !self.models.is_empty() {
            return self.models.iter().map(|m| m.public().to_string()).collect();
        }
        self.provider.builtin_models().iter().map(|s| s.to_string()).collect()
    }

    pub fn cooling_until(&self, model: &str) -> Option<DateTime<Utc>> {
        let st = self.state.lock();
        let now = Utc::now();
        [st.cooldowns.get("*"), st.cooldowns.get(model)].into_iter().flatten().filter(|t| **t > now).max().copied()
    }

    pub fn cool(&self, model: Option<&str>, until: DateTime<Utc>, reason: &str) {
        let mut st = self.state.lock();
        st.cooldowns.insert(model.unwrap_or("*").to_string(), until);
        st.last_error = Some(reason.to_string());
    }

    pub fn record_ok(&self) {
        let mut st = self.state.lock();
        st.strikes = 0;
        st.last_error = None;
    }

    pub fn snapshot(&self) -> Value {
        let st = self.state.lock();
        let now = Utc::now();
        let cooldowns: BTreeMap<&str, String> =
            st.cooldowns.iter().filter(|(_, t)| **t > now).map(|(k, t)| (k.as_str(), t.to_rfc3339())).collect();
        let (kind, expires, email) = match &*self.cred.read() {
            Credential::OAuth(o) => ("oauth", o.expires_at.map(|t| t.to_rfc3339()), o.email.clone()),
            Credential::ApiKey { .. } => ("api-key", None, None),
        };
        serde_json::json!({
            "id": self.id,
            "provider": self.provider,
            "label": self.label,
            "email": email,
            "kind": kind,
            "group": self.group,
            "file": self.path.as_ref().and_then(|p| p.file_name()).map(|f| f.to_string_lossy().to_string()),
            "disabled": st.disabled,
            "cooldowns": cooldowns,
            "last_error": st.last_error,
            "last_used": st.last_used.map(|t| t.to_rfc3339()),
            "expires_at": expires,
            "counters": st.counters,
            "models": self.public_models(),
        })
    }
}

// --------------------------------------------------------------------- loading

fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(s) => DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc)),
        Value::Number(n) => {
            let n = n.as_i64()?;
            let secs = if n > 10_000_000_000 { n / 1000 } else { n };
            DateTime::from_timestamp(secs, 0)
        }
        _ => None,
    }
}

pub fn read_oauth_file(path: &Path) -> Option<(Provider, OAuth, bool, Map<String, Value>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let map: Map<String, Value> = serde_json::from_str(&text).ok()?;
    let provider = match map.get("type").and_then(Value::as_str)? {
        "claude" | "anthropic" => Provider::Claude,
        "codex" | "openai" => Provider::Codex,
        _ => return None,
    };
    let s = |k: &str| map.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let oauth = OAuth {
        access_token: s("access_token").unwrap_or_default(),
        refresh_token: s("refresh_token").unwrap_or_default(),
        expires_at: map.get("expired").or_else(|| map.get("expires_at")).and_then(parse_time),
        email: s("email"),
        account_id: s("account_id").or_else(|| s("account_uuid")),
        base_url: s("base_url"),
    };
    let disabled = map.get("disabled").and_then(Value::as_bool).unwrap_or(false);
    Some((provider, oauth, disabled, map))
}

/// Writes refreshed tokens back into the credential file, keeping unknown fields.
pub fn write_oauth_file(path: &Path, provider: Provider, o: &OAuth, extra: &[(&str, Value)]) -> std::io::Result<()> {
    let mut map: Map<String, Value> =
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    map.insert("type".into(), provider.as_str().into());
    map.insert("access_token".into(), o.access_token.clone().into());
    map.insert("refresh_token".into(), o.refresh_token.clone().into());
    if let Some(e) = &o.email {
        map.insert("email".into(), e.clone().into());
    }
    if let Some(t) = o.expires_at {
        map.insert("expired".into(), t.to_rfc3339().into());
    }
    if let Some(a) = &o.account_id {
        let key = if provider == Provider::Codex { "account_id" } else { "account_uuid" };
        map.insert(key.into(), a.clone().into());
    }
    map.insert("last_refresh".into(), Utc::now().to_rfc3339().into());
    for (k, v) in extra {
        map.insert((*k).into(), v.clone());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&Value::Object(map))?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(tmp, path)
}

pub fn set_file_disabled(path: &Path, disabled: bool) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut map: Map<String, Value> = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    if disabled {
        map.insert("disabled".into(), true.into());
    } else {
        map.remove("disabled");
    }
    std::fs::write(path, serde_json::to_vec_pretty(&Value::Object(map))?)
}

fn mask(key: &str) -> String {
    let k = key.trim();
    if k.len() <= 10 {
        return format!("{}…", k.get(..3).unwrap_or(""));
    }
    format!("{}…{}", &k[..6], &k[k.len() - 4..])
}

fn key_id(prefix: &str, key: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(key.as_bytes());
    format!("{prefix}:{}", &hex::encode(h)[..10])
}

fn random_hex(n: usize) -> String {
    let bytes: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    hex::encode(bytes)
}

struct Spec {
    id: String,
    provider: Provider,
    label: String,
    path: Option<PathBuf>,
    group: Option<String>,
    models: Vec<ModelAlias>,
    headers: BTreeMap<String, String>,
    proxy_url: Option<String>,
    cred: Credential,
    disabled: bool,
    device_id: Option<String>,
}

fn collect(cfg: &Config) -> Vec<Spec> {
    let mut specs = Vec::new();
    let dir = cfg.auth_dir();
    let mut files: Vec<PathBuf> =
        std::fs::read_dir(&dir).map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect()).unwrap_or_default();
    files.sort();
    for path in files {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some((provider, oauth, disabled, map)) = read_oauth_file(&path) else { continue };
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let device_id = map
            .get("claude_device_ids")
            .and_then(|v| v.get(0))
            .and_then(Value::as_str)
            .filter(|s| s.len() == 64)
            .map(String::from);
        specs.push(Spec {
            id: format!("file:{name}"),
            provider,
            label: oauth.email.clone().unwrap_or_else(|| name.trim_end_matches(".json").to_string()),
            path: Some(path),
            group: None,
            models: vec![],
            headers: BTreeMap::new(),
            proxy_url: map.get("proxy_url").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from),
            cred: Credential::OAuth(oauth),
            disabled,
            device_id,
        });
    }
    let keys = [
        (Provider::Claude, &cfg.claude_api_key),
        (Provider::Codex, &cfg.codex_api_key),
        (Provider::Gemini, &cfg.gemini_api_key),
    ];
    for (provider, entries) in keys {
        for e in entries.iter().filter(|e| !e.api_key.trim().is_empty()) {
            specs.push(Spec {
                id: key_id(provider.as_str(), &e.api_key),
                provider,
                label: e.label.clone().unwrap_or_else(|| mask(&e.api_key)),
                path: None,
                group: None,
                models: e.models.clone(),
                headers: e.headers.clone(),
                proxy_url: e.proxy_url.clone(),
                cred: Credential::ApiKey { key: e.api_key.trim().to_string(), base_url: e.base_url.clone() },
                disabled: false,
                device_id: None,
            });
        }
    }
    for c in &cfg.openai_compatibility {
        for k in c.api_keys.iter().filter(|k| !k.trim().is_empty()) {
            specs.push(Spec {
                id: key_id(&format!("compat-{}", c.name), k),
                provider: Provider::Compat,
                label: format!("{} {}", c.name, mask(k)),
                path: None,
                group: Some(c.name.clone()),
                models: c.models.clone(),
                headers: c.headers.clone(),
                proxy_url: c.proxy_url.clone(),
                cred: Credential::ApiKey { key: k.trim().to_string(), base_url: Some(c.base_url.clone()) },
                disabled: false,
                device_id: None,
            });
        }
        // Keyless local endpoints (Ollama, LM Studio, ...)
        if c.api_keys.iter().all(|k| k.trim().is_empty()) && !c.base_url.is_empty() {
            specs.push(Spec {
                id: format!("compat-{}:nokey", c.name),
                provider: Provider::Compat,
                label: c.name.clone(),
                path: None,
                group: Some(c.name.clone()),
                models: c.models.clone(),
                headers: c.headers.clone(),
                proxy_url: c.proxy_url.clone(),
                cred: Credential::ApiKey { key: String::new(), base_url: Some(c.base_url.clone()) },
                disabled: false,
                device_id: None,
            });
        }
    }
    specs
}

// ------------------------------------------------------------------------ pool

#[derive(Default)]
pub struct Pool {
    accounts: RwLock<Vec<Arc<Account>>>,
    cursor: Mutex<HashMap<String, usize>>,
}

pub enum Pick {
    Ok(Arc<Account>, String),
    /// Every candidate is cooling down; earliest availability.
    Cooling(DateTime<Utc>),
    None,
}

impl Pool {
    pub fn reload(&self, cfg: &Config) {
        let specs = collect(cfg);
        let old: HashMap<String, Arc<Account>> =
            self.accounts.read().iter().map(|a| (a.id.clone(), a.clone())).collect();
        let mut next = Vec::with_capacity(specs.len());
        for s in specs {
            if let Some(prev) = old.get(&s.id) {
                // Keep counters and cooldowns; refresh credentials from disk/config.
                let same_shape =
                    prev.models.len() == s.models.len() && prev.headers == s.headers && prev.proxy_url == s.proxy_url;
                if same_shape {
                    *prev.cred.write() = s.cred;
                    prev.state.lock().disabled = s.disabled;
                    next.push(prev.clone());
                    continue;
                }
            }
            let state = AccountState {
                disabled: s.disabled,
                counters: old.get(&s.id).map(|p| p.state.lock().counters.clone()).unwrap_or_default(),
                ..Default::default()
            };
            next.push(Arc::new(Account {
                id: s.id,
                provider: s.provider,
                label: s.label,
                path: s.path,
                group: s.group,
                models: s.models,
                headers: s.headers,
                proxy_url: s.proxy_url,
                cred: RwLock::new(s.cred),
                state: Mutex::new(state),
                refresh_lock: tokio::sync::Mutex::new(()),
                device_id: s.device_id.unwrap_or_else(|| random_hex(32)),
                session_id: uuid::Uuid::new_v4().to_string(),
            }));
        }
        *self.accounts.write() = next;
    }

    pub fn all(&self) -> Vec<Arc<Account>> {
        self.accounts.read().clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Account>> {
        self.accounts.read().iter().find(|a| a.id == id).cloned()
    }

    /// Public models served by at least one enabled account.
    pub fn models(&self) -> Vec<(String, Provider)> {
        let mut seen = std::collections::BTreeMap::new();
        for a in self.accounts.read().iter() {
            if a.state.lock().disabled {
                continue;
            }
            for m in a.public_models() {
                seen.entry(m).or_insert(a.provider);
            }
        }
        seen.into_iter().collect()
    }

    pub fn pick(&self, model: &str, exclude: &[String], routing: Routing, pinned: Option<&str>) -> Pick {
        let accounts = self.accounts.read();
        let mut candidates: Vec<(&Arc<Account>, String)> = Vec::new();
        let mut earliest: Option<DateTime<Utc>> = None;
        for a in accounts.iter() {
            if exclude.contains(&a.id) || a.state.lock().disabled {
                continue;
            }
            let Some(upstream) = a.resolve(model) else { continue };
            if let Some(until) = a.cooling_until(model) {
                earliest = Some(earliest.map_or(until, |e| e.min(until)));
                continue;
            }
            candidates.push((a, upstream));
        }
        if candidates.is_empty() {
            return earliest.map(Pick::Cooling).unwrap_or(Pick::None);
        }
        if let Some(pin) = pinned
            && let Some((a, m)) = candidates.iter().find(|(a, _)| a.id == pin)
        {
            return Pick::Ok((*a).clone(), m.clone());
        }
        let idx = match routing {
            Routing::FillFirst => 0,
            Routing::RoundRobin => {
                let mut cur = self.cursor.lock();
                let c = cur.entry(model.to_ascii_lowercase()).or_insert(0);
                let i = *c % candidates.len();
                *c = c.wrapping_add(1);
                i
            }
        };
        let (a, m) = &candidates[idx];
        Pick::Ok((*a).clone(), m.clone())
    }
}
