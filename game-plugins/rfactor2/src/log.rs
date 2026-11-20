//! The plugin's DLL globals and its
//! debug trace (`DEBUG_MSG`), microsecond timer
//! and `TraceLastWin32Error`.

use crate::cfmt::Arg;
use crate::crt::{CrtFile, StreamSlot};
use crate::sys::{OpenMode, Sys};

pub const DEBUG_OUTPUT_FILE: &[u8] = b"UserData\\Log\\RF2SMMP_DebugOutput.txt\0";
pub const INTERNALS_TELEMETRY_FILE: &[u8] = b"UserData\\Log\\RF2SMMP_InternalsTelemetryOutput.txt\0";
pub const INTERNALS_SCORING_FILE: &[u8] = b"UserData\\Log\\RF2SMMP_InternalsScoringOutput.txt\0";

/// Debug levels (`DebugOutputLevel`, clamped 0..7).
pub mod level {
    pub const ERRORS: i32 = 1;
    pub const CRITICAL_INFO: i32 = 2;
    pub const WARNINGS: i32 = 3;
    pub const SYNCHRONIZATION: i32 = 4;
    pub const TIMING: i32 = 6;
    pub const ALL: i32 = 7;
}

pub struct Globals {
    /// `DebugOutputLevel` (0 at load).
    pub debug_level: i32,
    /// `DebugISIInternals`.
    pub debug_isi_internals: bool,
    /// `DedicatedServerMapGlobally`.
    pub dedicated_server_map_globally: bool,
    /// `EnableStockCarRulesPlugin`.
    pub enable_stock_car_rules: bool,
    /// The debug log stream.
    pub debug_file: StreamSlot,
    /// The DebugISIInternals telemetry stream.
    pub telemetry_dump: StreamSlot,
    /// The DebugISIInternals scoring stream.
    pub scoring_dump: StreamSlot,
    /// QueryPerformanceFrequency * 1e-6, and its init flag.
    pub perf_ticks_per_us: f64,
    pub perf_init: bool,
    /// GetTickCount64 of the last debug log flush.
    pub last_flush_ticks: u64,
    /// Host compatibility errno; Windows observes the shared CRT's thread-local value.
    pub errno: i32,
}

impl Globals {
    pub const fn new() -> Self {
        Globals {
            debug_level: 0,
            debug_isi_internals: false,
            dedicated_server_map_globally: false,
            enable_stock_car_rules: false,
            debug_file: StreamSlot::None,
            telemetry_dump: StreamSlot::None,
            scoring_dump: StreamSlot::None,
            perf_ticks_per_us: 0.0,
            perf_init: false,
            last_flush_ticks: 0,
            errno: 0,
        }
    }
}

impl Default for Globals {
    fn default() -> Self {
        Self::new()
    }
}

/// What every routine needs: the OS and the DLL globals.
#[derive(Clone, Copy)]
pub struct Ctx {
    pub sys: &'static dyn Sys,
    pub g: *mut Globals,
}

impl Ctx {
    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub fn g(&self) -> &mut Globals {
        unsafe { &mut *self.g }
    }

    pub fn level(&self) -> i32 {
        self.g().debug_level
    }

    /// `DEBUG_MSG(level, fmt, ...)`.
    pub fn trace(&self, lvl: i32, fmt: &[u8], args: &[Arg]) {
        let g = self.g();
        if lvl > g.debug_level {
            return;
        }
        unsafe {
            if g.debug_file.is_null() {
                match CrtFile::open(self.sys, &mut g.errno, DEBUG_OUTPUT_FILE, OpenMode::Append) {
                    Some(mut f) => {
                        f.setvbuf(0x800);
                        g.debug_file = StreamSlot::Open(f);
                    }
                    // setvbuf(NULL, ...) and fprintf(NULL, ...) on a failed open.
                    None => self.sys.invalid_parameter(),
                }
            }
            let t = self.sys.local_time();
            let tid = self.sys.current_thread_id();
            if let StreamSlot::Open(f) = &mut g.debug_file {
                f.printf(
                    self.sys,
                    b"%.2d:%.2d:%.2d.%.3d TID:0x%04x  ",
                    &[
                        Arg::Int(t.hour as i32),
                        Arg::Int(t.minute as i32),
                        Arg::Int(t.second as i32),
                        Arg::Int(t.milliseconds as i32),
                        Arg::Int(tid as i32),
                    ],
                );
                f.printf(self.sys, fmt, args);
            }
            let now = self.sys.tick_count64();
            let elapsed = now.wrapping_sub(g.last_flush_ticks) as f64;
            if 10.0 < elapsed * 0.001 {
                if let StreamSlot::Open(f) = &mut g.debug_file {
                    f.flush(self.sys);
                }
                g.last_flush_ticks = now;
            }
        }
    }

    /// The `/GS` epilogue of a function whose stack buffer was overrun
    /// (`smashed`): the cookie no longer matches and the process ends there.
    pub fn gs_check(&self, smashed: bool) {
        if smashed {
            self.sys.report_gs_failure();
        }
    }

    /// `DEBUG_MSG(lvl, "%s(%d) : %s\n", __FUNCTION__, __LINE__, msg)`.
    pub fn dbg(&self, lvl: i32, func: &[u8], line: i32, msg: &[u8]) {
        if lvl <= self.level() {
            self.trace(lvl, b"%s(%d) : %s\n", &[Arg::Str(func), Arg::Int(line), Arg::Str(msg)]);
        }
    }

    /// `"%s(%d) : %s %s\n"`.
    pub fn dbg_s(&self, lvl: i32, func: &[u8], line: i32, msg: &[u8], s: &[u8]) {
        if lvl <= self.level() {
            self.trace(lvl, b"%s(%d) : %s %s\n", &[Arg::Str(func), Arg::Int(line), Arg::Str(msg), Arg::Str(s)]);
        }
    }

    /// `"%s(%d) : %s %d\n"`.
    pub fn dbg_d(&self, lvl: i32, func: &[u8], line: i32, msg: &[u8], d: i32) {
        if lvl <= self.level() {
            self.trace(lvl, b"%s(%d) : %s %d\n", &[Arg::Str(func), Arg::Int(line), Arg::Str(msg), Arg::Int(d)]);
        }
    }

    /// `"%s(%d) : %s %s %s\n"`.
    pub fn dbg_ss(&self, lvl: i32, func: &[u8], line: i32, msg: &[u8], s1: &[u8], s2: &[u8]) {
        if lvl <= self.level() {
            self.trace(
                lvl,
                b"%s(%d) : %s %s %s\n",
                &[Arg::Str(func), Arg::Int(line), Arg::Str(msg), Arg::Str(s1), Arg::Str(s2)],
            );
        }
    }

    /// QueryPerformanceCounter in microseconds.
    pub fn ticks_now_us(&self) -> f64 {
        let g = self.g();
        if !g.perf_init {
            let f = self.sys.query_performance_frequency();
            g.perf_init = true;
            g.perf_ticks_per_us = f as f64 * 1e-6;
        }
        let c = self.sys.query_performance_counter();
        c as f64 / g.perf_ticks_per_us
    }

    /// `SharedMemoryPlugin::TraceLastWin32Error`.
    pub fn trace_last_win32_error(&self) {
        if self.level() < level::ERRORS {
            return;
        }
        let err = self.sys.get_last_error();
        if err == 0 {
            return;
        }
        unsafe {
            let (n, buf) = self.sys.format_message(err);
            self.dbg_d(level::ERRORS, b"SharedMemoryPlugin::TraceLastWin32Error", 0x473, b"Win32 error code:", err as i32);
            if n != 0 && !buf.is_null() {
                self.dbg_s(
                    level::ERRORS,
                    b"SharedMemoryPlugin::TraceLastWin32Error",
                    0x476,
                    b"Win32 error description:",
                    crate::layout::cstr(buf),
                );
            }
            self.sys.local_free(buf as *mut core::ffi::c_void);
        }
    }
}
