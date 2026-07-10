use crate::{tmux, App};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
/// Bytes buffered towards a slow client before the PTY reader blocks.
/// A blocked reader simply pauses the tmux client; tmux repaints when we resume.
const OUT_QUEUE: usize = 256;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Auth {
        token: String,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    TakeControl,
    ReleaseControl,
    Ping,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg<'a> {
    Status {
        state: &'a str,
        session: &'a str,
        device: &'a str,
    },
    Error {
        code: &'a str,
        message: &'a str,
    },
    Pong,
}

fn json(msg: &ServerMsg) -> Message {
    Message::Text(serde_json::to_string(msg).unwrap().into())
}

pub async fn handler(State(app): State<Arc<App>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| async move {
        if let Err(e) = handle(socket, app).await {
            tracing::debug!("ws session ended: {e:#}");
        }
    })
}

async fn handle(socket: WebSocket, app: Arc<App>) -> anyhow::Result<()> {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // ---- Auth: nothing happens before a valid token arrives. ----
    let (device, mut cols, mut rows) = match tokio::time::timeout(AUTH_TIMEOUT, ws_rx.next()).await
    {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str(&text) {
            Ok(ClientMsg::Auth { token, cols, rows }) => match app.auth.authenticate(&token) {
                Some(device) => (device, cols.unwrap_or(80), rows.unwrap_or(24)),
                None => {
                    let _ = ws_tx
                        .send(json(&ServerMsg::Error {
                            code: "auth_failed",
                            message: "invalid device token — re-pair this device",
                        }))
                        .await;
                    let _ = ws_tx.send(Message::Close(None)).await;
                    return Ok(());
                }
            },
            _ => {
                let _ = ws_tx
                    .send(json(&ServerMsg::Error {
                        code: "auth_required",
                        message: "first message must be auth",
                    }))
                    .await;
                let _ = ws_tx.send(Message::Close(None)).await;
                return Ok(());
            }
        },
        _ => return Ok(()), // timeout, close or protocol error
    };
    tracing::info!(%device, "client authenticated");

    // ---- Spawn the per-connection tmux client inside a PTY. ----
    let session = app.args.session.clone();
    tokio::task::spawn_blocking({
        let session = session.clone();
        move || tmux::ensure_session(&session)
    })
    .await??;

    clamp_size(&mut cols, &mut rows);
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new("tmux");
    cmd.args(tmux::attach_args(&session));
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "C.UTF-8");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let master = pair.master;
    let mut pty_reader = master.try_clone_reader()?;
    let mut pty_writer = master.take_writer()?;
    let child_pid = child.process_id();

    // Outgoing frames (terminal bytes + control JSON) towards the websocket.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(OUT_QUEUE);
    // Terminal input towards the PTY.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    // PTY -> ws pump (blocking thread; backpressure = blocked send).
    let pty_out = out_tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if pty_out
                        .blocking_send(Message::Binary(buf[..n].to_vec().into()))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = pty_out.blocking_send(Message::Close(None));
    });

    // ws -> PTY input pump (blocking thread).
    std::thread::spawn(move || {
        while let Some(bytes) = in_rx.blocking_recv() {
            if pty_writer.write_all(&bytes).is_err() || pty_writer.flush().is_err() {
                break;
            }
        }
    });

    // Merge outgoing frames onto the websocket.
    let sender = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let closing = matches!(msg, Message::Close(_));
            if ws_tx.send(msg).await.is_err() || closing {
                break;
            }
        }
    });

    // Resolve our tmux client name (needed to toggle observer/controller flags).
    let client_name = resolve_client_name(child_pid).await;
    if client_name.is_none() {
        tracing::warn!("could not resolve tmux client name; take_control disabled");
    }

    let mut controller = false;
    let status = |state: &str| {
        json(&ServerMsg::Status {
            state,
            session: &session,
            device: &device,
        })
    };
    let _ = out_tx.send(status("observer")).await;

    // ---- Main receive loop. ----
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(bytes) => {
                if controller {
                    if in_tx.send(bytes.to_vec()).await.is_err() {
                        break;
                    }
                } else {
                    let _ = out_tx
                        .send(json(&ServerMsg::Error {
                            code: "observer",
                            message: "take control before typing",
                        }))
                        .await;
                }
            }
            Message::Text(text) => match serde_json::from_str::<ClientMsg>(&text) {
                Ok(ClientMsg::Resize {
                    cols: mut c,
                    rows: mut r,
                }) => {
                    clamp_size(&mut c, &mut r);
                    (cols, rows) = (c, r);
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                Ok(ClientMsg::TakeControl) => match &client_name {
                    Some(name) => {
                        let name = name.clone();
                        let res =
                            tokio::task::spawn_blocking(move || tmux::promote_client(&name)).await?;
                        match res {
                            Ok(()) => {
                                controller = true;
                                // Nudge the size so tmux re-evaluates layout now that
                                // this client participates in sizing.
                                let _ = master.resize(PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                                let _ = out_tx.send(status("controller")).await;
                            }
                            Err(e) => {
                                tracing::warn!("promote failed: {e:#}");
                                let _ = out_tx
                                    .send(json(&ServerMsg::Error {
                                        code: "take_control_failed",
                                        message: "could not take control",
                                    }))
                                    .await;
                            }
                        }
                    }
                    None => {
                        let _ = out_tx
                            .send(json(&ServerMsg::Error {
                                code: "take_control_failed",
                                message: "tmux client not resolved",
                            }))
                            .await;
                    }
                },
                Ok(ClientMsg::ReleaseControl) => {
                    if let Some(name) = &client_name {
                        let name = name.clone();
                        let _ =
                            tokio::task::spawn_blocking(move || tmux::demote_client(&name)).await?;
                    }
                    controller = false;
                    let _ = out_tx.send(status("observer")).await;
                }
                Ok(ClientMsg::Ping) => {
                    let _ = out_tx.send(json(&ServerMsg::Pong)).await;
                }
                Ok(ClientMsg::Auth { .. }) => {} // already authed; ignore
                Err(e) => {
                    tracing::debug!("bad client message: {e}");
                }
            },
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Connection gone: kill the attach client. tmux drops it from sizing and the
    // remaining clients (e.g. the Mac) immediately get their dimensions back.
    let _ = child.kill();
    sender.abort();
    tracing::info!(%device, "client disconnected");
    Ok(())
}

fn clamp_size(cols: &mut u16, rows: &mut u16) {
    *cols = (*cols).clamp(20, 500);
    *rows = (*rows).clamp(5, 300);
}

async fn resolve_client_name(pid: Option<u32>) -> Option<String> {
    let pid = pid?;
    for _ in 0..20 {
        let found = tokio::task::spawn_blocking(move || tmux::client_name_for_pid(pid))
            .await
            .ok()?
            .ok()
            .flatten();
        if found.is_some() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}
