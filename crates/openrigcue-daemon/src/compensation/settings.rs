//! The VR settings: where the rig's centre of rotation sits relative to the eyes (the
//! rig-pose consumers need it) and whether the SRS-compatible
//! pose export is written. Kept in their own file, `vr-compensation.toml` in the configuration directory, and
//! saved on every change (they are not part of `config.ini` and its Save button).

use std::path::Path;

use openrigcue_rigpose::CorOffset;
use serde::{Deserialize, Serialize};

/// `vr-compensation.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VrSettings {
    /// How far the centre of rotation is in front of the eyes, mm (negative: behind).
    pub cor_forward_mm: f64,
    /// How far it is below the eyes, mm; absent: `MOTION_COMPENSATION.height` (Seated Height), which
    /// is also the head's height above the rotation centre for the derived surge and sway.
    pub cor_down_mm: Option<f64>,
    /// How far it is to the right of the eyes, mm.
    pub cor_right_mm: f64,
    /// Also write the SRS-compatible `Local\SimRacingStudioMotionRigPose` section (six doubles) for the
    /// bridge, for OpenXR-MotionCompensation or OVRMC running inside a Proton prefix.
    pub srs_export: bool,
}

impl Default for VrSettings {
    fn default() -> Self {
        VrSettings { cor_forward_mm: 0.0, cor_down_mm: None, cor_right_mm: 0.0, srs_export: true }
    }
}

/// Why the settings could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum VrSettingsError {
    #[error("{path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("{path}: {source}")]
    Parse { path: String, source: toml::de::Error },
    #[error("{0}")]
    Invalid(String),
}

impl VrSettings {
    /// The offset published in the rig-pose file, in metres; `seated_height_mm` is the Seated Height setting as a float.
    pub fn cor_offset(&self, seated_height_mm: f64) -> CorOffset {
        CorOffset {
            forward: self.cor_forward_mm / 1000.0,
            down: self.cor_down_mm.unwrap_or(seated_height_mm) / 1000.0,
            right: self.cor_right_mm / 1000.0,
        }
    }

    /// Every distance must be a finite number.
    pub fn validate(&self) -> Result<(), VrSettingsError> {
        let values = [Some(self.cor_forward_mm), self.cor_down_mm, Some(self.cor_right_mm)];
        if values.iter().flatten().all(|v| v.is_finite()) {
            Ok(())
        } else {
            Err(VrSettingsError::Invalid("the centre-of-rotation distances must be finite numbers".into()))
        }
    }

    /// Reads `path`; a missing file gives the defaults.
    pub fn load(path: &Path) -> Result<VrSettings, VrSettingsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(VrSettings::default()),
            Err(source) => return Err(VrSettingsError::Io { path: path.display().to_string(), source }),
        };
        let settings: VrSettings = toml::from_str(&text)
            .map_err(|source| VrSettingsError::Parse { path: path.display().to_string(), source })?;
        settings.validate()?;
        Ok(settings)
    }

    /// Writes `path` atomically and durably, as every configuration file is written
    /// ([`openrigcue_config::settings::paths::write_atomic`]).
    pub fn save(&self, path: &Path) -> Result<(), VrSettingsError> {
        let text = toml::to_string(self).map_err(|e| VrSettingsError::Invalid(e.to_string()))?;
        openrigcue_config::settings::paths::write_atomic(path, text.as_bytes()).map_err(|error| match error {
            openrigcue_config::StoreError::Io { path, source } => {
                VrSettingsError::Io { path: path.display().to_string(), source }
            }
            other => VrSettingsError::Invalid(other.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CoR below the eyes follows the Seated Height (`MOTION_COMPENSATION.height`, mm) unless set.
    #[test]
    fn cor_offset_in_metres() {
        let mut s = VrSettings::default();
        assert_eq!(s.cor_offset(1000.0), CorOffset { forward: 0.0, down: 1.0, right: 0.0 });
        s.cor_forward_mm = 120.0;
        s.cor_down_mm = Some(650.0);
        s.cor_right_mm = -20.0;
        assert_eq!(s.cor_offset(1000.0), CorOffset { forward: 0.12, down: 0.65, right: -0.02 });
    }

    #[test]
    fn file_round_trip_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vr-compensation.toml");
        assert_eq!(VrSettings::load(&path).unwrap(), VrSettings::default());
        let s = VrSettings { cor_forward_mm: 80.0, cor_down_mm: Some(600.0), cor_right_mm: 0.0, srs_export: false };
        s.save(&path).unwrap();
        assert_eq!(VrSettings::load(&path).unwrap(), s);
        let defaults = VrSettings::default();
        defaults.save(&path).unwrap();
        assert_eq!(VrSettings::load(&path).unwrap().cor_down_mm, None, "absent key = follow Seated Height");
        std::fs::write(&path, "cor_dwon_mm = 1.0\n").unwrap();
        assert!(matches!(VrSettings::load(&path), Err(VrSettingsError::Parse { .. })));
        std::fs::write(&path, "cor_forward_mm = nan\n").unwrap();
        assert!(matches!(VrSettings::load(&path), Err(VrSettingsError::Invalid(_))));
    }
}
