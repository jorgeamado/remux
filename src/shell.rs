//! M4c shell-event socket: a `SOCK_DGRAM` Unix socket, **separate** from the
//! agent ingest socket, carrying high-volume shell command events into the
//! command feed.
//!
//! Why a separate datagram socket (Codex review):
//! - **Non-blocking for the shell.** A datagram `send_to` returns immediately
//!   whatever the daemon's state — a shell hook on every prompt must never
//!   stall. A dead/wedged daemon simply drops the event.
//! - **Isolated budget.** A busy build session firing hundreds of events must
//!   never starve the *actionable* `agent_permission` events on the other
//!   socket. This socket has its own lossy budget; overflow is dropped.
//!
//! 0600 perms are the authentication: a datagram socket at 0600 already
//! excludes other uids, and these events are informational-only (forgeable by
//! any same-uid process, so they may never trigger an action — at worst they
//! pollute the feed). No peer-uid check is needed at this trust level.

use crate::App;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UnixDatagram;

/// Datagrams larger than this are truncated on receive and fail to parse.
const MAX_DGRAM: usize = 4096;
const MAX_SOURCE: usize = 32;
const MAX_SHELL_ID: usize = 64;
/// Lossy budget, isolated from the agent socket. Generous for shells; overflow
/// drops silently (informational data).
const RATE_MAX: u32 = 600;
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Sweep cadence: mark stale-running / evict aged feed entries.
const SWEEP_EVERY: Duration = Duration::from_secs(300);

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("shell.sock")
}

#[derive(Deserialize)]
struct Envelope {
    v: u32,
    kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartEvent {
    #[allow(dead_code)]
    v: u32,
    #[allow(dead_code)]
    kind: String,
    pane: String,
    source: String,
    shell_id: String,
    command_id: u64,
    command: String,
    #[serde(default)]
    cwd: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishEvent {
    #[allow(dead_code)]
    v: u32,
    #[allow(dead_code)]
    kind: String,
    source: String,
    shell_id: String,
    command_id: u64,
    exit: i32,
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

fn sane_id(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn spawn(app: Arc<App>, state_dir: &Path) -> Result<()> {
    let path = socket_path(state_dir);
    // Admin's live-probe is the single-instance guard (admin spawns first); a
    // leftover shell socket here is stale.
    let _ = std::fs::remove_file(&path);
    let sock = UnixDatagram::bind(&path)
        .with_context(|| format!("bind shell socket {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    let mut limiter = RateLimiter::new();
    let recv_app = app.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DGRAM];
        loop {
            let n = match sock.recv(&mut buf).await {
                Ok(n) => n,
                Err(_) => continue,
            };
            if n == 0 || !limiter.allow() {
                continue; // over budget or empty → drop (lossy by design)
            }
            process(&recv_app, &buf[..n]);
        }
    });
    // Timer sweeper: nothing else wakes stale-running / aged entries.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            app.feed.sweep();
        }
    });
    Ok(())
}

fn process(app: &App, bytes: &[u8]) {
    let Ok(line) = std::str::from_utf8(bytes) else {
        return;
    };
    let line = line.trim();
    let kind = match serde_json::from_str::<Envelope>(line) {
        Ok(e) if e.v == 1 => e.kind,
        _ => return,
    };
    match kind.as_str() {
        "command_started" => {
            let Ok(ev) = serde_json::from_str::<StartEvent>(line) else {
                return;
            };
            if !crate::ingest::valid_pane(&ev.pane)
                || !sane_id(&ev.source, MAX_SOURCE)
                || !sane_id(&ev.shell_id, MAX_SHELL_ID)
            {
                return;
            }
            let Ok(session) = crate::ingest::sessions_of_pane(app, &ev.pane) else {
                return; // unknown or ambiguous pane
            };
            // start() returns Some when a finish had raced ahead of it — run the
            // same finish handling so a failed command whose finish arrived
            // first still resets the detector and notifies.
            if let Some(fin) = app.feed.start(
                &session,
                &ev.pane,
                &ev.shell_id,
                ev.command_id,
                &ev.command,
                &ev.cwd,
            ) {
                on_finished(app, fin);
            }
        }
        "command_finished" => {
            let Ok(ev) = serde_json::from_str::<FinishEvent>(line) else {
                return;
            };
            if !sane_id(&ev.source, MAX_SOURCE) || !sane_id(&ev.shell_id, MAX_SHELL_ID) {
                return;
            }
            // The match carries the session; pane isn't needed on finish.
            if let Some(fin) = app.feed.finish(&ev.shell_id, ev.command_id, ev.exit) {
                on_finished(app, fin);
            }
        }
        _ => {}
    }
}

/// React to a matched command finish (whether it arrived in order, or as a
/// finish that raced ahead of its start and was applied when the start landed).
fn on_finished(app: &App, fin: crate::feed::Finished) {
    // Every matched finish consumes this session's busy→quiet epoch so the
    // heuristic doesn't also fire for the same command.
    let _ = app.detector_reset.send(fin.session.clone());
    // Notable finishes (a failure, or a long-running command) raise a precise
    // attention. Secrets-safe by construction: the text is built ONLY from exit
    // + duration + session — never the command, which the service worker would
    // render on the lock screen.
    if fin.exit != 0 || fin.elapsed_ms >= LONG_MS {
        let _ = app.attention.send(crate::Attention {
            session: fin.session,
            kind: "command_finished".into(),
            pane: None,
            reason: Some(notable_reason(fin.exit, fin.elapsed_ms)),
            source: Some("shell".into()),
        });
    }
}

/// A command finish is worth a notification if it failed or ran at least this
/// long (a quick success is silent).
const LONG_MS: u64 = 30_000;

/// Secrets-safe notification text: exit + duration only, never the command.
fn notable_reason(exit: i32, elapsed_ms: u64) -> String {
    let dur = human_duration(elapsed_ms);
    if exit != 0 {
        format!("failed ({exit}) after {dur}")
    } else {
        format!("took {dur}")
    }
}

fn human_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// Client side: fire-and-forget a shell event. Non-blocking — a nonblocking
/// datagram `send_to` returns immediately regardless of the daemon's state, so
/// a shell hook can call this on every prompt without ever stalling. All errors
/// (no daemon, full buffer) are ignored: the event is simply lost.
pub fn emit(state_dir: &Path, body: &serde_json::Value) {
    let path = socket_path(state_dir);
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else {
        return;
    };
    let _ = sock.set_nonblocking(true);
    let _ = sock.send_to(body.to_string().as_bytes(), &path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_schema_is_strict() {
        let ok = r#"{"v":1,"kind":"command_started","pane":"%1","source":"shell",
            "shell_id":"abc","command_id":3,"command":"ls","cwd":"/w"}"#;
        assert!(serde_json::from_str::<StartEvent>(ok).is_ok());
        let extra = r#"{"v":1,"kind":"command_started","pane":"%1","source":"s",
            "shell_id":"a","command_id":1,"command":"ls","cwd":"/w","x":1}"#;
        assert!(serde_json::from_str::<StartEvent>(extra).is_err());
    }

    #[test]
    fn finish_schema_is_strict() {
        let ok = r#"{"v":1,"kind":"command_finished","source":"shell",
            "shell_id":"abc","command_id":3,"exit":0}"#;
        assert!(serde_json::from_str::<FinishEvent>(ok).is_ok());
        let missing = r#"{"v":1,"kind":"command_finished","shell_id":"a","command_id":1}"#;
        assert!(serde_json::from_str::<FinishEvent>(missing).is_err());
    }

    #[test]
    fn sane_id_rejects_junk() {
        assert!(sane_id("shell-42_x", 64));
        assert!(!sane_id("", 64));
        assert!(!sane_id("has space", 64));
        assert!(!sane_id("semi;colon", 64));
        assert!(!sane_id(&"a".repeat(65), 64));
    }

    #[test]
    fn notable_reason_never_contains_a_command() {
        // Only exit + duration; a failing command and a long success.
        assert_eq!(notable_reason(101, 245_000), "failed (101) after 4m");
        assert_eq!(notable_reason(0, 45_000), "took 45s");
        assert_eq!(notable_reason(1, 7_400_000), "failed (1) after 2h3m");
    }

    #[test]
    fn human_duration_buckets() {
        assert_eq!(human_duration(5_000), "5s");
        assert_eq!(human_duration(90_000), "1m");
        assert_eq!(human_duration(3_600_000), "1h0m");
    }

    #[test]
    fn rate_limiter_caps_then_resets() {
        let mut r = RateLimiter::new();
        for _ in 0..RATE_MAX {
            assert!(r.allow());
        }
        assert!(!r.allow());
        r.window_start = Instant::now() - RATE_WINDOW;
        assert!(r.allow());
    }
}
