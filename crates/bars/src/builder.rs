use chrono::{NaiveDate, NaiveTime, TimeDelta, Timelike};
use saarthi_core::{Bar, IST, MarketClock, Price, SessionTimes, Tf, Tick, Ts};
use serde::{Deserialize, Serialize};

/// What to do with ticks from the 09:00–09:15 pre-open session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreOpenPolicy {
    /// Count the auction trade (price and volume) in the 09:15 bar.
    #[default]
    FoldIntoOpen,
    /// Ignore pre-open trades; their volume only sets the baseline.
    Ignore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BarConfig {
    pub pre_open: PreOpenPolicy,
    /// How long after a minute ends a bar waits for late ticks before
    /// [`BarBuilder::on_time`] closes it.
    pub close_grace_ms: u64,
    /// Timeframes above 1 minute to build. Each must divide the session into
    /// whole minutes; 5m is what the strategies use.
    pub higher: Vec<Tf>,
    #[serde(skip)]
    pub session: SessionTimes,
}

impl Default for BarConfig {
    fn default() -> Self {
        BarConfig {
            pre_open: PreOpenPolicy::default(),
            close_grace_ms: 2_000,
            higher: vec![Tf::M5],
            session: SessionTimes::default(),
        }
    }
}

/// A completed bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarEvent {
    pub tf: Tf,
    pub bar: Bar,
    /// The bar may be missing trades: it is the first after a mid-session
    /// start or a feed gap. Compare and trade on it with care.
    pub partial: bool,
}

/// A trade inferred from a tick, after pre-open folding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    /// The time it is bucketed at (09:15 for folded pre-open trades).
    pub ts: Ts,
    pub price: Price,
    /// Volume since the previous tick (0 in LTP mode or after a gap).
    pub qty: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickOutcome {
    /// A new IST trading day started with this tick.
    pub new_session: bool,
    pub trade: Option<Trade>,
    /// Bars this tick closed, 1m first, then higher timeframes.
    pub bars: Vec<BarEvent>,
}

#[derive(Debug, Clone)]
struct Building {
    bar: Bar,
    end: Ts,
    partial: bool,
}

impl Building {
    fn new(start: Ts, len: TimeDelta, price: Price, qty: u64, partial: bool) -> Self {
        Building {
            bar: Bar {
                ts: start,
                open: price,
                high: price,
                low: price,
                close: price,
                volume: qty,
            },
            end: start + len,
            partial,
        }
    }

    fn add(&mut self, price: Price, qty: u64) {
        let b = &mut self.bar;
        b.high = b.high.max(price);
        b.low = b.low.min(price);
        b.close = price;
        b.volume += qty;
    }

    fn merge(&mut self, other: &Bar, partial: bool) {
        let b = &mut self.bar;
        b.high = b.high.max(other.high);
        b.low = b.low.min(other.low);
        b.close = other.close;
        b.volume += other.volume;
        self.partial |= partial;
    }
}

/// Rolls 1-minute bars up into one higher timeframe.
#[derive(Debug, Clone)]
struct Rollup {
    tf: Tf,
    cur: Option<Building>,
}

impl Rollup {
    fn bucket(&self, open: Ts, ts: Ts) -> Ts {
        let n = i64::from(self.tf.minutes());
        let mins = (ts - open).num_minutes().div_euclid(n) * n;
        open + TimeDelta::minutes(mins)
    }

    fn on_1m(&mut self, open: Ts, ev: &BarEvent, out: &mut Vec<BarEvent>) {
        let start = self.bucket(open, ev.bar.ts);
        match &mut self.cur {
            Some(c) if c.bar.ts == start => c.merge(&ev.bar, ev.partial),
            _ => {
                self.flush(out);
                let len = TimeDelta::minutes(i64::from(self.tf.minutes()));
                let mut b = Building::new(start, len, ev.bar.open, 0, ev.partial);
                b.merge(&ev.bar, ev.partial);
                self.cur = Some(b);
            }
        }
        // The bucket's last minute is in: no need to wait.
        if self
            .cur
            .as_ref()
            .is_some_and(|c| ev.bar.ts + TimeDelta::minutes(1) >= c.end)
        {
            self.flush(out);
        }
    }

    fn flush(&mut self, out: &mut Vec<BarEvent>) {
        if let Some(c) = self.cur.take() {
            out.push(BarEvent {
                tf: self.tf,
                bar: c.bar,
                partial: c.partial,
            });
        }
    }
}

/// Builds one instrument's bars from its ticks.
#[derive(Debug, Clone)]
pub struct BarBuilder {
    cfg: BarConfig,
    day: Option<NaiveDate>,
    open: Ts,
    pre_open: Ts,
    close: Ts,
    last_volume: Option<u64>,
    gap: bool,
    cur: Option<Building>,
    /// End of the last closed 1m bar; later ticks never reopen it.
    last_closed: Option<Ts>,
    rollups: Vec<Rollup>,
    late_ticks: u64,
}

impl BarBuilder {
    pub fn new(cfg: BarConfig) -> Self {
        for tf in &cfg.higher {
            assert!(
                tf.is_intraday() && *tf != Tf::M1,
                "higher timeframes must be intraday and above 1m"
            );
        }
        let rollups = cfg.higher.iter().map(|&tf| Rollup { tf, cur: None }).collect();
        BarBuilder {
            cfg,
            day: None,
            open: Ts::default(),
            pre_open: Ts::default(),
            close: Ts::default(),
            last_volume: None,
            gap: false,
            cur: None,
            last_closed: None,
            rollups,
            late_ticks: 0,
        }
    }

    /// Ticks that arrived for a minute already closed and were counted in the
    /// next bar instead.
    pub fn late_ticks(&self) -> u64 {
        self.late_ticks
    }

    pub fn session_open(&self) -> Option<Ts> {
        self.day.map(|_| self.open)
    }

    /// Call when the feed reconnects. The volume traded while disconnected
    /// cannot be placed in the right minutes, so it is dropped and the next bar
    /// is marked partial.
    pub fn mark_gap(&mut self) {
        self.gap = true;
    }

    fn start_day(&mut self, day: NaiveDate, out: &mut Vec<BarEvent>) {
        self.flush_into(out);
        let at = |t: NaiveTime| MarketClock::at(day, t);
        let s = self.cfg.session;
        self.day = Some(day);
        self.pre_open = at(s.pre_open);
        self.open = at(s.open);
        self.close = at(s.close);
        self.last_volume = None;
        self.last_closed = None;
        self.gap = false;
    }

    pub fn on_tick(&mut self, t: &Tick) -> TickOutcome {
        let mut out = TickOutcome::default();
        let day = t.ts.with_timezone(&IST).date_naive();
        if self.day != Some(day) {
            if self.day.is_some_and(|d| day < d) {
                return out; // a stale tick from a previous day
            }
            self.start_day(day, &mut out.bars);
            out.new_session = true;
        }
        if t.ts < self.pre_open || t.ts >= self.close {
            return out;
        }
        let pre_open = t.ts < self.open;
        if pre_open && self.cfg.pre_open == PreOpenPolicy::Ignore {
            if t.day_volume.is_some() {
                self.last_volume = t.day_volume;
            }
            return out;
        }

        let first_minute = t.ts < self.open + TimeDelta::minutes(1);
        let (qty, is_trade, partial) = match (t.day_volume, self.last_volume) {
            // LTP mode: every tick is a trade we cannot size.
            (None, _) => (0, true, false),
            (Some(v), _) if self.gap => {
                self.gap = false;
                self.last_volume = Some(v);
                (0, true, true)
            }
            // First sight of the day. Zero volume means no trade yet, so the
            // price is still yesterday's. Before the first minute is over the
            // day's volume so far belongs in the opening bar; later it means
            // we started mid-session.
            (Some(v), None) => {
                self.last_volume = Some(v);
                if v == 0 {
                    (0, false, false)
                } else if pre_open || first_minute {
                    (v, true, false)
                } else {
                    (0, true, true)
                }
            }
            (Some(v), Some(last)) if v >= last => {
                self.last_volume = Some(v);
                (v - last, v > last, false)
            }
            (Some(v), Some(last)) => {
                tracing::warn!(token = %t.token, v, last, "cumulative volume went backwards; rebasing");
                self.last_volume = Some(v);
                (0, false, true)
            }
        };
        if !is_trade {
            return out;
        }

        let mut bucket = if pre_open { self.open } else { floor_minute(t.ts) };
        let min_bucket = self.cur.as_ref().map(|c| c.bar.ts);
        if let Some(m) = min_bucket
            && bucket < m
        {
            bucket = m; // late tick: count it in the bar still open
        }
        out.trade = Some(Trade {
            ts: bucket,
            price: t.ltp,
            qty,
        });
        match &mut self.cur {
            Some(c) if c.bar.ts == bucket => {
                c.add(t.ltp, qty);
                c.partial |= partial;
            }
            _ => {
                self.close_current(&mut out.bars);
                if let Some(last_end) = self.last_closed
                    && bucket < last_end
                {
                    self.late_ticks += 1;
                    bucket = last_end;
                    if let Some(tr) = out.trade.as_mut() {
                        tr.ts = bucket;
                    }
                }
                self.cur = Some(Building::new(bucket, TimeDelta::minutes(1), t.ltp, qty, partial));
            }
        }
        out
    }

    /// Closes bars whose time is up. Call about once a second with the wall
    /// clock so quiet symbols still produce bars on time.
    pub fn on_time(&mut self, now: Ts) -> Vec<BarEvent> {
        let mut out = Vec::new();
        let grace = TimeDelta::milliseconds(self.cfg.close_grace_ms as i64);
        if self.cur.as_ref().is_some_and(|c| now >= c.end + grace) {
            self.close_current(&mut out);
        }
        for r in &mut self.rollups {
            if r.cur.as_ref().is_some_and(|c| now >= c.end + grace) {
                r.flush(&mut out);
            }
        }
        out
    }

    /// Closes everything still open (end of session or shutdown).
    pub fn flush(&mut self) -> Vec<BarEvent> {
        let mut out = Vec::new();
        self.flush_into(&mut out);
        out
    }

    fn flush_into(&mut self, out: &mut Vec<BarEvent>) {
        self.close_current(out);
        for r in &mut self.rollups {
            r.flush(out);
        }
    }

    fn close_current(&mut self, out: &mut Vec<BarEvent>) {
        let Some(c) = self.cur.take() else { return };
        self.last_closed = Some(c.end);
        let ev = BarEvent {
            tf: Tf::M1,
            bar: c.bar,
            partial: c.partial,
        };
        out.push(ev);
        let open = self.open;
        for r in &mut self.rollups {
            r.on_1m(open, &ev, out);
        }
    }
}

fn floor_minute(ts: Ts) -> Ts {
    ts.with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .expect("valid time")
}
