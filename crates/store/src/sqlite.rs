//! The hot store: SQLite in WAL mode, one database per account.
//!
//! It holds what the engine must not lose to a crash: trades, orders (written
//! *before* they are sent to the broker), fills, an append-only event log,
//! per-day summaries and small key/value state such as the risk guard's
//! counters. Every write is a transaction with `synchronous = FULL`, so a
//! committed row survives `kill -9` and power loss alike.
//!
//! Records are stored as JSON next to the few columns that are queried, so
//! the OMS owns its types and the schema stays small.

use std::path::Path;

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{StoreError, StoreResult};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS trades (
    id         TEXT PRIMARY KEY,
    day        TEXT NOT NULL,
    token      TEXT NOT NULL,
    state      TEXT NOT NULL,
    body       TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS trades_day ON trades(day);
CREATE INDEX IF NOT EXISTS trades_state ON trades(state);

CREATE TABLE IF NOT EXISTS orders (
    client_id  TEXT PRIMARY KEY,
    trade_id   TEXT NOT NULL REFERENCES trades(id),
    broker_id  TEXT,
    status     TEXT NOT NULL,
    body       TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS orders_trade ON orders(trade_id);
CREATE UNIQUE INDEX IF NOT EXISTS orders_broker ON orders(broker_id) WHERE broker_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS fills (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id  TEXT NOT NULL REFERENCES orders(client_id),
    -- Cumulative filled quantity after this fill: makes replays idempotent.
    cum_qty    INTEGER NOT NULL,
    qty        INTEGER NOT NULL,
    price      INTEGER NOT NULL,
    ts         TEXT NOT NULL,
    UNIQUE (client_id, cum_qty)
);

CREATE TABLE IF NOT EXISTS events (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    ts     TEXT NOT NULL,
    kind   TEXT NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_ts ON events(ts);

CREATE TABLE IF NOT EXISTS days (
    day  TEXT PRIMARY KEY,
    body TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// One row of the event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub detail: String,
}

/// One recorded fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillRow {
    pub client_id: String,
    pub cum_qty: u32,
    pub qty: u32,
    /// Paise.
    pub price: i64,
    pub ts: String,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens (creating if needed) the database at `path` in WAL mode and
    /// migrates it.
    pub fn open(path: &Path) -> StoreResult<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::io(dir, e))?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn in_memory() -> StoreResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> StoreResult<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::Sqlite(format!(
                "database schema v{version} is newer than this build (v{SCHEMA_VERSION})"
            )));
        }
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Db { conn })
    }

    /// `wal` for a file database.
    pub fn journal_mode(&self) -> StoreResult<String> {
        Ok(self.conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?)
    }

    /// Runs `f` in one transaction: all of its writes land, or none do.
    pub fn write<R>(&mut self, f: impl FnOnce(&Writer<'_>) -> StoreResult<R>) -> StoreResult<R> {
        let tx = self.conn.transaction()?;
        let r = f(&Writer { tx: &tx })?;
        tx.commit()?;
        Ok(r)
    }

    /// Trades whose state is not one of `done`.
    pub fn trades_excluding<T: DeserializeOwned>(&self, done: &[&str]) -> StoreResult<Vec<T>> {
        let mut st = self.conn.prepare("SELECT state, body FROM trades ORDER BY rowid")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (state, body) = row?;
            if !done.contains(&state.as_str()) {
                out.push(serde_json::from_str(&body)?);
            }
        }
        Ok(out)
    }

    pub fn trades_on<T: DeserializeOwned>(&self, day: NaiveDate) -> StoreResult<Vec<T>> {
        let mut st = self
            .conn
            .prepare("SELECT body FROM trades WHERE day = ?1 ORDER BY rowid")?;
        let rows = st.query_map([day.to_string()], |r| r.get::<_, String>(0))?;
        rows.map(|b| Ok(serde_json::from_str(&b?)?)).collect()
    }

    pub fn trade_count_on(&self, day: NaiveDate) -> StoreResult<u32> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM trades WHERE day = ?1", [day.to_string()], |r| {
                r.get(0)
            })?)
    }

    pub fn orders_of<T: DeserializeOwned>(&self, trade_id: &str) -> StoreResult<Vec<T>> {
        let mut st = self
            .conn
            .prepare("SELECT body FROM orders WHERE trade_id = ?1 ORDER BY rowid")?;
        let rows = st.query_map([trade_id], |r| r.get::<_, String>(0))?;
        rows.map(|b| Ok(serde_json::from_str(&b?)?)).collect()
    }

    pub fn fills_of(&self, client_id: &str) -> StoreResult<Vec<FillRow>> {
        let mut st = self
            .conn
            .prepare("SELECT client_id, cum_qty, qty, price, ts FROM fills WHERE client_id = ?1 ORDER BY cum_qty")?;
        let rows = st.query_map([client_id], |r| {
            Ok(FillRow {
                client_id: r.get(0)?,
                cum_qty: r.get(1)?,
                qty: r.get(2)?,
                price: r.get(3)?,
                ts: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Events with id greater than `after`, oldest first.
    pub fn events_after(&self, after: i64, limit: usize) -> StoreResult<Vec<EventRow>> {
        let mut st = self
            .conn
            .prepare("SELECT id, ts, kind, detail FROM events WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let rows = st.query_map(params![after, limit as i64], |r| {
            Ok(EventRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                detail: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn kv_get<T: DeserializeOwned>(&self, key: &str) -> StoreResult<Option<T>> {
        let v: Option<String> = self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?;
        v.map(|s| serde_json::from_str(&s).map_err(StoreError::from))
            .transpose()
    }

    pub fn day<T: DeserializeOwned>(&self, day: NaiveDate) -> StoreResult<Option<T>> {
        let v: Option<String> = self
            .conn
            .query_row("SELECT body FROM days WHERE day = ?1", [day.to_string()], |r| r.get(0))
            .optional()?;
        v.map(|s| serde_json::from_str(&s).map_err(StoreError::from))
            .transpose()
    }

    /// Every stored day summary, oldest first.
    pub fn days<T: DeserializeOwned>(&self) -> StoreResult<Vec<(NaiveDate, T)>> {
        let mut st = self.conn.prepare("SELECT day, body FROM days ORDER BY day")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (d, b) = row?;
            let d = d.parse().map_err(|e| StoreError::Decode(format!("day {d:?}: {e}")))?;
            out.push((d, serde_json::from_str(&b)?));
        }
        Ok(out)
    }
}

/// Writes inside one [`Db::write`] transaction.
pub struct Writer<'a> {
    tx: &'a Transaction<'a>,
}

impl Writer<'_> {
    pub fn put_trade(
        &self,
        id: &str,
        day: NaiveDate,
        token: &str,
        state: &str,
        body: &impl Serialize,
    ) -> StoreResult<()> {
        self.tx.execute(
            "INSERT INTO trades (id, day, token, state, body) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET state = excluded.state, body = excluded.body,
               updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![id, day.to_string(), token, state, serde_json::to_string(body)?],
        )?;
        Ok(())
    }

    pub fn put_order(
        &self,
        client_id: &str,
        trade_id: &str,
        broker_id: Option<&str>,
        status: &str,
        body: &impl Serialize,
    ) -> StoreResult<()> {
        self.tx.execute(
            "INSERT INTO orders (client_id, trade_id, broker_id, status, body) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(client_id) DO UPDATE SET broker_id = excluded.broker_id, status = excluded.status,
               body = excluded.body, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![client_id, trade_id, broker_id, status, serde_json::to_string(body)?],
        )?;
        Ok(())
    }

    /// Records a fill; a fill already recorded (same order, same cumulative
    /// quantity) is ignored, so replaying the broker's order book is safe.
    pub fn add_fill(&self, f: &FillRow) -> StoreResult<bool> {
        let n = self.tx.execute(
            "INSERT OR IGNORE INTO fills (client_id, cum_qty, qty, price, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![f.client_id, f.cum_qty, f.qty, f.price, f.ts],
        )?;
        Ok(n == 1)
    }

    pub fn event(&self, ts: &str, kind: &str, detail: &impl Serialize) -> StoreResult<()> {
        self.tx.execute(
            "INSERT INTO events (ts, kind, detail) VALUES (?1, ?2, ?3)",
            params![ts, kind, serde_json::to_string(detail)?],
        )?;
        Ok(())
    }

    pub fn kv_put(&self, key: &str, value: &impl Serialize) -> StoreResult<()> {
        self.tx.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    pub fn put_day(&self, day: NaiveDate, body: &impl Serialize) -> StoreResult<()> {
        self.tx.execute(
            "INSERT INTO days (day, body) VALUES (?1, ?2) ON CONFLICT(day) DO UPDATE SET body = excluded.body",
            params![day.to_string(), serde_json::to_string(body)?],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct T {
        id: String,
        n: u32,
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
    }

    #[test]
    fn file_db_uses_wal_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/saarthi.db");
        {
            let mut db = Db::open(&path).unwrap();
            assert_eq!(db.journal_mode().unwrap(), "wal");
            db.write(|w| {
                w.put_trade("t1", day(), "2885", "open", &T { id: "t1".into(), n: 1 })?;
                w.put_order("t1-e", "t1", Some("B1"), "filled", &T { id: "e".into(), n: 2 })?;
                w.kv_put("risk", &42u32)
            })
            .unwrap();
            // Dropped without any shutdown: a committed write must still be there.
        }
        let db = Db::open(&path).unwrap();
        let live: Vec<T> = db.trades_excluding(&["closed"]).unwrap();
        assert_eq!(live, vec![T { id: "t1".into(), n: 1 }]);
        assert_eq!(db.orders_of::<T>("t1").unwrap().len(), 1);
        assert_eq!(db.kv_get::<u32>("risk").unwrap(), Some(42));
        assert_eq!(db.trade_count_on(day()).unwrap(), 1);
    }

    #[test]
    fn failed_transaction_writes_nothing() {
        let mut db = Db::in_memory().unwrap();
        let r = db.write(|w| {
            w.put_trade("t1", day(), "1", "open", &1)?;
            // Foreign key violation: unknown trade.
            w.put_order("x", "nope", None, "new", &1)
        });
        assert!(r.is_err());
        assert!(db.trades_excluding::<u32>(&[]).unwrap().is_empty());
    }

    #[test]
    fn upserts_and_state_filter() {
        let mut db = Db::in_memory().unwrap();
        db.write(|w| w.put_trade("t1", day(), "1", "open", &1)).unwrap();
        db.write(|w| w.put_trade("t1", day(), "1", "closed", &2)).unwrap();
        assert!(db.trades_excluding::<u32>(&["closed"]).unwrap().is_empty());
        assert_eq!(db.trades_on::<u32>(day()).unwrap(), vec![2]);
    }

    #[test]
    fn duplicate_fills_are_ignored() {
        let mut db = Db::in_memory().unwrap();
        db.write(|w| {
            w.put_trade("t1", day(), "1", "open", &1)?;
            w.put_order("t1-e", "t1", None, "open", &1)
        })
        .unwrap();
        let f = FillRow {
            client_id: "t1-e".into(),
            cum_qty: 10,
            qty: 10,
            price: 100,
            ts: "x".into(),
        };
        assert!(db.write(|w| w.add_fill(&f)).unwrap());
        assert!(!db.write(|w| w.add_fill(&f)).unwrap());
        assert_eq!(db.fills_of("t1-e").unwrap().len(), 1);
    }

    #[test]
    fn events_and_days() {
        let mut db = Db::in_memory().unwrap();
        db.write(|w| {
            w.event("2026-10-28T04:00:00Z", "boot", &"hello")?;
            w.event("2026-10-28T04:00:01Z", "halt", &"x")?;
            w.put_day(day(), &T { id: "d".into(), n: 5 })
        })
        .unwrap();
        let ev = db.events_after(0, 10).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(db.events_after(ev[0].id, 10).unwrap()[0].kind, "halt");
        assert_eq!(db.days::<T>().unwrap()[0].1.n, 5);
        assert_eq!(db.day::<T>(day()).unwrap().unwrap().n, 5);
    }
}
