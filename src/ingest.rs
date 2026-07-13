//! Local ingest interface (M4a): a Unix socket in the state dir where hook
//! scripts report semantic events — "Claude Code is waiting for input in
//! pane %3". Same filesystem authentication as the admin socket (0600 +
//! peer-uid), but a deliberately separate surface: admin accepts *commands*
//! from the owner, ingest accepts *data* from same-uid hook processes. An
//! ingest event can raise attention; it can never act.
//!
//! Protocol: one JSON line in, one JSON ack out, connection closed. The
//! schema is strict (unknown fields and kinds are errors, fields are
//! length-capped) and the socket is rate-limited — a confused or hostile
//! same-uid process can at worst make notifications noisy, and even that
//! is bounded.

use crate::App;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Cap on the bytes read from one connection; a document that large without
/// its newline inside the cap fails parsing. Events are tiny; anything
/// bigger is a bug or an abuse attempt.
const MAX_LINE: u64 = 4096;
const MAX_SOURCE: usize = 32;
const MAX_MESSAGE: usize = 256;
const MAX_PANE: usize = 16;
/// Rate limit: events *offered* per window, across all producers — charged
/// before parsing, deliberately: malformed floods must not get free parse
/// work, at the cost that one confused producer can starve the window.
const RATE_MAX: u32 = 60;
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Concurrent connections; excess connects are dropped (hooks fail fast and
/// exit non-zero — never queue a slow client).
const MAX_CONNS: usize = 16;
/// A connection must deliver its line and take its ack within this budget.
const CONN_TIMEOUT: Duration = Duration::from_secs(5);

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ingest.sock")
}

/// Wire format. A plain struct + `kind` match (not an internally-tagged
/// enum) so `deny_unknown_fields` applies reliably.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    v: u32,
    kind: String,
    /// tmux pane id (`%N`) — `$TMUX_PANE` in the producer's environment.
    pane: String,
    /// Producer label ("claude-code", "shell", …). Informational.
    source: String,
    /// Optional human-readable detail. Informational; sanitized and capped.
    #[serde(default)]
    message: Option<String>,
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
            // Slow-client defence: bounded concurrency + a hard per-connection
            // deadline. Excess or stalled producers are dropped, not queued.
            let Ok(permit) = conns.clone().try_acquire_owned() else {
                tracing::warn!("ingest socket: connection limit reached, dropping");
                continue;
            };
            let app = app.clone();
            let limiter = limiter.clone();
            tokio::spawn(async move {
                let _permit = permit;
                match tokio::time::timeout(CONN_TIMEOUT, handle(stream, app, limiter)).await {
                    Ok(Err(e)) => tracing::debug!("ingest event failed: {e:#}"),
                    Err(_) => tracing::debug!("ingest connection timed out"),
                    Ok(Ok(())) => {}
                }
            });
        }
    });
    Ok(())
}

async fn handle(
    stream: UnixStream,
    app: Arc<App>,
    limiter: Arc<std::sync::Mutex<RateLimiter>>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    // take() caps how much one connection can feed us; a line that hits the
    // cap can't have a trailing newline and fails parsing below.
    let mut reader = BufReader::new(read).take(MAX_LINE);
    reader.read_line(&mut line).await?;
    let response = if !limiter.lock().unwrap().allow() {
        serde_json::json!({ "ok": false, "error": "rate limited" })
    } else {
        match serde_json::from_str::<Event>(line.trim()) {
            Ok(ev) => process(&app, ev),
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
        }
    };
    write.write_all(format!("{response}\n").as_bytes()).await?;
    Ok(())
}

fn process(app: &App, ev: Event) -> serde_json::Value {
    if ev.v != 1 {
        return serde_json::json!({ "ok": false, "error": "unsupported version" });
    }
    if ev.source.is_empty() || ev.source.len() > MAX_SOURCE {
        return serde_json::json!({ "ok": false, "error": "bad source" });
    }
    if !valid_pane(&ev.pane) {
        return serde_json::json!({ "ok": false, "error": "bad pane id (want %N)" });
    }
    let message = ev.message.as_deref().map(sanitize).unwrap_or_default();
    // All producer strings are same-uid-controlled; sanitize before logging.
    let source = sanitize(&ev.source);
    match ev.kind.as_str() {
        "agent_needs_input" => {
            let session = match sessions_of_pane(app, &ev.pane).as_slice() {
                [] => return serde_json::json!({ "ok": false, "error": "unknown pane" }),
                [one] => one.clone(),
                // Linked windows: the same pane can live in several sessions.
                // Guessing the wrong one would notify for the wrong session.
                _ => {
                    return serde_json::json!({ "ok": false,
                        "error": "pane is linked into multiple sessions" })
                }
            };
            tracing::info!(
                session = %session, pane = %ev.pane, source = %source,
                message = %message, "ingest: agent needs input"
            );
            // The existing attention pipeline does the rest: push dispatch
            // (with its suppression rules), in-band ws frames, and the
            // /api/attention deep link.
            let _ = app.attention.send(session.clone());
            serde_json::json!({ "ok": true, "session": session })
        }
        _ => serde_json::json!({ "ok": false, "error": "unknown kind" }),
    }
}

/// Terminal-controlled text: strip control chars, cap length.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_MESSAGE)
        .collect()
}

fn valid_pane(s: &str) -> bool {
    s.len() >= 2
        && s.len() <= MAX_PANE
        && s.starts_with('%')
        && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Sessions containing pane `%N` per the latest topology snapshot. More than
/// one is possible with linked windows.
fn sessions_of_pane(app: &App, pane: &str) -> Vec<String> {
    let snap = app.topology.borrow();
    snap.iter()
        .filter(|s| {
            s.windows
                .iter()
                .any(|w| w.panes.iter().any(|p| p.id == pane))
        })
        .map(|s| s.name.clone())
        .collect()
}

/// CLI side (`remux emit`): one line-JSON event, one ack. Deadlined — a
/// wedged daemon must fail the hook fast, not hang it (connect on a local
/// Unix socket never blocks meaningfully; read/write can).
pub fn request(state_dir: &Path, body: serde_json::Value) -> Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let path = socket_path(state_dir);
    let stream = std::os::unix::net::UnixStream::connect(&path).with_context(|| {
        format!(
            "is the daemon running? (no ingest socket at {})",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(CONN_TIMEOUT))?;
    stream.set_write_timeout(Some(CONN_TIMEOUT))?;
    let mut stream = stream;
    stream.write_all(format!("{body}\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).context("bad ingest response")?;
    if v["ok"] != serde_json::json!(true) {
        anyhow::bail!("daemon refused event: {}", v["error"]);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_schema() {
        let ok = r#"{"v":1,"kind":"agent_needs_input","pane":"%1","source":"claude-code"}"#;
        assert!(serde_json::from_str::<Event>(ok).is_ok());
        // Unknown fields are errors — the schema is a contract, not a hint.
        let extra = r#"{"v":1,"kind":"x","pane":"%1","source":"s","cmd":"revoke"}"#;
        assert!(serde_json::from_str::<Event>(extra).is_err());
        let missing = r#"{"v":1,"kind":"agent_needs_input"}"#;
        assert!(serde_json::from_str::<Event>(missing).is_err());
    }

    #[test]
    fn sanitize_strips_and_caps() {
        assert_eq!(sanitize("a\x1b[31mb\nc"), "a[31mbc");
        assert_eq!(sanitize(&"x".repeat(1000)).len(), MAX_MESSAGE);
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
