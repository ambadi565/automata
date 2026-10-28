//! End-to-end fetch against a mocked SmartAPI: download, then resume.

use std::collections::HashMap;

use chrono::NaiveDate;
use fetch::{CandleJob, fetch_candles, fetch_instruments};
use saarthi_broker::smartapi::{Credentials, SmartApi, SmartApiConfig};
use saarthi_core::{Exchange, Symbol, Tf};
use saarthi_store::parquet::{read_bars, read_instruments};
use saarthi_store::{DataLayout, Manifest};
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LOGIN: &str = "/rest/auth/angelbroking/user/v1/loginByPassword";
const CANDLES: &str = "/rest/secure/angelbroking/historical/v1/getCandleData";

const MASTER: &str = r#"[
  {"token":"2885","symbol":"RELIANCE-EQ","name":"RELIANCE","expiry":"","strike":"-1","lotsize":"1","instrumenttype":"","exch_seg":"NSE","tick_size":"10"},
  {"token":"11536","symbol":"TCS-EQ","name":"TCS","expiry":"","strike":"-1","lotsize":"1","instrumenttype":"","exch_seg":"NSE","tick_size":"10"}
]"#;

fn ok(data: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"status": true, "message": "SUCCESS", "errorcode": "", "data": data}))
}

async fn setup() -> (MockServer, SmartApiConfig, SmartApi) {
    let server = MockServer::start().await;
    Mock::given(path(LOGIN))
        .respond_with(ok(json!({"jwtToken": "jwt", "refreshToken": "r", "feedToken": "f"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/scrip.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MASTER))
        .mount(&server)
        .await;
    let cfg = SmartApiConfig {
        base_url: server.uri(),
        instruments_url: format!("{}/scrip.json", server.uri()),
        historical_requests_per_sec: 1000.0,
        requests_per_sec: 1000.0,
        max_retries: 1,
        retry_base_delay_ms: 1,
        ..SmartApiConfig::default()
    };
    let vars: HashMap<String, String> = [
        ("ANGEL_API_KEY", "k"),
        ("ANGEL_CLIENT_CODE", "T1"),
        ("ANGEL_MPIN", "0000"),
        ("ANGEL_TOTP_SECRET", "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let api = SmartApi::new(cfg.clone(), Credentials::from_map(&vars).unwrap()).unwrap();
    (server, cfg, api)
}

async fn candle_requests(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == CANDLES)
        .count()
}

fn d(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

#[tokio::test]
async fn downloads_then_resumes_without_refetching() {
    let (server, cfg, api) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let layout = DataLayout::new(dir.path());

    // RELIANCE has data; TCS returns nothing for any range.
    Mock::given(path(CANDLES))
        .and(body_partial_json(json!({"symboltoken": "2885"})))
        .respond_with(ok(json!([
            ["2026-10-26T09:15:00+05:30", 2810.5, 2815.0, 2805.0, 2812.0, 1000],
            ["2026-10-27T09:15:00+05:30", 2812.0, 2820.0, 2811.0, 2818.0, 1100]
        ])))
        .mount(&server)
        .await;
    Mock::given(path(CANDLES))
        .and(body_partial_json(json!({"symboltoken": "11536"})))
        .respond_with(ok(json!([])))
        .mount(&server)
        .await;

    let instruments = fetch_instruments(&cfg, &layout).await.unwrap();
    assert_eq!(read_instruments(&layout.instruments()).unwrap().len(), 2);

    let job = CandleJob {
        tf: Tf::M5,
        exchange: Exchange::Nse,
        symbols: vec![
            Symbol::new("RELIANCE-EQ"),
            Symbol::new("TCS-EQ"),
            Symbol::new("NOPE-EQ"),
        ],
        from: d("2025-10-27"),
        to: d("2026-10-27"),
    };
    let r1 = fetch_candles(&api, &layout, &instruments, &job).await.unwrap();
    assert_eq!(r1.updated.len(), 2);
    assert_eq!(r1.failed.len(), 1, "unknown symbol is reported, not fatal");
    assert_eq!(r1.requests, 8, "4 × 100-day windows per symbol");
    assert_eq!(candle_requests(&server).await, 8);

    let bars = read_bars(&layout.candles(Tf::M5, &Symbol::new("RELIANCE-EQ"))).unwrap();
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[1].close.paise(), 281800);

    let m = Manifest::load(&layout.manifest(Tf::M5)).unwrap();
    let cov = m.get("TCS-EQ").expect("empty ranges are still recorded as covered");
    assert_eq!((cov.from, cov.to), (d("2025-10-27"), d("2026-10-27")));

    // Same request again: only the last (possibly partial) day is refetched.
    let r2 = fetch_candles(&api, &layout, &instruments, &job).await.unwrap();
    assert_eq!(r2.requests, 2);
    assert_eq!(candle_requests(&server).await, 10);

    // Extending one day forward costs one request per symbol.
    let next = CandleJob {
        to: d("2026-10-28"),
        ..job.clone()
    };
    let r3 = fetch_candles(&api, &layout, &instruments, &next).await.unwrap();
    assert_eq!(r3.requests, 2);
    assert_eq!(
        read_bars(&layout.candles(Tf::M5, &Symbol::new("RELIANCE-EQ")))
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn failure_mid_symbol_keeps_progress_and_resumes() {
    let (server, cfg, api) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let layout = DataLayout::new(dir.path());
    let instruments = fetch_instruments(&cfg, &layout).await.unwrap();

    // First two windows succeed, then the broker fails hard.
    Mock::given(path(CANDLES))
        .respond_with(ok(json!([["2025-11-03T09:15:00+05:30", 1, 1, 1, 1, 1]])))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(path(CANDLES))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"status": false, "message": "Something Went Wrong", "errorcode": "AB1004", "data": null}),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path(CANDLES))
        .respond_with(ok(json!([])))
        .mount(&server)
        .await;

    let job = CandleJob {
        tf: Tf::M5,
        exchange: Exchange::Nse,
        symbols: vec![Symbol::new("RELIANCE-EQ")],
        from: d("2025-10-27"),
        to: d("2026-10-27"),
    };
    let r1 = fetch_candles(&api, &layout, &instruments, &job).await.unwrap();
    assert_eq!(r1.failed.len(), 1);
    assert_eq!(r1.requests, 3);
    assert_eq!(
        read_bars(&layout.candles(Tf::M5, &Symbol::new("RELIANCE-EQ")))
            .unwrap()
            .len(),
        1
    );

    // Resume: windows 1–2 are not requested again; window 2's last day is.
    let r2 = fetch_candles(&api, &layout, &instruments, &job).await.unwrap();
    assert!(r2.failed.is_empty());
    assert_eq!(r2.requests, 2, "{r2:?}");
    let cov = Manifest::load(&layout.manifest(Tf::M5))
        .unwrap()
        .get("RELIANCE-EQ")
        .unwrap();
    assert_eq!((cov.from, cov.to), (d("2025-10-27"), d("2026-10-27")));
}

#[tokio::test]
async fn auth_failure_aborts_the_run() {
    let server = MockServer::start().await;
    Mock::given(path(LOGIN))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"status": false, "message": "Invalid totp", "errorcode": "AB1050", "data": null}),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(path("/scrip.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MASTER))
        .mount(&server)
        .await;
    let cfg = SmartApiConfig {
        base_url: server.uri(),
        instruments_url: format!("{}/scrip.json", server.uri()),
        ..SmartApiConfig::default()
    };
    let vars: HashMap<String, String> = [
        ("ANGEL_API_KEY", "k"),
        ("ANGEL_CLIENT_CODE", "T1"),
        ("ANGEL_MPIN", "0000"),
        ("ANGEL_TOTP_SECRET", "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let api = SmartApi::new(cfg.clone(), Credentials::from_map(&vars).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let layout = DataLayout::new(dir.path());
    let instruments = fetch_instruments(&cfg, &layout).await.unwrap();
    let job = CandleJob {
        tf: Tf::D1,
        exchange: Exchange::Nse,
        symbols: vec![Symbol::new("RELIANCE-EQ"), Symbol::new("TCS-EQ")],
        from: d("2026-01-01"),
        to: d("2026-10-27"),
    };
    let err = fetch_candles(&api, &layout, &instruments, &job).await.unwrap_err();
    assert!(err.to_string().contains("authentication"), "{err:#}");
}
