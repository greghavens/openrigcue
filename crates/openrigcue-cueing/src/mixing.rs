//! Per-axis normalisation and per-slot mixing: the computational core of the mixing stage.
//!
//! Each output slot reads its shares and weights before computing axis percentages ([`normalize`]),
//! then applies deadzone caps and the signed sum ([`slot_formulas_with_normalize`], [`slot_power`]).
//! Keeping these reads within the slot lets live profile changes take effect and fixes the failure order.
//!
//! Every tune and configuration value is read at the point of use, with its conversion
//! (integer or float) and the error a failed conversion gives there. Values are numbers ([`Num`]): the sway
//! and surge curves of a negative telemetry maximum with a fractional curving factor are complex,
//! and that complex value travels through the formulas, the smoothing history and the signal
//! sum until a comparison or the output map rejects it.

use std::cmp::Ordering;

use crate::axis::{Axis, PerAxis};
use crate::error::{functions, mh, CueError};
use crate::platform::{Platform, SlotMix, EFFECTS_ORDER};
use crate::pyint::Exact;
use crate::pymath::{power, rescale, trunc, Num};
use crate::pyvalue::{Cmp, Op, PyValue};
use crate::settings::ClipFactors;
use crate::views::{config_float, config_str, game_item, game_item2, Sources};

/// The mixing stage's trace frame at `line`.
pub(crate) const fn cmp_frame(line: u32) -> crate::error::Frame {
    mh("calculate_motor_power", line)
}

/// The range map's argument conversion: each argument becomes a double.
pub(crate) fn map_arg(value: &PyValue) -> Result<f64, CueError> {
    value.as_double().map_err(|e| e.at(functions("map", 204)))
}

/// Heave range and gain chosen from the running game: real games use 30 % of the heave,
/// `test`/`none`/`emulator` the full heave, and the H4/P4 then also the fixed range 5.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HeaveScale {
    pub maximum: PyValue,
    pub factor: f64,
}

impl HeaveScale {
    pub fn select(sources: &Sources<'_>, platform: &Platform) -> Result<Self, CueError> {
        let maximum = game_item2(sources.game, "max_telemetry_vars", "heave").map_err(|e| e.at(cmp_frame(329)))?;
        let game = sources.config.current_game();
        if !matches!(game.as_deref().unwrap_or(sources.game_running), "test" | "none" | "emulator") {
            return Ok(HeaveScale { maximum, factor: 0.3 });
        }
        let fixed = match sources.config.current_model() {
            Some(value) => {
                let value = value.map_err(|e| e.at(cmp_frame(334)))?;
                value.equals_str("H4") || value.equals_str("P4")
            }
            None => platform.has_fixed_idle_heave_range(),
        };
        let maximum = if fixed { PyValue::Int(5) } else { maximum };
        Ok(HeaveScale { maximum, factor: 1.0 })
    }
}

/// Axis percentages of one frame, before the per-slot deadzone caps.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Normalized {
    pub percent: PerAxis<Num>,
    /// The surge value after its clip stage (clipped, or the input where the clip failed): its sign
    /// selects the acceleration or deceleration weight.
    pub surge: f64,
    /// The roll value after its clip stage (clipped, or 0 where the stage failed): the real-heave
    /// test.
    pub roll: f64,
    pub native_surge: PyValue,
    pub native_roll: PyValue,
}

/// The clip: limit `value` to `±|clip · maximum|`. `None` for
/// a maximum that is not a number, which the callers turn into 0 %.
#[cfg(test)]
pub(crate) fn clip(value: f64, maximum: &PyValue, clip_factor: f64) -> Option<f64> {
    let maximum = maximum.as_double().ok()?;
    let sign = if value < 0.0 { -1.0 } else { 1.0 };
    let limit = (clip_factor * maximum).abs();
    Some(if value.abs() > limit { limit * sign } else { value })
}

/// The sway/surge curve: `v^e / (2 · max^e) · 100` for a positive
/// value, else `-1 · (|v|^e / (2 · max^e) · 100)`. `None` where it fails (a division by zero,
/// an overflow, a non-number); a negative maximum with a fractional exponent gives a complex percentage.
fn curved_percent(value: f64, maximum: &PyValue, exponent: f64) -> Option<Num> {
    let maximum = maximum.as_double().ok()?;
    let numerator = power(value.abs(), exponent).ok()?;
    let denominator = Num::Int(2).mul(power(maximum, exponent).ok()?);
    let magnitude = numerator.div(denominator).ok()?.mul(Num::Int(100));
    Some(if value > 0.0 { magnitude } else { Num::Int(-1).mul(magnitude) })
}

/// The tune's `<axis>_type`: `'angle'` selects the `max_telemetry_vars` entry `<axis>`, anything else
/// the entry `<axis>_rate`; the type itself falls back to `'angle'` where it cannot be read.
fn rotation_maximum(sources: &Sources<'_>, axis: &str, lines: (u32, u32)) -> Result<PyValue, CueError> {
    let angle =
        game_item2(sources.game, "motion_effects", &format!("{axis}_type")).map_or(true, |t| t.equals_str("angle"));
    let (key, line) = if angle { (axis.to_owned(), lines.0) } else { (format!("{axis}_rate"), lines.1) };
    game_item2(sources.game, "max_telemetry_vars", &key).map_err(|e| e.at(cmp_frame(line)))
}

/// The clip and percentage stage for all eight axes.
///
/// `inputs` are the frame's forces (roll, pitch, yaw, lateral acceleration, longitudinal
/// acceleration, vertical acceleration, lateral velocity, front traction loss), by axis.
pub(crate) fn normalize(
    inputs: &PerAxis<f64>,
    sources: &Sources<'_>,
    clips: &ClipFactors,
    heave: impl FnOnce() -> Result<HeaveScale, CueError>,
) -> Result<Normalized, CueError> {
    normalize_native(&PerAxis::from_fn(|axis| PyValue::Float(inputs[axis])), sources, clips, heave)
}

fn clip_native(value: &PyValue, maximum: &PyValue, factor: f64) -> Result<PyValue, CueError> {
    let sign = if value.compare(Cmp::Lt, &PyValue::Int(0))? { -1 } else { 1 };
    let limit = PyValue::Float(factor).binop(Op::Mul, maximum)?.abs()?;
    if value.abs()?.compare(Cmp::Gt, &limit)? {
        limit.binop(Op::Mul, &PyValue::Int(sign))
    } else {
        Ok(value.clone())
    }
}
fn linear_native(value: &PyValue, maximum: &PyValue) -> Option<Num> {
    let span = PyValue::Int(2).binop(Op::Mul, maximum).ok()?;
    value.binop(Op::Div, &span).ok()?.binop(Op::Mul, &PyValue::Int(100)).ok()?.as_double().ok().map(Num::Real)
}
pub(crate) fn normalize_native(
    inputs: &PerAxis<PyValue>,
    sources: &Sources<'_>,
    clips: &ClipFactors,
    heave: impl FnOnce() -> Result<HeaveScale, CueError>,
) -> Result<Normalized, CueError> {
    let max_pitch = rotation_maximum(sources, "pitch", (257, 259))?;
    let max_roll = rotation_maximum(sources, "roll", (267, 269))?;
    let maximum = |key: &str| game_item2(sources.game, "max_telemetry_vars", key).ok();
    let mut percent = PerAxis::splat(Num::Int(0));
    let linear = |axis: Axis, max: Option<&PyValue>| -> Option<(PyValue, Num)> {
        let max = max?;
        let v = clip_native(&inputs[axis], max, clips.0[axis]).ok()?;
        let p = linear_native(&v, max)?;
        Some((v, p))
    };
    if let Some((_, p)) = linear(Axis::Pitch, Some(&max_pitch)) {
        percent[Axis::Pitch] = p;
    }
    let native_roll = match linear(Axis::Roll, Some(&max_roll)) {
        Some((value, p)) => {
            percent[Axis::Roll] = p;
            value
        }
        None => PyValue::Int(0),
    };
    if let Some((_, p)) = linear(Axis::Yaw, maximum("yaw").as_ref()) {
        percent[Axis::Yaw] = p;
    }
    let curve = |axis: Axis, max: Option<PyValue>, key: &str, value: &mut PyValue| -> Option<Num> {
        let max = max?;
        *value = clip_native(value, &max, clips.0[axis]).ok()?;
        let exponent = config_float(sources.config, "MOTIONBOX", key).ok()?;
        curved_percent(value.as_double().ok()?, &max, exponent)
    };
    let mut sway = inputs[Axis::Sway].clone();
    if let Some(p) = curve(Axis::Sway, maximum("sway"), "sway_curving_factor", &mut sway) {
        percent[Axis::Sway] = p;
    }
    let mut native_surge = inputs[Axis::Surge].clone();
    let surge_max = match native_surge.to_native().py_ge(&openrigcue_model::native_value::LegacyValue::Int(0)) {
        Ok(true) => maximum("surge_acceleration"),
        Ok(false) => maximum("surge_deceleration"),
        Err(_) => None,
    };
    if let Some(p) = curve(Axis::Surge, surge_max, "surge_curving_factor", &mut native_surge) {
        percent[Axis::Surge] = p;
    }
    let heave = heave()?;
    if let Ok(value) = clip_native(&inputs[Axis::Heave], &heave.maximum, clips.0[Axis::Heave]) {
        if let Ok(value) = value.binop(Op::Mul, &PyValue::Float(heave.factor)) {
            if let Some(p) = linear_native(&value, &heave.maximum) {
                percent[Axis::Heave] = p;
            }
        }
    }
    for (axis, key) in [(Axis::TractionLoss, "traction_loss"), (Axis::TractionLossFront, "traction_loss_front")] {
        if let Some((_, p)) = linear(axis, maximum(key).as_ref()) {
            percent[axis] = p;
        }
    }
    Ok(Normalized {
        percent,
        surge: native_surge.as_double().unwrap_or(f64::NAN),
        roll: native_roll.as_double().unwrap_or(f64::NAN),
        native_surge,
        native_roll,
    })
}

/// An axis share of one slot: a table cell (an int) or an override from the tune.
#[derive(Debug, Clone, PartialEq)]
enum Share {
    /// The percentages table cell of the slot and axis.
    Table(crate::pyint::TableInt),
    /// A share in percent from the tune (belt tensioner, main actuators).
    Tune(PyValue),
}

/// Overflow error text of an integer division to a float (`percentages / 100`).
const DIVISION_OVERFLOW: &str = "integer division result too large for a float";
/// Overflow error text of an integer-to-float conversion (a capped percentage times its weight).
const CONVERSION_OVERFLOW: &str = "int too large to convert to float";

impl Share {
    /// `share / 100`.
    fn fraction(&self) -> Result<f64, CueError> {
        match self {
            Share::Table(cell) => cell.hundredth().ok_or_else(|| CueError::overflow(DIVISION_OVERFLOW)),
            Share::Tune(v) => match v.binop(Op::Div, &PyValue::Int(100))? {
                PyValue::Float(f) => Ok(f),
                other => other.as_double(),
            },
        }
    }
}

/// Trace line labels of the eight percentage reads, by axis.
const SHARE_LINES: PerAxis<u32> = PerAxis([197, 198, 199, 200, 208, 213, 214, 215]);

/// The percentages cell of a motor and axis; an index error for a short row.
fn table_share(mix: &SlotMix, axis: Axis) -> Result<Share, CueError> {
    mix.percentages
        .get(axis.index())
        .map(|cell| Share::Table(cell.clone()))
        .ok_or_else(|| CueError::index_out_of_range().at(cmp_frame(SHARE_LINES[axis])))
}

/// The axis shares of one slot after the belt-tensioner and main-actuator overrides.
fn slot_shares(
    mix: &SlotMix,
    slot: usize,
    platform: &Platform,
    sources: &Sources<'_>,
) -> Result<PerAxis<Share>, CueError> {
    let mut shares = PerAxis::splat(Share::Tune(PyValue::Int(0)));
    for axis in [Axis::Pitch, Axis::Roll, Axis::Yaw, Axis::Sway] {
        shares[axis] = table_share(mix, axis)?;
    }
    let belt = match sources.model_field("belt_tensioner_motor") {
        Ok(Some(value)) => value.equals(&PyValue::Int(slot as i128)),
        Ok(None) => platform.belt_tensioner_slot == Some(slot),
        Err(_) => false,
    };
    shares[Axis::Surge] = table_share(mix, Axis::Surge)?;
    if belt {
        // The tune's `motion_belt_tensioner` mapped from 0..100 onto 0..1000, truncated.
        let at = |e: CueError| e.at(cmp_frame(211));
        let value = game_item(sources.game, "motion_belt_tensioner").map_err(at)?;
        let mapped = rescale(map_arg(&value).map_err(at)?, 0.0, 100.0, 0.0, 1000.0);
        shares[Axis::Surge] = Share::Tune(PyValue::Int(trunc(mapped).map_err(at)? as i128));
    }
    for axis in [Axis::Heave, Axis::TractionLoss, Axis::TractionLossFront] {
        shares[axis] = table_share(mix, axis)?;
    }
    let has_main = match sources.model_field("main_actuators") {
        Ok(Some(_)) => {
            let main = sources.model_field("main_actuators").map_err(|e| e.at(cmp_frame(224)))?;
            matches!(main,Some(PyValue::List(ref values))if values.iter().any(|v|v.equals(&PyValue::Int(slot as i128))))
        }
        Ok(None) => platform.has_main_actuator_shares(slot),
        Err(_) => false,
    };
    if has_main {
        for (axis, key, line) in [
            (Axis::Sway, "motion_main_actuators_sway", 225),
            (Axis::Surge, "motion_main_actuators_surge", 226),
            (Axis::TractionLoss, "motion_main_actuators_traction_loss", 227),
            (Axis::TractionLossFront, "motion_main_actuators_traction_loss_front", 228),
        ] {
            let value = game_item(sources.game, key).map_err(|e| e.at(cmp_frame(line)))?;
            shares[axis] = Share::Tune(value.into_owned());
        }
    }
    Ok(shares)
}

/// The weights of one slot: the effect's `motion_effects` gain times its integer `motion_switch`,
/// `/ 20 * (share / 100)`, in evaluation order. Both surge weights are computed.
struct Weights {
    axes: PerAxis<f64>,
    surge_deceleration: f64,
}

fn weight(sources: &Sources<'_>, effect: &str, share: &Share, line: u32) -> Result<f64, CueError> {
    let at = |e: CueError| e.at(cmp_frame(line));
    let gain = game_item2(sources.game, "motion_effects", effect).map_err(at)?;
    let switch = game_item2(sources.game, "motion_switch", effect).map_err(at)?.to_integer_value().map_err(at)?;
    let scaled = gain.binop(Op::Mul, &switch).and_then(|v| v.binop(Op::Div, &PyValue::Int(20))).map_err(at)?;
    let fraction = share.fraction().map_err(|e| e.at(cmp_frame(line + 1)))?;
    let product = scaled.binop(Op::Mul, &PyValue::Float(fraction)).map_err(at)?;
    product.as_double().map_err(at)
}

fn slot_weights(shares: &PerAxis<Share>, sources: &Sources<'_>) -> Result<Weights, CueError> {
    let mut axes = PerAxis::splat(0.0);
    axes[Axis::Pitch] = weight(sources, "pitch", &shares[Axis::Pitch], 231)?;
    axes[Axis::Roll] = weight(sources, "roll", &shares[Axis::Roll], 233)?;
    axes[Axis::Yaw] = weight(sources, "yaw", &shares[Axis::Yaw], 235)?;
    axes[Axis::Sway] = weight(sources, "sway", &shares[Axis::Sway], 237)?;
    axes[Axis::Surge] = weight(sources, "surge_acceleration", &shares[Axis::Surge], 239)?;
    let surge_deceleration = weight(sources, "surge_deceleration", &shares[Axis::Surge], 241)?;
    axes[Axis::Heave] = weight(sources, "heave", &shares[Axis::Heave], 243)?;
    axes[Axis::TractionLoss] = weight(sources, "traction_loss", &shares[Axis::TractionLoss], 245)?;
    axes[Axis::TractionLossFront] = weight(sources, "traction_loss_front", &shares[Axis::TractionLossFront], 247)?;
    Ok(Weights { axes, surge_deceleration })
}

/// An axis percentage after the deadzone stage: unchanged, or replaced by the int `±(100 − deadzone)`.
#[derive(Debug, Clone, PartialEq)]
enum Percent {
    Value(Num),
    Capped(Exact),
}

impl Percent {
    /// `|percent| < minimum`.
    fn below(&self, minimum: &PyValue) -> Result<bool, CueError> {
        let magnitude = match self {
            Percent::Value(Num::Int(i)) => PyValue::Int(*i).abs()?,
            Percent::Value(v) => PyValue::Float(v.magnitude().ok_or_else(complex_abs_overflow)?),
            Percent::Capped(c) => {
                return c
                    .native()
                    .py_abs()
                    .and_then(|value| value.py_lt(&minimum.to_native()))
                    .map_err(crate::pyvalue::native_error);
            }
        };
        magnitude.compare(Cmp::Lt, minimum)
    }

    /// `percent * weight`; an int percentage is converted to a float first.
    fn weighted(&self, weight: f64) -> Result<Num, CueError> {
        let value = match self {
            Percent::Value(v) => *v,
            Percent::Capped(c) => Num::Real(c.to_float().ok_or_else(|| CueError::overflow(CONVERSION_OVERFLOW))?),
        };
        Ok(value.mul(Num::Real(weight)))
    }
}

fn complex_abs_overflow() -> CueError {
    CueError::overflow("absolute value too large")
}

/// The slot's deadzone: when `100 − deadzone` differs from 100 and `|percent| > 100 − deadzone`,
/// the percent becomes the int `±(100 − deadzone)` by its sign. Both comparisons are exact; ordering a
/// complex percent is a type error.
fn cap_by_deadzone(percent: Num, ceiling: Exact) -> Result<Percent, CueError> {
    if ceiling.is(100.0) {
        return Ok(Percent::Value(percent));
    }
    let magnitude = percent.magnitude().ok_or_else(|| complex_abs_overflow().at(cmp_frame(369)))?;
    if ceiling.compare_float(magnitude) != Some(Ordering::Greater) {
        return Ok(Percent::Value(percent));
    }
    match percent {
        Num::Int(p) => Ok(Percent::Capped(if p > 0 { ceiling } else { ceiling.neg() })),
        Num::Real(p) => Ok(Percent::Capped(if p > 0.0 { ceiling } else { ceiling.neg() })),
        Num::Complex(_) => {
            Err(CueError::type_error("'>' not supported between instances of 'complex' and 'int'").at(cmp_frame(370)))
        }
    }
}

/// The start of the mixing stage: the tune's `motion_enhanced_gforces` value.
pub(crate) fn enhanced_type(sources: &Sources<'_>) -> Result<PyValue, CueError> {
    game_item(sources.game, "motion_enhanced_gforces").map(|v| v.into_owned()).map_err(|e| e.at(cmp_frame(189)))
}

/// The mixing table the enhanced type selects, as the first percentage read finds it.
pub(crate) fn slot_table<'p>(platform: &'p Platform, enhanced: &PyValue) -> Result<&'p [SlotMix], CueError> {
    platform.table(enhanced).map_err(|e| e.at(cmp_frame(197)))
}

/// The per-slot part of the mixing stage up to the smoothing stage: the
/// formula value of every axis, the stages in a fixed order, every value read at the point of use.
#[cfg(test)]
pub(crate) fn slot_formulas(
    normalized: &Result<Normalized, CueError>,
    mix: &SlotMix,
    slot: usize,
    platform: &Platform,
    sources: &Sources<'_>,
) -> Result<PerAxis<Num>, CueError> {
    slot_formulas_with_normalize(|| normalized.clone(), mix, slot, platform, sources)
}

/// The production path evaluates normalization after this slot's shares and weights, so a live
/// profile edit is read in slot order.
pub(crate) fn slot_formulas_with_normalize(
    normalize: impl FnOnce() -> Result<Normalized, CueError>,
    mix: &SlotMix,
    slot: usize,
    platform: &Platform,
    sources: &Sources<'_>,
) -> Result<PerAxis<Num>, CueError> {
    let shares = slot_shares(mix, slot, platform, sources)?;
    let weights = slot_weights(&shares, sources)?;
    let normalized = normalize()?;
    let mut percent = normalized.percent.map(Percent::Value);
    for (index, axis) in EFFECTS_ORDER.into_iter().enumerate() {
        let cell = mix.deadzone.get(index).ok_or_else(|| CueError::index_out_of_range().at(cmp_frame(367)))?;
        let Percent::Value(value) = percent[axis] else { unreachable!("each axis is capped once") };
        percent[axis] = cap_by_deadzone(value, cell.complement())?;
    }
    let mut formulas = PerAxis::splat(Num::Int(0));
    for (axis, line) in [(Axis::Pitch, 376), (Axis::Roll, 377), (Axis::Yaw, 378), (Axis::Sway, 379)] {
        formulas[axis] = percent[axis].weighted(weights.axes[axis]).map_err(|e| e.at(cmp_frame(line)))?;
    }
    let surge_weight = if normalized
        .native_surge
        .to_native()
        .py_ge(&openrigcue_model::native_value::LegacyValue::Int(0))
        .map_err(crate::pyvalue::native_error)
        .map_err(|e| e.at(cmp_frame(381)))?
    {
        weights.axes[Axis::Surge]
    } else {
        weights.surge_deceleration
    };
    formulas[Axis::Surge] = percent[Axis::Surge].weighted(surge_weight).map_err(|e| e.at(cmp_frame(386)))?;
    let real_heave = if normalized.native_roll.compare(Cmp::Lt, &PyValue::Int(-1)).map_err(|e| e.at(cmp_frame(388)))?
        || normalized.native_roll.compare(Cmp::Gt, &PyValue::Int(1)).map_err(|e| e.at(cmp_frame(388)))?
    {
        // Only a stored boolean false enables real heave here. Stored strings and
        // numeric zero do not count as false; an in-memory bool can.
        matches!(
            config_str(sources.config, sources.manufacturer, "real_heave")
                .map_err(|e| e.at(cmp_frame(388)))?
                .to_native()
                .resolved(),
            openrigcue_model::native_value::LegacyValue::Bool(false)
        )
    } else {
        false
    };
    let heave = percent[Axis::Heave]
        .weighted(weights.axes[Axis::Heave])
        .map_err(|e| e.at(cmp_frame(if real_heave { 389 } else { 392 })))?;
    formulas[Axis::Heave] =
        if real_heave { heave.div(Num::Real(normalized.roll.abs())).map_err(|e| e.at(cmp_frame(389)))? } else { heave };
    for (axis, key, lines) in [
        (Axis::TractionLoss, "min_traction_loss_activation", (397, 401)),
        (Axis::TractionLossFront, "min_traction_loss_front_activation", (404, 408)),
    ] {
        let at = |e: CueError| e.at(cmp_frame(lines.0));
        let minimum = game_item(sources.game, key).map_err(at)?;
        formulas[axis] = if percent[axis].below(&minimum).map_err(at)? {
            Num::Int(0)
        } else {
            percent[axis].weighted(weights.axes[axis]).map_err(|e| e.at(cmp_frame(lines.1)))?
        };
    }
    Ok(formulas)
}

/// The signal sum at the end of the mixing stage: the signed axis values
/// added left to right in table order (`'-'` multiplies by −1, `'0'` gives the integer 0). A missing
/// cell is an index error at the read of term `k`; a cell other than `+`, `-`, `0` gives
/// no value, and the addition with it is a type error.
pub(crate) fn slot_power(mix: &SlotMix, formulas: &PerAxis<Num>) -> Result<Num, CueError> {
    let signal = |k: usize, axis: Axis| -> Result<Option<Num>, CueError> {
        let cell = mix
            .direction
            .get(axis.index())
            .ok_or_else(|| CueError::index_out_of_range().at(mh("get_signal", 82)).at(cmp_frame(447 + k as u32)))?;
        Ok(cell.map(|sign| sign.apply(formulas[axis])))
    };
    let name = |v: &Option<Num>| v.map_or("NoneType", |n| n.type_name());
    let mut total = signal(0, Axis::Pitch)?;
    for (k, &axis) in Axis::ALL.iter().enumerate().skip(1) {
        let value = signal(k, axis)?;
        total = match (total, value) {
            (Some(a), Some(b)) => Some(a.add(b)),
            (a, b) => {
                return Err(CueError::type_error(format!(
                    "unsupported operand type(s) for +: '{}' and '{}'",
                    name(&a),
                    name(&b)
                ))
                .at(cmp_frame(446 + k as u32)))
            }
        };
    }
    // A lone missing value cannot reach here: 7 additions follow the first term.
    Ok(total.unwrap_or(Num::Int(0)))
}

/// The output map of the stream and test paths: slot polarity (slots 1 and 4 are
/// inverted, at trace line `direction_line`), then `power * motors_direction` mapped from [-100, 100] onto
/// `[max_motor, max_motor_abs - max_motor]` and truncated, at `map_line`. A complex power fails in the map (a type error).
pub(crate) fn command_position(
    power: Num,
    slot: usize,
    platform: &Platform,
    caller: &'static str,
    map_line: u32,
) -> Result<f64, CueError> {
    let slot_polarity = if slot == 1 || slot == 4 { -1 } else { 1 };
    let power = power.mul(Num::Int(slot_polarity)).mul(Num::Real(platform.motors_direction));
    let value = match power {
        Num::Int(i) => i as f64,
        Num::Real(x) => x,
        Num::Complex(_) => {
            return Err(CueError::type_error("must be real number, not complex")
                .at(functions("map", 204))
                .at(mh(caller, map_line)))
        }
    };
    let margin = platform.margin();
    let mapped = rescale(value, -100.0, 100.0, margin, platform.max_motor_abs() - margin);
    trunc(mapped).map_err(|e| e.at(mh(caller, map_line)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{builtin, Sign};
    use crate::pyint::{PyInt, TableInt};
    use crate::settings::MotionSettings;
    use crate::tune::{MotionTune, RotationInput};
    use crate::views::testing::{raw_config, raw_game, set_game};
    use crate::views::{ConfigView, GameConfigView, RawGameConfig};

    fn p3() -> Platform {
        builtin("dof-reality-p3").unwrap().unwrap()
    }

    fn inputs(f: impl Fn(Axis) -> f64) -> PerAxis<f64> {
        PerAxis::from_fn(f)
    }

    fn src<'a>(config: &'a dyn ConfigView, game: &'a dyn GameConfigView) -> Sources<'a> {
        Sources { config, game, manufacturer: "DOF REALITY", game_running: "dirtrally2" }
    }

    fn real_game() -> Result<HeaveScale, CueError> {
        Ok(HeaveScale { maximum: PyValue::Float(5.0), factor: 0.3 })
    }

    fn re(n: Num) -> f64 {
        match n {
            Num::Real(x) => x,
            Num::Int(i) => i as f64,
            Num::Complex(c) => panic!("complex {c:?}"),
        }
    }

    /// A slot that receives nothing (every direction `'0'`, shares 100, deadzone 0).
    fn silent() -> SlotMix {
        SlotMix {
            direction: vec![Some(Sign::Zero); Axis::COUNT],
            percentages: vec![TableInt::from(100); Axis::COUNT],
            deadzone: vec![TableInt::from(0); Axis::COUNT],
        }
    }

    fn normalized(percent: f64) -> Normalized {
        Normalized {
            percent: PerAxis::splat(Num::Real(percent)),
            surge: 1.0,
            roll: 0.0,
            native_surge: PyValue::Float(1.0),
            native_roll: PyValue::Float(0.0),
        }
    }

    #[test]
    fn inject_clip_factor_limits_symmetrically() {
        let five = PyValue::Float(5.0);
        assert_eq!(clip(7.0, &five, 1.0), Some(5.0));
        assert_eq!(clip(-7.0, &five, 1.0), Some(-5.0));
        assert_eq!(clip(5.0, &five, 1.0), Some(5.0)); // boundary: not strictly greater
        assert_eq!(clip(-7.0, &PyValue::Int(-5), 1.0), Some(-5.0)); // |clip · max|
        assert_eq!(clip(-7.0, &PyValue::from("5"), 1.0), None); // a text maximum fails
    }

    #[test]
    fn calculate_motor_power_percentages() {
        let (s, t) = (MotionSettings::default(), MotionTune::default());
        let n = normalize(
            &inputs(|a| if a == Axis::Heave { 2.0 } else { 2.5 }),
            &src(&s, &t),
            &ClipFactors::default(),
            real_game,
        )
        .unwrap();
        // 2.5 / (2 · 5) · 100 = 25 %; heave 2 · 0.3 / 10 · 100 = 6 %.
        assert_eq!(re(n.percent[Axis::Pitch]), 25.0);
        assert_eq!(re(n.percent[Axis::Sway]), 25.0);
        assert_eq!(re(n.percent[Axis::Surge]), 25.0);
        assert!((re(n.percent[Axis::Heave]) - 6.0).abs() < 1e-12);
        assert_eq!((n.surge, n.roll), (2.5, 2.5));
    }

    #[test]
    fn calculate_motor_power_curving_and_surge_range() {
        let mut tune = MotionTune::default();
        tune.maxima.surge_acceleration = 4.0;
        tune.maxima.surge_deceleration = 8.0;
        let s = MotionSettings { surge_curving_factor: 2.0, sway_curving_factor: 2.0, ..MotionSettings::default() };
        let n = normalize(
            &inputs(|a| if a == Axis::Surge { -4.0 } else { 0.0 }),
            &src(&s, &tune),
            &ClipFactors::default(),
            real_game,
        )
        .unwrap();
        // Negative surge uses surge_deceleration: -(16 / (2 · 64) · 100) = -12.5.
        assert_eq!(re(n.percent[Axis::Surge]), -12.5);
        assert!(n.surge < 0.0);
        // sway == 0 takes the negative branch: -1 · 0 = -0.0.
        assert_eq!(re(n.percent[Axis::Sway]), 0.0);
        assert!(re(n.percent[Axis::Sway]).is_sign_negative());
    }

    #[test]
    fn calculate_motor_power_failed_try_blocks_give_the_int_zero() {
        let mut tune = MotionTune::default();
        tune.maxima.pitch = 0.0;
        tune.maxima.sway = 0.0;
        tune.maxima.traction_loss = 0.0;
        let s = MotionSettings::default();
        let n = normalize(&inputs(|_| 3.0), &src(&s, &tune), &ClipFactors::default(), || {
            Ok(HeaveScale { maximum: PyValue::Float(0.0), factor: 1.0 })
        })
        .unwrap();
        for axis in [Axis::Pitch, Axis::Sway, Axis::Heave, Axis::TractionLoss] {
            assert_eq!(n.percent[axis], Num::Int(0), "{axis:?}");
        }
        assert_eq!(re(n.percent[Axis::Roll]), 30.0);

        // A missing key of a guarded stage (yaw, sway, surge, traction loss) or a malformed curving factor: 0 %.
        let mut raw = raw_game(&MotionTune::default());
        set_game(&mut raw, &["max_telemetry_vars", "yaw"], None);
        set_game(&mut raw, &["max_telemetry_vars", "roll"], Some(PyValue::from("5")));
        let mut config = raw_config(&s, "DOF REALITY");
        config.set("MOTIONBOX", "sway_curving_factor", "1,5");
        let n = normalize(&inputs(|_| 3.0), &src(&config, &raw), &ClipFactors::default(), real_game).unwrap();
        assert_eq!(n.percent[Axis::Yaw], Num::Int(0));
        assert_eq!(n.percent[Axis::Sway], Num::Int(0));
        // The failed roll stage also resets the roll value used by the real-heave test.
        assert_eq!((n.percent[Axis::Roll], n.roll), (Num::Int(0), 0.0));
        assert_eq!(re(n.percent[Axis::Surge]), 30.0);
    }

    #[test]
    fn calculate_motor_power_maxima_read_outside_a_try_raise() {
        let s = MotionSettings::default();
        let tune = MotionTune { roll_input: RotationInput::Rate, ..MotionTune::default() };
        let err = normalize(&inputs(|_| 1.0), &src(&s, &tune), &ClipFactors::default(), real_game).unwrap_err();
        assert_eq!(err.to_string(), "'roll_rate'");
        assert_eq!(err.frames().next().map(|f| f.line), Some(269));
        let mut tune = tune;
        tune.maxima.roll_rate = Some(60.0);
        let n = normalize(&inputs(|_| 6.0), &src(&s, &tune), &ClipFactors::default(), real_game).unwrap();
        assert_eq!(re(n.percent[Axis::Roll]), 5.0);
        // pitch_type other than 'angle' selects the rate maximum; a missing pitch_type is 'angle'.
        let mut raw = raw_game(&MotionTune::default());
        set_game(&mut raw, &["motion_effects", "pitch_type"], Some(PyValue::from("Angle")));
        let err = normalize(&inputs(|_| 1.0), &src(&s, &raw), &ClipFactors::default(), real_game).unwrap_err();
        assert_eq!((err.to_string(), err.frames().next().map(|f| f.line)), ("'pitch_rate'".into(), Some(259)));
        set_game(&mut raw, &["motion_effects", "pitch_type"], None);
        assert!(normalize(&inputs(|_| 1.0), &src(&s, &raw), &ClipFactors::default(), real_game).is_ok());
        // The heave maximum is read unguarded: a failure is an error.
        set_game(&mut raw, &["max_telemetry_vars", "heave"], None);
        let p = p3();
        let sources = src(&s, &raw);
        let err = normalize(&inputs(|_| 1.0), &sources, &ClipFactors::default(), || HeaveScale::select(&sources, &p))
            .unwrap_err();
        assert_eq!((err.to_string(), err.frames().next().map(|f| f.line)), ("'heave'".into(), Some(329)));
    }

    #[test]
    fn calculate_motor_power_heave_branch() {
        let p4 = builtin("dof-reality-p4").unwrap().unwrap();
        let s = MotionSettings::default();
        let mut tune = MotionTune::default();
        tune.maxima.heave = 2.0;
        let at = |game| Sources { game_running: game, ..src(&s, &tune) };
        assert_eq!(HeaveScale::select(&at("test"), &p4), Ok(HeaveScale { maximum: PyValue::Int(5), factor: 1.0 }));
        assert_eq!(
            HeaveScale::select(&at("none"), &p3()),
            Ok(HeaveScale { maximum: PyValue::Float(2.0), factor: 1.0 })
        );
        assert_eq!(
            HeaveScale::select(&at("dirtrally2"), &p4),
            Ok(HeaveScale { maximum: PyValue::Float(2.0), factor: 0.3 })
        );
    }

    #[test]
    fn calculate_motor_power_deadzone_and_traction_activation() {
        let h6 = builtin("dof-reality-h6").unwrap().unwrap();
        let s = MotionSettings::default();
        let tune = MotionTune { min_traction_loss_activation: 30.0, ..MotionTune::default() };
        let n = Normalized {
            percent: PerAxis::from_fn(|a| match a {
                Axis::Roll => Num::Real(-90.0),
                Axis::TractionLoss => Num::Real(25.0),
                _ => Num::Real(40.0),
            }),
            surge: 1.0,
            roll: 0.0,
            native_surge: PyValue::Float(1.0),
            native_roll: PyValue::Float(0.0),
        };
        // Slot 1 (normal table): roll deadzone 30 -> capped at -70; surge deadzone 50 -> 50.
        let f = slot_formulas(&Ok(n.clone()), h6.slot_mix(1, false), 1, &h6, &src(&s, &tune)).unwrap();
        let w = 10.0 / 20.0; // gain 10, share 100 %
        assert_eq!(re(f[Axis::Roll]), -70.0 * w);
        assert_eq!(re(f[Axis::Surge]), 40.0 * w);
        assert_eq!(re(f[Axis::Pitch]), 40.0 * w);
        // Traction loss below the activation threshold is the int 0.
        assert_eq!(f[Axis::TractionLoss], Num::Int(0));
        // Share 0 on slot 1 for traction loss front: 40 · 0.
        assert_eq!(re(f[Axis::TractionLossFront]), 0.0);
    }

    #[test]
    fn calculate_motor_power_activation_and_real_heave_reads() {
        let p = p3();
        let s = MotionSettings::default();
        let mut raw = raw_game(&MotionTune::default());
        set_game(&mut raw, &["min_traction_loss_activation"], Some(PyValue::from("30")));
        let err = slot_formulas(&Ok(normalized(40.0)), &silent(), 0, &p, &src(&s, &raw)).unwrap_err();
        assert_eq!(err.to_string(), "'<' not supported between instances of 'float' and 'str'");
        assert_eq!(err.frames().next().map(|f| f.line), Some(397));
        // The int 0 of a failed percentage compares as an int.
        let zero = Normalized {
            percent: PerAxis::splat(Num::Int(0)),
            surge: 0.0,
            roll: 0.0,
            native_surge: PyValue::Float(0.0),
            native_roll: PyValue::Float(0.0),
        };
        let err = slot_formulas(&Ok(zero), &silent(), 0, &p, &src(&s, &raw)).unwrap_err();
        assert_eq!(err.to_string(), "'<' not supported between instances of 'int' and 'str'");
        // The manufacturer's `real_heave` is read only when |roll| > 1.
        let tune = MotionTune::default();
        let mut config = raw_config(&s, "DOF REALITY");
        config.remove("DOF REALITY", "real_heave");
        assert!(slot_formulas(&Ok(normalized(40.0)), &silent(), 0, &p, &src(&config, &tune)).is_ok());
        let rolled = Normalized { roll: 1.5, native_roll: PyValue::Float(1.5), ..normalized(40.0) };
        let err = slot_formulas(&Ok(rolled), &silent(), 0, &p, &src(&config, &tune)).unwrap_err();
        assert_eq!((err.to_string(), err.frames().next().map(|f| f.line)), ("'real_heave'".into(), Some(388)));
    }

    #[test]
    fn real_heave_branch_uses_false_singleton_not_numeric_equality() {
        use crate::views::NativeConfig;
        let p = p3();
        let tune = MotionTune::default();
        let raw = raw_config(&MotionSettings::default(), "DOF REALITY");
        let base = PyValue::Dict(vec![(
            "DOF REALITY".into(),
            PyValue::Dict(vec![("real_heave".into(), PyValue::Bool(false))]),
        )]);
        struct NativeHeave<'a> {
            base: &'a dyn ConfigView,
            native: NativeConfig,
        }
        impl ConfigView for NativeHeave<'_> {
            fn value(&self, section: &str, key: &str) -> Result<std::borrow::Cow<'_, str>, crate::Missing> {
                self.base.value(section, key)
            }
            fn read_value(&self, section: &str, key: &str) -> Result<PyValue, CueError> {
                if key == "real_heave" {
                    self.native.read_value(section, key)
                } else {
                    self.base.read_value(section, key)
                }
            }
        }
        let mut config = NativeHeave { base: &raw, native: NativeConfig(base) };
        let rolled = Normalized { roll: 2.0, native_roll: PyValue::Float(2.0), ..normalized(40.0) };
        let reduced = slot_formulas(&Ok(rolled.clone()), &silent(), 0, &p, &src(&config, &tune)).unwrap();
        assert_eq!(re(reduced[Axis::Heave]), 10.0);
        if let PyValue::Dict(sections) = &mut config.native.0 {
            if let PyValue::Dict(values) = &mut sections[0].1 {
                values[0].1 = PyValue::Int(0);
            }
        }
        let unchanged = slot_formulas(&Ok(rolled), &silent(), 0, &p, &src(&config, &tune)).unwrap();
        assert_eq!(re(unchanged[Axis::Heave]), 20.0);
    }

    #[test]
    fn calculate_motor_power_share_overrides() {
        let p = builtin("pt-actuator-6-actuators-2-front-2-rear-surge-belt-canbus").unwrap().unwrap();
        let s = MotionSettings::default();
        let mut tune = MotionTune { belt_tensioner: 30.0, ..MotionTune::default() };
        tune.main_actuator_shares.surge = 40.0;
        let mix = silent();
        // Belt slot 5: surge share = 30 mapped from 0..100 onto 0..1000 = 300.
        assert_eq!(slot_shares(&mix, 5, &p, &src(&s, &tune)).unwrap()[Axis::Surge], Share::Tune(PyValue::Int(300)));
        // Main actuator slot 0: overrides from the tune.
        let s0 = slot_shares(&mix, 0, &p, &src(&s, &tune)).unwrap();
        assert_eq!(s0[Axis::Surge], Share::Tune(PyValue::Float(40.0)));
        assert_eq!(s0[Axis::TractionLoss], Share::Tune(PyValue::Float(0.0)));
        assert_eq!(s0[Axis::Pitch], Share::Table(TableInt::from(100)));
        // A belt value that is not a number fails in the range map's argument conversion.
        let mut raw = raw_game(&tune);
        set_game(&mut raw, &["motion_belt_tensioner"], Some(PyValue::from("30")));
        let err = slot_shares(&mix, 5, &p, &src(&s, &raw)).unwrap_err();
        assert_eq!(err.to_string(), "must be real number, not str");
        let lines: Vec<_> = err.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(lines, [("calculate_motor_power", 211), ("map", 204)]);
    }

    #[test]
    fn calculate_motor_power_weights() {
        let s = MotionSettings::default();
        let mut tune = MotionTune::default();
        tune.effects.surge_acceleration = 20.0;
        tune.effects.surge_deceleration = 4.0;
        let shares = PerAxis::splat(Share::Table(TableInt::from(100)));
        let w = slot_weights(&shares, &src(&s, &tune)).unwrap();
        assert_eq!((w.axes[Axis::Surge], w.surge_deceleration), (1.0, 0.2));
        tune.switches.pitch = false;
        assert_eq!(slot_weights(&shares, &src(&s, &tune)).unwrap().axes[Axis::Pitch], 0.0);

        // Values as a JSON tune can hold them: a string gain repeats and then fails the division by 20.
        let mut raw = raw_game(&MotionTune::default());
        set_game(&mut raw, &["motion_effects", "yaw"], Some(PyValue::from("10")));
        let err = slot_weights(&shares, &src(&s, &raw)).err().unwrap();
        assert_eq!(err.to_string(), "unsupported operand type(s) for /: 'str' and 'int'");
        assert_eq!(err.frames().next().map(|f| f.line), Some(235));
        // Both surge weights are computed: a missing deceleration switch is an error whatever the surge sign.
        let mut raw = raw_game(&MotionTune::default());
        set_game(&mut raw, &["motion_switch", "surge_deceleration"], None);
        let err = slot_weights(&shares, &src(&s, &raw)).err().unwrap();
        assert_eq!((err.to_string(), err.frames().next().map(|f| f.line)), ("'surge_deceleration'".into(), Some(241)));
        // The integer of a switch string.
        let mut raw = raw_game(&MotionTune::default());
        set_game(&mut raw, &["motion_switch", "roll"], Some(PyValue::from("on")));
        let err = slot_weights(&shares, &src(&s, &raw)).err().unwrap();
        assert_eq!(err.to_string(), "invalid literal for int() with base 10: 'on'");
        // A share from the tune that is not a number fails at its own line (for pitch, ...).
        let mut tune_shares = shares.clone();
        tune_shares[Axis::Sway] = Share::Tune(PyValue::from("100"));
        let err = slot_weights(&tune_shares, &src(&s, &MotionTune::default())).err().unwrap();
        assert_eq!((err.frames().next().map(|f| f.line)), Some(238));
    }

    #[test]
    fn get_signal_sum_and_motor_command() {
        let p = p3();
        let mix = p.slot_mix(1, false);
        let f = PerAxis::from_fn(|a| Num::Real((a.index() + 1) as f64));
        // slot 1 directions: + - 0 - + + 0 0 -> 1 - 2 - 4 + 5 + 6 = 6
        assert_eq!(slot_power(mix, &f), Ok(Num::Real(6.0)));
        assert_eq!(mix.direction_of(Axis::Yaw), Some(Sign::Zero));
        // Slot 1 is inverted: -6 mapped from [-100, 100] onto [204, 3891] = 1936.89 -> 1936.
        assert_eq!(command_position(Num::Real(6.0), 1, &p, "send_stream", 850).unwrap(), 1936.0);
        assert_eq!(command_position(Num::Real(6.0), 0, &p, "send_stream", 850).unwrap(), 2158.0);
        assert_eq!(command_position(Num::Int(0), 7, &p, "send_stream", 850).unwrap(), 2047.0);
        // A slot of '0' directions sums the integer 0.
        assert_eq!(slot_power(p.slot_mix(7, false), &f), Ok(Num::Int(0)));
    }

    #[test]
    fn calculate_motor_power_negative_maximum_curves_to_a_complex_percent() {
        // max_telemetry_vars sway -18 (shipped in game configs) with sway_curving_factor 1.5:
        // 2.0^1.5 / (2 · (-18)^1.5) · 100 = -3.401796664298203e-16 + 1.8518518518518516i
        // and -1 · (|-3.0|^1.5 / (2 · (-18)^1.5) · 100) = 6.249499527174358e-16 - 3.4020690871988584i.
        let mut tune = MotionTune::default();
        tune.maxima.sway = -18.0;
        tune.maxima.surge_acceleration = -18.0;
        tune.maxima.surge_deceleration = -18.0;
        let s = MotionSettings { sway_curving_factor: 1.5, surge_curving_factor: 1.5, ..MotionSettings::default() };
        let n = normalize(
            &inputs(|a| match a {
                Axis::Sway => 2.0,
                Axis::Surge => -3.0,
                _ => 0.0,
            }),
            &src(&s, &tune),
            &ClipFactors::default(),
            real_game,
        )
        .unwrap();
        let Num::Complex(sway) = n.percent[Axis::Sway] else { panic!("complex sway") };
        assert_eq!((sway.re, sway.im), (-3.401796664298203e-16, 1.8518518518518516));
        let Num::Complex(surge) = n.percent[Axis::Surge] else { panic!("complex surge") };
        assert_eq!((surge.re, surge.im), (6.249499527174358e-16, -3.4020690871988584));

        // The complex value passes a slot without a deadzone and fails the output map.
        let p = p3();
        let mix = p.slot_mix(0, false);
        let f = slot_formulas(&Ok(n.clone()), mix, 0, &p, &src(&s, &tune)).unwrap();
        assert!(matches!(f[Axis::Sway], Num::Complex(_)));
        let power = slot_power(mix, &f).unwrap();
        assert!(matches!(power, Num::Complex(_)));
        let err = command_position(power, 0, &p, "send_stream", 850).unwrap_err();
        assert_eq!(err.to_string(), "must be real number, not complex");
        let frames: Vec<_> = err.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 850), ("map", 204)]);
        // A slot whose sway direction is '0' adds the integer 0: its power stays real.
        let yaw_slot = p.slot_mix(2, false);
        let f2 = slot_formulas(&Ok(n.clone()), yaw_slot, 2, &p, &src(&s, &tune)).unwrap();
        assert!(matches!(slot_power(yaw_slot, &f2), Ok(Num::Real(_))));
    }

    fn ceiling(deadzone: i64) -> Exact {
        TableInt::from(deadzone).complement()
    }

    #[test]
    fn deadzone_compares_like_python() {
        // |percent| > ceiling does not hold for NaN: the value passes unchanged.
        assert!(
            matches!(cap_by_deadzone(Num::Real(f64::NAN), ceiling(30)), Ok(Percent::Value(Num::Real(x))) if x.is_nan())
        );
        assert_eq!(cap_by_deadzone(Num::Real(-90.0), ceiling(30)), Ok(Percent::Capped(ceiling(30).neg())));
        assert_eq!(cap_by_deadzone(Num::Real(-70.0), ceiling(30)), Ok(Percent::Value(Num::Real(-70.0))));
        // A complex percent within the ceiling passes; above it, the sign test is a type error.
        let c = Num::Complex(crate::pymath::Complex { re: 0.0, im: 60.0 });
        assert_eq!(cap_by_deadzone(c, ceiling(30)), Ok(Percent::Value(c)));
        let err = cap_by_deadzone(c, ceiling(50)).unwrap_err();
        assert_eq!(err.to_string(), "'>' not supported between instances of 'complex' and 'int'");
        assert_eq!(err.frames().next().map(|f| f.line), Some(370));
        // No deadzone: never compared.
        let big = Num::Complex(crate::pymath::Complex { re: 1.5e308, im: 1.5e308 });
        assert_eq!(cap_by_deadzone(big, ceiling(0)), Ok(Percent::Value(big)));
        assert_eq!(cap_by_deadzone(big, ceiling(10)).unwrap_err().to_string(), "absolute value too large");
        // A deadzone above 100 gives a negative ceiling: every real percentage is capped.
        assert_eq!(cap_by_deadzone(Num::Real(0.0), ceiling(130)), Ok(Percent::Capped(ceiling(130).neg())));
    }

    /// A slot row whose cells are the given ints (deadzone in the effects-list order).
    fn row(percentages: [&str; 8], deadzone: [&str; 8]) -> SlotMix {
        let int = |s: &str| {
            let (negative, digits) = s.strip_prefix('-').map_or((false, s), |rest| (true, rest));
            TableInt::new(&PyInt::from_decimal(negative, digits.bytes()))
        };
        SlotMix {
            direction: vec![Some(Sign::Plus); 8],
            percentages: percentages.iter().map(|s| int(s)).collect(),
            deadzone: deadzone.iter().map(|s| int(s)).collect(),
        }
    }

    #[test]
    fn calculate_motor_power_with_python_sized_table_ints() {
        // Gain 10 → weight 10 / 20 · (pct / 100).
        let p = p3();
        let (s, tune) = (MotionSettings::default(), MotionTune::default());
        let sources = src(&s, &tune);
        let n = normalized(40.0);
        // Share 2^53 + 1: 10 / 20 · (9007199254740993 / 100) = 45035996273704.97, times 40.
        let big_share = "9007199254740993";
        let f = slot_formulas(
            &Ok(n.clone()),
            &row([big_share, "100", "100", "100", "100", "100", "100", "100"], ["0"; 8]),
            0,
            &p,
            &sources,
        )
        .unwrap();
        assert_eq!(f[Axis::Pitch], Num::Real(40.0 * (10.0 / 20.0 * 90071992547409.94)));
        // A share whose quotient does not fit a float: an overflow error at the weights.
        let huge = "1".repeat(320);
        let r = row([huge.as_str(), "100", "100", "100", "100", "100", "100", "100"], ["0"; 8]);
        let err = slot_formulas(&Ok(n.clone()), &r, 0, &p, &sources).unwrap_err();
        assert_eq!((err.to_string(), err.frames().next().map(|f| f.line)), (DIVISION_OVERFLOW.into(), Some(232)));
        // ... which comes before the percentage stage's own failure.
        let failed = Err(CueError::key("pitch_rate"));
        assert_eq!(slot_formulas(&failed, &r, 0, &p, &sources).unwrap_err().to_string(), DIVISION_OVERFLOW);
        // Deadzone 2^53 + 101 (roll): ceiling -(2^53 + 1); |40| > ceiling, 40 > 0 → the percent becomes the ceiling,
        // the ceiling as a float is -9007199254740992.0.
        let f = slot_formulas(
            &Ok(n.clone()),
            &row(["100"; 8], ["9007199254741101", "0", "0", "0", "0", "0", "0", "0"]),
            0,
            &p,
            &sources,
        )
        .unwrap();
        assert_eq!(f[Axis::Roll], Num::Real(-9007199254741000.0 * 0.5));
        // A capped value beyond the float range (ceiling 100 - 11…1): an overflow error at its formula.
        let r = row(["100"; 8], [huge.as_str(), "0", "0", "0", "0", "0", "0", "0"]);
        let err = slot_formulas(&Ok(n.clone()), &r, 0, &p, &sources).unwrap_err();
        assert_eq!((err.to_string(), err.frames().next().map(|f| f.line)), (CONVERSION_OVERFLOW.into(), Some(377)));
        // Traction loss compares the capped int exactly: ceiling -(2^53 + 3), whose float is
        // -(2^53 + 4); |-(2^53 + 3)| < 2^53 + 4 holds in exact comparison, so the formula is 0.
        let mut tl = ["0"; 8];
        tl[6] = "9007199254741095";
        let threshold = MotionTune { min_traction_loss_activation: 9007199254740996.0, ..MotionTune::default() };
        let f = slot_formulas(&Ok(n.clone()), &row(["100"; 8], tl), 0, &p, &src(&s, &threshold)).unwrap();
        assert_eq!(f[Axis::TractionLoss], Num::Int(0));
    }

    #[test]
    fn calculate_motor_power_reads_short_rows_where_the_original_does() {
        let p = p3();
        let (s, tune) = (MotionSettings::default(), MotionTune::default());
        let sources = src(&s, &tune);
        let n = normalized(40.0);
        let line = |r: Result<PerAxis<Num>, CueError>| {
            let e = r.unwrap_err();
            let line = e.frames().next().map(|f| f.line);
            (e.to_string(), line)
        };
        let mut r = silent();
        r.percentages.pop();
        assert_eq!(
            line(slot_formulas(&Ok(n.clone()), &r, 3, &p, &sources)),
            ("list index out of range".into(), Some(215))
        );
        // A short percentages row fails before the percentage stage's own error.
        assert_eq!(line(slot_formulas(&Err(CueError::key("roll_rate")), &r, 3, &p, &sources)).1, Some(215));
        let mut r = silent();
        r.deadzone.truncate(7);
        assert_eq!(
            line(slot_formulas(&Ok(n.clone()), &r, 4, &p, &sources)),
            ("list index out of range".into(), Some(367))
        );
        // A short deadzone row fails after the percentage stage.
        assert_eq!(line(slot_formulas(&Err(CueError::key("roll_rate")), &r, 4, &p, &sources)).0, "'roll_rate'");
        // A missing row: an index error at the first percentage read.
        assert_eq!(line(slot_formulas(&Ok(n), Platform::row(&[], 0), 0, &p, &sources)).1, Some(197));

        // Direction: an index error when the signal sum reaches the missing cell, a type error at the addition after
        // an invalid one, whichever comes first.
        let f = PerAxis::splat(Num::Real(1.0));
        let mut r = silent();
        r.direction.truncate(5);
        let e = slot_power(&r, &f).unwrap_err();
        let frames: Vec<_> = e.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("calculate_motor_power", 452), ("get_signal", 82)]);
        r.direction[3] = None;
        let e = slot_power(&r, &f).unwrap_err();
        assert_eq!(e.to_string(), "unsupported operand type(s) for +: 'int' and 'NoneType'");
        assert_eq!(e.frames().next().map(|f| f.line), Some(449));
        let mut r = silent();
        r.direction[0] = None;
        r.direction.truncate(1);
        assert_eq!(slot_power(&r, &f).unwrap_err().frames().next().map(|f| f.line), Some(448));
    }

    #[test]
    fn enhanced_type_selects_the_table() {
        let p = p3();
        let s = MotionSettings::default();
        let mut raw = RawGameConfig(PyValue::Dict(vec![]));
        let err = enhanced_type(&src(&s, &raw)).unwrap_err();
        assert_eq!(
            (err.to_string(), err.frames().next().map(|f| f.line)),
            ("'motion_enhanced_gforces'".into(), Some(189))
        );
        set_game(&mut raw, &["motion_enhanced_gforces"], Some(PyValue::Int(2)));
        let enhanced = enhanced_type(&src(&s, &raw)).unwrap();
        let err = slot_table(&p, &enhanced).unwrap_err();
        assert_eq!(
            (err.to_string(), err.frames().next().map(|f| f.line)),
            ("list index out of range".into(), Some(197))
        );
    }
}
