//! Local admin interface: a line-JSON protocol over a Unix socket in the
//! state dir. Filesystem permissions (0600) are the authentication — this
//! never rides the network listener. `remux pair` uses it today; device
//! management commands (M2) extend the same protocol.

use crate::App;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    /// Mint a pairing token; responds with the full pairing URL.
    Pair,
    /// List paired devices.
    Devices,
    /// Revoke a device: token invalid + sockets closed + push subscriptions
    /// deleted + all outstanding pairing tokens cancelled.
    Revoke {
        id: String,
    },
    Rename {
        id: String,
        name: String,
    },
}

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("admin.sock")
}

/// Peer credential check: only the uid that owns the socket (the daemon's
/// own user) may drive admin commands. Defence in depth beyond 0600 — a
/// leaked/inherited fd from another user is still rejected.
fn peer_allowed(stream: &UnixStream, owner_uid: u32) -> bool {
    match stream.peer_cred() {
        Ok(cred) => cred.uid() == owner_uid,
        Err(_) => false,
    }
}

pub fn spawn(app: Arc<App>, state_dir: &Path) -> Result<()> {
    let path = socket_path(state_dir);
    // Never unlink a *live* socket: a second `remux serve` would silently
    // orphan the running daemon's `remux pair` before dying on the busy
    // HTTP port. Probe first — connectable means a daemon is alive.
    match std::os::unix::net::UnixStream::connect(&path) {
        Ok(_) => bail!(
            "another remux daemon is already running (admin socket {} is live)",
            path.display()
        ),
        Err(_) => {
            let _ = std::fs::remove_file(&path); // stale leftover, safe to clear
        }
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind admin socket {}", path.display()))?;
    let owner_uid = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        std::fs::metadata(&path)?.uid()
    };
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            if !peer_allowed(&stream, owner_uid) {
                tracing::warn!("admin socket: rejected connection from another uid");
                continue;
            }
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, app).await {
                    tracing::debug!("admin request failed: {e:#}");
                }
            });
        }
    });
    Ok(())
}

async fn handle(stream: UnixStream, app: Arc<App>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let response = match serde_json::from_str::<Request>(line.trim()) {
        Ok(Request::Pair) => {
            let token = app.auth.new_pairing_token();
            tracing::info!("pairing token minted via admin socket");
            serde_json::json!({
                "ok": true,
                "url": format!("{}/#pair={token}", app.public_url),
            })
        }
        Ok(Request::Devices) => serde_json::json!({
            "ok": true,
            "devices": app.auth.devices(),
        }),
        Ok(Request::Revoke { id }) => match app.auth.revoke(&id) {
            Ok(()) => {
                app.push.remove_device(&id);
                let _ = app.revoked.send(id.clone());
                tracing::info!(device = %id, "device revoked via admin socket");
                serde_json::json!({ "ok": true })
            }
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
        },
        Ok(Request::Rename { id, name }) => {
            if app.auth.rename(&id, &name) {
                serde_json::json!({ "ok": true })
            } else {
                serde_json::json!({ "ok": false, "error": "no such device" })
            }
        }
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    };
    write.write_all(format!("{response}\n").as_bytes()).await?;
    Ok(())
}

/// CLI side: one line-JSON request, one response.
pub fn request(state_dir: &Path, body: serde_json::Value) -> Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let path = socket_path(state_dir);
    let mut stream = std::os::unix::net::UnixStream::connect(&path).with_context(|| {
        format!(
            "is the daemon running? (no admin socket at {})",
            path.display()
        )
    })?;
    stream.write_all(format!("{body}\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).context("bad admin response")?;
    if v["ok"] != serde_json::json!(true) {
        bail!("daemon refused: {}", v["error"]);
    }
    Ok(v)
}

/// CLI side (`remux pair`): ask the running daemon for a pairing URL.
pub fn request_pairing(state_dir: &Path) -> Result<String> {
    let v = request(state_dir, serde_json::json!({"cmd": "pair"}))?;
    v["url"]
        .as_str()
        .map(str::to_string)
        .context("admin response missing url")
}
