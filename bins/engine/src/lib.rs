//! Library half of the `engine` daemon, so the whole trading loop can be
//! driven by tests and by `engine replay`.

pub mod alerts;
pub mod config;
pub mod engine;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, TimeDelta};
use saarthi_broker::backtest::ExecutionCosts;
use saarthi_broker::{Paper, PaperConfig};
use saarthi_core::{Bar, HolidayCalendar, Instrument, MarketClock, Symbol, Tf, Ts};
use saarthi_risk::RiskLimits;
use saarthi_store::DataLayout;
use saarthi_store::parquet::{read_bars, read_instruments};
use serde::Serialize;

pub use engine::{DaySummary, Engine, Parts};

use crate::config::Config;

/// Files of one account's paper-trading state.
#[derive(Debug, Clone)]
pub struct AccountFiles {
    pub dir: PathBuf,
}

impl AccountFiles {
    /// `data/accounts/<client>/<mode>/`.
    pub fn new(layout: &DataLayout, client_code: &str, mode: &str) -> Self {
        AccountFiles {
            dir: layout.account_dir(client_code).join(mode),
        }
    }

    pub fn db(&self) -> PathBuf {
        self.dir.join("saarthi.db")
    }

    pub fn paper_state(&self) -> PathBuf {
        self.dir.join("paper.json")
    }

    pub fn halt_file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// Everything loaded from config files (no network).
pub struct Loaded {
    pub cfg: Config,
    pub layout: DataLayout,
    pub limits: RiskLimits,
    pub costs: ExecutionCosts,
    pub clock: MarketClock,
    pub symbols: Vec<Symbol>,
    pub exchange: saarthi_core::Exchange,
}

pub fn load(cfg: Config, data_dir: Option<PathBuf>) -> Result<Loaded> {
    let layout = DataLayout::new(data_dir.unwrap_or_else(|| cfg.paths.data_dir.clone()));
    let limits = RiskLimits::load(&cfg.engine.risk)?;
    let costs = ExecutionCosts::load(&cfg.engine.costs)?;
    let calendar = HolidayCalendar::load(&cfg.paths.holidays)?;
    let times = limits.session()?;
    let clock = MarketClock::new(times, calendar);
    let universe = fetch::universe::Universe::load(&cfg.paths.universe)?;
    let symbols = if cfg.engine.symbols.is_empty() {
        universe.symbols.clone()
    } else {
        let allowed: BTreeSet<&Symbol> = universe.symbols.iter().collect();
        let mut v = Vec::new();
        for s in &cfg.engine.symbols {
            let s = Symbol::new(s.trim());
            if !allowed.contains(&s) {
                bail!("[engine] symbols: {s} is not in the universe");
            }
            v.push(s);
        }
        v
    };
    Ok(Loaded {
        cfg,
        layout,
        limits,
        costs,
        clock,
        symbols,
        exchange: universe.exchange,
    })
}

/// The instruments to trade, from the saved instrument master.
pub fn instruments_from(all: Vec<Instrument>, l: &Loaded) -> Result<Vec<Instrument>> {
    let missing = fetch::missing_symbols(&all, l.exchange, &l.symbols);
    if !missing.is_empty() {
        tracing::warn!(?missing, "symbols not in the instrument master are skipped");
    }
    let wanted: BTreeSet<&Symbol> = l.symbols.iter().collect();
    let v: Vec<Instrument> = all
        .into_iter()
        .filter(|i| i.exchange == l.exchange && wanted.contains(&i.symbol))
        .collect();
    if v.is_empty() {
        bail!("no tradable instruments (is data/instruments.parquet current?)");
    }
    Ok(v)
}

pub fn saved_instruments(l: &Loaded) -> Result<Vec<Instrument>> {
    let p = l.layout.instruments();
    let all = read_instruments(&p).with_context(|| format!("reading {} (run `fetch instruments`)", p.display()))?;
    instruments_from(all, l)
}

/// The last `days` trading days of candles on `tf` before `date`, per token.
pub fn warmup(l: &Loaded, insts: &[Instrument], tf: Tf, date: NaiveDate, days: usize) -> HashMap<String, Vec<Bar>> {
    let mut out = HashMap::new();
    if days == 0 {
        return out;
    }
    let start = MarketClock::at(date, l.clock.times.pre_open);
    for i in insts {
        let path = l.layout.candles(tf, &i.symbol);
        let Ok(bars) = read_bars(&path) else { continue };
        let before: Vec<Bar> = bars.into_iter().filter(|b| b.ts < start).collect();
        let dates: BTreeSet<NaiveDate> = before.iter().map(|b| MarketClock::ist_date(b.ts)).collect();
        let Some(&first) = dates.iter().rev().nth(days.saturating_sub(1)).or(dates.iter().next()) else {
            continue;
        };
        out.insert(
            i.token.clone(),
            before
                .into_iter()
                .filter(|b| MarketClock::ist_date(b.ts) >= first)
                .collect(),
        );
    }
    out
}

pub fn paper(l: &Loaded, state: Option<PathBuf>) -> Result<Paper> {
    Ok(Paper::open(PaperConfig {
        capital: saarthi_risk::inr(l.cfg.engine.paper_capital_inr),
        costs: l.costs.charges.clone(),
        slippage: l.costs.slippage.clone(),
        max_fill_qty_per_tick: None,
        state_path: state,
    })?)
}

pub fn secs(s: u64) -> TimeDelta {
    TimeDelta::seconds(s as i64)
}

/// The M3 exit check: `n` consecutive trading days with no crash or
/// restart and everything flat after the square-off.
#[derive(Debug, Clone, Serialize)]
pub struct ExitCheck {
    pub required: usize,
    pub streak: usize,
    pub passed: bool,
    pub days: Vec<(NaiveDate, bool, String)>,
}

pub fn exit_check(days: &[(NaiveDate, DaySummary)], clock: &MarketClock, n: usize) -> ExitCheck {
    let mut rows = Vec::new();
    let mut streak = 0usize;
    let mut best = 0usize;
    let mut prev: Option<NaiveDate> = None;
    for (d, s) in days {
        let mut why = Vec::new();
        if s.restarts > 0 {
            why.push(format!("{} restart(s)", s.restarts));
        }
        if !s.flat_after_square_off {
            why.push("not confirmed flat".to_string());
        }
        let ok = why.is_empty();
        let consecutive = prev.is_none_or(|p| clock.next_trading_day(p).ok() == Some(*d));
        streak = if ok {
            if consecutive { streak + 1 } else { 1 }
        } else {
            0
        };
        best = best.max(streak);
        rows.push((*d, ok, if ok { "ok".into() } else { why.join(", ") }));
        prev = Some(*d);
    }
    ExitCheck {
        required: n,
        streak,
        passed: best >= n,
        days: rows,
    }
}

/// Is `now` past the daemon's stop time on its day?
pub fn past(now: Ts, date: NaiveDate, hhmm: &str) -> Result<bool> {
    let t = saarthi_core::clock::parse_hhmm(hhmm)?;
    Ok(now >= MarketClock::at(date, t))
}

pub fn ensure_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> MarketClock {
        let p = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/holidays.toml"));
        MarketClock::new(Default::default(), HolidayCalendar::load(p).unwrap())
    }

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn ok(date: &str) -> (NaiveDate, DaySummary) {
        (
            d(date),
            DaySummary {
                date: d(date),
                flat_after_square_off: true,
                ..DaySummary::default()
            },
        )
    }

    #[test]
    fn exit_check_needs_consecutive_clean_trading_days() {
        // Mon 26 Oct .. Fri 30 Oct 2026 are trading days.
        let mut days: Vec<_> = ["2026-10-26", "2026-10-27", "2026-10-28", "2026-10-29"]
            .iter()
            .map(|s| ok(s))
            .collect();
        assert!(!exit_check(&days, &clock(), 5).passed);
        days.push(ok("2026-10-30"));
        assert!(exit_check(&days, &clock(), 5).passed);
        // A restart breaks the streak.
        days[2].1.restarts = 1;
        let r = exit_check(&days, &clock(), 5);
        assert!(!r.passed);
        assert_eq!(r.streak, 2);
        // A missing trading day breaks it too.
        let gap: Vec<_> = ["2026-10-26", "2026-10-27", "2026-10-29", "2026-10-30", "2026-11-02"]
            .iter()
            .map(|s| ok(s))
            .collect();
        assert!(!exit_check(&gap, &clock(), 5).passed);
    }
}
