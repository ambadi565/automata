//! Indian intraday equity (NSE cash, MIS) charges, per executed order.
//!
//! Rates come from `config/costs.toml`. Statutory rates change with budgets
//! and exchange circulars, and broker tariffs change at will, so the file
//! records an as-of date; check it against a recent contract note.
//!
//! Every component is computed on the order's turnover and rounded to the
//! paisa. Contract notes round some items (STT, stamp duty) to the rupee per
//! note, so a day's real total can differ by a few rupees.

use std::path::Path;

use saarthi_core::{Price, Qty, Side};
use serde::{Deserialize, Serialize};

use crate::error::{BrokerError, BrokerResult};

/// Charge rates. `*_pct` fields are percentages of turnover (`0.025` means
/// 0.025 %); `*_per_crore_inr` are rupees per ₹1 crore of turnover.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostModel {
    pub as_of: String,
    pub brokerage: Brokerage,
    pub stt_buy_pct: f64,
    pub stt_sell_pct: f64,
    pub exchange_txn_pct: f64,
    pub sebi_per_crore_inr: f64,
    #[serde(default)]
    pub ipft_per_crore_inr: f64,
    pub stamp_buy_pct: f64,
    pub gst_pct: f64,
}

/// Broker commission per executed order: `min(flat, pct × turnover)`, but at
/// least `min_inr`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Brokerage {
    pub flat_inr: f64,
    pub pct: f64,
    #[serde(default)]
    pub min_inr: f64,
}

/// The charges on one executed order, in paise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Charges {
    pub brokerage: Price,
    pub stt: Price,
    pub exchange: Price,
    pub sebi: Price,
    pub ipft: Price,
    pub stamp: Price,
    pub gst: Price,
}

impl Charges {
    pub fn total(&self) -> Price {
        self.brokerage + self.stt + self.exchange + self.sebi + self.ipft + self.stamp + self.gst
    }
}

impl std::ops::AddAssign for Charges {
    fn add_assign(&mut self, o: Charges) {
        self.brokerage = self.brokerage + o.brokerage;
        self.stt = self.stt + o.stt;
        self.exchange = self.exchange + o.exchange;
        self.sebi = self.sebi + o.sebi;
        self.ipft = self.ipft + o.ipft;
        self.stamp = self.stamp + o.stamp;
        self.gst = self.gst + o.gst;
    }
}

fn pct_of(turnover: f64, pct: f64) -> f64 {
    turnover * pct / 100.0
}

fn per_crore(turnover: f64, inr: f64) -> f64 {
    // ₹1 crore = 1e7 rupees = 1e9 paise; `inr` rupees = inr * 100 paise.
    turnover * inr * 100.0 / 1e9
}

fn paise(v: f64) -> Price {
    Price(v.round() as i64)
}

/// What `config/costs.toml` holds: the charge rates plus `[slippage]`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionCosts {
    pub charges: CostModel,
    pub slippage: Slippage,
}

impl ExecutionCosts {
    pub fn load(path: &Path) -> BrokerResult<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| BrokerError::Config(format!("reading {}: {e}", path.display())))?;
        Self::from_toml_str(&s).map_err(|e| BrokerError::Config(format!("{}: {e}", path.display())))
    }

    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let mut t: toml::Table = toml::from_str(s).map_err(|e| e.to_string())?;
        let slippage = match t.remove("slippage") {
            Some(v) => v.try_into().map_err(|e: toml::de::Error| format!("[slippage]: {e}"))?,
            None => Slippage::default(),
        };
        let charges: CostModel = t.try_into().map_err(|e: toml::de::Error| e.to_string())?;
        charges.validate()?;
        Ok(ExecutionCosts { charges, slippage })
    }
}

impl CostModel {
    fn validate(&self) -> Result<(), String> {
        let rates = [
            ("brokerage.flat_inr", self.brokerage.flat_inr),
            ("brokerage.pct", self.brokerage.pct),
            ("brokerage.min_inr", self.brokerage.min_inr),
            ("stt_buy_pct", self.stt_buy_pct),
            ("stt_sell_pct", self.stt_sell_pct),
            ("exchange_txn_pct", self.exchange_txn_pct),
            ("sebi_per_crore_inr", self.sebi_per_crore_inr),
            ("ipft_per_crore_inr", self.ipft_per_crore_inr),
            ("stamp_buy_pct", self.stamp_buy_pct),
            ("gst_pct", self.gst_pct),
        ];
        for (k, v) in rates {
            if !v.is_finite() || v < 0.0 {
                return Err(format!("{k} must be a non-negative number, got {v}"));
            }
        }
        // Guard against "0.025 meant as a fraction" style unit mistakes.
        if self.stt_sell_pct > 1.0 || self.exchange_txn_pct > 0.1 || self.stamp_buy_pct > 0.1 {
            return Err("a *_pct rate is implausibly large; rates are percentages (0.025 = 0.025 %)".into());
        }
        Ok(())
    }

    /// Charges for one executed order of `qty` at `price`.
    pub fn charges(&self, side: Side, price: Price, qty: Qty) -> Charges {
        let turnover = price.0 as f64 * f64::from(qty);
        if turnover <= 0.0 {
            return Charges::default();
        }
        let b = &self.brokerage;
        let brokerage = b.flat_inr * 100.0;
        let brokerage = if b.pct > 0.0 {
            brokerage.min(pct_of(turnover, b.pct))
        } else {
            brokerage
        };
        let brokerage = brokerage.max(b.min_inr * 100.0);
        let (stt_pct, stamp_pct) = match side {
            Side::Buy => (self.stt_buy_pct, self.stamp_buy_pct),
            Side::Sell => (self.stt_sell_pct, 0.0),
        };
        let mut c = Charges {
            brokerage: paise(brokerage),
            stt: paise(pct_of(turnover, stt_pct)),
            exchange: paise(pct_of(turnover, self.exchange_txn_pct)),
            sebi: paise(per_crore(turnover, self.sebi_per_crore_inr)),
            ipft: paise(per_crore(turnover, self.ipft_per_crore_inr)),
            stamp: paise(pct_of(turnover, stamp_pct)),
            gst: Price::ZERO,
        };
        // GST is levied on the broker's and exchange's services, not on taxes.
        let taxable = c.brokerage + c.exchange + c.sebi + c.ipft;
        c.gst = paise(taxable.0 as f64 * self.gst_pct / 100.0);
        c
    }

    /// Total charges for a round trip (buy then sell, or the reverse).
    pub fn round_trip(&self, entry: Price, exit: Price, qty: Qty) -> Price {
        self.charges(Side::Buy, entry, qty).total() + self.charges(Side::Sell, exit, qty).total()
    }
}

/// Adverse execution cost on market and stop fills, on top of charges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Slippage {
    /// Basis points of price.
    pub bps: f64,
    /// Plus this many ticks.
    pub ticks: i64,
    /// NSE equity tick size in rupees (0.05 for most stocks; 0.01 for some
    /// low-priced ones since 2025).
    pub tick_size_inr: f64,
    /// A resting limit order fills only once price trades this many ticks
    /// through it (0 = a touch fills). Touch fills flatter backtests because
    /// the queue ahead of you is unknown.
    pub limit_through_ticks: i64,
}

impl Default for Slippage {
    fn default() -> Self {
        Slippage {
            bps: 2.0,
            ticks: 1,
            tick_size_inr: 0.05,
            limit_through_ticks: 1,
        }
    }
}

impl Slippage {
    pub fn tick(&self) -> i64 {
        ((self.tick_size_inr * 100.0).round() as i64).max(1)
    }

    /// The price actually paid (buy) or received (sell) when executing
    /// aggressively around `base`, rounded to the tick against us.
    pub fn adverse(&self, side: Side, base: Price) -> Price {
        let tick = self.tick();
        let slip = (base.0 as f64 * self.bps / 1e4).round() as i64 + self.ticks * tick;
        match side {
            Side::Buy => {
                let p = base.0 + slip;
                Price(p.div_euclid(tick) * tick + if p.rem_euclid(tick) > 0 { tick } else { 0 })
            }
            Side::Sell => {
                let p = (base.0 - slip).max(tick);
                Price(p.div_euclid(tick) * tick)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> CostModel {
        let p = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/costs.toml"));
        ExecutionCosts::load(p).unwrap().charges
    }

    #[test]
    fn shipped_costs_toml_parses() {
        let m = model();
        assert!(
            m.stt_sell_pct > 0.0 && m.stt_buy_pct.abs() < 1e-12,
            "intraday STT is sell-side only"
        );
        assert!(m.stamp_buy_pct > 0.0, "stamp duty is buy-side");
    }

    /// Worked example: 100 shares at ₹1,000 (₹1 lakh turnover each side).
    #[test]
    fn round_trip_on_one_lakh() {
        let m = model();
        let buy = m.charges(Side::Buy, Price(100_000), 100);
        // 0.1 % of ₹1,00,000 = ₹100, capped at ₹20.
        assert_eq!(buy.brokerage, Price(2_000));
        assert_eq!(buy.stt, Price(0));
        // 0.00297 % of ₹1,00,000 = ₹2.97
        assert_eq!(buy.exchange, Price(297));
        // ₹10 / crore on ₹1 lakh = ₹0.10
        assert_eq!(buy.sebi, Price(10));
        // 0.003 % = ₹3
        assert_eq!(buy.stamp, Price(300));
        // 18 % of (20 + 2.97 + 0.10 + ipft)
        let taxable = buy.brokerage + buy.exchange + buy.sebi + buy.ipft;
        assert_eq!(buy.gst, Price((taxable.0 as f64 * 0.18).round() as i64));

        let sell = m.charges(Side::Sell, Price(100_000), 100);
        // 0.025 % = ₹25
        assert_eq!(sell.stt, Price(2_500));
        assert_eq!(sell.stamp, Price(0));
        let rt = buy.total() + sell.total();
        // Roughly ₹85 for a ₹1 lakh round trip: ~0.085 % of one side.
        assert!((8_000..9_000).contains(&rt.0), "round trip {rt}");
    }

    #[test]
    fn small_orders_pay_percentage_brokerage_with_a_floor() {
        let m = model();
        // ₹2,000 turnover: 0.1 % = ₹2, floored at the minimum.
        let c = m.charges(Side::Buy, Price(20_000), 10);
        assert_eq!(c.brokerage, Price((m.brokerage.min_inr * 100.0).max(200.0) as i64));
        // ₹10,000: 0.1 % = ₹10, below the ₹20 cap.
        let c = m.charges(Side::Buy, Price(100_000), 10);
        assert_eq!(c.brokerage, Price(1_000));
    }

    #[test]
    fn rejects_fraction_units() {
        let s = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/costs.toml")).unwrap();
        let bad = s.replace("stt_sell_pct       = 0.025", "stt_sell_pct       = 2.5");
        assert_ne!(s, bad, "test must edit the line");
        assert!(ExecutionCosts::from_toml_str(&bad).is_err());
    }

    #[test]
    fn slippage_is_adverse_and_on_tick() {
        let s = Slippage {
            bps: 2.0,
            ticks: 1,
            tick_size_inr: 0.05,
            limit_through_ticks: 0,
        };
        // ₹1,000: 2 bps = 20 paise, + 1 tick = 25 paise.
        assert_eq!(s.adverse(Side::Buy, Price(100_000)), Price(100_025));
        assert_eq!(s.adverse(Side::Sell, Price(100_000)), Price(99_975));
        // Off-tick results round against us.
        assert_eq!(s.adverse(Side::Buy, Price(100_001)), Price(100_030));
        assert_eq!(s.adverse(Side::Sell, Price(100_001)), Price(99_975));
    }
}
