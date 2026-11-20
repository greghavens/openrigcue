//! `Aerofly_FS_2_GamePlugin_Telemetry.dll` (PE32+ x86-64), an Aerofly FS 2
//! "external DLL" as the Aerofly SDK defines it.
//!
//! Exports (ordinals 1-4 in this order):
//!   Aerofly_FS_2_External_DLL_GetInterfaceVersion  returns 2
//!   Aerofly_FS_2_External_DLL_Init                 returns true
//!   Aerofly_FS_2_External_DLL_Shutdown             does nothing
//!   Aerofly_FS_2_External_DLL_Update               see below
//!
//! Update parses the received message list, picks Aircraft.Pitch, .Bank,
//! .RateOfTurn, .AngularVelocity, .IndicatedAirspeed and .Gear, and sends
//! `"%lld;%lld;%lld;%lld;%lld;%lld;%lld;%lld"` by UDP to 127.0.0.1:4123.
//!
//! On Windows the DLL is `no_std` (no Rust allocator: the message vector,
//! name table, std::mutex and C++ exceptions are implemented in msvc_*,
//! sdk_table, std_mutex and cxx_eh) and imports
//! nothing from WS2_32.dll beyond socket/bind/sendto (by ordinal) and
//! getaddrinfo/freeaddrinfo; host builds use std for the unit tests.
#![cfg_attr(windows, no_std)]
#![allow(non_snake_case)] // crate/DLL name must stay Aerofly_FS_2_GamePlugin_Telemetry

#[cfg(not(windows))]
extern crate alloc;

pub mod hash;
pub mod message;
pub mod msvc_heap;
pub mod msvc_vector;
pub mod sdk_names;
pub mod sdk_table;
pub mod std_mutex;
pub mod telemetry;

#[cfg(windows)]
mod crt;
#[cfg(windows)]
mod cxx_eh;
#[cfg(windows)]
mod net;
#[cfg(windows)]
mod scrt;
#[cfg(windows)]
mod win;

#[cfg(windows)]
mod exports {
    use core::ffi::c_void;

    use crate::telemetry::{collect, datagram_for};
    use crate::win::MESSAGES;

    /// Returns interface version 2.
    #[no_mangle]
    pub extern "C" fn Aerofly_FS_2_External_DLL_GetInterfaceVersion() -> i32 {
        2
    }

    /// Returns true (the HINSTANCE argument is ignored).
    #[no_mangle]
    pub extern "C" fn Aerofly_FS_2_External_DLL_Init(_aerofly_hinstance: *mut c_void) -> bool {
        true
    }

    /// Does nothing.
    #[no_mangle]
    pub extern "C" fn Aerofly_FS_2_External_DLL_Shutdown() {}

    /// Arguments used: the received byte stream and
    /// the number of received messages. The delta time, the stream
    /// size and the further arguments (sent-message list) are not read,
    /// and nothing is written to the sent-message list. A failed
    /// allocation raises std::bad_alloc through here (see crt::RaiseException).
    #[no_mangle]
    pub unsafe extern "C" fn Aerofly_FS_2_External_DLL_Update(
        _delta_time: f64,
        received_byte_stream: *const u8,
        _received_byte_stream_size: u32,
        received_num_messages: u32,
    ) {
        let list = MESSAGES.get();
        // Unchecked indexing: stream + (u32)cursor.
        let base = received_byte_stream;
        collect(|i| unsafe { *base.add(i as usize) }, received_num_messages, list);
        if let Some(text) = datagram_for(list.as_slice()) {
            crate::net::send_frame(text.as_bytes());
        }
    }
}
