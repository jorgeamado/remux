//! The local ingest socket: 0600, strict one-line JSON events, pane→session
//! mapping via topology, and the attention pipeline as the only effect.

mod common;

use clap::Parser;
use remux::{auth::Auth, permit::Decision, tmux, App, Args};
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
        agents: Default::default(),
        pane_views: Default::default(),
        dash_windows: Default::default(),
        feed: Default::default(),
        detector_reset: tokio::sync::broadcast::channel(16).0,
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
    let att = attention.try_recv().unwrap();
    assert_eq!(att.session, "ing1");
    assert_eq!(att.kind, "agent_needs_input");
    assert_eq!(att.reason.as_deref(), Some("permission needed"));
    assert_eq!(att.source.as_deref(), Some("claude-code"));
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

/// Wait (bounded) until the registry snapshot satisfies `pred`, returning it.
async fn await_perms<F: Fn(&[remux::permit::Card]) -> bool>(
    app: &Arc<App>,
    pred: F,
) -> Vec<remux::permit::Card> {
    for _ in 0..200 {
        let snap = app.perms.snapshot();
        if pred(&snap) {
            return snap;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("registry predicate not satisfied within 2s");
}

#[tokio::test]
async fn permission_card_resolves_and_wakes_the_blocked_hook() {
    let dir = std::env::temp_dir().join(format!("remux-ingest-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "perm1");
    app.topology.send_replace(snapshot("perm1"));
    remux::ingest::spawn(app.clone(), &dir).unwrap();

    // The "hook": send the permission event and block reading the ack.
    let dir2 = dir.clone();
    let client = tokio::task::spawn_blocking(move || {
        send(
            &dir2,
            serde_json::json!({
                "v": 1, "kind": "agent_permission", "pane": "%1",
                "source": "claude-code", "tool": "Bash", "summary": "touch x"
            }),
        )
    });

    // The card appears; the device (here, the test) approves it.
    let snap = await_perms(&app, |s| s.len() == 1).await;
    let card = &snap[0];
    assert_eq!(card.session, "perm1");
    assert_eq!(card.tool, "Bash");
    assert_eq!(card.summary, "touch x");
    let (resolved, confirm) = app
        .perms
        .resolve(&card.id, Decision::Allow, || true)
        .unwrap();
    assert_eq!(resolved.id, card.id);

    // The blocked hook wakes with the decision, and the card is consumed.
    let v = client.await.unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["decision"], "allow");
    assert_eq!(app.perms.snapshot().len(), 0);
    // Delivery is confirmed: the waiter wrote the decision to a live socket.
    assert!(tokio::time::timeout(Duration::from_secs(2), confirm)
        .await
        .unwrap()
        .is_ok());
}

#[tokio::test]
async fn mid_length_summary_is_shown_in_full_not_capped() {
    // Regression guard: an earlier double-cap (sanitize's 256) would clip a
    // 300-char command to 256 and *still* mark it truncated:false. It must now
    // reach the card in full, and Allow must work.
    let dir = std::env::temp_dir().join(format!("remux-ingest-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "permM");
    app.topology.send_replace(snapshot("permM"));
    remux::ingest::spawn(app.clone(), &dir).unwrap();

    let long = "a".repeat(300);
    let long2 = long.clone();
    let dir2 = dir.clone();
    let client = tokio::task::spawn_blocking(move || {
        send(
            &dir2,
            serde_json::json!({
                "v": 1, "kind": "agent_permission", "pane": "%1",
                "source": "claude-code", "tool": "Bash", "summary": long2
            }),
        )
    });

    let snap = await_perms(&app, |s| s.len() == 1).await;
    let card = &snap[0];
    assert_eq!(card.summary.chars().count(), 300, "shown in full");
    assert!(!card.truncated, "300 < MAX_SUMMARY, nothing hidden");
    assert_eq!(card.summary, long);
    app.perms
        .resolve(&card.id, Decision::Allow, || true)
        .unwrap();
    let v = client.await.unwrap();
    assert_eq!(v["decision"], "allow");
}

#[tokio::test]
async fn over_long_summary_is_flagged_and_refuses_remote_allow() {
    let dir = std::env::temp_dir().join(format!("remux-ingest-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "permT");
    app.topology.send_replace(snapshot("permT"));
    remux::ingest::spawn(app.clone(), &dir).unwrap();

    // Over MAX_SUMMARY (2048), sent with truncated omitted → the daemon must
    // detect the cut itself and flag it.
    let huge = "b".repeat(2500);
    let dir2 = dir.clone();
    let client = tokio::task::spawn_blocking(move || {
        send(
            &dir2,
            serde_json::json!({
                "v": 1, "kind": "agent_permission", "pane": "%1",
                "source": "claude-code", "tool": "Bash", "summary": huge
            }),
        )
    });

    let snap = await_perms(&app, |s| s.len() == 1).await;
    let card = &snap[0];
    assert_eq!(card.summary.chars().count(), 2048, "capped to MAX_SUMMARY");
    assert!(card.truncated, "a hidden suffix must flag the card");

    // A remote Allow is refused server-side and the card stays open...
    assert_eq!(
        app.perms.resolve(&card.id, Decision::Allow, || true).err(),
        Some(remux::permit::ResolveError::Truncated)
    );
    assert_eq!(app.perms.snapshot().len(), 1, "left open for Deny/host");
    // ...but Deny still resolves it and wakes the blocked hook.
    app.perms
        .resolve(&card.id, Decision::Deny, || true)
        .unwrap();
    let v = client.await.unwrap();
    assert_eq!(v["decision"], "deny");
}

#[tokio::test]
async fn permission_card_dropped_when_the_hook_disconnects() {
    // The Mac-answered case: Claude SIGTERMs the hook, its socket closes; the
    // daemon must notice the EOF and drop the card so no late device decision
    // can resolve it. (See docs/spikes/M4.0-protocol.md.)
    let dir = std::env::temp_dir().join(format!("remux-ingest-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "perm2");
    app.topology.send_replace(snapshot("perm2"));
    remux::ingest::spawn(app.clone(), &dir).unwrap();

    let path = remux::ingest::socket_path(&dir);
    let (drop_tx, drop_rx) = std::sync::mpsc::channel::<()>();
    let hook = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut s = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let body = serde_json::json!({
            "v": 1, "kind": "agent_permission", "pane": "%1",
            "source": "claude-code", "tool": "Bash", "summary": "rm -rf /"
        });
        s.write_all(format!("{body}\n").as_bytes()).unwrap();
        // Hold the connection open until told to drop it (simulating the hook
        // being alive), then close without ever reading a decision.
        drop_rx.recv().unwrap();
        drop(s);
    });

    // Card is open while the hook is connected.
    let snap = await_perms(&app, |s| s.len() == 1).await;
    let id = snap[0].id.clone();

    // Hook dies (connection closes) → EOF → daemon drops the card.
    drop_tx.send(()).unwrap();
    hook.await.unwrap();
    await_perms(&app, |s| s.is_empty()).await;

    // A decision arriving after the drop resolves nothing.
    assert_eq!(
        app.perms.resolve(&id, Decision::Allow, || true).err(),
        Some(remux::permit::ResolveError::Unknown)
    );
}

#[tokio::test]
async fn permission_for_unknown_pane_opens_no_card() {
    let dir = std::env::temp_dir().join(format!("remux-ingest-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = test_app(&dir, "perm3");
    app.topology.send_replace(snapshot("perm3"));
    remux::ingest::spawn(app.clone(), &dir).unwrap();

    let dir2 = dir.clone();
    let v = tokio::task::spawn_blocking(move || {
        send(
            &dir2,
            serde_json::json!({
                "v": 1, "kind": "agent_permission", "pane": "%99",
                "source": "claude-code", "tool": "Bash", "summary": "x"
            }),
        )
    })
    .await
    .unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(app.perms.snapshot().len(), 0);
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
