//! PT-ACTUATOR motion drivers.
//!
//! - [`ptamotion`]: "PTA Motion System UDP", 6-DOF poses as XOR-obfuscated UDP datagrams.
//! - [`ptcan`]: the "… CANBus" actuator models behind a serial CAN bridge.

pub mod ptamotion;
pub mod ptcan;

use crate::motion::LiveSettings;

/// The motion warning: text the UI shows under the motion status. Both PT drivers
/// clear it after a successful connect; the daemon passes the same holder to every driver that touches it.
pub type MotionWarning = LiveSettings<String>;
