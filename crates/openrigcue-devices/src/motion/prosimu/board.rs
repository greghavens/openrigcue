//! The PROSIMU PRS200 / PRS50 actuator boards.
//!
//! Frames are `AA <cmd> <8 bytes>`,
//! followed by the big-endian CRC-16/XMODEM of everything before it and `55` when the firmware version is 3.

use std::io::{Read, Write};

use super::crc16::crc16_xmodem;
use crate::motion::MotionError;
use crate::serial::{Link, LinkError};
use openrigcue_model::native_value::LegacyValue;

/// The baud rate of a board's port (8 data bits, no parity, 2 stop bits).
pub const BAUD: u32 = 19_200;
/// Command byte of the speed/acceleration frame.
const CMD_SPEED_AND_ACC: u8 = 1;
/// Command byte of the position frame.
const CMD_POSITION: u8 = 2;
const HEADER: u8 = 0xAA;
const TRAILER: u8 = 0x55;
/// Firmware version whose frames carry the CRC (`[PROSIMU] prs200_firmware`, default `'3'`).
pub const CRC_FIRMWARE: i64 = 3;
/// First parameter byte of the speed/acceleration frame for non-SV models.
const SPEED_FRAME_MODE: u8 = 4;

/// A frame: `AA` + payload, then the CRC (big-endian) for firmware 3, then `55`.
pub fn pack(firmware: i64, payload: &[u8]) -> Vec<u8> {
    pack_with(firmware == CRC_FIRMWARE, payload)
}

/// [`pack`] for a board whose firmware test gave `crc`.
pub fn pack_with(crc: bool, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.push(HEADER);
    frame.extend_from_slice(payload);
    if crc {
        let crc = crc16_xmodem(&frame);
        frame.extend_from_slice(&crc.to_be_bytes());
    }
    frame.push(TRAILER);
    frame
}

/// The position frame for four channels. Values are wrapped to 16 bits and written big-endian.
pub fn position_frame(firmware: i64, positions: [i64; 4]) -> Vec<u8> {
    position_frame_words(firmware == CRC_FIRMWARE, positions.map(|p| p as u16))
}

/// The position frame of words already reduced to 16 bits.
pub fn position_frame_words(crc: bool, words: [u16; 4]) -> Vec<u8> {
    let mut payload = vec![CMD_POSITION];
    for p in words {
        payload.extend_from_slice(&p.to_be_bytes());
    }
    pack_with(crc, &payload)
}

/// How the eight speed/acceleration parameter bytes are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedLayout {
    /// `[4, speed1, acc1, 0, 0, 0, 0, 0]`.
    Mode4,
    /// `[acc1, speed1, acc2, speed2, acc3, speed3, acc4, speed4]` (SV models).
    AccelSpeedPairs,
}

/// The speed/acceleration frame; the same pair is used for all four channels.
///
/// A value outside 0..=255 is an error.
pub fn speed_frame(firmware: i64, layout: SpeedLayout, speed: i64, accel: i64) -> Result<Vec<u8>, MotionError> {
    speed_frame_with(firmware == CRC_FIRMWARE, layout, speed, accel)
}

fn speed_frame_with(crc: bool, layout: SpeedLayout, speed: i64, accel: i64) -> Result<Vec<u8>, MotionError> {
    let byte =
        |v: i64| u8::try_from(v).map_err(|_| MotionError::Native("ValueError: bytes must be in range(0, 256)".into()));
    let (s, a) = (byte(speed)?, byte(accel)?);
    let params = match layout {
        SpeedLayout::Mode4 => [SPEED_FRAME_MODE, s, a, 0, 0, 0, 0, 0],
        SpeedLayout::AccelSpeedPairs => [a, s, a, s, a, s, a, s],
    };
    let mut payload = vec![CMD_SPEED_AND_ACC];
    payload.extend_from_slice(&params);
    Ok(pack_with(crc, &payload))
}

/// One open PRS200/PRS50 board.
pub struct Prs200Board {
    link: Box<dyn Link>,
    /// Whether the firmware version is 3.
    crc: bool,
}

impl Prs200Board {
    pub fn new(link: Box<dyn Link>, firmware: i64) -> Self {
        Prs200Board { link, crc: firmware == CRC_FIRMWARE }
    }

    /// A board whose firmware `version` is the integer of `PROSIMU.prs200_firmware`, of any width.
    pub fn new_native(link: Box<dyn Link>, version: &num_bigint::BigInt) -> Self {
        Prs200Board { link, crc: *version == num_bigint::BigInt::from(CRC_FIRMWARE) }
    }

    pub fn port(&self) -> &str {
        self.link.name()
    }

    pub fn send_position(&mut self, positions: [i64; 4]) -> Result<(), LinkError> {
        self.link.write_all(&position_frame_words(self.crc, positions.map(|p| p as u16)))
    }

    /// [`Self::send_position`] on dynamic values: each reduced to 16 bits before the write.
    pub fn send_position_native(&mut self, positions: &[LegacyValue]) -> Result<(), MotionError> {
        let mut words = [0u16; 4];
        for (word, value) in words.iter_mut().zip(positions) {
            *word = crate::motion::native::c_uint16(value)?;
        }
        self.link.write_all(&position_frame_words(self.crc, words))?;
        Ok(())
    }

    pub fn send_speed(&mut self, layout: SpeedLayout, speed: i64, accel: i64) -> Result<(), MotionError> {
        let frame = speed_frame_with(self.crc, layout, speed, accel)?;
        self.link.write_all(&frame)?;
        Ok(())
    }
}

/// Opens PRS200 board ports (19200 8N2), which the shared host cannot open (it only opens 8N1).
pub trait BoardPorts: Send {
    fn open(&mut self, path: &str) -> Result<Box<dyn Link>, LinkError>;
}

/// Opens real serial ports 19200 8N2, blocking writes.
#[derive(Debug, Default)]
pub struct SystemBoardPorts;

impl BoardPorts for SystemBoardPorts {
    fn open(&mut self, path: &str) -> Result<Box<dyn Link>, LinkError> {
        let result =
            serialport::new(path, BAUD).stop_bits(serialport::StopBits::Two).timeout(std::time::Duration::MAX).open();
        #[cfg(target_os = "linux")]
        crate::serial::record_open_result(path, result.as_ref().err());
        let port = result.map_err(|source| LinkError::Open { path: path.to_string(), source })?;
        Ok(Box::new(BoardLink { port, name: path.to_string() }))
    }
}

/// A serial port opened 8N2.
struct BoardLink {
    port: Box<dyn serialport::SerialPort>,
    name: String,
}

impl Link for BoardLink {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        self.port.write_all(bytes)?;
        Ok(())
    }

    fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
        let waiting = usize::try_from(self.port.bytes_to_read()?).unwrap_or(usize::MAX);
        let mut buf = vec![0; waiting];
        if waiting > 0 {
            let read = self.port.read(&mut buf)?;
            buf.truncate(read);
        }
        Ok(buf)
    }

    fn clear_input(&mut self) -> Result<(), LinkError> {
        self.port.clear(serialport::ClearBuffer::Input)?;
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_frame_layout_firmware_3() {
        // AA 02 P1hi P1lo … P4hi P4lo CRChi CRClo 55, CRC-16/XMODEM over AA..P4lo.
        let f = position_frame(3, [0x7FFF, 0x8000, 0x1234, 0]);
        assert_eq!(f, [0xAA, 0x02, 0x7F, 0xFF, 0x80, 0x00, 0x12, 0x34, 0x00, 0x00, 0x78, 0xAA, 0x55]);
    }

    #[test]
    fn other_firmware_omits_the_crc_only() {
        let f = position_frame(2, [1, 2, 3, 4]);
        assert_eq!(f, [0xAA, 0x02, 0, 1, 0, 2, 0, 3, 0, 4, 0x55]);
    }

    #[test]
    fn positions_wrap_like_c_uint16() {
        // 65536 wraps to 0, -1 to 65535.
        let f = position_frame(2, [65536, -1, 65535, 3276]);
        assert_eq!(&f[2..10], &[0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0x0C, 0xCC]);
    }

    #[test]
    fn speed_frame_layouts() {
        // Non-SV: [4, speed1, acc1, 0…]; the first reaction-speed send is (4, 200).
        let f = speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap();
        assert_eq!(f, [0xAA, 0x01, 4, 4, 200, 0, 0, 0, 0, 0, 0xDF, 0x02, 0x55]);
        // SV: [acc1, speed1, acc2, speed2, …].
        let f = speed_frame(2, SpeedLayout::AccelSpeedPairs, 6, 199).unwrap();
        assert_eq!(&f[2..], &[199, 6, 199, 6, 199, 6, 199, 6, 0x55]);
    }

    #[test]
    fn speed_frame_rejects_non_byte_values() {
        // 256 does not fit a byte.
        assert!(speed_frame(3, SpeedLayout::Mode4, 256, 0).is_err());
        assert!(speed_frame(3, SpeedLayout::Mode4, 0, -1).is_err());
    }
}
