//! The C runtime behaviour the plugin relies on, implemented on top of
//! [`Sys`]: `FILE` streams (text-mode `\n` → `\r\n` translation, `setvbuf`
//! full buffering, flush on close), `errno` as the UCRT maps Win32 errors,
//! `strcpy_s`, `_stricmp`, `isdigit`, `atol`, `_itoa_s`.

use crate::cfmt::{self, Arg, Sink};
use crate::sys::{Handle, OpenMode, Sys};

pub const EINVAL: i32 = 22;
pub const ERANGE: i32 = 34;

/// Win32 error → errno.
pub fn errno_from_win32(err: u32) -> i32 {
    const ENOENT: i32 = 2;
    const E2BIG: i32 = 7;
    const ENOEXEC: i32 = 8;
    const EBADF: i32 = 9;
    const ECHILD: i32 = 10;
    const EAGAIN: i32 = 11;
    const ENOMEM: i32 = 12;
    const EACCES: i32 = 13;
    const EEXIST: i32 = 17;
    const EXDEV: i32 = 18;
    const EMFILE: i32 = 24;
    const ENOSPC: i32 = 28;
    const EPIPE: i32 = 32;
    const ENOTEMPTY: i32 = 41;
    match err {
        1 => EINVAL,
        2 | 3 => ENOENT,
        4 => EMFILE,
        5 => EACCES,
        6 => EBADF,
        7..=9 => ENOMEM,
        10 => E2BIG,
        11 => ENOEXEC,
        12 | 13 => EINVAL,
        15 => ENOENT,
        16 => EACCES,
        17 => EXDEV,
        18 => ENOENT,
        19..=36 => EACCES,
        53 => ENOENT,
        65 => EACCES,
        67 => ENOENT,
        80 => EEXIST,
        82 | 83 => EACCES,
        87 => EINVAL,
        89 => EAGAIN,
        108 => EACCES,
        109 => EPIPE,
        112 => ENOSPC,
        114 => EBADF,
        128 | 129 => ECHILD,
        130 => EBADF,
        131 => EINVAL,
        132 => EACCES,
        145 => ENOTEMPTY,
        158 => EACCES,
        161 => ENOENT,
        164 => EAGAIN,
        167 => EACCES,
        183 => EEXIST,
        188..=202 => ENOEXEC,
        206 => ENOENT,
        215 => EAGAIN,
        1816 => ENOMEM,
        _ => EINVAL,
    }
}

/// UCRT default stream buffer (`_INTERNAL_BUFSIZ`).
pub const DEFAULT_BUFSIZ: usize = 0x1000;
const MAX_BUF: usize = 0x1000;

/// An open `FILE`. Data is buffered untranslated (as the CRT's stream
/// buffer holds it) and translated `\n` → `\r\n` when the buffer is written
/// out, as lowio `_write` does for text-mode handles.
pub struct CrtFile {
    h: Handle,
    text: bool,
    cap: usize,
    len: usize,
    error: bool,
    buf: [u8; MAX_BUF],
}

impl CrtFile {
    /// `fopen(path, "a"/"w")`, `_fsopen(path, "a", _SH_DENYNO)` (text mode)
    /// or `fopen_s(&f, path, "rb")` (binary). Err(errno).
    pub unsafe fn open(sys: &dyn Sys, errno: &mut i32, path: &[u8], mode: OpenMode) -> Option<CrtFile> {
        match sys.file_open(path, mode) {
            Ok(h) => Some(CrtFile {
                h,
                text: mode != OpenMode::ReadSecure,
                cap: DEFAULT_BUFSIZ,
                len: 0,
                error: false,
                buf: [0; MAX_BUF],
            }),
            Err(e) => {
                *errno = errno_from_win32(e);
                sys.set_crt_errno(*errno);
                None
            }
        }
    }

    /// `setvbuf(f, NULL, _IOFBF, size)`.
    pub fn setvbuf(&mut self, size: usize) {
        self.cap = size.clamp(2, MAX_BUF);
    }

    unsafe fn write_out(&mut self, sys: &dyn Sys) -> bool {
        if self.len == 0 {
            return true;
        }
        let ok = if self.text {
            let mut tmp = [0u8; 2 * MAX_BUF];
            let mut n = 0;
            for &b in &self.buf[..self.len] {
                if b == b'\n' {
                    tmp[n] = b'\r';
                    n += 1;
                }
                tmp[n] = b;
                n += 1;
            }
            sys.file_write(self.h, &tmp[..n])
        } else {
            sys.file_write(self.h, &self.buf[..self.len])
        };
        self.len = 0;
        if !ok {
            self.error = true;
        }
        ok
    }

    pub unsafe fn write(&mut self, sys: &dyn Sys, data: &[u8]) {
        for &b in data {
            self.buf[self.len] = b;
            self.len += 1;
            if self.len == self.cap {
                self.write_out(sys);
            }
        }
    }

    /// `fflush(f)`: 0 or EOF.
    pub unsafe fn flush(&mut self, sys: &dyn Sys) -> i32 {
        if self.write_out(sys) {
            0
        } else {
            -1
        }
    }

    /// `fclose(f)`: 0 or EOF.
    pub unsafe fn close(mut self, sys: &dyn Sys) -> i32 {
        let flushed = self.write_out(sys);
        let closed = sys.file_close(self.h);
        if flushed && closed && !self.error {
            0
        } else {
            -1
        }
    }

    /// `fprintf(f, fmt, ...)`.
    pub unsafe fn printf(&mut self, sys: &dyn Sys, fmt: &[u8], args: &[Arg]) {
        struct W<'a> {
            f: &'a mut CrtFile,
            sys: &'a dyn Sys,
        }
        impl Sink for W<'_> {
            fn put(&mut self, b: &[u8]) {
                unsafe { self.f.write(self.sys, b) }
            }
        }
        cfmt::format(&mut W { f: self, sys }, fmt, args);
    }

    /// `fseek` + `ftell` + `rewind` + `fread(buf, size, 1, f) == 1` of
    /// GetFileContents (binary stream, nothing buffered yet).
    pub unsafe fn seek_end(&mut self, sys: &dyn Sys) -> Result<i64, ()> {
        sys.file_seek(self.h, 0, 2).map_err(|_| ())
    }
    pub unsafe fn rewind(&mut self, sys: &dyn Sys) {
        let _ = sys.file_seek(self.h, 0, 0);
    }
    /// `fread(buf, n, 1, f)`: 1 when all n bytes were read (0 when n == 0).
    pub unsafe fn read_exact_one(&mut self, sys: &dyn Sys, buf: *mut u8, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let mut got = 0;
        while got < n {
            let s = core::slice::from_raw_parts_mut(buf.add(got), n - got);
            match sys.file_read(self.h, s) {
                Ok(0) | Err(_) => return 0,
                Ok(k) => got += k,
            }
        }
        1
    }
}

/// A `static FILE *` the plugin keeps open across calls (the debug log
/// and the two DebugISIInternals dump streams).
pub enum StreamSlot {
    /// NULL.
    None,
    Open(CrtFile),
    /// A pointer that was passed to `fclose` but not reset: later writes
    /// through it reach a closed stream and are lost.
    Stale,
}

impl StreamSlot {
    pub fn is_null(&self) -> bool {
        matches!(self, StreamSlot::None)
    }
}

/// `strcpy_s(dst, size, src)`: copies `src` (up to its NUL) into `dst`;
/// when it does not fit, `dst[0] = 0` and the invalid-parameter handler ends
/// the process, as in the UCRT.
pub unsafe fn strcpy_s(sys: &dyn Sys, dst: *mut u8, size: usize, src: *const u8) {
    let mut i = 0;
    loop {
        if i >= size {
            *dst = 0;
            sys.invalid_parameter();
        }
        let c = *src.add(i);
        *dst.add(i) = c;
        if c == 0 {
            return;
        }
        i += 1;
    }
}

/// `_stricmp(a, b) == 0` (C locale: ASCII case folding).
pub fn stricmp_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_ascii_lowercase() == y.to_ascii_lowercase())
}

/// `isdigit((int)c)` for a `char` (C locale).
pub fn isdigit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// `atol(s)` (C locale): skips white space, optional sign, decimal digits;
/// on overflow returns LONG_MAX/LONG_MIN and sets errno = ERANGE.
pub fn atol(s: &[u8], errno: &mut i32) -> i32 {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut v: i64 = 0;
    let mut overflow = false;
    while i < s.len() && s[i].is_ascii_digit() {
        v = v * 10 + (s[i] - b'0') as i64;
        if v > i32::MAX as i64 + 1 {
            overflow = true;
            v = i32::MAX as i64 + 1;
        }
        i += 1;
    }
    if neg {
        v = -v;
    }
    if overflow || v > i32::MAX as i64 || v < i32::MIN as i64 {
        *errno = ERANGE;
        return if neg { i32::MIN } else { i32::MAX };
    }
    v as i32
}

/// `_itoa_s(v, buf, 0x14, 10)` (radix 10, signed).
pub fn itoa10(v: i32, out: &mut [u8; 20]) -> usize {
    let b = cfmt::sprintf::<20>(b"%d", &[Arg::Int(v)]);
    let s = b.as_bytes();
    out[..s.len()].copy_from_slice(s);
    out[s.len()] = 0;
    s.len()
}
