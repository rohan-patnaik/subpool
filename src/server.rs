use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use serde_json::{Value, json};
use tower_http::cors::CorsLayer;

use crate::formats::{self, Frame};
use crate::ir::Format;
use crate::proxy::{self, Call, FrameStream, Reply};
use crate::state::App;

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(responses).get(responses_ws))
        .route("/backend-api/codex/responses", post(responses).get(responses_ws))
        .route("/v1/models", get(models))
        .route("/v1beta/models", get(gemini_models))
        .route("/v1beta/models/{*rest}", post(gemini))
        .layer(middleware::from_fn_with_state(app.clone(), client_auth))
        .layer(DefaultBodyLimit::max(256 << 20))
        .layer(CorsLayer::permissive());

    Router::new()
        .merge(api)
        .nest("/api", crate::mgmt::router(app.clone()))
        .route("/", get(ui_index))
        .route("/ui/{file}", get(ui_asset))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

// ------------------------------------------------------------------------ auth

async fn client_auth(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let cfg = app.cfg();
    if cfg.api_keys.is_empty() {
        return next.run(req).await;
    }
    let h = req.headers();
    let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::trim).map(String::from);
    let provided = get("authorization")
        .and_then(|v| v.strip_prefix("Bearer ").map(|s| s.trim().to_string()))
        .or_else(|| get("x-api-key"))
        .or_else(|| get("x-goog-api-key"))
        .or_else(|| {
            req.uri().query().and_then(|q| {
                url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "key").map(|(_, v)| v.into_owned())
            })
        });
    if provided.is_some_and(|k| cfg.api_keys.iter().any(|a| crate::mgmt::constant_eq(a, &k))) {
        return next.run(req).await;
    }
    let format = format_for_path(req.uri().path());
    (StatusCode::UNAUTHORIZED, axum::Json(formats::error_body(format, 401, "invalid or missing API key")))
        .into_response()
}

fn format_for_path(path: &str) -> Format {
    if path.starts_with("/v1/messages") {
        Format::Claude
    } else if path.starts_with("/v1beta") {
        Format::Gemini
    } else if path.contains("/responses") {
        Format::Responses
    } else {
        Format::Chat
    }
}

// -------------------------------------------------------------------- handlers

fn parse_body(format: Format, body: &Bytes) -> Result<Value, Box<Response>> {
    serde_json::from_slice::<Value>(body).ok().filter(Value::is_object).ok_or_else(|| {
        Box::new(reply(
            format,
            Reply::Error(400, formats::error_body(format, 400, "request body must be a JSON object")),
            false,
        ))
    })
}

async fn run(app: Arc<App>, format: Format, headers: HeaderMap, body: Bytes) -> Response {
    let body = match parse_body(format, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let stream = body["stream"].as_bool().unwrap_or(false);
    let call = Call { format, body, headers, stream, transport: "http", path_model: None, pinned: None };
    reply(format, proxy::execute(app, call).await, false)
}

async fn chat(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    run(app, Format::Chat, headers, body).await
}

async fn messages(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    run(app, Format::Claude, headers, body).await
}

async fn responses(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    run(app, Format::Responses, headers, body).await
}

async fn responses_ws(State(app): State<Arc<App>>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(256 << 20)
        .max_frame_size(256 << 20)
        .on_upgrade(move |socket| crate::ws::handle(app, headers, socket))
}

async fn count_tokens(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    match parse_body(Format::Claude, &body) {
        Ok(v) => axum::Json(proxy::count_tokens(app, headers, v).await).into_response(),
        Err(r) => *r,
    }
}

async fn gemini(
    State(app): State<Arc<App>>,
    Path(rest): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((model, action)) = rest.rsplit_once(':') else {
        return reply(
            Format::Gemini,
            Reply::Error(404, formats::error_body(Format::Gemini, 404, "unknown method")),
            false,
        );
    };
    let body = match parse_body(Format::Gemini, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let stream = match action {
        "generateContent" => false,
        "streamGenerateContent" => true,
        "countTokens" => return axum::Json(json!({ "totalTokens": proxy::estimate_tokens(&body) })).into_response(),
        _ => {
            return reply(
                Format::Gemini,
                Reply::Error(404, formats::error_body(Format::Gemini, 404, "unknown method")),
                false,
            );
        }
    };
    let json_array = stream && q.get("alt").map(String::as_str) != Some("sse");
    let call = Call {
        format: Format::Gemini,
        body,
        headers,
        stream,
        transport: "http",
        path_model: Some(model.trim_start_matches("models/").to_string()),
        pinned: None,
    };
    reply(Format::Gemini, proxy::execute(app, call).await, json_array)
}

async fn models(State(app): State<Arc<App>>) -> Response {
    let created = app.started.timestamp();
    let data: Vec<Value> = app
        .pool
        .models()
        .into_iter()
        .map(|(id, provider)| {
            json!({
                "id": id, "object": "model", "created": created, "owned_by": provider.as_str(),
                "type": "model", "display_name": id, "created_at": app.started.to_rfc3339(),
            })
        })
        .collect();
    let first = data.first().and_then(|m| m["id"].as_str()).map(String::from);
    let last = data.last().and_then(|m| m["id"].as_str()).map(String::from);
    axum::Json(json!({ "object": "list", "data": data, "has_more": false, "first_id": first, "last_id": last }))
        .into_response()
}

async fn gemini_models(State(app): State<Arc<App>>) -> Response {
    let models: Vec<Value> = app
        .pool
        .models()
        .into_iter()
        .map(|(id, _)| {
            json!({
                "name": format!("models/{id}"), "displayName": id, "version": "001",
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent", "countTokens"],
            })
        })
        .collect();
    axum::Json(json!({ "models": models })).into_response()
}

// -------------------------------------------------------------------- replies

fn reply(_format: Format, r: Reply, json_array: bool) -> Response {
    match r {
        Reply::Json(v) => axum::Json(v).into_response(),
        Reply::Error(status, v) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            (code, axum::Json(v)).into_response()
        }
        Reply::Stream { frames, account } => {
            let (body, ctype) = if json_array {
                (json_array_body(frames), "application/json")
            } else {
                (sse_body(frames), "text/event-stream")
            };
            let mut resp = Response::new(body);
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            h.insert("x-accel-buffering", HeaderValue::from_static("no"));
            if let Ok(v) = HeaderValue::from_str(&account) {
                h.insert("x-subpool-account", v);
            }
            resp
        }
    }
}

const KEEPALIVE: Duration = Duration::from_secs(15);

fn sse_body(mut frames: FrameStream) -> Body {
    Body::from_stream(async_stream::stream! {
        loop {
            match tokio::time::timeout(KEEPALIVE, frames.next()).await {
                Ok(Some(f)) => yield Ok::<_, Infallible>(Bytes::from(f.to_sse())),
                Ok(None) => break,
                Err(_) => yield Ok(Bytes::from_static(b": keepalive\n\n")),
            }
        }
    })
}

/// Gemini without `alt=sse` streams a JSON array.
fn json_array_body(mut frames: FrameStream) -> Body {
    Body::from_stream(async_stream::stream! {
        let mut first = true;
        yield Ok::<_, Infallible>(Bytes::from_static(b"["));
        loop {
            match tokio::time::timeout(KEEPALIVE, frames.next()).await {
                Ok(Some(Frame { data, .. })) => {
                    let sep = if first { "" } else { ",\r\n" };
                    first = false;
                    yield Ok(Bytes::from(format!("{sep}{data}")));
                }
                Ok(None) => break,
                Err(_) => yield Ok(Bytes::from_static(b"\n")),
            }
        }
        yield Ok(Bytes::from_static(b"]"));
    })
}

// ------------------------------------------------------------------------- ui

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const STYLE: &str = include_str!("../ui/style.css");
const ICON: &str = include_str!("../ui/icon.svg");

async fn ui_index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], INDEX).into_response()
}

async fn ui_asset(Path(file): Path<String>) -> Response {
    let (body, ctype) = match file.as_str() {
        "app.js" => (APP_JS, "text/javascript; charset=utf-8"),
        "style.css" => (STYLE, "text/css; charset=utf-8"),
        "icon.svg" => (ICON, "image/svg+xml"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}
