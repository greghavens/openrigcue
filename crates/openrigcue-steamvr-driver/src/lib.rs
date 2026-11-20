//! OpenRigCue's SteamVR driver for Linux: removes the motion rig's movement from the headset and
//! controller poses inside `vrserver`, so every OpenVR application (and, on SteamVR, every OpenXR
//! application and the compositor's reprojection) sees the cockpit stay put while the rig moves.
//!
//! - [`provider`]: the C++-ABI entry points and the vtable detours (the only FFI code, with
//!   [`hook`] and [`openvr`]).
//! - [`compensate`]: the pose maths: inverse rig motion about the calibrated centre of rotation,
//!   predicted to each pose's time, with the rig-induced velocities removed.
//! - [`rig`] / [`shm`]: the rig pose from the `openrigcue-rigpose` shared-memory contract, the
//!   active-consumer claim, recentre requests, settings reload and the status file.
//!
//!

pub mod compensate;
pub mod config;
pub mod hook;
pub mod math;
pub mod openvr;
pub mod provider;
pub mod rig;
pub mod seqlock;
pub mod shm;

pub use provider::HmdDriverFactory;
