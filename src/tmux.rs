//! The only module that knows tmux exists.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Socket name override so tests can run against an isolated tmux server.
/// Public so the async control-mode client (topology.rs) can target the same
/// server without duplicating the env convention.
pub fn socket_name() -> Option<String> {
    std::env::var("REMUX_TMUX_SOCKET")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Strip control characters and cap length. Window/pane names are set from
/// terminal output (OSC titles, automatic-rename) — hostile content must not
/// reach clients as-is even though the client also renders them as text.
fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(64).collect()
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

/// Does this tmux stderr mean "no server is running yet" (a benign, expected
/// state → empty result) rather than an operational failure (missing binary,
/// bad flags, protocol change)? Only benign cases may be mapped to empty; every
/// other failure must surface, or we'd publish a plausible-but-false empty
/// topology and skip pushes while someone is active (Codex).
fn is_no_server(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    // The canonical message, OR the socket-connect error *specifically because
    // the socket file is absent*. We require BOTH parts of the latter so an
    // "error connecting to … (Permission denied)" — an operational failure —
    // is NOT swallowed as no-server (Codex).
    s.contains("no server running")
        || (s.contains("error connecting to") && s.contains("no such file or directory"))
}

/// A *scoped* query (`-t session[:window]`) can also fail benignly because the
/// target vanished (killed, or never existed) — that's an empty result, not an
/// operational error. Server-wide queries never see this.
fn is_missing_target(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("can't find")
}

/// Like [`run`], but maps benign absence to `Ok(None)`. `tolerate_missing`
/// additionally treats "can't find <target>" as absence, for session/window
/// scoped queries. Any other non-zero exit stays `Err`.
fn run_classified(mut cmd: Command, tolerate_missing: bool) -> Result<Option<String>> {
    let out = cmd.output().context("failed to run tmux")?;
    if out.status.success() {
        return Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if is_no_server(&stderr) || (tolerate_missing && is_missing_target(&stderr)) {
        return Ok(None);
    }
    bail!(
        "tmux {:?} failed: {}",
        cmd.get_args().collect::<Vec<_>>(),
        stderr.trim()
    );
}

/// Server-wide query: only "no server" is benign.
fn run_optional(cmd: Command) -> Result<Option<String>> {
    run_classified(cmd, false)
}

/// Capture the *rendered* visible screen of a pane (`capture-pane -p`), as plain
/// text (no escapes). `Ok(None)` if the server or the pane is gone. This is how
/// the pane-view capture adapter reads a real tool's output (e.g. htop) without
/// touching the running program.
pub fn capture_pane(pane: &str) -> Result<Option<String>> {
    let mut cmd = tmux();
    cmd.args(["capture-pane", "-p", "-t", pane]);
    run_classified(cmd, true)
}

/// Capture a pane's visible screen plus up to `back` lines of scrollback, as
/// plain text with wrapped lines joined (`-J`, so a wrapped command/URL is one
/// line the phone can copy cleanly). For the copy overlay. `Ok(None)` if the
/// pane/server is gone. tmux clamps `back` to the available history.
pub fn capture_scrollback(pane: &str, back: u32) -> Result<Option<String>> {
    let mut cmd = tmux();
    cmd.args([
        "capture-pane",
        "-p",
        "-J",
        "-t",
        pane,
        "-S",
        &format!("-{back}"),
    ]);
    run_classified(cmd, true)
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
        if run(cmd).is_err() {
            // Race: another caller (e.g. the topology supervisor and a ws
            // handler both starting up) may have created it between our
            // has-session check and now. Tolerate if it exists now.
            let now_exists = tmux()
                .args(["has-session", "-t", &format!("={session}")])
                .output()?
                .status
                .success();
            if !now_exists {
                bail!("failed to create tmux session {session:?}");
            }
        }
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

/// Send literal key(s) to a pane — for whitelisted dashboard actions. Literal
/// (`-l`) so the string is never interpreted as a tmux key-name.
pub fn send_keys(pane: &str, keys: &str) -> Result<()> {
    let mut c = tmux();
    c.args(["send-keys", "-t", pane, "-l", keys]);
    run_classified(c, true)?;
    Ok(())
}

/// Send named keys to a pane (e.g. `F5`, `BSpace`, `Enter`) — NOT literal, so
/// tmux resolves each to the real key. Several in one call to keep it cheap.
pub fn send_named(pane: &str, keys: &[&str]) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut c = tmux();
    c.args(["send-keys", "-t", pane]);
    c.args(keys);
    run_classified(c, true)?;
    Ok(())
}

/// All panes whose *current* foreground command is one of `tools`, server-wide.
/// A fresh poll — unlike topology's cached `pane_current_command`, which is not
/// refreshed on a foreground-process change, so it can't be trusted to detect a
/// tool starting/exiting.
pub fn panes_running(tools: &[&str]) -> Result<std::collections::HashSet<String>> {
    let mut c = tmux();
    c.args([
        "list-panes",
        "-a",
        "-F",
        "#{pane_id} #{pane_current_command}",
    ]);
    let out = run_optional(c)?.unwrap_or_default();
    Ok(out
        .lines()
        .filter_map(|l| {
            let (id, cmd) = l.split_once(' ')?;
            tools.contains(&cmd).then(|| id.to_string())
        })
        .collect())
}

/// The current foreground command of a pane (freshly polled), or `None` if the
/// pane/server is gone. Used to re-verify a tool owns the pane before an action.
pub fn pane_command(pane: &str) -> Option<String> {
    let mut c = tmux();
    c.args([
        "display-message",
        "-p",
        "-t",
        pane,
        "#{pane_current_command}",
    ]);
    run_classified(c, true)
        .ok()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The window id (`@N`) a pane belongs to — for window-scoped options.
pub fn window_of_pane(pane: &str) -> Result<Option<String>> {
    let mut c = tmux();
    c.args(["display-message", "-p", "-t", pane, "#{window_id}"]);
    Ok(run_classified(c, true)?
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

/// Press-relevant flags for a pane in a session's current window — the
/// window every attached client displays (window selection is per-session,
/// so a client's view is always the session's active window).
#[derive(Debug, PartialEq)]
pub struct PressPane {
    pub id: String,
    pub active: bool,
    /// Pane is in a tmux mode (copy-mode etc.) — a click there would drive
    /// the mode (move the selection cursor, exit copy-mode), not the app.
    pub in_mode: bool,
    /// The foreground application requested mouse reporting; without it a
    /// click never reaches the app, only tmux's own pane bindings.
    pub mouse_any: bool,
}

/// Press-relevant state of a session's current window. Deliberately NO pane
/// rectangles: tmux routes a client's click itself (cursor-following pan
/// when the client is smaller than the window, zoom, top/multi-line status),
/// and the daemon cannot reproduce that per-client mapping — validation that
/// pretends to would approve the wrong pane (Codex). The press gate instead
/// checks the status rows on the client grid and requires every pane the
/// click could land in to be pressable.
#[derive(Debug, PartialEq)]
pub struct PressWindow {
    pub zoomed: bool,
    /// Status-line rows drawn at the top / bottom of the CLIENT grid. tmux
    /// draws status on the client's own rows regardless of window panning,
    /// so client-coordinate guards against these are exact.
    pub status_top: u16,
    pub status_bottom: u16,
    pub panes: Vec<PressPane>,
}

/// Freshly polled press state of `session`'s current window. Polled per
/// press, right before delivery — layout, zoom, and foreground mouse mode
/// change at any moment. `Ok(None)` if the server/session is gone.
pub fn press_window(session: &str) -> Result<Option<PressWindow>> {
    let target = format!("{session}:");
    let mut c = tmux();
    // Option values (#{status}, #{status-position}) expand in formats.
    c.args([
        "display-message",
        "-p",
        "-t",
        &target,
        "#{window_zoomed_flag} #{status} #{status-position}",
    ]);
    let Some(header) = run_classified(c, true)? else {
        return Ok(None);
    };
    let mut c = tmux();
    c.args([
        "list-panes",
        "-t",
        &target,
        "-F",
        "#{pane_id} #{pane_active} #{pane_in_mode} #{mouse_any_flag}",
    ]);
    let Some(panes) = run_classified(c, true)? else {
        return Ok(None);
    };
    Ok(parse_press_window(&header, &panes))
}

fn parse_press_window(header: &str, panes: &str) -> Option<PressWindow> {
    let f: Vec<&str> = header.split_whitespace().collect();
    if f.len() != 3 {
        return None;
    }
    let zoomed = f[0] != "0";
    // status option: off | on | 2..=5 (number of lines).
    let lines: u16 = match f[1] {
        "off" => 0,
        "on" => 1,
        n => n.parse().unwrap_or(1),
    };
    let top = f[2] == "top";
    let panes: Vec<PressPane> = panes
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() != 4 || !f[0].starts_with('%') {
                return None;
            }
            Some(PressPane {
                id: f[0].to_string(),
                active: f[1] != "0",
                in_mode: f[2] != "0",
                mouse_any: f[3] != "0",
            })
        })
        .collect();
    Some(PressWindow {
        zoomed,
        status_top: if top { lines } else { 0 },
        status_bottom: if top { 0 } else { lines },
        panes,
    })
}

/// Force a large fixed "capture resolution" on a window so a full-screen tool
/// (e.g. htop) renders all its columns and rows regardless of a small phone
/// client. Used only while a dashboard view is on screen — the terminal itself
/// is hidden then, so the oversized render is invisible. Reverted by
/// [`clear_capture_size`].
pub fn set_capture_size(window: &str, cols: u16, rows: u16) -> Result<()> {
    let mut c = tmux();
    c.args(["set-option", "-w", "-t", window, "window-size", "manual"]);
    run_classified(c, true)?;
    let mut c = tmux();
    c.args([
        "resize-window",
        "-t",
        window,
        "-x",
        &cols.to_string(),
        "-y",
        &rows.to_string(),
    ]);
    run_classified(c, true)?;
    Ok(())
}

/// Restore client-driven sizing (remux's `latest` default) on a window.
pub fn clear_capture_size(window: &str) -> Result<()> {
    let mut c = tmux();
    c.args(["set-option", "-w", "-t", window, "window-size", "latest"]);
    run_classified(c, true)?;
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

/// Real (non-control-mode) attached client count per session. `session_attached`
/// counts our own internal topology control client too, so "attached" (exposed
/// to the UI) would wrongly show sessions as attached; count only regular
/// clients here. Keyed by session name.
fn real_attached_counts() -> Result<std::collections::HashMap<String, u32>> {
    let mut cmd = tmux();
    // control_mode is a single 0/1; session name (may contain spaces) is the
    // remainder. (No tab separator — tmux 3.3a would sanitize it to `_`.)
    cmd.args([
        "list-clients",
        "-F",
        "#{client_control_mode} #{client_session}",
    ]);
    let mut m = std::collections::HashMap::new();
    // No server → no clients. Any other failure propagates rather than silently
    // reporting everything as unattached (Codex).
    if let Some(out) = run_optional(cmd)? {
        for line in out.lines() {
            if let Some((ctrl, sess)) = line.split_once(' ') {
                if ctrl == "0" {
                    *m.entry(sess.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    Ok(m)
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
    let Some(out) = run_optional(cmd)? else {
        return Ok(Vec::new()); // no tmux server running yet
    };
    let counts = real_attached_counts()?;
    Ok(out
        .lines()
        .filter_map(parse_session_line)
        .map(|mut s| {
            // Override tmux's attached count with real (non-control) clients.
            s.attached = counts.get(&s.name).copied().unwrap_or(0);
            s
        })
        .collect())
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
    let Some(out) = run_optional(cmd)? else {
        return Ok(Vec::new()); // no tmux server running yet
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

#[derive(serde::Serialize, Debug, PartialEq, Clone)]
pub struct WindowInfo {
    pub index: u32,
    pub active: bool,
    /// The active pane is zoomed (fills the window). On phones we auto-zoom
    /// split windows so no split geometry is rendered on a small screen.
    pub zoomed: bool,
    pub name: String,
    /// Panes in this window — surfaced as sub-tabs on the phone so a split can
    /// be navigated pane-by-pane rather than rendered as split geometry.
    pub panes: Vec<PaneInfo>,
}

#[derive(serde::Serialize, Debug, PartialEq, Clone)]
pub struct PaneInfo {
    /// tmux pane id (`%N`) — stable for the pane's lifetime, unlike `index`.
    /// Matches `$TMUX_PANE` in the pane's environment, which is how M4
    /// ingest events are mapped back to a session.
    pub id: String,
    pub index: u32,
    pub active: bool,
    pub command: String,
}

/// Windows of one session ("tabs" in the mobile UI), each with its panes.
pub fn list_windows(session: &str) -> Result<Vec<WindowInfo>> {
    let mut cmd = tmux();
    cmd.args([
        "list-windows",
        "-t",
        &format!("={session}"),
        "-F",
        "#{window_index} #{window_active} #{window_zoomed_flag} #{window_name}",
    ]);
    let Some(out) = run_classified(cmd, true)? else {
        return Ok(Vec::new()); // no server, or the session is gone
    };
    let mut windows = Vec::new();
    for line in out.lines() {
        if let Some((index, active, zoomed, name)) = parse_window_line(line) {
            // A pane-list failure that isn't benign absence is a real error;
            // propagate rather than silently dropping the window's panes.
            let panes = list_panes(session, index)?;
            windows.push(WindowInfo {
                index,
                active,
                zoomed,
                name,
                panes,
            });
        }
    }
    Ok(windows)
}

/// `<index> <active> <zoomed> <name (may contain spaces)>` — numerics first.
fn parse_window_line(line: &str) -> Option<(u32, bool, bool, String)> {
    let mut f = line.splitn(4, ' ');
    Some((
        f.next()?.parse().ok()?,
        f.next()? == "1",
        f.next()? == "1",
        sanitize(f.next().unwrap_or("")),
    ))
}

fn list_panes(session: &str, window_index: u32) -> Result<Vec<PaneInfo>> {
    let mut cmd = tmux();
    cmd.args([
        "list-panes",
        "-t",
        &format!("={session}:{window_index}"),
        "-F",
        "#{pane_id} #{pane_index} #{pane_active} #{pane_current_command}",
    ]);
    // No server, or the window/session vanished mid-capture → no panes. Any
    // other failure propagates rather than being silently dropped.
    let Some(out) = run_classified(cmd, true)? else {
        return Ok(Vec::new());
    };
    Ok(out.lines().filter_map(parse_pane_line).collect())
}

/// `<id> <index> <active> <command>` — command is the remainder (comm has no
/// spaces in practice, but be permissive). The id must be `%N`-shaped.
fn parse_pane_line(line: &str) -> Option<PaneInfo> {
    let mut f = line.splitn(4, ' ');
    let id = f.next()?;
    if !id.starts_with('%') || !id[1..].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(PaneInfo {
        id: id.to_string(),
        index: f.next()?.parse().ok()?,
        active: f.next()? == "1",
        command: sanitize(f.next().unwrap_or("")),
    })
}

/// One session with its windows — the topology unit streamed to clients.
#[derive(serde::Serialize, Debug, PartialEq, Clone)]
pub struct SessionWindows {
    pub name: String,
    pub attached: bool,
    pub windows: Vec<WindowInfo>,
}

/// Full server topology: every session and its windows. Rebuilt from scratch
/// on each control-mode dirty-bit (not parsed incrementally). Empty when no
/// tmux server is running.
pub fn capture_topology() -> Result<Vec<SessionWindows>> {
    let sessions = list_sessions()?;
    let mut out = Vec::with_capacity(sessions.len());
    for s in sessions {
        out.push(SessionWindows {
            windows: list_windows(&s.name)?,
            name: sanitize(&s.name),
            attached: s.attached > 0,
        });
    }
    Ok(out)
}

/// Argument vector for the read-only control-mode client (topology.rs builds
/// the async Command; kept here so all tmux flags live in one module).
/// Verified on tmux 3.3a: these flags keep the client out of window sizing,
/// suppress %output, and still deliver structural %notifications.
pub fn control_attach_args(session: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(sock) = socket_name() {
        args.push("-L".into());
        args.push(sock);
    }
    args.extend([
        "-C".into(),
        "attach-session".into(),
        "-t".into(),
        format!("={session}"),
        "-f".into(),
        "read-only,no-output,ignore-size".into(),
    ]);
    args
}

/// Controller-initiated window/pane operations, whitelisted by name.
pub fn window_action(session: &str, action: &str, index: Option<u32>) -> Result<()> {
    let target = format!("={session}");
    let mut cmd = tmux();
    match action {
        "new_window" => {
            cmd.args(["new-window", "-t", &target]);
        }
        // NB: split-window needs a pane-shaped target — "=session" alone is
        // "can't find pane" on tmux 3.3a; the trailing ":" (current window's
        // active pane) resolves.
        "split_h" => {
            cmd.args(["split-window", "-h", "-t", &format!("{target}:")]);
        }
        "split_v" => {
            cmd.args(["split-window", "-v", "-t", &format!("{target}:")]);
        }
        "next_pane" => {
            // Pure pane cycling — NO zoom here: this path affects every
            // client, and desktop controllers must keep their split layout.
            // Small screens re-zoom client-side (maybeAutoZoom) via zoom_pane
            // after the topology update.
            cmd.args(["select-pane", "-t", &format!("{target}:.+")]);
        }
        // Ensure the active pane fills the window (phones auto-zoom splits so
        // they never render split geometry on a small screen). Idempotent, and
        // only ever sent by small-screen clients.
        "zoom_pane" => {
            ensure_zoom(session)?;
            return Ok(());
        }
        "select_window" => {
            let i = index.context("select_window requires an index")?;
            cmd.args(["select-window", "-t", &format!("{target}:{i}")]);
        }
        // Switch to a specific pane in the current window. Pure select — no
        // zoom (affects all clients); small screens re-zoom client-side.
        "select_pane" => {
            let i = index.context("select_pane requires an index")?;
            cmd.args(["select-pane", "-t", &format!("{target}:.{i}")]);
        }
        other => bail!("unknown window action {other:?}"),
    }
    run(cmd)?;
    Ok(())
}

/// Zoom the active pane if the current window is split and not already zoomed.
/// `resize-pane -Z` toggles, so we check the flag first to stay idempotent.
fn ensure_zoom(session: &str) -> Result<()> {
    let mut q = tmux();
    q.args([
        "display-message",
        "-p",
        "-t",
        &format!("={session}:"),
        "#{window_zoomed_flag} #{window_panes}",
    ]);
    let out = run(q)?;
    let mut f = out.split_whitespace();
    let zoomed = f.next() == Some("1");
    let panes: u32 = f.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    if !zoomed && panes > 1 {
        let mut cmd = tmux();
        cmd.args(["resize-pane", "-Z", "-t", &format!("={session}:")]);
        run(cmd)?;
    }
    Ok(())
}

/// True when any attached tmux client sent input within `within_secs` —
/// i.e. someone is sitting at a keyboard and does not need a push.
pub fn any_client_active_within(within_secs: u64) -> Result<bool> {
    let mut cmd = tmux();
    cmd.args(["list-clients", "-F", "#{client_activity}"]);
    let Some(out) = run_optional(cmd)? else {
        return Ok(false); // no server → no clients active
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(out
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .any(|ts| now.saturating_sub(ts) < within_secs))
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
    fn press_window_parsing() {
        // Unzoomed window, default single bottom status, two panes (one in
        // copy-mode without mouse), plus malformed lines that must be skipped.
        let win = parse_press_window("0 on bottom", "%1 1 0 1\n%2 0 1 0\ngarbage\n%3 1\n").unwrap();
        assert!(!win.zoomed);
        assert_eq!((win.status_top, win.status_bottom), (0, 1));
        assert_eq!(win.panes.len(), 2);
        assert_eq!(
            win.panes[0],
            PressPane {
                id: "%1".into(),
                active: true,
                in_mode: false,
                mouse_any: true,
            }
        );
        assert!(win.panes[1].in_mode);
        assert!(!win.panes[1].mouse_any);

        // Zoomed, two status lines on top; "off" means no status rows at all.
        let win = parse_press_window("1 2 top", "%1 1 0 1\n").unwrap();
        assert!(win.zoomed);
        assert_eq!((win.status_top, win.status_bottom), (2, 0));
        let win = parse_press_window("0 off bottom", "%1 1 0 1\n").unwrap();
        assert_eq!((win.status_top, win.status_bottom), (0, 0));

        // A malformed header is a poll failure, not a permissive default.
        assert!(parse_press_window("0 on", "%1 1 0 1\n").is_none());
    }

    #[test]
    fn benign_absence_classification() {
        // "No server" is benign server-wide → empty, not an error.
        assert!(is_no_server("no server running on /tmp/tmux-501/default"));
        assert!(is_no_server(
            "error connecting to /tmp/tmux-501/default (No such file or directory)"
        ));
        assert!(is_no_server("No Server Running On ...")); // case-insensitive
                                                           // "can't find <target>" is NOT no-server, but IS benign for a scoped
                                                           // query (the session/window is simply gone → empty).
        assert!(!is_no_server("can't find session: main"));
        assert!(is_missing_target("can't find session: main"));
        assert!(is_missing_target("can't find window: 3"));
        // Operational failures are neither — they must surface, not become a
        // false-empty topology. Critically a *permission-denied* socket-connect
        // error must NOT be mistaken for the absent-socket case, nor a bare
        // "no such file or directory" without the connect context.
        for e in [
            "unknown option -- Q",
            "lost server",
            "error connecting to /tmp/tmux-0/default (Permission denied)",
            "no such file or directory",
            "",
        ] {
            assert!(!is_no_server(e), "{e:?}");
            assert!(!is_missing_target(e), "{e:?}");
        }
    }

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
    fn window_line_parsing() {
        // index active zoomed name(may have spaces)
        assert_eq!(
            parse_window_line("2 1 1 my window"),
            Some((2, true, true, "my window".to_string()))
        );
        let (idx, active, zoomed, name) = parse_window_line("0 0 0 bash").unwrap();
        assert_eq!(
            (idx, active, zoomed, name.as_str()),
            (0, false, false, "bash")
        );
        assert_eq!(parse_window_line("garbage"), None);
    }

    #[test]
    fn pane_line_parsing() {
        assert_eq!(
            parse_pane_line("%5 1 1 vim"),
            Some(PaneInfo {
                id: "%5".into(),
                index: 1,
                active: true,
                command: "vim".into(),
            })
        );
        assert!(!parse_pane_line("%0 0 0 bash").unwrap().active);
        assert_eq!(parse_pane_line("x"), None);
        // id must be %N-shaped: reject a line missing it (old format) or
        // with a mangled id, rather than mis-assigning fields.
        assert_eq!(parse_pane_line("1 1 vim"), None);
        assert_eq!(parse_pane_line("%x 1 1 vim"), None);
    }

    #[test]
    fn unknown_window_action_rejected() {
        assert!(window_action("s", "kill_server", None).is_err());
        assert!(window_action("s", "select_window", None).is_err()); // no index
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
