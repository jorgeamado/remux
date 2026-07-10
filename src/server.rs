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
        .route("/ws", any(ws::handler))
        .fallback(static_handler)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
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

    let host_ok = header_host(headers, header::HOST)
        .map(|h| allowed(&app, &h))
        .unwrap_or(false);
    if !host_ok {
        tracing::warn!("rejected request: bad Host header");
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
    app.allowed_hosts.iter().any(|h| h.eq_ignore_ascii_case(host))
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
