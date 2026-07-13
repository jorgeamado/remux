//! Attention → Web Push dispatch against a fake local push service:
//! VAPID-authorized POST arrives, and an expired (410) subscription is pruned.

mod common;

use axum::{extract::State, http::HeaderMap, routing::post, Router};
use clap::Parser;
use remux::{auth::Auth, push::Subscription, App, Args};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Seen {
    auth_headers: Vec<String>,
}

async fn fake_push(State(seen): State<Arc<Mutex<Seen>>>, headers: HeaderMap) -> &'static str {
    seen.lock().unwrap().auth_headers.push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
    );
    "created"
}

#[tokio::test]
async fn attention_dispatches_vapid_push_and_prunes_gone() {
    std::env::set_var("REMUX_PUSH_ALLOW_HOST", "127.0.0.1");

    // Fake push service: /ok records requests, /gone answers 410.
    let seen = Arc::new(Mutex::new(Seen::default()));
    let router = Router::new()
        .route("/ok", post(fake_push))
        .route("/ok2", post(fake_push))
        .route(
            "/gone",
            post(|| async { (axum::http::StatusCode::GONE, "gone") }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let push_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let dir = std::env::temp_dir().join(format!("remux-pd-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let auth = Auth::load(dir.join("devices.json")).unwrap();
    let args = Args::parse_from(["remux", "--session", "pd", "--no-pair"]);
    let app = Arc::new(App {
        allowed_hosts: vec![],
        auth,
        args,
        attention: tokio::sync::broadcast::channel(8).0,
        public_url: "http://x".into(),
        push: remux::push::Push::load(&dir).unwrap(),
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
    });
    app.push
        .subscribe(Subscription {
            device_id: "locked-phone".into(),
            endpoint: format!("http://{push_addr}/ok"),
            p256dh: String::new(),
            auth: String::new(),
        })
        .unwrap();
    app.push
        .subscribe(Subscription {
            device_id: "stale-phone".into(),
            endpoint: format!("http://{push_addr}/gone"),
            p256dh: String::new(),
            auth: String::new(),
        })
        .unwrap();
    // A device with a live socket on the session must NOT be pushed.
    app.push
        .subscribe(Subscription {
            device_id: "watching-phone".into(),
            endpoint: format!("http://{push_addr}/ok2"),
            p256dh: String::new(),
            auth: String::new(),
        })
        .unwrap();

    app.connections
        .lock()
        .unwrap()
        .insert(("watching-phone".into(), "pd".into()), 1);

    remux::push::spawn_dispatcher(app.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    app.attention.send("pd".to_string()).unwrap();

    // One push to /ok with a VAPID header; /gone pruned from the store.
    let mut delivered = false;
    for _ in 0..50 {
        if !seen.lock().unwrap().auth_headers.is_empty() {
            delivered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(delivered, "no push arrived at the fake service");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.auth_headers.len(), 1, "expected exactly one push");
        assert!(
            seen.auth_headers[0].starts_with("vapid t="),
            "missing VAPID auth: {:?}",
            seen.auth_headers[0]
        );
        assert!(seen.auth_headers[0].contains(", k="));
    }

    // /api/attention bookkeeping happened.
    assert!(app.pending_attention.lock().unwrap().contains_key("pd"));

    // The 410 endpoint disappears from the persisted store.
    let mut pruned = false;
    for _ in 0..50 {
        let raw = std::fs::read_to_string(dir.join("push.json")).unwrap_or_default();
        if !raw.contains("/gone") {
            pruned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(pruned, "410 subscription was not pruned");
}
