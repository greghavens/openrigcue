//! Time-driven smoothing states (short-lived timed sequences), modelled as state
//! evaluated against a caller-supplied clock.

use std::time::{Duration, Instant};

use crate::error::CueError;
use crate::pyvalue::{Cmp, Op, PyValue};

/// The longest sleep in whole seconds: a sleep duration is held as signed 64-bit nanoseconds.
pub(crate) const MAX_SLEEP_SECONDS: u64 = (i64::MAX / 1_000_000_000) as u64;

/// Soft start/stop: all axes use the manufacturer's soft start window for a while after motion starts
/// or stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SoftStart {
    #[default]
    Idle,
    /// Engaged until the given instant.
    Until(Instant),
    /// Engaged for good: the timed sequence failed before clearing the flag.
    Latched,
}

impl SoftStart {
    pub fn engaged(&self, now: Instant) -> bool {
        match *self {
            SoftStart::Idle => false,
            SoftStart::Until(end) => now < end,
            SoftStart::Latched => true,
        }
    }

    /// Engages soft start for the manufacturer's `soft_start_stop_timer` seconds. `timer` is that
    /// integer read; where it fails (a missing key, a malformed value) or the duration is negative or
    /// too long, soft start stays engaged for good.
    #[cfg(test)]
    pub fn engage(&mut self, now: Instant, timer: Result<i128, CueError>) {
        self.engage_value(now, timer.map(PyValue::Int));
    }

    pub fn engage_value(&mut self, now: Instant, timer: Result<PyValue, CueError>) {
        *self = match timer
            .ok()
            .and_then(|t| t.to_native().integer()?.try_into().ok())
            .filter(|seconds| *seconds <= MAX_SLEEP_SECONDS)
            .and_then(|t| now.checked_add(Duration::from_secs(t)))
        {
            Some(end) => SoftStart::Until(end),
            None => SoftStart::Latched,
        };
    }
}

/// Whether motion just started or stopped, which (re)starts soft start/stop.
///
/// The trigger test of the stream path: from still (`0`) to moving or back, while
/// soft start is not engaged. Before the first frame the previous movement is unknown, which counts as
/// moving.
#[cfg(test)]
pub(crate) fn motion_toggled(previous: Option<f64>, movement: f64) -> bool {
    let was_still = previous == Some(0.0);
    let is_still = movement == 0.0;
    (was_still && !is_still) || (!was_still && is_still)
}

/// 20 checks of 0.04 s at the full factor.
const CHECK: Duration = Duration::from_millis(40);
const CHECKS: u32 = 20;
/// One ramp step.
const STEP: Duration = Duration::from_millis(50);
const RAMP_STEPS: u32 = 20;

/// The schedule of a running gear smoothing sequence: the next step it runs after a sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Schedule {
    start: Instant,
    /// Sleeps completed so far: 0..20 plateau checks, then 20..40 ramp steps, then the end.
    done: u32,
}

impl Schedule {
    /// When the step after sleep number `done` runs.
    fn next_at(&self) -> Instant {
        if self.done < CHECKS {
            self.start + CHECK * self.done
        } else {
            self.start + CHECK * CHECKS + STEP * (self.done - CHECKS)
        }
    }
}

/// Legacy gear smoothing (`enable_legacy_gear_smoothing`): the status, the value, the stop flag
/// and the timed sequence that writes them.
///
/// After a gear change the surge smoothing window is inflated by `1 + value / 50` while the status is
/// set: the full gear smoothing factor for 0.8 s, then 20 ramp steps of 0.05 s, then the status is
/// cleared. A sequence started while the status is set sees the stop flag at once: it writes the full
/// factor, clears the flag and ends, and the running schedule continues. A sequence that fails (an
/// unreadable `gear_smoothing_factor`) leaves the status set.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GearSmoothing {
    status: bool,
    value: PyValue,
    stop: bool,
    running: Option<Schedule>,
}

impl Default for GearSmoothing {
    fn default() -> Self {
        Self { status: false, value: PyValue::Int(0), stop: false, running: None }
    }
}

impl GearSmoothing {
    /// Runs the sequence's steps due by `now`. `factor` reads the tune's integer `gear_smoothing_factor`
    /// for a ramp step.
    fn advance(&mut self, now: Instant, factor: &dyn Fn() -> Result<PyValue, CueError>) {
        while let Some(mut schedule) = self.running {
            let at = schedule.next_at();
            if at > now {
                return;
            }
            if schedule.done < CHECKS {
                // The stop check of a plateau iteration.
                if self.stop {
                    self.stop = false;
                    self.running = None;
                    return;
                }
            } else if schedule.done < CHECKS + RAMP_STEPS {
                // A ramp step: write the value, then check the stop flag.
                let i = i128::from(CHECKS + RAMP_STEPS - schedule.done);
                match factor() {
                    Ok(f) => match f
                        .binop(Op::Div, &PyValue::Float(20.0))
                        .and_then(|v| v.binop(Op::Mul, &PyValue::Int(i)))
                        .and_then(|v| v.to_integer_value())
                    {
                        Ok(value) => self.value = value,
                        Err(_) => {
                            self.running = None;
                            return;
                        }
                    },
                    Err(_) => {
                        self.running = None;
                        return;
                    }
                }
                if self.stop {
                    self.stop = false;
                    self.running = None;
                    return;
                }
            } else {
                // The end of the ramp.
                self.status = false;
                self.running = None;
                return;
            }
            schedule.done += 1;
            self.running = Some(schedule);
        }
    }

    /// A gear change: starts a new gear smoothing sequence and runs it up to its first sleep
    /// (the status is set, the value becomes `start_factor`, then the first stop check).
    #[cfg(test)]
    pub fn on_gear_change(
        &mut self,
        now: Instant,
        start_factor: Result<i128, CueError>,
        factor: &dyn Fn() -> Result<i128, CueError>,
    ) {
        self.on_gear_change_value(now, start_factor.map(PyValue::Int), &|| factor().map(PyValue::Int));
    }

    pub fn on_gear_change_value(
        &mut self,
        now: Instant,
        start_factor: Result<PyValue, CueError>,
        factor: &dyn Fn() -> Result<PyValue, CueError>,
    ) {
        self.advance(now, factor);
        self.stop = self.status;
        self.status = true;
        let Ok(full) = start_factor else {
            return;
        };
        self.value = full;
        if self.stop {
            self.stop = false;
            return;
        }
        self.running = Some(Schedule { start: now, done: 1 });
    }

    /// The gear smoothing value while the status is set, at `now`.
    #[cfg(test)]
    pub fn value(&mut self, now: Instant, factor: &dyn Fn() -> Result<i128, CueError>) -> Option<i128> {
        self.native_value(now, &|| factor().map(PyValue::Int)).and_then(|value| value.to_int().ok())
    }
    pub fn native_value(&mut self, now: Instant, factor: &dyn Fn() -> Result<PyValue, CueError>) -> Option<PyValue> {
        self.advance(now, factor);
        self.status.then(|| self.value.clone())
    }
}

/// The default (non-legacy) longitudinal follower of the stream path: a step of the
/// longitudinal acceleration larger than the integer `gear_smoothing_min_delta` is cut to `(100 − factor)`
/// percent, in racing games. `factor` reads the tune's integer `gear_smoothing_factor`, only when
/// the step is cut.
#[cfg(test)]
pub(crate) fn follow_longitudinal(
    previous: f64,
    current: f64,
    min_delta: i128,
    factor: impl FnOnce() -> Result<i128, CueError>,
    racing: bool,
) -> Result<f64, CueError> {
    follow_longitudinal_native(
        &PyValue::Float(previous),
        &PyValue::Float(current),
        || Ok(PyValue::Int(min_delta)),
        || factor().map(PyValue::Int),
        racing,
    )?
    .as_double()
}

pub(crate) fn follow_longitudinal_native(
    previous: &PyValue,
    current: &PyValue,
    min_delta: impl FnOnce() -> Result<PyValue, CueError>,
    factor: impl FnOnce() -> Result<PyValue, CueError>,
    racing: bool,
) -> Result<PyValue, CueError> {
    let step = current
        .binop(Op::Sub, previous)
        .and_then(|v| v.abs())
        .map_err(|e| e.at(crate::error::mh("send_stream", 805)))?;
    let min_delta = min_delta()?;
    if !(step.compare(Cmp::Gt, &min_delta).map_err(|e| e.at(crate::error::mh("send_stream", 808)))? && racing) {
        return Ok(current.clone());
    }
    let factor = factor()?;
    let kept = step
        .binop(Op::Mul, &PyValue::Int(100).binop(Op::Sub, &factor)?)
        .and_then(|v| v.binop(Op::Div, &PyValue::Int(100)))
        .map_err(|e| e.at(crate::error::mh("send_stream", 813)))?;
    let direction = if previous.compare(Cmp::Gt, current).map_err(|e| e.at(crate::error::mh("send_stream", 818)))? {
        -1
    } else {
        1
    };
    previous
        .binop(Op::Add, &kept.binop(Op::Mul, &PyValue::Int(direction))?)
        .map_err(|e| e.at(crate::error::mh("send_stream", 822)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn f(v: i128) -> impl Fn() -> Result<i128, CueError> {
        move || Ok(v)
    }

    #[test]
    fn enable_force_smoothing_timer() {
        let t0 = Instant::now();
        let mut s = SoftStart::default();
        assert!(!s.engaged(t0));
        s.engage(t0, Ok(4));
        assert!(s.engaged(at(t0, 3999)));
        assert!(!s.engaged(at(t0, 4000)));
        // A timer that cannot be read, or a negative one: soft start stays engaged.
        s.engage(t0, Err(CueError::key("soft_start_stop_timer")));
        assert!(s.engaged(at(t0, 1_000_000)));
        s.engage(t0, Ok(-1));
        assert_eq!(s, SoftStart::Latched);
        s.engage(t0, Ok(0));
        assert!(!s.engaged(t0));
        s.engage(t0, Ok(i128::from(MAX_SLEEP_SECONDS)));
        assert!(matches!(s, SoftStart::Until(_)));
        s.engage(t0, Ok(i128::from(MAX_SLEEP_SECONDS) + 1));
        assert_eq!(s, SoftStart::Latched);
    }

    #[test]
    fn send_stream_soft_start_trigger() {
        assert!(motion_toggled(None, 0.0));
        assert!(!motion_toggled(None, 1.5));
        assert!(motion_toggled(Some(0.0), 0.2));
        assert!(motion_toggled(Some(0.2), 0.0));
        assert!(!motion_toggled(Some(0.2), 0.3));
        assert!(!motion_toggled(Some(0.0), 0.0));
        assert!(motion_toggled(Some(-0.0), 1.0));
    }

    #[test]
    fn enable_gear_smoothing_schedule() {
        let t0 = Instant::now();
        let mut g = GearSmoothing::default();
        assert_eq!(g.value(t0, &f(95)), None);
        g.on_gear_change(t0, Ok(95), &f(95));
        assert_eq!(g.value(t0, &f(95)), Some(95));
        assert_eq!(g.value(at(t0, 799), &f(95)), Some(95));
        // Ramp: i = 20 at 0.80 s -> 95 / 20 · 20 = 95, i = 19 at 0.85 s -> 90.25 truncated to 90.
        assert_eq!(g.value(at(t0, 800), &f(95)), Some(95));
        assert_eq!(g.value(at(t0, 850), &f(95)), Some(90));
        // i = 1 at 1.75 s -> 4.75 truncated to 4; ends at 1.80 s.
        assert_eq!(g.value(at(t0, 1799), &f(95)), Some(4));
        assert_eq!(g.value(at(t0, 1800), &f(95)), None);
    }

    #[test]
    fn enable_gear_smoothing_gear_change_while_running() {
        let t0 = Instant::now();
        let mut g = GearSmoothing::default();
        g.on_gear_change(t0, Ok(95), &f(95));
        // The new sequence writes the full factor and ends on the stop flag; the running schedule goes on.
        g.on_gear_change(at(t0, 1010), Ok(95), &f(95));
        assert_eq!(g.value(at(t0, 1020), &f(95)), Some(95));
        assert_eq!(g.value(at(t0, 1050), &f(95)), Some((95.0f64 / 20.0 * 15.0).trunc() as i128));
        assert_eq!(g.value(at(t0, 1800), &f(95)), None);
        g.on_gear_change(at(t0, 2000), Ok(95), &f(95));
        assert_eq!(g.value(at(t0, 2700), &f(95)), Some(95));
    }

    #[test]
    fn enable_gear_smoothing_keeps_the_value_it_wrote() {
        let t0 = Instant::now();
        let mut g = GearSmoothing::default();
        g.on_gear_change(t0, Ok(95), &f(95));
        assert_eq!(g.value(at(t0, 300), &f(50)), Some(95));
        g.on_gear_change(at(t0, 400), Ok(60), &f(50));
        assert_eq!(g.value(at(t0, 500), &f(50)), Some(60));
        assert_eq!(g.value(at(t0, 860), &f(50)), Some((50.0f64 / 20.0 * 19.0).trunc() as i128));
    }

    #[test]
    fn enable_gear_smoothing_thread_death_leaves_the_status_set() {
        let t0 = Instant::now();
        let bad = || Err(CueError::key("gear_smoothing_factor"));
        // An unreadable factor: the status stays set with the old value (0) for good.
        let mut g = GearSmoothing::default();
        g.on_gear_change(t0, Err(CueError::key("gear_smoothing_factor")), &bad);
        assert_eq!(g.value(at(t0, 60_000), &bad), Some(0));
        // A later change writes the factor and exits at once (the status is set, so the stop flag is set).
        g.on_gear_change(at(t0, 60_000), Ok(80), &f(80));
        assert_eq!(g.value(at(t0, 120_000), &f(80)), Some(80));
        // A ramp step that cannot read the factor ends the sequence with the last value written.
        let mut g = GearSmoothing::default();
        g.on_gear_change(t0, Ok(95), &f(95));
        assert_eq!(g.value(at(t0, 900), &f(95)), Some((95.0f64 / 20.0 * 18.0).trunc() as i128));
        assert_eq!(g.value(at(t0, 5000), &bad), Some((95.0f64 / 20.0 * 18.0).trunc() as i128));
    }

    #[test]
    fn send_stream_longitudinal_follower() {
        let factor = || Ok(95);
        assert_eq!(follow_longitudinal(0.0, 2.0, 1, factor, true), Ok(0.1));
        assert_eq!(follow_longitudinal(2.0, 0.0, 1, factor, true), Ok(1.9));
        assert_eq!(follow_longitudinal(0.0, 1.0, 1, factor, true), Ok(1.0));
        assert_eq!(follow_longitudinal(0.0, 2.0, 1, factor, false), Ok(2.0));
        // The factor is read only when the step is cut.
        let missing = || Err(CueError::key("gear_smoothing_factor"));
        assert_eq!(follow_longitudinal(0.0, 0.5, 1, missing, true), Ok(0.5));
        assert!(follow_longitudinal(0.0, 2.0, 1, missing, true).is_err());
    }

    #[test]
    fn longitudinal_follower_subtracts_integer_factor_before_float_conversion() {
        // The step is multiplied by the integer (100 - factor). At 2^53 + 1,
        // converting factor first loses one bit which the subtraction retains.
        let result = follow_longitudinal(0.0, 2.0, 1, || Ok(9_007_199_254_740_993), true).unwrap();
        assert_eq!(result.to_bits(), 0xc2e4_7ae1_47ae_143c);
        let reverse = follow_longitudinal(0.0, -2.0, 1, || Ok(9_007_199_254_740_993), true).unwrap();
        assert_eq!(reverse.to_bits(), 0x42e4_7ae1_47ae_143c);

        // The exact positive complement exceeds i128, but remains a finite float.
        let extreme = follow_longitudinal(0.0, 2.0, 1, || Ok(i128::MIN), true).unwrap();
        assert_eq!(extreme.to_bits(), 0x4784_7ae1_47ae_147b);
    }
}
