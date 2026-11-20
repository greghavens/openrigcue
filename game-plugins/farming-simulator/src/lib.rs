//! Game-side telemetry mods and their Rust packet/pipe implementation.

pub mod deflate7z;
pub mod frame;
pub mod icon;
pub mod package;
pub mod pipe;

/// Windows named pipe consumed by the Proton telemetry bridge.
pub const PIPE_PATH: &str = r"\\.\pipe\FMTelemetry";
pub const PIPE_NAME: &str = "FMTelemetry";

pub const LUA_FS19_FS22: &[u8] = include_bytes!("../mod/openrigcue/fs19-fs22.lua");
pub const LUA_FS25: &[u8] = include_bytes!("../mod/openrigcue/fs25.lua");
pub const MODDESC_FS19: &[u8] = include_bytes!("../mod/openrigcue/modDesc-19.xml");
pub const MODDESC_FS22: &[u8] = include_bytes!("../mod/openrigcue/modDesc-22.xml");
pub const MODDESC_FS25: &[u8] = include_bytes!("../mod/openrigcue/modDesc-25.xml");

pub const FS19_FS22_SCRIPT_LEN: usize = LUA_FS19_FS22.len();
pub const FS19_FS22_SCRIPT_CRC32: u32 = 0x5d655f3f;

pub fn script_fs19_fs22() -> Vec<u8> {
    LUA_FS19_FS22.to_vec()
}
