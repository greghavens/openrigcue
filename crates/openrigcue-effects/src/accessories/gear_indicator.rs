//! SymProjects "gimax" gear indicator: a USB HID display taking three-byte reports.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use tracing::debug;

use super::numeric::{map_clamped, truncate};

/// USB ids of the display (vendor 1240, product 515).
pub const VENDOR_ID: u16 = 1240;
pub const PRODUCT_ID: u16 = 515;

/// Segment code of a gear; -1 is reverse, 0 neutral, 10 and 11 are blanking codes.
pub fn gear_code(gear: f64) -> Option<u8> {
    if gear.fract() != 0.0 {
        return None;
    }
    Some(match gear as i64 {
        -1 => 40,
        0 => 42,
        1 => 6,
        2 => 109,
        3 => 103,
        4 => 54,
        5 => 115,
        6 => 123,
        7 => 7,
        8 => 127,
        9 => 119,
        10 => 32,
        11 => 0,
        _ => return None,
    })
}

/// Brightness byte of a level 0..5.
fn brightness_code(level: i64) -> Option<u8> {
    Some(match level {
        0 => 55,
        1 => 95,
        2 => 135,
        3 => 175,
        4 => 215,
        5 => 255,
        _ => return None,
    })
}

/// The gear report `[0, 1 if shift light else 2, code]`; `None` for a gear without a code.
pub fn gear_report(gear: f64, shift_light: f64) -> Option<[u8; 3]> {
    Some([0, if shift_light == 1.0 { 1 } else { 2 }, gear_code(gear)?])
}

/// The brightness report `[0, 0, code]` for 0..100 % mapped to the six levels.
pub fn brightness_report(percent: i64) -> Option<[u8; 3]> {
    let level = truncate(map_clamped(percent as f64, 0.0, 100.0, 0.0, 5.0))?;
    Some([0, 0, brightness_code(level)?])
}

/// Writes HID output reports to the display.
pub trait HidWriter {
    /// Send one report; `Ok(false)` when the device is absent.
    fn write_report(&mut self, report: &[u8; 3]) -> io::Result<bool>;
}

/// The display through Linux hidraw, found by its USB ids in sysfs and opened
/// anew for every report.
#[derive(Debug, Default, Clone, Copy)]
pub struct Hidraw;

impl Hidraw {
    /// `/dev/hidrawN` of the first hidraw node whose `HID_ID` is the display's.
    fn find() -> Option<PathBuf> {
        let wanted = format!("HID_ID=0003:{:08X}:{:08X}", VENDOR_ID, PRODUCT_ID);
        fs::read_dir("/sys/class/hidraw").ok()?.flatten().find_map(|entry| {
            let uevent = fs::read_to_string(entry.path().join("device/uevent")).ok()?;
            uevent
                .lines()
                .any(|l| l.eq_ignore_ascii_case(&wanted))
                .then(|| PathBuf::from("/dev").join(entry.file_name()))
        })
    }
}

impl HidWriter for Hidraw {
    fn write_report(&mut self, report: &[u8; 3]) -> io::Result<bool> {
        let Some(node) = Self::find() else { return Ok(false) };
        let mut device = fs::OpenOptions::new().write(true).open(node)?;
        device.write_all(report)?;
        Ok(true)
    }
}

/// Pause of the update loop without a game and with one.
pub fn loop_period(game_running: bool) -> Duration {
    if game_running {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(1)
    }
}

/// One pass of the display loop: brightness, then gear and shift light, each attempted on its own; reports
/// that cannot be built are skipped and failed writes are returned.
///
/// `brightness_percent` is `SYMPROJECTS.gi_max_brightness` as an integer, `None` when it does not
/// convert (the brightness report is then skipped).
pub fn update(
    display: &mut dyn HidWriter,
    brightness_percent: Option<i64>,
    gear: f64,
    shift_light: f64,
) -> Vec<io::Error> {
    let reports = [brightness_percent.and_then(brightness_report), gear_report(gear, shift_light)];
    reports
        .into_iter()
        .flatten()
        .filter_map(|report| display.write_report(&report).err())
        .inspect(|e| debug!("SymProjects report failed: {e}"))
        .collect()
}

/// The first report of the loop: reverse with the indicator on.
pub fn start(display: &mut dyn HidWriter) -> io::Result<()> {
    if let Some(report) = gear_report(-1.0, 1.0) {
        display.write_report(&report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorder(Vec<[u8; 3]>);

    impl HidWriter for Recorder {
        fn write_report(&mut self, report: &[u8; 3]) -> io::Result<bool> {
            self.0.push(*report);
            Ok(true)
        }
    }

    /// Gear and brightness byte tables.
    #[test]
    fn reports() {
        assert_eq!(gear_report(3.0, 0.0), Some([0, 2, 103]));
        assert_eq!(gear_report(-1.0, 1.0), Some([0, 1, 40]));
        assert_eq!(gear_report(12.0, 0.0), None);
        assert_eq!(gear_report(2.5, 0.0), None);
        assert_eq!(brightness_report(100), Some([0, 0, 255]));
        assert_eq!(brightness_report(39), Some([0, 0, 95]));
        assert_eq!(brightness_report(120), Some([0, 0, 255]));
    }

    /// Display loop order and pauses.
    #[test]
    fn clamped_auxiliary_maps_bound_gear_brightness_before_lookup() {
        assert_eq!(brightness_report(-100), Some([0, 0, 55]));
        assert_eq!(brightness_report(200), Some([0, 0, 255]));
    }

    #[test]
    fn loop_pass() {
        let mut r = Recorder(Vec::new());
        start(&mut r).unwrap();
        assert!(update(&mut r, Some(50), 4.0, 1.0).is_empty());
        assert_eq!(r.0, vec![[0, 1, 40], [0, 0, 135], [0, 1, 54]]);
        assert_eq!(loop_period(false), Duration::from_secs(1));
        assert_eq!(loop_period(true), Duration::from_millis(200));
    }

    /// Refuses brightness reports (`[0, 0, x]`), as a hidraw node without write permission would.
    struct NoBrightness(Vec<[u8; 3]>);

    impl HidWriter for NoBrightness {
        fn write_report(&mut self, report: &[u8; 3]) -> io::Result<bool> {
            if report[1] == 0 {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            self.0.push(*report);
            Ok(true)
        }
    }

    /// Brightness and gear are attempted independently, so a failed brightness write
    /// or an unreadable brightness setting still sends the gear.
    #[test]
    fn gear_is_sent_whatever_the_brightness() {
        let mut w = NoBrightness(Vec::new());
        let errors = update(&mut w, Some(100), 2.0, 0.0);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(w.0, vec![[0, 2, 109]]);
        let mut r = Recorder(Vec::new());
        assert!(update(&mut r, None, 0.0, 0.0).is_empty());
        assert_eq!(r.0, vec![[0, 2, 42]]);
    }
}
