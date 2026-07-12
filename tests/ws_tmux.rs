//! End-to-end integration test of the WebSocket <-> tmux path, on an isolated
//! tmux server (REMUX_TMUX_SOCKET). Skipped when tmux is not installed.

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

async fn collect_output_until(ws: &mut Ws, needle: &str) -> String {
    let mut acc = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for output containing {needle:?}; got so far: {acc:?}")
            })
            .expect("socket closed")
            .expect("socket error");
        if let WsMsg::Binary(b) = msg {
            acc.push_str(&String::from_utf8_lossy(&b));
            if acc.contains(needle) {
                return acc;
            }
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

#[tokio::test]
async fn full_terminal_flow_over_tmux() {
    if !common::tmux_available() {
        eprintln!("tmux not available; skipping");
        return;
    }
    let sock = format!("remux-it-{}", common::rand_suffix());
    std::env::set_var("REMUX_TMUX_SOCKET", &sock);
    // Fast attention thresholds so the busy→quiet heuristic is testable.
    // window_activity has whole-second resolution, hence >= 1s values.
    std::env::set_var("REMUX_ATTENTION_POLL_SECS", "0.2");
    std::env::set_var("REMUX_ATTENTION_MIN_BUSY_SECS", "1");
    std::env::set_var("REMUX_ATTENTION_QUIET_SECS", "2");
    let session = "itmain";
    let (addr, app) = common::start_server(session).await;

    let pairing = app.auth.new_pairing_token();
    let device_token = app.auth.pair(&pairing, "it-device").unwrap();
    let url = format!("ws://{addr}/ws");

    // --- Bad token is rejected before anything happens. ---
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(WsMsg::text(
        serde_json::json!({"type": "auth", "token": "bogus"}).to_string(),
    ))
    .await
    .unwrap();
    let err = next_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["code"], "auth_failed");
    drop(ws);

    // --- Real flow. ---
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
    assert_eq!(status["session"], session);
    // Window dims ride along for observer fit-width.
    assert!(status["window_cols"].is_u64(), "unexpected: {status}");

    // Observers cannot type.
    ws.send(WsMsg::binary(b"ls\r".to_vec())).await.unwrap();
    let err = next_json(&mut ws).await;
    assert_eq!(err["code"], "observer");

    // ...but observers CAN scroll: wheel reports are whitelisted and drive
    // tmux copy-mode.
    ws.send(WsMsg::binary(b"\x1b[<64;10;10M".repeat(5)))
        .await
        .unwrap();
    let mut in_mode = false;
    for _ in 0..50 {
        if tmux_sock(
            &sock,
            &["display-message", "-t", session, "-p", "#{pane_in_mode}"],
        ) == "1"
        {
            in_mode = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(in_mode, "observer wheel-up did not enter copy-mode");
    // Wheel back down to the bottom exits copy-mode (live view resumes).
    ws.send(WsMsg::binary(b"\x1b[<65;10;10M".repeat(30)))
        .await
        .unwrap();
    let mut out_of_mode = false;
    for _ in 0..50 {
        if tmux_sock(
            &sock,
            &["display-message", "-t", session, "-p", "#{pane_in_mode}"],
        ) == "0"
        {
            out_of_mode = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(out_of_mode, "observer wheel-down did not exit copy-mode");

    // Window actions are also gated on control.
    ws.send(WsMsg::text(
        serde_json::json!({"type": "window_action", "action": "new_window"}).to_string(),
    ))
    .await
    .unwrap();
    let err = next_json(&mut ws).await;
    assert_eq!(err["code"], "observer");

    // Take control.
    ws.send(WsMsg::text(
        serde_json::json!({"type": "take_control"}).to_string(),
    ))
    .await
    .unwrap();
    let status = next_json(&mut ws).await;
    assert_eq!(status["type"], "status", "unexpected: {status}");
    assert_eq!(status["state"], "controller");

    // Type a command; $((...)) ensures the marker only exists in *output*,
    // never in our typed input echo.
    ws.send(WsMsg::binary(b"echo remux$((1+1))marker\r".to_vec()))
        .await
        .unwrap();
    collect_output_until(&mut ws, "remux2marker").await;

    // Resize: as the latest active client we should drive the window size.
    ws.send(WsMsg::text(
        serde_json::json!({"type": "resize", "cols": 90, "rows": 28}).to_string(),
    ))
    .await
    .unwrap();
    let mut sized = false;
    for _ in 0..50 {
        let wh = tmux_sock(
            &sock,
            &[
                "list-windows",
                "-t",
                session,
                "-F",
                "#{window_width} #{window_height}",
            ],
        );
        // window height = client rows minus one for the tmux status line
        if wh == "90 27" {
            sized = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !sized {
        let dbg_clients = tmux_sock(
            &sock,
            &[
                "list-clients",
                "-F",
                "#{client_width}x#{client_height} #{client_flags}",
            ],
        );
        let dbg_win = tmux_sock(
            &sock,
            &[
                "list-windows",
                "-a",
                "-F",
                "#{session_name}:#{window_index} #{window_width}x#{window_height} #{window_size}",
            ],
        );
        panic!("window did not resize to 90x28; clients: {dbg_clients:?}; window: {dbg_win:?}");
    }

    // tmux should report exactly one attached client, in writable mode.
    let clients = tmux_sock(&sock, &["list-clients", "-F", "#{client_flags}"]);
    assert_eq!(clients.lines().count(), 1, "clients: {clients:?}");
    assert!(
        !clients.contains("read-only"),
        "controller should not be read-only: {clients:?}"
    );

    // --- Window actions: new window, split, select. ---
    ws.send(WsMsg::text(
        serde_json::json!({"type": "window_action", "action": "new_window"}).to_string(),
    ))
    .await
    .unwrap();
    let mut created = false;
    for _ in 0..50 {
        if tmux_sock(&sock, &["list-windows", "-t", session, "-F", "x"])
            .lines()
            .count()
            == 2
        {
            created = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(created, "new_window did not create a second window");

    ws.send(WsMsg::text(
        serde_json::json!({"type": "window_action", "action": "split_v"}).to_string(),
    ))
    .await
    .unwrap();
    let mut split = false;
    for _ in 0..50 {
        let panes = tmux_sock(
            &sock,
            &["display-message", "-t", session, "-p", "#{window_panes}"],
        );
        if panes == "2" {
            split = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(split, "split_v did not create a second pane");

    ws.send(WsMsg::text(
        serde_json::json!({"type": "window_action", "action": "select_window", "index": 0})
            .to_string(),
    ))
    .await
    .unwrap();
    let mut selected = false;
    for _ in 0..50 {
        let idx = tmux_sock(
            &sock,
            &["display-message", "-t", session, "-p", "#{window_index}"],
        );
        if idx == "0" {
            selected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(selected, "select_window did not switch back to window 0");

    // --- Session picker: an auth carrying a session name attaches (and
    // creates) that session; invalid names are rejected before any tmux call. ---
    let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws2.send(WsMsg::text(
        serde_json::json!({"type": "auth", "token": device_token, "session": "it-alt"}).to_string(),
    ))
    .await
    .unwrap();
    let status = next_json(&mut ws2).await;
    assert_eq!(status["type"], "status", "unexpected: {status}");
    assert_eq!(status["session"], "it-alt");
    let sessions = tmux_sock(&sock, &["list-sessions", "-F", "#{session_name}"]);
    assert!(sessions.contains("it-alt"), "sessions: {sessions:?}");
    ws2.close(None).await.unwrap();
    drop(ws2);

    // The sessions API sees the same tmux server and parses real output.
    let http = reqwest::Client::new();
    let resp = http
        .get(format!("http://{addr}/api/sessions"))
        .header("Authorization", format!("Bearer {device_token}"))
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = resp.json().await.unwrap();
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"itmain") && names.contains(&"it-alt"),
        "sessions: {names:?}"
    );

    let (mut ws3, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws3.send(WsMsg::text(
        serde_json::json!({"type": "auth", "token": device_token, "session": "bad:name"})
            .to_string(),
    ))
    .await
    .unwrap();
    let err = next_json(&mut ws3).await;
    assert_eq!(err["code"], "invalid_session", "unexpected: {err}");
    drop(ws3);

    // --- Attention: a busy period (>= 1s of output) followed by quiet must
    // produce an attention frame on the websocket. ---
    ws.send(WsMsg::binary(
        b"for i in 1 2 3 4; do echo busy$i; sleep 0.5; done\r".to_vec(),
    ))
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("timed out waiting for attention frame")
            .expect("socket closed")
            .expect("socket error");
        if let WsMsg::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "attention" {
                break;
            }
        }
    }

    // Disconnect: the attach client must disappear (this is what gives the
    // Mac its dimensions back instantly).
    ws.close(None).await.unwrap();
    drop(ws);
    let mut gone = false;
    for _ in 0..50 {
        let clients = tmux_sock(&sock, &["list-clients", "-F", "#{client_name}"]);
        if clients.is_empty() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gone, "tmux client still attached after disconnect");

    // Session itself must survive the disconnect (persistence!).
    let sessions = tmux_sock(&sock, &["list-sessions", "-F", "#{session_name}"]);
    assert!(sessions.contains(session));

    tmux_sock(&sock, &["kill-server"]);
}
