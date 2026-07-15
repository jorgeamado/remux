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

const AUTH_TIMEOUT: Duration = Duration::from_secs(8);
/// Bytes buffered towards a slow client before the PTY reader blocks.
/// A blocked reader simply pauses the tmux client; tmux repaints when we resume.
const OUT_QUEUE: usize = 256;
/// Each connection spawns a PTY + tmux client + threads; cap concurrency so a
/// stolen token (or a buggy client) cannot exhaust the host's processes/FDs.
const MAX_TOTAL_CONNECTIONS: usize = 64;
const MAX_PER_DEVICE_CONNECTIONS: usize = 8;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Auth {
        token: String,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        /// tmux session to attach (created if missing); server default if absent.
        #[serde(default)]
        session: Option<String>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    TakeControl,
    ReleaseControl,
    /// Window/pane operations (new window, splits, switching) — controller only.
    WindowAction {
        action: String,
        #[serde(default)]
        index: Option<u32>,
    },
    /// Complete the composer draft in the shell (Tab) and echo the completed
    /// line back — controller only. `text` is the full desired command line;
    /// `synced` is what a previous tab-complete already left in the shell's
    /// input buffer.
    TabComplete {
        text: String,
        #[serde(default)]
        synced: String,
    },
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
    /// Someone/something in this session wants the user: a hook-fed event
    /// (agent_needs_input) or the busy→quiet heuristic.
    Attention {
        kind: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<&'a str>,
    },
    /// The shell's command line after a TabComplete, prompt stripped — the
    /// client mirrors it into the input field.
    TabCompleted {
        text: &'a str,
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
    let (device, mut cols, mut rows, requested_session) =
        match tokio::time::timeout(AUTH_TIMEOUT, ws_rx.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str(&text) {
                Ok(ClientMsg::Auth {
                    token,
                    cols,
                    rows,
                    session,
                }) => match app.auth.authenticate(&token) {
                    Some(device) => (device, cols.unwrap_or(80), rows.unwrap_or(24), session),
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
    tracing::info!(device = %device.name, "client authenticated");

    // ---- Resolve the target session (picker), then spawn the tmux client. ----
    let session = match requested_session {
        Some(name) if !tmux::valid_session_name(&name) => {
            let _ = ws_tx
                .send(json(&ServerMsg::Error {
                    code: "invalid_session",
                    message: "invalid session name",
                }))
                .await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return Ok(());
        }
        Some(name) => name,
        None => app.args.session.clone(),
    };

    // Connection caps FIRST — before ensure_session, so a capped client can't
    // create new detached tmux sessions (and their shells) by requesting
    // unique names. Decide under the lock, then release it before any await.
    let conn_key = (device.id.clone(), session.clone());
    let admitted = {
        let mut conns = app.connections.lock().unwrap();
        let total: usize = conns.values().sum();
        let per_device: usize = conns
            .iter()
            .filter(|((id, _), _)| id == &device.id)
            .map(|(_, n)| *n)
            .sum();
        if total >= MAX_TOTAL_CONNECTIONS || per_device >= MAX_PER_DEVICE_CONNECTIONS {
            false
        } else {
            *conns.entry(conn_key.clone()).or_insert(0) += 1;
            true
        }
    };
    if !admitted {
        let _ = ws_tx
            .send(json(&ServerMsg::Error {
                code: "too_many_connections",
                message: "connection limit reached",
            }))
            .await;
        let _ = ws_tx.send(Message::Close(None)).await;
        return Ok(());
    }
    let _conn_guard = ConnGuard {
        app: app.clone(),
        key: conn_key,
    };

    // Now that this connection is admitted under the cap, create/attach the
    // session and record last-seen.
    tokio::task::spawn_blocking({
        let session = session.clone();
        move || tmux::ensure_session(&session)
    })
    .await??;

    // Attaching resets this session's push throttle so a genuinely new
    // busy→quiet notifies promptly rather than being suppressed by a lingering
    // throttle. The pending-attention marker is deliberately NOT cleared here:
    // it is the deep-link hint another device's notification tap resolves via
    // /api/attention, and it self-expires. The client already ignores the
    // session it is currently viewing.
    app.push.clear_session_throttle(&session);
    tokio::task::spawn_blocking({
        let app = app.clone();
        let id = device.id.clone();
        move || app.auth.touch(&id)
    });

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

    // Forward tmux topology (sessions → windows) to this client: the current
    // snapshot now, then every update. Metadata only — never in the byte path.
    let topology_task = tokio::spawn({
        let mut rx = app.topology.subscribe();
        let out = out_tx.clone();
        async move {
            loop {
                let snap = rx.borrow_and_update().clone();
                if !snap.is_empty() {
                    let msg = Message::Text(
                        serde_json::json!({ "type": "topology", "sessions": *snap })
                            .to_string()
                            .into(),
                    );
                    if out.send(msg).await.is_err() {
                        break;
                    }
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }
    });

    // Close this socket when its device is revoked (management cascade).
    let revoked_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let revoke_task = tokio::spawn({
        let mut rx = app.revoked.subscribe();
        let out = out_tx.clone();
        let my_id = device.id.clone();
        let flag = revoked_flag.clone();
        let app = app.clone();
        async move {
            loop {
                let mine = match rx.recv().await {
                    Ok(id) => id == my_id,
                    // A lagged receiver may have skipped its own revocation:
                    // fail safe by re-checking the device registry.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        !app.auth.devices().iter().any(|d| d.id == my_id)
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if mine {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = out
                        .send(json(&ServerMsg::Error {
                            code: "revoked",
                            message: "this device was revoked",
                        }))
                        .await;
                    let _ = out.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    });

    // Fan attention events for *this connection's session* into the socket.
    let attention_task = tokio::spawn({
        let mut rx = app.attention.subscribe();
        let out = out_tx.clone();
        let session = session.clone();
        async move {
            loop {
                match rx.recv().await {
                    Ok(att) => {
                        if att.session == session
                            && out
                                .send(json(&ServerMsg::Attention {
                                    kind: &att.kind,
                                    reason: att.reason.as_deref(),
                                    source: att.source.as_deref(),
                                }))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    // Push open permission cards (M4b) to approve-capable devices, and keep
    // them reconciled. Broadcast is a hint only: on every change (and once at
    // start, and on lag) we re-read the registry snapshot — a lagged receiver
    // can't miss state. Capability is re-checked by id each time, so a
    // grant/revoke takes effect on this live socket. Non-approve devices get an
    // empty list (details are privileged), never the cards.
    let permits_task = tokio::spawn({
        let mut rx = app.perms.subscribe();
        let out = out_tx.clone();
        let app = app.clone();
        let device_id = device.id.clone();
        async move {
            // Skip empty frames until we've actually sent cards, so a fresh
            // connection with no open cards stays quiet (like topology) — but
            // once a set was sent, an empty frame is meaningful: it clears the
            // resolved/expired cards from the UI.
            let mut sent_nonempty = false;
            loop {
                let cards: Vec<serde_json::Value> = if app.auth.can_approve(&device_id) {
                    app.perms.snapshot().iter().map(|c| c.view()).collect()
                } else {
                    Vec::new()
                };
                if !cards.is_empty() || sent_nonempty {
                    sent_nonempty = !cards.is_empty();
                    let msg = Message::Text(
                        serde_json::json!({ "type": "permission_cards", "cards": cards })
                            .to_string()
                            .into(),
                    );
                    if out.send(msg).await.is_err() {
                        break;
                    }
                }
                match rx.recv().await {
                    Ok(()) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    // Push this session's shell command feed (M4c) and keep it reconciled.
    // Session-filtered (a feed snapshot is far larger than a permission frame,
    // and it shares the PTY-byte outgoing queue — so it must not carry other
    // sessions' commands) and debounced: a fast command loop fires many hints,
    // but the client only needs the latest snapshot.
    let feed_task = tokio::spawn({
        let mut rx = app.feed.subscribe();
        let out = out_tx.clone();
        let app = app.clone();
        let session = session.clone();
        async move {
            // The change hint is global (not per-session), so activity in any
            // session wakes this task. Only actually send when *this* session's
            // rendered snapshot changed — otherwise session B would resend its
            // unchanged (up to 200-entry) feed every time session A ran a
            // command, competing with PTY bytes on the shared outgoing queue.
            let mut last_sent: Option<String> = None;
            loop {
                let commands = app.feed.snapshot(&session);
                let is_empty = commands.is_empty();
                let json =
                    serde_json::json!({ "type": "command_feed", "commands": commands }).to_string();
                // Send when it changed, and either non-empty or a clear of a
                // previously-sent set (a fresh empty connection stays quiet so
                // it can't race the status frame).
                if last_sent.as_deref() != Some(json.as_str()) && (!is_empty || last_sent.is_some())
                {
                    if out.send(Message::Text(json.clone().into())).await.is_err() {
                        break;
                    }
                    last_sent = Some(json);
                }
                // Block for the next change, then coalesce a burst: absorb
                // further hints for a short window and send one fresh snapshot.
                match rx.recv().await {
                    Ok(()) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                // Drain buffered hints so the next recv() blocks until a genuine
                // change. Bounded: on lag, stop — the fresh snapshot above
                // reconciles regardless, so exhaustive draining is unnecessary
                // (and a Lagged-continue loop could spin).
                while matches!(rx.try_recv(), Ok(())) {}
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
            device: &device.name,
        })
    };
    let _ = out_tx.send(status("observer")).await;

    // ---- Main receive loop. ----
    while let Some(Ok(msg)) = ws_rx.next().await {
        // Revocation gate for EVERY message type (input, resize, take_control,
        // window_action): synchronous is_active() closes the window between
        // revoke() committing and the async broadcast setting revoked_flag.
        if revoked_flag.load(std::sync::atomic::Ordering::Relaxed)
            || !app.auth.is_active(&device.id)
        {
            break;
        }
        match msg {
            Message::Binary(bytes) => {
                // Observers may scroll (mouse-wheel reports drive tmux
                // copy-mode and cannot type or execute anything); all other
                // input still requires control.
                if controller || wheel_reports_only(&bytes) {
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
                        let res = tokio::task::spawn_blocking(move || tmux::promote_client(&name))
                            .await?;
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
                Ok(ClientMsg::WindowAction { action, index }) => {
                    if !controller {
                        let _ = out_tx
                            .send(json(&ServerMsg::Error {
                                code: "observer",
                                message: "take control before changing windows",
                            }))
                            .await;
                    } else {
                        let session = session.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            tmux::window_action(&session, &action, index)
                        })
                        .await?;
                        if let Err(e) = res {
                            tracing::warn!("window action failed: {e:#}");
                            let _ = out_tx
                                .send(json(&ServerMsg::Error {
                                    code: "window_action_failed",
                                    message: "could not perform window action",
                                }))
                                .await;
                        }
                    }
                }
                Ok(ClientMsg::TabComplete { text, synced }) => {
                    if !controller {
                        let _ = out_tx
                            .send(json(&ServerMsg::Error {
                                code: "observer",
                                message: "take control before completing",
                            }))
                            .await;
                    } else if text.len() > 512
                        || synced.len() > 512
                        || text.chars().any(|c| c.is_control())
                        || synced.chars().any(|c| c.is_control())
                    {
                        let _ = out_tx
                            .send(json(&ServerMsg::Error {
                                code: "tab_complete_failed",
                                message: "draft too long or invalid",
                            }))
                            .await;
                    } else {
                        // Awaiting here serialises the round-trip against other
                        // input — correct (typing mid-completion would corrupt
                        // the readback) and short (≤ ~700ms; the user is
                        // waiting on the completion anyway).
                        let session = session.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            tmux::tab_complete(&session, &text, &synced)
                        })
                        .await?;
                        match res {
                            Ok(completed) => {
                                let _ = out_tx
                                    .send(json(&ServerMsg::TabCompleted { text: &completed }))
                                    .await;
                            }
                            Err(e) => {
                                tracing::warn!("tab complete failed: {e:#}");
                                let _ = out_tx
                                    .send(json(&ServerMsg::Error {
                                        code: "tab_complete_failed",
                                        message: "could not complete in the shell",
                                    }))
                                    .await;
                            }
                        }
                    }
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
    attention_task.abort();
    permits_task.abort();
    feed_task.abort();
    revoke_task.abort();
    topology_task.abort();
    sender.abort();
    tracing::info!(device = %device.name, "client disconnected");
    Ok(())
}

/// Decrements the live-connection registry on any exit path.
struct ConnGuard {
    app: Arc<App>,
    key: (String, String),
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut conns = self.app.connections.lock().unwrap();
        if let Some(n) = conns.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                conns.remove(&self.key);
            }
        }
    }
}

fn clamp_size(cols: &mut u16, rows: &mut u16) {
    *cols = (*cols).clamp(20, 500);
    *rows = (*rows).clamp(5, 300);
}

/// True when the payload is nothing but SGR mouse *wheel* press reports
/// (`ESC [ < 64|65 ; col ; row M`) — the only input observers may send.
fn wheel_reports_only(bytes: &[u8]) -> bool {
    fn digits(rest: &[u8], stop: u8) -> Option<&[u8]> {
        let end = rest.iter().position(|&b| b == stop)?;
        if end == 0 || end > 4 || !rest[..end].iter().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(&rest[end + 1..])
    }
    if bytes.is_empty() {
        return false;
    }
    let mut rest = bytes;
    while !rest.is_empty() {
        rest = match rest
            .strip_prefix(b"\x1b[<64;".as_slice())
            .or_else(|| rest.strip_prefix(b"\x1b[<65;".as_slice()))
        {
            Some(r) => r,
            None => return false,
        };
        let Some(r) = digits(rest, b';').and_then(|r| digits(r, b'M')) else {
            return false;
        };
        rest = r;
    }
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_report_whitelist() {
        assert!(wheel_reports_only(b"\x1b[<64;12;5M"));
        assert!(wheel_reports_only(b"\x1b[<65;1;1M\x1b[<65;1;2M"));
        assert!(!wheel_reports_only(b""));
        assert!(!wheel_reports_only(b"ls\r"));
        assert!(!wheel_reports_only(b"\x1b[<0;12;5M")); // click, not wheel
        assert!(!wheel_reports_only(b"\x1b[<64;12;5m")); // release marker
        assert!(!wheel_reports_only(b"\x1b[<64;12;5Mq")); // trailing key
        assert!(!wheel_reports_only(b"\x1b[<64;12345;5M")); // oversized field
        assert!(!wheel_reports_only(b"\x1b[<64;;5M")); // empty field
    }
}
