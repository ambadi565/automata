"""Read-only view of the engine's SQLite store, for the EOD review (the engine
has usually exited by then, so its API is gone)."""

from __future__ import annotations

import json
import sqlite3
from datetime import date
from pathlib import Path


def account_db(data_dir: Path, client_code: str, mode: str = "paper") -> Path:
    safe = "".join(c for c in client_code if c.isascii() and c.isalnum()) or "default"
    return data_dir / "accounts" / safe / mode / "saarthi.db"


def _rs(p: int | None) -> float | None:
    return None if p is None else round(p / 100.0, 2)


def trade_view(t: dict) -> dict:
    eq, xq = t.get("entry_qty", 0), t.get("exit_qty", 0)
    avg_in = t["entry_value"] / eq if eq else None
    avg_out = t["exit_value"] / xq if xq else None
    sign = 1 if t.get("side") == "buy" else -1
    realized = sign * (t["exit_value"] - avg_in * xq) if (avg_in is not None and xq) else 0
    slip = None
    if avg_in is not None and t.get("signal_px"):
        slip = round(sign * (avg_in - t["signal_px"]) / 100.0, 2)
    return {
        "id": t["id"],
        "symbol": t["symbol"],
        "strategy": t["strategy"],
        "side": "long" if sign > 0 else "short",
        "state": t["state"],
        "qty": eq,
        "signal_px": _rs(t.get("signal_px")),
        "avg_entry": _rs(avg_in),
        "avg_exit": _rs(avg_out),
        "stop": _rs(t.get("stop")),
        "target": _rs(t.get("target")),
        "planned_risk": _rs(t.get("planned_risk")),
        "entry_slippage_per_share": slip,
        "gross": _rs(realized),
        "charges": _rs(t.get("charges")),
        "net": _rs(realized - t.get("charges", 0)),
        "reason": t.get("reason"),
        "exit_reason": t.get("exit_reason"),
        "opened_at": t.get("opened_at"),
        "closed_at": t.get("closed_at"),
    }


class Store:
    def __init__(self, path: Path):
        self.path = path

    def _conn(self) -> sqlite3.Connection:
        if not self.path.exists():
            raise FileNotFoundError(f"{self.path} does not exist yet (has the engine run?)")
        return sqlite3.connect(f"file:{self.path}?mode=ro", uri=True, timeout=5)

    def trades(self, day: date) -> list[dict]:
        with self._conn() as c:
            rows = c.execute("SELECT body FROM trades WHERE day = ? ORDER BY id", (day.isoformat(),))
            return [trade_view(json.loads(b)) for (b,) in rows]

    def day(self, day: date) -> dict | None:
        with self._conn() as c:
            r = c.execute("SELECT body FROM days WHERE day = ?", (day.isoformat(),)).fetchone()
        if not r:
            return None
        d = json.loads(r[0])
        for k in ("gross", "charges", "net", "worst_pnl"):
            if k in d:
                d[k] = _rs(d[k])
        return d

    def plan_events(self, day: date) -> list[dict]:
        with self._conn() as c:
            rows = c.execute(
                "SELECT ts, detail FROM events WHERE kind = 'plan' AND ts >= ? ORDER BY id",
                (day.isoformat(),),
            ).fetchall()
        return [{"ts": ts, **json.loads(d)} for ts, d in rows]

    def recent_days(self, n: int = 10) -> list[dict]:
        with self._conn() as c:
            rows = c.execute("SELECT body FROM days ORDER BY day DESC LIMIT ?", (n,)).fetchall()
        out = []
        for (b,) in rows:
            d = json.loads(b)
            out.append(
                {
                    k: (_rs(d[k]) if k in ("gross", "charges", "net", "worst_pnl") else d[k])
                    for k in ("date", "trades", "wins", "losses", "gross", "charges", "net", "worst_pnl", "halted")
                    if k in d
                }
            )
        return out
