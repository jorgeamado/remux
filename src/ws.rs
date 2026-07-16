use crate::{paneview, tmux, App};
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
/// The large fixed size a window is forced to while a client views its dashboard,
/// so a full-screen tool (htop) renders every column/row for the capture. The
/// terminal is hidden then, so the oversized render is invisible.
const DASH_COLS: u16 = 210;
const DASH_ROWS: u16 = 60;
/// Min spacing between pane actions from one connection — a rate cap applied
/// BEFORE any registry/topology work, so a client can't CPU-flood the daemon
/// (Codex). Comfortably faster than any human tap.
const PANE_ACTION_MIN_INTERVAL: Duration = Duration::from_millis(50);
/// Cap on any inbound WebSocket message/frame.
const MAX_WS_MESSAGE: usize = 64 * 1024;
/// Text-message token bucket: burst size and refill rate (msgs/sec). Control
/// messages (resize/ping/taps) are well under this; a flood is bounded before
/// the JSON parse. Keystrokes/scroll are Binary and not counted here.
const TEXT_MSG_BURST: f64 = 32.0;
const TEXT_MSG_REFILL_PER_SEC: f64 = 32.0;
/// Copy-overlay capture: scrollback lines requested, response byte cap (most
/// recent kept), and min spacing between captures on one connection. A capture
/// response is large (unlike other frames) and OUT_QUEUE counts messages, so
/// captures get their own rate cap on top of the text token bucket.
const CAPTURE_LINES: u32 = 2000;
const CAPTURE_MAX_BYTES: usize = 256 * 1024;
const CAPTURE_MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Max bytes of a single chat message typed to Claude.
const CHAT_SEND_MAX: usize = 8 * 1024;

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
    /// The client entered (or left) the custom dashboard for a pane. While ≥1
    /// client is on a pane's dashboard, its window is forced to a large capture
    /// resolution so a full-screen tool renders all its info; `pane` is unused
    /// when leaving.
    ViewMode {
        #[serde(default)]
        pane: String,
        dashboard: bool,
    },
    /// A semantic action from a pane's dashboard (e.g. `sort:mem`). The daemon
    /// maps it to the real tool's key(s) via a per-view whitelist and sends them
    /// through tmux — a client can never send raw keystrokes this way.
    PaneAction {
        pane: String,
        action: String,
    },
    /// Window/pane operations (new window, splits, switching) — controller only.
    WindowAction {
        action: String,
        #[serde(default)]
        index: Option<u32>,
    },
    /// Capture a pane's screen + scrollback as plain text for the copy overlay.
    /// The client sends the pane id it was viewing at tap time (snapshot).
    Capture {
        pane: String,
    },
    /// Subscribe this connection to a pane's rendered Claude chat (opt-in;
    /// transcript content is never broadcast). Replaces any prior subscription.
    ChatSubscribe {
        pane: String,
    },
    /// Stop receiving chat updates.
    ChatUnsubscribe,
    /// Type a message to Claude in `pane` (controller-only, like terminal input).
    ChatSend {
        pane: String,
        text: String,
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
    /// A pane's captured text (screen + scrollback) for the copy overlay.
    /// `truncated` = the capture exceeded the byte cap and only the most recent
    /// portion is included.
    PaneCapture {
        pane: &'a str,
        text: &'a str,
        truncated: bool,
    },
    /// Someone/something in this session wants the user: a hook-fed event
    /// (agent_needs_input) or the busy→quiet heuristic.
    Attention {
        kind: &'a str,
        /// Originating pane (`%N`) for hook-fed events — drives the pane-scoped
        /// Claude status chip. Absent for the session-level heuristic.
        #[serde(skip_serializing_if = "Option::is_none")]
        pane: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<&'a str>,
    },
    Pong,
}

fn json(msg: &ServerMsg) -> Message {
    Message::Text(serde_json::to_string(msg).unwrap().into())
}

pub async fn handler(State(app): State<Arc<App>>, upgrade: WebSocketUpgrade) -> Response {
    // Cap inbound frames/messages. Axum defaults to 64 MiB per message; every
    // client message (control frames, pane actions) is small, so a tight bound
    // stops a paired device from forcing huge transient allocations by padding a
    // message (Codex). Well above any legitimate client payload.
    upgrade
        .max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| async move {
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
                                    pane: att.pane.as_deref(),
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

    // Push this session's pane views (structured state a source streams for a
    // pane, rendered by the PWA as a custom interface) and keep them reconciled.
    // Session-filtered like the feed — only panes in *this* session — and
    // debounced to coalesce a fast source. A full-set frame each time so
    // add/update/remove all reconcile client-side.
    let paneview_task = tokio::spawn({
        let mut rx = app.pane_views.subscribe();
        // Also wake on topology changes: a pane moving between sessions (or a
        // window closing) shifts which views belong here WITHOUT a pane-view
        // hint, so the filter must re-run then too — otherwise a moved pane's
        // frame goes stale in the old session and never appears in the new one.
        let mut topo_rx = app.topology.subscribe();
        let out = out_tx.clone();
        let app = app.clone();
        let session = session.clone();
        async move {
            let mut last_sent: Option<String> = None;
            loop {
                // Pane ids that belong to this session right now. borrow_and_update
                // marks this topology version seen so `topo_rx.changed()` only
                // fires on a genuinely new one.
                let session_panes: std::collections::HashSet<String> = topo_rx
                    .borrow_and_update()
                    .iter()
                    .filter(|s| s.name == session)
                    .flat_map(|s| s.windows.iter())
                    .flat_map(|w| w.panes.iter())
                    .map(|p| p.id.clone())
                    .collect();
                let views: Vec<_> = app
                    .pane_views
                    .snapshot()
                    .into_iter()
                    .filter(|v| session_panes.contains(&v.pane))
                    .collect();
                let is_empty = views.is_empty();
                let json = serde_json::json!({ "type": "pane_views", "views": views }).to_string();
                // Send on change; stay quiet on a fresh empty connection, but do
                // send an empty set to clear a previously-sent one.
                if last_sent.as_deref() != Some(json.as_str()) && (!is_empty || last_sent.is_some())
                {
                    if out.send(Message::Text(json.clone().into())).await.is_err() {
                        break;
                    }
                    last_sent = Some(json);
                }
                // Wake on either a pane-view change or a topology change.
                tokio::select! {
                    r = rx.recv() => match r {
                        Ok(()) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    c = topo_rx.changed() => {
                        if c.is_err() {
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
                while matches!(rx.try_recv(), Ok(())) {}
            }
        }
    });

    // Per-connection Claude chat: opt-in (the main loop sets the subscribed pane
    // via this watch). Transcript content is served ONLY here, never broadcast;
    // reading is gated on session membership (checked before the watch is set),
    // NOT the `approve` capability.
    let (chat_sub_tx, chat_sub_rx) = tokio::sync::watch::channel::<Option<String>>(None);
    let chat_task = tokio::spawn({
        let mut hint = app.chat.subscribe();
        let mut sub_rx = chat_sub_rx.clone();
        let out = out_tx.clone();
        let app = app.clone();
        async move {
            let mut cur: Option<String> = None;
            let mut cursor_gen: u64 = 0;
            let mut cursor_seq: u64 = 0;
            let mut sent_gen: Option<u64> = None;
            loop {
                // Reconcile to the currently-subscribed pane; a change resets the
                // cursor so the new pane gets a fresh snapshot.
                let want = sub_rx.borrow().clone();
                if want != cur {
                    cur = want;
                    cursor_gen = 0;
                    cursor_seq = 0;
                    sent_gen = None;
                }
                if let Some(pane) = cur.clone() {
                    if let Some(u) = app.chat.update_since(&pane, cursor_gen, cursor_seq) {
                        let deliver =
                            !u.messages.is_empty() || (u.full && sent_gen != Some(u.generation));
                        if deliver {
                            cursor_gen = u.generation;
                            if let Some(last) = u.messages.last() {
                                cursor_seq = last.seq + 1;
                            }
                            sent_gen = Some(u.generation);
                            let msg = Message::Text(
                                serde_json::json!({
                                    "type": "chat_update", "pane": u.pane,
                                    "generation": u.generation, "full": u.full,
                                    "messages": u.messages,
                                })
                                .to_string()
                                .into(),
                            );
                            if out.send(msg).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                tokio::select! {
                    r = hint.recv() => match r {
                        Ok(()) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    c = sub_rx.changed() => { if c.is_err() { break } }
                }
            }
        }
    });

    // Resolve our tmux client name (needed to toggle observer/controller flags).
    let client_name = resolve_client_name(child_pid).await;
    if client_name.is_none() {
        tracing::warn!("could not resolve tmux client name; take_control disabled");
    }

    let mut controller = false;
    // The window (if any) this client is holding at dashboard capture size.
    let mut current_dash: Option<String> = None;
    // Rate-limit pane actions from this connection (applied before any registry/
    // topology work) so a client can't flood a source's back-channel (Codex).
    let mut last_pane_action: Option<std::time::Instant> = None;
    // Token bucket over ALL inbound text messages, spent before the JSON parse,
    // so a client can't CPU-flood the daemon with large/malformed control frames.
    let mut text_tokens: f64 = TEXT_MSG_BURST;
    let mut text_refill = std::time::Instant::now();
    // Last copy-overlay capture on this connection (separate rate cap — a capture
    // response is large and OUT_QUEUE counts messages, not bytes).
    let mut last_capture: Option<std::time::Instant> = None;
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
            Message::Text(text) => {
                // Token-bucket EVERY text message before the (relatively costly)
                // JSON parse, so an authenticated client can't CPU-flood the daemon
                // with a stream of large/malformed JSON control frames (Codex). All
                // legitimate control messages (resize, ping, taps) are far below
                // this rate; keystrokes/scroll are Binary and unaffected.
                let now = std::time::Instant::now();
                text_tokens = (text_tokens
                    + now.duration_since(text_refill).as_secs_f64() * TEXT_MSG_REFILL_PER_SEC)
                    .min(TEXT_MSG_BURST);
                text_refill = now;
                if text_tokens < 1.0 {
                    continue; // over budget — drop before parsing
                }
                text_tokens -= 1.0;
                match serde_json::from_str::<ClientMsg>(&text) {
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
                                tokio::task::spawn_blocking(move || tmux::promote_client(&name))
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
                        // Only drop the controller role if tmux actually demoted us.
                        // Otherwise the client stays a sizing participant (window-size
                        // latest) and reporting "observer" would be a lie — the exact
                        // failure the pane-view dashboard relies on NOT happening.
                        let demoted = match &client_name {
                            Some(name) => {
                                let name = name.clone();
                                tokio::task::spawn_blocking(move || tmux::demote_client(&name))
                                    .await?
                                    .is_ok()
                            }
                            None => true, // no client name → never really driving size
                        };
                        if demoted {
                            controller = false;
                            let _ = out_tx.send(status("observer")).await;
                        } else {
                            let _ = out_tx
                                .send(json(&ServerMsg::Error {
                                    code: "release_failed",
                                    message: "could not release control",
                                }))
                                .await;
                        }
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
                    Ok(ClientMsg::ViewMode { pane, dashboard }) => {
                        // Force this pane's window to the big capture resolution while
                        // the dashboard is shown, and restore it on leave/switch. Only
                        // a pane in THIS session that actually has a view may be
                        // forced — else a client could resize another session's window.
                        let target = if dashboard {
                            let in_session = app.topology.borrow().iter().any(|s| {
                                s.name == session
                                    && s.windows
                                        .iter()
                                        .any(|w| w.panes.iter().any(|p| p.id == pane))
                            });
                            let has_view = app.pane_views.view_of(&pane).is_some();
                            if in_session && has_view {
                                match tokio::task::spawn_blocking(move || {
                                    tmux::window_of_pane(&pane)
                                })
                                .await
                                {
                                    Ok(Ok(w)) => w,
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        match target {
                            Some(win) if current_dash.as_deref() != Some(&win) => {
                                if let Some(old) = current_dash.take() {
                                    dash_leave(&app, old).await;
                                }
                                dash_enter(&app, win.clone()).await;
                                current_dash = Some(win);
                            }
                            Some(_) => {} // already sized for this window
                            None => {
                                if let Some(old) = current_dash.take() {
                                    dash_leave(&app, old).await;
                                }
                            }
                        }
                    }
                    Ok(ClientMsg::PaneAction { pane, action }) => {
                        // Rate-limit BEFORE any registry/topology work so a flood of
                        // tiny frames can't spin the daemon (Codex). Too-soon → drop.
                        let now = std::time::Instant::now();
                        if last_pane_action
                            .is_some_and(|t| now.duration_since(t) < PANE_ACTION_MIN_INTERVAL)
                        {
                            continue;
                        }
                        last_pane_action = Some(now);
                        // Only for a pane in THIS session. Compute before any await
                        // (don't hold the topology borrow across it). Route by the
                        // view's PROVENANCE, not its id: an internal-htop view runs the
                        // tmux whitelist; a socket (plugin) view forwards to the source.
                        let in_session = app.topology.borrow().iter().any(|s| {
                            s.name == session
                                && s.windows
                                    .iter()
                                    .any(|w| w.panes.iter().any(|p| p.id == pane))
                        });
                        let kind = app.pane_views.action_kind(&pane);
                        if in_session && kind == Some(paneview::ActionKind::Source) {
                            // A plugin action: the daemon does not interpret it. Cap
                            // its size (matches the token grammar); send_action
                            // enforces membership + policy.
                            if action.len() <= paneview::MAX_ACTION_TOKEN {
                                app.pane_views.send_action(
                                    &pane,
                                    &action,
                                    app.auth.can_approve(&device.id),
                                );
                            }
                        } else if in_session && kind == Some(paneview::ActionKind::Htop) {
                            match paneview::parse_htop_action(&action) {
                                Some(paneview::HtopAction::Kill { pid, signal }) => {
                                    // Killing is destructive → require the host-granted
                                    // `approve` capability (not just any paired device),
                                    // and only a pid the dashboard is actually showing.
                                    if app.auth.can_approve(&device.id)
                                        && app.pane_views.pane_has_pid(&pane, pid)
                                    {
                                        let _ = tokio::task::spawn_blocking(move || {
                                            paneview::kill_process(pid, signal)
                                        })
                                        .await;
                                    }
                                }
                                Some(act @ paneview::HtopAction::Filter(_)) => {
                                    // Filter types attacker-controlled LITERAL text into
                                    // the pane. send-keys -l stops tmux key interpretation
                                    // but not shell syntax, and check-then-send is not
                                    // atomic: if htop exits mid-sequence the text can land
                                    // at the shell for the user's next Enter. So restrict
                                    // it — like kill — to a device holding the host-granted
                                    // `approve` capability, which is already trusted to
                                    // approve arbitrary command execution. exec_htop_action
                                    // additionally re-verifies htop owns the pane per phase.
                                    if app.auth.can_approve(&device.id) {
                                        let p = pane.clone();
                                        let _ = tokio::task::spawn_blocking(move || {
                                            paneview::exec_htop_action(&p, &act)
                                        })
                                        .await;
                                    }
                                }
                                Some(act) => {
                                    // sort/invert/tree: fixed single keys, no attacker-
                                    // controlled content. exec re-verifies htop owns the
                                    // pane right before sending (no junk at a shell).
                                    let p = pane.clone();
                                    let _ = tokio::task::spawn_blocking(move || {
                                        paneview::exec_htop_action(&p, &act)
                                    })
                                    .await;
                                }
                                None => {}
                            }
                        }
                    }
                    Ok(ClientMsg::Capture { pane }) => {
                        // Read access to this session's pane contents + recent
                        // scrollback (more than just bytes already streamed to
                        // THIS device — incl. pre-connect history and full-screen
                        // apps), but never another session. In-session gate, no
                        // `approve` (read-only, like scrolling). Own rate cap.
                        let in_session = app.topology.borrow().iter().any(|s| {
                            s.name == session
                                && s.windows
                                    .iter()
                                    .any(|w| w.panes.iter().any(|p| p.id == pane))
                        });
                        let now = std::time::Instant::now();
                        let spaced = last_capture
                            .is_none_or(|t| now.duration_since(t) >= CAPTURE_MIN_INTERVAL);
                        if in_session && spaced {
                            last_capture = Some(now);
                            let p = pane.clone();
                            let captured = tokio::task::spawn_blocking(move || {
                                tmux::capture_scrollback(&p, CAPTURE_LINES)
                            })
                            .await;
                            match captured {
                                Ok(Ok(Some(raw))) => {
                                    let (text, truncated) = cap_capture(&raw);
                                    let _ = out_tx
                                        .send(json(&ServerMsg::PaneCapture {
                                            pane: &pane,
                                            text: &text,
                                            truncated,
                                        }))
                                        .await;
                                }
                                _ => {
                                    let _ = out_tx
                                        .send(json(&ServerMsg::Error {
                                            code: "capture_unavailable",
                                            message: "could not capture the pane",
                                        }))
                                        .await;
                                }
                            }
                        }
                    }
                    Ok(ClientMsg::ChatSubscribe { pane }) => {
                        // Gate on SESSION MEMBERSHIP (not approve) — the terminal
                        // already shows this pane's text to any in-session device.
                        let in_session = app.topology.borrow().iter().any(|s| {
                            s.name == session
                                && s.windows
                                    .iter()
                                    .any(|w| w.panes.iter().any(|p| p.id == pane))
                        });
                        // The chat push task sends a fresh snapshot on this change.
                        let _ = chat_sub_tx.send(in_session.then_some(pane));
                    }
                    Ok(ClientMsg::ChatUnsubscribe) => {
                        let _ = chat_sub_tx.send(None);
                    }
                    Ok(ClientMsg::ChatSend { pane, text }) => {
                        // Typing to Claude is CONTROLLER-only (same authority as
                        // terminal input), this session's pane, rate-limited and
                        // length-capped. send-keys -l (literal) so the message is
                        // never interpreted as tmux keys, then a separate Enter.
                        let now = std::time::Instant::now();
                        let spaced = last_pane_action
                            .is_none_or(|t| now.duration_since(t) >= PANE_ACTION_MIN_INTERVAL);
                        let in_session = app.topology.borrow().iter().any(|s| {
                            s.name == session
                                && s.windows
                                    .iter()
                                    .any(|w| w.panes.iter().any(|p| p.id == pane))
                        });
                        if controller
                            && in_session
                            && spaced
                            && !text.is_empty()
                            && text.len() <= CHAT_SEND_MAX
                        {
                            last_pane_action = Some(now);
                            let p = pane.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                tmux::send_keys(&p, &text)
                                    .and_then(|_| tmux::send_named(&p, &["Enter"]))
                            })
                            .await;
                        }
                    }
                    Ok(ClientMsg::Ping) => {
                        let _ = out_tx.send(json(&ServerMsg::Pong)).await;
                    }
                    Ok(ClientMsg::Auth { .. }) => {} // already authed; ignore
                    Err(e) => {
                        tracing::debug!("bad client message: {e}");
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Release any dashboard capture-size hold so the window returns to normal
    // sizing (the client left without a clean ViewMode{false}).
    if let Some(win) = current_dash.take() {
        dash_leave(&app, win).await;
    }

    // Connection gone: kill the attach client. tmux drops it from sizing and the
    // remaining clients (e.g. the Mac) immediately get their dimensions back.
    let _ = child.kill();
    attention_task.abort();
    permits_task.abort();
    feed_task.abort();
    paneview_task.abort();
    chat_task.abort();
    revoke_task.abort();
    topology_task.abort();
    sender.abort();
    tracing::info!(device = %device.name, "client disconnected");
    Ok(())
}

/// A client is now viewing `window`'s dashboard: bump the count and, if it's the
/// first, force the big capture resolution.
async fn dash_enter(app: &Arc<App>, window: String) {
    let first = {
        let mut m = app.dash_windows.lock().unwrap();
        let n = m.entry(window.clone()).or_insert(0);
        *n += 1;
        *n == 1
    };
    if first {
        let w = window;
        let _ =
            tokio::task::spawn_blocking(move || tmux::set_capture_size(&w, DASH_COLS, DASH_ROWS))
                .await;
    }
}

/// A client left `window`'s dashboard: drop the count and, if it's the last,
/// restore client-driven sizing.
async fn dash_leave(app: &Arc<App>, window: String) {
    let last = {
        let mut m = app.dash_windows.lock().unwrap();
        match m.get_mut(&window) {
            Some(n) => {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    m.remove(&window);
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    };
    if last {
        let w = window;
        let _ = tokio::task::spawn_blocking(move || tmux::clear_capture_size(&w)).await;
    }
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

/// Prepare a captured pane for the copy overlay: trim trailing whitespace on
/// every line (tmux pads cells with spaces), then keep at most
/// `CAPTURE_MAX_BYTES` of the MOST RECENT text (the tail), cut on a char
/// boundary so the JSON stays valid UTF-8. Returns `(text, truncated)`.
fn cap_capture(raw: &str) -> (String, bool) {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    if out.len() <= CAPTURE_MAX_BYTES {
        return (out, false);
    }
    // Keep the tail; advance to the next char boundary so we never split a char.
    let mut start = out.len() - CAPTURE_MAX_BYTES;
    while start < out.len() && !out.is_char_boundary(start) {
        start += 1;
    }
    (out[start..].to_string(), true)
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
    fn cap_capture_trims_and_bounds() {
        // Per-line trailing whitespace is trimmed; each line ends with \n.
        let (text, trunc) = cap_capture("ls   \n  hi \t\n");
        assert_eq!(text, "ls\n  hi\n");
        assert!(!trunc);

        // Over-cap keeps the most-recent tail and flags truncation.
        let big = "x".repeat(CAPTURE_MAX_BYTES + 5000);
        let (text, trunc) = cap_capture(&big);
        assert!(trunc);
        assert!(text.len() <= CAPTURE_MAX_BYTES + 1); // +1 for the appended \n

        // A multibyte char straddling the cut boundary is never split.
        let mut s = "é".repeat(CAPTURE_MAX_BYTES); // 2 bytes each
        s.push('\n');
        let (text, trunc) = cap_capture(&s);
        assert!(trunc);
        assert!(text.is_char_boundary(0) && std::str::from_utf8(text.as_bytes()).is_ok());
    }

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
