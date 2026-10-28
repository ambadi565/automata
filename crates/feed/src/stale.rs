//! Feed staleness: "no ticks for N seconds during market hours".
//!
//! Pure logic with an injected clock, so it is tested without sleeping. The
//! client calls [`StalenessMonitor::check`] about once a second.

use std::time::Duration;

use saarthi_core::Ts;

/// How the feed looks to the risk guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    /// Outside the hours in which ticks are expected.
    Idle,
    /// Ticks are arriving.
    Fresh,
    /// No ticks for at least `warn_after`: block new entries.
    Stale { silent: Duration },
    /// No ticks for at least `critical_after`: flatten open positions.
    Critical { silent: Duration },
}

impl Staleness {
    fn level(self) -> u8 {
        match self {
            Staleness::Idle => 0,
            Staleness::Fresh => 1,
            Staleness::Stale { .. } => 2,
            Staleness::Critical { .. } => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StalenessMonitor {
    warn_after: Duration,
    critical_after: Duration,
    last_tick: Option<Ts>,
    /// When the current active window (market hours) began being watched.
    active_since: Option<Ts>,
    state: Staleness,
}

impl StalenessMonitor {
    pub fn new(warn_after: Duration, critical_after: Duration) -> Self {
        assert!(warn_after <= critical_after, "warn threshold must not exceed critical");
        StalenessMonitor {
            warn_after,
            critical_after,
            last_tick: None,
            active_since: None,
            state: Staleness::Idle,
        }
    }

    pub fn on_tick(&mut self, now: Ts) {
        self.last_tick = Some(now);
    }

    pub fn last_tick(&self) -> Option<Ts> {
        self.last_tick
    }

    pub fn state(&self) -> Staleness {
        self.state
    }

    /// Re-evaluates at `now`. Returns the new state when its level changed.
    ///
    /// Silence is measured from the later of the last tick and the moment the
    /// active window started, so the open at 09:15 does not count the
    /// overnight gap as silence.
    pub fn check(&mut self, now: Ts, active: bool) -> Option<Staleness> {
        let next = if !active {
            self.active_since = None;
            Staleness::Idle
        } else {
            let since = *self.active_since.get_or_insert(now);
            let from = self.last_tick.map_or(since, |t| t.max(since));
            let silent = (now - from).to_std().unwrap_or_default();
            if silent >= self.critical_after {
                Staleness::Critical { silent }
            } else if silent >= self.warn_after {
                Staleness::Stale { silent }
            } else {
                Staleness::Fresh
            }
        };
        let changed = next.level() != self.state.level();
        self.state = next;
        changed.then_some(next)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};

    use super::*;

    #[test]
    fn escalates_and_recovers() {
        let t0 = Utc::now();
        let s = |n: i64| t0 + TimeDelta::seconds(n);
        let mut m = StalenessMonitor::new(Duration::from_secs(10), Duration::from_secs(120));
        assert_eq!(m.check(s(0), false), None); // already Idle
        assert_eq!(m.check(s(1), true), Some(Staleness::Fresh));
        assert_eq!(m.check(s(10), true), None);
        assert_eq!(
            m.check(s(11), true),
            Some(Staleness::Stale {
                silent: Duration::from_secs(10)
            })
        );
        assert_eq!(m.check(s(60), true), None, "same level, no event");
        assert!(matches!(m.check(s(121), true), Some(Staleness::Critical { .. })));
        m.on_tick(s(122));
        assert_eq!(m.check(s(123), true), Some(Staleness::Fresh));
        assert_eq!(m.check(s(200), false), Some(Staleness::Idle));
    }

    #[test]
    fn overnight_gap_is_not_silence() {
        let t0 = Utc::now();
        let mut m = StalenessMonitor::new(Duration::from_secs(10), Duration::from_secs(120));
        m.on_tick(t0);
        m.check(t0, false);
        let open = t0 + TimeDelta::hours(18);
        assert_eq!(m.check(open, true), Some(Staleness::Fresh));
        assert_eq!(m.check(open + TimeDelta::seconds(5), true), None);
    }
}
