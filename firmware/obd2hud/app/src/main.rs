//! OBD2HUD ESP32 firmware entry point.
//!
//! On the ESP-IDF target this is `app_main`, which starts the loop task (set-up once,
//! then the main loop forever). Host builds only exist so that `cargo test` can build the
//! workspace; the binary does nothing there.

#![cfg_attr(target_os = "espidf", no_std)]
#![cfg_attr(target_os = "espidf", no_main)]
#![cfg_attr(target_arch = "xtensa", feature(asm_experimental_arch))]
#[cfg(target_os = "espidf")]
extern crate alloc;
#[cfg(target_os = "espidf")]
mod fw;

#[cfg(target_os = "espidf")]
#[no_mangle]
pub extern "C" fn app_main() {
    esp_idf_sys::link_patches();
    fw::app_main();
}

#[cfg(not(target_os = "espidf"))]
fn main() {}
