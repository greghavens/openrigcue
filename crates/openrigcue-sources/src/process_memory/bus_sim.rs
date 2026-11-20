//! Fernbus Simulator, The Bus, Tourist Bus Simulator: the game's telemetry web interface
//! `http://localhost:<port>/Vehicles/Current` (listener key `bussim`).

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::http::{HttpClient, HttpGet};
use super::python::json::{loads_bytes, Json};
use crate::udp_flight::{clear_listener_channels, AngularRates, FlightSource};
use crate::{Source, SourceError};

/// `NETWORK_PORT/bussim` default.
pub const DEFAULT_PORT: u16 = 37337;
/// Timeout of one GET request.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(100);

/// The bus simulator listener.
pub struct BusSim<H: HttpGet = HttpClient> {
    http: H,
    url: String,
    latest: Option<Json>,
    connected: bool,
}

impl BusSim<HttpClient> {
    pub fn new(port: u16) -> Self {
        Self::with_http(HttpClient::new(REQUEST_TIMEOUT), port)
    }

    /// The URL uses the configured `NETWORK_PORT/bussim` text as it is, without converting it to an integer.
    pub fn from_port_text(port: String) -> Self {
        Self::with_http_port_text(HttpClient::new(REQUEST_TIMEOUT), port)
    }
}

impl<H: HttpGet> BusSim<H> {
    pub fn with_http(http: H, port: u16) -> Self {
        Self::with_http_port_text(http, port.to_string())
    }

    pub fn with_http_port_text(http: H, port: String) -> Self {
        BusSim { http, url: format!("http://localhost:{port}/Vehicles/Current"), latest: None, connected: false }
    }

    /// A channel read: every answer comes from the last JSON document and any failure reads 0.
    fn write_channels(document: &Json, t: &mut Telemetry) {
        let previous_vehicle = t.vehicle.clone();
        clear_listener_channels(t);
        let get = |path: &[&str]| path.iter().try_fold(document, |node, key| node.get(key));
        let int_or_zero = |key: &str| get(&[key]).and_then(Json::python_int).unwrap_or(0.0);
        let raw_or_zero = |path: &[&str]| get(path).and_then(Json::channel_value).unwrap_or(0.0);
        let suspension = |wheel: usize| {
            document
                .get("Wheels")
                .and_then(|wheels| wheels.index(wheel))
                .and_then(|w| w.get("CurrentSuspension"))
                .and_then(Json::number)
                .map_or(0.0, |travel| travel / 1000.0)
        };
        // Speed in the game's unit (km/h), not converted.
        t.speed = int_or_zero("Speed");
        t.rpm = int_or_zero("RPM");
        t.max_rpm = int_or_zero("MaxRPM");
        t.yaw = raw_or_zero(&["Rotation", "Yaw"]);
        t.roll = raw_or_zero(&["Rotation", "Roll"]);
        t.pitch = raw_or_zero(&["Rotation", "Pitch"]);
        t.gear = raw_or_zero(&["Gearbox", "CurrentGear"]);
        // Wheel order of the listener: 0 lf, 1 lr, 2 rf, 3 rr.
        t.suspension_travel_lf = suspension(0);
        t.suspension_travel_lr = suspension(1);
        t.suspension_travel_rf = suspension(2);
        t.suspension_travel_rr = suspension(3);
        t.steering_wheel_angle = raw_or_zero(&["Steering"]);
        t.vehicle = match document.get("ActorName") {
            Some(Json::Str(name)) => name.clone(),
            // A non-string name is not a usable vehicle id.
            Some(_) => previous_vehicle,
            None => String::new(),
        };
    }
}

impl<H: HttpGet> Source for BusSim<H> {
    fn id(&self) -> &'static str {
        "bussim"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int};
        let Some(document) = self.latest.as_ref().filter(|_| self.connected) else { return Some(Int(0)) };
        let get = |path: &[&str]| path.iter().try_fold(document, |node, key| node.get(key));
        let answer = match name {
            "speed" | "rpm" | "max_rpm" => {
                let key = match name {
                    "speed" => "Speed",
                    "rpm" => "RPM",
                    _ => "MaxRPM",
                };
                get(&[key]).and_then(Json::native_integer)
            }
            "yaw" | "roll" | "pitch" => {
                let key = match name {
                    "yaw" => "Yaw",
                    "roll" => "Roll",
                    _ => "Pitch",
                };
                get(&["Rotation", key]).map(Json::native)
            }
            "gear" => get(&["Gearbox", "CurrentGear"]).map(Json::native),
            "steering_wheel_angle" => get(&["Steering"]).map(Json::native),
            "car_name" => get(&["ActorName"]).map(Json::native),
            "suspension_travel_lf" | "suspension_travel_lr" | "suspension_travel_rf" | "suspension_travel_rr" => {
                let i = match name {
                    "suspension_travel_lf" => 0,
                    "suspension_travel_lr" => 1,
                    "suspension_travel_rf" => 2,
                    _ => 3,
                };
                document
                    .get("Wheels")
                    .and_then(|wheels| wheels.index(i))
                    .and_then(|wheel| wheel.get("CurrentSuspension"))
                    .and_then(Json::number)
                    .map(|travel| Float(travel / 1000.0))
            }
            _ => None,
        };
        Some(answer.unwrap_or(Int(0)))
    }

    /// The liveness test: one GET per call (any status code), JSON body, connected when the
    /// document has an `ActorName` entry. A failed request keeps the previous document.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let document = self.http.get(&self.url, &[]).ok().and_then(|reply| loads_bytes(&reply.body).ok());
        let Some(document) = document else {
            self.connected = false;
            return Ok(false);
        };
        self.connected = document.get("ActorName").is_some();
        self.latest = Some(document);
        Ok(self.connected)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    /// Numeric compatibility view (all 0 unless connected).
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        match (&self.latest, self.connected) {
            (Some(document), true) => Self::write_channels(document, t),
            _ => clear_listener_channels(t),
        }
        Ok(true)
    }
}

impl<H: HttpGet> FlightSource for BusSim<H> {
    fn angular_rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_memory::http::testing::ScriptedHttp;

    const URL: &str = "http://localhost:37337/Vehicles/Current";

    fn source() -> (BusSim<ScriptedHttp>, ScriptedHttp) {
        let http = ScriptedHttp::default();
        (BusSim::with_http(http.clone(), DEFAULT_PORT), http)
    }

    #[test]
    fn native_url_keeps_port_text_and_prior_response_on_request_failure() {
        for text in ["0037337", "+37337", " 37337 ", "70000", "-1", "bad", "True", "None"] {
            let http = ScriptedHttp::default();
            let url = format!("http://localhost:{text}/Vehicles/Current");
            let mut source = BusSim::with_http_port_text(http.clone(), text.into());
            assert!(http.requested.lock().unwrap().is_empty(), "construction does not send requests");
            http.reply(&url, 404, r#"{"ActorName":"Bus","Speed":12}"#);
            assert!(source.is_running().unwrap(), "status code is not a liveness gate");
            assert_eq!(source.native_channel("speed"), Some(LegacyValue::Int(12)));
            let prior = source.latest.clone();
            http.fail(&url);
            assert!(!source.is_running().unwrap());
            assert_eq!(source.latest, prior);
            assert_eq!(source.native_channel("speed"), Some(LegacyValue::Int(0)));
            http.reply(&url, 200, r#"{"ActorName":"Bus","Speed":25}"#);
            assert!(source.is_running().unwrap());
            assert_eq!(source.native_channel("speed"), Some(LegacyValue::Int(25)));
            assert_eq!(*http.requested.lock().unwrap(), [url.clone(), url.clone(), url]);
        }
    }

    #[test]
    fn native_json_kinds_integer_precision_and_local_catches() {
        use LegacyValue::{Bool, Float, Int, None as Null, Str};
        let (mut s, http) = source();
        assert_eq!(s.native_channel("speed"), Some(Int(0)));
        http.reply(URL, 200, r#"{"ActorName":"Bus","Speed":true,"RPM":9007199254740993,"MaxRPM":"１２_٣٤","Rotation":{"Yaw":12,"Pitch":null,"Roll":false},"Gearbox":{"CurrentGear":"3"},"Steering":-0.0,"Wheels":[{"CurrentSuspension":100},{"CurrentSuspension":true},{"CurrentSuspension":null}]}"#);
        assert!(s.is_running().unwrap());
        for (name, expected) in [
            ("speed", Int(1)),
            ("rpm", Int(9_007_199_254_740_993)),
            ("max_rpm", Int(1234)),
            ("yaw", Int(12)),
            ("pitch", Null),
            ("roll", Bool(false)),
            ("gear", Str("3".into())),
            ("car_name", Str("Bus".into())),
            ("suspension_travel_lf", Float(0.1)),
            ("suspension_travel_lr", Float(0.001)),
            ("suspension_travel_rf", Int(0)),
            ("unknown", Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(expected), "{name}");
        }
        assert!(
            matches!(s.native_channel("steering_wheel_angle"), Some(Float(v)) if v.to_bits() == (-0.0f64).to_bits())
        );
        http.reply(URL, 200, r#"{"ActorName":null,"Speed":NaN,"RPM":Infinity,"MaxRPM":null}"#);
        assert!(s.is_running().unwrap());
        assert_eq!(s.native_channel("car_name"), Some(Null));
        for name in ["speed", "rpm", "max_rpm"] {
            assert_eq!(s.native_channel(name), Some(Int(0)), "caught {name}");
        }
        http.reply(URL, 200, &format!(r#"{{"ActorName":"Bus","Speed":2,"RPM":"{}"}}"#, "0".repeat(4301)));
        assert!(s.is_running().unwrap());
        assert_eq!(s.native_channel("rpm"), Some(Int(0)), "digit-limit failure reaches the local catch");
        assert_eq!(s.native_channel("speed"), Some(Int(2)));
        http.fail(URL);
        assert!(!s.is_running().unwrap());
        assert_eq!(s.native_channel("car_name"), Some(Int(0)), "disconnected gate precedes retained document");
    }

    /// Liveness: connection failure, non-JSON body and a document without ActorName all fail;
    /// the status code is not checked.
    #[test]
    fn check_status_paths() {
        let (mut s, http) = source();
        http.fail(URL);
        assert!(!s.is_running().unwrap());
        http.reply(URL, 200, "not json");
        assert!(!s.is_running().unwrap());
        http.reply(URL, 200, r#"{"Speed": 10}"#);
        assert!(!s.is_running().unwrap());
        http.reply(URL, 404, r#"{"ActorName": "Bus"}"#);
        assert!(s.is_running().unwrap());
    }

    /// Channels: integer Speed/RPM/MaxRPM, raw rotation/gear/steering, /1000 suspension in the
    /// listener's wheel order, name from ActorName, 0 for missing values.
    #[test]
    fn get_channel_mapping() {
        let (mut s, http) = source();
        http.reply(
            URL,
            200,
            r#"{"ActorName": "MAN Lion's Coach", "Speed": 54.9, "RPM": 1500.7, "MaxRPM": 2400,
                "Rotation": {"Pitch": 1.5, "Roll": -2.0, "Yaw": 180.0}, "Gearbox": {"CurrentGear": 3},
                "Steering": -0.25,
                "Wheels": [{"CurrentSuspension": 100}, {"CurrentSuspension": 200},
                           {"CurrentSuspension": 300}, {"CurrentSuspension": 400}]}"#,
        );
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.speed, t.rpm, t.max_rpm), (54.0, 1500.0, 2400.0));
        assert_eq!((t.pitch, t.roll, t.yaw, t.gear), (1.5, -2.0, 180.0, 3.0));
        assert_eq!(t.steering_wheel_angle, -0.25);
        assert_eq!(
            (t.suspension_travel_lf, t.suspension_travel_lr, t.suspension_travel_rf, t.suspension_travel_rr),
            (0.1, 0.2, 0.3, 0.4)
        );
        assert_eq!(t.vehicle, "MAN Lion's Coach");

        http.reply(URL, 200, r#"{"ActorName": "X", "Speed": "fast"}"#);
        assert!(s.is_running().unwrap());
        s.poll(&mut t).unwrap();
        assert_eq!((t.speed, t.rpm, t.suspension_travel_lf), (0.0, 0.0, 0.0));
    }

    /// Every channel reads 0 while not connected.
    #[test]
    fn disconnected_reads_zero() {
        let (mut s, http) = source();
        http.reply(URL, 200, r#"{"ActorName": "Bus", "Speed": 30}"#);
        assert!(s.is_running().unwrap());
        http.fail(URL);
        assert!(!s.is_running().unwrap());
        let mut t = Telemetry::off();
        t.speed = 9.0;
        s.poll(&mut t).unwrap();
        assert_eq!(t.speed, 0.0);
    }
}
