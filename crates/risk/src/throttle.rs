//! Order-rate throttle: at most `n` broker order actions (place, modify,
//! cancel) in any rolling one-second window, to stay under the
//! exchange/broker algo threshold. Exits are delayed, never dropped.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

#[derive(Clone)]
pub struct OrderThrottle {
    inner: Arc<Mutex<VecDeque<Instant>>>,
    per_sec: usize,
}

const WINDOW: Duration = Duration::from_secs(1);

impl OrderThrottle {
    pub fn new(per_sec: u32) -> Self {
        OrderThrottle {
            inner: Arc::new(Mutex::new(VecDeque::new())),
            per_sec: per_sec.max(1) as usize,
        }
    }

    /// Waits until one more order action fits in the window, then takes it.
    pub async fn acquire(&self) {
        let mut q = self.inner.lock().await;
        loop {
            let now = Instant::now();
            while q.front().is_some_and(|&t| now.duration_since(t) >= WINDOW) {
                q.pop_front();
            }
            if q.len() < self.per_sec {
                q.push_back(now);
                return;
            }
            let wait = WINDOW - now.duration_since(q[0]);
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn breach_order_rate_never_exceeds_limit_in_any_window() {
        let t = OrderThrottle::new(5);
        let start = Instant::now();
        let mut stamps = Vec::new();
        for _ in 0..23 {
            t.acquire().await;
            stamps.push(Instant::now().duration_since(start));
        }
        for (i, &a) in stamps.iter().enumerate() {
            let in_window = stamps[i..].iter().take_while(|&&b| b - a < WINDOW).count();
            assert!(in_window <= 5, "{in_window} orders within 1 s starting at {a:?}");
        }
        // 23 orders at 5/s need at least 4 full seconds.
        assert!(*stamps.last().unwrap() >= Duration::from_secs(4));
    }
}
