//! The notifications of the page header: those the main loop raises about the
//! running game ("Game not configured", "Game instructions", "Game Telemetry Not Detected"), those
//! the hardware widget raises about the devices (Yaw VR not centred, belt not calibrated, iFlag firmware), the
//! short alerts and the sound-card notifications of the sound-device monitor.
//!
//! A hidden notification still counts as added, so
//! the checks that look for an existing notification by kind still find it; [`Notifications`] remembers which
//! kinds were ever added for those checks. No notification is spoken.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::state::{ServiceEvent, ServiceState};

/// The default notification colour.
pub const POPUP_COLOUR: &str = "#83440f";

/// An alert is hidden after three seconds.
pub const ALERT_TIMEOUT: Duration = Duration::from_secs(3);

/// What a notification is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationKind {
    /// The game's status is "not patched": its telemetry is not configured.
    GameNotConfigured,
    /// The game's status is "instructions": it is set up by following its instructions.
    GameInstructions,
    /// A listener could not bind its port because `process` holds it.
    PortInUse { process: String, port: u16 },
    /// The sound-device monitor's default sound card error.
    DefaultSoundCard,
    /// The sound-device monitor's surround sound card error: the speaker layout `profile` needs the channel
    /// configuration `label`; `suggest_stereo` when the message also suggests a 2-channel layout. The UI fills
    /// these into its translated title and message.
    SurroundRequired { profile: String, label: String, suggest_stereo: bool },
    /// The Yaw VR is not centred: buttons "Center" and "Dismiss".
    YawVrNotCentered,
    /// The belt is not calibrated: buttons "Calibrate" and "Dismiss".
    BeltCalibration,
    /// The iFlag firmware is old: button "Dismiss".
    IflagFirmware,
    /// A supported Linux controller could not be opened due to device permissions.
    DevicePermissions { devices: Vec<String> },
    /// The system refused the Elite Dangerous listener's reads of the game's memory: button "Dismiss".
    MemoryReadRefused,
    /// A short alert: no buttons, hidden after [`ALERT_TIMEOUT`].
    Alert,
}

impl NotificationKind {
    /// Whether `self` and `other` are of the same kind; notifications of one kind are hidden as a group.
    fn same_prefix(&self, other: &NotificationKind) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    /// The surround kind without values, for [`Notifications::withdraw`], which hides every notification of the
    /// kind whatever layout it names.
    pub fn surround() -> NotificationKind {
        NotificationKind::SurroundRequired { profile: String::new(), label: String::new(), suggest_stereo: false }
    }
}

/// One shown notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notification {
    pub id: u64,
    pub game: String,
    #[serde(flatten)]
    pub kind: NotificationKind,
    pub title: String,
    pub message: String,
    /// The background colour.
    pub colour: String,
}

/// What a notification's first button does once it hid the notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmed {
    /// "Game Page": opens the Games page on its PC tab.
    GamePage,
    /// "Instructions" of a game: opens the game's instructions.
    Instructions { game: String },
    /// "Center": centres the Yaw VR.
    CenterYawVr,
    /// "Calibrate": calibrates the belt.
    CalibrateBelt,
}

/// The state the hardware widget reads for its device notifications.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HardwareWidget {
    /// `MOTIONBOX.auto_search`.
    pub motion_auto_search: bool,
    /// The motion platform is connected.
    pub motion_connected: bool,
    /// The model needs an axis assignment and it is not complete.
    pub axis_assignment_incomplete: bool,
    /// The model's manufacturer is Yaw VR.
    pub yaw_vr: bool,
    /// The Yaw VR is centred.
    pub yaw_vr_centered: bool,
    /// `IFLAG.auto_search`.
    pub iflag_auto_search: bool,
    /// The iFlag is connected.
    pub iflag_connected: bool,
    /// The iFlag firmware is 1.3 or older.
    pub iflag_old_firmware: bool,
    /// `BELT_TENSIONER.auto_search`.
    pub belt_auto_search: bool,
    /// The belt is connected.
    pub belt_connected: bool,
    /// The belt is not calibrated and its switch is on.
    pub belt_needs_calibration: bool,
}

/// The notifications currently shown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Notifications {
    pub items: Vec<Notification>,
    next_id: u64,
    /// Games (the one running at the click, `none` included) whose port
    /// notification was dismissed in this run.
    #[serde(skip)]
    port_dismissed: BTreeSet<String>,
    /// Device notifications ever added (hidden ones too).
    #[serde(skip)]
    added: DeviceFlags,
    /// Whether a belt / iFlag notification is being shown (false until the first one).
    #[serde(skip)]
    showing: DeviceFlags,
    /// A Dismiss click on a device notification, for the rest of the run.
    #[serde(skip)]
    dismissed: DeviceFlags,
    #[serde(skip)]
    device_permissions_notified: bool,
    /// The memory-read notification was raised in the game session that is running.
    #[serde(skip)]
    memory_read_notified: bool,
}

/// One flag per device notification.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DeviceFlags {
    yaw_vr: bool,
    belt: bool,
    iflag: bool,
}

/// Text a listener stores in the game errors when its port is taken; the main loop tests the set for exactly
/// this text.
pub const PORT_IN_USE_TEXT: &str = openrigcue_sources::udp_racing::parameters::PORT_IN_USE;

/// Text the Elite Dangerous listener stores in the game errors when the system refuses a read of the game's
/// memory for lack of permission; the main loop tests the set for exactly this text.
pub const MEMORY_READ_REFUSED_TEXT: &str = openrigcue_sources::process_memory::elite_dangerous::memory::READ_REFUSED;

/// Catalog id of Elite Dangerous, the game whose listener reads the game's memory.
const MEMORY_READ_GAME: &str = "ed";

impl Notifications {
    /// The runtime flags the UI's hardware widget snapshot reads.
    pub fn hardware_widget_state(&self) -> openrigcue_ui::backend::HardwareNotificationState {
        openrigcue_ui::backend::HardwareNotificationState {
            showing_belt: self.showing.belt,
            showing_iflag: self.showing.iflag,
            dismissed_yaw_vr: self.dismissed.yaw_vr,
            dismissed_belt: self.dismissed.belt,
            dismissed_iflag: self.dismissed.iflag,
        }
    }

    /// Shows a new notification after hiding every earlier notification of the
    /// same kind (game, port and device notifications): at most one of each kind shows, and every
    /// call shows a new one.
    fn push(&mut self, game: &str, kind: NotificationKind, title: &str, message: String, colour: &str) {
        self.items.retain(|n| !n.kind.same_prefix(&kind));
        self.add(game, kind, title, message, colour);
    }

    /// Shows a new notification under a new id; nothing is hidden.
    fn add(&mut self, game: &str, kind: NotificationKind, title: &str, message: String, colour: &str) -> u64 {
        self.next_id += 1;
        self.items.push(Notification {
            id: self.next_id,
            game: game.to_owned(),
            kind,
            title: title.to_owned(),
            message,
            colour: colour.to_owned(),
        });
        self.next_id
    }

    /// Checks the running game's configuration: a "not patched" game raises the red "Game not configured"
    /// notification, an "instructions" game the instructions notification, unless the user dismissed the game's
    /// notification before.
    pub fn check_game_config(&mut self, game: &str, status: &openrigcue_config::patch::GameStatus, dismissed: bool) {
        use openrigcue_config::patch::GameStatus;
        if dismissed {
            return;
        }
        let name = openrigcue_config::catalog::Catalog::builtin().game(game).map_or(game, |entry| entry.name.as_str());
        match status {
            GameStatus::NotPatched => self.push(
                game,
                NotificationKind::GameNotConfigured,
                "This game is not set up",
                "Leave the game, then set it up on the OpenRigCue Games page.".into(),
                "red",
            ),
            GameStatus::Instructions => self.push(
                game,
                NotificationKind::GameInstructions,
                &format!("{name} telemetry"),
                format!("Open the {name} setup instructions to enable telemetry."),
                POPUP_COLOUR,
            ),
            _ => {}
        }
    }

    /// The main loop's clean-up, after [`Notifications::check_port_in_use`]:
    /// - a shown port notification is hidden once it was dismissed for the running game, once the game errors no
    ///   longer hold the port-in-use text, or once no game runs;
    /// - the instructions are hidden while no game runs or a listener delivers;
    /// - "not configured" is hidden while a game runs that is not the previous game (when that is
    ///   set), so it outlives its game, as its text asks the user to exit the game.
    pub fn tidy(
        &mut self,
        game: Option<&str>,
        previous_game: Option<&str>,
        listener_running: bool,
        errors: &BTreeSet<String>,
    ) {
        let in_use = errors.contains(PORT_IN_USE_TEXT);
        let port_dismissed = self.port_dismissed.contains(game.unwrap_or("none"));
        let hide_config = matches!((game, previous_game), (Some(g), Some(p)) if g != p);
        self.items.retain(|n| match n.kind {
            NotificationKind::GameInstructions => game.is_some() && !listener_running,
            NotificationKind::GameNotConfigured => !hide_config,
            NotificationKind::PortInUse { .. } => game.is_some() && in_use && !port_dismissed,
            NotificationKind::DefaultSoundCard
            | NotificationKind::SurroundRequired { .. }
            | NotificationKind::YawVrNotCentered
            | NotificationKind::BeltCalibration
            | NotificationKind::IflagFirmware
            | NotificationKind::DevicePermissions { .. }
            | NotificationKind::MemoryReadRefused
            | NotificationKind::Alert => true,
        });
    }

    /// The main loop's port check: while no port notification is shown, the
    /// error set holds the port-in-use text and it was not dismissed for this game, a port notification
    /// names the process that holds the port (looked up again every tick until one is found).
    pub fn check_port_in_use(
        &mut self,
        game: &str,
        errors: &BTreeSet<String>,
        port: Option<u16>,
        owner: impl FnOnce(u16) -> Option<String>,
    ) {
        if self.items.iter().any(|n| matches!(n.kind, NotificationKind::PortInUse { .. }))
            || self.port_dismissed.contains(game)
            || !errors.contains(PORT_IN_USE_TEXT)
        {
            return;
        }
        let Some(port) = port else { return };
        let Some(process) = owner(port) else { return };
        let message = format!(
            "No game telemetry reaches OpenRigCue because {process} occupies port {port}. Close {process} so that OpenRigCue can receive the telemetry"
        );
        self.push(game, NotificationKind::PortInUse { process, port }, "Game Telemetry Not Detected", message, "red");
    }

    /// The main loop's memory-read check: while Elite Dangerous runs and the error set holds the refused-read
    /// text, one notification says that the system restricts reading another program's memory and names the
    /// setting that allows it. One per game session: the next can only follow a pass in which another game or
    /// none runs. It stays until it is dismissed; nothing on the system is changed.
    pub fn check_memory_read_refused(&mut self, game: Option<&str>, errors: &BTreeSet<String>) {
        if game != Some(MEMORY_READ_GAME) {
            self.memory_read_notified = false;
            return;
        }
        if self.memory_read_notified || !errors.contains(MEMORY_READ_REFUSED_TEXT) {
            return;
        }
        self.memory_read_notified = true;
        self.push(
            MEMORY_READ_GAME,
            NotificationKind::MemoryReadRefused,
            "Game Telemetry Cannot Be Read",
            "OpenRigCue cannot read the telemetry of Elite Dangerous because this system restricts reading another program's memory. The system setting kernel.yama.ptrace_scope allows it when its value is 0.".to_owned(),
            "red",
        );
    }

    /// The device notifications of the hardware widget, in order:
    /// - Motionbox: with auto search on and the platform connected (and its actuator assignment complete), a Yaw
    ///   VR that is not centred raises "The Yaw VR has not being centered yet" unless a `yawvr` notification was
    ///   ever added or one was dismissed; otherwise every `yawvr` notification is hidden.
    /// - iFlag: with auto search on and the iFlag connected, firmware 1.3 or older raises the firmware notification
    ///   when none was added yet or none is marked as showing, unless one was dismissed. It is never
    ///   hidden here and its showing mark never goes back to false.
    /// - Belt: with auto search on and the belt connected, an uncalibrated belt with its switch on raises the
    ///   calibration notification when none was added yet or none is marked as showing, unless one was
    ///   dismissed; otherwise every `belt` notification is hidden and the showing mark is cleared.
    pub fn set_hardware_widget(&mut self, widget: &HardwareWidget) {
        if widget.motion_auto_search && widget.motion_connected && !widget.axis_assignment_incomplete {
            self.yaw_vr_widget(!widget.yaw_vr_centered && widget.yaw_vr);
        }
        if widget.iflag_auto_search && widget.iflag_connected && widget.iflag_old_firmware {
            self.iflag_firmware_widget();
        }
        if widget.belt_auto_search && widget.belt_connected {
            self.belt_widget(widget.belt_needs_calibration);
        }
    }

    /// Reached only by the connected motion branch after its assignment check.
    pub fn yaw_vr_widget(&mut self, needs_center: bool) {
        if needs_center {
            if !self.added.yaw_vr && !self.dismissed.yaw_vr {
                self.push(
                    "",
                    NotificationKind::YawVrNotCentered,
                    "YawVR",
                    "The Yaw VR still needs to be centered".into(),
                    POPUP_COLOUR,
                );
                self.added.yaw_vr = true;
            }
        } else {
            self.items.retain(|n| n.kind != NotificationKind::YawVrNotCentered);
        }
    }

    pub fn iflag_firmware_widget(&mut self) {
        let create = !self.added.iflag || !self.showing.iflag;
        if create && !self.dismissed.iflag {
            self.push(
                    "",
                    NotificationKind::IflagFirmware,
                    "iFlag",
                    "OpenRigCue cannot work with the firmware on this iFlag. Press the Instructions button to see how the firmware is updated.".into(),
                    POPUP_COLOUR,
                );
            self.added.iflag = true;
            self.showing.iflag = true;
        }
    }

    /// Reached only while the enabled belt is connected.
    pub fn belt_widget(&mut self, needs_calibration: bool) {
        if needs_calibration {
            let create = !self.added.belt || !self.showing.belt;
            if create && !self.dismissed.belt {
                self.push(
                    "",
                    NotificationKind::BeltCalibration,
                    "Belt",
                    "The belt needs calibration. Put the belt on before you start calibrating".into(),
                    POPUP_COLOUR,
                );
                self.added.belt = true;
                self.showing.belt = true;
            }
        } else {
            self.items.retain(|n| n.kind != NotificationKind::BeltCalibration);
            self.showing.belt = false;
        }
    }

    /// The controller access notice: one per run.
    pub fn device_permissions(&mut self, devices: &[String]) -> bool {
        if self.device_permissions_notified || devices.is_empty() {
            return false;
        }
        self.device_permissions_notified = true;
        self.add(
            "",
            NotificationKind::DevicePermissions { devices: devices.to_vec() },
            "Controller access required",
            format!("Allow OpenRigCue to access {}.", devices.join(", ")),
            "red",
        );
        true
    }

    /// A short alert: a new notification under its own id; the caller hides it after
    /// [`ALERT_TIMEOUT`] ([`show_alert`]). Returns its id.
    pub fn alert(&mut self, title: &str, message: &str, colour: &str) -> u64 {
        self.add("", NotificationKind::Alert, title, message.to_owned(), colour)
    }

    /// A notification from the sound-device monitor: always shows a new red notification of `kind` (not
    /// tied to a game), stacked under those already shown, even with the same text. Returns whether anything
    /// changed (always).
    ///
    /// Earlier notifications of the kind are never replaced; [`Notifications::withdraw`] hides them all.
    pub fn raise_device(&mut self, kind: NotificationKind, title: &str, message: String) -> bool {
        self.add("", kind, title, message, "red");
        true
    }

    /// Hides every
    /// notification of `kind`. Returns whether one was shown.
    pub fn withdraw(&mut self, kind: &NotificationKind) -> bool {
        let before = self.items.len();
        self.items.retain(|n| !n.kind.same_prefix(kind));
        self.items.len() != before
    }

    /// Hides one notification, which every button handler starts with; nothing remembered. Returns whether it
    /// was shown.
    pub fn hide(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|n| n.id != id);
        self.items.len() != before
    }

    /// A notification's first button ("Game Page", "Instructions", "Center", "Calibrate"): hides it, then the
    /// action the caller runs. `None` when no such notification is shown or it has no such button.
    pub fn confirm(&mut self, id: u64) -> Option<Confirmed> {
        let notification = self.items.iter().find(|n| n.id == id)?;
        let confirmed = match &notification.kind {
            NotificationKind::GameNotConfigured => Confirmed::GamePage,
            NotificationKind::GameInstructions => Confirmed::Instructions { game: notification.game.clone() },
            NotificationKind::YawVrNotCentered => Confirmed::CenterYawVr,
            NotificationKind::BeltCalibration => Confirmed::CalibrateBelt,
            _ => return None,
        };
        self.hide(id);
        Some(confirmed)
    }

    /// The user dismissed a notification while `running` was the running game (`none` included).
    /// Returns the game to remember as dismissed (stored in the game notification table) for a
    /// setup notification; a port notification is remembered for the run.
    /// Both use the game running at the click, not the notification's own game. The device
    /// notifications set their dismissed flag, which stops them for the run.
    pub fn dismiss(&mut self, id: u64, running: &str) -> Option<String> {
        let index = self.items.iter().position(|n| n.id == id)?;
        let notification = self.items.remove(index);
        match notification.kind {
            NotificationKind::PortInUse { .. } => {
                self.port_dismissed.insert(running.to_owned());
                None
            }
            NotificationKind::GameNotConfigured | NotificationKind::GameInstructions => Some(running.to_owned()),
            NotificationKind::YawVrNotCentered => {
                self.dismissed.yaw_vr = true;
                None
            }
            NotificationKind::BeltCalibration => {
                self.dismissed.belt = true;
                None
            }
            NotificationKind::IflagFirmware => {
                self.dismissed.iflag = true;
                None
            }
            NotificationKind::DefaultSoundCard
            | NotificationKind::SurroundRequired { .. }
            | NotificationKind::Alert
            | NotificationKind::MemoryReadRefused
            | NotificationKind::DevicePermissions { .. } => None,
        }
    }
}

/// Shows an alert at once and hides it [`ALERT_TIMEOUT`] later on a
/// thread of its own.
pub fn show_alert(state: &ServiceState, title: &str, message: &str, colour: &str) {
    let id = state.notifications.write().alert(title, message, colour);
    state.announce(ServiceEvent::Notifications);
    let state = state.clone();
    let timer = std::thread::Builder::new().name("Timer".into()).spawn(move || {
        std::thread::sleep(ALERT_TIMEOUT);
        if state.notifications.write().hide(id) {
            state.announce(ServiceEvent::Notifications);
        }
    });
    if let Err(error) = timer {
        tracing::warn!(%error, "cannot start the alert timer");
    }
}

/// The name of the process that has a UDP socket bound to `port`: the pid comes from `/proc/net/udp{,6}` and
/// the processes' socket descriptors, the name from the process list game detection keeps
/// ([`openrigcue_config::detect::running_processes`]): the key whose pid is the
/// holder. A holder missing from that list names nothing.
pub fn udp_port_owner(proc_root: &Path, port: u16) -> Option<String> {
    let pid = udp_port_holder(proc_root, port)?;
    openrigcue_config::detect::running_processes(proc_root)
        .into_iter()
        .find_map(|(name, process)| (process == pid).then_some(name))
}

/// The pid that holds a UDP socket bound to `port`.
fn udp_port_holder(proc_root: &Path, port: u16) -> Option<u32> {
    let inodes: Vec<String> = ["net/udp", "net/udp6"]
        .iter()
        .filter_map(|table| std::fs::read_to_string(proc_root.join(table)).ok())
        .flat_map(|text| socket_inodes(&text, port))
        .collect();
    if inodes.is_empty() {
        return None;
    }
    let targets: Vec<String> = inodes.iter().map(|inode| format!("socket:[{inode}]")).collect();
    let mut pids: Vec<u32> =
        std::fs::read_dir(proc_root).ok()?.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok()).collect();
    pids.sort_unstable();
    pids.into_iter().find(|pid| {
        std::fs::read_dir(proc_root.join(pid.to_string()).join("fd")).is_ok_and(|fds| {
            fds.filter_map(Result::ok)
                .filter_map(|fd| std::fs::read_link(fd.path()).ok())
                .any(|link| targets.iter().any(|t| link.as_os_str() == t.as_str()))
        })
    })
}

/// Inodes of the sockets in a `/proc/net/udp` table whose local port is `port`.
fn socket_inodes(table: &str, port: u16) -> Vec<String> {
    let wanted = format!("{port:04X}");
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let local_port = fields.get(1)?.rsplit(':').next()?;
            (local_port.eq_ignore_ascii_case(&wanted)).then(|| fields.get(9).map(|inode| (*inode).to_owned()))?
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_config::patch::GameStatus;

    #[test]
    fn device_permissions_notification_is_once_per_run_and_dismissible() {
        let mut notices = Notifications::default();
        assert!(!notices.device_permissions(&[]));
        assert!(notices.device_permissions(&["/dev/ttyUSB0".into()]));
        assert!(!notices.device_permissions(&["/dev/ttyUSB1".into()]));
        let notice = notices.items[0].clone();
        assert!(notice.message.contains("/dev/ttyUSB0"));
        notices.tidy(None, None, false, &BTreeSet::new());
        assert_eq!(notices.items.len(), 1);
        notices.dismiss(notice.id, "none");
        assert!(notices.items.is_empty());
        assert!(!notices.device_permissions(&["/dev/ttyUSB0".into()]));
    }

    fn errors(texts: &[&str]) -> BTreeSet<String> {
        texts.iter().map(|t| (*t).to_owned()).collect()
    }

    /// The refused memory read of Elite Dangerous raises one notification per game session, which names the
    /// setting, outlives the game and can be dismissed; other games and other error texts raise none.
    #[test]
    fn memory_read_refused_notification_is_once_per_game_session() {
        let refused = errors(&[MEMORY_READ_REFUSED_TEXT]);
        let mut n = Notifications::default();
        n.check_memory_read_refused(Some("ed"), &errors(&["some other error"]));
        n.check_memory_read_refused(Some("ed"), &errors(&[PORT_IN_USE_TEXT]));
        n.check_memory_read_refused(Some("acc"), &refused);
        n.check_memory_read_refused(None, &refused);
        assert!(n.items.is_empty());

        for _ in 0..5 {
            n.check_memory_read_refused(Some("ed"), &refused);
            n.tidy(Some("ed"), Some("ed"), false, &refused);
        }
        assert_eq!(n.items.len(), 1);
        let notice = n.items[0].clone();
        assert_eq!(notice.kind, NotificationKind::MemoryReadRefused);
        assert_eq!((notice.game.as_str(), notice.colour.as_str()), ("ed", "red"));
        assert_eq!(notice.title, "Game Telemetry Cannot Be Read");
        assert!(notice.message.contains("Elite Dangerous"));
        assert!(notice.message.contains("restricts reading another program's memory"));
        assert!(notice.message.contains("kernel.yama.ptrace_scope"));
        assert_eq!(serde_json::to_value(&notice).unwrap()["kind"], "memory_read_refused");
        assert_eq!(n.confirm(notice.id), None, "no action button");

        // Dismissed during the session: not raised again while the game keeps running.
        assert_eq!(n.dismiss(notice.id, "ed"), None);
        n.check_memory_read_refused(Some("ed"), &refused);
        n.check_memory_read_refused(Some("ed"), &errors(&[]));
        n.check_memory_read_refused(Some("ed"), &refused);
        assert!(n.items.is_empty());

        // The game ends (the errors are emptied while no game runs) and starts again: one new notification,
        // which stays after the game ended and replaces nothing but its own kind.
        n.check_memory_read_refused(None, &errors(&[]));
        n.check_memory_read_refused(Some("ed"), &refused);
        n.check_memory_read_refused(Some("ed"), &refused);
        assert_eq!(n.items.len(), 1);
        assert_ne!(n.items[0].id, notice.id);
        n.check_memory_read_refused(None, &errors(&[]));
        n.tidy(None, Some("ed"), false, &errors(&[]));
        assert_eq!(n.items.len(), 1);
        n.check_memory_read_refused(Some("ed"), &refused);
        assert_eq!(n.items.len(), 1, "the earlier one of the kind is replaced");
    }

    /// The game configuration check: not patched -> red "Game not configured", instructions -> "Game instructions",
    /// nothing once dismissed or for other statuses; a second call hides the shown one and adds a new one.
    #[test]
    fn check_game_config_rules() {
        let mut n = Notifications::default();
        n.check_game_config("dirt4", &GameStatus::NotPatched, false);
        let first = n.items[0].id;
        n.check_game_config("dirt4", &GameStatus::NotPatched, false);
        n.check_game_config("beamng", &GameStatus::Instructions, false);
        n.check_game_config("acc", &GameStatus::ReadyToPlay, false);
        n.check_game_config("iracing", &GameStatus::NotPatched, true);
        let titles: Vec<&str> = n.items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, ["This game is not set up", "BeamNG telemetry"]);
        assert!(n.items[0].id > first, "telemetry_notification appends a new notification");
        n.check_game_config("rbr", &GameStatus::NotPatched, false);
        let games: Vec<&str> = n.items.iter().map(|i| i.game.as_str()).collect();
        assert_eq!(games, ["beamng", "rbr"], "every 'config' notification is hidden first");
    }

    /// Main loop: instructions hidden while a listener runs or no game runs.
    #[test]
    fn instructions_follow_the_running_game() {
        let mut n = Notifications::default();
        n.check_game_config("beamng", &GameStatus::Instructions, false);
        n.tidy(Some("beamng"), None, false, &errors(&[]));
        assert_eq!(n.items.len(), 1);
        n.tidy(Some("beamng"), None, true, &errors(&[]));
        assert!(n.items.is_empty());
        n.check_game_config("beamng", &GameStatus::Instructions, false);
        n.tidy(None, Some("beamng"), false, &errors(&[]));
        assert!(n.items.is_empty());
    }

    #[test]
    fn telemetry_instructions_identify_the_game_and_keep_it_when_confirmed() {
        let mut notifications = Notifications::default();
        assert!(notifications.items.is_empty(), "startup alone does not create a game notification");
        notifications.check_game_config("forzahorizon5", &GameStatus::Instructions, false);
        let notification = &notifications.items[0];
        assert_eq!(notification.title, "Forza Horizon 5 telemetry");
        assert_eq!(notification.message, "Open the Forza Horizon 5 setup instructions to enable telemetry.");
        assert_eq!(
            notifications.confirm(notification.id),
            Some(Confirmed::Instructions { game: "forzahorizon5".into() })
        );
        assert!(notifications.items.is_empty());
    }

    /// Main loop: "not configured" stays when the game closes and when the same game runs again; it is hidden
    /// once another game runs and the previous game is set and differs.
    #[test]
    fn not_configured_follows_previous_actual_game() {
        let mut n = Notifications::default();
        n.check_game_config("dirt4", &GameStatus::NotPatched, false);
        n.tidy(Some("dirt4"), None, false, &errors(&[]));
        n.tidy(None, Some("dirt4"), false, &errors(&[]));
        n.tidy(Some("dirt4"), Some("dirt4"), false, &errors(&[]));
        n.tidy(Some("acc"), None, false, &errors(&[]));
        assert_eq!(n.items.len(), 1);
        n.tidy(Some("acc"), Some("dirt4"), false, &errors(&[]));
        assert!(n.items.is_empty());
    }

    /// Dismissing a setup notification records the running game; a port
    /// notification stays dismissed for the running game during the run.
    #[test]
    fn dismissals() {
        let mut n = Notifications::default();
        n.check_game_config("dirt4", &GameStatus::NotPatched, false);
        let id = n.items[0].id;
        assert_eq!(n.dismiss(id, "none").as_deref(), Some("none"));
        let errors = errors(&[PORT_IN_USE_TEXT]);
        n.check_port_in_use("forzahorizon5", &errors, Some(10001), |_| Some("other".into()));
        assert_eq!(n.items[0].message, "No game telemetry reaches OpenRigCue because other occupies port 10001. Close other so that OpenRigCue can receive the telemetry");
        let id = n.items[0].id;
        assert_eq!(n.dismiss(id, "forzahorizon5"), None);
        n.check_port_in_use("forzahorizon5", &errors, Some(10001), |_| Some("other".into()));
        assert!(n.items.is_empty());
        n.check_port_in_use("forzahorizon4", &errors, Some(10001), |_| Some("other".into()));
        assert_eq!(n.items.len(), 1);
    }

    /// "Game Page" / "Instructions": the notification is hidden only; nothing is recorded and the next ticks do not show it
    /// again.
    #[test]
    fn hide_removes_for_good() {
        let mut n = Notifications::default();
        n.check_game_config("beamng", &GameStatus::Instructions, false);
        let id = n.items[0].id;
        assert!(n.hide(id));
        assert!(!n.hide(id));
        n.tidy(Some("beamng"), None, false, &errors(&[]));
        assert!(n.items.is_empty());
    }

    /// While shown, the port notification is hidden once the port-in-use text leaves the game errors, once no game
    /// runs, or once it is dismissed for the running game; no second one is looked up while one is shown.
    #[test]
    fn port_notification_follows_the_error() {
        let mut n = Notifications::default();
        let bind = errors(&[PORT_IN_USE_TEXT]);
        n.check_port_in_use("forzahorizon5", &bind, Some(10001), |_| Some("SimHub.exe".into()));
        n.check_port_in_use("forzahorizon5", &bind, Some(10001), |_| panic!("not looked up while shown"));
        n.tidy(Some("forzahorizon5"), None, false, &bind);
        assert_eq!(n.items.len(), 1);
        n.tidy(Some("forzahorizon5"), None, false, &errors(&[]));
        assert!(n.items.is_empty());
        n.check_port_in_use("forzahorizon5", &bind, Some(10001), |_| Some("SimHub.exe".into()));
        n.tidy(None, Some("forzahorizon5"), false, &bind);
        assert!(n.items.is_empty());
    }

    /// Sound-device notifications: every raise stacks a new notification (same text
    /// too, with a new id); a withdraw hides all of that kind and none of another.
    #[test]
    fn device_notifications_stack() {
        let mut n = Notifications::default();
        let surround = |profile: &str, label: &str, suggest_stereo| NotificationKind::SurroundRequired {
            profile: profile.into(),
            label: label.into(),
            suggest_stereo,
        };
        assert!(n.raise_device(
            surround("SRS UShake6", "6-channel (5.1 Surround)", true),
            "Shakers - 6-channel (5.1 Surround) Required",
            "a".into()
        ));
        assert!(n.raise_device(
            surround("SRS UShake6", "6-channel (5.1 Surround)", true),
            "Shakers - 6-channel (5.1 Surround) Required",
            "a".into()
        ));
        assert!(n.raise_device(
            surround("4 Speakers - FL,FR,RL,RR", "4-channel (Quadraphonic)", false),
            "Shakers - 4-channel (Quadraphonic) Required",
            "b".into()
        ));
        assert!(n.raise_device(NotificationKind::DefaultSoundCard, "Shakers - Default Soundcard", "c".into()));
        let ids: Vec<u64> = n.items.iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), 4);
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "a new key per notification: {ids:?}");
        // The values travel with the notification, for the UI's translated texts.
        assert_eq!(
            serde_json::to_value(&n.items[2]).unwrap()["profile"],
            serde_json::json!("4 Speakers - FL,FR,RL,RR"),
            "{:?}",
            n.items[2]
        );
        assert_eq!(serde_json::to_value(&n.items[2]).unwrap()["label"], serde_json::json!("4-channel (Quadraphonic)"));
        assert_eq!(serde_json::to_value(&n.items[0]).unwrap()["suggest_stereo"], serde_json::json!(true));
        assert!(n.withdraw(&NotificationKind::surround()));
        assert_eq!(n.items.iter().map(|i| i.message.as_str()).collect::<Vec<_>>(), ["c"]);
        assert!(!n.withdraw(&NotificationKind::surround()));
    }

    /// The port notification: only the port-in-use text triggers the owner lookup; no owner, no
    /// notification.
    #[test]
    fn port_check_needs_the_bind_error() {
        let mut n = Notifications::default();
        let other = BTreeSet::from(["timed out".to_owned()]);
        n.check_port_in_use("forzahorizon5", &other, Some(10001), |_| panic!("not looked up"));
        let errors = BTreeSet::from([PORT_IN_USE_TEXT.to_owned()]);
        n.check_port_in_use("forzahorizon5", &errors, Some(10001), |_| None);
        n.check_port_in_use("forzahorizon5", &errors, None, |_| Some("x".into()));
        assert!(n.items.is_empty());
    }

    /// The port owner lookup: the socket inode of the port's table row leads to the pid, the pid
    /// to its name in the game-detection process list (the executable base name, not the 15-character `comm`).
    #[test]
    fn udp_port_owner_from_proc() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("net")).unwrap();
        std::fs::write(
            root.join("net/udp"),
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             12: 0100007F:2711 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 424242 2 0\n\
             13: 0100007F:2712 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 515151 2 0\n",
        )
        .unwrap();
        for (pid, inode, cmdline) in
            [(77, 424242, &b"C:\\Program Files\\SimHub\\SimHubWPF.exe\0"[..]), (78, 515151, b"")]
        {
            let fd_dir = root.join(format!("{pid}/fd"));
            std::fs::create_dir_all(&fd_dir).unwrap();
            std::os::unix::fs::symlink(format!("socket:[{inode}]"), fd_dir.join("5")).unwrap();
            std::fs::write(root.join(format!("{pid}/cmdline")), cmdline).unwrap();
            std::fs::write(root.join(format!("{pid}/comm")), if pid == 77 { "SimHubWPF.exe\n" } else { "other\n" })
                .unwrap();
        }
        assert_eq!(udp_port_owner(root, 10001).as_deref(), Some("SimHubWPF.exe"));
        assert_eq!(udp_port_owner(root, 10002).as_deref(), Some("other"));
        // Same name twice: the list keeps the later pid, so the earlier holder has no name.
        std::fs::write(root.join("78/comm"), "SimHubWPF.exe\n").unwrap();
        assert_eq!(udp_port_owner(root, 10001), None);
        assert_eq!(udp_port_owner(root, 10002).as_deref(), Some("SimHubWPF.exe"));
        assert_eq!(udp_port_owner(root, 10003), None);
    }

    fn connected() -> HardwareWidget {
        HardwareWidget {
            motion_auto_search: true,
            motion_connected: true,
            axis_assignment_incomplete: false,
            yaw_vr: true,
            yaw_vr_centered: false,
            iflag_auto_search: true,
            iflag_connected: true,
            iflag_old_firmware: false,
            belt_auto_search: true,
            belt_connected: true,
            belt_needs_calibration: false,
        }
    }

    fn kinds(n: &Notifications) -> Vec<NotificationKind> {
        n.items.iter().map(|i| i.kind.clone()).collect()
    }

    /// The device notifications, Yaw VR: raised while not centred, hidden once centred; a `yawvr` notification that was ever
    /// added (hidden ones too) stops any further one, as does a Dismiss.
    #[test]
    fn yaw_vr_notification_once_per_run() {
        let mut n = Notifications::default();
        n.set_hardware_widget(&connected());
        assert_eq!(kinds(&n), [NotificationKind::YawVrNotCentered]);
        assert_eq!(n.items[0].title, "YawVR");
        assert_eq!(n.items[0].colour, POPUP_COLOUR);
        let id = n.items[0].id;
        n.set_hardware_widget(&connected());
        assert_eq!(n.items[0].id, id, "not raised again while shown");
        n.set_hardware_widget(&HardwareWidget { yaw_vr_centered: true, ..connected() });
        assert!(n.items.is_empty());
        n.set_hardware_widget(&connected());
        assert!(n.items.is_empty(), "the hidden child still exists");
        let mut fresh = Notifications::default();
        fresh.set_hardware_widget(&HardwareWidget { axis_assignment_incomplete: true, ..connected() });
        fresh.set_hardware_widget(&HardwareWidget { motion_connected: false, ..connected() });
        assert!(fresh.items.is_empty());
        fresh.set_hardware_widget(&connected());
        let id = fresh.items[0].id;
        assert_eq!(fresh.dismiss(id, "none"), None);
        assert!(fresh.dismissed.yaw_vr);
    }

    /// The device notifications, belt: raised while connected, not calibrated and switched on; hidden (and
    /// its showing mark cleared) once that ends, so the next need raises a new one; nothing changes while
    /// the belt is not connected; Dismiss stops it for the run.
    #[test]
    fn belt_notification_follows_calibration() {
        let mut n = Notifications::default();
        let needs = HardwareWidget { yaw_vr: false, belt_needs_calibration: true, ..connected() };
        n.set_hardware_widget(&needs);
        assert_eq!(kinds(&n), [NotificationKind::BeltCalibration]);
        let first = n.items[0].id;
        n.set_hardware_widget(&HardwareWidget { belt_connected: false, ..needs });
        n.set_hardware_widget(&needs);
        assert_eq!(n.items[0].id, first);
        n.set_hardware_widget(&HardwareWidget { belt_needs_calibration: false, ..needs });
        assert!(n.items.is_empty());
        n.set_hardware_widget(&needs);
        assert!(n.items[0].id > first, "a new one after the calibration was needed again");
        let id = n.items[0].id;
        assert_eq!(n.confirm(id), Some(Confirmed::CalibrateBelt));
        assert!(n.items.is_empty());
        n.set_hardware_widget(&needs);
        assert!(n.items.is_empty(), "showing_belt_notification stays set after Calibrate");
        n.set_hardware_widget(&HardwareWidget { belt_needs_calibration: false, ..needs });
        n.set_hardware_widget(&needs);
        let id = n.items[0].id;
        n.dismiss(id, "acc");
        n.set_hardware_widget(&HardwareWidget { belt_needs_calibration: false, ..needs });
        n.set_hardware_widget(&needs);
        assert!(n.items.is_empty(), "belt_dismiss_center_notification");
    }

    /// The device notifications, iFlag: firmware 1.3 or older raises one notification per run (it is never hidden
    /// here and its showing mark never clears).
    #[test]
    fn iflag_notification_once() {
        let mut n = Notifications::default();
        let old = HardwareWidget { yaw_vr: false, iflag_old_firmware: true, ..connected() };
        n.set_hardware_widget(&HardwareWidget { iflag_auto_search: false, ..old });
        assert!(n.items.is_empty());
        n.set_hardware_widget(&old);
        assert_eq!(kinds(&n), [NotificationKind::IflagFirmware]);
        n.set_hardware_widget(&HardwareWidget { iflag_old_firmware: false, ..old });
        assert_eq!(n.items.len(), 1, "not hidden by a newer firmware");
        let id = n.items[0].id;
        assert!(n.hide(id));
        n.set_hardware_widget(&old);
        assert!(n.items.is_empty());
        assert_eq!(n.confirm(id), None);
    }

    /// Alerts: a new notification per call, other notifications untouched; show_alert hides it after
    /// three seconds.
    #[test]
    fn alerts_stack_and_expire() {
        let mut n = Notifications::default();
        n.check_game_config("dirt4", &GameStatus::NotPatched, false);
        let a = n.alert("Success", "Calibration has finished", "green");
        let b = n.alert("Success", "Calibration has finished", "green");
        assert_ne!(a, b);
        assert_eq!(n.items.len(), 3);
        n.tidy(Some("dirt4"), None, false, &errors(&[]));
        assert_eq!(n.items.len(), 3);
        assert_eq!(n.confirm(a), None);
        let state = crate::state::ServiceState::new(
            crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()),
            crate::state::OriginalPlatform::Windows,
        );
        show_alert(&state, "Failed", "Calibration did not succeed", "red");
        assert_eq!(state.notifications.read().items[0].colour, "red");
        std::thread::sleep(ALERT_TIMEOUT + Duration::from_millis(300));
        assert!(state.notifications.read().items.is_empty());
    }

    /// "Game Page" and "Instructions" hide their notification and name the action.
    #[test]
    fn confirm_setup_notifications() {
        let mut n = Notifications::default();
        n.check_game_config("dirt4", &GameStatus::NotPatched, false);
        n.check_game_config("beamng", &GameStatus::Instructions, false);
        let (config, instructions) = (n.items[0].id, n.items[1].id);
        assert_eq!(n.confirm(config), Some(Confirmed::GamePage));
        assert_eq!(n.confirm(instructions), Some(Confirmed::Instructions { game: "beamng".into() }));
        assert!(n.items.is_empty());
        assert_eq!(n.confirm(config), None);
    }
}
