//! The "default" logger: every message goes to
//! `driver_motionpose.log` in the working directory (opened for appending) and to standard output, formatted
//! `[%level] %datetime{%Y-%M-%d %H:%m:%s}: %msg`. The file is flushed when `LOG_FLUSH_THRESHOLD` (0 unless
//! logging.conf sets it) says so; `MAX_LOG_FILE_SIZE` never rolls the file (the strict
//! file size check is not enabled).
//!
//! Output is written in text mode: LF becomes CR LF.

use super::config::{self, Level, Typed};
use std::cell::UnsafeCell;
use std::ffi::c_int;
use std::fs::{File, OpenOptions};
use std::io::Write;

/// Log file name (FILENAME of the default configuration text).
pub const LOG_FILE_NAME: &str = "driver_motionpose.log";
/// The size of the buffer in front of the log file.
pub const FILE_BUFFER_SIZE: usize = 4096;

/// The text `%level` becomes.
pub fn level_value(level: Level) -> &'static [u8] {
    match level {
        Level::Info => b"INFO ",
        Level::Debug => b"DEBUG",
        Level::Warning => b"WARN ",
        Level::Error => b"ERROR",
        Level::Fatal => b"FATAL",
        Level::Verbose => b"VER",
        Level::Trace => b"TRACE",
        Level::Global | Level::Unknown => b"",
    }
}

/// LF -> CR LF (text mode).
pub fn crt_text_write(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 8);
    for &b in bytes {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// Appends the `len` lowest decimal digits of `n`, zero padded.
fn convert_and_add(out: &mut Vec<u8>, mut n: u64, mut len: i32) {
    let mut digits: Vec<u8> = Vec::new();
    if n > 0 {
        while n > 0 && digits.len() < 8 && len > 0 {
            digits.push(b'0' + (n % 10) as u8);
            n /= 10;
            len -= 1;
        }
    } else {
        digits.push(b'0');
        len -= 1;
    }
    while digits.len() < 8 && len > 0 {
        digits.push(b'0');
        len -= 1;
    }
    digits.reverse();
    out.extend_from_slice(&digits);
}

/// Broken-down local time fields used by `%Y-%M-%d %H:%m:%s`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

pub fn local_time_now() -> LocalTime {
    // SAFETY: time/localtime_r write into local structures.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        LocalTime {
            year: i64::from(tm.tm_year) + 1900,
            month: (tm.tm_mon + 1) as u32,
            day: tm.tm_mday as u32,
            hour: tm.tm_hour as u32,
            minute: tm.tm_min as u32,
            second: tm.tm_sec as u32,
        }
    }
}

/// Builds a log line in the configured format: `[LEVEL] YYYY-MM-DD HH:MM:SS: msg` + LF.
pub fn build_line(level: Level, t: &LocalTime, msg: &[u8]) -> Vec<u8> {
    let mut line = Vec::with_capacity(msg.len() + 32);
    line.push(b'[');
    line.extend_from_slice(level_value(level));
    line.extend_from_slice(b"] ");
    convert_and_add(&mut line, t.year.max(0) as u64, 4);
    line.push(b'-');
    convert_and_add(&mut line, u64::from(t.month), 2);
    line.push(b'-');
    convert_and_add(&mut line, u64::from(t.day), 2);
    line.push(b' ');
    convert_and_add(&mut line, u64::from(t.hour), 2);
    line.push(b':');
    convert_and_add(&mut line, u64::from(t.minute), 2);
    line.push(b':');
    convert_and_add(&mut line, u64::from(t.second), 2);
    line.extend_from_slice(b": ");
    line.extend_from_slice(msg);
    line.push(b'\n');
    line
}

/// The stream of the log file: a 4096-byte buffer in front of a text-mode descriptor.
pub struct LogFile {
    file: File,
    buf: Vec<u8>,
    failed: bool,
}

impl LogFile {
    /// Opens the file for appending, creating it when missing; `None` when it cannot be opened.
    pub fn open(path: &str) -> Option<LogFile> {
        let file = OpenOptions::new().append(true).create(true).open(path).ok()?;
        Some(LogFile { file, buf: Vec::with_capacity(FILE_BUFFER_SIZE), failed: false })
    }

    fn write_out(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let bytes = crt_text_write(&self.buf);
        self.buf.clear();
        if self.file.write_all(&bytes).is_err() {
            self.failed = true;
        }
    }

    /// Writes a line; false once the stream has failed (the failure is sticky).
    pub fn write(&mut self, mut bytes: &[u8]) -> bool {
        if self.failed {
            return false;
        }
        while !bytes.is_empty() {
            let room = FILE_BUFFER_SIZE - self.buf.len();
            let n = room.min(bytes.len());
            self.buf.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.buf.len() == FILE_BUFFER_SIZE {
                self.write_out();
                if self.failed {
                    return false;
                }
            }
        }
        true
    }

    /// Flushes the buffer to the file.
    pub fn flush(&mut self) {
        if !self.failed {
            self.write_out();
        }
    }
}

/// Write all bytes to a descriptor (stdout/stderr).
pub fn write_fd(fd: c_int, bytes: &[u8]) {
    let mut rest = bytes;
    while !rest.is_empty() {
        // SAFETY: writing from a live buffer.
        let n = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if n < 0 {
            // SAFETY: errno is thread-local.
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            return;
        }
        if n == 0 {
            return;
        }
        rest = &rest[n as usize..];
    }
}

/// The logging state of the "default" logger.
pub struct Storage {
    pub typed: Typed,
    /// The number of unflushed lines per level.
    unflushed: Vec<(Level, u32)>,
    pub file: Option<LogFile>,
    /// stdout descriptor (1); a test can redirect it.
    pub stdout_fd: c_int,
}

impl Storage {
    pub fn new(typed: Typed, file: Option<LogFile>) -> Storage {
        let unflushed = config::LEVELS.iter().map(|&l| (l, 0u32)).collect();
        Storage { typed, unflushed, file, stdout_fd: 1 }
    }

    /// Whether a flush is needed: the level's unflushed count, incremented, has reached its flush threshold.
    fn is_flush_needed(&mut self, level: Level) -> bool {
        let threshold = self.typed.log_flush_threshold(level);
        let entry = self.unflushed.iter_mut().find(|(l, _)| *l == level);
        match entry {
            Some((_, count)) => {
                *count = count.wrapping_add(1);
                u64::from(*count) >= threshold
            }
            None => true,
        }
    }

    fn reset_unflushed(&mut self, level: Level) {
        if let Some((_, count)) = self.unflushed.iter_mut().find(|(l, _)| *l == level) {
            *count = 0;
        }
    }

    /// Dispatches one built line to the file and to standard output.
    pub fn dispatch(&mut self, level: Level, line: &[u8]) {
        if self.file.is_some() {
            let written = self.file.as_mut().is_some_and(|f| f.write(line));
            if written && self.is_flush_needed(level) {
                if let Some(f) = self.file.as_mut() {
                    f.flush();
                }
                self.reset_unflushed(level);
            }
        }
        write_fd(self.stdout_fd, &crt_text_write(line));
    }

    pub fn log(&mut self, level: Level, msg: &[u8]) {
        let line = build_line(level, &local_time_now(), msg);
        self.dispatch(level, &line);
    }

    /// Storage destructor at unload / exit: the file stream is flushed and closed.
    pub fn shutdown(&mut self) {
        if let Some(f) = self.file.as_mut() {
            f.flush();
        }
    }
}

/// A recursive lock around the storage, so the
/// crash handler can log from a thread that was interrupted while logging.
pub struct Global {
    mutex: UnsafeCell<libc::pthread_mutex_t>,
    storage: UnsafeCell<Option<Storage>>,
    init: std::sync::Once,
}

// SAFETY: every access goes through the recursive pthread mutex.
unsafe impl Sync for Global {}

impl Global {
    pub const fn new() -> Global {
        Global {
            mutex: UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER),
            storage: UnsafeCell::new(None),
            init: std::sync::Once::new(),
        }
    }

    fn ensure_recursive(&self) {
        self.init.call_once(|| {
            // SAFETY: initialising the mutex once, before first use.
            unsafe {
                let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
                libc::pthread_mutexattr_init(&mut attr);
                libc::pthread_mutexattr_settype(&mut attr, libc::PTHREAD_MUTEX_RECURSIVE);
                libc::pthread_mutex_init(self.mutex.get(), &attr);
                libc::pthread_mutexattr_destroy(&mut attr);
            }
        });
    }

    /// Runs `f` on the storage under the lock.
    pub fn with<R>(&self, f: impl FnOnce(&mut Option<Storage>) -> R) -> R {
        self.ensure_recursive();
        // SAFETY: the recursive mutex serialises threads; a re-entrant call comes only from a signal handler on
        // the owning thread.
        unsafe {
            libc::pthread_mutex_lock(self.mutex.get());
            let r = f(&mut *self.storage.get());
            libc::pthread_mutex_unlock(self.mutex.get());
            r
        }
    }
}

impl Default for Global {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_are_width_limited_and_padded() {
        let mut v = Vec::new();
        convert_and_add(&mut v, 7, 2);
        convert_and_add(&mut v, 0, 2);
        convert_and_add(&mut v, 12345, 4);
        assert_eq!(v, b"07002345");
    }

    #[test]
    fn flush_threshold_buffers_until_reached() {
        let dir = std::env::temp_dir().join(format!("srs-vr-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.log");
        let _ = std::fs::remove_file(&path);
        let typed = Typed { log_flush_threshold: vec![(Level::Global, 2)] };
        let sink = File::create(dir.join("stdout")).unwrap();
        use std::os::fd::AsRawFd;
        let mut s = Storage::new(typed, LogFile::open(path.to_str().unwrap()));
        s.stdout_fd = sink.as_raw_fd();
        s.dispatch(Level::Info, b"a\n");
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        s.dispatch(Level::Info, b"b\n");
        assert_eq!(std::fs::read(&path).unwrap(), b"a\r\nb\r\n");
        s.dispatch(Level::Debug, b"c\n");
        assert_eq!(std::fs::read(&path).unwrap(), b"a\r\nb\r\n");
        s.shutdown();
        assert_eq!(std::fs::read(&path).unwrap(), b"a\r\nb\r\nc\r\n");
        assert_eq!(std::fs::read(dir.join("stdout")).unwrap(), b"a\r\nb\r\nc\r\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
