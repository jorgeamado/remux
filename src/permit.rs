//! M4b permission-card registry: agent permission requests that block a hook
//! (`remux emit permission --wait`) awaiting a decision from the phone.
//!
//! A card carries **no authority** — opening one only makes a prompt appear.
//! Only a paired, `approve`-capable device resolves it (that check lives in
//! the WS/HTTP decision path, increment 2), and no failure path here ever
//! fabricates a decision: expiry, a broken wait (the Mac answered and Claude
//! SIGTERM'd the hook — see docs/spikes/M4.0-protocol.md), and daemon
//! shutdown all surface as "no decision", which makes the hook fall back to
//! the Mac dialog.
//!
//! Concurrency contract: `resolve` is a single-winner operation under one
//! mutex — the first caller removes the entry and wakes the waiter; any later
//! caller (a second device, the HTTP path racing the WS path) gets `Unknown`.
//! Nothing is awaited while the mutex is held (`oneshot::Sender::send` is
//! synchronous).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot};

/// What a held-wait receives when its card is resolved: the decision, plus a
/// one-shot the waiter fires once it has actually written the decision back to
/// the (live) hook socket. That fired signal is what tells the deciding device
/// "the hook got it" — distinct from "the registry consumed the card".
type DecisionMsg = (Decision, oneshot::Sender<()>);

/// How long a card stays open. Must sit comfortably below the Claude Code
/// hook `timeout` (the install snippet uses 120s) so the fallback is the hook
/// exiting cleanly, not Claude killing it mid-decision.
pub const CARD_TTL: Duration = Duration::from_secs(100);
/// Global cap on open cards. Held waits must never exhaust the ingest
/// connection pool; the registry size is the real bound.
const MAX_PENDING: usize = 8;
/// Per-pane cap: an agent may have a couple of concurrent prompts (a retry, a
/// queued second tool) but not unbounded.
const MAX_PER_PANE: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Decision::Allow),
            "deny" => Some(Decision::Deny),
            _ => None,
        }
    }
}

/// Public card metadata: what a device is shown and what a listing returns.
/// Never holds the oneshot — that lives in the private registry entry, so a
/// `Card` can be cloned into a broadcast frame without touching the waiter.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Card {
    pub id: String,
    pub session: String,
    pub pane: String,
    pub source: String,
    pub tool: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    /// Not serialized: delivery adds a remaining-TTL in increment 2 instead of
    /// leaking an absolute clock the client can't interpret.
    #[serde(skip)]
    pub created: Instant,
    #[serde(skip)]
    pub deadline: Instant,
}

impl Card {
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// The client-facing view: card metadata plus a *relative* remaining-TTL
    /// (never the absolute deadline the client can't interpret) so the UI can
    /// disable the buttons when it runs out.
    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "session": self.session,
            "pane": self.pane,
            "source": self.source,
            "tool": self.tool,
            "summary": self.summary,
            "prompt_id": self.prompt_id,
            "remaining_secs": self.remaining().as_secs(),
        })
    }
}

struct Entry {
    card: Card,
    decide_tx: oneshot::Sender<DecisionMsg>,
}

/// Why a resolve did not decide a card. `Expired` vs `Unknown` lets the UI say
/// "you were too late" instead of "no such request" without revealing whether
/// an arbitrary guessed id ever existed.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// No such id: never existed, already resolved, or the waiter is gone.
    Unknown,
    /// Present but past its deadline.
    Expired,
    /// Present and live, but the deciding device is not (or no longer)
    /// `approve`-capable. The card is left open for a device that is.
    Forbidden,
}

pub struct Registry {
    inner: Mutex<HashMap<String, Entry>>,
    /// A hint fired whenever the set of open cards changes (opened / resolved /
    /// expired). Deliberately payload-less: receivers reconcile against
    /// `snapshot()`, so a lagged receiver just re-reads — it can't miss state.
    events: broadcast::Sender<()>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            events: broadcast::channel(16).0,
        }
    }
}

impl Registry {
    /// Subscribe to change hints. Pair with `snapshot()` to reconcile.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.events.subscribe()
    }

    /// Fire a change hint without touching the card set — used when *who may
    /// see* the cards changes (a device's `approve` capability was granted or
    /// revoked), so live sockets re-evaluate their capability and reconcile.
    pub fn notify_watchers(&self) {
        let _ = self.events.send(());
    }

    /// Insert a new card, returning the receiver the held-wait awaits. Rejects
    /// when the global or per-pane cap is hit so the hook falls back to the Mac
    /// dialog rather than queueing. Sweeps expired entries first so a waiter
    /// that hasn't cleaned up yet can't hold a slot hostage.
    pub fn insert(&self, card: Card) -> Result<oneshot::Receiver<DecisionMsg>, &'static str> {
        // A rejection that still swept expired entries must fire a hint too, or
        // the swept cards linger on connected clients until the next change.
        let (result, changed) = {
            let mut map = self.inner.lock().unwrap();
            let now = Instant::now();
            let before = map.len();
            map.retain(|_, e| e.card.deadline > now);
            let swept = map.len() != before;
            // Dedup by prompt_id: a Claude retry (same prompt_id) while the
            // first card is still open must not open a second independently-
            // resolvable card for one operation. The first card's waiter is
            // still blocking; reject the duplicate (the retry falls back to Mac).
            let dup = card.prompt_id.as_deref().is_some_and(|pid| {
                map.values()
                    .any(|e| e.card.prompt_id.as_deref() == Some(pid))
            });
            if dup {
                (Err("duplicate permission request (same prompt_id)"), swept)
            } else if map.len() >= MAX_PENDING {
                (Err("too many pending permission requests"), swept)
            } else if map.values().filter(|e| e.card.pane == card.pane).count() >= MAX_PER_PANE {
                (Err("too many pending requests for this pane"), swept)
            } else {
                let (tx, rx) = oneshot::channel();
                map.insert(
                    card.id.clone(),
                    Entry {
                        card,
                        decide_tx: tx,
                    },
                );
                (Ok(rx), true)
            }
        };
        if changed {
            let _ = self.events.send(());
        }
        result
    }

    /// Atomically consume a card and wake its waiter. Single-winner: the mutex
    /// makes exactly one caller succeed; a later one gets `Unknown`. A failed
    /// `send` means the waiter already left (EOF/expiry) — reported as
    /// `Unknown` so nothing is treated as decided.
    ///
    /// `authorized` is evaluated **under the registry lock**, so the deciding
    /// device's `approve` capability is checked at the instant of decision — a
    /// `revoke-approve` that committed just before cannot race a stale
    /// authorization through (Codex review). It runs only for a live,
    /// unexpired card; if it returns false the card is left open for a device
    /// that is capable, and the caller gets `Forbidden`.
    /// On success returns the `Card` and a receiver that fires once the waiting
    /// hook has actually received the decision (see [`DecisionMsg`]). The
    /// deciding device should await it before reporting success, so a decision
    /// that raced a socket-close isn't reported as delivered.
    pub fn resolve(
        &self,
        id: &str,
        decision: Decision,
        authorized: impl FnOnce() -> bool,
    ) -> Result<(Card, oneshot::Receiver<()>), ResolveError> {
        let outcome = {
            let mut map = self.inner.lock().unwrap();
            // Copy the deadline out so the immutable borrow ends before remove.
            let Some(deadline) = map.get(id).map(|e| e.card.deadline) else {
                return Err(ResolveError::Unknown); // nothing changed
            };
            if deadline <= Instant::now() {
                map.remove(id);
                Err(ResolveError::Expired)
            } else if !authorized() {
                return Err(ResolveError::Forbidden); // card left open, nothing changed
            } else {
                let entry = map.remove(id).expect("present under the same lock");
                let (conf_tx, conf_rx) = oneshot::channel();
                match entry.decide_tx.send((decision, conf_tx)) {
                    Ok(()) => Ok((entry.card, conf_rx)),
                    // Waiter already gone (EOF/expiry) — the card was consumed
                    // but nothing was decided.
                    Err(_) => Err(ResolveError::Unknown),
                }
            }
        };
        // Every branch that reached here removed an entry (Unknown-from-get and
        // Forbidden returned early). Signal the change so views reconcile.
        let _ = self.events.send(());
        outcome
    }

    /// Remove a card without deciding it — the waiter's own cleanup on EOF,
    /// expiry, or task abort. Idempotent; safe to call after `resolve` already
    /// took the entry. Fires a change hint only when it actually removed one.
    pub fn remove(&self, id: &str) {
        if self.inner.lock().unwrap().remove(id).is_some() {
            let _ = self.events.send(());
        }
    }

    /// Live (non-expired) cards, for a listing / reconcile-on-subscribe.
    pub fn snapshot(&self) -> Vec<Card> {
        let now = Instant::now();
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.card.deadline > now)
            .map(|e| e.card.clone())
            .collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}

/// A 128-bit random card id (32 hex chars). 48-bit ids collide too readily and
/// a collision would overwrite an entry, dropping the first waiter's sender.
pub fn mint_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, pane: &str) -> Card {
        let now = Instant::now();
        Card {
            id: id.into(),
            session: "s".into(),
            pane: pane.into(),
            source: "claude-code".into(),
            tool: "Bash".into(),
            summary: "touch x".into(),
            prompt_id: None,
            created: now,
            deadline: now + CARD_TTL,
        }
    }

    #[tokio::test]
    async fn resolve_wakes_the_waiter_once() {
        let reg = Registry::default();
        let mut rx = reg.insert(card("a", "%1")).unwrap();
        let (card, _conf_rx) = reg.resolve("a", Decision::Allow, || true).unwrap();
        assert_eq!(card.id, "a");
        // The waiter receives the decision + a confirmation sender.
        let (decision, _conf_tx) = rx.try_recv().unwrap();
        assert_eq!(decision, Decision::Allow);
        // Second resolve of the same id finds nothing.
        assert_eq!(
            reg.resolve("a", Decision::Deny, || true).err(),
            Some(ResolveError::Unknown)
        );
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn unauthorized_device_cannot_resolve_and_leaves_the_card() {
        let reg = Registry::default();
        let _rx = reg.insert(card("a", "%1")).unwrap();
        assert_eq!(
            reg.resolve("a", Decision::Allow, || false).err(),
            Some(ResolveError::Forbidden)
        );
        // Card is left open for a device that is capable.
        assert_eq!(reg.len(), 1);
        assert!(reg.resolve("a", Decision::Allow, || true).is_ok());
    }

    #[tokio::test]
    async fn notify_watchers_wakes_subscribers() {
        let reg = Registry::default();
        let mut sub = reg.subscribe();
        reg.notify_watchers();
        assert!(sub.try_recv().is_ok());
    }

    #[tokio::test]
    async fn rejected_insert_still_fires_when_it_swept_expired() {
        let reg = Registry::default();
        // A: live, prompt "p".
        let mut a = card("a", "%1");
        a.prompt_id = Some("p".into());
        let _rxa = reg.insert(a).unwrap();
        // B: already expired, distinct pane.
        let mut b = card("b", "%2");
        b.deadline = Instant::now() - Duration::from_secs(1);
        let _rxb = reg.insert(b).unwrap();

        let mut sub = reg.subscribe();
        // C: duplicate prompt "p" → rejected, but it sweeps the expired B, so a
        // reconcile hint must still fire (else B lingers on clients).
        let mut c = card("c", "%3");
        c.prompt_id = Some("p".into());
        assert!(reg.insert(c).is_err());
        assert!(
            sub.try_recv().is_ok(),
            "a rejected insert that swept an expired card must still fire a hint"
        );
        assert_eq!(reg.len(), 1); // only A remains
    }

    #[test]
    fn duplicate_prompt_id_is_rejected() {
        let reg = Registry::default();
        let mut c1 = card("a", "%1");
        c1.prompt_id = Some("p-1".into());
        let mut c2 = card("b", "%2");
        c2.prompt_id = Some("p-1".into());
        let _rx = reg.insert(c1).unwrap();
        assert!(reg.insert(c2).is_err());
    }

    #[test]
    fn unknown_id_is_unknown_not_a_panic() {
        let reg = Registry::default();
        assert_eq!(
            reg.resolve("ghost", Decision::Allow, || true).err(),
            Some(ResolveError::Unknown)
        );
    }

    #[test]
    fn resolve_after_waiter_dropped_is_unknown() {
        let reg = Registry::default();
        let rx = reg.insert(card("a", "%1")).unwrap();
        drop(rx); // hook died / Mac answered
        assert_eq!(
            reg.resolve("a", Decision::Allow, || true).err(),
            Some(ResolveError::Unknown)
        );
    }

    #[test]
    fn expired_card_resolves_as_expired() {
        let reg = Registry::default();
        // A card already past its deadline still inserts (the sweep only culls
        // *other* stale entries); resolving it reports Expired, not a decision.
        let mut c = card("a", "%1");
        c.deadline = Instant::now() - Duration::from_secs(1);
        let _rx = reg.insert(c).unwrap();
        assert_eq!(
            reg.resolve("a", Decision::Allow, || true).err(),
            Some(ResolveError::Expired)
        );
    }

    #[test]
    fn global_and_per_pane_caps_reject() {
        let reg = Registry::default();
        // Per-pane cap: MAX_PER_PANE on one pane, then reject.
        for i in 0..MAX_PER_PANE {
            reg.insert(card(&format!("p{i}"), "%1")).unwrap();
        }
        assert!(reg.insert(card("pX", "%1")).is_err());
        // Different panes fill toward the global cap.
        let mut n = MAX_PER_PANE;
        let mut pane = 2;
        while n < MAX_PENDING {
            reg.insert(card(&format!("g{n}"), &format!("%{pane}")))
                .unwrap();
            n += 1;
            pane += 1;
        }
        assert_eq!(reg.len(), MAX_PENDING);
        assert!(reg.insert(card("overflow", "%999")).is_err());
    }

    #[test]
    fn mint_id_is_128_bit_hex() {
        let id = mint_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(mint_id(), mint_id());
    }
}
