//! Live market data from Angel One's SmartAPI WebSocket 2.0.
//!
//! - [`wire`] decodes (and, for tests, encodes) the binary packets.
//! - [`client`] runs the connection: subscribe, heartbeat, reconnect with
//!   exponential backoff, resubscribe, and a staleness watchdog whose state the
//!   risk guard reads through [`FeedHandle::status`].
//! - [`record`] writes raw frames to disk so a session can be replayed.
//!
//! The feed only reads market data. Nothing here can place an order.

pub mod backoff;
pub mod client;
pub mod record;
pub mod request;
pub mod stale;
pub mod wire;

pub use client::{
    ActiveHours, FeedAuth, FeedConfig, FeedError, FeedEvent, FeedHandle, FeedStatus, always_active, market_hours, spawn,
};
pub use stale::Staleness;
pub use wire::{DecodeError, ExchangeType, Mode, Packet, decode, encode};
