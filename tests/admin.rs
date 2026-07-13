//! The local admin socket: 0600, line-JSON, mints working pairing tokens.

mod common;

use clap::Parser;
use remux::{auth::Auth, App, Args};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

#[tokio::test]
async fn admin_socket_mints_usable_pairing_tokens() {
    let dir = std::env::temp_dir().join(format!("remux-admin-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let auth = Auth::load(dir.join("devices.json")).unwrap();
    let args = Args::parse_from(["remux", "--session", "adm", "--no-pair"]);
    let app = Arc::new(App {
        allowed_hosts: vec![],
        auth,
        args,
        attention: tokio::sync::broadcast::channel(4).0,
        public_url: "https://host.example:7777".into(),
        push: remux::push::Push::load(&dir).unwrap(),
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
    });
    remux::admin::spawn(app.clone(), &dir).unwrap();

    // Socket must not be readable by anyone else.
    let mode = std::fs::metadata(remux::admin::socket_path(&dir))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    let url = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || remux::admin::request_pairing(&dir))
            .await
            .unwrap()
            .unwrap()
    };
    let token = url
        .split("#pair=")
        .nth(1)
        .expect("pair fragment")
        .to_string();
    assert!(url.starts_with("https://host.example:7777/#pair="));

    // The minted token actually pairs a device.
    let device_token = app.auth.pair(&token, "cli-invited").unwrap();
    assert!(app.auth.authenticate(&device_token).is_some());
}

#[tokio::test]
async fn second_daemon_does_not_steal_live_admin_socket() {
    let dir = std::env::temp_dir().join(format!("remux-admin-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let auth = Auth::load(dir.join("devices.json")).unwrap();
    let args = Args::parse_from(["remux", "--session", "adm3", "--no-pair"]);
    let app = Arc::new(App {
        allowed_hosts: vec![],
        auth,
        args,
        attention: tokio::sync::broadcast::channel(4).0,
        public_url: "http://x".into(),
        push: remux::push::Push::load(&dir).unwrap(),
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
    });
    remux::admin::spawn(app.clone(), &dir).unwrap();
    // A second spawn on the same state dir must refuse — and must NOT have
    // removed the live socket.
    assert!(remux::admin::spawn(app.clone(), &dir).is_err());
    let dir2 = dir.clone();
    let url = tokio::task::spawn_blocking(move || remux::admin::request_pairing(&dir2))
        .await
        .unwrap()
        .unwrap();
    assert!(url.contains("#pair="));
}

#[tokio::test]
async fn stale_socket_file_is_replaced() {
    let dir = std::env::temp_dir().join(format!("remux-admin-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    // A leftover socket file with no listener behind it (unclean shutdown).
    drop(tokio::net::UnixListener::bind(remux::admin::socket_path(&dir)).unwrap());
    let auth = Auth::load(dir.join("devices.json")).unwrap();
    let args = Args::parse_from(["remux", "--session", "adm4", "--no-pair"]);
    let app = Arc::new(App {
        allowed_hosts: vec![],
        auth,
        args,
        attention: tokio::sync::broadcast::channel(4).0,
        public_url: "http://x".into(),
        push: remux::push::Push::load(&dir).unwrap(),
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
    });
    remux::admin::spawn(app, &dir).unwrap();
}

#[tokio::test]
async fn admin_socket_rejects_garbage() {
    let dir = std::env::temp_dir().join(format!("remux-admin-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let auth = Auth::load(dir.join("devices.json")).unwrap();
    let args = Args::parse_from(["remux", "--session", "adm2", "--no-pair"]);
    let app = Arc::new(App {
        allowed_hosts: vec![],
        auth,
        args,
        attention: tokio::sync::broadcast::channel(4).0,
        public_url: "http://x".into(),
        push: remux::push::Push::load(&dir).unwrap(),
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
    });
    remux::admin::spawn(app.clone(), &dir).unwrap();

    let path = remux::admin::socket_path(&dir);
    let reply = tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, BufReader, Write};
        let mut s = std::os::unix::net::UnixStream::connect(&path).unwrap();
        s.write_all(b"{\"cmd\":\"rm_rf\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        line
    })
    .await
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(reply.trim()).unwrap();
    assert_eq!(v["ok"], false);
}

#[tokio::test]
async fn revoke_cascades_and_cancels_pairing() {
    let dir = std::env::temp_dir().join(format!("remux-admin-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let auth = Auth::load(dir.join("devices.json")).unwrap();
    let args = Args::parse_from(["remux", "--session", "adm5", "--no-pair"]);
    let app = Arc::new(App {
        allowed_hosts: vec![],
        auth,
        args,
        attention: tokio::sync::broadcast::channel(4).0,
        public_url: "http://x".into(),
        push: remux::push::Push::load(&dir).unwrap(),
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
    });
    remux::admin::spawn(app.clone(), &dir).unwrap();

    // Pair a device and give it a push subscription.
    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "victim phone").unwrap();
    let id = app.auth.devices()[0].id.clone();
    app.push
        .subscribe(remux::push::Subscription {
            device_id: id.clone(),
            endpoint: "https://web.push.apple.com/x".into(),
            p256dh: String::new(),
            auth: String::new(),
        })
        .unwrap();
    // A second, unused pairing token is outstanding when the incident hits.
    let outstanding = app.auth.new_pairing_token();

    let mut revoked_rx = app.revoked.subscribe();
    let v = {
        let dir = dir.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            remux::admin::request(&dir, serde_json::json!({"cmd": "revoke", "id": id}))
        })
        .await
        .unwrap()
        .unwrap()
    };
    assert_eq!(v["ok"], true);

    // Cascade: token dead, subscriptions gone, live sockets signalled,
    // outstanding pairing tokens cancelled.
    assert!(app.auth.authenticate(&token).is_none());
    assert!(app.auth.devices().is_empty());
    assert_eq!(revoked_rx.try_recv().unwrap(), id);
    assert!(std::fs::read_to_string(dir.join("push.json"))
        .unwrap_or_default()
        .trim_start()
        .starts_with("[]"));
    assert!(app.auth.pair(&outstanding, "attacker").is_err());

    // devices list over the admin socket reflects it.
    let v = tokio::task::spawn_blocking(move || {
        remux::admin::request(&dir, serde_json::json!({"cmd": "devices"}))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(v["devices"], serde_json::json!([]));
}
