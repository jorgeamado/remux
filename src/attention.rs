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

async fn monitor(app: Arc<App>, cfg: Config) {
    let session = app.args.session.clone();
    let mut detector = Detector::new(&cfg);
    loop {
        tokio::time::sleep(cfg.poll).await;
        let s = session.clone();
        let activity = tokio::task::spawn_blocking(move || tmux::last_activity(&s)).await;
        let Ok(Ok(Some(ts))) = activity else {
            continue; // session not created yet, or tmux briefly unavailable
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        if detector.observe(ts as f64, now) {
            tracing::debug!(session = %session, "attention raised");
            let _ = app.attention.send(());
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
