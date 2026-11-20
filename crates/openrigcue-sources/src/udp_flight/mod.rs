//! Flight-simulator listeners. The game-side files they need (DCS `SRSExport.lua` / `Export.lua`, Condor
//! `UDP.ini`) are shipped by the game-configuration library (`openrigcue-config` `patch::library`).
//!
//! | source | game | transport |
//! |---|---|---|
//! | [`xplane::XPlaneSource`] | X-Plane 10/11/12 | RREF subscription, persistent socket |
//! | [`dcs::DcsSource`] | DCS World | text from our export script |
//! | [`il2::Il2Source`] | IL-2 Great Battles | motion + telemetry device, background thread |
//! | [`condor::CondorSource`] | Condor 2/3 | `key=value` text |
//! | [`silent_wings::SilentWingsSource`] | Silent Wings | binary |
//! | [`aerofly::AeroflySource`] | Aerofly FS 2 | `;`-separated text from a game plugin |
//! | [`aces_high::AcesHighSource`] | Aces High III | `,`-separated text |
//! | [`fly_inside::FlyInsideSource`] | FlyInside | `,`-separated text from a game script |
//!
//! Every source implements [`crate::Source`] and [`FlightSource`] (the rate channels).

pub mod aces_high;
pub mod aerofly;
mod channels;
pub mod condor;
pub mod dcs;
pub mod fly_inside;
pub mod il2;
pub(crate) mod python;
pub mod silent_wings;
pub mod transport;
pub mod xplane;

pub(crate) use channels::{assign, clear_listener_channels};
pub use channels::{AngularRates, FlightSource};
pub use transport::{Decoder, PolledUdpSource, RateDecoder, UdpConfig};
