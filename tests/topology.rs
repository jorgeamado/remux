//! M3a: the control-mode topology client publishes session/window structure
//! to websocket clients, and structural changes propagate.

mod common;

use futures_util::{SinkExt, StreamExt};
use std::process::Command;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMsg;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn tmux_sock(sock: &str, args: &[&str]) {
    Command::new("tmux")
        .arg("-L")
        .arg(sock)
        .args(args)
        .output()
        .expect("tmux");
}

/// Wait for a topology frame where `session` has `want_windows` windows.
async fn wait_topology(ws: &mut Ws, session: &str, want_windows: usize) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for topology {session} x{want_windows}"))
            .expect("closed")
            .expect("error");
        if let WsMsg::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "topology" {
                if let Some(s) = v["sessions"]
                    .as_array()
                    .and_then(|a| a.iter().find(|s| s["name"] == session))
                {
                    if s["windows"].as_array().map(|w| w.len()) == Some(want_windows) {
                        return v;
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn topology_reflects_window_changes() {
    if !common::tmux_available() {
        eprintln!("tmux not available; skipping");
        return;
    }
    let sock = format!("remux-topo-{}", common::rand_suffix());
    std::env::set_var("REMUX_TMUX_SOCKET", &sock);
    let session = "topomain";
    let (addr, app) = common::start_server(session).await;
    remux::topology::spawn(app.clone());

    let pairing = app.auth.new_pairing_token();
    let token = app.auth.pair(&pairing, "topo").unwrap();
    let url = format!("ws://{addr}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(WsMsg::text(
        serde_json::json!({"type": "auth", "token": token, "session": session}).to_string(),
    ))
    .await
    .unwrap();

    // Initial snapshot: the session exists with one window.
    wait_topology(&mut ws, session, 1).await;

    // A window created out-of-band (as if from the Mac) propagates.
    tmux_sock(&sock, &["new-window", "-t", &format!("={session}")]);
    let topo = wait_topology(&mut ws, session, 2).await;
    let sess = topo["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == session)
        .unwrap();
    assert_eq!(sess["attached"], true);
    assert!(sess["windows"][0]["index"].is_u64());

    // The terminal byte path is unaffected by topology: take control, run a
    // command, see its output.
    ws.send(WsMsg::text(
        serde_json::json!({"type": "take_control"}).to_string(),
    ))
    .await
    .unwrap();
    ws.send(WsMsg::binary(b"echo topo$((3+4))ok\r".to_vec()))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(WsMsg::Binary(b)))) = tokio::time::timeout_at(deadline, ws.next()).await {
            if String::from_utf8_lossy(&b).contains("topo7ok") {
                saw = true;
                break;
            }
        }
    }
    assert!(saw, "terminal output not received while topology active");

    ws.close(None).await.unwrap();
    tmux_sock(&sock, &["kill-server"]);
}
