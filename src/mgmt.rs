//! Management API used by the dashboard, plus OAuth login orchestration.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{Credential, Provider, set_file_disabled, write_oauth_file};
use crate::config::{CompatEntry, Config, KeyEntry, ModelAlias};
use crate::oauth;
use crate::state::App;

#[derive(Clone, Serialize)]
pub struct Login {
    pub provider: Provider,
    pub status: &'static str,
    pub message: Option<String>,
    pub url: String,
    pub callback: bool,
    #[serde(skip)]
    pub verifier: String,
    #[serde(skip)]
    pub created: Instant,
}

static CLAUDE_CB: AtomicBool = AtomicBool::new(false);
static CODEX_CB: AtomicBool = AtomicBool::new(false);

pub async fn start_login(app: &Arc<App>, provider: Provider) -> Result<(String, Login)> {
    if !matches!(provider, Provider::Claude | Provider::Codex) {
        return Err(anyhow!("OAuth login is available for claude and codex"));
    }
    let pkce = oauth::pkce();
    let state = oauth::random_state();
    let url = oauth::auth_url(provider, &state, &pkce);
    let callback = ensure_callback_server(app, provider).await;
    let login = Login {
        provider,
        status: "pending",
        message: None,
        url,
        callback,
        verifier: pkce.verifier,
        created: Instant::now(),
    };
    let mut logins = app.logins.lock();
    logins.retain(|_, l| l.created.elapsed() < Duration::from_secs(1800));
    logins.insert(state.clone(), login.clone());
    Ok((state, login))
}

/// Listens on the fixed OAuth redirect port while logins are pending.
async fn ensure_callback_server(app: &Arc<App>, provider: Provider) -> bool {
    let (flag, port, path) = match provider {
        Provider::Claude => (&CLAUDE_CB, oauth::claude::PORT, "/callback"),
        _ => (&CODEX_CB, oauth::codex::PORT, "/auth/callback"),
    };
    if flag.load(Ordering::SeqCst) {
        return true;
    }
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                "cannot listen on localhost:{port} for the OAuth callback ({e}); paste the redirect URL instead"
            );
            return false;
        }
    };
    flag.store(true, Ordering::SeqCst);
    let router = Router::new().route(path, get(callback)).with_state(app.clone());
    let app2 = app.clone();
    tokio::spawn(async move {
        let shutdown = async move {
            // Stay up while a login for this provider is pending (max 15 minutes).
            let started = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let pending = app2.logins.lock().values().any(|l| l.provider == provider && l.status == "pending");
                if !pending || started.elapsed() > Duration::from_secs(900) {
                    break;
                }
            }
        };
        let _ = axum::serve(listener, router).with_graceful_shutdown(shutdown).await;
        flag.store(false, Ordering::SeqCst);
    });
    true
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(State(app): State<Arc<App>>, Query(q): Query<CallbackQuery>) -> Html<String> {
    let result = match (q.code, q.state, q.error) {
        (_, _, Some(e)) => Err(q.error_description.unwrap_or(e)),
        (Some(code), Some(state), _) => complete_login(&app, &state, &code).await.map_err(|e| format!("{e:#}")),
        _ => Err("missing code or state".to_string()),
    };
    let (title, body) = match result {
        Ok(label) => ("Signed in", format!("Connected <b>{}</b>. You can close this tab.", html_escape(&label))),
        Err(e) => ("Sign-in failed", html_escape(&e)),
    };
    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><meta name="color-scheme" content="dark"><title>{title}</title>
<body style="margin:0;height:100vh;display:grid;place-items:center;background:#000;color:#f1efed;font:15px/1.5 ui-sans-serif,system-ui,-apple-system,sans-serif">
<div style="text-align:center;max-width:420px;padding:24px"><div style="font-size:13px;letter-spacing:.08em;text-transform:uppercase;color:#c5ac98">subpool</div>
<h1 style="font-size:22px;font-weight:600;margin:10px 0">{title}</h1><p style="color:#aaa39d;margin:0">{body}</p></div></body>"#
    ))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub async fn complete_login(app: &Arc<App>, state: &str, code: &str) -> Result<String> {
    let (provider, verifier) = {
        let logins = app.logins.lock();
        let l = logins.get(state).ok_or_else(|| anyhow!("unknown or expired login, start again"))?;
        if l.status != "pending" {
            return Err(anyhow!("this login was already completed"));
        }
        (l.provider, l.verifier.clone())
    };
    let result = async {
        let (cred, name, extra) = oauth::exchange(app, provider, code.trim(), state, &verifier).await?;
        let path = app.cfg().auth_dir().join(&name);
        write_oauth_file(&path, provider, &cred, &extra)?;
        app.reload_accounts();
        Ok::<_, anyhow::Error>(cred.email.unwrap_or(name))
    }
    .await;
    let mut logins = app.logins.lock();
    if let Some(l) = logins.get_mut(state) {
        match &result {
            Ok(label) => {
                l.status = "done";
                l.message = Some(label.clone());
            }
            Err(e) => {
                l.status = "error";
                l.message = Some(format!("{e:#}"));
            }
        }
    }
    drop(logins);
    app.broadcast("login", json!({ "state": state }));
    result
}

/// Accepts a pasted redirect URL, a `code#state` string or a bare code.
pub fn parse_pasted(input: &str) -> (String, Option<String>) {
    let input = input.trim();
    if let Some(q) = input.split_once('?').map(|(_, q)| q).filter(|q| q.contains("code=")) {
        let mut code = String::new();
        let mut state = None;
        for (k, v) in url::form_urlencoded::parse(q.split('#').next().unwrap_or(q).as_bytes()) {
            match k.as_ref() {
                "code" => code = v.into_owned(),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        return (code, state);
    }
    (input.to_string(), None)
}

// ---------------------------------------------------------------------- router

pub fn router(app: Arc<App>) -> Router<Arc<App>> {
    Router::new()
        .route("/overview", get(overview))
        .route("/accounts", get(accounts))
        .route("/accounts/{id}", delete(delete_account))
        .route("/accounts/{id}/toggle", post(toggle_account))
        .route("/accounts/{id}/refresh", post(refresh_account))
        .route("/accounts/{id}/reset", post(reset_account))
        .route("/keys", post(add_key))
        .route("/requests", get(requests))
        .route("/models", get(models))
        .route("/config", get(get_config).put(put_config))
        .route("/login/{target}", post(login_start).get(login_status))
        .route("/login/{target}/code", post(login_code))
        .route("/live", get(live))
        .layer(middleware::from_fn_with_state(app, auth))
}

async fn auth(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let key = app.cfg().management_key.clone();
    if key.is_empty() {
        if addr.ip().is_loopback() {
            return next.run(req).await;
        }
        return err(
            StatusCode::FORBIDDEN,
            "the dashboard is only reachable from localhost until you set management-key",
        );
    }
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(String::from);
    let query = req
        .uri()
        .query()
        .and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "key").map(|(_, v)| v.into_owned()));
    if bearer.or(query).is_some_and(|k| constant_eq(&k, &key)) {
        return next.run(req).await;
    }
    err(StatusCode::UNAUTHORIZED, "management key required")
}

pub fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn ok() -> Response {
    Json(json!({ "ok": true })).into_response()
}

async fn overview(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = app.cfg();
    let accounts = app.pool.all();
    let (mut active, mut cooling, mut disabled) = (0, 0, 0);
    let mut providers = std::collections::BTreeMap::<&str, usize>::new();
    for a in &accounts {
        *providers.entry(a.provider.as_str()).or_default() += 1;
        let st = a.state.lock();
        if st.disabled {
            disabled += 1;
        } else if st.cooldowns.get("*").is_some_and(|t| *t > chrono::Utc::now()) {
            cooling += 1;
        } else {
            active += 1;
        }
    }
    let host = if cfg.host == "0.0.0.0" || cfg.host.is_empty() { "127.0.0.1".to_string() } else { cfg.host.clone() };
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": app.started.to_rfc3339(),
        "uptime_secs": (chrono::Utc::now() - app.started).num_seconds(),
        "base_url": format!("http://{host}:{}", cfg.port),
        "client_keys": cfg.api_keys,
        "management_key": !cfg.management_key.is_empty(),
        "totals": *app.stats.totals.lock(),
        "active": app.stats.active.load(Ordering::Relaxed),
        "series": app.stats.series(),
        "accounts": { "total": accounts.len(), "active": active, "cooling": cooling, "disabled": disabled, "providers": providers },
        "models": app.pool.models().len(),
        "config_path": app.cfg_path.display().to_string(),
        "auth_dir": cfg.auth_dir().display().to_string(),
    }))
}

async fn accounts(State(app): State<Arc<App>>) -> Json<Value> {
    Json(Value::Array(app.pool.all().iter().map(|a| a.snapshot()).collect()))
}

async fn requests(State(app): State<Arc<App>>) -> Json<Value> {
    let recent = app.stats.recent.lock();
    Json(serde_json::to_value(recent.iter().rev().collect::<Vec<_>>()).unwrap_or_default())
}

async fn models(State(app): State<Arc<App>>) -> Json<Value> {
    Json(Value::Array(app.pool.models().into_iter().map(|(m, p)| json!({ "id": m, "provider": p })).collect()))
}

#[derive(Deserialize)]
struct ToggleBody {
    disabled: bool,
}

async fn toggle_account(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<ToggleBody>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if let Some(path) = &acct.path
        && let Err(e) = set_file_disabled(path, b.disabled)
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    acct.state.lock().disabled = b.disabled;
    app.broadcast("accounts", Value::Null);
    ok()
}

async fn refresh_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if !acct.is_oauth() {
        return err(StatusCode::BAD_REQUEST, "API keys don't need refreshing");
    }
    match oauth::ensure_fresh(&app, &acct, chrono::Duration::minutes(5), true).await {
        Ok(()) => {
            acct.state.lock().cooldowns.remove("*");
            app.broadcast("accounts", Value::Null);
            ok()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn reset_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    let mut st = acct.state.lock();
    st.cooldowns.clear();
    st.strikes = 0;
    st.last_error = None;
    drop(st);
    app.broadcast("accounts", Value::Null);
    ok()
}

async fn delete_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if let Some(path) = &acct.path {
        if let Err(e) = std::fs::remove_file(path) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        app.reload_accounts();
        return ok();
    }
    let key = match &*acct.cred.read() {
        Credential::ApiKey { key, .. } => key.clone(),
        _ => String::new(),
    };
    let mut cfg = (*app.cfg()).clone();
    let keep = |e: &KeyEntry| e.api_key.trim() != key;
    match acct.provider {
        Provider::Claude => cfg.claude_api_key.retain(keep),
        Provider::Codex => cfg.codex_api_key.retain(keep),
        Provider::Gemini => cfg.gemini_api_key.retain(keep),
        Provider::Compat => {
            // Removing a group's last key removes the group (a keyless group would remain usable).
            let group = acct.group.clone();
            for c in cfg.openai_compatibility.iter_mut().filter(|c| Some(&c.name) == group.as_ref()) {
                c.api_keys.retain(|k| !key.is_empty() && k.trim() != key);
            }
            cfg.openai_compatibility.retain(|c| Some(&c.name) != group.as_ref() || !c.api_keys.is_empty());
        }
    }
    save_config(&app, cfg)
}

#[derive(Deserialize)]
struct KeyBody {
    provider: String,
    api_key: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    models: String,
}

async fn add_key(State(app): State<Arc<App>>, Json(b): Json<KeyBody>) -> Response {
    let key = b.api_key.trim().to_string();
    let base = Some(b.base_url.trim().to_string()).filter(|s| !s.is_empty());
    let models: Vec<ModelAlias> = b
        .models
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|m| match m.split_once('=') {
            Some((alias, name)) => ModelAlias { name: name.trim().into(), alias: Some(alias.trim().into()) },
            None => ModelAlias { name: m.into(), alias: None },
        })
        .collect();
    let mut cfg = (*app.cfg()).clone();
    let entry = KeyEntry { api_key: key.clone(), base_url: base.clone(), models: models.clone(), ..Default::default() };
    match b.provider.as_str() {
        "claude" | "codex" | "gemini" if key.is_empty() => return err(StatusCode::BAD_REQUEST, "API key is required"),
        "claude" => cfg.claude_api_key.push(entry),
        "codex" => cfg.codex_api_key.push(entry),
        "gemini" => cfg.gemini_api_key.push(entry),
        "compat" | "openai-compat" => {
            let Some(base) = base else { return err(StatusCode::BAD_REQUEST, "base URL is required") };
            if models.is_empty() {
                return err(StatusCode::BAD_REQUEST, "list at least one model");
            }
            let name = if b.name.trim().is_empty() {
                url::Url::parse(&base)
                    .ok()
                    .and_then(|u| u.host_str().map(String::from))
                    .unwrap_or_else(|| "provider".into())
            } else {
                b.name.trim().to_string()
            };
            match cfg.openai_compatibility.iter_mut().find(|c| c.name == name && c.base_url == base) {
                Some(c) => {
                    if !key.is_empty() {
                        c.api_keys.push(key);
                    }
                    for m in models {
                        if !c.models.iter().any(|x| x.public() == m.public()) {
                            c.models.push(m);
                        }
                    }
                }
                None => cfg.openai_compatibility.push(CompatEntry {
                    name,
                    base_url: base,
                    api_keys: if key.is_empty() { vec![] } else { vec![key] },
                    models,
                    ..Default::default()
                }),
            }
        }
        _ => return err(StatusCode::BAD_REQUEST, "unknown provider"),
    }
    save_config(&app, cfg)
}

fn save_config(app: &Arc<App>, cfg: Config) -> Response {
    let text = match serde_yaml::to_string(&cfg) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if let Err(e) = std::fs::write(&app.cfg_path, text) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    app.set_config(cfg);
    ok()
}

async fn get_config(State(app): State<Arc<App>>) -> Json<Value> {
    let text = std::fs::read_to_string(&app.cfg_path).unwrap_or_default();
    Json(json!({ "text": text, "path": app.cfg_path.display().to_string() }))
}

#[derive(Deserialize)]
struct ConfigBody {
    text: String,
}

async fn put_config(State(app): State<Arc<App>>, Json(b): Json<ConfigBody>) -> Response {
    let cfg = match Config::parse(&b.text) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let old = app.cfg();
    if let Err(e) = std::fs::write(&app.cfg_path, &b.text) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let restart = cfg.host != old.host || cfg.port != old.port;
    app.set_config(cfg);
    Json(json!({ "ok": true, "restart_required": restart })).into_response()
}

async fn login_start(State(app): State<Arc<App>>, Path(target): Path<String>) -> Response {
    let provider = match target.as_str() {
        "claude" | "anthropic" => Provider::Claude,
        "codex" | "openai" | "chatgpt" => Provider::Codex,
        _ => return err(StatusCode::BAD_REQUEST, "unknown provider"),
    };
    match start_login(&app, provider).await {
        Ok((state, l)) => Json(json!({ "state": state, "url": l.url, "callback": l.callback })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

async fn login_status(State(app): State<Arc<App>>, Path(target): Path<String>) -> Response {
    match app.logins.lock().get(&target) {
        Some(l) => Json(serde_json::to_value(l).unwrap_or_default()).into_response(),
        None => err(StatusCode::NOT_FOUND, "unknown login"),
    }
}

#[derive(Deserialize)]
struct CodeBody {
    input: String,
}

async fn login_code(State(app): State<Arc<App>>, Path(target): Path<String>, Json(b): Json<CodeBody>) -> Response {
    let (code, state) = parse_pasted(&b.input);
    if code.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no authorization code found");
    }
    if state.as_deref().is_some_and(|s| s != target) {
        return err(StatusCode::BAD_REQUEST, "that URL belongs to a different login attempt");
    }
    match complete_login(&app, &target, &code).await {
        Ok(label) => Json(json!({ "ok": true, "label": label })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn live(State(app): State<Arc<App>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        let mut rx = app.live.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(m) => if socket.send(Message::Text(m.into())).await.is_err() { break },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                _ = tick.tick() => {
                    let msg = json!({ "type": "tick", "data": {
                        "active": app.stats.active.load(Ordering::Relaxed),
                        "totals": *app.stats.totals.lock(),
                    }}).to_string();
                    if socket.send(Message::Text(msg.into())).await.is_err() { break }
                }
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
            }
        }
    })
}
