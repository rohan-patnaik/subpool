//! Responses API over websocket (`GET /v1/responses`), the transport Codex uses.
//!
//! Each `response.create` message is one turn. Codex OAuth accounts get a
//! native upstream websocket (server-side `previous_response_id` works as-is);
//! every other provider is served through the normal pipeline, with
//! `previous_response_id` expanded from a small local history.

use std::collections::VecDeque;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::http::HeaderMap;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite;

use crate::accounts::{Account, Pick, Provider};
use crate::formats::{StreamParser, responses};
use crate::ir::{self, Event, Format, Usage};
use crate::proxy::{self, Call, Reply, Tracker};
use crate::sse::SseEvent;
use crate::state::App;

type Upstream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type ClientTx = futures::stream::SplitSink<WebSocket, Message>;

const HISTORY: usize = 4;

#[derive(Default)]
struct Session {
    /// response id -> full conversation input including that response's output.
    history: VecDeque<(String, Vec<Value>)>,
    upstream: Option<(Arc<Account>, Upstream)>,
    pinned: Option<String>,
}

impl Session {
    fn lookup(&self, id: &str) -> Option<Vec<Value>> {
        self.history.iter().find(|(k, _)| k == id).map(|(_, v)| v.clone())
    }

    fn remember(&mut self, id: String, items: Vec<Value>) {
        self.history.push_back((id, items));
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }
}

struct ClientGone;

async fn send(tx: &mut ClientTx, text: String) -> Result<(), ClientGone> {
    tx.send(Message::Text(text.into())).await.map_err(|_| ClientGone)
}

fn error_event(status: u16, body: &Value) -> String {
    let err = if body["error"].is_object() {
        body["error"].clone()
    } else {
        json!({ "message": proxy::error_message(&body.to_string()) })
    };
    json!({ "type": "error", "status": status, "error": err }).to_string()
}

fn input_items(body: &Value) -> Vec<Value> {
    match &body["input"] {
        Value::String(s) => {
            vec![json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": s }] })]
        }
        Value::Array(a) => a.clone(),
        _ => vec![],
    }
}

pub async fn handle(app: Arc<App>, headers: HeaderMap, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut sess = Session::default();
    while let Some(msg) = rx.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let Ok(mut body) = serde_json::from_str::<Value>(&text) else {
            if send(&mut tx, error_event(400, &json!({ "error": { "message": "invalid JSON" } }))).await.is_err() {
                break;
            }
            continue;
        };
        if body["type"] != "response.create" {
            let msg = format!("unsupported message type `{}`", body["type"].as_str().unwrap_or_default());
            if send(&mut tx, error_event(400, &json!({ "error": { "message": msg, "type": "invalid_request_error" } })))
                .await
                .is_err()
            {
                break;
            }
            continue;
        }
        if let Some(o) = body.as_object_mut() {
            o.remove("type");
        }
        if turn(&app, &headers, &mut sess, body, &mut tx).await.is_err() {
            break;
        }
    }
    if let Some((_, mut up)) = sess.upstream.take() {
        let _ = up.close(None).await;
    }
}

async fn turn(
    app: &Arc<App>,
    headers: &HeaderMap,
    sess: &mut Session,
    mut body: Value,
    tx: &mut ClientTx,
) -> Result<(), ClientGone> {
    // Full conversation for local history (and for providers without server state).
    let prev = body["previous_response_id"].as_str().map(String::from);
    let mut full = prev.as_deref().and_then(|id| sess.lookup(id)).unwrap_or_default();
    full.extend(input_items(&body));

    let cfg = app.cfg();
    if cfg.codex_websockets {
        match native_turn(app, sess, &body, &full, tx).await {
            Native::Done => return Ok(()),
            Native::Gone => return Err(ClientGone),
            Native::Fallback => {}
        }
    }

    if let Some(id) = &prev {
        if sess.lookup(id).is_none() {
            let err = json!({ "error": {
                "message": "Previous response is not available on this websocket; resend the full conversation input without previous_response_id",
                "type": "invalid_request_error", "code": "previous_response_not_found", "param": "previous_response_id"
            }});
            return send(tx, error_event(400, &err)).await;
        }
        body["input"] = Value::Array(full.clone());
        body.as_object_mut().unwrap().remove("previous_response_id");
    }

    let call = Call {
        format: Format::Responses,
        body,
        headers: headers.clone(),
        stream: true,
        transport: "ws",
        path_model: None,
        pinned: sess.pinned.clone(),
    };
    match proxy::execute(app.clone(), call).await {
        Reply::Stream { mut frames, .. } => {
            while let Some(f) = frames.next().await {
                if f.event.as_deref() == Some("response.completed") {
                    capture(sess, &f.data, &full);
                }
                send(tx, f.data).await?;
            }
            Ok(())
        }
        Reply::Json(v) => {
            capture(sess, &json!({ "response": v }).to_string(), &full);
            send(tx, json!({ "type": "response.completed", "response": v }).to_string()).await
        }
        Reply::Error(status, body) => send(tx, error_event(status, &body)).await,
    }
}

fn capture(sess: &mut Session, data: &str, full: &[Value]) {
    let Ok(v) = serde_json::from_str::<Value>(data) else { return };
    let r = &v["response"];
    let Some(id) = r["id"].as_str() else { return };
    let mut items = full.to_vec();
    items.extend(r["output"].as_array().cloned().unwrap_or_default());
    sess.remember(id.to_string(), items);
}

enum Native {
    Done,
    Gone,
    Fallback,
}

async fn connect(acct: &Arc<Account>) -> Result<Upstream, String> {
    let (url, headers) = crate::upstream::codex_ws_url(acct);
    let mut req =
        tungstenite::client::IntoClientRequest::into_client_request(url.as_str()).map_err(|e| e.to_string())?;
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) =
            (tungstenite::http::HeaderName::from_bytes(k.as_bytes()), tungstenite::http::HeaderValue::from_str(&v))
        {
            req.headers_mut().insert(name, val);
        }
    }
    let (ws, _) = tokio::time::timeout(std::time::Duration::from_secs(20), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| "websocket handshake timed out".to_string())?
        .map_err(|e| match e {
            tungstenite::Error::Http(resp) => format!("websocket handshake rejected: {}", resp.status()),
            other => other.to_string(),
        })?;
    Ok(ws)
}

async fn native_turn(app: &Arc<App>, sess: &mut Session, body: &Value, full: &[Value], tx: &mut ClientTx) -> Native {
    let cfg = app.cfg();
    // Native websockets don't go through HTTP proxies.
    if !cfg.proxy_url.is_empty() {
        return Native::Fallback;
    }
    let (model, suffix) = ir::split_model_suffix(body["model"].as_str().unwrap_or_default());
    if model.is_empty() {
        return Native::Fallback;
    }

    // Reuse the session's upstream socket when it can serve this model.
    let reuse = sess.upstream.as_ref().is_some_and(|(a, _)| {
        a.resolve(&model).is_some() && a.cooling_until(&model).is_none() && !a.state.lock().disabled
    });
    if !reuse {
        if let Some((_, mut up)) = sess.upstream.take() {
            let _ = up.close(None).await;
        }
        let (acct, _) = match app.pool.pick(&model, &[], cfg.routing, sess.pinned.as_deref()) {
            Pick::Ok(a, m) => (a, m),
            _ => return Native::Fallback,
        };
        if acct.provider != Provider::Codex || !acct.is_oauth() || acct.proxy_url.is_some() {
            return Native::Fallback;
        }
        if crate::oauth::ensure_fresh(app, &acct, chrono::Duration::minutes(5), false).await.is_err() {
            return Native::Fallback;
        }
        match connect(&acct).await {
            Ok(ws) => {
                sess.pinned = Some(acct.id.clone());
                sess.upstream = Some((acct, ws));
            }
            Err(e) => {
                tracing::warn!(account = %acct.label, "codex websocket unavailable, using HTTP: {e}");
                return Native::Fallback;
            }
        }
    }
    let (acct, mut up) = sess.upstream.take().unwrap();
    let upstream_model = acct.resolve(&model).unwrap_or(model.clone());

    let mut payload = body.clone();
    crate::upstream::sanitize_codex_body(&mut payload, &upstream_model, true);
    if let Some(r) = &suffix
        && let Some(e) = r.effort_level()
    {
        payload["reasoning"]["effort"] = e.into();
    }
    payload["type"] = "response.create".into();

    let mut tracker = Tracker::new(app, Format::Responses, true, "ws", &model);
    tracker.attempt(&acct);
    if up.send(tungstenite::Message::Text(payload.to_string().into())).await.is_err() {
        tracker.cancel();
        return Native::Fallback;
    }

    let mut parser = responses::Parser::default();
    let mut usage = Usage::default();
    let mut evs = Vec::new();
    let mut error: Option<(u16, String)> = None;
    let mut forwarded = false;
    let mut terminal = false;
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(600), up.next()).await;
        let text = match next {
            Ok(Some(Ok(tungstenite::Message::Text(t)))) => t.to_string(),
            Ok(Some(Ok(tungstenite::Message::Binary(b)))) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Some(Ok(tungstenite::Message::Close(_)))) | Ok(None) => {
                error = Some((502, "codex websocket closed".into()));
                break;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                error = Some((502, format!("codex websocket error: {e}")));
                break;
            }
            Err(_) => {
                error = Some((504, "codex websocket idle timeout".into()));
                break;
            }
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let kind = v["type"].as_str().unwrap_or_default().to_string();
        parser.feed(&SseEvent { event: None, data: text.clone() }, &mut evs);
        for ev in evs.drain(..) {
            match ev {
                Event::Usage(u) => usage.merge(&u),
                Event::Error { status, message } => error = Some((status, message)),
                Event::Text(_) | Event::Reasoning(_) | Event::ToolStart { .. } => tracker.first_token(),
                _ => {}
            }
        }
        // Rate limits / auth failures before any output can be retried elsewhere.
        if !forwarded
            && matches!(kind.as_str(), "error" | "response.failed")
            && let Some((status @ (429 | 401 | 403), msg)) = error.clone()
        {
            if status == 429 {
                acct.cool(Some(&model), chrono::Utc::now() + chrono::Duration::seconds(60), &format!("429: {msg}"));
            } else {
                acct.cool(None, chrono::Utc::now() + chrono::Duration::minutes(10), &format!("{status}: {msg}"));
            }
            let _ = up.close(None).await;
            sess.pinned = None;
            tracker.finish(status, &usage, Some(msg));
            return Native::Fallback;
        }
        if kind == "response.completed" || kind == "response.incomplete" {
            capture(sess, &text, full);
        }
        terminal = matches!(kind.as_str(), "response.completed" | "response.incomplete" | "response.failed" | "error");
        forwarded = true;
        if send(tx, text).await.is_err() {
            tracker.finish(499, &usage, Some("client disconnected".into()));
            return Native::Gone;
        }
        if terminal {
            break;
        }
    }
    if terminal {
        sess.upstream = Some((acct.clone(), up));
    } else {
        // The upstream socket died mid-turn: tell the client and reconnect next turn.
        let (status, msg) = error.clone().unwrap_or((502, "codex websocket closed".into()));
        let body = json!({ "error": { "message": msg, "type": "upstream_error" } });
        if send(tx, error_event(status, &body)).await.is_err() {
            tracker.finish(status, &usage, Some(msg));
            return Native::Gone;
        }
    }
    match error {
        Some((s, m)) => tracker.finish(s, &usage, Some(m)),
        None => {
            acct.record_ok();
            tracker.finish(200, &usage, None)
        }
    }
    Native::Done
}
