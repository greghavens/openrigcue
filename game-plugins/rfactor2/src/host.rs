//! `PluginHost`: optionally loads ISI's
//! `Bin64\Plugins\StockCarRules.dll` inside this plugin, forwards its
//! configuration from CustomPluginVariables.JSON and forwards game callbacks
//! to it. Only active with `EnableStockCarRulesPlugin` = 1.

use crate::crt::{self, CrtFile};
use crate::layout::{cstr, custom_var};
use crate::log::{level, Ctx};
use crate::sys::{Handle, OpenMode};
use crate::vtbl;
use core::ffi::c_void;

pub const CUSTOM_PLUGIN_VARIABLES_REL: &[u8] = b"\\UserData\\player\\CustomPluginVariables.JSON\0";
pub const SCR_PLUGIN_REL: &[u8] = b"\\Bin64\\Plugins\\StockCarRules.dll\0";
pub const SCR_PLUGIN_NAME: &[u8] = b"StockCarRules.dll\0";
/// The line IsPluginDisabled looks for.
pub const DISABLED_NEEDLE: &[u8] = b" \" Enabled\":0";

type GetPluginNameFn = unsafe extern "C" fn() -> *const u8;
type GetPluginTypeFn = unsafe extern "C" fn() -> i32;
type GetPluginVersionFn = unsafe extern "C" fn() -> i32;
type CreatePluginObjectFn = unsafe extern "C" fn() -> *mut u8;
type DestroyPluginObjectFn = unsafe extern "C" fn(*mut u8);

pub struct PluginHost {
    /// +0x10 mStockCarRulesPluginLoaded.
    pub loaded: bool,
    /// +0x18 module handle.
    pub module: Handle,
    /// +0x20 the hosted InternalsPluginV07 (CreatePluginObject() - 8).
    pub plugin: *mut u8,
    /// +0x28 DestroyPluginObject.
    pub destroy: Option<DestroyPluginObjectFn>,
    /// +0x30 value of the forwarded "DoubleFileType" variable (-1 until seen).
    pub double_file_type: i32,
}

/// Find `needle` in the NUL-terminated buffer at `hay` (`strstr`).
unsafe fn strstr(hay: *mut u8, needle: &[u8]) -> *mut u8 {
    let s = cstr(hay);
    if needle.is_empty() {
        return hay;
    }
    match s.windows(needle.len()).position(|w| w == needle) {
        Some(i) => hay.add(i),
        None => core::ptr::null_mut(),
    }
}

/// `strchr(s, c)` (c != 0).
unsafe fn strchr(s: *mut u8, c: u8) -> *mut u8 {
    match cstr(s).iter().position(|&b| b == c) {
        Some(i) => s.add(i),
        None => core::ptr::null_mut(),
    }
}

impl PluginHost {
    pub const fn new() -> Self {
        PluginHost { loaded: false, module: core::ptr::null_mut(), plugin: core::ptr::null_mut(), destroy: None, double_file_type: -1 }
    }

    // ---- forwarding (vftable slots) -------------------------------------

    /// +0x08 Startup.
    pub unsafe fn startup(&self, version: i32) {
        if self.loaded {
            vtbl::call_i32(self.plugin, vtbl::STARTUP, version)
        }
    }
    /// +0x10 Shutdown.
    pub unsafe fn shutdown(&self) {
        if self.loaded {
            vtbl::call0(self.plugin, vtbl::SHUTDOWN)
        }
    }
    /// +0x28 StartSession.
    pub unsafe fn start_session(&self) {
        if self.loaded {
            vtbl::call0(self.plugin, vtbl::START_SESSION)
        }
    }
    /// +0x30 EndSession.
    pub unsafe fn end_session(&self) {
        if self.loaded {
            vtbl::call0(self.plugin, vtbl::END_SESSION)
        }
    }
    /// +0x38 EnterRealtime.
    pub unsafe fn enter_realtime(&self) {
        if self.loaded {
            vtbl::call0(self.plugin, vtbl::ENTER_REALTIME)
        }
    }
    /// +0x40 ExitRealtime.
    pub unsafe fn exit_realtime(&self) {
        if self.loaded {
            vtbl::call0(self.plugin, vtbl::EXIT_REALTIME)
        }
    }
    /// +0x50 UpdateScoring.
    pub unsafe fn update_scoring(&self, info: *const u8) {
        if self.loaded && vtbl::call_ret_u8(self.plugin, vtbl::WANTS_SCORING_UPDATES) != 0 {
            vtbl::call_p(self.plugin, vtbl::UPDATE_SCORING, info)
        }
    }
    /// +0x60 UpdateTelemetry.
    pub unsafe fn update_telemetry(&self, info: *const u8) {
        if self.loaded && vtbl::call_ret_i32(self.plugin, vtbl::WANTS_TELEMETRY_UPDATES) != 0 {
            vtbl::call_p(self.plugin, vtbl::UPDATE_TELEMETRY, info)
        }
    }
    /// +0xb8 SetPhysicsOptions.
    pub unsafe fn set_physics_options(&self, opts: *mut u8) {
        if self.loaded {
            vtbl::call_p(self.plugin, vtbl::SET_PHYSICS_OPTIONS, opts)
        }
    }
    /// +0xd0 WantsToDisplayMessage.
    pub unsafe fn wants_to_display_message(&self, msg: *mut u8) -> u8 {
        if !self.loaded {
            return 0;
        }
        vtbl::call_p_ret_u8(self.plugin, vtbl::WANTS_TO_DISPLAY_MESSAGE, msg)
    }
    /// +0xd8 SetEnvironment.
    pub unsafe fn set_environment(&self, info: *const u8) {
        if self.loaded {
            vtbl::call_p(self.plugin, vtbl::SET_ENVIRONMENT, info)
        }
    }
    /// +0x138 ThreadStarted.
    pub unsafe fn thread_started(&self, ty: i32) {
        if self.loaded {
            vtbl::call_i32(self.plugin, vtbl::THREAD_STARTED, ty)
        }
    }
    /// +0x140 ThreadStopping.
    pub unsafe fn thread_stopping(&self, ty: i32) {
        if self.loaded {
            vtbl::call_i32(self.plugin, vtbl::THREAD_STOPPING, ty)
        }
    }
    /// +0x168 AccessMultiSessionRules: no WantsMultiSessionRulesAccess check.
    pub unsafe fn access_multi_session_rules(&self, info: *mut u8) -> u8 {
        if !self.loaded {
            return 0;
        }
        vtbl::call_p_ret_u8(self.plugin, vtbl::ACCESS_MULTI_SESSION_RULES, info)
    }
    /// +0x178 AccessTrackRules: no WantsTrackRulesAccess check.
    pub unsafe fn access_track_rules(&self, info: *mut u8) -> u8 {
        if !self.loaded {
            return 0;
        }
        vtbl::call_p_ret_u8(self.plugin, vtbl::ACCESS_TRACK_RULES, info)
    }
    /// +0x188 AccessPitMenu.
    pub unsafe fn access_pit_menu(&self, info: *mut u8) -> u8 {
        if !self.loaded || vtbl::call_ret_u8(self.plugin, vtbl::WANTS_PIT_MENU_ACCESS) == 0 {
            return 0;
        }
        vtbl::call_p_ret_u8(self.plugin, vtbl::ACCESS_PIT_MENU, info)
    }

    // ---- life cycle -------------------------------------------------------

    /// Cleanup: destroys the hosted plugin and unloads its library.
    pub unsafe fn cleanup(&mut self, cx: Ctx) {
        if !self.loaded {
            return;
        }
        if let Some(destroy) = self.destroy {
            let obj = if self.plugin.is_null() { core::ptr::null_mut() } else { self.plugin.add(8) };
            destroy(obj);
        }
        self.plugin = core::ptr::null_mut();
        if cx.sys.free_library(self.module) != 1 {
            cx.dbg(level::ERRORS, b"PluginHost::Cleanup", 0x97, b"Failed to unload StockCarRules.dll library");
            cx.trace_last_win32_error();
        }
        self.module = core::ptr::null_mut();
        self.loaded = false;
        cx.dbg(level::CRITICAL_INFO, b"PluginHost::Cleanup", 0x9e, b"Successfully unloaded StockCarRules.dll plugin.");
    }

    /// GetFileContents: the whole file, NUL-terminated, in a
    /// buffer from operator new; NULL on failure.
    pub unsafe fn get_file_contents(cx: Ctx, path: &[u8]) -> *mut u8 {
        const FUNC: &[u8] = b"PluginHost::GetFileContents";
        let sys = cx.sys;
        let g = cx.g();
        let f = match CrtFile::open(sys, &mut g.errno, path, OpenMode::ReadSecure) {
            Some(f) => f,
            None => {
                let e = cx.g().errno;
                cx.dbg_d(level::ERRORS, FUNC, 0x15d, b"fopen_s() failed with:", e);
                return core::ptr::null_mut();
            }
        };
        struct ReadFile {
            cx: Ctx,
            file: Option<CrtFile>,
        }
        unsafe fn read(context: *mut core::ffi::c_void) -> *mut u8 {
            let read = &mut *(context as *mut ReadFile);
            let cx = read.cx;
            let sys = cx.sys;
            let f = read.file.as_mut().expect("file exists until cleanup");
            match f.seek_end(sys) {
                Err(()) => {
                    cx.dbg_d(level::ERRORS, FUNC, 0x163, b"fseek() failed with:", -1);
                    core::ptr::null_mut()
                }
                Ok(size) => {
                    // ftell returns a long.
                    let size = size as i32 as isize as usize;
                    f.rewind(sys);
                    let buf = crate::allocation::operator_new(sys, size.wrapping_add(1));
                    if f.read_exact_one(sys, buf, size) == 1 {
                        *buf.add(size) = 0;
                        buf
                    } else {
                        sys.free(buf);
                        cx.dbg(level::ERRORS, FUNC, 0x16f, b"fread() failed.");
                        core::ptr::null_mut()
                    }
                }
            }
        }
        // C++ exceptions cross the Windows callback without Rust's abort-on-unwind
        // guard. Host tests use C-unwind for the fake exception payload.
        #[cfg(windows)]
        unsafe extern "C" fn body(context: *mut core::ffi::c_void) -> *mut u8 {
            read(context)
        }
        #[cfg(not(windows))]
        unsafe extern "C-unwind" fn body(context: *mut core::ffi::c_void) -> *mut u8 {
            read(context)
        }
        unsafe extern "C" fn cleanup(context: *mut core::ffi::c_void) {
            let read = &mut *(context as *mut ReadFile);
            if let Some(file) = read.file.take() {
                let r = file.close(read.cx.sys);
                if r != 0 {
                    read.cx.dbg_d(
                        level::ERRORS,
                        b"PluginHost::GetFileContents::<lambda_5e93d10f80b14f0d7ff55014dce0bc5b>::operator ()",
                        0x155,
                        b"fclose() failed with:",
                        r,
                    );
                }
            }
        }
        let mut read = ReadFile { cx, file: Some(f) };
        crate::allocation::with_cleanup(&mut read as *mut ReadFile as *mut core::ffi::c_void, body, cleanup)
    }

    /// IsPluginDisabled: finds `plugin` in the JSON text and
    /// scans its lines for ` " Enabled":0` before a line with `}`.
    pub unsafe fn is_plugin_disabled(buf: *mut u8, plugin: &[u8], cfg: &mut *mut u8) -> bool {
        let mut p = strstr(buf, &plugin[..plugin.len() - 1]);
        *cfg = p;
        if p.is_null() {
            return false;
        }
        while !p.is_null() {
            let eol = strstr(p, b"\r\n");
            if !eol.is_null() {
                *eol = 0;
            }
            let restore = |eol: *mut u8| {
                if !eol.is_null() {
                    *eol = b'\r';
                }
            };
            if !strchr(p, b'}').is_null() {
                restore(eol);
                return false;
            }
            if !strstr(p, DISABLED_NEEDLE).is_null() {
                restore(eol);
                return true;
            }
            p = if eol.is_null() { core::ptr::null_mut() } else { eol.add(2) };
            restore(eol);
        }
        false
    }

    /// ForwardPluginConfig: passes every `"name":number` line
    /// of the plugin's block to its AccessCustomVariable.
    pub unsafe fn forward_plugin_config(&mut self, cx: Ctx, cfg: *mut u8, plugin: *mut u8) -> bool {
        const FUNC: &[u8] = b"PluginHost::ForwardPluginConfig";
        if cfg.is_null() {
            return false;
        }
        // char varName[128] lives across lines (only terminated on a
        // successful "name": parse).
        let mut var_name = [0u8; 0x80];
        let mut line = cfg;
        while !line.is_null() {
            let eol = strstr(line, b"\r\n");
            if !eol.is_null() {
                *eol = 0;
            }
            let restore = |eol: *mut u8| {
                if !eol.is_null() {
                    *eol = b'\r';
                }
            };
            if !strchr(line, b'}').is_null() {
                restore(eol);
                return true;
            }
            let q = strchr(line, b'"');
            if q.is_null() {
                cx.dbg_s(level::ERRORS, FUNC, 0x1b5, b"Failed to locate opening quoute in line:", cstr(line));
                restore(eol);
                return false;
            }
            let mut q = q.add(1);
            let mut n = 0usize;
            let mut is_var = true;
            loop {
                let c = *q;
                if c == 0 || c == b'"' {
                    break;
                }
                if c == b'{' {
                    is_var = false;
                    break;
                }
                if n > 0x7e {
                    cx.dbg_s(level::ERRORS, FUNC, 0x1c6, b"Exceeded max variable length in line:", cstr(line));
                    restore(eol);
                    return false;
                }
                var_name[n] = c;
                n += 1;
                q = q.add(1);
            }
            if is_var {
                let mut value: i32 = 0;
                if *q == b'"' && *q.add(1) == b':' && {
                    q = q.add(2);
                    *q != 0
                } {
                    var_name[n] = 0;
                    let mut num = [0u8; 7];
                    let mut nd = 0usize;
                    loop {
                        let c = *q;
                        if c == 0 {
                            break;
                        }
                        if c == b'\r' || c == b',' || !crt::isdigit(c) {
                            break;
                        }
                        if nd > 5 {
                            cx.dbg_s(level::ERRORS, FUNC, 0x1d8, b"Exceeded max number length in line:", cstr(line));
                            restore(eol);
                            return false;
                        }
                        num[nd] = c;
                        nd += 1;
                        q = q.add(1);
                    }
                    num[nd] = 0;
                    let g = cx.g();
                    value = cx.sys.crt_atol(&num[..=nd], &mut g.errno);
                    if value == 0 && (cx.sys.crt_errno(g.errno) == crt::EINVAL || cx.sys.crt_errno(g.errno) == crt::ERANGE) {
                        cx.dbg_s(
                            level::ERRORS,
                            FUNC,
                            0x1e5,
                            b"Failed to convert line value part to number:",
                            cstr(line),
                        );
                        restore(eol);
                        return false;
                    }
                }
                let name_len = var_name.iter().position(|&c| c == 0).unwrap_or(var_name.len());
                let name = &var_name[..name_len];
                if name == b" Enabled" {
                    value = 1;
                } else if name == b"DoubleFileType" {
                    self.double_file_type = value;
                }
                // CustomVariableV01 (mNumSettings is not set).
                let mut var = [0u8; custom_var::SIZE];
                var[..name_len].copy_from_slice(name);
                var[custom_var::CURRENT_SETTING..custom_var::CURRENT_SETTING + 4].copy_from_slice(&value.to_le_bytes());
                vtbl::call_p(plugin, vtbl::ACCESS_CUSTOM_VARIABLE, var.as_mut_ptr());
                cx.dbg_d(level::CRITICAL_INFO, FUNC, 500, name, value);
            }
            line = if eol.is_null() { core::ptr::null_mut() } else { eol.add(2) };
            restore(eol);
        }
        false
    }

    /// Initialize: loads and configures the hosted plugin.
    pub unsafe fn initialize(&mut self, cx: Ctx, enable: bool) {
        if !enable {
            return;
        }
        // The path buffer is char[0x110] with the stack cookie right after
        // it. lstrcatA has no bound, so with a working directory
        // of 229 characters or more (229 + 43 + NUL > 0x110; 240 or more for
        // the 32-character StockCarRules.dll path) the path runs over the
        // cookie: the full path is used, and the process ends
        // when Initialize returns. `path` holds the longest case (0x103 + 43
        // + NUL); `smashed` records a write at offset 0x110 or beyond.
        let mut path = [0u8; 0x104 + 0x30];
        let mut smashed = false;
        self.initialize_frame(cx, &mut path, &mut smashed);
        cx.gs_check(smashed);
    }

    /// Initialize's body from the first GetCurrentDirectoryA on; every return
    /// goes through the cookie check in [`PluginHost::initialize`].
    unsafe fn initialize_frame(&mut self, cx: Ctx, path: &mut [u8; 0x134], smashed: &mut bool) {
        const FUNC: &[u8] = b"PluginHost::Initialize";
        const FRAME: usize = 0x110;
        let sys = cx.sys;
        let json = {
            sys.current_directory(&mut path[..0x104]);
            *smashed |= lstrcat(&mut path[..], CUSTOM_PLUGIN_VARIABLES_REL) >= FRAME;
            Self::get_file_contents(cx, &path[..])
        };
        if json.is_null() {
            cx.dbg(level::ERRORS, FUNC, 0x1f, b"Failed to load CustomPluginVariables.JSON file");
            return;
        }
        let mut cfg: *mut u8 = core::ptr::null_mut();
        let disabled = Self::is_plugin_disabled(json, SCR_PLUGIN_NAME, &mut cfg);
        if !disabled || cfg.is_null() {
            cx.dbg(level::ERRORS, FUNC, 0x29, b"Error: StockCarRules.dll plugin is not disabled in CustomPluginVariables.JSON");
            sys.free(json);
            return;
        }
        sys.current_directory(&mut path[..0x104]);
        *smashed |= lstrcat(&mut path[..], SCR_PLUGIN_REL) >= FRAME;
        let path_len = path.iter().position(|&c| c == 0).unwrap_or(path.len() - 1);
        self.module = sys.load_library(&path[..path_len + 1]);
        if self.module.is_null() {
            cx.dbg(level::ERRORS, FUNC, 0x34, b"Failed to load StockCarRules.dll library");
            cx.trace_last_win32_error();
            sys.free(json);
            return;
        }
        let mut dismissed = false;
        'load: {
            let get_name = sys.get_proc_address(self.module, b"GetPluginName\0");
            if get_name.is_null() {
                cx.dbg(level::ERRORS, FUNC, 0x47, b"Function GetPluginName not found.");
                break 'load;
            }
            let get_type = sys.get_proc_address(self.module, b"GetPluginType\0");
            if get_type.is_null() {
                cx.dbg(level::ERRORS, FUNC, 0x4d, b"Function GetPluginType not found.");
                break 'load;
            }
            let get_version = sys.get_proc_address(self.module, b"GetPluginVersion\0");
            if get_version.is_null() {
                cx.dbg(level::ERRORS, FUNC, 0x53, b"Function GetPluginVersion not found.");
                break 'load;
            }
            let create = sys.get_proc_address(self.module, b"CreatePluginObject\0");
            if create.is_null() {
                cx.dbg(level::ERRORS, FUNC, 0x59, b"Function CreatePluginObject not found.");
                break 'load;
            }
            let destroy = sys.get_proc_address(self.module, b"DestroyPluginObject\0");
            self.destroy = if destroy.is_null() {
                None
            } else {
                Some(core::mem::transmute::<*const c_void, DestroyPluginObjectFn>(destroy))
            };
            if destroy.is_null() {
                cx.dbg(level::ERRORS, FUNC, 0x5f, b"Function DestroyPluginObject not found.");
                break 'load;
            }
            let ty = core::mem::transmute::<*const c_void, GetPluginTypeFn>(get_type)();
            if ty != 3 {
                cx.dbg_d(level::ERRORS, FUNC, 0x67, b"Unexpected plugin type.", ty);
                break 'load;
            }
            let ver = core::mem::transmute::<*const c_void, GetPluginVersionFn>(get_version)();
            if ver != 7 {
                cx.dbg_d(level::ERRORS, FUNC, 0x6d, b"Unexpected plugin version.", ver);
                break 'load;
            }
            let obj = core::mem::transmute::<*const c_void, CreatePluginObjectFn>(create)();
            self.plugin = if obj.is_null() { core::ptr::null_mut() } else { obj.sub(8) };
            if self.plugin.is_null() {
                cx.dbg(level::ERRORS, FUNC, 0x73, b"Failed to create plugin object.");
                break 'load;
            }
            cx.dbg(level::CRITICAL_INFO, FUNC, 0x77, b"Forwarding StockCarRules.dll plugin configuration:");
            if !self.forward_plugin_config(cx, cfg, self.plugin) {
                cx.dbg(level::ERRORS, FUNC, 0x79, b"Failed to forward StockCarRules.dll plugin configuration.");
                break 'load;
            }
            let name = core::mem::transmute::<*const c_void, GetPluginNameFn>(get_name)();
            cx.dbg_s(
                level::CRITICAL_INFO,
                FUNC,
                0x7e,
                b"Successfully loaded StockCarRules.dll plugin.  Name:",
                cstr(name),
            );
            self.loaded = true;
            dismissed = true;
        }
        // Scope guard: unload the library unless loading completed.
        if !dismissed && !self.loaded && sys.free_library(self.module) != 1 {
            cx.dbg(
                level::ERRORS,
                b"PluginHost::Initialize::<lambda_9d571723a1f0e6178dbd826529f5db36>::operator ()",
                0x3d,
                b"Failed to unload StockCarRules.dll library",
            );
            cx.trace_last_win32_error();
        }
        sys.free(json);
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

/// `lstrcatA(buf, s)` (s NUL-terminated); the offset of the NUL it wrote.
/// lstrcatA has no bound: `buf` must hold the result (it is truncated only
/// past `buf`, which callers size for their longest case).
fn lstrcat(buf: &mut [u8], s: &[u8]) -> usize {
    let start = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())];
    let n = s.len().min(buf.len().saturating_sub(start + 1));
    buf[start..start + n].copy_from_slice(&s[..n]);
    buf[start + n] = 0;
    start + n
}
