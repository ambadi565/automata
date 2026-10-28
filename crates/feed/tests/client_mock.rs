//! Runs the feed client against a local WebSocket server that speaks the
//! SmartAPI 2.0 protocol with synthetic binary frames.

// The handshake callback signature is fixed by tungstenite.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use saarthi_broker::Secret;
use saarthi_core::Price;
use saarthi_feed::wire::Quote;
use saarthi_feed::{
    ExchangeType, FeedAuth, FeedConfig, FeedError, FeedEvent, FeedHandle, Mode, Packet, Staleness, always_active,
    encode, spawn,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;

struct Conn {
    ws: WebSocketStream<TcpStream>,
    headers: Vec<(String, String)>,
}

impl Conn {
    fn header(&self, k: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    }

    async fn next_text(&mut self) -> String {
        loop {
            match timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("text in time")
            {
                Some(Ok(Message::Text(t))) => return t.to_string(),
                Some(Ok(_)) => continue,
                other => panic!("connection ended: {other:?}"),
            }
        }
    }

    async fn send_packet(&mut self, p: &Packet) {
        self.ws.send(Message::binary(encode(p))).await.unwrap();
    }
}

/// Accepts connections forever. Handshakes whose feed token is "bad" get 401.
async fn server() -> (String, mpsc::Receiver<Conn>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut headers = Vec::new();
                let cb = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                    for (k, v) in req.headers() {
                        headers.push((k.to_string(), v.to_str().unwrap_or("").to_string()));
                    }
                    if req.headers().get("x-feed-token").is_some_and(|v| v == "bad") {
                        let mut e = ErrorResponse::new(Some("unauthorized".into()));
                        *e.status_mut() = StatusCode::UNAUTHORIZED;
                        return Err(e);
                    }
                    Ok(resp)
                };
                if let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await {
                    let _ = tx.send(Conn { ws, headers }).await;
                }
            });
        }
    });
    (url, rx)
}

fn cfg(url: &str) -> FeedConfig {
    FeedConfig {
        url: url.to_string(),
        heartbeat_ms: 100,
        read_timeout_ms: 2_000,
        connect_timeout_ms: 2_000,
        reconnect_initial_ms: 20,
        reconnect_max_ms: 100,
        healthy_after_ms: 10_000,
        stale_warn_ms: 150,
        stale_critical_ms: 400,
        watchdog_ms: 20,
        max_tokens: 3,
        event_buffer: 1024,
        bearer_prefix: true,
    }
}

fn auth(feed_token: &str) -> FeedAuth {
    FeedAuth {
        jwt: Secret::new("Bearer eyJjwt"),
        api_key: Secret::new("key123"),
        client_code: "A123".into(),
        feed_token: Secret::new(feed_token),
    }
}

fn quote(token: &str, seq: i64, ltp: i64, volume: i64) -> Packet {
    Packet {
        mode: Mode::Quote,
        exchange_type: ExchangeType::NseCm,
        token: token.into(),
        sequence: seq,
        exchange_ts: DateTime::<Utc>::from_timestamp_millis(1_793_072_700_000 + seq * 250).unwrap(),
        ltp: Price(ltp),
        quote: Some(Quote {
            last_qty: 1,
            avg_price: Price(ltp),
            day_volume: volume,
            total_buy_qty: 0.0,
            total_sell_qty: 0.0,
            open: Price(ltp),
            high: Price(ltp),
            low: Price(ltp),
            close: Price(ltp),
        }),
        snap: None,
    }
}

async fn next_event(rx: &mut mpsc::Receiver<FeedEvent>, want: impl Fn(&FeedEvent) -> bool) -> FeedEvent {
    loop {
        let ev = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("event in time")
            .expect("feed alive");
        if want(&ev) {
            return ev;
        }
    }
}

async fn subscribed(handle: &FeedHandle) {
    handle
        .subscribe(Mode::Quote, vec![(ExchangeType::NseCm, "2885".into())])
        .await
        .unwrap();
}

#[tokio::test]
async fn subscribes_with_auth_headers_and_decodes_frames() {
    let (url, mut conns) = server().await;
    let (handle, mut events) = spawn(cfg(&url), auth("feedtok"), always_active(), None);
    subscribed(&handle).await;
    let mut c = conns.recv().await.unwrap();
    assert_eq!(c.header("authorization"), Some("Bearer eyJjwt"));
    assert_eq!(c.header("x-api-key"), Some("key123"));
    assert_eq!(c.header("x-client-code"), Some("A123"));
    assert_eq!(c.header("x-feed-token"), Some("feedtok"));

    let sub: serde_json::Value = serde_json::from_str(&c.next_text().await).unwrap();
    assert_eq!(sub["action"], 1);
    assert_eq!(sub["params"]["mode"], 2);
    assert_eq!(sub["params"]["tokenList"][0]["tokens"][0], "2885");
    assert_eq!(sub["correlationID"].as_str().unwrap().len(), 10);

    for i in 0..3 {
        c.send_packet(&quote("2885", i, 281_000 + i, 1000 + i)).await;
    }
    for i in 0..3 {
        match next_event(&mut events, |e| matches!(e, FeedEvent::Packet(_))).await {
            FeedEvent::Packet(p) => {
                assert_eq!(p.sequence, i);
                assert_eq!(p.tick().unwrap().day_volume, Some(1000 + i as u64));
            }
            _ => unreachable!(),
        }
    }
    assert!(handle.status().connected);
    assert!(handle.status().last_tick.is_some());

    // A malformed frame is counted, not fatal.
    c.ws.send(Message::binary(vec![1u8, 1, 2])).await.unwrap();
    c.send_packet(&quote("2885", 9, 1, 1)).await;
    next_event(&mut events, |e| matches!(e, FeedEvent::Packet(p) if p.sequence == 9)).await;
    assert_eq!(handle.status().decode_errors, 1);
    handle.shutdown().await;
}

#[tokio::test]
async fn sends_heartbeats_and_reports_server_errors() {
    let (url, mut conns) = server().await;
    let (handle, mut events) = spawn(cfg(&url), auth("feedtok"), always_active(), None);
    subscribed(&handle).await;
    let mut c = conns.recv().await.unwrap();
    let _sub = c.next_text().await;
    assert_eq!(c.next_text().await, "ping");
    c.ws.send(Message::text("pong")).await.unwrap();
    c.ws.send(Message::text(
        r#"{"correlationID":"saarthi001","errorCode":"E1002","errorMessage":"Invalid Request Payload."}"#,
    ))
    .await
    .unwrap();
    match next_event(&mut events, |e| matches!(e, FeedEvent::ServerError { .. })).await {
        FeedEvent::ServerError { code, .. } => assert_eq!(code, "E1002"),
        _ => unreachable!(),
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn reconnects_and_resubscribes_after_a_drop() {
    let (url, mut conns) = server().await;
    let (handle, mut events) = spawn(cfg(&url), auth("feedtok"), always_active(), None);
    subscribed(&handle).await;
    handle
        .subscribe(Mode::Ltp, vec![(ExchangeType::NseCm, "11536".into())])
        .await
        .unwrap();
    let mut c = conns.recv().await.unwrap();
    // Both subscriptions arrive on the first connection (in whatever order
    // the handshake raced them).
    let _ = c.next_text().await;
    let _ = c.next_text().await;
    drop(c);
    next_event(&mut events, |e| matches!(e, FeedEvent::Disconnected { .. })).await;
    assert!(!handle.status().connected);

    let mut c2 = conns.recv().await.unwrap();
    let mut modes = Vec::new();
    for _ in 0..2 {
        let v: serde_json::Value = serde_json::from_str(&c2.next_text().await).unwrap();
        modes.push((
            v["params"]["mode"].as_u64().unwrap(),
            v["params"]["tokenList"][0]["tokens"][0].as_str().unwrap().to_string(),
        ));
    }
    modes.sort();
    assert_eq!(modes, vec![(1, "11536".to_string()), (2, "2885".to_string())]);
    match next_event(&mut events, |e| matches!(e, FeedEvent::Connected { .. })).await {
        FeedEvent::Connected { attempt } => assert!(attempt >= 1),
        _ => unreachable!(),
    }
    c2.send_packet(&quote("2885", 1, 100, 1)).await;
    next_event(&mut events, |e| matches!(e, FeedEvent::Packet(_))).await;
    assert_eq!(handle.status().reconnects, 1);
    handle.shutdown().await;
}

#[tokio::test]
async fn silence_goes_stale_then_critical_then_recovers() {
    let (url, mut conns) = server().await;
    let (handle, mut events) = spawn(cfg(&url), auth("feedtok"), always_active(), None);
    subscribed(&handle).await;
    let mut c = conns.recv().await.unwrap();
    let _ = c.next_text().await;
    next_event(&mut events, |e| matches!(e, FeedEvent::Health(Staleness::Fresh))).await;
    assert!(handle.status().entries_allowed());

    next_event(&mut events, |e| matches!(e, FeedEvent::Health(Staleness::Stale { .. }))).await;
    assert!(!handle.status().entries_allowed());
    assert!(!handle.status().should_flatten());
    next_event(&mut events, |e| {
        matches!(e, FeedEvent::Health(Staleness::Critical { .. }))
    })
    .await;
    assert!(handle.status().should_flatten());

    c.send_packet(&quote("2885", 1, 100, 1)).await;
    next_event(&mut events, |e| matches!(e, FeedEvent::Health(Staleness::Fresh))).await;
    assert!(handle.status().entries_allowed());
    handle.shutdown().await;
}

#[tokio::test]
async fn idle_outside_market_hours() {
    let (url, mut conns) = server().await;
    let active = Arc::new(AtomicBool::new(false));
    let a = active.clone();
    let hours = Arc::new(move |_| a.load(Ordering::SeqCst));
    let (handle, mut events) = spawn(cfg(&url), auth("feedtok"), hours, None);
    subscribed(&handle).await;
    let _c = conns.recv().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(handle.status().staleness, Staleness::Idle);
    active.store(true, Ordering::SeqCst);
    next_event(&mut events, |e| matches!(e, FeedEvent::Health(Staleness::Fresh))).await;
    handle.shutdown().await;
}

#[tokio::test]
async fn auth_rejection_waits_for_new_credentials() {
    let (url, mut conns) = server().await;
    let (handle, mut events) = spawn(cfg(&url), auth("bad"), always_active(), None);
    match next_event(&mut events, |e| matches!(e, FeedEvent::AuthRejected { .. })).await {
        FeedEvent::AuthRejected { status } => assert_eq!(status, 401),
        _ => unreachable!(),
    }
    assert!(!handle.status().connected);
    handle.set_auth(auth("good")).await.unwrap();
    let c = conns.recv().await.unwrap();
    assert_eq!(c.header("x-feed-token"), Some("good"));
    next_event(&mut events, |e| matches!(e, FeedEvent::Connected { .. })).await;
    handle.shutdown().await;
}

#[tokio::test]
async fn token_limit_is_enforced() {
    let (url, _conns) = server().await;
    let (handle, _events) = spawn(cfg(&url), auth("feedtok"), always_active(), None);
    let toks = |r: std::ops::Range<u32>| r.map(|i| (ExchangeType::NseCm, i.to_string())).collect::<Vec<_>>();
    handle.subscribe(Mode::Quote, toks(0..3)).await.unwrap();
    assert_eq!(
        handle.subscribe(Mode::Quote, toks(3..4)).await,
        Err(FeedError::TooManyTokens(4, 3))
    );
    // Re-subscribing a held token in another mode does not count twice.
    handle.subscribe(Mode::Ltp, toks(0..1)).await.unwrap();
    handle.unsubscribe(toks(0..1)).await.unwrap();
    handle.subscribe(Mode::Quote, toks(3..4)).await.unwrap();
    handle.shutdown().await;
}

#[tokio::test]
async fn records_raw_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frames.bin");
    let rec = saarthi_feed::record::Recorder::open(&path).unwrap();
    let (url, mut conns) = server().await;
    let (handle, mut events) = spawn(cfg(&url), auth("feedtok"), always_active(), Some(rec));
    subscribed(&handle).await;
    let mut c = conns.recv().await.unwrap();
    let _ = c.next_text().await;
    let sent: Vec<_> = (0..5).map(|i| quote("2885", i, 100 + i, i)).collect();
    for p in &sent {
        c.send_packet(p).await;
    }
    next_event(&mut events, |e| matches!(e, FeedEvent::Packet(p) if p.sequence == 4)).await;
    handle.shutdown().await;
    // Wait for the task to stop and flush.
    while events.recv().await.is_some() {}
    let frames = saarthi_feed::record::read_frames(&path).unwrap();
    let decoded: Vec<_> = frames.iter().map(|(_, b)| saarthi_feed::decode(b).unwrap()).collect();
    assert_eq!(decoded, sent);
}
