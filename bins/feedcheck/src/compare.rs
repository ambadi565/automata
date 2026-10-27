//! Live bars vs Angel's 1-minute candles, the M1 exit criterion: every
//! minute's OHLC within ±1 tick.

use std::collections::{BTreeMap, BTreeSet};

use saarthi_core::{Bar, Symbol, Ts};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub ts: Ts,
    pub field: &'static str,
    pub live: i64,
    pub reference: i64,
}

#[derive(Debug, Clone)]
pub struct SymbolReport {
    pub symbol: Symbol,
    pub compared: usize,
    /// Partial live bars (late start or feed gap) left out of the check.
    pub excluded: usize,
    /// Minutes Angel has a candle for but the live feed built no bar.
    pub missing_live: Vec<Ts>,
    /// Minutes the live feed built a bar for but Angel has no candle.
    pub missing_reference: Vec<Ts>,
    pub price_mismatches: Vec<Mismatch>,
    /// Largest |live − Angel| / Angel volume over compared minutes, in %.
    pub max_volume_diff_pct: f64,
    pub median_volume_diff_pct: f64,
}

impl SymbolReport {
    pub fn passes(&self, volume_tolerance_pct: Option<f64>) -> bool {
        self.compared > 0
            && self.missing_live.is_empty()
            && self.missing_reference.is_empty()
            && self.price_mismatches.is_empty()
            && volume_tolerance_pct.is_none_or(|t| self.max_volume_diff_pct <= t)
    }
}

/// Compares bars with start times in `[from, to)`.
///
/// `tolerance` is in paise (one tick is 5 paise for most NSE equities).
pub fn compare(
    symbol: Symbol,
    live: &[Bar],
    reference: &[Bar],
    tolerance: i64,
    excluded: &BTreeSet<Ts>,
    from: Ts,
    to: Ts,
) -> SymbolReport {
    let in_window = |b: &&Bar| b.ts >= from && b.ts < to;
    let live: BTreeMap<Ts, &Bar> = live.iter().filter(in_window).map(|b| (b.ts, b)).collect();
    let refs: BTreeMap<Ts, &Bar> = reference.iter().filter(in_window).map(|b| (b.ts, b)).collect();
    let mut r = SymbolReport {
        symbol,
        compared: 0,
        excluded: 0,
        missing_live: vec![],
        missing_reference: vec![],
        price_mismatches: vec![],
        max_volume_diff_pct: 0.0,
        median_volume_diff_pct: 0.0,
    };
    let mut vol_diffs = Vec::new();
    let minutes: BTreeSet<Ts> = live.keys().chain(refs.keys()).copied().collect();
    for ts in minutes {
        if excluded.contains(&ts) {
            r.excluded += 1;
            continue;
        }
        match (live.get(&ts), refs.get(&ts)) {
            (Some(l), Some(a)) => {
                r.compared += 1;
                for (field, lv, av) in [
                    ("open", l.open.0, a.open.0),
                    ("high", l.high.0, a.high.0),
                    ("low", l.low.0, a.low.0),
                    ("close", l.close.0, a.close.0),
                ] {
                    if (lv - av).abs() > tolerance {
                        r.price_mismatches.push(Mismatch {
                            ts,
                            field,
                            live: lv,
                            reference: av,
                        });
                    }
                }
                if a.volume > 0 {
                    vol_diffs.push((l.volume as f64 - a.volume as f64).abs() / a.volume as f64 * 100.0);
                }
            }
            (None, Some(_)) => r.missing_live.push(ts),
            (Some(_), None) => r.missing_reference.push(ts),
            (None, None) => unreachable!(),
        }
    }
    vol_diffs.sort_by(f64::total_cmp);
    r.max_volume_diff_pct = vol_diffs.last().copied().unwrap_or(0.0);
    r.median_volume_diff_pct = vol_diffs.get(vol_diffs.len() / 2).copied().unwrap_or(0.0);
    r
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};
    use saarthi_core::Price;

    use super::*;

    fn t(m: i64) -> Ts {
        DateTime::<Utc>::from_timestamp(1_793_072_700, 0).unwrap() + TimeDelta::minutes(m)
    }

    fn bar(m: i64, c: i64, v: u64) -> Bar {
        Bar {
            ts: t(m),
            open: Price(c),
            high: Price(c + 10),
            low: Price(c - 10),
            close: Price(c),
            volume: v,
        }
    }

    #[test]
    fn within_one_tick_passes() {
        let live = [bar(0, 1000, 100), bar(1, 1005, 99)];
        let angel = [bar(0, 1000, 100), bar(1, 1000, 100)];
        let r = compare(Symbol::new("X"), &live, &angel, 5, &BTreeSet::new(), t(0), t(375));
        assert!(r.passes(None), "{r:?}");
        assert_eq!(r.compared, 2);
        assert!((r.max_volume_diff_pct - 1.0).abs() < 1e-9);
        assert!(!r.passes(Some(0.5)));
    }

    #[test]
    fn reports_every_kind_of_difference() {
        let live = [bar(0, 1000, 100), bar(1, 1010, 100), bar(3, 1000, 1)];
        let angel = [bar(0, 1000, 100), bar(1, 1000, 100), bar(2, 1000, 1)];
        let r = compare(Symbol::new("X"), &live, &angel, 5, &BTreeSet::new(), t(0), t(375));
        assert!(!r.passes(None));
        assert_eq!(r.missing_live, vec![t(2)]);
        assert_eq!(r.missing_reference, vec![t(3)]);
        assert_eq!(r.price_mismatches.len(), 4, "all four prices are 2 ticks off");
        let excl: BTreeSet<_> = [t(1), t(2), t(3)].into();
        let r = compare(Symbol::new("X"), &live, &angel, 5, &excl, t(0), t(375));
        assert!(r.passes(None));
        assert_eq!(r.excluded, 3);
        // Outside the window nothing is compared, and nothing compared fails.
        let r = compare(Symbol::new("X"), &live, &angel, 5, &BTreeSet::new(), t(500), t(600));
        assert!(!r.passes(None));
    }
}
