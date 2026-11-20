//! `XR_APILAYER_OPENRIGCUE_motion_compensation`: a Linux implicit OpenXR API layer that removes the
//! motion rig's movement from what an OpenXR application sees.
//!
//! The daemon publishes the rig pose to the shared rig-pose file (`openrigcue-rigpose`). Per frame
//! the layer predicts that pose to the display time and applies the inverse rig motion, about the
//! calibrated centre of rotation, to:
//! - the eye poses from `xrLocateViews`;
//! - `xrLocateSpace` / `xrLocateSpaces` results between rig-attached spaces (VIEW, controllers)
//!   and room-fixed ones, including velocities;
//! - and, inverted, to the composition layers submitted in `xrEndFrame`, so the compositor's
//!   reprojection sees the real head pose and does not undo the compensation.
//!
//! It passes everything through unchanged while the pose file is missing, stale, switched off, or
//! claimed by the SteamVR driver (which compensates every application on SteamVR itself).
//!
//! The method follows OpenXR-MotionCompensation's per-frame approach; the code is our own.

mod log;

pub mod compensation;
pub mod config;
pub mod motion;

mod ffi;

pub use ffi::{xrNegotiateLoaderApiLayerInterface, LAYER_NAME};

#[cfg(test)]
mod tests;
