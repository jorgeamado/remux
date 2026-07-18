//! Integration tests for the HTTP surface: Host/Origin guard and pairing.

mod common;

use common::start_server;
use reqwest::StatusCode;

#[tokio::test]
async fn health_ok_with_valid_host() {
    let (addr, _app) = start_server("it-health").await;
    let resp = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn rejects_bad_host_header() {
    let (addr, _app) = start_server("it-host").await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/api/health"))
        .header("Host", "evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rejects_bad_origin_header() {
    let (addr, _app) = start_server("it-origin").await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/api/health"))
        .header("Origin", "https://evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn allows_same_origin() {
    let (addr, _app) = start_server("it-sameorigin").await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/api/health"))
        .header("Origin", format!("http://127.0.0.1:{}", addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn rejects_origin_with_embedded_credentials() {
    // The real host is evil.example.com; the allowlisted 127.0.0.1 is only
    // userinfo. The old string-splitter read the host as 127.0.0.1 and allowed
    // it; the url-based parse must reject.
    let (addr, _app) = start_server("it-origin-creds").await;
    let client = reqwest::Client::new();
    for origin in [
        "http://127.0.0.1:80@evil.example.com",
        "http://127.0.0.1@evil.example.com",
        "not-a-valid-origin",
    ] {
        let resp = client
            .get(format!("http://{addr}/api/health"))
            .header("Origin", origin)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "origin: {origin}");
    }
}

#[tokio::test]
async fn rejects_non_text_origin() {
    // An Origin header that exists but isn't valid text must be rejected, not
    // silently skipped as if absent.
    let (addr, _app) = start_server("it-origin-bytes").await;
    let client = reqwest::Client::new();
    let val = reqwest::header::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap();
    let resp = client
        .get(format!("http://{addr}/api/health"))
        .header("Origin", val)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn api_responses_are_no_store() {
    // Authenticated API responses can carry command summaries — keep them out of
    // the browser HTTP cache.
    let (addr, _app) = start_server("it-nostore").await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/api/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store, private")
    );
}

#[tokio::test]
async fn pairing_flow() {
    let (addr, app) = start_server("it-pair").await;
    let client = reqwest::Client::new();

    // invalid token -> 401
    let resp = client
        .post(format!("http://{addr}/api/pair"))
        .json(&serde_json::json!({"token": "bogus", "device_name": "t"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // valid token -> device token
    let pairing = app.auth.new_pairing_token();
    let resp = client
        .post(format!("http://{addr}/api/pair"))
        .json(&serde_json::json!({"token": pairing, "device_name": "test phone"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let device_token = body["device_token"].as_str().unwrap().to_string();
    assert_eq!(device_token.len(), 64);
    assert_eq!(
        app.auth.authenticate(&device_token).map(|d| d.name),
        Some("test phone".to_string())
    );

    // Tokens are reusable within their TTL (iOS pairs twice: Safari tab +
    // installed PWA with partitioned storage).
    let resp = client
        .post(format!("http://{addr}/api/pair"))
        .json(&serde_json::json!({"token": pairing, "device_name": "installed pwa"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn sessions_require_device_token() {
    let (addr, app) = start_server("it-sessions").await;
    let client = reqwest::Client::new();

    // no/bad token -> 401
    let resp = client
        .get(format!("http://{addr}/api/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = client
        .get(format!("http://{addr}/api/sessions"))
        .header("Authorization", "Bearer bogus")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // paired device -> JSON array
    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "t").unwrap();
    let resp = client
        .get(format!("http://{addr}/api/sessions"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body.is_array(), "expected array, got {body}");
}

#[tokio::test]
async fn windows_endpoint_auth_and_validation() {
    let (addr, app) = start_server("it-windows").await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{addr}/api/windows?session=it-windows"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "t").unwrap();

    let resp = client
        .get(format!("http://{addr}/api/windows?session=bad:name"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = client
        .get(format!("http://{addr}/api/windows?session=it-windows"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body.is_array(), "expected array, got {body}");
}

#[tokio::test]
async fn push_endpoints_auth_and_validation() {
    let (addr, app) = start_server("it-push").await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{addr}/api/push/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "t").unwrap();
    let auth = format!("Bearer {token}");

    let resp = client
        .get(format!("http://{addr}/api/push/key"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(!body["key"].as_str().unwrap().is_empty());

    // Non-allowlisted endpoint is refused (SSRF guard).
    let resp = client
        .post(format!("http://{addr}/api/push/subscribe"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"endpoint": "https://evil.example.com/x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A real push-service endpoint is accepted, and unsubscribe works.
    let resp = client
        .post(format!("http://{addr}/api/push/subscribe"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({
            "endpoint": "https://web.push.apple.com/QOXtest",
            "keys": {"p256dh": "pk", "auth": "as"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = client
        .post(format!("http://{addr}/api/push/unsubscribe"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"endpoint": "https://web.push.apple.com/QOXtest"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Attention listing starts empty.
    let resp = client
        .get(format!("http://{addr}/api/attention"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["sessions"], serde_json::json!([]));
}

#[tokio::test]
async fn devices_endpoint_read_only_list() {
    let (addr, app) = start_server("it-devices").await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/api/devices"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "sheet phone").unwrap();
    let resp = client
        .get(format!("http://{addr}/api/devices"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let list = body.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["name"], "sheet phone");
    assert_eq!(list[0]["this_device"], true);
    assert!(
        list[0].get("token_sha256").is_none(),
        "no secrets in the sheet"
    );
}

#[tokio::test]
async fn serves_embedded_index() {
    let (addr, _app) = start_server("it-static").await;
    let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let csp = resp
        .headers()
        .get("content-security-policy")
        .expect("CSP header on index");
    assert!(csp.to_str().unwrap().contains("default-src 'self'"));
}

#[tokio::test]
async fn shell_revalidates_but_hashed_assets_are_immutable() {
    // A new deploy must be picked up: the HTML shell and the service worker must
    // not be cached (or the old index.html keeps pointing at the old, hashed-away
    // JS bundle). The content-hashed build assets, in contrast, are immutable.
    let (addr, _app) = start_server("it-cache").await;
    let cc = |body: reqwest::Response| {
        body.headers()
            .get("cache-control")
            .map(|v| v.to_str().unwrap().to_string())
    };

    let index = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(
        cc(index).as_deref(),
        Some("no-cache"),
        "index.html must revalidate"
    );

    let sw = reqwest::get(format!("http://{addr}/sw.js")).await.unwrap();
    assert_eq!(cc(sw).as_deref(), Some("no-cache"), "sw.js must revalidate");

    // Discover a hashed asset from the served index and check it's immutable.
    let html = reqwest::get(format!("http://{addr}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let asset = html
        .split('"')
        .find(|s| s.starts_with("/assets/") && (s.ends_with(".js") || s.ends_with(".css")))
        .expect("index references a hashed asset");
    let a = reqwest::get(format!("http://{addr}{asset}")).await.unwrap();
    assert_eq!(a.status(), StatusCode::OK);
    assert_eq!(
        cc(a).as_deref(),
        Some("public, max-age=31536000, immutable"),
        "hashed assets are immutable"
    );
}

/// Build an open card and keep its waiter receiver alive so the registry holds
/// it (dropping the receiver would still leave it listed, but this mirrors a
/// live hook). `_rx` must be held by the caller.
fn a_card(id: &str, session: &str) -> remux::permit::Card {
    let now = std::time::Instant::now();
    remux::permit::Card {
        id: id.into(),
        session: session.into(),
        pane: "%1".into(),
        source: "claude-code".into(),
        tool: "Bash".into(),
        summary: "touch x".into(),
        truncated: false,
        prompt_id: None,
        created: now,
        deadline: now + remux::permit::CARD_TTL,
    }
}

#[tokio::test]
async fn permissions_visibility_is_approve_gated() {
    let (addr, app) = start_server("it-perm-vis").await;
    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "phone").unwrap();
    let id = app.auth.devices()[0].id.clone();
    let _rx = app.perms.insert(a_card("card1", "it-perm-vis")).unwrap();
    let client = reqwest::Client::new();

    // no token -> 401
    let resp = client
        .get(format!("http://{addr}/api/permissions"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // paired but not approve-capable -> empty list (no details, no leak)
    let resp = client
        .get(format!("http://{addr}/api/permissions"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["cards"].as_array().unwrap().len(), 0);

    // grant approve -> the card (with details + a live countdown) is visible
    app.auth.set_approve(&id, true).unwrap();
    let resp = client
        .get(format!("http://{addr}/api/permissions"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let cards = body["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], "card1");
    assert_eq!(cards[0]["tool"], "Bash");
    assert_eq!(cards[0]["summary"], "touch x");
    assert!(cards[0]["remaining_secs"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn permission_decide_gating_and_validation() {
    let (addr, app) = start_server("it-perm-decide").await;
    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "phone").unwrap();
    let id = app.auth.devices()[0].id.clone();
    let client = reqwest::Client::new();

    // not approve-capable -> 403
    let resp = client
        .post(format!("http://{addr}/api/permissions/whatever/decide"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"decision": "allow"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    app.auth.set_approve(&id, true).unwrap();

    // bad decision string -> 400
    let resp = client
        .post(format!("http://{addr}/api/permissions/whatever/decide"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"decision": "maybe"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // unknown id -> 404 (fast: resolve returns Unknown, no confirm wait)
    let resp = client
        .post(format!("http://{addr}/api/permissions/nonexistent/decide"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"decision": "allow"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn permission_decide_confirms_write() {
    let (addr, app) = start_server("it-perm-deliver").await;
    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "phone").unwrap();
    let id = app.auth.devices()[0].id.clone();
    app.auth.set_approve(&id, true).unwrap();

    // A stand-in for the held-wait: receive the decision and confirm the write.
    let rx = app
        .perms
        .insert(a_card("cardD", "it-perm-deliver"))
        .unwrap();
    tokio::spawn(async move {
        if let Ok((_decision, confirm)) = rx.await {
            let _ = confirm.send(());
        }
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/api/permissions/cardD/decide"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"decision": "allow"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["written"], true);
    assert_eq!(body["session"], "it-perm-deliver");
    // Consumed.
    assert!(app.perms.snapshot().is_empty());
}

// ---------- multi-machine: /api/meta + client-origin CORS ----------

#[tokio::test]
async fn meta_requires_auth_and_reports_identity() {
    let (addr, app) = common::start_server_with("it-meta", &[]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{addr}/api/meta"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "t").unwrap();
    let resp = client
        .get(format!("http://{addr}/api/meta"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["machine_id"], app.machine_id);
    assert_eq!(body["name"], "test-machine");
    assert_eq!(body["protocol"]["api"], 1);
    assert!(body["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "terminal"));
}

#[tokio::test]
async fn allowlisted_client_origin_gets_cors_on_api() {
    let (addr, app) = common::start_server_with("it-cors", &["https://home.ts.net:7777"]).await;
    let client = reqwest::Client::new();

    // Preflight (what the browser sends before a cross-origin authed fetch).
    let resp = client
        .request(reqwest::Method::OPTIONS, format!("http://{addr}/api/meta"))
        .header("Origin", "https://home.ts.net:7777")
        .header("Access-Control-Request-Method", "GET")
        .header("Access-Control-Request-Headers", "authorization")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let h = resp.headers();
    assert_eq!(
        h.get("access-control-allow-origin").unwrap(),
        "https://home.ts.net:7777"
    );
    assert_eq!(h.get("access-control-allow-methods").unwrap(), "GET, POST");
    assert_eq!(
        h.get("access-control-allow-headers").unwrap(),
        "authorization, content-type"
    );

    // Actual request: grant echoed, Vary set, and the endpoint works.
    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "t").unwrap();
    let resp = client
        .get(format!("http://{addr}/api/meta"))
        .header("Origin", "https://home.ts.net:7777")
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "https://home.ts.net:7777"
    );
    assert_eq!(resp.headers().get("vary").unwrap(), "Origin");
}

#[tokio::test]
async fn client_origin_is_exact_not_hostname() {
    // The grant is the whole origin: same hostname on a different scheme or
    // port must NOT pass the guard (that's what --allowed-host would do).
    let (addr, _app) =
        common::start_server_with("it-cors-exact", &["https://home.ts.net:7777"]).await;
    let client = reqwest::Client::new();
    for origin in [
        "http://home.ts.net:7777",  // scheme downgrade
        "https://home.ts.net",      // different (default) port
        "https://home.ts.net:7778", // different port
    ] {
        let resp = client
            .get(format!("http://{addr}/api/health"))
            .header("Origin", origin)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "origin: {origin}");
    }
}

#[tokio::test]
async fn foreign_origin_without_allowlist_gets_no_cors() {
    // Same-origin (host-allowlisted) requests are served but never get a CORS
    // grant — the browser doesn't need one, and echoing would be a footgun.
    let (addr, _app) = common::start_server_with("it-cors-none", &[]).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/api/health"))
        .header("Origin", format!("http://127.0.0.1:{}", addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}
