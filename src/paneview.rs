//! Pane views: a *source* streams structured state for a tmux pane, which the
//! PWA can render as a custom, phone-friendly interface instead of the raw
//! terminal. This is the daemon side of the `source -> remux -> custom PWA
//! renderer` pipe.
//!
//! Design (per the architecture review):
//! - The terminal stays canonical; a pane view is an *optional projection*.
//! - Transport is a dedicated persistent Unix socket (`pane-view.sock`), NOT the
//!   one-shot ingest socket — a stream at 1 Hz would exhaust ingest's 60/min cap.
//! - A source connects, sends a header `{pane, view}` (pane verified against the
//!   live topology, view must be a known id), then streams newline-delimited
//!   JSON snapshots. We keep only the *latest* validated snapshot per pane.
//! - Exactly one live view per pane; a second claim is rejected.
//! - Cleanup: connection EOF drops the entry (RAII guard); a pane leaving the
//!   topology drops it too (GC by set-difference).
//! - Caps: per-line byte cap, per-connection update-rate cap, per-view shape
//!   validation (e.g. taskscope.v1 worker-count cap).
//!
//! Trust model unchanged: same-uid is trusted; the peer-uid gate rejects other
//! users. This is a projection of a local program's state, not a security
//! boundary.

use crate::{tmux, App};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

/// Socket filename in the state dir.
pub const SOCKET: &str = "pane-view.sock";

/// Built-in view identifiers the PWA ships a hard-coded renderer for.
pub const KNOWN_VIEWS: &[&str] = &["taskscope.v1", "htop.v1"];

/// Views produced ONLY by an in-process adapter (the htop capture adapter),
/// never claimable over the source socket. This is a provenance boundary: the
/// daemon executes `htop.v1` actions itself (tmux/kill), so an external source
/// must not be able to impersonate that view and drive the tmux path with a
/// forged process list (Codex). External sources get `SOCKET_VIEWS` only.
pub const INTERNAL_VIEWS: &[&str] = &["htop.v1"];

/// Foreground commands the daemon auto-captures into a pane view (best-effort
/// "semantic lens" over the real tool's rendered screen — see `parse_htop`).
const CAPTURE_TOOLS: &[&str] = &["htop"];
/// How often the capture adapter re-reads a pane's screen.
const CAPTURE_INTERVAL: Duration = Duration::from_millis(1500);
/// How often to poll which panes run a captured tool (start/stop capture tasks).
const CAPTURE_POLL: Duration = Duration::from_millis(2000);
/// Cap the process rows published from one htop capture.
const MAX_PROCS: usize = 300;

/// Per-snapshot line cap (bytes). Bounds a buggy source; well above any real
/// view payload.
const MAX_LINE: u64 = 64 * 1024;
/// Minimum spacing between accepted snapshots on one connection — a rate cap so
/// a runaway source can't spin the broadcast/PWA. Snapshots arriving faster are
/// dropped (latest-wins, so dropping is safe).
const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(100);
/// Max concurrent source connections.
const MAX_STREAMS: usize = 32;
/// taskscope.v1: cap the worker array.
const MAX_WORKERS: usize = 128;
/// A source must send its header this quickly, or we drop the (slot-holding)
/// connection — a half-open connection can't squat a stream slot forever.
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// The capped, buffered read half of a source connection.
type SourceReader = tokio::io::Take<BufReader<tokio::net::unix::OwnedReadHalf>>;

/// Read one newline-terminated line, capped at `MAX_LINE`. Returns the line
/// bytes (newline trimmed), `None` on EOF, or `Err` if the line exceeds the cap
/// (the limit is hit before a `\n`).
async fn read_line(reader: &mut SourceReader, buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    reader.set_limit(MAX_LINE);
    buf.clear();
    let n = reader.read_until(b'\n', buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        anyhow::bail!("line exceeds cap");
    }
    Ok(Some(trim_line(buf).to_vec()))
}

/// Where a pane's view came from — the provenance boundary that decides how its
/// actions are routed (Codex finding 4).
#[derive(Clone, Copy, PartialEq, Debug)]
enum SourceKind {
    /// The in-process htop capture adapter. Actions run through the tmux/kill
    /// whitelist in this module.
    InternalHtop,
    /// An external `remux stream` source. Actions are forwarded back to it over
    /// the connection's back-channel; the daemon never interprets them.
    Socket,
}

/// Whether a menu action may be triggered by any in-session device, or only by a
/// device holding the host-granted `approve` capability. Default is `Approve`;
/// a trusted source opts a low-risk action down to `Session` explicitly, and a
/// `danger`-styled option is always `Approve` regardless (Codex finding 1).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ActionPolicy {
    Session,
    Approve,
}

/// Bounded depth of a source's action back-channel. A client can enqueue at most
/// this many un-delivered actions; further ones are dropped (Codex finding 2).
const ACTION_QUEUE: usize = 16;
/// How long a single action-line write to a source may block before we give up
/// and drop the connection — bounds a source that stopped reading its socket.
const ACTION_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Max bytes of a source action token (matches the menu-token grammar cap).
pub const MAX_ACTION_TOKEN: usize = 64;

/// Latest structured state for one pane.
struct Entry {
    view: String,
    /// Distinguishes the owning connection so a stale guard can't clobber a
    /// pane that was re-claimed by a newer source.
    instance: u64,
    rev: u64,
    /// `None` until the first snapshot — a claimed-but-empty pane shows nothing.
    state: Option<Value>,
    kind: SourceKind,
    /// Action tokens the *current* snapshot's `menu` advertises, each with its
    /// policy. A client action is only forwarded if it is a key here (so a
    /// client can never trigger an action the view is not currently offering),
    /// and only if its policy is satisfied.
    menu: HashMap<String, ActionPolicy>,
    /// Back-channel to the owning source connection (set only for `Socket`
    /// sources). `send_action` pushes a chosen menu token here; the connection
    /// task writes it to the source. Bounded so a stuck source can't grow it.
    action_tx: Option<mpsc::Sender<String>>,
}

/// The token→policy map a snapshot's `menu` advertises. Empty if there is no
/// menu. `validate` has already shape-checked the menu, so this trusts the
/// structure; it re-derives policy defensively (default Approve; `danger` forces
/// Approve; explicit `requires:"session"` opts down).
fn menu_policies(state: &Value) -> HashMap<String, ActionPolicy> {
    let mut out = HashMap::new();
    let Some(opts) = state
        .get("menu")
        .and_then(|m| m.get("options"))
        .and_then(Value::as_array)
    else {
        return out;
    };
    for o in opts {
        let Some(action) = o.get("action").and_then(Value::as_str) else {
            continue;
        };
        let danger = o.get("style").and_then(Value::as_str) == Some("danger");
        let session_ok = !danger && o.get("requires").and_then(Value::as_str) == Some("session");
        out.insert(
            action.to_string(),
            if session_ok {
                ActionPolicy::Session
            } else {
                ActionPolicy::Approve
            },
        );
    }
    out
}

/// How a pane's client actions are routed (by provenance, not view id).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ActionKind {
    /// Run through the in-module tmux/kill whitelist (`exec_htop_action`).
    Htop,
    /// Forward to the external source over its back-channel (`send_action`).
    Source,
}

/// One pane's current view, as sent to the PWA.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct PaneView {
    pub pane: String,
    pub view: String,
    pub rev: u64,
    pub state: Value,
}

/// Latest-state-per-pane registry. Modeled on `permit::Registry`: a payload-less
/// `events` broadcast is only a *wake hint* — every subscriber reconciles via
/// [`snapshot`](Registry::snapshot), so a lagged receiver can never miss state.
pub struct Registry {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
    events: broadcast::Sender<()>,
    next_instance: Arc<AtomicU64>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            inner: Arc::new(Mutex::new(HashMap::new())),
            events: broadcast::channel(16).0,
            next_instance: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl Registry {
    /// Subscribe to change hints. Reconcile via [`snapshot`](Registry::snapshot)
    /// on every wake (and on `Lagged`).
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.events.subscribe()
    }

    /// Claim `pane` for an in-process (`InternalHtop`) view — the capture
    /// adapter. No back-channel.
    pub fn claim_internal(&self, pane: &str, view: &str) -> Result<ClaimGuard, &'static str> {
        self.claim_inner(pane, view, SourceKind::InternalHtop, None)
            .map(|(g, _)| g)
    }

    /// Claim `pane` for an external `remux stream` (`Socket`) source. Rejects
    /// `INTERNAL_VIEWS` so an external source can't impersonate the htop adapter
    /// and drive the tmux/kill path (Codex finding 4). Returns the guard and the
    /// receive half of a bounded action back-channel; the caller writes each
    /// received token to the source.
    pub fn claim_socket(
        &self,
        pane: &str,
        view: &str,
    ) -> Result<(ClaimGuard, mpsc::Receiver<String>), &'static str> {
        if INTERNAL_VIEWS.contains(&view) {
            return Err("view not available to external sources");
        }
        let (tx, rx) = mpsc::channel(ACTION_QUEUE);
        let (guard, _) = self.claim_inner(pane, view, SourceKind::Socket, Some(tx))?;
        Ok((guard, rx))
    }

    fn claim_inner(
        &self,
        pane: &str,
        view: &str,
        kind: SourceKind,
        action_tx: Option<mpsc::Sender<String>>,
    ) -> Result<(ClaimGuard, ()), &'static str> {
        if !KNOWN_VIEWS.contains(&view) {
            return Err("unknown view");
        }
        let mut map = self.inner.lock().unwrap();
        if map.contains_key(pane) {
            return Err("pane already has a live view");
        }
        let instance = self.next_instance.fetch_add(1, Ordering::Relaxed);
        map.insert(
            pane.to_string(),
            Entry {
                view: view.to_string(),
                instance,
                rev: 0,
                state: None,
                kind,
                menu: HashMap::new(),
                action_tx,
            },
        );
        // No hint yet: nothing is renderable until the first snapshot lands.
        Ok((
            ClaimGuard {
                inner: self.inner.clone(),
                events: self.events.clone(),
                pane: pane.to_string(),
                instance,
            },
            (),
        ))
    }

    /// How a pane's actions should be routed, by provenance — `None` if the pane
    /// has no live view.
    pub fn action_kind(&self, pane: &str) -> Option<ActionKind> {
        self.inner.lock().unwrap().get(pane).map(|e| match e.kind {
            SourceKind::InternalHtop => ActionKind::Htop,
            SourceKind::Socket => ActionKind::Source,
        })
    }

    /// Forward a chosen menu `token` to the pane's `Socket` source. Returns
    /// `false` (dropped) unless: the pane is a socket source, the token is in the
    /// pane's *current* menu, its policy is satisfied (`can_approve` for an
    /// `Approve` action), and the bounded channel has room. The daemon never
    /// interprets the token — semantics are the source's.
    pub fn send_action(&self, pane: &str, token: &str, can_approve: bool) -> bool {
        let map = self.inner.lock().unwrap();
        let Some(e) = map.get(pane) else {
            return false;
        };
        let Some(policy) = e.menu.get(token) else {
            return false; // not currently advertised
        };
        if *policy == ActionPolicy::Approve && !can_approve {
            return false;
        }
        let Some(tx) = &e.action_tx else {
            return false; // not a socket source
        };
        // try_send: a full queue means the source is stuck; drop rather than
        // grow memory or block (Codex finding 2).
        tx.try_send(token.to_string()).is_ok()
    }

    /// Current renderable views (entries that have received at least one
    /// snapshot), for reconcile-on-subscribe.
    pub fn snapshot(&self) -> Vec<PaneView> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(pane, e)| {
                e.state.as_ref().map(|s| PaneView {
                    pane: pane.clone(),
                    view: e.view.clone(),
                    rev: e.rev,
                    state: s.clone(),
                })
            })
            .collect()
    }

    /// The view id currently held for `pane`, if any.
    pub fn view_of(&self, pane: &str) -> Option<String> {
        self.inner.lock().unwrap().get(pane).map(|e| e.view.clone())
    }

    /// Whether the pane is an **internal htop** view listing a process with this
    /// pid — the kill gate. The `InternalHtop` check and the pid check happen
    /// under ONE lock, so a prune+reclaim race can't swap in a socket source with
    /// a forged `processes` array between a separate provenance check and this
    /// one (Codex finding 6). A socket source can never satisfy this.
    pub fn pane_has_pid(&self, pane: &str, pid: u32) -> bool {
        let map = self.inner.lock().unwrap();
        let Some(e) = map.get(pane) else {
            return false;
        };
        if e.kind != SourceKind::InternalHtop {
            return false;
        }
        e.state
            .as_ref()
            .and_then(|s| s.get("processes"))
            .and_then(Value::as_array)
            .is_some_and(|arr| {
                arr.iter()
                    .any(|p| p.get("pid").and_then(Value::as_u64) == Some(pid as u64))
            })
    }

    /// Drop views whose pane is no longer in the live topology set.
    pub fn prune(&self, live: &HashSet<String>) {
        let mut map = self.inner.lock().unwrap();
        let before = map.len();
        map.retain(|pane, _| live.contains(pane));
        let changed = map.len() != before;
        drop(map);
        if changed {
            let _ = self.events.send(());
        }
    }
}

/// RAII owner of a pane's view. Updating goes through the guard so a stale
/// connection (whose `instance` no longer owns the pane) is a no-op. Drop
/// removes the entry and fires a hint.
pub struct ClaimGuard {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
    events: broadcast::Sender<()>,
    pane: String,
    instance: u64,
}

impl ClaimGuard {
    /// Store a new latest snapshot, bump the rev, and wake watchers. Returns the
    /// new rev, or `None` if this guard no longer owns the pane.
    pub fn update(&self, state: Value) -> Option<u64> {
        let mut map = self.inner.lock().unwrap();
        let e = map.get_mut(&self.pane)?;
        if e.instance != self.instance {
            return None;
        }
        e.rev += 1;
        // Recompute the authorized action set from the new snapshot's menu, so a
        // client can only ever trigger what the view is *currently* advertising.
        e.menu = menu_policies(&state);
        e.state = Some(state);
        let rev = e.rev;
        drop(map);
        let _ = self.events.send(());
        Some(rev)
    }

    /// Whether this guard still owns its pane's entry — false once the entry was
    /// pruned (topology GC) or replaced by a newer claim. Lets a connection that
    /// isn't currently publishing snapshots still notice promptly that its view
    /// is gone and free its stream slot (Codex finding 5).
    pub fn owns(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(&self.pane)
            .is_some_and(|e| e.instance == self.instance)
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let mut map = self.inner.lock().unwrap();
        // Only remove if we still own it (a newer instance may have replaced us
        // after a topology-driven prune + re-claim).
        if map
            .get(&self.pane)
            .is_some_and(|e| e.instance == self.instance)
        {
            map.remove(&self.pane);
            drop(map);
            let _ = self.events.send(());
        }
    }
}

/// A vetted dashboard action for htop, parsed from a client's semantic action
/// string. ONLY these can ever reach the pane — never arbitrary keystrokes.
#[derive(Debug, PartialEq)]
pub enum HtopAction {
    /// A literal character key (`P`/`M`/`T`/`I`).
    Key(&'static str),
    /// A named key (`F5`).
    Named(&'static str),
    /// Set/replace the incremental filter to this already-sanitized text.
    Filter(String),
    /// Signal a process — gated against the pane's visible set (and the caller's
    /// `approve` capability) by the caller.
    Kill { pid: u32, signal: KillSignal },
}

/// The only signals a client may request — a tiny whitelist, so a popup option
/// can never smuggle an arbitrary `kill -N`.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum KillSignal {
    Term,
    Kill,
}

impl KillSignal {
    /// The `kill(1)` flag.
    fn flag(self) -> &'static str {
        match self {
            KillSignal::Term => "-TERM",
            KillSignal::Kill => "-KILL",
        }
    }
}

/// Parse a client action string for the `htop.v1` view. `None` for anything not
/// whitelisted, so a client can only trigger vetted actions.
pub fn parse_htop_action(action: &str) -> Option<HtopAction> {
    match action {
        "sort:cpu" => Some(HtopAction::Key("P")),
        "sort:mem" => Some(HtopAction::Key("M")),
        "sort:time" => Some(HtopAction::Key("T")),
        "invert" => Some(HtopAction::Key("I")),
        "tree" => Some(HtopAction::Named("F5")),
        _ => {
            if let Some(q) = action.strip_prefix("filter:") {
                // Only printable text reaches htop's filter field; cap length.
                Some(HtopAction::Filter(
                    q.chars().filter(|c| !c.is_control()).take(64).collect(),
                ))
            } else if let Some(rest) = action.strip_prefix("kill:") {
                // `kill:<pid>` (defaults to TERM) or `kill:<pid>:TERM|KILL`. The
                // signal is a closed whitelist; any other suffix is rejected.
                let (pid_s, signal) = match rest.split_once(':') {
                    None => (rest, Some(KillSignal::Term)),
                    Some((p, "TERM")) => (p, Some(KillSignal::Term)),
                    Some((p, "KILL")) => (p, Some(KillSignal::Kill)),
                    Some((p, _)) => (p, None),
                };
                match (pid_s.parse::<u32>(), signal) {
                    (Ok(pid), Some(signal)) => Some(HtopAction::Kill { pid, signal }),
                    _ => None,
                }
            } else {
                None
            }
        }
    }
}

/// Drive a non-kill htop action through tmux. Kill is handled by the caller
/// (it needs a capability + visibility check).
///
/// Re-verifies htop *currently* owns the pane immediately before sending keys:
/// otherwise a stale view could type the filter text at a shell prompt (a
/// command-execution path). The residual window is the few ms between this check
/// and send-keys; the capture task drops the view within one tick of htop
/// exiting, so a stale view is not durable.
pub fn exec_htop_action(pane: &str, action: &HtopAction) -> Result<()> {
    if tmux::pane_command(pane).as_deref() != Some("htop") {
        return Ok(());
    }
    match action {
        HtopAction::Key(k) => tmux::send_keys(pane, k),
        HtopAction::Named(k) => tmux::send_named(pane, &[k]),
        HtopAction::Filter(q) => htop_set_filter(pane, q),
        HtopAction::Kill { .. } => Ok(()),
    }
}

/// Set (or clear, if empty) htop's incremental filter: F4, clear any existing
/// text, type the query, Enter.
///
/// The sequence spans several tmux calls, so an upfront ownership check is not
/// enough: if htop exits *between* calls, the remaining keys land at the shell
/// underneath. `send-keys -l` stops tmux key-name interpretation but does NOT
/// neutralize shell syntax, so `filter:; rm -rf ~` typed at a prompt and then
/// Enter would execute. We therefore re-verify htop still owns the pane before
/// the two dangerous phases — typing the literal query, and (tightest, since it
/// is Enter that turns typed text into a command) sending the confirming Enter.
/// F4 and BSpace are inert at a shell, so they need no gate. The residual window
/// is the microseconds between the final check and the Enter syscall.
fn htop_set_filter(pane: &str, query: &str) -> Result<()> {
    let owns = || tmux::pane_command(pane).as_deref() == Some("htop");
    tmux::send_named(pane, &["F4"])?;
    let clear = vec!["BSpace"; 48];
    tmux::send_named(pane, &clear)?;
    if !query.is_empty() {
        if !owns() {
            return Ok(()); // htop gone — never type the query at a shell
        }
        tmux::send_keys(pane, query)?;
    }
    if !owns() {
        return Ok(()); // htop gone — never send the Enter that would execute it
    }
    tmux::send_named(pane, &["Enter"])
}

/// Signal a process with one of the whitelisted signals. Best-effort. `kill`
/// takes the pid via argv (no shell), so there is no injection surface.
pub fn kill_process(pid: u32, signal: KillSignal) {
    let _ = std::process::Command::new("kill")
        .arg(signal.flag())
        .arg(pid.to_string())
        .status();
}

/// Cap the option count of a generic menu.
const MAX_MENU_OPTIONS: usize = 16;
/// Cap menu title / option-label length (chars).
const MAX_MENU_LABEL: usize = 64;
/// Cap menu detail length (chars).
const MAX_MENU_DETAIL: usize = 160;

/// Opaque action-token grammar: `[A-Za-z0-9._:-]`, 1..=MAX_ACTION_TOKEN chars.
/// No whitespace, control chars, or Unicode line separators — so a token is safe
/// to relay to a source as one JSON line and can never smuggle a second frame
/// (Codex Q1: `char::is_control` misses U+2028/U+2029; an explicit allowlist
/// does not). ASCII-only, so byte length == char length.
fn is_valid_action_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ACTION_TOKEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// Validate the optional generic `menu` (present on any view). A menu declares
/// interactive options a source (or the htop adapter) offers; each option's
/// `action` is an opaque token the client can trigger. Absent menu is fine.
fn validate_menu(obj: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    let Some(menu) = obj.get("menu") else {
        return Ok(());
    };
    let menu = menu.as_object().ok_or("menu must be an object")?;
    let title = menu
        .get("title")
        .and_then(Value::as_str)
        .ok_or("menu.title must be a string")?;
    if title.is_empty() || title.chars().count() > MAX_MENU_LABEL {
        return Err("menu.title length");
    }
    if let Some(d) = menu.get("detail") {
        if d.as_str()
            .ok_or("menu.detail must be a string")?
            .chars()
            .count()
            > MAX_MENU_DETAIL
        {
            return Err("menu.detail too long");
        }
    }
    let opts = menu
        .get("options")
        .and_then(Value::as_array)
        .ok_or("menu.options must be an array")?;
    if opts.is_empty() || opts.len() > MAX_MENU_OPTIONS {
        return Err("menu.options count");
    }
    let mut seen = HashSet::new();
    for o in opts {
        let o = o.as_object().ok_or("menu option must be an object")?;
        let label = o
            .get("label")
            .and_then(Value::as_str)
            .ok_or("option.label must be a string")?;
        if label.is_empty() || label.chars().count() > MAX_MENU_LABEL {
            return Err("option.label length");
        }
        let action = o
            .get("action")
            .and_then(Value::as_str)
            .ok_or("option.action must be a string")?;
        if !is_valid_action_token(action) {
            return Err("option.action grammar");
        }
        if !seen.insert(action) {
            return Err("duplicate option.action");
        }
        if let Some(s) = o.get("style") {
            let s = s.as_str().ok_or("option.style must be a string")?;
            if !matches!(s, "default" | "danger" | "cancel") {
                return Err("option.style enum");
            }
        }
        if let Some(r) = o.get("requires") {
            let r = r.as_str().ok_or("option.requires must be a string")?;
            if !matches!(r, "approve" | "session") {
                return Err("option.requires enum");
            }
        }
    }
    Ok(())
}

/// Validate a snapshot's shape for `view`. Kept strict but minimal — enough that
/// the PWA renderer can trust the structure. Per-view knowledge lives here for
/// now; the generic `menu` is validated for every view.
pub fn validate(view: &str, state: &Value) -> Result<(), &'static str> {
    let obj = state.as_object().ok_or("state must be a JSON object")?;
    match view {
        "taskscope.v1" => {
            let workers = obj
                .get("workers")
                .and_then(Value::as_array)
                .ok_or("taskscope.v1: `workers` must be an array")?;
            if workers.len() > MAX_WORKERS {
                return Err("taskscope.v1: too many workers");
            }
            for w in workers {
                let w = w
                    .as_object()
                    .ok_or("taskscope.v1: worker must be an object")?;
                if !w.get("name").is_some_and(Value::is_string) {
                    return Err("taskscope.v1: worker.name must be a string");
                }
                if !w.get("status").is_some_and(Value::is_string) {
                    return Err("taskscope.v1: worker.status must be a string");
                }
                for f in ["cpu", "mem", "progress"] {
                    if !w.get(f).is_some_and(Value::is_number) {
                        return Err("taskscope.v1: worker.{cpu,mem,progress} must be numbers");
                    }
                }
            }
        }
        "htop.v1" => {
            if !obj.get("processes").is_some_and(Value::is_array) {
                return Err("htop.v1: `processes` must be an array");
            }
        }
        _ => return Err("unknown view"),
    }
    validate_menu(obj)
}

// ---------------------------------------------------------------------------
// htop capture adapter: read a real tool's rendered screen and project it.
// ---------------------------------------------------------------------------

/// Watch topology; for each pane whose foreground command is a captured tool
/// (htop), run a task that reads its rendered screen and feeds an `htop.v1`
/// view. Stop when the tool exits or the pane vanishes. Best-effort: the
/// terminal stays the real tool; this is only a projection of visible state.
pub fn spawn_capture(app: Arc<App>) {
    tokio::spawn(async move {
        let mut tasks: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
        let mut poll = tokio::time::interval(CAPTURE_POLL);
        loop {
            poll.tick().await;
            // Fresh poll of which panes run a captured tool. topology's cached
            // command is NOT refreshed on a foreground-process change, so it
            // can't detect a tool starting/exiting — poll authoritatively.
            let want =
                match tokio::task::spawn_blocking(|| tmux::panes_running(CAPTURE_TOOLS)).await {
                    Ok(Ok(w)) => w,
                    _ => continue,
                };
            for pane in &want {
                // (Re)start if there's no task, or the last one exited (e.g. it
                // lost the claim to a stream, or htop briefly stopped).
                if tasks
                    .get(pane)
                    .is_none_or(tokio::task::AbortHandle::is_finished)
                {
                    let app = app.clone();
                    let p = pane.clone();
                    let h = tokio::spawn(async move { capture_task(app, p).await });
                    tasks.insert(pane.clone(), h.abort_handle());
                }
            }
            // Stop tasks whose pane no longer runs the tool (abort drops the
            // task's ClaimGuard → the view is removed → action gates go false).
            tasks.retain(|pane, ah| {
                let keep = want.contains(pane) && !ah.is_finished();
                if !keep {
                    ah.abort();
                }
                keep
            });
        }
    });
}

async fn capture_task(app: Arc<App>, pane: String) {
    // Claim the pane's view; if it already has one (e.g. a `remux stream`), skip.
    let guard = match app.pane_views.claim_internal(&pane, "htop.v1") {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut ticker = tokio::time::interval(CAPTURE_INTERVAL);
    loop {
        ticker.tick().await;
        // Authoritatively re-verify the pane still runs the tool before each
        // capture, so the view (and every action gate keyed on it) drops the
        // instant htop exits — no window where a stale view accepts actions.
        let p = pane.clone();
        let still = tokio::task::spawn_blocking(move || tmux::pane_command(&p)).await;
        if !matches!(&still, Ok(Some(c)) if CAPTURE_TOOLS.contains(&c.as_str())) {
            break; // guard drop removes the view
        }
        let p = pane.clone();
        let captured = tokio::task::spawn_blocking(move || tmux::capture_pane(&p)).await;
        let text = match captured {
            Ok(Ok(Some(t))) => t,
            _ => continue,
        };
        // `None` means our claim was pruned out from under us (e.g. a transient
        // empty-topology GC pass at startup, or a `remux stream` re-claim). Exit
        // so the task handle finishes and `spawn_capture` re-claims on its next
        // poll — otherwise we'd loop forever updating an entry we no longer own,
        // and the still-"running" handle would block any restart.
        if guard.update(parse_htop(&text)).is_none() {
            break;
        }
    }
}

/// Parse an htop screen (from `capture-pane -p`) into `htop.v1` state. Tolerant
/// and visible-slice-only: it never infers hidden rows, derives columns from the
/// header (so column reordering is tolerated), and reports `confidence: "low"`
/// when the screen doesn't look like htop (the PWA then keeps the terminal).
pub fn parse_htop(text: &str) -> Value {
    let lines: Vec<&str> = text.lines().collect();

    // The header row names the columns. htop renders the sort column with a
    // marker that can glue "CPU%-MEM%"; replacing '-' (never a real label char)
    // splits them back apart.
    let header_idx = lines.iter().position(|l| {
        let u = l.to_ascii_uppercase();
        // Don't require COMMAND: a narrow pane (e.g. a phone at 64 cols) drops
        // it, but the rest of the table is still worth showing.
        u.contains("PID") && (u.contains("CPU%") || u.contains("MEM%"))
    });
    // The meters live above the header; scan only there so a process command
    // that happens to contain "Uptime:" etc. can't corrupt the summary.
    let summary = parse_htop_summary(match header_idx {
        Some(hi) => &lines[..hi],
        None => &lines,
    });
    let Some(hi) = header_idx else {
        return serde_json::json!({
            "confidence": "low", "reason": "no htop header", "summary": summary, "processes": []
        });
    };
    let cols: Vec<String> = lines[hi]
        .replace('-', " ")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let idx = |name: &str| cols.iter().position(|c| c.eq_ignore_ascii_case(name));
    // Command is optional — absent when the pane is too narrow to show it.
    let cmd_i = idx("Command");
    let (pid_i, user_i, res_i, cpu_i, mem_i, time_i) = (
        idx("PID"),
        idx("USER"),
        idx("RES"),
        idx("CPU%"),
        idx("MEM%"),
        idx("TIME+"),
    );

    let mut procs = Vec::new();
    for line in &lines[hi + 1..] {
        let head = line.trim_start();
        if head.is_empty() || head.starts_with("F1") {
            continue; // blank tail / function-key footer
        }
        // Split the fixed leading columns from the command remainder (the command
        // contains spaces, so it can't be a plain token). If there's no Command
        // column (narrow pane), every token is a leading column and command="".
        let mut it = line.split_whitespace();
        let (leading, command): (Vec<&str>, String) = match cmd_i {
            Some(ci) => {
                let lead: Vec<&str> = it.by_ref().take(ci).collect();
                if lead.len() < ci {
                    continue;
                }
                (lead, it.collect::<Vec<_>>().join(" "))
            }
            None => (it.collect(), String::new()),
        };
        // A row is only a process if its PID cell is numeric.
        let pid = match pid_i
            .and_then(|i| leading.get(i))
            .and_then(|s| s.parse::<u64>().ok())
        {
            Some(p) => p,
            None => continue,
        };
        let cell = |oi: Option<usize>| oi.and_then(|i| leading.get(i)).copied().unwrap_or("");
        let num = |s: &str| s.parse::<f64>().unwrap_or(0.0);
        procs.push(serde_json::json!({
            "pid": pid,
            "user": cell(user_i),
            "cpu": num(cell(cpu_i)),
            "mem": num(cell(mem_i)),
            "res": cell(res_i),
            "time": cell(time_i),
            "command": command,
        }));
        if procs.len() >= MAX_PROCS {
            break;
        }
    }
    serde_json::json!({
        "confidence": if procs.is_empty() { "low" } else { "ok" },
        "summary": summary,
        "processes": procs,
    })
}

/// Best-effort parse of htop's top meters (above the process header).
fn parse_htop_summary(lines: &[&str]) -> Value {
    let mut cores: Vec<f64> = Vec::new();
    let mut mem = String::new();
    let mut swap = String::new();
    let mut tasks = String::new();
    let mut load = String::new();
    let mut uptime = String::new();
    let after = |l: &str, label: &str| {
        l.find(label)
            .map(|p| l[p + label.len()..].trim().to_string())
    };
    for l in lines {
        cores.extend(core_percents(l));
        if let Some(v) = meter_value(l, "Mem[") {
            mem = v;
        }
        if let Some(v) = meter_value(l, "Swp[") {
            swap = v;
        }
        // Tasks / Load sit to the RIGHT of the Mem / Swp meters on the same line,
        // and Uptime is on its own — so match them as substrings, not prefixes.
        if let Some(v) = after(l, "Tasks:") {
            tasks = v;
        }
        if let Some(v) = after(l, "Load average:") {
            load = v;
        }
        if let Some(v) = after(l, "Uptime:") {
            uptime = v;
        }
    }
    let cpu = if cores.is_empty() {
        0.0
    } else {
        (cores.iter().sum::<f64>() / cores.len() as f64 * 10.0).round() / 10.0
    };
    serde_json::json!({
        "cpu_pct": cpu, "cores": cores.len(),
        "mem": mem, "swap": swap, "tasks": tasks, "load": load, "uptime": uptime,
    })
}

/// Each htop CPU meter ends `…X.X%]`; pull the percentages out of a line (a line
/// can hold several cores). Non-CPU meters (Mem/Swp) end `…]` without `%`, so
/// they don't match.
fn core_percents(line: &str) -> Vec<f64> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = line[from..].find("%]") {
        let end = from + rel; // index of '%'
        let mut start = end;
        while start > 0 && (b[start - 1].is_ascii_digit() || b[start - 1] == b'.') {
            start -= 1;
        }
        if start < end {
            if let Ok(v) = line[start..end].parse::<f64>() {
                out.push(v);
            }
        }
        from = end + 2;
    }
    out
}

/// The value htop shows at the right of a meter, e.g. `Mem[|||2.27G/3.83G]` →
/// `2.27G/3.83G`.
fn meter_value(line: &str, prefix: &str) -> Option<String> {
    let inner = line.split_once(prefix)?.1;
    // Up to the meter's own closing ']', which may be mid-line (Tasks/Load
    // follow it), then drop the leading bar/spaces to leave just the value.
    let inner = inner.split_once(']').map(|(a, _)| a).unwrap_or(inner);
    Some(inner.trim_start_matches(['|', ' ']).to_string())
}

// ---------------------------------------------------------------------------
// Daemon: the stream socket server.
// ---------------------------------------------------------------------------

fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SOCKET)
}

/// Bind the pane-view socket and serve source connections. Also starts the
/// topology-driven GC. Spawns background tasks and returns.
pub fn spawn(app: Arc<App>, state_dir: &Path) -> Result<()> {
    let path = socket_path(state_dir);
    // A leftover socket is stale by construction (the admin socket's live-probe
    // is the real single-instance guard), so unlink before binding.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {path:?}"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let owner_uid = std::fs::metadata(&path)?.uid();
    let conns = Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS));

    // GC: drop views whose pane left the topology.
    {
        let app = app.clone();
        tokio::spawn(async move { gc_loop(app).await });
    }

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "pane-view accept failed");
                    break;
                }
            };
            if !crate::admin::peer_allowed(&stream, owner_uid) {
                continue;
            }
            let Ok(permit) = conns.clone().try_acquire_owned() else {
                // Too many concurrent streams — drop, don't queue.
                continue;
            };
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, app).await {
                    tracing::debug!(error = %e, "pane-view stream ended");
                }
                drop(permit);
            });
        }
    });
    Ok(())
}

#[derive(serde::Deserialize)]
struct Header {
    pane: String,
    view: String,
}

/// Handle one source connection: header (claim) → stream of snapshots.
async fn handle(stream: UnixStream, app: Arc<App>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read).take(MAX_LINE);
    let mut buf = Vec::new();

    // --- header (time-bounded so a half-open connection can't hold a slot) ---
    let header_bytes =
        match tokio::time::timeout(HEADER_TIMEOUT, read_line(&mut reader, &mut buf)).await {
            Ok(Ok(Some(b))) => b,
            _ => return Ok(()), // timeout, EOF, or over-cap → just close
        };
    let header: Header = match serde_json::from_slice(&header_bytes) {
        Ok(h) => h,
        Err(_) => return ack_err(&mut write, "bad header json").await,
    };
    if header.pane.is_empty() {
        return ack_err(&mut write, "missing pane").await;
    }
    if !pane_exists(&app, &header.pane) {
        return ack_err(&mut write, "no such pane in this session").await;
    }
    let (guard, mut action_rx) = match app.pane_views.claim_socket(&header.pane, &header.view) {
        Ok(pair) => pair,
        Err(e) => return ack_err(&mut write, e).await,
    };
    write.write_all(b"{\"ok\":true}\n").await?;

    // --- snapshots (source → daemon) + actions (daemon → source), duplex ---
    // Latest-wins rate cap: coalesce snapshots into a single `pending` slot and
    // publish it at most once per MIN_UPDATE_INTERVAL. Unlike a plain drop, the
    // *final* snapshot of a source that then goes idle (but stays connected) is
    // never stranded — the timer flushes it within one interval.
    let mut pending: Option<Value> = None;
    let mut flush = tokio::time::interval(MIN_UPDATE_INTERVAL);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // NB: not `biased` — a perpetually-ready action branch must not starve
        // snapshot reads / EOF (Codex finding 4); fair polling gives reads a turn.
        tokio::select! {
            _ = flush.tick() => {
                match pending.take() {
                    // Publish a coalesced snapshot; break if our claim is gone.
                    Some(state) => if guard.update(state).is_none() { break; },
                    // Idle tick: still notice a prune promptly so a pruned-but-
                    // quiet connection frees its slot instead of lingering until
                    // the whole action queue drains. (An action write already in
                    // flight can delay this by up to ACTION_WRITE_TIMEOUT, but it
                    // is always bounded and never a wedge.)
                    None => if !guard.owns() { break; },
                }
            }
            // A chosen menu action to relay back to the source. The token was
            // validated against the ASCII grammar at menu-validate time, so it
            // holds no newline; serde_json framing keeps it one JSON line. The
            // write is timeout-bounded so a source that stops reading its socket
            // can't wedge this task (and thus the stream slot) — on timeout we
            // drop the connection (Codex finding 2).
            act = action_rx.recv() => {
                let Some(token) = act else { break }; // channel closed
                let line = serde_json::json!({ "action": token }).to_string();
                let w = async {
                    write.write_all(line.as_bytes()).await?;
                    write.write_all(b"\n").await
                };
                match tokio::time::timeout(ACTION_WRITE_TIMEOUT, w).await {
                    Ok(Ok(())) => {}
                    _ => break, // slow/broken source → drop it
                }
            }
            line = read_line(&mut reader, &mut buf) => {
                match line? {
                    None => break, // EOF → guard drop removes the view
                    Some(bytes) => {
                        if bytes.is_empty() {
                            continue;
                        }
                        let Ok(state) = serde_json::from_slice::<Value>(&bytes) else {
                            continue; // skip a malformed frame, keep the stream alive
                        };
                        if validate(&header.view, &state).is_err() {
                            continue;
                        }
                        pending = Some(state); // coalesced; the timer publishes it
                    }
                }
            }
        }
    }
    Ok(())
}

fn trim_line(buf: &[u8]) -> &[u8] {
    let mut end = buf.len();
    while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
        end -= 1;
    }
    &buf[..end]
}

async fn ack_err(write: &mut tokio::net::unix::OwnedWriteHalf, msg: &str) -> Result<()> {
    let line = serde_json::json!({ "ok": false, "error": msg }).to_string();
    write.write_all(line.as_bytes()).await?;
    write.write_all(b"\n").await?;
    Ok(())
}

/// Is `pane` (a tmux `%N` id) present in the current topology?
fn pane_exists(app: &App, pane: &str) -> bool {
    app.topology.borrow().iter().any(|s| {
        s.windows
            .iter()
            .any(|w| w.panes.iter().any(|p| p.id == pane))
    })
}

/// Prune pane views whenever the topology changes (set-difference — tmux
/// rebuilds topology wholesale, so there is no per-pane removal event).
async fn gc_loop(app: Arc<App>) {
    let mut rx = app.topology.subscribe();
    loop {
        let live: HashSet<String> = rx
            .borrow_and_update()
            .iter()
            .flat_map(|s| s.windows.iter())
            .flat_map(|w| w.panes.iter())
            .map(|p| p.id.clone())
            .collect();
        app.pane_views.prune(&live);
        if rx.changed().await.is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// CLI: `remux stream` — the source-side client.
// ---------------------------------------------------------------------------

/// `remux stream --view <id> [--pane %N]`: read newline-delimited JSON snapshots
/// on stdin and forward them to the daemon as pane `pane`'s view. Blocks until
/// stdin closes (or the daemon goes away).
pub fn stream(
    state_dir: &Path,
    pane: Option<String>,
    view: String,
    actions: Option<PathBuf>,
) -> Result<()> {
    use std::io::{BufRead, Read, Write};

    let pane = pane
        .or_else(|| std::env::var("TMUX_PANE").ok())
        .context("no --pane and no $TMUX_PANE in the environment")?;

    let path = socket_path(state_dir);
    let mut sock = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("is the daemon running? (no pane-view socket at {path:?})"))?;

    // Header, then read the ack under a deadline so a hung daemon can't wedge us.
    let header = serde_json::json!({ "pane": &pane, "view": &view });
    writeln!(sock, "{header}")?;
    sock.flush()?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut ackline = String::new();
    std::io::BufReader::new(sock.try_clone()?)
        .take(4096)
        .read_line(&mut ackline)
        .context("no ack from the daemon (timed out)")?;
    sock.set_read_timeout(None)?;
    let ack: Value = serde_json::from_str(ackline.trim())
        .with_context(|| format!("unexpected ack from daemon: {ackline:?}"))?;
    if ack["ok"] != serde_json::json!(true) {
        anyhow::bail!(
            "remux stream rejected: {}",
            ack["error"].as_str().unwrap_or("unknown")
        );
    }
    // Feedback in the (otherwise silent, since stdout is piped in) pane.
    eprintln!(
        "remux: streaming '{view}' for pane {pane} — open the Dashboard on your phone (Ctrl-C to stop)"
    );

    // Back-channel: if --actions was given, a detached thread relays daemon
    // action lines to that path (a FIFO the source reads). It holds a socket
    // CLONE, so on stdin EOF we must explicitly shutdown() the socket — dropping
    // our handle alone would not close the connection while the clone lives, and
    // the daemon would never see EOF to drop the view (Codex finding 3).
    if let Some(actions_path) = actions {
        let rsock = sock.try_clone()?;
        // Open read+write, NOT write-only: opening a FIFO write-only blocks until
        // a reader appears, which would hang the client (holding the daemon view)
        // if the source hasn't opened its read end. O_RDWR on a FIFO never blocks
        // (Codex finding 3); we only ever write to it.
        let mut out = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false) // a FIFO/existing file must not be truncated
            .open(&actions_path)
            .with_context(|| format!("open --actions path {actions_path:?}"))?;
        // Detached, NOT joined: a relay blocked writing to a full/undrained FIFO
        // can't be interrupted by the socket shutdown, so joining could hang
        // teardown. The daemon already learns of EOF from our `shutdown(Both)`;
        // the OS reaps this thread on process exit.
        std::thread::spawn(move || {
            let mut lines = std::io::BufReader::new(rsock);
            let mut line = String::new();
            loop {
                line.clear();
                match lines.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // daemon closed / socket shut down
                    Ok(_) => {
                        // Relay verbatim (one JSON line per action); if the source
                        // stops reading the FIFO, bail so we don't spin.
                        if out
                            .write_all(line.as_bytes())
                            .and_then(|_| out.flush())
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }

    // Forward stdin lines until EOF, each capped at MAX_LINE (like the daemon)
    // so a runaway source can't allocate unbounded.
    let mut reader = std::io::stdin().lock().take(MAX_LINE);
    let mut buf: Vec<u8> = Vec::new();
    let result = loop {
        reader.set_limit(MAX_LINE);
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break Ok(()); // stdin EOF
        }
        if buf.last() != Some(&b'\n') {
            // Over-cap line: drain the rest of it (bounded MAX_LINE chunks) up to
            // the real newline, so its suffix isn't forwarded as a bogus new line.
            loop {
                reader.set_limit(MAX_LINE);
                buf.clear();
                let m = reader.read_until(b'\n', &mut buf)?;
                if m == 0 || buf.last() == Some(&b'\n') {
                    break;
                }
            }
            continue;
        }
        let line = trim_line(&buf);
        if line.is_empty() {
            continue;
        }
        if sock
            .write_all(line)
            .and_then(|_| sock.write_all(b"\n"))
            .is_err()
        {
            break Err(anyhow::anyhow!("daemon closed the pane-view stream"));
        }
        sock.flush().ok();
    };

    // Signal the daemon (EOF) so it drops the view; the detached relay thread (if
    // any) exits on the resulting socket close, or is reaped on process exit.
    let _ = sock.shutdown(std::net::Shutdown::Both);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(workers: usize) -> Value {
        let ws: Vec<Value> = (0..workers)
            .map(|i| json!({"name": format!("w{i}"), "status": "running", "cpu": 1, "mem": 2, "progress": 3}))
            .collect();
        json!({ "t": 0, "workers": ws })
    }

    // Claim as a socket source, dropping the action back-channel — for the tests
    // that only exercise the state registry, not action delivery.
    fn claim<'a>(reg: &Registry, pane: &'a str, view: &'a str) -> Result<ClaimGuard, &'static str> {
        reg.claim_socket(pane, view).map(|(g, _rx)| g)
    }

    #[test]
    fn claim_update_snapshot_and_rev() {
        let reg = Registry::default();
        assert!(reg.snapshot().is_empty());
        let g = claim(&reg, "%1", "taskscope.v1").unwrap();
        // Claimed but no snapshot yet → not renderable.
        assert!(reg.snapshot().is_empty());
        assert_eq!(g.update(ts(2)), Some(1));
        assert_eq!(g.update(ts(3)), Some(2));
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].pane, "%1");
        assert_eq!(snap[0].view, "taskscope.v1");
        assert_eq!(snap[0].rev, 2);
    }

    #[test]
    fn one_live_view_per_pane_and_unknown_view_rejected() {
        let reg = Registry::default();
        let _g = claim(&reg, "%1", "taskscope.v1").unwrap();
        assert_eq!(
            claim(&reg, "%1", "taskscope.v1").err(),
            Some("pane already has a live view")
        );
        assert_eq!(claim(&reg, "%2", "nope.v9").err(), Some("unknown view"));
    }

    #[test]
    fn dropping_the_guard_removes_the_view() {
        let reg = Registry::default();
        {
            let g = claim(&reg, "%1", "taskscope.v1").unwrap();
            g.update(ts(1));
            assert_eq!(reg.snapshot().len(), 1);
        }
        assert!(reg.snapshot().is_empty(), "guard drop must remove the view");
        // Pane is free to re-claim.
        assert!(claim(&reg, "%1", "taskscope.v1").is_ok());
    }

    #[test]
    fn a_stale_guard_never_clobbers_a_reclaimed_pane() {
        let reg = Registry::default();
        let old = claim(&reg, "%1", "taskscope.v1").unwrap();
        // Pane vanished + re-claimed by a new source.
        reg.prune(&HashSet::new());
        let new = claim(&reg, "%1", "taskscope.v1").unwrap();
        new.update(ts(1));
        // The stale guard's update must be a no-op, and its Drop must not remove
        // the new owner's entry.
        assert_eq!(old.update(ts(9)), None);
        drop(old);
        assert_eq!(reg.snapshot().len(), 1, "new owner survives the stale drop");
        assert_eq!(reg.snapshot()[0].rev, 1);
    }

    #[test]
    fn prune_drops_missing_panes() {
        let reg = Registry::default();
        let _a = claim(&reg, "%1", "taskscope.v1").unwrap();
        _a.update(ts(1));
        let _b = claim(&reg, "%2", "taskscope.v1").unwrap();
        _b.update(ts(1));
        reg.prune(&HashSet::from(["%1".to_string()]));
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].pane, "%1");
    }

    #[test]
    fn validate_taskscope() {
        assert!(validate("taskscope.v1", &ts(3)).is_ok());
        assert!(validate("taskscope.v1", &json!({"workers": []})).is_ok());
        // Wrong shapes.
        assert!(validate("taskscope.v1", &json!([1, 2, 3])).is_err());
        assert!(validate("taskscope.v1", &json!({"workers": "no"})).is_err());
        assert!(validate("taskscope.v1", &json!({"workers": [{"name": "x"}]})).is_err());
        assert!(validate(
            "taskscope.v1",
            &json!({"workers": [{"name": 1, "status": "s", "cpu": 0, "mem": 0, "progress": 0}]})
        )
        .is_err());
        assert!(validate("other.v1", &ts(1)).is_err());
        assert!(validate("taskscope.v1", &ts(MAX_WORKERS + 1)).is_err());
    }

    fn ts_menu(opts: Value) -> Value {
        json!({ "workers": [], "menu": { "title": "t", "options": opts } })
    }

    #[test]
    fn validate_menu_shapes() {
        // Good: one option with an explicit session policy.
        assert!(validate(
            "taskscope.v1",
            &ts_menu(json!([{"label": "Pause", "action": "pause", "requires": "session"}]))
        )
        .is_ok());
        // Good: danger style.
        assert!(validate(
            "taskscope.v1",
            &ts_menu(json!([{"label": "Del", "action": "del:1", "style": "danger"}]))
        )
        .is_ok());
        // No menu at all is fine.
        assert!(validate("taskscope.v1", &json!({"workers": []})).is_ok());
        // Bad: empty options, too many options, bad token grammar, dup token,
        // bad style/requires enums, non-string label.
        assert!(validate("taskscope.v1", &ts_menu(json!([]))).is_err());
        let many: Vec<Value> = (0..MAX_MENU_OPTIONS + 1)
            .map(|i| json!({"label": "x", "action": format!("a{i}")}))
            .collect();
        assert!(validate("taskscope.v1", &ts_menu(json!(many))).is_err());
        assert!(validate(
            "taskscope.v1",
            &ts_menu(json!([{"label": "x", "action": "bad token"}]))
        )
        .is_err());
        assert!(validate(
            "taskscope.v1",
            &ts_menu(json!([{"label": "x", "action": "touch /tmp/p"}]))
        )
        .is_err());
        assert!(validate(
            "taskscope.v1",
            &ts_menu(json!([
                {"label": "a", "action": "dup"},
                {"label": "b", "action": "dup"}
            ]))
        )
        .is_err());
        assert!(validate(
            "taskscope.v1",
            &ts_menu(json!([{"label": "x", "action": "ok", "style": "boom"}]))
        )
        .is_err());
        assert!(validate(
            "taskscope.v1",
            &ts_menu(json!([{"label": "x", "action": "ok", "requires": "root"}]))
        )
        .is_err());
        assert!(validate(
            "taskscope.v1",
            &ts_menu(json!([{"label": 1, "action": "ok"}]))
        )
        .is_err());
    }

    #[test]
    fn action_token_grammar() {
        assert!(is_valid_action_token("pause"));
        assert!(is_valid_action_token("del:run-123.v2_final"));
        assert!(!is_valid_action_token("")); // empty
        assert!(!is_valid_action_token("has space"));
        assert!(!is_valid_action_token("line\nbreak"));
        assert!(!is_valid_action_token("sep\u{2028}here")); // U+2028 (is_control misses it)
        assert!(!is_valid_action_token(&"a".repeat(MAX_ACTION_TOKEN + 1)));
    }

    #[test]
    fn socket_source_cannot_claim_internal_view() {
        let reg = Registry::default();
        // htop.v1 is internal-only: an external source is rejected...
        assert_eq!(
            reg.claim_socket("%1", "htop.v1").err(),
            Some("view not available to external sources")
        );
        // ...but the in-process adapter may claim it.
        assert!(reg.claim_internal("%1", "htop.v1").is_ok());
    }

    #[test]
    fn action_kind_by_provenance() {
        let reg = Registry::default();
        let _h = reg.claim_internal("%1", "htop.v1").unwrap();
        let (_s, _rx) = reg.claim_socket("%2", "taskscope.v1").unwrap();
        assert_eq!(reg.action_kind("%1"), Some(ActionKind::Htop));
        assert_eq!(reg.action_kind("%2"), Some(ActionKind::Source));
        assert_eq!(reg.action_kind("%9"), None);
    }

    #[test]
    fn send_action_membership_and_policy() {
        let reg = Registry::default();
        let (guard, mut rx) = reg.claim_socket("%1", "taskscope.v1").unwrap();
        // Advertise: `pause` (session) and `wipe` (danger → approve).
        guard.update(ts_menu(json!([
            {"label": "Pause", "action": "pause", "requires": "session"},
            {"label": "Wipe", "action": "wipe", "style": "danger"}
        ])));

        // Not advertised → dropped.
        assert!(!reg.send_action("%1", "nope", true));
        // Session action: any in-session device (no approve) is OK.
        assert!(reg.send_action("%1", "pause", false));
        assert_eq!(rx.try_recv().ok(), Some("pause".to_string()));
        // Approve action: rejected without approve, accepted with it.
        assert!(!reg.send_action("%1", "wipe", false));
        assert!(reg.send_action("%1", "wipe", true));
        assert_eq!(rx.try_recv().ok(), Some("wipe".to_string()));

        // A new snapshot without the menu revokes both actions.
        guard.update(json!({"workers": []}));
        assert!(!reg.send_action("%1", "pause", true));
    }

    #[test]
    fn send_action_bounded_queue_drops_when_full() {
        let reg = Registry::default();
        let (guard, _rx) = reg.claim_socket("%1", "taskscope.v1").unwrap();
        guard.update(ts_menu(
            json!([{"label": "P", "action": "pause", "requires": "session"}]),
        ));
        // Never draining rx: the first ACTION_QUEUE sends fit, the next is dropped.
        for _ in 0..ACTION_QUEUE {
            assert!(reg.send_action("%1", "pause", true));
        }
        assert!(
            !reg.send_action("%1", "pause", true),
            "full queue must drop"
        );
    }

    #[test]
    fn internal_htop_has_no_back_channel() {
        let reg = Registry::default();
        let guard = reg.claim_internal("%1", "htop.v1").unwrap();
        // Even if an htop snapshot somehow carried a menu, an internal view has no
        // action_tx, so send_action can never forward.
        guard.update(json!({
            "processes": [],
            "menu": {"title": "t", "options": [{"label": "x", "action": "ok", "requires": "session"}]}
        }));
        assert!(!reg.send_action("%1", "ok", true));
    }

    // A real `tmux capture-pane -p` of htop (trimmed), including the meters, the
    // "CPU%-MEM%" sort-marker glitch in the header, a command with spaces, and
    // the function-key footer.
    const HTOP: &str = "\
    0[|||||||                                        10.1%]   4[||||||       7.6%]
    1[||||||                                          8.8%]   5[||||||       8.1%]
  Mem[|||||||||||||||||||||||||||||||||||||||||2.27G/3.83G] Tasks: 13, 10 thr, 0 kthr; 2 running
  Swp[|||||||||||||||||||||||||||||||||||||||||||430M/512M] Load average: 1.15 0.72 0.69
                                                            Uptime: 01:06:41

  [Main] [I/O]
  PID USER       PRI  NI  VIRT   RES   SHR S  CPU%-MEM%   TIME+  Command
    1 root        20   0  817M 10720  7736 S   0.0  0.3  0:04.36 /workspaces/remux/target/debug/remux serve --listen 0.0
 2243 root        20   0  4624  3080  2300 R   0.5  0.1  0:00.00 htop
F1Help  F2Setup F3SearchF4FilterF5Tree  F6SortByF7Nice -F8Nice +F9Kill  F10Quit";

    #[test]
    fn parse_htop_reads_the_process_table_and_summary() {
        let v = parse_htop(HTOP);
        assert_eq!(v["confidence"], "ok");
        let ps = v["processes"].as_array().unwrap();
        assert_eq!(
            ps.len(),
            2,
            "the footer and blank/meter lines are not processes"
        );

        assert_eq!(ps[0]["pid"], 1);
        assert_eq!(ps[0]["user"], "root");
        assert_eq!(ps[0]["cpu"], 0.0);
        assert_eq!(ps[0]["mem"], 0.3);
        assert_eq!(ps[0]["res"], "10720");
        assert_eq!(ps[0]["time"], "0:04.36");
        // The command keeps its spaces (it is the remainder, not a token).
        assert_eq!(
            ps[0]["command"],
            "/workspaces/remux/target/debug/remux serve --listen 0.0"
        );

        assert_eq!(ps[1]["pid"], 2243);
        assert_eq!(ps[1]["cpu"], 0.5);
        assert_eq!(ps[1]["command"], "htop");

        let s = &v["summary"];
        assert_eq!(s["cores"], 4); // two meter lines × two cores
        assert_eq!(s["mem"], "2.27G/3.83G");
        assert_eq!(s["load"], "1.15 0.72 0.69");
        assert_eq!(s["uptime"], "01:06:41");
        assert!(s["tasks"].as_str().unwrap().contains("13"));
    }

    // A NARROW pane (≈64 cols, a phone driving the size): htop drops the Command
    // column. We should still parse the visible columns, command empty.
    const HTOP_NARROW: &str = "\
  Mem[||||||||||||||||||||||1.9G/3.8G] Tasks: 40, 1 thr; 1 running
  PID USER       PRI  NI  VIRT   RES   SHR S  CPU%-MEM%   TIME+
    1 root        20   0  883M 19812 17080 S   0.4  0.5  0:00.68
   18 root        20   0  8268  3828  2784 S   0.1  0.1  0:00.17
F1Help  F2Setup F3SearchF4FilterF5Tree  F6SortBy";

    #[test]
    fn parse_htop_tolerates_a_narrow_pane_without_the_command_column() {
        let v = parse_htop(HTOP_NARROW);
        assert_eq!(
            v["confidence"], "ok",
            "still parses without a Command column"
        );
        let ps = v["processes"].as_array().unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0]["pid"], 1);
        assert_eq!(ps[0]["cpu"], 0.4);
        assert_eq!(ps[0]["mem"], 0.5);
        assert_eq!(ps[0]["res"], "19812");
        assert_eq!(ps[0]["command"], ""); // no Command column at this width
    }

    #[test]
    fn dashboard_actions_are_whitelisted() {
        use HtopAction::*;
        assert_eq!(parse_htop_action("sort:cpu"), Some(Key("P")));
        assert_eq!(parse_htop_action("sort:mem"), Some(Key("M")));
        assert_eq!(parse_htop_action("sort:time"), Some(Key("T")));
        assert_eq!(parse_htop_action("invert"), Some(Key("I")));
        assert_eq!(parse_htop_action("tree"), Some(Named("F5")));
        assert_eq!(
            parse_htop_action("filter:remux"),
            Some(Filter("remux".into()))
        );
        // kill defaults to TERM; the signal is an explicit whitelist.
        assert_eq!(
            parse_htop_action("kill:4702"),
            Some(Kill {
                pid: 4702,
                signal: KillSignal::Term
            })
        );
        assert_eq!(
            parse_htop_action("kill:4702:TERM"),
            Some(Kill {
                pid: 4702,
                signal: KillSignal::Term
            })
        );
        assert_eq!(
            parse_htop_action("kill:4702:KILL"),
            Some(Kill {
                pid: 4702,
                signal: KillSignal::Kill
            })
        );
        // A filter strips control chars (no smuggling Enter/keys into htop).
        assert_eq!(
            parse_htop_action("filter:a\nb\tc"),
            Some(Filter("abc".into()))
        );
        // Rejected: unknown, non-numeric kill, non-whitelisted signal, raw keys.
        assert_eq!(parse_htop_action("sort:bogus"), None);
        assert_eq!(parse_htop_action("kill:; rm -rf"), None);
        assert_eq!(parse_htop_action("kill:4702:HUP"), None);
        assert_eq!(parse_htop_action("kill:4702:9"), None);
        assert_eq!(parse_htop_action("q"), None);
        assert_eq!(parse_htop_action("rm -rf /"), None);
    }

    #[test]
    fn parse_htop_is_low_confidence_on_non_htop_and_never_invents_rows() {
        let v = parse_htop("just some\nrandom terminal output\n$ ls\n");
        assert_eq!(v["confidence"], "low");
        assert_eq!(v["processes"].as_array().unwrap().len(), 0);
        // Valid as an htop.v1 payload (processes is an array), so the socket path
        // and the renderer agree on the shape.
        assert!(validate("htop.v1", &v).is_ok());
    }
}
