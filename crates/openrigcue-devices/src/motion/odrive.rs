//! ODrive "CAN Simple" commands sent as 12-byte frames over a serial CAN adapter.
//!
//! The byte helpers and the frame builders, including the big-endian
//! `int32` of [`set_controller_modes`]. There is no motion model and no Hardware-page control for
//! ODrive, so no [`super::MotionDriver`] is built on these: how motion slots would map to
//! ODrive positions is not decided.

mod runtime;
pub use runtime::{DebugSink, Error as ODriveError, Lifecycle, LoopStep, ODrive, ReceiverAttribute, StdoutDebug};

/// Four little-endian bytes read as a 32-bit float.
pub fn bytes_to_ieee754_float(bytes: [u8; 4]) -> f64 {
    f64::from(f32::from_le_bytes(bytes))
}
/// The value wrapped to 16 bits, as two little-endian bytes.
pub fn uint16_bytes(value: i64) -> [u8; 2] {
    (value as u16).to_le_bytes()
}
/// The value as an unsigned big-endian 32-bit integer; out of range is an error.
pub fn uint32_be_bytes(value: i64) -> Result<[u8; 4], FieldOverflow> {
    u32::try_from(value).map(u32::to_be_bytes).map_err(|_| FieldOverflow::Uint32)
}
/// Returns only [value[1],value[0]].
pub fn swap<T: Clone>(value: &[T]) -> Result<[T; 2], ODriveError> {
    Ok([value.get(1).ok_or(ODriveError::Index)?.clone(), value.first().ok_or(ODriveError::Index)?.clone()])
}

/// CAN Simple command ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    SetControllerModes = 0x0B,
    SetInputPos = 0x0C,
    SetInputVel = 0x0D,
    SetTrajAccelLimits = 0x12,
    Reboot = 0x16,
    GetBusVoltageAndCurrent = 0x17,
}

/// Range failures of the two 32-bit integer formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FieldOverflow {
    #[error("'i' format requires -2147483648 <= number <= 2147483647")]
    Int32,
    #[error("'I' format requires 0 <= number <= 4294967295")]
    Uint32,
}

/// The value cast to a 32-bit float (a finite value too large becomes infinity), little-endian.
/// The Result signature is retained for the existing encoder API.
pub fn float_bytes(value: f64) -> Result<[u8; 4], FieldOverflow> {
    Ok((value as f32).to_le_bytes())
}

/// Signed 16-bit: little-endian, wrapped to 16 bits.
pub fn int16_bytes(v: i64) -> [u8; 2] {
    (v as i16).to_le_bytes()
}

/// Big-endian signed 32-bit; out of range is an error.
pub fn int32_be_bytes(v: i64) -> Result<[u8; 4], FieldOverflow> {
    i32::try_from(v).map(i32::to_be_bytes).map_err(|_| FieldOverflow::Int32)
}

/// `(node << 5) | cmd`, the 11-bit CAN id.
pub fn message_id(node: u8, command: CommandId) -> u16 {
    (u16::from(node) << 5) | command as u16
}

/// `IDhi IDlo D0..D7 0D 0A`.
pub fn frame(node: u8, command: CommandId, data: [u8; 8]) -> [u8; 12] {
    let id = message_id(node, command);
    let mut out = [0u8; 12];
    out[0] = (id >> 8) as u8;
    out[1] = (id & 0xFF) as u8;
    out[2..10].copy_from_slice(&data);
    out[10] = 0x0D;
    out[11] = 0x0A;
    out
}

fn join(a: [u8; 4], b: [u8; 4]) -> [u8; 8] {
    let mut d = [0u8; 8];
    d[..4].copy_from_slice(&a);
    d[4..].copy_from_slice(&b);
    d
}

/// Input position: float position, int16 velocity and torque feed-forward
/// (unscaled).
pub fn set_input_pos(node: u8, position: f64, velocity: i64, torque: i64) -> Result<[u8; 12], FieldOverflow> {
    let p = float_bytes(position)?;
    let v = int16_bytes(velocity);
    let t = int16_bytes(torque);
    Ok(frame(node, CommandId::SetInputPos, [p[0], p[1], p[2], p[3], v[0], v[1], t[0], t[1]]))
}

/// Input velocity and torque: two floats.
pub fn set_input_vel(node: u8, velocity: f64, torque: f64) -> Result<[u8; 12], FieldOverflow> {
    Ok(frame(node, CommandId::SetInputVel, join(float_bytes(velocity)?, float_bytes(torque)?)))
}

/// Control mode and input mode: two big-endian int32.
pub fn set_controller_modes(node: u8, control_mode: i64, input_mode: i64) -> Result<[u8; 12], FieldOverflow> {
    Ok(frame(node, CommandId::SetControllerModes, join(int32_be_bytes(control_mode)?, int32_be_bytes(input_mode)?)))
}

/// Trajectory acceleration and deceleration limits: two floats.
pub fn set_traj_accel_limits(node: u8, accel: f64, decel: f64) -> Result<[u8; 12], FieldOverflow> {
    Ok(frame(node, CommandId::SetTrajAccelLimits, join(float_bytes(accel)?, float_bytes(decel)?)))
}

/// A command without payload (reboot, bus voltage and current request).
pub fn bare(node: u8, command: CommandId) -> [u8; 12] {
    frame(node, command, [0; 8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_is_node_shifted_by_five() {
        // The message id: (node id << 5) | command id.
        assert_eq!(message_id(3, CommandId::SetInputPos), 0x6C);
        assert_eq!(message_id(63, CommandId::GetBusVoltageAndCurrent), 0x7F7);
    }

    #[test]
    fn pack_message_layout() {
        // The frame: [(id >> 8) & 0xFF, id & 0xFF, b0..b7, 13, 10].
        let f = bare(63, CommandId::Reboot);
        assert_eq!(f, [0x07, 0xF6, 0, 0, 0, 0, 0, 0, 0, 0, 0x0D, 0x0A]);
    }

    #[test]
    fn set_input_pos_encodes_float_and_int16() {
        // The float 1.0 is 00 00 80 3F; the int16 -1 is FF FF; 65536 wraps to 0.
        let f = set_input_pos(1, 1.0, -1, 65536).unwrap();
        assert_eq!(&f[2..10], &[0x00, 0x00, 0x80, 0x3F, 0xFF, 0xFF, 0x00, 0x00]);
        assert_eq!(&set_input_pos(1, 1e300, 0, 0).unwrap()[2..6], &f32::INFINITY.to_le_bytes());
    }

    #[test]
    fn controller_modes_are_big_endian_int32() {
        // The int32 values are big-endian.
        let f = set_controller_modes(0, 3, 5).unwrap();
        assert_eq!(&f[2..10], &[0, 0, 0, 3, 0, 0, 0, 5]);
        assert!(set_controller_modes(0, 1 << 40, 0).is_err());
    }

    #[test]
    fn float_pairs() {
        let f = set_traj_accel_limits(2, 0.5, -2.0).unwrap();
        assert_eq!(&f[2..10], &[0, 0, 0, 0x3F, 0, 0, 0, 0xC0]);
        let f = set_input_vel(2, 0.0, 1.0).unwrap();
        assert_eq!(f[1], 0x4D);
    }
}
