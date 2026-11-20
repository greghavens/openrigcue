//! Motion platform data: the model table (one entry per model) and the mixing tables by
//! mapping id.
//!
//! Models come from the shared model crate; mixing uses `data/motion_config.toml`.
//! A [`Platform`] is one model entry together with the mixing tables its `mapping_id` names.

use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::axis::Axis;
use crate::error::CueError;
use crate::external::{ExternalMixing, ExternalMixingError};
use crate::pyint::TableInt;
use crate::pymath::Num;
use crate::pyvalue::PyValue;
use crate::timing::HostOs;

/// Number of output slots that are always computed and sent.
pub const SLOTS: usize = 12;

/// Platform manufacturer (the model entry's `manufacturer`). Selects the rate-shaping variant and the
/// configuration section the motion code reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize)]
pub enum Manufacturer {
    #[serde(rename = "None")]
    None,
    /// Also the fallback when a model has no `manufacturer` key.
    #[default]
    #[serde(rename = "DOF REALITY")]
    DofReality,
    #[serde(rename = "Yaw VR")]
    YawVr,
    #[serde(rename = "PROSIMU")]
    Prosimu,
    #[serde(rename = "PT-ACTUATOR")]
    PtActuator,
    #[serde(rename = "THANOS")]
    Thanos,
    #[serde(rename = "VNM SIMULATION")]
    VnmSimulation,
    #[serde(rename = "SIMTECHREVOLUTION")]
    SimTechRevolution,
}

impl Manufacturer {
    /// The manufacturer string, which is also the name of its `config` section.
    pub fn name(self) -> &'static str {
        match self {
            Manufacturer::None => "None",
            Manufacturer::DofReality => "DOF REALITY",
            Manufacturer::YawVr => "Yaw VR",
            Manufacturer::Prosimu => "PROSIMU",
            Manufacturer::PtActuator => "PT-ACTUATOR",
            Manufacturer::Thanos => "THANOS",
            Manufacturer::VnmSimulation => "VNM SIMULATION",
            Manufacturer::SimTechRevolution => "SIMTECHREVOLUTION",
        }
    }

    /// Whether this manufacturer's output is shaped with the rotary rate shaping
    /// (DOF REALITY, Yaw VR) rather than the linear-actuator rate shaping (everything else).
    pub fn uses_rotary_shaping(self) -> bool {
        matches!(self, Manufacturer::DofReality | Manufacturer::YawVr)
    }

    /// The manufacturer's default refresh rate, which the Hardware page sets when the
    /// manufacturer is chosen; `None` where the manufacturer table has no entry (a missing-key error for the
    /// caller). Outside Windows only None and DOF REALITY have one, and SIMTECHREVOLUTION has none
    /// on any platform.
    pub fn default_refresh_rate(self, os: HostOs) -> Option<i64> {
        match self {
            Manufacturer::None | Manufacturer::DofReality => Some(30),
            Manufacturer::SimTechRevolution => None,
            _ if os != HostOs::Windows => None,
            Manufacturer::PtActuator => Some(400),
            Manufacturer::Prosimu | Manufacturer::Thanos | Manufacturer::VnmSimulation | Manufacturer::YawVr => {
                Some(500)
            }
        }
    }
}

/// Direction of one axis on one slot (`'+'`, `'-'`, `'0'` in the direction table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sign {
    Minus,
    Zero,
    Plus,
}

impl Sign {
    /// The signed signal of one table cell: `value * -1`, `value` or the integer 0.
    pub(crate) fn apply(self, value: Num) -> Num {
        match self {
            Sign::Plus => value,
            Sign::Minus => value.mul(Num::Int(-1)),
            Sign::Zero => Num::Int(0),
        }
    }

    /// A direction cell; `None` for anything but `'+'`, `'-'`, `'0'` (such a cell yields no signal).
    pub(crate) fn from_cell(cell: &str) -> Option<Sign> {
        match cell {
            "+" => Some(Sign::Plus),
            "-" => Some(Sign::Minus),
            "0" => Some(Sign::Zero),
            _ => None,
        }
    }
}

/// The order in which the mixing stage indexes the deadzone row.
pub(crate) const EFFECTS_ORDER: [Axis; Axis::COUNT] = [
    Axis::Roll,
    Axis::Pitch,
    Axis::Yaw,
    Axis::Sway,
    Axis::Surge,
    Axis::Heave,
    Axis::TractionLoss,
    Axis::TractionLossFront,
];

/// The three table rows of one output slot, as the mixing stage reads them.
///
/// The rows are kept as loaded: a row may be shorter than 8 cells (or empty, for a
/// row the table lacks), which is an index error where the first missing cell is read. The
/// built-in tables always have 12 rows of 8 cells.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotMix {
    /// `direction[slot]` in table order (pitch, roll, yaw, sway, surge, heave, tl, tlf); `None` is a
    /// cell other than `'+'`, `'-'`, `'0'`.
    pub direction: Vec<Option<Sign>>,
    /// `percentages[slot]`: the share of each axis on this slot, percent, in table order.
    pub percentages: Vec<TableInt>,
    /// `deadzone[slot]` in the effects-list order ([`EFFECTS_ORDER`]: roll, pitch, yaw, ...): the axis
    /// percentage is capped at `±(100 − deadzone)` when the deadzone is non-zero.
    pub deadzone: Vec<TableInt>,
}

/// A row a table does not have.
static NO_ROW: SlotMix = SlotMix { direction: Vec::new(), percentages: Vec::new(), deadzone: Vec::new() };

impl SlotMix {
    /// The direction of `axis`, `None` where the row has no valid cell for it.
    pub fn direction_of(&self, axis: Axis) -> Option<Sign> {
        self.direction.get(axis.index()).copied().flatten()
    }

    /// The deadzone cell of `axis`.
    pub fn deadzone_of(&self, axis: Axis) -> Option<&TableInt> {
        self.deadzone.get(EFFECTS_ORDER.iter().position(|&a| a == axis)?)
    }
}

/// Wheel position keys, in the order the per-wheel tables are iterated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wheel {
    RearLeft,
    FrontLeft,
    FrontRight,
    RearRight,
}

impl Wheel {
    pub const ORDER: [Wheel; 4] = [Wheel::RearLeft, Wheel::FrontLeft, Wheel::FrontRight, Wheel::RearRight];
}

/// One value per wheel position (keys `lr`, `lf`, `rf`, `rr`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct WheelSlots<T> {
    pub lr: T,
    pub lf: T,
    pub rf: T,
    pub rr: T,
}

impl<T: Copy> WheelSlots<T> {
    pub fn get(&self, wheel: Wheel) -> T {
        match wheel {
            Wheel::RearLeft => self.lr,
            Wheel::FrontLeft => self.lf,
            Wheel::FrontRight => self.rf,
            Wheel::RearRight => self.rr,
        }
    }
}

/// Where wheel and event effects land on the platform.
#[derive(Debug, Clone, PartialEq)]
pub struct WheelLayout {
    /// Output slot per wheel (`wheels`).
    pub slots: WheelSlots<usize>,
    /// Sign of the effect per wheel (`wheels_motor_dir`); `None` means +1 everywhere.
    pub directions: Option<WheelSlots<i64>>,
    /// `wheel_shake_factor`.
    pub shake_factor: f64,
}

impl WheelLayout {
    pub fn direction(&self, wheel: Wheel) -> i64 {
        self.directions.map_or(1, |d| d.get(wheel))
    }
}

/// One motion platform model: a model table entry with its mixing tables.
#[derive(Debug, Clone, PartialEq)]
pub struct Platform {
    /// OpenRigCue identifier, e.g. `dof-reality-p3` (manufacturer and model key).
    pub id: String,
    /// The model table key, e.g. `P3`.
    pub model: String,
    /// The model table key; entries have no other name.
    pub name: String,
    /// `manufacturer`; DOF REALITY where the entry has none.
    pub manufacturer: Manufacturer,
    /// `controller`: the driver key (`smc3`, `prs200`, ...).
    pub controller: String,
    /// `nbr_motors`; the cueing still computes all [`SLOTS`].
    pub motors: usize,
    /// `nbr_controllers`.
    pub controllers: usize,
    /// `max_motor_abs`.
    pub max_motor_abs: u32,
    /// `motor_max_perc`; the rest is the guard band of rate shaping.
    pub motor_max_perc: f64,
    /// `motors_direction`.
    pub motors_direction: f64,
    pub mapping_id: u32,
    /// `main_actuators`. Absent: no share overrides, and every slot scales with `max_power`.
    pub main_actuators: Option<Vec<usize>>,
    /// `belt_tensioner_motor` (absent or −1: none).
    pub belt_tensioner_slot: Option<usize>,
    /// Wheel effect layout; `None` when the model lacks `wheels` or `wheel_shake_factor`.
    pub wheels: Option<WheelLayout>,
    /// The mixing tables of `mapping_id`: the table for `motion_enhanced_gforces` 0, then 1, each one
    /// row per motor.
    pub mapping: Vec<Vec<SlotMix>>,
    /// Whether the model is a model table entry. `false` for [`MotionConfig::unlisted`]: the stream
    /// path fails with a missing-key error for such a model and test mode computes with the `'None'` model's values.
    pub listed: bool,
}

/// The embedded data cannot be used (never the case for the shipped files).
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("invalid motion data: {0}")]
    Data(String),
}

pub use openrigcue_model::motion_models::{motion_model, motion_models, MotionModel};

/// The fields of an entry the motion code reads.
#[derive(Deserialize)]
struct EntryFields {
    manufacturer: Option<Manufacturer>,
    controller: String,
    nbr_motors: usize,
    nbr_controllers: usize,
    max_motor_abs: u32,
    motor_max_perc: f64,
    motors_direction: f64,
    mapping_id: u32,
    main_actuators: Option<Vec<usize>>,
    belt_tensioner_motor: Option<i64>,
    wheel_shake_factor: Option<f64>,
    wheels: Option<WheelSlots<usize>>,
    wheels_motor_dir: Option<WheelSlots<i64>>,
}

#[derive(Deserialize)]
struct MappingFile {
    mapping: Vec<MappingEntry>,
}

#[derive(Deserialize)]
struct MappingEntry {
    id: u32,
    table: Vec<TableFile>,
}

#[derive(Deserialize)]
struct TableFile {
    direction: Vec<Vec<String>>,
    deadzone: Vec<Vec<i64>>,
    percentages: Vec<Vec<i64>>,
}

/// The mixing tables by mapping id.
#[derive(Debug, Clone, PartialEq)]
pub struct MotionConfig {
    mapping: Vec<(u32, Vec<Vec<SlotMix>>)>,
}

/// Mapping ids whose "enhanced g-forces" off table is replaced with the external CSV tables.
const EXTERNAL_MAPPING_IDS: [u32; 3] = [2, 3, 6];

fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

impl MotionConfig {
    /// The embedded tables, used when no external CSV table exists.
    pub fn builtin() -> &'static MotionConfig {
        static CONFIG: OnceLock<MotionConfig> = OnceLock::new();
        CONFIG.get_or_init(|| {
            let file: MappingFile =
                toml::from_str(include_str!("../data/motion_config.toml")).expect("data/motion_config.toml is valid");
            let mapping = file
                .mapping
                .into_iter()
                .map(|entry| {
                    let tables = entry
                        .table
                        .into_iter()
                        .map(|t| {
                            (0..t.direction.len().max(t.deadzone.len()).max(t.percentages.len()))
                                .map(|row| SlotMix {
                                    direction: t.direction.get(row).map_or(Vec::new(), |r| {
                                        r.iter().take(Axis::COUNT).map(|c| Sign::from_cell(c)).collect()
                                    }),
                                    percentages: t.percentages.get(row).map_or(Vec::new(), |r| {
                                        r.iter().take(Axis::COUNT).map(|&v| TableInt::from(v)).collect()
                                    }),
                                    deadzone: t.deadzone.get(row).map_or(Vec::new(), |r| {
                                        r.iter().take(Axis::COUNT).map(|&v| TableInt::from(v)).collect()
                                    }),
                                })
                                .collect()
                        })
                        .collect();
                    (entry.id, tables)
                })
                .collect();
            MotionConfig { mapping }
        })
    }

    /// The tables for the documents folder `documents`: when any of the three override CSV files
    /// exists there, the "enhanced g-forces" off table of mapping ids 2, 3 and 6 is replaced by the
    /// external tables. An error means an override file could not be loaded.
    pub fn load(documents: &Path) -> Result<MotionConfig, ExternalMixingError> {
        let config = MotionConfig::builtin().clone();
        Ok(match ExternalMixing::load_dir(documents)? {
            Some(external) => config.with_external_mixing(&external),
            None => config,
        })
    }

    /// The tables with the external CSV tables applied ([`MotionConfig::load`]).
    pub fn with_external_mixing(mut self, external: &ExternalMixing) -> MotionConfig {
        let rows: Vec<SlotMix> = (0..external.rows()).map(|slot| external.slot(slot)).collect();
        for (id, tables) in &mut self.mapping {
            if EXTERNAL_MAPPING_IDS.contains(id) {
                tables[0] = rows.clone();
            }
        }
        self
    }

    /// The tables of mapping `id`.
    pub fn tables(&self, id: u32) -> Option<&[Vec<SlotMix>]> {
        self.mapping.iter().find(|(i, _)| *i == id).map(|(_, t)| t.as_slice())
    }

    /// The platform of the model table entry `model`, `None` for a key the table does not have.
    pub fn platform(&self, model: &str) -> Option<Result<Platform, PlatformError>> {
        motion_model(model).map(|entry| self.assemble(entry))
    }

    /// The platform the `MOTIONBOX.model` setting selects: `None` for `'None'` (the stream and test
    /// paths do nothing), the model's platform, or [`MotionConfig::unlisted`] for a name
    /// the model table lacks.
    pub fn select(&self, model: &str) -> Result<Option<Platform>, PlatformError> {
        if model == "None" {
            return Ok(None);
        }
        match self.platform(model) {
            Some(platform) => platform.map(Some),
            None => self.unlisted(model).map(Some),
        }
    }

    /// The platform of a model setting that names no model table entry (for example `STR-SCN6150`).
    ///
    /// The stream path fails with a missing-key error every frame for such a model, after the soft start
    /// and gear smoothing state has advanced. Test mode falls back to the `'None'` entry's
    /// `motors_direction`, `mapping_id` and `max_motor_abs`, and the mixing stage to manufacturer
    /// DOF REALITY, no belt tensioner slot and no main actuators.
    pub fn unlisted(&self, model: &str) -> Result<Platform, PlatformError> {
        let none = motion_model("None").ok_or_else(|| PlatformError::Data("motion_models has no 'None'".into()))?;
        let fallback = self.assemble(none)?;
        Ok(Platform {
            id: "unlisted".to_owned(),
            model: model.to_owned(),
            name: model.to_owned(),
            manufacturer: Manufacturer::DofReality,
            controller: fallback.controller,
            motors: 0,
            controllers: 0,
            main_actuators: None,
            belt_tensioner_slot: None,
            wheels: None,
            listed: false,
            ..fallback
        })
    }

    fn assemble(&self, entry: &MotionModel) -> Result<Platform, PlatformError> {
        let invalid = |e: String| PlatformError::Data(format!("motion_models[{:?}]: {e}", entry.key));
        let f: EntryFields =
            toml::Value::Table(entry.fields.clone()).try_into().map_err(|e| invalid(format!("{e}")))?;
        let mapping =
            self.tables(f.mapping_id).ok_or_else(|| invalid(format!("no mapping {}", f.mapping_id)))?.to_vec();
        // The wheel layout needs both wheels and wheel_shake_factor: a missing shake factor
        // disables the wheel effects even when wheels are present.
        let wheels = match (f.wheels, f.wheel_shake_factor) {
            (Some(slots), Some(shake_factor)) => {
                Some(WheelLayout { slots, directions: f.wheels_motor_dir, shake_factor })
            }
            _ => None,
        };
        if let Some(layout) = &wheels {
            if Wheel::ORDER.iter().any(|&w| layout.slots.get(w) >= SLOTS) {
                return Err(invalid("a wheel slot is outside the 12 output slots".into()));
            }
        }
        let manufacturer = f.manufacturer.unwrap_or_default();
        Ok(Platform {
            id: format!("{}-{}", slug(manufacturer.name()), slug(&entry.key)),
            model: entry.key.clone(),
            name: entry.key.clone(),
            manufacturer,
            controller: f.controller,
            motors: f.nbr_motors,
            controllers: f.nbr_controllers,
            max_motor_abs: f.max_motor_abs,
            motor_max_perc: f.motor_max_perc,
            motors_direction: f.motors_direction,
            mapping_id: f.mapping_id,
            main_actuators: f.main_actuators,
            belt_tensioner_slot: f.belt_tensioner_motor.and_then(|s| usize::try_from(s).ok()),
            wheels,
            mapping,
            listed: true,
        })
    }
}

impl Platform {
    /// [`MotionConfig::unlisted`] of the built-in tables.
    pub fn unlisted(model: &str) -> Platform {
        MotionConfig::builtin().unlisted(model).expect("the built-in motion_models has 'None'")
    }

    /// The table `enhanced_type` selects from the platform's list of two: the index is an int
    /// (negative counts from the end) or a bool; an index error out of range, a type error for another type.
    pub(crate) fn table(&self, enhanced_type: &PyValue) -> Result<&[SlotMix], CueError> {
        let index = match enhanced_type {
            PyValue::Bool(b) => i128::from(*b),
            PyValue::Int(i) => *i,
            other if other.to_native().integer().is_some() => other.to_int().map_err(|_| {
                CueError::new(crate::error::Exception::IndexError, "cannot fit 'int' into an index-sized integer")
            })?,
            other => {
                return Err(CueError::type_error(format!(
                    "list indices must be integers or slices, not {}",
                    other.type_name()
                )))
            }
        };
        if index < isize::MIN as i128 || index > isize::MAX as i128 {
            return Err(CueError::new(
                crate::error::Exception::IndexError,
                "cannot fit 'int' into an index-sized integer",
            ));
        }
        let len = self.mapping.len() as i128;
        let index = if index < 0 { index + len } else { index };
        if !(0..len).contains(&index) {
            return Err(CueError::index_out_of_range());
        }
        Ok(&self.mapping[index as usize])
    }

    /// The row of `motor` as the mixing stage reads it: a missing row reads as an empty one, which is
    /// an index error at the first cell read.
    pub(crate) fn row(table: &[SlotMix], slot: usize) -> &SlotMix {
        table.get(slot).unwrap_or(&NO_ROW)
    }

    /// The mixing rows of `slot` for "enhanced g-forces" off (`false`) or on.
    pub fn slot_mix(&self, slot: usize, enhanced: bool) -> &SlotMix {
        Platform::row(&self.mapping[usize::from(enhanced)], slot)
    }

    pub fn max_motor_abs(&self) -> f64 {
        f64::from(self.max_motor_abs)
    }

    /// 5 % of `max_motor_abs`, truncated: the margin of the ±100 % output map and of wheel effects.
    pub fn margin(&self) -> f64 {
        (self.max_motor_abs() * 5.0 / 100.0).trunc()
    }

    /// `(max_motor_abs + 1) / 2`, truncated: the centre used by travel scaling.
    pub fn centre(&self) -> f64 {
        ((self.max_motor_abs() + 1.0) / 2.0).trunc()
    }

    /// `max_motor_abs * (100 - motor_max_perc) / 100`, truncated: the guard band of the rotary boost clamp.
    pub fn guard(&self) -> f64 {
        (self.max_motor_abs() * (100.0 - self.motor_max_perc) / 100.0).trunc()
    }

    /// Whether travel scaling scales `slot` with `motion_max_power` (else `motion_max_horizontal`).
    /// Without `main_actuators` every slot counts as a main actuator.
    pub fn scales_with_max_power(&self, slot: usize) -> bool {
        self.main_actuators.as_ref().is_none_or(|main| main.contains(&slot))
    }

    /// Whether the mixing stage replaces this slot's sway/surge/traction-loss shares with the tune's
    /// `motion_main_actuators_*` values. Without `main_actuators` no slot does.
    pub fn has_main_actuator_shares(&self, slot: usize) -> bool {
        self.main_actuators.as_ref().is_some_and(|main| main.contains(&slot))
    }

    /// Models whose idle/test heave uses the fixed range 5 (models `H4` and `P4`).
    pub fn has_fixed_idle_heave_range(&self) -> bool {
        matches!(self.model.as_str(), "H4" | "P4")
    }
}

/// The OpenRigCue identifiers of the built-in models (every model table entry except `'None'`), sorted.
pub fn builtin_ids() -> impl Iterator<Item = &'static str> {
    builtin_index().iter().map(|(id, _)| id.as_str())
}

/// (identifier, model table key) of every built-in model, sorted by identifier.
fn builtin_index() -> &'static [(String, String)] {
    static IDS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    IDS.get_or_init(|| {
        let mut ids: Vec<(String, String)> = motion_models()
            .iter()
            .filter(|m| m.key != "None")
            .filter_map(|m| MotionConfig::builtin().platform(&m.key)?.ok().map(|p| (p.id, m.key.clone())))
            .collect();
        ids.sort();
        ids
    })
}

/// A built-in platform by OpenRigCue identifier (for example `dof-reality-p3`).
pub fn builtin(id: &str) -> Option<Result<Platform, PlatformError>> {
    let key = &builtin_index().iter().find(|(i, _)| i == id)?.1;
    MotionConfig::builtin().platform(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(id: &str) -> Platform {
        builtin(id).expect("built in").expect("valid")
    }

    #[test]
    fn every_builtin_platform_loads() {
        let ids: Vec<_> = builtin_ids().collect();
        // 71 model table entries besides 'None'; the two STR-SCN6150 models are not among them.
        assert_eq!(ids.len(), 71);
        assert_eq!(motion_models().len(), 72);
        assert_eq!(motion_models()[0].key, "None");
        for id in ids {
            let p = load(id);
            assert_eq!(p.id, id);
            assert_eq!(p.name, p.model);
            assert_eq!(p.mapping.len(), 2);
            assert!(p.mapping.iter().all(|t| t.len() == SLOTS));
        }
    }

    #[test]
    fn motion_models_entries_keep_every_field() {
        let p3 = motion_model("P3").unwrap();
        assert_eq!(p3.fields.get("controller").and_then(|v| v.as_str()), Some("smc3"));
        assert_eq!(p3.fields.get("nav-image").and_then(|v| v.as_str()), Some("nav-motion.png"));
        assert!(p3.fields.get("name").is_none());
        assert!(motion_model("STR-SCN6150").is_none());
    }

    #[test]
    fn p3_matches_motion_models_and_mapping_3() {
        let p = load("dof-reality-p3");
        assert_eq!(p.model, "P3");
        assert_eq!(p.manufacturer, Manufacturer::DofReality);
        assert_eq!(p.max_motor_abs, 4095);
        assert_eq!(p.motors_direction, 1.0);
        assert_eq!(p.margin(), 204.0);
        assert_eq!(p.centre(), 2048.0);
        assert_eq!(p.guard(), 204.0);
        assert!(p.main_actuators.is_none());
        let w = p.wheels.as_ref().unwrap();
        assert_eq!(w.slots.get(Wheel::FrontRight), 1);
        assert_eq!(w.direction(Wheel::RearRight), -1);
        assert_eq!(w.shake_factor, 0.05);
        let s1 = p.slot_mix(1, true);
        assert_eq!(s1.direction_of(Axis::Roll), Some(Sign::Minus));
        assert_eq!(s1.direction_of(Axis::Sway), Some(Sign::Minus));
        assert_eq!(p.slot_mix(6, false).direction_of(Axis::Heave), Some(Sign::Plus));
        assert_eq!(p.slot_mix(7, false).direction_of(Axis::Heave), Some(Sign::Zero));
    }

    #[test]
    fn h6_deadzone_columns_follow_the_effects_list_order() {
        // Mapping 6, table 0, slot 1: the deadzone row is [30, 0, 0, 0, 50, 0, 0, 0] indexed by
        // the effect order roll, pitch, yaw, ...: the 30 applies to roll, the 50 to surge.
        let p = load("dof-reality-h6");
        let s1 = p.slot_mix(1, false);
        assert_eq!(s1.deadzone_of(Axis::Roll), Some(&TableInt::from(30)));
        assert_eq!(s1.deadzone_of(Axis::Pitch), Some(&TableInt::from(0)));
        assert_eq!(s1.deadzone_of(Axis::Surge), Some(&TableInt::from(50)));
        let e = p.slot_mix(2, true);
        assert_eq!(e.percentages[Axis::TractionLoss.index()], TableInt::from(200));
        assert_eq!(p.slot_mix(1, true).deadzone_of(Axis::Surge), Some(&TableInt::from(30)));
        assert!(p.wheels.as_ref().unwrap().directions.is_none());
    }

    #[test]
    fn enhanced_type_indexes_the_mapping_list() {
        let p = load("dof-reality-p3");
        assert!(p.table(&PyValue::Int(1)).is_ok());
        assert_eq!(p.table(&PyValue::Int(-1)).unwrap().as_ptr(), p.mapping[1].as_ptr());
        assert_eq!(p.table(&PyValue::Bool(false)).unwrap().as_ptr(), p.mapping[0].as_ptr());
        assert_eq!(p.table(&PyValue::Int(2)).unwrap_err().to_string(), "list index out of range");
        assert_eq!(
            p.table(&PyValue::Float(1.0)).unwrap_err().to_string(),
            "list indices must be integers or slices, not float"
        );
    }

    #[test]
    fn main_actuator_fallbacks_follow_the_two_readers() {
        // T1000-3M PRS200: main_actuators [0, 1, 3, 4] with only 3 motors.
        let p = load("prosimu-t1000-3m-prs200");
        assert!(p.scales_with_max_power(4));
        assert!(!p.scales_with_max_power(2));
        assert!(p.has_main_actuator_shares(3));
        let dof = load("dof-reality-h3");
        assert!(dof.scales_with_max_power(11));
        assert!(!dof.has_main_actuator_shares(0));
    }

    #[test]
    fn belt_and_heave_special_cases() {
        let p = load("pt-actuator-7-actuators-2-front-2-rear-tl-surge-belt-canbus");
        assert_eq!(p.belt_tensioner_slot, Some(6));
        assert!(load("dof-reality-p4").has_fixed_idle_heave_range());
        assert!(!load("dof-reality-p3").has_fixed_idle_heave_range());
        let yaw = load("yaw-vr-yaw3");
        assert_eq!(yaw.guard(), 0.0);
        assert_eq!(yaw.centre(), 32768.0);
        assert_eq!(yaw.margin(), 3276.0);
    }

    #[test]
    fn simtech_models_are_not_built_in() {
        for p in builtin_ids().map(load) {
            assert_ne!(p.manufacturer, Manufacturer::SimTechRevolution, "{}", p.id);
            assert!(p.listed);
        }
    }

    #[test]
    fn an_unlisted_model_takes_the_none_model_fallbacks() {
        let p = Platform::unlisted("STR-SCN6150");
        assert!(!p.listed);
        assert_eq!(p.model, "STR-SCN6150");
        assert_eq!((p.mapping_id, p.max_motor_abs, p.motors_direction), (2, 1023, 1.0));
        assert_eq!(p.manufacturer, Manufacturer::DofReality);
        assert!(p.main_actuators.is_none() && p.belt_tensioner_slot.is_none() && p.wheels.is_none());
        assert_eq!(p.margin(), 51.0);
        assert_eq!(p.mapping, load("dof-reality-p2").mapping);
        assert!(!p.has_fixed_idle_heave_range());
    }

    #[test]
    fn select_follows_the_model_setting() {
        let config = MotionConfig::builtin();
        assert_eq!(config.select("None").unwrap(), None);
        assert!(config.select("P3").unwrap().unwrap().listed);
        assert!(!config.select("STR-SCN6150").unwrap().unwrap().listed);
    }

    #[test]
    fn default_refresh_rates_follow_motion_manufacturers() {
        assert_eq!(Manufacturer::DofReality.default_refresh_rate(HostOs::Linux), Some(30));
        assert_eq!(Manufacturer::None.default_refresh_rate(HostOs::Linux), Some(30));
        assert_eq!(Manufacturer::Prosimu.default_refresh_rate(HostOs::Windows), Some(500));
        assert_eq!(Manufacturer::PtActuator.default_refresh_rate(HostOs::Windows), Some(400));
        assert_eq!(Manufacturer::Prosimu.default_refresh_rate(HostOs::Linux), None);
        assert_eq!(Manufacturer::YawVr.default_refresh_rate(HostOs::Other), None);
        assert_eq!(Manufacturer::SimTechRevolution.default_refresh_rate(HostOs::Windows), None);
    }
}
