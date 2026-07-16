//! Agent lifecycle state (the honest half of `claude.v1`): a per-pane, in-memory,
//! TTL'd view of what an agent (e.g. Claude Code) is doing, fed by generic
//! `remux emit agent-state` events. A projector (see `paneview`) turns this —
//! joined with open permit cards — into a `claude.v1` pane view.
//!
//! Nothing here is persisted, and it holds ONLY coarse, broadcast-safe status:
//! a base status, the agent's session id, and the *active operations* (op id →
//! tool NAME). Sensitive decision content (commands, tool input, summaries)
//! never lives here — it stays in `permit::Registry`, which reaches only
//! approve-capable devices. The projector references a pending card by ID.
//!
//! Late events from a superseded session cannot corrupt a newer one: a new
//! session only takes over via `SessionStart` (or bootstrap when no state
//! exists); every other event is applied only when its `session_id` matches.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Safety-net TTL: drop an agent's state if no event arrives for this long
/// (`SessionEnded` is the normal cleanup; this covers a crashed/killed agent).
const AGENT_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// Cap tracked concurrent operations per pane (a runaway producer can't grow it).
const MAX_OPS: usize = 32;

/// Status independent of pending approvals (those are derived from permit cards
/// by the projector, and take precedence over the base).
#[derive(Clone, Copy, PartialEq, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BaseStatus {
    Working,
    Idle,
}

/// A generic agent lifecycle event. The Claude Code adapter maps its hooks onto
/// these; remux stays agent-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    SessionStart {
        session_id: String,
    },
    PromptSubmitted {
        session_id: String,
    },
    OperationStarted {
        session_id: String,
        op_id: String,
        tool: String,
    },
    OperationEnded {
        session_id: String,
        op_id: String,
    },
    Idle {
        session_id: String,
    },
    SessionEnded {
        session_id: String,
    },
    /// Keep-alive: refresh the TTL without changing status.
    Touch {
        session_id: String,
    },
}

impl Event {
    fn session_id(&self) -> &str {
        match self {
            Event::SessionStart { session_id }
            | Event::PromptSubmitted { session_id }
            | Event::OperationStarted { session_id, .. }
            | Event::OperationEnded { session_id, .. }
            | Event::Idle { session_id }
            | Event::SessionEnded { session_id }
            | Event::Touch { session_id } => session_id,
        }
    }
}

/// One active operation the agent has started but not finished.
#[derive(Clone, Debug, PartialEq)]
pub struct Op {
    pub op_id: String,
    pub tool: String,
}

struct State {
    session_id: String,
    base: BaseStatus,
    /// Active ops in start order; the last is the "current" one.
    ops: Vec<Op>,
    /// The session's transcript file (JSONL), learned from an `agent_session`
    /// event — the source the chat tailer reads. `None` until known.
    transcript_path: Option<String>,
    /// Last observed Claude permission mode (default / acceptEdits / auto …).
    /// Observational only; `None` until known.
    permission_mode: Option<String>,
    updated: Instant,
}

/// A pane's agent state, as the projector / chat tailer reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentView {
    pub pane: String,
    pub session_id: String,
    pub base: BaseStatus,
    /// Active operations (op id + tool name), oldest first.
    pub ops: Vec<Op>,
    pub transcript_path: Option<String>,
    pub permission_mode: Option<String>,
}

/// Per-pane agent-state registry. Like the other registries, a payload-less
/// `events` broadcast is only a wake hint — subscribers reconcile via
/// [`views`](Registry::views).
pub struct Registry {
    inner: Arc<Mutex<HashMap<String, State>>>,
    events: broadcast::Sender<()>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            inner: Arc::new(Mutex::new(HashMap::new())),
            events: broadcast::channel(16).0,
        }
    }
}

impl Registry {
    /// Subscribe to change hints.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.events.subscribe()
    }

    /// Apply a lifecycle event for `pane`. Returns whether anything changed
    /// (so the caller can decide to hint; we hint internally regardless of the
    /// fine-grained result).
    pub fn apply(&self, pane: &str, ev: Event) {
        {
            let mut map = self.inner.lock().unwrap();
            let sid = ev.session_id().to_string();
            match map.get_mut(pane) {
                // A new session only takes over via SessionStart.
                Some(s) if s.session_id != sid => {
                    if matches!(ev, Event::SessionStart { .. }) {
                        map.insert(pane.to_string(), State::fresh(sid));
                    } else {
                        return; // stale event from a superseded session
                    }
                }
                // Session matches. SessionEnded REMOVES the entry (so the
                // projector stops publishing and releases the pane) — everything
                // else is a state transition.
                Some(_) if matches!(ev, Event::SessionEnded { .. }) => {
                    map.remove(pane);
                }
                Some(s) => s.apply(ev),
                // No state yet: bootstrap from any event except SessionEnded
                // (which on an unknown pane is a no-op) — so a pane still shows
                // up if its SessionStart hook was missed / the daemon restarted.
                None if !matches!(ev, Event::SessionEnded { .. }) => {
                    let mut s = State::fresh(sid);
                    s.apply(ev);
                    map.insert(pane.to_string(), s);
                }
                None => {}
            }
        }
        let _ = self.events.send(());
    }

    /// The (session id, transcript path) for a pane — for the chat tailer to
    /// know which file to read and when the session has been superseded.
    pub fn transcript_of(&self, pane: &str) -> Option<(String, Option<String>)> {
        self.inner
            .lock()
            .unwrap()
            .get(pane)
            .map(|s| (s.session_id.clone(), s.transcript_path.clone()))
    }

    /// Current agent views (for the projector to reconcile).
    pub fn views(&self) -> Vec<AgentView> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(pane, s)| AgentView {
                pane: pane.clone(),
                session_id: s.session_id.clone(),
                base: s.base,
                ops: s.ops.clone(),
                transcript_path: s.transcript_path.clone(),
                permission_mode: s.permission_mode.clone(),
            })
            .collect()
    }

    /// Record session metadata (transcript path + permission mode) for `pane`,
    /// from an `agent_session` event. Session-guarded like `apply`: a new session
    /// takes over (fresh state), a matching one updates in place, a stale one is
    /// ignored. Values are only overwritten when present (a mode-only update
    /// keeps the known transcript path).
    pub fn set_session(
        &self,
        pane: &str,
        session_id: &str,
        transcript_path: Option<String>,
        permission_mode: Option<String>,
    ) {
        {
            let mut map = self.inner.lock().unwrap();
            // Reuse a matching-session entry; otherwise (absent or a different,
            // superseded session) start fresh for this session.
            let matches = map.get(pane).is_some_and(|s| s.session_id == session_id);
            if !matches {
                map.insert(pane.to_string(), State::fresh(session_id.to_string()));
            }
            let s = map.get_mut(pane).unwrap();
            s.updated = Instant::now();
            if transcript_path.is_some() {
                s.transcript_path = transcript_path;
            }
            if permission_mode.is_some() {
                s.permission_mode = permission_mode;
            }
        }
        let _ = self.events.send(());
    }

    /// Drop entries whose pane is no longer live, or that have gone stale (TTL).
    pub fn prune(&self, live: &std::collections::HashSet<String>) {
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap();
        let before = map.len();
        map.retain(|pane, s| live.contains(pane) && now.duration_since(s.updated) < AGENT_TTL);
        let changed = map.len() != before;
        drop(map);
        if changed {
            let _ = self.events.send(());
        }
    }
}

impl State {
    fn fresh(session_id: String) -> Self {
        State {
            session_id,
            base: BaseStatus::Idle,
            ops: Vec::new(),
            transcript_path: None,
            permission_mode: None,
            updated: Instant::now(),
        }
    }

    fn apply(&mut self, ev: Event) {
        self.updated = Instant::now();
        match ev {
            Event::SessionStart { .. } => {
                self.base = BaseStatus::Idle;
                self.ops.clear();
            }
            Event::PromptSubmitted { .. } => self.base = BaseStatus::Working,
            Event::OperationStarted { op_id, tool, .. } => {
                self.base = BaseStatus::Working;
                if !self.ops.iter().any(|o| o.op_id == op_id) {
                    // Evict the oldest so a leaked op (a failed/interrupted tool
                    // that never sent operation-ended) can't push out the NEWEST
                    // op — the one most likely to correlate with a pending card.
                    if self.ops.len() >= MAX_OPS {
                        self.ops.remove(0);
                    }
                    self.ops.push(Op { op_id, tool });
                }
            }
            Event::OperationEnded { op_id, .. } => {
                self.ops.retain(|o| o.op_id != op_id);
                // base stays Working — Claude keeps going until Stop.
            }
            Event::Idle { .. } => {
                self.base = BaseStatus::Idle;
                self.ops.clear();
            }
            // SessionEnded is handled by the Registry (it removes the entry).
            Event::SessionEnded { .. } | Event::Touch { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn started(reg: &Registry, pane: &str, sid: &str) {
        reg.apply(
            pane,
            Event::SessionStart {
                session_id: sid.into(),
            },
        );
    }

    #[test]
    fn lifecycle_transitions() {
        let reg = Registry::default();
        started(&reg, "%1", "s1");
        assert_eq!(reg.views()[0].base, BaseStatus::Idle);

        reg.apply(
            "%1",
            Event::PromptSubmitted {
                session_id: "s1".into(),
            },
        );
        assert_eq!(reg.views()[0].base, BaseStatus::Working);

        reg.apply(
            "%1",
            Event::OperationStarted {
                session_id: "s1".into(),
                op_id: "op1".into(),
                tool: "Bash".into(),
            },
        );
        let v = &reg.views()[0];
        assert_eq!(v.base, BaseStatus::Working);
        assert_eq!(
            v.ops,
            vec![Op {
                op_id: "op1".into(),
                tool: "Bash".into()
            }]
        );

        reg.apply(
            "%1",
            Event::OperationEnded {
                session_id: "s1".into(),
                op_id: "op1".into(),
            },
        );
        assert!(reg.views()[0].ops.is_empty());

        reg.apply(
            "%1",
            Event::Idle {
                session_id: "s1".into(),
            },
        );
        assert_eq!(reg.views()[0].base, BaseStatus::Idle);
    }

    #[test]
    fn session_ended_removes_state() {
        let reg = Registry::default();
        started(&reg, "%1", "s1");
        reg.apply(
            "%1",
            Event::PromptSubmitted {
                session_id: "s1".into(),
            },
        );
        assert_eq!(reg.views().len(), 1);
        // SessionEnded from the SAME session clears the entry entirely.
        reg.apply(
            "%1",
            Event::SessionEnded {
                session_id: "s1".into(),
            },
        );
        assert!(reg.views().is_empty());
        // A stale SessionEnded from a different session doesn't remove a live one.
        started(&reg, "%1", "s2");
        reg.apply(
            "%1",
            Event::SessionEnded {
                session_id: "old".into(),
            },
        );
        assert_eq!(reg.views().len(), 1);
    }

    #[test]
    fn set_session_stores_metadata_and_is_session_guarded() {
        let reg = Registry::default();
        started(&reg, "%1", "s1");
        reg.apply(
            "%1",
            Event::PromptSubmitted {
                session_id: "s1".into(),
            },
        );
        // Session metadata for the current session updates in place.
        reg.set_session(
            "%1",
            "s1",
            Some("/t/s1.jsonl".into()),
            Some("default".into()),
        );
        let v = &reg.views()[0];
        assert_eq!(v.transcript_path.as_deref(), Some("/t/s1.jsonl"));
        assert_eq!(v.permission_mode.as_deref(), Some("default"));
        assert_eq!(v.base, BaseStatus::Working); // didn't clobber status
                                                 // A mode-only update keeps the known path.
        reg.set_session("%1", "s1", None, Some("acceptEdits".into()));
        let v = &reg.views()[0];
        assert_eq!(v.transcript_path.as_deref(), Some("/t/s1.jsonl"));
        assert_eq!(v.permission_mode.as_deref(), Some("acceptEdits"));
        // A different session replaces (fresh) — old path is gone.
        reg.set_session("%1", "s2", Some("/t/s2.jsonl".into()), None);
        let v = &reg.views()[0];
        assert_eq!(v.session_id, "s2");
        assert_eq!(v.transcript_path.as_deref(), Some("/t/s2.jsonl"));
        assert_eq!(v.permission_mode, None);
    }

    #[test]
    fn oldest_op_evicted_at_cap() {
        let reg = Registry::default();
        started(&reg, "%1", "s1");
        for i in 0..(MAX_OPS + 5) {
            reg.apply(
                "%1",
                Event::OperationStarted {
                    session_id: "s1".into(),
                    op_id: format!("op{i}"),
                    tool: "Bash".into(),
                },
            );
        }
        let ops = &reg.views()[0].ops;
        assert_eq!(ops.len(), MAX_OPS);
        // The NEWEST op survives (most likely to correlate with a card).
        assert_eq!(ops.last().unwrap().op_id, format!("op{}", MAX_OPS + 4));
    }

    #[test]
    fn stale_session_events_ignored() {
        let reg = Registry::default();
        started(&reg, "%1", "s2"); // current session s2
                                   // A late op from the OLD session s1 must not corrupt s2.
        reg.apply(
            "%1",
            Event::OperationStarted {
                session_id: "s1".into(),
                op_id: "old".into(),
                tool: "Edit".into(),
            },
        );
        assert!(reg.views()[0].ops.is_empty());
        assert_eq!(reg.views()[0].session_id, "s2");
        // A newer SessionStart replaces it.
        started(&reg, "%1", "s3");
        assert_eq!(reg.views()[0].session_id, "s3");
    }

    #[test]
    fn prune_drops_missing_and_stale() {
        let reg = Registry::default();
        started(&reg, "%1", "s1");
        started(&reg, "%2", "s2");
        reg.prune(&HashSet::from(["%1".to_string()]));
        let v = reg.views();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].pane, "%1");
    }
}
