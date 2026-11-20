//! Local maintenance UI. The main service and its device workers are not started.
use crate::{dfu, firmware, hid, smc, usb};
use axum::{
    extract::Request,
    extract::{Query, State},
    http::{Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
pub struct Tools {
    pub session: Option<smc::Session>,
    pub error: Option<String>,
    pub firmware_folder: Option<PathBuf>,
    pub firmware_log: Vec<String>,
    pub flashing: bool,
    pub connecting: bool,
}
pub type Shared = Arc<Mutex<Tools>>;
fn lock(s: &Shared) -> std::sync::MutexGuard<'_, Tools> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
#[derive(Debug)]
struct Failure(String);
impl From<smc::Error> for Failure {
    fn from(e: smc::Error) -> Self {
        Self(e.to_string())
    }
}
impl From<String> for Failure {
    fn from(e: String) -> Self {
        Self(e)
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":self.0}))).into_response()
    }
}
type Result<T> = std::result::Result<T, Failure>;
#[derive(Serialize)]
struct View {
    connected: bool,
    port: Option<String>,
    settings: Option<smc::Settings>,
    mode: smc::Mode,
    motor: u8,
    samples: Vec<smc::Sample>,
    error: Option<String>,
    firmware_log: Vec<String>,
    flashing: bool,
    udp_packets: u64,
    udp_port: Option<u16>,
}
async fn status(State(state): State<Shared>) -> Json<View> {
    let t = lock(&state);
    let s = t.session.as_ref();
    Json(View {
        connected: s.is_some(),
        port: s.map(|s| s.port().into()),
        settings: s.map(|s| s.settings.clone()),
        mode: s.map(|s| s.mode).unwrap_or_default(),
        motor: s.map(|s| s.motor).unwrap_or(1),
        samples: s.map(|s| s.samples.iter().cloned().collect()).unwrap_or_default(),
        error: t.error.clone(),
        firmware_log: t.firmware_log.clone(),
        flashing: t.flashing,
        udp_packets: s.map_or(0, |s| s.udp_packets),
        udp_port: s.and_then(|s| s.udp_port),
    })
}
async fn ports() -> Json<Vec<serde_json::Value>> {
    Json(
        openrigcue_devices::discovery::serial_ports()
            .into_iter()
            .map(|p| serde_json::json!({"path":p.path,"name":p.describe()}))
            .collect(),
    )
}
#[derive(Deserialize)]
struct Connect {
    port: String,
}
async fn connect(State(state): State<Shared>, Json(request): Json<Connect>) -> Result<Json<bool>> {
    {
        let mut t = lock(&state);
        if t.flashing || t.connecting || t.session.is_some() {
            return Err(Failure("Disconnect the current controller or wait for the active operation".into()));
        }
        t.connecting = true;
    }
    let result = tokio::task::spawn_blocking(move || smc::Session::connect(&request.port)).await;
    let mut t = lock(&state);
    t.connecting = false;
    t.session = Some(result.map_err(|e| Failure(e.to_string()))??);
    t.error = None;
    Ok(Json(true))
}
async fn disconnect(State(state): State<Shared>) -> Json<bool> {
    lock(&state).session = None;
    Json(true)
}
fn session<T>(state: &Shared, run: impl FnOnce(&mut smc::Session) -> smc::Result<T>) -> Result<Json<T>> {
    let mut t = lock(state);
    let s = t.session.as_mut().ok_or_else(|| Failure("Connect a controller first".into()))?;
    Ok(Json(run(s)?))
}
#[derive(Deserialize)]
struct Motor {
    motor: u8,
}
async fn motor(State(state): State<Shared>, Json(r): Json<Motor>) -> Result<Json<()>> {
    session(&state, |s| s.select_motor(r.motor))
}
#[derive(Deserialize)]
struct Motion {
    mode: smc::Mode,
    center: u16,
    amplitude: u16,
    period: f64,
}
#[derive(Deserialize)]
struct Udp {
    port: u16,
}
async fn udp(State(state): State<Shared>, Json(r): Json<Udp>) -> Result<Json<()>> {
    session(&state, |s| s.start_udp(r.port))
}
#[derive(Deserialize)]
struct ControllerCommand {
    command: String,
}
async fn controller_command(State(state): State<Shared>, Json(r): Json<ControllerCommand>) -> Result<Json<()>> {
    session(&state, |s| s.controller_command(&r.command))
}
async fn motion(State(state): State<Shared>, Json(r): Json<Motion>) -> Result<Json<()>> {
    session(&state, |s| s.set_mode(r.mode, r.center, r.amplitude, r.period))
}
async fn configure(State(state): State<Shared>, Json(r): Json<smc::Settings>) -> Result<Json<()>> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || session(&state, |s| s.apply_settings(r)))
        .await
        .map_err(|e| Failure(e.to_string()))?
}
async fn save(State(state): State<Shared>) -> Result<Json<()>> {
    session(&state, |s| s.save_controller())
}
async fn enable(State(state): State<Shared>) -> Result<Json<()>> {
    session(&state, |s| s.enable_motor())
}
async fn settings_export(State(state): State<Shared>) -> Result<impl IntoResponse> {
    let t = lock(&state);
    let s = t.session.as_ref().ok_or_else(|| Failure("Connect a controller first".into()))?;
    Ok((
        [("Content-Type", "text/plain"), ("Content-Disposition", "attachment; filename=SMC3-settings.txt")],
        s.settings.to_legacy(),
    ))
}
async fn settings_import(State(state): State<Shared>, body: String) -> Result<Json<smc::Settings>> {
    let r = smc::Settings::from_legacy(&body)?;
    let mut t = lock(&state);
    let s = t.session.as_mut().ok_or_else(|| Failure("Connect a controller first".into()))?;
    s.apply_settings(r)?;
    Ok(Json(s.settings.clone()))
}
async fn csv(State(state): State<Shared>) -> impl IntoResponse {
    let t = lock(&state);
    let mut text = String::from("seconds,motor,target,feedback,pwm,enabled,calculations\n");
    if let Some(s) = &t.session {
        for p in &s.samples {
            text.push_str(&format!(
                "{},{},{},{},{},{},{}\n",
                p.seconds, p.motor, p.target, p.feedback, p.pwm, p.enabled, p.calculations
            ));
        }
    }
    ([("Content-Type", "text/csv"), ("Content-Disposition", "attachment; filename=SMC3-capture.csv")], text)
}
async fn devices() -> Result<Json<Vec<usb::Device>>> {
    Ok(Json(usb::enumerate(std::path::Path::new("/sys/bus/usb/devices"))?))
}
#[derive(Deserialize)]
struct UsbId {
    id: String,
}
async fn holders(Query(r): Query<UsbId>) -> Result<Json<Vec<(u32, String)>>> {
    let devices = usb::enumerate(std::path::Path::new("/sys/bus/usb/devices"))?;
    Ok(Json(usb::holders(usb::find(&devices, &r.id)?)))
}
#[derive(Deserialize)]
struct Report {
    format: Option<String>,
}
async fn report(Query(r): Query<Report>) -> Result<impl IntoResponse> {
    let d = usb::enumerate(std::path::Path::new("/sys/bus/usb/devices"))?;
    let (kind, text) = if r.format.as_deref() == Some("xml") {
        ("application/xml", usb::xml_report(&d))
    } else {
        ("text/plain", usb::text_report(&d))
    };
    Ok(([("Content-Type", kind), ("Content-Disposition", "attachment; filename=usb-report.txt")], text))
}
#[derive(Deserialize)]
struct UsbAction {
    id: String,
    action: String,
}
async fn usb_action(Json(r): Json<UsbAction>) -> Result<Json<bool>> {
    let devices = usb::enumerate(std::path::Path::new("/sys/bus/usb/devices"))?;
    let d = usb::find(&devices, &r.id)?;
    match r.action.as_str() {
        "reset" => usb::reset_port(d)?,
        "restart" => usb::restart_driver(d)?,
        "remove" => usb::set_authorized(d, false)?,
        "authorize" => usb::set_authorized(d, true)?,
        _ => return Err(Failure("Unknown USB action".into())),
    }
    Ok(Json(true))
}
#[derive(Deserialize)]
struct FirmwareFolder {
    path: PathBuf,
}
async fn images(State(state): State<Shared>, Json(r): Json<FirmwareFolder>) -> Result<Json<Vec<firmware::Image>>> {
    let files = firmware::inventory(&r.path)?;
    lock(&state).firmware_folder = Some(r.path);
    Ok(Json(files))
}
#[derive(Deserialize)]
struct ArchiveImport {
    path: PathBuf,
    destination: PathBuf,
}
async fn import_firmware(
    State(state): State<Shared>,
    Json(r): Json<ArchiveImport>,
) -> Result<Json<Vec<firmware::Image>>> {
    let destination = r.destination.clone();
    let images = tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&r.path).map_err(|e| e.to_string())?;
        firmware::import_archive(bytes, &r.destination)
    })
    .await
    .map_err(|e| Failure(e.to_string()))??;
    lock(&state).firmware_folder = Some(destination);
    Ok(Json(images))
}
async fn download_firmware(
    State(state): State<Shared>,
    Json(r): Json<FirmwareFolder>,
) -> Result<Json<Vec<firmware::Image>>> {
    let destination = r.path.clone();
    let images = tokio::task::spawn_blocking(move || firmware::download_vendor_archive(&r.path))
        .await
        .map_err(|e| Failure(e.to_string()))??;
    lock(&state).firmware_folder = Some(destination);
    Ok(Json(images))
}
async fn import_usb(body: String) -> Result<Json<Vec<usb::Device>>> {
    Ok(Json(usb::import_report(&body)?))
}
#[derive(Deserialize)]
struct SaveFile {
    path: PathBuf,
    text: String,
    png: bool,
}
async fn save_file(Json(r): Json<SaveFile>) -> Result<Json<PathBuf>> {
    use std::io::Write;
    let path = if let Ok(relative) = r.path.strip_prefix("~") {
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| Failure("Home directory is unavailable".into()))?)
            .join(relative)
    } else {
        r.path
    };
    let bytes = if r.png {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(r.text.strip_prefix("data:image/png;base64,").ok_or_else(|| Failure("Invalid PNG data".into()))?)
            .map_err(|e| Failure(e.to_string()))?;
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(Failure("Invalid PNG header".into()));
        }
        bytes
    } else {
        r.text.into_bytes()
    };
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| f.write_all(&bytes))
        .map_err(|e| Failure(format!("Could not save {}: {e}", path.display())))?;
    Ok(Json(path))
}
#[derive(Deserialize)]
struct Flash {
    port: String,
    path: PathBuf,
    address: Option<u32>,
    baud: Option<u32>,
    programmer: String,
}
#[derive(Deserialize)]
struct BootOperation {
    port: String,
    programmer: hid::Protocol,
    operation: String,
}
async fn boot_operation(State(state): State<Shared>, Json(r): Json<BootOperation>) -> Result<Json<String>> {
    {
        let mut t = lock(&state);
        if t.connecting || t.flashing || t.session.is_some() {
            return Err(Failure("Disconnect the motor monitor and wait for any update to finish".into()));
        }
        t.flashing = true;
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut io = hid::LinuxReports::open(&r.port)?;
        match r.operation.as_str() {
            "information" => hid::information(&mut io),
            "erase" => hid::erase(&mut io, r.programmer).map(|()| "Erase command completed".into()),
            "reboot" => hid::reboot(&mut io, r.programmer).map(|()| "Controller reboot requested".into()),
            _ => Err("Unknown bootloader operation".into()),
        }
    })
    .await;
    let mut t = lock(&state);
    t.flashing = false;
    let message = result.map_err(|e| Failure(e.to_string()))??;
    t.firmware_log.push(message.clone());
    Ok(Json(message))
}
async fn flash(State(state): State<Shared>, Json(r): Json<Flash>) -> Result<Json<bool>> {
    let payload = firmware::load(&r.path, r.address)?;
    {
        let mut t = lock(&state);
        if t.flashing || t.connecting {
            return Err(Failure("A firmware operation is already running".into()));
        }
        if t.session.is_some() {
            return Err(Failure("Disconnect the motor monitor before a firmware update".into()));
        }
        t.flashing = true;
        t.firmware_log = vec![format!("Programming {} bytes", payload.bytes.len())];
    }
    let state2 = state.clone();
    tokio::task::spawn_blocking(move || {
        let result = (|| -> std::result::Result<(), String> {
            match r.programmer.as_str() {
                "avr" => {
                    let mut io = firmware::SerialTransport::open(&r.port, r.baud.unwrap_or(115200))?;
                    firmware::flash_avr(&mut io, &payload, |n, total| {
                        let mut t = lock(&state2);
                        t.firmware_log = vec![format!("Verified {n} / {total} bytes")];
                    })
                }
                "dfu" => {
                    let mut device = dfu::open(&r.port)?;
                    device.flash(&payload, |n, total| {
                        lock(&state2).firmware_log = vec![format!("Verified {n} / {total} bytes")]
                    })?;
                    device.reboot(payload.address)
                }
                "weact"
                | "dof_legacy_encoder"
                | "dof_legacy_cueing"
                | "dof_legacy_cueing_v1"
                | "dof_encoder"
                | "dof_cueing" => {
                    let protocol: hid::Protocol =
                        serde_json::from_value(serde_json::Value::String(r.programmer.clone()))
                            .map_err(|e| e.to_string())?;
                    let mut io = hid::LinuxReports::open(&r.port)?;
                    hid::program(&mut io, &payload, protocol, |n, total| {
                        lock(&state2).firmware_log = vec![format!("Transferred {n} / {total} bytes")]
                    })?;
                    hid::reboot(&mut io, protocol)?;
                    lock(&state2).firmware_log.push(
                        "HID transfer complete; this bootloader does not support flash readback verification".into(),
                    );
                    Ok(())
                }
                _ => Err("Select a supported controller bootloader".into()),
            }
        })();
        let mut t = lock(&state2);
        t.flashing = false;
        t.firmware_log.push(match result {
            Ok(()) if matches!(r.programmer.as_str(), "avr" | "dfu") => "Firmware verified successfully".into(),
            Ok(()) => "Firmware transfer finished; controller reboot requested".into(),
            Err(e) => format!("Failed: {e}"),
        });
    });
    Ok(Json(true))
}
async fn index() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}
async fn local_request(request: Request, next: Next) -> axum::response::Response {
    let host = request.headers().get("host").and_then(|h| h.to_str().ok()).unwrap_or("");
    let local =
        host == "localhost" || host == "127.0.0.1" || host.starts_with("localhost:") || host.starts_with("127.0.0.1:");
    let modifying = request.method() != Method::GET && request.method() != Method::HEAD;
    let header = request.headers().get("x-openrigcue-tools").and_then(|h| h.to_str().ok()) == Some("1");
    let same_origin =
        request.headers().get("origin").is_none_or(|o| o.to_str().is_ok_and(|s| s == format!("http://{host}")));
    if !local || (modifying && (!header || !same_origin)) {
        return (StatusCode::FORBIDDEN, "This operation must be initiated from OpenRigCue Tools").into_response();
    }
    next.run(request).await
}
pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/tools/status", get(status))
        .route("/tools/ports", get(ports))
        .route("/tools/connect", post(connect))
        .route("/tools/disconnect", post(disconnect))
        .route("/tools/motor", post(motor))
        .route("/tools/mode", post(motion))
        .route("/tools/udp", post(udp))
        .route("/tools/controller-command", post(controller_command))
        .route("/tools/configure", post(configure))
        .route("/tools/save", post(save))
        .route("/tools/enable", post(enable))
        .route("/tools/settings", get(settings_export).post(settings_import))
        .route("/tools/capture.csv", get(csv))
        .route("/tools/usb", get(devices).post(usb_action))
        .route("/tools/usb-holders", get(holders))
        .route("/tools/usb-import", post(import_usb))
        .route("/tools/save-file", post(save_file))
        .route("/tools/usb-report", get(report))
        .route("/tools/images", post(images))
        .route("/tools/import-firmware", post(import_firmware))
        .route("/tools/download-firmware", post(download_firmware))
        .route("/tools/flash", post(flash))
        .route("/tools/boot-operation", post(boot_operation))
        .layer(middleware::from_fn(local_request))
        .with_state(state)
}
pub fn poller(state: Shared, stop: Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let mut t = lock(&state);
            if let Some(s) = &mut t.session {
                if let Err(e) = s.tick() {
                    t.error = Some(e.to_string());
                    t.session = None;
                }
            }
            drop(t);
            std::thread::sleep(Duration::from_millis(10));
        }
    })
}
