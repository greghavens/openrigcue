//! The OpenRigCue rig-pose contract.
//!
//! The daemon publishes the motion rig's pose to a small fixed-layout file in `/dev/shm`
//! ([`DEFAULT_PATH`], overridable with `$OPENRIGCUE_RIG_POSE`); the VR motion-compensation consumers
//! (the OpenXR API layer and the SteamVR driver) read it without ever blocking the writer.
//!
//! - [`layout`]: the byte layout (little-endian, magic, version, seqlock sequence, payload,
//!   active-consumer field).
//! - [`RigPoseWriter`] / [`RigPoseReader`]: publishing and lock-free consistent reads; the
//!   active-consumer claim that keeps two consumers from compensating the same frames twice.
//! - [`RigSample`] / [`RigValues`]: the six values with their order and units and the axis convention
//!   ([`RigValues::transform`]) every consumer must use.
//! - [`RateEstimator`]: rates for producers that only measure positions, for prediction.
//! - [`math`]: the small rigid-body toolkit the consumers share.

pub mod layout;
pub mod math;

mod clock;
mod file;
mod rates;
mod sample;

pub use clock::monotonic_now_ns;
pub use file::{
    default_path, may_claim, ClaimOutcome, ConsumerClaim, ConsumerKind, RigPoseError, RigPoseReader, RigPoseWriter,
    Snapshot, CLAIM_TIMEOUT_NS, DEFAULT_PATH, PATH_ENV,
};
pub use rates::RateEstimator;
pub use sample::{CorOffset, RigSample, RigValues, Unusable, AXES};
