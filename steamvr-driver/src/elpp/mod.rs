//! The driver's logging: the configuration at load (easylogging++ configuration file format), logging
//! on the "default" logger, and the crash handler.

pub mod config;
pub mod logger;

use config::Level;
use logger::{Global, LogFile, Storage};
use std::ffi::c_int;

static STORAGE: Global = Global::new();

/// Whether `logging.conf` in the working directory exists, and its contents when it can be read.
fn read_config_file() -> Option<Option<Vec<u8>>> {
    let path = std::str::from_utf8(config::CONFIG_FILE_NAME).unwrap_or("logging.conf");
    // Any existing path (directories included) passes.
    std::fs::symlink_metadata(path).ok()?;
    let meta = std::fs::metadata(path).ok();
    if meta.as_ref().is_some_and(|m| m.is_dir()) || meta.is_none() {
        return Some(None);
    }
    match std::fs::read(path) {
        Ok(bytes) => Some(Some(bytes)),
        Err(_) => Some(None),
    }
}

/// Sets up logging: flags, configuration file + default text, reconfiguration of all loggers,
/// which opens the log file.
pub fn init_logging() {
    let mut err = Vec::new();
    let conf = config::init_logging_configurations(read_config_file(), &mut err);
    let typed = config::reconfigure_all_loggers(&conf, &mut err);
    logger::write_fd(2, &logger::crt_text_write(&err));
    let file = LogFile::open(logger::LOG_FILE_NAME);
    STORAGE.with(|s| *s = Some(Storage::new(typed, file)));
}

/// Logs a message at a level on the "default" logger.
pub fn log(level: Level, msg: &[u8]) {
    STORAGE.with(|s| {
        if let Some(storage) = s.as_mut() {
            storage.log(level, msg);
        }
    });
}

/// Storage destructor (atexit at unload or process exit): flushes and closes the log file.
pub fn shutdown() {
    STORAGE.with(|s| {
        if let Some(storage) = s.as_mut() {
            storage.shutdown();
        }
    });
}

/// Replaces the logging state (tests: a storage writing into temporary files).
pub fn set_storage(storage: Option<Storage>) {
    STORAGE.with(|s| *s = storage);
}

/// The crash signals that are handled (SIGABRT is not among them).
pub const CRASH_SIGNALS: [(c_int, &str); 4] =
    [(libc::SIGFPE, "SIGFPE"), (libc::SIGILL, "SIGILL"), (libc::SIGSEGV, "SIGSEGV"), (libc::SIGINT, "SIGINT")];

/// The crash message of a signal, starting with "CRASH HANDLED; " (no detailed reason, no stack trace).
pub fn crash_message(sig: c_int) -> Vec<u8> {
    let mut msg = b"CRASH HANDLED; ".to_vec();
    let mut found = false;
    for (numb, name) in CRASH_SIGNALS {
        if numb == sig {
            msg.extend_from_slice(b"Application has crashed due to [");
            msg.extend_from_slice(name.as_bytes());
            msg.extend_from_slice(b"] signal");
            found = true;
        }
    }
    if !found {
        msg.extend_from_slice(format!("Application has crashed due to unknown signal [{sig}]").as_bytes());
    }
    msg
}

/// The crash handler: a FATAL log (the log itself does not abort), then
/// `abort()`.
extern "C" fn default_crash_handler(sig: c_int) {
    log(Level::Fatal, &crash_message(sig));
    // SAFETY: terminating the process.
    unsafe { libc::abort() }
}

/// Installs the crash handler for each crash signal; a
/// handler is reset to SIG_DFL before it is called and the signal is not blocked meanwhile.
pub fn install_crash_handler() {
    for (sig, _) in CRASH_SIGNALS {
        // SAFETY: installing a handler with a zeroed, then filled, sigaction.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = default_crash_handler as extern "C" fn(c_int) as usize;
            sa.sa_flags = libc::SA_RESETHAND | libc::SA_NODEFER;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_messages() {
        assert_eq!(crash_message(libc::SIGSEGV), b"CRASH HANDLED; Application has crashed due to [SIGSEGV] signal");
        assert_eq!(crash_message(libc::SIGINT), b"CRASH HANDLED; Application has crashed due to [SIGINT] signal");
        assert_eq!(crash_message(libc::SIGTERM), b"CRASH HANDLED; Application has crashed due to unknown signal [15]");
    }
}
