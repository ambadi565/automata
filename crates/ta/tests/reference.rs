//! Compares every indicator with reference values from TA-Lib, pandas and
//! numpy. See `data/gen_reference.py` for how `reference.csv` was produced.

use saarthi_ta::{Atr, Ema, Hlc, Rsi, VolumeZScore, VwapBands};

struct Row {
    high: f64,
    low: f64,
    close: f64,
    volume: u64,
    expected: [Option<f64>; 7],
}

fn rows() -> Vec<Row> {
    let csv = include_str!("data/reference.csv");
    csv.lines()
        .skip(1)
        .map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            assert_eq!(f.len(), 12, "{line}");
            let num = |s: &str| s.parse::<f64>().unwrap();
            let opt = |s: &str| (!s.is_empty()).then(|| num(s));
            Row {
                high: num(f[1]),
                low: num(f[2]),
                close: num(f[3]),
                volume: f[4].parse().unwrap(),
                expected: std::array::from_fn(|i| opt(f[5 + i])),
            }
        })
        .collect()
}

fn check(name: &str, i: usize, got: Option<f64>, want: Option<f64>) {
    match (got, want) {
        (None, None) => {}
        (Some(g), Some(w)) => {
            // Relative 1e-9 or 1e-6 absolute (paise), whichever is looser.
            let tol = (w.abs() * 1e-9).max(1e-6);
            assert!((g - w).abs() <= tol, "{name} row {i}: got {g}, want {w}");
        }
        _ => panic!("{name} row {i}: got {got:?}, want {want:?}"),
    }
}

#[test]
fn indicators_match_reference_values() {
    let rows = rows();
    assert_eq!(rows.len(), 400);
    let (mut ema9, mut ema21, mut atr, mut rsi) = (Ema::new(9), Ema::new(21), Atr::new(14), Rsi::new(14));
    let mut volz = VolumeZScore::new(20);
    let mut vwap = VwapBands::new();
    let mut checked = 0;
    for (i, r) in rows.iter().enumerate() {
        let hlc = Hlc {
            high: r.high,
            low: r.low,
            close: r.close,
        };
        vwap.update((r.high + r.low + r.close) / 3.0, r.volume as f64);
        let got = [
            ema9.update(r.close),
            ema21.update(r.close),
            atr.update(hlc),
            rsi.update(r.close),
            volz.update(r.volume),
            vwap.vwap(),
            vwap.sigma(),
        ];
        let names = ["ema9", "ema21", "atr14", "rsi14", "volz20", "vwap", "vwap_sigma"];
        for k in 0..7 {
            check(names[k], i, got[k], r.expected[k]);
            checked += usize::from(r.expected[k].is_some());
        }
    }
    assert!(checked > 2500, "only {checked} values compared");
}

#[test]
fn volume_spike_stands_out() {
    let rows = rows();
    let mut volz = VolumeZScore::new(20);
    let z: Vec<_> = rows.iter().map(|r| volz.update(r.volume)).collect();
    assert!(z[123].unwrap() > 10.0);
}
