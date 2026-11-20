//! Motion cueing: telemetry to one position per output slot, and the pacing of the motion loop.
//!
//! Per frame ([`MotionCueing::send_stream`]): soft start/stop and gear smoothing state, per-axis clip and
//! normalisation, per-slot shares, weights, deadzone caps and moving-average smoothing, the signed mix,
//! the ±100 % map onto the actuator range, travel scaling, rate shaping against the last delivered
//! frame, and the wheel/event effects. [`MotionCueing::test`] is the slider-driven test mode;
//! [`loop_period`] and [`test_period`] pace the loops.
//!
//! The configuration and the game tune hold dynamically typed values that are converted at the point
//! of use ([`ConfigView`], [`GameConfigView`]); a failed lookup or conversion aborts the frame with
//! an error that carries its kind, text and call trace ([`CueError`]). Time is passed in and the effect
//! randomness is injectable for deterministic fixtures. Production starts independent smoothing
//! workers with live inputs using [`MotionCueing::start_smoothing_workers`].

mod axis;
mod cueing;
mod effects;
mod error;
mod external;
mod mixing;
mod platform;
mod pyint;
mod pymath;
mod pyvalue;
mod settings;
mod shaping;
mod smoothing;
mod test_mode;
mod timers;
mod timing;
mod tune;
mod views;
mod worker_smoothing;

pub use axis::{Axis, PerAxis};
pub use cueing::{GameKind, Listener, MotionCueing, NativePositions, Positions, StreamContext, StreamOutput};
pub use effects::{python_random, DefaultRng, EffectRng, SharedRandom};
pub use error::{CueError, DivisionKind, Exception, Frame, REFRESH_MOTIONBOX_FRAME};
pub use external::{ExternalMixing, ExternalMixingError, DEADZONE_FILE, DIRECTION_FILE, PERCENTAGES_FILE};
pub use platform::{
    builtin, builtin_ids, motion_model, motion_models, Manufacturer, MotionConfig, MotionModel, Platform,
    PlatformError, Sign, SlotMix, Wheel, WheelLayout, WheelSlots, SLOTS,
};
pub use pyint::{PyInt, TableInt};
pub use pyvalue::{str2bool, PyValue};
pub use settings::{
    ClipFactors, LowSpeedSmoothing, ManufacturerSettings, MotionSettings, SliderMap, TractionLossAutoSmoothing,
};
pub use test_mode::{test_action, MotionTestSnapshot, TestAction, TestContext, TestSliders};
pub use timing::{
    capture_motion_arguments, clear_telemetry_nan, clear_telemetry_nan_checked, loop_period, loop_period_from_config,
    test_period, test_period_from_config, HostOs, IDLE_TEST_PERIOD,
};
pub use tune::{Effects, MainActuatorShares, MotionTune, RotationInput, TelemetryMaxima};
pub use views::{ConfigView, GameConfigView, Missing, NativeConfig, RawConfig, RawGameConfig};
pub use worker_smoothing::SmoothingInputs;

#[cfg(test)]
mod native_runtime_tests;
