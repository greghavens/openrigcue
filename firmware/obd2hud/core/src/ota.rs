//! BLE firmware update: the write handler of the `173c2db0-…` characteristic.
//!
//! The first write begins an update into the next OTA partition; the update flag
//! it sets is never cleared. Every non-empty value is written to the update. A
//! value that is not a full 512-byte chunk ends the update and sets the boot
//! partition; on success the device restarts after two seconds, otherwise an
//! error line is printed. Every write that does not restart is acknowledged with a
//! notification of the bytes 1 to 5.
//! No result is checked except that of setting the boot partition. The host
//! writes the image in 512-byte chunks with response and waits for the
//! notification after each one.

/// The chunk size that keeps the update open.
pub const CHUNK: usize = 0x200;
/// The delay before the restart.
pub const RESTART_DELAY_MS: u32 = 2000;
/// The lines printed on the serial link; each is followed by `\r\n`.
pub const BEGIN_TEXT: &[u8] = b"BeginOTA";
pub const END_TEXT: &[u8] = b"EndOTA";
pub const ERROR_TEXT: &[u8] = b"Upload Error";
/// The update size passed when the image size is not known.
pub const OTA_SIZE_UNKNOWN: u32 = 0xffff_ffff;

/// The calls the write handler makes, in the order it makes them.
pub trait OtaHost {
    /// Prints a line on UART0.
    fn println(&mut self, text: &[u8]);
    /// Begins an update of unknown size into the next OTA partition.
    fn begin(&mut self);
    /// Writes data to the update.
    fn write(&mut self, data: &[u8]);
    /// Ends the update.
    fn end(&mut self);
    /// Sets the next OTA partition as the boot partition; true on success.
    fn set_boot_partition(&mut self) -> bool;
    /// Waits, then restarts (does not return on the target).
    fn restart(&mut self, delay_ms: u32);
    /// Sets the characteristic's value and notifies it.
    fn ack(&mut self, value: &[u8]);
}

/// The update flag (the update handle lives in the host).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Ota {
    pub update_flag: bool,
}

impl Ota {
    /// Handles one write with the characteristic's new value.
    pub fn on_write<H: OtaHost>(&mut self, value: &[u8], h: &mut H) {
        if !self.update_flag {
            h.println(BEGIN_TEXT);
            h.begin();
            self.update_flag = true;
        }
        if !value.is_empty() {
            h.write(value);
            if value.len() != CHUNK {
                h.end();
                h.println(END_TEXT);
                if h.set_boot_partition() {
                    h.restart(RESTART_DELAY_MS);
                    return;
                }
                h.println(ERROR_TEXT);
            }
        }
        h.ack(&crate::ble::OTA_ACK_VALUE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    #[derive(Default)]
    struct Rec {
        log: Vec<String>,
        boot_ok: bool,
    }

    impl OtaHost for Rec {
        fn println(&mut self, t: &[u8]) {
            self.log.push(format!("println {}", core::str::from_utf8(t).unwrap()));
        }
        fn begin(&mut self) {
            self.log.push("begin".into());
        }
        fn write(&mut self, d: &[u8]) {
            self.log.push(format!("write {}", d.len()));
        }
        fn end(&mut self) {
            self.log.push("end".into());
        }
        fn set_boot_partition(&mut self) -> bool {
            self.log.push("set_boot".into());
            self.boot_ok
        }
        fn restart(&mut self, ms: u32) {
            self.log.push(format!("restart {ms}"));
        }
        fn ack(&mut self, v: &[u8]) {
            self.log.push(format!("ack {v:?}"));
        }
    }

    #[test]
    fn full_update_sequence() {
        let mut o = Ota::default();
        let mut h = Rec { boot_ok: true, ..Default::default() };
        o.on_write(&[0xe9; 512], &mut h);
        o.on_write(&[0; 512], &mut h);
        o.on_write(&[0; 100], &mut h);
        assert_eq!(
            h.log,
            [
                "println BeginOTA",
                "begin",
                "write 512",
                "ack [1, 2, 3, 4, 5]",
                "write 512",
                "ack [1, 2, 3, 4, 5]",
                "write 100",
                "end",
                "println EndOTA",
                "set_boot",
                "restart 2000",
            ]
        );
    }

    #[test]
    fn invalid_image_reports_and_keeps_the_flag() {
        let mut o = Ota::default();
        let mut h = Rec::default();
        o.on_write(&[1; 10], &mut h);
        assert_eq!(
            h.log,
            [
                "println BeginOTA",
                "begin",
                "write 10",
                "end",
                "println EndOTA",
                "set_boot",
                "println Upload Error",
                "ack [1, 2, 3, 4, 5]"
            ]
        );
        assert!(o.update_flag);
        h.log.clear();
        // The flag stays set: later writes go to the ended handle without a new begin.
        o.on_write(&[1; 512], &mut h);
        assert_eq!(h.log, ["write 512", "ack [1, 2, 3, 4, 5]"]);
    }

    #[test]
    fn empty_write_only_begins_and_acks() {
        let mut o = Ota::default();
        let mut h = Rec::default();
        o.on_write(&[], &mut h);
        assert_eq!(h.log, ["println BeginOTA", "begin", "ack [1, 2, 3, 4, 5]"]);
    }
}
