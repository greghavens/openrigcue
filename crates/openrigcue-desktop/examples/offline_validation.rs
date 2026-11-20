//! Explicit offline Desktop validation. Run only on a private X display and DBus session.
//! Opens a local CSP-protected exported page; never constructs a rig Service or device worker.
use openrigcue_desktop::{instance::Instance, Desktop, Tray, TrayAction};
use openrigcue_model::native_value::{Dict, LegacyValue};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

struct Watcher(mpsc::Sender<String>);
#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: String) {
        self.0.send(service).unwrap();
    }
}
fn pump(desktop: &Desktop) {
    let end = Instant::now() + Duration::from_millis(600);
    while Instant::now() < end {
        desktop.dispatch_pending();
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("OPENRIGCUE_OFFLINE_DESKTOP_VALIDATION").as_deref() != Ok("1") {
        return Err("requires explicit offline validation environment".into());
    }
    let uri = std::env::args().nth(1).ok_or("requires local file URI")?;
    if !uri.starts_with("file:///") {
        return Err("only local exported HTML accepted".into());
    }
    let (watch_tx, watch_rx) = mpsc::channel();
    let _watcher = zbus::blocking::connection::Builder::session()?
        .name("org.kde.StatusNotifierWatcher")?
        .serve_at("/StatusNotifierWatcher", Watcher(watch_tx))?
        .build()?;
    let mut desktop = Desktop::new()?;
    desktop.splash(include_bytes!("../../openrigcue-ui/res/images/openrigcue/splash.png"), || Ok(true))?;
    pump(&desktop);
    let initial = desktop.capture_geometry();
    assert!(!initial.monitors.is_empty(), "Xvfb must expose a monitor");
    let config: LegacyValue = serde_json::json!({"APP":{
        "window_width":"900","window_height":"760","pixel_ratio":"1.0",
        "window_x_pos":"100","window_y_pos":"100","start_minimized":"no",
        "x_button_exit":"no","force_manual_quit":"no"},"MONITORS":{}})
    .into();
    let mut config = config.as_dict().unwrap().clone();
    let monitors: Dict = initial
        .monitors
        .iter()
        .flatten()
        .enumerate()
        .map(|(i, m)| {
            (
                i.to_string().into(),
                LegacyValue::Str(
                    (i64::from(m.x) + i64::from(m.y) + i64::from(m.width) + i64::from(m.height)).to_string(),
                ),
            )
        })
        .collect();
    config.insert("MONITORS", LegacyValue::Dict(monitors));
    let config = Arc::new(Mutex::new(config));
    let read = config.clone();
    desktop.set_config_reader(Arc::new(move || read.lock().unwrap().clone()));
    desktop.open(&uri)?;
    pump(&desktop);
    assert!(desktop.capture_geometry().position.is_some(), "X11 geometry must be available");
    let (action_tx, action_rx) = mpsc::channel();
    let tray_tx = action_tx.clone();
    let _tray = Tray::start(Arc::new(move |a| tray_tx.send(a).unwrap()))?;
    let service = watch_rx.recv_timeout(Duration::from_secs(3))?;
    let bus = zbus::blocking::Connection::session()?;
    let item = zbus::blocking::Proxy::new(&bus, service.as_str(), "/StatusNotifierItem", "org.kde.StatusNotifierItem")?;
    assert_eq!(item.get_property::<String>("Title")?, "OpenRigCue");
    item.call::<_, _, ()>("Activate", &(0i32, 0i32))?;
    assert_eq!(action_rx.recv_timeout(Duration::from_secs(3))?, TrayAction::Open);
    desktop.activate_existing();
    desktop.show_menu(180, 180, Arc::new(move |a| action_tx.send(a).unwrap()))?;
    pump(&desktop);
    let menu = zbus::blocking::Proxy::new(&bus, service.as_str(), "/Menu", "com.canonical.dbusmenu")?;
    menu.call::<_, _, ()>("Event", &(2i32, "clicked", zbus::zvariant::Value::from(0i32), 0u32))?;
    assert_eq!(action_rx.recv_timeout(Duration::from_secs(3))?, TrayAction::Quit);
    let (activate_tx, activate_rx) = mpsc::channel();
    let (close_tx, close_rx) = mpsc::channel();
    let _instance = Instance::register(
        Arc::new(move || activate_tx.send(()).unwrap()),
        Arc::new(move || close_tx.send(()).unwrap()),
    )?;
    Instance::activate_existing()?;
    activate_rx.recv_timeout(Duration::from_secs(3))?;
    assert!(!Instance::close_existing_major("wrong-version")?);
    assert!(close_rx.try_recv().is_err());
    assert!(Instance::close_existing_major(env!("CARGO_PKG_VERSION_MAJOR"))?);
    close_rx.recv_timeout(Duration::from_secs(3))?;
    desktop.close_existing();
    assert!(!desktop.dispatch_pending(), "close with exit=no must keep desktop alive");
    desktop.open(&uri)?;
    pump(&desktop);
    config.lock().unwrap().get_mut("APP").unwrap().as_dict_mut().unwrap().insert("x_button_exit", "yes".into());
    desktop.close_existing();
    assert!(desktop.dispatch_pending(), "close must read current exit=yes");
    println!("PASS: actual GTK splash/window/geometry/reopen/current-close and private DBus tray/instance routes; no rig Service or devices");
    Ok(())
}
