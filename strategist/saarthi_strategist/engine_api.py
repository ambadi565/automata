"""Client for the engine's control API (localhost, bearer token).

There is deliberately nothing here that can place or cancel an order: the API
has no such endpoint.
"""

from __future__ import annotations

from typing import Any

import httpx


class EngineError(Exception):
    def __init__(self, status: int, message: str):
        super().__init__(f"engine said {status}: {message}")
        self.status = status
        self.message = message


class EngineApi:
    def __init__(self, base_url: str, token: str, timeout: float = 15.0, transport: Any = None):
        self._c = httpx.Client(
            base_url=base_url,
            headers={"Authorization": f"Bearer {token}"},
            timeout=timeout,
            transport=transport,
        )

    def _do(self, method: str, path: str, **kw: Any) -> Any:
        try:
            r = self._c.request(method, path, **kw)
        except httpx.HTTPError as e:
            raise EngineError(503, f"engine unreachable: {e}") from e
        try:
            body = r.json()
        except ValueError:
            body = {"error": r.text[:500]}
        if r.status_code != 200:
            raise EngineError(r.status_code, str(body.get("error", body)))
        return body

    def health(self) -> bool:
        try:
            return bool(self._do("GET", "/health").get("ok"))
        except EngineError:
            return False

    def state(self) -> dict:
        return self._do("GET", "/state")

    def positions(self) -> dict:
        return self._do("GET", "/positions")

    def pnl(self) -> dict:
        return self._do("GET", "/pnl")

    def bars(self, symbol: str, n: int = 30) -> dict:
        return self._do("GET", "/bars", params={"symbol": symbol, "n": n})

    def signals(self, n: int = 20) -> dict:
        return self._do("GET", "/signals", params={"n": n})

    def context(self) -> dict:
        return self._do("GET", "/context")

    def plan(self) -> dict:
        return self._do("GET", "/plan")

    def submit_plan(self, plan: dict) -> dict:
        import json

        return self._do("POST", "/plan", content=json.dumps(plan), headers={"Content-Type": "application/json"})

    def halt(self, reason: str) -> dict:
        return self._do("POST", "/halt", json={"reason": reason})
