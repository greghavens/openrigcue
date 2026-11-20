//! Safety layer between cueing and the motion driver (04 §3).
//!
//! The supervisor decides what is actually sent: it ramps in on engage, limits the step per tick while
//! running, ramps to rest on release, timeout or shutdown, and stops sending after an emergency stop.
//! It is a pure state machine driven by `update(now, target)`.

use std::time::Duration;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorState {
    /// Not driving the rig; nothing is sent.
    Idle,
    /// Ramping from the last known position to the cueing output.
    Engaging,
    /// Forwarding cueing output with a per-tick step limit.
    Running,
    /// Ramping to the rest position.
    Releasing,
    /// At rest; the rest position is held.
    Parked,
    /// Emergency stop: nothing is sent until reset.
    Stopped,
}

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub engage_time: Duration,
    pub release_time: Duration,
    /// No new target for this long while running triggers a release.
    pub telemetry_timeout: Duration,
    /// Largest change per tick while running, in the driver's position units.
    pub max_step: u32,
    /// Rest position per actuator.
    pub rest: Vec<u32>,
}

impl SupervisorConfig {
    /// Defaults for a platform with `actuators` actuators on `0..=full_scale`, resting at the centre.
    pub fn centred(actuators: usize, full_scale: u32) -> Self {
        SupervisorConfig {
            engage_time: Duration::from_secs(5),
            release_time: Duration::from_secs(3),
            telemetry_timeout: Duration::from_millis(200),
            // 10 % of full scale per tick: never limits normal cueing at 100 Hz, catches glitches.
            max_step: full_scale / 10,
            rest: vec![full_scale.div_ceil(2); actuators],
        }
    }
}

#[derive(Debug, Clone)]
struct Ramp {
    from: Vec<f64>,
    start: Duration,
    duration: Duration,
}

impl Ramp {
    /// Cosine-shaped progress 0..1.
    fn progress(&self, now: Duration) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let t = (now.saturating_sub(self.start).as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0);
        (1.0 - (t * std::f64::consts::PI).cos()) / 2.0
    }

    fn at(&self, now: Duration, to: &[f64]) -> Vec<f64> {
        let k = self.progress(now);
        self.from.iter().zip(to).map(|(a, b)| a + (b - a) * k).collect()
    }

    fn done(&self, now: Duration) -> bool {
        now.saturating_sub(self.start) >= self.duration
    }
}

pub struct Supervisor {
    config: SupervisorConfig,
    state: SupervisorState,
    /// Last position sent (or assumed: the rest position before the first engage).
    current: Vec<f64>,
    ramp: Option<Ramp>,
    last_target_at: Option<Duration>,
}

impl Supervisor {
    pub fn new(config: SupervisorConfig) -> Self {
        let current = config.rest.iter().map(|&v| f64::from(v)).collect();
        Supervisor { config, state: SupervisorState::Idle, current, ramp: None, last_target_at: None }
    }

    pub fn state(&self) -> SupervisorState {
        self.state
    }

    /// Tell the supervisor where the rig actually is (measured, or a safe assumption) before engaging.
    pub fn set_position(&mut self, positions: &[u32]) {
        if matches!(self.state, SupervisorState::Idle | SupervisorState::Parked | SupervisorState::Stopped) {
            self.current = positions.iter().map(|&v| f64::from(v)).collect();
        }
    }

    /// Start driving the rig: ramp from the current position into the cueing output.
    pub fn engage(&mut self, now: Duration) {
        if matches!(self.state, SupervisorState::Idle | SupervisorState::Parked | SupervisorState::Releasing) {
            self.ramp = Some(Ramp { from: self.current.clone(), start: now, duration: self.config.engage_time });
            self.state = SupervisorState::Engaging;
            self.last_target_at = Some(now);
        }
    }

    /// Ramp to rest.
    pub fn release(&mut self, now: Duration) {
        if matches!(self.state, SupervisorState::Engaging | SupervisorState::Running) {
            self.ramp = Some(Ramp { from: self.current.clone(), start: now, duration: self.config.release_time });
            self.state = SupervisorState::Releasing;
        }
    }

    /// First press: release immediately. Second press (or while already releasing): stop sending.
    pub fn emergency_stop(&mut self, now: Duration) {
        match self.state {
            SupervisorState::Engaging | SupervisorState::Running => self.release(now),
            _ => {
                self.state = SupervisorState::Stopped;
                self.ramp = None;
            }
        }
    }

    /// Leave the stopped state; the rig must be engaged again.
    pub fn reset(&mut self) {
        if self.state == SupervisorState::Stopped {
            self.state = SupervisorState::Idle;
        }
    }

    /// The driver reported a fault: the rig position is unknown, stop driving.
    pub fn fault(&mut self) {
        self.state = SupervisorState::Idle;
        self.ramp = None;
    }

    /// Advance one tick. `target` is the cueing output, if a fresh one exists for this tick. Returns the
    /// positions to send, or `None` when nothing must be sent.
    pub fn update(&mut self, now: Duration, target: Option<&[u32]>) -> Option<Vec<u32>> {
        if target.is_some() {
            self.last_target_at = Some(now);
        }
        let timed_out = self.last_target_at.is_none_or(|t| now.saturating_sub(t) > self.config.telemetry_timeout);
        let rest: Vec<f64> = self.config.rest.iter().map(|&v| f64::from(v)).collect();

        let next = match self.state {
            SupervisorState::Idle | SupervisorState::Stopped => return None,
            SupervisorState::Parked => rest,
            SupervisorState::Engaging | SupervisorState::Running if timed_out => {
                self.release(now);
                return self.update(now, None);
            }
            SupervisorState::Engaging => {
                let to: Vec<f64> = match target {
                    Some(t) => t.iter().map(|&v| f64::from(v)).collect(),
                    None => self.current.clone(),
                };
                let ramp = self.ramp.as_ref().expect("engaging has a ramp");
                let out = ramp.at(now, &to);
                if ramp.done(now) {
                    self.state = SupervisorState::Running;
                    self.ramp = None;
                }
                out
            }
            SupervisorState::Running => match target {
                Some(t) => {
                    let step = f64::from(self.config.max_step);
                    self.current.iter().zip(t).map(|(&c, &v)| c + (f64::from(v) - c).clamp(-step, step)).collect()
                }
                None => self.current.clone(),
            },
            SupervisorState::Releasing => {
                let ramp = self.ramp.as_ref().expect("releasing has a ramp");
                let out = ramp.at(now, &rest);
                if ramp.done(now) {
                    self.state = SupervisorState::Parked;
                    self.ramp = None;
                }
                out
            }
        };
        self.current = next;
        Some(self.current.iter().map(|&v| v.round().max(0.0) as u32).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn sup() -> Supervisor {
        Supervisor::new(SupervisorConfig::centred(2, 4095))
    }

    #[test]
    fn engage_starts_from_the_measured_position() {
        let mut s = sup();
        s.set_position(&[300, 300]);
        s.engage(ms(0));
        assert_eq!(s.update(ms(0), Some(&[2048, 2048])).unwrap(), vec![300, 300]);
        let early = s.update(ms(500), Some(&[2048, 2048])).unwrap();
        assert!(early[0] < 400, "{early:?}");
    }

    #[test]
    fn idle_sends_nothing() {
        let mut s = sup();
        assert_eq!(s.update(ms(0), Some(&[3000, 3000])), None);
    }

    #[test]
    fn engage_ramps_then_runs() {
        let mut s = sup();
        s.engage(ms(0));
        let first = s.update(ms(0), Some(&[3000, 1000])).unwrap();
        assert_eq!(first, vec![2048, 2048]);
        let mid = s.update(ms(2500), Some(&[3000, 1000])).unwrap();
        assert_eq!(mid, vec![2524, 1524]);
        s.update(ms(5000), Some(&[3000, 1000])).unwrap();
        assert_eq!(s.state(), SupervisorState::Running);
    }

    #[test]
    fn running_limits_step_size() {
        let mut s = sup();
        s.engage(ms(0));
        for t in (0..=5000).step_by(10) {
            s.update(ms(t), Some(&[2048, 2048]));
        }
        let out = s.update(ms(5010), Some(&[4095, 0])).unwrap();
        assert_eq!(out, vec![2048 + 409, 2048 - 409]);
    }

    #[test]
    fn telemetry_timeout_releases_to_rest() {
        let mut s = sup();
        s.engage(ms(0));
        for t in (0..=5000).step_by(10) {
            s.update(ms(t), Some(&[3000, 3000]));
        }
        s.update(ms(5300), None);
        assert_eq!(s.state(), SupervisorState::Releasing);
        let end = s.update(ms(8400), None).unwrap();
        assert_eq!(end, vec![2048, 2048]);
        assert_eq!(s.state(), SupervisorState::Parked);
    }

    #[test]
    fn emergency_stop_twice_stops_sending() {
        let mut s = sup();
        s.engage(ms(0));
        s.update(ms(10), Some(&[2048, 2048]));
        s.emergency_stop(ms(20));
        assert_eq!(s.state(), SupervisorState::Releasing);
        s.emergency_stop(ms(30));
        assert_eq!(s.update(ms(40), None), None);
        s.reset();
        assert_eq!(s.state(), SupervisorState::Idle);
    }
}
