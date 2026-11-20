//! The shared time base: `CLOCK_MONOTONIC`, which is system-wide, so the daemon, vrserver and games
//! (also inside Steam's pressure-vessel containers, which share the kernel) agree on it.

/// `CLOCK_MONOTONIC` now, in nanoseconds.
pub fn monotonic_now_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    // CLOCK_MONOTONIC is never negative.
    (t.tv_sec as u64).wrapping_mul(1_000_000_000).wrapping_add(t.tv_nsec as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn monotonic_and_matches_std_instant_rate() {
        let a = super::monotonic_now_ns();
        let i = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = super::monotonic_now_ns();
        let elapsed = i.elapsed().as_nanos() as u64;
        assert!(b > a);
        assert!((b - a).abs_diff(elapsed) < 2_000_000, "{} vs {elapsed}", b - a);
    }
}
