use crate::{auth::PairError, ws, App};
use anyhow::Result;
use axum::{
    extract::{Path, Request, State},
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
        .route("/api/permissions", get(permissions_pending))
        .route("/api/permissions/{id}/decide", post(permission_decide))
        .route("/api/devices", get(devices))
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
    if let Err(e) = app.push.unsubscribe(&device.id, &req.endpoint) {
        tracing::error!("failed to persist unsubscribe: {e:#}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not persist unsubscribe",
        )
            .into_response();
    }
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
    pending.retain(|_, (t, _)| now.duration_since(*t) < PENDING_TTL);
    // Info on purpose: this is how we tell "service worker never asked"
    // from "asked but showed generic text" when debugging notifications.
    tracing::info!(pending = pending.len(), "attention details served");
    let mut sessions: Vec<&String> = pending.keys().collect();
    sessions.sort();
    // `details` is additive; `sessions` stays for older clients. Freshest
    // first — the service worker names the most recent event in the
    // notification it shows for a payload-less push.
    // Sort by the Instant, not by whole-second age: same-second events would
    // otherwise inherit HashMap iteration order and "freshest" would lie.
    let mut by_time: Vec<(&std::time::Instant, &crate::Attention)> =
        pending.values().map(|(t, a)| (t, a)).collect();
    by_time.sort_by(|(a, _), (b, _)| b.cmp(a));
    let details: Vec<serde_json::Value> = by_time
        .iter()
        .map(|(t, a)| {
            let mut v = serde_json::to_value(a).unwrap_or_default();
            v["age_secs"] = now.duration_since(**t).as_secs().into();
            v
        })
        .collect();
    Json(serde_json::json!({ "sessions": sessions, "details": details })).into_response()
}

/// Open agent permission cards (M4b), for the PWA to render Approve/Deny.
/// Approve-capable devices only — the command/path in a card is sensitive, so
/// a non-approve device gets an empty list (no details, no existence leak),
/// mirroring what its websocket receives.
async fn permissions_pending(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Some(device) = bearer_device(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "device token required").into_response();
    };
    let cards = if app.auth.can_approve(&device.id) {
        app.perms
            .snapshot()
            .iter()
            .map(|c| c.view())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    Json(serde_json::json!({ "cards": cards })).into_response()
}

#[derive(Deserialize)]
struct DecideRequest {
    decision: String,
}

/// Resolve a permission card. The canonical decision op (the websocket only
/// *delivers* cards). Approve-gated; the capability is re-checked under the
/// registry lock at the moment of decision. Reports success only once the
/// decision has been written to the live hook socket (not a guaranteed
/// end-to-end ACK) — a decision that raced the hook's socket closing returns
/// 409, not a false "approved".
async fn permission_decide(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<DecideRequest>,
) -> Response {
    let Some(device) = bearer_device(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "device token required").into_response();
    };
    if !app.auth.can_approve(&device.id) {
        return (StatusCode::FORBIDDEN, "approve capability required").into_response();
    }
    let Some(decision) = crate::permit::Decision::parse(&req.decision) else {
        return (StatusCode::BAD_REQUEST, "decision must be allow or deny").into_response();
    };
    let device_id = device.id.clone();
    match app
        .perms
        .resolve(&id, decision, || app.auth.can_approve(&device_id))
    {
        Ok((card, confirm)) => {
            // Wait for the held-wait to confirm it wrote the decision to a live
            // hook. No confirmation → the hook vanished (the Mac answered)
            // between consume and delivery. The deadline must exceed the
            // ingest write timeout (5s) with margin, or a slow-but-successful
            // write could be reported as a false 409.
            match tokio::time::timeout(std::time::Duration::from_secs(8), confirm).await {
                // `written`: the decision reached the live hook socket (the
                // write succeeded before the connection closed). We deliberately
                // do NOT claim end-to-end receipt — proving the hook parsed and
                // acted on it would need an application-level ACK (Codex).
                Ok(Ok(())) => Json(serde_json::json!({
                    "ok": true, "written": true, "session": card.session,
                }))
                .into_response(),
                _ => (
                    StatusCode::CONFLICT,
                    "decision recorded but the agent was no longer waiting",
                )
                    .into_response(),
            }
        }
        Err(crate::permit::ResolveError::Forbidden) => {
            (StatusCode::FORBIDDEN, "approve capability required").into_response()
        }
        Err(crate::permit::ResolveError::Truncated) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "the full command was not shown — approve on the host",
        )
            .into_response(),
        Err(crate::permit::ResolveError::Expired) => {
            (StatusCode::GONE, "this request expired").into_response()
        }
        Err(crate::permit::ResolveError::Unknown) => {
            (StatusCode::NOT_FOUND, "no such pending request").into_response()
        }
    }
}

/// Read-only device list for the PWA sheet. Management (revoke/rename) is
/// deliberately host-CLI-only until per-device capabilities exist.
async fn devices(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Some(me) = bearer_device(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "device token required").into_response();
    };
    let list: Vec<serde_json::Value> = app
        .auth
        .devices()
        .into_iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "name": d.name,
                "created_unix": d.created_unix,
                "last_seen_unix": d.last_seen_unix,
                "this_device": d.id == me.id,
            })
        })
        .collect();
    Json(list).into_response()
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

    // Branch on *presence* first: an Origin that exists but isn't valid text
    // must be rejected, not skipped as if absent (Codex).
    if let Some(origin_val) = headers.get(header::ORIGIN) {
        // Parse strictly with `url` rather than string-splitting: reject an
        // Origin carrying credentials (`https://allowed@evil` — the old splitter
        // read the host as `allowed`) and compare the REAL host. A non-text or
        // malformed Origin is rejected outright.
        let origin_ok = origin_val
            .to_str()
            .ok()
            .and_then(|origin| url::Url::parse(origin).ok())
            .filter(|u| u.username().is_empty() && u.password().is_none())
            .and_then(|u| u.host_str().map(|h| allowed(&app, strip_brackets(h))))
            .unwrap_or(false);
        if !origin_ok {
            tracing::warn!("rejected request: bad or non-text Origin header");
            return Err(StatusCode::FORBIDDEN);
        }
    }

    // Authenticated API responses can carry command summaries / attention detail;
    // keep them out of the browser's HTTP cache. Static assets may still cache.
    let is_api = request.uri().path().starts_with("/api");
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    if is_api {
        h.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store, private"),
        );
    }
    Ok(response)
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

/// `url::Url::host_str` keeps the brackets on an IPv6 literal (`[::1]`); the
/// allowlist stores bare hosts, so strip them to compare like-for-like.
fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
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
