//! `getCandleData`: interval mapping, request chunking and row decoding.

use chrono::{DateTime, Duration, FixedOffset};
use reqwest::Method;
use saarthi_core::{Bar, IST, Instrument, Price, Tf, Ts};
use serde_json::Value;

use super::SmartApi;
use crate::error::{BrokerError, BrokerResult};

const CANDLE_PATH: &str = "/rest/secure/angelbroking/historical/v1/getCandleData";

/// SmartAPI interval name for a timeframe.
pub fn interval(tf: Tf) -> &'static str {
    match tf {
        Tf::M1 => "ONE_MINUTE",
        Tf::M3 => "THREE_MINUTE",
        Tf::M5 => "FIVE_MINUTE",
        Tf::M10 => "TEN_MINUTE",
        Tf::M15 => "FIFTEEN_MINUTE",
        Tf::M30 => "THIRTY_MINUTE",
        Tf::H1 => "ONE_HOUR",
        Tf::D1 => "ONE_DAY",
    }
}

/// Longest date range SmartAPI serves in one request for a timeframe.
pub fn max_days_per_request(tf: Tf) -> i64 {
    match tf {
        Tf::M1 => 30,
        Tf::M3 => 60,
        Tf::M5 | Tf::M10 => 100,
        Tf::M15 | Tf::M30 => 200,
        Tf::H1 => 400,
        Tf::D1 => 2000,
    }
}

/// Splits `[from, to]` into request-sized windows. Windows are one day short
/// of the documented limit to stay clear of off-by-one rejections, and are
/// contiguous at minute resolution (SmartAPI's date format has no seconds).
pub fn chunks(tf: Tf, from: Ts, to: Ts) -> Vec<(Ts, Ts)> {
    let span = Duration::days(max_days_per_request(tf) - 1);
    let mut out = Vec::new();
    let mut start = from;
    while start <= to {
        let end = (start + span).min(to);
        out.push((start, end));
        start = end + Duration::minutes(1);
    }
    out
}

pub fn format_ist(ts: Ts) -> String {
    ts.with_timezone(&IST).format("%Y-%m-%d %H:%M").to_string()
}

pub fn sort_dedup(bars: &mut Vec<Bar>) {
    bars.sort_by_key(|b| b.ts);
    bars.dedup_by_key(|b| b.ts);
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Decodes one `[ts, open, high, low, close, volume]` row.
pub fn parse_row(row: &Value) -> BrokerResult<Bar> {
    let bad = || BrokerError::Decode(format!("bad candle row: {row}"));
    let a = row.as_array().filter(|a| a.len() >= 6).ok_or_else(bad)?;
    let ts = a[0]
        .as_str()
        .and_then(|s| DateTime::<FixedOffset>::parse_from_rfc3339(s).ok())
        .ok_or_else(bad)?
        .to_utc();
    let px = |i: usize| num(&a[i]).and_then(Price::from_rupees_f64).ok_or_else(bad);
    let volume = num(&a[5]).filter(|v| *v >= 0.0).ok_or_else(bad)?.round() as u64;
    Ok(Bar {
        ts,
        open: px(1)?,
        high: px(2)?,
        low: px(3)?,
        close: px(4)?,
        volume,
    })
}

impl SmartApi {
    /// One `getCandleData` request. The caller keeps the range within
    /// [`max_days_per_request`]; use [`chunks`] for longer ranges.
    pub async fn candles_chunk(&self, inst: &Instrument, tf: Tf, from: Ts, to: Ts) -> BrokerResult<Vec<Bar>> {
        let body = serde_json::json!({
            "exchange": inst.exchange.as_str(),
            "symboltoken": inst.token,
            "interval": interval(tf),
            "fromdate": format_ist(from),
            "todate": format_ist(to),
        });
        let rows: Option<Vec<Value>> = self
            .secure(Method::POST, CANDLE_PATH, Some(&body), &self.hist_limiter)
            .await?;
        let mut bars = rows
            .unwrap_or_default()
            .iter()
            .map(parse_row)
            .collect::<BrokerResult<Vec<_>>>()?;
        sort_dedup(&mut bars);
        Ok(bars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use saarthi_core::MarketClock;

    fn ist(d: &str, hm: &str) -> Ts {
        MarketClock::at(
            d.parse::<NaiveDate>().unwrap(),
            saarthi_core::clock::parse_hhmm(hm).unwrap(),
        )
    }

    #[test]
    fn one_year_of_5m_is_four_requests() {
        let c = chunks(Tf::M5, ist("2025-10-27", "09:15"), ist("2026-10-27", "15:30"));
        assert_eq!(c.len(), 4);
        for (a, b) in &c {
            assert!(*b - *a < Duration::days(100));
        }
        for w in c.windows(2) {
            assert_eq!(w[1].0 - w[0].1, Duration::minutes(1), "windows must be contiguous");
        }
        assert_eq!(c.first().unwrap().0, ist("2025-10-27", "09:15"));
        assert_eq!(c.last().unwrap().1, ist("2026-10-27", "15:30"));
    }

    #[test]
    fn empty_and_single_ranges() {
        let t = ist("2026-10-27", "09:15");
        assert!(chunks(Tf::M5, t, t - Duration::minutes(1)).is_empty());
        assert_eq!(chunks(Tf::D1, t, t).len(), 1);
    }

    #[test]
    fn formats_in_ist() {
        assert_eq!(format_ist(ist("2026-10-27", "09:15")), "2026-10-27 09:15");
    }

    #[test]
    fn parses_rows() {
        let row = serde_json::json!(["2026-10-27T09:15:00+05:30", 2810.5, 2815.0, 2805.05, 2812.35, 12345]);
        let bar = parse_row(&row).unwrap();
        assert_eq!(bar.ts, ist("2026-10-27", "09:15"));
        assert_eq!(bar.open, Price(281050));
        assert_eq!(bar.low, Price(280505));
        assert_eq!(bar.volume, 12345);

        for bad in [
            serde_json::json!(["2026-10-27T09:15:00+05:30", 1, 2, 3]),
            serde_json::json!(["not a date", 1, 2, 3, 4, 5]),
            serde_json::json!(["2026-10-27T09:15:00+05:30", "x", 2, 3, 4, 5]),
            serde_json::json!(["2026-10-27T09:15:00+05:30", 1, 2, 3, 4, -5]),
        ] {
            assert!(parse_row(&bad).is_err(), "{bad}");
        }
    }
}
