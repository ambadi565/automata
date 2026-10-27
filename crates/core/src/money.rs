use std::fmt;
use std::iter::Sum;
use std::ops::{Add, Mul, Neg, Sub};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// A price or an amount of money in integer **paise** (1 INR = 100 paise).
///
/// Signed so that P&L and differences use the same type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Price(pub i64);

/// Order quantity in shares.
pub type Qty = u32;

impl Price {
    pub const ZERO: Price = Price(0);

    pub const fn from_paise(paise: i64) -> Self {
        Price(paise)
    }

    pub const fn paise(self) -> i64 {
        self.0
    }

    /// Converts a rupee amount from a broker payload, rounding to the nearest
    /// paisa. This is the only place floats are allowed to touch money.
    ///
    /// Returns `None` for NaN, infinities and values outside the `i64` range.
    pub fn from_rupees_f64(rupees: f64) -> Option<Self> {
        if !rupees.is_finite() {
            return None;
        }
        let paise = (rupees * 100.0).round();
        if paise.abs() >= i64::MAX as f64 {
            return None;
        }
        Some(Price(paise as i64))
    }

    /// Rupees as `f64`, for display and charting only.
    pub fn as_rupees_f64(self) -> f64 {
        self.0 as f64 / 100.0
    }

    /// Rounds to the nearest multiple of `tick` (in paise). Ties round away
    /// from zero. A non-positive tick returns the price unchanged.
    pub fn round_to_tick(self, tick: i64) -> Self {
        if tick <= 0 {
            return self;
        }
        let rem = self.0.rem_euclid(tick);
        let down = self.0 - rem;
        if rem * 2 >= tick {
            Price(down + tick)
        } else {
            Price(down)
        }
    }

    pub fn checked_mul_qty(self, qty: Qty) -> Option<Self> {
        self.0.checked_mul(i64::from(qty)).map(Price)
    }
}

impl Add for Price {
    type Output = Price;
    fn add(self, rhs: Price) -> Price {
        Price(self.0 + rhs.0)
    }
}

impl Sub for Price {
    type Output = Price;
    fn sub(self, rhs: Price) -> Price {
        Price(self.0 - rhs.0)
    }
}

impl Neg for Price {
    type Output = Price;
    fn neg(self) -> Price {
        Price(-self.0)
    }
}

impl Mul<Qty> for Price {
    type Output = Price;
    fn mul(self, qty: Qty) -> Price {
        Price(self.0 * i64::from(qty))
    }
}

impl Sum for Price {
    fn sum<I: Iterator<Item = Price>>(iter: I) -> Price {
        Price(iter.map(|p| p.0).sum())
    }
}

impl fmt::Display for Price {
    /// Formats as rupees with exactly two decimals, e.g. `2810.50`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        write!(f, "{sign}{}.{:02}", abs / 100, abs % 100)
    }
}

impl FromStr for Price {
    type Err = CoreError;

    /// Parses a decimal rupee string exactly (no float round-trip), e.g.
    /// `"2810.5"`, `"-0.05"`, `"100"`. More than two decimals is an error.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || CoreError::InvalidPrice(s.to_string());
        let t = s.trim();
        let (neg, t) = match t.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, t),
        };
        let (int, frac) = t.split_once('.').unwrap_or((t, ""));
        if int.is_empty() && frac.is_empty() {
            return Err(err());
        }
        if frac.len() > 2 || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
            return Err(err());
        }
        let int: i64 = if int.is_empty() {
            0
        } else {
            int.parse().map_err(|_| err())?
        };
        let frac: i64 = match frac.len() {
            0 => 0,
            1 => frac.parse::<i64>().map_err(|_| err())? * 10,
            _ => frac.parse().map_err(|_| err())?,
        };
        let paise = int.checked_mul(100).and_then(|v| v.checked_add(frac)).ok_or_else(err)?;
        Ok(Price(if neg { -paise } else { paise }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_boundary_rounds_to_nearest_paisa() {
        assert_eq!(Price::from_rupees_f64(2810.5), Some(Price(281050)));
        // 0.1 + 0.2 style artefacts must not leak through.
        assert_eq!(Price::from_rupees_f64(1234.5699999), Some(Price(123457)));
        assert_eq!(Price::from_rupees_f64(-0.05), Some(Price(-5)));
        assert_eq!(Price::from_rupees_f64(f64::NAN), None);
        assert_eq!(Price::from_rupees_f64(f64::INFINITY), None);
    }

    #[test]
    fn parse_and_display_round_trip() {
        for (s, p) in [
            ("2810.5", 281050),
            ("100", 10000),
            ("-0.05", -5),
            (".5", 50),
            ("0.01", 1),
        ] {
            let parsed: Price = s.parse().unwrap();
            assert_eq!(parsed, Price(p), "{s}");
        }
        assert_eq!(Price(281050).to_string(), "2810.50");
        assert_eq!(Price(-5).to_string(), "-0.05");
        assert_eq!(Price(0).to_string(), "0.00");
    }

    #[test]
    fn parse_rejects_garbage() {
        for s in ["", "-", ".", "1.234", "abc", "1e5", "1.-2"] {
            assert!(s.parse::<Price>().is_err(), "{s:?} should fail");
        }
    }

    #[test]
    fn tick_rounding() {
        assert_eq!(Price(281052).round_to_tick(5), Price(281050));
        assert_eq!(Price(281053).round_to_tick(5), Price(281055));
        assert_eq!(Price(-3).round_to_tick(5), Price(-5));
        assert_eq!(Price(7).round_to_tick(0), Price(7));
    }

    #[test]
    fn arithmetic() {
        assert_eq!(Price(150) * 3, Price(450));
        assert_eq!(Price(100) - Price(250), Price(-150));
        assert_eq!([Price(1), Price(2)].into_iter().sum::<Price>(), Price(3));
        assert_eq!(Price(i64::MAX).checked_mul_qty(2), None);
    }
}
