//! The Joystick page in the service: the joystick profiles, the selected profile,
//! the assignment dialog with its axis capture, and the live bars.
//!
//! Holds the page's handlers (profile change, add, duplicate, remove, assign, reset, joystick and mode
//! change, confirmation, close), its capture loop (every 0.1 s) and its bar pass.
//! The page state lives here, on the server: every browser shows the same dialog.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use openrigcue_config::settings::{AxisMapping, JoystickProfiles};
use openrigcue_sources::api::joystick::page::{self, Capture, Entry};
use openrigcue_sources::api::JoystickInputs;
use serde::Serialize;

use crate::runtime::Shared;
use crate::state::ServiceState;

/// The capture loop sleeps 0.1 s per pass.
const CAPTURE_PERIOD: Duration = Duration::from_millis(100);

/// What a page action can fail with.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JoystickError {
    /// The alert of the add / duplicate dialogs.
    #[error("A profile with this name exists already; choose another name.")]
    NameExists,
    /// The profile table has no profile of that name.
    #[error("no joystick profile {0:?}")]
    NoProfile(String),
    /// The assignment dialog is not open on a row.
    #[error("no row is being assigned")]
    NoRow,
    #[error("cannot store the joystick profiles: {0}")]
    Store(String),
}

/// The assignment dialog and the widgets in it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Dialog {
    pub open: bool,
    /// The row being assigned.
    pub key: Option<String>,
    /// The joystick dropdown's entries and value.
    pub devices: Vec<String>,
    pub device: String,
    /// The mode dropdown's value.
    pub mode: String,
    /// The invert switch's value and visibility (with its label).
    pub invert: bool,
    pub invert_visible: bool,
    /// The mode example text.
    pub mode_example: String,
    /// The instruction text and whether it has the red bold style.
    pub instruction: String,
    pub instruction_alert: bool,
    /// The "Type" / "Axis" labels.
    pub kind: String,
    pub trigger: String,
    /// The reset button's visibility.
    pub reset_visible: bool,
    /// Whether the capture loop runs.
    pub listening: bool,
}

impl Default for Dialog {
    /// The widgets as first built: the mode list on its first entry, no example text, the invert
    /// switch and the reset button visible.
    fn default() -> Self {
        Dialog {
            open: false,
            key: None,
            devices: Vec::new(),
            device: String::new(),
            mode: page::SINGLE_AXIS.to_owned(),
            invert: false,
            invert_visible: true,
            mode_example: String::new(),
            instruction: String::new(),
            instruction_alert: false,
            kind: String::new(),
            trigger: String::new(),
            reset_visible: true,
            listening: false,
        }
    }
}

impl Dialog {
    /// Fills the joystick list: `None`, then every connected joystick by name; `No joystick found` alone
    /// without any. The first entry is selected.
    fn load_joysticks(&mut self, devices: &[String]) {
        self.devices = if devices.is_empty() {
            vec![page::NO_JOYSTICK_FOUND.to_owned()]
        } else {
            std::iter::once(page::NO_JOYSTICK_SELECTED.to_owned()).chain(devices.iter().cloned()).collect()
        };
        self.device = self.devices[0].clone();
    }

    fn show_mode(&mut self, mode: &str) {
        let (example, invert_visible) = page::mode_example(mode);
        self.mode_example = example.to_owned();
        self.invert_visible = invert_visible;
    }
}

/// One row of the page.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub key: String,
    pub label: String,
    /// The row's instruction text.
    pub instructions: String,
    /// The settings button's title.
    pub tooltip: String,
    /// The bar (0..100).
    pub progress: i64,
}

/// The Magic Wheel battery button (non-Windows platforms).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Battery {
    /// Hidden by a pass that found no Magic Wheel; visible as first built.
    pub visible: bool,
    /// The level last shown; `None` until a Magic Wheel was found.
    pub percent: Option<i64>,
    pub icon: &'static str,
    pub title: String,
}

impl Default for Battery {
    /// As first built: `bluetooth_off.svg`, "Magicwheel not connected", visible.
    fn default() -> Self {
        Battery { visible: true, percent: None, icon: "bluetooth_off", title: "No Magicwheel connected".to_owned() }
    }
}

type Assignments = BTreeMap<String, AxisMapping>;

/// What the rows show and what the bar pass compares against.
#[derive(Default)]
struct Events {
    /// The profile's assignments as the last `load_events` read them; `None` before the first one, when the rows
    /// show their plain labels and the buttons have no title.
    loaded: Option<Option<Assignments>>,
    /// The assignments the previous bar pass saw.
    previous_config: Option<Option<Assignments>>,
}

/// What the page shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PageView {
    /// The profile dropdown's entries and value (the selected profile).
    pub profiles: Vec<String>,
    pub profile: String,
    /// Whether the Remove button is enabled.
    pub remove_enabled: bool,
    pub rows: Vec<Row>,
    /// `None` on Windows, where the page has no battery button.
    pub battery: Option<Battery>,
    pub dialog: Dialog,
}

struct Inner {
    state: ServiceState,
    inputs: JoystickInputs,
    profile: Shared<String>,
    dialog: Mutex<Dialog>,
    /// Bars as last set; `load_events` zeroes them.
    progress: Mutex<BTreeMap<String, i64>>,
    events: Mutex<Events>,
    battery: Mutex<Battery>,
    /// Whether Remove is enabled: only a profile change by the user sets it (it starts disabled).
    remove_enabled: AtomicBool,
    /// Ends capture loops started before the latest one.
    capture_generation: AtomicU64,
    /// Whether opening the dialog starts the capture thread (tests drive the passes themselves).
    capture_thread: bool,
}

/// The Joystick page's server side.
#[derive(Clone)]
pub struct JoystickService {
    inner: Arc<Inner>,
}

fn entry(mapping: &AxisMapping) -> Entry<'_> {
    Entry {
        joystick: &mapping.joystick,
        kind: &mapping.kind,
        trigger: &mapping.trigger,
        mode: &mapping.mode,
        invert_axis: mapping.invert_axis,
    }
}

impl JoystickService {
    /// `profile` is the selected profile name, shared with the joystick listener.
    pub fn new(state: ServiceState, inputs: JoystickInputs, profile: Shared<String>) -> JoystickService {
        if profile.read().is_empty() {
            *profile.write() = page::DEFAULT_PROFILE.to_owned();
        }
        JoystickService {
            inner: Arc::new(Inner {
                state,
                inputs,
                profile,
                dialog: Mutex::new(Dialog::default()),
                progress: Mutex::new(BTreeMap::new()),
                events: Mutex::new(Events::default()),
                battery: Mutex::new(Battery::default()),
                remove_enabled: AtomicBool::new(false),
                capture_generation: AtomicU64::new(0),
                capture_thread: true,
            }),
        }
    }

    /// A service whose capture passes are run by the caller.
    #[cfg(test)]
    fn without_capture_thread(state: ServiceState, inputs: JoystickInputs) -> JoystickService {
        let service = JoystickService::new(state, inputs, Shared::new(String::new()));
        let inner = Arc::try_unwrap(service.inner).unwrap_or_else(|_| unreachable!("just created"));
        JoystickService { inner: Arc::new(Inner { capture_thread: false, ..inner }) }
    }

    /// A service on a first-run configuration of its own, for tests of other parts of the API.
    #[cfg(test)]
    pub(crate) fn offline() -> JoystickService {
        let config = crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run());
        let state = ServiceState::new(config, crate::state::OriginalPlatform::Windows);
        JoystickService::new(state, JoystickInputs::default(), Shared::new(String::new()))
    }

    fn dialog(&self) -> MutexGuard<'_, Dialog> {
        self.inner.dialog.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn progress(&self) -> MutexGuard<'_, BTreeMap<String, i64>> {
        self.inner.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn events(&self) -> MutexGuard<'_, Events> {
        self.inner.events.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn battery(&self) -> MutexGuard<'_, Battery> {
        self.inner.battery.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The selected profile name.
    pub fn selected_profile(&self) -> String {
        self.inner.profile.read().clone()
    }

    /// Sets the selected profile name; the UI page owns the subsequent widget updates.
    pub fn set_selected_profile(&self, name: String) {
        *self.inner.profile.write() = name;
    }

    pub fn profiles_json(&self) -> Result<serde_json::Value, JoystickError> {
        let table = self
            .inner
            .state
            .config
            .read()
            .metadata_table("joystick")
            .map_err(|error| JoystickError::Store(error.native_detail()))?;
        openrigcue_config::import::de::from_legacy(&openrigcue_config::import::value::LegacyValue::Dict(table))
            .map_err(|error| JoystickError::Store(error.to_string()))
    }

    pub fn store_profile_value(&self, name: &str, value: serde_json::Value) -> Result<(), JoystickError> {
        self.inner
            .state
            .config
            .write()
            .set_joystick_entry(name, Some(value.into()))
            .map_err(|error| JoystickError::Store(error.native_detail()))
    }

    pub fn delete_profile_key(&self, name: &str) -> Result<(), JoystickError> {
        let mut store = self.inner.state.config.write();
        if !store.delete_joystick_entry(name).map_err(|error| JoystickError::Store(error.native_detail()))? {
            return Err(JoystickError::NoProfile(name.to_owned()));
        }
        Ok(())
    }

    pub fn devices_json(&self) -> serde_json::Value {
        self.inner.inputs.page_snapshot()
    }

    pub fn clear_last_state(&self) {
        self.inner.inputs.clear_last_state();
    }

    fn profiles(&self) -> JoystickProfiles {
        self.inner.state.config.read().config().joysticks.clone()
    }

    /// The profile table is autocommitted: every change is written at once
    /// ([`crate::store::ConfigStore::replace_joysticks`], which also covers a folder that was never saved).
    fn store(
        &self,
        change: impl FnOnce(&mut JoystickProfiles) -> Result<(), JoystickError>,
    ) -> Result<(), JoystickError> {
        let mut config = self.inner.state.config.write();
        let mut profiles = config.config().joysticks.clone();
        change(&mut profiles)?;
        config.replace_joysticks(profiles).map_err(|e| JoystickError::Store(e.to_string()))
    }

    /// Runs one bar pass and returns the page's widget state. Whatever the Status switch
    /// says, the bars follow the assigned inputs, the Magic Wheel battery is read
    /// (non-Windows), and a profile whose assignments differ from the previous pass's (`previous_config`) is
    /// reloaded (`load_events`). The switch itself is the controls state.
    pub fn view(&self) -> PageView {
        let profiles = self.profiles();
        let profile = self.selected_profile();
        self.slider_pass(&profile, profiles.profile(&profile));
        let progress = self.progress().clone();
        let loaded = self.events().loaded.clone();
        let rows = page::ASSIGNMENTS
            .iter()
            .map(|(key, label)| {
                let (instructions, tooltip) = match &loaded {
                    None => ((*label).to_owned(), String::new()),
                    Some(assignments) => match assignments.as_ref().and_then(|a| a.get(*key)) {
                        Some(m) => (entry(m).status(), entry(m).tooltip()),
                        None => (page::NOT_CONFIGURED.to_owned(), page::NOT_CONFIGURED.to_owned()),
                    },
                };
                Row {
                    key: (*key).to_owned(),
                    label: (*label).to_owned(),
                    instructions,
                    tooltip,
                    progress: progress.get(*key).copied().unwrap_or(0),
                }
            })
            .collect();
        let battery = (!self.inner.state.platform.is_windows()).then(|| self.battery().clone());
        PageView {
            profiles: profiles.0.keys().cloned().collect(),
            remove_enabled: self.inner.remove_enabled.load(Ordering::SeqCst),
            profile,
            rows,
            battery,
            dialog: self.dialog().clone(),
        }
    }

    /// The body of one bar pass: bars, battery, and a reload when the assignments changed.
    fn slider_pass(&self, profile: &str, assignments: Option<&Assignments>) {
        let controllers = self.inner.inputs.snapshot();
        {
            let mut progress = self.progress();
            for (key, mapping) in assignments.into_iter().flatten() {
                match entry(mapping).progress(&controllers) {
                    Ok(Some(value)) => {
                        progress.insert(key.clone(), value);
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(profile, key, %error, "joystick assignment without an integer axis"),
                }
            }
        }
        if !self.inner.state.platform.is_windows() {
            let mut battery = self.battery();
            match page::magic_wheel_level(&controllers) {
                Some(level) => {
                    let percent = page::battery_percent(level);
                    battery.percent = Some(percent as i64);
                    battery.icon = page::battery_icon(percent);
                    battery.title = format!("Magicwheel battery charge: {}%", percent as i64);
                    battery.visible = true;
                }
                // The button is hidden; its image and title stay.
                None => battery.visible = false,
            }
        }
        let current = assignments.cloned();
        let changed = self.events().previous_config.as_ref() != Some(&current);
        if changed {
            self.load_events();
        }
        self.events().previous_config = Some(current);
    }

    /// Reloads the rows: they show the selected profile's assignments and the bars restart at 0.
    fn load_events(&self) {
        let profile = self.selected_profile();
        let assignments = self.profiles().profile(&profile).cloned();
        self.events().loaded = Some(assignments);
        self.progress().clear();
    }

    /// Selects a profile and reloads the rows; used by the add / duplicate / remove confirmations, which
    /// leave the Remove button as it was.
    fn choose_profile(&self, name: &str) {
        *self.inner.profile.write() = name.to_owned();
        self.load_events();
    }

    /// The user picks a profile: Remove is enabled except on `Default`.
    pub fn select_profile(&self, name: &str) {
        self.inner.remove_enabled.store(name != page::DEFAULT_PROFILE, Ordering::SeqCst);
        self.choose_profile(name);
    }

    /// Confirms the add dialog: an empty profile under a new name, then selected.
    pub fn add_profile(&self, name: &str) -> Result<(), JoystickError> {
        self.store(|profiles| {
            if profiles.0.contains_key(name) {
                return Err(JoystickError::NameExists);
            }
            profiles.0.insert(name.to_owned(), BTreeMap::new());
            Ok(())
        })?;
        self.choose_profile(name);
        Ok(())
    }

    /// The name the duplicate dialog proposes for a copy of the selected profile.
    pub fn proposed_duplicate_name(&self) -> String {
        let profiles = self.profiles();
        page::propose_name(&self.selected_profile(), |name| profiles.0.contains_key(name))
    }

    /// Confirms the duplicate dialog: a copy of the selected profile, then selected.
    pub fn duplicate_profile(&self, name: &str) -> Result<(), JoystickError> {
        let current = self.selected_profile();
        self.store(|profiles| {
            if profiles.0.contains_key(name) {
                return Err(JoystickError::NameExists);
            }
            let copy = profiles.0.get(&current).cloned().ok_or_else(|| JoystickError::NoProfile(current.clone()))?;
            profiles.0.insert(name.to_owned(), copy);
            Ok(())
        })?;
        self.choose_profile(name);
        Ok(())
    }

    /// The remove dialog's question.
    pub fn remove_question(&self) -> String {
        format!("{}{}", page::REMOVE_QUESTION, self.selected_profile())
    }

    /// Confirms the remove dialog: the selected profile goes (`Default` too, as Remove stays enabled on
    /// it after a removal), `Default` is selected and the rows are reloaded, which is an error once `Default`
    /// is gone.
    pub fn remove_profile(&self) -> Result<(), JoystickError> {
        let current = self.selected_profile();
        self.store(|profiles| {
            profiles.0.remove(&current).map(|_| ()).ok_or_else(|| JoystickError::NoProfile(current.clone()))
        })?;
        self.choose_profile(page::DEFAULT_PROFILE);
        if self.profiles().profile(page::DEFAULT_PROFILE).is_none() {
            return Err(JoystickError::NoProfile(page::DEFAULT_PROFILE.to_owned()));
        }
        Ok(())
    }

    /// Opens the assignment dialog on the row `key`: a configured row shows its settings; another starts a capture with
    /// the row's default mode.
    pub fn open_dialog(&self, key: &str) -> Result<Dialog, JoystickError> {
        let profiles = self.profiles();
        let profile = self.selected_profile();
        let assignments = profiles.profile(&profile).ok_or_else(|| JoystickError::NoProfile(profile.clone()))?;
        let mut dialog = self.dialog();
        match assignments.get(key) {
            Some(settings) => {
                dialog.devices = vec![settings.joystick.clone()];
                dialog.device.clone_from(&settings.joystick);
                dialog.instruction = page::CURRENT_SETTINGS.to_owned();
                dialog.instruction_alert = false;
                dialog.kind.clone_from(&settings.kind);
                dialog.trigger.clone_from(&settings.trigger);
                dialog.reset_visible = true;
                dialog.invert = settings.invert_axis;
                dialog.mode.clone_from(&settings.mode);
                dialog.show_mode(&settings.mode);
                dialog.listening = false;
            }
            None => {
                dialog.instruction.clear();
                dialog.instruction_alert = false;
                dialog.load_joysticks(&self.inner.inputs.devices());
                dialog.kind.clear();
                dialog.trigger.clear();
                dialog.reset_visible = false;
                let mode = page::default_mode(key);
                dialog.mode = mode.to_owned();
                dialog.show_mode(mode);
                dialog.listening = true;
            }
        }
        dialog.key = Some(key.to_owned());
        dialog.open = true;
        let listening = dialog.listening;
        let snapshot = dialog.clone();
        drop(dialog);
        if listening {
            self.start_capture();
        }
        Ok(snapshot)
    }

    /// The dialog's Reset button: back to choosing a joystick and capturing.
    pub fn reset_dialog(&self) -> Dialog {
        let mut dialog = self.dialog();
        dialog.instruction.clear();
        dialog.instruction_alert = false;
        dialog.load_joysticks(&self.inner.inputs.devices());
        dialog.kind.clear();
        dialog.trigger.clear();
        dialog.reset_visible = false;
        dialog.listening = true;
        let snapshot = dialog.clone();
        drop(dialog);
        self.start_capture();
        snapshot
    }

    /// A joystick is chosen in the dialog: the capture instruction once a joystick is chosen.
    pub fn choose_device(&self, device: &str) -> Dialog {
        let mut dialog = self.dialog();
        device.clone_into(&mut dialog.device);
        if device != page::NO_JOYSTICK_SELECTED {
            dialog.instruction = page::CAPTURE_INSTRUCTION.to_owned();
            dialog.instruction_alert = true;
        } else {
            // The mode example is cleared here, not the instruction.
            dialog.mode_example.clear();
            dialog.instruction_alert = false;
        }
        dialog.clone()
    }

    /// A mode is chosen in the dialog: its example text and the invert switch's visibility follow.
    pub fn choose_mode(&self, mode: &str) -> Dialog {
        let mut dialog = self.dialog();
        mode.clone_into(&mut dialog.mode);
        dialog.show_mode(mode);
        dialog.clone()
    }

    /// The invert switch (no handler of its own; read on confirmation).
    pub fn set_invert(&self, invert: bool) -> Dialog {
        let mut dialog = self.dialog();
        dialog.invert = invert;
        dialog.clone()
    }

    /// The dialog's close and cancel: the capture ends.
    pub fn close_dialog(&self) -> Dialog {
        let mut dialog = self.dialog();
        dialog.listening = false;
        dialog.open = false;
        dialog.clone()
    }

    /// Confirms the assignment dialog: an empty "Axis" removes the row's assignment, anything else stores the
    /// dialog's joystick, type, axis, mode and invert switch.
    pub fn confirm_dialog(&self) -> Result<Dialog, JoystickError> {
        let dialog = self.dialog().clone();
        let key = dialog.key.clone().ok_or(JoystickError::NoRow)?;
        let profile = self.selected_profile();
        self.store(|profiles| {
            let assignments = profiles.0.get_mut(&profile).ok_or_else(|| JoystickError::NoProfile(profile.clone()))?;
            if dialog.trigger.is_empty() {
                assignments.remove(&key);
            } else {
                assignments.insert(
                    key.clone(),
                    AxisMapping {
                        joystick: dialog.device.clone(),
                        kind: dialog.kind.clone(),
                        trigger: dialog.trigger.clone(),
                        mode: dialog.mode.clone(),
                        invert_axis: dialog.invert,
                    },
                );
            }
            Ok(())
        })?;
        self.load_events();
        let mut dialog = self.dialog();
        dialog.listening = false;
        dialog.open = false;
        Ok(dialog.clone())
    }

    /// Starts the capture loop in its own thread; it runs while the dialog listens and no newer capture
    /// started.
    fn start_capture(&self) {
        if !self.inner.capture_thread {
            return;
        }
        let generation = self.inner.capture_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let service = self.clone();
        let spawned = std::thread::Builder::new().name("hook_joystick_config".into()).spawn(move || {
            let mut capture = Capture::default();
            let mut previous_devices: Option<Vec<String>> = None;
            while service.dialog().listening && service.inner.capture_generation.load(Ordering::SeqCst) == generation {
                service.capture_pass(&mut capture, &mut previous_devices);
                std::thread::sleep(CAPTURE_PERIOD);
            }
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot start the joystick capture");
        }
    }

    /// One capture pass: the strongest new axis of the chosen joystick goes to "Type"/"Axis";
    /// after a joystick was plugged in or out the labels are cleared and the list reloaded.
    fn capture_pass(&self, capture: &mut Capture, previous_devices: &mut Option<Vec<String>>) {
        let controllers = self.inner.inputs.snapshot();
        let devices = self.inner.inputs.devices();
        let mut dialog = self.dialog();
        let device = dialog.device.clone();
        if let Some((kind, trigger)) = capture.step(&device, &controllers) {
            dialog.kind = kind;
            dialog.trigger = trigger;
        }
        if let Some(previous) = previous_devices.as_ref() {
            if *previous != devices {
                dialog.kind.clear();
                dialog.trigger.clear();
                dialog.load_joysticks(&devices);
            }
        }
        *previous_devices = Some(devices);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OriginalPlatform;
    use crate::store::ConfigStore;
    use openrigcue_config::settings::Configuration;

    fn service(platform: OriginalPlatform) -> (JoystickService, JoystickInputs) {
        // The first-run configuration under the Windows rules has an empty `Default` profile.
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), platform);
        let inputs = JoystickInputs::default();
        (JoystickService::without_capture_thread(state, inputs.clone()), inputs)
    }

    /// Sets the Status switch.
    fn set_status(service: &JoystickService, on: bool) {
        service.inner.state.controls.write().switches.joystick = on;
    }

    /// The first bar pass on a first run (Status switch off): the pass runs whatever
    /// the switch, finds no previous assignments
    /// and reloads the rows, so the rows show the magicbox Leonardo pedals of the Default profile at once.
    #[test]
    fn first_run_rows() {
        let (service, _) = service(OriginalPlatform::Windows);
        assert!(!service.inner.state.controls.read().switches.joystick);
        let view = service.view();
        assert_eq!(view.profile, "Default");
        assert_eq!(view.profiles, ["Default"]);
        assert!(!view.remove_enabled);
        let labels: Vec<_> = view.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["Left", "Right", "Front", "Back", "Rotate Left", "Rotate Right", "Speed RPM"]);
        assert_eq!(
            (view.rows[0].instructions.as_str(), view.rows[0].tooltip.as_str()),
            ("Not configured", "Not configured")
        );
        assert_eq!(view.rows[2].instructions, "Not configured", "Windows' Default profile is empty");
        assert!(view.battery.is_none(), "no battery button on Windows");
    }

    /// The add / duplicate / remove confirmations, and the Remove button, which only
    /// a profile change by the user enables or disables.
    #[test]
    fn profile_actions() {
        let (service, _) = service(OriginalPlatform::Windows);
        assert_eq!(service.add_profile("Default"), Err(JoystickError::NameExists));
        service.add_profile("Wheel").unwrap();
        assert_eq!(service.selected_profile(), "Wheel");
        let view = service.view();
        assert!(view.rows.iter().all(|r| r.instructions == "Not configured"));
        assert!(!view.remove_enabled, "set_value does not fire on_profile_change");
        service.select_profile("Default");
        assert_eq!(service.proposed_duplicate_name(), "Default (1)");
        service.duplicate_profile("Default (1)").unwrap();
        assert_eq!(service.view().rows[2].instructions, "Not configured");
        assert!(!service.view().remove_enabled);
        service.select_profile("Default (1)");
        assert!(service.view().remove_enabled);
        assert_eq!(service.remove_question(), "Please confirm that this profile should be deleted: Default (1)");
        service.remove_profile().unwrap();
        assert_eq!(service.selected_profile(), "Default");
        assert!(service.view().remove_enabled, "still enabled on Default after a removal");
        assert_eq!(service.remove_profile(), Err(JoystickError::NoProfile("Default".into())), "load_events raises");
        assert_eq!(service.selected_profile(), "Default");
        assert_eq!(service.view().profiles, ["Wheel"]);
    }

    /// Adding a profile on a configuration folder that was never saved: the autocommitted profile is
    /// there at the next start.
    #[test]
    fn added_profile_on_a_fresh_folder_survives_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = openrigcue_config::settings::paths::ConfigDir::at(tmp.path().join("openrigcue"));
        let state = ServiceState::new(ConfigStore::load(dir.clone()).unwrap(), OriginalPlatform::Windows);
        let service = JoystickService::without_capture_thread(state, JoystickInputs::default());
        service.add_profile("Wheel").unwrap();
        let restarted = ConfigStore::load(dir).unwrap();
        assert_eq!(restarted.config().joysticks.0.keys().collect::<Vec<_>>(), ["Default", "Wheel"]);
    }

    /// Opening the dialog on a new row, the joystick choice, a capture pass and the confirmation.
    #[test]
    fn assign_by_capture() {
        let (service, inputs) = service(OriginalPlatform::Windows);
        set_status(&service, true);
        inputs.connect("Wheel");
        let dialog = service.open_dialog("left").unwrap();
        assert_eq!(dialog.devices, ["None", "Wheel"]);
        assert_eq!((dialog.device.as_str(), dialog.mode.as_str()), ("None", "Dual Axis"));
        assert_eq!(dialog.mode_example, page::DUAL_AXIS_EXAMPLE);
        assert!(!dialog.invert_visible && !dialog.reset_visible && dialog.listening);
        let dialog = service.choose_device("Wheel");
        assert!(dialog.instruction_alert);
        assert_eq!(dialog.instruction, page::CAPTURE_INSTRUCTION);
        inputs.set_axis("Wheel", 0, -25000);
        let (mut capture, mut previous) = (Capture::default(), None);
        service.capture_pass(&mut capture, &mut previous);
        let dialog = service.dialog().clone();
        assert_eq!((dialog.kind.as_str(), dialog.trigger.as_str()), ("axis <", "0"));
        service.confirm_dialog().unwrap();
        // The first pass after the change sets the bar, then finds changed assignments and reloads the rows.
        let rows = service.view().rows;
        assert_eq!(rows[0].instructions, "Wheel - dual axis 0 <");
        assert_eq!(rows[0].progress, 0);
        let rows = service.view().rows;
        assert_eq!(rows[0].progress, 100 - 23, "100 - int(map(-25000, -32768, 0, 0, 100))");
        assert!(!service.dialog().listening);
        // A configured row opens on its settings; an empty axis on confirmation removes the assignment.
        let dialog = service.open_dialog("left").unwrap();
        assert_eq!(
            (dialog.instruction.as_str(), dialog.devices.len(), dialog.listening),
            ("Current Settings", 1, false)
        );
        service.reset_dialog();
        assert_eq!(service.dialog().trigger, "");
        service.confirm_dialog().unwrap();
        assert_eq!(service.view().rows[0].instructions, "Not configured");
        service.close_dialog();
    }

    /// Capture: a joystick plugged in or out clears the labels and reloads the list.
    #[test]
    fn capture_reloads_on_device_change() {
        let (service, inputs) = service(OriginalPlatform::Windows);
        service.open_dialog("rotate_left").unwrap();
        assert_eq!(service.dialog().devices, ["No joystick found"]);
        let (mut capture, mut previous) = (Capture::default(), None);
        service.capture_pass(&mut capture, &mut previous);
        inputs.connect("Pedals");
        service.dialog().kind = "axis >".into();
        service.capture_pass(&mut capture, &mut previous);
        let dialog = service.dialog().clone();
        assert_eq!(dialog.devices, ["None", "Pedals"]);
        assert_eq!(dialog.kind, "");
        service.close_dialog();
    }
}
