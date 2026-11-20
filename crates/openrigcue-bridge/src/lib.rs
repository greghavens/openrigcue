//! openrigcue-bridge: runs inside a game's Wine/Proton prefix and makes the game's Windows-only telemetry channels
//! visible to the native openrigcue process.
//!
//! - Named file mappings the game listeners read are copied into `/dev/shm/openrigcue/<name>`, and the state of
//!   their data-ready events written beside them (see [`mirror`], [`event`]), which
//!   `openrigcue_sources::shared_memory::mapping::MappedView` reads.
//! - Named pipes that games connect to are served here and relayed to a Unix socket or localhost TCP
//!   (see [`relay`]).
//! - Sections published for other Windows programs are created from openrigcue's Linux files
//!   (see [`export`]).
//!
//! The platform-independent logic lives in this library and is tested on Linux; the Win32 side is in the
//! `windows` module, compiled only for Windows targets.

pub mod catalog;
pub mod cli;
pub mod control;
pub mod event;
pub mod export;
pub mod mirror;
pub mod paths;
pub mod plan;
pub mod relay;

#[cfg(any(windows, test))]
mod section_mapping;

#[cfg(windows)]
pub mod windows;
