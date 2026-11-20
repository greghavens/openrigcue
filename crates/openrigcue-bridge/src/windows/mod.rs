//! The Win32 side of the bridge: section access, the control connection, pipe servers, relay sockets, launching
//! the game.
//!
//! Calling Win32 requires `unsafe` FFI, which the rest of the workspace forbids; it is allowed in this module
//! tree only. Every `unsafe` block wraps a single documented API call or a copy between a mapped view and an
//! owned buffer, behind a safe type that owns the handle or view.
#![allow(unsafe_code)]

mod control;
mod events;
mod launch;
mod pipes;
mod sections;
mod socket;
mod stop;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::Options;
use crate::event::EventRelay;
use crate::export::Exports;
use crate::mirror::{Mirror, MirrorDir};
use crate::paths::{wine_path, MIRROR_DIR};
use crate::plan::{every_section_name, Plan};
use crate::relay::RELAY_TCP_PORT;

/// Exit code when the program cannot be started.
const EXIT_LAUNCH_FAILED: i32 = 2;
/// Upper bound for the first control-connection attempt (a refused localhost connection answers at once).
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
/// How long the game's start then waits for the sections OpenRigCue already holds.
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);
/// Time between two mirror passes (100 copies per second).
const MIRROR_PERIOD: Duration = Duration::from_millis(10);

/// The mirror, its directory and the prefix's sections, shared by the mirroring loop, the control connection
/// and the event waiters.
pub(crate) struct Shared {
    sections: sections::LiveSections,
    mirror: Mirror,
    dir: MirrorDir,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run the bridge until the launched program has finished (or until interrupted when nothing was launched).
/// Returns the process exit code: the launched program's, or 0.
pub fn run(options: Options) -> i32 {
    stop::install_handler();
    let plan = Plan::watch_all();

    let mirror_dir = wine_path(MIRROR_DIR);
    let mut dir = MirrorDir::new(&mirror_dir);
    let mut mirror = Mirror::new(plan.mirrors.clone());
    mirror.prepare(&mut dir, &every_section_name());
    // Preserve event diagnostics without opening or consuming the kernel event.
    for event in plan.mirrors.iter().filter_map(|target| target.ready_event) {
        EventRelay::start(event, &mut dir);
    }
    let shared = Arc::new(Mutex::new(Shared { sections: sections::LiveSections::default(), mirror, dir }));

    let app = Arc::new(control::AppState::default());
    control::spawn(Arc::clone(&shared), Arc::clone(&app), RELAY_TCP_PORT, pipes::wake_listening);
    pipes::spawn_servers(&plan.pipes, socket::Upstream::from_env(RELAY_TCP_PORT), Arc::clone(&app));
    // The held sections exist before the game starts when the application runs; when it does
    // not run, the game starts at once.
    if app.first_attempt_found_running(CONNECT_TIMEOUT) && !app.wait_synced(SYNC_TIMEOUT) {
        tracing::warn!("OpenRigCue's sections did not all arrive before the game started");
    }

    let mut launched = match options.command.split_first() {
        Some((program, args)) => match launch::Launched::start(program, args) {
            Ok(launched) => Some(launched),
            Err(error) => {
                tracing::error!(program, %error, "cannot start program");
                return EXIT_LAUNCH_FAILED;
            }
        },
        None => None,
    };

    let mut exports = Exports::new(&mirror_dir, &plan.exports);
    let period = MIRROR_PERIOD;
    let mut publisher = sections::OwnedSections::default();

    let exit_code = loop {
        let started = Instant::now();
        {
            let mut shared = lock(&shared);
            let Shared { sections, mirror, dir } = &mut *shared;
            mirror.tick(sections, dir);
        }
        exports.tick(&mut publisher);
        if stop::requested() {
            tracing::info!("interrupted");
            break 0;
        }
        if let Some(code) = launched.as_mut().and_then(launch::Launched::finished) {
            tracing::info!(code, "program finished");
            break code;
        }
        if let Some(rest) = period.checked_sub(started.elapsed()) {
            thread::sleep(rest);
        }
    };
    {
        let mut shared = lock(&shared);
        let Shared { sections, mirror, dir } = &mut *shared;
        mirror.finish(sections, dir);
    }
    exports.finish(&mut publisher);
    exit_code
}

/// NUL-terminated UTF-16 for `*W` APIs.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
