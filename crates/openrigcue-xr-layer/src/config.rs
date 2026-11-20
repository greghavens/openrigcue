//! Layer settings, read once per `XrInstance` from the environment (so they can be set per game in
//! Steam launch options, and pass into Proton's container).

use std::path::PathBuf;

/// Environment variables the layer reads, with their defaults.
pub mod env {
    /// The rig-pose file (shared with every consumer; see `openrigcue_rigpose::PATH_ENV`).
    pub const POSE_PATH: &str = openrigcue_rigpose::PATH_ENV;
    /// Samples older than this many milliseconds are ignored (pass-through). Default 100.
    pub const MAX_AGE_MS: &str = "OPENRIGCUE_XR_MAX_AGE_MS";
    /// Furthest the rig pose is extrapolated past its measurement, milliseconds. Default 50; 0 disables prediction.
    pub const MAX_PREDICTION_MS: &str = "OPENRIGCUE_XR_MAX_PREDICTION_MS";
    /// `0` leaves controllers (action spaces) uncompensated. Default 1.
    pub const COMPENSATE_CONTROLLERS: &str = "OPENRIGCUE_XR_COMPENSATE_CONTROLLERS";
    /// `0` waits for a re-centre from the daemon instead of calibrating when the session gains focus. Default 1.
    pub const AUTO_CALIBRATE: &str = "OPENRIGCUE_XR_AUTO_CALIBRATE";
    /// How far (ms) an `xrEndFrame` display time may be from a located frame to reuse its compensation. Default 500.
    pub const SUBMIT_TOLERANCE_MS: &str = "OPENRIGCUE_XR_SUBMIT_TOLERANCE_MS";
    /// Without `XR_KHR_convert_timespec_time`: assumed time from `xrWaitFrame` to display, ms. Default 20.
    pub const FALLBACK_LOOKAHEAD_MS: &str = "OPENRIGCUE_XR_FALLBACK_LOOKAHEAD_MS";
    /// Log verbosity on stderr: 0 off, 1 state changes (default), 2 debug.
    pub const LOG: &str = "OPENRIGCUE_XR_LOG";
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub pose_path: PathBuf,
    pub max_age_ns: u64,
    pub max_prediction_ns: u64,
    pub compensate_controllers: bool,
    pub auto_calibrate: bool,
    pub submit_tolerance_ns: u64,
    pub fallback_lookahead_ns: u64,
    pub log_level: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            pose_path: PathBuf::from(openrigcue_rigpose::DEFAULT_PATH),
            max_age_ns: 100 * MS,
            max_prediction_ns: 50 * MS,
            compensate_controllers: true,
            auto_calibrate: true,
            submit_tolerance_ns: 500 * MS,
            fallback_lookahead_ns: 20 * MS,
            log_level: 1,
        }
    }
}

const MS: u64 = 1_000_000;

impl Settings {
    /// Settings from the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Settings from any key lookup; unset or unparsable values keep their defaults.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let d = Settings::default();
        let ms = |k: &str, default: u64| {
            get(k).and_then(|v| v.trim().parse::<u64>().ok()).map_or(default, |v| v.saturating_mul(MS))
        };
        let flag = |k: &str, default: bool| match get(k).as_deref().map(str::trim) {
            Some("0" | "false" | "no" | "off") => false,
            Some("1" | "true" | "yes" | "on") => true,
            _ => default,
        };
        Settings {
            pose_path: get(env::POSE_PATH).filter(|p| !p.is_empty()).map_or(d.pose_path, PathBuf::from),
            max_age_ns: ms(env::MAX_AGE_MS, d.max_age_ns),
            max_prediction_ns: ms(env::MAX_PREDICTION_MS, d.max_prediction_ns),
            compensate_controllers: flag(env::COMPENSATE_CONTROLLERS, d.compensate_controllers),
            auto_calibrate: flag(env::AUTO_CALIBRATE, d.auto_calibrate),
            submit_tolerance_ns: ms(env::SUBMIT_TOLERANCE_MS, d.submit_tolerance_ns),
            fallback_lookahead_ns: ms(env::FALLBACK_LOOKAHEAD_MS, d.fallback_lookahead_ns),
            log_level: get(env::LOG).and_then(|v| v.trim().parse().ok()).unwrap_or(d.log_level),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn defaults_and_overrides() {
        assert_eq!(Settings::from_lookup(|_| None), Settings::default());
        let vars: HashMap<&str, &str> = [
            (env::POSE_PATH, "/tmp/x"),
            (env::MAX_AGE_MS, "250"),
            (env::MAX_PREDICTION_MS, "0"),
            (env::COMPENSATE_CONTROLLERS, "0"),
            (env::AUTO_CALIBRATE, "off"),
            (env::LOG, "2"),
            (env::SUBMIT_TOLERANCE_MS, "junk"),
        ]
        .into_iter()
        .collect();
        let s = Settings::from_lookup(|k| vars.get(k).map(|v| v.to_string()));
        assert_eq!(s.pose_path, PathBuf::from("/tmp/x"));
        assert_eq!(s.max_age_ns, 250 * MS);
        assert_eq!(s.max_prediction_ns, 0);
        assert!(!s.compensate_controllers && !s.auto_calibrate);
        assert_eq!(s.log_level, 2);
        assert_eq!(s.submit_tolerance_ns, Settings::default().submit_tolerance_ns);
    }
}
