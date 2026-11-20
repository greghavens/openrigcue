//! The ODrive object: its send, parse and stop-loop behavior. Nothing drives it yet.
use super::{CommandId, FieldOverflow};
use std::io::{self, Write};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Field(#[from] FieldOverflow),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("'ODrive' object has no attribute '{0}'")]
    MissingAttribute(&'static str),
    #[error("index out of range")]
    Index,
    #[error("bytes_to_ieee754_float() takes exactly 4 positional arguments ({0} given)")]
    FloatArity(usize),
}
impl Error {
    pub fn class_name(&self) -> &'static str {
        match self {
            Self::MissingAttribute(_) => "AttributeError",
            Self::Index => "IndexError",
            Self::FloatArity(_) => "TypeError",
            Self::Io(_) => "OSError",
            Self::Field(_) => "error",
        }
    }
}

/// Missing constructor attributes remain distinct from an explicitly cleared receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReceiverAttribute {
    Missing = 0,
    None = 1,
    Present = 2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStep {
    Done,
    Sleep(Duration),
}
#[derive(Debug, Default)]
pub struct Lifecycle {
    flag: AtomicU8,
    receiver: AtomicU8,
}
impl Lifecycle {
    pub fn stop_flag(&self) -> Option<bool> {
        match self.flag.load(Ordering::SeqCst) {
            0 => None,
            1 => Some(false),
            _ => Some(true),
        }
    }
    pub fn set_stop_flag(&self, value: Option<bool>) {
        self.flag.store(
            match value {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            },
            Ordering::SeqCst,
        );
    }
    pub fn receiver(&self) -> ReceiverAttribute {
        match self.receiver.load(Ordering::SeqCst) {
            0 => ReceiverAttribute::Missing,
            1 => ReceiverAttribute::None,
            _ => ReceiverAttribute::Present,
        }
    }
    pub fn set_receiver(&self, value: ReceiverAttribute) {
        self.receiver.store(value as u8, Ordering::SeqCst);
    }
    pub fn receive_step(&self) -> Result<LoopStep, Error> {
        match self.stop_flag().ok_or(Error::MissingAttribute("flag_recv_thread_stop"))? {
            true => {
                self.set_receiver(ReceiverAttribute::None);
                Ok(LoopStep::Done)
            }
            false => Ok(LoopStep::Sleep(Duration::from_millis(10))),
        }
    }
    fn stop_step(&self) -> Result<LoopStep, Error> {
        match self.receiver() {
            ReceiverAttribute::Missing => Err(Error::MissingAttribute("recv_thread")),
            ReceiverAttribute::None => Ok(LoopStep::Done),
            ReceiverAttribute::Present => Ok(LoopStep::Sleep(Duration::from_millis(100))),
        }
    }
    pub fn receive_with(&self, mut sleep: impl FnMut(Duration) -> io::Result<()>) -> Result<(), Error> {
        while let LoopStep::Sleep(duration) = self.receive_step()? {
            sleep(duration)?;
        }
        Ok(())
    }
    pub fn stop_with(&self, mut sleep: impl FnMut(Duration) -> io::Result<()>) -> Result<(), Error> {
        self.set_stop_flag(Some(true));
        while let LoopStep::Sleep(duration) = self.stop_step()? {
            sleep(duration)?;
        }
        Ok(())
    }
    pub fn stop(&self) -> Result<(), Error> {
        self.stop_with(|duration| {
            std::thread::sleep(duration);
            Ok(())
        })
    }
}

/// Where the two debug lines go, before the serial write.
pub trait DebugSink {
    fn line(&mut self, line: &str) -> io::Result<()>;
}
#[derive(Debug, Default)]
pub struct StdoutDebug;
impl DebugSink for StdoutDebug {
    fn line(&mut self, line: &str) -> io::Result<()> {
        writeln!(io::stdout().lock(), "{line}")
    }
}

pub struct ODrive<C: Write, L: DebugSink = StdoutDebug> {
    connection: C,
    log: L,
    pub can_node_id: i64,
    voltage: Option<f64>,
    current: Option<f64>,
    lifecycle: Arc<Lifecycle>,
}
impl<C: Write> ODrive<C, StdoutDebug> {
    /// Only stores the provided connection/node. No receiver is started here.
    pub fn new(connection: C, node: i64) -> Self {
        Self::with_log(connection, node, StdoutDebug)
    }
}
impl<C: Write, L: DebugSink> ODrive<C, L> {
    pub fn with_log(connection: C, node: i64, log: L) -> Self {
        Self { connection, log, can_node_id: node, voltage: None, current: None, lifecycle: Arc::default() }
    }
    pub fn lifecycle(&self) -> Arc<Lifecycle> {
        Arc::clone(&self.lifecycle)
    }
    pub fn connection(&self) -> &C {
        &self.connection
    }
    pub fn connection_mut(&mut self) -> &mut C {
        &mut self.connection
    }
    pub fn voltage(&self) -> Result<f64, Error> {
        self.voltage.ok_or(Error::MissingAttribute("voltage"))
    }
    pub fn current(&self) -> Result<f64, Error> {
        self.current.ok_or(Error::MissingAttribute("current"))
    }
    pub fn message_id(&self, command: i64) -> i128 {
        (i128::from(self.can_node_id) << 5) | i128::from(command)
    }
    fn address(&self, mut frame: [u8; 12], command: CommandId) -> [u8; 12] {
        let id = self.message_id(command as i64);
        frame[0] = ((id >> 8) & 255) as u8;
        frame[1] = (id & 255) as u8;
        frame
    }
    pub fn send_raw(&mut self, message: &[u8]) -> Result<(), Error> {
        self.log.line(&format!("calculated bytes, node:  {} bytes:  {message:?}", self.can_node_id))?;
        let hex = message.iter().map(|byte| format!("0x{byte:x}")).collect::<Vec<_>>().join(" ");
        self.log.line(&format!("writing to the comport, node: {} value {hex}", self.can_node_id))?;
        let _ = self.connection.write(message)?;
        Ok(())
    }
    pub fn set_input_pos(&mut self, position: f64, velocity: i64, torque: i64) -> Result<(), Error> {
        let frame = self.address(super::set_input_pos(0, position, velocity, torque)?, CommandId::SetInputPos);
        self.send_raw(&frame)
    }
    pub fn set_input_vel(&mut self, velocity: f64, torque: f64) -> Result<(), Error> {
        let frame = self.address(super::set_input_vel(0, velocity, torque)?, CommandId::SetInputVel);
        self.send_raw(&frame)
    }
    pub fn set_controller_modes(&mut self, control_mode: i64, input_mode: i64) -> Result<(), Error> {
        let frame =
            self.address(super::set_controller_modes(0, control_mode, input_mode)?, CommandId::SetControllerModes);
        self.send_raw(&frame)
    }
    pub fn set_traj_accel_limits(&mut self, accel: f64, decel: f64) -> Result<(), Error> {
        let frame = self.address(super::set_traj_accel_limits(0, accel, decel)?, CommandId::SetTrajAccelLimits);
        self.send_raw(&frame)
    }
    pub fn reboot_odrive(&mut self) -> Result<(), Error> {
        let frame = self.address(super::bare(0, CommandId::Reboot), CommandId::Reboot);
        self.send_raw(&frame)
    }
    pub fn get_bus_voltage_and_current(&mut self) -> Result<(), Error> {
        let frame =
            self.address(super::bare(0, CommandId::GetBusVoltageAndCurrent), CommandId::GetBusVoltageAndCurrent);
        self.send_raw(&frame)
    }
    pub fn parse_message(&mut self, message: &[u8]) -> Result<(), Error> {
        let message = message.get(1..).unwrap_or_default();
        let command = message.first().ok_or(Error::Index)? & 31;
        let message = &message[1..];
        if command == CommandId::GetBusVoltageAndCurrent as u8 {
            fn float(bytes: &[u8]) -> Result<f64, Error> {
                let bytes: [u8; 4] = bytes.try_into().map_err(|_| Error::FloatArity(bytes.len()))?;
                Ok(super::bytes_to_ieee754_float(bytes))
            }
            self.voltage = Some(float(&message[..message.len().min(4)])?);
            self.current = Some(float(message.get(4..message.len().min(8)).unwrap_or_default())?);
        }
        Ok(())
    }
    /// The receiver contains no serial reads; it only waits on the stop flag.
    pub fn recv_thread_func(&self) -> Result<(), Error> {
        self.lifecycle.receive_with(|duration| {
            std::thread::sleep(duration);
            Ok(())
        })
    }
    pub fn stop(&self) -> Result<(), Error> {
        self.lifecycle.stop()
    }
}
impl<C: Write, L: DebugSink> Drop for ODrive<C, L> {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            // A failure while dropping cannot be returned; it is written to standard error.
            let _ =
                writeln!(io::stderr().lock(), "Exception ignored in ODrive.__del__: {}: {error}", error.class_name());
        }
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
