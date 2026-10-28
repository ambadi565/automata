use std::collections::VecDeque;

/// How unusual a bar's volume is: `(v - mean) / std` over the `period` bars
/// **before** it, with the sample standard deviation.
///
/// Excluding the current bar keeps a spike from inflating its own baseline.
/// Volumes are integers, so the window sums are kept exactly in `u128` and
/// never drift however long the engine runs.
#[derive(Debug, Clone)]
pub struct VolumeZScore {
    period: usize,
    window: VecDeque<u64>,
    sum: u128,
    sum_sq: u128,
    value: Option<f64>,
}

impl VolumeZScore {
    pub fn new(period: usize) -> Self {
        assert!(period >= 2, "volume z-score needs at least 2 bars");
        VolumeZScore {
            period,
            window: VecDeque::with_capacity(period + 1),
            sum: 0,
            sum_sq: 0,
            value: None,
        }
    }

    pub fn update(&mut self, volume: u64) -> Option<f64> {
        self.value = (self.window.len() == self.period).then(|| self.score(volume)).flatten();
        self.window.push_back(volume);
        self.sum += u128::from(volume);
        self.sum_sq += u128::from(volume) * u128::from(volume);
        if self.window.len() > self.period
            && let Some(old) = self.window.pop_front()
        {
            self.sum -= u128::from(old);
            self.sum_sq -= u128::from(old) * u128::from(old);
        }
        self.value
    }

    /// The z-score `volume` would get against the current window, without
    /// updating anything. Lets a strategy judge a bar that is still forming.
    pub fn peek(&self, volume: u64) -> Option<f64> {
        (self.window.len() == self.period).then(|| self.score(volume)).flatten()
    }

    fn score(&self, volume: u64) -> Option<f64> {
        let n = self.period as u128;
        // n * sum_sq - sum^2 is exact and never negative (Cauchy-Schwarz).
        let num = n * self.sum_sq - self.sum * self.sum;
        if num == 0 {
            return None;
        }
        let var = num as f64 / (n * (n - 1)) as f64;
        let mean = self.sum as f64 / n as f64;
        Some((volume as f64 - mean) / var.sqrt())
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn reset(&mut self) {
        *self = VolumeZScore::new(self.period);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_against_prior_bars() {
        let mut z = VolumeZScore::new(3);
        assert_eq!(z.update(10), None);
        assert_eq!(z.update(20), None);
        assert_eq!(z.update(30), None);
        // prior: 10, 20, 30 -> mean 20, sample std 10
        assert_eq!(z.peek(40), Some(2.0));
        assert_eq!(z.update(40), Some(2.0));
        // prior: 20, 30, 40 -> mean 30, std 10
        assert_eq!(z.update(25), Some(-0.5));
    }

    #[test]
    fn constant_volume_has_no_score() {
        let mut z = VolumeZScore::new(3);
        for _ in 0..5 {
            z.update(100);
        }
        assert_eq!(z.value(), None);
    }
}
