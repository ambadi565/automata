"""Plan checks on the strategist's side, before the engine sees it.

The engine is the authority (it validates and clamps again); checking here
first lets the model fix a malformed plan in the same run. `version` and a
missing `valid_until` are filled in by code, not left to the model.
"""

from __future__ import annotations

import json
from datetime import date, datetime, time
from pathlib import Path
from zoneinfo import ZoneInfo

from jsonschema import Draft202012Validator

IST = ZoneInfo("Asia/Kolkata")
SQUARE_OFF = time(15, 10)


def load_schema(path: Path) -> dict:
    return json.loads(path.read_text())


def validator(schema: dict) -> Draft202012Validator:
    return Draft202012Validator(schema, format_checker=Draft202012Validator.FORMAT_CHECKER)


def input_schema(schema: dict) -> dict:
    """The plan schema as a tool input, minus the fields code fills in."""
    s = json.loads(json.dumps(schema))
    for k in ("$schema", "$id", "title"):
        s.pop(k, None)
    s["properties"].pop("version")
    s["required"] = [r for r in s["required"] if r not in ("version", "valid_until")]
    return s


def prepare(plan: dict, current_version: int, today: date) -> dict:
    """Sets the version and defaults `valid_until` to today's square-off."""
    p = dict(plan)
    p["version"] = current_version + 1
    if not p.get("valid_until"):
        p["valid_until"] = datetime.combine(today, SQUARE_OFF, IST).isoformat()
    return p


def errors(v: Draft202012Validator, plan: dict) -> list[str]:
    out = []
    for e in sorted(v.iter_errors(plan), key=lambda e: list(e.path)):
        where = "/".join(str(x) for x in e.path) or "(plan)"
        out.append(f"{where}: {e.message}")
    return out[:20]
