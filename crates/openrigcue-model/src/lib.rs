//! Shared data types.
//!
//! [`Telemetry`] is the contract between telemetry sources and every output (motion, shakers, accessories,
//! UI). It defines the full telemetry channel registry with each channel's units.
//! [`Frame`] is the earlier typed frame and is being retired in favour of [`Telemetry`].

pub mod automation;
mod frame;
pub mod motion_models;
pub mod motion_sensor;
pub mod native_arithmetic;
pub mod native_key_sort;
pub mod native_object;
#[cfg(test)]
mod native_storage_tests;
pub mod native_value;
pub mod python_compare;
pub mod python_numeric;
pub mod python_repr;
pub mod python_string;
pub mod python_time;
pub mod python_unicode;
pub mod serial_access;
mod telemetry;

pub use frame::{Corner, Flag, Frame, Gear, Wheels};
pub use telemetry::{NativeChannels, Telemetry};
