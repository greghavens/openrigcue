//! Opening and waiting on a game's named event (OpenEventW/WaitForSingleObject).
//! The creator decides reset behavior; this consumer neither creates nor resets the event.

pub const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
pub const WAIT_FAILED: u32 = u32::MAX;

pub trait EventApi {
    type Handle;
    fn open(&mut self, name: &str, access: u32, inherit: bool) -> Option<Self::Handle>;
    fn wait(&mut self, handle: &Self::Handle, milliseconds: u32) -> u32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitResult {
    pub token: u64,
    pub result: u32,
}

/// Keeps every opened handle for this app control session without closing any;
/// the kernel decides what each requested wait consumes.
pub struct Consumer<A: EventApi> {
    api: A,
    handles: Vec<A::Handle>,
}

impl<A: EventApi> Consumer<A> {
    pub fn new(api: A) -> Self {
        Self { api, handles: Vec::new() }
    }
    pub fn has_handles(&self) -> bool {
        !self.handles.is_empty()
    }

    pub fn open_and_wait(&mut self, name: &str) -> WaitResult {
        let Some(handle) = self.api.open(name, SYNCHRONIZE_ACCESS, false) else {
            return WaitResult { token: 0, result: WAIT_FAILED };
        };
        self.handles.push(handle);
        self.wait(self.handles.len() as u64)
    }

    pub fn wait(&mut self, token: u64) -> WaitResult {
        let handle = token
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.handles.get(index));
        let result = handle.map_or(WAIT_FAILED, |handle| self.api.wait(handle, super::DATA_VALID_WAIT_MS));
        WaitResult { token, result }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<String>>>;

    struct Handle(Arc<Mutex<Vec<String>>>);
    impl Drop for Handle {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("close".into());
        }
    }
    struct Api {
        calls: Arc<Mutex<Vec<String>>>,
        opens: VecDeque<bool>,
        waits: VecDeque<u32>,
    }
    impl EventApi for Api {
        type Handle = Handle;
        fn open(&mut self, name: &str, access: u32, inherit: bool) -> Option<Handle> {
            self.calls.lock().unwrap().push(format!("open {name} {access:x} {inherit}"));
            self.opens.pop_front().unwrap_or(true).then(|| Handle(self.calls.clone()))
        }
        fn wait(&mut self, _: &Handle, ms: u32) -> u32 {
            self.calls.lock().unwrap().push(format!("wait {ms}"));
            self.waits.pop_front().expect("scripted wait")
        }
    }
    fn consumer(opens: &[bool], waits: &[u32]) -> (Consumer<Api>, Calls) {
        let calls = Arc::default();
        (
            Consumer::new(Api {
                calls: Arc::clone(&calls),
                opens: opens.iter().copied().collect(),
                waits: waits.iter().copied().collect(),
            }),
            calls,
        )
    }

    #[test]
    fn current_readiness_is_delegated_to_each_native_wait_without_assuming_reset_type() {
        // Both legal Win32 traces are supported; neither asserts the game's
        // unknown creator mode. No background wait can consume a signal.
        for trace in [[0, 0, 0], [0, 258, 0]] {
            let (mut consumer, calls) = consumer(&[true], &trace);
            let opened = consumer.open_and_wait("Local\\IRSDKDataValidEvent");
            assert_eq!(opened, WaitResult { token: 1, result: trace[0] });
            assert_eq!(consumer.wait(1).result, trace[1]);
            assert_eq!(consumer.wait(1).result, trace[2]);
            assert_eq!(
                *calls.lock().unwrap(),
                ["open Local\\IRSDKDataValidEvent 100000 false", "wait 32", "wait 32", "wait 32"]
            );
        }
    }

    #[test]
    fn each_startup_retries_open_and_failed_open_does_not_wait() {
        let (mut consumer, calls) = consumer(&[false, true, true], &[258, WAIT_FAILED]);
        assert_eq!(consumer.open_and_wait("event"), WaitResult { token: 0, result: WAIT_FAILED });
        assert_eq!(consumer.open_and_wait("event"), WaitResult { token: 1, result: 258 });
        assert_eq!(consumer.open_and_wait("event"), WaitResult { token: 2, result: WAIT_FAILED });
        assert!(!calls.lock().unwrap().iter().any(|call| call == "close"));
        drop(consumer);
        assert_eq!(calls.lock().unwrap().iter().filter(|call| *call == "close").count(), 2);
    }

    #[test]
    fn unknown_handle_fails_without_touching_native_wait() {
        let (mut consumer, calls) = consumer(&[], &[]);
        assert_eq!(consumer.wait(0).result, WAIT_FAILED);
        assert_eq!(consumer.wait(u64::MAX).result, WAIT_FAILED);
        assert!(calls.lock().unwrap().is_empty());
    }
}
