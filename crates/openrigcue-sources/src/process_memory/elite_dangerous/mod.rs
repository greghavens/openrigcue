//! Elite Dangerous Horizons / Odyssey: forces read from the game's memory through community pointer chains
//! (listener key `ed`).

pub mod memory;
pub mod offsets;
pub mod updater;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use self::memory::{ProcFs, ProcessAccess};
use self::offsets::{EdBuild, Force, Offsets};
use self::updater::{check_for_update_with, CheckOutcome, OffsetFiles, CHECK_PERIOD, REQUEST_TIMEOUT};
use super::http::HttpClient;
use super::listener_support::apply_raising_tick;
use crate::udp_flight::{clear_listener_channels, AngularRates, FlightSource};
use crate::udp_racing::parameters::Parameters;
use crate::udp_racing::unhandled::DispatcherEnds;
use crate::{NativeRead, Source, SourceError};

/// Executable name of the game process.
pub const PROCESS_NAME: &str = "EliteDangerous64.exe";

/// The current PID published by the process scanner.
pub type NativePidReader = Arc<dyn Fn() -> Option<u32> + Send + Sync>;

/// Settings read (once) from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EliteSettings {
    /// `CUSTOM_FEATURES/check_ed_updates` (default `'yes'`): run the 8-hourly offsets check.
    pub check_updates: bool,
    /// `CUSTOM_FEATURES/ed_invert_yaw|roll|pitch` (default `'no'`).
    pub invert_yaw: bool,
    pub invert_roll: bool,
    pub invert_pitch: bool,
    /// The configured motion platform's manufacturer is Yaw VR.
    pub yaw_vr: bool,
    /// The documents folder that holds the offsets archive.
    pub documents_dir: PathBuf,
}

impl EliteSettings {
    pub fn new(documents_dir: impl Into<PathBuf>) -> Self {
        EliteSettings {
            check_updates: true,
            invert_yaw: false,
            invert_roll: false,
            invert_pitch: false,
            yaw_vr: false,
            documents_dir: documents_dir.into(),
        }
    }

    /// The sign factors (yaw, roll, pitch): -1 unless inverted, except that a Yaw VR platform takes the
    /// opposite yaw sign.
    fn factors(&self) -> (f64, f64, f64) {
        let sign = |positive: bool| if positive { 1.0 } else { -1.0 };
        (sign(self.invert_yaw != self.yaw_vr), sign(self.invert_roll), sign(self.invert_pitch))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The forces of the last receive step, by name.
#[derive(Debug, Clone, Default, PartialEq)]
struct Forces {
    values: Vec<(Force, LegacyValue)>,
}

impl Forces {
    fn get(&self, force: Force) -> Option<f64> {
        self.native(force).and_then(LegacyValue::number)
    }

    fn native(&self, force: Force) -> Option<&LegacyValue> {
        self.values.iter().find(|(f, _)| *f == force).map(|(_, v)| v)
    }

    fn set(&mut self, force: Force, value: f64) {
        self.set_native(force, LegacyValue::Float(value));
    }

    fn set_native(&mut self, force: Force, value: LegacyValue) {
        match self.values.iter_mut().find(|(f, _)| *f == force) {
            Some(entry) => entry.1 = value,
            None => self.values.push((force, value)),
        }
    }

    /// The sum of the forces named by `keys`, in key order; `None` when one is missing.
    fn sum(&self, keys: &[Force]) -> Option<f64> {
        keys.iter().try_fold(0.0, |total, key| Some(total + self.get(*key)?))
    }
}

/// Ship and SRV forces in summation order, and the matching SRV force of each ship force.
const SHIP: [Force; 7] = [Force::Speed, Force::Sway, Force::Heave, Force::Surge, Force::Pitch, Force::Roll, Force::Yaw];
const SRV: [Force; 7] =
    [Force::SrvSpeed, Force::SrvSway, Force::SrvHeave, Force::SrvSurge, Force::SrvPitch, Force::SrvRoll, Force::SrvYaw];

/// The Elite Dangerous listener and its offsets
/// update thread.
pub struct EliteDangerous<P: ProcessAccess = ProcFs> {
    settings: EliteSettings,
    process: P,
    pid_reader: Option<NativePidReader>,
    offsets: Arc<Mutex<Offsets>>,
    pid: Option<u32>,
    previous_pid: Option<u32>,
    build: Option<EdBuild>,
    base: Option<u64>,
    /// The resolved force addresses; an empty chain gives none, read as a null pointer.
    addresses: Vec<(Force, Option<u64>)>,
    forces: Mutex<Forces>,
    stop: Arc<AtomicBool>,
    /// The game errors, which get [`memory::READ_REFUSED`] when the system refuses a memory read.
    parameters: Arc<Parameters>,
}

impl EliteDangerous<ProcFs> {
    /// Starts the offsets update thread when `check_updates` is set.
    pub fn new(settings: EliteSettings) -> Self {
        let source = Self::with_process(settings, ProcFs::default());
        if source.settings.check_updates {
            source.spawn_updater();
        }
        source
    }

    fn spawn_updater(&self) {
        let files = OffsetFiles::in_dir(&self.settings.documents_dir);
        let (offsets, stop) = (Arc::clone(&self.offsets), Arc::clone(&self.stop));
        let spawned = thread::Builder::new().name("run_periodic_check".into()).spawn(move || {
            let mut http = HttpClient::new(REQUEST_TIMEOUT);
            while !stop.load(Ordering::Relaxed) {
                match check_for_update_with(&mut http, &files, |change| lock(&offsets).apply(change)) {
                    Ok(CheckOutcome::Loaded(_)) => {}
                    Ok(CheckOutcome::NothingFound) => {}
                    Err(error) => {
                        tracing::error!(%error, "Elite Dangerous offsets update stopped (as the original's thread)");
                        return;
                    }
                }
                sleep_unless_stopped(CHECK_PERIOD, &stop);
            }
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "Elite Dangerous offsets update not started");
        }
    }
}

/// Sleeps `period` in short steps so a dropped source ends its thread.
fn sleep_unless_stopped(period: Duration, stop: &AtomicBool) {
    let step = Duration::from_secs(1);
    let mut slept = Duration::ZERO;
    while slept < period && !stop.load(Ordering::Relaxed) {
        thread::sleep(step);
        slept += step;
    }
}

impl<P: ProcessAccess> EliteDangerous<P> {
    pub fn with_process(settings: EliteSettings, process: P) -> Self {
        EliteDangerous {
            settings,
            process,
            pid_reader: None,
            offsets: Arc::default(),
            pid: None,
            previous_pid: None,
            build: None,
            base: None,
            addresses: Vec::new(),
            forces: Mutex::new(Forces::default()),
            stop: Arc::new(AtomicBool::new(false)),
            parameters: Parameters::for_listener(),
        }
    }

    /// A failed open or module lookup: one the system refused for lack of permission is added to the game
    /// errors, any other is not reported.
    fn note_refusal(&self, error: &std::io::Error) {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            self.parameters.add_game_error(memory::READ_REFUSED.to_owned());
        }
    }

    /// Adds a refused read since the last call to the game errors.
    fn note_refused_reads(&mut self) {
        if self.process.take_refused() {
            self.parameters.add_game_error(memory::READ_REFUSED.to_owned());
        }
    }

    /// Replaces the loaded offsets (what the update thread does after each check).
    pub fn set_offsets(&self, offsets: Offsets) {
        *lock(&self.offsets) = offsets;
    }

    pub fn set_pid_reader(&mut self, reader: NativePidReader) {
        self.pid_reader = Some(reader);
    }

    /// The game's pid; when it changes, the build is taken from the image path
    /// (an unrecognised path keeps the previous build).
    fn client_pid(&mut self) -> Option<u32> {
        let (pid, image) = match &self.pid_reader {
            Some(reader) => (reader(), None),
            None => {
                let found = self.process.find_process(PROCESS_NAME);
                (found.as_ref().map(|(pid, _)| *pid), found.map(|(_, image)| image))
            }
        };
        if pid != self.previous_pid {
            let image = image.or_else(|| pid.and_then(|pid| self.process.image_path(pid))).unwrap_or_default();
            if image.contains("elite-dangerous-odyssey-64") || image.contains("FORC-FDEV-DO-") {
                self.build = Some(EdBuild::Odyssey);
            } else if image.contains("elite-dangerous-64") || image.contains("FORC-FDEV-D-") {
                self.build = Some(EdBuild::Horizons);
            }
            self.previous_pid = pid;
        }
        pid
    }

    /// Resolves a pointer chain: add each offset to the base (first) or to the pointer read at the previous
    /// address; the last offset is added but not dereferenced. An empty chain gives
    /// `None`. Addresses wrap modulo
    /// 2^64, so only those low bits are needed for subsequent reads.
    #[cfg(test)]
    fn resolve(&mut self, chain: &[u64], base: u64) -> Option<u64> {
        let (last, links) = chain.split_last()?;
        let mut address = base;
        for (index, offset) in links.iter().enumerate() {
            let pointer_at = if index == 0 { base } else { address }.wrapping_add(*offset);
            let mut pointer = [0u8; 8];
            self.process.read(pointer_at, &mut pointer);
            address = u64::from_le_bytes(pointer);
        }
        Some(if links.is_empty() { base } else { address }.wrapping_add(*last))
    }

    /// Chain resolution keeps its initial chain iterator but rereads the current chain length
    /// before deciding whether an offset is the final, non-dereferenced address.
    fn resolve_current(&mut self, build: EdBuild, force: Force, base: u64) -> Result<Option<u64>, ()> {
        let chain = lock(&self.offsets).chain(build, force).ok_or(())?.to_vec();
        let mut pointer = base;
        for (index, offset) in chain.into_iter().enumerate() {
            let address = pointer.wrapping_add(offset);
            let length = lock(&self.offsets).chain(build, force).ok_or(())?.len();
            if Some(index) == length.checked_sub(1) {
                return Ok(Some(address));
            }
            let mut bytes = [0; 8];
            self.process.read(address, &mut bytes);
            pointer = u64::from_le_bytes(bytes);
        }
        Ok(None)
    }

    /// A force value: a float32 at the resolved address, zero when
    /// unreadable. A missing address reads as a null pointer.
    fn value_at(&mut self, address: Option<u64>) -> f64 {
        let address = address.unwrap_or(0);
        let mut bytes = [0u8; 4];
        self.process.read(address, &mut bytes);
        f64::from(f32::from_le_bytes(bytes))
    }

    /// Forgets the process (the failure branches of the liveness test).
    fn detach(&mut self) {
        self.pid = None;
        self.base = None;
        self.process.close();
    }

    /// The liveness test and the running check.
    fn check(&mut self) -> Result<bool, DispatcherEnds> {
        if lock(&self.offsets).is_empty() {
            return Ok(false);
        }
        if self.pid.is_none() {
            let Some(pid) = self.client_pid() else { return Ok(false) };
            if let Err(error) = self.process.open(pid) {
                self.note_refusal(&error);
                self.detach();
                return Ok(false);
            }
            self.pid = Some(pid);
            match self.process.module_base(PROCESS_NAME) {
                Ok(base) => self.base = base,
                Err(error) => {
                    self.note_refusal(&error);
                    self.detach();
                    return Ok(false);
                }
            }
        }
        // Builds without any chain are published too. They skip address
        // assignment but still reach the running check with the retained addresses.
        // The updater can replace offsets during the process/module calls above.
        // Capture only the selected build at the point where its entries are iterated.
        let keys = self.build.and_then(|build| {
            lock(&self.offsets).chains(build).map(|chains| chains.iter().map(|(force, _)| *force).collect::<Vec<_>>())
        });
        let (Some(base), Some(build)) = (self.base.filter(|&b| b != 0), self.build) else {
            self.detach();
            self.build = None;
            return Ok(false);
        };
        let Some(keys) = keys else {
            self.detach();
            return Ok(false);
        };
        for force in keys {
            let Ok(address) = self.resolve_current(build, force, base) else {
                self.detach();
                return Ok(false);
            };
            match self.addresses.iter_mut().find(|(f, _)| *f == force) {
                Some(entry) => entry.1 = address,
                None => self.addresses.push((force, address)),
            }
        }
        // Running check: any non-zero force means the game is running; otherwise look for the process again.
        for (_, address) in self.addresses.clone() {
            if self.value_at(address) != 0.0 {
                return Ok(true);
            }
        }
        self.pid = None;
        Ok(false)
    }

    /// Every channel: ship forces ×10 while the ship sums non-zero,
    /// else SRV forces ×3 (an SRV roll beyond ±180 counts as 0), angles with their factors; all else 0. A sum with
    /// a force missing counts as 0. Without an SRV roll the guard fails for every
    /// channel, which the dispatcher's handlers answer ([`apply_raising_tick`]).
    fn write_channels(&mut self, t: &mut Telemetry) {
        let values = self.forces.get_mut().unwrap_or_else(|error| error.into_inner());
        let ship = values.sum(&SHIP).unwrap_or(0.0);
        let Some(srv_roll) = values.get(Force::SrvRoll) else {
            apply_raising_tick(t);
            return;
        };
        if srv_roll.abs() > 180.0 {
            values.set_native(Force::SrvRoll, LegacyValue::Int(0));
        }
        let srv = values.sum(&SRV).unwrap_or(0.0);
        clear_listener_channels(t);
        let (yaw_factor, roll_factor, pitch_factor) = self.settings.factors();
        let factors = [1.0, 1.0, 1.0, 1.0, pitch_factor, roll_factor, yaw_factor];
        let (forces, scale, vehicle) = if ship != 0.0 {
            (&SHIP, 10.0, "spaceship")
        } else if srv != 0.0 {
            (&SRV, 3.0, "srv")
        } else {
            return;
        };
        let value = |i: usize| values.get(forces[i]).map_or(0.0, |v| v * factors[i] * scale);
        t.vehicle = vehicle.to_owned();
        t.speed = value(0);
        t.lateral_acceleration = value(1);
        t.vertical_acceleration = value(2);
        t.longitudinal_acceleration = value(3);
        t.pitch = value(4);
        t.roll = value(5);
        t.yaw = value(6);
    }
    fn native_answer(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};
        let mut values = lock(&self.forces);
        let ship = values.sum(&SHIP).unwrap_or(0.0);
        // This guard is outside the local sum fallbacks, and its assignment survives every name branch.
        let srv_roll = values.get(Force::SrvRoll)?;
        if srv_roll.abs() > 180.0 {
            values.set_native(Force::SrvRoll, Int(0));
        }
        let srv = values.sum(&SRV).unwrap_or(0.0);
        let (forces, scale, vehicle) = if ship != 0.0 {
            (&SHIP, 10.0, "spaceship")
        } else if srv != 0.0 {
            (&SRV, 3.0, "srv")
        } else {
            return Some(Int(0));
        };
        if name == "car_name" {
            return Some(Str(vehicle.into()));
        }
        let i = match name {
            "speed" => 0,
            "lateral_acceleration" => 1,
            "vertical_acceleration" => 2,
            "longitudinal_acceleration" => 3,
            "pitch" => 4,
            "roll" => 5,
            "yaw" => 6,
            _ => return Some(Int(0)),
        };
        let force = forces[i];
        let value = values.native(force)?;
        if force == Force::SrvRoll && matches!(value, Int(0)) {
            return Some(Int(0));
        }
        let (yaw, roll, pitch) = self.settings.factors();
        let factor = [1.0, 1.0, 1.0, 1.0, pitch, roll, yaw][i];
        Some(Float(value.number()? * factor * scale))
    }
}

impl<P: ProcessAccess> Source for EliteDangerous<P> {
    fn id(&self) -> &'static str {
        "ed"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        self.native_answer(name)
    }

    fn read_native(&self, name: &str) -> NativeRead {
        self.native_answer(name).map_or(NativeRead::Raised, NativeRead::Value)
    }

    fn channel_read_raised(&self, _name: &str) -> bool {
        // Numeric compatibility probes must not execute the mutating getter.
        lock(&self.forces).get(Force::SrvRoll).is_none()
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = self.check();
        self.note_refused_reads();
        Ok(running?)
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        for (force, address) in self.addresses.clone() {
            let value = self.value_at(address);
            self.forces.get_mut().unwrap_or_else(|error| error.into_inner()).set(force, value);
        }
        self.note_refused_reads();
        Ok(true)
    }

    /// Numeric compatibility projection after the receive step.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        self.receive_native(t)?;
        self.write_channels(t);
        Ok(true)
    }
}

impl<P: ProcessAccess> FlightSource for EliteDangerous<P> {
    /// The rate names read 0.
    fn angular_rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

impl<P: ProcessAccess> Drop for EliteDangerous<P> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;

    use super::*;

    /// A fake game process: sparse memory, one module.
    #[derive(Default)]
    struct FakeProcess {
        process: Option<(u32, String)>,
        base: Option<u64>,
        memory: HashMap<u64, u8>,
        opened: Option<u32>,
        open_fails: bool,
        /// The error of a failing open (permission denied when unset).
        open_error: Option<io::ErrorKind>,
        /// The error of the module lookup, when it fails.
        module_error: Option<io::ErrorKind>,
        /// A read was refused for lack of permission.
        refused_read: bool,
        find_calls: usize,
        image_pids: Vec<u32>,
        read_addresses: Vec<u64>,
        close_calls: usize,
        offsets_on_open: Option<(Arc<Mutex<Offsets>>, Offsets)>,
        offsets_on_read: Option<(Arc<Mutex<Offsets>>, Offsets)>,
    }

    impl FakeProcess {
        fn write(&mut self, address: u64, bytes: &[u8]) {
            for (i, b) in bytes.iter().enumerate() {
                self.memory.insert(address + i as u64, *b);
            }
        }
    }

    impl ProcessAccess for FakeProcess {
        fn find_process(&mut self, exe_name: &str) -> Option<(u32, String)> {
            self.find_calls += 1;
            self.process.clone().filter(|(_, image)| memory::base_name(image) == exe_name)
        }
        fn image_path(&mut self, pid: u32) -> Option<String> {
            self.image_pids.push(pid);
            self.process.as_ref().filter(|(found, _)| *found == pid).map(|(_, image)| image.clone())
        }
        fn open(&mut self, pid: u32) -> io::Result<()> {
            if self.open_fails {
                return Err(self.open_error.unwrap_or(io::ErrorKind::PermissionDenied).into());
            }
            if let Some((shared, updated)) = self.offsets_on_open.take() {
                *lock(&shared) = updated;
            }
            self.opened = Some(pid);
            Ok(())
        }
        fn close(&mut self) {
            self.opened = None;
            self.close_calls += 1;
        }
        fn module_base(&mut self, _module_name: &str) -> io::Result<Option<u64>> {
            match self.module_error {
                Some(kind) => Err(kind.into()),
                None => Ok(self.base),
            }
        }
        fn take_refused(&mut self) -> bool {
            std::mem::take(&mut self.refused_read)
        }
        fn read(&mut self, address: u64, buffer: &mut [u8]) {
            if let Some((shared, updated)) = self.offsets_on_read.take() {
                *lock(&shared) = updated;
            }
            self.read_addresses.push(address);
            for (i, b) in buffer.iter_mut().enumerate() {
                *b = self.memory.get(&(address + i as u64)).copied().unwrap_or(0);
            }
        }
    }

    const ODYSSEY: &str = "C:\\Games\\Frontier\\Products\\elite-dangerous-odyssey-64\\EliteDangerous64.exe";
    const BASE: u64 = 0x1_4000_0000;

    fn xml(chains: &str) -> Offsets {
        offsets::parse_offsets_xml(&format!("<Root><Odyssey>{chains}</Odyssey></Root>"))
    }

    const FORCE_TAGS: [&str; 14] = [
        "Speed",
        "Sway",
        "Heave",
        "Surge",
        "Pitch",
        "Roll",
        "Yaw",
        "SRV_Speed",
        "SRV_Sway",
        "SRV_Heave",
        "SRV_Surge",
        "SRV_Pitch",
        "SRV_Roll",
        "SRV_Yaw",
    ];

    /// A game whose force `i` sits behind the chain base+0x100 -> pointer, +0x10 + 4*i, for the forces `keep`
    /// selects (by index into [`FORCE_TAGS`]).
    fn game_with(keep: impl Fn(usize) -> bool) -> EliteDangerous<FakeProcess> {
        let mut process = FakeProcess { process: Some((4242, ODYSSEY.into())), base: Some(BASE), ..Default::default() };
        process.write(BASE + 0x100, &0x2000u64.to_le_bytes());
        let source = EliteDangerous::with_process(EliteSettings::new("/tmp"), process);
        let chains: String = FORCE_TAGS
            .iter()
            .enumerate()
            .filter(|(i, _)| keep(*i))
            .map(|(i, tag)| format!(r#"<{tag} Offsets="2" Offset_0="100" Offset_1="{:X}"/>"#, 0x10 + 4 * i))
            .collect();
        source.set_offsets(xml(&chains));
        source
    }

    fn game() -> EliteDangerous<FakeProcess> {
        game_with(|_| true)
    }

    #[test]
    fn attaching_process_uses_offsets_published_after_initial_presence_check() {
        let mut source = game();
        source.process.offsets_on_open = Some((source.offsets.clone(), xml(r#"<Speed Offsets="1" Offset_0="A0"/>"#)));
        source.process.write(BASE + 0xa0, &12.5_f32.to_le_bytes());
        assert!(source.check().unwrap());
        assert_eq!(source.addresses, [(Force::Speed, Some(BASE + 0xa0))]);
        assert_eq!(source.process.read_addresses, [BASE + 0xa0]);
        source.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(source.forces.lock().unwrap().get(Force::Speed), Some(12.5));
    }

    #[test]
    fn removed_offsets_after_initial_gate_use_inner_catch_without_resetting_build() {
        let mut source = game();
        source.process.offsets_on_open = Some((source.offsets.clone(), Offsets::default()));
        assert!(!source.check().unwrap());
        assert_eq!(source.build, Some(EdBuild::Odyssey));
        assert_eq!(source.process.close_calls, 1);
        assert!(source.process.read_addresses.is_empty());
    }

    #[test]
    fn removed_current_chain_catches_failure_after_captured_iterator() {
        let mut source = game_with(|i| i <= 1);
        source.process.offsets_on_read = Some((source.offsets.clone(), Offsets::default()));
        set_force(&mut source, 0, 12.0);
        assert!(!source.check().unwrap());
        assert!(lock(&source.offsets).is_empty());
        assert!(source.addresses.is_empty());
        assert_eq!(source.process.read_addresses, [BASE + 0x100]);
        assert_eq!(source.process.close_calls, 1);
        assert_eq!(source.build, Some(EdBuild::Odyssey));
    }

    #[test]
    fn current_chain_length_changes_final_offset_without_replacing_captured_values() {
        let mut source = game_with(|i| i == 0);
        // The captured chain is [0x100, 0x10], but after reading its first pointer
        // the current replacement has length three. The second old offset is therefore
        // dereferenced, and exhausting the captured two-element iterator returns None.
        source.process.offsets_on_read =
            Some((source.offsets.clone(), xml(r#"<Speed Offsets="3" Offset_0="AAA" Offset_1="BBB" Offset_2="CCC"/>"#)));
        source.process.write(0, &5.0_f32.to_le_bytes());
        assert!(source.check().unwrap());
        assert_eq!(source.addresses, [(Force::Speed, None)]);
        assert_eq!(source.process.read_addresses, [BASE + 0x100, 0x2010, 0]);
    }

    #[test]
    fn subsequent_force_uses_current_replacement_chain() {
        let mut source = game_with(|i| i <= 1);
        // Replacing equal-length chains keeps the current speed iterator but the next
        // force resolves using its new chain. The replacement's extra Yaw is not in
        // the list of forces captured when the liveness test started.
        source.process.offsets_on_read = Some((
            source.offsets.clone(),
            xml(
                r#"<Speed Offsets="2" Offset_0="300" Offset_1="50"/><Sway Offsets="1" Offset_0="A0"/><Yaw Offsets="1" Offset_0="B0"/>"#,
            ),
        ));
        source.process.write(BASE + 0xa0, &2.0_f32.to_le_bytes());
        assert!(source.check().unwrap());
        assert_eq!(source.addresses, [(Force::Speed, Some(0x2010)), (Force::Sway, Some(BASE + 0xa0))]);
        assert_eq!(source.process.read_addresses, [BASE + 0x100, 0x2010, BASE + 0xa0]);
    }

    #[test]
    fn native_cached_pid_controls_lookup_before_live_process_metadata() {
        let mut source = game();
        let cached = Arc::new(Mutex::new(None));
        let cache = cached.clone();
        source.set_pid_reader(Arc::new(move || *lock(&cache)));
        assert_eq!(source.client_pid(), None, "live process must not bypass unpublished scanner state");
        assert_eq!(source.process.find_calls, 0);
        assert!(source.process.image_pids.is_empty());
        *lock(&cached) = Some(4242);
        assert_eq!(source.client_pid(), Some(4242));
        assert_eq!(source.build, Some(EdBuild::Odyssey));
        assert_eq!(source.process.image_pids, [4242]);
        source.process.process = Some((4242, "C:/elite-dangerous-64/EliteDangerous64.exe".into()));
        assert_eq!(source.client_pid(), Some(4242));
        assert_eq!(source.build, Some(EdBuild::Odyssey), "same PID does not re-read the path");
        assert_eq!(source.process.image_pids, [4242]);
        *lock(&cached) = Some(5000);
        assert_eq!(source.client_pid(), Some(5000), "cached PID survives failed image-path lookup");
        assert_eq!(source.build, Some(EdBuild::Odyssey), "empty path retains previous build");
        assert_eq!(source.process.image_pids, [4242, 5000]);
        *lock(&cached) = None;
        assert_eq!(source.client_pid(), None);
        assert_eq!(source.previous_pid, None);
        assert_eq!(source.process.find_calls, 0);
    }

    #[test]
    fn native_cached_pid_refreshes_only_after_retained_process_stops() {
        let mut source = game();
        set_force(&mut source, 0, 10.0);
        let cached = Arc::new(Mutex::new(Some(4242)));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cache = cached.clone();
        let read_count = reads.clone();
        source.set_pid_reader(Arc::new(move || {
            read_count.fetch_add(1, Ordering::SeqCst);
            *lock(&cache)
        }));
        assert!(source.is_running().unwrap());
        assert_eq!(source.process.image_pids, [4242]);
        *lock(&cached) = None;
        assert!(source.is_running().unwrap(), "existing nonzero process remains attached");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        set_force(&mut source, 0, 0.0);
        assert!(!source.is_running().unwrap());
        assert!(!source.is_running().unwrap(), "next lookup sees the cleared scanner entry");
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(source.process.find_calls, 0);
    }

    fn set_force(source: &mut EliteDangerous<FakeProcess>, index: usize, value: f32) {
        source.process.write(0x2000 + 0x10 + 4 * index as u64, &value.to_le_bytes());
    }

    #[test]
    fn native_receive_keeps_raw_roll_until_an_actual_getter() {
        let mut source = game();
        set_force(&mut source, 0, 50.0);
        set_force(&mut source, 12, 200.0);
        assert!(source.is_running().unwrap());
        let mut t = Telemetry { speed: 999.0, ..Telemetry::off() };
        source.receive_native(&mut t).unwrap();
        assert_eq!(lock(&source.forces).native(Force::SrvRoll), Some(&LegacyValue::Float(200.0)));
        assert_eq!(t.speed, 999.0);
        assert!(!source.channel_read_raised("unknown"));
        assert_eq!(lock(&source.forces).native(Force::SrvRoll), Some(&LegacyValue::Float(200.0)));
        assert_eq!(source.native_channel("unknown"), Some(LegacyValue::Int(0)));
        assert_eq!(lock(&source.forces).native(Force::SrvRoll), Some(&LegacyValue::Int(0)));
    }

    #[test]
    fn native_receive_reads_null_and_keeps_all_force_writes() {
        use crate::derive::{Listener, SourceListener};
        let mut source = game();
        set_force(&mut source, 0, 30.0);
        assert!(source.is_running().unwrap());
        source.addresses = vec![(Force::Speed, Some(0x2010)), (Force::SrvRoll, None)];
        let mut t = Telemetry { speed: 999.0, ..Telemetry::off() };
        assert!(source.receive_native(&mut t).unwrap());
        assert_eq!(lock(&source.forces).native(Force::Speed), Some(&LegacyValue::Float(30.0)));
        assert_eq!(lock(&source.forces).native(Force::SrvRoll), Some(&LegacyValue::Float(0.0)));
        assert_eq!(t.speed, 999.0);
        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.failure_point(), None);
    }

    #[test]
    fn native_active_float_values_defaults_and_integer_srv_clamp() {
        use LegacyValue::{Float, Int, Str};
        let mut source = game();
        assert!(source.channel_read_raised("unknown"), "the SRV-roll guard precedes name matching");
        set_force(&mut source, 0, 50.0);
        assert!(source.is_running().unwrap());
        assert!(source.channel_read_raised("speed"), "checkRunning probes do not populate forces");
        source.poll(&mut Telemetry::off()).unwrap();
        for (name, expected) in [
            ("speed", Float(500.0)),
            ("lateral_acceleration", Float(0.0)),
            ("car_name", Str("spaceship".into())),
            ("rpm", Int(0)),
            ("unknown", Int(0)),
        ] {
            assert_eq!(source.native_channel(name), Some(expected), "{name}");
        }
        set_force(&mut source, 0, 0.0);
        set_force(&mut source, 7, 20.0);
        set_force(&mut source, 12, 200.0);
        source.poll(&mut Telemetry::off()).unwrap();
        assert_eq!(source.native_channel("car_name"), Some(Str("srv".into())));
        assert_eq!(source.native_channel("speed"), Some(Float(60.0)));
        assert_eq!(source.native_channel("roll"), Some(Int(0)), "guard writes integer zero into forces");
        set_force(&mut source, 12, 0.0);
        source.poll(&mut Telemetry::off()).unwrap();
        assert!(
            matches!(source.native_channel("roll"), Some(Float(v)) if v.to_bits() == (-0.0f64).to_bits()),
            "a new raw float read replaces the integer clamp"
        );
        set_force(&mut source, 12, f32::NAN);
        source.poll(&mut Telemetry::off()).unwrap();
        assert!(matches!(source.native_channel("roll"), Some(Float(v)) if v.is_nan()));
        assert!(!source.channel_read_raised("roll"));
        set_force(&mut source, 7, 0.0);
        set_force(&mut source, 12, 0.0);
        source.poll(&mut Telemetry::off()).unwrap();
        assert_eq!(source.native_channel("speed"), Some(Int(0)), "both zero sums return integer defaults");
        assert_eq!(source.native_channel("car_name"), Some(Int(0)));
    }

    #[test]
    fn missing_srv_roll_is_unavailable_through_the_actual_source_adapter() {
        use crate::derive::{Listener, SourceListener};
        let mut source = game_with(|i| i < 7);
        set_force(&mut source, 0, 30.0);
        assert!(source.is_running().unwrap());
        let mut listener = SourceListener::new(source);
        listener.receive();
        for name in ["speed", "rpm", "raw_flag", "unknown"] {
            assert_eq!(listener.native_channel(name), None, "{name}");
        }
        assert_eq!(listener.channel("speed"), None);
        assert_eq!(listener.vehicle(), None);
        assert_eq!(listener.location(), None);
    }

    /// Chain resolution: the last offset is added, not dereferenced.
    #[test]
    fn pointer_chain_walk() {
        let mut source = game();
        source.process.write(0x2008, &0x3000u64.to_le_bytes());
        assert_eq!(source.resolve(&[0x100, 0x10], BASE), Some(0x2010));
        assert_eq!(source.resolve(&[0x100, 0x8, 0x4], BASE), Some(0x3004));
        assert_eq!(source.resolve(&[0x20], BASE), Some(BASE + 0x20));
        assert_eq!(source.resolve(&[], BASE), None);
        assert_eq!(source.resolve(&[u64::MAX, 0x10], BASE), Some(0x10));
        assert_eq!(source.resolve(&[u64::MAX], BASE), Some(BASE - 1));
        assert_eq!(source.value_at(None), 0.0);
    }

    #[test]
    fn native_xml_empty_chain_reads_null_without_ending_the_listener() {
        use crate::derive::{Listener, SourceListener};

        let mut source = game();
        source.set_offsets(xml(
            r#"<Speed Offsets="0"/><Sway Offsets="2" Offset_0="100" Offset_1="14"/><SRV_Roll Offsets="0"/>"#,
        ));
        set_force(&mut source, 1, 1.5);
        assert!(source.is_running().unwrap());
        assert_eq!(source.addresses, [(Force::Speed, None), (Force::Sway, Some(0x2014)), (Force::SrvRoll, None)]);
        assert_eq!(source.process.close_calls, 0);
        assert_eq!(&source.process.read_addresses[1..], &[0, 0x2014]);

        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.failure_point(), None);
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn native_xml_pointer_sums_use_win64_pointer_masking() {
        let mut source = game();
        source.set_offsets(xml(
            r#"<Speed Offsets="1" Offset_0="FFFFFFFFFFFFFFFF"/><Sway Offsets="2" Offset_0="FFFFFFFFFFFFFFEF" Offset_1="10"/><SRV_Roll Offsets="0"/>"#,
        ));
        source.process.write(BASE - 1, &2.5f32.to_le_bytes());
        source.process.write(BASE - 17, &(u64::MAX - 7).to_le_bytes());
        source.process.write(8, &1.5f32.to_le_bytes());
        assert!(source.is_running().unwrap());
        assert_eq!(source.addresses, [(Force::Speed, Some(BASE - 1)), (Force::Sway, Some(8)), (Force::SrvRoll, None)]);
        assert_eq!(source.process.read_addresses, [BASE - 17, BASE - 1]);
        let mut telemetry = Telemetry { speed: 999.0, ..Telemetry::off() };
        assert!(source.receive_native(&mut telemetry).unwrap());
        assert_eq!(lock(&source.forces).native(Force::Speed), Some(&LegacyValue::Float(2.5)));
        assert_eq!(lock(&source.forces).native(Force::Sway), Some(&LegacyValue::Float(1.5)));
        assert_eq!(lock(&source.forces).native(Force::SrvRoll), Some(&LegacyValue::Float(0.0)));
        assert_eq!(telemetry.speed, 999.0, "receiveData only publishes forces");
        assert_eq!(source.process.close_calls, 0);
    }

    #[test]
    fn native_xml_wide_hex_offsets_reach_the_masked_pointer_consumer() {
        let mut source = game();
        source.set_offsets(xml(
            r#"<Speed Offsets="1" Offset_0="10000000000000020"/><Sway Offsets="2" Offset_0="10000000000000100" Offset_1="_١4"/><SRV_Roll Offsets="0"/>"#,
        ));
        source.process.write(BASE + 0x20, &2.5f32.to_le_bytes());
        source.process.write(0x2014, &1.5f32.to_le_bytes());
        assert!(source.is_running().unwrap());
        assert_eq!(
            source.addresses,
            [(Force::Speed, Some(BASE + 0x20)), (Force::Sway, Some(0x2014)), (Force::SrvRoll, None)]
        );
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(lock(&source.forces).native(Force::Speed), Some(&LegacyValue::Float(2.5)));
        assert_eq!(lock(&source.forces).native(Force::Sway), Some(&LegacyValue::Float(1.5)));
    }

    /// Liveness: no offsets, no process, all-zero forces are "not running".
    #[test]
    fn check_status_paths() {
        let mut source = game();
        source.set_offsets(Offsets::default());
        assert!(!source.is_running().unwrap());
        let mut source = game();
        assert!(!source.is_running().unwrap());
        assert_eq!(source.pid, None);
        set_force(&mut source, 0, 120.0);
        assert!(source.is_running().unwrap());
        assert_eq!((source.pid, source.build), (Some(4242), Some(EdBuild::Odyssey)));
        source.process.open_fails = true;
        set_force(&mut source, 0, 0.0);
        assert!(!source.is_running().unwrap());
        assert!(!source.is_running().unwrap());
        assert_eq!(source.pid, None);
    }

    /// An open, a module lookup or a read the system refuses for lack of permission adds the refusal text to
    /// the game errors (the same text however often); the listener still answers "not running".
    #[test]
    fn refused_memory_access_is_added_to_the_game_errors() {
        let refused: std::collections::BTreeSet<String> = [memory::READ_REFUSED.to_owned()].into();
        let mut source = game();
        assert!(source.parameters.game_errors().is_empty());
        source.process.open_fails = true;
        for _ in 0..3 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.pid, None);
        }
        assert_eq!(source.parameters.game_errors(), refused);

        let mut source = game();
        source.process.module_error = Some(io::ErrorKind::PermissionDenied);
        assert!(!source.is_running().unwrap());
        assert_eq!(source.parameters.game_errors(), refused);

        let mut source = game();
        set_force(&mut source, 0, 120.0);
        source.process.refused_read = true;
        assert!(source.is_running().unwrap());
        assert_eq!(source.parameters.game_errors(), refused);
        source.parameters.clear_game_errors();
        let mut telemetry = Telemetry::off();
        source.process.refused_read = true;
        assert!(source.receive_native(&mut telemetry).unwrap());
        assert_eq!(source.parameters.game_errors(), refused);
    }

    /// Any other failure, and a game that is read normally, report nothing.
    #[test]
    fn other_memory_failures_are_not_reported() {
        let mut source = game();
        source.process.open_fails = true;
        source.process.open_error = Some(io::ErrorKind::NotFound);
        assert!(!source.is_running().unwrap());
        assert!(source.parameters.game_errors().is_empty());

        let mut source = game();
        source.process.module_error = Some(io::ErrorKind::NotFound);
        assert!(!source.is_running().unwrap());
        assert!(source.parameters.game_errors().is_empty());

        let mut source = game();
        set_force(&mut source, 0, 120.0);
        assert!(source.is_running().unwrap());
        let mut telemetry = Telemetry::off();
        assert!(source.receive_native(&mut telemetry).unwrap());
        assert!(source.parameters.game_errors().is_empty());
    }

    /// Loading publishes the chain list of both builds, including empty sections.
    #[test]
    fn native_empty_build_keeps_process_and_checks_retained_addresses() {
        let mut source = game();
        source.process.process = Some((7, "D:\\Products\\elite-dangerous-64\\EliteDangerous64.exe".into()));
        set_force(&mut source, 0, 1.0);
        assert!(!source.is_running().unwrap());
        assert_eq!((source.pid, source.base, source.build), (None, Some(BASE), Some(EdBuild::Horizons)));
        assert_eq!(source.process.opened, Some(7));
        assert_eq!(source.process.close_calls, 0);
        assert!(source.addresses.is_empty());

        let mut source = game();
        set_force(&mut source, 0, 30.0);
        assert!(source.is_running().unwrap());
        let addresses = source.addresses.clone();
        source.set_offsets(xml(""));
        source.process.read_addresses.clear();
        assert!(source.is_running().unwrap(), "empty replacement still checks the retained force_addr");
        assert_eq!(source.addresses, addresses);
        assert_eq!(source.process.read_addresses, vec![0x2010]);
        assert_eq!(source.process.close_calls, 0);

        set_force(&mut source, 0, 0.0);
        assert!(!source.is_running().unwrap());
        assert_eq!((source.pid, source.base, source.build), (None, Some(BASE), Some(EdBuild::Odyssey)));
        assert_eq!(source.addresses, addresses);
        assert_eq!(source.process.opened, Some(4242));
        assert_eq!(source.process.close_calls, 0);
    }

    #[test]
    fn native_empty_build_remains_running_through_source_listener() {
        use crate::derive::{Listener, SourceListener};

        let mut source = game();
        set_force(&mut source, 0, 30.0);
        assert!(source.is_running().unwrap());
        source.set_offsets(xml(""));
        let offsets = Arc::clone(&source.offsets);
        let mut listener = SourceListener::new(source);
        assert!(listener.is_running());
        assert_eq!(listener.failure_point(), None);
        listener.receive();
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Float(300.0)));

        *lock(&offsets) = Offsets::default();
        assert!(!listener.is_running(), "empty top-level offsets still stop the check before old addresses");
        assert_eq!(listener.failure_point(), None);
    }

    /// Channels: ship forces ×10 with factors (default -1 for the angles), vehicle "spaceship".
    #[test]
    fn ship_channels() {
        let mut source = game();
        for (i, v) in [50.0, 0.5, -0.25, 1.0, 2.0, 3.0, 4.0].into_iter().enumerate() {
            set_force(&mut source, i, v);
        }
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, "spaceship");
        assert_eq!((t.speed, t.lateral_acceleration, t.vertical_acceleration), (500.0, 5.0, -2.5));
        assert_eq!((t.longitudinal_acceleration, t.pitch, t.roll, t.yaw), (10.0, -20.0, -30.0, -40.0));
        assert_eq!(t.rpm, 0.0);
    }

    /// Channels: with the ship at rest the SRV forces ×3 apply; an SRV roll beyond 180 reads 0.
    #[test]
    fn srv_channels() {
        let mut source = game();
        for (i, v) in [(7, 20.0), (8, 0.5), (12, 200.0), (13, 10.0)] {
            set_force(&mut source, i, v);
        }
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, "srv");
        assert_eq!((t.speed, t.lateral_acceleration, t.roll, t.yaw), (60.0, 1.5, 0.0, -30.0));
    }

    /// Channels: the SRV sum fails (no SRV chains but the roll) and counts as 0; the ship forces still apply.
    #[test]
    fn ship_chains_only() {
        let srv_roll = 12;
        let mut source = game_with(|i| i < 7 || i == srv_roll);
        set_force(&mut source, 0, 30.0);
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!((t.vehicle.as_str(), t.speed), ("spaceship", 300.0));
    }

    /// Channels: the ship sum fails (no ship chains) and counts as 0, so non-zero SRV forces apply ×3.
    #[test]
    fn srv_chains_only() {
        let mut source = game_with(|i| i >= 7);
        set_force(&mut source, 7, 5.0);
        set_force(&mut source, 13, 2.0);
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!((t.vehicle.as_str(), t.speed, t.yaw), ("srv", 15.0, -6.0));
    }

    /// Channels: without an SRV roll the roll-magnitude guard fails for every channel; the
    /// dispatcher keeps speed, rpm and the vehicle, writes registry defaults and stores 0 for yaw/pitch/roll.
    #[test]
    fn missing_srv_roll_raises_every_channel() {
        let srv_roll = 12;
        let mut source = game_with(|i| i != srv_roll);
        set_force(&mut source, 0, 30.0);
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        (t.speed, t.rpm, t.raw_flag, t.yaw, t.pitch, t.roll) = (42.0, 900.0, 3.0, 1.0, 2.0, 3.0);
        (t.vehicle, t.location) = ("spaceship".into(), "Sol".into());
        source.poll(&mut t).unwrap();
        assert_eq!((t.speed, t.rpm, t.raw_flag), (42.0, 900.0, -1.0));
        assert_eq!((t.yaw, t.pitch, t.roll), (0.0, 0.0, 0.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("spaceship", ""));
    }

    /// The sign factors: inversion flips roll/pitch; Yaw VR flips the yaw rule.
    #[test]
    fn factors() {
        let mut settings = EliteSettings::new("/tmp");
        assert_eq!(settings.factors(), (-1.0, -1.0, -1.0));
        settings.invert_yaw = true;
        settings.invert_roll = true;
        assert_eq!(settings.factors(), (1.0, 1.0, -1.0));
        settings.yaw_vr = true;
        assert_eq!(settings.factors().0, -1.0);
        settings.invert_yaw = false;
        assert_eq!(settings.factors().0, 1.0);
    }
}
