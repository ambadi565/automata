"""Plan schema and preparation round-trips."""

import json
from datetime import date

from conftest import REPO

from saarthi_strategist import planio


def good_plan() -> dict:
    return {
        "market_bias": "bullish",
        "regime": "trend",
        "global_risk_multiplier": 0.6,
        "watchlist": [
            {
                "symbol": "RELIANCE-EQ",
                "strategies": ["orb"],
                "direction": "long_only",
                "max_risk_pct": 0.4,
                "key_levels": {"support": [1410.5], "resistance": [1452.0]},
                "notes": "Results beat; gap up expected.",
            }
        ],
        "paused_strategies": ["vwap_revert"],
        "rationale": "GIFT Nifty +0.6%, VIX 13: trend day likely.",
    }


def test_prepared_plan_validates(schema):
    p = planio.prepare(good_plan(), current_version=3, today=date(2026, 10, 28))
    assert p["version"] == 4
    assert p["valid_until"] == "2026-10-28T15:10:00+05:30"
    assert planio.errors(planio.validator(schema), p) == []


def test_default_plan_shape_is_close_to_schema(schema):
    # The engine's fallback has no valid_until (it is per day) and version 0;
    # everything else must already satisfy the schema.
    d = json.loads((REPO / "config" / "plan.default.json").read_text())
    p = planio.prepare(d, current_version=0, today=date(2026, 10, 28))
    assert planio.errors(planio.validator(schema), p) == []


def test_garbage_is_reported(schema):
    v = planio.validator(schema)
    bad = planio.prepare(
        {
            "market_bias": "yolo",
            "regime": "trend",
            "global_risk_multiplier": 4,
            "watchlist": [{"symbol": "reliance", "strategies": ["martingale"], "direction": "both", "max_risk_pct": 9}],
            "paused_strategies": [],
            "rationale": "",
            "place_order": True,
        },
        0,
        date(2026, 10, 28),
    )
    errs = "\n".join(planio.errors(v, bad))
    for needle in [
        "market_bias",
        "global_risk_multiplier",
        "watchlist/0/symbol",
        "martingale",
        "max_risk_pct",
        "rationale",
        "place_order",
    ]:
        assert needle in errs, (needle, errs)


def test_model_cannot_set_the_version(schema):
    p = good_plan() | {"version": 999}
    assert planio.prepare(p, 2, date(2026, 10, 28))["version"] == 3


def test_tool_input_schema_hides_code_filled_fields(schema):
    s = planio.input_schema(schema)
    assert "version" not in s["properties"]
    assert "valid_until" not in s["required"]
    assert s["additionalProperties"] is False
