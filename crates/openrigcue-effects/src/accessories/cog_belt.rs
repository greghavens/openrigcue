//! CogBelt: an eight-channel actuator controller on a 250000-baud serial line (development driver;
//! no other module calls it).

use std::io;
use std::time::Duration;

use super::link::{Clock, PortOpener, PortSettings, SerialLink};
use super::numeric::{map_clamped, truncate};

/// Baud rate of the controller.
pub const BAUD_RATE: u32 = 250_000;

/// Field names of the `CMD55` configuration answer, in order.
pub const CONFIG_KEYS: [&str; 42] = [
    "data",
    "firmware",
    "T4U",
    "motor nb",
    "park pos",
    "park move speed",
    "park move timeout",
    "standby start pos",
    "start move step",
    "start move timeout",
    "disable park type",
    "0",
    "klm mode",
    "spike f range",
    "spike f mult",
    "spike f en",
    "platform check",
    "estop mode",
    "spike move speed",
    "spike finemove speed",
    "spike f mult tl",
    "spike f en tl",
    "pulse freq",
    "button speed",
    "force offline mode",
    "filter factor",
    "stroke",
    "stroke 2",
    "stroke 3",
    "stroke 4",
    "lead screw",
    "lead screw 2",
    "lead screw 3",
    "lead screw 4",
    "inline",
    "inline 2",
    "inline 3",
    "inline 4",
    "ratio",
    "ratio 2",
    "ratio 3",
    "ratio 4",
];

/// Pause between a query and reading its answer.
const ANSWER_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum CogBeltError {
    #[error("serial: {0}")]
    Io(#[from] io::Error),
    #[error("answer is not UTF-8")]
    NotText,
    #[error("configuration answer has {0} fields, more than the known keys")]
    TooManyFields(usize),
}

/// An open controller.
pub struct CogBelt {
    link: Box<dyn SerialLink>,
}

/// One position frame: `FF FF`, eight big-endian 16-bit channels, `0A 0D`.
pub fn position_frame(channels: &[u16; 8]) -> Vec<u8> {
    let mut frame = vec![0xFF, 0xFF];
    for value in channels {
        frame.extend_from_slice(&value.to_be_bytes());
    }
    frame.extend_from_slice(&[10, 13]);
    frame
}

impl CogBelt {
    /// Open the controller (250000 8N1, blocking reads).
    pub fn open(opener: &dyn PortOpener, port: &str) -> io::Result<Self> {
        let link = opener.open(port, PortSettings { baud_rate: BAUD_RATE, read_timeout: None })?;
        Ok(CogBelt { link })
    }

    /// Send a text query and return the first answer line: write the command, wait
    /// 0.1 s, read what is waiting and split on CRLF.
    fn query(&mut self, command: &str, clock: &dyn Clock) -> Result<String, CogBeltError> {
        self.link.write(command.as_bytes())?;
        clock.sleep(ANSWER_DELAY);
        let answer = self.link.read_available()?;
        let text = String::from_utf8(answer).map_err(|_| CogBeltError::NotText)?;
        Ok(text.split("\r\n").next().unwrap_or_default().to_string())
    }

    /// Firmware query: `RQM`.
    pub fn firmware(&mut self, clock: &dyn Clock) -> Result<String, CogBeltError> {
        self.query("RQM", clock)
    }

    /// Actuator query: `CMD44`.
    pub fn actuator(&mut self, clock: &dyn Clock) -> Result<String, CogBeltError> {
        self.query("CMD44", clock)
    }

    /// Configuration query: `CMD55`, a `:`-separated list whose last element is dropped, paired with the keys.
    pub fn config(&mut self, clock: &dyn Clock) -> Result<Vec<(&'static str, String)>, CogBeltError> {
        let line = self.query("CMD55", clock)?;
        let mut values: Vec<&str> = line.split(':').collect();
        values.pop();
        if values.len() > CONFIG_KEYS.len() {
            return Err(CogBeltError::TooManyFields(values.len()));
        }
        Ok(CONFIG_KEYS.iter().copied().zip(values.into_iter().map(str::to_string)).collect())
    }

    /// Sends one position frame: true when the whole frame was written.
    pub fn send_position(&mut self, channels: &[u16; 8]) -> io::Result<bool> {
        let frame = position_frame(channels);
        Ok(self.link.write(&frame)? == frame.len())
    }

    /// Test output: channel 0 follows the belt test slider, 0..100 -> 3276..62260.
    pub fn send_test(&mut self, mut channels: [u16; 8], test_value: f64) -> io::Result<bool> {
        let mapped = truncate(map_clamped(test_value, 0.0, 100.0, 3276.0, 62260.0)).unwrap_or(0);
        channels[0] = mapped as u16;
        self.send_position(&channels)
    }
}

#[cfg(test)]
mod tests {
    use super::super::link::fakes::{FakeLink, FakeOpener, ManualClock};
    use super::*;

    /// Position frame layout (terminator LF CR).
    #[test]
    fn position_frame_layout() {
        let frame = position_frame(&[0x1234, 0, 0, 0, 0, 0, 0, 0xFFFF]);
        assert_eq!(frame.len(), 20);
        assert_eq!(&frame[..4], &[0xFF, 0xFF, 0x12, 0x34]);
        assert_eq!(&frame[16..], &[0xFF, 0xFF, 0x0A, 0x0D]);
    }

    /// Test-output mapping and configuration answer parsing.
    #[test]
    fn clamped_auxiliary_maps_bound_cogbelt_test_positions_before_packing() {
        let link = FakeLink::new();
        let opener = FakeOpener::with("/dev/ttyACM3", &link);
        let mut belt = CogBelt::open(&opener, "/dev/ttyACM3").unwrap();
        for (value, expected) in [(-10.0, 3276u16), (110.0, 62260), (f64::NAN, 62260)] {
            belt.send_test([0; 8], value).unwrap();
            assert_eq!(&link.take_written()[2..4], &expected.to_be_bytes());
        }
    }

    #[test]
    fn test_mapping_and_config() {
        let link = FakeLink::new();
        let opener = FakeOpener::with("/dev/ttyACM3", &link);
        let mut belt = CogBelt::open(&opener, "/dev/ttyACM3").unwrap();
        assert_eq!(opener.opened()[0].1.baud_rate, 250_000);
        belt.send_test([0; 8], 50.0).unwrap();
        // 50 of 0..100 maps to 32768 in 3276..62260 (truncated)
        assert_eq!(&link.take_written()[2..4], &[0x80, 0x00]);
        link.queue_reply(b"D:1.2:5:\r\nnext");
        let clock = ManualClock::default();
        let cfg = belt.config(&clock).unwrap();
        assert_eq!(cfg, vec![("data", "D".to_string()), ("firmware", "1.2".to_string()), ("T4U", "5".to_string())]);
        assert_eq!(link.take_written(), b"CMD55".to_vec());
    }
}
