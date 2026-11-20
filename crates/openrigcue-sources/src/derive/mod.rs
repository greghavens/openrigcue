//! The telemetry dispatcher: picks the delivering listener, derives the
//! dispatcher-owned channels every tick (speed, top speed, steering range, DAMS slip angles, yaw/heading,
//! pitch/roll source, harmonized gear, shift light), loads every other channel through the registry rules, and
//! keeps the session bookkeeping (stand-by reset, min/max table, chart history, Intellitune capture, export
//! table, configuration-refresh trigger, loop pacing).

mod attitude;
mod channels;
mod dams;
mod dispatcher;
mod export;
mod history;
mod intellitune;
mod listener;
mod max_speed;
mod min_max;
mod pacing;
mod pyfloat;
mod steering;
mod test_pattern;
#[cfg(test)]
mod testing;

pub use attitude::{heading, pitch, roll, AttitudeSource, Heading};
pub use channels::{channel_spec, ChannelSpec, CHANNEL_SPECS};
pub use dams::{front_traction_loss, rear_traction_loss, DamsMode, DamsSettings, SlipInputs};
pub use dispatcher::{
    DispatchEvent, DispatchPublication, DispatchSettings, Dispatcher, GameDirectory, ListenerActivity, ProfileSettings,
    RuntimeInputs, ScanHit, TickInput, TickOutcome,
};
pub use export::{export_rows, live_table_rows, ExportRow, LiveTableRow, SpeedUnit, EXPORT_HEADER};
pub use history::TelemetryHistory;
pub use intellitune::{IntellituneHandle, IntellituneReference, IntellituneSamples};
pub use listener::{FailurePoint, Listener, Rate, SourceListener};
pub use max_speed::{MaxSpeedTracker, SpeedSample};
pub use min_max::{Extremes, MinMaxError, MinMaxHandle, MinMaxTable, NativeExtremes};
pub use pacing::{tick_period, RefreshRates};
pub use steering::{steering_range, wind_steering_angle};
pub use test_pattern::{TestPattern, TEST_PATTERN_PERIOD};
