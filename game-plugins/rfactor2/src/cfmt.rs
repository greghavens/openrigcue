//! The subset of the C runtime's printf family the plugin uses
//! (`fprintf`/`vfprintf`/`sprintf`/`sprintf_s`), so
//! that every line the plugin writes is formatted as the Windows C runtime
//! formats it.
//!
//! Conversions used by the plugin's format strings: `%s`, `%d`, `%x`,
//! `%f` with flags `0`, width and precision (`%.2d`, `%.3d`, `%04x`, `%6.3f`,
//! `%.1f`, ...) and `%%`. Floating point follows the UCRT (Windows 10 2004
//! and later): the exact binary value is rounded to the requested precision
//! with ties to even, which is what core's formatter does; infinities and
//! NaNs print as the UCRT does (`inf`, `nan`, `-nan(ind)`, `nan(snan)`).

use core::fmt::Write;

#[derive(Clone, Copy, Debug)]
pub enum Arg<'a> {
    /// `int`/`long`/`unsigned` passed to `%d`/`%x` (printed through the int's 32 bits).
    Int(i32),
    /// `double` passed to `%f`.
    Dbl(f64),
    /// `char *` passed to `%s` (bytes up to, not including, the NUL).
    Str(&'a [u8]),
}

pub trait Sink {
    fn put(&mut self, bytes: &[u8]);
}

/// A `char buf[N]` filled by `sprintf`; writes beyond N-1 bytes are dropped.
///
/// Plain `sprintf`
/// has no bound: where a message can outgrow its `char[0x200]` buffer
/// (`%f` of a huge double), the caller formats into a [`TRACE_CAP`] buffer
/// so the whole line is kept, and tests
/// [`FixedBuf::written`] against the distance to the frame's `/GS` cookie.
pub struct FixedBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> FixedBuf<N> {
    pub const fn new() -> Self {
        Self { buf: [0; N], len: 0 }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
    /// Bytes `sprintf` stored, the terminating NUL included.
    pub fn written(&self) -> usize {
        self.len + 1
    }
    pub fn clear(&mut self) {
        self.len = 0;
        self.buf = [0; N];
    }
}

impl<const N: usize> Default for FixedBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Sink for FixedBuf<N> {
    fn put(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.len + 1 < N {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
    }
}

impl<const N: usize> Write for FixedBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.put(s.as_bytes());
        Ok(())
    }
}

/// Capacity that holds any trace line of the plugin in full: the longest,
/// TelemetryTraceSkipUpdate's "Pos Mismatch" line, is at most ~2660 bytes
/// (8 `%f` of at most 317 characters each, one `%d`, the format text).
pub const TRACE_CAP: usize = 0x1000;

/// `sprintf(buf, fmt, ...)` into a fresh buffer.
pub fn sprintf<const N: usize>(fmt: &[u8], args: &[Arg]) -> FixedBuf<N> {
    let mut b = FixedBuf::<N>::new();
    format(&mut b, fmt, args);
    b
}

#[derive(Default, Clone, Copy)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: usize,
    prec: Option<usize>,
}

fn pad(out: &mut dyn Sink, n: usize, c: u8) {
    for _ in 0..n {
        out.put(&[c]);
    }
}

/// Emit `prefix` (sign / 0x) and `body` honouring width, `-` and `0`.
fn emit(out: &mut dyn Sink, spec: &Spec, prefix: &[u8], body: &[u8], zero_ok: bool) {
    let len = prefix.len() + body.len();
    let fill = spec.width.saturating_sub(len);
    if spec.left {
        out.put(prefix);
        out.put(body);
        pad(out, fill, b' ');
    } else if spec.zero && zero_ok {
        out.put(prefix);
        pad(out, fill, b'0');
        out.put(body);
    } else {
        pad(out, fill, b' ');
        out.put(prefix);
        out.put(body);
    }
}

fn fmt_int(out: &mut dyn Sink, spec: &Spec, v: i32, hex: Option<bool>) {
    let mut digits = [0u8; 16];
    let mut n = 0;
    let neg;
    let mut mag: u64;
    match hex {
        Some(_) => {
            neg = false;
            mag = v as u32 as u64;
        }
        None => {
            neg = v < 0;
            mag = (v as i64).unsigned_abs();
        }
    }
    let base = if hex.is_some() { 16 } else { 10 };
    let upper = hex == Some(true);
    while mag != 0 {
        let d = (mag % base) as u8;
        digits[n] = if d < 10 {
            b'0' + d
        } else if upper {
            b'A' + d - 10
        } else {
            b'a' + d - 10
        };
        n += 1;
        mag /= base;
    }
    let min_digits = spec.prec.unwrap_or(1);
    let mut body = [0u8; 48];
    let mut bl = 0;
    let zeros = min_digits.saturating_sub(n).min(32);
    for _ in 0..zeros {
        body[bl] = b'0';
        bl += 1;
    }
    for i in (0..n).rev() {
        body[bl] = digits[i];
        bl += 1;
    }
    let mut prefix = [0u8; 2];
    let mut pl = 0;
    if hex.is_some() {
        if spec.alt && v != 0 {
            prefix = [b'0', if upper { b'X' } else { b'x' }];
            pl = 2;
        }
    } else if neg {
        prefix[0] = b'-';
        pl = 1;
    } else if spec.plus {
        prefix[0] = b'+';
        pl = 1;
    } else if spec.space {
        prefix[0] = b' ';
        pl = 1;
    }
    // The 0 flag is ignored when a precision is given (C11 7.21.6.1).
    emit(out, spec, &prefix[..pl], &body[..bl], spec.prec.is_none());
}

fn fmt_f(out: &mut dyn Sink, spec: &Spec, v: f64) {
    let prec = spec.prec.unwrap_or(6);
    let bits = v.to_bits();
    let neg = bits >> 63 != 0;
    let mut prefix = [0u8; 1];
    let pl = if neg {
        prefix[0] = b'-';
        1
    } else if spec.plus {
        prefix[0] = b'+';
        1
    } else if spec.space {
        prefix[0] = b' ';
        1
    } else {
        0
    };
    if v.is_infinite() {
        emit(out, spec, &prefix[..pl], b"inf", false);
        return;
    }
    if v.is_nan() {
        let quiet = bits & (1u64 << 51) != 0;
        let payload = bits & ((1u64 << 51) - 1);
        let body: &[u8] = if !quiet {
            b"nan(snan)"
        } else if neg && payload == 0 {
            b"nan(ind)"
        } else {
            b"nan"
        };
        emit(out, spec, &prefix[..pl], body, false);
        return;
    }
    // Large enough for any finite double with the precisions used here.
    let mut body = FixedBuf::<1200>::new();
    let _ = write!(body, "{:.*}", prec, f64::from_bits(bits & !(1u64 << 63)));
    if spec.alt && prec == 0 {
        body.put(b".");
    }
    emit(out, spec, &prefix[..pl], body.as_bytes(), true);
}

/// `vfprintf(out, fmt, args)`.
pub fn format(out: &mut dyn Sink, fmt: &[u8], args: &[Arg]) {
    let mut i = 0;
    let mut a = 0;
    let mut next = || {
        let r = args.get(a).copied();
        a += 1;
        r
    };
    while i < fmt.len() {
        let c = fmt[i];
        if c != b'%' {
            let start = i;
            while i < fmt.len() && fmt[i] != b'%' {
                i += 1;
            }
            out.put(&fmt[start..i]);
            continue;
        }
        i += 1;
        let mut spec = Spec::default();
        while i < fmt.len() {
            match fmt[i] {
                b'-' => spec.left = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.alt = true,
                b'0' => spec.zero = true,
                _ => break,
            }
            i += 1;
        }
        while i < fmt.len() && fmt[i].is_ascii_digit() {
            spec.width = spec.width * 10 + (fmt[i] - b'0') as usize;
            i += 1;
        }
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            let mut p = 0;
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                p = p * 10 + (fmt[i] - b'0') as usize;
                i += 1;
            }
            spec.prec = Some(p);
        }
        while i < fmt.len() && matches!(fmt[i], b'h' | b'l' | b'L') {
            i += 1;
        }
        if i >= fmt.len() {
            break;
        }
        let conv = fmt[i];
        i += 1;
        match conv {
            b'%' => out.put(b"%"),
            b'd' | b'i' => {
                let v = match next() {
                    Some(Arg::Int(v)) => v,
                    _ => 0,
                };
                fmt_int(out, &spec, v, None)
            }
            b'x' | b'X' => {
                let v = match next() {
                    Some(Arg::Int(v)) => v,
                    _ => 0,
                };
                fmt_int(out, &spec, v, Some(conv == b'X'))
            }
            b'f' | b'F' => {
                let v = match next() {
                    Some(Arg::Dbl(v)) => v,
                    _ => 0.0,
                };
                fmt_f(out, &spec, v)
            }
            b's' => {
                let s: &[u8] = match next() {
                    Some(Arg::Str(s)) => s,
                    _ => b"(null)",
                };
                let s = match spec.prec {
                    Some(p) if p < s.len() => &s[..p],
                    _ => s,
                };
                emit(out, &spec, b"", s, false)
            }
            _ => {}
        }
    }
}
