"""History scan, news filtering and the read-only trade store."""

import json
import sqlite3
import time
from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

import polars as pl

from saarthi_strategist import history, news
from saarthi_strategist.store import Store, account_db

IST = ZoneInfo("Asia/Kolkata")


def write_5m(root, symbol, days=20, base=100_000, vol=1000, last_vol=None):
    rows = []
    d0 = date(2026, 9, 28)
    for i in range(days):
        d = d0 + timedelta(days=i)
        for k in range(75):
            ts = datetime(d.year, d.month, d.day, 9, 15, tzinfo=IST) + timedelta(minutes=5 * k)
            px = base + i * 100 + (k % 5) * 50
            v = last_vol if (last_vol and i == days - 1) else vol
            rows.append((ts, px, px + 100, px - 100, px + 20, v))
    df = pl.DataFrame(rows, schema=["ts", "open", "high", "low", "close", "volume"], orient="row")
    df = df.with_columns(
        pl.col("ts").dt.convert_time_zone("Asia/Kolkata").dt.cast_time_unit("ms"), pl.col("volume").cast(pl.UInt64)
    )
    p = root / "candles" / "5m" / f"{symbol}.parquet"
    p.parent.mkdir(parents=True, exist_ok=True)
    df.write_parquet(p)


def test_history_levels_and_scan(tmp_path):
    write_5m(tmp_path, "AAA-EQ", last_vol=3000)
    write_5m(tmp_path, "BBB-EQ")
    h = history.history(tmp_path, "AAA-EQ", "1d", 5)
    assert len(h["bars"]) == 5
    pd = h["prior_day"]
    assert pd["high"] > pd["close"] > pd["low"]
    assert h["pivots"]["s1"] < h["pivots"]["p"] < h["pivots"]["r1"]
    intraday = history.history(tmp_path, "AAA-EQ", "5m", 1)
    assert len(intraday["bars"]) == 75
    s = history.scan(tmp_path, ["AAA-EQ", "BBB-EQ", "NOPE-EQ"])
    assert [r["symbol"] for r in s["results"]] == ["AAA-EQ", "BBB-EQ"]
    assert s["results"][0]["rel_volume"] == 3.0
    assert s["no_data"] == ["NOPE-EQ"]
    assert history.scan(tmp_path, ["AAA-EQ", "BBB-EQ"], min_rel_volume=2)["matched"] == 1
    assert "error" in history.history(tmp_path, "NOPE-EQ")


def test_news_filters_by_symbol_and_age():
    rss = b"""<?xml version="1.0"?><rss version="2.0"><channel><title>Mkt</title>
    <item><title>Reliance Q2 profit beats</title><pubDate>%s</pubDate></item>
    <item><title>TCS wins deal</title><pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate></item>
    </channel></rss>""" % time.strftime("%a, %d %b %Y %H:%M:%S GMT", time.gmtime()).encode()

    def get(url):
        if "bad" in url:
            raise OSError("down")
        return rss

    items = news.fetch(["https://ok", "https://bad"], get=get)
    r = news.select(items, ["RELIANCE-EQ", "TCS-EQ"], since_hours=24)
    assert [i["title"] for i in r["items"]] == ["Reliance Q2 profit beats"]
    assert r["failed_sources"] == ["https://bad"]


def test_store_reads_trades_in_rupees(tmp_path):
    db = account_db(tmp_path, "A1-23")
    assert db == tmp_path / "accounts" / "A123" / "paper" / "saarthi.db"
    db.parent.mkdir(parents=True)
    c = sqlite3.connect(db)
    c.executescript("""
        CREATE TABLE trades (id TEXT, day TEXT, token TEXT, state TEXT, body TEXT, updated_at TEXT);
        CREATE TABLE days (day TEXT, body TEXT);
        CREATE TABLE events (id INTEGER PRIMARY KEY, ts TEXT, kind TEXT, detail TEXT);
    """)
    t = {
        "id": "T1",
        "symbol": "RELIANCE-EQ",
        "strategy": "orb",
        "side": "buy",
        "state": "closed",
        "entry_qty": 10,
        "entry_value": 10 * 100_500,
        "exit_qty": 10,
        "exit_value": 10 * 101_000,
        "signal_px": 100_400,
        "stop": 99_500,
        "target": None,
        "planned_risk": 10_000,
        "charges": 4_000,
        "exit_reason": "square_off",
    }
    c.execute("INSERT INTO trades VALUES ('T1','2026-10-28','2885','closed',?, '')", (json.dumps(t),))
    c.execute("INSERT INTO days VALUES ('2026-10-28', ?)", (json.dumps({"date": "2026-10-28", "net": 1000}),))
    c.execute("INSERT INTO events (ts, kind, detail) VALUES ('2026-10-28T03:20:00Z','plan','{\"version\":1}')")
    c.commit()
    s = Store(db)
    [v] = s.trades(date(2026, 10, 28))
    assert (v["avg_entry"], v["avg_exit"], v["gross"], v["net"]) == (1005.0, 1010.0, 50.0, 10.0)
    assert v["entry_slippage_per_share"] == 1.0
    assert s.day(date(2026, 10, 28))["net"] == 10.0
    assert s.plan_events(date(2026, 10, 28))[0]["version"] == 1
