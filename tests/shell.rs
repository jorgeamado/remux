//! The shell datagram socket (M4c): fire-and-forget command events landing in
//! the per-session feed, pane→session mapping, and forgery being inert.

mod common;

use clap::Parser;
use remux::{auth::Auth, tmux, App, Args};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

fn test_app(dir: &std::path::Path, session: &str) -> Arc<App> {
    let auth = Auth::load(dir.join("devices.json")).unwrap();
    let args = Args::parse_from(["remux", "--session", session, "--no-pair"]);
    Arc::new(App {
        allowed_hosts: vec![],
        auth,
        args,
        attention: tokio::sync::broadcast::channel(4).0,
        public_url: "http://x".into(),
        push: remux::push::Push::load(dir).unwrap(),
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
        perms: Default::default(),
        feed: Default::default(),
    })
}

fn snapshot(session: &str) -> remux::topology::Snapshot {
    Arc::new(vec![tmux::SessionWindows {
        name: session.into(),
        attached: false,
        windows: vec![tmux::WindowInfo {
            index: 0,
            active: true,
            zoomed: false,
            name: "zsh".into(),
            panes: vec![tmux::PaneInfo {
                id: "%1".into(),
                index: 0,
                active: true,
                command: "zsh".into(),
            }],
        }],
    }])
}

fn send(dir: &std::path::Path, body: serde_json::Value) {
    let path = remux::shell::socket_path(dir);
    let sock = std::os::unix::net::UnixDatagram::unbound().unwrap();
    sock.send_to(body.to_string().as_bytes(), &path).unwrap();
}

async fn await_feed<F: Fn(&[serde_json::Value]) -> bool>(
    app: &Arc<App>,
    session: &str,
    pred: F,
) -> Vec<serde_json::Value> {
    for _ in 0..200 {
        let snap = app.feed.snapshot(session);
        if pred(&snap) {
            return snap;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("feed predicate not satisfied within 2s");
}

#[tokio::test]
async fn command_start_then_finish_lands_in_the_session_feed() {
    let dir = std::env::temp_dir().join(format!("remux-shell-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "sh1");
    app.topology.send_replace(snapshot("sh1"));
    remux::shell::spawn(app.clone(), &dir).unwrap();

    // 0600 socket.
    let mode = std::fs::metadata(remux::shell::socket_path(&dir))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    send(
        &dir,
        serde_json::json!({
            "v": 1, "kind": "command_started", "pane": "%1", "source": "shell",
            "shell_id": "sh-a", "command_id": 1, "command": "cargo build", "cwd": "/w"
        }),
    );
    let snap = await_feed(&app, "sh1", |s| s.len() == 1).await;
    assert_eq!(snap[0]["command"], "cargo build");
    assert_eq!(snap[0]["state"], "running");

    send(
        &dir,
        serde_json::json!({
            "v": 1, "kind": "command_finished", "source": "shell",
            "shell_id": "sh-a", "command_id": 1, "exit": 101
        }),
    );
    let snap = await_feed(&app, "sh1", |s| {
        s.first().map(|c| c["state"] == "done").unwrap_or(false)
    })
    .await;
    assert_eq!(snap[0]["exit"], 101);
}

#[tokio::test]
async fn unknown_pane_and_malformed_events_are_inert() {
    let dir = std::env::temp_dir().join(format!("remux-shell-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "sh2");
    app.topology.send_replace(snapshot("sh2"));
    remux::shell::spawn(app.clone(), &dir).unwrap();

    // pane not in topology
    send(
        &dir,
        serde_json::json!({
            "v": 1, "kind": "command_started", "pane": "%99", "source": "shell",
            "shell_id": "x", "command_id": 1, "command": "ls", "cwd": "/w"
        }),
    );
    // junk shell_id
    send(
        &dir,
        serde_json::json!({
            "v": 1, "kind": "command_started", "pane": "%1", "source": "shell",
            "shell_id": "has space", "command_id": 1, "command": "ls", "cwd": "/w"
        }),
    );
    // unknown kind
    send(
        &dir,
        serde_json::json!({"v": 1, "kind": "rm_rf", "pane": "%1", "source": "shell"}),
    );
    // give the datagrams time to be processed (and ignored)
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        app.feed.snapshot("sh2").is_empty(),
        "no malformed event may populate the feed"
    );
}
