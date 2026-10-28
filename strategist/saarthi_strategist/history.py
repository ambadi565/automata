"""History from the Parquet store (`data/candles/<tf>/<SYMBOL>.parquet`).

Prices are stored in paise; everything returned here is in rupees, rounded,
because that is what the model reasons in.
"""

from __future__ import annotations

import math
import tomllib
from pathlib import Path

import polars as pl

IST = "Asia/Kolkata"


def _path(data_dir: Path, tf: str, symbol: str) -> Path:
    stem = "".join(c if (c.isalnum() or c in "-_&") else "_" for c in symbol)
    return data_dir / "candles" / tf / f"{stem}.parquet"


def load_bars(data_dir: Path, symbol: str, tf: str = "5m") -> pl.DataFrame:
    p = _path(data_dir, tf, symbol)
    if not p.exists():
        return pl.DataFrame()
    df = pl.read_parquet(p)
    return df.with_columns(pl.col("ts").dt.convert_time_zone(IST)).sort("ts")


def daily(data_dir: Path, symbol: str) -> pl.DataFrame:
    """Daily bars: the `1d` file if present, else built from 5-minute bars."""
    d = load_bars(data_dir, symbol, "1d")
    if d.height:
        return d.with_columns(pl.col("ts").dt.date().alias("date"))
    m = load_bars(data_dir, symbol, "5m")
    if not m.height:
        return pl.DataFrame()
    return (
        m.with_columns(pl.col("ts").dt.date().alias("date"))
        .group_by("date", maintain_order=True)
        .agg(
            pl.col("open").first(),
            pl.col("high").max(),
            pl.col("low").min(),
            pl.col("close").last(),
            pl.col("volume").sum(),
        )
        .sort("date")
    )


def _rs(paise: float | int | None) -> float | None:
    if paise is None or (isinstance(paise, float) and math.isnan(paise)):
        return None
    return round(float(paise) / 100.0, 2)


def history(data_dir: Path, symbol: str, tf: str = "1d", days: int = 10) -> dict:
    """Recent bars plus prior-day levels and classic pivots."""
    if tf not in ("1d", "5m", "15m", "1h"):
        return {"error": f"tf must be 1d, 5m, 15m or 1h, not {tf}"}
    days = max(1, min(days, 60))
    d = daily(data_dir, symbol)
    if not d.height:
        return {"error": f"no history for {symbol} (run `fetch candles`)"}
    last = d.row(-1, named=True)
    h, low, c = last["high"], last["low"], last["close"]
    pivot = (h + low + c) / 3
    out = {
        "symbol": symbol,
        "prior_day": {
            "date": str(last["date"]),
            "open": _rs(last["open"]),
            "high": _rs(h),
            "low": _rs(low),
            "close": _rs(c),
            "volume": int(last["volume"]),
        },
        "pivots": {
            "p": _rs(pivot),
            "r1": _rs(2 * pivot - low),
            "s1": _rs(2 * pivot - h),
            "r2": _rs(pivot + (h - low)),
            "s2": _rs(pivot - (h - low)),
        },
    }
    if tf == "1d":
        rows = d.tail(days)
        out["bars"] = [
            {
                "date": str(r["date"]),
                "o": _rs(r["open"]),
                "h": _rs(r["high"]),
                "l": _rs(r["low"]),
                "c": _rs(r["close"]),
                "v": int(r["volume"]),
            }
            for r in rows.iter_rows(named=True)
        ]
    else:
        b = load_bars(data_dir, symbol, tf)
        if b.height:
            first_day = b.select(pl.col("ts").dt.date().unique().sort().tail(days).first()).item()
            b = b.filter(pl.col("ts").dt.date() >= first_day).tail(400)
            out["bars"] = [
                {
                    "ts": r["ts"].strftime("%Y-%m-%d %H:%M"),
                    "o": _rs(r["open"]),
                    "h": _rs(r["high"]),
                    "l": _rs(r["low"]),
                    "c": _rs(r["close"]),
                    "v": int(r["volume"]),
                }
                for r in b.iter_rows(named=True)
            ]
        else:
            out["bars"] = []
            out["note"] = f"no {tf} file; daily levels only"
    return out


def stats(d: pl.DataFrame) -> dict | None:
    """Scan metrics from daily bars (needs at least 15 days)."""
    if d.height < 15:
        return None
    d = d.with_columns(pl.col("high", "low", "close", "volume").cast(pl.Float64))
    prev_close = d["close"].shift(1)
    tr = d.select(
        pl.max_horizontal(
            pl.col("high") - pl.col("low"),
            (pl.col("high") - pl.col("close").shift(1)).abs(),
            (pl.col("low") - pl.col("close").shift(1)).abs(),
        )
    ).to_series()
    atr = tr.tail(14).mean()
    last = d.row(-1, named=True)
    close = last["close"]
    vol20 = d["volume"].head(d.height - 1).tail(20).mean()
    c5 = d["close"].tail(6).head(1).item()
    return {
        "close": _rs(close),
        "atr_pct": round(100 * atr / close, 2) if close else None,
        "range_pct": round(100 * (last["high"] - last["low"]) / close, 2) if close else None,
        "rel_volume": round(last["volume"] / vol20, 2) if vol20 else None,
        "ret_1d_pct": round(100 * (close / prev_close[-1] - 1), 2) if prev_close[-1] else None,
        "ret_5d_pct": round(100 * (close / c5 - 1), 2) if c5 else None,
        "avg_turnover_cr": round(float((d["close"] * d["volume"]).tail(20).mean()) / 100 / 1e7, 1),
        "pdh": _rs(last["high"]),
        "pdl": _rs(last["low"]),
        "date": str(last["date"]),
    }


def universe(path: Path) -> list[str]:
    return list(tomllib.loads(path.read_text()).get("symbols", []))


def scan(
    data_dir: Path,
    symbols: list[str],
    min_atr_pct: float = 0.0,
    max_atr_pct: float = 100.0,
    min_rel_volume: float = 0.0,
    min_turnover_cr: float = 0.0,
    sort_by: str = "rel_volume",
    limit: int = 25,
) -> dict:
    """Ranks the universe by `sort_by` after filters, from prior-day data."""
    rows, missing = [], []
    for s in symbols:
        st = stats(daily(data_dir, s))
        if st is None:
            missing.append(s)
            continue
        if not (min_atr_pct <= (st["atr_pct"] or 0) <= max_atr_pct):
            continue
        if (st["rel_volume"] or 0) < min_rel_volume or st["avg_turnover_cr"] < min_turnover_cr:
            continue
        rows.append({"symbol": s, **st})
    key = sort_by if sort_by in ("rel_volume", "atr_pct", "ret_1d_pct", "ret_5d_pct", "range_pct") else "rel_volume"
    rows.sort(key=lambda r: abs(r.get(key) or 0), reverse=True)
    return {
        "sorted_by": key,
        "results": rows[: max(1, min(limit, 100))],
        "matched": len(rows),
        "no_data": missing[:20],
        "no_data_count": len(missing),
        "note": "Prior-day data. Gaps are known only after 09:15: use get_bars then.",
    }
