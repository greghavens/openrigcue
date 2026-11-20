//! The version steps of the migration chain, in order.
//!
//! Every step runs when the stored version is below its own. Each step names its version; the
//! values and key names (typos included) are those of the files being migrated. Some steps are
//! deliberately asymmetric (for example v238 setting different belt smoothing on game and
//! vehicle level, or v328 seeding `min_traction_loss_front_activation` from the main-actuator share).

use super::{as_dict, DictView, LegacyLayouts, LegacyState, MigrationReport, MigrationSources, Platform};
use crate::import::value::{str2bool, BigInt, Dict, Key, LegacyValue, PythonString};
use crate::patch::{ModulePorts, PatchContext, Patcher, PortKey, UserFolder, UserPath, DCS_LEGACY, GAME_ENTRIES};

mod rebuild;

pub(super) type StepResult = Result<(), String>;

pub(super) struct Step {
    pub version: u32,
    pub apply: fn(&mut Migrator<'_, '_>) -> StepResult,
}

impl Step {
    /// Whether the step advances `APP.config_file_version`; v88 and v300 write their version into
    /// INTELLIBOX instead.
    pub fn advances_cursor(&self) -> bool {
        self.version != 88 && self.version != 300
    }
}

/// Shaker layouts that derive new per-speaker effects from existing ones.
const DERIVED_LAYOUTS: [&str; 8] = [
    "1 Speaker - Center",
    "2 Speakers - Center / Sub",
    "2 Speakers - Front Left / Right",
    "4 Speakers - FL,FR,RL,RR",
    "6 Speakers - 5.1",
    "SRS Shakebox Elite + Sub + Center",
    "SRS ShakeKit or SRS Shakebox Elite",
    "SRS UShake6",
];

/// Whether a tune dict is a game entry or a vehicle entry inside a game's `vehicles`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Level {
    Game,
    Vehicle,
}

type ProfileRows = Box<dyn Iterator<Item = Result<(Key, LegacyValue), String>>>;

pub(super) struct Migrator<'s, 'a> {
    pub state: &'s mut LegacyState,
    pub src: &'s MigrationSources<'a>,
    /// `APP.config_file_version` as an integer, read once before the steps.
    pub file_version: BigInt,
    /// The step being applied (for messages).
    pub current: u32,
    pub report: MigrationReport,
    /// The transient active tune during startup. It is replaced from the
    /// emulator row after migration; it is never saved.
    pub(super) startup_game_config: Dict,
    /// The runtime's non-autocommit profile connection, shared by seeding
    /// groups so v306's pending replacements join v307's later commit.
    pub(super) profile_db: Option<crate::import::sqlitedict::PendingTable>,
    pub(super) metadata_db: std::collections::BTreeMap<String, crate::import::sqlitedict::AutocommitTable>,
    pub(super) snapshot_rebuild: bool,
}

fn s(text: &str) -> LegacyValue {
    LegacyValue::Str(text.to_owned())
}

fn key_error(what: &str) -> String {
    format!("KeyError: {what:?}")
}

/// Restores runtime defaults: no section is hidden, so the
/// only effect is giving `LED.model` the default `RGB` when it is missing.
pub(super) fn restore_hidden_defaults(config: &mut Dict) {
    if let Some(mut led) = config.get_mut("LED").and_then(|v| as_dict(v, "LED").ok()) {
        let _ = led.set_default("model", s("RGB"));
    }
}

impl<'s, 'a> Migrator<'s, 'a> {
    pub fn new(state: &'s mut LegacyState, src: &'s MigrationSources<'a>) -> Result<Self, super::MigrationError> {
        let fail = |message: String| super::MigrationError { version: 0, message };
        let app = state.config.get("APP").ok_or_else(|| fail(key_error("APP")))?;
        let version = app.item_owned("config_file_version").map_err(|_| fail(key_error("config_file_version")))?;
        let file_version = version.py_integer().map_err(fail)?;
        // A missing `APP.first_time_installation` is set to 'no', but the chain still needs the value it
        // could not read, so it fails before any step.
        if app.item_owned("first_time_installation").is_err() {
            as_dict(state.config.get_mut("APP").expect("APP already selected"), "APP")
                .map_err(fail)?
                .insert("first_time_installation", s("no"))
                .map_err(fail)?;
            return Err(fail(
                "cannot access local variable 'first_time_installation' where it is not associated with a value"
                    .to_owned(),
            ));
        }
        let migrator = Migrator {
            state,
            src,
            file_version,
            current: 0,
            report: MigrationReport::default(),
            startup_game_config: Dict::new(),
            profile_db: None,
            metadata_db: Default::default(),
            snapshot_rebuild: true,
        };
        Ok(migrator)
    }

    fn windows(&self) -> bool {
        self.src.platform == Platform::Windows
    }

    fn skipped(&mut self, what: &str) {
        self.report.skipped_side_effects.push(format!("v{}: {what}", self.current));
    }

    /// A registry value, or `None` when it is not set.
    fn get_reg(&self, name: &str) -> Option<LegacyValue> {
        let value = if let Some(effects) = self.src.effects {
            // The registry file reads only the selected value, and a failed
            // read is absent; it is independent of metadata caches.
            crate::import::registry::read_item(effects.docs, name).ok().flatten()
        } else {
            self.state.registry.get(name).cloned()
        };
        value.filter(|value| value.type_name() != "NoneType")
    }

    /// Sets a registry text value.
    fn set_reg(&mut self, name: &str, value: &str) {
        if let Some(effects) = self.src.effects {
            // Write failures are handled here; migration callers ignore the
            // result. Do not decode the previous value before assigning.
            if let Err(error) = crate::import::registry::set_item(effects.docs, name, &s(value)) {
                tracing::error!(%error, name, "error while setting migration launcher flag");
                return;
            }
        }
        self.state.registry.insert(name, s(value));
    }

    // ---- config.ini access; a missing key is an error ----

    pub fn section(&mut self, name: &str) -> Result<DictView<'_>, String> {
        self.state.config.get_mut(name).ok_or_else(|| key_error(name)).and_then(|v| as_dict(v, name))
    }

    fn section_ref(&self, name: &str) -> Result<&LegacyValue, String> {
        self.state.config.get(name).ok_or_else(|| key_error(name))
    }
    pub fn get(&self, section: &str, key: &str) -> Result<LegacyValue, String> {
        self.section_ref(section)?.item_owned(key).map_err(|_| key_error(key))
    }
    fn get_owned(&self, section: &str, key: &str) -> Result<LegacyValue, String> {
        self.get(section, key)
    }

    pub fn set(&mut self, section: &str, key: &str, value: LegacyValue) -> StepResult {
        self.section(section)?.insert(key, value)?;
        Ok(())
    }

    fn set_all(&mut self, section: &str, entries: &[(&str, LegacyValue)]) -> StepResult {
        for (k, v) in entries {
            self.set(section, k, v.clone())?;
        }
        Ok(())
    }

    /// Deletes a key; a missing one is an error.
    fn del(&mut self, section: &str, key: &str) -> StepResult {
        self.section(section)?.remove(key)?.map(|_| ()).ok_or_else(|| key_error(key))
    }

    /// Deletes a key; a missing one is ignored.
    fn try_del(&mut self, section: &str, key: &str) {
        if let Ok(mut sec) = self.section(section) {
            let _ = sec.remove(key);
        }
    }

    /// Creates (or empties) a section.
    fn new_section(&mut self, name: &str) {
        self.state.config.insert(name, LegacyValue::Dict(Dict::new()));
    }

    /// Section copies clear the destination before looking up the source.
    fn copy_section(&mut self, from: &str, to: &str) -> StepResult {
        self.new_section(to);
        let value = self.state.config.get(from).ok_or_else(|| key_error(from))?;
        let copied = value.copy_builtin()?;
        self.state.config.insert(to, copied);
        Ok(())
    }

    fn copy_key(&mut self, from: (&str, &str), to: (&str, &str)) -> StepResult {
        let value = self.get_owned(from.0, from.1)?;
        self.set(to.0, to.1, value)
    }

    fn int_of(&self, section: &str, key: &str) -> Result<BigInt, String> {
        let v = self.get(section, key)?;
        v.py_integer()
    }

    /// Whether `APP.first_time_installation` is exactly `yes`.
    fn first_install_literal(&self) -> bool {
        self.get("APP", "first_time_installation").ok().and_then(|v| v.python_string()).is_some_and(|v| v == "yes")
    }

    /// `APP.first_time_installation` as a boolean (an unknown spelling is an error).
    fn first_install_bool(&self) -> Result<bool, String> {
        let value = self.get("APP", "first_time_installation")?;
        if value.type_name() == "bool" {
            return Ok(value.is_truthy());
        }
        if let Some(text) = value.python_string().and_then(|s| s.scalar_string()) {
            return str2bool(&text).ok_or_else(|| format!("Boolean value expected. Got {text}"));
        }
        Err(format!("Boolean value expected. Got {value}"))
    }

    // ---- shortcuts ----

    fn shortcut(&mut self, name: &str, value: &str) -> StepResult {
        self.set("SHORTCUTS", name, s(value))
    }

    /// The four `<prefix><id>_{up,down}_{btns,keys}` entries, all empty, in file order.
    fn up_down_shortcuts(&mut self, prefix: &str, id: &str) -> StepResult {
        for suffix in ["_up_btns", "_up_keys", "_down_btns", "_down_keys"] {
            self.shortcut(&format!("{prefix}{id}{suffix}"), "")?;
        }
        Ok(())
    }

    /// Empty entries for every id, grouped by suffix: `up_keys`, `up_btns`, `down_keys`, `down_btns`.
    fn grouped_up_down_shortcuts(&mut self, prefix: &str, ids: &[String]) -> StepResult {
        for up_down in ["up_keys", "up_btns", "down_keys", "down_btns"] {
            for id in ids {
                self.shortcut(&format!("{prefix}{id}_{up_down}"), "")?;
            }
        }
        Ok(())
    }

    // ---- profile.db ----

    fn profile_database(&mut self) -> Result<Option<&crate::import::sqlitedict::PendingTable>, String> {
        if self.profile_db.is_none() {
            if let Some(effects) = self.src.effects {
                let path = effects.docs.join("profile.db");
                if !path.exists() {
                    // The template is copied before migrations.
                    crate::import::sqlitedict::replace_table(&path, "unnamed", &self.state.profiles)
                        .map_err(|error| error.to_string())?;
                }
                self.profile_db = Some(
                    crate::import::sqlitedict::PendingTable::open(&path, "unnamed")
                        .map_err(|error| error.to_string())?,
                );
            }
        }
        Ok(self.profile_db.as_ref())
    }

    /// Stores the factory tune of `source` under `target`, before its commit boundary.
    fn assign_game(&mut self, target: &str, source: &str) -> StepResult {
        let tune = self.src.master_tunes.get(source).cloned().ok_or_else(|| key_error(source))?;
        self.assign_profile(target, tune)
    }

    fn assign_profile(&mut self, target: &str, tune: LegacyValue) -> StepResult {
        if let Some(database) = self.profile_database()? {
            database.assign(target, Some(&tune)).map_err(|error| error.to_string())?;
        }
        self.state.profiles.remove(target); // REPLACE moves the rowid
        self.state.profiles.insert(target, tune);
        Ok(())
    }

    fn commit_games(&self) -> StepResult {
        if self.current != 306 {
            self.commit_profiles()?;
        }
        Ok(())
    }

    fn commit_profiles(&self) -> StepResult {
        if let Some(database) = &self.profile_db {
            database.commit().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn add_game(&mut self, target: &str, source: &str) -> StepResult {
        self.assign_game(target, source)?;
        self.commit_games()
    }

    fn add_games(&mut self, ids: &[&str]) -> StepResult {
        for id in ids {
            self.assign_game(id, id)?;
        }
        self.commit_games()
    }

    /// Drops what a configuration carries for 4D videos, which OpenRigCue does not have: the port, the
    /// menu switch and the game's tune.
    pub(super) fn drop_4d_videos(&mut self) -> StepResult {
        self.try_del("NETWORK_PORT", "4dvideo");
        self.try_del("CUSTOM_FEATURES", "4dvideo_editor_mode");
        let stored = match self.profile_db.as_ref() {
            Some(database) => database.get("4dvideo").map_err(|error| error.to_string())?.is_some(),
            None => self.state.profiles.contains("4dvideo"),
        };
        if stored {
            self.delete_game("4dvideo")?;
            self.commit_profiles()?;
        }
        Ok(())
    }

    fn delete_game(&mut self, game: &str) -> StepResult {
        if let Some(database) = self.profile_database()? {
            database.assign(game, None).map_err(|error| error.to_string())?;
        } else if !self.state.profiles.contains(game) {
            return Err(key_error(game));
        }
        self.state.profiles.remove(game);
        Ok(())
    }

    fn read_profile(&mut self, game: &str) -> Result<LegacyValue, String> {
        if let Some(database) = self.profile_database()? {
            database.get(game).map_err(|error| error.to_string())?.ok_or_else(|| key_error(game))
        } else {
            self.state.profiles.get(game).cloned().ok_or_else(|| key_error(game))
        }
    }

    fn profile_rows(&mut self) -> Result<ProfileRows, String> {
        if let Some(database) = self.profile_database()? {
            let rows = database.rows().map_err(|error| error.to_string())?;
            Ok(Box::new(
                rows.map(|row| row.map(|(key, value)| (Key::from(key), value)).map_err(|error| error.to_string())),
            ))
        } else {
            let rows: Vec<_> =
                self.state.profiles.iter().map(|(key, value)| Ok((key.clone(), value.clone()))).collect();
            Ok(Box::new(rows.into_iter()))
        }
    }

    fn profile(&mut self, game: &str) -> Result<LegacyValue, String> {
        self.read_profile(game)
    }

    fn edit_profile(&mut self, game: &str, edit: impl FnOnce(&mut DictView<'_>) -> StepResult) -> StepResult {
        let mut value = self.read_profile(game)?;
        edit(&mut as_dict(&mut value, game)?)?;
        self.assign_profile(game, value)?;
        self.commit_profiles()
    }

    fn master_vehicle(&self, game: &str, vehicle: &str) -> Result<LegacyValue, String> {
        self.src
            .master_tunes
            .get(game)
            .ok_or_else(|| key_error(game))?
            .item_owned("vehicles")?
            .item_owned(vehicle)
            .map_err(|_| key_error(vehicle))
    }

    /// Reads a game row, replaces one vehicle with its factory tune and stores the row again.
    fn add_master_vehicle(&mut self, game: &str, vehicle: &str) -> StepResult {
        self.add_master_vehicles(game, &[vehicle])
    }

    fn add_master_vehicles(&mut self, game: &str, vehicles: &[&str]) -> StepResult {
        let mut value = self.read_profile(game)?;
        for vehicle in vehicles {
            let factory = self.master_vehicle(game, vehicle)?;
            vehicles_of(&mut as_dict(&mut value, game)?)?.insert(*vehicle, factory)?;
        }
        self.assign_profile(game, value)?;
        self.commit_profiles()
    }

    fn rekey_profile_vehicles(
        &mut self,
        selected: impl Fn(&str) -> bool,
        new_key: impl Fn(&PythonString) -> Option<PythonString>,
    ) -> StepResult {
        for row in self.profile_rows()? {
            let (game, value) = row?;
            // The copy is made even for games outside the selected set.
            let copied = value.copy_builtin()?;
            let game = game.to_text();
            if selected(&game) {
                let mut value = copied;
                rekey_vehicles(&mut vehicles_of(&mut as_dict(&mut value, &game)?)?, &new_key)?;
                self.assign_profile(&game, value)?;
            }
        }
        self.commit_profiles()
    }

    /// Applies `f` to every game tune and then to each of its vehicle tunes.
    fn each_tune(&mut self, f: impl FnMut(&str, &mut DictView<'_>, Level) -> StepResult) -> StepResult {
        self.each_tune_pending(f)?;
        self.commit_profiles()
    }

    fn each_tune_pending(&mut self, f: impl FnMut(&str, &mut DictView<'_>, Level) -> StepResult) -> StepResult {
        self.each_selected_tune_pending(|_| true, f)
    }

    fn each_selected_tune(
        &mut self,
        selected: impl FnMut(&str) -> bool,
        f: impl FnMut(&str, &mut DictView<'_>, Level) -> StepResult,
    ) -> StepResult {
        self.each_selected_tune_pending(selected, f)?;
        self.commit_profiles()
    }

    fn each_selected_tune_pending(
        &mut self,
        mut selected: impl FnMut(&str) -> bool,
        mut f: impl FnMut(&str, &mut DictView<'_>, Level) -> StepResult,
    ) -> StepResult {
        // Each game row is decoded afresh, then assigned only after all
        // its vehicles succeed. A failed vehicle leaves that database row intact.
        for row in self.profile_rows()? {
            let (game, value) = row?;
            let game_id = game.to_text();
            if !selected(&game_id) {
                continue;
            }
            let mut value = value.clone();
            let mut tune = as_dict(&mut value, &game_id)?;
            f(&game_id, &mut tune, Level::Game)?;
            let mut vehicles = vehicles_of(&mut tune)?;
            let mut cursor = vehicles.cursor()?;
            while let Some(vehicle) = cursor.next(&vehicles)? {
                let mut child = vehicles.child(&vehicle)?;
                f(&game_id, &mut child, Level::Vehicle)?;
            }
            self.assign_profile(&game_id, value)?;
        }
        Ok(())
    }

    /// Sets `key = value` on every game and vehicle tune.
    fn set_every_tune(&mut self, entries: &[(&str, LegacyValue)]) -> StepResult {
        self.set_every_tune_pending(entries)?;
        self.commit_profiles()
    }

    fn set_every_tune_pending(&mut self, entries: &[(&str, LegacyValue)]) -> StepResult {
        self.each_tune_pending(|_, tune, _| {
            for (k, v) in entries {
                tune.insert(*k, v.clone())?;
            }
            Ok(())
        })
    }

    /// Sets `tune[group][key] = value` on every game and vehicle tune.
    fn set_every_nested(&mut self, entries: &[(&str, &str, LegacyValue)]) -> StepResult {
        self.set_every_nested_pending(entries)?;
        self.commit_profiles()
    }

    fn set_every_nested_pending(&mut self, entries: &[(&str, &str, LegacyValue)]) -> StepResult {
        self.each_tune_pending(|_, tune, _| {
            for (group, k, v) in entries {
                nested(tune, group)?.insert(*k, v.clone())?;
            }
            Ok(())
        })
    }

    /// Adds a shaker effect with its standard DSP defaults (level 10, filter 0, cutoff 400,
    /// resonance 10, enabled) to every tune (v205, v206, v234, v321, v324).
    fn add_shaker_effect_everywhere(&mut self, effect: &str) -> StepResult {
        self.each_tune(|_, tune, _| add_shaker_effect(tune, effect))
    }

    // ---- metadata.db shakers ----

    fn metadata_values(&self, table: &str) -> &Dict {
        match table {
            "joystick" => &self.state.joysticks,
            "shakers" => &self.state.shaker_layouts,
            "game_config_notifications" => &self.state.game_config_notifications,
            "notifications" => &self.state.notifications,
            _ => unreachable!("registered migration metadata table"),
        }
    }

    fn metadata_database(
        &mut self,
        table: &str,
    ) -> Result<Option<&crate::import::sqlitedict::AutocommitTable>, String> {
        let Some(effects) = self.src.effects else { return Ok(None) };
        if !self.metadata_db.contains_key(table) {
            let path = effects.docs.join("metadata.db");
            if !path.exists() {
                for (table, values) in [("joystick", &self.state.joysticks), ("shakers", &self.state.shaker_layouts)] {
                    crate::import::sqlitedict::replace_table(&path, table, values)
                        .map_err(|error| error.to_string())?;
                }
            }
            let database =
                crate::import::sqlitedict::AutocommitTable::open(&path, table).map_err(|error| error.to_string())?;
            self.metadata_db.insert(table.to_owned(), database);
        }
        Ok(self.metadata_db.get(table))
    }

    fn metadata_row(&mut self, table: &str, key: &str) -> Result<Option<LegacyValue>, String> {
        if let Some(database) = self.metadata_database(table)? {
            database.get(key).map_err(|error| error.to_string())
        } else {
            Ok(self.metadata_values(table).get(key).cloned())
        }
    }

    fn metadata_rows(&mut self, table: &str) -> Result<ProfileRows, String> {
        if let Some(database) = self.metadata_database(table)? {
            let rows = database.rows().map_err(|error| error.to_string())?;
            Ok(Box::new(
                rows.map(|row| row.map(|(key, value)| (Key::from(key), value)).map_err(|error| error.to_string())),
            ))
        } else {
            let rows: Vec<_> =
                self.metadata_values(table).iter().map(|(key, value)| Ok((key.clone(), value.clone()))).collect();
            Ok(Box::new(rows.into_iter()))
        }
    }

    fn commit_metadata(&mut self, table: &str) -> StepResult {
        if let Some(database) = self.metadata_database(table)? {
            database.commit().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn persist_metadata_row(&mut self, table: &str, key: &str, value: &LegacyValue) -> StepResult {
        if let Some(database) = self.metadata_database(table)? {
            database.set(key, value).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn assign_layout(&mut self, name: &str, value: LegacyValue) -> StepResult {
        self.persist_metadata_row("shakers", name, &value)?;
        self.state.shaker_layouts.remove(name);
        self.state.shaker_layouts.insert(name, value);
        Ok(())
    }

    fn assign_joystick(&mut self, name: &str, value: LegacyValue) -> StepResult {
        self.persist_metadata_row("joystick", name, &value)?;
        self.state.joysticks.remove(name);
        self.state.joysticks.insert(name, value);
        Ok(())
    }

    /// Applies `f` to the effect levels of every channel of every layout.
    fn each_layout_channel(&mut self, mut f: impl FnMut(&str, &mut DictView<'_>) -> StepResult) -> StepResult {
        self.each_layout_channel_named(|layout, _, sfx| f(layout, sfx))
    }

    /// Built-in layouts copy `sources[i]` into `targets[i]`; custom layouts get 0.
    fn derive_layout_effects(&mut self, pairs: &[(&str, &str)]) -> StepResult {
        self.each_layout_channel(|layout, sfx| {
            let derived = DERIVED_LAYOUTS.contains(&layout);
            for (target, source) in pairs {
                let value = if derived { sfx.get_result(source)? } else { 0.into() };
                sfx.insert(*target, value)?;
            }
            Ok(())
        })
    }
}

fn add_shaker_effect(tune: &mut DictView<'_>, effect: &str) -> StepResult {
    for (group, value) in [
        ("shaker_effects", 10.into()),
        ("shaker_dsp_type", 0.into()),
        ("shaker_cutoff", 400.into()),
        ("shaker_resonance", 10.into()),
        ("shaker_switch", true.into()),
    ] {
        nested(tune, group)?.insert(effect, value)?;
    }
    Ok(())
}

fn vehicles_of<'a>(tune: &'a mut DictView<'_>) -> Result<DictView<'a>, String> {
    nested(tune, "vehicles")
}
fn nested<'a>(tune: &'a mut DictView<'_>, group: &str) -> Result<DictView<'a>, String> {
    tune.nested(group)
}
fn copy_within(tune: &mut DictView<'_>, from: &str, to: &str) -> StepResult {
    let value = tune.get_result(from)?;
    tune.insert(to, value)?;
    Ok(())
}
fn copy_nested(tune: &mut DictView<'_>, group: &str, from: &str, to: &str) -> StepResult {
    copy_within(&mut nested(tune, group)?, from, to)
}

macro_rules! steps {
    ($($version:literal => $apply:ident),* $(,)?) => {
        pub(super) const STEPS: &[Step] = &[$(Step { version: $version, apply: $apply }),*];
    };
}

steps! {
    84 => v84, 85 => v85, 86 => v86, 87 => v87, 88 => v88, 89 => v89, 90 => v90, 91 => v91, 92 => v92,
    93 => v93, 94 => v94, 95 => v95, 96 => v96, 97 => v97, 98 => v98, 99 => v99, 100 => v100,
    101 => v101, 102 => v102, 103 => v103, 104 => v104, 105 => v105, 106 => v106, 107 => v107,
    108 => v108, 109 => v109, 110 => v110, 111 => v111, 112 => v112, 113 => v113, 114 => v114,
    115 => v115, 116 => v116, 119 => v119, 120 => v120, 121 => v121, 122 => v122, 123 => v123,
    124 => v124, 125 => v125, 126 => v126, 127 => v127, 128 => v128, 129 => v129, 130 => v130,
    131 => v131, 132 => v132, 133 => v133, 134 => v134, 135 => v135, 136 => v136, 137 => v137,
    138 => v138, 139 => v139, 140 => v140, 141 => v141, 142 => v142, 143 => v143, 144 => v144,
    145 => v145, 147 => v147, 148 => v148, 149 => v149, 150 => v150, 151 => v151, 152 => v152,
    153 => v153, 154 => v154, 155 => v155, 156 => v156, 157 => v157, 158 => v158, 159 => v159,
    160 => v160, 161 => v161, 162 => v162, 163 => v163, 164 => v164, 165 => v165, 166 => v166,
    167 => v167, 168 => v168, 169 => v169, 170 => v170, 171 => v169, 172 => v172, 173 => v173,
    174 => v174, 175 => v175, 176 => v176, 177 => v177, 178 => v178, 179 => v179, 180 => v180,
    181 => v181, 182 => v182, 183 => v183, 184 => v184, 185 => v185, 186 => v186, 187 => v187,
    190 => v190, 191 => v191, 196 => v196, 197 => v197, 198 => v198, 199 => v199, 205 => v205,
    206 => v206, 207 => v207, 208 => v208, 209 => v209, 210 => v210, 211 => v211, 212 => v212,
    213 => v213, 214 => v214, 215 => v215, 216 => v216, 217 => v217, 218 => v218, 219 => v219,
    220 => v220, 221 => v221, 222 => v222, 223 => v223, 224 => v224, 225 => v225, 226 => v226,
    227 => v227, 228 => v228, 229 => v229, 230 => v230, 232 => v232, 233 => v233, 234 => v234,
    235 => v235, 236 => v236, 237 => v237, 238 => v238, 239 => v239, 240 => v240, 241 => v241,
    242 => v242, 243 => v243, 244 => v244, 245 => v245, 246 => v246, 247 => v247, 248 => v248,
    249 => v249, 250 => v250, 251 => v251, 252 => v252, 253 => v253, 254 => v254, 255 => v255,
    256 => v256, 257 => v257, 258 => v258, 259 => v259, 260 => v260, 261 => v261, 262 => v262,
    263 => v263, 264 => v264, 265 => v265, 266 => v266, 267 => v267, 268 => v268, 269 => v269,
    270 => v270, 271 => v271, 272 => v272, 273 => v273, 274 => v274, 275 => v275, 276 => v276,
    277 => v277, 278 => v278, 279 => v279, 280 => v280, 281 => v281, 282 => v282, 283 => v283,
    285 => v285, 286 => v286, 287 => v287, 288 => v288, 289 => v287,
    290 => v290, 291 => v291, 292 => v292, 293 => v293, 294 => v294, 295 => v295, 297 => v297,
    298 => v298, 299 => v299, 300 => v300, 301 => v301, 302 => v302, 303 => v303, 306 => v306,
    307 => v302, 308 => v308, 309 => v309, 310 => v310, 311 => v311, 312 => v312, 314 => v314,
    315 => v315, 316 => v316, 317 => v317, 318 => v318, 319 => v319, 320 => v320, 321 => v321,
    322 => v322, 323 => v323, 324 => v324, 325 => v325, 326 => v326, 327 => v327, 328 => v328,
    329 => v329, 330 => v330, 331 => v331, 332 => v332, 333 => v333, 334 => v334, 335 => v335,
    336 => v336, 337 => v337, 338 => v338, 339 => v339, 340 => v340, 341 => v341, 342 => v342,
    345 => v345, 346 => v346, 347 => v347, 349 => v349, 350 => v350, 351 => v351, 352 => v352,
    353 => v353, 354 => v354, 355 => v355, 356 => cursor_only, 357 => cursor_only, 358 => v358,
    359 => v359, 360 => v360, 361 => v361, 362 => v362, 363 => v363, 365 => v365, 366 => v366,
    367 => v367, 368 => v368, 369 => v369, 370 => v370, 371 => v371, 372 => v372, 373 => v373,
    374 => v374, 375 => v375, 376 => v376, 377 => v377, 378 => v378, 379 => v379, 381 => v381,
    382 => v382, 383 => v383, 386 => v386, 388 => v388, 389 => v389, 390 => v390, 391 => v391,
    393 => v393, 394 => v394, 395 => v395, 396 => v396, 397 => v397, 398 => v398, 400 => v400,
    401 => v401, 402 => v402, 404 => v404, 405 => cursor_only, 406 => v406, 407 => v407, 408 => v408,
    409 => v409, 410 => v410, 412 => v412,
}

/// v356/357 make no hardware/port changes; v405
/// only advances the version. Their version writes still occur in step order.
fn cursor_only(_m: &mut Migrator) -> StepResult {
    Ok(())
}

fn v84(m: &mut Migrator) -> StepResult {
    m.set_all(
        "CUSTOM_FEATURES",
        &[("p3d_invert_yaw", s("no")), ("p3d_invert_pitch", s("no")), ("p3d_invert_roll", s("no"))],
    )
}

fn v85(m: &mut Migrator) -> StepResult {
    let rgb = |r: i64, g: i64, b: i64| LegacyValue::List(vec![r.into(), g.into(), b.into()]);
    m.set_all(
        "LED",
        &[
            ("segment1_color", rgb(255, 0, 0)),
            ("segment2_color", rgb(0, 128, 0)),
            ("segment3_color", rgb(0, 0, 255)),
            ("segment4_color", rgb(255, 165, 0)),
        ],
    )
}

fn v86(m: &mut Migrator) -> StepResult {
    m.set("APP", "measurement_unit", s("K"))
}

fn v87(m: &mut Migrator) -> StepResult {
    m.set_all("APP", &[("window_width", 1280.into()), ("window_height", 800.into())])
}

/// Copies the emulator wind settings into INTELLIBOX; the version is written to INTELLIBOX.
fn v88(m: &mut Migrator) -> StepResult {
    let wind_type = m.profile("emulator")?.item_owned("wind_type")?;
    m.set("INTELLIBOX", "wind_type", wind_type)?;
    let curving = m.profile("emulator")?.item_owned("wind_curving")?;
    let fans = curving.py_add(&1.into())?;
    m.set("INTELLIBOX", "nbr_fans", fans)?;
    m.set("INTELLIBOX", "config_file_version", 88.into())
}

fn v89(m: &mut Migrator) -> StepResult {
    m.set("APP", "additional_telemetry", s("no"))
}

fn v90(m: &mut Migrator) -> StepResult {
    m.set("APP", "disable_tuning_alert_standby", s("no"))
}

fn v91(m: &mut Migrator) -> StepResult {
    m.set("APP", "startup_page", s("Wind"))
}

fn v92(m: &mut Migrator) -> StepResult {
    m.set("APP", "default_browser", s("auto"))
}

fn v93(m: &mut Migrator) -> StepResult {
    m.set_all("NETWORK_PORT", &[("rbr", s("6776")), ("thecrew", s("1337"))])?;
    m.add_games(&["thecrew2"])
}

fn v94(m: &mut Migrator) -> StepResult {
    m.add_games(&["thecrew", "rbr"])
}

fn v95(m: &mut Migrator) -> StepResult {
    m.assign_joystick("Default", LegacyValue::Dict(Dict::new()))
}

fn v96(m: &mut Migrator) -> StepResult {
    m.add_games(&["joy"])
}

fn v97(m: &mut Migrator) -> StepResult {
    m.shortcut("shake_box_switch_btns", "")?;
    m.shortcut("shake_box_switch_keys", "ctrl+shift+f5")
}

fn v98(m: &mut Migrator) -> StepResult {
    m.add_games(&["gtav"])?;
    m.set("NETWORK_PORT", "gtav", s("20777"))
}

/// Resolve the startup input at the point where v99 reads it.
fn v99_layouts(source: LegacyLayouts, shipped: &Dict) -> Result<Dict, String> {
    match source {
        LegacyLayouts::DeferredFile(path) => {
            let source = crate::import::read_legacy_layouts(&path).map_err(|error| error.to_string())?;
            v99_layouts(source, shipped)
        }
        LegacyLayouts::Absent | LegacyLayouts::Unreadable => Ok(shipped.clone()),
        LegacyLayouts::Layouts(layouts) => Ok(layouts),
        LegacyLayouts::NotAnObject(type_name) => Err(format!("'{type_name}' object has no attribute 'items'")),
    }
}

/// Seeds layouts from the user or shipped file, then attempts user-file deletion.
fn v99(m: &mut Migrator) -> StepResult {
    let layouts = v99_layouts(m.src.legacy_layouts.clone(), m.src.shipped_layouts)?;
    for (name, layout) in layouts.iter() {
        if m.src.effects.is_some() {
            let Key::Str(key) = name else { return Err("sqlitedict layout keys must be strings".into()) };
            // The shakers table autocommits: each assignment commits
            // before the next layout, source-file deletion or version update.
            m.persist_metadata_row("shakers", key, layout)?;
        }
        if let Key::Str(key) = name {
            // SQLite REPLACE gives an existing key a new rowid.
            m.state.shaker_layouts.remove(key);
        }
        m.state.shaker_layouts.insert(name.clone(), layout.clone());
    }
    match m.src.effects {
        Some(effects) => {
            let _ = std::fs::remove_file(effects.docs.join("profiles.json"));
        }
        None if *m.src.legacy_layouts != LegacyLayouts::Absent => m.skipped("delete <docs>/profiles.json"),
        None => {}
    }
    Ok(())
}

fn v100(m: &mut Migrator) -> StepResult {
    m.add_games(&["vracer"])?;
    m.set("NETWORK_PORT", "vracer", s("8051"))?;
    m.add_games(&["aeroflyfs2"])?;
    m.set("NETWORK_PORT", "aeroflyfs2", s("4123"))
}

/// Version 101 belongs to the sales-gating key `APP.trial_subscription_started` of imported files.
/// OpenRigCue has no such concept and ignores the key on import, so the step changes nothing.
fn v101(_m: &mut Migrator) -> StepResult {
    Ok(())
}

fn v102(m: &mut Migrator) -> StepResult {
    m.add_games(&["fs2020"])
}

fn v103(m: &mut Migrator) -> StepResult {
    m.add_games(&["f12020"])?;
    m.set("NETWORK_PORT", "f12020", s("20782"))
}

fn v104(m: &mut Migrator) -> StepResult {
    let mut axes = Dict::new();
    axes.insert("0", s("None"));
    axes.insert("1", s("None"));
    m.set("MOTIONBOX", "axis_assignment", LegacyValue::Dict(axes))
}

/// Splits the DOF Reality controller settings out of MOTIONBOX and creates PROSIMU.
fn v105(m: &mut Migrator) -> StepResult {
    m.new_section("DOF REALITY");
    m.new_section("PROSIMU");
    for key in [
        "left_controller_com_port",
        "right_controller_com_port",
        "left_debug_port",
        "right_debug_port",
        "smoothing_threshold",
        "soft_crash_threshold",
        "boost_threshold",
        "update_kp",
    ] {
        m.copy_key(("MOTIONBOX", key), ("DOF REALITY", key))?;
    }
    m.set_all(
        "PROSIMU",
        &[("smoothing_threshold", 1.into()), ("soft_crash_threshold", 5000.into()), ("boost_threshold", 1500.into())],
    )?;
    for key in [
        "left_controller_com_port",
        "right_controller_com_port",
        "left_debug_port",
        "right_debug_port",
        "smoothing_threshold",
        "soft_crash_threshold",
        "boost_threshold",
        "update_kp",
        "allow_3rd_party_microcontroller",
    ] {
        m.del("MOTIONBOX", key)?;
    }
    Ok(())
}

fn v106(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_profile_manager", s("no"))?;
    m.set("SHAKEBOX", "refresh_rate", s("27"))?;
    m.set("INTELLIBOX", "refresh_rate", s("7"))?;
    m.set("MOTIONBOX", "refresh_rate", s("30"))?;
    m.copy_key(("CUSTOM_FEATURES", "webui_port"), ("APP", "webui_port"))?;
    m.del("CUSTOM_FEATURES", "webui_port")
}

fn v107(m: &mut Migrator) -> StepResult {
    m.try_del("CUSTOM_FEATURES", "enable_profile_manager");
    Ok(())
}

fn v108(m: &mut Migrator) -> StepResult {
    let master = m.src.master_tunes;
    m.edit_profile("api", |tune| {
        let epic = master.get("epic").cloned().ok_or_else(|| key_error("epic"))?;
        vehicles_of(tune)?.insert("Epic Roller Coasters", epic)?;
        Ok(())
    })
}

/// Caps `motion_boost_factor` at 35 on games and vehicles.
fn v109(m: &mut Migrator) -> StepResult {
    m.each_tune(|_, tune, _| {
        let boost = tune.get_result("motion_boost_factor")?;
        let over = boost.py_gt(&35.into())?;
        if over {
            tune.insert("motion_boost_factor", 35.into())?;
        }
        Ok(())
    })
}

fn v110(m: &mut Migrator) -> StepResult {
    m.set("PROSIMU", "prs200_com_port", s(""))
}

/// Rescales the thresholds: DOF Reality integer value / 10, PROSIMU integer value / 100 (float results).
fn v111(m: &mut Migrator) -> StepResult {
    for (section, divisor) in [("DOF REALITY", 10), ("PROSIMU", 100)] {
        for key in ["smoothing_threshold", "soft_crash_threshold", "boost_threshold"] {
            let value = openrigcue_model::native_arithmetic::integer_true_divide(
                &m.int_of(section, key)?,
                &BigInt::from(divisor),
            )?;
            m.set(section, key, value.into())?;
        }
    }
    Ok(())
}

fn v112(m: &mut Migrator) -> StepResult {
    m.new_section("Yaw VR");
    m.set_all(
        "Yaw VR",
        &[("pitch_fwd_limit", s("30")), ("pitch_bwd_limit", s("30")), ("roll_limit", s("30")), ("yaw_limit", s("60"))],
    )?;
    for key in ["smoothing_threshold", "soft_crash_threshold", "boost_threshold"] {
        m.copy_key(("PROSIMU", key), ("Yaw VR", key))?;
    }
    Ok(())
}

fn v113(m: &mut Migrator) -> StepResult {
    m.set("PROSIMU", "enable_dyatic_tl", s("yes"))
}

fn v114(m: &mut Migrator) -> StepResult {
    m.set("PROSIMU", "prs200_firmware", s("3"))
}

fn v115(m: &mut Migrator) -> StepResult {
    m.set("Yaw VR", "power", s("50"))?;
    m.set("MOTIONBOX", "real_heave", s("no"))
}

fn v116(m: &mut Migrator) -> StepResult {
    m.shortcut("motion_box_homing_btns", "")?;
    m.shortcut("motion_box_homing_keys", "ctrl+shift+f6")
}

fn v119(m: &mut Migrator) -> StepResult {
    m.add_master_vehicles("ed", &["srv", "spaceship"])
}

/// Project CARS 3 entries start as copies of the Project CARS 2 ones.
fn v120(m: &mut Migrator) -> StepResult {
    for (target, source) in [("projectcars3", "projectcars2"), ("projectcars3udp", "projectcars2udp")] {
        let tune = m.read_profile(source)?;
        m.assign_profile(target, tune)?;
        m.commit_profiles()?;
    }
    Ok(())
}

fn v121(m: &mut Migrator) -> StepResult {
    for key in ["power", "pitch_fwd_limit", "pitch_bwd_limit", "roll_limit", "yaw_limit"] {
        m.del("Yaw VR", key)?;
    }
    Ok(())
}

fn v122(m: &mut Migrator) -> StepResult {
    m.copy_key(("PROSIMU", "prs200_com_port"), ("PROSIMU", "prs200_com_port1"))?;
    m.set("PROSIMU", "prs200_com_port2", s(""))?;
    m.del("PROSIMU", "prs200_com_port")
}

/// Rebuilds both databases. Every vehicle entry gets an empty `vehicles` (dropping vehicles nested
/// inside vehicles), and the new `metadata.db` receives only the `joystick` and `shakers` tables,
/// so the notification tables start empty.
fn v123(m: &mut Migrator) -> StepResult {
    rebuild::apply(m)
}

fn v124(m: &mut Migrator) -> StepResult {
    m.set("Yaw VR", "auto_park", s("yes"))
}

fn v125(m: &mut Migrator) -> StepResult {
    m.set("APP", "pixel_ratio", 1.into())
}

fn v126(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[("gear_smoothing_factor", 0.into())])
}

fn v127(m: &mut Migrator) -> StepResult {
    m.set("Yaw VR", "center_before_start_alert", s("yes"))
}

fn v128(m: &mut Migrator) -> StepResult {
    m.add_games(&["fs2020"])
}

fn v129(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[("publish", false.into())])?;
    m.set("CUSTOM_FEATURES", "tuner_mode", s("no"))
}

fn v130(m: &mut Migrator) -> StepResult {
    m.set("Yaw VR", "udp_port", 33333.into())
}

/// IL-2 switches pitch and roll to angle mode with 60°/s rate ranges (game entry only).
fn v131(m: &mut Migrator) -> StepResult {
    m.edit_profile("il2", |il2| {
        let mut effects = nested(il2, "motion_effects")?;
        effects.insert("pitch_type", s("angle"))?;
        effects.insert("roll_type", s("angle"))?;
        let mut ranges = nested(il2, "max_telemetry_vars")?;
        ranges.insert("pitch_rate", 60.into())?;
        ranges.insert("roll_rate", 60.into())?;
        Ok(())
    })
}

fn v132(m: &mut Migrator) -> StepResult {
    m.set("Yaw VR", "device_id", s(""))
}

fn v133(m: &mut Migrator) -> StepResult {
    m.shortcut("telemetry_capture_keys", "ctrl+shift+f7")?;
    m.shortcut("telemetry_reset_keys", "ctrl+shift+f8")
}

fn v134(m: &mut Migrator) -> StepResult {
    m.set_all("MOTIONBOX", &[("sway_curving_factor", 1.into()), ("surge_curving_factor", 1.into())])
}

/// Versions 135 and 136 belong to 4D videos, which OpenRigCue does not have: nothing is added.
fn v135(_m: &mut Migrator) -> StepResult {
    Ok(())
}

fn v136(_m: &mut Migrator) -> StepResult {
    Ok(())
}

fn v137(m: &mut Migrator) -> StepResult {
    m.set("Yaw VR", "disable_two_way_communication", s("no"))
}

fn v138(m: &mut Migrator) -> StepResult {
    m.set("Yaw VR", "vibration_power", s("100"))
}

/// Creates PT-ACTUATOR and moves `real_heave` from MOTIONBOX to the manufacturer sections.
fn v139(m: &mut Migrator) -> StepResult {
    m.new_section("PT-ACTUATOR");
    m.set("PT-ACTUATOR", "thanos_com_port", s(""))?;
    m.copy_key(("PROSIMU", "smoothing_threshold"), ("PT-ACTUATOR", "smoothing_threshold"))?;
    m.copy_key(("PROSIMU", "soft_crash_threshold"), ("PT-ACTUATOR", "soft_crash_threshold"))?;
    m.set_all(
        "PT-ACTUATOR",
        &[
            ("boost_threshold", 5.into()),
            ("reverse_controller_port_5", s("no")),
            ("reverse_controller_port_6", s("no")),
            ("reverse_controller_port_7", s("no")),
        ],
    )?;
    for section in ["Yaw VR", "PROSIMU", "DOF REALITY"] {
        m.copy_key(("MOTIONBOX", "real_heave"), (section, "real_heave"))?;
    }
    m.set("PT-ACTUATOR", "real_heave", s("yes"))?;
    m.del("MOTIONBOX", "real_heave")
}

fn v140(m: &mut Migrator) -> StepResult {
    m.set_all(
        "MOTIONBOX",
        &[
            ("surge_smoothing_factor", 0.into()),
            ("sway_smoothing_factor", 0.into()),
            ("heave_smoothing_factor", 0.into()),
        ],
    )
}

fn smoothing_dict(entries: &[(&str, i64)]) -> LegacyValue {
    LegacyValue::Dict(entries.iter().map(|(k, v)| (Key::from(*k), LegacyValue::Int(*v))).collect())
}

fn v141(m: &mut Migrator) -> StepResult {
    for key in ["surge_smoothing_factor", "sway_smoothing_factor", "heave_smoothing_factor"] {
        m.del("MOTIONBOX", key)?;
    }
    let zero = smoothing_dict(&[("surge", 0), ("sway", 0), ("heave", 0), ("traction_loss", 0)]);
    m.set_every_tune(&[("motion_effect_smoothing_factor", zero)])
}

/// Adds roll and pitch smoothing; game level sets roll then pitch, vehicles pitch then roll.
fn v142(m: &mut Migrator) -> StepResult {
    m.each_tune(|_, tune, level| {
        let mut smoothing = nested(tune, "motion_effect_smoothing_factor")?;
        let order: [&str; 2] = if level == Level::Game { ["roll", "pitch"] } else { ["pitch", "roll"] };
        for key in order {
            smoothing.insert(key, 0.into())?;
        }
        Ok(())
    })
}

fn v143(m: &mut Migrator) -> StepResult {
    m.edit_profile("emulator", |emulator| {
        emulator.insert("motion_boost_factor", 1.into())?;
        emulator.insert("motion_smoothing_factor", 20.into())?;
        Ok(())
    })?;
    m.shortcut("telemetry_capture_btns", "")?;
    m.shortcut("telemetry_reset_btns", "")
}

fn v144(m: &mut Migrator) -> StepResult {
    m.edit_profile("emulator", |emulator| {
        let mut values = nested(emulator, "motion_effect_smoothing_factor")?;
        let mut cursor = values.cursor()?;
        while let Some(key) = cursor.next(&values)? {
            values.insert(key, 20.into())?;
        }
        Ok(())
    })
}

fn v145(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[
        ("motion_main_actuators_surge", 100.into()),
        ("motion_main_actuators_sway", 100.into()),
        ("motion_main_actuators_traction_loss", 0.into()),
    ])
}

fn v147(m: &mut Migrator) -> StepResult {
    m.set_all("PT-ACTUATOR", &[("lateral_distance_actuators", 0.into()), ("vertical_actuators_stroke", 0.into())])
}

fn v148(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[("motion_max_horizontal", 100.into())])
}

/// Yaw smoothing (20 on the emulator, 0 elsewhere) and the Thanos filter defaults.
fn v149(m: &mut Migrator) -> StepResult {
    m.each_tune(|game, tune, _| {
        let yaw: i64 = if game == "emulator" { 20 } else { 0 };
        nested(tune, "motion_effect_smoothing_factor")?.insert("yaw", yaw.into())?;
        tune.insert("thanos_spike_filter_horizontal", 1800.into())?;
        tune.insert("thanos_spike_filter_vertical", 1800.into())?;
        tune.insert("thanos_smoothing_factor", 0.into())?;
        Ok(())
    })
}

fn v150(m: &mut Migrator) -> StepResult {
    m.add_master_vehicle("api", "Torque Drift")
}

fn v151(m: &mut Migrator) -> StepResult {
    m.set("NETWORK_PORT", "bussim", s("37337"))?;
    m.add_games(&["fernbus", "touristbus"])
}

fn v152(m: &mut Migrator) -> StepResult {
    m.set("NETWORK_PORT", "driftphysicscrew", s("20782"))?;
    m.add_games(&["rds", "furidashi"])
}

/// On a first installation: boost 5, smoothing 20 everywhere, and Jeff Weaver's published
/// `max_telemetry_vars` (expert tune sets) for the games and vehicles he published.
fn v153(m: &mut Migrator) -> StepResult {
    m.set("APP", "force_manual_quit", s("no"))?;
    if !m.first_install_literal() {
        return Ok(());
    }
    let expert = m.src.expert_tunes;
    // A `publish` flag that cannot be read counts as false; `max_telemetry_vars` is then
    // required (a published tune without it fails the step).
    let jeff_tune = |game: &str, vehicle: Option<&Key>| -> Result<Option<LegacyValue>, String> {
        let published = (|| {
            let mut tune = expert.get("jeffweaver")?.item_owned("db").ok()?.item_owned(game).ok()?;
            if let Some(vehicle) = vehicle {
                tune = tune.item_owned("vehicles").ok()?.lookup_owned(&vehicle.to_value()).ok()?;
            }
            tune.item_owned("publish").ok()?.is_truthy().then_some(tune)
        })();
        match published {
            Some(tune) => tune.item_owned("max_telemetry_vars").map(Some).map_err(|_| key_error("max_telemetry_vars")),
            None => Ok(None),
        }
    };
    let smoothing = smoothing_dict(&[
        ("surge", 20),
        ("sway", 20),
        ("heave", 20),
        ("traction_loss", 20),
        ("pitch", 20),
        ("roll", 20),
        ("yaw", 20),
    ]);
    for row in m.profile_rows()? {
        let (game, value) = row?;
        let game_id = game.to_text();
        let mut value = value.clone();
        let mut tune = as_dict(&mut value, &game_id)?;
        tune.insert("motion_boost_factor", 5.into())?;
        tune.insert("motion_effect_smoothing_factor", smoothing.clone())?;
        if let Some(ranges) = jeff_tune(&game_id, None)? {
            tune.insert("max_telemetry_vars", ranges)?;
        }
        let mut vehicles = vehicles_of(&mut tune)?;
        let mut cursor = vehicles.cursor()?;
        while let Some(vehicle) = cursor.next(&vehicles)? {
            let mut vehicle_tune = vehicles.child(&vehicle)?;
            vehicle_tune.insert("motion_effect_smoothing_factor", smoothing.clone())?;
            if let Some(ranges) = jeff_tune(&game_id, Some(&vehicle))? {
                vehicle_tune.insert("max_telemetry_vars", ranges)?;
            }
        }
        m.assign_profile(&game_id, value)?;
    }
    m.commit_profiles()
}

fn v154(m: &mut Migrator) -> StepResult {
    m.add_games(&["rds", "furidashi"])
}

fn v155(m: &mut Migrator) -> StepResult {
    m.set("PT-ACTUATOR", "send_spike_filter", s("yes"))
}

fn v156(m: &mut Migrator) -> StepResult {
    m.shortcut("export_csv_btns", "")?;
    m.shortcut("export_csv_keys", "ctrl+shift+f9")
}

fn v157(m: &mut Migrator) -> StepResult {
    m.set_every_nested(&[("shaker_effects", "wheel_slip", 10.into()), ("motion_effects", "wheel_slip", 4.into())])
}

fn v158(m: &mut Migrator) -> StepResult {
    m.derive_layout_effects(&[
        ("wheel_slip_lf", "suspension_lf"),
        ("wheel_slip_lr", "suspension_lr"),
        ("wheel_slip_rr", "suspension_rr"),
        ("wheel_slip_rf", "suspension_rf"),
    ])?;
    m.commit_profiles()
}

fn switch_dict(keys: &[&str]) -> LegacyValue {
    LegacyValue::Dict(keys.iter().map(|k| (Key::from(*k), LegacyValue::Bool(true))).collect())
}

fn v159(m: &mut Migrator) -> StepResult {
    let motion = switch_dict(&[
        "wheel_slip",
        "pitch",
        "roll",
        "yaw",
        "sway",
        "surge_acceleration",
        "surge_deceleration",
        "heave",
        "traction_loss",
    ]);
    let shaker = switch_dict(&[
        "wheel_slip",
        "road_bump",
        "suspension",
        "engine",
        "gear",
        "impact",
        "deceleration",
        "acceleration",
        "lateral_acceleration",
        "traction_loss",
    ]);
    m.set_every_tune(&[("motion_switch", motion), ("shaker_switch", shaker)])
}

fn v160(m: &mut Migrator) -> StepResult {
    m.set("PT-ACTUATOR", "thanos_controller", s("AMC-AASD15A"))
}

/// Belt tensioner share 50 and the `2 Surge` → `Surge + Belt` model rename.
fn v161(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[("motion_belt_tensioner", 50.into())])?;
    let model_value = m.get("MOTIONBOX", "model")?;
    let model = model_value.python_string().ok_or("MOTIONBOX.model is not a string")?;
    let needle: Vec<u32> = "2 Surge".chars().map(u32::from).collect();
    let replacement: Vec<u32> = "Surge + Belt".chars().map(u32::from).collect();
    let mut result = Vec::new();
    let mut at = 0;
    while at < model.len() {
        if model.codepoints()[at..].starts_with(&needle) {
            result.extend_from_slice(&replacement);
            at += needle.len();
        } else {
            result.push(model.codepoints()[at]);
            at += 1;
        }
    }
    m.set(
        "MOTIONBOX",
        "model",
        LegacyValue::from_python_string(
            PythonString::from_codepoints(result).expect("original and literal codepoints"),
        ),
    )
}

fn v162(m: &mut Migrator) -> StepResult {
    m.edit_profile("emulator", |emulator| {
        nested(emulator, "max_telemetry_vars")?.insert("heave", 5.into())?;
        Ok(())
    })
}

fn v163(m: &mut Migrator) -> StepResult {
    m.copy_key(("SHORTCUTS", "export_csv_btns"), ("SHORTCUTS", "telemetry_snapshot_btns"))?;
    m.copy_key(("SHORTCUTS", "export_csv_keys"), ("SHORTCUTS", "telemetry_snapshot_keys"))?;
    m.del("SHORTCUTS", "export_csv_btns")?;
    m.del("SHORTCUTS", "export_csv_keys")?;
    m.shortcut("telemetry_export_btns", "")?;
    m.shortcut("telemetry_export_keys", "ctrl+shift+f10")
}

fn v164(m: &mut Migrator) -> StepResult {
    m.set("NETWORK_PORT", "hyperstorm", 20741.into())?;
    m.add_games(&["hyperstorm"])
}

fn v165(m: &mut Migrator) -> StepResult {
    for axis in ["pitch", "roll", "yaw", "surge", "sway", "heave", "traction_loss"] {
        m.set("MOTIONBOX", &format!("{axis}_clip_factor"), 1000.into())?;
    }
    Ok(())
}

fn v166(m: &mut Migrator) -> StepResult {
    let shaker: Vec<String> = m.src.registry.shaker_effects.iter().map(|e| e.id.clone()).collect();
    let motion: Vec<String> = m.src.registry.motion_effects.iter().map(|e| e.id.clone()).collect();
    for id in &shaker {
        m.up_down_shortcuts("shake_box_", id)?;
    }
    for id in &motion {
        m.up_down_shortcuts("motion_box_", id)?;
    }
    Ok(())
}

fn v167(m: &mut Migrator) -> StepResult {
    m.set_every_nested(&[
        ("motion_effects", "rumble_strips", 4.into()),
        ("motion_switch", "rumble_strips", true.into()),
    ])
}

/// Note the trailing space in the first key; it is removed again at v174.
fn v168(m: &mut Migrator) -> StepResult {
    m.shortcut("motion_box_rumble_strips_up_btns ", "")?;
    m.shortcut("motion_box_rumble_strips_up_keys", "")?;
    m.shortcut("motion_box_rumble_strips_down_btns", "")?;
    m.shortcut("motion_box_rumble_strips_down_keys", "")
}

/// v169 and v171 both reset Dirt Showdown to its master profile.
fn v169(m: &mut Migrator) -> StepResult {
    m.add_games(&["dirtshowdown"])
}

fn v170(m: &mut Migrator) -> StepResult {
    m.set("MOTIONBOX", "intelli_tune_capture_secs", 180.into())?;
    m.set("SHAKEBOX", "enable_test_effect", s("no"))
}

fn v172(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "check_ed_updates", s("yes"))
}

fn v173(m: &mut Migrator) -> StepResult {
    for key in [
        "motion_max_power",
        "motion_max_horizontal",
        "motion_smoothing_factor",
        "reaction_speed",
        "motion_boost_factor",
    ] {
        m.up_down_shortcuts("motion_box_", key)?;
    }
    Ok(())
}

/// Assigns the typo key to itself and deletes it, i.e. drops it (missing key ignored).
fn v174(m: &mut Migrator) -> StepResult {
    m.try_del("SHORTCUTS", "motion_box_rumble_strips_up_btns ");
    Ok(())
}

fn v175(m: &mut Migrator) -> StepResult {
    for axis in ["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss"] {
        m.up_down_shortcuts("motion_box_smoothing_", axis)?;
    }
    Ok(())
}

fn v176(m: &mut Migrator) -> StepResult {
    m.add_games(&["trackmania2", "trackmaniaturbo"])
}

fn v177(m: &mut Migrator) -> StepResult {
    m.set_all("APP", &[("window_x_pos", (-1).into()), ("window_y_pos", (-1).into())])
}

fn v178(m: &mut Migrator) -> StepResult {
    m.new_section("MONITORS");
    Ok(())
}

fn v179(m: &mut Migrator) -> StepResult {
    m.set("Yaw VR", "6dof_mode", s("yes"))
}

fn compensation_axis() -> LegacyValue {
    let mut axis = Dict::new();
    axis.insert("offset", 0.into());
    axis.insert("enabled", true.into());
    axis.insert("reverse", false.into());
    LegacyValue::Dict(axis)
}

fn v180(m: &mut Migrator) -> StepResult {
    m.new_section("MOTION_COMPENSATION");
    for axis in ["yaw", "roll", "pitch"] {
        m.shortcut(&format!("motion_compensation_{axis}_switch_keys"), "")?;
        m.shortcut(&format!("motion_compensation_{axis}_switch_btns"), "")?;
        m.set("MOTION_COMPENSATION", axis, compensation_axis())?;
    }
    Ok(())
}

fn v181(m: &mut Migrator) -> StepResult {
    m.set_all("MOTION_COMPENSATION", &[("refresh_rate", 90.into()), ("com_port", s(""))])
}

fn v182(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_dash_telemetry", s("no"))
}

fn v183(m: &mut Migrator) -> StepResult {
    for axis in ["yaw", "pitch", "roll", "heave", "surge", "sway"] {
        m.shortcut(&format!("motion_compensation_{axis}_switch_keys"), "")?;
        m.shortcut(&format!("motion_compensation_{axis}_switch_btns"), "")?;
        m.set("MOTION_COMPENSATION", axis, compensation_axis())?;
    }
    m.shortcut("motion_compensation_switch_keys", "")?;
    m.shortcut("motion_compensation_switch_btns", "")?;
    m.set("MOTION_COMPENSATION", "height", 0.into())
}

fn v184(m: &mut Migrator) -> StepResult {
    m.set_all("MOTION_COMPENSATION", &[("swap_roll_pitch", s("no")), ("installation", s("H"))])
}

fn v185(m: &mut Migrator) -> StepResult {
    m.set("MOTION_COMPENSATION", "auto_search", s("yes"))
}

fn v186(m: &mut Migrator) -> StepResult {
    m.set_all("MOTION_COMPENSATION", &[("sway_factor", 100.into()), ("surge_factor", 100.into())])
}

fn v187(m: &mut Migrator) -> StepResult {
    m.set("MOTION_COMPENSATION", "height", 1000.into())
}

fn v190(m: &mut Migrator) -> StepResult {
    m.add_games(&["spacemonkey"])
}

/// Setting the `spacemonkey` type to `racing` would edit a decoded copy that is never stored: no effect.
fn v191(m: &mut Migrator) -> StepResult {
    m.profile("spacemonkey")?;
    m.commit_profiles()
}

/// Per-effect shaker DSP settings (filter type 0, cutoff 400, resonance 35 for engine, 10 otherwise).
fn v196(m: &mut Migrator) -> StepResult {
    const EFFECTS: [&str; 10] = [
        "wheel_slip",
        "road_bump",
        "suspension",
        "engine",
        "gear",
        "impact",
        "deceleration",
        "acceleration",
        "lateral_acceleration",
        "traction_loss",
    ];
    m.each_tune(|_, tune, _| {
        let mut dsp = Dict::new();
        let mut cutoff = Dict::new();
        let mut resonance = Dict::new();
        for effect in EFFECTS {
            dsp.insert(effect, 0.into());
            cutoff.insert(effect, 400.into());
            resonance.insert(effect, LegacyValue::Int(if effect == "engine" { 35 } else { 10 }));
        }
        tune.insert("shaker_dsp_type", dsp.into())?;
        tune.insert("shaker_cutoff", cutoff.into())?;
        tune.insert("shaker_resonance", resonance.into())?;
        Ok(())
    })
}

fn v197(m: &mut Migrator) -> StepResult {
    m.set_every_nested(&[
        ("shaker_cutoff", "engine_fadeoff_start", 95.into()),
        ("shaker_cutoff", "engine_fadeoff_end", 100.into()),
    ])
}

fn v198(m: &mut Migrator) -> StepResult {
    if m.windows() {
        m.skipped("copy WinSparkle registry values");
    }
    Ok(())
}

fn v199(m: &mut Migrator) -> StepResult {
    m.add_games(&["falconbms", "redrover", "f12012", "f12011"])?;
    m.set("NETWORK_PORT", "redrover", s("3001"))
}

/// Flight shaker effects; built-in layouts take their levels from the shipped flight shaker levels.
fn v205(m: &mut Migrator) -> StepResult {
    m.each_tune_pending(|_, tune, _| {
        for effect in ["afterburner", "cannon_firing", "landing_gear", "canopy"] {
            add_shaker_effect(tune, effect)?;
        }
        Ok(())
    })?;
    let flight = m.src.flight_shaker_levels;
    m.each_layout_channel_named(|layout, channel, sfx| {
        for effect in
            ["afterburner", "cannon_firing", "landing_gear_left", "landing_gear_right", "landing_gear_nose", "canopy"]
        {
            let level = if DERIVED_LAYOUTS.contains(&layout) {
                flight_level(flight, layout, effect, channel)?
            } else {
                0.into()
            };
            sfx.insert(effect, level)?;
        }
        Ok(())
    })?;
    m.commit_profiles()
}

/// The level of `effect` on `channel` in a built-in
/// layout. 0 when the layout has no row for the effect; the first row naming the effect must hold
/// the channel (otherwise the migration fails).
pub(super) fn flight_level(table: &Dict, layout: &str, effect: &str, channel: &str) -> Result<LegacyValue, String> {
    let Some(rows) = table.get(layout) else { return Ok(0.into()) };
    if rows.type_name() != "list" {
        return Err(format!("TypeError: shaker levels of {layout:?} are a {}, not a list", rows.type_name()));
    }
    let mut index = 0;
    while index < rows.py_len().expect("list length") {
        let row = rows.get_index(index)?;
        index += 1;
        if row.type_name() != "dict" {
            return Err(format!("TypeError: shaker level row is a {}", row.type_name()));
        }
        let found = row.item_owned("effect").map_err(|_| key_error("effect"))?;
        if found.python_string().is_some_and(|text| text == effect) {
            return row.item_owned(channel).map_err(|_| key_error(channel));
        }
    }
    Ok(0.into())
}

impl Migrator<'_, '_> {
    /// Like [`Migrator::each_layout_channel`] but also passes the channel name.
    fn each_layout_channel_named(
        &mut self,
        mut f: impl FnMut(&str, &str, &mut DictView<'_>) -> StepResult,
    ) -> StepResult {
        for row in self.metadata_rows("shakers")? {
            let (layout, value) = row?;
            let name = layout.to_text();
            let mut value = value;
            let mut layout = as_dict(&mut value, &name)?;
            let mut cursor = layout.cursor()?;
            while let Some(channel) = cursor.next(&layout)? {
                let channel_name = channel.to_text();
                let mut row = layout.child(&channel)?;
                let mut sfx = nested(&mut row, "sfx")?;
                f(&name, &channel_name, &mut sfx)?;
            }
            self.assign_layout(&name, value)?;
        }
        Ok(())
    }
}

fn v206(m: &mut Migrator) -> StepResult {
    m.each_tune_pending(|_, tune, _| add_shaker_effect(tune, "missile_firing"))?;
    m.derive_layout_effects(&[("missile_firing", "cannon_firing")])?;
    m.commit_profiles()
}

/// `cannon_firing_btns ` has a trailing space (fixed at v237).
fn v207(m: &mut Migrator) -> StepResult {
    m.shortcut("cannon_firing_btns ", "")?;
    m.shortcut("cannon_firing_keys", "")?;
    m.shortcut("missile_firing_btns", "")?;
    m.shortcut("missile_firing_keys", "")
}

fn v208(m: &mut Migrator) -> StepResult {
    m.each_tune(|_, tune, _| {
        for effect in ["afterburner", "cannon_firing", "landing_gear", "missile_firing", "canopy"] {
            nested(tune, "motion_effects")?.insert(effect, 4.into())?;
            nested(tune, "motion_switch")?.insert(effect, true.into())?;
        }
        Ok(())
    })
}

/// Drops the per-profile `type` key (for the two TrackMania games the key `racing` instead); on a
/// first installation the Dirt port becomes 30777.
fn v209(m: &mut Migrator) -> StepResult {
    if m.first_install_literal() {
        m.set("NETWORK_PORT", "dirt", s("30777"))?;
    }
    m.each_tune(|game, tune, _| {
        let key = if game == "trackmania2" || game == "trackmaniaturbo" { "racing" } else { "type" };
        let _ = tune.remove(key);
        Ok(())
    })
}

fn v210(m: &mut Migrator) -> StepResult {
    m.add_games(&["acudp"])?;
    m.set("NETWORK_PORT", "acudp", s("9997"))
}

/// Removes `<docs>/crash.txt`; a failure is ignored.
fn v211(m: &mut Migrator) -> StepResult {
    match m.src.effects {
        Some(effects) => {
            let _ = std::fs::remove_file(effects.docs.join("crash.txt"));
        }
        None => m.skipped("delete <docs>/crash.txt"),
    }
    Ok(())
}

fn v212(m: &mut Migrator) -> StepResult {
    for key in ["afterburner", "cannon_firing", "canopy", "landing_gear", "missile_firing"] {
        for prefix in ["motion_box_", "shake_box_"] {
            m.up_down_shortcuts(prefix, key)?;
        }
    }
    Ok(())
}

fn v213(m: &mut Migrator) -> StepResult {
    if m.first_install_literal() {
        m.set("NETWORK_PORT", "dirt", s("30778"))?;
    }
    Ok(())
}

/// When the legacy DCS patch is installed and applied: removes `~/Saved Games/DCS/Scripts/Export.lua`
/// (errors ignored), then, when that file is gone and the current DCS patch is not applied, applies it (its
/// result ignored). A failing patch-status check fails the step. The ports the patches use
/// are the `[NETWORK_PORT]` values of the `config.ini` being migrated.
fn v214(m: &mut Migrator) -> StepResult {
    let Some(context) = m.src.effects.and_then(|effects| effects.patch) else {
        m.skipped("re-patch DCS Export.lua");
        return Ok(());
    };
    let config = &m.state.config;
    let ports = |key: PortKey| {
        let section = config.get("NETWORK_PORT")?;
        u16::try_from(section.item_owned(key.config_key()).ok()?.py_int()?).ok()
    };
    let loaded_ports = ModulePorts::at_startup(&ports);
    let context = PatchContext {
        prefixes: context.prefixes,
        steam: context.steam,
        library: context.library,
        ports: &ports,
        loaded_ports: &loaded_ports,
    };
    let legacy = Patcher::Template(&DCS_LEGACY);
    let legacy_env = context.environment_for(&legacy);
    if !legacy.installed(&legacy_env) || !legacy.patched(&legacy_env).map_err(|e| e.to_string())? {
        return Ok(());
    }
    let Some(prefix) = legacy_env.prefix else { return Ok(()) };
    let export_lua = UserPath::new(UserFolder::Profile, "Saved Games\\DCS\\Scripts\\Export.lua").resolve(prefix);
    let _ = std::fs::remove_file(&export_lua);
    let dcs = GAME_ENTRIES
        .iter()
        .find(|entry| entry.key == "dcs")
        .and_then(|entry| entry.kind.patcher())
        .ok_or_else(|| "no dcs patch module".to_owned())?;
    let dcs_env = context.environment_for(dcs);
    if !export_lua.is_file() && !dcs.patched(&dcs_env).map_err(|e| e.to_string())? {
        let _ = dcs.patch(&dcs_env);
    }
    Ok(())
}

/// Drops the retired `ccd` game; IntelliTune preference is `''` on games but `None` on vehicles.
fn v215(m: &mut Migrator) -> StepResult {
    let _ = m.delete_game("ccd"); // a missing row is ignored
    m.each_tune(|_, tune, level| {
        let value = if level == Level::Game { s("") } else { LegacyValue::None };
        tune.insert("intellitune_motion_preference", value)?;
        Ok(())
    })
}

fn v216(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "hide_effects_not_in_game", s("no"))
}

fn v217(m: &mut Migrator) -> StepResult {
    m.set("APP", "enable_cloud_backup", s("yes"))
}

fn v218(m: &mut Migrator) -> StepResult {
    m.set_every_nested(&[("motion_effects", "engine", 1.into()), ("motion_switch", "engine", false.into())])
}

fn v219(m: &mut Migrator) -> StepResult {
    m.set("LED", "model", s("RGB"))
}

/// `motion_box_engine_up_btns ` has a trailing space (fixed at v237).
fn v220(m: &mut Migrator) -> StepResult {
    m.shortcut("motion_box_engine_up_btns ", "")?;
    m.shortcut("motion_box_engine_up_keys", "")?;
    m.shortcut("motion_box_engine_down_btns", "")?;
    m.shortcut("motion_box_engine_down_keys", "")
}

fn v221(m: &mut Migrator) -> StepResult {
    m.set("MOTIONBOX", "auto_search", s("yes"))
}

/// Re-keys flight-sim vehicles that contain a NUL: the key loses its trailing NULs. A key with an
/// interior NUL but no trailing one maps to itself and is therefore deleted.
fn v222(m: &mut Migrator) -> StepResult {
    m.rekey_profile_vehicles(
        |game| ["fs2020", "xplane", "p3d", "fsx"].contains(&game),
        |name| {
            name.codepoints().contains(&0).then(|| {
                let end = name.codepoints().iter().rposition(|&v| v != 0).map_or(0, |v| v + 1);
                PythonString::from_codepoints(name.codepoints()[..end].to_vec()).expect("original codepoints")
            })
        },
    )
}

/// rFactor 2 vehicle keys are cut before the last `#`, dropping the character before it too.
fn v223(m: &mut Migrator) -> StepResult {
    m.rekey_profile_vehicles(
        |game| game == "rf2",
        |name| {
            name.codepoints().iter().rposition(|&point| point == u32::from('#')).map(|index| {
                let end = if index == 0 { name.len().saturating_sub(1) } else { index - 1 };
                PythonString::from_codepoints(name.codepoints()[..end].to_vec()).expect("original codepoints")
            })
        },
    )
}

/// Re-keys every vehicle: the tune is stored under the new key, then the old key is deleted.
fn rekey_vehicles(vehicles: &mut DictView<'_>, new_key: impl Fn(&PythonString) -> Option<PythonString>) -> StepResult {
    // The keys are snapshotted before the loop, so re-keyed entries are not visited again.
    for key in vehicles.keys()? {
        let name =
            key.to_value().python_string().ok_or_else(|| "vehicle key does not support string search".to_owned())?;
        if let Some(new) = new_key(&name) {
            let Some(value) = vehicles.lookup(&key) else { continue };
            vehicles.insert(Key::from_value(&LegacyValue::from_python_string(new))?, value)?;
            vehicles.remove_key(&key)?;
        }
    }
    Ok(())
}

fn v224(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "smoothing_slider_step", 1.into())
}

fn v225(m: &mut Migrator) -> StepResult {
    m.set_all("MOTIONBOX", &[("soft_start_stop_factor", 90.into()), ("soft_start_stop_timer", 4.into())])
}

fn v226(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "show_debug_page", s("no"))
}

fn v227(m: &mut Migrator) -> StepResult {
    m.add_games(&["f12021"])?;
    m.set("NETWORK_PORT", "f12021", s("20783"))
}

fn v228(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_belt_tensioner", s("no"))
}

fn v229(m: &mut Migrator) -> StepResult {
    m.new_section("BELT_TENSIONER");
    m.set_all("BELT_TENSIONER", &[("refresh_rate", 90.into()), ("auto_search", s("yes")), ("com_port", s(""))])
}

/// Windows: no analytics id is written; on a first installation the sound
/// card and platform model found in the registry
/// are taken, any failure being ignored. Other platforms: a fresh analytics id and, on a
/// first installation, the magicbox hardware defaults (CM106 sound card, SRS UShake6 layout, H3
/// platform at its manufacturer's rate).
fn v230(m: &mut Migrator) -> StepResult {
    if m.windows() {
        m.skipped("registry UUID (analytics id, not ported)");
        // Every failure in this block is ignored.
        let _ = (|| -> StepResult {
            if !m.first_install_bool()? {
                return Ok(());
            }
            if let Some(card) = m.get_reg("Soundcard") {
                m.set("SHAKEBOX", "soundcard", card)?;
            }
            if let Some(model) = m.get_reg("MotionModel") {
                let name = model.python_string().and_then(|text| text.scalar_string());
                if let Some(manufacturer) = name.as_deref().and_then(|name| m.src.registry.manufacturer_of(name)) {
                    let rate = m.src.registry.manufacturer(manufacturer, true).map(|info| info.default_refresh_rate);
                    m.set("MOTIONBOX", "model", model)?;
                    if let Some(rate) = rate {
                        m.set("MOTIONBOX", "refresh_rate", i64::from(rate).into())?;
                    }
                }
            }
            Ok(())
        })();
        return Ok(());
    }
    m.set("APP", "uuid", s(""))?;
    m.skipped("generate APP.uuid (analytics id, not ported)");
    if m.first_install_bool()? {
        m.set("SHAKEBOX", "soundcard", s("CM106 Like Sound Device Analog Surround 5.1"))?;
        m.set("SHAKEBOX", "profile", s("SRS UShake6"))?;
        let model = "H3";
        if let Some(manufacturer) = m.src.registry.manufacturer_of(model) {
            let rate = m
                .src
                .registry
                .manufacturer(manufacturer, false)
                .map(|info| info.default_refresh_rate)
                .ok_or_else(|| key_error(manufacturer))?;
            m.set("MOTIONBOX", "model", s(model))?;
            m.set("MOTIONBOX", "refresh_rate", i64::from(rate).into())?;
        }
    }
    Ok(())
}

/// Soft start/stop moves to the manufacturer sections (DOF Reality 30/4, others copy MOTIONBOX).
fn v232(m: &mut Migrator) -> StepResult {
    for section in ["PT-ACTUATOR", "Yaw VR", "PROSIMU", "DOF REALITY"] {
        if section == "DOF REALITY" {
            m.set_all(section, &[("soft_start_stop_factor", 30.into()), ("soft_start_stop_timer", 4.into())])?;
        } else {
            m.copy_key(("MOTIONBOX", "soft_start_stop_factor"), (section, "soft_start_stop_factor"))?;
            m.copy_key(("MOTIONBOX", "soft_start_stop_timer"), (section, "soft_start_stop_timer"))?;
        }
    }
    m.del("MOTIONBOX", "soft_start_stop_factor")?;
    m.del("MOTIONBOX", "soft_start_stop_timer")
}

fn v233(m: &mut Migrator) -> StepResult {
    m.set_all("BELT_TENSIONER", &[("retraction_power", 0.into()), ("start_on_auto_mode", s("no"))])?;
    m.set_every_tune(&[
        ("belt_sway_curving_enabled", true.into()),
        ("belt_surge", 5.into()),
        ("belt_sway_curving_factor", 5.into()),
    ])
}

/// ABS lock: motion level 4, shaker defaults, shortcuts, and per-speaker levels copied from `brake`.
fn v234(m: &mut Migrator) -> StepResult {
    m.each_tune_pending(|_, tune, _| {
        nested(tune, "motion_effects")?.insert("abs_lock", 4.into())?;
        nested(tune, "motion_switch")?.insert("abs_lock", true.into())?;
        add_shaker_effect(tune, "abs_lock")
    })?;
    m.up_down_shortcuts("motion_box_", "abs_lock")?;
    m.up_down_shortcuts("shake_box_", "abs_lock")?;
    m.derive_layout_effects(&[("abs_lock", "brake")])?;
    m.commit_profiles()
}

/// THANOS starts as a copy of PT-ACTUATOR.
fn v235(m: &mut Migrator) -> StepResult {
    m.copy_section("PT-ACTUATOR", "THANOS")
}

fn v236(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "surge_when_pitch_joy", s("no"))
}

/// Belt shortcuts, and the two trailing-space typo keys of v207/v220 moved to their proper names.
fn v237(m: &mut Migrator) -> StepResult {
    m.shortcut("belt_switch_btns", "")?;
    m.shortcut("belt_switch_keys", "ctrl+shift+f11")?;
    m.copy_key(("SHORTCUTS", "motion_box_engine_up_btns "), ("SHORTCUTS", "motion_box_engine_up_btns"))?;
    m.copy_key(("SHORTCUTS", "cannon_firing_btns "), ("SHORTCUTS", "cannon_firing_btns"))?;
    m.del("SHORTCUTS", "motion_box_engine_up_btns ")?;
    m.del("SHORTCUTS", "cannon_firing_btns ")
}

/// Belt smoothing: 20 on games but 5 on vehicles.
fn v238(m: &mut Migrator) -> StepResult {
    m.each_tune(|_, tune, level| {
        let value: i64 = if level == Level::Game { 20 } else { 5 };
        tune.insert("belt_surge_smoothing", value.into())?;
        tune.insert("belt_sway_smoothing", value.into())?;
        Ok(())
    })
}

fn v239(m: &mut Migrator) -> StepResult {
    m.shortcut("belt_homing_btns", "")?;
    m.shortcut("belt_homing_keys", "ctrl+shift+f12")
}

fn v240(m: &mut Migrator) -> StepResult {
    for id in ["belt_surge", "belt_surge_smoothing", "belt_sway_curving", "belt_sway_smoothing"] {
        m.up_down_shortcuts("", id)?;
    }
    // Deleting `retraction_power` then `start_on_auto_mode` stops at the first missing key.
    if m.section("BELT_TENSIONER").is_ok_and(|mut b| b.remove("retraction_power").ok().flatten().is_some()) {
        m.try_del("BELT_TENSIONER", "start_on_auto_mode");
    }
    m.set_every_tune(&[("belt_surge", 10.into()), ("belt_surge_smoothing", 15.into())])
}

fn v241(m: &mut Migrator) -> StepResult {
    m.add_games(&["thebus"])
}

fn v242(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[("belt_surge_enabled", true.into()), ("belt_sway_curving_reverse", false.into())])
}

fn v243(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "sway_when_roll_joy", s("no"))
}

fn v244(m: &mut Migrator) -> StepResult {
    m.new_section("PORT_FORWARDING");
    Ok(())
}

fn v245(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[
        ("belt_sway_smoothing", 15.into()),
        ("belt_sway_curving_factor", 10.into()),
        ("belt_sway_curving_reverse", false.into()),
        ("belt_sway_curving_enabled", true.into()),
    ])?;
    m.try_del("CUSTOM_FEATURES", "enable_belt_tensioner");
    Ok(())
}

fn v246(m: &mut Migrator) -> StepResult {
    m.set("BELT_TENSIONER", "belt_calibration_factor", 1.18.into())
}

fn v247(m: &mut Migrator) -> StepResult {
    m.set("BELT_TENSIONER", "belt_calibration_factor", 50.into())
}

fn v248(m: &mut Migrator) -> StepResult {
    m.set("APP", "voice_notifications", s("no"))
}

fn v249(m: &mut Migrator) -> StepResult {
    m.each_selected_tune(
        |game| game == "projectcars" || game == "projectcarsudp",
        |game, tune, _| {
            if game == "projectcars" || game == "projectcarsudp" {
                let mut ranges = nested(tune, "max_telemetry_vars")?;
                ranges.insert("heave", 1.into())?;
                ranges.insert("sway", 22.into())?;
                let mut effects = nested(tune, "motion_effects")?;
                effects.insert("heave", 14.into())?;
                effects.insert("sway", 14.into())?;
            }
            Ok(())
        },
    )
}

fn v250(m: &mut Migrator) -> StepResult {
    m.add_games(&["forzahorizon5"])
}

fn v251(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[
        ("belt_heave_smoothing", 15.into()),
        ("belt_heave_factor", 10.into()),
        ("belt_heave_reverse", false.into()),
        ("belt_heave_enabled", true.into()),
    ])
}

fn v252(m: &mut Migrator) -> StepResult {
    m.set_all("MOTION_COMPENSATION", &[("surge_horizontal_factor", 0.into()), ("surge_horizontal_stroke", 100.into())])
}

fn v253(m: &mut Migrator) -> StepResult {
    m.up_down_shortcuts("", "belt_heave")?;
    m.up_down_shortcuts("", "belt_heave_smoothing")
}

fn v254(m: &mut Migrator) -> StepResult {
    m.set("MOTION_COMPENSATION", "surge_horizontal_reverse", false.into())
}

/// Aces High is created from the Silent Wings master profile.
fn v255(m: &mut Migrator) -> StepResult {
    m.set("NETWORK_PORT", "aceshigh", 556.into())?;
    m.add_game("aceshigh", "silentwings")
}

fn v256(m: &mut Migrator) -> StepResult {
    m.set("PROSIMU", "scn6_protocol", s("D"))
}

fn v257(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[
        ("belt_surge_enabled", true.into()),
        ("belt_surge", 10.into()),
        ("belt_surge_smoothing", 15.into()),
    ])
}

fn v258(m: &mut Migrator) -> StepResult {
    m.edit_profile("emulator", |emulator| {
        let mut ranges = nested(emulator, "max_telemetry_vars")?;
        ranges.insert("pitch", 5.into())?;
        ranges.insert("roll", 5.into())?;
        ranges.insert("heave", 3.into())?;
        Ok(())
    })
}

fn v259(m: &mut Migrator) -> StepResult {
    m.set("BELT_TENSIONER", "belt_calibration_retraction", 50.into())
}

fn v260(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_hud", s("no"))
}

fn v261(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "allow_edit_emulator", s("no"))?;
    m.edit_profile("emulator", |emulator| {
        let mut smoothing = nested(emulator, "motion_effect_smoothing_factor")?;
        for key in ["surge", "sway", "heave", "traction_loss", "roll", "pitch", "yaw"] {
            smoothing.insert(key, 100.into())?;
        }
        emulator.insert("thanos_spike_filter_horizontal", 255.into())?;
        emulator.insert("thanos_spike_filter_vertical", 255.into())?;
        Ok(())
    })
}

/// Splits the `surge` range into acceleration and deceleration ranges. Each part ignores its
/// own failure, so a tune without `max_telemetry_vars` or `surge` is left alone.
fn v262(m: &mut Migrator) -> StepResult {
    m.each_tune(|_, tune, _| {
        for target in ["surge_acceleration", "surge_deceleration"] {
            let _ = (|| -> StepResult {
                let source = nested(tune, "max_telemetry_vars")?.get_result("surge")?;
                nested(tune, "max_telemetry_vars")?.insert(target, source)
            })();
        }
        let _ = (|| -> StepResult {
            nested(tune, "max_telemetry_vars")?.remove("surge")?;
            Ok(())
        })();
        Ok(())
    })
}

fn v263(m: &mut Migrator) -> StepResult {
    m.set_all(
        "MOTIONBOX",
        &[
            ("auto_smoothing_low_speed", s("no")),
            ("auto_smoothing_speed", 1.66.into()),
            ("auto_smoothing_slider", 60.into()),
        ],
    )
}

fn v264(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_dyadic_surge", s("no"))?;
    let source = m.metadata_row("shakers", "6 Speakers - 5.1")?.ok_or_else(|| key_error("6 Speakers - 5.1"))?;
    m.assign_layout("SRS UShake6", source)?;
    m.commit_profiles()
}

/// When the motion refresh rate is still 30 Hz, use the manufacturer's default rate (DOF Reality
/// and None keep 30). A model that is missing (no `MOTIONBOX.model`, no `MOTIONBOX` section), not
/// text, or unknown counts as manufacturer `None`.
fn v265(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_legacy_gear_smoothing", s("no"))?;
    let registry = m.src.registry;
    let model = m.get("MOTIONBOX", "model").ok().and_then(|v| v.python_string()).and_then(|s| s.scalar_string());
    let manufacturer =
        model.as_deref().and_then(|model| registry.manufacturer_of(model)).or_else(|| registry.manufacturer_of("None"));
    let manufacturer = manufacturer.ok_or_else(|| key_error("None"))?.to_owned();
    if m.int_of("MOTIONBOX", "refresh_rate")? == BigInt::from(30)
        && manufacturer != "DOF REALITY"
        && manufacturer != "None"
    {
        let rate = m
            .src
            .registry
            .manufacturer(&manufacturer, m.windows())
            .map(|info| info.default_refresh_rate)
            .ok_or_else(|| key_error(&manufacturer))?;
        m.set("MOTIONBOX", "refresh_rate", i64::from(rate).into())?;
    }
    Ok(())
}

fn v266(m: &mut Migrator) -> StepResult {
    m.new_section("HUD");
    m.set_all("HUD", &[("refresh_rate", 5.into()), ("auto_search", s("no")), ("model", s("H"))])
}

fn v267(m: &mut Migrator) -> StepResult {
    m.try_del("CUSTOM_FEATURES", "enable_hud");
    Ok(())
}

fn v268(m: &mut Migrator) -> StepResult {
    m.set("PROSIMU", "invert_tl", s("no"))
}

fn v269(m: &mut Migrator) -> StepResult {
    m.set("MOTIONBOX", "enable_debug", s("no"))
}

fn v270(m: &mut Migrator) -> StepResult {
    m.set("PT-ACTUATOR", "reverse_controller_port_8", s("no"))
}

/// Shaker engine sound type: `E` on a first installation, `O` (original) for existing users.
fn v271(m: &mut Migrator) -> StepResult {
    let engine = if m.first_install_bool()? { "E" } else { "O" };
    m.set_every_tune(&[("shaker_engine_type", s(engine))])
}

fn v272(m: &mut Migrator) -> StepResult {
    m.add_games(&["gridlegends"])
}

fn v273(m: &mut Migrator) -> StepResult {
    m.new_section("IFLAG");
    m.set_all("IFLAG", &[("refresh_rate", 90.into()), ("auto_search", s("yes")), ("com_port", s(""))])
}

fn v274(m: &mut Migrator) -> StepResult {
    m.set_all("IFLAG", &[("rotation", 0.into()), ("brightness", 10.into())])
}

/// SIMTECHREVOLUTION starts as PROSIMU without the PRS200 keys (or loses its retired keys), and the
/// `DEMO` model becomes `STR-SCN6150`.
fn v275(m: &mut Migrator) -> StepResult {
    if m.state.config.contains("SIMTECHREVOLUTION") {
        for key in ["stroke_mm", "max_motor_abs", "model_code", "nbr_controllers", "nbr_motors"] {
            m.del("SIMTECHREVOLUTION", key)?;
        }
    } else {
        m.copy_section("PROSIMU", "SIMTECHREVOLUTION")?;
        for key in ["prs200_firmware", "prs200_com_port1", "prs200_com_port2"] {
            m.del("SIMTECHREVOLUTION", key)?;
        }
    }
    if m.get("MOTIONBOX", "model")?.python_string().is_some_and(|s| s == "DEMO") {
        m.set("MOTIONBOX", "model", s("STR-SCN6150"))?;
    }
    Ok(())
}

/// Per-tune iFlag switches: every flag whose group is not `none`, enabled.
fn v276(m: &mut Migrator) -> StepResult {
    let flags: Vec<String> = m.src.registry.switchable_flag_names().map(str::to_owned).collect();
    let switches = LegacyValue::Dict(flags.iter().map(|f| (Key::from(f.as_str()), LegacyValue::Bool(true))).collect());
    m.set_every_tune(&[("iflag", switches)])
}

fn v277(m: &mut Migrator) -> StepResult {
    m.new_section("SIM3D");
    m.set_all("SIM3D", &[("refresh_rate", 90.into()), ("auto_search", s("yes")), ("com_port", s(""))])
}

fn v278(m: &mut Migrator) -> StepResult {
    m.set_all(
        "SIM3D",
        &[("model", s("None")), ("port1", s("None")), ("port2", s("None")), ("port3", s("None")), ("port4", s("None"))],
    )?;
    let ids: Vec<&str> = m.src.registry.rumble_effects.iter().map(|e| e.id.as_str()).collect();
    let levels = LegacyValue::Dict(ids.iter().map(|id| (Key::from(*id), LegacyValue::Int(10))).collect());
    let switches = LegacyValue::Dict(ids.iter().map(|id| (Key::from(*id), LegacyValue::Bool(true))).collect());
    m.set_every_tune(&[("rumble_effects", levels), ("rumble_switch", switches)])
}

fn v279(m: &mut Migrator) -> StepResult {
    m.each_tune(|_, tune, _| {
        for effect in ["brake_gear", "brake_heave", "brake_rumble_strips"] {
            nested(tune, "rumble_effects")?.insert(effect, 10.into())?;
            nested(tune, "rumble_switch")?.insert(effect, true.into())?;
        }
        Ok(())
    })
}

fn v280(m: &mut Migrator) -> StepResult {
    m.set_all(
        "SIM3D",
        &[("rpm_start_rumble", 0.into()), ("rpm_start_fading", 70.into()), ("rpm_stop_fading", 95.into())],
    )
}

/// Rumble RPM fading moves from SIM3D into each tune, plus the rumble shortcuts.
fn v281(m: &mut Migrator) -> StepResult {
    if m.section("SIM3D").is_ok_and(|mut sim| sim.remove("rpm_start_rumble").ok().flatten().is_some())
        && m.section("SIM3D").is_ok_and(|mut sim| sim.remove("rpm_start_fading").ok().flatten().is_some())
    {
        m.try_del("SIM3D", "rpm_stop_fading");
    }
    m.set_every_nested(&[
        ("rumble_effects", "rpm_start_rumble", 0.into()),
        ("rumble_effects", "rpm_start_fading", 70.into()),
        ("rumble_effects", "rpm_stop_fading", 95.into()),
    ])?;
    m.shortcut("rumble_switch_keys", "")?;
    m.shortcut("rumble_switch_btns", "")?;
    let ids: Vec<String> = m.src.registry.rumble_effects.iter().map(|e| e.id.clone()).collect();
    m.grouped_up_down_shortcuts("rumble_", &ids)
}

fn v282(m: &mut Migrator) -> StepResult {
    m.set_every_nested(&[
        ("rumble_effects", "wheel_slip_min_activation", 30.into()),
        ("rumble_effects", "wheel_lock_min_activation", 50.into()),
    ])
}

fn v283(m: &mut Migrator) -> StepResult {
    let new_effects = ["brake_traction_loss", "seat_traction_loss", "throttle_traction_loss"];
    m.each_tune_pending(|_, tune, _| {
        for effect in new_effects {
            nested(tune, "rumble_effects")?.insert(effect, 10.into())?;
            nested(tune, "rumble_switch")?.insert(effect, true.into())?;
        }
        Ok(())
    })?;
    m.set_every_nested(&[("rumble_effects", "traction_loss_min_activation", 40.into())])?;
    let ids: Vec<String> = new_effects.iter().map(|e| (*e).to_owned()).collect();
    m.grouped_up_down_shortcuts("rumble_", &ids)?;
    m.commit_profiles()
}

/// First installation only: the UShake6 centre channel engine level becomes 6.
fn v285(m: &mut Migrator) -> StepResult {
    if !m.first_install_bool()? {
        return Ok(());
    }
    for row in m.metadata_rows("shakers")? {
        let (name, mut value) = row?;
        if name != Key::from("SRS UShake6") {
            continue;
        }
        let mut layout = as_dict(&mut value, "SRS UShake6")?;
        let mut center = nested(&mut layout, "CE")?;
        let mut ce = nested(&mut center, "sfx")?;
        ce.insert("engine", 6.into())?;
        m.assign_layout("SRS UShake6", value)?;
        m.commit_metadata("shakers")?;
        break;
    }
    Ok(())
}

fn v286(m: &mut Migrator) -> StepResult {
    m.startup_game_config.insert("scn6_speed", 5000.into());
    m.startup_game_config.insert("scn6_acceleration", 80.into());
    Ok(())
}

/// v287 and v289 both reset F1 22 to its master profile.
fn v287(m: &mut Migrator) -> StepResult {
    m.add_games(&["f12022"])?;
    m.set("NETWORK_PORT", "f12022", s("20784"))
}

/// `start_with_windows` moves to the registry (`StartWithWindows` from its boolean value,
/// `'1'` when it does not read; `ShowSplashScreen` `'1'`); the key is dropped everywhere.
fn v288(m: &mut Migrator) -> StepResult {
    if m.windows() {
        m.set_reg("ShowSplashScreen", "1");
        let start = m
            .get("APP", "start_with_windows")
            .ok()
            .and_then(|value| {
                if value.type_name() == "bool" {
                    Some(value.is_truthy())
                } else {
                    value.python_string().and_then(|s| s.scalar_string()).and_then(|s| str2bool(&s))
                }
            })
            .unwrap_or(true);
        m.set_reg("StartWithWindows", if !start { "0" } else { "1" });
    }
    m.try_del("APP", "start_with_windows");
    Ok(())
}

fn v290(m: &mut Migrator) -> StepResult {
    m.each_selected_tune(
        |game| game == "iracing",
        |game, tune, _| {
            if game == "iracing" {
                nested(tune, "motion_effects")?.insert("wheel_slip", 1.into())?;
            }
            Ok(())
        },
    )
}

fn v291(m: &mut Migrator) -> StepResult {
    m.set("NETWORK_PORT", "vector36", 4123.into())?;
    m.add_games(&["vector36"])
}

fn v292(m: &mut Migrator) -> StepResult {
    m.add_games(&["vector36"])
}

fn v293(m: &mut Migrator) -> StepResult {
    m.new_section("MAGICBOX");
    m.set("MAGICBOX", "magic_wheel_scan_code", s(""))
}

fn v294(m: &mut Migrator) -> StepResult {
    m.set("IFLAG", "mirror", s("no"))
}

fn v295(m: &mut Migrator) -> StepResult {
    m.set("APP", "playstation_ip", s(""))?;
    m.add_games(&["gt7"])?;
    m.set("NETWORK_PORT", "gt7", s("33740"))
}

fn v297(m: &mut Migrator) -> StepResult {
    m.add_games(&["gtsport"])?;
    m.set("NETWORK_PORT", "gtsport", s("33340"))
}

fn v298(m: &mut Migrator) -> StepResult {
    m.new_section("SYMPROJECTS");
    m.set_all(
        "SYMPROJECTS",
        &[("refresh_rate", 90.into()), ("auto_search", s("yes")), ("gi_max_brightness", 100.into())],
    )
}

fn v299(m: &mut Migrator) -> StepResult {
    m.add_games(&["fs22"])
}

/// The version is written to INTELLIBOX again.
fn v300(m: &mut Migrator) -> StepResult {
    m.set_all("INTELLIBOX", &[("fan_frequency", 3.into()), ("config_file_version", 300.into())])
}

fn v301(m: &mut Migrator) -> StepResult {
    m.add_games(&["fs19"])
}

/// v302 and v307 both reset the two Warplanes games.
fn v302(m: &mut Migrator) -> StepResult {
    m.add_games(&["warplanes_ww1", "warplanes_bop"])
}

/// The centre fan starts with the front fan's speed range.
fn v303(m: &mut Migrator) -> StepResult {
    m.each_tune(|_, tune, _| {
        copy_within(tune, "power_wind_min_speed_perc", "power_center_wind_min_speed_perc")?;
        copy_within(tune, "power_wind_max_speed_perc", "power_center_wind_max_speed_perc")
    })?;
    for name in [
        "power_center_wind_switch_keys",
        "power_center_wind_up_keys",
        "power_center_wind_down_keys",
        "power_center_wind_switch_btns",
        "power_center_wind_up_btns",
        "power_center_wind_down_btns",
    ] {
        m.shortcut(name, "")?;
    }
    Ok(())
}

/// Console GT games are enabled by default except on Windows; both profiles are reset.
fn v306(m: &mut Migrator) -> StepResult {
    let enabled = if m.windows() { "no" } else { "yes" };
    m.set("CUSTOM_FEATURES", "enable_gt", s(enabled))?;
    for game in ["gt7", "gtsport"] {
        m.delete_game(game)?;
    }
    m.add_games(&["gt7", "gtsport"])
}

fn v308(m: &mut Migrator) -> StepResult {
    m.add_master_vehicles("api", &["Fly Dangerous", "Flashout 3"])
}

fn v309(m: &mut Migrator) -> StepResult {
    m.add_master_vehicle("api", "Flashout 3")
}

fn v310(m: &mut Migrator) -> StepResult {
    m.set("APP", "webui_hostname", s("localhost"))
}

fn v311(m: &mut Migrator) -> StepResult {
    m.new_section("HAPTICR");
    m.set_all(
        "HAPTICR",
        &[("refresh_rate", 90.into()), ("auto_search", s("yes")), ("com_port", s("")), ("model", s("None"))],
    )
}

/// HapticR effects (level 10, enabled) and their fading/activation settings, plus shortcuts.
fn v312(m: &mut Migrator) -> StepResult {
    let ids: Vec<String> = m.src.registry.haptic_effects.iter().map(|e| e.id.clone()).collect();
    let levels = LegacyValue::Dict(ids.iter().map(|id| (Key::from(id.as_str()), LegacyValue::Int(10))).collect());
    let switches = LegacyValue::Dict(ids.iter().map(|id| (Key::from(id.as_str()), LegacyValue::Bool(true))).collect());
    m.set_every_tune_pending(&[("haptic_effects", levels), ("haptic_switch", switches)])?;
    m.set_every_nested_pending(&[
        ("haptic_effects", "traction_loss_min_activation", 40.into()),
        ("haptic_effects", "rpm_start_rumble", 0.into()),
        ("haptic_effects", "rpm_start_fading", 94.into()),
        ("haptic_effects", "rpm_stop_fading", 100.into()),
        ("haptic_effects", "wheel_slip_min_activation", 30.into()),
        ("haptic_effects", "wheel_lock_min_activation", 50.into()),
    ])?;
    m.shortcut("haptic_switch_keys", "")?;
    m.shortcut("haptic_switch_btns", "")?;
    m.grouped_up_down_shortcuts("haptic_", &ids)?;
    m.commit_profiles()
}

fn v314(m: &mut Migrator) -> StepResult {
    m.set("NETWORK_PORT", "wrc", 30779.into())?;
    m.add_games(&["wrcg"])
}

fn v315(m: &mut Migrator) -> StepResult {
    m.set_all("SIMTECHREVOLUTION", &[("tl_act_speed", 5000.into()), ("tl_act_accel", 80.into())])
}

/// `ejection_seat_btns ` keeps its trailing space: no later step fixes it.
fn v316(m: &mut Migrator) -> StepResult {
    m.shortcut("ejection_seat_btns ", "")?;
    m.shortcut("ejection_seat_keys", "")?;
    m.set_every_tune(&[("enable_ejection_seat", false.into())])
}

fn v317(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_routines", s("no"))
}

fn v318(m: &mut Migrator) -> StepResult {
    for axis in ["yaw", "roll", "pitch", "all"] {
        m.shortcut(&format!("motion_compensation_{axis}_calibrate_btns"), "")?;
        m.shortcut(&format!("motion_compensation_{axis}_calibrate_keys"), "")?;
    }
    Ok(())
}

fn v319(m: &mut Migrator) -> StepResult {
    m.set("PT-ACTUATOR", "udp_config", LegacyValue::List(vec![s("192.168.1.1:7408")]))
}

fn v320(m: &mut Migrator) -> StepResult {
    for section in ["SIMTECHREVOLUTION", "PROSIMU"] {
        m.set_all(section, &[("post_homing_speed", 500.into()), ("post_homing_speed_timeout", 5.into())])?;
    }
    Ok(())
}

fn v321(m: &mut Migrator) -> StepResult {
    m.add_shaker_effect_everywhere("touchdown")?;
    m.derive_layout_effects(&[
        ("touchdown_left", "landing_gear_left"),
        ("touchdown_right", "landing_gear_right"),
        ("touchdown_nose", "landing_gear_nose"),
    ])?;
    m.up_down_shortcuts("shake_box_", "touchdown")?;
    m.commit_profiles()
}

/// F1 23 is created from the F1 22 master profile.
fn v322(m: &mut Migrator) -> StepResult {
    m.add_game("f12023", "f12022")?;
    m.set("NETWORK_PORT", "f12023", s("20785"))
}

fn v323(m: &mut Migrator) -> StepResult {
    m.del("APP", "default_browser")
}

fn v324(m: &mut Migrator) -> StepResult {
    m.add_shaker_effect_everywhere("turbulence")?;
    m.derive_layout_effects(&[("turbulence", "impact")])?;
    m.up_down_shortcuts("shake_box_", "turbulence")?;
    m.commit_profiles()
}

fn joystick_axis(kind: &str, trigger: &str, invert: bool) -> LegacyValue {
    let mut axis = Dict::new();
    axis.insert("joystick", s("Arduino LLC Arduino Leonardo"));
    axis.insert("type", s(kind));
    axis.insert("trigger", s(trigger));
    axis.insert("mode", s("Single Axis"));
    axis.insert("invert_axis", invert.into());
    LegacyValue::Dict(axis)
}

fn set_default_joystick(m: &mut Migrator, axes: [(&str, LegacyValue); 3]) -> StepResult {
    let mut value = m.metadata_row("joystick", "Default")?.ok_or_else(|| key_error("Default"))?;
    let mut default = as_dict(&mut value, "Default")?;
    for (name, axis) in axes {
        default.insert(name, axis)?;
    }
    m.assign_joystick("Default", value)
}

/// Non-Windows: the magicbox Arduino Leonardo pedal mapping.
fn v325(m: &mut Migrator) -> StepResult {
    if m.windows() {
        return Ok(());
    }
    set_default_joystick(
        m,
        [
            ("front", joystick_axis("axis <", "0", true)),
            ("back", joystick_axis("axis >", "1", true)),
            ("speed", joystick_axis("axis >", "0", true)),
        ],
    )
}

fn v326(m: &mut Migrator) -> StepResult {
    if m.windows() {
        m.skipped("regenerate APP.uuid (analytics id, not ported)");
    }
    Ok(())
}

fn v327(m: &mut Migrator) -> StepResult {
    m.set_all("SHAKEBOX", &[("check_surround", s("yes")), ("default_soundcard", s("yes"))])
}

/// Front traction loss copies the rear settings; the activation threshold is seeded from the rear
/// main-actuator share (`motion_main_actuators_traction_loss`).
fn v328(m: &mut Migrator) -> StepResult {
    m.set("MOTIONBOX", "traction_loss_front_clip_factor", 1000.into())?;
    m.each_tune(|_, tune, _| {
        copy_within(tune, "motion_main_actuators_traction_loss", "motion_main_actuators_traction_loss_front")?;
        copy_within(tune, "motion_main_actuators_traction_loss", "min_traction_loss_front_activation")?;
        for group in ["motion_effects", "motion_switch", "max_telemetry_vars", "motion_effect_smoothing_factor"] {
            copy_nested(tune, group, "traction_loss", "traction_loss_front")?;
        }
        Ok(())
    })
}

fn v329(m: &mut Migrator) -> StepResult {
    m.set_all("CUSTOM_FEATURES", &[("dams_front_tl_mode", 0.into()), ("dams_front_tl_factor", 1.into())])
}

/// Forza Motorsport 8 is created from the Forza Motorsport 7 master profile.
fn v330(m: &mut Migrator) -> StepResult {
    m.add_game("forzamotorsports8", "forzamotorsports7")
}

fn v331(m: &mut Migrator) -> StepResult {
    m.set_all("CUSTOM_FEATURES", &[("dams_front_tl_a", 1.6929.into()), ("dams_rear_tl_b", 1.44.into())])?;
    m.set("MOTIONBOX", "min_degree_null_yaw_movement", 30.into())
}

fn v332(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "dams_front_steerratio", 14.5.into())
}

fn v333(m: &mut Migrator) -> StepResult {
    m.set_all(
        "CUSTOM_FEATURES",
        &[("dams_tl_max_auto_smoothing", 80.into()), ("dams_tl_auto_smoothing_max_speed_kmh", 80.into())],
    )
}

fn v334(m: &mut Migrator) -> StepResult {
    m.set("NETWORK_PORT", "wrc2023", 20999.into())?;
    m.add_games(&["wrc2023"])
}

fn v335(m: &mut Migrator) -> StepResult {
    m.new_section("VNM");
    m.set_all(
        "VNM",
        &[
            ("vnm_com_port", s("")),
            ("smoothing_threshold", 0.01.into()),
            ("soft_crash_threshold", 50.0.into()),
            ("boost_threshold", 5.into()),
            ("reverse_controller_port_5", s("no")),
            ("reverse_controller_port_6", s("no")),
            ("reverse_controller_port_7", s("no")),
            ("reverse_controller_port_8", s("no")),
            ("reverse_controller_port_9", s("no")),
            ("real_heave", s("yes")),
            ("lateral_distance_actuators", 0.into()),
            ("vertical_actuators_stroke", 0.into()),
            ("soft_start_stop_factor", 90.into()),
            ("soft_start_stop_timer", 4.into()),
        ],
    )
}

fn v336(m: &mut Migrator) -> StepResult {
    let vnm = m.section_ref("VNM")?.clone();
    m.state.config.insert("VNM SIMULATION", vnm);
    m.state.config.remove("VNM").ok_or_else(|| key_error("VNM")).map(|_| ())
}

fn v337(m: &mut Migrator) -> StepResult {
    m.set("APP", "enable_sdl2", s("yes"))
}

/// rFpro is created from the rFactor master profile.
fn v338(m: &mut Migrator) -> StepResult {
    m.add_game("rfpro", "rf1")
}

fn v339(m: &mut Migrator) -> StepResult {
    m.set("PT-ACTUATOR", "pt_com_port", s(""))
}

fn v340(m: &mut Migrator) -> StepResult {
    if m.windows() {
        return Ok(());
    }
    set_default_joystick(
        m,
        [
            ("front", joystick_axis("axis >", "2", false)),
            ("back", joystick_axis("axis <", "4", false)),
            ("speed", joystick_axis("axis >", "2", false)),
        ],
    )
}

fn v341(m: &mut Migrator) -> StepResult {
    m.add_games(&["lemans"])
}

fn v342(m: &mut Migrator) -> StepResult {
    if m.first_install_bool()? {
        for row in m.metadata_rows("shakers")? {
            let (name, mut value) = row?;
            if name != Key::from("SRS UShake6") {
                continue;
            }
            let mut layout = as_dict(&mut value, "SRS UShake6")?;
            nested(&mut nested(&mut layout, "CE")?, "sfx")?.insert("engine", 4.into())?;
            nested(&mut nested(&mut layout, "SUB")?, "sfx")?.insert("engine", 4.into())?;
            m.assign_layout("SRS UShake6", value)?;
            m.commit_metadata("shakers")?;
            break;
        }
    }
    Ok(())
}

fn v345(m: &mut Migrator) -> StepResult {
    m.assign_layout("SRS UShake6 Enhanced", m.src.ushake6_enhanced.clone().into())?;
    m.commit_profiles()
}

fn v346(m: &mut Migrator) -> StepResult {
    m.set_all(
        "SIMTECHREVOLUTION",
        &[
            ("post_homing_speed", 800.into()),
            ("post_homing_acceleration", 100.into()),
            ("post_homing_speed_timeout", 15.into()),
        ],
    )
}

/// The PT-Actuator `(…)` model names (without the CANBus suffix) become Thanos `[…]` names.
pub(crate) const THANOS_MODEL_RENAMES: [(&str, &str); 10] = [
    ("3 Actuators (2 Front + 1 Rear)", "3 Actuators [2 Front + 1 Rear]"),
    ("3 Actuators (1 Front + 2 Rear)", "3 Actuators [1 Front + 2 Rear]"),
    ("5 Actuators (2 Front + 2 Rear + TL)", "5 Actuators [2 Front + 2 Rear + TL]"),
    ("5 Actuators (2 Front + 2 Rear + Surge)", "5 Actuators [2 Front + 2 Rear + Surge]"),
    ("6 Actuators (2 Front + 2 Rear + TL + Surge)", "6 Actuators [2 Front + 2 Rear + TL + Surge]"),
    ("6 Actuators (2 Front + 2 Rear + Surge + Belt)", "6 Actuators [2 Front + 2 Rear + Surge + Belt]"),
    ("6 Actuators (2 Front + 2 Rear + 2 TL)", "6 Actuators [2 Front + 2 Rear + 2 TL]"),
    ("7 Actuators (2 Front + 2 Rear + TL + Surge + Belt)", "7 Actuators [2 Front + 2 Rear + TL + Surge + Belt]"),
    ("7 Actuators (2 Front + 2 Rear + 2 TL + Surge)", "7 Actuators [2 Front + 2 Rear + 2 TL + Surge]"),
    ("8 Actuators (2 Front + 2 Rear + 2 TL + Surge + Belt)", "8 Actuators [2 Front + 2 Rear + 2 TL + Surge + Belt]"),
];

fn v347(m: &mut Migrator) -> StepResult {
    let model = m.get("MOTIONBOX", "model")?.clone();
    if let Some((_, renamed)) =
        THANOS_MODEL_RENAMES.iter().find(|(old, _)| model.python_string().is_some_and(|s| s == *old))
    {
        m.set("MOTIONBOX", "model", s(renamed))?;
    }
    Ok(())
}

fn v349(m: &mut Migrator) -> StepResult {
    m.set("MOTIONBOX", "gear_smoothing_min_delta", 1.into())?;
    m.set_every_tune(&[("gear_smoothing_factor", 95.into())])
}

fn v350(m: &mut Migrator) -> StepResult {
    m.set("PROSIMU", "use_scn6_legacy", s("no"))
}

fn v351(m: &mut Migrator) -> StepResult {
    m.add_games(&["f12024"])?;
    m.set("NETWORK_PORT", "f12024", s("20786"))
}

fn v352(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[("scn6_speed", 5000.into()), ("scn6_acceleration", 80.into())])
}

fn v353(m: &mut Migrator) -> StepResult {
    m.set("PT-ACTUATOR", "pt_com_baud_rate", s("115200"))
}

/// Picks the compensation sensor type: the untouched factory values (sum 300) mean the virtual
/// IntelliComp sensor (`V`, surge reversed), anything else a WITMotion IMU (`W`).
fn v354(m: &mut Migrator) -> StepResult {
    let mut integer_sum = BigInt::from(0);
    for key in ["sway_factor", "surge_factor", "surge_horizontal_factor", "surge_horizontal_stroke"] {
        integer_sum += m.int_of("MOTION_COMPENSATION", key)?;
    }
    let mut sum = LegacyValue::from_integer(integer_sum);
    for axis in ["roll", "pitch"] {
        let section = m.section_ref("MOTION_COMPENSATION")?;
        let axis_dict = section.item_owned(axis).map_err(|_| key_error(axis))?;
        let offset = axis_dict.item_owned("offset").map_err(|_| key_error("offset"))?;
        sum = sum.py_add(&LegacyValue::Float(offset.py_float_native()?))?;
    }
    if sum.py_float_native()? == 300.0 {
        m.set("MOTION_COMPENSATION", "type", s("V"))?;
        compensation_axis_mut(m, "surge")?.insert("reverse", true.into())?;
    } else {
        m.set("MOTION_COMPENSATION", "type", s("W"))?;
    }
    m.set_all(
        "MOTION_COMPENSATION",
        &[
            ("mm_distance_left_right", 558.into()),
            ("mm_distance_front_back", 1270.into()),
            ("vertical_actuators_stroke", 150.into()),
        ],
    )
}

fn v355(m: &mut Migrator) -> StepResult {
    m.set_all(
        "PT-ACTUATOR",
        &[
            ("parking_speed", 1000.into()),
            ("reaction_speed_map_min", 2000.into()),
            ("reaction_speed_map_max", 7000.into()),
            ("park_to_motion_speed_delay_s", 5.into()),
        ],
    )
}

fn v358(m: &mut Migrator) -> StepResult {
    m.set("APP", "language", s("en"))
}

/// Language: on a Windows first installation the NSIS installer language (registry, unavailable
/// here, so the fallback `'en'`); otherwise `'en'`.
fn v359(m: &mut Migrator) -> StepResult {
    // The first-installation flag is converted before the platform is checked (the conversion may fail).
    if m.windows() {
        for name in ["StartWithWindows", "ShowSplashScreen"] {
            if m.get_reg(name).is_none() {
                m.set_reg(name, "1");
            }
        }
    }
    m.first_install_bool()?;
    if m.windows() {
        m.skipped("registry HKLM installer Language (absent: 'en') and Sim Racing Studio 2.0 / WinSparkle copies");
    }
    m.set("APP", "language", s("en"))
}

fn v360(m: &mut Migrator) -> StepResult {
    m.set_every_tune(&[("belt_surge_reverse", false.into())])
}

fn v361(m: &mut Migrator) -> StepResult {
    m.add_games(&["condor3", "fs2024"])
}

fn v362(m: &mut Migrator) -> StepResult {
    m.add_games(&["fs25"])
}

fn v363(m: &mut Migrator) -> StepResult {
    m.add_games(&["il2_cliffs"])
}

/// The PT-ACTUATOR reaction map minimum becomes 1000.
fn v365(m: &mut Migrator) -> StepResult {
    m.set("PT-ACTUATOR", "reaction_speed_map_min", 1000.into())
}

/// Every tune gets the global reaction speed (`MOTIONBOX.reaction_speed` as an integer).
fn v366(m: &mut Migrator) -> StepResult {
    let speed = m.int_of("MOTIONBOX", "reaction_speed")?;
    m.set_every_tune(&[("motion_reaction_speed", LegacyValue::from_integer(speed))])
}

/// The hidden-page switch of the HapticR page, off.
fn v367(m: &mut Migrator) -> StepResult {
    m.set("APP", "enable_hapticr", s("no"))
}

fn v368(m: &mut Migrator) -> StepResult {
    m.add_games(&["acevo"])
}

fn v369(m: &mut Migrator) -> StepResult {
    m.skipped("write <docs>/lib_v_m.txt (SRS sales-gating library selector)");
    Ok(())
}

fn v370(m: &mut Migrator) -> StepResult {
    m.set_all(
        "MOTIONBOX",
        &[("overall_smoothing_map_min_slider", 70.into()), ("overall_smoothing_map_max_slider", 99.into())],
    )
}

fn v371(m: &mut Migrator) -> StepResult {
    m.set("MOTIONBOX", "overall_smoothing_map_min_slider", 90.into())
}

fn v372(m: &mut Migrator) -> StepResult {
    m.set("PT-ACTUATOR", "smoothing_threshold", 100000.into())
}

/// Version 373 belongs to the sales-gating keys `APP.trial_started` and `APP.trial_subscription_started`
/// of imported files. OpenRigCue has no such concept and ignores both keys on import,
/// so the step changes nothing and never fails.
fn v373(_m: &mut Migrator) -> StepResult {
    Ok(())
}

fn v374(m: &mut Migrator) -> StepResult {
    m.set("DOF REALITY", "smoothing_threshold", 15.into())?;
    m.set("APP", "marketing_notifications", true.into())
}

fn v375(m: &mut Migrator) -> StepResult {
    m.set_all("CUSTOM_FEATURES", &[("enable_flyinside_overwrite", s("no")), ("rollback_xplane_latg", s("no"))])?;
    m.set("NETWORK_PORT", "flyinside_fsx", s("31235"))
}

fn v376(m: &mut Migrator) -> StepResult {
    m.set_all("DOF REALITY", &[("left_controller_resolution", s("10")), ("right_controller_resolution", s("10"))])
}

fn v377(m: &mut Migrator) -> StepResult {
    m.add_games(&["f12025"])?;
    m.set("NETWORK_PORT", "f12025", s("20787"))
}

fn v378(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "disable_rfpro", s("no"))
}

fn v379(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_smc3_ymotor", s("no"))
}

fn v381(m: &mut Migrator) -> StepResult {
    m.add_games(&["wreckfest2"])?;
    m.set("NETWORK_PORT", "wreckfest2", s("23123"))
}

fn v382(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_mc_telemetry", s("no"))
}

fn compensation_axis_mut<'d>(m: &'d mut Migrator, axis: &str) -> Result<DictView<'d>, String> {
    let value = m.state.config.get_mut("MOTION_COMPENSATION").ok_or_else(|| key_error("MOTION_COMPENSATION"))?;
    super::views::at_path(value, &[axis])
}

fn v383(m: &mut Migrator) -> StepResult {
    compensation_axis_mut(m, "heave")?.insert("enabled", s("no"))?;
    Ok(())
}

fn v386(m: &mut Migrator) -> StepResult {
    m.add_games(&["acrally"])
}

fn v388(m: &mut Migrator) -> StepResult {
    m.add_games(&["pmracing"])?;
    m.set("NETWORK_PORT", "pmracing", s("7576"))
}

fn v389(m: &mut Migrator) -> StepResult {
    m.set("SIMTECHREVOLUTION", "search_com_ports", s(""))
}

fn v390(m: &mut Migrator) -> StepResult {
    compensation_axis_mut(m, "yaw")?.insert("enabled", s("no"))?;
    Ok(())
}

/// THANOS receives the PT-ACTUATOR controller keys.
fn v391(m: &mut Migrator) -> StepResult {
    for key in [
        "vertical_actuators_stroke",
        "thanos_com_port",
        "send_spike_filter",
        "reverse_controller_port_5",
        "reverse_controller_port_6",
        "reverse_controller_port_7",
        "reverse_controller_port_8",
    ] {
        m.copy_key(("PT-ACTUATOR", key), ("THANOS", key))?;
    }
    Ok(())
}

fn v393(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_motion_measurement_mode", s("no"))
}

/// Writes a stray `motion_switch` key into the emulator's `motion_effects`.
fn v394(m: &mut Migrator) -> StepResult {
    m.edit_profile("emulator", |emulator| {
        nested(emulator, "motion_effects")?.insert("motion_switch", true.into())?;
        Ok(())
    })
}

fn v395(m: &mut Migrator) -> StepResult {
    m.set_all(
        "MOTION_COMPENSATION",
        &[("roll_factor", 100.into()), ("pitch_factor", 100.into()), ("yaw_factor", 100.into())],
    )
}

/// `SIMTECHREVOLUTION.search_com_ports` becomes `APP.allowed_com_ports` (`''` when missing).
fn v396(m: &mut Migrator) -> StepResult {
    match m.get("SIMTECHREVOLUTION", "search_com_ports") {
        Ok(ports) => {
            m.set("APP", "allowed_com_ports", ports)?;
            m.del("SIMTECHREVOLUTION", "search_com_ports")
        }
        Err(_) => m.set("APP", "allowed_com_ports", s("")),
    }
}

fn v397(m: &mut Migrator) -> StepResult {
    m.add_games(&["tdusc"])?;
    m.set("NETWORK_PORT", "tdusc", s("31123"))
}

fn v398(m: &mut Migrator) -> StepResult {
    m.add_games(&["rrevo"])
}

fn v400(m: &mut Migrator) -> StepResult {
    m.add_games(&["tdusc"])
}

fn v401(m: &mut Migrator) -> StepResult {
    m.add_games(&["acesofthunder"])
}

/// Log level names become numbers (`debug` 10 … `critical` 50, anything else 30).
fn v402(m: &mut Migrator) -> StepResult {
    let current = m.section_ref("APP")?.item_owned("log_level").ok();
    let numeric = current.as_ref().is_some_and(|v| v.py_integer().is_ok());
    if !numeric {
        let raw = current.and_then(|v| v.py_str()).unwrap_or_else(|| "30".to_owned()).to_lowercase();
        let level = match raw.as_str() {
            "debug" => 10,
            "info" => 20,
            "warning" => 30,
            "error" => 40,
            "critical" => 50,
            _ => 30,
        };
        m.set("APP", "log_level", LegacyValue::Int(level))?;
    }
    Ok(())
}

fn v404(m: &mut Migrator) -> StepResult {
    m.add_games(&["ironrebellion"])?;
    m.set("NETWORK_PORT", "ironrebellion", s("11631"))
}

fn v406(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_event_driven_device_scan", s("no"))
}

fn v407(m: &mut Migrator) -> StepResult {
    m.add_games(&["forzahorizon6"])
}

fn v408(m: &mut Migrator) -> StepResult {
    if !m.windows() {
        m.set("HUD", "auto_search", s("no"))?;
        m.set("BELT_TENSIONER", "auto_search", s("no"))?;
    }
    Ok(())
}

fn v409(m: &mut Migrator) -> StepResult {
    m.set("CUSTOM_FEATURES", "enable_event_driven_device_scan", s("yes"))
}

fn v410(m: &mut Migrator) -> StepResult {
    m.add_games(&["ironrebellion"])
}

/// Renames `thanos_com_port` → `com_port` and `send_spike_filter` → `filter` (existing new keys win),
/// drops `thanos_controller`, and guarantees both new keys.
fn v412(m: &mut Migrator) -> StepResult {
    for name in ["THANOS", "PT-ACTUATOR"] {
        let Some(value) = m.state.config.get_mut(name) else { continue };
        let mut section = as_dict(value, name)?;
        for (old, new) in [("thanos_com_port", "com_port"), ("send_spike_filter", "filter")] {
            if let Some(value) = section.get(old) {
                // The new key is assigned first and the old one deleted after, so a new key
                // created here lands at the end of the section.
                if !section.contains(new) {
                    section.insert(new, value)?;
                }
                section.remove(old)?;
            }
        }
        section.remove("thanos_controller")?;
        section.set_default("com_port", s(""))?;
        section.set_default("filter", s("yes"))?;
    }
    Ok(())
}

#[cfg(test)]
mod aliases;
