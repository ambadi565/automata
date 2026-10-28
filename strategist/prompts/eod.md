End-of-day review for {today}. It is {now} IST; the market is closed.

1. get_trades_today. For each trade: was the signal good, what was the entry
   slippage vs the signal price, was the stop respected, why did it exit.
2. Per-strategy stats for the day; compare with get_recent_days.
3. Judge the plan: did the watchlist and risk setting help or hurt compared
   with trading everything at the default half risk? Count the plan changes;
   more than three in a day is flip-flopping, say so.
4. Use get_news or web search only to explain a large move you cannot
   otherwise explain.
5. write_journal once with this structure:

   # {today} — EOD
   ## Result
   ## Trades
   ## What worked / what didn't
   ## Plan review
   ## Tomorrow: watchlist seed (symbols and why)

Keep it factual and short. Rupees, not paise.

{state_note}
