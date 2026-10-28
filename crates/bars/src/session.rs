use chrono::TimeDelta;
use saarthi_core::{Bar, Price, Ts};

/// The high and low of the first `minutes` of the session (09:15–09:30 by
/// default), for opening-range breakouts.
#[derive(Debug, Clone)]
pub struct OpeningRange {
    minutes: u32,
    start: Option<Ts>,
    high: Option<Price>,
    low: Option<Price>,
    complete: bool,
    /// Set when the range was rebuilt from bars after a late start.
    seeded: bool,
}

impl OpeningRange {
    pub fn new(minutes: u32) -> Self {
        assert!(minutes > 0, "opening range needs at least one minute");
        OpeningRange {
            minutes,
            start: None,
            high: None,
            low: None,
            complete: false,
            seeded: false,
        }
    }

    pub fn reset(&mut self, session_open: Ts) {
        *self = OpeningRange::new(self.minutes);
        self.start = Some(session_open);
    }

    /// The session open the range is anchored to, once reset.
    pub fn session_start(&self) -> Option<Ts> {
        self.start
    }

    fn end(&self) -> Option<Ts> {
        self.start.map(|s| s + TimeDelta::minutes(i64::from(self.minutes)))
    }

    /// Adds a trade bucketed at `ts`. Trades after the window complete it.
    pub fn on_trade(&mut self, ts: Ts, price: Price) {
        let (Some(start), Some(end)) = (self.start, self.end()) else {
            return;
        };
        if ts >= end {
            self.complete = true;
        } else if ts >= start && !self.complete {
            self.high = Some(self.high.map_or(price, |h| h.max(price)));
            self.low = Some(self.low.map_or(price, |l| l.min(price)));
        }
    }

    pub fn on_time(&mut self, now: Ts) {
        if self.end().is_some_and(|e| now >= e) {
            self.complete = true;
        }
    }

    /// Rebuilds the range from today's 1-minute bars (from the REST API after
    /// a restart past 09:15). Bars outside the window are ignored.
    pub fn seed_from_bars(&mut self, bars: &[Bar]) {
        let (Some(start), Some(end)) = (self.start, self.end()) else {
            return;
        };
        for b in bars.iter().filter(|b| b.ts >= start && b.ts < end) {
            self.high = Some(self.high.map_or(b.high, |h| h.max(b.high)));
            self.low = Some(self.low.map_or(b.low, |l| l.min(b.low)));
            self.seeded = true;
        }
    }

    /// `(high, low)`, available once the window has closed.
    pub fn range(&self) -> Option<(Price, Price)> {
        if !self.complete {
            return None;
        }
        Some((self.high?, self.low?))
    }

    /// The range so far, even while it is still forming.
    pub fn forming(&self) -> Option<(Price, Price)> {
        Some((self.high?, self.low?))
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn was_seeded(&self) -> bool {
        self.seeded
    }
}

/// Session VWAP from trades, accumulated exactly in integers.
///
/// The exchange also sends its own day VWAP (average traded price) in Quote
/// mode, which covers trades from before the engine connected; the snapshot
/// prefers that and keeps this as the fallback and cross-check.
#[derive(Debug, Clone, Default)]
pub struct SessionVwap {
    pv: i128,
    volume: u128,
}

impl SessionVwap {
    pub fn reset(&mut self) {
        *self = SessionVwap::default();
    }

    pub fn on_trade(&mut self, price: Price, qty: u64) {
        self.pv += i128::from(price.0) * i128::from(qty);
        self.volume += u128::from(qty);
    }

    pub fn volume(&self) -> u128 {
        self.volume
    }

    /// Rounded to the nearest paisa.
    pub fn vwap(&self) -> Option<Price> {
        if self.volume == 0 {
            return None;
        }
        let v = self.volume as i128;
        let q = self.pv.div_euclid(v);
        let r = self.pv.rem_euclid(v);
        Some(Price((q + i128::from(r * 2 >= v)) as i64))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;

    fn t(min: i64) -> Ts {
        DateTime::<Utc>::from_timestamp(1_793_072_700, 0).unwrap() + TimeDelta::minutes(min)
    }

    #[test]
    fn opening_range_forms_then_locks() {
        let mut or = OpeningRange::new(15);
        or.reset(t(0));
        or.on_trade(t(0), Price(100));
        or.on_trade(t(5), Price(110));
        or.on_trade(t(14), Price(95));
        assert_eq!(or.range(), None);
        assert_eq!(or.forming(), Some((Price(110), Price(95))));
        or.on_trade(t(15), Price(200));
        assert_eq!(or.range(), Some((Price(110), Price(95))));
        or.on_trade(t(16), Price(50));
        assert_eq!(or.range(), Some((Price(110), Price(95))), "locked");

        let mut quiet = OpeningRange::new(15);
        quiet.reset(t(0));
        quiet.on_trade(t(3), Price(10));
        quiet.on_time(t(15));
        assert!(quiet.is_complete());
    }

    #[test]
    fn opening_range_seeds_from_bars() {
        let mut or = OpeningRange::new(15);
        or.reset(t(0));
        let bar = |m, h, l| Bar {
            ts: t(m),
            open: Price(l),
            high: Price(h),
            low: Price(l),
            close: Price(h),
            volume: 1,
        };
        or.seed_from_bars(&[bar(0, 105, 100), bar(14, 120, 101), bar(15, 999, 1)]);
        or.on_time(t(20));
        assert_eq!(or.range(), Some((Price(120), Price(100))));
        assert!(or.was_seeded());
    }

    #[test]
    fn vwap_is_exact_and_rounds() {
        let mut v = SessionVwap::default();
        assert_eq!(v.vwap(), None);
        v.on_trade(Price(100), 1);
        v.on_trade(Price(101), 1);
        assert_eq!(v.vwap(), Some(Price(101))); // 100.5 rounds up
        v.on_trade(Price(100), 1);
        assert_eq!(v.vwap(), Some(Price(100))); // 100.33
        // No overflow at Rs 1 lakh x a billion shares.
        let mut big = SessionVwap::default();
        for _ in 0..1000 {
            big.on_trade(Price(10_000_000), 1_000_000_000);
        }
        assert_eq!(big.vwap(), Some(Price(10_000_000)));
    }
}
