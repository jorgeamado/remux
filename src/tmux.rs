//! The only module that knows tmux exists.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Socket name override so tests can run against an isolated tmux server.
fn socket_name() -> Option<String> {
    std::env::var("REMUX_TMUX_SOCKET").ok().filter(|s| !s.is_empty())
}

fn tmux() -> Command {
    let mut cmd = Command::new("tmux");
    if let Some(sock) = socket_name() {
        cmd.args(["-L", &sock]);
    }
    cmd
}

fn run(mut cmd: Command) -> Result<String> {
    let out = cmd.output().context("failed to run tmux")?;
    if !out.status.success() {
        bail!(
            "tmux {:?} failed: {}",
            cmd.get_args().collect::<Vec<_>>(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Create the managed session if missing and apply session-scoped options.
/// Never touches global tmux configuration.
pub fn ensure_session(session: &str) -> Result<()> {
    let exists = tmux()
        .args(["has-session", "-t", &format!("={session}")])
        .output()
        .context("is tmux installed?")?
        .status
        .success();
    if !exists {
        let mut cmd = tmux();
        cmd.args(["new-session", "-d", "-s", session]);
        run(cmd)?;
    }
    // Latest active client drives pane size: this is what makes Mac<->phone
    // handoff work with zero daemon logic. window-size is a *window* option;
    // "session:" targets the session's current window. New windows fall back
    // to the global default, which is already "latest" since tmux 3.1.
    // NB: set-option does not accept the "=" exact-match prefix (tmux 3.3a).
    let mut cmd = tmux();
    cmd.args([
        "set-option",
        "-w",
        "-t",
        &format!("{session}:"),
        "window-size",
        "latest",
    ]);
    run(cmd)?;
    // Mouse on so touch/wheel scrolling reaches tmux copy-mode (V1 scrollback).
    let mut cmd = tmux();
    cmd.args(["set-option", "-t", session, "mouse", "on"]);
    run(cmd)?;
    Ok(())
}

/// Command line for the per-connection attach client, spawned inside a PTY.
/// Starts as an observer: not participating in window sizing.
///
/// NB: we deliberately do NOT use the tmux `read-only` client flag — in tmux
/// 3.3a it cannot be cleared again via `refresh-client -f '!read-only'`, which
/// would make take-control impossible. Observer input is enforced by the
/// daemon instead: observer bytes are never written to this PTY.
pub fn attach_args(session: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(sock) = socket_name() {
        args.push("-L".into());
        args.push(sock);
    }
    args.extend([
        "-u".into(),
        "attach-session".into(),
        "-t".into(),
        format!("={session}"),
        "-f".into(),
        "ignore-size".into(),
    ]);
    args
}

/// Find the tmux client name (its tty) for the attach process we spawned.
pub fn client_name_for_pid(pid: u32) -> Result<Option<String>> {
    let mut cmd = tmux();
    cmd.args(["list-clients", "-F", "#{client_pid} #{client_name}"]);
    let out = run(cmd)?;
    tracing::trace!(pid, clients = ?out.trim(), "resolving tmux client");
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(p), Some(name)) = (parts.next(), parts.next()) {
            if p == pid.to_string() {
                return Ok(Some(name.to_string()));
            }
        }
    }
    Ok(None)
}

/// Unix timestamp (seconds) of the most recent content change in any window
/// of the session, from tmux's own activity tracking. None when the session
/// does not exist (yet).
pub fn last_activity(session: &str) -> Result<Option<u64>> {
    let mut cmd = tmux();
    cmd.args([
        "list-windows",
        "-t",
        &format!("={session}"),
        "-F",
        "#{window_activity}",
    ]);
    match run(cmd) {
        Ok(out) => Ok(out.lines().filter_map(|l| l.trim().parse().ok()).max()),
        Err(_) => Ok(None),
    }
}

/// Promote our attach client to controller (drives window size).
pub fn promote_client(client: &str) -> Result<()> {
    let mut cmd = tmux();
    cmd.args(["refresh-client", "-t", client, "-f", "!ignore-size"]);
    run(cmd)?;
    Ok(())
}

/// Demote our attach client back to observer (stops driving window size).
pub fn demote_client(client: &str) -> Result<()> {
    let mut cmd = tmux();
    cmd.args(["refresh-client", "-t", client, "-f", "ignore-size"]);
    run(cmd)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_args_shape() {
        std::env::remove_var("REMUX_TMUX_SOCKET");
        let args = attach_args("main");
        assert_eq!(
            args,
            vec!["-u", "attach-session", "-t", "=main", "-f", "ignore-size"]
        );
    }
}
