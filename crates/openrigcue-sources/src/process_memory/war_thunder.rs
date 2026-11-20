//! War Thunder / Aces of Thunder: the game's local web server, `/indicators` and `/state` polled by a
//! background thread (listener key `warthunder`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, python_compare::native_equal, Telemetry};

use super::http::{HttpClient, HttpGet};
use super::python::int_of_round6;
use super::python::json::{loads_bytes, Json};
use crate::udp_flight::python::fold_roll;
use crate::udp_flight::{clear_listener_channels, AngularRates, FlightSource};
use crate::udp_racing::project_motor_racing::NetworkPortsReader;
use crate::{NativeRead, Source, SourceError};

/// `NETWORK_PORT/warthunder` default.
pub const DEFAULT_PORT: u16 = 8111;
/// Timeout of one GET request.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(50);
/// Poll pause while the game answers, and while it does not.
const CONNECTED_PAUSE: Duration = Duration::from_millis(10);
const DISCONNECTED_PAUSE: Duration = Duration::from_secs(1);

/// The two documents of the last successful poll.
#[derive(Debug, Clone, Default)]
struct Documents {
    indicators: Option<Json>,
    state: Option<Json>,
    connected: bool,
}

/// One poll: both GETs must answer 200 with JSON for `connected`.
fn poll_once(http: &mut dyn HttpGet, indicators_url: &str, state_url: &str, documents: &Mutex<Documents>) -> bool {
    // A failed `/indicators` request skips the `/state` request.
    let responses = match http.get(indicators_url, &[]) {
        Ok(indicators) => http.get(state_url, &[]).map(|state| (indicators, state)),
        Err(error) => Err(error),
    };
    let Ok((i, s)) = responses else {
        documents.lock().unwrap_or_else(PoisonError::into_inner).connected = false;
        return false;
    };
    if i.status != 200 || s.status != 200 {
        documents.lock().unwrap_or_else(PoisonError::into_inner).connected = false;
        return false;
    }
    let Ok(indicators) = loads_bytes(&i.body) else {
        documents.lock().unwrap_or_else(PoisonError::into_inner).connected = false;
        return false;
    };
    // Each JSON assignment precedes the next decode. A bad state document leaves
    // newly published indicators and the previous state visible to its independently gated getters.
    documents.lock().unwrap_or_else(PoisonError::into_inner).indicators = Some(indicators);
    let Ok(state) = loads_bytes(&s.body) else {
        documents.lock().unwrap_or_else(PoisonError::into_inner).connected = false;
        return false;
    };
    // The state assignment and the connected flag are published together: one step for a getter.
    let mut documents = documents.lock().unwrap_or_else(PoisonError::into_inner);
    documents.state = Some(state);
    documents.connected = true;
    true
}

enum StartupPort {
    Fixed(String),
    Native(NetworkPortsReader),
}

impl StartupPort {
    /// Called once when the worker starts, before its retry loop. No integer conversion occurs.
    fn urls(self) -> Option<(String, String)> {
        let port = match self {
            Self::Fixed(port) => port,
            Self::Native(read) => {
                openrigcue_model::python_repr::str_native(read().get("warthunder")?).ok()?.scalar_string()?
            }
        };
        let base = format!("http://localhost:{port}");
        Some((format!("{base}/indicators"), format!("{base}/state")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocumentField {
    State(&'static str),
    Indicator(&'static str),
}

/// The per-tick memory of `longitudinal_acceleration` (the speed change between two requests).
#[derive(Debug, Clone, Default, PartialEq)]
struct SurgeMemory {
    previous_speed: Option<LegacyValue>,
    previous_change: Option<LegacyValue>,
}

impl SurgeMemory {
    /// The `longitudinal_acceleration` read: 0 on the first call;
    /// then the speed difference since the previous call, or the last non-zero difference while the speed holds.
    fn next(&mut self, speed: f64) -> f64 {
        self.read(LegacyValue::Float(speed)).and_then(|value| value.number()).unwrap_or(0.0)
    }

    fn read(&mut self, speed: LegacyValue) -> Option<LegacyValue> {
        self.read_with(|| Some(speed.clone()))
    }

    fn read_with(&mut self, mut speed: impl FnMut() -> Option<LegacyValue>) -> Option<LegacyValue> {
        let Some(previous) = self.previous_speed.as_ref().filter(|value| !matches!(value, LegacyValue::None)) else {
            self.previous_speed = Some(speed()?);
            return Some(LegacyValue::Int(0));
        };
        let change = difference(&speed()?, previous)?;
        self.previous_speed = Some(speed()?);
        if !native_equal(&change, &LegacyValue::Int(0)) {
            self.previous_change = Some(change.clone());
            return Some(change);
        }
        Some(self.previous_change.clone().unwrap_or(LegacyValue::Int(0)))
    }
}

fn integer(value: &LegacyValue) -> Option<i64> {
    match value {
        LegacyValue::Int(n) => Some(*n),
        LegacyValue::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

fn bounded_integer(value: i128) -> LegacyValue {
    i64::try_from(value).map_or_else(|_| LegacyValue::Float(value as f64), LegacyValue::Int)
}

fn difference(left: &LegacyValue, right: &LegacyValue) -> Option<LegacyValue> {
    if let (Some(left), Some(right)) = (integer(left), integer(right)) {
        Some(bounded_integer(i128::from(left) - i128::from(right)))
    } else {
        Some(LegacyValue::Float(left.number()? - right.number()?))
    }
}

fn multiply(value: &LegacyValue, factor: i64) -> Option<LegacyValue> {
    if let Some(integer) = integer(value) {
        return Some(bounded_integer(i128::from(integer) * i128::from(factor)));
    }
    Some(match value {
        LegacyValue::Float(value) => LegacyValue::Float(value * factor as f64),
        LegacyValue::Str(text) => LegacyValue::Str(text.repeat(factor.max(0) as usize)),
        LegacyValue::List(values) => {
            LegacyValue::List((0..factor.max(0)).flat_map(|_| values.iter().cloned()).collect())
        }
        _ => return None,
    })
}

fn attitude(value: &LegacyValue) -> Option<LegacyValue> {
    let value = multiply(value, -1)?;
    let number = value.number()?;
    if number > 90.0 {
        difference(&LegacyValue::Int(180), &value)
    } else if number < -90.0 {
        difference(&LegacyValue::Int(-180), &value)
    } else {
        Some(value)
    }
}

fn rounded_text(value: &Json) -> Option<LegacyValue> {
    let text = match value {
        Json::Int(value) => value.to_string(),
        Json::Bool(value) => i64::from(*value).to_string(),
        Json::Float(value) => crate::udp_racing::int_text(int_of_round6(*value)?)?,
        _ => return None,
    };
    Some(LegacyValue::Str(text))
}

/// The War Thunder listener; the poll thread runs from construction
/// until the source is dropped.
pub struct WarThunder {
    documents: Arc<Mutex<Documents>>,
    surge: Mutex<SurgeMemory>,
    rates: AngularRates,
    stop: Arc<AtomicBool>,
    poller: Option<JoinHandle<()>>,
}

impl WarThunder {
    pub fn new(port: u16) -> Self {
        Self::with_http(HttpClient::new(REQUEST_TIMEOUT), port)
    }

    pub fn with_http(http: impl HttpGet + 'static, port: u16) -> Self {
        Self::with_startup(http, StartupPort::Fixed(port.to_string()))
    }

    /// Keep the settings the worker read at entry, independently of dispatcher progress.
    pub fn with_live_network_ports(reader: NetworkPortsReader) -> Self {
        Self::with_startup(HttpClient::new(REQUEST_TIMEOUT), StartupPort::Native(reader))
    }

    fn with_startup(http: impl HttpGet + 'static, startup: StartupPort) -> Self {
        let documents = Arc::new(Mutex::new(Documents::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let poller = {
            let (documents, stop) = (Arc::clone(&documents), Arc::clone(&stop));
            let mut http = http;
            thread::Builder::new()
                .name("warthunder_main_loop".into())
                .spawn(move || {
                    let Some((indicators_url, state_url)) = startup.urls() else { return };
                    while !stop.load(Ordering::Relaxed) {
                        let connected = poll_once(&mut http, &indicators_url, &state_url, &documents);
                        thread::sleep(if connected { CONNECTED_PAUSE } else { DISCONNECTED_PAUSE });
                    }
                })
                .ok()
        };
        WarThunder { documents, surge: Mutex::default(), rates: AngularRates::default(), stop, poller }
    }

    fn snapshot(&self) -> Documents {
        self.documents.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Every channel read; each reads 0 on any failure, and every
    /// channel reads 0 while there is no state document or its `valid` entry is `false`.
    fn write_channels(&mut self, documents: &Documents, t: &mut Telemetry) {
        clear_listener_channels(t);
        self.rates = AngularRates::default();
        let (Some(state), Some(indicators)) = (&documents.state, &documents.indicators) else { return };
        if !matches!(state, Json::Object(_)) || matches!(state.get("valid"), None | Some(Json::Bool(false))) {
            return;
        }
        let number = |doc: &Json, key: &str| doc.get(key).and_then(Json::number);
        let raw = |doc: &Json, key: &str| doc.get(key).and_then(Json::channel_value).unwrap_or(0.0);
        let attitude = |key: &str| number(indicators, key).map_or(0.0, |a| fold_roll(-a));
        t.speed = number(indicators, "speed").and_then(int_of_round6).unwrap_or(0.0);
        t.rpm = number(indicators, "rpm").and_then(int_of_round6).unwrap_or(0.0);
        t.yaw = raw(indicators, "compass");
        t.lateral_acceleration = raw(state, "AoA, deg");
        t.vertical_acceleration = number(state, "Ny").map_or(0.0, |ny| ny - 1.0);
        t.max_rpm = 20000.0;
        t.roll = attitude("aviahorizon_roll");
        t.pitch = attitude("aviahorizon_pitch");
        t.longitudinal_acceleration = number(indicators, "speed")
            .map_or(0.0, |speed| self.surge.get_mut().unwrap_or_else(PoisonError::into_inner).next(speed));
        // Anything but a string cannot be upper-cased, which reads 0 (no name).
        t.vehicle = match indicators.get("type") {
            Some(Json::Str(kind)) => kind.to_uppercase(),
            _ => String::new(),
        };
        self.rates = AngularRates {
            yaw: number(indicators, "turn").map_or(0.0, |turn| turn * 10.0),
            pitch: raw(state, "elevator, %"),
            roll: raw(state, "Wx, deg/s"),
        };
    }

    fn native_with(&self, name: &str, mut read: impl FnMut(DocumentField) -> Option<Json>) -> NativeRead {
        use LegacyValue::{Int, Str};
        let mut answer = || -> Option<LegacyValue> {
            // The gate closes only for the literal false; numeric zero is allowed.
            if matches!(read(DocumentField::State("valid"))?, Json::Bool(false)) {
                return None;
            }
            match name {
                "speed" => rounded_text(&read(DocumentField::Indicator("speed"))?),
                "rpm" => rounded_text(&read(DocumentField::Indicator("rpm"))?),
                "max_rpm" => Some(Str("20000".into())),
                "car_name" => match read(DocumentField::Indicator("type"))? {
                    Json::Str(kind) => Some(Str(kind.to_uppercase())),
                    _ => None,
                },
                "yaw" => Some(read(DocumentField::Indicator("compass"))?.native()),
                "yaw_rate" => multiply(&read(DocumentField::Indicator("turn"))?.native(), 10),
                "pitch_rate" => Some(read(DocumentField::State("elevator, %"))?.native()),
                "roll_rate" => Some(read(DocumentField::State("Wx, deg/s"))?.native()),
                "lateral_acceleration" => Some(read(DocumentField::State("AoA, deg"))?.native()),
                "vertical_acceleration" => difference(&read(DocumentField::State("Ny"))?.native(), &Int(1)),
                "roll" => attitude(&read(DocumentField::Indicator("aviahorizon_roll"))?.native()),
                "pitch" => attitude(&read(DocumentField::Indicator("aviahorizon_pitch"))?.native()),
                "longitudinal_acceleration" => self
                    .surge
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .read_with(|| read(DocumentField::Indicator("speed")).map(|value| value.native())),
                _ => Some(Int(0)),
            }
        };
        NativeRead::Value(answer().unwrap_or(Int(0)))
    }
}

impl Source for WarThunder {
    fn id(&self) -> &'static str {
        "warthunder"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            _ => None,
        }
    }

    /// A channel read is atomic with respect to the poll thread: it cannot replace a document in the
    /// middle of it, so the documents stay held for the whole call.
    fn read_native(&self, name: &str) -> NativeRead {
        let docs = self.documents.lock().unwrap_or_else(PoisonError::into_inner);
        self.native_with(name, |field| match field {
            DocumentField::State(key) => docs.state.as_ref()?.get(key).cloned(),
            DocumentField::Indicator(key) => docs.indicators.as_ref()?.get(key).cloned(),
        })
    }

    /// The liveness test: the poll thread's `connected` flag.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.documents.lock().unwrap_or_else(PoisonError::into_inner).connected)
    }

    /// Receives (the `connected` flag) and writes the channels from the latest documents.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let documents = self.snapshot();
        self.write_channels(&documents, t);
        Ok(documents.connected)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.documents.lock().unwrap_or_else(PoisonError::into_inner).connected)
    }
}

impl FlightSource for WarThunder {
    fn angular_rates(&self) -> AngularRates {
        self.rates
    }
}

impl Drop for WarThunder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(poller) = self.poller.take() {
            let _ = poller.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_memory::http::testing::ScriptedHttp;
    use openrigcue_model::native_value::Dict;

    const IND: &str = "http://localhost:8111/indicators";
    const ST: &str = "http://localhost:8111/state";

    #[test]
    fn native_worker_reads_raw_port_once_at_start_and_keeps_url_for_retries() {
        use std::sync::atomic::AtomicUsize;
        for text in ["008111", "+8111", "70000", "bad"] {
            let reads = Arc::new(AtomicUsize::new(0));
            let current = Arc::new(Mutex::new(text.to_owned()));
            let reader: NetworkPortsReader = {
                let reads = reads.clone();
                let current = current.clone();
                Arc::new(move || {
                    reads.fetch_add(1, Ordering::Relaxed);
                    let mut ports = Dict::new();
                    ports.insert("warthunder", LegacyValue::Str(current.lock().unwrap().clone()));
                    ports
                })
            };
            let startup = StartupPort::Native(reader);
            assert_eq!(reads.load(Ordering::Relaxed), 0);
            *current.lock().unwrap() = format!("{text}0");
            let (ind, state) = startup.urls().unwrap();
            assert_eq!(reads.load(Ordering::Relaxed), 1);
            assert_eq!(ind, format!("http://localhost:{text}0/indicators"));
            assert_eq!(state, format!("http://localhost:{text}0/state"));
            *current.lock().unwrap() = "9000".into();
            let mut http = ScriptedHttp::default();
            let shared = Mutex::new(Documents::default());
            for _ in 0..2 {
                http.reply(&ind, 200, r#"{"speed":12}"#);
                http.reply(&state, 200, r#"{"valid":true}"#);
                assert!(poll_once(&mut http, &ind, &state, &shared));
            }
            assert_eq!(*http.requested.lock().unwrap(), [ind.clone(), state.clone(), ind, state]);
            assert_eq!(reads.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn native_worker_missing_port_ends_initialization_without_fallback() {
        let reader: NetworkPortsReader = Arc::new(Dict::new);
        assert!(StartupPort::Native(reader).urls().is_none());
        assert_eq!(StartupPort::Fixed("8111".into()).urls(), Some((IND.into(), ST.into())));
    }

    #[test]
    fn native_port_decimal_output_failure_precedes_url_and_retry_initialization() {
        let huge = LegacyValue::from_integer(openrigcue_model::native_value::BigInt::from(10u8).pow(4300));
        let reader: NetworkPortsReader = Arc::new(move || {
            let mut ports = Dict::new();
            ports.insert("warthunder", huge.clone());
            ports
        });
        assert!(StartupPort::Native(reader).urls().is_none());
    }

    /// The second JSON failure retains the first assignment, which getters can observe.
    #[test]
    fn native_second_json_failure_keeps_new_indicators_and_previous_state() {
        use LegacyValue::{Float, Str};
        let mut source = source();
        set_documents(&source, r#"{"speed":12,"compass":10}"#, r#"{"valid":true,"Ny":2}"#);
        let mut http = ScriptedHttp::default();
        http.reply(IND, 200, r#"{"speed":27,"compass":35}"#);
        http.reply(ST, 200, "{bad");
        assert!(!poll_once(&mut http, IND, ST, &source.documents));
        assert!(!source.is_running().unwrap());
        assert_eq!(source.native_channel("speed"), Some(Str("27".into())));
        assert_eq!(source.native_channel("yaw"), Some(LegacyValue::Int(35)));
        assert_eq!(source.native_channel("vertical_acceleration"), Some(LegacyValue::Int(1)));
        http.reply(IND, 200, "{bad");
        http.reply(ST, 200, r#"{"valid":true,"Ny":5}"#);
        assert!(!poll_once(&mut http, IND, ST, &source.documents));
        assert_eq!(source.native_channel("speed"), Some(Str("27".into())));
        assert_eq!(source.native_channel("vertical_acceleration"), Some(LegacyValue::Int(1)));
        http.reply(IND, 200, r#"{"speed":50,"compass":20}"#);
        http.reply(ST, 200, r#"{"valid":true,"Ny":3.5}"#);
        assert!(poll_once(&mut http, IND, ST, &source.documents));
        assert_eq!(source.native_channel("vertical_acceleration"), Some(Float(2.5)));
    }

    #[test]
    fn poll_requires_both_documents() {
        let docs = Mutex::new(Documents::default());
        let mut http = ScriptedHttp::default();
        http.reply(IND, 200, r#"{"speed": 1}"#);
        http.reply(ST, 500, r#"{}"#);
        assert!(!poll_once(&mut http, IND, ST, &docs));
        http.reply(IND, 200, r#"{"speed": 1}"#);
        http.reply(ST, 200, r#"{"valid": true}"#);
        assert!(poll_once(&mut http, IND, ST, &docs));
        http.reply(IND, 200, "oops");
        http.reply(ST, 200, r#"{"valid": true}"#);
        assert!(!poll_once(&mut http, IND, ST, &docs));
        assert!(docs.lock().unwrap().indicators.is_some());
    }

    /// A transport failure of the `/indicators` GET skips the `/state` GET.
    #[test]
    fn failed_indicators_request_skips_state_request() {
        let docs = Mutex::new(Documents::default());
        let mut http = ScriptedHttp::default();
        http.fail(IND);
        http.reply(ST, 200, r#"{"valid": true}"#);
        assert!(!poll_once(&mut http, IND, ST, &docs));
        assert_eq!(*http.requested.lock().unwrap(), [IND]);
        // A non-200 answer is not a transport failure: both requests are made.
        http.reply(IND, 404, "{}");
        assert!(!poll_once(&mut http, IND, ST, &docs));
        assert_eq!(*http.requested.lock().unwrap(), [IND, IND, ST]);
    }

    fn documents(indicators: &str, state: &str) -> Documents {
        Documents {
            indicators: Some(super::super::python::json::loads(indicators).unwrap()),
            state: Some(super::super::python::json::loads(state).unwrap()),
            connected: true,
        }
    }

    fn source() -> WarThunder {
        WarThunder {
            documents: Arc::default(),
            surge: Mutex::default(),
            rates: AngularRates::default(),
            stop: Arc::new(AtomicBool::new(true)),
            poller: None,
        }
    }

    fn set_documents(source: &WarThunder, indicators: &str, state: &str) {
        *source.documents.lock().unwrap() = documents(indicators, state);
    }

    #[test]
    fn native_gate_and_branch_read_independently_published_documents() {
        use std::collections::VecDeque;
        let source = source();
        for (channel, reads, expected) in [
            (
                "yaw",
                vec![
                    (DocumentField::State("valid"), Json::Bool(true)),
                    (DocumentField::Indicator("compass"), Json::Int(123)),
                ],
                LegacyValue::Int(123),
            ),
            (
                "vertical_acceleration",
                vec![(DocumentField::State("valid"), Json::Bool(true)), (DocumentField::State("Ny"), Json::Float(4.5))],
                LegacyValue::Float(3.5),
            ),
            ("max_rpm", vec![(DocumentField::State("valid"), Json::Bool(false))], LegacyValue::Int(0)),
            ("max_rpm", vec![(DocumentField::State("valid"), Json::Int(0))], LegacyValue::Str("20000".into())),
        ] {
            let mut reads = VecDeque::from(reads);
            assert_eq!(
                source.native_with(channel, |field| {
                    let (expected, value) = reads.pop_front().expect("unexpected document read");
                    assert_eq!(field, expected);
                    Some(value)
                }),
                NativeRead::Value(expected)
            );
            assert!(reads.is_empty());
        }
    }

    #[test]
    fn native_surge_samples_current_speed_again_before_assigning_previous() {
        use std::collections::VecDeque;
        let source = source();
        *source.surge.lock().unwrap() =
            SurgeMemory { previous_speed: Some(LegacyValue::Int(10)), previous_change: None };
        let mut reads = VecDeque::from([
            (DocumentField::State("valid"), Json::Bool(true)),
            (DocumentField::Indicator("speed"), Json::Int(12)),
            (DocumentField::Indicator("speed"), Json::Int(20)),
        ]);
        assert_eq!(
            source.native_with("longitudinal_acceleration", |field| {
                let (expected, value) = reads.pop_front().expect("unexpected document read");
                assert_eq!(field, expected);
                Some(value)
            }),
            NativeRead::Value(LegacyValue::Int(2))
        );
        assert!(reads.is_empty());
        assert_eq!(source.surge.lock().unwrap().previous_speed, Some(LegacyValue::Int(20)));
        assert_eq!(source.surge.lock().unwrap().previous_change, Some(LegacyValue::Int(2)));
    }

    #[test]
    fn native_surge_failed_second_read_keeps_both_previous_values() {
        use std::collections::VecDeque;
        let source = source();
        let before =
            SurgeMemory { previous_speed: Some(LegacyValue::Int(10)), previous_change: Some(LegacyValue::Int(3)) };
        *source.surge.lock().unwrap() = before.clone();
        let mut reads = VecDeque::from([
            (DocumentField::State("valid"), Some(Json::Bool(true))),
            (DocumentField::Indicator("speed"), Some(Json::Int(12))),
            (DocumentField::Indicator("speed"), None),
        ]);
        assert_eq!(
            source.native_with("longitudinal_acceleration", |field| {
                let (expected, value) = reads.pop_front().expect("unexpected document read");
                assert_eq!(field, expected);
                value
            }),
            NativeRead::Value(LegacyValue::Int(0))
        );
        assert!(reads.is_empty());
        assert_eq!(*source.surge.lock().unwrap(), before);
    }

    #[test]
    fn native_getters_keep_text_raw_and_computed_numeric_kinds() {
        use LegacyValue::{Float, Int, Str};
        let s = source();
        set_documents(
            &s,
            r#"{"speed":99.9999996,"rpm":9007199254740993,"compass":271,"turn":1.5,"type":"spitfire_mk1","aviahorizon_roll":-100,"aviahorizon_pitch":10.0}"#,
            r#"{"valid":true,"AoA, deg":2.5,"Wx, deg/s":-4,"elevator, %":30,"Ny":1}"#,
        );
        for (name, expected) in [
            ("speed", Str("100".into())),
            ("rpm", Str("9007199254740993".into())),
            ("max_rpm", Str("20000".into())),
            ("car_name", Str("SPITFIRE_MK1".into())),
            ("yaw", Int(271)),
            ("yaw_rate", Float(15.0)),
            ("pitch_rate", Int(30)),
            ("roll_rate", Int(-4)),
            ("lateral_acceleration", Float(2.5)),
            ("vertical_acceleration", Int(0)),
            ("roll", Int(80)),
            ("pitch", Float(-10.0)),
            ("unknown", Int(0)),
        ] {
            assert_eq!(s.read_native(name), NativeRead::Value(expected), "{name}");
        }
    }

    #[test]
    fn native_acceleration_memory_advances_on_getters_and_keeps_native_delta_kinds() {
        use LegacyValue::{Float, Int, Str};
        let mut s = source();
        set_documents(&s, r#"{"speed":10}"#, r#"{"valid":true}"#);
        let mut t = Telemetry::off();
        t.longitudinal_acceleration = 71.0;
        assert!(s.receive_native(&mut t).unwrap());
        assert_eq!(t.longitudinal_acceleration, 71.0);
        set_documents(&s, r#"{"speed":12}"#, r#"{"valid":true}"#);
        assert_eq!(s.read_native("speed"), NativeRead::Value(Str("12".into())));
        set_documents(&s, r#"{"speed":20}"#, r#"{"valid":true}"#);
        assert_eq!(
            s.read_native("longitudinal_acceleration"),
            NativeRead::Value(Int(0)),
            "neither receive nor speed reads consumed the first acceleration sample"
        );
        for (speed, expected) in [("23", Int(3)), ("23", Int(3)), ("23.0", Int(3)), ("24.5", Float(1.5))] {
            set_documents(&s, &format!(r#"{{"speed":{speed}}}"#), r#"{"valid":true}"#);
            assert_eq!(s.read_native("longitudinal_acceleration"), NativeRead::Value(expected));
        }
        set_documents(&s, r#"{"speed":100}"#, r#"{"valid":false}"#);
        assert_eq!(s.read_native("longitudinal_acceleration"), NativeRead::Value(Int(0)));
        set_documents(&s, r#"{"speed":25.5}"#, r#"{"valid":true}"#);
        assert_eq!(
            s.read_native("longitudinal_acceleration"),
            NativeRead::Value(Float(1.0)),
            "the false-state gate did not change previous_speed"
        );
    }

    #[test]
    fn native_local_catches_false_identity_gate_and_sequence_multiplication() {
        use LegacyValue::{Int, List, None as Null, Str};
        let s = source();
        for valid in ["0", "0.0", "null", "\"\""] {
            set_documents(
                &s,
                r#"{"speed":NaN,"rpm":Infinity,"turn":"1"}"#,
                &format!(r#"{{"valid":{valid},"Ny":1.0,"elevator, %":null}}"#),
            );
            assert_eq!(s.read_native("max_rpm"), NativeRead::Value(Str("20000".into())));
            assert_eq!(s.read_native("speed"), NativeRead::Value(Int(0)));
            assert_eq!(s.read_native("rpm"), NativeRead::Value(Int(0)));
            assert_eq!(s.read_native("yaw_rate"), NativeRead::Value(Str("1111111111".into())));
            assert_eq!(s.read_native("pitch_rate"), NativeRead::Value(Null));
            assert_eq!(s.read_native("vertical_acceleration"), NativeRead::Value(LegacyValue::Float(0.0)));
        }
        set_documents(&s, r#"{"turn":[1]}"#, r#"{"valid":true}"#);
        assert_eq!(s.read_native("yaw_rate"), NativeRead::Value(List(vec![Int(1); 10])));
        s.documents.lock().unwrap().indicators = None;
        assert_eq!(
            s.read_native("max_rpm"),
            NativeRead::Value(Str("20000".into())),
            "constant does not access indicators"
        );
        s.documents.lock().unwrap().state = Some(super::super::python::json::loads(r#"{"valid":false}"#).unwrap());
        assert_eq!(s.read_native("max_rpm"), NativeRead::Value(Int(0)));
    }

    #[test]
    fn native_surge_failed_subtraction_preserves_the_previous_raw_sample() {
        use LegacyValue::{Float, Int, Str};
        let mut memory = SurgeMemory::default();
        assert_eq!(memory.read(Int(10)), Some(Int(0)));
        assert_eq!(memory.read(Str("bad".into())), None);
        assert_eq!(memory.read(Int(12)), Some(Int(2)));
        assert_eq!(memory.read(Float(12.0)), Some(Int(2)));
        let mut memory = SurgeMemory::default();
        assert_eq!(memory.read(Str("bad".into())), Some(Int(0)), "first sample is stored without arithmetic");
        assert_eq!(memory.read(Int(10)), None);
        assert_eq!(memory.read(Int(11)), None, "the failed subtraction did not replace the raw text sample");
    }

    #[test]
    fn source_adapter_observes_live_documents_without_an_early_acceleration_read() {
        use crate::derive::{Listener, SourceListener};
        let source = source();
        set_documents(&source, r#"{"speed":10}"#, r#"{"valid":true}"#);
        let shared = source.documents.clone();
        let mut listener = SourceListener::new(source);
        listener.receive();
        *shared.lock().unwrap() = documents(r#"{"speed":12}"#, r#"{"valid":true}"#);
        assert_eq!(listener.native_channel("longitudinal_acceleration"), Some(LegacyValue::Int(0)));
        *shared.lock().unwrap() = documents(r#"{"speed":14}"#, r#"{"valid":true}"#);
        assert_eq!(listener.native_channel("longitudinal_acceleration"), Some(LegacyValue::Int(2)));
    }

    /// Channel mapping and the gate on a false `valid`.
    #[test]
    fn get_channel_mapping() {
        let mut s = source();
        let docs = documents(
            r#"{"speed": 99.9999996, "rpm": 2400.4, "compass": 271.5, "turn": 1.5, "type": "spitfire_mk1",
                "aviahorizon_roll": -100.0, "aviahorizon_pitch": 10.0}"#,
            r#"{"valid": true, "AoA, deg": 2.5, "Wx, deg/s": -4.0, "elevator, %": 30, "Ny": 1.25}"#,
        );
        let mut t = Telemetry::off();
        s.write_channels(&docs, &mut t);
        assert_eq!((t.speed, t.rpm, t.yaw, t.max_rpm), (100.0, 2400.0, 271.5, 20000.0));
        assert_eq!((t.lateral_acceleration, t.vertical_acceleration), (2.5, 0.25));
        assert_eq!((t.roll, t.pitch), (80.0, -10.0));
        assert_eq!(t.longitudinal_acceleration, 0.0);
        assert_eq!(t.vehicle, "SPITFIRE_MK1");
        assert_eq!(s.angular_rates(), AngularRates { yaw: 15.0, pitch: 30.0, roll: -4.0 });

        let invalid = documents(r#"{"speed": 50}"#, r#"{"valid": false}"#);
        s.write_channels(&invalid, &mut t);
        assert_eq!((t.speed, t.max_rpm), (0.0, 0.0));
        // Only the literal false closes the gate: 0 does not.
        let zero_valid = documents(r#"{"speed": 50}"#, r#"{"valid": 0}"#);
        s.write_channels(&zero_valid, &mut t);
        assert_eq!(t.speed, 50.0);
    }

    /// longitudinal_acceleration: first call 0, then speed deltas, the last non-zero
    /// delta while the speed holds.
    #[test]
    fn surge_memory() {
        let mut m = SurgeMemory::default();
        assert_eq!(m.next(100.0), 0.0);
        assert_eq!(m.next(100.0), 0.0);
        assert_eq!(m.next(103.0), 3.0);
        assert_eq!(m.next(103.0), 3.0);
        assert_eq!(m.next(101.0), -2.0);
    }
}
