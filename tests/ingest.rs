//! The local ingest socket: 0600, strict one-line JSON events, pane→session
//! mapping via topology, and the attention pipeline as the only effect.

mod common;

use clap::Parser;
use remux::{auth::Auth, tmux, App, Args};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

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
    })
}

/// A topology snapshot with one session holding pane %1.
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

fn send(dir: &std::path::Path, body: serde_json::Value) -> serde_json::Value {
    use std::io::{BufRead, BufReader, Write};
    let path = remux::ingest::socket_path(dir);
    let mut s = std::os::unix::net::UnixStream::connect(&path).unwrap();
    s.write_all(format!("{body}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

#[tokio::test]
async fn needs_input_event_raises_attention_for_the_panes_session() {
    let dir = std::env::temp_dir().join(format!("remux-ingest-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "ing1");
    app.topology.send_replace(snapshot("ing1"));
    remux::ingest::spawn(app.clone(), &dir).unwrap();

    let mode = std::fs::metadata(remux::ingest::socket_path(&dir))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    let mut attention = app.attention.subscribe();
    let dir2 = dir.clone();
    let v = tokio::task::spawn_blocking(move || {
        send(
            &dir2,
            serde_json::json!({
                "v": 1, "kind": "agent_needs_input", "pane": "%1",
                "source": "claude-code", "message": "permission needed"
            }),
        )
    })
    .await
    .unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["session"], "ing1");
    assert_eq!(attention.try_recv().unwrap(), "ing1");
}

#[tokio::test]
async fn rejects_unknown_pane_kind_version_and_extra_fields() {
    let dir = std::env::temp_dir().join(format!("remux-ingest-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "ing2");
    app.topology.send_replace(snapshot("ing2"));
    remux::ingest::spawn(app.clone(), &dir).unwrap();
    let mut attention = app.attention.subscribe();

    let cases = [
        // pane not in topology
        serde_json::json!({"v":1,"kind":"agent_needs_input","pane":"%99","source":"s"}),
        // unknown kind
        serde_json::json!({"v":1,"kind":"open_the_pod_bay_doors","pane":"%1","source":"s"}),
        // wrong version
        serde_json::json!({"v":2,"kind":"agent_needs_input","pane":"%1","source":"s"}),
        // extra field — strict schema
        serde_json::json!({"v":1,"kind":"agent_needs_input","pane":"%1","source":"s","cmd":"revoke"}),
        // pane id must be %N-shaped
        serde_json::json!({"v":1,"kind":"agent_needs_input","pane":"main:0.0","source":"s"}),
        serde_json::json!({"v":1,"kind":"agent_needs_input","pane":"%","source":"s"}),
        // empty source
        serde_json::json!({"v":1,"kind":"agent_needs_input","pane":"%1","source":""}),
    ];
    for body in cases {
        let dir2 = dir.clone();
        let v = tokio::task::spawn_blocking(move || send(&dir2, body))
            .await
            .unwrap();
        assert_eq!(v["ok"], false, "accepted: {v}");
    }
    assert!(
        attention.try_recv().is_err(),
        "no event may raise attention"
    );
}

#[tokio::test]
async fn pane_linked_into_two_sessions_is_ambiguous() {
    let dir = std::env::temp_dir().join(format!("remux-ingest-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "ing4");
    // Linked window: pane %1 appears in both sessions. Guessing would
    // notify the wrong one — the daemon must refuse instead.
    let mut two = (*snapshot("ing4")).clone();
    two.extend((*snapshot("ing4b")).clone());
    app.topology.send_replace(Arc::new(two));
    remux::ingest::spawn(app.clone(), &dir).unwrap();
    let mut attention = app.attention.subscribe();

    let dir2 = dir.clone();
    let v = tokio::task::spawn_blocking(move || {
        send(
            &dir2,
            serde_json::json!({"v":1,"kind":"agent_needs_input","pane":"%1","source":"s"}),
        )
    })
    .await
    .unwrap();
    assert_eq!(v["ok"], false);
    assert!(attention.try_recv().is_err());
}

#[tokio::test]
async fn oversized_line_is_rejected_not_buffered() {
    let dir = std::env::temp_dir().join(format!("remux-ingest-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "ing3");
    app.topology.send_replace(snapshot("ing3"));
    remux::ingest::spawn(app.clone(), &dir).unwrap();
    let mut attention = app.attention.subscribe();

    // The daemon stops reading at its line cap and answers/closes; our
    // remaining bytes may hit a broken pipe — that IS the rejection.
    let big = "x".repeat(64 * 1024);
    let body = serde_json::json!(
        {"v":1,"kind":"agent_needs_input","pane":"%1","source":"s","message":big}
    );
    let path = remux::ingest::socket_path(&dir);
    let rejected = tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, BufReader, Write};
        let mut s = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let write_failed = s.write_all(format!("{body}\n").as_bytes()).is_err();
        let mut line = String::new();
        let _ = BufReader::new(s).read_line(&mut line);
        let error_response = serde_json::from_str::<serde_json::Value>(line.trim())
            .map(|v| v["ok"] == serde_json::json!(false))
            .unwrap_or(false);
        write_failed || error_response
    })
    .await
    .unwrap();
    assert!(rejected);
    assert!(
        attention.try_recv().is_err(),
        "oversized event must not fire"
    );
}
