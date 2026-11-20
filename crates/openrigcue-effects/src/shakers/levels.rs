//! Channel-group levels: profile matrix x speaker volume x effect level, and the engine fade.

use super::layout::{Effect, Group, PerGroup, PerSpeaker, Speaker, GROUP_COUNT};
use super::profiles::SpeakerProfile;
use super::tuning::{DspError, NativeEngineSettings, ShakerTuning};

/// The output level of every channel group on every speaker (linear gain, unclamped).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupLevels {
    levels: PerGroup<PerSpeaker>,
    present: PerGroup<bool>,
}

impl GroupLevels {
    /// For each group and speaker:
    /// `profile / 10 * (volume / 2.0 * 20 / 100) * effect_level`, where
    /// `effect_level = level * enabled / 2.0 * 20 / 100` (`enabled` as 0 or 1). With `focus` set (the effect test),
    /// groups of other effects get `effect_level = 0`. An unreadable level/switch
    /// ([`super::tuning::EffectTuning::level`] `None`) gives `effect_level = 1` normally and 0
    /// in the effect test. Nothing is clamped.
    pub fn compute(profile: &SpeakerProfile, tuning: &ShakerTuning, focus: Option<Effect>) -> GroupLevels {
        let mut levels = Self::empty();
        let _ = levels.rebuild(profile, tuning, focus);
        levels
    }

    pub(super) fn empty() -> Self {
        Self { levels: [[0.0; 6]; GROUP_COUNT], present: [false; GROUP_COUNT] }
    }

    /// The published mix levels are cleared first, then each complete row is published in build order.
    /// The caller commits new sample/DSP groups only after this entire calculation succeeds.
    pub(super) fn rebuild(
        &mut self,
        profile: &SpeakerProfile,
        tuning: &ShakerTuning,
        focus: Option<Effect>,
    ) -> Result<(), DspError> {
        *self = Self::empty();
        for group in Group::ALL {
            self.publish_group(profile, tuning, focus, group)?;
        }
        Ok(())
    }

    pub(super) fn publish_group(
        &mut self,
        profile: &SpeakerProfile,
        tuning: &ShakerTuning,
        focus: Option<Effect>,
        group: Group,
    ) -> Result<(), DspError> {
        let effect = group.effect();
        let effect_level = match focus {
            None => effect_gain(tuning, effect).unwrap_or(UNREADABLE_LEVEL_GAIN),
            Some(f) if f == effect => effect_gain(tuning, effect).unwrap_or(0.0),
            Some(_) => 0.0,
        };
        let row = if let Some(bases) = &tuning.native_group_bases {
            bases[group.index()].as_ref().map_err(Clone::clone)?.map(|base| base * effect_level)
        } else {
            Speaker::ALL.map(|speaker| {
                profile.level(group, speaker) / 10.0 * slider_gain(tuning.volume[speaker.index()]) * effect_level
            })
        };
        self.levels[group.index()] = row;
        self.present[group.index()] = true;
        Ok(())
    }

    /// Levels of one group, indexed by [`Speaker::index`].
    pub fn of(&self, group: Group) -> PerSpeaker {
        self.levels[group.index()]
    }

    /// Required row lookup; absent rows remain distinct from published zero levels.
    pub fn required_of(&self, group: Group) -> Result<PerSpeaker, DspError> {
        self.present[group.index()]
            .then_some(self.levels[group.index()])
            .ok_or_else(|| DspError { class: "KeyError", message: format!("'{}'", group.key()) })
    }
}

/// A 0..20 slider as a gain: `value / 2.0 * 20 / 100` (10 -> 1.0).
fn slider_gain(value: f64) -> f64 {
    value / 2.0 * 20.0 / 100.0
}

/// The effect level used when an effect's level or switch is unreadable outside the effect test.
const UNREADABLE_LEVEL_GAIN: f64 = 1.0;

/// An effect's level times its switch (0 or 1), as a gain; `None` when the entries are unreadable.
fn effect_gain(tuning: &ShakerTuning, effect: Effect) -> Option<f64> {
    if let Some(gains) = &tuning.native_effect_gains {
        return gains[effect.index()];
    }
    let level = tuning.effect(effect).level?;
    Some(slider_gain(if level.enabled { level.slider } else { 0.0 }))
}

/// Level of the speaker-test group: the speaker's volume / 20.0 on the tested speaker only.
pub fn speaker_test_levels(tuning: &ShakerTuning, speaker: Speaker) -> PerSpeaker {
    let mut levels = [0.0; 6];
    levels[speaker.index()] = tuning.volume[speaker.index()] / 20.0;
    levels
}

pub(super) fn required_speaker_test_levels(tuning: &ShakerTuning, speaker: Speaker) -> Result<PerSpeaker, DspError> {
    let Some(native) = &tuning.native_speaker_test else {
        return Ok(speaker_test_levels(tuning, speaker));
    };
    let volumes = native.as_ref().as_ref().map_err(Clone::clone)?;
    let mut levels = [0.0; 6];
    levels[speaker.index()] = volumes[speaker.index()];
    Ok(levels)
}

/// The engine fade: with the engine
/// DSP switch on, the engine level is cut above `cutoff` % of max rpm and reduced (by a product)
/// between `fade` and `cutoff` %. `None` on a division by zero
/// (`max_rpm == 0` with the switch on).
pub fn engine_fade(enabled: bool, volume: f64, rpm: f64, max_rpm: f64, fade: f64, cutoff: f64) -> Option<f64> {
    if !enabled {
        return Some(volume);
    }
    if max_rpm == 0.0 {
        return None;
    }
    let perc = rpm / max_rpm;
    let faded = if perc * 100.0 < fade {
        volume
    } else if perc * 100.0 > cutoff {
        0.0
    } else {
        let v = volume - (perc - fade / 100.0) * (cutoff / 100.0 - fade / 100.0) * 100.0;
        if v < 0.0 {
            0.0
        } else {
            v
        }
    };
    Some(faded)
}

/// The engine fade on raw values: both required endpoints are read before the flag/percentage branches.
/// The steps on the raw operands: `flag == 1`, `perc * 100 < start`,
/// `perc * 100 > end`, then `start / 100` and `end / 100` (exact any-width integer division,
/// an overflow error beyond the float range) before the float arithmetic.
pub(super) fn native_engine_levels(
    settings: &NativeEngineSettings,
    levels: Result<PerSpeaker, DspError>,
    rpm: f64,
    max_rpm: f64,
) -> Result<PerSpeaker, DspError> {
    use openrigcue_model::native_value::LegacyValue;
    use openrigcue_model::python_compare::native_equal_checked;
    let native = DspError::from_native;
    let flag = settings.flag.as_ref().map_err(Clone::clone)?;
    let levels = levels?;
    let start = settings.fade_start.as_ref().map_err(Clone::clone)?;
    let end = settings.fade_end.as_ref().map_err(Clone::clone)?;
    if !native_equal_checked(flag, &LegacyValue::Int(1)).map_err(native)? {
        return Ok(levels);
    }
    if max_rpm == 0.0 {
        return Err(DspError { class: "ZeroDivisionError", message: "float division by zero".into() });
    }
    let percentage = rpm / max_rpm;
    let scaled = LegacyValue::Float(percentage * 100.0);
    if scaled.py_lt(start).map_err(native)? {
        return Ok(levels);
    }
    if scaled.py_gt(end).map_err(native)? {
        return Ok([0.0; 6]);
    }
    let hundredth =
        |value: &LegacyValue| value.py_truediv(&LegacyValue::Int(100)).and_then(|v| v.py_as_double()).map_err(native);
    let start = hundredth(start)?;
    let end = hundredth(end)?;
    Ok(levels.map(|volume| {
        let result = volume - (percentage - start) * (end - start) * 100.0;
        if result < 0.0 {
            0.0
        } else {
            result
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shakers::tuning::EffectLevel;

    fn center() -> SpeakerProfile {
        SpeakerProfile::built_in("1 Speaker - Center").unwrap()
    }

    #[test]
    fn load_channel_groups_default_sliders() {
        // engine CE = 8 -> 8/10 * 1.0 * 1.0.
        let l = GroupLevels::compute(&center(), &ShakerTuning::default(), None);
        assert_eq!(l.of(Group::Engine)[Speaker::Center.index()], 0.8);
        // road_bump CE = 8, effect level 6 -> 0.8 * 0.6.
        assert!((l.of(Group::RoadBump)[Speaker::Center.index()] - 0.48).abs() < 1e-12);
        assert_eq!(l.of(Group::Engine)[Speaker::FrontLeft.index()], 0.0);
    }

    #[test]
    fn load_channel_groups_switch_off_and_no_clamp() {
        let mut t = ShakerTuning::default();
        t.effect_mut(Effect::Impact).level = Some(EffectLevel { slider: 10.0, enabled: false });
        t.effect_mut(Effect::Gear).level = Some(EffectLevel { slider: 20.0, enabled: true });
        t.volume[Speaker::Center.index()] = 20.0;
        let l = GroupLevels::compute(&center(), &t, None);
        assert_eq!(l.of(Group::Impact), [0.0; 6]);
        // 8/10 * 2.0 * 2.0 = 3.2: above unity, not clamped.
        assert!((l.of(Group::Gear)[Speaker::Center.index()] - 3.2).abs() < 1e-12);
    }

    #[test]
    fn load_channel_groups_test_focus() {
        let l = GroupLevels::compute(&center(), &ShakerTuning::default(), Some(Effect::Deceleration));
        assert_eq!(l.of(Group::Engine), [0.0; 6]);
        assert!(l.of(Group::BrakeLf)[Speaker::Center.index()] > 0.0);
    }

    #[test]
    fn load_channel_groups_unreadable_level_fallback() {
        // An unreadable effect level: 1 normally, 0 in the effect test.
        let mut t = ShakerTuning::default();
        t.effect_mut(Effect::Gear).level = None;
        let ce = Speaker::Center.index();
        let normal = GroupLevels::compute(&center(), &t, None);
        assert_eq!(normal.of(Group::Gear)[ce], 0.8);
        let test = GroupLevels::compute(&center(), &t, Some(Effect::Gear));
        assert_eq!(test.of(Group::Gear)[ce], 0.0);
        // A readable level in the effect test is unchanged.
        let test = GroupLevels::compute(&center(), &ShakerTuning::default(), Some(Effect::Gear));
        assert_eq!(test.of(Group::Gear)[ce], 0.8);
    }

    #[test]
    fn load_test_group_level() {
        let t = ShakerTuning::default();
        assert_eq!(speaker_test_levels(&t, Speaker::Sub), [0.0, 0.0, 0.0, 0.3, 0.0, 0.0]);
    }

    #[test]
    fn calc_fade_cases() {
        assert_eq!(engine_fade(false, 1.0, 9000.0, 9000.0, 95.0, 100.0), Some(1.0));
        assert_eq!(engine_fade(true, 1.0, 9000.0, 0.0, 95.0, 100.0), None);
        assert_eq!(engine_fade(true, 1.0, 9000.0, 10000.0, 95.0, 100.0), Some(1.0));
        assert_eq!(engine_fade(true, 1.0, 10001.0, 10000.0, 95.0, 100.0), Some(0.0));
        // At 100 %: 1 - (1 - 0.95) * (1 - 0.95) * 100 = 0.75.
        let v = engine_fade(true, 1.0, 10000.0, 10000.0, 95.0, 100.0).unwrap();
        assert!((v - 0.75).abs() < 1e-12);
        // Clamped at 0 when the product exceeds the volume.
        assert_eq!(engine_fade(true, 0.1, 9900.0, 10000.0, 50.0, 100.0), Some(0.0));
    }

    #[test]
    fn native_engine_fade_keeps_required_reads_raw_comparisons_and_error_text() {
        use openrigcue_model::native_value::LegacyValue;
        let settings = NativeEngineSettings {
            flag: Ok(LegacyValue::Int(1)),
            fade_start: Ok(LegacyValue::Int(95)),
            fade_end: Ok(LegacyValue::Int(100)),
            resonance: Ok(8.0),
        };
        for start in
            [LegacyValue::from("95"), LegacyValue::Bytes(b"95".to_vec()), LegacyValue::None, LegacyValue::List(vec![])]
        {
            let mut bad = settings.clone();
            bad.fade_start = Ok(start.clone());
            let error = native_engine_levels(&bad, Ok([1.0; 6]), 8000.0, 10000.0).unwrap_err();
            assert_eq!(error.class, "TypeError");
            assert_eq!(
                error.message,
                format!("'<' not supported between instances of 'float' and '{}'", start.type_name())
            );
        }
        let mut bad = settings.clone();
        bad.fade_end = Ok(LegacyValue::from("100"));
        assert_eq!(
            native_engine_levels(&bad, Ok([1.0; 6]), 8000.0, 10000.0).unwrap(),
            [1.0; 6],
            "present end is not yet compared"
        );
        assert_eq!(
            native_engine_levels(&bad, Ok([1.0; 6]), 9700.0, 10000.0).unwrap_err().message,
            "'>' not supported between instances of 'float' and 'str'"
        );
        bad.fade_end = Err(DspError { class: "KeyError", message: "'engine_fadeoff_end'".into() });
        for flag in [0, 1] {
            bad.flag = Ok(LegacyValue::Int(flag));
            let error = native_engine_levels(&bad, Ok([1.0; 6]), 8000.0, 10000.0).unwrap_err();
            assert_eq!(error, DspError { class: "KeyError", message: "'engine_fadeoff_end'".into() });
        }
        let error = native_engine_levels(&settings, Ok([1.0; 6]), 8000.0, 0.0).unwrap_err();
        assert_eq!(error, DspError { class: "ZeroDivisionError", message: "float division by zero".into() });
        bad.fade_start = Err(DspError { class: "KeyError", message: "'engine_fadeoff_start'".into() });
        assert_eq!(
            native_engine_levels(&bad, Ok([1.0; 6]), 8000.0, 0.0).unwrap_err().message,
            "'engine_fadeoff_start'"
        );
    }

    #[test]
    fn native_engine_missing_mix_row_follows_flag_but_precedes_fade_endpoints() {
        use openrigcue_model::native_value::LegacyValue;
        let missing = |key: &str| DspError { class: "KeyError", message: format!("'{key}'") };
        let mut settings = NativeEngineSettings {
            flag: Ok(LegacyValue::Int(0)),
            fade_start: Err(missing("engine_fadeoff_start")),
            fade_end: Err(missing("engine_fadeoff_end")),
            resonance: Ok(8.0),
        };
        let error = native_engine_levels(&settings, Err(missing("engine")), 0.0, 0.0).unwrap_err();
        assert_eq!(error.message, "'engine'");
        settings.flag = Err(missing("shaker_dsp_type"));
        let error = native_engine_levels(&settings, Err(missing("engine")), 0.0, 0.0).unwrap_err();
        assert_eq!(error.message, "'shaker_dsp_type'");
    }

    #[test]
    fn native_engine_fade_matches_the_numeric_band_and_raw_flag() {
        use openrigcue_model::native_value::LegacyValue;
        for flag in [LegacyValue::Int(1), LegacyValue::Float(1.0), LegacyValue::Bool(true)] {
            let settings = NativeEngineSettings {
                flag: Ok(flag),
                fade_start: Ok(LegacyValue::Int(95)),
                fade_end: Ok(LegacyValue::Int(100)),
                resonance: Ok(8.0),
            };
            for (rpm, expected) in [(8000.0, 1.0), (9700.0, 0.9), (10000.0, 0.75), (10001.0, 0.0)] {
                for level in native_engine_levels(&settings, Ok([1.0; 6]), rpm, 10000.0).unwrap() {
                    assert!((level - expected).abs() < 1e-12, "rpm={rpm}, level={level}");
                }
            }
        }
        let settings = NativeEngineSettings {
            flag: Ok(LegacyValue::from("1")),
            fade_start: Ok(LegacyValue::None),
            fade_end: Ok(LegacyValue::from("bad")),
            resonance: Ok(8.0),
        };
        assert_eq!(native_engine_levels(&settings, Ok([1.0; 6]), 8000.0, 0.0).unwrap(), [1.0; 6]);
    }

    /// Endpoints of any width: a huge start keeps the volume (exact `perc * 100 < start`), a huge
    /// end inside the band is an overflow error at `end / 100`; an
    /// object-wrapped flag and endpoints act as their values.
    #[test]
    fn native_engine_fade_takes_wide_integer_and_object_endpoints() {
        use openrigcue_model::native_object::NativeObject;
        use openrigcue_model::native_value::LegacyValue;
        let int = |text: &str| LegacyValue::from_integer(LegacyValue::from(text).py_integer().unwrap());
        let huge = int(&format!("1{}", "0".repeat(400)));
        let settings = |start: LegacyValue, end: LegacyValue| NativeEngineSettings {
            flag: Ok(LegacyValue::Int(1)),
            fade_start: Ok(start),
            fade_end: Ok(end),
            resonance: Ok(8.0),
        };
        assert_eq!(
            native_engine_levels(&settings(huge.clone(), huge.clone()), Ok([1.0; 6]), 9700.0, 10000.0),
            Ok([1.0; 6])
        );
        let error =
            native_engine_levels(&settings(LegacyValue::Int(95), huge), Ok([1.0; 6]), 9700.0, 10000.0).unwrap_err();
        assert_eq!(
            (error.class, error.message.as_str()),
            ("OverflowError", "integer division result too large for a float")
        );
        let object = |value| LegacyValue::Object(NativeObject::new(value));
        let mut wrapped = settings(object(LegacyValue::Int(95)), object(LegacyValue::Int(100)));
        wrapped.flag = Ok(object(LegacyValue::Int(1)));
        assert_eq!(
            native_engine_levels(&wrapped, Ok([1.0; 6]), 9700.0, 10000.0),
            native_engine_levels(&settings(LegacyValue::Int(95), LegacyValue::Int(100)), Ok([1.0; 6]), 9700.0, 10000.0)
        );
    }
}
