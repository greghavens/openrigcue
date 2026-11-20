//! The application's own inputs: the public telemetry API (UDP port 33001) and joystick motion.
//!
//! Neither needs a running game: the dispatcher tries them, API first, whenever no game is detected.

pub mod joystick;
pub mod listener;
pub mod packet;

pub use joystick::{
    ControllerView, JoystickEvent, JoystickEvents, JoystickInputs, JoystickOptions, JoystickProfile, JoystickReader,
    JoystickSource, JoystickTiming, StoredJoystickEntry,
};
pub use listener::{ApiDecoder, ApiPacket, ApiSource};
