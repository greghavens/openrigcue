//! Test mode: the Hardware page sliders drive the rig directly, on the test loop's thread.

use crate::axis::{Axis, PerAxis};
use crate::cueing::{sources, Listener, MotionCueing, Positions};
use crate::error::{mh, CueError};
use crate::mixing::{
    command_position, enhanced_type, normalize, slot_formulas_with_normalize, slot_power, slot_table, HeaveScale,
};
use crate::platform::{Platform, SLOTS};
use crate::pymath::Num;
use crate::pyvalue::{Op, PyValue};
use crate::views::{ConfigView, GameConfigView};

/// The eight test sliders, each −100…100.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TestSliders {
    pub pitch: f64,
    pub roll: f64,
    pub yaw: f64,
    pub sway: f64,
    pub surge: f64,
    pub heave: f64,
    pub traction_loss: f64,
    pub traction_loss_front: f64,
}

impl TestSliders {
    /// The slider conversion of test mode: a tenth of each slider, with pitch,
    /// surge and front traction loss inverted.
    fn axis_inputs(&self) -> PerAxis<f64> {
        PerAxis::from_fn(|axis| match axis {
            Axis::Pitch => -self.pitch / 10.0,
            Axis::Roll => self.roll / 10.0,
            Axis::Yaw => self.yaw / 10.0,
            Axis::Sway => self.sway / 10.0,
            Axis::Surge => -self.surge / 10.0,
            Axis::Heave => self.heave / 10.0,
            Axis::TractionLoss => self.traction_loss / 10.0,
            Axis::TractionLossFront => -self.traction_loss_front / 10.0,
        })
    }

    /// The heave value published with a delivered test frame.
    pub fn heave_cue(&self) -> f64 {
        self.heave / 10.0
    }
}

/// Everything a test frame reads besides the sliders.
#[derive(Clone, Copy)]
pub struct TestContext<'a> {
    /// The configuration.
    pub settings: &'a dyn ConfigView,
    /// The game configuration.
    pub tune: &'a dyn GameConfigView,
    /// Id of the running game; selects the heave scale as in the stream path.
    pub game: &'a str,
}

/// What the test loop does with a computed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestAction {
    /// Deliver the frame to the driver's test entry point.
    pub send: bool,
    /// Disconnect the motion controller (the motion switch is off).
    pub disconnect: bool,
}

/// The delivery decision of test mode, evaluated after the loop's sleep: send
/// while no game listener runs and the controller is connected with the motion switch on; disconnect
/// whenever the motion switch is off.
pub fn test_action(listener: Listener, connected: bool, motion_switch_on: bool) -> TestAction {
    TestAction { send: listener.is_idle() && connected && motion_switch_on, disconnect: !motion_switch_on }
}

/// Immutable computation data for the independent motion-test worker.
#[derive(Clone)]
pub struct MotionTestSnapshot {
    platform: Option<Platform>,
    clips: crate::ClipFactors,
    motion_config: std::sync::Arc<crate::MotionConfig>,
}
impl MotionCueing {
    pub fn test_snapshot(&self) -> MotionTestSnapshot {
        MotionTestSnapshot::new(self.platform().cloned(), *self.clip_factors(), self.motion_config_handle())
    }
    pub fn test(&self, sliders: &TestSliders, ctx: &TestContext<'_>) -> Result<Option<Positions>, CueError> {
        self.test_snapshot().test(sliders, ctx)
    }
    pub fn test_native(
        &self,
        sliders: &[PyValue; Axis::COUNT],
        ctx: &TestContext<'_>,
    ) -> Result<Option<Positions>, CueError> {
        self.test_snapshot().test_native(sliders, ctx)
    }
    pub fn test_native_frame(
        &self,
        sliders: &[PyValue; Axis::COUNT],
        ctx: &TestContext<'_>,
    ) -> Result<Option<(Positions, f64)>, CueError> {
        self.test_snapshot().test_native_frame(sliders, ctx)
    }
}
impl MotionTestSnapshot {
    pub fn new(
        platform: Option<Platform>,
        clips: crate::ClipFactors,
        motion_config: std::sync::Arc<crate::MotionConfig>,
    ) -> Self {
        Self { platform, clips, motion_config }
    }
    fn platform(&self) -> Option<&Platform> {
        self.platform.as_ref()
    }
    fn clip_factors(&self) -> &crate::ClipFactors {
        &self.clips
    }
    fn motion_config(&self) -> &crate::MotionConfig {
        &self.motion_config
    }

    /// The frame computation of test mode: the stream mixing without smoothing,
    /// travel scaling, rate shaping or effects, and without touching the stream state. `Ok(None)` while no
    /// platform model is selected (the model is `'None'`). A model
    /// missing from the model table ([`crate::MotionConfig::unlisted`]) computes with the `'None'` model's
    /// values. An error ends the test thread.
    pub fn test(&self, sliders: &TestSliders, ctx: &TestContext<'_>) -> Result<Option<Positions>, CueError> {
        let Some(platform) = self.platform() else {
            return Ok(None);
        };
        self.test_with_inputs(&sliders.axis_inputs(), ctx, platform)?.map(bounded_positions).transpose()
    }

    /// The runtime test slider entries, before any arithmetic is applied to them.
    pub fn test_native(
        &self,
        sliders: &[PyValue; Axis::COUNT],
        ctx: &TestContext<'_>,
    ) -> Result<Option<Positions>, CueError> {
        self.test_native_frame(sliders, ctx).map(|frame| frame.map(|v| v.0))
    }

    pub fn test_native_frame(
        &self,
        sliders: &[PyValue; Axis::COUNT],
        ctx: &TestContext<'_>,
    ) -> Result<Option<(Positions, f64)>, CueError> {
        self.test_native_values_frame(sliders, ctx)?
            .map(|(values, heave)| bounded_positions(values).map(|positions| (positions, heave)))
            .transpose()
    }

    /// The live test loop's positions as integers of any size, before any device packing.
    pub fn test_native_values_frame(
        &self,
        sliders: &[PyValue; Axis::COUNT],
        ctx: &TestContext<'_>,
    ) -> Result<Option<(crate::NativePositions, f64)>, CueError> {
        let mut live = false;
        if let Some(model) = ctx.settings.current_model() {
            live = true;
            if model.map_err(|e| e.at(mh("send_test", 1083)))?.equals_str("None") {
                return Ok(None);
            }
        }
        let fallback = if self.platform().is_none() && live {
            Some(self.motion_config().platform("None").expect("None model").expect("valid model"))
        } else {
            None
        };
        let Some(original) = self.platform().or(fallback.as_ref()) else { return Ok(None) };
        let source = sources(ctx.settings, ctx.tune, original, ctx.game);
        let none = crate::platform::motion_model("None").expect("None model");
        let get = |key: &str, fallback: PyValue| {
            source.model_field(key).map(|v| v.unwrap_or(fallback)).unwrap_or_else(|_| {
                match none.fields.get(key).expect("None model field") {
                    toml::Value::Integer(v) => PyValue::Int((*v).into()),
                    toml::Value::Float(v) => PyValue::Float(*v),
                    _ => unreachable!("numeric model field"),
                }
            })
        };
        let direction = get("motors_direction", PyValue::Float(original.motors_direction)).as_double()?;
        let mapping = get("mapping_id", PyValue::Int(original.mapping_id.into()))
            .to_native()
            .integer()
            .expect("model int")
            .try_into()
            .expect("model mapping");
        let maximum = get("max_motor_abs", PyValue::Int(original.max_motor_abs.into()))
            .to_native()
            .integer()
            .expect("model int")
            .try_into()
            .expect("model range");
        let mut changed;
        let platform = if direction != original.motors_direction
            || mapping != original.mapping_id
            || maximum != original.max_motor_abs
        {
            changed = original.clone();
            changed.motors_direction = direction;
            changed.max_motor_abs = maximum;
            if mapping != original.mapping_id {
                changed.mapping = self.motion_config().tables(mapping).expect("model table").to_vec();
                changed.mapping_id = mapping;
            }
            &changed
        } else {
            original
        };
        let inputs = native_axis_inputs(sliders, ctx.settings)?;
        let heave = inputs[Axis::Heave];
        self.test_with_inputs(&inputs, ctx, platform).map(|frame| frame.map(|positions| (positions, heave)))
    }

    fn test_with_inputs(
        &self,
        inputs: &PerAxis<f64>,
        ctx: &TestContext<'_>,
        platform: &Platform,
    ) -> Result<Option<crate::NativePositions>, CueError> {
        let sources = sources(ctx.settings, ctx.tune, platform, ctx.game);
        let normalized = || normalize(inputs, &sources, self.clip_factors(), || HeaveScale::select(&sources, platform));
        // The test-mode mix for all 12 slots, then the output map.
        let mut powers = [Num::Int(0); SLOTS];
        for (slot, power) in powers.iter_mut().enumerate() {
            let at = |e: CueError| e.at(mh("send_test", 1122));
            let enhanced = enhanced_type(&sources).map_err(at)?;
            let mix = Platform::row(slot_table(platform, &enhanced).map_err(at)?, slot);
            let formulas = slot_formulas_with_normalize(normalized, mix, slot, platform, &sources).map_err(at)?;
            *power = slot_power(mix, &formulas).map_err(at)?;
        }
        let mut positions = std::array::from_fn(|_| openrigcue_model::native_value::LegacyValue::Int(0));
        for (slot, position) in positions.iter_mut().enumerate() {
            *position = PyValue::Float(command_position(powers[slot], slot, platform, "send_test", 1130)?)
                .to_integer_value()?
                .to_native();
        }
        Ok(Some(positions))
    }
}

fn bounded_positions(values: crate::NativePositions) -> Result<Positions, CueError> {
    let mut positions = [0; SLOTS];
    for (slot, value) in values.into_iter().enumerate() {
        positions[slot] = value
            .integer()
            .and_then(|value| value.try_into().ok())
            .ok_or_else(|| CueError::overflow("Python int too large to convert to C long"))?;
    }
    Ok(positions)
}

fn native_axis_inputs(sliders: &[PyValue; Axis::COUNT], config: &dyn ConfigView) -> Result<PerAxis<f64>, CueError> {
    use openrigcue_model::native_value::LegacyValue;
    let mut inputs = PerAxis([0.0; Axis::COUNT]);
    // Roll is read before pitch. Inversions multiply by the integer -1, then divide;
    // this keeps int/bool zero positive and repeats a string/bytes/list/tuple to an empty sequence.
    for (index, axis) in [
        Axis::Roll,
        Axis::Pitch,
        Axis::Yaw,
        Axis::Sway,
        Axis::Surge,
        Axis::Heave,
        Axis::TractionLoss,
        Axis::TractionLossFront,
    ]
    .into_iter()
    .enumerate()
    {
        let at = |error: CueError| error.at(mh("send_test", 1101 + index as u32));
        let live = config.current_test_value(axis.index());
        let value = live.as_ref().unwrap_or(&sliders[axis.index()]);
        let value = if matches!(axis, Axis::Pitch | Axis::Surge | Axis::TractionLossFront) {
            match value {
                PyValue::Native(LegacyValue::Bytes(_)) => PyValue::Native(LegacyValue::Bytes(Vec::new())),
                PyValue::Native(LegacyValue::Tuple(_)) => PyValue::Native(LegacyValue::Tuple(Vec::new())),
                _ => value.binop(Op::Mul, &PyValue::Int(-1)).map_err(at)?,
            }
        } else {
            value.clone()
        };
        let value = value.binop(Op::Div, &PyValue::Float(10.0)).map_err(at)?;
        inputs[axis] = value.as_double().map_err(at)?;
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::builtin;
    use crate::settings::{ClipFactors, MotionSettings};
    use crate::tune::MotionTune;

    fn p3_cueing() -> MotionCueing {
        MotionCueing::new(builtin("dof-reality-p3").unwrap().unwrap(), ClipFactors::default())
    }

    #[test]
    fn native_motion_test_scalars_keep_zero_kinds_through_multiply_then_divide() {
        let mut values = std::array::from_fn(|_| PyValue::Int(0));
        let inputs = native_axis_inputs(&values, &MotionSettings::default()).unwrap();
        assert!(!inputs[Axis::Pitch].is_sign_negative());
        values[Axis::Pitch.index()] = PyValue::Float(0.0);
        assert!(native_axis_inputs(&values, &MotionSettings::default()).unwrap()[Axis::Pitch].is_sign_negative());
        values[Axis::Pitch.index()] = PyValue::Bool(false);
        assert!(!native_axis_inputs(&values, &MotionSettings::default()).unwrap()[Axis::Pitch].is_sign_negative());
    }

    #[test]
    fn native_motion_test_scalars_preserve_sequence_errors_and_roll_first_order() {
        use openrigcue_model::native_value::LegacyValue;
        let mut values = std::array::from_fn(|_| PyValue::Int(0));
        values[Axis::Pitch.index()] = PyValue::Native(LegacyValue::Bytes(b"17".to_vec()));
        let error = native_axis_inputs(&values, &MotionSettings::default()).unwrap_err();
        assert_eq!(error.message(), "unsupported operand type(s) for /: 'bytes' and 'float'");
        assert_eq!(error.frames().next().unwrap().line, 1102);
        values[Axis::Roll.index()] = PyValue::Str("18".into());
        let error = native_axis_inputs(&values, &MotionSettings::default()).unwrap_err();
        assert_eq!(error.message(), "unsupported operand type(s) for /: 'str' and 'float'");
        assert_eq!(error.frames().next().unwrap().line, 1101);
    }

    #[test]
    fn native_motion_test_scalars_keep_the_no_model_gate_and_centred_frame() {
        let (settings, tune) = (MotionSettings::default(), MotionTune::default());
        let ctx = TestContext { settings: &settings, tune: &tune, game: "none" };
        let mut values = std::array::from_fn(|_| PyValue::Int(0));
        assert_eq!(p3_cueing().test_native(&values, &ctx).unwrap(), Some([2047; SLOTS]));
        values[Axis::Roll.index()] = PyValue::None;
        assert_eq!(MotionCueing::without_platform(ClipFactors::default()).test_native(&values, &ctx).unwrap(), None);
    }

    #[test]
    fn send_test_centred_sliders() {
        let c = p3_cueing();
        let (settings, tune) = (MotionSettings::default(), MotionTune::default());
        let ctx = TestContext { settings: &settings, tune: &tune, game: "none" };
        // Power 0 on every slot: 0 mapped from [-100, 100] onto [204, 3891] is 2047.5, truncated to 2047.
        assert_eq!(c.test(&TestSliders::default(), &ctx), Ok(Some([2047; SLOTS])));
    }

    #[test]
    fn send_test_skips_without_a_model() {
        // Nothing is computed while MOTIONBOX.model is 'None'.
        let c = MotionCueing::without_platform(ClipFactors::default());
        let (settings, tune) = (MotionSettings::default(), MotionTune::default());
        let ctx = TestContext { settings: &settings, tune: &tune, game: "none" };
        assert_eq!(c.test(&TestSliders::default(), &ctx), Ok(None));
    }

    #[test]
    fn send_test_pitch_slider() {
        let c = p3_cueing();
        let (settings, tune) = (MotionSettings::default(), MotionTune::default());
        let ctx = TestContext { settings: &settings, tune: &tune, game: "none" };
        let sliders = TestSliders { pitch: 50.0, ..TestSliders::default() };
        // pitch input -5 -> -50 % · (10 / 20) = -25 on slots 0 and 1 (direction '+').
        // slot 0: -25 maps to 1586.625 -> 1586; slot 1 inverted: +25 maps to 2508.375 -> 2508.
        let p = c.test(&sliders, &ctx).unwrap().unwrap();
        assert_eq!(p[0], 1586);
        assert_eq!(p[1], 2508);
        assert_eq!(p[2], 2047);
    }

    #[test]
    fn send_test_heave_uses_the_game_scale() {
        let c = p3_cueing();
        let (settings, tune) = (MotionSettings::default(), MotionTune::default());
        let sliders = TestSliders { heave: 50.0, ..TestSliders::default() };
        let idle = c.test(&sliders, &TestContext { settings: &settings, tune: &tune, game: "test" }).unwrap().unwrap();
        let racing =
            c.test(&sliders, &TestContext { settings: &settings, tune: &tune, game: "dirtrally2" }).unwrap().unwrap();
        // heave 5 -> 50 % · 0.5 = 25 (factor 1.0) or 7.5 (factor 0.3), slot 6 also carries heave.
        assert_eq!(idle[0], 2508);
        assert_eq!(racing[0], 2185);
        assert_eq!(idle[6], 2508);
        assert_eq!(sliders.heave_cue(), 5.0);
    }

    #[test]
    fn send_test_with_a_model_missing_from_motion_models() {
        // 'STR-SCN6150' is not in the model table; test mode takes
        // motors_direction, mapping_id and max_motor_abs of the 'None' entry (1, 2, 1023) and
        // the mixing stage the DOF REALITY fallback.
        let c = MotionCueing::new(crate::Platform::unlisted("STR-SCN6150"), ClipFactors::default());
        let (settings, tune) = (MotionSettings::default(), MotionTune::default());
        let sliders = TestSliders {
            pitch: 30.0,
            roll: -40.0,
            yaw: 20.0,
            sway: 50.0,
            surge: -60.0,
            heave: 70.0,
            traction_loss: 80.0,
            traction_loss_front: -90.0,
        };
        for (game, head) in
            [("none", [764, 304, 741, 511, 511, 511, 672]), ("dirtrally2", [651, 417, 741, 511, 511, 511, 559])]
        {
            let got = c.test(&sliders, &TestContext { settings: &settings, tune: &tune, game }).unwrap().unwrap();
            let mut want = [511; SLOTS];
            want[..7].copy_from_slice(&head);
            assert_eq!(got, want, "{game}");
        }
    }

    #[test]
    fn send_test_delivery_rules() {
        assert_eq!(test_action(Listener::None, true, true), TestAction { send: true, disconnect: false });
        assert_eq!(test_action(Listener::Game, true, true), TestAction { send: false, disconnect: false });
        assert_eq!(test_action(Listener::Emulator, false, true), TestAction { send: false, disconnect: false });
        assert_eq!(test_action(Listener::Test, true, false), TestAction { send: false, disconnect: true });
    }

    #[test]
    fn send_test_reference_frames() {
        // Expected positions of the scenario frames in test mode.
        let sliders = TestSliders {
            pitch: 30.0,
            roll: -40.0,
            yaw: 20.0,
            sway: 50.0,
            surge: -60.0,
            heave: 70.0,
            traction_loss: 80.0,
            traction_loss_front: -90.0,
        };
        let cases: [(&str, &str, [i64; 7], i64); 4] = [
            ("dof-reality-p3", "none", [3061, 1217, 2969, 2047, 2047, 2047, 2692], 2047),
            ("dof-reality-h4", "test", [1770, 1586, 1125, 1125, 2047, 2047, 2692], 2047),
            ("dof-reality-h6", "dirtrally2", [1300, 2425, 471, 2241, 1485, 305, 2047], 2047),
            ("yaw-vr-yaw3", "emulator", [47514, 34242, 18022, 32768, 32768, 32768, 32768], 32768),
        ];
        let tune = MotionTune::default();
        for (id, game, head, rest) in cases {
            let platform = builtin(id).unwrap().unwrap();
            let settings = MotionSettings::for_manufacturer(platform.manufacturer, crate::HostOs::Windows);
            let c = MotionCueing::new(platform, ClipFactors::default());
            let got = c.test(&sliders, &TestContext { settings: &settings, tune: &tune, game }).unwrap().unwrap();
            let mut want = [rest; SLOTS];
            want[..7].copy_from_slice(&head);
            assert_eq!(got, want, "{id}");
        }
    }
}
