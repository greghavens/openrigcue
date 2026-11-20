//! The data-valid event's Windows operations, relayed on demand over the existing
//! application control connection. Counters cannot reproduce current Win32
//! readiness, and an eager bridge waiter would consume auto-reset events.

use std::sync::Arc;
use std::time::Duration;

use super::super::held::{Registry, RemoteEvent};
use super::DATA_VALID_EVENT;

/// Native WaitForSingleObject timeout; the control proxy adds transport latency.
pub const DATA_VALID_TIMEOUT: Duration = Duration::from_millis(32);

pub trait DataValidEvent: Send {
    fn open_and_wait(&mut self) -> bool;
    fn wait(&mut self) -> bool;
    /// Shutdown forgets the handle value without closing the handle.
    fn forget(&mut self) {}
}

pub struct BridgeEvent {
    registry: Arc<Registry>,
    current: Option<RemoteEvent>,
}

impl BridgeEvent {
    pub fn new() -> Self {
        Self { registry: Registry::global(), current: None }
    }
}

impl Default for BridgeEvent {
    fn default() -> Self {
        Self::new()
    }
}

impl DataValidEvent for BridgeEvent {
    fn open_and_wait(&mut self) -> bool {
        // A new OpenEventW is performed on every startup attempt. Timeout or
        // failure forgets the logical handle, leaving the Windows handle open.
        let reply = self.registry.open_event(DATA_VALID_EVENT);
        self.current = reply.filter(|reply| reply.result == 0).and_then(|reply| reply.handle);
        self.current.is_some()
    }

    fn wait(&mut self) -> bool {
        // The wait succeeds with no handle (file mode).
        // A real opened handle succeeds only for WAIT_OBJECT_0.
        self.current.is_none_or(|handle| self.registry.wait_event(handle).is_some_and(|reply| reply.result == 0))
    }

    fn forget(&mut self) {
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::held::event_metadata::Bridge;

    fn endpoint() -> (BridgeEvent, Bridge) {
        let registry = Registry::new("/nonexistent/openrigcue-event-metadata-fixture");
        let bridge = Bridge::connect(&registry);
        (BridgeEvent { registry, current: None }, bridge)
    }

    #[test]
    fn current_signaled_event_succeeds_without_a_future_counter_change() {
        let (mut event, bridge) = endpoint();
        let callback = std::thread::spawn(move || {
            let opened = event.open_and_wait();
            let waited = event.wait();
            event.forget();
            (opened, waited, event.wait())
        });
        let open = bridge.next();
        assert_eq!(open[0], b'E');
        assert_eq!(&open[11..], DATA_VALID_EVENT.as_bytes());
        bridge.reply(&open, 7, 0);
        let wait = bridge.next();
        assert_eq!(wait[0], b'W');
        assert_eq!(&wait[9..], &7u64.to_le_bytes());
        bridge.reply(&wait, 7, 0);
        assert_eq!(callback.join().unwrap(), (true, true, true));
        // Forgetting the handle value sends neither CloseHandle nor a reset.
        assert!(bridge.try_next().is_none());
    }

    #[test]
    fn startup_retries_open_and_accepts_only_wait_object_zero() {
        let (mut event, bridge) = endpoint();
        let callback = std::thread::spawn(move || {
            let outcomes = (0..4).map(|_| event.open_and_wait()).collect::<Vec<_>>();
            (outcomes, event.wait())
        });
        for (token, result) in [(0, u32::MAX), (1, 258), (2, u32::MAX), (3, 0)] {
            let open = bridge.next();
            assert_eq!(open[0], b'E');
            bridge.reply(&open, token, result);
        }
        let wait = bridge.next();
        assert_eq!(wait[0], b'W');
        assert_eq!(&wait[9..], &3u64.to_le_bytes());
        bridge.reply(&wait, 3, 258);
        assert_eq!(callback.join().unwrap(), (vec![false, false, false, true], false));
    }

    #[test]
    fn event_handle_is_not_rebound_to_a_different_prefix_after_disconnect() {
        let (mut event, bridge) = endpoint();
        let callback = std::thread::spawn(move || {
            assert!(event.open_and_wait());
            event
        });
        let open = bridge.next();
        bridge.reply(&open, 4, 0);
        let mut event = callback.join().unwrap();
        drop(bridge);
        let replacement = Bridge::connect(&event.registry);
        assert!(!event.wait(), "a token from the old prefix cannot target its replacement");
        assert!(replacement.try_next().is_none());
        event.forget();
        assert!(event.wait());
    }

    #[test]
    fn no_reachable_prefix_is_an_open_failure_without_starting_a_worker() {
        let registry = Registry::new("/nonexistent/openrigcue-event-metadata-fixture");
        let mut event = BridgeEvent { registry, current: None };
        assert!(!event.open_and_wait());
        assert!(event.wait(), "failed startup forgets the SDK handle");
    }
}
