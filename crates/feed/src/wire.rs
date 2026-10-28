//! SmartAPI WebSocket 2.0 binary packets.
//!
//! Every packet is little-endian. LTP mode is 51 bytes, Quote 123 and
//! SnapQuote 379; each mode extends the previous one:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 1 | subscription mode (1 LTP, 2 Quote, 3 SnapQuote) |
//! | 1 | 1 | exchange type |
//! | 2 | 25 | token, UTF-8, NUL padded |
//! | 27 | 8 | sequence number |
//! | 35 | 8 | exchange timestamp, epoch ms |
//! | 43 | 8 | last traded price |
//! | 51 | 8 | last traded quantity |
//! | 59 | 8 | average traded price |
//! | 67 | 8 | volume traded for the day |
//! | 75 | 8 | total buy quantity (f64) |
//! | 83 | 8 | total sell quantity (f64) |
//! | 91 | 8 | open price of the day |
//! | 99 | 8 | high price of the day |
//! | 107 | 8 | low price of the day |
//! | 115 | 8 | close price (previous day) |
//! | 123 | 8 | last traded timestamp, epoch ms |
//! | 131 | 8 | open interest |
//! | 139 | 8 | open interest change % (f64; dummy per Angel's docs) |
//! | 147 | 200 | best five buy + sell, 10 × 20-byte entries |
//! | 347 | 8 | upper circuit |
//! | 355 | 8 | lower circuit |
//! | 363 | 8 | 52-week high |
//! | 371 | 8 | 52-week low |
//!
//! A best-five entry is: buy/sell flag i16 (1 = buy), quantity i64, price i64,
//! number of orders i16.
//!
//! Prices are in paise, except for currency derivatives (exchange type 13),
//! which use 10⁻⁷ rupees and are converted to paise here.

use chrono::{DateTime, Utc};
use saarthi_core::{Exchange, Price, Tick, Ts};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const LTP_LEN: usize = 51;
pub const QUOTE_LEN: usize = 123;
pub const SNAP_QUOTE_LEN: usize = 379;
const TOKEN_LEN: usize = 25;
const DEPTH_ENTRY_LEN: usize = 20;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("packet too short: {got} bytes, mode {mode} needs {need}")]
    TooShort { mode: u8, got: usize, need: usize },
    #[error("unsupported subscription mode {0}")]
    UnsupportedMode(u8),
    #[error("unknown exchange type {0}")]
    UnknownExchange(u8),
    #[error("token is not valid UTF-8")]
    BadToken,
    #[error("timestamp {0} out of range")]
    BadTimestamp(i64),
}

/// Subscription mode. Angel's Depth-20 mode (4) has a different packet layout
/// and is not supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "u8", try_from = "u8")]
pub enum Mode {
    Ltp = 1,
    Quote = 2,
    SnapQuote = 3,
}

impl Mode {
    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn packet_len(self) -> usize {
        match self {
            Mode::Ltp => LTP_LEN,
            Mode::Quote => QUOTE_LEN,
            Mode::SnapQuote => SNAP_QUOTE_LEN,
        }
    }
}

impl From<Mode> for u8 {
    fn from(m: Mode) -> u8 {
        m.code()
    }
}

impl TryFrom<u8> for Mode {
    type Error = DecodeError;
    fn try_from(v: u8) -> Result<Self, DecodeError> {
        match v {
            1 => Ok(Mode::Ltp),
            2 => Ok(Mode::Quote),
            3 => Ok(Mode::SnapQuote),
            _ => Err(DecodeError::UnsupportedMode(v)),
        }
    }
}

/// WebSocket 2.0 exchange segment codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "u8", try_from = "u8")]
pub enum ExchangeType {
    NseCm = 1,
    NseFo = 2,
    BseCm = 3,
    BseFo = 4,
    McxFo = 5,
    NcxFo = 7,
    CdeFo = 13,
}

impl ExchangeType {
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The core [`Exchange`] this segment belongs to (NCDEX has none).
    pub fn exchange(self) -> Option<Exchange> {
        Some(match self {
            ExchangeType::NseCm => Exchange::Nse,
            ExchangeType::NseFo => Exchange::Nfo,
            ExchangeType::BseCm => Exchange::Bse,
            ExchangeType::BseFo => Exchange::Bfo,
            ExchangeType::McxFo => Exchange::Mcx,
            ExchangeType::CdeFo => Exchange::Cds,
            ExchangeType::NcxFo => return None,
        })
    }

    pub fn from_exchange(e: Exchange) -> Self {
        match e {
            Exchange::Nse => ExchangeType::NseCm,
            Exchange::Nfo => ExchangeType::NseFo,
            Exchange::Bse => ExchangeType::BseCm,
            Exchange::Bfo => ExchangeType::BseFo,
            Exchange::Mcx => ExchangeType::McxFo,
            Exchange::Cds => ExchangeType::CdeFo,
        }
    }

    /// Converts a raw wire price to paise.
    pub fn to_price(self, raw: i64) -> Price {
        match self {
            // 10^-7 rupees -> paise, rounded half away from zero.
            ExchangeType::CdeFo => {
                let q = raw / 100_000;
                let r = raw % 100_000;
                Price(q + if r.abs() * 2 >= 100_000 { r.signum() } else { 0 })
            }
            _ => Price(raw),
        }
    }

    /// The inverse of [`ExchangeType::to_price`], for building test frames.
    pub fn to_raw(self, p: Price) -> i64 {
        match self {
            ExchangeType::CdeFo => p.0 * 100_000,
            _ => p.0,
        }
    }
}

impl From<ExchangeType> for u8 {
    fn from(e: ExchangeType) -> u8 {
        e.code()
    }
}

impl TryFrom<u8> for ExchangeType {
    type Error = DecodeError;
    fn try_from(v: u8) -> Result<Self, DecodeError> {
        Ok(match v {
            1 => ExchangeType::NseCm,
            2 => ExchangeType::NseFo,
            3 => ExchangeType::BseCm,
            4 => ExchangeType::BseFo,
            5 => ExchangeType::McxFo,
            7 => ExchangeType::NcxFo,
            13 => ExchangeType::CdeFo,
            _ => return Err(DecodeError::UnknownExchange(v)),
        })
    }
}

/// Quote-mode fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub last_qty: i64,
    pub avg_price: Price,
    pub day_volume: i64,
    pub total_buy_qty: f64,
    pub total_sell_qty: f64,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    /// Previous session's close.
    pub close: Price,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthLevel {
    pub qty: i64,
    pub price: Price,
    pub orders: i16,
}

/// SnapQuote-mode fields.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapQuote {
    pub last_trade_ts: Ts,
    pub open_interest: i64,
    pub oi_change_pct: f64,
    /// Best five bids, in the order the exchange sent them.
    pub bids: Vec<DepthLevel>,
    pub asks: Vec<DepthLevel>,
    pub upper_circuit: Price,
    pub lower_circuit: Price,
    pub high_52w: Price,
    pub low_52w: Price,
}

/// One decoded market-data packet.
#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub mode: Mode,
    pub exchange_type: ExchangeType,
    pub token: String,
    pub sequence: i64,
    pub exchange_ts: Ts,
    pub ltp: Price,
    pub quote: Option<Quote>,
    pub snap: Option<SnapQuote>,
}

impl Packet {
    /// The engine-facing tick. `None` for segments with no core exchange.
    pub fn tick(&self) -> Option<Tick> {
        let q = self.quote.as_ref();
        Some(Tick {
            token: self.token.clone(),
            exchange: self.exchange_type.exchange()?,
            ts: self.exchange_ts,
            ltp: self.ltp,
            last_qty: q.and_then(|q| u64::try_from(q.last_qty).ok()),
            day_volume: q.and_then(|q| u64::try_from(q.day_volume).ok()),
            avg_price: q.map(|q| q.avg_price),
        })
    }
}

struct Reader<'a> {
    b: &'a [u8],
}

impl Reader<'_> {
    fn i64(&self, at: usize) -> i64 {
        i64::from_le_bytes(self.b[at..at + 8].try_into().expect("8 bytes"))
    }
    fn f64(&self, at: usize) -> f64 {
        f64::from_le_bytes(self.b[at..at + 8].try_into().expect("8 bytes"))
    }
    fn i16(&self, at: usize) -> i16 {
        i16::from_le_bytes(self.b[at..at + 2].try_into().expect("2 bytes"))
    }
    fn ts(&self, at: usize) -> Result<Ts, DecodeError> {
        let ms = self.i64(at);
        DateTime::<Utc>::from_timestamp_millis(ms).ok_or(DecodeError::BadTimestamp(ms))
    }
}

/// Decodes one binary frame. Trailing bytes beyond the mode's length are
/// ignored, so a future field appended by Angel does not break decoding.
pub fn decode(b: &[u8]) -> Result<Packet, DecodeError> {
    let Some(&mode_byte) = b.first() else {
        return Err(DecodeError::TooShort {
            mode: 0,
            got: 0,
            need: LTP_LEN,
        });
    };
    let mode = Mode::try_from(mode_byte)?;
    let need = mode.packet_len();
    if b.len() < need {
        return Err(DecodeError::TooShort {
            mode: mode_byte,
            got: b.len(),
            need,
        });
    }
    let ex = ExchangeType::try_from(b[1])?;
    let raw_token = &b[2..2 + TOKEN_LEN];
    let end = raw_token.iter().position(|&c| c == 0).unwrap_or(TOKEN_LEN);
    let token = std::str::from_utf8(&raw_token[..end])
        .map_err(|_| DecodeError::BadToken)?
        .to_string();
    let r = Reader { b };
    let px = |at| ex.to_price(r.i64(at));

    let quote = (mode >= Mode::Quote).then(|| Quote {
        last_qty: r.i64(51),
        avg_price: px(59),
        day_volume: r.i64(67),
        total_buy_qty: r.f64(75),
        total_sell_qty: r.f64(83),
        open: px(91),
        high: px(99),
        low: px(107),
        close: px(115),
    });

    let snap = if mode == Mode::SnapQuote {
        let mut bids = Vec::with_capacity(5);
        let mut asks = Vec::with_capacity(5);
        for i in 0..10 {
            let at = 147 + i * DEPTH_ENTRY_LEN;
            let level = DepthLevel {
                qty: r.i64(at + 2),
                price: px(at + 10),
                orders: r.i16(at + 18),
            };
            if r.i16(at) == 1 {
                bids.push(level);
            } else {
                asks.push(level);
            }
        }
        Some(SnapQuote {
            last_trade_ts: r.ts(123)?,
            open_interest: r.i64(131),
            oi_change_pct: r.f64(139),
            bids,
            asks,
            upper_circuit: px(347),
            lower_circuit: px(355),
            high_52w: px(363),
            low_52w: px(371),
        })
    } else {
        None
    };

    Ok(Packet {
        mode,
        exchange_type: ex,
        token,
        sequence: r.i64(27),
        exchange_ts: r.ts(35)?,
        ltp: px(43),
        quote,
        snap,
    })
}

/// Encodes a packet in the wire layout. Used to build synthetic frames for
/// tests and replays; the engine never sends these.
///
/// # Panics
/// If the token is longer than 25 bytes, or a SnapQuote has more than five
/// levels per side.
pub fn encode(p: &Packet) -> Vec<u8> {
    let mut b = vec![0u8; p.mode.packet_len()];
    let ex = p.exchange_type;
    let put = |b: &mut [u8], at: usize, v: i64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    let putf = |b: &mut [u8], at: usize, v: f64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    b[0] = p.mode.code();
    b[1] = ex.code();
    assert!(p.token.len() <= TOKEN_LEN, "token too long");
    b[2..2 + p.token.len()].copy_from_slice(p.token.as_bytes());
    put(&mut b, 27, p.sequence);
    put(&mut b, 35, p.exchange_ts.timestamp_millis());
    put(&mut b, 43, ex.to_raw(p.ltp));
    if let Some(q) = &p.quote {
        put(&mut b, 51, q.last_qty);
        put(&mut b, 59, ex.to_raw(q.avg_price));
        put(&mut b, 67, q.day_volume);
        putf(&mut b, 75, q.total_buy_qty);
        putf(&mut b, 83, q.total_sell_qty);
        put(&mut b, 91, ex.to_raw(q.open));
        put(&mut b, 99, ex.to_raw(q.high));
        put(&mut b, 107, ex.to_raw(q.low));
        put(&mut b, 115, ex.to_raw(q.close));
    }
    if let (Mode::SnapQuote, Some(s)) = (p.mode, &p.snap) {
        assert!(s.bids.len() <= 5 && s.asks.len() <= 5, "at most five levels per side");
        put(&mut b, 123, s.last_trade_ts.timestamp_millis());
        put(&mut b, 131, s.open_interest);
        putf(&mut b, 139, s.oi_change_pct);
        let levels = s.bids.iter().map(|l| (1i16, l)).chain(s.asks.iter().map(|l| (0i16, l)));
        for (i, (flag, l)) in levels.enumerate() {
            let at = 147 + i * DEPTH_ENTRY_LEN;
            b[at..at + 2].copy_from_slice(&flag.to_le_bytes());
            put(&mut b, at + 2, l.qty);
            put(&mut b, at + 10, ex.to_raw(l.price));
            b[at + 18..at + 20].copy_from_slice(&l.orders.to_le_bytes());
        }
        put(&mut b, 347, ex.to_raw(s.upper_circuit));
        put(&mut b, 355, ex.to_raw(s.lower_circuit));
        put(&mut b, 363, ex.to_raw(s.high_52w));
        put(&mut b, 371, ex.to_raw(s.low_52w));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An LTP frame written out byte by byte, independently of `encode`:
    /// NSE_CM token 2885 (RELIANCE), seq 7, 2026-10-27 09:15:00.250 IST,
    /// LTP Rs 2810.50.
    const LTP_FRAME_HEX: &str = concat!(
        "01", // mode
        "01", // NSE_CM
        "32383835",
        "000000000000000000000000000000000000000000", // "2885" + 21 NUL
        "0700000000000000",                           // seq 7
        "5a1b767ba1010000",                           // 1793072700250 ms
        "da49040000000000",                           // 281050 paise
    );

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_hand_written_ltp_frame() {
        let b = unhex(LTP_FRAME_HEX);
        assert_eq!(b.len(), LTP_LEN);
        let p = decode(&b).unwrap();
        assert_eq!(p.mode, Mode::Ltp);
        assert_eq!(p.exchange_type, ExchangeType::NseCm);
        assert_eq!(p.token, "2885");
        assert_eq!(p.sequence, 7);
        assert_eq!(p.exchange_ts.to_rfc3339(), "2026-10-27T03:45:00.250+00:00");
        assert_eq!(p.ltp, Price(281050));
        assert!(p.quote.is_none() && p.snap.is_none());
        assert_eq!(encode(&p), b, "encode must produce the same bytes");
        let t = p.tick().unwrap();
        assert_eq!((t.exchange, t.day_volume), (Exchange::Nse, None));
    }

    pub(crate) fn snap_packet() -> Packet {
        let ts = DateTime::<Utc>::from_timestamp_millis(1_793_072_700_250).unwrap();
        let lvl = |q, p, o| DepthLevel {
            qty: q,
            price: Price(p),
            orders: o,
        };
        Packet {
            mode: Mode::SnapQuote,
            exchange_type: ExchangeType::NseCm,
            token: "11536".into(),
            sequence: 99,
            exchange_ts: ts,
            ltp: Price(412_345),
            quote: Some(Quote {
                last_qty: 3,
                avg_price: Price(411_870),
                day_volume: 1_234_567,
                total_buy_qty: 456_789.0,
                total_sell_qty: 345_678.0,
                open: Price(410_000),
                high: Price(413_000),
                low: Price(409_500),
                close: Price(409_000),
            }),
            snap: Some(SnapQuote {
                last_trade_ts: ts,
                open_interest: 0,
                oi_change_pct: 0.0,
                bids: (0..5).map(|i| lvl(100 + i, 412_300 - i * 5, 2)).collect(),
                asks: (0..5).map(|i| lvl(200 + i, 412_350 + i * 5, 3)).collect(),
                upper_circuit: Price(449_900),
                lower_circuit: Price(368_100),
                high_52w: Price(460_000),
                low_52w: Price(320_000),
            }),
        }
    }

    #[test]
    fn snap_quote_round_trips() {
        let p = snap_packet();
        let b = encode(&p);
        assert_eq!(b.len(), SNAP_QUOTE_LEN);
        assert_eq!(decode(&b).unwrap(), p);
        // Spot-check offsets against the documented layout.
        assert_eq!(i64::from_le_bytes(b[67..75].try_into().unwrap()), 1_234_567);
        assert_eq!(i16::from_le_bytes(b[147..149].try_into().unwrap()), 1);
        assert_eq!(i64::from_le_bytes(b[371..379].try_into().unwrap()), 320_000);
        let t = p.tick().unwrap();
        assert_eq!(t.day_volume, Some(1_234_567));
        assert_eq!(t.avg_price, Some(Price(411_870)));
    }

    #[test]
    fn quote_mode_is_a_prefix_of_snap_quote() {
        let mut p = snap_packet();
        p.mode = Mode::Quote;
        p.snap = None;
        let b = encode(&p);
        assert_eq!(b.len(), QUOTE_LEN);
        assert_eq!(decode(&b).unwrap(), p);
        // Trailing bytes are tolerated.
        let mut longer = b.clone();
        longer.extend_from_slice(&[0xff; 9]);
        assert_eq!(decode(&longer).unwrap(), p);
    }

    #[test]
    fn rejects_malformed_frames() {
        assert!(matches!(decode(&[]), Err(DecodeError::TooShort { .. })));
        assert_eq!(decode(&[4; 60]), Err(DecodeError::UnsupportedMode(4)));
        let mut b = unhex(LTP_FRAME_HEX);
        assert!(matches!(decode(&b[..50]), Err(DecodeError::TooShort { got: 50, .. })));
        b[1] = 9;
        assert_eq!(decode(&b), Err(DecodeError::UnknownExchange(9)));
        let mut b = unhex(LTP_FRAME_HEX);
        b[2] = 0xff;
        assert_eq!(decode(&b), Err(DecodeError::BadToken));
        let mut q = snap_packet();
        q.mode = Mode::Quote;
        q.snap = None;
        assert!(matches!(
            decode(&encode(&q)[..100]),
            Err(DecodeError::TooShort { mode: 2, .. })
        ));
    }

    #[test]
    fn currency_prices_scale_to_paise() {
        let ex = ExchangeType::CdeFo;
        assert_eq!(ex.to_price(834_512_500), Price(8345)); // Rs 83.4512500
        assert_eq!(ex.to_price(834_550_000), Price(8346)); // .5 paise rounds up
        assert_eq!(ex.to_price(-834_550_000), Price(-8346));
        assert_eq!(ex.to_raw(Price(8345)), 834_500_000);
        assert_eq!(ExchangeType::NcxFo.exchange(), None);
        for e in [
            Exchange::Nse,
            Exchange::Nfo,
            Exchange::Bse,
            Exchange::Bfo,
            Exchange::Mcx,
            Exchange::Cds,
        ] {
            assert_eq!(ExchangeType::from_exchange(e).exchange(), Some(e));
        }
    }
}
