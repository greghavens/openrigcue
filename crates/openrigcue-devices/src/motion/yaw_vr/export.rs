//! The `YawVRGEFile` shared-memory block that Windows programs reading the Yaw VR "game engine"
//! data expect.
//!
//! - The structure is packed: four 32-bit floats `YAW`, `PITCH`, `ROLL`, `BATTERY` and one boolean
//!   byte `SIXDOF`, 17 bytes in all.
//! - The block is created once, with every field zero, and kept for the whole run.
//! - Each feedback datagram that starts with `SY` stores its `YAW`, `PITCH`, `ROLL` and `BATTERY`
//!   values (0 for a field that is not a number), then `SIXDOF` from the `Yaw VR.6dof_mode`
//!   setting. Nothing else writes it: an unexpected or non-ASCII datagram, a timeout or a
//!   disconnect leave the last values in place.
//!
//! OpenRigCue writes these 17 bytes to a Linux file named like a bridge mirror
//! (`/dev/shm/openrigcue/YawVRGEFile`); `openrigcue-bridge` publishes it inside a game's Wine/Proton prefix as
//! the Windows section `YawVRGEFile` (its `catalog::EXPORTS`).

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::protocol::YawReport;

/// The tag name of the mapping, unchanged by the bridge's
/// file-name flattening.
pub const SECTION_NAME: &str = "YawVRGEFile";

/// The size of the block: four 32-bit floats and one boolean byte, packed.
pub const SIZE: usize = 17;

/// Byte offsets of the fields (packed: no padding).
const YAW_OFFSET: usize = 0;
const PITCH_OFFSET: usize = 4;
const ROLL_OFFSET: usize = 8;
const BATTERY_OFFSET: usize = 12;
const SIXDOF_OFFSET: usize = 16;

/// The content of the `YawVRGEFile` block.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GameEngineRecord {
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    pub battery: f32,
    pub sixdof: bool,
}

impl GameEngineRecord {
    /// The record after a report: the four values stored as 32-bit floats (the
    /// 64-bit value rounded to single precision) and `SIXDOF` from `6dof_mode`.
    pub fn from_report(report: &YawReport, sixdof: bool) -> GameEngineRecord {
        GameEngineRecord {
            yaw: report.yaw as f32,
            pitch: report.pitch as f32,
            roll: report.roll as f32,
            battery: report.battery as f32,
            sixdof,
        }
    }

    /// The structure's bytes as a Windows x64 program sees them (little endian, the boolean 0 or 1).
    pub fn to_bytes(&self) -> [u8; SIZE] {
        let mut bytes = [0; SIZE];
        for (offset, value) in [
            (YAW_OFFSET, self.yaw),
            (PITCH_OFFSET, self.pitch),
            (ROLL_OFFSET, self.roll),
            (BATTERY_OFFSET, self.battery),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[SIXDOF_OFFSET] = u8::from(self.sixdof);
        bytes
    }
}

/// The Linux side of the `YawVRGEFile` section: created zeroed, rewritten in place (same
/// inode, always [`SIZE`] bytes) on every report.
#[derive(Debug)]
pub struct GameEngineExport {
    path: PathBuf,
    file: Mutex<File>,
}

impl GameEngineExport {
    /// The file for the section in `dir` (the bridge's mirror directory).
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(SECTION_NAME)
    }

    /// Creates the block with every field zero. A file left by an earlier run is
    /// reset.
    pub fn create(path: impl Into<PathBuf>) -> io::Result<GameEngineExport> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).truncate(true).write(true).open(&path)?;
        let export = GameEngineExport { path, file: Mutex::new(file) };
        export.write(&GameEngineRecord::default())?;
        Ok(export)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stores `record` in the block.
    pub fn write(&self, record: &GameEngineRecord) -> io::Result<()> {
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&record.to_bytes())
    }

    /// The end of the run: the file is removed, so the bridge
    /// releases its section. A write still in flight lands in the unlinked file and is never seen.
    pub fn remove(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openrigcue-yawvr-export-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Packed fields YAW, PITCH, ROLL, BATTERY (32-bit floats) and SIXDOF (a boolean byte): 17 bytes, no padding,
    /// little endian.
    #[test]
    fn layout_matches_the_ctypes_structure() {
        let record = GameEngineRecord { yaw: 1.5, pitch: -2.25, roll: 0.125, battery: 87.0, sixdof: true };
        let bytes = record.to_bytes();
        assert_eq!(bytes.len(), 17);
        assert_eq!(&bytes[0..4], &1.5f32.to_le_bytes());
        assert_eq!(&bytes[4..8], &(-2.25f32).to_le_bytes());
        assert_eq!(&bytes[8..12], &0.125f32.to_le_bytes());
        assert_eq!(&bytes[12..16], &87.0f32.to_le_bytes());
        assert_eq!(bytes[16], 1);
        assert_eq!(GameEngineRecord { sixdof: false, ..record }.to_bytes()[16], 0);
        assert_eq!(GameEngineRecord::default().to_bytes(), [0; 17], "init_mmap zeroes every field");
    }

    /// A field stores the 64-bit value rounded to single precision.
    #[test]
    fn report_values_become_c_floats() {
        let report = YawReport { yaw: 0.1, pitch: -179.99, roll: 1e40, battery: 90.0 };
        let record = GameEngineRecord::from_report(&report, false);
        assert_eq!(record.yaw, 0.1f32);
        assert_eq!(record.pitch, -179.99f32);
        assert_eq!(record.roll, f32::INFINITY);
        assert_eq!(record.battery, 90.0);
        assert!(!record.sixdof);
    }

    /// Initialisation creates the zeroed block (also over a stale file); each report rewrites it in place; the
    /// end of the run removes it.
    #[test]
    fn file_lifecycle() {
        let dir = temp_dir("lifecycle");
        let path = GameEngineExport::path_in(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, [0xAA; 40]).unwrap();
        let export = GameEngineExport::create(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), [0; 17]);

        let record = GameEngineRecord { yaw: 12.5, pitch: -3.0, roll: 0.25, battery: 87.0, sixdof: true };
        export.write(&record).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), record.to_bytes());

        export.remove().unwrap();
        assert!(!path.exists());
        export.write(&record).unwrap();
        assert!(!path.exists(), "a late write does not bring the section back");
        export.remove().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
