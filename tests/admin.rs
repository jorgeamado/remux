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
