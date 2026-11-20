//! On-demand native event waits and diagnostic event observations.
//!
//! The iRacing listener decides the connection state with `Local\IRSDKDataValidEvent`: start-up fails when
//! `OpenEventW` fails or a 32 ms `WaitForSingleObject` does not return `WAIT_OBJECT_0`, and staying connected
//! requires the event handle. Linux requests those native operations through the
//! control connection. The bridge also retains diagnostic observations next to
//! the section mirror (`/dev/shm/openrigcue/Local_IRSDKDataValidEvent`):
//!
//! | bytes | content |
//! |---|---|
//! | 0..8 | `signals`: little-endian `u64`, incremented after every wait that returned `WAIT_OBJECT_0`, written after the section copy that followed that wait |
//! | 8 | `open`: 1 while the bridge holds an open handle to the event, else 0 |
//! | 9..16 | zero |
//!
//! `signals` never decreases across runs. This file records requested waits;
//! product readiness uses the actual native wait result over the control route,
//! not a future change in this diagnostic counter.

pub mod consumer;

use crate::mirror::MirrorDir;

/// Length of an event file.
pub const EVENT_FILE_LEN: usize = 16;

/// Timeout of one wait on the data-valid event, in milliseconds.
pub const DATA_VALID_WAIT_MS: u32 = 32;

/// The published state of one event (its file next to the mirrors).
pub struct EventRelay {
    name: &'static str,
    state: EventState,
}

impl EventRelay {
    /// Start a run: the count continues from the previous run's file, with no handle yet.
    pub fn start(name: &'static str, dir: &mut MirrorDir) -> EventRelay {
        let previous = dir.last_copy(name).ok().flatten();
        let relay = EventRelay { name, state: EventState::resumed_from(previous.as_deref()) };
        relay.publish(dir);
        relay
    }

    /// Record one wait; the file is rewritten when the state changed.
    pub fn observe(&mut self, wait: EventWait, dir: &mut MirrorDir) {
        let next = self.state.after(wait);
        if next != self.state {
            self.state = next;
            self.publish(dir);
        }
    }

    fn publish(&self, dir: &mut MirrorDir) {
        if let Err(error) = dir.publish(self.name, &self.state.encode()) {
            tracing::warn!(event = self.name, %error, "write event file failed");
        }
    }
}

/// Outcome of one wait on a data-ready event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventWait {
    /// `OpenEventW` failed: the game has not created the event.
    Unavailable,
    /// `WaitForSingleObject` returned `WAIT_OBJECT_0`.
    Signalled,
    /// Any other result (timeout, failure), which counts as no valid data.
    NotSignalled,
}

/// What the bridge publishes about one event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventState {
    pub open: bool,
    pub signals: u64,
}

impl EventState {
    /// The state after one more wait.
    pub fn after(self, wait: EventWait) -> EventState {
        match wait {
            EventWait::Unavailable => EventState { open: false, ..self },
            EventWait::NotSignalled => EventState { open: true, ..self },
            EventWait::Signalled => EventState { open: true, signals: self.signals.wrapping_add(1) },
        }
    }

    /// The state a new run starts from: the signal count of the previous run (so it never goes backwards) and
    /// no handle yet.
    pub fn resumed_from(previous: Option<&[u8]>) -> EventState {
        let signals = previous.and_then(EventState::decode).map_or(0, |state| state.signals);
        EventState { open: false, signals }
    }

    pub fn encode(&self) -> [u8; EVENT_FILE_LEN] {
        let mut bytes = [0; EVENT_FILE_LEN];
        bytes[..8].copy_from_slice(&self.signals.to_le_bytes());
        bytes[8] = u8::from(self.open);
        bytes
    }

    /// Parse an event file; `None` when it is shorter than [`EVENT_FILE_LEN`].
    pub fn decode(bytes: &[u8]) -> Option<EventState> {
        let head = bytes.get(..EVENT_FILE_LEN)?;
        let mut count = [0; 8];
        count.copy_from_slice(&head[..8]);
        Some(EventState { open: head[8] != 0, signals: u64::from_le_bytes(count) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only WAIT_OBJECT_0 counts as a signal; a failed OpenEventW clears `open`.
    #[test]
    fn waits_update_the_state() {
        let start = EventState::default();
        let opened = start.after(EventWait::NotSignalled);
        assert_eq!(opened, EventState { open: true, signals: 0 });
        let signalled = opened.after(EventWait::Signalled).after(EventWait::Signalled);
        assert_eq!(signalled, EventState { open: true, signals: 2 });
        assert_eq!(signalled.after(EventWait::Unavailable), EventState { open: false, signals: 2 });
    }

    #[test]
    fn encoding_round_trips() {
        let state = EventState { open: true, signals: 0x0102_0304_0506_0708 };
        let bytes = state.encode();
        assert_eq!(bytes, [8, 7, 6, 5, 4, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(EventState::decode(&bytes), Some(state));
        assert_eq!(EventState::decode(&bytes[..15]), None);
    }

    /// Start-up and per-frame waits: the event file says whether OpenEventW worked
    /// and counts WAIT_OBJECT_0 results; a timed-out wait leaves the count unchanged; the next run resumes the count
    /// with no handle yet.
    #[test]
    fn relay_publishes_the_state() {
        let root = std::env::temp_dir().join(format!("openrigcue-bridge-event-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut dir = MirrorDir::new(&root);
        let file = || EventState::decode(&std::fs::read(root.join("Local_IRSDKDataValidEvent")).unwrap()).unwrap();
        let mut relay = EventRelay::start("Local\\IRSDKDataValidEvent", &mut dir);
        assert_eq!(file(), EventState { open: false, signals: 0 });
        relay.observe(EventWait::NotSignalled, &mut dir);
        assert_eq!(file(), EventState { open: true, signals: 0 });
        relay.observe(EventWait::Signalled, &mut dir);
        assert_eq!(file(), EventState { open: true, signals: 1 });
        relay.observe(EventWait::Unavailable, &mut dir);
        assert_eq!(file(), EventState { open: false, signals: 1 });
        EventRelay::start("Local\\IRSDKDataValidEvent", &mut MirrorDir::new(&root));
        assert_eq!(file(), EventState { open: false, signals: 1 });
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A new run continues the count (a reader comparing counts never sees a spurious change) without a handle.
    #[test]
    fn new_run_resumes_the_count_closed() {
        let previous = EventState { open: true, signals: 41 }.encode();
        assert_eq!(EventState::resumed_from(Some(&previous)), EventState { open: false, signals: 41 });
        assert_eq!(EventState::resumed_from(None), EventState::default());
        assert_eq!(EventState::resumed_from(Some(&[1, 2])), EventState::default());
    }
}
