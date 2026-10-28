//! `risk.toml`: the hard limits. Claude cannot edit this file and the plan is
//! clamped to it.

use std::path::Path;

use saarthi_core::{Price, SessionTimes};
use serde::{Deserialize, Serialize};

use crate::error::{RiskError, RiskResult};

/// Every hard limit from ARCHITECTURE §5.7. Money is rupees in the file and
/// paise everywhere else; percentages are of the day's capital base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskLimits {
    /// 0 = use the broker's net funds at the start of the day.
    pub capital_base_inr: f64,
    /// (entry − stop) × qty as % of capital.
    pub max_risk_per_trade_pct: f64,
    pub max_open_positions: usize,
    /// Gross exposure / capital, intraday leverage included.
    pub max_gross_exposure_x: f64,
    /// Hit → flatten everything and halt for the day.
    pub max_daily_loss_pct: f64,
    /// 0 = off. Once hit, no new entries; open trades run to their exits.
    #[serde(default)]
    pub daily_profit_lock_pct: f64,
    pub max_trades_per_day: u32,
    /// Hit → no new entries for `consecutive_loss_cooldown_min`.
    pub max_consecutive_losses: u32,
    #[serde(default = "default_cooldown")]
    pub consecutive_loss_cooldown_min: i64,
    pub min_price_inr: f64,
    pub no_new_entries_after: String,
    pub square_off_at: String,
    pub max_order_qty: u32,
    pub max_orders_per_second: u32,
    /// Reject an entry whose reference price is further than this from the
    /// last traded price (stale signal or fat finger).
    #[serde(default = "default_deviation")]
    pub max_entry_deviation_pct: f64,
    /// A repeat of the same symbol + strategy + side within this many
    /// seconds is a duplicate.
    #[serde(default = "default_duplicate_window")]
    pub duplicate_signal_window_secs: i64,
}

fn default_cooldown() -> i64 {
    30
}

fn default_deviation() -> f64 {
    1.0
}

fn default_duplicate_window() -> i64 {
    300
}

impl RiskLimits {
    pub fn load(path: &Path) -> RiskResult<Self> {
        let s = std::fs::read_to_string(path).map_err(|e| RiskError::Config(format!("{}: {e}", path.display())))?;
        Self::from_toml_str(&s).map_err(|e| RiskError::Config(format!("{}: {e}", path.display())))
    }

    pub fn from_toml_str(s: &str) -> RiskResult<Self> {
        let r: RiskLimits = toml::from_str(s).map_err(|e| RiskError::Config(e.to_string()))?;
        r.validate()?;
        Ok(r)
    }

    /// Rejects limits that are nonsensical or would switch a protection off
    /// by accident. A limit can be strict; it cannot be absent.
    pub fn validate(&self) -> RiskResult<()> {
        let bad = |m: String| Err(RiskError::Config(m));
        let pcts = [
            ("max_risk_per_trade_pct", self.max_risk_per_trade_pct, 0.0, 5.0),
            ("max_daily_loss_pct", self.max_daily_loss_pct, 0.0, 20.0),
            ("max_gross_exposure_x", self.max_gross_exposure_x, 0.0, 10.0),
            ("max_entry_deviation_pct", self.max_entry_deviation_pct, 0.0, 20.0),
        ];
        for (k, v, lo, hi) in pcts {
            if !v.is_finite() || v <= lo || v > hi {
                return bad(format!("{k} must be in ({lo}, {hi}], got {v}"));
            }
        }
        if !self.daily_profit_lock_pct.is_finite() || self.daily_profit_lock_pct < 0.0 {
            return bad(format!(
                "daily_profit_lock_pct must be >= 0 (0 = off), got {}",
                self.daily_profit_lock_pct
            ));
        }
        if !self.capital_base_inr.is_finite() || self.capital_base_inr < 0.0 {
            return bad("capital_base_inr must be >= 0 (0 = read from funds)".into());
        }
        if !self.min_price_inr.is_finite() || self.min_price_inr < 0.0 {
            return bad("min_price_inr must be >= 0".into());
        }
        for (k, v) in [
            ("max_open_positions", self.max_open_positions as u64),
            ("max_trades_per_day", u64::from(self.max_trades_per_day)),
            ("max_order_qty", u64::from(self.max_order_qty)),
            ("max_orders_per_second", u64::from(self.max_orders_per_second)),
        ] {
            if v == 0 {
                return bad(format!("{k} must be at least 1"));
            }
        }
        if self.consecutive_loss_cooldown_min < 0 || self.duplicate_signal_window_secs < 0 {
            return bad("cooldown and duplicate window must be >= 0".into());
        }
        self.session()?;
        Ok(())
    }

    /// Session times with this file's cut-offs.
    pub fn session(&self) -> RiskResult<SessionTimes> {
        SessionTimes::default()
            .with_cutoffs(&self.no_new_entries_after, &self.square_off_at)
            .map_err(|e| RiskError::Config(e.to_string()))
    }

    pub fn min_price(&self) -> Price {
        inr(self.min_price_inr)
    }

    /// The configured capital base, if fixed (not read from funds).
    pub fn fixed_capital(&self) -> Option<Price> {
        (self.capital_base_inr > 0.0).then(|| inr(self.capital_base_inr))
    }

    /// The same limits with every loss-related allowance scaled by `f`
    /// (0 < f ≤ 1), e.g. 0.5 for the first live days. Counts round down but
    /// stay at least 1.
    pub fn scaled(&self, f: f64) -> RiskResult<Self> {
        if !(f > 0.0 && f <= 1.0) {
            return Err(RiskError::Config(format!("scale must be in (0, 1], got {f}")));
        }
        let n = |v: u32| ((f64::from(v) * f).floor() as u32).max(1);
        let r = RiskLimits {
            max_risk_per_trade_pct: self.max_risk_per_trade_pct * f,
            max_open_positions: ((self.max_open_positions as f64 * f).floor() as usize).max(1),
            max_gross_exposure_x: self.max_gross_exposure_x * f,
            max_daily_loss_pct: self.max_daily_loss_pct * f,
            max_trades_per_day: n(self.max_trades_per_day),
            max_order_qty: n(self.max_order_qty),
            ..self.clone()
        };
        r.validate()?;
        Ok(r)
    }
}

/// Rupees from config to paise.
pub fn inr(v: f64) -> Price {
    Price((v * 100.0).round() as i64)
}

/// `pct` % of `of`, rounded down to the paisa.
pub fn pct_of(of: Price, pct: f64) -> Price {
    Price((of.0 as f64 * pct / 100.0).floor() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn shipped() -> RiskLimits {
        let p = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/risk.toml"));
        RiskLimits::load(p).unwrap()
    }

    #[test]
    fn shipped_risk_toml_parses_and_validates() {
        let r = shipped();
        assert_eq!(r.max_open_positions, 3);
        assert_eq!(r.max_orders_per_second, 5);
        assert!(r.fixed_capital().is_none());
        assert_eq!(r.min_price(), Price(5000));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let mut s = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/risk.toml")).unwrap();
        s.push_str("\nmax_dailyloss_pct = 50\n");
        assert!(
            RiskLimits::from_toml_str(&s).is_err(),
            "a typo must not silently drop a limit"
        );
    }

    #[test]
    fn nonsense_limits_are_rejected() {
        let base = shipped();
        for r in [
            RiskLimits {
                max_daily_loss_pct: 0.0,
                ..base.clone()
            },
            RiskLimits {
                max_risk_per_trade_pct: 50.0,
                ..base.clone()
            },
            RiskLimits {
                max_open_positions: 0,
                ..base.clone()
            },
            RiskLimits {
                max_orders_per_second: 0,
                ..base.clone()
            },
            RiskLimits {
                daily_profit_lock_pct: f64::NAN,
                ..base.clone()
            },
            RiskLimits {
                square_off_at: "16:00".into(),
                ..base.clone()
            },
        ] {
            assert!(r.validate().is_err(), "{r:?}");
        }
    }

    #[test]
    fn halving_for_first_live_days() {
        let h = shipped().scaled(0.5).unwrap();
        assert!((h.max_daily_loss_pct - 1.0).abs() < 1e-9);
        assert!((h.max_risk_per_trade_pct - 0.25).abs() < 1e-9);
        assert_eq!(h.max_open_positions, 1);
        assert_eq!(h.max_trades_per_day, 7);
        assert!(shipped().scaled(1.5).is_err());
    }
}
