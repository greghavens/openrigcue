//! `MappedBuffer<T>` (one per buffer):
//! Initialize, MapMemoryFile, BeginUpdate,
//! EndUpdate, ReleaseResources.

use crate::layout::*;
use crate::log::{level, Ctx};
use crate::sys::Handle;
use core::sync::atomic::{AtomicU32, Ordering};

/// The `__FUNCTION__` strings of one instantiation.
pub struct Names {
    pub initialize: &'static [u8],
    pub map_memory_file: &'static [u8],
    pub begin_update: &'static [u8],
    pub end_update: &'static [u8],
    pub release_resources: &'static [u8],
}

macro_rules! names {
    ($t:literal) => {
        Names {
            initialize: concat!("MappedBuffer<struct ", $t, ">::Initialize").as_bytes(),
            map_memory_file: concat!("MappedBuffer<struct ", $t, ">::MapMemoryFile").as_bytes(),
            begin_update: concat!("MappedBuffer<struct ", $t, ">::BeginUpdate").as_bytes(),
            end_update: concat!("MappedBuffer<struct ", $t, ">::EndUpdate").as_bytes(),
            release_resources: concat!("MappedBuffer<struct ", $t, ">::ReleaseResources").as_bytes(),
        }
    };
}

pub static TELEMETRY_NAMES: Names = names!("rF2Telemetry");
pub static SCORING_NAMES: Names = names!("rF2Scoring");
pub static RULES_NAMES: Names = names!("rF2Rules");
pub static MULTI_RULES_NAMES: Names = names!("rF2MultiRules");
pub static EXTENDED_NAMES: Names = names!("rF2Extended");

pub struct MappedBuffer {
    /// +0x00 mpBuffVersionBlock (= view).
    pub version: *mut u8,
    /// +0x08 mpBuff (= view + 8).
    pub buff: *mut u8,
    /// +0x10 the mapped view.
    pub view: *mut u8,
    /// +0x18 mapping name (NUL-terminated).
    pub name: &'static [u8],
    /// +0x20 mapping handle.
    pub handle: Handle,
    /// +0x28 mMapped.
    pub mapped: bool,
    pub size: usize,
    pub names: &'static Names,
}

/// SDDL used when mapping globally.
pub const GLOBAL_SDDL: &[u8] = b"D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)(A;OICI;GA;;;WD)\0";

impl MappedBuffer {
    pub const fn new(name: &'static [u8], size: usize, names: &'static Names) -> Self {
        MappedBuffer {
            version: core::ptr::null_mut(),
            buff: core::ptr::null_mut(),
            view: core::ptr::null_mut(),
            name,
            handle: core::ptr::null_mut(),
            mapped: false,
            size,
            names,
        }
    }

    fn begin(&self) -> &AtomicU32 {
        unsafe { AtomicU32::from_ptr(self.version as *mut u32) }
    }
    fn end(&self) -> &AtomicU32 {
        unsafe { AtomicU32::from_ptr(self.version.add(4) as *mut u32) }
    }

    /// (mVersionUpdateBegin, mVersionUpdateEnd).
    pub fn versions(&self) -> (u32, u32) {
        unsafe { (rd_u32(self.version, 0), rd_u32(self.version, 4)) }
    }

    /// Initialize(mapGlobally).
    pub fn initialize(&mut self, cx: Ctx, map_globally: bool) -> bool {
        self.handle = self.map_memory_file(cx, map_globally);
        if self.handle.is_null() {
            cx.dbg(level::ERRORS, self.names.initialize, 0x29, b"Failed to map file");
            self.release_resources(cx);
            return false;
        }
        unsafe { core::ptr::write_bytes(self.view, 0, self.size) };
        self.mapped = true;
        true
    }

    /// MapMemoryFile(name, mapGlobally, &view, &versionBlock, &buff).
    fn map_memory_file(&mut self, cx: Ctx, map_globally: bool) -> Handle {
        let sys = cx.sys;
        let mut module = [0u8; 0x400];
        sys.module_file_name(&mut module);
        let mut full = [0u8; 0x104];
        let module_name = unsafe { cstr(module.as_ptr()) };
        let dedicated = module_name.windows(13).any(|w| w == b"Dedicated.exe");
        let name = &self.name[..self.name.len() - 1];
        let use_sd;
        if !dedicated {
            unsafe { crate::crt::strcpy_s(sys, full.as_mut_ptr(), 0x104, self.name.as_ptr()) };
            use_sd = map_globally;
        } else {
            let mut pid = [0u8; 20];
            let n = {
                let b = crate::cfmt::sprintf::<20>(b"%d", &[crate::cfmt::Arg::Int(sys.current_process_id() as i32)]);
                let s = b.as_bytes();
                pid[..s.len()].copy_from_slice(s);
                s.len()
            };
            let fmt: &[u8] = if map_globally { b"Global\\%s%s" } else { b"%s%s" };
            let b = crate::cfmt::sprintf::<0x110>(fmt, &[crate::cfmt::Arg::Str(name), crate::cfmt::Arg::Str(&pid[..n])]);
            let s = b.as_bytes();
            full[..s.len()].copy_from_slice(s);
            use_sd = map_globally;
        }
        let full_len = full.iter().position(|&c| c == 0).unwrap_or(full.len());
        let full_name = &full[..full_len + 1];
        let full_str = &full[..full_len];
        let h = unsafe {
            if use_sd {
                let sd = sys.convert_sddl(GLOBAL_SDDL);
                if sd.is_null() {
                    cx.dbg_s(
                        level::ERRORS,
                        self.names.map_memory_file,
                        0xda,
                        b"Failed to create security descriptor for mapping:",
                        full_str,
                    );
                    cx.trace_last_win32_error();
                    sys.local_free(sd);
                    return core::ptr::null_mut();
                }
                let h = sys.create_file_mapping(Some(sd), self.size as u32, full_name);
                sys.local_free(sd);
                h
            } else {
                sys.create_file_mapping(None, self.size as u32, full_name)
            }
        };
        if h.is_null() {
            cx.dbg_s(level::ERRORS, self.names.map_memory_file, 0xea, b"Failed to create file mapping for file:", full_str);
            cx.trace_last_win32_error();
            return core::ptr::null_mut();
        }
        if sys.get_last_error() == 183 {
            cx.dbg_s(
                level::WARNINGS,
                self.names.map_memory_file,
                0xf0,
                b"WARNING: File mapping already exists for file:",
                full_str,
            );
        }
        let view = unsafe { sys.map_view(h, self.size) };
        self.view = view;
        if view.is_null() {
            cx.dbg(level::ERRORS, self.names.map_memory_file, 0xfa, b"Failed to map buffer view.");
            cx.trace_last_win32_error();
            // The mapping handle is not closed.
            return core::ptr::null_mut();
        }
        self.version = self.view;
        self.buff = unsafe { self.view.add(VERSION_BLOCK_SIZE) };
        h
    }

    /// BeginUpdate().
    pub fn begin_update(&self, cx: Ctx) {
        if !self.mapped {
            cx.dbg(level::ERRORS, self.names.begin_update, 0x3e, b"Accessing unmapped buffer.");
            return;
        }
        let b = self.begin().load(Ordering::Relaxed);
        let e = self.end().load(Ordering::Relaxed);
        if b != e {
            if cx.level() > level::SYNCHRONIZATION - 1 {
                let msg = crate::cfmt::sprintf::<0x200>(
                    b"BeginUpdate: versions out of sync.  Version Begin:%d  End:%d",
                    &[crate::cfmt::Arg::Int(b as i32), crate::cfmt::Arg::Int(e as i32)],
                );
                cx.dbg(level::SYNCHRONIZATION, self.names.begin_update, 0x4a, msg.as_bytes());
            }
            let v = self.begin().load(Ordering::Relaxed);
            self.end().swap(v, Ordering::SeqCst);
        }
        self.begin().fetch_add(1, Ordering::SeqCst);
    }

    /// EndUpdate().
    pub fn end_update(&self, cx: Ctx) {
        if !self.mapped {
            cx.dbg(level::ERRORS, self.names.end_update, 0x55, b"Accessing unmapped buffer.");
            return;
        }
        self.end().fetch_add(1, Ordering::SeqCst);
        let b = self.begin().load(Ordering::Relaxed);
        let e = self.end().load(Ordering::Relaxed);
        if b != e {
            if cx.level() > level::SYNCHRONIZATION - 1 {
                let msg = crate::cfmt::sprintf::<0x200>(
                    b"EndUpdate: versions out of sync.  Version Begin:%d  End:%d",
                    &[crate::cfmt::Arg::Int(b as i32), crate::cfmt::Arg::Int(e as i32)],
                );
                cx.dbg(level::SYNCHRONIZATION, self.names.end_update, 99, msg.as_bytes());
            }
            let v = self.end().load(Ordering::Relaxed);
            self.begin().swap(v, Ordering::SeqCst);
        }
    }

    /// ReleaseResources().
    pub fn release_resources(&mut self, cx: Ctx) {
        self.mapped = false;
        if !self.view.is_null() && unsafe { !cx.sys.unmap_view(self.view) } {
            cx.dbg(level::ERRORS, self.names.release_resources, 0x7f, b"Failed to unmap mapped buffer view");
            cx.trace_last_win32_error();
        }
        self.view = core::ptr::null_mut();
        self.buff = core::ptr::null_mut();
        self.version = core::ptr::null_mut();
        if !self.handle.is_null() && unsafe { !cx.sys.close_handle(self.handle) } {
            cx.dbg(level::ERRORS, self.names.release_resources, 0x8a, b"Failed to close mapped file handle.");
            cx.trace_last_win32_error();
        }
        self.handle = core::ptr::null_mut();
    }

    /// `if (mapped) { BeginUpdate(); memset(mpBuff, 0, payload); EndUpdate(); }`
    /// (ClearState and Shutdown).
    pub fn clear_payload(&self, cx: Ctx) {
        if self.mapped {
            self.begin_update(cx);
            unsafe { core::ptr::write_bytes(self.buff, 0, self.size - VERSION_BLOCK_SIZE) };
            self.end_update(cx);
        }
    }
}
