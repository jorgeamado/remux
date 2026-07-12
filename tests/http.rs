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
        app.auth.authenticate(&device_token),
        Some("test phone".to_string())
    );

    // token is single use -> 401 on reuse
    let resp = client
        .post(format!("http://{addr}/api/pair"))
        .json(&serde_json::json!({"token": pairing, "device_name": "again"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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
