//! Minimal stderr logging (Proton writes the game's stderr to its log). Only state changes are logged
//! at the default level, never per frame.

use std::sync::atomic::{AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(1);

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn enabled(level: u8) -> bool {
    level <= LEVEL.load(Ordering::Relaxed)
}

/// Writes one line to stderr, ignoring every error. `eprintln!` panics when stderr fails (e.g. EPIPE
/// once the reader of Proton's log pipe is gone), and a panic escaping an `extern "system"` entry point
/// aborts the game, so logging must never panic.
pub fn write(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "[openrigcue-xr] {args}");
}

macro_rules! info {
    ($($arg:tt)*) => {
        if $crate::log::enabled(1) {
            $crate::log::write(format_args!($($arg)*));
        }
    };
}

macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::log::enabled(2) {
            $crate::log::write(format_args!($($arg)*));
        }
    };
}

pub(crate) use {debug, info};
