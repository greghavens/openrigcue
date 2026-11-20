//! Time source for probes, so handshakes can be tested without waiting.

use std::time::{Duration, Instant};

pub trait Clock {
    fn now(&self) -> Duration;
    fn sleep(&mut self, d: Duration);
}

/// Wall clock.
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock { start: Instant::now() }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}
