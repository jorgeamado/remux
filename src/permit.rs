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
use tokio::sync::oneshot;

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
}

struct Entry {
    card: Card,
    decide_tx: oneshot::Sender<Decision>,
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

#[derive(Default)]
pub struct Registry {
    inner: Mutex<HashMap<String, Entry>>,
}

impl Registry {
    /// Insert a new card, returning the receiver the held-wait awaits. Rejects
    /// when the global or per-pane cap is hit so the hook falls back to the Mac
    /// dialog rather than queueing. Sweeps expired entries first so a waiter
    /// that hasn't cleaned up yet can't hold a slot hostage.
    pub fn insert(&self, card: Card) -> Result<oneshot::Receiver<Decision>, &'static str> {
        let mut map = self.inner.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, e| e.card.deadline > now);
        // Dedup by prompt_id: a Claude retry (same prompt_id) while the first
        // card is still open must not open a second independently-resolvable
        // card for one operation. The first card's waiter is still blocking;
        // reject the duplicate (the retrying hook falls back to the Mac).
        if let Some(pid) = &card.prompt_id {
            if map
                .values()
                .any(|e| e.card.prompt_id.as_deref() == Some(pid.as_str()))
            {
                return Err("duplicate permission request (same prompt_id)");
            }
        }
        if map.len() >= MAX_PENDING {
            return Err("too many pending permission requests");
        }
        if map.values().filter(|e| e.card.pane == card.pane).count() >= MAX_PER_PANE {
            return Err("too many pending requests for this pane");
        }
        let (tx, rx) = oneshot::channel();
        map.insert(
            card.id.clone(),
            Entry {
                card,
                decide_tx: tx,
            },
        );
        Ok(rx)
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
    pub fn resolve(
        &self,
        id: &str,
        decision: Decision,
        authorized: impl FnOnce() -> bool,
    ) -> Result<Card, ResolveError> {
        let mut map = self.inner.lock().unwrap();
        let Some(entry) = map.get(id) else {
            return Err(ResolveError::Unknown);
        };
        if entry.card.deadline <= Instant::now() {
            map.remove(id);
            return Err(ResolveError::Expired);
        }
        if !authorized() {
            return Err(ResolveError::Forbidden);
        }
        // Authorized and live — now consume it.
        let entry = map.remove(id).expect("present under the same lock");
        match entry.decide_tx.send(decision) {
            Ok(()) => Ok(entry.card),
            Err(_) => Err(ResolveError::Unknown),
        }
    }

    /// Remove a card without deciding it — the waiter's own cleanup on EOF,
    /// expiry, or task abort. Idempotent; safe to call after `resolve` already
    /// took the entry.
    pub fn remove(&self, id: &str) {
        self.inner.lock().unwrap().remove(id);
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
        assert_eq!(
            reg.resolve("a", Decision::Allow, || true).map(|c| c.id),
            Ok("a".into())
        );
        assert_eq!(rx.try_recv(), Ok(Decision::Allow));
        // Second resolve of the same id finds nothing.
        assert_eq!(
            reg.resolve("a", Decision::Deny, || true),
            Err(ResolveError::Unknown)
        );
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn unauthorized_device_cannot_resolve_and_leaves_the_card() {
        let reg = Registry::default();
        let _rx = reg.insert(card("a", "%1")).unwrap();
        assert_eq!(
            reg.resolve("a", Decision::Allow, || false),
            Err(ResolveError::Forbidden)
        );
        // Card is left open for a device that is capable.
        assert_eq!(reg.len(), 1);
        assert!(reg.resolve("a", Decision::Allow, || true).is_ok());
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
            reg.resolve("ghost", Decision::Allow, || true),
            Err(ResolveError::Unknown)
        );
    }

    #[test]
    fn resolve_after_waiter_dropped_is_unknown() {
        let reg = Registry::default();
        let rx = reg.insert(card("a", "%1")).unwrap();
        drop(rx); // hook died / Mac answered
        assert_eq!(
            reg.resolve("a", Decision::Allow, || true),
            Err(ResolveError::Unknown)
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
            reg.resolve("a", Decision::Allow, || true),
            Err(ResolveError::Expired)
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
