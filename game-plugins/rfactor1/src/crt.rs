//! The C run-time pieces the plugin code uses: `clock()` with
//! its initialisation and its count scaling.

use crate::os::Os;

/// QPF / QPC captured at CRT initialisation;
/// all -1 when either call failed or the frequency is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    pub freq: i64,
    pub start: i64,
}

impl Clock {
    pub const UNSET: Clock = Clock { freq: -1, start: -1 };

    /// Captures the clock base (DLL_PROCESS_ATTACH).
    pub fn init(os: &dyn Os) -> Clock {
        let f = os.qpf();
        let c = match f {
            Some(_) => os.qpc(),
            None => None,
        };
        match (f, c) {
            (Some(f), Some(c)) if f != 0 => Clock { freq: f, start: c },
            _ => Clock::UNSET,
        }
    }

    /// `clock()`: milliseconds since CRT start, -1 on failure / overflow.
    pub fn clock(&self, os: &dyn Os) -> i32 {
        // (lo & hi) != 0xffffffff
        if ((self.start as u32) & ((self.start >> 32) as u32)) == 0xffff_ffff {
            return -1;
        }
        let now = match os.qpc() {
            Some(v) => v,
            None => return -1,
        };
        let diff = now.wrapping_sub(self.start);
        if diff < 0 {
            return -1;
        }
        let r = scale_count(diff, self.freq);
        if r < 0x8000_0000 {
            r as i32
        } else {
            -1
        }
    }
}

/// Scales a count to milliseconds: `(c / f) * 1000 + (c % f) * 1000 / f` in signed 64 bit.
pub fn scale_count(c: i64, f: i64) -> i64 {
    let q = c.wrapping_div(f);
    let r = c.wrapping_rem(f);
    q.wrapping_mul(1000).wrapping_add(r.wrapping_mul(1000).wrapping_div(f))
}
