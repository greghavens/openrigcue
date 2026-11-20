//! The HUD driver against a fake BLE stack: no adapter, bus or radio is touched.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use openrigcue_model::Telemetry;

use super::*;

/// The fake HUD and radio, shared by the fake central and its connections.
#[derive(Default)]
struct Bench {
    devices: Vec<Advertiser>,
    discover_error: Option<BleError>,
    services: Vec<Service>,
    revision: Vec<u8>,
    connected: bool,
    /// Response writes that fail before the next one succeeds.
    failing_writes: usize,
    /// The HUD answers every chunk with a notification.
    notify_on_write: bool,
    pending_notification: bool,
    /// The HUD drops each link after this many telemetry lines.
    lines_before_drop: Option<usize>,
    /// Telemetry lines on the current link.
    link_lines: usize,
    lines: Vec<String>,
    chunks: Vec<usize>,
    events: Vec<String>,
}

type SharedBench = Arc<Mutex<Bench>>;

struct FakeCentral(SharedBench);

struct FakeLink(SharedBench);

impl Central for FakeCentral {
    fn discover(&mut self) -> Result<Vec<Advertiser>, BleError> {
        let mut b = self.0.lock().unwrap();
        b.events.push("discover".into());
        match b.discover_error.clone() {
            Some(error) => Err(error),
            None => Ok(b.devices.clone()),
        }
    }

    fn connect(&mut self, address: &str) -> Result<Box<dyn Peripheral>, BleError> {
        let mut b = self.0.lock().unwrap();
        b.events.push(format!("connect {address}"));
        b.connected = true;
        b.link_lines = 0;
        Ok(Box::new(FakeLink(Arc::clone(&self.0))))
    }
}

impl Peripheral for FakeLink {
    fn services(&mut self) -> Result<Vec<Service>, BleError> {
        Ok(self.0.lock().unwrap().services.clone())
    }

    fn read(&mut self, uuid: &str) -> Result<Vec<u8>, BleError> {
        let mut b = self.0.lock().unwrap();
        b.events.push(format!("read {uuid}"));
        Ok(b.revision.clone())
    }

    fn write(&mut self, characteristic: &Characteristic, data: &[u8], with_response: bool) -> Result<(), BleError> {
        let mut b = self.0.lock().unwrap();
        if with_response {
            assert_eq!(characteristic.uuid, FIRMWARE_FILE_CHARACTERISTIC);
            if b.failing_writes > 0 {
                b.failing_writes -= 1;
                b.events.push("chunk failed".into());
                return Err(BleError("write failed".into()));
            }
            b.chunks.push(data.len());
            b.pending_notification = b.notify_on_write;
        } else {
            assert_eq!(characteristic.uuid, SENDER_CHARACTERISTIC);
            b.lines.push(String::from_utf8(data.to_vec()).unwrap());
            b.link_lines += 1;
            if b.lines_before_drop == Some(b.link_lines) {
                b.connected = false;
            }
        }
        Ok(())
    }

    fn start_notify(&mut self, characteristic: &Characteristic) -> Result<(), BleError> {
        self.0.lock().unwrap().events.push(format!("notify {}", characteristic.uuid));
        Ok(())
    }

    fn take_notification(&mut self) -> bool {
        std::mem::take(&mut self.0.lock().unwrap().pending_notification)
    }

    fn is_connected(&mut self) -> bool {
        self.0.lock().unwrap().connected
    }

    fn disconnect(&mut self) -> Result<(), BleError> {
        let mut b = self.0.lock().unwrap();
        b.events.push("disconnect".into());
        b.connected = false;
        Ok(())
    }
}

/// What the service exposes to the driver, changed by `on_sleep` between telemetry lines.
#[derive(Clone)]
struct HostState {
    rate: u32,
    model: String,
    switch: bool,
    game: String,
    telemetry: Telemetry,
}

/// What a wait does to the host state; `false` stops the service.
type SleepHook = Box<dyn FnMut(&mut HostState, usize) -> bool + Send>;

struct FakeHost {
    state: Mutex<HostState>,
    firmware: Option<Vec<u8>>,
    statuses: Mutex<Vec<HudStatus>>,
    sleeps: Mutex<Vec<Duration>>,
    /// Called at every wait with the state and the wait number.
    on_sleep: Mutex<SleepHook>,
}

impl FakeHost {
    fn new() -> FakeHost {
        FakeHost {
            state: Mutex::new(HostState {
                rate: 5,
                model: "H".into(),
                switch: true,
                game: "none".into(),
                telemetry: Telemetry::off(),
            }),
            firmware: None,
            statuses: Mutex::new(Vec::new()),
            sleeps: Mutex::new(Vec::new()),
            on_sleep: Mutex::new(Box::new(|_, _| true)),
        }
    }

    fn on_sleep(self, f: impl FnMut(&mut HostState, usize) -> bool + Send + 'static) -> FakeHost {
        *self.on_sleep.lock().unwrap() = Box::new(f);
        self
    }

    fn statuses(&self) -> Vec<String> {
        self.statuses.lock().unwrap().iter().map(HudStatus::text).collect()
    }
}

impl HudHost for FakeHost {
    fn refresh_rate(&self) -> u32 {
        self.state.lock().unwrap().rate
    }
    fn model(&self) -> String {
        self.state.lock().unwrap().model.clone()
    }
    fn hud_switch(&self) -> bool {
        self.state.lock().unwrap().switch
    }
    fn game_running(&self) -> String {
        self.state.lock().unwrap().game.clone()
    }
    fn telemetry(&self) -> Telemetry {
        self.state.lock().unwrap().telemetry.clone()
    }
    fn set_status(&self, status: HudStatus) {
        self.statuses.lock().unwrap().push(status);
    }
    fn translate(&self, text: &str) -> String {
        text.to_owned()
    }
    fn firmware_image(&self) -> std::io::Result<Vec<u8>> {
        self.firmware.clone().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    }
    fn sleep(&self, duration: Duration) -> bool {
        let n = {
            let mut sleeps = self.sleeps.lock().unwrap();
            sleeps.push(duration);
            sleeps.len()
        };
        let mut state = self.state.lock().unwrap();
        (self.on_sleep.lock().unwrap())(&mut state, n)
    }
}

fn hud_service(with_file: bool) -> Service {
    let mut characteristics = vec![
        Characteristic { uuid: RECEIVER_CHARACTERISTIC.into(), handle: "c0".into() },
        Characteristic { uuid: SENDER_CHARACTERISTIC.into(), handle: "c1".into() },
    ];
    if with_file {
        characteristics.push(Characteristic { uuid: FIRMWARE_FILE_CHARACTERISTIC.into(), handle: "c2".into() });
    }
    Service { uuid: HUD_SERVICE.into(), characteristics }
}

fn hud_bench(names: &[&str], revision: &str) -> SharedBench {
    Arc::new(Mutex::new(Bench {
        devices: names
            .iter()
            .enumerate()
            .map(|(i, n)| Advertiser { address: format!("AA:{i:02}"), name: Some((*n).into()) })
            .collect(),
        services: vec![
            Service { uuid: "00001800-0000-1000-8000-00805f9b34fb".into(), characteristics: vec![] },
            hud_service(true),
        ],
        revision: revision.as_bytes().to_vec(),
        notify_on_write: true,
        lines_before_drop: Some(3),
        ..Bench::default()
    }))
}

/// Field order, integer truncation, speed `* 3.6`, 0 for NaN/inf, VACUUM always 0.
#[test]
fn telemetry_line_format() {
    let mut t = Telemetry::off();
    t.speed = 34.2;
    t.water_temperature = 90.9;
    t.rpm = 4500.7;
    t.turbo_boost_pressure = -0.4;
    t.fuel_level = 55.0;
    t.oil_temperature = 95.2;
    t.oil_pressure = 4.99;
    assert_eq!(hud_line(1, &t), "1~123~90~4500~0~55~95~4~0");
    t.rpm = f64::NAN;
    t.speed = f64::INFINITY;
    t.fuel_level = -12.9;
    assert_eq!(hud_line(0, &t), "0~0~90~0~0~-12~95~4~0");
    assert_eq!(hud_line(0, &Telemetry::off()), "0~0~0~0~0~0~0~0~0");
    assert_eq!(py_int_text(1e30).as_deref(), Some("1000000000000000019884624838656"));
}

/// The percentage is `100 - remaining / total * 100`, truncated.
#[test]
fn flashing_percent_and_status_text() {
    assert_eq!([flash_percent(1300, 1300), flash_percent(788, 1300), flash_percent(276, 1300)], [0, 39, 78]);
    let status = HudStatus::Flashing { prefix: FLASHING_PREFIX.into(), percent: 39 };
    assert_eq!(status.text(), "Installing new firmware - progress: 39%");
    assert_eq!(HudStatus::default().text(), "Not Connected");
    assert_eq!(HudStatus::Connected.text(), "Connected");
}

/// Up-to-date HUD: services listed, revision read (last character dropped), 'Connected', one line per
/// `1 / refresh_rate` s written without response while connected, then 'Not Connected'.
#[test]
fn up_to_date_hud_streams_telemetry() {
    let bench = hud_bench(&["obd2hud-1234"], "1.0.0\0");
    let host = FakeHost::new().on_sleep(|s, n| {
        s.telemetry.rpm = 1000.0 * n as f64;
        true
    });
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    let b = bench.lock().unwrap();
    assert_eq!(b.lines, ["0~0~0~1000~0~0~0~0~0", "0~0~0~2000~0~0~0~0~0", "0~0~0~3000~0~0~0~0~0"]);
    assert_eq!(
        b.events,
        ["discover", "connect AA:00", &format!("read {FIRMWARE_REVISION_CHARACTERISTIC}"), "disconnect"]
    );
    assert!(b.chunks.is_empty());
    assert_eq!(host.statuses(), ["Connected", "Not Connected"]);
    assert_eq!(*host.sleeps.lock().unwrap(), vec![Duration::from_millis(200); 3]);
}

#[test]
fn hud_stream_uses_native_timeout_rounding() {
    let bench = hud_bench(&["obd2hud-1234"], "1.0.0\0");
    let host = FakeHost::new();
    host.state.lock().unwrap().rate = 30;
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    let nanos = if cfg!(windows) { 33_333_400 } else { 33_333_334 };
    assert_eq!(*host.sleeps.lock().unwrap(), vec![Duration::from_nanos(nanos); 3]);
}

/// Only names starting with `OBD2HUD-` (any case) are tried; every one of them is.
#[test]
fn devices_are_matched_by_name_prefix() {
    let bench = hud_bench(&["Speaker", "OBD2HUD-A", "xOBD2HUD-", "Obd2Hud-B"], "1.0\0");
    scan(&mut FakeCentral(Arc::clone(&bench)), &FakeHost::new()).unwrap();
    let connects: Vec<String> =
        bench.lock().unwrap().events.iter().filter(|e| e.starts_with("connect")).cloned().collect();
    assert_eq!(connects, ["connect AA:01", "connect AA:03"]);
}

/// A nameless device and a failing discovery end the scan with an error.
#[test]
fn unnamed_device_or_failed_discovery_fails_the_scan() {
    let bench = hud_bench(&["OBD2HUD-A"], "1.0.0\0");
    bench.lock().unwrap().devices.insert(0, Advertiser { address: "BB".into(), name: None });
    let host = FakeHost::new();
    assert_eq!(scan(&mut FakeCentral(Arc::clone(&bench)), &host), Err(ScanError::UnnamedDevice("BB".into())));
    assert!(!bench.lock().unwrap().events.iter().any(|e| e.starts_with("connect")));
    let failing = hud_bench(&[], "");
    failing.lock().unwrap().discover_error = Some(BleError("No Bluetooth adapters found.".into()));
    assert!(matches!(scan(&mut FakeCentral(failing), &host), Err(ScanError::Failed(_))));
}

/// `POWER`: 0 at scan start with no previous game; with the HUD button on it
/// follows whether a game runs, only when the running game changes; the button off forces 0 (and turning it
/// back on during the same game keeps 0); model 'B' always sends 1.
#[test]
fn power_field_follows_game_changes() {
    let bench = hud_bench(&["OBD2HUD-A"], "1.0.0\0");
    bench.lock().unwrap().lines_before_drop = Some(7);
    let host = FakeHost::new().on_sleep(|s, n| {
        match n {
            2 => s.game = "dirtrally2".into(),
            4 => s.switch = false,
            5 => s.switch = true,
            6 => s.game = "none".into(),
            7 => s.model = "B".into(),
            _ => {}
        }
        true
    });
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    let powers: Vec<String> = bench.lock().unwrap().lines.iter().map(|l| l[..1].to_owned()).collect();
    assert_eq!(powers, ["0", "1", "1", "0", "0", "0", "1"]);
}

/// A HUD whose game was already running at connection: the first line already has POWER 1.
#[test]
fn power_is_on_when_a_game_runs_at_connection() {
    let bench = hud_bench(&["OBD2HUD-A"], "1.0.0\0");
    bench.lock().unwrap().lines_before_drop = Some(1);
    let host = FakeHost::new();
    host.state.lock().unwrap().game = "iracing".into();
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    assert_eq!(bench.lock().unwrap().lines, ["1~0~0~0~0~0~0~0~0"]);
}

/// A different revision flashes `firmware.bin` in 512-byte chunks written with response, one notification
/// per chunk, the status showing the percentage before each chunk; then the HUD is disconnected and the scan
/// returns without trying further devices.
#[test]
fn outdated_firmware_is_flashed_then_the_scan_ends() {
    let bench = hud_bench(&["OBD2HUD-A", "OBD2HUD-B"], "0.9.0\0");
    let mut host = FakeHost::new();
    host.firmware = Some(vec![7; 1300]);
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    let b = bench.lock().unwrap();
    assert_eq!(b.chunks, [512, 512, 276]);
    assert!(b.lines.is_empty());
    assert_eq!(
        b.events,
        [
            "discover".to_owned(),
            "connect AA:00".into(),
            format!("read {FIRMWARE_REVISION_CHARACTERISTIC}"),
            format!("notify {FIRMWARE_FILE_CHARACTERISTIC}"),
            "disconnect".into(),
            "disconnect".into(),
        ]
    );
    assert_eq!(
        host.statuses(),
        [
            "Installing new firmware - progress: 0%",
            "Installing new firmware - progress: 39%",
            "Installing new firmware - progress: 78%",
            "Not Connected"
        ]
    );
    assert!(host.sleeps.lock().unwrap().is_empty(), "notifications arrive at once: no wait");
}

/// No notification: the chunk waits 1000 polls of 1 ms, then the
/// update is aborted.
#[test]
fn missing_notification_aborts_the_update() {
    let bench = hud_bench(&["OBD2HUD-A"], "2.0\0");
    bench.lock().unwrap().notify_on_write = false;
    let mut host = FakeHost::new();
    host.firmware = Some(vec![1; 2000]);
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    assert_eq!(bench.lock().unwrap().chunks, [512]);
    let sleeps = host.sleeps.lock().unwrap();
    assert_eq!(sleeps.len(), 1000);
    assert!(sleeps.iter().all(|d| *d == Duration::from_millis(1)));
}

/// A failed chunk write aborts while more than 512 bytes are left; the last chunk is retried.
#[test]
fn failed_chunk_writes() {
    let bench = hud_bench(&["OBD2HUD-A"], "2.0\0");
    bench.lock().unwrap().failing_writes = 1;
    let mut host = FakeHost::new();
    host.firmware = Some(vec![1; 600]);
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    assert!(bench.lock().unwrap().chunks.is_empty());

    let bench = bench_with_failures(1);
    let mut host = FakeHost::new();
    host.firmware = Some(vec![1; 400]);
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    assert_eq!(bench.lock().unwrap().chunks, [400]);
    assert_eq!(
        host.statuses(),
        ["Installing new firmware - progress: 0%", "Installing new firmware - progress: 0%", "Not Connected"]
    );
}

fn bench_with_failures(failures: usize) -> SharedBench {
    let bench = hud_bench(&["OBD2HUD-A"], "2.0\0");
    bench.lock().unwrap().failing_writes = failures;
    bench
}

/// Errors inside a session ('something went wrong'): the link is closed, 'Not Connected' set and the next
/// device tried. Missing `firmware.bin`, a revision that is not a valid version and a refresh rate of 0.
#[test]
fn session_errors_disconnect_and_continue() {
    let bench = hud_bench(&["OBD2HUD-A", "OBD2HUD-B"], "1.0.1\0");
    let host = FakeHost::new();
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    let connects = bench.lock().unwrap().events.iter().filter(|e| e.starts_with("connect")).count();
    assert_eq!(connects, 2);
    assert_eq!(host.statuses(), ["Not Connected", "Not Connected"]);

    let bench = hud_bench(&["OBD2HUD-A"], "1.0.0");
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    assert!(bench.lock().unwrap().events.ends_with(&["disconnect".to_owned()]));

    let bench = hud_bench(&["OBD2HUD-A"], "1.0.0\0");
    let host = FakeHost::new();
    host.state.lock().unwrap().rate = 0;
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    assert_eq!(host.statuses(), ["Connected", "Not Connected"]);
    assert!(bench.lock().unwrap().lines.is_empty());
}

/// A HUD without the OBD2HUD service: nothing is read or written, the link is closed.
#[test]
fn other_services_are_ignored() {
    let bench = hud_bench(&["OBD2HUD-A"], "1.0.0\0");
    bench.lock().unwrap().services.retain(|s| s.uuid != HUD_SERVICE);
    let host = FakeHost::new();
    scan(&mut FakeCentral(Arc::clone(&bench)), &host).unwrap();
    assert_eq!(bench.lock().unwrap().events, ["discover", "connect AA:00", "disconnect"]);
    assert_eq!(host.statuses(), ["Not Connected"]);
}

/// The scan loop: a new scan five seconds after each one, failed or not, until the service stops.
#[test]
fn scans_repeat_every_five_seconds() {
    let bench = hud_bench(&[], "");
    let host = FakeHost::new().on_sleep(|_, n| n < 3);
    run(&mut FakeCentral(Arc::clone(&bench)), &host);
    assert_eq!(bench.lock().unwrap().events, ["discover"; 3]);
    assert_eq!(*host.sleeps.lock().unwrap(), vec![RESCAN_DELAY; 3]);
}

/// Stopping during the telemetry loop disconnects and ends the loop without another scan.
#[test]
fn stop_during_streaming() {
    let bench = hud_bench(&["OBD2HUD-A"], "1.0.0\0");
    bench.lock().unwrap().lines_before_drop = None;
    let host = FakeHost::new().on_sleep(|_, n| n < 4);
    run(&mut FakeCentral(Arc::clone(&bench)), &host);
    let b = bench.lock().unwrap();
    assert_eq!(b.lines.len(), 3);
    assert_eq!(b.events.iter().filter(|e| *e == "discover").count(), 1);
    assert_eq!(b.events.last().map(String::as_str), Some("disconnect"));
    assert_eq!(host.statuses(), ["Connected", "Not Connected"]);
}

/// The firmware location inside an application folder.
#[test]
fn firmware_location() {
    assert_eq!(
        firmware_path(std::path::Path::new("/srs")),
        std::path::PathBuf::from("/srs/firmware/obd2hud/firmware.bin")
    );
}
