//! Hotkeys and controller-button shortcuts: the keyboard and controller loops, and
//! the UI main loop's consumption of the queued UI actions.
//!
//! - Keyboard ([`keyboard`], [`combo`]): every key event of every readable keyboard tests the bindings and
//!   fires each `<action>_keys` binding whose keys are all held. Keyboards are read through evdev
//!   ([`evdev_keys`]).
//! - Controllers ([`buttons`]): one pass every 10 ms (25 ms under the Linux rules) over the
//!   SDL2 controller events, firing `<action>_btns` bindings and publishing [`JoystickInputs`].
//! - Effects ([`actions`]): an action changes the runtime switches, the tune's wind speed, homing
//!   requests and the telemetry controls directly, writes the CSV snapshot ([`snapshot`]) and queues every other
//!   action in `ui_actions`; the canned effects (`cannon_firing`, `missile_firing`, `ejection_seat`) are held
//!   in [`crate::runtime::CannedEffects`].
//! - `ui_actions` ([`sliders`]): every [`MAIN_LOOP_PERIOD`] the queued slider steps are applied (only while a game
//!   runs) and the motion-compensation calibrate / axis-switch / re-centre actions run
//!   ([`crate::compensation::run_shortcut`]).
//!
//! [`MAIN_LOOP_PERIOD`]: crate::profile::MAIN_LOOP_PERIOD

pub mod access;
pub mod actions;
pub mod buttons;
pub mod combo;
pub mod evdev_keys;
pub mod keyboard;
pub mod names;
pub mod routes;
pub mod sliders;
pub mod snapshot;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_config::settings::Settings;
use openrigcue_config::tunes::Tune;
use openrigcue_sources::api::JoystickInputs;

use self::buttons::ButtonHotkeys;
use crate::api::ApiContext;
use crate::state::ServiceEvent;

/// The canned effect names.
pub const CANNED_EFFECTS: [&str; 3] = ["cannon_firing", "missile_firing", "ejection_seat"];

/// The controller loop period: 10 ms on Windows, 25 ms elsewhere.
pub fn axis_period(windows: bool) -> Duration {
    Duration::from_millis(if windows { 10 } else { 25 })
}

#[derive(Debug)]
struct Inner {
    inputs: JoystickInputs,
    /// Where `telemetry_snapshot` writes: `<documents>/export`.
    export_dir: PathBuf,
    /// The actions queued for the UI main loop.
    ui_actions: Mutex<Vec<String>>,
    /// Whether the shortcut editor is open.
    list_shortcuts: AtomicBool,
    /// Counts the editor's openings: each one starts a fresh capture.
    capture_session: AtomicU64,
    /// The keyboard capture's last hotkey (the editor's keyboard field) and how many it read.
    captured_keys: Mutex<Option<(String, u64)>>,
    /// The gamepad table, in first-insertion order.
    gamepad: Mutex<Vec<(String, u8)>>,
    /// Whether controller input is enabled, as the controller loop read it; off, there are no controllers.
    controllers_enabled: AtomicBool,
    /// Whether the keyboard scanner's last pass read no keyboard and was refused one for lack of permission.
    keyboard_denied: Arc<AtomicBool>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The hotkey state the API and the loops share.
#[derive(Debug, Clone)]
pub struct Hotkeys(Arc<Inner>);

impl Hotkeys {
    /// `<documents>/export`, where the telemetry snapshot is written.
    pub fn export_dir(&self) -> &Path {
        &self.0.export_dir
    }

    /// The state for controllers read into `inputs`; `documents` is the configuration folder,
    /// whose `export` folder receives the telemetry snapshots.
    pub fn new(inputs: JoystickInputs, documents: &Path) -> Hotkeys {
        Hotkeys(Arc::new(Inner {
            inputs,
            export_dir: crate::export_csv::export_dir(documents),
            ui_actions: Mutex::default(),
            list_shortcuts: AtomicBool::new(false),
            capture_session: AtomicU64::new(0),
            captured_keys: Mutex::default(),
            gamepad: Mutex::default(),
            controllers_enabled: AtomicBool::new(true),
            keyboard_denied: Arc::default(),
        }))
    }

    /// For tests: no controllers.
    #[cfg(test)]
    pub(crate) fn offline() -> Hotkeys {
        Hotkeys::new(JoystickInputs::default(), Path::new(""))
    }

    /// The controllers.
    pub fn inputs(&self) -> &JoystickInputs {
        &self.0.inputs
    }

    /// Queues `action` for the UI main loop.
    pub fn queue(&self, action: &str) {
        lock(&self.0.ui_actions).push(action.to_owned());
    }

    /// The queued actions as the main loop reads them.
    pub fn ui_actions(&self) -> Vec<String> {
        lock(&self.0.ui_actions).clone()
    }

    /// Empties the queue: everything queued so far goes, including what was queued while the main
    /// loop pass ran.
    pub fn clear_ui_actions(&self) {
        lock(&self.0.ui_actions).clear();
    }

    /// Takes the queued actions (tests).
    #[cfg(test)]
    pub fn take_ui_actions(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.0.ui_actions))
    }

    /// Set when the shortcut editor opens, cleared when it closes.
    /// Every opening starts a new capture session: nothing the previous one read is shown.
    pub fn set_capture(&self, open: bool) {
        let mut captured = lock(&self.0.captured_keys);
        if open {
            self.0.capture_session.fetch_add(1, Ordering::SeqCst);
        }
        self.0.list_shortcuts.store(open, Ordering::SeqCst);
        *captured = None;
    }

    /// The running capture session, `None` while the editor is closed.
    fn capture_session(&self) -> Option<u64> {
        let _captured = lock(&self.0.captured_keys);
        self.capturing().then(|| self.0.capture_session.load(Ordering::SeqCst))
    }

    pub fn capturing(&self) -> bool {
        self.0.list_shortcuts.load(Ordering::SeqCst)
    }

    /// The hotkey the keyboard capture read last, with the number of reads so far.
    pub fn captured_keys(&self) -> Option<(String, u64)> {
        lock(&self.0.captured_keys).clone()
    }

    /// The first pressed button of `controller` in the gamepad (`'BTN_<n>'`), in insertion order (the
    /// button that first sent an event wins).
    pub fn captured_button(&self, controller: &str) -> Option<String> {
        lock(&self.0.gamepad).iter().find_map(|(key, value)| {
            let (name, button) = key.split_once('^')?;
            (*value == 1 && name == controller).then(|| button.split('^').next().unwrap_or_default().to_owned())
        })
    }

    /// The connected controllers by name, or none while controller input is disabled,
    /// which leaves the editor's dropdown at `'None'`, disabled.
    pub fn controllers(&self) -> Vec<String> {
        if !self.0.controllers_enabled.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let mut names = self.0.inputs.devices();
        names.dedup();
        names
    }

    /// Whether keyboard shortcuts cannot work: no keyboard is read and the system refused to open one for
    /// lack of permission (the keyboard access rule is not enabled).
    pub fn keyboard_denied(&self) -> bool {
        self.0.keyboard_denied.load(Ordering::SeqCst)
    }
}

/// Applies canned-effect writes (name, value).
fn set_canned(ctx: &ApiContext, writes: &[(String, u8)]) {
    if writes.is_empty() {
        return;
    }
    let mut controls = ctx.state.controls.write();
    for (name, value) in writes {
        let slot = match name.as_str() {
            "cannon_firing" => &mut controls.canned.cannon_firing,
            "missile_firing" => &mut controls.canned.missile_firing,
            "ejection_seat" => &mut controls.canned.ejection_seat,
            _ => continue,
        };
        *slot = Some(*value);
    }
}

/// One controller pass: the queued controller events against the button bindings.
pub fn axis_pass(ctx: &ApiContext, buttons: &mut ButtonHotkeys, events: &[openrigcue_sources::api::JoystickEvent]) {
    let devices = ctx.hotkeys.inputs().devices();
    axis_pass_for_devices(ctx, buttons, events, &devices);
}

fn axis_pass_for_devices(
    ctx: &ApiContext,
    buttons: &mut ButtonHotkeys,
    events: &[openrigcue_sources::api::JoystickEvent],
    devices: &[String],
) {
    let capturing = ctx.hotkeys.capturing();
    let pass = {
        let config = ctx.state.config.read();
        buttons.update(events, &config.settings().shortcuts, devices, capturing)
    };
    set_canned(ctx, &pass.canned);
    for action in &pass.fired {
        actions::run(ctx, &ctx.hotkeys.0.export_dir, action);
    }
    let current = buttons.gamepad_in_order();
    let mut gamepad = lock(&ctx.hotkeys.0.gamepad);
    if *gamepad != current {
        *gamepad = current;
    }
}

/// One keyboard event: the test of the keyboard bindings, and the editor's capture.
pub fn key_pass(ctx: &ApiContext, keys: &mut keyboard::KeyboardHotkeys, event: keyboard::KeyEvent) {
    let session = ctx.hotkeys.capture_session();
    keys.set_capture_session(session);
    let pass = {
        let config = ctx.state.config.read();
        let bindings = config.settings().shortcuts.bindings.iter().map(|(a, b)| (a.as_str(), b.keys.as_str()));
        keys.event(event, bindings, std::time::Instant::now())
    };
    if let Some(captured) = keys.captured() {
        let mut shared = lock(&ctx.hotkeys.0.captured_keys);
        // Published only if the editor session it was read in is still the open one.
        let still_open =
            ctx.hotkeys.capturing() && Some(ctx.hotkeys.0.capture_session.load(Ordering::SeqCst)) == session;
        if still_open {
            *shared = Some((captured.to_owned(), keys.captures()));
        }
    }
    if let Some((action, error)) = &pass.invalid {
        // A binding that could not be tested is reported.
        tracing::error!(%action, %error, "processKeys");
    }
    set_canned(ctx, &pass.canned);
    for action in &pass.fired {
        actions::run(ctx, &ctx.hotkeys.0.export_dir, action);
    }
}

/// The `ui_actions` part of one UI main-loop pass: the slider steps, the motion-compensation shortcuts, then
/// the queue is emptied.
/// `Err` names an action whose slider row is not on its page: the rows before it have
/// moved and the error ends the main loop.
pub fn consume_ui_actions(ctx: &ApiContext) -> Result<(), crate::profile::MainLoopEnd> {
    let actions = ctx.hotkeys.ui_actions();
    if actions.is_empty() {
        return Ok(());
    }
    let game_running = ctx.state.session.read().game.is_some();
    let settings_before = ctx.state.config.read().settings().clone();
    let mut settings_value = serde_json::to_value(&settings_before).unwrap_or_default();
    let mut new_settings = None;
    let mut tune_changed = false;
    let missing;
    {
        let mut profile = ctx.state.profile.write();
        let mut tune_value = serde_json::to_value(&profile.tune).unwrap_or_default();
        let outcome = sliders::apply(
            &actions,
            game_running,
            settings_before.app.enable_hapticr,
            settings_before.features.smoothing_slider_step,
            &mut sliders::Stores { tune: &mut tune_value, settings: &mut settings_value },
        );
        missing = outcome.missing;
        if !outcome.moved.is_empty() {
            match serde_json::from_value::<Tune>(tune_value) {
                Ok(tune) if tune != profile.tune => {
                    let rebuild = tune.shaker_effects != profile.tune.shaker_effects;
                    profile.tune = tune;
                    profile.generation += 1;
                    tune_changed = true;
                    if rebuild {
                        drop(profile);
                        ctx.shakers
                            .load_channel_groups(&ctx.state)
                            .map_err(crate::profile::MainLoopEnd::ShakerRebuild)?;
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "hotkey slider change not applied"),
            }
            match serde_json::from_value::<Settings>(settings_value) {
                Ok(settings) if settings != settings_before => new_settings = Some(settings),
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "hotkey settings change not applied"),
            }
        }
    }
    if tune_changed {
        ctx.state.announce(ServiceEvent::TuneChanged);
    }
    if let Some(settings) = new_settings {
        ctx.state.config.write().replace_settings(settings);
        ctx.state.announce(ServiceEvent::Settings);
    }
    if let Some(action) = missing {
        return Err(MissingSlider(action).into());
    }
    let raw = crate::compensation::RawSources {
        accessories: &ctx.accessories,
        yaw_vr: &ctx.motion.driver_settings().yaw_shared,
    };
    let has = |name: &str| actions.iter().any(|a| a == name);
    for axis in ["pitch", "roll", "yaw"] {
        let action = format!("motion_compensation_{axis}_calibrate");
        if has(&action) || has("motion_compensation_all_calibrate") {
            crate::compensation::run_shortcut(&ctx.state, raw, &ctx.compensation, &action);
        }
    }
    for axis in ["pitch", "roll", "sway", "surge", "yaw", "heave"] {
        let action = format!("motion_compensation_{axis}_switch");
        if has(&action) {
            crate::compensation::run_shortcut(&ctx.state, raw, &ctx.compensation, &action);
        }
    }
    if has("motion_compensation_recenter") {
        crate::compensation::run_shortcut(&ctx.state, raw, &ctx.compensation, "motion_compensation_recenter");
    }
    ctx.hotkeys.clear_ui_actions();
    Ok(())
}

/// The error for a queued UI action whose slider row is not on its page.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("KeyError: no slider for the action {0:?}")]
pub struct MissingSlider(pub String);

/// The controller loop's button hotkeys: enabled or disabled by `APP.enable_sdl2`, read once here. The flag
/// is kept for the editor's controller list ([`Hotkeys::controllers`]).
pub fn button_hotkeys(ctx: &ApiContext) -> ButtonHotkeys {
    let enabled = ctx.state.config.read().settings().app.sdl2_joysticks;
    ctx.hotkeys.0.controllers_enabled.store(enabled, Ordering::SeqCst);
    ButtonHotkeys::new(enabled)
}

/// The running hotkey loops.
pub struct HotkeyThreads {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    keyboards: Option<evdev_keys::KeyboardReaders>,
}

impl HotkeyThreads {
    /// Starts the controller loop and, under the Windows rules, the keyboard loop with the keyboard readers.
    /// The `ui_actions` are consumed by the UI main loop ([`consume_ui_actions`]).
    pub fn start(ctx: ApiContext) -> std::io::Result<HotkeyThreads> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        let windows = ctx.state.platform.is_windows();
        {
            let (ctx, stop) = (ctx.clone(), stop.clone());
            let events = ctx.hotkeys.inputs().events();
            let mut buttons = button_hotkeys(&ctx);
            let enabled = ctx.hotkeys.0.controllers_enabled.load(Ordering::SeqCst);
            handles.push(std::thread::Builder::new().name("hookAxis".into()).spawn(move || {
                let mut joysticks = if enabled {
                    match openrigcue_sources::api::joystick::sdl::SdlJoysticks::open(ctx.hotkeys.inputs().clone()) {
                        Ok(joysticks) => Some(joysticks),
                        Err(error) => {
                            tracing::error!(%error, "Exception in thread hookAxis: SDL2 initialization");
                            return;
                        }
                    }
                } else {
                    None
                };
                while !stop.load(Ordering::SeqCst) {
                    // The shortcut cache reads the registry before SDL events
                    // can reload it during this pass.
                    let devices = ctx.hotkeys.inputs().devices();
                    if let Some(joysticks) = &mut joysticks {
                        if let Err(error) = joysticks.update() {
                            tracing::error!(%error, "Exception in thread hookAxis: SDL2 update");
                            return;
                        }
                    }
                    axis_pass_for_devices(&ctx, &mut buttons, &events.take(), &devices);
                    std::thread::sleep(axis_period(windows));
                }
            })?);
        }
        let keyboards = if windows {
            let (sender, receiver) = std::sync::mpsc::channel::<keyboard::KeyEvent>();
            let readers = evdev_keys::KeyboardReaders::start(
                access::InputRoots::system(),
                sender,
                ctx.hotkeys.0.keyboard_denied.clone(),
            )?;
            let (ctx, stop) = (ctx.clone(), stop.clone());
            handles.push(std::thread::Builder::new().name("hookKeyboard".into()).spawn(move || {
                let mut keys = keyboard::KeyboardHotkeys::default();
                while !stop.load(Ordering::SeqCst) {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(event) => key_pass(&ctx, &mut keys, event),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?);
            Some(readers)
        } else {
            None
        };
        Ok(HotkeyThreads { stop, handles, keyboards })
    }

    /// Ends every loop and waits for it.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        drop(self.keyboards);
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests;
