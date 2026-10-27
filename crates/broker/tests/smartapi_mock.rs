//! SmartAPI adapter against a mocked HTTP server. No real credentials needed.

use std::collections::HashMap;

use chrono::NaiveDate;
use saarthi_broker::smartapi::{Credentials, SmartApi, SmartApiConfig};
use saarthi_broker::{Broker, BrokerError, OrderKind, OrderRequest, Product};
use saarthi_core::{Exchange, Instrument, MarketClock, Price, Side, Symbol, Tf};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LOGIN: &str = "/rest/auth/angelbroking/user/v1/loginByPassword";
const CANDLES: &str = "/rest/secure/angelbroking/historical/v1/getCandleData";
const RMS: &str = "/rest/secure/angelbroking/user/v1/getRMS";

fn creds() -> Credentials {
    let vars: HashMap<String, String> = [
        ("ANGEL_API_KEY", "test-key"),
        ("ANGEL_CLIENT_CODE", "T123"),
        ("ANGEL_MPIN", "1111"),
        ("ANGEL_TOTP_SECRET", "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    Credentials::from_map(&vars).unwrap()
}

fn api(server: &MockServer) -> SmartApi {
    let cfg = SmartApiConfig {
        base_url: server.uri(),
        instruments_url: format!("{}/scrip.json", server.uri()),
        historical_requests_per_sec: 1000.0,
        requests_per_sec: 1000.0,
        max_retries: 3,
        retry_base_delay_ms: 1,
        ..SmartApiConfig::default()
    };
    SmartApi::new(cfg, creds()).unwrap()
}

fn ok(data: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"status": true, "message": "SUCCESS", "errorcode": "", "data": data}))
}

fn fail(code: &str, msg: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"status": false, "message": msg, "errorcode": code, "data": null}))
}

async fn mock_login(server: &MockServer, jwt: &str, times: u64) {
    Mock::given(method("POST"))
        .and(path(LOGIN))
        .and(header("X-PrivateKey", "test-key"))
        .and(header("X-UserType", "USER"))
        .and(header_exists("X-ClientPublicIP"))
        .and(body_partial_json(json!({"clientcode": "T123", "password": "1111"})))
        .respond_with(ok(
            json!({"jwtToken": format!("Bearer {jwt}"), "refreshToken": "r", "feedToken": "f"}),
        ))
        .expect(times)
        .mount(server)
        .await;
}

fn reliance() -> Instrument {
    Instrument {
        token: "2885".into(),
        symbol: Symbol::new("RELIANCE-EQ"),
        name: "RELIANCE".into(),
        exchange: Exchange::Nse,
        instrument_type: String::new(),
        lot_size: 1,
        tick_size: Price(10),
        expiry: None,
        strike: None,
    }
}

fn day(d: &str, hm: &str) -> saarthi_core::Ts {
    MarketClock::at(
        d.parse::<NaiveDate>().unwrap(),
        saarthi_core::clock::parse_hhmm(hm).unwrap(),
    )
}

#[tokio::test]
async fn login_sends_totp_and_strips_bearer_prefix() {
    let server = MockServer::start().await;
    mock_login(&server, "jwt-1", 1).await;
    let s = api(&server).login().await.unwrap();
    assert_eq!(s.jwt.expose(), "jwt-1");
    assert_eq!(s.client_code, "T123");

    let req = &server.received_requests().await.unwrap()[0];
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    let totp = body["totp"].as_str().unwrap();
    assert_eq!(totp.len(), 6);
    assert!(totp.chars().all(|c| c.is_ascii_digit()));
}

#[tokio::test]
async fn bad_credentials_are_an_auth_error_without_retries() {
    let server = MockServer::start().await;
    Mock::given(path(LOGIN))
        .respond_with(fail("AB1050", "Invalid totp"))
        .expect(1)
        .mount(&server)
        .await;
    let err = api(&server).login().await.unwrap_err();
    assert!(matches!(err, BrokerError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn candles_are_fetched_in_chunks_and_decoded() {
    let server = MockServer::start().await;
    mock_login(&server, "jwt-1", 1).await;
    Mock::given(method("POST"))
        .and(path(CANDLES))
        .and(header("Authorization", "Bearer jwt-1"))
        .and(body_partial_json(
            json!({"exchange": "NSE", "symboltoken": "2885", "interval": "FIVE_MINUTE"}),
        ))
        .respond_with(ok(json!([
            ["2026-10-27T09:20:00+05:30", 2811.0, 2812.0, 2810.0, 2811.5, 900],
            ["2026-10-27T09:15:00+05:30", 2810.5, 2815.0, 2805.05, 2811.0, 1000]
        ])))
        .expect(4)
        .mount(&server)
        .await;

    let bars = api(&server)
        .candles(
            &reliance(),
            Tf::M5,
            day("2025-10-27", "09:15"),
            day("2026-10-27", "15:30"),
        )
        .await
        .unwrap();
    // Four chunks returned the same two bars; they are sorted and deduplicated.
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].ts, day("2026-10-27", "09:15"));
    assert_eq!(bars[0].low, Price(280505));
}

#[tokio::test]
async fn null_data_means_no_bars() {
    let server = MockServer::start().await;
    mock_login(&server, "jwt-1", 1).await;
    Mock::given(path(CANDLES))
        .respond_with(ok(json!(null)))
        .mount(&server)
        .await;
    let bars = api(&server)
        .candles_chunk(
            &reliance(),
            Tf::D1,
            day("2026-10-26", "09:15"),
            day("2026-10-27", "15:30"),
        )
        .await
        .unwrap();
    assert!(bars.is_empty());
}

#[tokio::test]
async fn rate_limit_is_retried_with_backoff() {
    let server = MockServer::start().await;
    mock_login(&server, "jwt-1", 1).await;
    Mock::given(path(CANDLES))
        .respond_with(ResponseTemplate::new(403).set_body_string("Access denied because of exceeding access rate"))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(path(CANDLES))
        .respond_with(ok(json!([["2026-10-27T09:15:00+05:30", 1, 1, 1, 1, 1]])))
        .expect(1)
        .mount(&server)
        .await;
    let bars = api(&server)
        .candles_chunk(
            &reliance(),
            Tf::M5,
            day("2026-10-27", "09:15"),
            day("2026-10-27", "15:30"),
        )
        .await
        .unwrap();
    assert_eq!(bars.len(), 1);
}

#[tokio::test]
async fn persistent_server_errors_give_up_after_max_retries() {
    let server = MockServer::start().await;
    mock_login(&server, "jwt-1", 1).await;
    Mock::given(path(CANDLES))
        .respond_with(ResponseTemplate::new(502))
        .expect(4)
        .mount(&server)
        .await;
    let err = api(&server)
        .candles_chunk(
            &reliance(),
            Tf::M5,
            day("2026-10-27", "09:15"),
            day("2026-10-27", "15:30"),
        )
        .await
        .unwrap_err();
    assert!(err.is_transient(), "{err:?}");
}

#[tokio::test]
async fn expired_session_triggers_one_relogin() {
    let server = MockServer::start().await;
    mock_login(&server, "jwt-1", 2).await;
    Mock::given(path(RMS))
        .respond_with(fail("AG8001", "Invalid Token"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path(RMS))
        .respond_with(ok(json!({"net": "100000.5000", "availablecash": "99999.99"})))
        .mount(&server)
        .await;
    let f = api(&server).funds().await.unwrap();
    assert_eq!(f.net, Price(10_000_050));
    assert_eq!(f.available_cash, Price(9_999_999));
}

#[tokio::test]
async fn auth_failure_after_relogin_is_fatal() {
    let server = MockServer::start().await;
    mock_login(&server, "jwt-1", 2).await;
    Mock::given(path(RMS))
        .respond_with(fail("AG8001", "Invalid Token"))
        .expect(2)
        .mount(&server)
        .await;
    let err = api(&server).funds().await.unwrap_err();
    assert!(matches!(err, BrokerError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn api_rejections_are_not_retried() {
    let server = MockServer::start().await;
    mock_login(&server, "jwt-1", 1).await;
    Mock::given(path(CANDLES))
        .respond_with(fail("AB1004", "Something Went Wrong"))
        .expect(1)
        .mount(&server)
        .await;
    let err = api(&server)
        .candles_chunk(
            &reliance(),
            Tf::M5,
            day("2026-10-27", "09:15"),
            day("2026-10-27", "15:30"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BrokerError::Api { code, .. } if code == "AB1004"),
        "{err:?}"
    );
}

#[tokio::test]
async fn instrument_master_downloads_without_login() {
    let server = MockServer::start().await;
    Mock::given(path(LOGIN))
        .respond_with(ok(json!({})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/scrip.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"token":"2885","symbol":"RELIANCE-EQ","name":"RELIANCE","expiry":"","strike":"-1","lotsize":"1","instrumenttype":"","exch_seg":"NSE","tick_size":"10"}]"#,
        ))
        .mount(&server)
        .await;
    let v = api(&server).instrument_master().await.unwrap();
    assert_eq!(v, vec![reliance()]);
}

#[tokio::test]
async fn order_placement_is_impossible_in_this_build() {
    let server = MockServer::start().await;
    let req = OrderRequest {
        client_order_id: "c1".into(),
        symbol: Symbol::new("RELIANCE-EQ"),
        exchange: Exchange::Nse,
        token: "2885".into(),
        side: Side::Buy,
        qty: 1,
        kind: OrderKind::Market,
        product: Product::Intraday,
        tag: "saarthi".into(),
    };
    let err = api(&server).place(req).await.unwrap_err();
    assert!(matches!(err, BrokerError::Unsupported(_)));
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "no HTTP call may be made"
    );
}
