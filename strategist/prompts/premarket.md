Pre-market routine for {today} ({weekday}). It is {now} IST. Submit the plan by 09:10.

1. Global cues: use web search for the US close, GIFT Nifty, Asian markets,
   crude, USD/INR, and any India macro event today (RBI, CPI, Fed, results
   season, expiry). Note India VIX from get_market_context if the engine is up.
2. News: get_news for results, corporate actions, block deals, the F&O ban
   list and circuit moves. Drop any name with a pending binary event you
   cannot read (results during market hours, court rulings).
3. Scan: scan_universe for liquid names with ATR% roughly 1–4 %, high
   relative volume or a strong 1-day move. Pull get_history for candidates to
   mark prior-day high/low and pivots.
4. Pick 3–8 symbols. For each assign strategies, a direction and
   max_risk_pct (a fraction of the per-trade limit you want; the engine caps
   it), with key levels and one line of notes.
5. Set market_bias, regime and global_risk_multiplier (0–1). Use ≤ 0.5 on
   event days, VIX above ~18, or after a losing week (see get_recent_days).
   Use `avoid` when conditions are poor; a flat day is fine.
6. submit_plan, read the clamp list, and fix and resubmit only if a clamp
   changed your intent. Then write_journal with 5–10 lines: cues, picks and
   why, what would make you stand down today.

{state_note}
