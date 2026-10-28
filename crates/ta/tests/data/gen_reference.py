"""Regenerates reference.csv for the `ta` crate tests.

EMA, ATR and RSI come from TA-Lib (the de-facto reference implementation),
the volume z-score from pandas' rolling mean/std over the prior bars and the VWAP bands from a
direct numpy evaluation of the definition. The input series is a seeded
random walk so the file is reproducible:

    pip install TA-Lib pandas numpy
    python3 -I crates/ta/tests/data/gen_reference.py > crates/ta/tests/data/reference.csv
"""

import sys

import numpy as np
import pandas as pd
import talib

N = 400
rng = np.random.default_rng(20261027)

# Prices in paise on a 5-paise tick, like an NSE large cap around Rs 2800.
steps = rng.normal(0, 400, N).round()
close = np.round((280000 + np.cumsum(steps)) / 5) * 5
open_ = np.concatenate([[close[0]], close[:-1]])
spread_hi = np.abs(rng.normal(0, 300, N)).round() / 5 * 5
spread_lo = np.abs(rng.normal(0, 300, N)).round() / 5 * 5
high = np.maximum(open_, close) + spread_hi
low = np.minimum(open_, close) - spread_lo
volume = rng.integers(1_000, 250_000, N).astype(float)
volume[123] = 2_000_000  # a spike for the z-score

ema9 = talib.EMA(close, timeperiod=9)
ema21 = talib.EMA(close, timeperiod=21)
atr14 = talib.ATR(high, low, close, timeperiod=14)
rsi14 = talib.RSI(close, timeperiod=14)
# z-score of each bar's volume against the 20 bars before it (sample std).
prior = pd.Series(volume).shift(1)
volz20 = ((pd.Series(volume) - prior.rolling(20).mean()) / prior.rolling(20).std(ddof=1)).to_numpy()

# Cumulative (session) VWAP of the typical price with volume-weighted sigma.
tp = (high + low + close) / 3
cv = np.cumsum(volume)
vwap = np.cumsum(tp * volume) / cv
var = np.cumsum(tp * tp * volume) / cv - vwap * vwap
sigma = np.sqrt(np.maximum(var, 0))

def f(x):
    return "" if np.isnan(x) else repr(float(x))

w = sys.stdout
w.write("open,high,low,close,volume,ema9,ema21,atr14,rsi14,volz20,vwap,vwap_sigma\n")
for i in range(N):
    w.write(",".join([
        str(int(open_[i])), str(int(high[i])), str(int(low[i])), str(int(close[i])), str(int(volume[i])),
        f(ema9[i]), f(ema21[i]), f(atr14[i]), f(rsi14[i]), f(volz20[i]), f(vwap[i]), f(sigma[i]),
    ]) + "\n")
