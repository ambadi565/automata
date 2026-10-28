//! SmartAPI's public instrument master ("scrip master") JSON.

use chrono::NaiveDate;
use saarthi_core::{Exchange, Instrument, Price, Symbol};
use serde::Deserialize;

use super::{SmartApi, SmartApiConfig};
use crate::error::{BrokerError, BrokerResult};

/// Raw row. Every field is a string in the upstream file.
#[derive(Debug, Deserialize)]
pub struct RawInstrument {
    pub token: String,
    pub symbol: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub expiry: String,
    #[serde(default)]
    pub strike: String,
    #[serde(default)]
    pub lotsize: String,
    #[serde(default)]
    pub instrumenttype: String,
    pub exch_seg: String,
    /// In paise, e.g. "5.000000" = ₹0.05.
    #[serde(default)]
    pub tick_size: String,
}

impl RawInstrument {
    /// `None` for segments we don't model (e.g. NCDEX) or unparseable rows.
    pub fn into_instrument(self) -> Option<Instrument> {
        let exchange: Exchange = self.exch_seg.parse().ok()?;
        let paise = |s: &str| s.trim().parse::<f64>().ok().map(|v| Price(v.round() as i64));
        let expiry = match self.expiry.trim() {
            "" => None,
            e => Some(NaiveDate::parse_from_str(&title_case_month(e), "%d%b%Y").ok()?),
        };
        Some(Instrument {
            token: self.token,
            symbol: Symbol(self.symbol),
            name: self.name,
            exchange,
            instrument_type: self.instrumenttype,
            lot_size: self.lotsize.trim().parse().unwrap_or(1),
            tick_size: paise(&self.tick_size).filter(|p| p.0 > 0).unwrap_or(Price(5)),
            expiry,
            strike: paise(&self.strike).filter(|p| p.0 > 0),
        })
    }
}

/// "26DEC2024" → "26Dec2024" for chrono's `%b`.
fn title_case_month(s: &str) -> String {
    s.char_indices()
        .map(|(i, c)| if (3..5).contains(&i) { c.to_ascii_lowercase() } else { c })
        .collect()
}

/// Decodes the full master, skipping rows we can't model.
pub fn parse_master(bytes: &[u8]) -> BrokerResult<Vec<Instrument>> {
    let raw: Vec<RawInstrument> =
        serde_json::from_slice(bytes).map_err(|e| BrokerError::Decode(format!("instrument master: {e}")))?;
    let total = raw.len();
    let out: Vec<_> = raw.into_iter().filter_map(RawInstrument::into_instrument).collect();
    tracing::info!(total, kept = out.len(), "parsed instrument master");
    if out.is_empty() {
        return Err(BrokerError::Decode("instrument master contained no usable rows".into()));
    }
    Ok(out)
}

/// Downloads the instrument master. It is a public file, so no account or
/// login is needed.
pub async fn download(cfg: &SmartApiConfig) -> BrokerResult<Vec<Instrument>> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(cfg.timeout_secs.max(120)))
        .build()
        .map_err(|e| BrokerError::Config(format!("http client: {e}")))?;
    let res = http.get(&cfg.instruments_url).send().await?;
    if !res.status().is_success() {
        return Err(BrokerError::Transient(format!(
            "instrument master: HTTP {}",
            res.status()
        )));
    }
    let bytes = res.bytes().await?;
    parse_master(&bytes)
}

impl SmartApi {
    pub async fn instrument_master(&self) -> BrokerResult<Vec<Instrument>> {
        download(&self.cfg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"token":"2885","symbol":"RELIANCE-EQ","name":"RELIANCE","expiry":"","strike":"-1.000000","lotsize":"1","instrumenttype":"","exch_seg":"NSE","tick_size":"10.000000"},
      {"token":"35003","symbol":"NIFTY26DEC2425000CE","name":"NIFTY","expiry":"26DEC2024","strike":"2500000.000000","lotsize":"75","instrumenttype":"OPTIDX","exch_seg":"NFO","tick_size":"5.000000"},
      {"token":"1","symbol":"X","name":"X","expiry":"","strike":"-1","lotsize":"1","instrumenttype":"","exch_seg":"NCDEX","tick_size":"1"}
    ]"#;

    #[test]
    fn parses_equity_and_derivative_rows() {
        let v = parse_master(SAMPLE.as_bytes()).unwrap();
        assert_eq!(v.len(), 2, "NCDEX row is skipped");
        let rel = &v[0];
        assert_eq!(rel.symbol.as_str(), "RELIANCE-EQ");
        assert_eq!(rel.exchange, Exchange::Nse);
        assert_eq!(rel.tick_size, Price(10));
        assert_eq!(rel.strike, None);
        assert_eq!(rel.expiry, None);
        let opt = &v[1];
        assert_eq!(opt.expiry, Some(NaiveDate::from_ymd_opt(2024, 12, 26).unwrap()));
        assert_eq!(opt.strike, Some(Price(2_500_000)));
        assert_eq!(opt.lot_size, 75);
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse_master(b"<html>oops</html>").is_err());
        assert!(parse_master(b"[]").is_err());
    }
}
