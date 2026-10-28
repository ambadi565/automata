"""Daily token budget (AGENT.md §2). Over budget, routines are skipped and the
engine keeps trading on the plan it has."""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from datetime import date
from pathlib import Path


@dataclass
class Budget:
    path: Path
    limit: int

    @classmethod
    def for_day(cls, state_dir: Path, day: date, limit: int) -> Budget:
        return cls(state_dir / f"usage-{day.isoformat()}.json", limit)

    def _read(self) -> dict:
        try:
            return json.loads(self.path.read_text())
        except (FileNotFoundError, ValueError):
            return {"tokens": 0, "runs": []}

    def used(self) -> int:
        return int(self._read().get("tokens", 0))

    def remaining(self) -> int:
        return max(0, self.limit - self.used())

    def exhausted(self) -> bool:
        return self.used() >= self.limit

    def add(self, mode: str, tokens: int, outcome: str) -> None:
        d = self._read()
        d["tokens"] = int(d.get("tokens", 0)) + int(tokens)
        d.setdefault("runs", []).append({"mode": mode, "tokens": int(tokens), "outcome": outcome})
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".tmp")
        tmp.write_text(json.dumps(d, indent=1))
        os.replace(tmp, self.path)
