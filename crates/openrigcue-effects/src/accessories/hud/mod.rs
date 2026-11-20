//! OBD2HUD: the SRS Bluetooth LE head-up display. The service finds a HUD named `OBD2HUD-…`, updates its
//! firmware when it differs from the expected version, then writes a line of telemetry (`POWER~SPEED~…`) to it at
//! `HUD.refresh_rate` Hz until it disconnects. Data only flows
//! to the HUD; nothing is read from a car.
//!
//! The BLE stack is behind [`Central`] / [`Peripheral`]; [`bluez`] implements them on
//! BlueZ over the system D-Bus, the tests use a fake.

pub mod bluez;
pub mod version;

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_model::Telemetry;
use tracing::debug;

use self::version::{InvalidVersion, Pep440};

/// The HUD's receiver characteristic; never used.
pub const RECEIVER_CHARACTERISTIC: &str = "2747c22c-730f-11ec-90d6-0242ac120003";
/// The GATT service of the HUD.
pub const HUD_SERVICE: &str = "c14fcc20-72f1-11ec-90d6-0242ac120003";
/// The characteristic the telemetry line is written to.
pub const SENDER_CHARACTERISTIC: &str = "c97c6c5a-72f1-11ec-90d6-0242ac120003";
/// The firmware-update characteristic (chunks written, one notification each).
pub const FIRMWARE_FILE_CHARACTERISTIC: &str = "173c2db0-7723-11ec-90d6-0242ac120003";
/// The standard Firmware Revision String characteristic (0x2A26) the HUD's version is read from.
pub const FIRMWARE_REVISION_CHARACTERISTIC: &str = "00002a26-0000-1000-8000-00805f9b34fb";
/// The advertised-name prefix of a HUD, matched case-insensitively.
pub const NAME_PREFIX: &str = "OBD2HUD-";
/// Bytes per firmware chunk.
pub const FIRMWARE_CHUNK: usize = 512;
/// The HUD firmware version this build expects.
pub const INSTALLER_FIRMWARE: &str = "1.0.0";
/// Pause between the end of one scan and the next.
pub const RESCAN_DELAY: Duration = Duration::from_secs(5);
/// Seconds a firmware chunk waits for the HUD's notification.
pub const CHUNK_TIMEOUT: f64 = 1.0;
/// The poll step of that wait, in seconds.
pub const CHUNK_POLL: f64 = 0.001;
/// The translated prefix of the flashing status (the sidebar recognises it by its English text).
pub const FLASHING_PREFIX: &str = "Installing new firmware - progress: ";

/// The fields of the HUD line, in order.
pub const FIELD_ORDER: [&str; 9] = [
    "POWER",
    "SPEED",
    "WATER_TEMPERATURE",
    "RPM",
    "TURBO_PRESSURE",
    "FUEL_LEVEL",
    "OIL_TEMPERATURE",
    "OIL_PRESSURE",
    "VACUUM",
];

/// The HUD status shown in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HudStatus {
    /// `'Not Connected'` (the initial value, and set again when the link ends).
    #[default]
    NotConnected,
    /// `'Connected'`: telemetry is being sent.
    Connected,
    /// The translated `Flashing new firmware - Status: ` followed by the percentage and `%`.
    Flashing { prefix: String, percent: i64 },
}

impl HudStatus {
    /// The parameter's text.
    pub fn text(&self) -> String {
        match self {
            HudStatus::NotConnected => "Not Connected".to_owned(),
            HudStatus::Connected => "Connected".to_owned(),
            HudStatus::Flashing { prefix, percent } => format!("{prefix}{percent}%"),
        }
    }
}

impl fmt::Display for HudStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

/// A failure of the BLE stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BleError(pub String);

impl fmt::Display for BleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BleError {}

/// A device seen by a discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertiser {
    pub address: String,
    /// The advertised name; a device without one ends the scan with an error.
    pub name: Option<String>,
}

/// A GATT characteristic: its UUID and the transport's handle for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Characteristic {
    pub uuid: String,
    pub handle: String,
}

/// A GATT service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Lower-case 128-bit UUID.
    pub uuid: String,
    pub characteristics: Vec<Characteristic>,
}

impl Service {
    /// The characteristic with `uuid`; `None` when the service has no such characteristic.
    pub fn characteristic(&self, uuid: &str) -> Option<&Characteristic> {
        self.characteristics.iter().find(|c| c.uuid == uuid)
    }
}

/// The scanning side of the BLE stack.
pub trait Central {
    /// Five seconds of LE discovery; returns the devices seen.
    fn discover(&mut self) -> Result<Vec<Advertiser>, BleError>;
    /// Connects to `address` and resolves the services.
    fn connect(&mut self, address: &str) -> Result<Box<dyn Peripheral>, BleError>;
}

/// A connected HUD.
pub trait Peripheral {
    /// The GATT services of the device.
    fn services(&mut self) -> Result<Vec<Service>, BleError>;
    /// Reads the characteristic with that UUID in any service.
    fn read(&mut self, uuid: &str) -> Result<Vec<u8>, BleError>;
    /// Writes `data` to a characteristic, with or without response.
    fn write(&mut self, characteristic: &Characteristic, data: &[u8], with_response: bool) -> Result<(), BleError>;
    /// Starts notifications of a characteristic.
    fn start_notify(&mut self, characteristic: &Characteristic) -> Result<(), BleError>;
    /// Whether a notification arrived since the last call.
    fn take_notification(&mut self) -> bool;
    /// Whether the link is up.
    fn is_connected(&mut self) -> bool;
    /// Disconnects (also done on every way out of a session).
    fn disconnect(&mut self) -> Result<(), BleError>;
}

/// What the driver reads from and writes to the service.
pub trait HudHost {
    /// `HUD.refresh_rate` as an integer, read at every update.
    fn refresh_rate(&self) -> u32;
    /// `HUD.model`.
    fn model(&self) -> String;
    /// The sidebar's HUD button.
    fn hud_switch(&self) -> bool;
    /// The running game (`'none'` without a game).
    fn game_running(&self) -> String;
    /// The current telemetry.
    fn telemetry(&self) -> Telemetry;
    /// Publishes the HUD status.
    fn set_status(&self, status: HudStatus);
    /// `text` translated into the selected language.
    fn translate(&self, text: &str) -> String;
    /// The bytes of the HUD firmware image (`firmware/obd2hud/firmware.bin`).
    fn firmware_image(&self) -> std::io::Result<Vec<u8>>;
    /// Waits; `false` when the service is stopping.
    fn sleep(&self, duration: Duration) -> bool;
}

/// Why one HUD's session ended with an error ("something went wrong").
#[derive(Debug)]
pub enum SessionError {
    Ble(BleError),
    /// `firmware.bin` could not be read.
    Firmware(std::io::Error),
    /// The HUD's firmware revision is not UTF-8.
    Decode,
    Version(InvalidVersion),
    /// The refresh rate is 0, so the line period cannot be computed.
    ZeroRefreshRate,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Ble(e) => write!(f, "{e}"),
            SessionError::Firmware(e) => write!(f, "{e}"),
            SessionError::Decode => f.write_str("'utf-8' codec can't decode the firmware revision"),
            SessionError::Version(e) => write!(f, "{e}"),
            SessionError::ZeroRefreshRate => f.write_str("division by zero"),
        }
    }
}

impl From<BleError> for SessionError {
    fn from(e: BleError) -> Self {
        SessionError::Ble(e)
    }
}

/// How a session ended without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEnd {
    /// The service loop went on (the HUD disconnected, or it had no HUD service).
    Done,
    /// A firmware update ran: the HUD is disconnected and the whole scan ends.
    EndScan,
}

/// The service is stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped;

/// Why a scan ended early: it failed (the failure is ignored and the next scan follows),
/// or the service is stopping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    Failed(BleError),
    /// A discovered device has no name.
    UnnamedDevice(String),
    Stopped,
}

/// The integer part of a float as decimal text; `None` for NaN and infinities.
pub fn py_int_text(x: f64) -> Option<String> {
    if !x.is_finite() {
        return None;
    }
    let t = x.trunc();
    // Rust prints the exact decimal value of an integral f64; -0.4 truncates to `0`, not `-0`.
    Some(if t == 0.0 { "0".to_owned() } else { format!("{t:.0}") })
}

/// The telemetry line: each field truncated to an integer (0 when it does not convert; speed m/s → km/h with
/// `* 3.6`, `VACUUM` always 0), joined with `~` in field order.
pub fn hud_line(power: i64, t: &Telemetry) -> String {
    let field = |x: f64| py_int_text(x).unwrap_or_else(|| "0".to_owned());
    let values = [
        power.to_string(),
        field(t.speed * 3.6),
        field(t.water_temperature),
        field(t.rpm),
        field(t.turbo_boost_pressure),
        field(t.fuel_level),
        field(t.oil_temperature),
        field(t.oil_pressure),
        "0".to_owned(),
    ];
    values.join("~")
}

/// Percentage completed: `100 - remaining / total * 100`, truncated.
pub fn flash_percent(remaining: usize, total: usize) -> i64 {
    (100.0 - remaining as f64 / total as f64 * 100.0).trunc() as i64
}

/// The state of one scan that outlives a HUD session.
struct ScanLocals {
    /// The game seen on the previous line; `'none'` at the start of every scan.
    previous_game: String,
    /// The current `POWER` value; 0 at the start of every scan.
    power: i64,
}

/// One scan: discover, then for every device named `OBD2HUD-…` a session whose error is
/// printed ("something went wrong") without ending the scan.
pub fn scan(central: &mut dyn Central, host: &dyn HudHost) -> Result<(), ScanError> {
    let mut locals = ScanLocals { previous_game: "none".to_owned(), power: 0 };
    let devices = central.discover().map_err(ScanError::Failed)?;
    for device in devices {
        let Some(name) = &device.name else {
            return Err(ScanError::UnnamedDevice(device.address));
        };
        if !name.to_lowercase().starts_with(&NAME_PREFIX.to_lowercase()) {
            continue;
        }
        match session(central, host, &device.address, &mut locals) {
            Ok(SessionEnd::Done) => {}
            Ok(SessionEnd::EndScan) => return Ok(()),
            Err(Interrupted::Stopped) => return Err(ScanError::Stopped),
            Err(Interrupted::Failed(error)) => debug!(address = %device.address, %error, "something went wrong"),
        }
    }
    Ok(())
}

enum Interrupted {
    Failed(SessionError),
    Stopped,
}

impl<E: Into<SessionError>> From<E> for Interrupted {
    fn from(e: E) -> Self {
        Interrupted::Failed(e.into())
    }
}

impl From<Stopped> for Interrupted {
    fn from(_: Stopped) -> Self {
        Interrupted::Stopped
    }
}

/// One session with a connected HUD. Once the link is up, its end
/// sets `'Not Connected'`; the HUD is disconnected on every way out.
fn session(
    central: &mut dyn Central,
    host: &dyn HudHost,
    address: &str,
    locals: &mut ScanLocals,
) -> Result<SessionEnd, Interrupted> {
    debug!(address, "start client");
    let mut client = central.connect(address)?;
    debug!(address, "finish client");
    let result = serve(client.as_mut(), host, locals);
    let _ = client.disconnect();
    host.set_status(HudStatus::NotConnected);
    result
}

/// The body of a session: the HUD service's firmware check, then the update or the telemetry loop.
fn serve(client: &mut dyn Peripheral, host: &dyn HudHost, locals: &mut ScanLocals) -> Result<SessionEnd, Interrupted> {
    for service in client.services()? {
        debug!(uuid = %service.uuid, "service");
        if service.uuid != HUD_SERVICE {
            continue;
        }
        let sender = service.characteristic(SENDER_CHARACTERISTIC).cloned();
        let revision = client.read(FIRMWARE_REVISION_CHARACTERISTIC)?;
        let mut revision = String::from_utf8(revision).map_err(|_| SessionError::Decode)?;
        revision.pop(); // the last character is not part of the version
        let installer = Pep440::parse(INSTALLER_FIRMWARE).map_err(SessionError::Version)?;
        let device = Pep440::parse(&revision).map_err(SessionError::Version)?;
        if installer != device {
            debug!(%revision, "there is a firmware update...");
            update_firmware(client, host, &service)?;
            client.disconnect()?;
            return Ok(SessionEnd::EndScan);
        }
        host.set_status(HudStatus::Connected);
        stream(client, host, sender.as_ref(), locals)?;
    }
    Ok(SessionEnd::Done)
}

/// The firmware update: 512-byte chunks written with response to the file characteristic, each followed by a
/// wait of at most one second for the HUD's notification; a timeout, or a failed write with more than one
/// chunk left, aborts.
fn update_firmware(client: &mut dyn Peripheral, host: &dyn HudHost, service: &Service) -> Result<(), Interrupted> {
    let image = host.firmware_image().map_err(SessionError::Firmware)?;
    let total = image.len();
    let file = service
        .characteristic(FIRMWARE_FILE_CHARACTERISTIC)
        .cloned()
        .ok_or_else(|| BleError("Characteristic None was not found!".into()))?;
    client.start_notify(&file)?;
    let mut remaining: &[u8] = &image;
    while !remaining.is_empty() {
        let percent = flash_percent(remaining.len(), total);
        host.set_status(HudStatus::Flashing { prefix: host.translate(FLASHING_PREFIX), percent });
        let _ = client.take_notification(); // clear the notified flag
        let chunk = &remaining[..remaining.len().min(FIRMWARE_CHUNK)];
        if let Err(error) = client.write(&file, chunk, true) {
            debug!(%error, "error");
            if remaining.len() > FIRMWARE_CHUNK {
                debug!("flashing firmware - EXCEPTION");
                break;
            }
            // The last chunk is retried at once; a zero wait only lets a stopping service end it.
            if !host.sleep(Duration::ZERO) {
                return Err(Interrupted::Stopped);
            }
            continue;
        }
        remaining = &remaining[chunk.len()..];
        let mut waiting = CHUNK_TIMEOUT;
        let mut notified = false;
        loop {
            notified |= client.take_notification();
            if notified || waiting <= 0.0 {
                break;
            }
            waiting -= CHUNK_POLL;
            if !host.sleep(Duration::from_secs_f64(CHUNK_POLL)) {
                return Err(Interrupted::Stopped);
            }
        }
        if notified {
            debug!("notify_callback");
        }
        if waiting <= 0.0 {
            debug!("flashing firmware - TIMEOUT ABORTED");
            break;
        }
        debug!(remaining = remaining.len(), "flashing firmware - remaining: ");
    }
    Ok(())
}

/// While connected: one telemetry line every `1 / refresh_rate` s. `POWER` is 1 with model `'B'`,
/// 0 while the HUD button is off, and otherwise follows whether a game runs, updated whenever the running
/// game changes. A failed write is ignored.
fn stream(
    client: &mut dyn Peripheral,
    host: &dyn HudHost,
    sender: Option<&Characteristic>,
    locals: &mut ScanLocals,
) -> Result<(), Interrupted> {
    while client.is_connected() {
        let rate = host.refresh_rate();
        if rate == 0 {
            return Err(SessionError::ZeroRefreshRate.into());
        }
        let pause = openrigcue_model::python_time::sleep_duration(1.0 / f64::from(rate))
            .expect("a nonzero u32 refresh rate produces a finite positive timeout");
        if !host.sleep(pause) {
            return Err(Stopped.into());
        }
        if host.model() == "B" {
            locals.power = 1;
        } else if host.hud_switch() {
            if host.game_running() != locals.previous_game {
                locals.power = i64::from(host.game_running() != "none");
            }
        } else {
            locals.power = 0;
        }
        let line = hud_line(locals.power, &host.telemetry());
        if let Some(sender) = sender {
            let _ = client.write(sender, line.as_bytes(), false);
        }
        locals.previous_game = host.game_running();
    }
    Ok(())
}

/// The scan loop: a new scan five seconds after the previous one ended, whatever
/// it ended with, until the service stops.
pub fn run(central: &mut dyn Central, host: &dyn HudHost) {
    loop {
        match scan(central, host) {
            Ok(()) => {}
            Err(ScanError::Stopped) => return,
            Err(error) => debug!(?error, "HUD scan failed"),
        }
        if !host.sleep(RESCAN_DELAY) {
            return;
        }
    }
}

/// Where the HUD firmware is inside an application folder (`firmware/obd2hud/firmware.bin`).
pub fn firmware_path(srs_app_path: &std::path::Path) -> PathBuf {
    srs_app_path.join("firmware").join("obd2hud").join("firmware.bin")
}

/// The HUD service: a thread running [`run`].
pub struct HudThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HudThread {
    /// Starts the scan loop on `central`. `host` must return `false` from [`HudHost::sleep`] once `stop` is set.
    pub fn start<C, H>(mut central: C, host: H, stop: Arc<AtomicBool>) -> std::io::Result<HudThread>
    where
        C: Central + Send + 'static,
        H: HudHost + Send + 'static,
    {
        let handle = std::thread::Builder::new().name("bleak_thread".into()).spawn(move || run(&mut central, &host))?;
        Ok(HudThread { stop, handle: Some(handle) })
    }

    /// Asks the loop to end. It ends at its next wait; a BLE call in progress (at most the transport's
    /// timeout) is not interrupted, so the thread is not joined.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        drop(self.handle.take());
    }
}

/// The HUD status as shared between the HUD thread and the API.
#[derive(Debug, Default)]
pub struct HudSlot {
    status: Mutex<HudStatus>,
}

impl HudSlot {
    pub fn status(&self) -> HudStatus {
        self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn set(&self, status: HudStatus) {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = status;
    }
}

#[cfg(test)]
mod tests;
