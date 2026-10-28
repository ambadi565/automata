use std::time::Duration;

/// Exponential reconnect backoff with ±20 % jitter, so a fleet of engines
/// does not reconnect in lockstep after an Angel outage.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    attempt: u32,
    rng: u64,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9e37_79b9, |d| d.as_nanos() as u64);
        Backoff {
            initial,
            max: max.max(initial),
            attempt: 0,
            rng: seed | 1,
        }
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The delay before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let base = self.initial.saturating_mul(1u32 << self.attempt.min(16)).min(self.max);
        self.attempt = self.attempt.saturating_add(1);
        // xorshift64: plenty for jitter.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let unit = (self.rng % 10_000) as f64 / 10_000.0; // [0, 1)
        base.mul_f64(0.8 + 0.4 * unit).min(self.max)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_caps_and_resets() {
        let ms = Duration::from_millis;
        let mut b = Backoff::new(ms(500), ms(30_000));
        let d: Vec<_> = (0..10).map(|_| b.next_delay()).collect();
        assert!(d[0] >= ms(400) && d[0] <= ms(600), "{:?}", d[0]);
        assert!(d[1] >= ms(800) && d[1] <= ms(1200));
        assert!(d.iter().all(|x| *x <= ms(30_000)));
        assert!(d[9] >= ms(24_000), "capped near max: {:?}", d[9]);
        b.reset();
        assert!(b.next_delay() <= ms(600));
    }
}
