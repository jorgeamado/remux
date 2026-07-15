//! TLS smoke test: the https listener must come up and answer. Catches
//! process-level rustls misconfiguration (e.g. two crypto providers in the
//! dependency graph with none installed), which plain-HTTP tests never hit.

mod common;

use clap::Parser;
use remux::{auth::Auth, App, Args};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn https_listener_serves_health() {
    remux::init_crypto();

    let dir = std::env::temp_dir().join(format!("remux-tls-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

    // Grab a free port (small race window; fine for a test).
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let listen = format!("127.0.0.1:{port}");

    let auth = Auth::load(dir.join("devices.json")).unwrap();
    let args = Args::parse_from([
        "remux",
        "--listen",
        &listen,
        "--session",
        "tlssmoke",
        "--no-pair",
        "--tls-cert",
        cert_path.to_str().unwrap(),
        "--tls-key",
        key_path.to_str().unwrap(),
    ]);
    let app = Arc::new(App {
        allowed_hosts: vec!["localhost".into(), "127.0.0.1".into()],
        auth,
        args,
        attention: tokio::sync::broadcast::channel(4).0,
        public_url: format!("https://{listen}"),
        push: remux::push::Push::load(&dir).unwrap(),
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
        perms: Default::default(),
        feed: Default::default(),
        detector_reset: tokio::sync::broadcast::channel(16).0,
    });
    tokio::spawn(async move {
        if let Err(e) = remux::server::run(app).await {
            eprintln!("server exited: {e:#}");
        }
    });

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let mut ok = false;
    for _ in 0..50 {
        if let Ok(resp) = client
            .get(format!("https://{listen}/api/health"))
            .send()
            .await
        {
            assert_eq!(resp.text().await.unwrap(), "ok");
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "https listener never answered");
}
