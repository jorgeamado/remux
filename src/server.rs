use crate::{auth::PairError, ws, App};
use anyhow::Result;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(RustEmbed)]
#[folder = "web/dist"]
struct Assets;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/pair", post(pair))
        .route("/api/sessions", get(sessions))
        .route("/api/windows", get(windows))
        .route("/api/push/key", get(push_key))
        .route("/api/push/subscribe", post(push_subscribe))
        .route("/api/push/unsubscribe", post(push_unsubscribe))
        .route("/api/attention", get(attention_pending))
        .route("/ws", any(ws::handler))
        .fallback(static_handler)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// Device-token check for plain HTTP endpoints (`Authorization: Bearer <token>`).
fn bearer_device(app: &App, headers: &HeaderMap) -> Option<crate::auth::Device> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    app.auth.authenticate(value.strip_prefix("Bearer ")?)
}

async fn sessions(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if bearer_device(&app, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "device token required").into_response();
    }
    match tokio::task::spawn_blocking(crate::tmux::list_sessions).await {
        Ok(Ok(list)) => Json(list).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "tmux error").into_response(),
    }
}

async fn push_key(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if bearer_device(&app, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "device token required").into_response();
    }
    Json(serde_json::json!({ "key": app.push.public_key() })).into_response()
}

#[derive(Deserialize)]
struct SubscribeRequest {
    endpoint: String,
    #[serde(default)]
    keys: SubscriptionKeys,
}

#[derive(Deserialize, Default)]
struct SubscriptionKeys {
    #[serde(default)]
    p256dh: String,
    #[serde(default)]
    auth: String,
}

async fn push_subscribe(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(req): Json<SubscribeRequest>,
) -> Response {
    let Some(device) = bearer_device(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "device token required").into_response();
    };
    match app.push.subscribe(crate::push::Subscription {
        device_id: device.id,
        endpoint: req.endpoint,
        p256dh: req.keys.p256dh,
        auth: req.keys.auth,
    }) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct UnsubscribeRequest {
    endpoint: String,
}

async fn push_unsubscribe(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(req): Json<UnsubscribeRequest>,
) -> Response {
    let Some(device) = bearer_device(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "device token required").into_response();
    };
    app.push.unsubscribe(&device.id, &req.endpoint);
    StatusCode::NO_CONTENT.into_response()
}

/// Sessions with recent attention — lets a notification tap land on the
/// right session without putting its name inside the push payload.
async fn attention_pending(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    const PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(600);
    if bearer_device(&app, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "device token required").into_response();
    }
    let mut pending = app.pending_attention.lock().unwrap();
    let now = std::time::Instant::now();
    pending.retain(|_, t| now.duration_since(*t) < PENDING_TTL);
    let mut sessions: Vec<&String> = pending.keys().collect();
    sessions.sort();
    Json(serde_json::json!({ "sessions": sessions })).into_response()
}

#[derive(Deserialize)]
struct WindowsQuery {
    session: String,
}

async fn windows(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<WindowsQuery>,
) -> Response {
    if bearer_device(&app, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "device token required").into_response();
    }
    if !crate::tmux::valid_session_name(&q.session) {
        return (StatusCode::BAD_REQUEST, "invalid session name").into_response();
    }
    match tokio::task::spawn_blocking(move || crate::tmux::list_windows(&q.session)).await {
        Ok(Ok(list)) => Json(list).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "tmux error").into_response(),
    }
}

pub async fn run(app: Arc<App>) -> Result<()> {
    let router = router(app.clone());
    let addr = app.args.listen;
    match (&app.args.tls_cert, &app.args.tls_key) {
        (Some(cert), Some(key)) => {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
            tracing::info!("listening on https://{addr}");
            axum_server::bind_rustls(addr, tls)
                .serve(router.into_make_service())
                .await?;
        }
        _ => {
            tracing::info!("listening on http://{addr}");
            axum_server::bind(addr)
                .serve(router.into_make_service())
                .await?;
        }
    }
    Ok(())
}

/// Reject requests whose Host or Origin is not allowlisted.
/// This blocks DNS-rebinding and cross-site WebSocket hijacking: a malicious
/// website in the user's browser can reach this daemon's address, but cannot
/// present an allowlisted Origin, and a rebound DNS name fails the Host check.
async fn guard(
    State(app): State<Arc<App>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let headers = request.headers();

    // HTTP/2 carries the host in the :authority pseudo-header (surfaced via
    // the request URI), not in a Host header.
    let host = header_host(headers, header::HOST)
        .or_else(|| request.uri().host().map(|h| strip_port(h).to_string()));
    let host_ok = host.map(|h| allowed(&app, &h)).unwrap_or(false);
    if !host_ok {
        tracing::warn!("rejected request: bad or missing Host/authority");
        return Err(StatusCode::FORBIDDEN);
    }

    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let origin_host = origin
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .map(strip_port);
        if !origin_host.map(|h| allowed(&app, h)).unwrap_or(false) {
            tracing::warn!(origin, "rejected request: bad Origin header");
            return Err(StatusCode::FORBIDDEN);
        }
    }

    Ok(next.run(request).await)
}

fn header_host(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|h| strip_port(h).to_string())
}

fn strip_port(host: &str) -> &str {
    // handle [::1]:7777 and host:port
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(host);
    }
    host.split(':').next().unwrap_or(host)
}

fn allowed(app: &App, host: &str) -> bool {
    app.allowed_hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host))
}

#[derive(Deserialize)]
struct PairRequest {
    token: String,
    #[serde(default = "default_device_name")]
    device_name: String,
}

fn default_device_name() -> String {
    "unnamed device".into()
}

#[derive(Serialize)]
struct PairResponse {
    device_token: String,
}

async fn pair(
    State(app): State<Arc<App>>,
    Json(req): Json<PairRequest>,
) -> Result<Json<PairResponse>, (StatusCode, String)> {
    match app.auth.pair(&req.token, &req.device_name) {
        Ok(device_token) => {
            tracing::info!(device = %req.device_name, "device paired");
            Ok(Json(PairResponse { device_token }))
        }
        Err(e @ PairError::RateLimited) => Err((StatusCode::TOO_MANY_REQUESTS, e.to_string())),
        Err(e @ PairError::InvalidToken) => Err((StatusCode::UNAUTHORIZED, e.to_string())),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let mut resp = ([(header::CONTENT_TYPE, mime.as_ref())], file.data).into_response();
            if path == "index.html" {
                resp.headers_mut().insert(
                    header::CONTENT_SECURITY_POLICY,
                    header::HeaderValue::from_static(
                        "default-src 'self'; connect-src 'self' ws: wss:; \
                         img-src 'self' data:; style-src 'self'; \
                         base-uri 'none'; frame-ancestors 'none'",
                    ),
                );
            }
            resp
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_port_variants() {
        assert_eq!(strip_port("example.com:7777"), "example.com");
        assert_eq!(strip_port("example.com"), "example.com");
        assert_eq!(strip_port("[::1]:7777"), "::1");
        assert_eq!(strip_port("127.0.0.1:80"), "127.0.0.1");
    }
}
