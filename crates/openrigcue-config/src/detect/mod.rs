//! Game detection: which catalog game is running.
//!
//! A snapshot of window titles and process names is taken every [`SNAPSHOT_PERIOD`] ([`Snapshotter`]); on every
//! dispatcher tick without a delivering listener, [`running_games`] lists the catalog games present in the latest
//! snapshot, in catalog order. The dispatcher (`openrigcue_sources::derive::Dispatcher`) then tries their listeners in
//! that order, keeps the first that delivers until it stops, and falls back to the API and joystick listeners
//! when no catalog game is found.
//!
//! Linux: process names come from `/proc` (Proton processes carry their Windows executable path in `argv[0]`);
//! window titles come from the X server, which covers Proton games
//! on X11 and on Wayland (through XWayland).

mod game;
mod processes;
mod scan;
mod snapshot;
mod x11;

pub use game::{TitleMatch, WindowRule};
pub use processes::running_processes;
pub use scan::running_games;
pub use snapshot::{NoWindowTitles, Snapshotter, SystemSnapshot, WindowTitles, SNAPSHOT_PERIOD};
pub use x11::{WindowActivationError, X11WindowTitles};
