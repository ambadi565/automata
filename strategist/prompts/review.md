Intraday review at {now} IST on {today}.

Check get_pnl, get_recent_signals, get_positions, get_market_context and
get_current_plan. Look for: a regime shift (trend → chop), breadth or VIX
moving sharply, repeated stop-outs on one strategy or symbol, a catalyst
in get_news.

The default is NO CHANGE. Only submit a plan when there is a clear reason,
and say it in `rationale`. Allowed changes: pause or resume a strategy, drop a
symbol, lower risk, switch a direction, or go `avoid`. Keep everything else as
in the current plan (copy it). Never add risk after two consecutive losses or
when more than half the daily loss limit is used (the engine also refuses).

If nothing needs changing, do not call submit_plan; reply with one line
starting "No change:" and the reason.

{state_note}
