//! Local ingest interface (M4a/M4b): a Unix socket in the state dir where hook
//! scripts report semantic events — "Claude Code is waiting for input in
//! pane %3" (M4a), or "Claude Code wants permission to run X; block until a
//! device decides" (M4b). Same filesystem authentication as the admin socket
//! (0600 + peer-uid), but a deliberately separate surface: admin accepts
//! *commands* from the owner, ingest accepts *data* from same-uid hook
//! processes. An ingest event can raise attention or open a permission card;
//! it can never act — only a paired, approve-capable device decides a card.
//!
//! Connection lifecycle (kind-dependent, Codex-reviewed):
//!
//! ```text
//! 1. bounded admission + read + parse (short deadline, fail-fast pool)
//! 2. dispatch on `kind`
//! 3a. fire-and-forget kinds ack immediately (M4a); or
//! 3b. agent_permission holds the connection up to CARD_TTL, releasing the
//!     admission slot and instead occupying a bounded registry slot, while
//!     concurrently watching for a decision, expiry, and the client vanishing
//!     (the Mac answered and Claude SIGTERM'd the hook).
//! ```

use crate::permit::{self, Card, Decision};
use crate::App;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::OwnedSemaphorePermit;

/// Cap on the bytes read from one connection; a document that large without
/// its newline inside the cap fails parsing. Most events are tiny, but a
/// permission `summary` can be ~2 KB of UTF-8 (a phone must see the whole
/// command it approves), so this leaves comfortable headroom while still
/// bounding abuse on a same-uid socket.
const MAX_LINE: u64 = 16384;
const MAX_SOURCE: usize = 32;
const MAX_MESSAGE: usize = 256;
const MAX_TOOL: usize = 32;
/// A phone approving a command must see the whole thing (a benign prefix can
/// hide a destructive suffix), so this is generous — it only clips pathological
/// inputs, and when it does the card is flagged `truncated` and Allow is
/// disabled. Still well under `MAX_LINE` so the event never overflows the wire.
const MAX_SUMMARY: usize = 2048;
const MAX_PROMPT_ID: usize = 64;
const MAX_PANE: usize = 16;
/// Rate limit: events *offered* per window, across all producers — charged
/// before parsing, deliberately: malformed floods must not get free parse
/// work, at the cost that one confused producer can starve the window.
/// Sized for agent-state lifecycle traffic (2 events per tool use) so a busy
/// agent can't starve the low-volume-but-critical permission requests (Codex).
const RATE_MAX: u32 = 300;
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Concurrent connections in the read/parse phase; excess connects are dropped
/// (hooks fail fast and exit non-zero — never queue a slow client). Held
/// permission waits do NOT sit in this pool: they release their admission slot
/// and are bounded instead by the permit registry.
const MAX_CONNS: usize = 16;
/// Budget for the short phases: reading the request line, and writing an ack.
/// The held-wait uses CARD_TTL instead, via its own timer.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ingest.sock")
}

/// Just enough to route by kind. No `deny_unknown_fields` — this only reads
/// the envelope; the per-kind struct below is the strict schema.
#[derive(Deserialize)]
struct Envelope {
    v: u32,
    kind: String,
}

/// M4a attention event. Strict schema (a contract, not a hint).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttentionEvent {
    #[allow(dead_code)]
    v: u32,
    #[allow(dead_code)]
    kind: String,
    /// tmux pane id (`%N`) — `$TMUX_PANE` in the producer's environment.
    pane: String,
    /// Producer label ("claude-code", "shell", …). Informational.
    source: String,
    /// Optional human-readable detail. Informational; sanitized and capped.
    #[serde(default)]
    message: Option<String>,
}

/// Generic agent lifecycle event (feeds the `claude.v1` dashboard). Coarse status
/// only — no secrets.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentStateEvent {
    #[allow(dead_code)]
    v: u32,
    #[allow(dead_code)]
    kind: String,
    pane: String,
    /// The lifecycle verb (see `AgentStateKind::verb`).
    verb: String,
    /// Agent session id — guards against stale events from a superseded session.
    session_id: String,
    #[serde(default)]
    operation_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
}

/// M4b permission event. Strict per-kind schema so the required fields can't be
/// omitted and meaningless field combinations can't slip through a shared flat
/// struct.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionEvent {
    #[allow(dead_code)]
    v: u32,
    #[allow(dead_code)]
    kind: String,
    pane: String,
    source: String,
    /// Tool name from the hook payload ("Bash", "Edit", …).
    tool: String,
    /// One-line human summary (command / file path). Sanitized and capped;
    /// never shown on the lock screen — fetched post-auth (secrets posture).
    summary: String,
    /// The producer already had to truncate `summary` (the real input was
    /// longer than it could send). The daemon ORs this with its own cap so the
    /// card's `truncated` flag is true if *either* side cut — the phone then
    /// won't offer Allow. Optional (older/simple producers omit it).
    #[serde(default)]
    truncated: bool,
    /// Claude Code's `prompt_id`, for dedup/correlation. Optional; validated.
    #[serde(default)]
    prompt_id: Option<String>,
}

struct RateLimiter {
    window_start: Instant,
    count: u32,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            count: 0,
        }
    }
    fn allow(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= RATE_WINDOW {
            self.window_start = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= RATE_MAX
    }
}

pub fn spawn(app: Arc<App>, state_dir: &Path) -> Result<()> {
    let path = socket_path(state_dir);
    // The admin socket's live-probe is the single-instance guard and admin
    // spawns first — a leftover ingest socket here is stale by construction.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind ingest socket {}", path.display()))?;
    let owner_uid = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        std::fs::metadata(&path)?.uid()
    };
    let limiter = Arc::new(std::sync::Mutex::new(RateLimiter::new()));
    let conns = Arc::new(tokio::sync::Semaphore::new(MAX_CONNS));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            if !crate::admin::peer_allowed(&stream, owner_uid) {
                tracing::warn!("ingest socket: rejected connection from another uid");
                continue;
            }
            // Slow-client defence: bounded admission concurrency. Excess or
            // stalled producers are dropped, not queued. The permit is handed
            // to the handler so the held-wait path can release it early.
            let Ok(permit) = conns.clone().try_acquire_owned() else {
                tracing::warn!("ingest socket: connection limit reached, dropping");
                continue;
            };
            let app = app.clone();
            let limiter = limiter.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, app, limiter, permit).await {
                    tracing::debug!("ingest connection failed: {e:#}");
                }
            });
        }
    });
    Ok(())
}

async fn write_ack<W: AsyncWriteExt + Unpin>(
    write: &mut W,
    resp: &serde_json::Value,
) -> Result<()> {
    write.write_all(format!("{resp}\n").as_bytes()).await?;
    Ok(())
}

async fn handle(
    stream: UnixStream,
    app: Arc<App>,
    limiter: Arc<std::sync::Mutex<RateLimiter>>,
    permit: OwnedSemaphorePermit,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    // take() caps how much one connection can feed us; a line that hits the
    // cap can't have a trailing newline and fails parsing below.
    let mut reader = BufReader::new(read).take(MAX_LINE);
    let mut line = String::new();
    tokio::time::timeout(IO_TIMEOUT, reader.read_line(&mut line))
        .await
        .context("ingest read timed out")??;

    // Rate limit charged pre-parse, before any per-kind work.
    if !limiter.lock().unwrap().allow() {
        return write_ack(&mut write, &json_err("rate limited")).await;
    }

    let line = line.trim();
    let kind = match serde_json::from_str::<Envelope>(line) {
        Ok(e) if e.v == 1 => e.kind,
        Ok(_) => return write_ack(&mut write, &json_err("unsupported version")).await,
        Err(e) => return write_ack(&mut write, &json_err(&e.to_string())).await,
    };

    match kind.as_str() {
        "agent_needs_input" => {
            let resp = match serde_json::from_str::<AttentionEvent>(line) {
                Ok(ev) => process_attention(&app, ev),
                Err(e) => json_err(&e.to_string()),
            };
            write_ack(&mut write, &resp).await
        }
        "agent_permission" => match serde_json::from_str::<PermissionEvent>(line) {
            Ok(ev) => handle_permission(app, ev, reader, write, permit).await,
            Err(e) => write_ack(&mut write, &json_err(&e.to_string())).await,
        },
        "agent_state" => {
            let resp = match serde_json::from_str::<AgentStateEvent>(line) {
                Ok(ev) => process_agent_state(&app, ev),
                Err(e) => json_err(&e.to_string()),
            };
            write_ack(&mut write, &resp).await
        }
        _ => write_ack(&mut write, &json_err("unknown kind")).await,
    }
}

fn json_err(msg: &str) -> serde_json::Value {
    serde_json::json!({ "ok": false, "error": msg })
}

fn process_attention(app: &App, ev: AttentionEvent) -> serde_json::Value {
    if !valid_pane(&ev.pane) {
        return json_err("bad pane id (want %N)");
    }
    let message = ev.message.as_deref().map(sanitize).unwrap_or_default();
    // All producer strings are same-uid-controlled; sanitize, then validate
    // what will actually be stored/logged (a control-chars-only source must
    // be rejected, not become "").
    let source = sanitize(&ev.source);
    if source.is_empty() || source.len() > MAX_SOURCE {
        return json_err("bad source");
    }
    let session = match sessions_of_pane(app, &ev.pane) {
        Ok(s) => s,
        Err(e) => return json_err(e),
    };
    tracing::info!(
        session = %session, pane = %ev.pane, source = %source,
        message = %message, "ingest: agent needs input"
    );
    // The existing attention pipeline does the rest: push dispatch (with its
    // suppression rules), in-band ws frames, and the /api/attention deep link.
    let _ = app.attention.send(crate::Attention {
        session: session.clone(),
        kind: "agent_needs_input".into(),
        pane: Some(ev.pane.clone()),
        reason: (!message.is_empty()).then_some(message),
        source: Some(source),
    });
    serde_json::json!({ "ok": true, "session": session })
}

/// Cap for agent-state identifier / tool-name fields.
const MAX_AGENT_FIELD: usize = 128;

/// Sanitize (strip control chars) then cap to a short field length.
fn agent_field(s: &str) -> String {
    sanitize(s).chars().take(MAX_AGENT_FIELD).collect()
}

fn process_agent_state(app: &App, ev: AgentStateEvent) -> serde_json::Value {
    if !valid_pane(&ev.pane) {
        return json_err("bad pane id (want %N)");
    }
    let sid = agent_field(&ev.session_id);
    if sid.is_empty() {
        return json_err("missing session_id");
    }
    let op = ev
        .operation_id
        .map(|o| agent_field(&o))
        .filter(|o| !o.is_empty());
    let tool = ev
        .tool_name
        .map(|t| agent_field(&t))
        .filter(|t| !t.is_empty());
    use crate::agent::Event;
    let event = match ev.verb.as_str() {
        "session-start" => Event::SessionStart { session_id: sid },
        "prompt-submitted" => Event::PromptSubmitted { session_id: sid },
        "operation-started" => match (op, tool) {
            (Some(op_id), Some(tool)) if !op_id.is_empty() => Event::OperationStarted {
                session_id: sid,
                op_id,
                tool,
            },
            _ => return json_err("operation-started needs operation_id + tool_name"),
        },
        "operation-ended" => match op {
            Some(op_id) if !op_id.is_empty() => Event::OperationEnded {
                session_id: sid,
                op_id,
            },
            _ => return json_err("operation-ended needs operation_id"),
        },
        "idle" => Event::Idle { session_id: sid },
        "session-ended" => Event::SessionEnded { session_id: sid },
        "touch" => Event::Touch { session_id: sid },
        _ => return json_err("unknown agent-state verb"),
    };
    app.agents.apply(&ev.pane, event);
    serde_json::json!({ "ok": true })
}

/// Removes a card from the registry on any exit of the held-wait future,
/// including task cancellation (the outer future being dropped) — so an
/// aborted wait cannot orphan a registry slot. `resolve` may have already
/// taken the entry; `remove` is idempotent.
struct CardGuard {
    app: Arc<App>,
    id: String,
}

impl Drop for CardGuard {
    fn drop(&mut self) {
        self.app.perms.remove(&self.id);
    }
}

async fn handle_permission(
    app: Arc<App>,
    ev: PermissionEvent,
    reader: tokio::io::Take<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut write: tokio::net::unix::OwnedWriteHalf,
    permit: OwnedSemaphorePermit,
) -> Result<()> {
    if !valid_pane(&ev.pane) {
        return write_ack(&mut write, &json_err("bad pane id (want %N)")).await;
    }
    let source = sanitize(&ev.source);
    if source.is_empty() || source.len() > MAX_SOURCE {
        return write_ack(&mut write, &json_err("bad source")).await;
    }
    let tool = sanitize(&ev.tool);
    if tool.is_empty() || tool.len() > MAX_TOOL {
        return write_ack(&mut write, &json_err("bad tool")).await;
    }
    // Cap daemon-side too, and mark truncation if *we* cut it (or the producer
    // already did) — the card carries this so the phone can refuse a remote
    // Allow it can't fully see. Strip control chars WITHOUT `sanitize`'s own
    // 256-char cap first, or that cap would silently truncate below MAX_SUMMARY
    // and leave `truncated` false (a real approval bypass — Codex).
    let clean_summary = strip_control(&ev.summary);
    let summary: String = clean_summary.chars().take(MAX_SUMMARY).collect();
    let truncated = ev.truncated || summary.chars().count() < clean_summary.chars().count();
    // A present-but-malformed prompt_id is rejected, not silently dropped —
    // dropping it to None would sever dedup/correlation and turn a retry into
    // an unrelated card.
    let prompt_id = match ev.prompt_id {
        Some(p) if valid_prompt_id(&p) => Some(p),
        Some(_) => return write_ack(&mut write, &json_err("bad prompt_id")).await,
        None => None,
    };
    let session = match sessions_of_pane(&app, &ev.pane) {
        Ok(s) => s,
        Err(e) => return write_ack(&mut write, &json_err(e)).await,
    };

    let now = Instant::now();
    let card = Card {
        id: permit::mint_id(),
        session: session.clone(),
        pane: ev.pane.clone(),
        source: source.clone(),
        tool: tool.clone(),
        summary,
        truncated,
        prompt_id,
        created: now,
        deadline: now + permit::CARD_TTL,
    };
    let rx = match app.perms.insert(card.clone()) {
        Ok(rx) => rx,
        // Cap hit → immediate reject → the hook falls back to the Mac dialog.
        Err(msg) => return write_ack(&mut write, &json_err(msg)).await,
    };
    let _guard = CardGuard {
        app: app.clone(),
        id: card.id.clone(),
    };
    // Past read/parse and now holding a bounded registry slot: release the
    // admission permit so a burst of held waits can't exhaust the ingest pool.
    drop(permit);
    tracing::info!(
        id = %card.id, session = %card.session, pane = %card.pane,
        source = %card.source, tool = %card.tool, "ingest: permission card opened"
    );
    // Wake the phone. Kind only — no command (secrets posture) and no source
    // either, so the in-band attention frame doesn't leak *which* agent asked
    // to a non-approve device; the card itself (source/tool/command) reaches
    // only approve-capable devices via the permission_cards frame. The
    // dispatcher pushes this but does not file it in the 600s attention
    // retention (the card registry's own TTL is the source of truth).
    let _ = app.attention.send(crate::Attention {
        session: card.session.clone(),
        kind: "agent_permission".into(),
        // Intentionally omitted: this frame reaches every session device, but the
        // pane→approval association is privileged (it derives from the card, which
        // only approve-capable devices receive). Non-approve devices see just
        // "something wants attention", never which pane has a pending approval.
        pane: None,
        reason: None,
        source: None,
    });

    // Recover the raw read half for EOF detection. Our protocol has the client
    // send exactly one line then wait, so the BufReader holds nothing more; the
    // Take limit is irrelevant here.
    let read_half = reader.into_inner().into_inner();
    match wait_for_decision(rx, read_half).await {
        Some((decision, confirm)) => {
            let resp = serde_json::json!({ "ok": true, "decision": decision.as_str() });
            // Confirm only if the write to the live socket succeeds — that's the
            // signal the deciding device gets ("written", not a guaranteed
            // end-to-end ACK). If the write fails, `confirm` drops and the
            // device sees the not-delivered (409) path.
            if let Ok(Ok(())) = tokio::time::timeout(IO_TIMEOUT, write_ack(&mut write, &resp)).await
            {
                let _ = confirm.send(());
            }
        }
        // No decision (expiry / client gone). Never fabricated — the hook exits
        // non-zero and Claude Code asks on the Mac.
        None => {
            let _ =
                tokio::time::timeout(IO_TIMEOUT, write_ack(&mut write, &json_err("expired"))).await;
        }
    }
    Ok(())
    // _guard drops here → app.perms.remove(card.id) (no-op if resolve took it).
}

/// Await whichever comes first: a device decision, expiry, or the client
/// disappearing. A broken/closed connection means the Mac already answered (or
/// the hook died) — treated as "no decision", never a fabricated allow/deny.
/// On a decision, yields the confirmation sender the caller fires after it has
/// written the decision back to the (live) hook socket.
async fn wait_for_decision(
    rx: tokio::sync::oneshot::Receiver<(Decision, tokio::sync::oneshot::Sender<()>)>,
    mut read_half: tokio::net::unix::OwnedReadHalf,
) -> Option<(Decision, tokio::sync::oneshot::Sender<()>)> {
    let mut buf = [0u8; 64];
    tokio::select! {
        // `biased`: poll the socket first so a closed connection wins any tie
        // with a simultaneously-ready decision. That keeps the invariant that a
        // broken wait (the Mac answered, Claude SIGTERM'd the hook) is never
        // overridden by a late phone decision — on this branch `rx` is dropped,
        // so a racing resolve()'s send fails and reports Unknown.
        biased;
        // Ok(0) = EOF, Ok(n) = unexpected trailing bytes, Err = reset. All mean
        // the waiting client is effectively gone.
        _ = read_half.read(&mut buf) => None,
        // Resolved by a device (Ok) or the sender was dropped by cleanup (Err).
        d = rx => d.ok(),
        _ = tokio::time::sleep(permit::CARD_TTL) => None,
    }
}

/// Strip control chars, no length cap. Callers that need a bound apply their
/// own `.take(...)` and can then tell whether *they* truncated.
fn strip_control(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Terminal-controlled text: strip control chars, cap length.
fn sanitize(s: &str) -> String {
    strip_control(s).chars().take(MAX_MESSAGE).collect()
}

pub(crate) fn valid_pane(s: &str) -> bool {
    s.len() >= 2
        && s.len() <= MAX_PANE
        && s.starts_with('%')
        && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Claude Code prompt ids are uuid-ish; accept a conservative shape and cap.
fn valid_prompt_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_PROMPT_ID
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The single session containing pane `%N` per the latest topology snapshot.
/// More than one is possible with linked windows — ambiguous, so refused
/// (guessing would notify/act for the wrong session).
pub(crate) fn sessions_of_pane(app: &App, pane: &str) -> Result<String, &'static str> {
    let snap = app.topology.borrow();
    let mut matches = snap.iter().filter(|s| {
        s.windows
            .iter()
            .any(|w| w.panes.iter().any(|p| p.id == pane))
    });
    match (matches.next(), matches.next()) {
        (None, _) => Err("unknown pane"),
        (Some(s), None) => Ok(s.name.clone()),
        (Some(_), Some(_)) => Err("pane is linked into multiple sessions"),
    }
}

/// CLI side (`remux emit`): one line-JSON event, one ack. Deadlined — a
/// wedged daemon must fail the hook fast, not hang it (connect on a local
/// Unix socket never blocks meaningfully; read/write can).
pub fn request(state_dir: &Path, body: serde_json::Value) -> Result<serde_json::Value> {
    let v = request_raw(state_dir, body, IO_TIMEOUT)?;
    if v["ok"] != serde_json::json!(true) {
        anyhow::bail!("daemon refused event: {}", v["error"]);
    }
    Ok(v)
}

/// CLI side for `remux emit permission --wait`: like `request`, but the read
/// deadline outlasts the daemon's own card expiry (the daemon decides first;
/// this timeout is only a backstop against a wedged daemon). Returns the raw
/// response — the caller distinguishes a decision from an `expired` ack.
pub fn request_wait(state_dir: &Path, body: serde_json::Value) -> Result<serde_json::Value> {
    // CARD_TTL plus slack: the daemon answers at CARD_TTL, so this must be
    // comfortably longer or a slow ack would look like a daemon hang.
    let deadline = permit::CARD_TTL + Duration::from_secs(30);
    request_raw(state_dir, body, deadline)
}

fn request_raw(
    state_dir: &Path,
    body: serde_json::Value,
    read_timeout: Duration,
) -> Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let path = socket_path(state_dir);
    let stream = std::os::unix::net::UnixStream::connect(&path).with_context(|| {
        format!(
            "is the daemon running? (no ingest socket at {})",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut stream = stream;
    stream.write_all(format!("{body}\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(line.trim()).context("bad ingest response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_schema() {
        let ok = r#"{"v":1,"kind":"agent_needs_input","pane":"%1","source":"claude-code"}"#;
        assert!(serde_json::from_str::<AttentionEvent>(ok).is_ok());
        // Unknown fields are errors — the schema is a contract, not a hint.
        let extra = r#"{"v":1,"kind":"x","pane":"%1","source":"s","cmd":"revoke"}"#;
        assert!(serde_json::from_str::<AttentionEvent>(extra).is_err());
        let missing = r#"{"v":1,"kind":"agent_needs_input"}"#;
        assert!(serde_json::from_str::<AttentionEvent>(missing).is_err());
    }

    #[test]
    fn permission_schema_requires_tool_and_summary() {
        let ok = r#"{"v":1,"kind":"agent_permission","pane":"%1","source":"claude-code",
            "tool":"Bash","summary":"touch x"}"#;
        assert!(serde_json::from_str::<PermissionEvent>(ok).is_ok());
        // Missing required fields.
        let no_tool = r#"{"v":1,"kind":"agent_permission","pane":"%1","source":"s","summary":"x"}"#;
        assert!(serde_json::from_str::<PermissionEvent>(no_tool).is_err());
        // Unknown field rejected.
        let extra = r#"{"v":1,"kind":"agent_permission","pane":"%1","source":"s",
            "tool":"Bash","summary":"x","behavior":"allow"}"#;
        assert!(serde_json::from_str::<PermissionEvent>(extra).is_err());
    }

    #[test]
    fn sanitize_strips_and_caps() {
        assert_eq!(sanitize("a\x1b[31mb\nc"), "a[31mbc");
        assert_eq!(sanitize(&"x".repeat(1000)).len(), MAX_MESSAGE);
    }

    #[tokio::test]
    async fn eof_wins_the_tie_with_a_ready_decision() {
        use tokio::sync::oneshot;
        // The exact race `biased` exists for: the hook's socket is closed (the
        // Mac answered and Claude SIGTERM'd the hook) AND a phone decision is
        // already queued. EOF must win, or we'd write an allow/deny the hook
        // will never read while the Mac already decided.
        let (hook, server) = tokio::net::UnixStream::pair().unwrap();
        drop(hook); // hook gone
                    // Rigorously establish the tie: confirm the peer-close is
                    // *actually* readable as EOF (readable() alone can report a
                    // false positive) BEFORE the decision is made ready, so both
                    // select branches are genuinely ready. EOF is level-triggered,
                    // so a confirmed Ok(0) here means the real read will be too.
        loop {
            server.readable().await.unwrap();
            let mut probe = [0u8; 1];
            match server.try_read(&mut probe) {
                Ok(0) => break, // EOF confirmed
                Ok(_) => unreachable!("peer wrote nothing"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {} // spurious wake — retry
                Err(e) => panic!("unexpected: {e}"),
            }
        }
        let (server_read, _server_write) = server.into_split();

        let (tx, rx) = oneshot::channel();
        let (conf_tx, conf_rx) = oneshot::channel();
        tx.send((Decision::Allow, conf_tx)).unwrap(); // decision already ready

        let out = wait_for_decision(rx, server_read).await;
        assert!(out.is_none(), "EOF must win the tie over a ready decision");
        // The confirmation sender was dropped unfired → the deciding device sees
        // no delivery (409), never a false "approved".
        assert!(conf_rx.await.is_err());
    }

    #[test]
    fn prompt_id_shape() {
        assert!(valid_prompt_id("9bf86345-4606-4072-86e8-c3a969332e11"));
        assert!(!valid_prompt_id(""));
        assert!(!valid_prompt_id(&"a".repeat(65)));
        assert!(!valid_prompt_id("has space"));
        assert!(!valid_prompt_id("semi;colon"));
    }

    #[test]
    fn rate_limiter_resets_after_window() {
        let mut rl = RateLimiter::new();
        for _ in 0..RATE_MAX {
            assert!(rl.allow());
        }
        assert!(!rl.allow());
        rl.window_start = Instant::now() - RATE_WINDOW;
        assert!(rl.allow());
    }
}
