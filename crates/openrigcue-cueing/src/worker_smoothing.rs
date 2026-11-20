//! The smoothing sequences run on independent threads in production.
//!
//! Each invocation owns its thread, so a blocked provider or sleeping helper cannot
//! prevent another invocation from reaching its own steps. Sleeps start
//! after the preceding step completes. Owner shutdown wakes and joins threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::pyvalue::{Op, PyValue};
use crate::CueError;

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Live values read by the independent helper invocations.
pub trait SmoothingInputs: Send + Sync + 'static {
    fn soft_start_manufacturer(&self) -> String;
    fn soft_start_timer(&self, manufacturer: &str) -> Result<i128, CueError>;
    fn gear_smoothing_factor(&self) -> Result<i128, CueError>;
    fn soft_start_timer_value(&self, manufacturer: &str) -> Result<PyValue, CueError> {
        self.soft_start_timer(manufacturer).map(PyValue::Int)
    }
    fn gear_smoothing_factor_value(&self) -> Result<PyValue, CueError> {
        self.gear_smoothing_factor().map(PyValue::Int)
    }
}

struct Flags {
    soft_start: AtomicBool,
    gear_status: AtomicBool,
    gear_value: Mutex<PyValue>,
    gear_stop: AtomicBool,
}

impl Default for Flags {
    fn default() -> Self {
        Self {
            soft_start: AtomicBool::new(false),
            gear_status: AtomicBool::new(false),
            gear_value: Mutex::new(PyValue::Int(0)),
            gear_stop: AtomicBool::new(false),
        }
    }
}

#[derive(Default)]
struct Shutdown {
    stopping: Mutex<bool>,
    wake: Condvar,
}
impl Shutdown {
    fn wait_until(&self, deadline: Instant) -> bool {
        let mut stopped = lock(&self.stopping);
        loop {
            if *stopped {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            let result =
                self.wake.wait_timeout(stopped, deadline - now).unwrap_or_else(std::sync::PoisonError::into_inner);
            stopped = result.0;
        }
    }
    fn stop(&self) {
        *lock(&self.stopping) = true;
        self.wake.notify_all();
    }
}

struct Job {
    wake: Instant,
    /// Gear: completed sleeps. Soft start does not use this field.
    done: u32,
}

/// Owns the per-invocation helper threads and interrupts their sleeps on shutdown.
pub struct SmoothingWorkers {
    flags: Arc<Flags>,
    inputs: Arc<dyn SmoothingInputs>,
    shutdown: Arc<Shutdown>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl SmoothingWorkers {
    pub fn start(inputs: Arc<dyn SmoothingInputs>) -> std::io::Result<Self> {
        Ok(Self { flags: Arc::default(), inputs, shutdown: Arc::default(), handles: Mutex::default() })
    }

    pub fn soft_start_engaged(&self) -> bool {
        self.flags.soft_start.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub fn gear_smoothing(&self) -> Option<i128> {
        self.gear_smoothing_value().and_then(|value| value.to_int().ok())
    }

    pub fn gear_smoothing_value(&self) -> Option<PyValue> {
        self.flags.gear_status.load(Ordering::SeqCst).then(|| lock(&self.flags.gear_value).clone())
    }

    fn launch(&self, gear: bool) -> std::io::Result<()> {
        let mut handles = lock(&self.handles);
        let mut i = 0;
        while i < handles.len() {
            if handles[i].is_finished() {
                let _ = handles.swap_remove(i).join();
            } else {
                i += 1;
            }
        }
        let flags = self.flags.clone();
        let inputs = self.inputs.clone();
        let shutdown = self.shutdown.clone();
        let name = if gear { "enable_gear_smoothing" } else { "enable_force_smoothing" };
        let handle = std::thread::Builder::new().name(name.into()).spawn(move || {
            let mut job = if gear { gear_start(&flags, inputs.as_ref()) } else { soft_start(&flags, inputs.as_ref()) };
            while let Some(next) = job {
                if !shutdown.wait_until(next.wake) {
                    break;
                }
                job = if gear {
                    gear_step(&flags, inputs.as_ref(), next)
                } else {
                    flags.soft_start.store(false, Ordering::SeqCst);
                    None
                };
            }
        })?;
        handles.push(handle);
        Ok(())
    }

    pub(crate) fn start_soft_start(&self) -> Result<(), CueError> {
        self.launch(false).map_err(thread_error)
    }

    pub(crate) fn start_gear_smoothing(&self) -> Result<(), CueError> {
        // Keep the caller's stop write separate from the new thread's first step.
        self.flags.gear_stop.store(self.flags.gear_status.load(Ordering::SeqCst), Ordering::SeqCst);
        self.launch(true).map_err(thread_error)
    }
}

fn thread_error(error: std::io::Error) -> CueError {
    tracing::error!(%error, "Unable to start motion smoothing thread");
    CueError::new(crate::error::Exception::Named("RuntimeError"), "can't start new thread")
}

impl Drop for SmoothingWorkers {
    fn drop(&mut self) {
        self.shutdown.stop();
        for handle in lock(&self.handles).drain(..) {
            let _ = handle.join();
        }
    }
}

fn worker_result<T>(result: Result<T, CueError>, name: &'static str, line: u32) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!("Exception in thread {name}:\n{}", error.at(crate::error::mh(name, line)).traceback().trim_end());
            None
        }
    }
}

fn soft_start(flags: &Flags, inputs: &dyn SmoothingInputs) -> Option<Job> {
    let manufacturer = inputs.soft_start_manufacturer();
    flags.soft_start.store(true, Ordering::SeqCst);
    let duration = inputs.soft_start_timer_value(&manufacturer).and_then(|value| {
        let integer = value.to_native().py_integer().map_err(crate::pyvalue::native_error)?;
        let nanos = integer * 1_000_000_000i64;
        let nanos: i64 =
            nanos.try_into().map_err(|_| CueError::overflow("timestamp out of range for platform time_t"))?;
        if nanos < 0 {
            return Err(CueError::value_error("sleep length must be non-negative"));
        }
        Ok(Duration::from_nanos(nanos as u64))
    });
    let duration = worker_result(duration, "enable_force_smoothing", 719)?;
    Some(Job { wake: Instant::now().checked_add(duration)?, done: 0 })
}

fn gear_start(flags: &Flags, inputs: &dyn SmoothingInputs) -> Option<Job> {
    flags.gear_status.store(true, Ordering::SeqCst);
    let factor = worker_result(inputs.gear_smoothing_factor_value(), "enable_gear_smoothing", 687)?;
    *lock(&flags.gear_value) = factor;
    if flags.gear_stop.load(Ordering::SeqCst) {
        flags.gear_stop.store(false, Ordering::SeqCst);
        return None;
    }
    Some(Job { wake: Instant::now() + Duration::from_millis(40), done: 1 })
}

fn gear_step(flags: &Flags, inputs: &dyn SmoothingInputs, job: Job) -> Option<Job> {
    if job.done == 40 {
        flags.gear_status.store(false, Ordering::SeqCst);
        return None;
    }
    let sleep = if job.done < 20 {
        Duration::from_millis(40)
    } else {
        let factor = worker_result(inputs.gear_smoothing_factor_value(), "enable_gear_smoothing", 698)?;
        let value = worker_result(
            factor
                .binop(Op::Div, &PyValue::Float(20.0))
                .and_then(|v| v.binop(Op::Mul, &PyValue::Int(i128::from(40 - job.done))))
                .and_then(|v| v.to_integer_value()),
            "enable_gear_smoothing",
            698,
        )?;
        *lock(&flags.gear_value) = value;
        Duration::from_millis(50)
    };
    if flags.gear_stop.load(Ordering::SeqCst) {
        flags.gear_stop.store(false, Ordering::SeqCst);
        return None;
    }
    Some(Job { wake: Instant::now() + sleep, done: job.done + 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicUsize};
    use std::sync::mpsc;

    struct Inputs {
        timer: i128,
        factor: AtomicI64,
        reads: Mutex<Vec<Instant>>,
    }

    impl Inputs {
        fn new(timer: i128, factor: i64) -> Self {
            Self { timer, factor: AtomicI64::new(factor), reads: Mutex::default() }
        }
    }

    impl SmoothingInputs for Inputs {
        fn soft_start_manufacturer(&self) -> String {
            "DOF REALITY".into()
        }
        fn soft_start_timer(&self, _: &str) -> Result<i128, CueError> {
            Ok(self.timer)
        }
        fn gear_smoothing_factor(&self) -> Result<i128, CueError> {
            lock(&self.reads).push(Instant::now());
            Ok(i128::from(self.factor.load(Ordering::SeqCst)))
        }
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < end, "worker failed to reach its next statement");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn zero_duration_sets_flag_before_read_and_clears_without_a_frame() {
        struct Gated {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl SmoothingInputs for Gated {
            fn soft_start_manufacturer(&self) -> String {
                "DOF REALITY".into()
            }
            fn soft_start_timer(&self, _: &str) -> Result<i128, CueError> {
                self.entered.send(()).unwrap();
                lock(&self.release).recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(0)
            }
            fn gear_smoothing_factor(&self) -> Result<i128, CueError> {
                Ok(0)
            }
        }
        let (entered, start) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        let workers = SmoothingWorkers::start(Arc::new(Gated { entered, release: Mutex::new(resumed) })).unwrap();
        workers.start_soft_start().unwrap();
        start.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(workers.soft_start_engaged(), "zero duration still writes true before sleep(0)");
        release.send(()).unwrap();
        wait_for(|| !workers.soft_start_engaged());
    }

    #[test]
    fn sleeping_helpers_progress_and_reread_factor_without_frames() {
        let inputs = Arc::new(Inputs::new(1, 100));
        let workers = SmoothingWorkers::start(inputs.clone()).unwrap();
        workers.start_soft_start().unwrap();
        workers.start_gear_smoothing().unwrap();
        wait_for(|| workers.soft_start_engaged() && workers.gear_smoothing() == Some(100));
        let started = lock(&inputs.reads)[0];
        std::thread::sleep(Duration::from_millis(100));
        inputs.factor.store(60, Ordering::SeqCst);
        assert_eq!(workers.gear_smoothing(), Some(100), "plateau retains its initial factor");
        wait_for(|| lock(&inputs.reads).len() >= 2);
        assert!(lock(&inputs.reads)[1].duration_since(started) >= Duration::from_millis(800));
        wait_for(|| workers.gear_smoothing() == Some(60));
        wait_for(|| lock(&inputs.reads).len() >= 3);
        let read_gap = {
            let reads = lock(&inputs.reads);
            reads[2].duration_since(reads[1])
        };
        assert!(read_gap >= Duration::from_millis(50));
        wait_for(|| workers.gear_smoothing() == Some(57));
        wait_for(|| !workers.soft_start_engaged());
        assert!(started.elapsed() >= Duration::from_secs(1));
        wait_for(|| workers.gear_smoothing().is_none());
        assert_eq!(lock(&inputs.reads).len(), 21, "initial read plus each of 20 ramp steps");
        assert!(started.elapsed() >= Duration::from_millis(1800));
    }

    #[test]
    fn gear_sleep_begins_after_actual_factor_read_not_nominal_deadline() {
        struct Delayed {
            reads: AtomicUsize,
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            times: Mutex<Vec<Instant>>,
        }
        impl SmoothingInputs for Delayed {
            fn soft_start_manufacturer(&self) -> String {
                "DOF REALITY".into()
            }
            fn soft_start_timer(&self, _: &str) -> Result<i128, CueError> {
                Ok(0)
            }
            fn gear_smoothing_factor(&self) -> Result<i128, CueError> {
                if self.reads.fetch_add(1, Ordering::SeqCst) == 1 {
                    self.entered.send(()).unwrap();
                    lock(&self.release).recv_timeout(Duration::from_secs(5)).unwrap();
                }
                lock(&self.times).push(Instant::now());
                Ok(100)
            }
        }
        let (entered, start) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        let inputs = Arc::new(Delayed {
            reads: AtomicUsize::new(0),
            entered,
            release: Mutex::new(resumed),
            times: Mutex::default(),
        });
        let workers = SmoothingWorkers::start(inputs.clone()).unwrap();
        workers.start_gear_smoothing().unwrap();
        start.recv_timeout(Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        release.send(()).unwrap();
        wait_for(|| lock(&inputs.times).len() >= 3);
        let times = lock(&inputs.times);
        assert!(times[2].duration_since(times[1]) >= Duration::from_millis(50), "no catch-up ramp steps");
    }

    #[test]
    fn both_gear_stop_orderings_keep_the_source_statement_effects() {
        let flags = Flags::default();
        let inputs = Inputs::new(0, 95);
        let old = gear_start(&flags, &inputs).unwrap();
        flags.gear_stop.store(true, Ordering::SeqCst);
        // New thread wins: it writes the full factor, consumes stop, exits; old continues.
        assert!(gear_start(&flags, &inputs).is_none());
        assert!(gear_step(&flags, &inputs, old).is_some());
        assert!(flags.gear_status.load(Ordering::SeqCst));
        let old = gear_start(&flags, &inputs).unwrap();
        flags.gear_stop.store(true, Ordering::SeqCst);
        // Old thread wins: it consumes stop and exits; new starts an independent full schedule.
        assert!(gear_step(&flags, &inputs, old).is_none());
        assert!(gear_start(&flags, &inputs).is_some());
        assert!(flags.gear_status.load(Ordering::SeqCst));
    }

    #[test]
    fn blocked_gear_read_does_not_serialize_new_invocation() {
        struct Gated {
            calls: AtomicUsize,
            entered: mpsc::Sender<usize>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl SmoothingInputs for Gated {
            fn soft_start_manufacturer(&self) -> String {
                "DOF REALITY".into()
            }
            fn soft_start_timer(&self, _: &str) -> Result<i128, CueError> {
                Ok(0)
            }
            fn gear_smoothing_factor(&self) -> Result<i128, CueError> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.send(call).unwrap();
                if call == 0 {
                    lock(&self.release).recv_timeout(Duration::from_secs(5)).unwrap();
                }
                Ok(95)
            }
        }
        let (entered, observed) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        let workers = SmoothingWorkers::start(Arc::new(Gated {
            calls: AtomicUsize::new(0),
            entered,
            release: Mutex::new(resumed),
        }))
        .unwrap();
        workers.start_gear_smoothing().unwrap();
        assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(), 0);
        workers.start_gear_smoothing().unwrap();
        let second = observed.recv_timeout(Duration::from_secs(1));
        // Always release the first provider before an assertion could unwind/drop.
        release.send(()).unwrap();
        assert_eq!(second.unwrap(), 1, "each native invocation starts its own thread");
    }

    #[test]
    fn overlapping_soft_start_invocations_keep_independent_completion() {
        struct Inputs {
            calls: AtomicUsize,
            entered: mpsc::Sender<usize>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl SmoothingInputs for Inputs {
            fn soft_start_manufacturer(&self) -> String {
                "DOF REALITY".into()
            }
            fn soft_start_timer(&self, _: &str) -> Result<i128, CueError> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.send(call).unwrap();
                if call == 0 {
                    lock(&self.release).recv_timeout(Duration::from_secs(5)).unwrap();
                }
                Ok(0)
            }
            fn gear_smoothing_factor(&self) -> Result<i128, CueError> {
                Ok(0)
            }
        }
        let (entered, observed) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        let workers = SmoothingWorkers::start(Arc::new(Inputs {
            calls: AtomicUsize::new(0),
            entered,
            release: Mutex::new(resumed),
        }))
        .unwrap();
        workers.start_soft_start().unwrap();
        assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(), 0);
        workers.start_soft_start().unwrap();
        let second = observed.recv_timeout(Duration::from_secs(1));
        if second.is_ok() {
            wait_for(|| !workers.soft_start_engaged());
        }
        release.send(()).unwrap();
        assert_eq!(second.unwrap(), 1);
        // The second invocation clears the shared flag while the first is blocked;
        // the flag is shared, not reference-counted per invocation.
        wait_for(|| !workers.soft_start_engaged());
    }

    #[test]
    fn invalid_sleep_latches_flag_and_drop_interrupts_valid_long_sleep() {
        let workers = SmoothingWorkers::start(Arc::new(Inputs::new(-1, 0))).unwrap();
        workers.start_soft_start().unwrap();
        wait_for(|| workers.soft_start_engaged());
        std::thread::sleep(Duration::from_millis(10));
        assert!(workers.soft_start_engaged());
        drop(workers);
        let workers = SmoothingWorkers::start(Arc::new(Inputs::new(3600, 0))).unwrap();
        workers.start_soft_start().unwrap();
        wait_for(|| workers.soft_start_engaged());
        let before = Instant::now();
        drop(workers);
        assert!(before.elapsed() < Duration::from_secs(1), "shutdown must wake sleeping helpers");
    }

    #[test]
    fn factor_conversion_failure_keeps_status_and_last_written_value() {
        struct Failed;
        impl SmoothingInputs for Failed {
            fn soft_start_manufacturer(&self) -> String {
                "DOF REALITY".into()
            }
            fn soft_start_timer(&self, _: &str) -> Result<i128, CueError> {
                Ok(0)
            }
            fn gear_smoothing_factor(&self) -> Result<i128, CueError> {
                Err(CueError::key("gear_smoothing_factor"))
            }
        }
        let flags = Flags::default();
        assert!(gear_start(&flags, &Failed).is_none());
        assert!(flags.gear_status.load(Ordering::SeqCst));
        assert_eq!(*lock(&flags.gear_value), PyValue::Int(0));
        *lock(&flags.gear_value) = PyValue::Int(51);
        assert!(gear_step(&flags, &Failed, Job { wake: Instant::now(), done: 21 }).is_none());
        assert!(flags.gear_status.load(Ordering::SeqCst));
        assert_eq!(*lock(&flags.gear_value), PyValue::Int(51));
    }

    #[test]
    fn source_sleep_nanosecond_range_latches_out_of_range_duration() {
        let flags = Flags::default();
        let max = i128::from(crate::timers::MAX_SLEEP_SECONDS);
        assert!(soft_start(&flags, &Inputs::new(max, 0)).is_some());
        assert!(soft_start(&flags, &Inputs::new(max + 1, 0)).is_none());
        assert!(flags.soft_start.load(Ordering::SeqCst));
    }

    #[test]
    fn smoothing_windows_observe_worker_changes_between_effects() {
        use crate::smoothing::{smoothing_window_with_workers, WindowContext};
        use crate::views::Sources;
        use crate::{Axis, MotionSettings, MotionTune, PyValue};
        let workers = SmoothingWorkers::start(Arc::new(Inputs::new(0, 0))).unwrap();
        let settings = MotionSettings::default();
        let tune = MotionTune::default();
        let context = WindowContext {
            sources: Sources { config: &settings, game: &tune, manufacturer: "DOF REALITY", game_running: "ac" },
            soft_start_engaged: false,
            gear_smoothing: None,
            speed: PyValue::Float(0.0),
        };
        workers.flags.soft_start.store(true, Ordering::SeqCst);
        assert_eq!(
            smoothing_window_with_workers(Axis::Roll, &context, Some(&workers)),
            Ok(PyValue::Int(settings.manufacturer.soft_start_stop_factor.unwrap().into()))
        );
        workers.flags.soft_start.store(false, Ordering::SeqCst);
        assert_eq!(
            smoothing_window_with_workers(Axis::Pitch, &context, Some(&workers)),
            Ok(PyValue::Float(tune.effect_smoothing[Axis::Pitch]))
        );
    }
    #[test]
    fn native_helper_retains_unbounded_initial_factor_and_failed_ramp_value() {
        use num_bigint::BigInt;
        use num_traits::One;
        struct Native {
            factor: PyValue,
        }
        impl SmoothingInputs for Native {
            fn soft_start_manufacturer(&self) -> String {
                "DOF REALITY".into()
            }
            fn soft_start_timer(&self, _: &str) -> Result<i128, CueError> {
                unreachable!("native path")
            }
            fn gear_smoothing_factor(&self) -> Result<i128, CueError> {
                unreachable!("native path")
            }
            fn gear_smoothing_factor_value(&self) -> Result<PyValue, CueError> {
                Ok(self.factor.clone())
            }
        }
        let factor =
            PyValue::from(&openrigcue_model::native_value::LegacyValue::from_integer(BigInt::one() << 2048usize));
        let inputs = Native { factor: factor.clone() };
        let flags = Flags::default();
        assert!(gear_start(&flags, &inputs).is_some());
        assert_eq!(*lock(&flags.gear_value), factor);
        assert!(gear_step(&flags, &inputs, Job { wake: Instant::now(), done: 20 }).is_none());
        assert_eq!(*lock(&flags.gear_value), factor);
        assert!(flags.gear_status.load(Ordering::SeqCst));
    }
}
