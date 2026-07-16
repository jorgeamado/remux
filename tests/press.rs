//! Integration test of the one-shot press path (`terminal_press`) over a real
//! tmux server. Own binary (not ws_tmux.rs) because REMUX_TMUX_SOCKET is
//! process-global and parallel tests in one binary would race on it.

mod common;

use futures_util::{SinkExt, StreamExt};
use std::process::Command;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMsg;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(ws: &mut Ws) -> serde_json::Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("timed out waiting for JSON frame")
            .expect("socket closed")
            .expect("socket error");
        if let WsMsg::Text(t) = msg {
            return serde_json::from_str(&t).expect("invalid JSON from server");
        }
    }
}

fn tmux_sock(sock: &str, args: &[&str]) -> String {
    let out = Command::new("tmux")
        .arg("-L")
        .arg(sock)
        .args(args)
        .output()
        .expect("tmux");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn press(ws: &mut Ws, id: &str, cols: u16, rows: u16, col: u16, row: u16) {
    ws.send(WsMsg::text(
        serde_json::json!({
            "type": "terminal_press", "request_id": id,
            "cols": cols, "rows": rows, "col": col, "row": row,
        })
        .to_string(),
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn terminal_press_flow_over_tmux() {
    if !common::tmux_available() {
        eprintln!("tmux not available; skipping");
        return;
    }
    let sock = format!("remux-it-{}", common::rand_suffix());
    std::env::set_var("REMUX_TMUX_SOCKET", &sock);
    let session = "itpress";
    let (addr, app) = common::start_server(session).await;

    let pairing = app.auth.new_pairing_token();
    let device_token = app.auth.pair(&pairing, "it-press").unwrap();
    let url = format!("ws://{addr}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(WsMsg::text(
        serde_json::json!({"type": "auth", "token": device_token, "cols": 100, "rows": 30})
            .to_string(),
    ))
    .await
    .unwrap();
    let status = next_json(&mut ws).await;
    assert_eq!(status["type"], "status", "unexpected: {status}");
    assert_eq!(status["state"], "observer");

    // Grid echo mismatch → stale: the tap aimed at a grid this connection no
    // longer renders, so the coordinates mean nothing.
    press(&mut ws, "p1", 80, 24, 1, 1).await;
    let r = next_json(&mut ws).await;
    assert_eq!(r["type"], "terminal_press_result", "unexpected: {r}");
    assert_eq!(r["request_id"], "p1");
    assert_eq!(r["status"], "stale");

    // A second press within the min interval is rate-capped before any
    // validation or tmux work.
    press(&mut ws, "p2", 100, 30, 1, 1).await;
    let r = next_json(&mut ws).await;
    assert_eq!(r["request_id"], "p2");
    assert_eq!(r["status"], "rate_limited");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The pane runs a plain shell — no app requested mouse reporting, so the
    // press is rejected, not silently degraded to tmux's own pane bindings.
    press(&mut ws, "p3", 100, 30, 1, 1).await;
    let r = next_json(&mut ws).await;
    assert_eq!(r["request_id"], "p3");
    assert_eq!(r["status"], "mouse_off");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Start a mouse-reporting app: printf enables SGR mouse mode (1000h sets
    // #{mouse_any_flag}, 1006h selects the SGR encoding), cat -v echoes stdin
    // with control chars made printable so the capture can prove delivery.
    tmux_sock(
        &sock,
        &[
            "send-keys",
            "-t",
            session,
            "printf '\\033[?1000h\\033[?1006h'; cat -v",
            "Enter",
        ],
    );
    let mut mouse_on = false;
    for _ in 0..50 {
        if tmux_sock(
            &sock,
            &["display-message", "-t", session, "-p", "#{mouse_any_flag}"],
        ) == "1"
        {
            mouse_on = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(mouse_on, "pane never enabled mouse reporting");

    // Now the press is delivered: the daemon synthesizes the click into this
    // connection's own PTY, tmux routes it to the pane, and the app receives
    // it — all while this connection stays a plain observer.
    press(&mut ws, "p4", 100, 30, 5, 5).await;
    let r = next_json(&mut ws).await;
    assert_eq!(r["request_id"], "p4");
    assert_eq!(r["status"], "delivered", "unexpected: {r}");

    // cat -v renders the re-encoded SGR click tmux hands the app.
    let mut echoed = false;
    for _ in 0..50 {
        let cap = tmux_sock(&sock, &["capture-pane", "-p", "-t", session]);
        if cap.contains("[<0;5;5M") {
            echoed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(echoed, "click never reached the pane's app");

    // The whole flow happened as observer — pressing must never promote.
    let flags = tmux_sock(&sock, &["list-clients", "-F", "#{client_flags}"]);
    assert!(
        flags.contains("ignore-size"),
        "press flipped client flags: {flags}"
    );

    tmux_sock(&sock, &["kill-server"]);
}
