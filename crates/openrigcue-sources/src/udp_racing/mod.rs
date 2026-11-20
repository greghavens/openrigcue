//! See the crate documentation for the scope of this module.

// Unit `udp-racing`: console, kart and struct-based UDP listeners.
pub mod assetto_corsa_udp;
pub mod beamng;
pub mod drift_physics;
pub mod epic_roller_coasters;
pub mod forza;
pub mod gran_turismo;
pub mod gtav;
pub mod hyperstorm;
pub mod iron_rebellion;
pub mod kartkraft;
pub mod live_for_speed;
pub mod motogp;
/// Process-wide parameters listeners read or change themselves.
pub mod parameters;
pub mod project_cars;
pub mod project_cars2;
pub mod project_motor_racing;
pub mod rbr;
pub mod red_rover;
mod support;
pub(crate) use support::pymath::{int_text, native_int};
pub(crate) use support::{net, receiver};
pub mod the_crew;
pub mod touring_karts;
/// Calls on which the dispatcher thread ends.
pub mod unhandled;
pub mod vector36;
pub mod wrc2023;
pub mod wreckfest2;

// Unit `udp-codemasters`: Codemasters/EA F1 and extradata listeners, shared socket and forwarding
//.
pub mod codemasters;
pub mod ea_f1;
pub mod forward;
mod listener_output;
pub mod socket;
