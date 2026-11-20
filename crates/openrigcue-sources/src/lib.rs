//! Game telemetry sources: one listener per game, producing the shared [`Telemetry`].
//!
//! Each listener implements one game's telemetry interface: transport, port,
//! packet identification, field decoding, unit conversions, channel mapping and liveness test.
//!
//! Layout:
//! - `udp_racing`: racing, rally, kart and road games that send UDP datagrams.
//! - `udp_flight`: flight, space and other UDP games (including text protocols).
//! - `shared_memory`: games that publish Windows shared memory; read on Linux from `/dev/shm` as copied by
//!   `openrigcue-bridge` inside the game's Proton prefix.
//! - `process_memory`: games read through another process's memory (`/proc/<pid>/mem`).
//! - `api`: the public third-party telemetry API (UDP port 33001) and joystick motion.

pub use openrigcue_model::Telemetry;

mod printed;
mod source;
pub use source::{NativeRead, Source, SourceError};

pub mod api;
/// Per-tick channel derivations of the dispatcher (max speed, DAMS, harmonized gear, ...).
pub mod derive;
pub mod process_memory;
pub mod shared_memory;
pub mod udp_flight;
pub mod udp_racing;
