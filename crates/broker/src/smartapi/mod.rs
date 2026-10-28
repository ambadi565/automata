//! Angel One SmartAPI adapter (REST).
//!
//! M0 covers login, funds, the instrument master and historical candles.
//! Order placement deliberately returns [`BrokerError::Unsupported`] until the
//! go-live milestone wires and tests the order endpoints, so this build cannot
//! send a live order. Paper trading uses [`crate::Paper`] instead.

pub mod auth;
pub mod historical;
pub mod instruments;

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use reqwest::{Method, StatusCode};
use saarthi_core::{Bar, Instrument, Price, Tf, Ts};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::Broker;
use crate::error::{BrokerError, BrokerResult};
use crate::ratelimit::RateLimiter;
use crate::secret::Secret;
use crate::types::*;

pub use auth::Credentials;

pub const DEFAULT_BASE_URL: &str = "https://apiconnect.angelone.in";
pub const DEFAULT_INSTRUMENTS_URL: &str =
    "https://margincalculator.angelbroking.com/OpenAPI_File/files/OpenAPIScripMaster.json";

const LOGIN_PATH: &str = "/rest/auth/angelbroking/user/v1/loginByPassword";
const PROFILE_PATH: &str = "/rest/secure/angelbroking/user/v1/getProfile";
const FUNDS_PATH: &str = "/rest/secure/angelbroking/user/v1/getRMS";

/// Error codes meaning "your session token is no longer valid".
const AUTH_ERROR_CODES: &[&str] = &["AG8001", "AG8002", "AG8003", "AB1010"];
/// Error codes SmartAPI uses for throttling (observed; not fully documented).
const RATE_LIMIT_CODES: &[&str] = &["AB1019", "AB1021"];

/// Tunables for one SmartAPI connection. Shared across accounts.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SmartApiConfig {
    pub base_url: String,
    pub instruments_url: String,
    /// SmartAPI documents 3 req/s for historical data; stay under it.
    pub historical_requests_per_sec: f64,
    /// Spacing for other REST calls.
    pub requests_per_sec: f64,
    pub max_retries: u32,
    pub retry_base_delay_ms: u64,
    pub timeout_secs: u64,
    /// Sent as X-ClientLocalIP / X-ClientPublicIP / X-MACAddress. For live
    /// trading the public IP must be the static IP registered with Angel One.
    pub client_local_ip: String,
    pub client_public_ip: String,
    pub mac_address: String,
}

impl Default for SmartApiConfig {
    fn default() -> Self {
        SmartApiConfig {
            base_url: DEFAULT_BASE_URL.into(),
            instruments_url: DEFAULT_INSTRUMENTS_URL.into(),
            historical_requests_per_sec: 2.0,
            requests_per_sec: 5.0,
            max_retries: 5,
            retry_base_delay_ms: 1000,
            timeout_secs: 30,
            client_local_ip: "127.0.0.1".into(),
            client_public_ip: "127.0.0.1".into(),
            mac_address: "00:00:00:00:00:00".into(),
        }
    }
}

/// SmartAPI's JSON envelope: `{status, message, errorcode, data}`.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    status: bool,
    #[serde(default)]
    message: String,
    #[serde(default)]
    errorcode: String,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginData {
    jwt_token: String,
    refresh_token: String,
    #[serde(default)]
    feed_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    #[serde(rename = "clientcode", default)]
    pub client_code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub exchanges: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RmsData {
    #[serde(default)]
    net: Option<String>,
    #[serde(rename = "availablecash", default)]
    available_cash: Option<String>,
}

/// One logged-in (or to-be-logged-in) Angel One account.
pub struct SmartApi {
    cfg: SmartApiConfig,
    creds: Credentials,
    http: reqwest::Client,
    session: RwLock<Option<Session>>,
    rest_limiter: RateLimiter,
    hist_limiter: RateLimiter,
}

/// What a single HTTP attempt produced.
enum Attempt<T> {
    Done(T),
    Retry(BrokerError),
    Relogin(BrokerError),
    Fail(BrokerError),
}

impl SmartApi {
    pub fn new(cfg: SmartApiConfig, creds: Credentials) -> BrokerResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .user_agent(concat!("saarthi/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| BrokerError::Config(format!("http client: {e}")))?;
        Ok(SmartApi {
            rest_limiter: RateLimiter::per_second(cfg.requests_per_sec),
            hist_limiter: RateLimiter::per_second(cfg.historical_requests_per_sec),
            cfg,
            creds,
            http,
            session: RwLock::new(None),
        })
    }

    pub fn config(&self) -> &SmartApiConfig {
        &self.cfg
    }

    pub fn client_code(&self) -> &str {
        &self.creds.client_code
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path);
        self.http
            .request(method, url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("X-UserType", "USER")
            .header("X-SourceID", "WEB")
            .header("X-ClientLocalIP", &self.cfg.client_local_ip)
            .header("X-ClientPublicIP", &self.cfg.client_public_ip)
            .header("X-MACAddress", &self.cfg.mac_address)
            .header("X-PrivateKey", self.creds.api_key.expose())
    }

    /// Logs in with client code + MPIN + TOTP and stores the session.
    pub async fn login_now(&self) -> BrokerResult<Session> {
        let mut now = Utc::now().timestamp() as u64;
        // A code generated in the last 2 s of its window may expire in flight.
        if auth::totp_seconds_left(now) <= 2 {
            tokio::time::sleep(Duration::from_secs(auth::totp_seconds_left(now))).await;
            now = Utc::now().timestamp() as u64;
        }
        let totp = auth::totp_code(&self.creds.totp_secret, now)?;
        let body = serde_json::json!({
            "clientcode": self.creds.client_code,
            "password": self.creds.mpin.expose(),
            "totp": totp,
        });

        let mut attempt = 0;
        let data: LoginData = loop {
            self.rest_limiter.acquire().await;
            let res = self.request(Method::POST, LOGIN_PATH).json(&body).send().await;
            match classify::<LoginData>(res).await {
                Attempt::Done(Some(d)) => break d,
                Attempt::Done(None) => return Err(BrokerError::Decode("login: empty data".into())),
                Attempt::Relogin(e) | Attempt::Fail(e) => return Err(BrokerError::Auth(e.to_string())),
                Attempt::Retry(e) if attempt < self.cfg.max_retries => {
                    tracing::warn!(error = %e, attempt, "login failed transiently; retrying");
                    self.backoff(attempt).await;
                    attempt += 1;
                }
                Attempt::Retry(e) => return Err(e),
            }
        };

        let strip = |s: String| s.strip_prefix("Bearer ").map(str::to_string).unwrap_or(s);
        let session = Session {
            client_code: self.creds.client_code.clone(),
            jwt: Secret::new(strip(data.jwt_token)),
            refresh_token: Secret::new(data.refresh_token),
            feed_token: Secret::new(data.feed_token),
            created_at: Utc::now(),
        };
        tracing::info!(client = %session.client_code, "SmartAPI login ok");
        *self.session.write().await = Some(session.clone());
        Ok(session)
    }

    async fn jwt(&self) -> BrokerResult<Secret> {
        if let Some(s) = self.session.read().await.as_ref() {
            return Ok(s.jwt.clone());
        }
        Ok(self.login_now().await?.jwt)
    }

    async fn backoff(&self, attempt: u32) {
        let ms = self
            .cfg
            .retry_base_delay_ms
            .saturating_mul(1 << attempt.min(6))
            .min(30_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    /// Authenticated call with throttling, retry on transient errors and one
    /// re-login on an expired session.
    async fn secure<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        limiter: &RateLimiter,
    ) -> BrokerResult<Option<T>> {
        let mut attempt = 0;
        let mut relogged = false;
        loop {
            let jwt = self.jwt().await?;
            limiter.acquire().await;
            let mut req = self.request(method.clone(), path).bearer_auth(jwt.expose());
            if let Some(b) = body {
                req = req.json(b);
            }
            match classify::<T>(req.send().await).await {
                Attempt::Done(d) => return Ok(d),
                Attempt::Relogin(e) if !relogged => {
                    tracing::warn!(error = %e, "session rejected; logging in again");
                    relogged = true;
                    self.login_now().await?;
                }
                Attempt::Relogin(e) => return Err(BrokerError::Auth(e.to_string())),
                Attempt::Fail(e) => return Err(e),
                Attempt::Retry(e) if attempt < self.cfg.max_retries => {
                    tracing::warn!(error = %e, path, attempt, "transient error; backing off");
                    self.backoff(attempt).await;
                    attempt += 1;
                }
                Attempt::Retry(e) => return Err(e),
            }
        }
    }

    pub async fn profile(&self) -> BrokerResult<Profile> {
        self.secure::<Profile>(Method::GET, PROFILE_PATH, None, &self.rest_limiter)
            .await?
            .ok_or_else(|| BrokerError::Decode("profile: empty data".into()))
    }
}

/// Turns an HTTP result into success / retry / re-login / hard error.
async fn classify<T: DeserializeOwned>(res: Result<reqwest::Response, reqwest::Error>) -> Attempt<Option<T>> {
    let res = match res {
        Ok(r) => r,
        Err(e) => return Attempt::Retry(BrokerError::Transient(crate::error::error_chain(&e))),
    };
    let status = res.status();
    let text = match res.text().await {
        Ok(t) => t,
        Err(e) => return Attempt::Retry(BrokerError::Transient(crate::error::error_chain(&e))),
    };
    let lower = text.to_ascii_lowercase();
    if status == StatusCode::TOO_MANY_REQUESTS || lower.contains("access rate") || lower.contains("too many requests") {
        return Attempt::Retry(BrokerError::RateLimited(snippet(&text)));
    }
    if status.is_server_error() {
        return Attempt::Retry(BrokerError::Transient(format!("HTTP {status}: {}", snippet(&text))));
    }
    let env: Envelope<T> = match serde_json::from_str(&text) {
        Ok(e) => e,
        Err(e) if status == StatusCode::UNAUTHORIZED => {
            return Attempt::Relogin(BrokerError::Auth(format!("HTTP 401: {e}")));
        }
        Err(e) => return Attempt::Fail(BrokerError::Decode(format!("HTTP {status}: {e}: {}", snippet(&text)))),
    };
    if env.status {
        return Attempt::Done(env.data);
    }
    let code = env.errorcode;
    let err = BrokerError::Api {
        code: code.clone(),
        message: env.message,
    };
    if AUTH_ERROR_CODES.contains(&code.as_str()) || status == StatusCode::UNAUTHORIZED {
        Attempt::Relogin(err)
    } else if RATE_LIMIT_CODES.contains(&code.as_str()) {
        Attempt::Retry(BrokerError::RateLimited(err.to_string()))
    } else {
        Attempt::Fail(err)
    }
}

fn snippet(s: &str) -> String {
    s.chars().take(200).collect()
}

/// Lenient rupee parser for broker payloads ("1000.0000", "", "-5.5").
pub(crate) fn rupees_str(s: Option<&str>) -> Price {
    s.and_then(|v| v.trim().parse::<f64>().ok())
        .and_then(Price::from_rupees_f64)
        .unwrap_or(Price::ZERO)
}

#[async_trait]
impl Broker for SmartApi {
    async fn login(&self) -> BrokerResult<Session> {
        self.login_now().await
    }

    async fn place(&self, _req: OrderRequest) -> BrokerResult<BrokerOrderId> {
        Err(BrokerError::Unsupported(
            "live order placement is disabled until go-live; use paper mode",
        ))
    }

    async fn modify(&self, _id: &BrokerOrderId, _m: Modify) -> BrokerResult<()> {
        Err(BrokerError::Unsupported(
            "live order modification is disabled until go-live",
        ))
    }

    async fn cancel(&self, _id: &BrokerOrderId) -> BrokerResult<()> {
        Err(BrokerError::Unsupported(
            "live order cancellation is disabled until go-live",
        ))
    }

    async fn positions(&self) -> BrokerResult<Vec<Position>> {
        Err(BrokerError::Unsupported("live positions arrive with go-live"))
    }

    async fn order_book(&self) -> BrokerResult<Vec<BrokerOrder>> {
        Err(BrokerError::Unsupported("live order book arrives with go-live"))
    }

    async fn funds(&self) -> BrokerResult<Funds> {
        let rms: RmsData = self
            .secure(Method::GET, FUNDS_PATH, None, &self.rest_limiter)
            .await?
            .ok_or_else(|| BrokerError::Decode("getRMS: empty data".into()))?;
        Ok(Funds {
            net: rupees_str(rms.net.as_deref()),
            available_cash: rupees_str(rms.available_cash.as_deref()),
        })
    }

    async fn candles(&self, inst: &Instrument, tf: Tf, from: Ts, to: Ts) -> BrokerResult<Vec<Bar>> {
        let mut out = Vec::new();
        for (f, t) in historical::chunks(tf, from, to) {
            out.extend(self.candles_chunk(inst, tf, f, t).await?);
        }
        historical::sort_dedup(&mut out);
        Ok(out)
    }
}
