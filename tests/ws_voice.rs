//! Protocol test for voice dictation (docs/voice.md). The default test build
//! has no `voice` feature and no model installed, so this exercises the
//! "voice off" contract: status advertises voice:false, voice_start is
//! answered with voice_unavailable, and stray chunks are ignored (never
//! buffered, never answered). Skipped when tmux is not installed.

mod common;

use futures_util::{SinkExt, StreamExt};
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

#[tokio::test]
async fn voice_off_contract() {
    if !common::tmux_available() {
        eprintln!("tmux not available; skipping");
        return;
    }
    let sock = format!("remux-it-{}", common::rand_suffix());
    std::env::set_var("REMUX_TMUX_SOCKET", &sock);
    let session = "itvoice";
    let (addr, app) = common::start_server(session).await;
    assert!(!app.voice.available(), "test build must not offer voice");

    let pairing = app.auth.new_pairing_token();
    let device_token = app.auth.pair(&pairing, "it-device").unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    ws.send(WsMsg::text(
        serde_json::json!({"type": "auth", "token": device_token}).to_string(),
    ))
    .await
    .unwrap();

    // Status must tell the client NOT to show a mic button.
    let status = next_json(&mut ws).await;
    assert_eq!(status["type"], "status", "unexpected: {status}");
    assert_eq!(status["voice"], false);

    // Chunks and end outside an utterance are ignored — no reply, no buffer.
    ws.send(WsMsg::text(
        serde_json::json!({"type": "voice_chunk", "data": "AAAA"}).to_string(),
    ))
    .await
    .unwrap();
    ws.send(WsMsg::text(
        serde_json::json!({"type": "voice_end"}).to_string(),
    ))
    .await
    .unwrap();

    // voice_start on a voice-less host is answered with voice_unavailable
    // (and must be the NEXT json frame — the ignored messages above produced
    // nothing).
    ws.send(WsMsg::text(
        serde_json::json!({"type": "voice_start"}).to_string(),
    ))
    .await
    .unwrap();
    let err = next_json(&mut ws).await;
    assert_eq!(err["type"], "voice_error", "unexpected: {err}");
    assert_eq!(err["code"], "voice_unavailable");

    // The connection is still healthy afterwards.
    ws.send(WsMsg::text(serde_json::json!({"type": "ping"}).to_string()))
        .await
        .unwrap();
    let pong = next_json(&mut ws).await;
    assert_eq!(pong["type"], "pong");

    let _ = std::process::Command::new("tmux")
        .args(["-L", &sock, "kill-server"])
        .output();
}
