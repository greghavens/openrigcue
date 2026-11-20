//! Driver settings, read from `~/.config/openrigcue/steamvr-driver.toml`.
//!
//! Every field is optional; a missing or unreadable file means defaults. The file is re-read when
//! it changes, so settings can be tuned while SteamVR runs.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::seqlock::Words;

/// How the derivatives SteamVR uses for its own pose prediction are treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VelocityMode {
    /// Remove the rig-induced velocity (including the lever arm about the CoR), so SteamVR's
    /// prediction extrapolates only the driver's own head motion.
    #[default]
    Compensate,
    /// Zero all four derivatives (no SteamVR prediction for compensated devices).
    Zero,
    /// Rotate the derivatives with the compensation but leave the rig's motion in them.
    Keep,
}

/// Where the centre-of-rotation offset comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CorSource {
    #[default]
    File,
    Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Master switch for this driver.
    pub enabled: bool,
    pub compensate_controllers: bool,
    /// Generic trackers (pucks). Off by default: a tracker may be mounted on the rig as a reference.
    pub compensate_trackers: bool,
    /// Calibrate automatically from the first headset pose once the rig pose is live.
    pub auto_calibrate: bool,
    /// Take the centre-of-rotation offset from the rig-pose file (the daemon's setting, default)
    /// or from the `cor_*` fields below.
    pub cor_source: CorSource,
    /// Position of the rig's centre of rotation relative to the head at calibration, in metres,
    /// measured in the seat's frame (forward = the way the seat faces). Used with
    /// `cor_source = "config"`.
    pub cor_forward_m: f64,
    pub cor_down_m: f64,
    pub cor_right_m: f64,
    /// Seat heading relative to the head's heading at calibration, degrees, positive = left.
    pub cor_yaw_offset_deg: f64,
    /// Pass through whenever a live OpenXR layer holds the active-consumer claim, instead of
    /// outranking it. Off by default: on SteamVR this driver also covers OpenXR apps and the
    /// compositor, so the contract ranks it above the layer.
    pub yield_to_openxr_layer: bool,
    /// Smoothing time constant for rates estimated from samples that carry none, ms.
    pub rate_smoothing_ms: f64,
    /// Time from the rig moving to the sample's timestamp (sensor filtering and transport), ms.
    pub rig_latency_ms: f64,
    /// Longest extrapolation of the rig pose towards the pose time, ms.
    pub max_extrapolation_ms: f64,
    /// A sample older than this starts fading compensation out, ms.
    pub stale_after_ms: f64,
    /// Fade-out duration once stale, ms; compensation is off after `stale_after_ms + fade_ms`.
    pub fade_ms: f64,
    pub velocity: VelocityMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: true,
            compensate_controllers: true,
            compensate_trackers: false,
            auto_calibrate: true,
            cor_source: CorSource::File,
            cor_forward_m: 0.0,
            cor_down_m: 0.0,
            cor_right_m: 0.0,
            cor_yaw_offset_deg: 0.0,
            yield_to_openxr_layer: false,
            rate_smoothing_ms: 15.0,
            rig_latency_ms: 0.0,
            max_extrapolation_ms: 50.0,
            stale_after_ms: 250.0,
            fade_ms: 500.0,
            velocity: VelocityMode::Compensate,
        }
    }
}

impl Settings {
    pub fn parse(text: &str) -> Result<Settings, toml::de::Error> {
        toml::from_str(text)
    }

    /// Reads `path`; a missing file gives defaults, a malformed one an error message.
    pub fn load(path: &Path) -> Result<Settings, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Settings::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// `$OPENRIGCUE_STEAMVR_CONFIG`, else `$XDG_CONFIG_HOME/openrigcue/steamvr-driver.toml`,
    /// else `$HOME/.config/openrigcue/steamvr-driver.toml`.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("OPENRIGCUE_STEAMVR_CONFIG") {
            return Some(PathBuf::from(p));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("openrigcue").join("steamvr-driver.toml"))
    }
}

pub(crate) const SETTINGS_WORDS: usize = 13;

impl Words<SETTINGS_WORDS> for Settings {
    fn to_words(&self) -> [u64; SETTINGS_WORDS] {
        let flags = u64::from(self.enabled)
            | u64::from(self.compensate_controllers) << 1
            | u64::from(self.compensate_trackers) << 2
            | u64::from(self.auto_calibrate) << 3
            | u64::from(self.cor_source == CorSource::Config) << 4
            | u64::from(self.yield_to_openxr_layer) << 5;
        let velocity = match self.velocity {
            VelocityMode::Compensate => 0,
            VelocityMode::Zero => 1,
            VelocityMode::Keep => 2,
        };
        [
            flags,
            velocity,
            self.cor_forward_m.to_bits(),
            self.cor_down_m.to_bits(),
            self.cor_right_m.to_bits(),
            self.cor_yaw_offset_deg.to_bits(),
            self.rig_latency_ms.to_bits(),
            self.max_extrapolation_ms.to_bits(),
            self.stale_after_ms.to_bits(),
            self.fade_ms.to_bits(),
            self.rate_smoothing_ms.to_bits(),
            0,
            0,
        ]
    }

    fn from_words(w: &[u64; SETTINGS_WORDS]) -> Self {
        let f = |i: usize| f64::from_bits(w[i]);
        Settings {
            enabled: w[0] & 1 != 0,
            compensate_controllers: w[0] & 2 != 0,
            compensate_trackers: w[0] & 4 != 0,
            auto_calibrate: w[0] & 8 != 0,
            cor_source: if w[0] & 16 != 0 { CorSource::Config } else { CorSource::File },
            yield_to_openxr_layer: w[0] & 32 != 0,
            rate_smoothing_ms: f(10),
            velocity: match w[1] {
                1 => VelocityMode::Zero,
                2 => VelocityMode::Keep,
                _ => VelocityMode::Compensate,
            },
            cor_forward_m: f(2),
            cor_down_m: f(3),
            cor_right_m: f(4),
            cor_yaw_offset_deg: f(5),
            rig_latency_ms: f(6),
            max_extrapolation_ms: f(7),
            stale_after_ms: f(8),
            fade_ms: f(9),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_defaults() {
        assert_eq!(Settings::parse("").unwrap(), Settings::default());
    }

    #[test]
    fn parses_fields_and_rejects_typos() {
        let s = Settings::parse("cor_down_m = 0.65\nvelocity = \"zero\"\ncompensate_trackers = true\n").unwrap();
        assert_eq!(s.cor_down_m, 0.65);
        assert_eq!(s.velocity, VelocityMode::Zero);
        assert!(s.compensate_trackers);
        assert!(Settings::parse("cor_dwon_m = 1.0").is_err());
    }

    #[test]
    fn word_round_trip() {
        let s = Settings {
            cor_right_m: -0.1,
            velocity: VelocityMode::Keep,
            enabled: false,
            cor_source: CorSource::Config,
            yield_to_openxr_layer: true,
            rate_smoothing_ms: 3.0,
            ..Settings::default()
        };
        assert_eq!(Settings::from_words(&s.to_words()), s);
    }

    #[test]
    fn shipped_example_parses() {
        let text = include_str!("../driver/openrigcue/resources/steamvr-driver.example.toml");
        Settings::parse(text).unwrap();
    }
}
