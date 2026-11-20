//! Helpers shared by the serial actuator drivers of this unit (VNM, PROSIMU, Dyadic SCN6, Actuon): the
//! clamped rescale, the host activity flags the drivers read,
//! and small checks they all make.

use crate::motion::{ActuatorPositions, MotionError, PyFrame};

/// Linear mapping clamped to the output range, on 64-bit floats.
///
/// `r = (x - a)·(d - c)/(b - a) + c`, then
/// `min(r, d)` and `max(…, c)`, compared in that order (so a reversed output range yields `c`).
pub fn map_clamped(x: f64, in_min: f64, in_max: f64, out_min: f64, out_max: f64) -> f64 {
    let r = (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min;
    let upper = if r <= out_max { r } else { out_max };
    if out_min <= upper {
        upper
    } else {
        out_min
    }
}

/// A float as an integer: truncation toward zero.
pub fn py_int(v: f64) -> i64 {
    v.trunc() as i64
}

/// The reaction speed rescaled from 70..100 to `out_min..out_max` and truncated — the slider mapping used by
/// the PROSIMU and Dyadic drivers (slider range 70..100).
pub fn reaction_map(reaction_speed: i64, in_min: i64, in_max: i64, out_min: i64, out_max: i64) -> i64 {
    py_int(map_clamped(reaction_speed as f64, in_min as f64, in_max as f64, out_min as f64, out_max as f64))
}

/// Vertical actuator travel at the four corners, in millimetres.
///
/// Consumed by the IntelliComp motion compensation.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct CornerTravel {
    pub front_left: f64,
    pub front_right: f64,
    pub rear_left: f64,
    pub rear_right: f64,
}

impl CornerTravel {
    /// Each value is its slot rescaled from 0..65535 to 0..`stroke_mm`, as for every 16-bit driver.
    pub fn from_16bit(corners: [i64; 4], stroke_mm: i64) -> Self {
        let mm = |v: i64| map_clamped(v as f64, 0.0, 65535.0, 0.0, stroke_mm as f64);
        let [front_left, front_right, rear_left, rear_right] = corners.map(mm);
        CornerTravel { front_left, front_right, rear_left, rear_right }
    }

    /// The shared [`ActuatorPositions`] record; millimetres are truncated to integers there.
    pub fn to_positions(self) -> ActuatorPositions {
        ActuatorPositions {
            front_left: py_int(self.front_left),
            front_right: py_int(self.front_right),
            rear_left: py_int(self.rear_left),
            rear_right: py_int(self.rear_right),
            motors: [0; 6],
        }
    }
}

/// What the host application is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostActivity {
    /// The running game's key, `'none'` (or `''`) when idle.
    pub game_running: String,
    /// The telemetry listener key, `'none'` when idle.
    pub listener_running: String,
    /// Whether the motion test mode is on (Hardware page "Motion Test").
    pub motion_test_mode: bool,
    /// Where the values are read when a driver needs them (the service state); `None` keeps the values
    /// above.
    pub runtime: RuntimeReader,
}

impl HostActivity {
    /// The values as a driver reads them now: from the live source when one is installed.
    pub fn live(&self) -> HostActivity {
        match self.runtime.read() {
            Some(view) => HostActivity {
                game_running: view.game_running,
                listener_running: view.listener_running,
                motion_test_mode: view.motion_test_mode,
                runtime: self.runtime.clone(),
            },
            None => self.clone(),
        }
    }
}

/// The service state and telemetry as a driver reads them at that moment.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RuntimeView {
    /// The running game's key.
    pub game_running: String,
    /// The telemetry listener key.
    pub listener_running: String,
    /// Whether the motion test mode is on.
    pub motion_test_mode: bool,
    /// Whether the motion switch is on.
    pub motion_switch: bool,
    /// Whether a motion error is set.
    pub motion_error: bool,
    /// The telemetry values speed, maximum speed, rpm, maximum rpm and yaw angle.
    pub speed: f64,
    pub max_speed: f64,
    pub rpm: f64,
    pub max_rpm: f64,
    pub yaw_angle: f64,
}

/// The live source of [`RuntimeView`]: the drivers read the state and the telemetry where they use
/// them, while other threads keep changing them. Without an installed source every reader keeps the
/// values its settings were refreshed with. Handles compare equal (they carry access, not a value).
#[derive(Clone, Default)]
pub struct RuntimeReader(std::sync::Arc<std::sync::RwLock<Sources>>);

#[derive(Clone, Default)]
struct Sources {
    runtime: Option<RuntimeSource>,
    game_config: Option<GameConfigSource>,
    config: Option<std::sync::Arc<dyn ConfigSource>>,
}

/// The live configuration (the service's settings tree) as a driver reads it at that moment.
pub trait ConfigSource: Send + Sync {
    /// The raw value of `section.key`, or the error of the lookup.
    fn value(&self, section: &str, key: &str) -> Result<openrigcue_model::native_value::LegacyValue, String>;
    /// `section.key` as a boolean.
    fn switch(&self, section: &str, key: &str) -> Result<bool, String>;
    /// The spike-filter switch of the manufacturer `section` as a boolean (`yes` when it is not set).
    fn filter(&self, section: &str) -> Result<bool, String>;
}

/// A function reading the current tune (the raw dictionary).
pub type GameConfigSource = std::sync::Arc<dyn Fn() -> openrigcue_model::native_value::LegacyValue + Send + Sync>;

/// A function reading the current [`RuntimeView`].
pub type RuntimeSource = std::sync::Arc<dyn Fn() -> RuntimeView + Send + Sync>;

impl RuntimeReader {
    /// Every reader of this handle (and of its clones) reads through `source` from now on.
    pub fn install(&self, source: RuntimeSource) {
        self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner).runtime = Some(source);
    }

    /// Every reader reads the configuration through `source` from now on.
    pub fn install_config(&self, source: std::sync::Arc<dyn ConfigSource>) {
        self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner).config = Some(source);
    }

    fn config(&self) -> Option<std::sync::Arc<dyn ConfigSource>> {
        self.0.read().unwrap_or_else(std::sync::PoisonError::into_inner).config.clone()
    }

    /// The raw value of `section.key` now; `refreshed` without a live source.
    pub fn value(
        &self,
        section: &str,
        key: &str,
        refreshed: &Result<openrigcue_model::native_value::LegacyValue, String>,
    ) -> Result<openrigcue_model::native_value::LegacyValue, String> {
        self.config().map_or_else(|| refreshed.clone(), |config| config.value(section, key))
    }

    /// `section.key` as a boolean now; `refreshed` without a live source.
    pub fn switch(&self, section: &str, key: &str, refreshed: &Result<bool, String>) -> Result<bool, String> {
        self.config().map_or_else(|| refreshed.clone(), |config| config.switch(section, key))
    }

    /// The spike-filter switch of `section` now; `refreshed` without a live source.
    pub fn filter(&self, section: &str, refreshed: &Result<bool, String>) -> Result<bool, String> {
        self.config().map_or_else(|| refreshed.clone(), |config| config.filter(section))
    }

    /// The tune's `key` now: the live value when a tune source is installed, else `refreshed`.
    pub fn game_value(
        &self,
        key: &str,
        refreshed: &Result<openrigcue_model::native_value::LegacyValue, String>,
    ) -> Result<openrigcue_model::native_value::LegacyValue, String> {
        match self.game_config() {
            Some(game_config) => game_config.lookup_owned(&openrigcue_model::native_value::LegacyValue::from(key)),
            None => refreshed.clone(),
        }
    }

    /// Every reader reads the tune through `source` from now on.
    pub fn install_game_config(&self, source: GameConfigSource) {
        self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner).game_config = Some(source);
    }

    /// The tune now, when a source is installed.
    pub fn game_config(&self) -> Option<openrigcue_model::native_value::LegacyValue> {
        let source = self.0.read().unwrap_or_else(std::sync::PoisonError::into_inner).game_config.clone();
        source.map(|source| source())
    }

    /// The values now, when a source is installed.
    pub fn read(&self) -> Option<RuntimeView> {
        let source = self.0.read().unwrap_or_else(std::sync::PoisonError::into_inner).runtime.clone();
        source.map(|source| source())
    }
}

impl std::fmt::Debug for RuntimeReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.read().map(|s| s.runtime.is_some()).unwrap_or(false) {
            "RuntimeReader(live)"
        } else {
            "RuntimeReader(refreshed)"
        })
    }
}

impl PartialEq for RuntimeReader {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for RuntimeReader {}

impl Default for HostActivity {
    fn default() -> Self {
        HostActivity {
            game_running: "none".into(),
            listener_running: "none".into(),
            motion_test_mode: false,
            runtime: RuntimeReader::default(),
        }
    }
}

/// Fails when the frame is shorter than the slots a driver reads.
pub fn require_slots(motors: &[i64], needed: usize) -> Result<(), MotionError> {
    if motors.len() < needed {
        return Err(MotionError::TooFewMotors { needed, got: motors.len() });
    }
    Ok(())
}

/// The frame of the boolean conversion, for a value it does not accept.
pub const STR2BOOL: super::PyFrame = super::PyFrame::new("backend/functions.py", "str2bool", 261);

/// The error of a setting that cannot be read as a boolean at `frame`: a failed lookup
/// fails at that frame; a value the conversion rejects (`Boolean value expected. Got …`, or a value that is
/// not text) fails inside the conversion's frame.
pub fn boolean_setting_error(detail: &str, frame: super::PyFrame) -> MotionError {
    let error = MotionError::Native(detail.to_owned());
    let message = error.python_str();
    if message.starts_with("Boolean value expected") || message.starts_with("can only concatenate str") {
        error.in_frames([frame, STR2BOOL])
    } else {
        error.in_frame(frame)
    }
}

/// Whether the USB identity of `port` is the FTDI FT232R (`0403:6001`) the drivers require.
pub fn is_ft232r(port: &crate::discovery::PortInfo) -> bool {
    port.usb.as_ref().is_some_and(|usb| usb.vendor_id == 0x0403 && usb.product_id == 0x6001)
}

/// Whether the raw hardware id names an FT232R, for the Actuon scan; a missing hardware id is an error.
pub fn native_is_ft232r(port: &crate::discovery::ListPortInfo) -> Result<bool, MotionError> {
    port.hwid_contains("0403:6001").map_err(|message| MotionError::Native(format!("TypeError: {message}")))
}

/// The file names in the frames of the logging error report (no source line follows such a frame).
const LOGGING_FILE: &str = "logging\\__init__.py";
const THREADING_FILE: &str = "threading.py";

/// The error of formatting arguments into a message without a `%` placeholder.
const UNUSED_ARGUMENT: &str = "TypeError: not all arguments converted during string formatting";

/// The frames of that error, from the handler that catches it down to where the message is formatted.
const FORMAT_FRAMES: [PyFrame; 4] = [
    PyFrame::new(LOGGING_FILE, "emit", 1151),
    PyFrame::new(LOGGING_FILE, "format", 999),
    PyFrame::new(LOGGING_FILE, "format", 712),
    PyFrame::new(LOGGING_FILE, "getMessage", 400),
];

/// The call stack of the logging error report for a driver's connect: the frames of the
/// connect thread, from the thread's start to the logging calls and the handler that reports the error.
const CONNECT_THREAD_STACK: [PyFrame; 11] = [
    PyFrame::new(THREADING_FILE, "_bootstrap", 1015),
    PyFrame::new(THREADING_FILE, "_bootstrap_inner", 1044),
    PyFrame::new(THREADING_FILE, "run", 995),
    PyFrame::new(LOGGING_FILE, "exception", 1555),
    PyFrame::new(LOGGING_FILE, "error", 1549),
    PyFrame::new(LOGGING_FILE, "_log", 1665),
    PyFrame::new(LOGGING_FILE, "handle", 1681),
    PyFrame::new(LOGGING_FILE, "callHandlers", 1737),
    PyFrame::new(LOGGING_FILE, "handle", 1027),
    PyFrame::new(LOGGING_FILE, "emit", 1265),
    PyFrame::new(LOGGING_FILE, "emit", 1159),
];

/// A driver's error log with arguments from its connect, while `handled` is being handled, where
/// `message` has no `%` placeholder. The record is never written: formatting the message fails, and a
/// report goes to standard error instead:
/// `--- Logging error ---`, the formatting error chained to the error being handled, `Call stack:`, then
/// the quoted message and the arguments.
///
/// `handled` must carry its frames and class ([`MotionError::caught_traceback`]); the report is written
/// without the chained part otherwise.
pub(crate) fn connect_logging_argument_error(message: &str, arguments: &[String], handled: &MotionError) {
    let raised = MotionError::Native(UNUSED_ARGUMENT.into()).in_frames(FORMAT_FRAMES);
    let traceback = chained_traceback(&raised, Some(handled))
        .or_else(|| raised.caught_traceback())
        .expect("the TypeError's class and frames are known");
    let stack: String = CONNECT_THREAD_STACK.iter().map(PyFrame::text).collect();
    let items: Vec<String> = arguments.iter().map(|argument| super::python::str_repr(argument)).collect();
    // The arguments as a parenthesised list; a single one keeps a trailing comma.
    let tuple = match items.as_slice() {
        [one] => format!("({one},)"),
        many => format!("({})", many.join(", ")),
    };
    super::python_stderr(&format!(
        "--- Logging error ---\n{traceback}\nCall stack:\n{stack}Message: {}\nArguments: {tuple}\n",
        super::python::str_repr(message)
    ));
}

/// The traceback text of `raised`, which occurred while `context` was being handled:
/// the context's traceback, `During handling of the above exception, another exception
/// occurred:`, then the traceback of `raised`. `None` when a class or the frames of either are not known.
pub(crate) fn chained_traceback(raised: &MotionError, context: Option<&MotionError>) -> Option<String> {
    let own = raised.caught_traceback()?;
    match context {
        None => Some(own),
        Some(context) => Some(format!(
            "{}\n\nDuring handling of the above exception, another exception occurred:\n\n{own}",
            context.caught_traceback()?
        )),
    }
}

/// The text of an error record in the log file: `message`, a newline and the
/// traceback of `raised`
/// (chained to `context` when it occurred while that one was handled). The traceback is left out only where
/// the error's class or frames are not known; the caller logs the text at its own target and level.
pub(crate) fn exception_record(message: &str, raised: &MotionError, context: Option<&MotionError>) -> String {
    match chained_traceback(raised, context) {
        Some(traceback) => format!("{message}\n{traceback}"),
        None => message.to_owned(),
    }
}

/// The error of disconnecting an interface that was never opened (the first port of a
/// scan that does not open), at `frame`.
pub(crate) fn unbound_interface(frame: PyFrame) -> MotionError {
    MotionError::Native("UnboundLocalError: local variable 'interface' referenced before assignment".into())
        .in_frame(frame)
}

#[cfg(test)]
pub(crate) mod testing {
    //! Scripted links shared by the tests of this unit (for drivers that open ports themselves).

    /// A live configuration for the [`super::RuntimeReader`]: the values set now, a key error for a key never set.
    #[derive(Default)]
    pub struct FakeConfig(
        pub Mutex<std::collections::BTreeMap<(String, String), openrigcue_model::native_value::LegacyValue>>,
    );

    impl FakeConfig {
        pub fn set(&self, section: &str, key: &str, value: openrigcue_model::native_value::LegacyValue) {
            self.0.lock().unwrap().insert((section.into(), key.into()), value);
        }
    }

    impl super::ConfigSource for FakeConfig {
        fn value(&self, section: &str, key: &str) -> Result<openrigcue_model::native_value::LegacyValue, String> {
            self.0
                .lock()
                .unwrap()
                .get(&(section.into(), key.into()))
                .cloned()
                .ok_or_else(|| format!("KeyError: '{key}'"))
        }
        fn switch(&self, section: &str, key: &str) -> Result<bool, String> {
            use openrigcue_model::native_value::LegacyValue;
            match self.value(section, key)? {
                LegacyValue::Bool(value) => Ok(value),
                LegacyValue::Str(text) if text == "yes" => Ok(true),
                LegacyValue::Str(text) if text == "no" => Ok(false),
                other => {
                    Err(format!("Exception: Boolean value expected. Got {}", crate::motion::native::str_of(&other)))
                }
            }
        }
        fn filter(&self, section: &str) -> Result<bool, String> {
            self.switch(section, "filter")
        }
    }

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use crate::discovery::{PortInfo, UsbIdentity};
    use crate::serial::{Link, LinkError};

    /// One diagnostic of a scripted driver operation, in the order the driver produced it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Diagnostic {
        /// A logger call: the message text.
        Log(String),
        /// A printed line as it reaches stdout, without its newline.
        Print(String),
        /// Text written to standard error (the logging error report), as written.
        Stderr(String),
    }

    impl Diagnostic {
        pub fn log(message: &str) -> Self {
            Diagnostic::Log(message.into())
        }

        pub fn print(line: &str) -> Self {
            Diagnostic::Print(line.into())
        }
    }

    thread_local! {
        /// Where this thread's `python_print` lines go while a capture runs.
        static PRINTS: std::cell::RefCell<Option<Arc<Mutex<Vec<Diagnostic>>>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Hands a `python_print` line to this thread's capture; `false` when none runs (the line goes to stdout).
    pub fn captured_print(line: &str) -> bool {
        PRINTS.with(|prints| match prints.borrow().as_ref() {
            Some(sink) => {
                sink.lock().unwrap().push(Diagnostic::print(line));
                true
            }
            None => false,
        })
    }

    /// Hands standard-error text to this thread's capture; `false` when none runs (the text goes to stderr).
    pub fn captured_stderr(text: &str) -> bool {
        PRINTS.with(|prints| match prints.borrow().as_ref() {
            Some(sink) => {
                sink.lock().unwrap().push(Diagnostic::Stderr(text.into()));
                true
            }
            None => false,
        })
    }

    /// Capture this thread's logger messages and `print` lines, interleaved as produced, while a scripted
    /// driver operation runs. Nothing reaches the process's stdout meanwhile.
    pub fn capture_diagnostics(operation: impl FnOnce()) -> Vec<Diagnostic> {
        struct Restore(Option<Arc<Mutex<Vec<Diagnostic>>>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                PRINTS.with(|prints| *prints.borrow_mut() = self.0.take());
            }
        }
        let sink: Arc<Mutex<Vec<Diagnostic>>> = Arc::default();
        let restore = Restore(PRINTS.with(|prints| prints.borrow_mut().replace(sink.clone())));
        capture_logs(Capture(sink.clone(), Diagnostic::Log), operation);
        drop(restore);
        let mut captured = sink.lock().unwrap();
        std::mem::take(&mut *captured)
    }

    /// The `print` lines of [`capture_diagnostics`].
    pub fn capture_prints(operation: impl FnOnce()) -> Vec<String> {
        capture_diagnostics(operation)
            .into_iter()
            .filter_map(|diagnostic| match diagnostic {
                Diagnostic::Print(line) => Some(line),
                Diagnostic::Log(_) | Diagnostic::Stderr(_) => None,
            })
            .collect()
    }

    /// Capture this thread's tracing message fields while a scripted driver operation runs.
    pub fn capture_debug_messages(operation: impl FnOnce()) -> Vec<String> {
        let messages = Arc::new(Mutex::new(Vec::new()));
        capture_logs(Capture(messages.clone(), |message| message), operation);
        let mut captured = messages.lock().unwrap();
        std::mem::take(&mut *captured)
    }

    /// Collects this thread's tracing message fields, converted by its second field.
    struct Capture<T>(Arc<Mutex<Vec<T>>>, fn(String) -> T);

    impl<T: Send + 'static> tracing::Subscriber for Capture<T> {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Message(Vec<String>);
            impl tracing::field::Visit for Message {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "message" {
                        self.0.push(format!("{value:?}"));
                    }
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "message" {
                        self.0.push(value.into());
                    }
                }
            }
            let mut message = Message(Vec::new());
            event.record(&mut message);
            self.0.lock().unwrap().extend(message.0.into_iter().map(self.1));
        }
    }

    fn capture_logs<T: Send + 'static>(capture: Capture<T>, operation: impl FnOnce()) {
        // tracing-core's one-dispatcher fast path registers new callsites against the calling
        // thread's default. Other test threads have no default, so keep a second registration
        // alive while capturing. This passive dispatcher is never installed as a default.
        let registration = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        tracing::subscriber::with_default(capture, operation);
        drop(registration);
    }

    /// Everything written to a [`ScriptedLink`], shared with the test.
    pub type Written = Arc<Mutex<Vec<Vec<u8>>>>;
    /// Produces the reply to one written frame.
    pub type Responder = Box<dyn FnMut(&[u8]) -> Vec<u8> + Send>;

    /// Answers each write with the reply produced by `responder`, queued for the next read.
    pub struct ScriptedLink {
        pub name: String,
        pub written: Written,
        pub pending: VecDeque<u8>,
        pub responder: Responder,
        pub fail_writes: bool,
    }

    impl ScriptedLink {
        pub fn new(name: &str, responder: impl FnMut(&[u8]) -> Vec<u8> + Send + 'static) -> Self {
            ScriptedLink {
                name: name.into(),
                written: Arc::default(),
                pending: VecDeque::new(),
                responder: Box::new(responder),
                fail_writes: false,
            }
        }

        pub fn silent(name: &str) -> Self {
            Self::new(name, |_| Vec::new())
        }
    }

    impl Link for ScriptedLink {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
            if self.fail_writes {
                return Err(LinkError::Io(std::io::Error::other("unplugged")));
            }
            self.written.lock().unwrap().push(bytes.to_vec());
            let reply = (self.responder)(bytes);
            self.pending.extend(reply);
            Ok(())
        }
        fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
            Ok(self.pending.drain(..).collect())
        }
        fn clear_input(&mut self) -> Result<(), LinkError> {
            self.pending.clear();
            Ok(())
        }
        fn name(&self) -> &str {
            &self.name
        }
    }

    /// A port that identifies as an FTDI FT232R (`0403:6001`).
    pub fn ftdi_port(path: &str) -> PortInfo {
        PortInfo {
            path: path.into(),
            stable_path: None,
            usb: Some(UsbIdentity {
                vendor_id: 0x0403,
                product_id: 0x6001,
                serial_number: None,
                manufacturer: None,
                product: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_matches_backend_functions_map() {
        // The rescale: linear, then clamped to [out_min, out_max].
        assert_eq!(map_clamped(32768.0, 0.0, 65536.0, 0.0, 10000.0), 5000.0);
        assert_eq!(map_clamped(70000.0, 0.0, 65536.0, 0.0, 10000.0), 10000.0);
        assert_eq!(map_clamped(-5.0, 0.0, 65536.0, 0.0, 10000.0), 0.0);
        // Reversed input range (the SV reaction speed rescales from 100..70 to 125..240).
        assert_eq!(map_clamped(70.0, 100.0, 70.0, 125.0, 240.0), 240.0);
        assert_eq!(map_clamped(100.0, 100.0, 70.0, 125.0, 240.0), 125.0);
        // Reversed output range always yields out_min, because of the comparison order.
        assert_eq!(map_clamped(5.0, 0.0, 10.0, 10.0, 0.0), 10.0);
    }

    #[test]
    fn python_int_truncates_toward_zero() {
        assert_eq!(py_int(-4999.9), -4999);
        assert_eq!(py_int(4999.9), 4999);
        // 85 rescaled from 70..100 to 150..255 is 202.5, truncated to 202
        assert_eq!(reaction_map(85, 70, 100, 150, 255), 202);
    }

    #[test]
    fn corner_travel_uses_the_16bit_map() {
        let t = CornerTravel::from_16bit([0, 65535, 32767, 70000], 100);
        assert_eq!(t.front_left, 0.0);
        assert_eq!(t.front_right, 100.0);
        assert!((t.rear_left - 32767.0 * 100.0 / 65535.0).abs() < 1e-9);
        assert_eq!(t.rear_right, 100.0);
        assert_eq!(t.to_positions().rear_left, 49);
    }

    #[test]
    fn short_frames_are_index_errors() {
        assert!(require_slots(&[1, 2, 3], 3).is_ok());
        assert!(matches!(require_slots(&[1, 2], 3), Err(MotionError::TooFewMotors { needed: 3, got: 2 })));
    }
}
