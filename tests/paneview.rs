//! The pane-view stream socket: a source claims a pane (verified against
//! topology), streams validated snapshots, and the registry keeps the latest.
//! Cleanup on EOF and on the pane leaving the topology.

mod common;

use clap::Parser;
use remux::{auth::Auth, tmux, App, Args};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

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
        chat: Default::default(),
        pane_views: Default::default(),
        dash_windows: Default::default(),
        feed: Default::default(),
        detector_reset: tokio::sync::broadcast::channel(16).0,
    })
}

/// A topology snapshot: one session holding the given pane ids.
fn snapshot(session: &str, panes: &[&str]) -> remux::topology::Snapshot {
    Arc::new(vec![tmux::SessionWindows {
        name: session.into(),
        attached: false,
        windows: vec![tmux::WindowInfo {
            index: 0,
            active: true,
            zoomed: false,
            name: "zsh".into(),
            panes: panes
                .iter()
                .enumerate()
                .map(|(i, id)| tmux::PaneInfo {
                    id: (*id).into(),
                    index: i as u32,
                    active: i == 0,
                    command: "zsh".into(),
                })
                .collect(),
        }],
    }])
}

async fn eventually<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not met in time: {what}");
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("remux-pv-{tag}-{}", common::rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const SNAP: &str =
    r#"{"t":1,"workers":[{"name":"api","status":"running","cpu":12,"mem":120,"progress":40}]}"#;

async fn connect(dir: &std::path::Path) -> tokio::net::UnixStream {
    let path = dir.join(remux::paneview::SOCKET);
    tokio::net::UnixStream::connect(&path).await.unwrap()
}

#[tokio::test]
async fn stream_populates_registry_and_cleans_up_on_eof() {
    let dir = scratch("eof");
    let app = test_app(&dir, "pv1");
    app.topology.send_replace(snapshot("pv1", &["%1"]));
    remux::paneview::spawn(app.clone(), &dir).unwrap();

    let sock = connect(&dir).await;
    let (r, mut w) = sock.into_split();
    let mut lines = tokio::io::BufReader::new(r);

    w.write_all(b"{\"pane\":\"%1\",\"view\":\"taskscope.v1\"}\n")
        .await
        .unwrap();
    let mut ack = String::new();
    lines.read_line(&mut ack).await.unwrap();
    assert!(ack.contains("\"ok\":true"), "ack was: {ack}");

    w.write_all(format!("{SNAP}\n").as_bytes()).await.unwrap();
    eventually("view appears", || app.pane_views.snapshot().len() == 1).await;

    let snap = app.pane_views.snapshot();
    assert_eq!(snap[0].pane, "%1");
    assert_eq!(snap[0].view, "taskscope.v1");
    assert_eq!(snap[0].state["workers"][0]["name"], "api");

    // Closing the source connection drops the view.
    drop(w);
    drop(lines);
    eventually("view removed on EOF", || {
        app.pane_views.snapshot().is_empty()
    })
    .await;
}

#[tokio::test]
async fn unknown_pane_and_unknown_view_are_rejected() {
    let dir = scratch("reject");
    let app = test_app(&dir, "pv2");
    app.topology.send_replace(snapshot("pv2", &["%1"]));
    remux::paneview::spawn(app.clone(), &dir).unwrap();

    // A pane not in the topology.
    let sock = connect(&dir).await;
    let (r, mut w) = sock.into_split();
    let mut lines = tokio::io::BufReader::new(r);
    w.write_all(b"{\"pane\":\"%99\",\"view\":\"taskscope.v1\"}\n")
        .await
        .unwrap();
    let mut ack = String::new();
    lines.read_line(&mut ack).await.unwrap();
    assert!(ack.contains("\"ok\":false"), "ack was: {ack}");
    assert!(app.pane_views.snapshot().is_empty());

    // A pane that exists, but an unknown view id.
    let sock = connect(&dir).await;
    let (r, mut w) = sock.into_split();
    let mut lines = tokio::io::BufReader::new(r);
    w.write_all(b"{\"pane\":\"%1\",\"view\":\"bogus.v1\"}\n")
        .await
        .unwrap();
    let mut ack = String::new();
    lines.read_line(&mut ack).await.unwrap();
    assert!(ack.contains("\"ok\":false"), "ack was: {ack}");
}

#[tokio::test]
async fn a_pane_leaving_the_topology_prunes_its_view() {
    let dir = scratch("gc");
    let app = test_app(&dir, "pv3");
    app.topology.send_replace(snapshot("pv3", &["%1"]));
    remux::paneview::spawn(app.clone(), &dir).unwrap();

    let sock = connect(&dir).await;
    let (r, mut w) = sock.into_split();
    let mut lines = tokio::io::BufReader::new(r);
    w.write_all(b"{\"pane\":\"%1\",\"view\":\"taskscope.v1\"}\n")
        .await
        .unwrap();
    let mut ack = String::new();
    lines.read_line(&mut ack).await.unwrap();
    w.write_all(format!("{SNAP}\n").as_bytes()).await.unwrap();
    eventually("view appears", || app.pane_views.snapshot().len() == 1).await;

    // The pane vanishes from topology (window closed) — GC drops the view even
    // though the source socket is still open.
    app.topology.send_replace(snapshot("pv3", &[]));
    eventually("view pruned by topology GC", || {
        app.pane_views.snapshot().is_empty()
    })
    .await;

    // Keep the source alive until here so the removal is GC's doing, not EOF's.
    drop(w);
    drop(lines);
}
