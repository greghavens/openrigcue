//! Ctrl-C / console close handling: request a clean shutdown so per-poll mirrors are marked absent.

use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{BOOL, TRUE};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_console_event(_kind: u32) -> BOOL {
    STOP.store(true, Ordering::SeqCst);
    TRUE
}

pub fn install_handler() {
    // SAFETY: registers a handler with the signature the API requires; it only stores to an atomic.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_event), TRUE) } == 0 {
        tracing::debug!("no console control handler installed");
    }
}

pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}
