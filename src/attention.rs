//! Attention heuristic: the managed session produced output for a while and
//! then went quiet — a job finished or a program is waiting for input. One
//! event kind, broadcast to every connected client; the client decides whether
//! the user needs to see it (V1: only when the app is not visible).

use crate::{tmux, App};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct Config {
    /// How often to sample tmux window activity.
    pub poll: Duration,
    /// Minimum busy span before quiet counts as noteworthy. Filters out the
    /// echo/prompt churn of a quick interactive command.
    pub min_busy: Duration,
    /// How long the pane must stay quiet before we raise attention.
    pub quiet: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        fn secs(var: &str, default: f64) -> Duration {
            Duration::from_secs_f64(
                std::env::var(var)
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(default),
            )
        }
        Self {
            poll: secs("REMUX_ATTENTION_POLL_SECS", 1.0),
            min_busy: secs("REMUX_ATTENTION_MIN_BUSY_SECS", 3.0),
            quiet: secs("REMUX_ATTENTION_QUIET_SECS", 5.0),
        }
    }
}

/// Pure busy→quiet detector, fed (last-activity, now) unix-seconds samples.
/// tmux `window_activity` has whole-second resolution; a "busy period" is a
/// stretch of samples whose activity timestamp keeps advancing.
pub struct Detector {
    min_busy: f64,
    quiet: f64,
    last_activity: Option<f64>,
    busy_start: Option<f64>,
}

impl Detector {
    pub fn new(cfg: &Config) -> Self {
        Self {
            min_busy: cfg.min_busy.as_secs_f64(),
            quiet: cfg.quiet.as_secs_f64(),
            last_activity: None,
            busy_start: None,
        }
    }

    /// Feed one sample; returns true when attention should be raised.
    pub fn observe(&mut self, activity: f64, now: f64) -> bool {
        let Some(prev) = self.last_activity else {
            // First sample is the baseline: never fire for output that
            // predates the monitor.
            self.last_activity = Some(activity);
            return false;
        };
        if activity > prev {
            // New output since the last sample: start or extend a busy period.
            self.busy_start.get_or_insert(activity);
            self.last_activity = Some(activity);
            return false;
        }
        let Some(start) = self.busy_start else {
            return false;
        };
        if now - prev < self.quiet {
            return false;
        }
        // Quiet long enough: the busy period is over. Fire once, only if it
        // was long enough to look like real work rather than a keystroke echo.
        self.busy_start = None;
        prev - start >= self.min_busy
    }
}

pub fn spawn(app: Arc<App>) {
    tokio::spawn(monitor(app, Config::from_env()));
}

/// Watches every session on the tmux server (one `list-windows -a` per poll;
/// window activity tracks content output, unlike `session_activity` which
/// tracks client input). Events carry the session name; each websocket
/// forwards only events for the session it is attached to.
/// Apply every queued detector reset (each drops that session's detector so it
/// re-baselines on the next sample). On Lagged the dropped payloads can't be
/// reconstructed, so clear all detectors — the conservative choice: miss a
/// suppression rather than let a stale busy epoch double-fire. Non-blocking.
fn drain_resets(
    resets: &mut tokio::sync::broadcast::Receiver<String>,
    detectors: &mut std::collections::HashMap<String, Detector>,
) {
    loop {
        match resets.try_recv() {
            Ok(session) => {
                detectors.remove(&session);
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => detectors.clear(),
            Err(_) => break, // Empty or Closed — stop draining
        }
    }
}

async fn monitor(app: Arc<App>, cfg: Config) {
    let mut detectors: std::collections::HashMap<String, Detector> =
        std::collections::HashMap::new();
    // A precise `command_finished` (M4c) resets a session's detector so the
    // busy→quiet heuristic doesn't *also* fire for the same command. Dropping
    // the detector re-baselines it on the next sample. NB: the detector is
    // per-session, so this suppresses the whole session's busy epoch, not just
    // the finishing pane — an accepted coarseness of the existing heuristic.
    let mut resets = app.detector_reset.subscribe();
    // Guard against a misconfigured zero poll (interval panics on zero); Delay
    // matches the old sleep-loop cadence (no back-to-back catch-up ticks).
    let mut ticker = tokio::time::interval(cfg.poll.max(Duration::from_millis(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            r = resets.recv() => match r {
                // The reset that *woke* us must be applied here — recv() has
                // already consumed it, so the drain below can't see it. (This
                // was the bug: dropping `Ok(session)` meant a lone
                // command_finished never reset its detector, and the heuristic
                // could still double-fire "went quiet".)
                Ok(session) => {
                    detectors.remove(&session);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break, // shutting down
                // Lagged: some resets were dropped and can't be reconstructed by
                // draining. Clear every detector so a missed reset can't leave a
                // stale busy epoch that double-fires (each re-baselines on the
                // next sample).
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => detectors.clear(),
            },
        }
        // Drain any further resets queued since the wake (each removes that
        // session's detector) before observing.
        drain_resets(&mut resets, &mut detectors);
        let sessions = tokio::task::spawn_blocking(tmux::sessions_activity).await;
        let Ok(Ok(sessions)) = sessions else {
            continue; // tmux briefly unavailable
        };
        // Drain AGAIN: a reset can land while the (awaited) tmux query is in
        // flight, and it must win over the sample we're about to observe — else
        // the heuristic could fire "went quiet" for the very command that just
        // reset us (Codex).
        drain_resets(&mut resets, &mut detectors);
        detectors.retain(|name, _| sessions.iter().any(|(n, _)| n == name));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        for (name, activity) in sessions {
            let detector = detectors
                .entry(name.clone())
                .or_insert_with(|| Detector::new(&cfg));
            if detector.observe(activity as f64, now) {
                tracing::debug!(session = %name, "attention raised");
                let _ = app.attention.send(crate::Attention::quiet(name));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector(min_busy: f64, quiet: f64) -> Detector {
        Detector::new(&Config {
            poll: Duration::from_secs(1),
            min_busy: Duration::from_secs_f64(min_busy),
            quiet: Duration::from_secs_f64(quiet),
        })
    }

    #[test]
    fn fires_after_busy_then_quiet() {
        let mut d = detector(3.0, 5.0);
        assert!(!d.observe(100.0, 100.0)); // baseline
        for t in 101..=110 {
            assert!(!d.observe(t as f64, t as f64)); // busy: activity advances
        }
        assert!(!d.observe(110.0, 112.0)); // quiet, but not long enough
        assert!(d.observe(110.0, 116.0)); // quiet >= 5s after 9s busy
        assert!(!d.observe(110.0, 130.0)); // fires only once
    }

    #[test]
    fn short_burst_does_not_fire() {
        let mut d = detector(3.0, 5.0);
        assert!(!d.observe(100.0, 100.0));
        assert!(!d.observe(101.0, 101.0)); // single echo: busy span 0
        assert!(!d.observe(101.0, 120.0)); // long quiet, but busy < min_busy
    }

    #[test]
    fn no_fire_without_any_activity() {
        let mut d = detector(3.0, 5.0);
        assert!(!d.observe(100.0, 200.0)); // baseline from old activity
        assert!(!d.observe(100.0, 300.0));
    }

    #[test]
    fn new_busy_period_can_fire_again() {
        let mut d = detector(2.0, 3.0);
        assert!(!d.observe(100.0, 100.0));
        for t in 101..=105 {
            assert!(!d.observe(t as f64, t as f64));
        }
        assert!(d.observe(105.0, 109.0)); // first fire
        for t in 120..=125 {
            assert!(!d.observe(t as f64, t as f64));
        }
        assert!(d.observe(125.0, 129.0)); // second busy period fires too
    }
}
