//! The strategist's plan (ARCHITECTURE §5.6, AGENT.md).
//!
//! Claude writes a [`Plan`]; the engine never trusts it. [`validate`] turns
//! it into an [`EffectivePlan`] the engine can apply:
//!
//! * **Rejected** outright: JSON that does not parse or has unknown fields,
//!   a non-finite number, an expired `valid_until`, a version that is not
//!   newer than the current plan.
//! * **Clamped**, each change listed in the result: the risk multiplier to
//!   [0, 1], per-symbol risk to the hard per-trade limit, unknown or
//!   disabled symbols and strategies dropped, duplicates merged, oversized
//!   lists and texts truncated, `valid_until` capped at today's square-off.
//!
//! Nothing in a plan can raise risk above `risk.toml`: the plan only scales
//! the guard's sizing down (`risk_scale` ∈ [0, 1]) and narrows what may
//! trade.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use chrono::{DateTime, FixedOffset, NaiveDate};
use saarthi_core::{MarketClock, Side, Symbol, Ts};
use saarthi_risk::RiskLimits;
use saarthi_strategy::Direction;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_WATCHLIST: usize = 20;
pub const MAX_TEXT: usize = 2_000;
pub const MAX_LEVELS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Bias {
    Bullish,
    Bearish,
    #[default]
    Neutral,
    /// Sit the day out: no new entries.
    Avoid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Regime {
    Trend,
    #[default]
    Range,
    Volatile,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyLevels {
    #[serde(default)]
    pub support: Vec<f64>,
    #[serde(default)]
    pub resistance: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchItem {
    pub symbol: Symbol,
    pub strategies: Vec<String>,
    #[serde(default)]
    pub direction: Direction,
    /// % of capital at risk per trade on this symbol (capped at the hard
    /// limit).
    pub max_risk_pct: f64,
    #[serde(default)]
    pub key_levels: KeyLevels,
    #[serde(default)]
    pub notes: String,
}

/// The plan as written by the strategist (`plan.schema.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u64,
    /// Required for submitted plans; the default plan has none (it is valid
    /// for whatever day it is loaded on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<DateTime<FixedOffset>>,
    pub market_bias: Bias,
    pub regime: Regime,
    pub global_risk_multiplier: f64,
    #[serde(default)]
    pub watchlist: Vec<WatchItem>,
    #[serde(default)]
    pub paused_strategies: Vec<String>,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Debug, Error, PartialEq)]
pub enum PlanError {
    #[error("plan does not parse: {0}")]
    Parse(String),
    #[error("plan is invalid: {0}")]
    Invalid(String),
}

/// What the engine needs to know about the world to validate a plan.
pub struct Context<'a> {
    pub now: Ts,
    pub limits: &'a RiskLimits,
    pub clock: &'a MarketClock,
    /// Symbols the engine trades.
    pub symbols: &'a BTreeSet<Symbol>,
    /// Strategies enabled in `engine.toml`.
    pub strategies: &'a BTreeSet<String>,
    /// Version of the plan in force (0 if only the default).
    pub current_version: u64,
}

/// A validated, clamped plan, ready to apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectivePlan {
    pub plan: Plan,
    pub valid_until: Ts,
    /// Every adjustment made to what was submitted.
    pub changes: Vec<String>,
    /// True for the conservative fallback.
    pub is_default: bool,
}

/// What a plan says about one entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Allowed, with this multiplier on the guard's risk budget (0–1].
    Allow {
        risk_scale: f64,
    },
    Deny(&'static str),
}

pub fn parse(json: &str) -> Result<Plan, PlanError> {
    serde_json::from_str(json).map_err(|e| PlanError::Parse(e.to_string()))
}

pub fn load(path: &Path) -> Result<Plan, PlanError> {
    let s = std::fs::read_to_string(path).map_err(|e| PlanError::Parse(format!("{}: {e}", path.display())))?;
    parse(&s)
}

fn truncate(s: &mut String, what: &str, changes: &mut Vec<String>) {
    if s.chars().count() > MAX_TEXT {
        *s = s.chars().take(MAX_TEXT).collect();
        changes.push(format!("{what} truncated to {MAX_TEXT} characters"));
    }
}

/// Validates and clamps `plan`. `default` marks the fallback plan (no
/// version or expiry requirements).
pub fn validate(mut plan: Plan, ctx: &Context<'_>, default: bool) -> Result<EffectivePlan, PlanError> {
    let mut changes = Vec::new();
    let today = MarketClock::ist_date(ctx.now);
    let square_off = MarketClock::at(today, ctx.clock.times.square_off);

    // Hard rejections.
    if !plan.global_risk_multiplier.is_finite() {
        return Err(PlanError::Invalid("global_risk_multiplier is not a number".into()));
    }
    if !default && plan.version <= ctx.current_version {
        return Err(PlanError::Invalid(format!(
            "version {} is not newer than the plan in force ({})",
            plan.version, ctx.current_version
        )));
    }
    let valid_until = match (plan.valid_until, default) {
        (Some(v), _) => {
            let v = v.to_utc();
            if v <= ctx.now {
                return Err(PlanError::Invalid(format!("valid_until {v} has already passed")));
            }
            if v > square_off {
                changes.push(format!("valid_until capped at today's square-off ({square_off})"));
                square_off
            } else {
                v
            }
        }
        (None, true) => square_off,
        (None, false) => return Err(PlanError::Invalid("valid_until is required".into())),
    };
    for item in &plan.watchlist {
        if !item.max_risk_pct.is_finite()
            || item
                .key_levels
                .support
                .iter()
                .chain(&item.key_levels.resistance)
                .any(|v| !v.is_finite())
        {
            return Err(PlanError::Invalid(format!("{}: a number is not finite", item.symbol)));
        }
    }

    // Clamping.
    let m = plan.global_risk_multiplier;
    if !(0.0..=1.0).contains(&m) {
        let c = m.clamp(0.0, 1.0);
        changes.push(format!("global_risk_multiplier {m} clamped to {c}"));
        plan.global_risk_multiplier = c;
    }
    let limit = ctx.limits.max_risk_per_trade_pct;
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for mut item in std::mem::take(&mut plan.watchlist) {
        if !ctx.symbols.contains(&item.symbol) {
            changes.push(format!("{}: not in the tradable universe, dropped", item.symbol));
            continue;
        }
        if !seen.insert(item.symbol.clone()) {
            changes.push(format!("{}: listed twice, kept the first", item.symbol));
            continue;
        }
        let before = item.strategies.len();
        let mut uniq = BTreeSet::new();
        item.strategies
            .retain(|s| ctx.strategies.contains(s) && uniq.insert(s.clone()));
        if item.strategies.len() != before {
            changes.push(format!(
                "{}: unknown, disabled or repeated strategies dropped",
                item.symbol
            ));
        }
        if item.strategies.is_empty() {
            changes.push(format!("{}: no usable strategy, dropped", item.symbol));
            continue;
        }
        if item.max_risk_pct > limit || item.max_risk_pct < 0.0 {
            let c = item.max_risk_pct.clamp(0.0, limit);
            changes.push(format!(
                "{}: max_risk_pct {} clamped to {c}",
                item.symbol, item.max_risk_pct
            ));
            item.max_risk_pct = c;
        }
        for (name, v) in [
            ("support", &mut item.key_levels.support),
            ("resistance", &mut item.key_levels.resistance),
        ] {
            if v.len() > MAX_LEVELS {
                v.truncate(MAX_LEVELS);
                changes.push(format!("{}: {name} levels truncated to {MAX_LEVELS}", item.symbol));
            }
        }
        truncate(&mut item.notes, &format!("{} notes", item.symbol), &mut changes);
        kept.push(item);
    }
    if kept.len() > MAX_WATCHLIST {
        changes.push(format!(
            "watchlist truncated from {} to {MAX_WATCHLIST} symbols",
            kept.len()
        ));
        kept.truncate(MAX_WATCHLIST);
    }
    plan.watchlist = kept;
    let before = plan.paused_strategies.len();
    let mut uniq = BTreeSet::new();
    plan.paused_strategies
        .retain(|s| ctx.strategies.contains(s) && uniq.insert(s.clone()));
    if plan.paused_strategies.len() != before {
        changes.push("unknown or repeated paused strategies dropped".into());
    }
    truncate(&mut plan.rationale, "rationale", &mut changes);
    plan.valid_until = Some(valid_until.with_timezone(&saarthi_core::IST));
    Ok(EffectivePlan {
        plan,
        valid_until,
        changes,
        is_default: default,
    })
}

impl EffectivePlan {
    pub fn version(&self) -> u64 {
        self.plan.version
    }

    pub fn expired(&self, now: Ts) -> bool {
        now >= self.valid_until
    }

    pub fn day(&self) -> NaiveDate {
        MarketClock::ist_date(self.valid_until)
    }

    /// No new entries at all under this plan.
    pub fn sits_out(&self) -> bool {
        self.plan.market_bias == Bias::Avoid || self.plan.global_risk_multiplier <= 0.0
    }

    /// Whether `strategy` may open a `side` trade on `symbol`, and at what
    /// fraction of the guard's risk budget.
    pub fn verdict(&self, symbol: &Symbol, strategy: &str, side: Side, limits: &RiskLimits) -> Verdict {
        if self.plan.market_bias == Bias::Avoid {
            return Verdict::Deny("plan_avoid");
        }
        if self.plan.paused_strategies.iter().any(|s| s == strategy) {
            return Verdict::Deny("plan_paused");
        }
        let m = self.plan.global_risk_multiplier;
        if m <= 0.0 {
            return Verdict::Deny("plan_zero_risk");
        }
        if self.plan.watchlist.is_empty() {
            return Verdict::Allow { risk_scale: m };
        }
        let Some(item) = self.plan.watchlist.iter().find(|i| &i.symbol == symbol) else {
            return Verdict::Deny("plan_not_on_watchlist");
        };
        if !item.strategies.iter().any(|s| s == strategy) {
            return Verdict::Deny("plan_strategy_not_assigned");
        }
        if !item.direction.allows(side) {
            return Verdict::Deny("plan_direction");
        }
        let per = (item.max_risk_pct / limits.max_risk_per_trade_pct).clamp(0.0, 1.0);
        let scale = m * per;
        if scale <= 0.0 {
            return Verdict::Deny("plan_zero_risk");
        }
        Verdict::Allow { risk_scale: scale }
    }

    /// Symbols on the watchlist (empty = no restriction).
    pub fn watch_symbols(&self) -> BTreeMap<Symbol, &WatchItem> {
        self.plan.watchlist.iter().map(|i| (i.symbol.clone(), i)).collect()
    }
}

#[cfg(test)]
mod tests;
