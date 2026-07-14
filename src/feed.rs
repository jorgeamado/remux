//! M4c command feed: a bounded, in-memory, per-session history of shell
//! commands, fed by out-of-band zsh hooks (`command_started` on preexec,
//! `command_finished` on precmd). **Informational only** — like all shell-hook
//! events, any same-uid process can forge these, so they may never trigger an
//! action; at worst they pollute this feed. No command output is ever stored
//! (that would be the gated M4d); only metadata.
//!
//! Correlation and ordering: the shell mints a `shell_id` (once per interactive
//! shell) and a per-shell monotonic `command_id`; both ride start and finish.
//! The two events are delivered by *separate* fire-and-forget datagrams, so
//! they can arrive **out of order** — the store is built to tolerate that
//! (Codex review): a finish arriving before its start is held as `pending`; a
//! delayed lower-id start cannot supersede a newer command (per-shell
//! high-water mark). Pairing is by `(shell_id, command_id)`, never "newest on
//! the pane".
//!
//! Lifecycle wakes: a reconcile-on-hint model never wakes for a shell that died
//! mid-command or for age-based eviction — so a timer [`Feed::sweep`] marks
//! stale-running entries aborted, evicts by age/count, and fires the hint.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// Max stored entries per session (enforced in the store, not just the view).
pub const SESSION_MAX: usize = 200;
/// Global entry cap across all sessions — abandoned sessions must not leak.
const GLOBAL_MAX: usize = 5000;
/// Max buffered finishes that arrived before their start.
const PENDING_MAX: usize = 512;
/// Evict completed entries older than this.
const AGE: Duration = Duration::from_secs(12 * 3600);
/// A still-"running" entry older than this is marked aborted.
const STALE_RUNNING: Duration = Duration::from_secs(6 * 3600);
const MAX_COMMAND_BYTES: usize = 512;
const MAX_CWD_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Done {
        exit: i32,
        elapsed_ms: u64,
    },
    /// Superseded by a newer command on the same shell, or swept as stale.
    Aborted,
}

#[derive(Clone, Debug)]
struct Cmd {
    id: u64, // daemon-minted, monotonic — the client keys/dedupes on this
    shell_id: String,
    command_id: u64,
    session: String,
    pane: String,
    command: String,
    cwd: String,
    started: Instant,
    started_unix: u64,
    state: State,
}

impl Cmd {
    fn view(&self, now: Instant) -> serde_json::Value {
        let (state, exit, elapsed_ms) = match &self.state {
            State::Running => ("running", serde_json::Value::Null, serde_json::Value::Null),
            State::Done { exit, elapsed_ms } => ("done", (*exit).into(), (*elapsed_ms).into()),
            State::Aborted => ("aborted", serde_json::Value::Null, serde_json::Value::Null),
        };
        serde_json::json!({
            "id": self.id,
            "session": self.session,
            "pane": self.pane,
            "command": self.command,
            "cwd": self.cwd,
            "state": state,
            "exit": exit,
            "elapsed_ms": elapsed_ms,
            "started_unix": self.started_unix,
            "age_ms": now.saturating_duration_since(self.started).as_millis() as u64,
        })
    }
}

/// A matched finish — the input to M4c's precise attention (increment 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub session: String,
    pub exit: i32,
    pub elapsed_ms: u64,
}

struct Inner {
    cmds: Vec<Cmd>, // insertion order == chronological (by daemon id)
    next_id: u64,
    /// (shell_id, command_id) -> index into `cmds`.
    by_key: HashMap<(String, u64), usize>,
    /// shell_id -> highest command_id seen (survives eviction of the entry).
    shell_max: HashMap<String, u64>,
    /// Finishes that arrived before their start: (shell_id, command_id) -> exit.
    pending: HashMap<(String, u64), i32>,
}

impl Inner {
    /// Rebuild `by_key` after any structural change (eviction). O(n), only on
    /// the rare eviction path — the common insert path updates it in place.
    fn reindex(&mut self) {
        self.by_key.clear();
        for (i, c) in self.cmds.iter().enumerate() {
            self.by_key.insert((c.shell_id.clone(), c.command_id), i);
        }
    }
}

pub struct Feed {
    inner: Mutex<Inner>,
    events: broadcast::Sender<()>,
}

impl Default for Feed {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                cmds: Vec::new(),
                next_id: 1,
                by_key: HashMap::new(),
                shell_max: HashMap::new(),
                pending: HashMap::new(),
            }),
            events: broadcast::channel(16).0,
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Strip control chars, then truncate to a byte budget at a char boundary.
fn cap_bytes(s: &str, max_bytes: usize) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control()).collect();
    if clean.len() <= max_bytes {
        return clean;
    }
    let mut end = max_bytes;
    while end > 0 && !clean.is_char_boundary(end) {
        end -= 1;
    }
    clean[..end].to_string()
}

impl Feed {
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.events.subscribe()
    }

    /// Record a command start. Order-tolerant and idempotent:
    /// - duplicate `(shell_id, command_id)` → no-op;
    /// - a buffered pending finish for this key → the entry lands already Done;
    /// - a start whose id is below the shell's high-water mark (a delayed start
    ///   for an earlier command) is recorded Aborted, and never supersedes the
    ///   newer command;
    /// - the newest start aborts any still-running predecessor on the shell.
    pub fn start(
        &self,
        session: &str,
        pane: &str,
        shell_id: &str,
        command_id: u64,
        command: &str,
        cwd: &str,
    ) {
        {
            let inner = &mut *self.inner.lock().unwrap();
            let key = (shell_id.to_string(), command_id);
            if inner.by_key.contains_key(&key) {
                return; // duplicate start
            }
            let prev_max = inner.shell_max.get(shell_id).copied();
            let is_latest = prev_max.is_none_or(|m| command_id >= m);

            // Newest start ends any still-running predecessor on this shell.
            if is_latest {
                for c in inner.cmds.iter_mut() {
                    if c.shell_id == shell_id
                        && c.state == State::Running
                        && c.command_id < command_id
                    {
                        c.state = State::Aborted;
                    }
                }
            }

            let state = if let Some(exit) = inner.pending.remove(&key) {
                State::Done {
                    exit,
                    elapsed_ms: 0,
                } // finish beat the start; elapsed unknown
            } else if is_latest {
                State::Running
            } else {
                State::Aborted // a newer command already started
            };

            let id = inner.next_id;
            inner.next_id += 1;
            let now = Instant::now();
            inner.cmds.push(Cmd {
                id,
                shell_id: shell_id.to_string(),
                command_id,
                session: session.to_string(),
                pane: pane.to_string(),
                command: cap_bytes(command, MAX_COMMAND_BYTES),
                cwd: cap_bytes(cwd, MAX_CWD_BYTES),
                started: now,
                started_unix: now_unix(),
                state,
            });
            let idx = inner.cmds.len() - 1;
            inner.by_key.insert(key, idx);
            inner
                .shell_max
                .entry(shell_id.to_string())
                .and_modify(|m| *m = (*m).max(command_id))
                .or_insert(command_id);

            if self.enforce_bounds(inner, session) {
                inner.reindex();
            }
        }
        let _ = self.events.send(());
    }

    /// Record a command finish, paired by `(shell_id, command_id)`. A finish
    /// with no matching start yet is buffered (bounded) and applied when the
    /// start arrives. Returns the finished command's session/exit/elapsed when a
    /// *running* start matched; `None` otherwise (duplicate, buffered, or a
    /// finish for an already-aborted/superseded entry). Fires the hint on a real
    /// transition.
    pub fn finish(&self, shell_id: &str, command_id: u64, exit: i32) -> Option<Finished> {
        let result = {
            let inner = &mut *self.inner.lock().unwrap();
            let key = (shell_id.to_string(), command_id);
            match inner.by_key.get(&key).copied() {
                Some(idx) => {
                    let now = Instant::now();
                    let cmd = &mut inner.cmds[idx];
                    if cmd.state != State::Running {
                        return None; // duplicate finish, or superseded entry
                    }
                    let elapsed_ms = now.saturating_duration_since(cmd.started).as_millis() as u64;
                    cmd.state = State::Done { exit, elapsed_ms };
                    Some(Finished {
                        session: cmd.session.clone(),
                        exit,
                        elapsed_ms,
                    })
                }
                None => {
                    // Finish beat its start — buffer it (bounded) for the start.
                    if inner.pending.len() < PENDING_MAX {
                        inner.pending.insert(key, exit);
                    }
                    None
                }
            }
        };
        if result.is_some() {
            let _ = self.events.send(());
        }
        result
    }

    /// The session's recent commands, newest last, capped to `SESSION_MAX`.
    pub fn snapshot(&self, session: &str) -> Vec<serde_json::Value> {
        let inner = self.inner.lock().unwrap();
        let now = Instant::now();
        inner
            .cmds
            .iter()
            .filter(|c| c.session == session)
            .map(|c| c.view(now))
            .collect()
    }

    /// Recent command *strings* for this session, newest first, deduped — the
    /// source for the composer's ↑ recall (increment 4). Memory-only; the
    /// caller must never persist these.
    pub fn recent_commands(&self, session: &str, max: usize) -> Vec<String> {
        if max == 0 {
            return Vec::new();
        }
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<String> = Vec::new();
        for c in inner.cmds.iter().rev() {
            if c.session != session || c.command.is_empty() {
                continue;
            }
            if !out.iter().any(|s| s == &c.command) {
                out.push(c.command.clone());
                if out.len() >= max {
                    break;
                }
            }
        }
        out
    }

    /// Timer maintenance: mark stale-running entries aborted, evict aged ones,
    /// drop stale pending finishes. Fires the hint if anything changed.
    pub fn sweep(&self) {
        let changed = {
            let inner = &mut *self.inner.lock().unwrap();
            let now = Instant::now();
            let before = inner.cmds.len();
            let mut mutated = false;
            for c in inner.cmds.iter_mut() {
                if c.state == State::Running
                    && now.saturating_duration_since(c.started) > STALE_RUNNING
                {
                    c.state = State::Aborted;
                    mutated = true;
                }
            }
            inner.cmds.retain(|c| {
                c.state == State::Running || now.saturating_duration_since(c.started) <= AGE
            });
            if inner.cmds.len() != before {
                mutated = true;
            }
            // A pending finish that never got a start is a leak — after a sweep
            // interval it's abandoned. Bounded already, but clear on sweep.
            if !inner.pending.is_empty() {
                inner.pending.clear();
                mutated = true;
            }
            if mutated {
                inner.reindex();
            }
            mutated
        };
        if changed {
            let _ = self.events.send(());
        }
    }

    /// Enforce per-session then global caps. Evicts the oldest *completed* entry
    /// first (a running entry is an active command). Returns whether it removed
    /// anything (caller reindexes). Runs under the lock.
    fn enforce_bounds(&self, inner: &mut Inner, session: &str) -> bool {
        let mut removed = false;
        // Per-session cap.
        loop {
            let count = inner.cmds.iter().filter(|c| c.session == session).count();
            if count <= SESSION_MAX {
                break;
            }
            let pos = inner
                .cmds
                .iter()
                .position(|c| c.session == session && c.state != State::Running)
                .or_else(|| inner.cmds.iter().position(|c| c.session == session));
            match pos {
                Some(p) => {
                    inner.cmds.remove(p);
                    removed = true;
                }
                None => break,
            }
        }
        // Global cap.
        while inner.cmds.len() > GLOBAL_MAX {
            let pos = inner
                .cmds
                .iter()
                .position(|c| c.state != State::Running)
                .unwrap_or(0);
            inner.cmds.remove(pos);
            removed = true;
        }
        removed
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().cmds.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().cmds.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_of(v: &serde_json::Value) -> &str {
        v["state"].as_str().unwrap()
    }

    #[test]
    fn start_then_finish_pairs_by_correlation() {
        let f = Feed::default();
        f.start("s", "%1", "sh1", 1, "cargo build", "/w");
        let fin = f.finish("sh1", 1, 101).unwrap();
        assert_eq!(fin.exit, 101);
        let snap = f.snapshot("s");
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0]["command"], "cargo build");
        assert_eq!(state_of(&snap[0]), "done");
        assert_eq!(snap[0]["exit"], 101);
    }

    #[test]
    fn finish_before_start_is_buffered_then_applied() {
        let f = Feed::default();
        // The two datagrams raced; finish landed first.
        assert!(f.finish("sh1", 1, 0).is_none());
        assert!(f.is_empty());
        f.start("s", "%1", "sh1", 1, "ls", "/w");
        let snap = f.snapshot("s");
        assert_eq!(snap.len(), 1);
        assert_eq!(state_of(&snap[0]), "done"); // pending finish applied
        assert_eq!(snap[0]["exit"], 0);
    }

    #[test]
    fn delayed_lower_start_does_not_supersede_newer() {
        let f = Feed::default();
        f.start("s", "%1", "sh1", 2, "newer", "/w"); // #2 first
        f.start("s", "%1", "sh1", 1, "delayed-older", "/w"); // #1 arrives late
        let snap = f.snapshot("s");
        // #2 must stay running; the late #1 is aborted, not the other way round.
        let newer = snap.iter().find(|c| c["command"] == "newer").unwrap();
        let older = snap
            .iter()
            .find(|c| c["command"] == "delayed-older")
            .unwrap();
        assert_eq!(state_of(newer), "running");
        assert_eq!(state_of(older), "aborted");
    }

    #[test]
    fn duplicate_start_and_finish_are_noops() {
        let f = Feed::default();
        f.start("s", "%1", "sh1", 1, "ls", "/w");
        f.start("s", "%1", "sh1", 1, "ls-again", "/w");
        assert_eq!(f.len(), 1);
        assert!(f.finish("sh1", 1, 0).is_some());
        assert!(f.finish("sh1", 1, 0).is_none());
    }

    #[test]
    fn new_start_supersedes_unfinished_on_same_shell() {
        let f = Feed::default();
        f.start("s", "%1", "sh1", 1, "sleep 100", "/w");
        f.start("s", "%1", "sh1", 2, "ls", "/w");
        let snap = f.snapshot("s");
        assert_eq!(state_of(&snap[0]), "aborted");
        assert_eq!(state_of(&snap[1]), "running");
        assert!(f.finish("sh1", 1, 0).is_none());
    }

    #[test]
    fn nested_shells_are_independent() {
        let f = Feed::default();
        f.start("s", "%1", "outer", 1, "bash", "/w");
        f.start("s", "%1", "inner", 1, "ls", "/w");
        let snap = f.snapshot("s");
        assert_eq!(snap.iter().filter(|c| state_of(c) == "running").count(), 2);
    }

    #[test]
    fn recent_commands_dedupes_newest_first_and_respects_zero() {
        let f = Feed::default();
        f.start("s", "%1", "sh1", 1, "ls", "/w");
        f.finish("sh1", 1, 0);
        f.start("s", "%1", "sh1", 2, "cargo test", "/w");
        f.finish("sh1", 2, 0);
        f.start("s", "%1", "sh1", 3, "ls", "/w");
        assert_eq!(
            f.recent_commands("s", 10),
            vec!["ls".to_string(), "cargo test".to_string()]
        );
        assert!(f.recent_commands("s", 0).is_empty());
    }

    #[test]
    fn per_session_storage_is_bounded() {
        let f = Feed::default();
        for i in 0..(SESSION_MAX + 50) as u64 {
            f.start("s", "%1", "sh1", i, &format!("cmd{i}"), "/w");
            f.finish("sh1", i, 0);
        }
        // Stored (not just the view) is capped at SESSION_MAX.
        assert!(f.len() <= SESSION_MAX);
        assert_eq!(f.snapshot("s").len(), f.len());
    }

    #[test]
    fn snapshot_is_session_scoped() {
        let f = Feed::default();
        f.start("a", "%1", "sh1", 1, "in-a", "/w");
        f.start("b", "%2", "sh2", 1, "in-b", "/w");
        assert_eq!(f.snapshot("a").len(), 1);
        assert_eq!(f.snapshot("a")[0]["command"], "in-a");
        assert_eq!(f.snapshot("b").len(), 1);
    }

    #[tokio::test]
    async fn start_and_finish_fire_hints() {
        let f = Feed::default();
        let mut rx = f.subscribe();
        f.start("s", "%1", "sh1", 1, "ls", "/w");
        assert!(rx.try_recv().is_ok());
        f.finish("sh1", 1, 0);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn command_and_cwd_are_byte_capped_and_sanitized() {
        let f = Feed::default();
        let long = "x".repeat(1000);
        f.start(
            "s",
            "%1",
            "sh1",
            1,
            &format!("a\x1b[31m{long}"),
            &"y".repeat(1000),
        );
        let snap = f.snapshot("s");
        let cmd = snap[0]["command"].as_str().unwrap();
        assert!(!cmd.contains('\x1b'));
        assert!(cmd.len() <= MAX_COMMAND_BYTES);
        assert!(snap[0]["cwd"].as_str().unwrap().len() <= MAX_CWD_BYTES);
    }
}
