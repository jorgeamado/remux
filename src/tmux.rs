//! The only module that knows tmux exists.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Socket name override so tests can run against an isolated tmux server.
fn socket_name() -> Option<String> {
    std::env::var("REMUX_TMUX_SOCKET")
        .ok()
        .filter(|s| !s.is_empty())
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

/// tmux itself forbids `:` and `.` in session names (they delimit targets);
/// we additionally reject control characters and unreasonable lengths. Any
/// real session listed by tmux therefore passes and can be attached.
pub fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.chars().any(|c| c == ':' || c == '.' || c.is_control())
}

#[derive(serde::Serialize, Debug, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    pub windows: u32,
    pub attached: u32,
    pub activity: u64,
}

/// All sessions on the tmux server. Empty when no server is running yet.
/// NB: tmux 3.3a replaces control characters (tabs included) in expanded
/// formats with `_`, so fields are space-separated and parsed from the right
/// (the name may contain spaces; the numeric fields cannot).
pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    let mut cmd = tmux();
    cmd.args([
        "list-sessions",
        "-F",
        "#{session_name} #{session_windows} #{session_attached} #{session_activity}",
    ]);
    let Ok(out) = run(cmd) else {
        return Ok(Vec::new()); // no tmux server running
    };
    Ok(out.lines().filter_map(parse_session_line).collect())
}

fn parse_session_line(line: &str) -> Option<SessionInfo> {
    let mut f = line.rsplitn(4, ' ');
    let activity = f.next()?.parse().ok()?;
    let attached = f.next()?.parse().ok()?;
    let windows = f.next()?.parse().ok()?;
    Some(SessionInfo {
        name: f.next()?.to_string(),
        windows,
        attached,
        activity,
    })
}

/// Latest content-activity timestamp per session, from one `list-windows -a`
/// call. NB: `session_activity` tracks *client* activity (input/attach), so
/// window activity is the right signal for "output happened".
pub fn sessions_activity() -> Result<Vec<(String, u64)>> {
    let mut cmd = tmux();
    cmd.args([
        "list-windows",
        "-a",
        "-F",
        "#{session_name} #{window_activity}",
    ]);
    let Ok(out) = run(cmd) else {
        return Ok(Vec::new()); // no tmux server running
    };
    let mut latest: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for line in out.lines() {
        if let Some((name, activity)) = parse_window_activity_line(line) {
            let entry = latest.entry(name).or_default();
            *entry = (*entry).max(activity);
        }
    }
    Ok(latest.into_iter().collect())
}

/// `<session name (may contain spaces)> <activity unix ts>`
fn parse_window_activity_line(line: &str) -> Option<(String, u64)> {
    let mut f = line.rsplitn(2, ' ');
    let activity = f.next()?.parse().ok()?;
    Some((f.next()?.to_string(), activity))
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
    fn session_line_parsing() {
        assert_eq!(
            parse_session_line("main 2 1 1783793484"),
            Some(SessionInfo {
                name: "main".into(),
                windows: 2,
                attached: 1,
                activity: 1783793484,
            })
        );
        // Session names may contain spaces; numeric fields parse from the right.
        assert_eq!(
            parse_session_line("my session 12 0 99").unwrap().name,
            "my session"
        );
        assert_eq!(parse_session_line("garbage"), None);
        assert_eq!(parse_session_line(""), None);
    }

    #[test]
    fn window_activity_line_parsing() {
        assert_eq!(
            parse_window_activity_line("main 1783793484"),
            Some(("main".into(), 1783793484))
        );
        assert_eq!(
            parse_window_activity_line("my session 99"),
            Some(("my session".into(), 99))
        );
        assert_eq!(parse_window_activity_line("garbage"), None);
    }

    #[test]
    fn session_name_validation() {
        assert!(valid_session_name("main"));
        assert!(valid_session_name("work-2_a"));
        assert!(valid_session_name("my session")); // spaces are legal in tmux
        assert!(!valid_session_name(""));
        assert!(!valid_session_name("a:b")); // target separator
        assert!(!valid_session_name("a.b")); // target separator
        assert!(!valid_session_name("a\x1bb")); // control chars
        assert!(!valid_session_name("../etc"));
        assert!(!valid_session_name(&"x".repeat(65)));
    }

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
