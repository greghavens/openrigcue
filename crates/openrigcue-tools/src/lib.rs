//! Linux maintenance tools for DOF Reality motion controllers.
//! Device I/O is explicit; discovering USB devices never opens a controller.
pub mod cli;
pub mod dfu;
pub mod firmware;
pub mod hid;
pub mod smc;
pub mod usb;
pub mod web;
