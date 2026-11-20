//! Global motion settings: the `config.ini` sections `[MOTIONBOX]`, `[CUSTOM_FEATURES]` and the
//! per-manufacturer section, as typed values.
//!
//! The motion code reads its configuration through [`ConfigView`] (string values converted where they are
//! read). [`MotionSettings`] is a configuration whose values are already numbers and
//! booleans; it serves them to the motion code as the strings a config file would hold. A configuration
//! held as strings is a [`crate::RawConfig`].
//!
//! Defaults are the values of a first install on Windows.

use std::borrow::Cow;

use crate::axis::{Axis, PerAxis};
use crate::error::{mh_module, CueError};
use crate::platform::Manufacturer;
use crate::timing::HostOs;
use crate::views::{config_float, ConfigView, Missing};

/// Global motion settings read on every frame.
#[derive(Debug, Clone, PartialEq)]
pub struct MotionSettings {
    /// `MOTIONBOX.refresh_rate`, Hz.
    pub refresh_rate: i64,
    /// `MOTIONBOX.soft_crash_factor`, percent of a large step that is removed (rotary shaping).
    pub soft_crash_factor: i64,
    /// `MOTIONBOX.soft_crash_enabled`: `true` is the string `yes`, `false` the string `no` (the rotary
    /// shaping compares the raw string with `'yes'`; the actuator shaping applies the lenient boolean parse).
    pub soft_crash_enabled: bool,
    /// `MOTIONBOX.boost_multiplier`.
    pub boost_multiplier: f64,
    /// `MOTIONBOX.sway_curving_factor`: exponent of the sway curve.
    pub sway_curving_factor: f64,
    /// `MOTIONBOX.surge_curving_factor`: exponent of the surge curve.
    pub surge_curving_factor: f64,
    /// `MOTIONBOX.auto_smoothing_low_speed / _speed / _slider`.
    pub low_speed_smoothing: LowSpeedSmoothing,
    /// `MOTIONBOX.gear_smoothing_min_delta`: smallest longitudinal step the follower filter acts on.
    pub gear_smoothing_min_delta: i64,
    /// `MOTIONBOX.overall_smoothing_map_min_slider / _max_slider`.
    pub overall_smoothing_map: SliderMap,
    /// `CUSTOM_FEATURES.enable_legacy_gear_smoothing`.
    pub legacy_gear_smoothing: bool,
    /// `CUSTOM_FEATURES.dams_*`: speed-dependent smoothing of the traction-loss axes.
    pub traction_loss_auto_smoothing: TractionLossAutoSmoothing,
    /// The section named after the platform's manufacturer.
    pub manufacturer: ManufacturerSettings,
}

impl Default for MotionSettings {
    /// The settings of a first install of the Windows build with a DOF REALITY platform.
    fn default() -> Self {
        MotionSettings::for_manufacturer(Manufacturer::DofReality, HostOs::Windows)
    }
}

impl MotionSettings {
    /// Settings as a fresh installation on `os` has them once `manufacturer` is chosen on the Hardware
    /// page: the refresh rate is the manufacturer's `default_refresh_rate`, or the MOTIONBOX validation
    /// default 30 where the manufacturer table has no entry for it on that platform.
    pub fn for_manufacturer(manufacturer: Manufacturer, os: HostOs) -> Self {
        MotionSettings {
            refresh_rate: manufacturer.default_refresh_rate(os).unwrap_or(30),
            soft_crash_factor: 90,
            soft_crash_enabled: true,
            boost_multiplier: 2.0,
            sway_curving_factor: 1.0,
            surge_curving_factor: 1.0,
            low_speed_smoothing: LowSpeedSmoothing::default(),
            gear_smoothing_min_delta: 1,
            overall_smoothing_map: SliderMap::default(),
            legacy_gear_smoothing: false,
            traction_loss_auto_smoothing: TractionLossAutoSmoothing::default(),
            manufacturer: ManufacturerSettings::for_manufacturer(manufacturer),
        }
    }
}

/// Fixed smoothing window below a speed (`auto_smoothing_*`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LowSpeedSmoothing {
    /// `auto_smoothing_low_speed`.
    pub enabled: bool,
    /// `auto_smoothing_speed`, same unit as the telemetry `speed` channel.
    pub below_speed: f64,
    /// `auto_smoothing_slider`: the smoothing window used below that speed.
    pub window: i64,
}

impl Default for LowSpeedSmoothing {
    fn default() -> Self {
        LowSpeedSmoothing { enabled: false, below_speed: 1.66, window: 60 }
    }
}

/// Range onto which the 0–100 smoothing factor is mapped by the actuator overall smoothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliderMap {
    pub min: i64,
    pub max: i64,
}

impl Default for SliderMap {
    fn default() -> Self {
        SliderMap { min: 90, max: 99 }
    }
}

/// Speed-dependent traction-loss smoothing (`CUSTOM_FEATURES.dams_*`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TractionLossAutoSmoothing {
    /// `dams_front_tl_mode`; modes 1 and 2 enable the speed scaling.
    pub mode: i64,
    /// `dams_tl_max_auto_smoothing`: the window at standstill.
    pub max_window: i64,
    /// `dams_tl_auto_smoothing_max_speed_kmh`: the speed at which the tune's own window is reached.
    pub max_speed_kmh: f64,
}

impl Default for TractionLossAutoSmoothing {
    fn default() -> Self {
        TractionLossAutoSmoothing { mode: 0, max_window: 80, max_speed_kmh: 80.0 }
    }
}

impl TractionLossAutoSmoothing {
    pub fn enabled(&self) -> bool {
        matches!(self.mode, 1 | 2)
    }
}

/// The per-manufacturer section. A `None` value is a key missing from the section; a section whose values
/// are all `None` is a section the configuration does not have. Every section of a fresh or migrated
/// configuration has `real_heave`, which the motion code only tests for presence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ManufacturerSettings {
    /// Rotary shaping: percent of `max_motor_abs` (missing: 15 %). Actuator shaping: a ratio margin used
    /// as is (missing: 0.01).
    pub smoothing_threshold: Option<f64>,
    /// Percent of `max_motor_abs` (missing: 50 %).
    pub soft_crash_threshold: Option<f64>,
    /// Percent of `max_motor_abs` (missing: 15 %).
    pub boost_threshold: Option<f64>,
    /// Smoothing window used on every axis during soft start/stop. Missing: a frame during soft start fails.
    pub soft_start_stop_factor: Option<i64>,
    /// Soft start/stop duration, seconds. Missing: soft start never ends once engaged.
    pub soft_start_stop_timer: Option<i64>,
}

impl ManufacturerSettings {
    /// The section as a current installation has it.
    ///
    /// THANOS shares PT-ACTUATOR's values except the smoothing threshold; SIMTECHREVOLUTION shares
    /// PROSIMU's; there is no `None` section. The sections are the same on every operating system.
    pub fn for_manufacturer(manufacturer: Manufacturer) -> Self {
        let section = |smoothing: f64, boost: f64, factor: i64| ManufacturerSettings {
            smoothing_threshold: Some(smoothing),
            soft_crash_threshold: Some(50.0),
            boost_threshold: Some(boost),
            soft_start_stop_factor: Some(factor),
            soft_start_stop_timer: Some(4),
        };
        match manufacturer {
            Manufacturer::None => ManufacturerSettings {
                smoothing_threshold: None,
                soft_crash_threshold: None,
                boost_threshold: None,
                soft_start_stop_factor: None,
                soft_start_stop_timer: None,
            },
            Manufacturer::DofReality => section(15.0, 15.0, 30),
            Manufacturer::Prosimu | Manufacturer::YawVr | Manufacturer::SimTechRevolution => section(0.01, 15.0, 90),
            Manufacturer::PtActuator => section(100000.0, 5.0, 90),
            Manufacturer::Thanos | Manufacturer::VnmSimulation => section(0.01, 5.0, 90),
        }
    }
}

/// `MOTIONBOX.*_clip_factor`: an axis value is limited to `|clip · maximum|`.
///
/// These are read once at start-up, so they are fixed for the life of a [`crate::MotionCueing`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipFactors(pub PerAxis<f64>);

impl Default for ClipFactors {
    fn default() -> Self {
        ClipFactors(PerAxis::splat(1000.0))
    }
}

impl ClipFactors {
    /// Reads `MOTIONBOX.<axis>_clip_factor` as a float for every axis, in axis order. An error
    /// names the first key that is missing or not a number.
    pub fn read(config: &dyn ConfigView) -> Result<ClipFactors, CueError> {
        let mut clips = PerAxis::splat(0.0);
        for (line, axis) in (60..).zip(Axis::ALL) {
            clips[axis] = config_float(config, "MOTIONBOX", &format!("{}_clip_factor", axis.name()))
                .map_err(|e| e.at(mh_module(line)))?;
        }
        Ok(ClipFactors(clips))
    }
}

/// A typed value as the string a config file holds for it.
fn number(v: f64) -> Cow<'static, str> {
    Cow::Owned(format!("{v}"))
}

fn integer(v: i64) -> Cow<'static, str> {
    Cow::Owned(v.to_string())
}

fn yes_no(v: bool) -> Cow<'static, str> {
    Cow::Borrowed(if v { "yes" } else { "no" })
}

impl ConfigView for MotionSettings {
    fn value(&self, section: &str, key: &str) -> Result<Cow<'_, str>, Missing> {
        let key = key.to_lowercase();
        match section {
            "MOTIONBOX" => Ok(match key.as_str() {
                "refresh_rate" => integer(self.refresh_rate),
                "soft_crash_factor" => integer(self.soft_crash_factor),
                "soft_crash_enabled" => yes_no(self.soft_crash_enabled),
                "boost_multiplier" => number(self.boost_multiplier),
                "sway_curving_factor" => number(self.sway_curving_factor),
                "surge_curving_factor" => number(self.surge_curving_factor),
                "auto_smoothing_low_speed" => yes_no(self.low_speed_smoothing.enabled),
                "auto_smoothing_speed" => number(self.low_speed_smoothing.below_speed),
                "auto_smoothing_slider" => integer(self.low_speed_smoothing.window),
                "gear_smoothing_min_delta" => integer(self.gear_smoothing_min_delta),
                "overall_smoothing_map_min_slider" => integer(self.overall_smoothing_map.min),
                "overall_smoothing_map_max_slider" => integer(self.overall_smoothing_map.max),
                _ => return Err(Missing::Key),
            }),
            "CUSTOM_FEATURES" => Ok(match key.as_str() {
                "enable_legacy_gear_smoothing" => yes_no(self.legacy_gear_smoothing),
                "dams_front_tl_mode" => integer(self.traction_loss_auto_smoothing.mode),
                "dams_tl_max_auto_smoothing" => integer(self.traction_loss_auto_smoothing.max_window),
                "dams_tl_auto_smoothing_max_speed_kmh" => number(self.traction_loss_auto_smoothing.max_speed_kmh),
                _ => return Err(Missing::Key),
            }),
            _ => self.manufacturer.value(&key),
        }
    }
}

impl ManufacturerSettings {
    fn value(&self, key: &str) -> Result<Cow<'static, str>, Missing> {
        let m = self;
        if [m.smoothing_threshold, m.soft_crash_threshold, m.boost_threshold].iter().all(Option::is_none)
            && m.soft_start_stop_factor.is_none()
            && m.soft_start_stop_timer.is_none()
        {
            return Err(Missing::Section);
        }
        let value = match key {
            "smoothing_threshold" => m.smoothing_threshold.map(number),
            "soft_crash_threshold" => m.soft_crash_threshold.map(number),
            "boost_threshold" => m.boost_threshold.map(number),
            "soft_start_stop_factor" => m.soft_start_stop_factor.map(integer),
            "soft_start_stop_timer" => m.soft_start_stop_timer.map(integer),
            "real_heave" => Some(yes_no(false)),
            _ => None,
        };
        value.ok_or(Missing::Key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_config_migrations() {
        let s = MotionSettings::default();
        assert_eq!(s.refresh_rate, 30);
        assert_eq!(s.soft_crash_factor, 90);
        assert!(s.soft_crash_enabled);
        assert_eq!(s.boost_multiplier, 2.0);
        assert_eq!(s.sway_curving_factor, 1.0);
        assert_eq!(s.surge_curving_factor, 1.0);
        assert_eq!(s.low_speed_smoothing, LowSpeedSmoothing { enabled: false, below_speed: 1.66, window: 60 });
        assert_eq!(s.gear_smoothing_min_delta, 1);
        assert_eq!(s.overall_smoothing_map, SliderMap { min: 90, max: 99 });
        assert!(!s.legacy_gear_smoothing);
        assert!(!s.traction_loss_auto_smoothing.enabled());
        assert_eq!(s.manufacturer.smoothing_threshold, Some(15.0));
        assert_eq!(s.manufacturer.soft_start_stop_factor, Some(30));
        assert_eq!(s.manufacturer.soft_start_stop_timer, Some(4));
    }

    #[test]
    fn manufacturer_sections() {
        let pt = ManufacturerSettings::for_manufacturer(Manufacturer::PtActuator);
        assert_eq!(pt.smoothing_threshold, Some(100000.0));
        assert_eq!(pt.boost_threshold, Some(5.0));
        let thanos = ManufacturerSettings::for_manufacturer(Manufacturer::Thanos);
        assert_eq!(thanos.smoothing_threshold, Some(0.01));
        let yaw = MotionSettings::for_manufacturer(Manufacturer::YawVr, HostOs::Windows);
        assert_eq!(
            MotionSettings::default(),
            MotionSettings::for_manufacturer(Manufacturer::DofReality, HostOs::Windows)
        );
        assert_eq!(yaw.refresh_rate, 500);
        assert_eq!(MotionSettings::for_manufacturer(Manufacturer::YawVr, HostOs::Linux).refresh_rate, 30);
        assert_eq!(yaw.manufacturer.soft_start_stop_factor, Some(90));
        assert_eq!(ClipFactors::default().0, PerAxis::splat(1000.0));
    }

    #[test]
    fn typed_settings_serve_config_strings() {
        let s = MotionSettings::default();
        assert_eq!(s.value("MOTIONBOX", "soft_crash_enabled").unwrap(), "yes");
        assert_eq!(s.value("MOTIONBOX", "auto_smoothing_speed").unwrap(), "1.66");
        assert_eq!(s.value("DOF REALITY", "smoothing_threshold").unwrap(), "15");
        assert_eq!(s.value("DOF REALITY", "nothing"), Err(Missing::Key));
        let none = MotionSettings { manufacturer: ManufacturerSettings::for_manufacturer(Manufacturer::None), ..s };
        assert_eq!(none.value("None", "real_heave"), Err(Missing::Section));
    }

    #[test]
    fn clip_factors_are_read_with_float() {
        let mut c = crate::views::RawConfig::new();
        for axis in Axis::ALL {
            c.set("MOTIONBOX", &format!("{}_clip_factor", axis.name()), "1000");
        }
        assert_eq!(ClipFactors::read(&c), Ok(ClipFactors::default()));
        c.set("MOTIONBOX", "yaw_clip_factor", "x");
        let e = ClipFactors::read(&c).unwrap_err();
        assert_eq!(e.to_string(), "could not convert string to float: 'x'");
        assert!(e.traceback().contains("line 62, in init backend.controllers.motion_helpers\n"));
    }
}
