//! The control connection to OpenRigCue (see [`crate::control`]): it carries the application's own
//! mapping objects, and while it is up the application counts as running (the pipe servers wait for it).

use std::net::{Ipv4Addr, Shutdown, TcpStream};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::events::NativeEvents;
use super::sections::Prefix;
use super::Shared;
use crate::control::{read_message, write_message, Holds, Request, CONTROL_ROUTE};
use crate::event::{consumer::Consumer, EventRelay, EventWait};
use crate::relay::write_frame;

/// Pause between connection attempts while OpenRigCue is not running.
const RECONNECT: Duration = Duration::from_millis(100);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether OpenRigCue is running, and whether its first burst of objects has arrived.
#[derive(Default)]
pub struct AppState {
    state: Mutex<Status>,
    changed: Condvar,
}

#[derive(Default, Clone, Copy)]
struct Status {
    running: bool,
    synced: bool,
    /// The first connection attempt has been made (it connected or was refused).
    attempted: bool,
}

impl AppState {
    pub fn is_running(&self) -> bool {
        lock(&self.state).running
    }

    /// Block until the application runs.
    pub fn wait_running(&self) {
        let mut state = lock(&self.state);
        while !state.running {
            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Wait for the first connection attempt (at most `timeout`) and tell whether it found the application
    /// running. A refused connection answers at once, so a game started while OpenRigCue does not run is not held.
    pub fn first_attempt_found_running(&self, timeout: Duration) -> bool {
        self.wait_for(timeout, |state| state.attempted) && self.is_running()
    }

    /// Wait up to `timeout` for the objects held when the connection was made (`S`).
    pub fn wait_synced(&self, timeout: Duration) -> bool {
        self.wait_for(timeout, |state| state.synced)
    }

    fn wait_for(&self, timeout: Duration, done: impl Fn(&Status) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.state);
        while !done(&state) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self.changed.wait_timeout(state, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }

    fn set(&self, running: bool, synced: bool) {
        *lock(&self.state) = Status { running, synced, attempted: true };
        self.changed.notify_all();
    }
}

/// Keep the control connection up for the rest of the run, reconnecting while OpenRigCue is not running.
/// `on_stop` runs whenever the application stops (the pipe servers' instances are woken to close).
pub fn spawn(shared: Arc<Mutex<Shared>>, app: Arc<AppState>, tcp_port: u16, on_stop: fn()) {
    let spawned = thread::Builder::new().name("control".into()).spawn(move || {
        let mut reported = false;
        loop {
            match connect(tcp_port) {
                Ok(stream) => {
                    tracing::info!("OpenRigCue connected");
                    reported = false;
                    serve(stream, &shared, &app);
                    tracing::info!("OpenRigCue disconnected: its sections are released and its pipes closed");
                    app.set(false, false);
                    on_stop();
                }
                Err(error) => {
                    if !reported {
                        tracing::info!(%error, "OpenRigCue not reachable, retrying");
                        reported = true;
                        app.set(false, false);
                    }
                }
            }
            thread::sleep(RECONNECT);
        }
    });
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot start the control connection");
    }
}

fn connect(tcp_port: u16) -> std::io::Result<TcpStream> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, tcp_port))?;
    stream.set_nodelay(true)?;
    write_frame(&mut stream, CONTROL_ROUTE.as_bytes())?;
    Ok(stream)
}

/// Carry out OpenRigCue's requests until the connection ends, then release every hold.
fn serve(stream: TcpStream, shared: &Mutex<Shared>, app: &AppState) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = stream;
    let mut holds = Holds::default();
    let mut events = Consumer::new(NativeEvents);
    let mut event_relay = EventRelay::start("Local\\IRSDKDataValidEvent", &mut lock(shared).dir);
    app.set(true, false);
    while let Ok(Some(message)) = read_message(&mut reader) {
        let request = match Request::decode(&message) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "control connection closed");
                break;
            }
        };
        if request == Request::Synced {
            app.set(true, true);
            continue;
        }
        let answer = if matches!(request, Request::OpenEvent { .. } | Request::WaitEvent { .. }) {
            let answer = crate::control::apply_event(&request, &mut events, || {
                // The listener copies after the wait even when the wait timed
                // out. Publish that fresh snapshot before acknowledging.
                let mut shared = lock(shared);
                let Shared { sections, mirror, dir } = &mut *shared;
                mirror.refresh("Local\\IRSDKMemMapFileName", sections, dir);
            });
            if let Some(reply) = &answer {
                let result = u32::from_le_bytes(reply[17..21].try_into().expect("event reply"));
                let observation = if result == 0 {
                    EventWait::Signalled
                } else if events.has_handles() {
                    EventWait::NotSignalled
                } else {
                    EventWait::Unavailable
                };
                event_relay.observe(observation, &mut lock(shared).dir);
            }
            answer
        } else {
            let mut shared = lock(shared);
            let Shared { sections, mirror, dir } = &mut *shared;
            holds.apply(request, &mut Prefix { sections, mirror, dir })
        };
        if let Some(answer) = answer {
            if write_message(&mut writer, &answer).is_err() {
                break;
            }
        }
    }
    let _ = reader.shutdown(Shutdown::Both);
    // Event handles last for the whole control session; a listener shutdown
    // never closes them. They are released when the session ends.
    drop(events);
    let mut shared = lock(shared);
    let Shared { sections, mirror, dir } = &mut *shared;
    holds.release_all(&mut Prefix { sections, mirror, dir });
    event_relay.observe(EventWait::Unavailable, dir);
}
