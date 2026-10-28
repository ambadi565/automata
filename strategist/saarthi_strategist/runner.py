"""The agent loop: one routine, one conversation, tools run here.

A failure anywhere (API error, refusal, timeout, budget) ends the run without
touching the engine, which keeps the plan in force or the conservative
default. That is the fallback.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any
from zoneinfo import ZoneInfo

import anthropic

from . import tools
from .budget import Budget
from .tools import Session

IST = ZoneInfo("Asia/Kolkata")
PROMPTS = Path(__file__).resolve().parents[1] / "prompts"
FALLBACK_BETA = "server-side-fallback-2026-07-01"


@dataclass
class Outcome:
    mode: str
    ok: bool
    reason: str
    text: str = ""
    tokens: int = 0
    turns: int = 0
    accepted: list[dict] = field(default_factory=list)
    plans: list[dict] = field(default_factory=list)
    plan_errors: list[str] = field(default_factory=list)


def prompt(mode: str, now: datetime, state_note: str = "") -> str:
    t = (PROMPTS / f"{mode}.md").read_text()
    return t.format(
        today=now.date().isoformat(),
        weekday=now.strftime("%A"),
        now=now.strftime("%H:%M"),
        state_note=state_note,
    ).strip()


def system_prompt() -> str:
    return (PROMPTS / "system.md").read_text().strip()


def _usage_tokens(u: Any) -> int:
    if u is None:
        return 0
    total = 0
    for k in ("input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"):
        total += int(getattr(u, k, 0) or 0)
    return total


def _block_dict(b: Any) -> Any:
    return b.model_dump(exclude_none=True) if hasattr(b, "model_dump") else b


def run(
    session: Session,
    budget: Budget,
    create: Any,
    now: datetime | None = None,
    state_note: str = "",
) -> Outcome:
    """Runs `session.mode`. `create(**params)` sends one Messages request."""
    mode = session.mode
    ms = session.settings.modes[mode]
    now = now or datetime.now(IST)
    if budget.exhausted():
        return Outcome(mode, False, f"daily token budget used ({budget.used()} ≥ {budget.limit}); skipped")

    deadline = time.monotonic() + ms.timeout_secs
    messages: list[dict] = [{"role": "user", "content": prompt(mode, now, state_note)}]
    params: dict[str, Any] = {
        "model": ms.model,
        "max_tokens": 16_000,
        "system": [{"type": "text", "text": system_prompt(), "cache_control": {"type": "ephemeral"}}],
        "tools": tools.definitions(session),
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": ms.effort},
    }
    if session.settings.server_fallbacks:
        params["betas"] = [FALLBACK_BETA]
        params["fallbacks"] = "default"

    spent, turns, text = 0, 0, ""
    out = Outcome(mode, False, "")

    def finish(ok: bool, reason: str) -> Outcome:
        budget.add(mode, spent, reason)
        out.ok, out.reason, out.text, out.tokens, out.turns = ok, reason, text, spent, turns
        out.accepted, out.plans, out.plan_errors = session.accepted, session.plans, session.plan_errors
        return out

    while True:
        if turns >= ms.max_turns:
            return finish(False, f"stopped after {turns} turns")
        left = deadline - time.monotonic()
        if left <= 1:
            return finish(False, f"timed out after {ms.timeout_secs}s")
        if spent >= budget.remaining():
            return finish(False, "daily token budget reached mid-run")
        turns += 1
        try:
            resp = create(timeout=left, messages=messages, **params)
        except anthropic.APIStatusError as e:
            return finish(False, f"API error {e.status_code}: {str(e)[:300]}")
        except anthropic.APIConnectionError as e:
            return finish(False, f"API unreachable: {e}")
        spent += _usage_tokens(getattr(resp, "usage", None))
        content = list(resp.content)
        text = "\n".join(b.text for b in content if getattr(b, "type", "") == "text").strip() or text
        stop = resp.stop_reason

        if stop == "refusal":
            return finish(False, "the model declined this request")
        if stop == "pause_turn":
            messages.append({"role": "assistant", "content": [_block_dict(b) for b in content]})
            continue
        if stop == "tool_use":
            messages.append({"role": "assistant", "content": [_block_dict(b) for b in content]})
            results = []
            for b in content:
                if getattr(b, "type", "") != "tool_use":
                    continue
                body, is_error = tools.call(session, b.name, dict(b.input or {}))
                r = {"type": "tool_result", "tool_use_id": b.id, "content": body}
                if is_error:
                    r["is_error"] = True
                results.append(r)
            messages.append({"role": "user", "content": results})
            continue
        if stop == "max_tokens":
            return finish(False, "response hit max_tokens")
        # end_turn / stop_sequence
        if mode in ("premarket",) and not session.accepted:
            return finish(False, "no plan was accepted; the engine keeps the plan in force")
        return finish(True, "done")
