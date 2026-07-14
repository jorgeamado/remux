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
async fn permission_decide_confirms_delivery() {
    let (addr, app) = start_server("it-perm-deliver").await;
    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "phone").unwrap();
    let id = app.auth.devices()[0].id.clone();
    app.auth.set_approve(&id, true).unwrap();

    // A stand-in for the held-wait: receive the decision and confirm delivery.
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
    assert_eq!(body["delivered"], true);
    assert_eq!(body["session"], "it-perm-deliver");
    // Consumed.
    assert!(app.perms.snapshot().is_empty());
}
