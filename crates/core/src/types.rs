use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::money::Price;

/// Instants are stored in UTC; convert to [`crate::IST`] only for display and
/// session logic.
pub type Ts = DateTime<Utc>;

/// A broker trading symbol, e.g. `RELIANCE-EQ`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Symbol(pub String);

impl Symbol {
    pub fn new(s: impl Into<String>) -> Self {
        Symbol(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A filesystem-safe form (e.g. `M&M-EQ` stays readable but path-safe).
    pub fn file_stem(&self) -> String {
        self.0
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '&') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Exchange {
    Nse,
    Bse,
    Nfo,
    Bfo,
    Mcx,
    Cds,
}

impl Exchange {
    pub fn as_str(self) -> &'static str {
        match self {
            Exchange::Nse => "NSE",
            Exchange::Bse => "BSE",
            Exchange::Nfo => "NFO",
            Exchange::Bfo => "BFO",
            Exchange::Mcx => "MCX",
            Exchange::Cds => "CDS",
        }
    }
}

impl FromStr for Exchange {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_uppercase().as_str() {
            "NSE" => Exchange::Nse,
            "BSE" => Exchange::Bse,
            "NFO" => Exchange::Nfo,
            "BFO" => Exchange::Bfo,
            "MCX" => Exchange::Mcx,
            "CDS" => Exchange::Cds,
            _ => return Err(CoreError::UnknownExchange(s.to_string())),
        })
    }
}

impl fmt::Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

/// Bar timeframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tf {
    M1,
    M3,
    M5,
    M10,
    M15,
    M30,
    H1,
    D1,
}

impl Tf {
    pub const ALL: [Tf; 8] = [Tf::M1, Tf::M3, Tf::M5, Tf::M10, Tf::M15, Tf::M30, Tf::H1, Tf::D1];

    pub fn as_str(self) -> &'static str {
        match self {
            Tf::M1 => "1m",
            Tf::M3 => "3m",
            Tf::M5 => "5m",
            Tf::M10 => "10m",
            Tf::M15 => "15m",
            Tf::M30 => "30m",
            Tf::H1 => "1h",
            Tf::D1 => "1d",
        }
    }

    pub fn minutes(self) -> u32 {
        match self {
            Tf::M1 => 1,
            Tf::M3 => 3,
            Tf::M5 => 5,
            Tf::M10 => 10,
            Tf::M15 => 15,
            Tf::M30 => 30,
            Tf::H1 => 60,
            Tf::D1 => 375,
        }
    }

    pub fn is_intraday(self) -> bool {
        self != Tf::D1
    }
}

impl FromStr for Tf {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Tf::ALL
            .into_iter()
            .find(|tf| tf.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| CoreError::UnknownTimeframe(s.to_string()))
    }
}

/// Serialized as its short name (`"5m"`), as in config files.
impl Serialize for Tf {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Tf {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Tf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the broker's instrument master.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    /// Broker token used by REST and WebSocket APIs.
    pub token: String,
    pub symbol: Symbol,
    pub name: String,
    pub exchange: Exchange,
    /// Empty for cash equity; `FUTSTK`, `OPTIDX`, ... for derivatives.
    pub instrument_type: String,
    pub lot_size: u32,
    pub tick_size: Price,
    pub expiry: Option<NaiveDate>,
    pub strike: Option<Price>,
}

/// An OHLCV bar. `ts` is the bar's **start** time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bar {
    pub ts: Ts,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub volume: u64,
}

impl Bar {
    /// Basic OHLC sanity: low ≤ open, close ≤ high and prices positive.
    pub fn is_consistent(&self) -> bool {
        self.low.0 > 0
            && self.low <= self.high
            && (self.low..=self.high).contains(&self.open)
            && (self.low..=self.high).contains(&self.close)
    }
}

/// One market-data update for an instrument, as the engine sees it.
///
/// Fields beyond the last price are `None` when the feed mode does not carry
/// them (SmartAPI's LTP mode has no volume, for example).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    /// Broker token, the key the feed and the instrument master share.
    pub token: String,
    pub exchange: Exchange,
    /// Exchange timestamp of the update.
    pub ts: Ts,
    pub ltp: Price,
    /// Quantity of the last trade.
    pub last_qty: Option<u64>,
    /// Cumulative traded volume for the day. Bar volumes are its differences.
    pub day_volume: Option<u64>,
    /// The exchange's average traded price for the day (its VWAP).
    pub avg_price: Option<Price>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tf_parse_round_trip() {
        for tf in Tf::ALL {
            assert_eq!(tf.as_str().parse::<Tf>().unwrap(), tf);
        }
        assert!("2m".parse::<Tf>().is_err());
        assert_eq!(serde_json::to_string(&Tf::M5).unwrap(), r#""5m""#);
        assert_eq!(serde_json::from_str::<Tf>(r#""1h""#).unwrap(), Tf::H1);
        assert!(serde_json::from_str::<Tf>(r#""7m""#).is_err());
    }

    #[test]
    fn symbol_file_stem_is_path_safe() {
        assert_eq!(Symbol::new("M&M-EQ").file_stem(), "M&M-EQ");
        assert_eq!(Symbol::new("A/B C").file_stem(), "A_B_C");
    }

    #[test]
    fn bar_consistency() {
        let b = Bar {
            ts: Utc::now(),
            open: Price(100),
            high: Price(110),
            low: Price(95),
            close: Price(105),
            volume: 10,
        };
        assert!(b.is_consistent());
        assert!(!Bar { high: Price(99), ..b }.is_consistent());
    }
}
