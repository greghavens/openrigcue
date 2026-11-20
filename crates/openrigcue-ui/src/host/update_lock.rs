//! The reentrant UI update lock, independent of the application's state mutex.

use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Condvar, Mutex};
use std::thread::{self, ThreadId};

#[derive(Default)]
struct State {
    owner: Option<ThreadId>,
    depth: usize,
}

#[derive(Default)]
pub struct UpdateLock {
    state: Mutex<State>,
    released: Condvar,
}

#[must_use = "the update lock is released when its guard is dropped"]
pub struct UpdateGuard<'a> {
    lock: &'a UpdateLock,
    // A reentrant lock must be released by the thread that acquired it.
    _thread: PhantomData<Rc<()>>,
}

impl UpdateLock {
    pub fn lock(&self) -> UpdateGuard<'_> {
        let owner = thread::current().id();
        let mut state = super::lock(&self.state);
        while state.owner.is_some_and(|current| current != owner) {
            state = self.released.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.owner = Some(owner);
        state.depth += 1;
        UpdateGuard { lock: self, _thread: PhantomData }
    }

    #[cfg(test)]
    pub(crate) fn owned_by_current_thread(&self) -> bool {
        super::lock(&self.state).owner == Some(thread::current().id())
    }
}

impl Drop for UpdateGuard<'_> {
    fn drop(&mut self) {
        let mut state = super::lock(&self.lock.state);
        debug_assert_eq!(state.owner, Some(thread::current().id()));
        state.depth -= 1;
        if state.depth == 0 {
            state.owner = None;
            self.lock.released.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    #[test]
    fn nested_update_guards_retain_ownership_until_the_last_release() {
        let gate = Arc::new(UpdateLock::default());
        let outer = gate.lock();
        let inner = gate.lock();
        assert!(gate.owned_by_current_thread());
        let worker_gate = gate.clone();
        let (ready, waiting) = mpsc::channel();
        let (send, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready.send(()).unwrap();
            let _guard = worker_gate.lock();
            send.send(()).unwrap();
        });
        waiting.recv().unwrap();
        drop(inner);
        assert!(received.recv_timeout(Duration::from_millis(30)).is_err());
        drop(outer);
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(!gate.owned_by_current_thread());
    }
}
