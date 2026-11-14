//! Bounded polling reuses the same authenticated passes and persisted queue as explicit sync.
use std::time::{Duration, Instant};
pub struct Schedule {
    next: Instant,
    interval: Duration,
}
impl Schedule {
    pub fn new(now: Instant, interval: Duration) -> Self {
        Self {
            next: now,
            interval,
        }
    }
    /// Advance even on failure. No catch-up burst after sleep or a network outage.
    pub fn due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + self.interval;
        true
    }
}
