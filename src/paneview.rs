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

use crate::App;
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
use tokio::sync::broadcast;

/// Socket filename in the state dir.
pub const SOCKET: &str = "pane-view.sock";

/// Built-in view identifiers. A source may only claim one of these; the PWA
/// ships a hard-coded renderer for each. (No third-party/plugin views yet.)
pub const KNOWN_VIEWS: &[&str] = &["taskscope.v1", "htop.v1"];

/// Foreground commands the daemon auto-captures into a pane view (best-effort
/// "semantic lens" over the real tool's rendered screen — see `parse_htop`).
const CAPTURE_TOOLS: &[&str] = &["htop"];
/// How often the capture adapter re-reads a pane's screen.
const CAPTURE_INTERVAL: Duration = Duration::from_millis(1500);
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

/// Latest structured state for one pane.
struct Entry {
    view: String,
    /// Distinguishes the owning connection so a stale guard can't clobber a
    /// pane that was re-claimed by a newer source.
    instance: u64,
    rev: u64,
    /// `None` until the first snapshot — a claimed-but-empty pane shows nothing.
    state: Option<Value>,
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

    /// Claim `pane` for `view`. Fails if the view is unknown or the pane already
    /// has a live view. The returned guard owns the entry: dropping it (on EOF /
    /// task exit) removes the pane's view.
    pub fn claim(&self, pane: &str, view: &str) -> Result<ClaimGuard, &'static str> {
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
            },
        );
        // No hint yet: nothing is renderable until the first snapshot lands.
        Ok(ClaimGuard {
            inner: self.inner.clone(),
            events: self.events.clone(),
            pane: pane.to_string(),
            instance,
        })
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
        e.state = Some(state);
        let rev = e.rev;
        drop(map);
        let _ = self.events.send(());
        Some(rev)
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

/// Map a whitelisted dashboard *action* for a view to the literal key(s) to send
/// to the real tool. Returns `None` for an unknown (view, action) pair — so a
/// client can only trigger vetted actions, never arbitrary keystrokes.
pub fn action_keys(view: &str, action: &str) -> Option<&'static str> {
    match (view, action) {
        ("htop.v1", "sort:cpu") => Some("P"),
        ("htop.v1", "sort:mem") => Some("M"),
        ("htop.v1", "sort:time") => Some("T"),
        _ => None,
    }
}

/// Validate a snapshot's shape for `view`. Kept strict but minimal — enough that
/// the PWA renderer can trust the structure. Per-view knowledge lives here for
/// now (one built-in view); a declarative schema can replace this later.
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
            Ok(())
        }
        "htop.v1" => {
            if obj.get("processes").is_some_and(Value::is_array) {
                Ok(())
            } else {
                Err("htop.v1: `processes` must be an array")
            }
        }
        _ => Err("unknown view"),
    }
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
        let mut rx = app.topology.subscribe();
        let mut tasks: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
        loop {
            let want: HashSet<String> = rx
                .borrow_and_update()
                .iter()
                .flat_map(|s| s.windows.iter())
                .flat_map(|w| w.panes.iter())
                .filter(|p| CAPTURE_TOOLS.contains(&p.command.as_str()))
                .map(|p| p.id.clone())
                .collect();
            for pane in &want {
                if !tasks.contains_key(pane) {
                    let app = app.clone();
                    let p = pane.clone();
                    let h = tokio::spawn(async move { capture_task(app, p).await });
                    tasks.insert(pane.clone(), h.abort_handle());
                }
            }
            // Stop tasks whose pane is no longer running the tool (aborting drops
            // the task's ClaimGuard → the view is removed).
            tasks.retain(|pane, ah| {
                let keep = want.contains(pane) && !ah.is_finished();
                if !keep {
                    ah.abort();
                }
                keep
            });
            if rx.changed().await.is_err() {
                break;
            }
        }
    });
}

async fn capture_task(app: Arc<App>, pane: String) {
    // Claim the pane's view; if it already has one (e.g. a `remux stream`), skip.
    let guard = match app.pane_views.claim(&pane, "htop.v1") {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut ticker = tokio::time::interval(CAPTURE_INTERVAL);
    loop {
        ticker.tick().await;
        let p = pane.clone();
        let captured = tokio::task::spawn_blocking(move || crate::tmux::capture_pane(&p)).await;
        let text = match captured {
            Ok(Ok(Some(t))) => t,
            _ => continue, // pane gone / tmux error — the spawner will stop us
        };
        guard.update(parse_htop(&text));
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
    let guard = match app.pane_views.claim(&header.pane, &header.view) {
        Ok(g) => g,
        Err(e) => return ack_err(&mut write, e).await,
    };
    write.write_all(b"{\"ok\":true}\n").await?;

    // --- snapshots ---
    // Latest-wins rate cap: coalesce snapshots into a single `pending` slot and
    // publish it at most once per MIN_UPDATE_INTERVAL. Unlike a plain drop, the
    // *final* snapshot of a source that then goes idle (but stays connected) is
    // never stranded — the timer flushes it within one interval.
    let mut pending: Option<Value> = None;
    let mut flush = tokio::time::interval(MIN_UPDATE_INTERVAL);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = flush.tick(), if pending.is_some() => {
                guard.update(pending.take().unwrap());
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
pub fn stream(state_dir: &Path, pane: Option<String>, view: String) -> Result<()> {
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
    sock.set_read_timeout(None)?; // we only write from here on
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

    // Forward stdin lines until EOF, each capped at MAX_LINE (like the daemon)
    // so a runaway source can't allocate unbounded. Closing the socket on EOF
    // signals the daemon to drop the view.
    let mut reader = std::io::stdin().lock().take(MAX_LINE);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        reader.set_limit(MAX_LINE);
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break; // stdin EOF
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
            anyhow::bail!("daemon closed the pane-view stream");
        }
        sock.flush().ok();
    }
    Ok(())
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

    #[test]
    fn claim_update_snapshot_and_rev() {
        let reg = Registry::default();
        assert!(reg.snapshot().is_empty());
        let g = reg.claim("%1", "taskscope.v1").unwrap();
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
        let _g = reg.claim("%1", "taskscope.v1").unwrap();
        assert_eq!(
            reg.claim("%1", "taskscope.v1").err(),
            Some("pane already has a live view")
        );
        assert_eq!(reg.claim("%2", "nope.v9").err(), Some("unknown view"));
    }

    #[test]
    fn dropping_the_guard_removes_the_view() {
        let reg = Registry::default();
        {
            let g = reg.claim("%1", "taskscope.v1").unwrap();
            g.update(ts(1));
            assert_eq!(reg.snapshot().len(), 1);
        }
        assert!(reg.snapshot().is_empty(), "guard drop must remove the view");
        // Pane is free to re-claim.
        assert!(reg.claim("%1", "taskscope.v1").is_ok());
    }

    #[test]
    fn a_stale_guard_never_clobbers_a_reclaimed_pane() {
        let reg = Registry::default();
        let old = reg.claim("%1", "taskscope.v1").unwrap();
        // Pane vanished + re-claimed by a new source.
        reg.prune(&HashSet::new());
        let new = reg.claim("%1", "taskscope.v1").unwrap();
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
        let _a = reg.claim("%1", "taskscope.v1").unwrap();
        _a.update(ts(1));
        let _b = reg.claim("%2", "taskscope.v1").unwrap();
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
    fn dashboard_actions_are_whitelisted_per_view() {
        assert_eq!(action_keys("htop.v1", "sort:cpu"), Some("P"));
        assert_eq!(action_keys("htop.v1", "sort:mem"), Some("M"));
        assert_eq!(action_keys("htop.v1", "sort:time"), Some("T"));
        // Unknown action / view / a raw keystroke smuggling attempt → nothing.
        assert_eq!(action_keys("htop.v1", "sort:bogus"), None);
        assert_eq!(action_keys("htop.v1", "q"), None);
        assert_eq!(action_keys("htop.v1", "rm -rf /"), None);
        assert_eq!(action_keys("taskscope.v1", "sort:cpu"), None);
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
