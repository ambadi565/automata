//! The WebSocket client task: connect, subscribe, heartbeat, decode,
//! reconnect with backoff, and watch for staleness.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use saarthi_broker::{Secret, Session};
use saarthi_core::{MarketClock, Phase, Ts};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::backoff::Backoff;
use crate::record::Recorder;
use crate::request::{Action, PING, ServerText, parse_server_text, request_json};
use crate::stale::{Staleness, StalenessMonitor};
use crate::wire::{ExchangeType, Mode, Packet, decode};

pub const DEFAULT_URL: &str = "wss://smartapisocket.angelone.in/smart-stream";

/// Feed tunables. Durations are in milliseconds so they read naturally in
/// `engine.toml`'s `[feed]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedConfig {
    pub url: String,
    /// Text `ping` interval. Angel drops connections silent for over 30 s.
    pub heartbeat_ms: u64,
    /// Reconnect if nothing at all (not even `pong`) arrives for this long.
    pub read_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
    /// A connection that lasts this long resets the backoff.
    pub healthy_after_ms: u64,
    /// No ticks for this long in market hours blocks new entries.
    pub stale_warn_ms: u64,
    /// No ticks for this long in market hours means flatten.
    pub stale_critical_ms: u64,
    pub watchdog_ms: u64,
    /// Angel allows 1000 token subscriptions per connection.
    pub max_tokens: usize,
    pub event_buffer: usize,
    /// Send `Authorization: Bearer <jwt>` (true) or the bare JWT.
    pub bearer_prefix: bool,
}

impl Default for FeedConfig {
    fn default() -> Self {
        FeedConfig {
            url: DEFAULT_URL.into(),
            heartbeat_ms: 25_000,
            read_timeout_ms: 70_000,
            connect_timeout_ms: 10_000,
            reconnect_initial_ms: 500,
            reconnect_max_ms: 30_000,
            healthy_after_ms: 60_000,
            stale_warn_ms: 10_000,
            stale_critical_ms: 120_000,
            watchdog_ms: 1_000,
            max_tokens: 1_000,
            event_buffer: 65_536,
            bearer_prefix: true,
        }
    }
}

/// What the feed needs from a logged-in session. Everything is a secret.
#[derive(Debug, Clone)]
pub struct FeedAuth {
    pub jwt: Secret,
    pub api_key: Secret,
    pub client_code: String,
    pub feed_token: Secret,
}

impl FeedAuth {
    pub fn from_session(session: &Session, api_key: &Secret) -> Self {
        FeedAuth {
            jwt: session.jwt.clone(),
            api_key: api_key.clone(),
            client_code: session.client_code.clone(),
            feed_token: session.feed_token.clone(),
        }
    }
}

/// When ticks are expected. Outside these hours silence is not staleness.
pub type ActiveHours = Arc<dyn Fn(Ts) -> bool + Send + Sync>;

/// Market hours from the clock: continuous trading through the close. If the
/// holiday calendar does not cover today, the feed is treated as active so a
/// silent feed still raises the alarm.
pub fn market_hours(clock: MarketClock) -> ActiveHours {
    Arc::new(move |now| {
        matches!(
            clock.phase(now),
            Ok(Phase::Open | Phase::NoNewEntries | Phase::SquareOff) | Err(_)
        )
    })
}

pub fn always_active() -> ActiveHours {
    Arc::new(|_| true)
}

#[derive(Debug, Clone)]
pub enum FeedEvent {
    Connected {
        attempt: u32,
    },
    Disconnected {
        reason: String,
    },
    /// The handshake was refused (401/403). Log in again and call
    /// [`FeedHandle::set_auth`]; the client keeps retrying at the max backoff.
    AuthRejected {
        status: u16,
    },
    Packet(Packet),
    Health(Staleness),
    ServerError {
        code: String,
        message: String,
    },
}

/// The feed's current state, readable at any time by the risk guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedStatus {
    pub connected: bool,
    pub staleness: Staleness,
    pub last_tick: Option<Ts>,
    pub reconnects: u64,
    pub decode_errors: u64,
}

impl FeedStatus {
    /// Whether the feed is healthy enough to open new positions.
    pub fn entries_allowed(&self) -> bool {
        self.connected && matches!(self.staleness, Staleness::Fresh | Staleness::Idle)
    }

    /// Whether the feed has been silent long enough to flatten.
    pub fn should_flatten(&self) -> bool {
        matches!(self.staleness, Staleness::Critical { .. })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FeedError {
    #[error("subscription would hold {0} tokens; the limit is {1}")]
    TooManyTokens(usize, usize),
    #[error("feed task has stopped")]
    Stopped,
}

enum Command {
    Subscribe(
        Mode,
        Vec<(ExchangeType, String)>,
        oneshot::Sender<Result<(), FeedError>>,
    ),
    Unsubscribe(Vec<(ExchangeType, String)>),
    SetAuth(FeedAuth),
    Shutdown,
}

/// Controls a running feed. Dropping every handle stops the feed.
#[derive(Clone)]
pub struct FeedHandle {
    cmd: mpsc::Sender<Command>,
    status: watch::Receiver<FeedStatus>,
}

impl FeedHandle {
    /// Subscribes `tokens` in `mode`; a token already subscribed in another
    /// mode is moved. The subscription survives reconnects.
    pub async fn subscribe(&self, mode: Mode, tokens: Vec<(ExchangeType, String)>) -> Result<(), FeedError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Command::Subscribe(mode, tokens, tx))
            .await
            .map_err(|_| FeedError::Stopped)?;
        rx.await.map_err(|_| FeedError::Stopped)?
    }

    pub async fn unsubscribe(&self, tokens: Vec<(ExchangeType, String)>) -> Result<(), FeedError> {
        self.cmd
            .send(Command::Unsubscribe(tokens))
            .await
            .map_err(|_| FeedError::Stopped)
    }

    /// New tokens after a re-login. Reconnects immediately if the last
    /// handshake was refused.
    pub async fn set_auth(&self, auth: FeedAuth) -> Result<(), FeedError> {
        self.cmd
            .send(Command::SetAuth(auth))
            .await
            .map_err(|_| FeedError::Stopped)
    }

    pub async fn shutdown(&self) {
        let _ = self.cmd.send(Command::Shutdown).await;
    }

    pub fn status(&self) -> FeedStatus {
        *self.status.borrow()
    }

    pub fn watch_status(&self) -> watch::Receiver<FeedStatus> {
        self.status.clone()
    }
}

/// Starts the feed on the current tokio runtime.
pub fn spawn(
    cfg: FeedConfig,
    auth: FeedAuth,
    active: ActiveHours,
    recorder: Option<Recorder>,
) -> (FeedHandle, mpsc::Receiver<FeedEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (ev_tx, ev_rx) = mpsc::channel(cfg.event_buffer.max(16));
    let initial = FeedStatus {
        connected: false,
        staleness: Staleness::Idle,
        last_tick: None,
        reconnects: 0,
        decode_errors: 0,
    };
    let (st_tx, st_rx) = watch::channel(initial);
    let ms = Duration::from_millis;
    let task = Task {
        monitor: StalenessMonitor::new(ms(cfg.stale_warn_ms), ms(cfg.stale_critical_ms)),
        backoff: Backoff::new(ms(cfg.reconnect_initial_ms), ms(cfg.reconnect_max_ms)),
        cfg,
        auth,
        active,
        recorder,
        subs: BTreeMap::new(),
        cmds: cmd_rx,
        events: ev_tx,
        status: st_tx,
        corr: 0,
        connections: 0,
        stop: false,
        auth_rejected: false,
    };
    tokio::spawn(task.run());
    (
        FeedHandle {
            cmd: cmd_tx,
            status: st_rx,
        },
        ev_rx,
    )
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

enum ConnectError {
    Auth(u16),
    Other(String),
}

struct Task {
    cfg: FeedConfig,
    auth: FeedAuth,
    active: ActiveHours,
    recorder: Option<Recorder>,
    subs: BTreeMap<(ExchangeType, String), Mode>,
    cmds: mpsc::Receiver<Command>,
    events: mpsc::Sender<FeedEvent>,
    status: watch::Sender<FeedStatus>,
    monitor: StalenessMonitor,
    backoff: Backoff,
    corr: u64,
    connections: u64,
    stop: bool,
    auth_rejected: bool,
}

impl Task {
    async fn run(mut self) {
        while !self.stop {
            let attempt = self.backoff.attempt();
            match self.connect().await {
                Ok(ws) => {
                    self.auth_rejected = false;
                    self.connections += 1;
                    let reconnects = self.connections - 1;
                    self.status.send_modify(|s| {
                        s.connected = true;
                        s.reconnects = reconnects;
                    });
                    tracing::info!(attempt, "feed connected");
                    self.emit(FeedEvent::Connected { attempt }).await;
                    let started = Instant::now();
                    let reason = self.session(ws).await;
                    self.status.send_modify(|s| s.connected = false);
                    if started.elapsed() >= Duration::from_millis(self.cfg.healthy_after_ms) {
                        self.backoff.reset();
                    }
                    if self.stop {
                        break;
                    }
                    tracing::warn!(%reason, "feed disconnected");
                    self.emit(FeedEvent::Disconnected { reason }).await;
                }
                Err(ConnectError::Auth(status)) => {
                    tracing::error!(status, "feed handshake refused; a fresh login is needed");
                    self.auth_rejected = true;
                    self.emit(FeedEvent::AuthRejected { status }).await;
                }
                Err(ConnectError::Other(e)) => {
                    tracing::warn!(error = %e, attempt, "feed connect failed");
                    self.emit(FeedEvent::Disconnected { reason: e }).await;
                }
            }
            if self.stop {
                break;
            }
            let delay = if self.auth_rejected {
                Duration::from_millis(self.cfg.reconnect_max_ms)
            } else {
                self.backoff.next_delay()
            };
            self.wait(delay).await;
        }
        if let Some(r) = self.recorder.as_mut() {
            let _ = r.flush();
        }
        self.status.send_modify(|s| s.connected = false);
    }

    async fn emit(&mut self, ev: FeedEvent) {
        if self.events.send(ev).await.is_err() {
            // Nobody is listening any more.
            self.stop = true;
        }
    }

    fn next_corr(&mut self) -> String {
        self.corr += 1;
        format!("saarthi{:03}", self.corr % 1000)
    }

    async fn connect(&mut self) -> Result<Ws, ConnectError> {
        let mut req = self
            .cfg
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| ConnectError::Other(format!("bad feed url: {e}")))?;
        let jwt = self.auth.jwt.expose().trim();
        let jwt = jwt.strip_prefix("Bearer ").unwrap_or(jwt);
        let authz = if self.cfg.bearer_prefix {
            format!("Bearer {jwt}")
        } else {
            jwt.to_string()
        };
        let header = |v: &str| {
            HeaderValue::from_str(v).map_err(|_| ConnectError::Other("credential is not a valid header".into()))
        };
        let h = req.headers_mut();
        h.insert("Authorization", header(&authz)?);
        h.insert("x-api-key", header(self.auth.api_key.expose())?);
        h.insert("x-client-code", header(&self.auth.client_code)?);
        h.insert("x-feed-token", header(self.auth.feed_token.expose())?);

        let fut = tokio_tungstenite::connect_async(req);
        match tokio::time::timeout(Duration::from_millis(self.cfg.connect_timeout_ms), fut).await {
            Err(_) => Err(ConnectError::Other("connect timed out".into())),
            Ok(Err(tungstenite::Error::Http(resp))) if matches!(resp.status().as_u16(), 401 | 403) => {
                Err(ConnectError::Auth(resp.status().as_u16()))
            }
            Ok(Err(e)) => Err(ConnectError::Other(e.to_string())),
            Ok(Ok((ws, _))) => Ok(ws),
        }
    }

    /// Sends subscriptions for everything in `subs`, one request per mode.
    async fn resubscribe(&mut self, ws: &mut Ws) -> Result<(), tungstenite::Error> {
        let mut by_mode: BTreeMap<Mode, Vec<(ExchangeType, String)>> = BTreeMap::new();
        for ((ex, t), m) in &self.subs {
            by_mode.entry(*m).or_default().push((*ex, t.clone()));
        }
        for (mode, tokens) in by_mode {
            let id = self.next_corr();
            ws.send(Message::text(request_json(&id, Action::Subscribe, mode, &tokens)))
                .await?;
        }
        Ok(())
    }

    /// Runs one connection until it fails or the feed is shut down.
    async fn session(&mut self, mut ws: Ws) -> String {
        if let Err(e) = self.resubscribe(&mut ws).await {
            return format!("subscribe failed: {e}");
        }
        let ms = Duration::from_millis;
        let mut heartbeat = tokio::time::interval(ms(self.cfg.heartbeat_ms));
        heartbeat.tick().await;
        let mut watchdog = tokio::time::interval(ms(self.cfg.watchdog_ms));
        let read_timeout = ms(self.cfg.read_timeout_ms);
        let mut last_rx = Instant::now();
        loop {
            tokio::select! {
                msg = ws.next() => {
                    last_rx = Instant::now();
                    match msg {
                        None => return "connection closed".into(),
                        Some(Err(e)) => return e.to_string(),
                        Some(Ok(Message::Binary(b))) => self.on_frame(&b).await,
                        Some(Ok(Message::Text(t))) => match parse_server_text(t.as_str()) {
                            ServerText::Pong => {}
                            ServerText::Error { code, message } => {
                                tracing::error!(%code, %message, "feed server error");
                                self.emit(FeedEvent::ServerError { code, message }).await;
                            }
                            ServerText::Other(s) => tracing::debug!(text = %s, "feed text frame"),
                        },
                        Some(Ok(Message::Close(f))) => {
                            return format!("server closed: {}", f.map(|f| f.reason.to_string()).unwrap_or_default());
                        }
                        Some(Ok(_)) => {} // ping/pong frames are answered by tungstenite
                    }
                    if self.stop {
                        return "event receiver dropped".into();
                    }
                }
                _ = heartbeat.tick() => {
                    if let Err(e) = ws.send(Message::text(PING)).await {
                        return format!("heartbeat failed: {e}");
                    }
                }
                _ = watchdog.tick() => {
                    self.watchdog().await;
                    if last_rx.elapsed() >= read_timeout {
                        return format!("nothing received for {read_timeout:?}");
                    }
                }
                cmd = self.cmds.recv() => {
                    let Some(cmd) = cmd else {
                        self.stop = true;
                        let _ = ws.close(None).await;
                        return "all handles dropped".into();
                    };
                    match self.apply(cmd) {
                        Applied::Send(msgs) => {
                            for m in msgs {
                                if let Err(e) = ws.send(Message::text(m)).await {
                                    return format!("send failed: {e}");
                                }
                            }
                        }
                        Applied::Reconnect => {
                            let _ = ws.close(None).await;
                            return "credentials updated".into();
                        }
                        Applied::Stop => {
                            let _ = ws.close(None).await;
                            return "shutdown".into();
                        }
                        Applied::Nothing => {}
                    }
                }
            }
        }
    }

    async fn on_frame(&mut self, b: &[u8]) {
        let now = Utc::now();
        if let Some(r) = self.recorder.as_mut()
            && let Err(e) = r.write(now, b)
        {
            tracing::error!(error = %e, "frame recording failed; recording stopped");
            self.recorder = None;
        }
        match decode(b) {
            Ok(p) => {
                self.monitor.on_tick(now);
                self.status.send_modify(|s| s.last_tick = Some(now));
                self.emit(FeedEvent::Packet(p)).await;
            }
            Err(e) => {
                self.status.send_modify(|s| s.decode_errors += 1);
                tracing::warn!(error = %e, len = b.len(), "undecodable feed frame");
            }
        }
    }

    async fn watchdog(&mut self) {
        let now = Utc::now();
        if let Some(state) = self.monitor.check(now, (self.active)(now)) {
            match state {
                Staleness::Stale { silent } | Staleness::Critical { silent } => {
                    tracing::warn!(?silent, ?state, "feed is stale");
                }
                _ => tracing::info!(?state, "feed health"),
            }
            self.status.send_modify(|s| s.staleness = state);
            self.emit(FeedEvent::Health(state)).await;
        }
    }

    /// Waits out a backoff delay while still serving commands and the
    /// watchdog, so staleness is reported during an outage too.
    async fn wait(&mut self, delay: Duration) {
        let deadline = tokio::time::Instant::now() + delay;
        let mut watchdog = tokio::time::interval(Duration::from_millis(self.cfg.watchdog_ms));
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return,
                _ = watchdog.tick() => self.watchdog().await,
                cmd = self.cmds.recv() => {
                    let Some(cmd) = cmd else {
                        self.stop = true;
                        return;
                    };
                    match self.apply(cmd) {
                        Applied::Stop => return,
                        Applied::Reconnect => return,
                        Applied::Send(_) | Applied::Nothing => {}
                    }
                }
            }
            if self.stop {
                return;
            }
        }
    }

    fn apply(&mut self, cmd: Command) -> Applied {
        match cmd {
            Command::Subscribe(mode, tokens, reply) => {
                let mut next = self.subs.clone();
                let mut moved: BTreeMap<Mode, Vec<(ExchangeType, String)>> = BTreeMap::new();
                for t in &tokens {
                    if let Some(old) = next.insert(t.clone(), mode)
                        && old != mode
                    {
                        moved.entry(old).or_default().push(t.clone());
                    }
                }
                if next.len() > self.cfg.max_tokens {
                    let _ = reply.send(Err(FeedError::TooManyTokens(next.len(), self.cfg.max_tokens)));
                    return Applied::Nothing;
                }
                self.subs = next;
                let _ = reply.send(Ok(()));
                let mut msgs = Vec::new();
                for (old, ts) in moved {
                    let id = self.next_corr();
                    msgs.push(request_json(&id, Action::Unsubscribe, old, &ts));
                }
                let id = self.next_corr();
                msgs.push(request_json(&id, Action::Subscribe, mode, &tokens));
                Applied::Send(msgs)
            }
            Command::Unsubscribe(tokens) => {
                let mut by_mode: BTreeMap<Mode, Vec<(ExchangeType, String)>> = BTreeMap::new();
                for t in tokens {
                    if let Some(m) = self.subs.remove(&t) {
                        by_mode.entry(m).or_default().push(t);
                    }
                }
                let msgs = by_mode
                    .into_iter()
                    .map(|(m, ts)| {
                        let id = self.next_corr();
                        request_json(&id, Action::Unsubscribe, m, &ts)
                    })
                    .collect();
                Applied::Send(msgs)
            }
            Command::SetAuth(auth) => {
                self.auth = auth;
                if self.auth_rejected {
                    self.backoff.reset();
                    Applied::Reconnect
                } else {
                    // Takes effect on the next reconnect; the live connection
                    // stays up.
                    Applied::Nothing
                }
            }
            Command::Shutdown => {
                self.stop = true;
                Applied::Stop
            }
        }
    }
}

enum Applied {
    Send(Vec<String>),
    Reconnect,
    Stop,
    Nothing,
}
