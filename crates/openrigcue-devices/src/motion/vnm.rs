//! VNM Simulation actuator controllers ("N Actuators <…>" models).
//!
//! One serial port at 57600 8N1, no handshake, and a 22-byte frame
//! `FF FF <9 × big-endian u16> 0A 0D` per motion tick. Slots are reordered from the cueing order to the VNM
//! port order of the model, and ports 5–9 can be reversed (`65536 − v`).

use tracing::debug;

use super::driver_support::{boolean_setting_error, CornerTravel};
use super::native::{self, NativeSetting};
use super::pt_actuator::MotionWarning;
use super::{
    ActuatorPositions, DriverFactory, LinkStatus, LiveSettings, MotionContext, MotionDriver, MotionError, PyFrame,
};
use crate::serial::{Link, OpenOptions};
use openrigcue_model::native_value::LegacyValue;

/// The largest motor value of every VNM model.
pub const FULL_SCALE: i64 = 65536;
/// Baud rate of the port.
pub const BAUD: u32 = 57600;
/// Slots a frame reads: the port mapping reorders 12 slots, the frame carries the first nine.
pub const MAPPED_SLOTS: usize = 12;
/// Ports whose values the "Reverse" switches of the Controller Settings dialog invert (ports 5..=9).
const FIRST_REVERSIBLE_PORT: usize = 5;

/// A physical VNM actuator position, as named in the port tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    FrontLeft,
    FrontRight,
    RearRight,
    RearLeft,
    Front,
    Rear,
    TractionLoss,
    TractionLossFront,
    TractionLossRear,
    Surge,
    Belt,
}

/// The eleven VNM SIMULATION models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VnmModel {
    Front2Rear1,
    Front1Rear2,
    Corners4,
    Corners4Tl,
    Corners4Surge,
    Corners4TlSurge,
    Corners4SurgeBelt,
    Corners4TwoTl,
    Corners4TlSurgeBelt,
    Corners4TwoTlSurge,
    Corners4TwoTlSurgeBelt,
}

use Port::*;

const CORNER_SLOTS: [Port; 4] = [RearLeft, FrontLeft, FrontRight, RearRight];
const CORNER_PORTS: [Port; 4] = [FrontLeft, FrontRight, RearRight, RearLeft];

impl VnmModel {
    pub const ALL: [VnmModel; 11] = [
        VnmModel::Front2Rear1,
        VnmModel::Front1Rear2,
        VnmModel::Corners4,
        VnmModel::Corners4Tl,
        VnmModel::Corners4Surge,
        VnmModel::Corners4TlSurge,
        VnmModel::Corners4SurgeBelt,
        VnmModel::Corners4TwoTl,
        VnmModel::Corners4TlSurgeBelt,
        VnmModel::Corners4TwoTlSurge,
        VnmModel::Corners4TwoTlSurgeBelt,
    ];

    /// The model name used in the configuration (`MOTIONBOX.model`).
    pub fn name(self) -> &'static str {
        match self {
            VnmModel::Front2Rear1 => "3 Actuators <2 Front + 1 Rear>",
            VnmModel::Front1Rear2 => "3 Actuators <1 Front + 2 Rear>",
            VnmModel::Corners4 => "4 Actuators <2 Front + 2 Rear>",
            VnmModel::Corners4Tl => "5 Actuators <2 Front + 2 Rear + TL>",
            VnmModel::Corners4Surge => "5 Actuators <2 Front + 2 Rear + Surge>",
            VnmModel::Corners4TlSurge => "6 Actuators <2 Front + 2 Rear + TL + Surge>",
            VnmModel::Corners4SurgeBelt => "6 Actuators <2 Front + 2 Rear + Surge + Belt>",
            VnmModel::Corners4TwoTl => "6 Actuators <2 Front + 2 Rear + 2 TL>",
            VnmModel::Corners4TlSurgeBelt => "7 Actuators <2 Front + 2 Rear + TL + Surge + Belt>",
            VnmModel::Corners4TwoTlSurge => "7 Actuators <2 Front + 2 Rear + 2 TL + Surge>",
            VnmModel::Corners4TwoTlSurgeBelt => "8 Actuators <2 Front + 2 Rear + 2 TL + Surge + Belt>",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }

    /// The number of motors of the model.
    pub fn actuators(self) -> usize {
        self.port_order().len()
    }

    /// The actuator held by each cueing slot (the source side of the port mapping).
    fn slot_order(self) -> Vec<Port> {
        let tail: &[Port] = match self {
            VnmModel::Front2Rear1 => return vec![Rear, FrontLeft, FrontRight],
            VnmModel::Front1Rear2 => return vec![Front, RearRight, RearLeft],
            VnmModel::Corners4 => &[],
            VnmModel::Corners4Tl => &[TractionLoss],
            VnmModel::Corners4Surge => &[Surge],
            VnmModel::Corners4TlSurge => &[TractionLoss, Surge],
            VnmModel::Corners4SurgeBelt => &[Surge, Belt],
            VnmModel::Corners4TwoTl => &[TractionLossRear, TractionLossFront],
            VnmModel::Corners4TlSurgeBelt => &[TractionLoss, Surge, Belt],
            VnmModel::Corners4TwoTlSurge => &[TractionLossRear, Surge, TractionLossFront],
            VnmModel::Corners4TwoTlSurgeBelt => &[TractionLossRear, Surge, TractionLossFront, Belt],
        };
        CORNER_SLOTS.iter().chain(tail).copied().collect()
    }

    /// The actuator wired to each VNM port 1..n (the target side of the port mapping).
    pub fn port_order(self) -> Vec<Port> {
        let tail: &[Port] = match self {
            VnmModel::Front2Rear1 => return vec![FrontLeft, FrontRight, Rear],
            VnmModel::Front1Rear2 => return vec![Front, RearRight, RearLeft],
            VnmModel::Corners4 => &[],
            VnmModel::Corners4Tl => &[TractionLoss],
            VnmModel::Corners4Surge => &[Surge],
            VnmModel::Corners4TlSurge => &[TractionLoss, Surge],
            VnmModel::Corners4SurgeBelt => &[Surge, Belt],
            VnmModel::Corners4TwoTl => &[TractionLossFront, TractionLossRear],
            VnmModel::Corners4TlSurgeBelt => &[TractionLoss, Surge, Belt],
            VnmModel::Corners4TwoTlSurge => &[TractionLossFront, TractionLossRear, Surge],
            VnmModel::Corners4TwoTlSurgeBelt => &[TractionLossFront, TractionLossRear, Surge, Belt],
        };
        CORNER_PORTS.iter().chain(tail).copied().collect()
    }

    /// For each of the 12 frame positions, the cueing slot it is taken from.
    ///
    /// Position `p` takes the slot that holds the actuator of port `p`, or `p` itself past the last port.
    pub fn frame_sources(self) -> [usize; MAPPED_SLOTS] {
        let slots = self.slot_order();
        let ports = self.port_order();
        std::array::from_fn(|p| ports.get(p).and_then(|port| slots.iter().position(|s| s == port)).unwrap_or(p))
    }

    /// Indices into the reordered frame for front-left, front-right, rear-left and rear-right,
    /// for the actuator positions.
    fn corner_axes(self) -> [usize; 4] {
        match self {
            VnmModel::Front2Rear1 => [0, 1, 2, 2],
            VnmModel::Front1Rear2 => [0, 0, 2, 1],
            _ => [0, 1, 3, 2],
        }
    }
}

/// Encoding of the VNM frame.
pub mod frame {
    use crate::motion::{native, MotionError};
    use openrigcue_model::native_value::LegacyValue;

    /// Length of one frame.
    pub const LEN: usize = 22;

    /// An axis value outside `0..=65535`, which is a value error ("Input must be a 16-bit integer (0 to
    /// 65535)"). The fields are the axis position in the frame (0-based) and its value.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
    #[error("Input must be a 16-bit integer (0 to 65535)")]
    pub struct OutOfRange(pub usize, pub i64);

    /// `FF FF`, nine big-endian 16-bit axes, `0A 0D`. Values outside 0..=65535 are rejected.
    pub fn encode(axes: &[i64; 9]) -> Result<[u8; LEN], OutOfRange> {
        let mut out = [0u8; LEN];
        out[0] = 0xFF;
        out[1] = 0xFF;
        for (i, &v) in axes.iter().enumerate() {
            let word = u16::try_from(v).map_err(|_| OutOfRange(i, v))?;
            out[2 + 2 * i..4 + 2 * i].copy_from_slice(&word.to_be_bytes());
        }
        out[20] = 0x0A;
        out[21] = 0x0D;
        Ok(out)
    }

    /// Why the frame could not be built for an axis (its 0-based position in the frame).
    #[derive(Debug)]
    pub enum NativeAxisError {
        /// The value error of a value outside `0..=65535`.
        OutOfRange(usize),
        /// Another error, at line `.1` of the byte conversion's frame.
        Raised(usize, u32, MotionError),
    }

    /// `FF FF`, nine big-endian 16-bit axes, `0A 0D`, on dynamic values: each is compared with
    /// `0..=65535` at any width, then its high and low byte are taken with integer operators.
    pub fn encode_native(axes: &[LegacyValue]) -> Result<[u8; LEN], NativeAxisError> {
        let mut out = [0u8; LEN];
        out[0] = 0xFF;
        out[1] = 0xFF;
        for (i, value) in axes.iter().enumerate() {
            let raised = |error| NativeAxisError::Raised(i, 27, error);
            let low = native::le(&native::int(0), value).map_err(raised)?;
            let inside = low && native::le(value, &native::int(65_535)).map_err(raised)?;
            if !inside {
                return Err(NativeAxisError::OutOfRange(i));
            }
            // The high byte, then the low byte: integer operators (a float in range is an error there).
            let high =
                native::shift_right_and(value, 8, 0xFF).map_err(|error| NativeAxisError::Raised(i, 31, error))?;
            let low = native::shift_right_and(value, 0, 0xFF).map_err(|error| NativeAxisError::Raised(i, 32, error))?;
            out[2 + 2 * i] = high;
            out[3 + 2 * i] = low;
        }
        out[20] = 0x0A;
        out[21] = 0x0D;
        Ok(out)
    }
}

/// The file name in the frames of this driver's errors.
const VNM_FILE: &str = "backend/controllers/vnm.py";
/// The frame line of the reverse switch of port 5; the four later ports
/// follow three lines apart.
const REVERSE_FIRST_LINE: u32 = 132;
/// The frame line of the reordering of the slots by the port mapping.
const MAPPING_LINE: u32 = 150;
/// The frame line where the frame is built, and the line of the first axis's byte conversion inside it.
const GEN_COMMAND_LINE: u32 = 156;
const GEN_COMMAND_FIRST_AXIS_LINE: u32 = 39;

/// A frame of the send pass at `line`.
const fn send_vnm_frame(line: u32) -> PyFrame {
    PyFrame::new(VNM_FILE, "send_vnm", line)
}

/// The `[VNM SIMULATION]` and related settings, re-read on every call.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VnmSettings {
    /// Where the settings are read live (the refreshed raw values are the fallback).
    pub runtime: super::driver_support::RuntimeReader,
    /// `vnm_com_port`; empty means "not configured".
    pub port: String,
    /// `reverse_controller_port_5` … `_9`.
    pub reverse_ports: [bool; 5],
    pub native_reverse_ports: Option<[Result<bool, String>; 5]>,
    /// `MOTION_COMPENSATION.vertical_actuators_stroke` (mm).
    pub vertical_stroke_mm: i64,
    /// The stroke as a raw value, converted to an integer after frame encoding and before the corners are published.
    pub native_vertical_stroke_mm: Option<NativeSetting>,
}

/// The VNM driver.
pub struct VnmDriver {
    settings: LiveSettings<VnmSettings>,
    /// The motion warning, cleared by a successful connect.
    warning: MotionWarning,
    /// The open serial port.
    link: Option<Box<dyn Link>>,
    travel: Option<CornerTravel>,
    /// The caller's list after the reverse switches (a reversed slot becomes 65536 minus itself, in place).
    edited: Option<Vec<i64>>,
    /// Whether the last `&[i64]` frame had a slot written.
    wrote: bool,
}

impl VnmDriver {
    pub fn new(settings: LiveSettings<VnmSettings>, warning: MotionWarning) -> Self {
        VnmDriver { settings, warning, link: None, travel: None, edited: None, wrote: false }
    }

    pub fn factory(settings: LiveSettings<VnmSettings>, warning: MotionWarning) -> DriverFactory {
        Box::new(move || Box::new(VnmDriver::new(settings.clone(), warning.clone())))
    }

    /// Corner travel in millimetres computed from the last frame.
    pub fn corner_travel(&self) -> Option<CornerTravel> {
        self.travel
    }
    /// The 12 reordered frame values: reversal of ports 5–9 first (in the caller's list, each written back at its
    /// own frame line), then the reordering, which reads the list in
    /// mapping order.
    fn reordered(
        &mut self,
        model: VnmModel,
        settings: &VnmSettings,
        motors: &mut LegacyValue,
    ) -> Result<Vec<LegacyValue>, MotionError> {
        for (k, &compatibility) in settings.reverse_ports.iter().enumerate() {
            // Each reverse switch is read as a boolean, each
            // followed by the reversal of its slot.
            let test = send_vnm_frame(REVERSE_FIRST_LINE + 3 * k as u32);
            let reversed = match settings.native_reverse_ports.as_ref() {
                Some(native) => settings
                    .runtime
                    .switch("VNM SIMULATION", &format!("reverse_controller_port_{}", k + 5), &native[k])
                    .map_err(|detail| boolean_setting_error(&detail, test))?,
                None => compatibility,
            };
            if reversed {
                let slot = FIRST_REVERSIBLE_PORT - 1 + k;
                let statement = send_vnm_frame(REVERSE_FIRST_LINE + 1 + 3 * k as u32);
                let value = native::item(motors, slot).map_err(|e| e.in_frame(statement))?;
                let value = native::sub(&native::int(FULL_SCALE), &value).map_err(|e| e.in_frame(statement))?;
                native::assign(motors, slot, value).map_err(|e| e.in_frame(statement))?;
                self.wrote = true;
            }
        }
        model
            .frame_sources()
            .into_iter()
            .map(|source| native::item(motors, source).map_err(|e| e.in_frame(send_vnm_frame(MAPPING_LINE))))
            .collect()
    }

    /// The send pass: nothing happens without an open port; an axis outside 16 bits fails the
    /// call before the actuator positions are updated; only a failed
    /// write is handled here (`Not Connected`, port 0 forgotten, port closed).
    fn send_vnm(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let model =
            VnmModel::from_name(ctx.model.name).ok_or_else(|| MotionError::UnknownModel(ctx.model.name.to_string()))?;
        let settings = self.settings.get();
        let axes = self.reordered(model, &settings, motors)?;
        let Some(link) = self.link.as_mut() else {
            return Ok(());
        };
        // The frame is built: each axis is converted on two frame lines of its own; the
        // first fails for a value outside 16 bits.
        let bytes = frame::encode_native(&axes[..9]).map_err(|e| match e {
            frame::NativeAxisError::OutOfRange(axis) => {
                MotionError::Native(format!("ValueError: {}", frame::OutOfRange(axis, 0))).in_frames([
                    send_vnm_frame(GEN_COMMAND_LINE),
                    PyFrame::new(VNM_FILE, "gen_command", GEN_COMMAND_FIRST_AXIS_LINE + 2 * axis as u32),
                    PyFrame::new(VNM_FILE, "int_to_bytes", 28),
                ])
            }
            frame::NativeAxisError::Raised(axis, line, error) => error.in_frames([
                send_vnm_frame(GEN_COMMAND_LINE),
                PyFrame::new(VNM_FILE, "gen_command", GEN_COMMAND_FIRST_AXIS_LINE + 2 * axis as u32),
                PyFrame::new(VNM_FILE, "int_to_bytes", line),
            ]),
        })?;
        // The model's actuator positions: one entry per frame line, each its value rescaled
        // from 0..65535 to 0..the vertical stroke setting as an integer.
        let first_line = match model {
            VnmModel::Front2Rear1 => 160,
            VnmModel::Front1Rear2 => 166,
            _ => 172,
        };
        let mut corners = [0.0; 4];
        for ((corner, axis), line) in corners.iter_mut().zip(model.corner_axes()).zip(first_line..) {
            let statement = send_vnm_frame(line);
            let stroke = match &settings.native_vertical_stroke_mm {
                Some(native) => settings
                    .runtime
                    .value("MOTION_COMPENSATION", "vertical_actuators_stroke", native)
                    .map_err(MotionError::Native)
                    .and_then(|raw| native::to_int(&raw))
                    .map_err(|e| e.in_frame(statement))?,
                None => native::int(settings.vertical_stroke_mm),
            };
            *corner = native::map([&axes[axis], &native::int(0), &native::int(65_535), &native::int(0), &stroke])
                .map_err(|e| e.in_frame(statement))?;
        }
        let [front_left, front_right, rear_left, rear_right] = corners;
        self.travel = Some(CornerTravel { front_left, front_right, rear_left, rear_right });
        if let Err(e) = link.write_all(&bytes) {
            // The write error is logged at debug level with its text.
            debug!("error writing to controller {}", super::python::link_error_str(&e));
            ctx.link.status = LinkStatus::NotConnectedAfterWriteError;
            // Removing motion port entry 0 is a key error, before the port is closed, when the entry is missing.
            if ctx.link.ports.remove(&0).is_none() {
                // It occurs while the write error is being handled, which is printed first where
                // its frames are known.
                let key_error = MotionError::Native("KeyError: 0".into()).in_frame(send_vnm_frame(194));
                let handled = MotionError::Link(e).in_serial_call(super::SerialCall::Write, send_vnm_frame(190));
                return Err(if handled.frames().is_empty() { key_error } else { key_error.while_handling(handled) });
            }
            self.link = None;
        }
        Ok(())
    }

    /// An `&[i64]` frame through the dynamic-value path; the reversed slots become the edited frame.
    fn send_vnm_compat(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.wrote = false;
        let (result, list) = native::compat(motors, |list| self.send_vnm(list, ctx));
        if self.wrote {
            self.edited = Some(native::edited_vec(&list, motors));
        }
        result
    }
}

impl MotionDriver for VnmDriver {
    /// Open the configured port; there is no handshake. An empty `vnm_com_port`
    /// returns without trying, an open error leaves the platform "not connected", and a successful open
    /// clears `motion_warning`.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let settings = self.settings.get();
        if settings.port.is_empty() {
            return Ok(());
        }
        let options = OpenOptions { baud_rate: BAUD, write_timeout: None };
        // A failed open is ignored; neither outcome is logged.
        // The three assignments follow a successful open.
        if let Ok(link) = ctx.host.open_serial(&settings.port, &options) {
            self.link = Some(link);
            ctx.link.status = LinkStatus::Connected;
            ctx.link.ports.insert(0, settings.port);
            self.warning.set(String::new());
        }
        Ok(())
    }

    /// Disconnecting does nothing: the port stays open until the next connect replaces it.
    fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        Ok(())
    }

    /// A stream frame: only while the motion status is `Connected`.
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if !ctx.link.is_connected() {
            return Ok(());
        }
        // The send pass; an error passes this frame.
        self.send_vnm_compat(motors, ctx).map_err(|e| e.through(PyFrame::new(VNM_FILE, "send_stream", 204)))
    }

    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if !ctx.link.is_connected() {
            return Ok(());
        }
        self.send_vnm(motors, ctx).map_err(|e| e.through(PyFrame::new(VNM_FILE, "send_stream", 204)))
    }

    /// A test frame, identical to a stream frame except for its frame line.
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if !ctx.link.is_connected() {
            return Ok(());
        }
        self.send_vnm_compat(motors, ctx).map_err(|e| e.through(PyFrame::new(VNM_FILE, "send_test", 209)))
    }

    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if !ctx.link.is_connected() {
            return Ok(());
        }
        self.send_vnm(motors, ctx).map_err(|e| e.through(PyFrame::new(VNM_FILE, "send_test", 209)))
    }

    fn actuators_position(&self) -> Option<ActuatorPositions> {
        self.travel.map(CornerTravel::to_positions)
    }

    fn actuators_travel(&self) -> Option<CornerTravel> {
        self.travel
    }

    fn take_edited_frame(&mut self) -> Option<Vec<i64>> {
        self.edited.take()
    }

    fn describe(&self) -> String {
        format!("VNM Simulation on {}", self.settings.get().port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::testing::{Device, FakeHost};
    use crate::motion::{motion_model, LinkState};

    const PORT: &str = "/dev/ttyUSB7";

    fn settings() -> VnmSettings {
        VnmSettings {
            port: PORT.into(),
            reverse_ports: [false; 5],
            native_reverse_ports: None,
            vertical_stroke_mm: 100,
            native_vertical_stroke_mm: None,
            runtime: Default::default(),
        }
    }

    struct Rig {
        driver: VnmDriver,
        warning: MotionWarning,
        host: FakeHost,
        link: LinkState,
        model: &'static str,
    }

    impl Rig {
        fn new(model: VnmModel, settings: VnmSettings) -> Self {
            let warning = MotionWarning::new("left by an earlier driver".into());
            Rig {
                driver: VnmDriver::new(LiveSettings::new(settings), warning.clone()),
                warning,
                host: FakeHost::with_ports(&[PORT]),
                link: LinkState::default(),
                model: model.name(),
            }
        }
        fn call<R>(&mut self, f: impl FnOnce(&mut VnmDriver, &mut MotionContext<'_>) -> R) -> R {
            let spec = motion_model(self.model, false).unwrap();
            let mut ctx = MotionContext { model: spec, link: &mut self.link, host: &mut self.host };
            f(&mut self.driver, &mut ctx)
        }
        fn connect(&mut self) {
            self.call(|d, ctx| d.connect(ctx)).unwrap();
        }
        fn stream(&mut self, motors: &[i64]) -> Result<(), MotionError> {
            self.call(|d, ctx| d.send_stream(motors, ctx))
        }
        fn frames(&mut self) -> Vec<Vec<u8>> {
            self.host.log(PORT).written()
        }
    }

    fn twelve(first: &[i64]) -> Vec<i64> {
        let mut v = first.to_vec();
        v.resize(12, 32768);
        v
    }

    #[test]
    fn gen_command_layout() {
        // The frame: FF FF, nine big-endian words, 0A 0D.
        let f = frame::encode(&[0x0102, 0, 65535, 3, 4, 5, 6, 7, 0xABCD]).unwrap();
        assert_eq!(f.len(), 22);
        assert_eq!(&f[..6], &[0xFF, 0xFF, 0x01, 0x02, 0x00, 0x00]);
        assert_eq!(&f[6..8], &[0xFF, 0xFF]);
        assert_eq!(&f[18..], &[0xAB, 0xCD, 0x0A, 0x0D]);
    }

    #[test]
    fn int_to_bytes_rejects_out_of_range() {
        // A value below 0 or above 65535 is a value error.
        assert_eq!(frame::encode(&[65536, 0, 0, 0, 0, 0, 0, 0, 0]), Err(frame::OutOfRange(0, 65536)));
        assert!(frame::encode(&[-1, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn get_port_mapping_tables() {
        // The port mapping: the slot of each port's actuator for the ports, identity beyond.
        assert_eq!(VnmModel::Corners4.frame_sources(), [1, 2, 3, 0, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(VnmModel::Front2Rear1.frame_sources(), [1, 2, 0, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(VnmModel::Front1Rear2.frame_sources(), [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(VnmModel::Corners4TwoTl.frame_sources(), [1, 2, 3, 0, 5, 4, 6, 7, 8, 9, 10, 11]);
        assert_eq!(VnmModel::Corners4TwoTlSurgeBelt.frame_sources(), [1, 2, 3, 0, 6, 4, 5, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn model_table_names_and_counts() {
        for m in VnmModel::ALL {
            assert_eq!(VnmModel::from_name(m.name()), Some(m));
            let expected: usize = m.name()[..1].parse().unwrap();
            assert_eq!(m.actuators(), expected, "{}", m.name());
            assert_eq!(motion_model(m.name(), false).unwrap().motors, expected);
        }
    }

    #[test]
    fn connect_opens_the_configured_port_at_57600_without_handshake() {
        // Connect: the port at 57600, 8 bits, 1 stop bit, timeout 2 s; motion port entry 0 set;
        // the motion warning cleared.
        let mut rig = Rig::new(VnmModel::Corners4, settings());
        rig.connect();
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some(PORT));
        assert_eq!(rig.warning.get(), "");
        let opens = rig.host.log(PORT).opens.clone();
        assert_eq!(opens[0].baud_rate, 57600);
        assert!(rig.frames().is_empty());
    }

    #[test]
    fn connect_without_port_does_nothing() {
        let mut rig = Rig::new(VnmModel::Corners4, VnmSettings::default());
        rig.connect();
        assert!(!rig.link.is_connected());
        assert!(rig.host.log(PORT).opens.is_empty());
    }

    #[test]
    fn connect_open_failure_stays_disconnected() {
        let mut rig = Rig::new(VnmModel::Corners4, settings());
        rig.host = FakeHost::with_ports(&[PORT]).device(PORT, Device { fail_open: true, ..Device::default() });
        rig.connect();
        assert!(!rig.link.is_connected());
        // The connect clears the motion warning only on success.
        assert_eq!(rig.warning.get(), "left by an earlier driver");
    }

    #[test]
    fn send_vnm_reorders_slots_to_vnm_ports() {
        // 4 Actuators: slots are RL, FL, FR, RR; VNM ports are FL, FR, RR, RL.
        let mut rig = Rig::new(VnmModel::Corners4, settings());
        rig.connect();
        let motors = [1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000, 9000, 10, 11, 12];
        rig.stream(&motors).unwrap();
        let f = rig.frames()[0].clone();
        let word = |i: usize| u16::from_be_bytes([f[2 + 2 * i], f[3 + 2 * i]]);
        assert_eq!((0..9).map(word).collect::<Vec<_>>(), [2000, 3000, 4000, 1000, 5000, 6000, 7000, 8000, 9000]);
        // The actuator positions: FL from 0, FR from 1, RL from 3, RR from 2, each rescaled from 0..65535 to 0..stroke.
        let t = rig.driver.corner_travel().unwrap();
        assert!((t.front_left - 2000.0 * 100.0 / 65535.0).abs() < 1e-9);
        assert!((t.rear_left - 1000.0 * 100.0 / 65535.0).abs() < 1e-9);
        assert!((t.rear_right - 4000.0 * 100.0 / 65535.0).abs() < 1e-9);
    }

    #[test]
    fn reverse_switches_invert_ports_five_to_nine() {
        // Slot k-1 becomes 65536 minus itself for reverse_controller_port_k, before mapping.
        let mut cfg = settings();
        cfg.reverse_ports = [true, false, false, false, true];
        let mut rig = Rig::new(VnmModel::Corners4Tl, cfg);
        rig.connect();
        rig.stream(&twelve(&[0, 0, 0, 0, 1000, 2000, 0, 0, 3000])).unwrap();
        let f = rig.frames()[0].clone();
        assert_eq!(u16::from_be_bytes([f[10], f[11]]), 64536);
        assert_eq!(u16::from_be_bytes([f[12], f[13]]), 2000);
        assert_eq!(u16::from_be_bytes([f[18], f[19]]), 62536);
        // The reversal is made in the caller's list, which the motion pipeline keeps as its previous frame.
        let edited = rig.driver.take_edited_frame().unwrap();
        assert_eq!(&edited[..9], &[0, 0, 0, 0, 64536, 2000, 0, 0, 62536]);
        assert!(rig.driver.take_edited_frame().is_none());
    }

    #[test]
    fn native_reverse_failure_keeps_the_completed_caller_edit() {
        let mut cfg = settings();
        cfg.native_reverse_ports =
            Some([Ok(true), Err("KeyError: 'reverse_controller_port_6'".into()), Ok(false), Ok(false), Ok(false)]);
        let mut rig = Rig::new(VnmModel::Corners4Tl, cfg);
        rig.connect();
        let motors = twelve(&[100, 200, 300, 400, 1000, 2000]);
        let error = rig.stream(&motors).unwrap_err();
        assert_eq!(error.python_str(), "'reverse_controller_port_6'");
        let edited = rig.driver.take_edited_frame().unwrap();
        assert_eq!(edited[4], 64_536);
        assert_eq!(edited[5], 2000);
        assert!(rig.frames().is_empty());
        assert!(rig.link.is_connected());
        assert!(rig.driver.corner_travel().is_none());
    }

    #[test]
    fn native_reverse_read_precedes_only_its_reached_motor_access() {
        let mut cfg = settings();
        cfg.native_reverse_ports = Some([
            Ok(false),
            Err("Exception: Boolean value expected. Got invalid".into()),
            Ok(false),
            Ok(false),
            Ok(false),
        ]);
        let mut rig = Rig::new(VnmModel::Corners4Tl, cfg.clone());
        rig.connect();
        let error = rig.stream(&[]).unwrap_err();
        assert_eq!(error.python_str(), "Boolean value expected. Got invalid");
        assert!(rig.driver.take_edited_frame().is_none());
        cfg.native_reverse_ports.as_mut().unwrap()[0] = Ok(true);
        rig.driver.settings.set(cfg);
        assert!(matches!(rig.stream(&[]).unwrap_err().root(), MotionError::TooFewMotors { needed: 5, got: 0 }));
        assert!(rig.frames().is_empty());
    }

    #[test]
    fn native_reverse_disconnected_guard_and_recovery_preserve_frame_order() {
        let mut cfg = settings();
        cfg.native_reverse_ports = Some(std::array::from_fn(|_| Err("KeyError: 'reverse'".into())));
        let mut rig = Rig::new(VnmModel::Corners4Tl, cfg);
        rig.stream(&[]).unwrap();
        rig.call(|driver, context| driver.send_test(&[], context)).unwrap();
        assert!(rig.driver.take_edited_frame().is_none());
        rig.connect();
        rig.driver.settings.update(|settings| settings.native_reverse_ports = Some(std::array::from_fn(|_| Ok(false))));
        let motors = twelve(&[100, 200, 300, 400, 1000, 2000]);
        rig.stream(&motors).unwrap();
        let before = rig.driver.corner_travel();
        rig.driver.settings.update(|settings| {
            settings.native_reverse_ports.as_mut().unwrap()[0] = Ok(true);
            settings.native_reverse_ports.as_mut().unwrap()[1] = Err("KeyError: 'reverse_controller_port_6'".into());
        });
        assert!(rig.stream(&motors).is_err());
        assert_eq!(rig.frames().len(), 1);
        assert_eq!(rig.driver.corner_travel(), before);
        assert_eq!(rig.driver.take_edited_frame().unwrap()[4], 64536);
        rig.driver.settings.update(|settings| settings.native_reverse_ports.as_mut().unwrap()[1] = Ok(false));
        rig.stream(&motors).unwrap();
        assert_eq!(rig.frames().len(), 2);
        assert_eq!(u16::from_be_bytes([rig.frames()[1][10], rig.frames()[1][11]]), 64536);
    }

    #[test]
    fn native_reverse_partial_edits_reach_the_registry_caller_on_error() {
        use crate::motion::{DriverEnvironment, DriverKind, MotionRegistry};
        let mut cfg = settings();
        cfg.native_reverse_ports =
            Some([Ok(true), Err("KeyError: 'reverse_controller_port_6'".into()), Ok(false), Ok(false), Ok(false)]);
        let live = LiveSettings::new(cfg);
        let mut registry =
            MotionRegistry::new(DriverEnvironment { windows_drivers: true, ..DriverEnvironment::srs_linux() });
        registry.register(DriverKind::Vnm, VnmDriver::factory(live, MotionWarning::default()));
        let mut host = FakeHost::with_ports(&[PORT]);
        registry.connect(VnmModel::Corners4Tl.name(), &mut host).unwrap();
        let mut motors = twelve(&[100, 200, 300, 400, 1000, 2000]);
        let error = registry.send_stream_in_place(VnmModel::Corners4Tl.name(), &mut motors, &mut host).unwrap_err();
        assert_eq!(error.python_str(), "'reverse_controller_port_6'");
        assert_eq!(motors[4], 64536);
        assert_eq!(motors[5], 2000);
        assert!(host.log(PORT).written().is_empty());
    }

    #[test]
    fn three_actuator_corner_branches() {
        // <2 Front + 1 Rear> → FL from 0, FR from 1, RL/RR from 2; <1 Front + 2 Rear> → FL/FR from 0, RL from 2,
        // RR from 1.
        let motors = twelve(&[65535, 0, 32767]);
        let mut rig = Rig::new(VnmModel::Front2Rear1, settings());
        rig.connect();
        rig.stream(&motors).unwrap();
        let t = rig.driver.corner_travel().unwrap();
        assert_eq!((t.front_left, t.front_right), (0.0, 32767.0 * 100.0 / 65535.0));
        assert_eq!((t.rear_left, t.rear_right), (100.0, 100.0));
        let mut rig = Rig::new(VnmModel::Front1Rear2, settings());
        rig.connect();
        rig.stream(&motors).unwrap();
        let t = rig.driver.corner_travel().unwrap();
        assert_eq!((t.front_left, t.front_right), (100.0, 100.0));
        assert_eq!((t.rear_left, t.rear_right), (32767.0 * 100.0 / 65535.0, 0.0));
    }

    #[test]
    fn out_of_range_axis_raises_the_source_value_error_text() {
        // The value error's text is 'Input must be a 16-bit integer (0 to 65535)', which
        // the motion loop logs after 'error sending data to motionbox: '. A reversed 0 is 65536.
        let mut cfg = settings();
        cfg.reverse_ports[0] = true;
        let mut rig = Rig::new(VnmModel::Corners4Tl, cfg);
        rig.connect();
        let error = rig.stream(&twelve(&[0, 0, 0, 0, 0])).unwrap_err();
        assert_eq!(error.python_str(), "Input must be a 16-bit integer (0 to 65535)");
    }

    #[test]
    fn out_of_range_value_is_raised_without_disconnecting() {
        // A reversed 0 becomes 65536; building the frame fails, which is not the handled write error, so
        // the port stays open, the motion status stays Connected and the actuator positions are not updated.
        let mut cfg = settings();
        cfg.reverse_ports[0] = true;
        let mut rig = Rig::new(VnmModel::Corners4Tl, cfg);
        rig.connect();
        let error = rig.stream(&twelve(&[0, 0, 0, 0, 0])).unwrap_err();
        // Port 5 is the fifth axis of the frame: the frames are the stream frame, the send pass, the frame
        // builder (the first conversion line of axis 5) and the byte conversion.
        assert_eq!(error.to_string(), "ValueError: Input must be a 16-bit integer (0 to 65535)");
        let frames: Vec<(&str, u32)> = error.frames().iter().map(|frame| (frame.function, frame.line)).collect();
        assert_eq!(frames, [("send_stream", 204), ("send_vnm", 156), ("gen_command", 47), ("int_to_bytes", 28)]);
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some(PORT));
        assert!(rig.frames().is_empty());
        assert!(rig.driver.corner_travel().is_none());
        // The next valid frame goes out on the same port.
        rig.stream(&twelve(&[30000, 0, 0, 0, 1000])).unwrap();
        assert_eq!(rig.frames().len(), 1);
    }

    #[test]
    fn native_stroke_failure_preserves_travel_reverse_prefix_and_open_port() {
        let mut rig = Rig::new(VnmModel::Corners4Tl, settings());
        rig.connect();
        let motors = twelve(&[100, 200, 300, 400, 1000, 2000]);
        rig.stream(&motors).unwrap();
        let previous = rig.driver.corner_travel();
        rig.driver.settings.update(|settings| {
            settings.reverse_ports[0] = true;
            settings.native_vertical_stroke_mm =
                Some(Err("ValueError: invalid literal for int() with base 10: '150.5'".into()));
        });
        let error = rig.stream(&motors).unwrap_err();
        assert_eq!(error.python_str(), "invalid literal for int() with base 10: '150.5'");
        assert_eq!(rig.frames().len(), 1, "no frame writes occur after failed corner conversion");
        assert_eq!(rig.driver.corner_travel(), previous);
        assert_eq!(rig.driver.take_edited_frame().unwrap()[4], 64536);
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some(PORT));
        assert!(rig.driver.link.is_some());
        rig.driver.settings.update(|settings| settings.native_vertical_stroke_mm = Some(Ok(LegacyValue::Int(275))));
        rig.stream(&motors).unwrap();
        assert_eq!(rig.frames().len(), 2);
        assert_eq!(rig.driver.corner_travel(), Some(CornerTravel::from_16bit([200, 300, 100, 400], 275)));
    }

    #[test]
    fn native_stroke_is_read_after_connection_and_frame_encoding_guards() {
        let mut cfg = settings();
        cfg.native_vertical_stroke_mm = Some(Err("KeyError: 'vertical_actuators_stroke'".into()));
        let mut rig = Rig::new(VnmModel::Corners4, cfg);
        rig.stream(&[]).unwrap();
        rig.call(|driver, context| driver.send_test(&[], context)).unwrap();
        rig.link.status = LinkStatus::Connected;
        rig.stream(&twelve(&[30000; 12])).unwrap();
        assert!(rig.driver.corner_travel().is_none(), "no serial object skips stroke and corner publication");
        rig.connect();
        let error = rig.stream(&twelve(&[65536])).unwrap_err();
        assert!(
            error.to_string().starts_with("ValueError: Input must"),
            "frame encoding fails before the missing stroke"
        );
        rig.driver.settings.update(|settings| {
            settings.native_reverse_ports = Some(std::array::from_fn(|_| Err("KeyError: 'reverse'".into())));
        });
        assert_eq!(rig.stream(&twelve(&[30000; 12])).unwrap_err().python_str(), "'reverse'");
        assert!(rig.frames().is_empty());
        assert!(rig.link.is_connected());
    }

    #[test]
    fn native_stroke_error_precedes_write_failure_and_atomic_publication() {
        let mut cfg = settings();
        cfg.native_vertical_stroke_mm = Some(Err("KeyError: 'vertical_actuators_stroke'".into()));
        let mut rig = Rig::new(VnmModel::Corners4, cfg);
        rig.host = FakeHost::with_ports(&[PORT]).device(PORT, Device { fail_writes: true, ..Device::default() });
        rig.connect();
        let motors = twelve(&[30000; 12]);
        assert_eq!(rig.stream(&motors).unwrap_err().python_str(), "'vertical_actuators_stroke'");
        assert!(rig.link.is_connected());
        assert!(rig.driver.link.is_some());
        assert!(rig.driver.corner_travel().is_none());
        rig.driver.settings.update(|settings| settings.native_vertical_stroke_mm = Some(Ok(LegacyValue::Int(275))));
        rig.stream(&motors).unwrap();
        assert_eq!(rig.link.status, LinkStatus::NotConnectedAfterWriteError);
        assert!(rig.driver.link.is_none());
        assert_eq!(rig.driver.corner_travel(), Some(CornerTravel::from_16bit([30000; 4], 275)));
    }

    #[test]
    fn write_failure_disconnects_and_forgets_port_zero() {
        // A failed write: the motion status becomes 'Not Connected', motion port entry 0 is removed, the port is closed.
        let mut rig = Rig::new(VnmModel::Corners4, settings());
        rig.host = FakeHost::with_ports(&[PORT]).device(PORT, Device { fail_writes: true, ..Device::default() });
        rig.connect();
        rig.stream(&twelve(&[30000; 12])).unwrap();
        assert_eq!(rig.link.status, LinkStatus::NotConnectedAfterWriteError, "'Not Connected', capital C");
        assert!(rig.link.ports.is_empty());
        // The actuator positions are computed before the write.
        assert!(rig.driver.corner_travel().is_some());
        // A stream frame does nothing while not connected.
        rig.stream(&twelve(&[30000; 12])).unwrap();
        assert_eq!(rig.host.log(PORT).writes.len(), 0);
    }

    #[test]
    fn nothing_is_sent_or_published_without_an_open_port() {
        // The send pass needs the status 'Connected' and an open port, else it returns.
        let mut rig = Rig::new(VnmModel::Corners4, VnmSettings::default());
        rig.link.status = LinkStatus::Connected;
        rig.stream(&twelve(&[30000; 12])).unwrap();
        assert!(rig.driver.corner_travel().is_none());
    }

    #[test]
    fn short_frame_is_an_index_error() {
        // The slots are read in port-mapping order: the first position past the list (slot 4) is an error.
        let mut rig = Rig::new(VnmModel::Corners4, settings());
        rig.connect();
        assert!(matches!(
            rig.stream(&[1, 2, 3, 4]).unwrap_err().root(),
            MotionError::TooFewMotors { needed: 5, got: 4 }
        ));
    }

    fn shared(values: Vec<LegacyValue>) -> LegacyValue {
        LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::List(values)))
    }

    fn wide(bits: usize) -> LegacyValue {
        LegacyValue::from_integer(num_bigint::BigInt::from(1) << bits)
    }

    /// The send pass on the caller's dynamic list: the reversal of slot 4 is written into the shared
    /// object at once, so another holder sees it even though building the frame then fails for the reversed
    /// axis.
    #[test]
    fn native_reversal_is_published_before_gen_command_raises() {
        let mut settings = settings();
        settings.reverse_ports[0] = true;
        let mut rig = Rig::new(VnmModel::Corners4Tl, settings);
        rig.connect();
        let mut values: Vec<LegacyValue> = (0..12).map(|_| native::int(32768)).collect();
        values[4] = native::int(0);
        let mut list = shared(values);
        let alias = list.clone();
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        assert_eq!(native::item(&alias, 4).unwrap(), native::int(65_536), "written before the error");
        assert_eq!(error.exception_line().unwrap(), "ValueError: Input must be a 16-bit integer (0 to 65535)");
        let frames: Vec<_> = error.frames().iter().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 204), ("send_vnm", 156), ("gen_command", 47), ("int_to_bytes", 28)]);
        assert!(rig.frames().is_empty());
    }

    /// A slot of any width reaches the 16-bit range test unnarrowed; the reversal computes with it.
    #[test]
    fn native_wide_slots_are_compared_and_reversed_at_full_width() {
        let mut reversing = settings();
        reversing.reverse_ports[0] = true;
        let mut rig = Rig::new(VnmModel::Corners4Tl, reversing);
        rig.connect();
        let mut values: Vec<LegacyValue> = (0..12).map(|_| native::int(32768)).collect();
        values[4] = wide(80);
        let mut list = shared(values);
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        let expected = native::sub(&native::int(65_536), &wide(80)).unwrap();
        assert_eq!(native::item(&list, 4).unwrap(), expected);
        assert_eq!(error.python_str(), "Input must be a 16-bit integer (0 to 65535)");
        // The integer conversion of the stroke for each corner entry follows the frame, at any width.
        let mut rig =
            Rig::new(VnmModel::Corners4, VnmSettings { native_vertical_stroke_mm: Some(Ok(wide(2000))), ..settings() });
        rig.connect();
        let mut list = shared((0..12).map(|_| native::int(32768)).collect());
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "OverflowError: int too large to convert to float");
        let frames: Vec<_> = error.frames().iter().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 204), ("send_vnm", 172), ("map", 204)]);
        assert!(rig.frames().is_empty(), "the actuators_position statement precedes the write");
    }
}
