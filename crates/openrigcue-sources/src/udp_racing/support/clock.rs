//! Time source for listeners that measure the interval between reads (Gran Turismo accelerations).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A timestamp. Gran Turismo reads wall time, so clocks may move backwards.
pub trait Clock: Send {
    fn now(&self) -> Duration;
}

/// Wall time, rather than a monotonic elapsed timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

fn wall_timestamp(now: SystemTime) -> Duration {
    // Whole seconds from year 1 to the Unix epoch; the origin does not affect the sub-second part of an interval.
    const EPOCH_OFFSET: Duration = Duration::from_secs(62_135_596_800);
    match now.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => EPOCH_OFFSET.saturating_add(elapsed),
        Err(before_epoch) => EPOCH_OFFSET.saturating_sub(before_epoch.duration()),
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        wall_timestamp(SystemTime::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_timestamp_preserves_microseconds_across_the_unix_epoch() {
        let before = wall_timestamp(UNIX_EPOCH - Duration::from_micros(1));
        let after = wall_timestamp(UNIX_EPOCH + Duration::from_micros(1));
        assert_eq!(before.subsec_micros(), 999_999);
        assert_eq!(after.subsec_micros(), 1);
        assert_eq!(after - before, Duration::from_micros(2));
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A clock frozen at the time tests set.
    #[derive(Debug, Clone, Default)]
    pub struct FakeClock(pub Arc<Mutex<Duration>>);

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            *self.0.lock().unwrap()
        }
    }
}
