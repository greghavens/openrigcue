//! Independent timer callbacks. The shared application-state mutex
//! protects the application's values; only callbacks marked as needing it take the
//! UI update lock before acquiring that state mutex.

use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Instant;

use super::MyApp;
use crate::host::lock;

#[derive(Default)]
pub(super) struct Wake {
    state: Mutex<WakeState>,
    changed: Condvar,
}

#[derive(Default)]
struct WakeState {
    cancelled: bool,
    dispatched: bool,
}

impl Wake {
    /// Cancellation can prevent dispatch, but cannot retract an entered callback.
    pub(super) fn cancel(&self) -> bool {
        let mut state = lock(&self.state);
        if state.dispatched {
            return false;
        }
        state.cancelled = true;
        self.changed.notify_all();
        true
    }

    fn dispatch(&self) -> bool {
        let mut state = lock(&self.state);
        if state.cancelled || state.dispatched {
            return false;
        }
        state.dispatched = true;
        self.changed.notify_all();
        true
    }

    fn wait(&self, due: Instant) -> bool {
        let mut state = lock(&self.state);
        while !state.cancelled {
            let delay = due.saturating_duration_since(Instant::now());
            if delay.is_zero() {
                return true;
            }
            (state, _) = self.changed.wait_timeout(state, delay).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        false
    }
}

struct Job {
    app: Arc<Mutex<MyApp>>,
    id: u64,
    due: Instant,
    wake: Arc<Wake>,
    update_lock: Option<Arc<crate::host::UpdateLock>>,
}

impl Job {
    fn run(self) {
        if self.wake.wait(self.due) {
            self.expire();
        }
    }

    fn expire(self) {
        // The timer checks its cancellation event before entering the
        // callback. The callback's own update lock is acquired afterward.
        if !self.wake.dispatch() {
            return;
        }
        // Only callbacks marked as needing the update lock use
        // this gate. Acquire it before touching the application state.
        let _update_guard = self.update_lock.as_ref().map(|gate| gate.lock());
        let mut app = lock(&self.app);
        let Some(index) = app.timers.iter().position(|timer| timer.id == self.id) else { return };
        let timer = app.timers.remove(index);
        (timer.run)(&mut app);
    }
}

pub(super) fn start(
    app: Weak<Mutex<MyApp>>,
    id: u64,
    due: Instant,
    wake: Arc<Wake>,
    update_lock: Option<Arc<crate::host::UpdateLock>>,
) {
    // A bound Timer callback retains its application until completion/cancel.
    let Some(app) = app.upgrade() else { return };
    let job = Job { app, id, due, wake, update_lock };
    #[cfg(test)]
    if tests::capture(&job) {
        return;
    }
    if let Err(error) = std::thread::Builder::new().name("Timer".into()).spawn(move || job.run()) {
        tracing::error!(%error, "timer thread not started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{loaded, FakeBackend};
    use crate::host::UiApp;

    use std::cell::RefCell;
    use std::time::Duration;

    thread_local! {
        static STARTED: RefCell<Option<Vec<Job>>> = const { RefCell::new(None) };
    }

    pub(super) fn capture(job: &Job) -> bool {
        STARTED.with_borrow_mut(|started| match started {
            Some(started) => {
                started.push(Job {
                    app: job.app.clone(),
                    id: job.id,
                    due: job.due,
                    wake: job.wake.clone(),
                    update_lock: job.update_lock.clone(),
                });
                true
            }
            None => false,
        })
    }

    struct Capture;
    impl Capture {
        fn new() -> Self {
            STARTED.with_borrow_mut(|started| *started = Some(Vec::new()));
            Self
        }
        fn take() -> Vec<Job> {
            STARTED.with_borrow_mut(|started| std::mem::take(started.as_mut().unwrap()))
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            STARTED.with_borrow_mut(|started| *started = None);
        }
    }

    #[test]
    fn native_timer_cancel_and_completion_do_not_depend_on_a_gui_tick() {
        let _capture = Capture::new();
        let shared = Arc::new(Mutex::new(MyApp::new(FakeBackend::new())));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first = {
            let mut app = lock(&shared);
            app.attach_shared(Arc::downgrade(&shared));
            let count = count.clone();
            app.timer(Duration::ZERO, move |_| {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        };
        let job = Capture::take().pop().expect("started timer");
        lock(&shared).poll(Instant::now() + Duration::from_secs(1));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0, "idle must not race a native timer");
        lock(&shared).cancel_timer(first);
        assert!(!job.wake.wait(Instant::now()), "cancel wakes an already waiting job");
        job.expire();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let weak = Arc::downgrade(&shared);
        {
            let count = count.clone();
            lock(&shared).timer(Duration::ZERO, move |_| {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        let job = Capture::take().pop().unwrap();
        job.run();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(lock(&shared).timers.is_empty());
        drop(shared);
        assert!(weak.upgrade().is_none());
    }

    /// The surround notice's Fix automatically (its second button).
    fn fix(app: &mut MyApp) {
        let notice = app.ui.notices.service[&2];
        crate::ui::notices::run(app, notice, 1).unwrap();
    }

    /// Whether the sound fix overlay shows its OK button.
    fn overlay_ready(app: &MyApp) -> Option<bool> {
        match app.ui.dialog {
            Some(crate::ui::dialogs::Dialog::ApplyingFix { ready, .. }) => Some(ready),
            _ => None,
        }
    }

    const SURROUND: &str = r#"[{"id":2,"kind":"surround_required","title":"Surround required","message":"Configure surround","colour":"red"}]"#;

    #[test]
    fn native_overlay_timer_waits_for_update_lock_without_retaining_app_lock() {
        let _capture = Capture::new();
        let backend = FakeBackend::new();
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["settings"]["shakers"]["sound_card"] = serde_json::json!("Shaker card");
            docs["hardware"]["sound_cards"] = serde_json::json!(["None", "Shaker card"]);
            docs["notifications"] = serde_json::from_str(SURROUND).unwrap();
        }
        let shared = Arc::new(Mutex::new(loaded(backend.clone())));
        let gate = {
            let mut app = lock(&shared);
            app.attach_shared(Arc::downgrade(&shared));
            crate::ui::notices::synchronize(&mut app);
            fix(&mut app);
            assert_eq!(overlay_ready(&app), Some(false));
            app.update_lock.clone()
        };
        let first = Capture::take().pop().unwrap();
        fix(&mut lock(&shared));
        assert!(!first.wake.wait(Instant::now()), "replacement cancels the old timer");
        let second = Capture::take().pop().unwrap();
        {
            backend.docs.lock().unwrap()["notifications"] = serde_json::json!([]);
            crate::ui::notices::synchronize(&mut lock(&shared));
        }
        assert!(!second.wake.wait(Instant::now()), "withdrawal cancels the timer");
        assert!(lock(&shared).ui.notices.fix_timer.is_none());
        assert_eq!(overlay_ready(&lock(&shared)), None, "withdrawal closes the overlay");
        {
            let mut app = lock(&shared);
            assert!(!app.ui.notices.service.contains_key(&2));
            backend.docs.lock().unwrap()["notifications"] = serde_json::from_str(SURROUND).unwrap();
            crate::ui::notices::synchronize(&mut app);
            fix(&mut app);
        }
        let job = Capture::take().pop().unwrap();
        let guard = gate.lock();
        let (done, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            job.expire();
            done.send(()).unwrap();
        });
        let blocked = completed.recv_timeout(Duration::from_millis(30)).is_err();
        let state_available = shared.try_lock().is_ok();
        drop(guard);
        worker.join().unwrap();
        assert!(blocked, "the callback enters update_lock");
        assert!(state_available, "waiting must not retain the application-state mutex");
        assert_eq!(overlay_ready(&lock(&shared)), Some(true), "OK appears after the wait");
    }

    #[test]
    fn cancellation_after_timer_dispatch_does_not_retract_a_waiting_callback() {
        let _capture = Capture::new();
        let shared = Arc::new(Mutex::new(MyApp::new(FakeBackend::new())));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (timer, gate) = {
            let mut app = lock(&shared);
            app.attach_shared(Arc::downgrade(&shared));
            let calls = calls.clone();
            let timer = app.update_locked_timer(Duration::ZERO, move |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
            (timer, app.update_lock.clone())
        };
        let job = Capture::take().pop().unwrap();
        let wake = job.wake.clone();
        let guard = gate.lock();
        let (started, running) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            job.run();
            finished.send(()).unwrap();
        });
        running.recv_timeout(Duration::from_secs(2)).unwrap();
        let state = lock(&wake.state);
        let (state, timeout) =
            wake.changed.wait_timeout_while(state, Duration::from_secs(2), |state| !state.dispatched).unwrap();
        assert!(!timeout.timed_out() && state.dispatched);
        drop(state);
        assert!(done.recv_timeout(Duration::from_millis(50)).is_err());
        // The timer has already entered the callback. Its function is
        // waiting on update_lock; cancel cannot undo that dispatched call.
        lock(&shared).cancel_timer(timer);
        drop(guard);
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(lock(&shared).timers.is_empty());
    }
    #[test]
    fn dispatched_overlay_timeout_reads_the_replacement_overlay_at_callback_time() {
        let _capture = Capture::new();
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["notifications"] = serde_json::from_str(SURROUND).unwrap();
        let shared = Arc::new(Mutex::new(loaded(backend)));
        let gate = {
            let mut app = lock(&shared);
            app.attach_shared(Arc::downgrade(&shared));
            crate::ui::notices::synchronize(&mut app);
            fix(&mut app);
            app.update_lock.clone()
        };
        let first = Capture::take().pop().unwrap();
        let wake = first.wake.clone();
        let guard = gate.lock();
        let (done, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            first.expire();
            done.send(()).unwrap();
        });
        let state = lock(&wake.state);
        let (state, timeout) =
            wake.changed.wait_timeout_while(state, Duration::from_secs(2), |state| !state.dispatched).unwrap();
        assert!(!timeout.timed_out() && state.dispatched);
        drop(state);
        {
            let mut app = lock(&shared);
            fix(&mut app);
            assert_eq!(overlay_ready(&app), Some(false), "the replacement overlay starts without OK");
        }
        let replacement_timeout = Capture::take().pop().unwrap();
        assert!(!replacement_timeout.wake.state.lock().unwrap().dispatched);
        drop(guard);
        completed.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert_eq!(overlay_ready(&lock(&shared)), Some(true), "entered callback reads the current overlay");
        assert!(!replacement_timeout.wake.state.lock().unwrap().dispatched);
        drop(replacement_timeout);
    }
}
