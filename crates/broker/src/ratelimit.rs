use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::{Instant, sleep_until};

/// Evenly spaces calls: at most one per `interval`, in FIFO order.
///
/// Spacing (rather than bursting) keeps us clear of SmartAPI's per-second
/// caps, which are enforced over short windows.
#[derive(Debug)]
pub struct RateLimiter {
    interval: Duration,
    next: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub fn new(interval: Duration) -> Self {
        RateLimiter {
            interval,
            next: Mutex::new(None),
        }
    }

    /// `per_sec` must be positive; non-positive values fall back to 1/s.
    pub fn per_second(per_sec: f64) -> Self {
        let per_sec = if per_sec.is_finite() && per_sec > 0.0 {
            per_sec
        } else {
            1.0
        };
        Self::new(Duration::from_secs_f64(1.0 / per_sec))
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Waits for the next free slot.
    pub async fn acquire(&self) {
        let slot = {
            let mut next = self.next.lock().await;
            let now = Instant::now();
            let slot = next.map_or(now, |n| n.max(now));
            *next = Some(slot + self.interval);
            slot
        };
        sleep_until(slot).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn spaces_calls_evenly() {
        let rl = RateLimiter::per_second(2.0);
        let start = Instant::now();
        for _ in 0..5 {
            rl.acquire().await;
        }
        // First call is immediate, then 4 × 500 ms.
        assert_eq!(start.elapsed(), Duration::from_millis(2000));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_time_is_not_banked() {
        let rl = RateLimiter::per_second(1.0);
        rl.acquire().await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        let start = Instant::now();
        rl.acquire().await;
        rl.acquire().await;
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[test]
    fn bad_rate_falls_back() {
        assert_eq!(RateLimiter::per_second(0.0).interval(), Duration::from_secs(1));
        assert_eq!(RateLimiter::per_second(f64::NAN).interval(), Duration::from_secs(1));
    }
}
