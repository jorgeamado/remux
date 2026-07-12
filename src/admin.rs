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
}

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("admin.sock")
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
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
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
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    };
    write.write_all(format!("{response}\n").as_bytes()).await?;
    Ok(())
}

/// CLI side (`remux pair`): ask the running daemon for a pairing URL.
pub fn request_pairing(state_dir: &Path) -> Result<String> {
    use std::io::{BufRead, BufReader, Write};
    let path = socket_path(state_dir);
    let mut stream = std::os::unix::net::UnixStream::connect(&path).with_context(|| {
        format!(
            "is the daemon running? (no admin socket at {})",
            path.display()
        )
    })?;
    stream.write_all(b"{\"cmd\":\"pair\"}\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).context("bad admin response")?;
    if v["ok"] != serde_json::json!(true) {
        bail!("daemon refused: {}", v["error"]);
    }
    v["url"]
        .as_str()
        .map(str::to_string)
        .context("admin response missing url")
}
