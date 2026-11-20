//! Bass shakers: the tactile effect engine.
//!
//! Telemetry frames go through [`rig::ShakeRig`], which decides per frame which effect
//! samples start or stop and how the engine vibration is driven.
//! [`mixer::Mixer`] plays those decisions into six speaker buses (5.1) using
//! the per-group speaker levels of [`levels::GroupLevels`]. The samples are OpenRigCue's built-in
//! sounds ([`synth`]) unless the user imports their SRS sound files from an installation or
//! installer ([`samples::SampleSource`]).
//! [`system::ShakerSystem`] ties it together; [`channel_map`] places the buses on a device's
//! channels, and [`output`] sends the mix to a sound card through cpal. Module `fmod` plays the
//! FMOD Studio engine events when the user supplies FMOD and their SRS banks; otherwise the
//! built-in engine sound plays.

pub mod channel_map;
pub mod effect_test;
pub mod engine_sound;
pub mod fmod;
pub mod layout;
pub mod levels;
pub mod mixer;
mod native_mono;
pub mod nsis;
pub mod output;
pub mod profiles;
pub mod rig;
pub mod samples;
pub mod suspension;
pub mod synth;
mod synth_data;
pub mod system;
pub mod tables;
pub mod tuning;

pub use engine_sound::EngineVoice;
pub use layout::{Effect, Group, Speaker};
pub use profiles::SpeakerProfile;
pub use rig::{Action, ShakeInput, ShakeRig};
pub use samples::{SampleBank, SampleSource};
pub use system::{FrameState, ShakerSettings, ShakerSystem};
pub use tuning::{ShakeboxConfig, ShakerTuning};
