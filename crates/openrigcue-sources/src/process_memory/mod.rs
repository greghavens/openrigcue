//! Games read through another process's memory (Elite Dangerous, via `/proc/<pid>/mem`), and the listeners
//! that use local IPC instead of shared memory: HTTP (War Thunder, bus simulators), TCP (NoLimits 2,
//! SimConnect for FSX / Prepar3D / MSFS), JSON over UDP (V-Racer) and Windows named pipes reached through the
//! bridge's relay (Farming Simulator, DynamicCab).
//!
//! `listener_support` and `python` hold what these listeners share; the TrackMania, Space Monkey and Warplanes
//! shared-memory listeners also use its open-or-create mapping rule.

pub mod http;
pub mod listener_support;
pub mod pipe_relay;
pub mod python;

pub mod bus_sim;
pub mod dynamic_cab;
pub mod elite_dangerous;
pub mod farming_simulator;
pub mod no_limits2;
pub mod simconnect;
pub mod vracer;
pub mod war_thunder;
