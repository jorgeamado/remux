//! Claude chat companion: tail a Claude Code session transcript (JSONL) into a
//! bounded, per-pane ring of *rendered* chat messages for the phone chat view.
//!
//! Transcript content is secrets-class (assistant text quotes file contents and
//! tool arguments), so it is NEVER put on the broadcast pane-view channel — the
//! WS layer serves it per-connection, gated on session membership. Thinking and
//! tool_result blocks are dropped entirely; a tool_use becomes a compact
//! "used tool: X" line (name only, no input).
//!
//! Continuity is keyed by a *generation* per pane: `(pane, session_id,
//! transcript_path)` plus truncation/rotation resets bump it, so a client that
//! sees a new generation re-snapshots instead of stitching unrelated messages.

use crate::App;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

/// Keep at most this many rendered messages / bytes per pane (oldest dropped).
const MAX_MESSAGES: usize = 120;
const MAX_BYTES: usize = 256 * 1024;
/// Cap a single rendered message's text.
const MAX_MSG_TEXT: usize = 8 * 1024;
/// How often a tailer re-reads its transcript.
const TAIL_POLL: Duration = Duration::from_millis(700);
/// How often the supervisor reconciles tailers against the agent registry.
const SUPERVISOR_POLL: Duration = Duration::from_millis(1000);
/// Max concurrent tailers.
const MAX_TAILERS: usize = 16;
/// Cap bytes read from the transcript per poll (bounds a huge append).
const READ_CHUNK: u64 = 1024 * 1024;

/// A rendered message before it enters the ring: (role, kind, text).
pub type Rendered = (&'static str, &'static str, String);
/// read_new output: (rendered messages, new tail state, reset?).
type ReadOutput = (Vec<Rendered>, Tailer, bool);

/// One rendered chat message. `seq` is monotonic *within a generation*.
#[derive(Clone, Serialize, Debug, PartialEq)]
pub struct Message {
    pub seq: u64,
    /// "user" | "assistant".
    pub role: &'static str,
    /// "text" (a message) | "tool" (a compact "used tool: X" line).
    pub kind: &'static str,
    pub text: String,
}

/// A snapshot or delta the WS layer sends a subscribed client.
#[derive(Serialize, Debug)]
pub struct Update {
    pub pane: String,
    /// Continuity token — a change means "discard what you had, this is fresh".
    pub generation: u64,
    /// True = a fresh snapshot (client should replace); false = append deltas.
    pub full: bool,
    pub messages: Vec<Message>,
}

struct Store {
    generation: u64,
    msgs: VecDeque<Message>,
    bytes: usize,
    next_seq: u64,
}

impl Store {
    fn new(generation: u64) -> Self {
        Store {
            generation,
            msgs: VecDeque::new(),
            bytes: 0,
            next_seq: 0,
        }
    }

    fn push(&mut self, role: &'static str, kind: &'static str, mut text: String) {
        if text.is_empty() {
            return;
        }
        if text.len() > MAX_MSG_TEXT {
            let mut end = MAX_MSG_TEXT;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        self.bytes += text.len();
        self.msgs.push_back(Message {
            seq: self.next_seq,
            role,
            kind,
            text,
        });
        self.next_seq += 1;
        while self.msgs.len() > MAX_MESSAGES || (self.bytes > MAX_BYTES && self.msgs.len() > 1) {
            if let Some(m) = self.msgs.pop_front() {
                self.bytes -= m.text.len();
            }
        }
    }
}

/// Per-pane rendered-chat registry. A payload-less `events` broadcast is a wake
/// hint; the WS layer reconciles via [`update_since`](ChatStore::update_since).
pub struct ChatStore {
    inner: Arc<Mutex<HashMap<String, Store>>>,
    events: broadcast::Sender<()>,
}

impl Default for ChatStore {
    fn default() -> Self {
        ChatStore {
            inner: Arc::new(Mutex::new(HashMap::new())),
            events: broadcast::channel(16).0,
        }
    }
}

impl ChatStore {
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.events.subscribe()
    }

    /// Append rendered messages for a pane (called by the tailer).
    fn append(&self, pane: &str, generation: u64, msgs: Vec<Rendered>) {
        if msgs.is_empty() {
            return;
        }
        {
            let mut map = self.inner.lock().unwrap();
            let s = map
                .entry(pane.to_string())
                .or_insert_with(|| Store::new(generation));
            for (role, kind, text) in msgs {
                s.push(role, kind, text);
            }
        }
        let _ = self.events.send(());
    }

    /// Start a new generation for a pane (truncation / rotation / new session):
    /// discard the ring so unrelated messages can't be stitched together.
    fn reset(&self, pane: &str, generation: u64) {
        {
            let mut map = self.inner.lock().unwrap();
            map.insert(pane.to_string(), Store::new(generation));
        }
        let _ = self.events.send(());
    }

    /// Build the update for a client that last saw `(since_gen, since_seq)`. A
    /// generation mismatch, or a `since_seq` older than what the ring still
    /// holds, forces a `full` snapshot; otherwise only newer messages are sent.
    pub fn update_since(&self, pane: &str, since_gen: u64, since_seq: u64) -> Option<Update> {
        let map = self.inner.lock().unwrap();
        let s = map.get(pane)?;
        let oldest = s.msgs.front().map(|m| m.seq);
        let full = since_gen != s.generation || oldest.is_none_or(|o| since_seq < o);
        let messages: Vec<Message> = if full {
            s.msgs.iter().cloned().collect()
        } else {
            s.msgs
                .iter()
                .filter(|m| m.seq >= since_seq)
                .cloned()
                .collect()
        };
        Some(Update {
            pane: pane.to_string(),
            generation: s.generation,
            full,
            messages,
        })
    }

    /// Drop panes no longer live (mirrors the agent registry's own GC).
    pub fn retain(&self, live: &HashSet<String>) {
        let mut map = self.inner.lock().unwrap();
        let before = map.len();
        map.retain(|p, _| live.contains(p));
        let changed = map.len() != before;
        drop(map);
        if changed {
            let _ = self.events.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// Transcript parsing (pure, testable)
// ---------------------------------------------------------------------------

/// Render one transcript JSONL record into 0+ chat messages. Only user/assistant
/// text and tool NAMES survive; thinking, tool_result, and other record types
/// (permission-mode, mode, summary, …) render to nothing.
pub fn render_record(line: &str) -> Vec<Rendered> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new(); // tolerate a partial / malformed line
    };
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    let role: &'static str = match ty {
        "user" => "user",
        "assistant" => "assistant",
        _ => return Vec::new(),
    };
    let content = v.get("message").and_then(|m| m.get("content"));
    let mut out = Vec::new();
    match content {
        // Plain string content (typically a user turn).
        Some(Value::String(s)) => {
            let t = s.trim();
            if !t.is_empty() {
                out.push((role, "text", t.to_string()));
            }
        }
        // Structured content blocks.
        Some(Value::Array(blocks)) => {
            for b in blocks {
                let bt = b.get("type").and_then(Value::as_str).unwrap_or("");
                match bt {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            let t = t.trim();
                            if !t.is_empty() {
                                out.push((role, "text", t.to_string()));
                            }
                        }
                    }
                    "tool_use" => {
                        if let Some(name) = b.get("name").and_then(Value::as_str) {
                            out.push((role, "tool", format!("used tool: {name}")));
                        }
                    }
                    // thinking / tool_result / anything else: dropped (secrets).
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

// ---------------------------------------------------------------------------
// Tailer + supervisor
// ---------------------------------------------------------------------------

/// Canonicalize `path` and confirm it is a regular file under the caller's
/// `~/.claude/projects/`, then open it. Canonicalize-under-base subsumes the
/// symlink-escape check. Same-uid trust means this is defense-in-depth (only a
/// same-uid hook can supply the path), but it stops a stray/rewritten path from
/// reading arbitrary files.
fn open_validated(path: &str) -> Option<std::fs::File> {
    let home = std::env::var_os("HOME")?;
    let base = Path::new(&home)
        .join(".claude")
        .join("projects")
        .canonicalize()
        .ok()?;
    let canon = Path::new(path).canonicalize().ok()?;
    if !canon.starts_with(&base) {
        return None;
    }
    let file = std::fs::File::open(&canon).ok()?;
    file.metadata().ok().filter(|m| m.is_file())?;
    Some(file)
}

struct Tailer {
    inode: u64,
    offset: u64,
    partial: Vec<u8>,
}

/// Tail one pane's transcript until its agent state disappears. Polls the file,
/// handling append, truncation (offset > len → reset), and rotation (inode
/// change → reopen). Each reset bumps the generation so clients re-snapshot.
async fn tail_pane(app: Arc<App>, pane: String, session_id: String) {
    let mut state: Option<Tailer> = None;
    let mut gen_counter: u64 = 0;
    let mut ticker = tokio::time::interval(TAIL_POLL);
    loop {
        ticker.tick().await;

        // Stop when this pane's session is gone / superseded.
        let path = match app.agents.transcript_of(&pane) {
            Some((sid, Some(p))) if sid == session_id => p,
            Some((sid, _)) if sid != session_id => break, // new session took over
            _ => continue,                                // path not known yet
        };

        let p2 = pane.clone();
        let prev = state.take();
        let read = tokio::task::spawn_blocking(move || read_new(&path, prev)).await;
        let (msgs, new_state, reset) = match read {
            Ok(Some(r)) => r,
            _ => {
                state = None;
                continue;
            }
        };
        if reset {
            gen_counter += 1;
            app.chat.reset(&p2, gen_counter);
        }
        if !msgs.is_empty() {
            app.chat.append(&p2, gen_counter.max(1), msgs);
        }
        state = Some(new_state);
    }
}

/// Blocking read of any new complete lines from `path`, given prior tail state.
/// Returns (rendered messages, new state, reset?) — `reset` is true when the
/// file was truncated/rotated/first-seen so the caller starts a new generation.
fn read_new(
    path: &str,
    prev: Option<Tailer>,
) -> Option<ReadOutput> {
    use std::os::unix::fs::MetadataExt;
    let mut file = open_validated(path)?;
    let md = file.metadata().ok()?;
    let (inode, len) = (md.ino(), md.size());

    // Decide continuity: new file, rotated (inode changed), or truncated
    // (shrunk) → reset and read from the start; else continue from offset.
    let (mut offset, mut partial, reset) = match prev {
        Some(t) if t.inode == inode && t.offset <= len => (t.offset, t.partial, false),
        _ => (0, Vec::new(), true),
    };

    let mut out = Vec::new();
    if len > offset {
        let to_read = (len - offset).min(READ_CHUNK);
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut reader = BufReader::new(file.take(to_read));
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line).ok()?;
            if n == 0 {
                break;
            }
            offset += n as u64;
            if line.last() == Some(&b'\n') {
                // A complete line (prepend any buffered partial from last poll).
                let mut full = std::mem::take(&mut partial);
                full.extend_from_slice(&line);
                if let Ok(s) = std::str::from_utf8(&full) {
                    out.extend(render_record(s.trim_end()));
                }
            } else {
                // Trailing partial line — buffer for next poll.
                partial.extend_from_slice(&line);
            }
        }
    }
    Some((
        out,
        Tailer {
            inode,
            offset,
            partial,
        },
        reset,
    ))
}

/// Supervise tailers: one per pane with a known transcript path, reaped when the
/// pane's agent state disappears. Modeled on the htop capture supervisor.
pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        let mut tasks: HashMap<String, (String, tokio::task::AbortHandle)> = HashMap::new();
        let mut poll = tokio::time::interval(SUPERVISOR_POLL);
        loop {
            poll.tick().await;
            // Panes with a known transcript, keyed to their session.
            let want: HashMap<String, String> = app
                .agents
                .views()
                .into_iter()
                .filter(|v| v.transcript_path.is_some())
                .map(|v| (v.pane, v.session_id))
                .collect();

            // Start/replace tailers (a new session id or a finished task restarts).
            for (pane, sid) in &want {
                let need_start = match tasks.get(pane) {
                    Some((s, h)) => s != sid || h.is_finished(),
                    None => true,
                };
                if need_start {
                    if let Some((_, h)) = tasks.remove(pane) {
                        h.abort();
                    }
                    if tasks.len() < MAX_TAILERS {
                        let app = app.clone();
                        let (p, s) = (pane.clone(), sid.clone());
                        let h = tokio::spawn(async move { tail_pane(app, p, s).await });
                        tasks.insert(pane.clone(), (sid.clone(), h.abort_handle()));
                    }
                }
            }
            // Reap tailers whose pane no longer wants one.
            tasks.retain(|pane, (_, h)| {
                let keep = want.contains_key(pane) && !h.is_finished();
                if !keep {
                    h.abort();
                }
                keep
            });
            // GC the chat ring to live panes.
            let live: HashSet<String> = want.keys().cloned().collect();
            app.chat.retain(&live);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_user_and_assistant_text_and_tool_names() {
        // User string turn.
        let u = r#"{"type":"user","message":{"content":"run the tests please"}}"#;
        assert_eq!(
            render_record(u),
            vec![("user", "text", "run the tests please".into())]
        );

        // Assistant text + tool_use; thinking + tool_result dropped.
        let a = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"secret reasoning"},
            {"type":"text","text":"I'll run them."},
            {"type":"tool_use","name":"Bash","input":{"command":"rm -rf /secret"}}
        ]}}"#;
        assert_eq!(
            render_record(a),
            vec![
                ("assistant", "text", "I'll run them.".into()),
                ("assistant", "tool", "used tool: Bash".into()),
            ]
        );
        // The command must never appear in a rendered message.
        assert!(!render_record(a)
            .iter()
            .any(|(_, _, t)| t.contains("rm -rf")));

        // A user tool_result turn renders nothing (results are hidden).
        let tr = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"secret output"}]}}"#;
        assert!(render_record(tr).is_empty());

        // Non-message records render nothing.
        assert!(
            render_record(r#"{"type":"permission-mode","permissionMode":"default"}"#).is_empty()
        );
        assert!(render_record("not json").is_empty());
    }

    #[test]
    fn ring_bounds_and_delta() {
        let store = ChatStore::default();
        for i in 0..(MAX_MESSAGES + 10) {
            store.append("%1", 1, vec![("assistant", "text", format!("m{i}"))]);
        }
        let snap = store.update_since("%1", 0, 0).unwrap();
        assert!(snap.full);
        assert_eq!(snap.messages.len(), MAX_MESSAGES); // bounded
        let last = snap.messages.last().unwrap().seq;
        // A caller up to date gets an empty (non-full) delta.
        let d = store.update_since("%1", snap.generation, last + 1).unwrap();
        assert!(!d.full);
        assert!(d.messages.is_empty());
        // A generation change forces a full snapshot.
        store.reset("%1", 2);
        store.append("%1", 2, vec![("user", "text", "hi".into())]);
        let after = store.update_since("%1", snap.generation, last + 1).unwrap();
        assert!(after.full);
        assert_eq!(after.generation, 2);
    }
}
