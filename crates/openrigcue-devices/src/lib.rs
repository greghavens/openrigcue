//! Device discovery, transports and drivers.
//!
//! - [`discovery`] lists serial ports and holds the port filters scans apply.
//! - [`serial`] opens ports behind the small [`serial::Link`] trait so protocols are testable.
//! - [`motion`] holds the platform model table, the [`motion::MotionRegistry`] that dispatches to motion
//!   drivers, the drivers themselves and the opt-in safety [`motion::Supervisor`].

pub mod discovery;
pub mod motion;
mod platform;
pub mod serial;
