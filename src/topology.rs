//! Control-mode metadata client (M3a). One persistent read-only tmux control
//! client per daemon watches for structural changes (windows/panes/sessions
//! created, renamed, killed) and publishes a fresh topology snapshot to a
//! watch channel that every websocket forwards to its client.
//!
//! Design (validated by the M3a spike on tmux 3.3a):
//! - Additive and metadata-only: the per-connection PTY attach remains the
//!   byte path. Losing this client only degrades (tabs stop updating); the
//!   terminal stream is unaffected.
//! - `read-only,no-output,ignore-size`: no window-size influence, no %output.
//! - Notifications are treated as dirty-bits only — on any `%…` line we
//!   re-list the whole server (`capture_topology`) rather than parse events
//!   incrementally (incremental parsing misses non-attached sessions and is
//!   the %begin/%end minefield the design review warned about).
//! - Control mode exits on stdin EOF, so we hold stdin open for the client's
//!   lifetime. On exit (tmux server gone / session killed) we respawn with
//!   backoff after re-ensuring the session.

use crate::{tmux, App};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub type Snapshot = Arc<Vec<tmux::SessionWindows>>;

/// Coalesce a burst of control-mode notifications: publish once the client has
/// been idle this long after the last event.
const DEBOUNCE: Duration = Duration::from_millis(150);
const RESPAWN_BACKOFF: Duration = Duration::from_secs(2);

pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            let session = app.args.session.clone();
            // The control client attaches to a session, so it must exist.
            let _ = tokio::task::spawn_blocking({
                let s = session.clone();
                move || tmux::ensure_session(&s)
            })
            .await;

            publish(&app).await; // fresh snapshot on (re)start

            if let Err(e) = run_client(&app, &session).await {
                tracing::debug!("topology control client ended: {e:#}");
            }
            tokio::time::sleep(RESPAWN_BACKOFF).await;
        }
    });
}

async fn run_client(app: &Arc<App>, session: &str) -> anyhow::Result<()> {
    let mut cmd = Command::new("tmux");
    cmd.args(tmux::control_attach_args(session))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    // Hold stdin open: control mode exits on EOF.
    let _stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("control client has no stdout"))?;
    let mut lines = BufReader::new(stdout).lines();

    tracing::info!("topology control client attached");
    let mut dirty = false;
    loop {
        match tokio::time::timeout(DEBOUNCE, lines.next_line()).await {
            // A control-mode notification line — mark dirty, keep draining.
            Ok(Ok(Some(line))) => {
                if line.starts_with('%') {
                    dirty = true;
                }
            }
            // EOF: the client exited (server/session gone). Let the supervisor
            // re-ensure the session and respawn.
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(e.into()),
            // Idle: the burst settled — rebuild and publish if anything changed.
            Err(_) => {
                if dirty {
                    dirty = false;
                    publish(app).await;
                }
            }
        }
    }
    let _ = child.kill().await;
    Ok(())
}

async fn publish(app: &Arc<App>) {
    if let Ok(Ok(snap)) = tokio::task::spawn_blocking(tmux::capture_topology).await {
        // watch::send only errors if all receivers dropped — harmless.
        let _ = app.topology.send(Arc::new(snap));
    }
}
