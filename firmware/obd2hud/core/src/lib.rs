//! Host-testable core of the OBD2HUD firmware.
//!
//! The firmware runs on an ESP32 (board `wemos_d1_mini32`). It is a Bluetooth LE peripheral
//! (`OBD2HUD-xxxxx`) that receives the `'~'`-joined telemetry line and plays a car ECU on
//! the CAN bus (OBD-II mode 01/09 responder, id 0x7E8) for an off-the-shelf OBD2 head-up
//! display, switches the HUD power output (GPIO19) and accepts BLE OTA updates.
//!
//! Hardware access is behind
//! small traits so the exact register / NVS / serial / BLE sequences can be checked on the host.
#![no_std]
#![cfg_attr(target_arch = "xtensa", feature(asm_experimental_arch))]

extern crate alloc;

pub mod analog;
pub mod ble;
pub mod can;
pub mod consts;
pub mod eeprom;
pub mod fpu;
pub mod gate;
pub mod messenger;
pub mod obd;
pub mod ota;
pub mod random;
pub mod serial;
pub mod telemetry;
pub mod xtea;
