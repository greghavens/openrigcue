//! Host-side tests: a scripted [`Sys`] records every OS call; the plugin is
//! driven through its vtable exactly as rFactor 2 calls it and the mapped
//! bytes, log files and call sequences are compared with fixed expected
//! values.

use crate::cfmt::{self, Arg};
use crate::layout::*;
use crate::log::{Ctx, Globals};
use crate::sys::{Handle, LocalTime, OpenMode, Sys};
use crate::vtbl;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::c_void;

std::thread_local! {
    static NATIVE_ERRNO: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    static CALLBACK_ERRNO: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

struct NativeErrno;
impl NativeErrno {
    fn enter(value: i32) -> Self {
        NATIVE_ERRNO.set(Some(value));
        Self
    }
}
impl Drop for NativeErrno {
    fn drop(&mut self) {
        NATIVE_ERRNO.set(None);
        CALLBACK_ERRNO.set(None);
    }
}

// ---------------------------------------------------------------------------
// Mock OS

pub struct Mapping {
    pub name: Vec<u8>,
    pub size: usize,
    pub sd: bool,
    pub data: Box<[u64]>,
}

#[derive(Default)]
pub struct State {
    pub calls: Vec<String>,
    pub mappings: Vec<Mapping>,
    pub files: BTreeMap<String, Vec<u8>>,
    pub handles: BTreeMap<usize, (String, OpenMode, usize)>,
    pub next_handle: usize,
    pub module_name: String,
    pub cur_dir: String,
    pub pid: u32,
    pub tid: u32,
    pub qpc: i64,
    pub ticks: u64,
    pub time: LocalTime,
    pub last_error: u32,
    pub fail_mapping: Option<String>,
    pub libraries: BTreeMap<String, Vec<(String, usize)>>,
    pub freed_libraries: usize,
    pub allocs: Vec<(usize, usize)>,
    pub allocation_failures: usize,
    pub new_handler_results: Vec<bool>,
    pub new_handler_throws: bool,
    pub errno_reads: usize,
}

pub struct MockSys(pub RefCell<State>);

impl MockSys {
    pub fn new() -> &'static MockSys {
        let st = State {
            module_name: "C:\\Games\\rFactor 2\\Bin64\\rFactor2.exe".into(),
            cur_dir: "C:\\Games\\rFactor 2".into(),
            pid: 4321,
            tid: 0x1a2b,
            next_handle: 0x100,
            time: LocalTime { hour: 9, minute: 5, second: 7, milliseconds: 42 },
            ..Default::default()
        };
        Box::leak(Box::new(MockSys(RefCell::new(st))))
    }
    pub fn st(&self) -> std::cell::RefMut<'_, State> {
        self.0.borrow_mut()
    }
    pub fn mapping(&self, name: &str) -> *mut u8 {
        let st = self.0.borrow();
        let m = st.mappings.iter().find(|m| m.name == name.as_bytes()).unwrap_or_else(|| panic!("no mapping {name}"));
        m.data.as_ptr() as *mut u8
    }
    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.0.borrow().files.get(path).cloned()
    }
    fn log(&self, s: String) {
        self.0.borrow_mut().calls.push(s);
    }
}

fn s(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

impl Sys for MockSys {
    fn crt_errno(&self, fallback: i32) -> i32 {
        self.st().errno_reads += 1;
        NATIVE_ERRNO.get().unwrap_or(fallback)
    }
    fn set_crt_errno(&self, value: i32) {
        if NATIVE_ERRNO.get().is_some() {
            NATIVE_ERRNO.set(Some(value));
        }
    }
    unsafe fn malloc(&self, size: usize) -> *mut u8 {
        if self.st().allocation_failures > 0 {
            self.st().allocation_failures -= 1;
            return core::ptr::null_mut();
        }
        let layout = std::alloc::Layout::from_size_align(size.max(1), 16).unwrap();
        let p = std::alloc::alloc(layout);
        self.st().allocs.push((p as usize, size.max(1)));
        p
    }
    unsafe fn free(&self, p: *mut u8) {
        let mut st = self.st();
        let i = st.allocs.iter().position(|a| a.0 == p as usize).expect("free of unknown block");
        let (_, size) = st.allocs.remove(i);
        std::alloc::dealloc(p, std::alloc::Layout::from_size_align(size, 16).unwrap());
    }
    fn call_new_handler(&self, size: usize) -> bool {
        self.log(format!("_callnewh({size})"));
        if self.st().new_handler_throws {
            std::panic::panic_any("new-handler-exception");
        }
        if self.st().new_handler_results.is_empty() {
            false
        } else {
            self.st().new_handler_results.remove(0)
        }
    }
    fn allocation_failure(&self, failure: crate::allocation::Failure) -> ! {
        self.log(format!("throw {failure:?}"));
        std::panic::panic_any(failure)
    }
    unsafe fn convert_sddl(&self, sddl: &[u8]) -> *mut c_void {
        self.log(format!("ConvertStringSecurityDescriptor({})", s(sddl)));
        0x5d as *mut c_void
    }
    unsafe fn local_free(&self, p: *mut c_void) {
        self.log(format!("LocalFree({:#x})", p as usize));
    }
    unsafe fn create_file_mapping(&self, sd: Option<*mut c_void>, size: u32, name: &[u8]) -> Handle {
        let name_s = s(name);
        self.log(format!("CreateFileMapping({name_s}, {size:#x}, sd={})", sd.is_some()));
        let mut st = self.st();
        if st.fail_mapping.as_deref() == Some(name_s.as_str()) {
            st.last_error = 5;
            return core::ptr::null_mut();
        }
        let exists = st.mappings.iter().any(|m| m.name == name_s.as_bytes());
        st.last_error = if exists { 183 } else { 0 };
        if !exists {
            st.mappings.push(Mapping {
                name: name_s.as_bytes().to_vec(),
                size: size as usize,
                sd: sd.is_some(),
                data: vec![0xa5a5a5a5a5a5a5a5u64; size as usize / 8 + 1].into_boxed_slice(),
            });
        }
        let i = st.mappings.iter().position(|m| m.name == name_s.as_bytes()).unwrap();
        (0x1000 + i) as Handle
    }
    unsafe fn map_view(&self, h: Handle, size: usize) -> *mut u8 {
        self.log(format!("MapViewOfFile({:#x}, {size:#x})", h as usize));
        let st = self.0.borrow();
        st.mappings[h as usize - 0x1000].data.as_ptr() as *mut u8
    }
    unsafe fn unmap_view(&self, _p: *mut u8) -> bool {
        self.log("UnmapViewOfFile".into());
        true
    }
    unsafe fn close_handle(&self, h: Handle) -> bool {
        self.log(format!("CloseHandle({:#x})", h as usize));
        true
    }
    fn get_last_error(&self) -> u32 {
        self.0.borrow().last_error
    }
    unsafe fn format_message(&self, _err: u32) -> (u32, *mut u8) {
        (0, core::ptr::null_mut())
    }
    fn module_file_name(&self, buf: &mut [u8; 0x400]) {
        let st = self.0.borrow();
        buf[..st.module_name.len()].copy_from_slice(st.module_name.as_bytes());
    }
    fn current_directory(&self, buf: &mut [u8]) {
        let st = self.0.borrow();
        buf[..st.cur_dir.len()].copy_from_slice(st.cur_dir.as_bytes());
        buf[st.cur_dir.len()] = 0;
    }
    fn current_process_id(&self) -> u32 {
        self.0.borrow().pid
    }
    fn current_thread_id(&self) -> u32 {
        self.0.borrow().tid
    }
    unsafe fn load_library(&self, path: &[u8]) -> Handle {
        let p = s(path);
        self.log(format!("LoadLibraryExA({p})"));
        if self.0.borrow().libraries.contains_key(&p) {
            0x7000 as Handle
        } else {
            self.st().last_error = 126;
            core::ptr::null_mut()
        }
    }
    unsafe fn get_proc_address(&self, _h: Handle, name: &[u8]) -> *const c_void {
        let st = self.0.borrow();
        let n = s(name);
        for procs in st.libraries.values() {
            if let Some((_, f)) = procs.iter().find(|(k, _)| *k == n) {
                return *f as *const c_void;
            }
        }
        core::ptr::null()
    }
    unsafe fn free_library(&self, _h: Handle) -> i32 {
        self.log("FreeLibrary".into());
        self.st().freed_libraries += 1;
        1
    }
    fn query_performance_frequency(&self) -> i64 {
        10_000_000
    }
    fn query_performance_counter(&self) -> i64 {
        let mut st = self.st();
        st.qpc += 1000;
        st.qpc
    }
    fn tick_count64(&self) -> u64 {
        self.0.borrow().ticks
    }
    fn local_time(&self) -> LocalTime {
        self.0.borrow().time
    }
    fn atan2(&self, y: f64, x: f64) -> f64 {
        y.atan2(x)
    }
    unsafe fn file_open(&self, path: &[u8], mode: OpenMode) -> Result<Handle, u32> {
        let p = s(path);
        self.log(format!("open({p}, {mode:?})"));
        let mut st = self.st();
        match mode {
            OpenMode::ReadSecure if !st.files.contains_key(&p) => return Err(2),
            OpenMode::Write => {
                st.files.insert(p.clone(), Vec::new());
            }
            OpenMode::Append => {
                st.files.entry(p.clone()).or_default();
            }
            _ => {}
        }
        let h = st.next_handle;
        st.next_handle += 1;
        st.handles.insert(h, (p, mode, 0));
        Ok(h as Handle)
    }
    unsafe fn file_write(&self, h: Handle, data: &[u8]) -> bool {
        let mut st = self.st();
        let path = st.handles[&(h as usize)].0.clone();
        st.files.get_mut(&path).unwrap().extend_from_slice(data);
        true
    }
    unsafe fn file_read(&self, h: Handle, buf: &mut [u8]) -> Result<usize, u32> {
        let mut st = self.st();
        let (path, _, pos) = st.handles[&(h as usize)].clone();
        let data = &st.files[&path];
        let n = buf.len().min(data.len() - pos);
        buf[..n].copy_from_slice(&data[pos..pos + n]);
        st.handles.get_mut(&(h as usize)).unwrap().2 = pos + n;
        Ok(n)
    }
    unsafe fn file_seek(&self, h: Handle, off: i64, whence: u32) -> Result<i64, u32> {
        let mut st = self.st();
        let (path, _, _) = st.handles[&(h as usize)].clone();
        let len = st.files[&path].len() as i64;
        let pos = if whence == 2 { len + off } else { off };
        st.handles.get_mut(&(h as usize)).unwrap().2 = pos as usize;
        Ok(pos)
    }
    unsafe fn file_close(&self, h: Handle) -> bool {
        self.log("close".into());
        self.st().handles.remove(&(h as usize)).is_some()
    }
    unsafe fn file_delete(&self, path: &[u8]) -> Result<(), u32> {
        let p = s(path);
        self.log(format!("DeleteFileA({p})"));
        if self.st().files.remove(&p).is_some() {
            Ok(())
        } else {
            Err(2)
        }
    }
    fn invalid_parameter(&self) -> ! {
        panic!("invalid parameter handler")
    }
    fn report_gs_failure(&self) -> ! {
        self.log("__report_gsfailure".into());
        std::panic::panic_any(GsFailure)
    }
}

#[test]
fn native_allocation_failure_is_not_an_invalid_parameter() {
    let sys = MockSys::new();
    sys.st().allocation_failures = 1;
    let globals = Box::leak(Box::new(Globals::new()));
    let cx = Ctx { sys, g: globals };
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { crate::create_plugin_object(cx) }))
        .unwrap_err();
    assert_eq!(failure.downcast_ref(), Some(&crate::allocation::Failure::BadAlloc));
    assert_eq!(
        sys.st().calls,
        [format!("_callnewh({})", core::mem::size_of::<crate::plugin::SharedMemoryPlugin>()), "throw BadAlloc".into()]
    );
}

#[test]
fn native_allocation_retries_only_when_new_handler_requests_it() {
    let sys = MockSys::new();
    sys.st().allocation_failures = 2;
    sys.st().new_handler_results = vec![true, true];
    let pointer = unsafe { crate::allocation::operator_new(sys, 24) };
    assert!(!pointer.is_null());
    assert_eq!(sys.st().calls, ["_callnewh(24)", "_callnewh(24)"]);
    unsafe { sys.free(pointer) };
    assert!(sys.st().allocs.is_empty());

    sys.st().allocation_failures = 1;
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        crate::allocation::operator_new(sys, usize::MAX)
    }))
    .unwrap_err();
    assert_eq!(failure.downcast_ref(), Some(&crate::allocation::Failure::BadArrayNewLength));
}

#[test]
fn native_throwing_new_handler_preserves_file_cleanup_and_exception() {
    let sys = MockSys::new();
    sys.st().files.insert("config.json".into(), b"{}".to_vec());
    sys.st().allocation_failures = 1;
    sys.st().new_handler_throws = true;
    let globals = Box::leak(Box::new(Globals::new()));
    let cx = Ctx { sys, g: globals };
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        crate::host::PluginHost::get_file_contents(cx, b"config.json\0")
    }))
    .unwrap_err();
    assert_eq!(failure.downcast_ref::<&str>(), Some(&"new-handler-exception"));
    assert!(sys.st().handles.is_empty());
    assert_eq!(sys.st().calls.last().map(String::as_str), Some("close"));
    assert!(!sys.st().calls.iter().any(|call| call.starts_with("throw ")));
}

/// Panic payload of the mock's stack-cookie failure.
pub struct GsFailure;

/// Runs `f` and reports whether it ended in the /GS cookie failure; any other
/// panic is re-raised.
fn ends_in_gs_failure(f: impl FnOnce()) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(()) => false,
        Err(e) if e.is::<GsFailure>() => true,
        Err(e) => std::panic::resume_unwind(e),
    }
}

// ---------------------------------------------------------------------------
// Driving the plugin through its vtable, as the game does.

pub struct Game {
    pub sys: &'static MockSys,
    pub cx: Ctx,
    /// The PluginObject * CreatePluginObject returned.
    pub obj: *mut c_void,
}

impl Game {
    pub fn new() -> Game {
        Self::with(|_| {})
    }
    pub fn with(f: impl FnOnce(&mut Globals)) -> Game {
        let sys = MockSys::new();
        let g = Box::leak(Box::new(Globals::new()));
        f(g);
        let cx = Ctx { sys, g };
        let obj = unsafe { crate::create_plugin_object(cx) };
        Game { sys, cx, obj }
    }
    pub fn this(&self) -> *mut u8 {
        unsafe { (self.obj as *mut u8).sub(8) }
    }
    pub fn plugin(&self) -> &mut crate::plugin::SharedMemoryPlugin {
        unsafe { &mut *(self.this() as *mut crate::plugin::SharedMemoryPlugin) }
    }
    fn slot(&self, off: usize) -> *const () {
        unsafe { *(*(self.this() as *const *const *const ())).add(off / 8) }
    }
    pub fn call0(&self, off: usize) {
        unsafe { vtbl::call0(self.this(), off) }
    }
    pub fn call_i(&self, off: usize, a: i32) {
        unsafe { vtbl::call_i32(self.this(), off, a) }
    }
    pub fn call_p(&self, off: usize, p: *const u8) {
        unsafe { vtbl::call_p(self.this(), off, p) }
    }
    pub fn call_p_u8(&self, off: usize, p: *mut u8) -> u8 {
        unsafe { vtbl::call_p_ret_u8(self.this(), off, p) }
    }
    pub fn startup(&self) {
        self.call_i(vtbl::STARTUP, 1100);
    }
    pub fn telemetry(&self) -> *mut u8 {
        self.sys.mapping("$rFactor2SMMP_Telemetry$")
    }
    pub fn scoring(&self) -> *mut u8 {
        self.sys.mapping("$rFactor2SMMP_Scoring$")
    }
    pub fn rules(&self) -> *mut u8 {
        self.sys.mapping("$rFactor2SMMP_Rules$")
    }
    pub fn multi_rules(&self) -> *mut u8 {
        self.sys.mapping("$rFactor2SMMP_MultiRules$")
    }
    pub fn extended(&self) -> *mut u8 {
        self.sys.mapping("$rFactor2SMMP_Extended$")
    }
    /// The debug log as on disk after an fflush of the cached stream.
    pub fn debug_log(&self) -> String {
        if let crate::crt::StreamSlot::Open(f) = &mut self.cx.g().debug_file {
            unsafe { f.flush(self.sys) };
        }
        String::from_utf8(self.sys.file("UserData\\Log\\RF2SMMP_DebugOutput.txt").unwrap_or_default()).unwrap()
    }
}

/// A TelemInfoV01 with recognisable bytes.
fn telem_info(id: i32, et: f64) -> Vec<u8> {
    let mut v: Vec<u8> = (0..telem::SIZE).map(|i| (i * 7 + id as usize) as u8).collect();
    v[telem::ID..telem::ID + 4].copy_from_slice(&id.to_le_bytes());
    v[telem::ELAPSED_TIME..telem::ELAPSED_TIME + 8].copy_from_slice(&et.to_le_bytes());
    v[telem::LAST_IMPACT_ET..telem::LAST_IMPACT_ET + 8].copy_from_slice(&0.0f64.to_le_bytes());
    v
}

struct Scoring {
    info: Vec<u8>,
    vehicles: Vec<u8>,
    stream: Vec<u8>,
}

fn scoring_info(n: i32, et: f64) -> Box<Scoring> {
    let mut sc = Box::new(Scoring {
        info: (0..scoring_info::SIZE).map(|i| (i * 3) as u8).collect(),
        vehicles: (0..vehicle_scoring::SIZE * n.max(0) as usize).map(|i| (i * 5) as u8).collect(),
        stream: b"<results/>\0".to_vec(),
    });
    let i = &mut sc.info;
    i[scoring_info::NUM_VEHICLES..scoring_info::NUM_VEHICLES + 4].copy_from_slice(&n.to_le_bytes());
    i[scoring_info::CURRENT_ET..scoring_info::CURRENT_ET + 8].copy_from_slice(&et.to_le_bytes());
    for k in 0..n.max(0) as usize {
        let v = &mut sc.vehicles[k * vehicle_scoring::SIZE..];
        v[..4].copy_from_slice(&(k as i32 + 10).to_le_bytes());
        v[vehicle_scoring::PIT_STATE] = 0;
    }
    let vp = sc.vehicles.as_ptr() as usize;
    let sp = sc.stream.as_ptr() as usize;
    sc.info[scoring_info::VEHICLE..scoring_info::VEHICLE + 8].copy_from_slice(&vp.to_le_bytes());
    sc.info[scoring_info::RESULTS_STREAM..scoring_info::RESULTS_STREAM + 8].copy_from_slice(&sp.to_le_bytes());
    sc
}

unsafe fn bytes<'a>(p: *const u8, n: usize) -> &'a [u8] {
    std::slice::from_raw_parts(p, n)
}

// ---------------------------------------------------------------------------
// Exports and vtable

#[test]
fn exports_match_original() {
    assert_eq!(crate::PLUGIN_NAME, b"rFactor 2 Shared Memory Map Plugin - v3.0\0");
    assert_eq!(crate::PLUGIN_TYPE, 3);
    assert_eq!(crate::PLUGIN_VERSION, 7);
    let g = Game::new();
    // CreatePluginObject returns object + 8 (PluginObject base after the vfptr).
    assert_eq!(g.obj as usize - g.this() as usize, 8);
    assert_eq!(g.plugin().vtable as *const _ as usize, &crate::plugin::VTABLE as *const _ as usize);
    let n = g.sys.st().allocs.len();
    unsafe { crate::destroy_plugin_object(g.obj) };
    assert_eq!(g.sys.st().allocs.len(), n - 1, "scalar deleting destructor frees the object");
    unsafe { crate::destroy_plugin_object(core::ptr::null_mut()) };
}

#[test]
fn vtable_default_slots() {
    let g = Game::new();
    let ret_u8 = |off| unsafe { vtbl::call_ret_u8(g.this(), off) };
    for off in [
        vtbl::WANTS_SCORING_UPDATES,
        vtbl::WANTS_MULTI_SESSION_RULES_ACCESS,
        vtbl::WANTS_TRACK_RULES_ACCESS,
        vtbl::WANTS_PIT_MENU_ACCESS,
    ] {
        assert_eq!(ret_u8(off), 1, "slot {off:#x}");
    }
    for off in [
        vtbl::WANTS_GRAPHICS_UPDATES,
        vtbl::REQUEST_COMMENTARY,
        vtbl::HAS_HARDWARE_INPUTS,
        vtbl::CHECK_HW_CONTROL,
        vtbl::FORCE_FEEDBACK,
        vtbl::WANTS_TO_VIEW_VEHICLE,
        vtbl::INIT_CUSTOM_CONTROL,
        vtbl::WANTS_WEATHER_ACCESS,
        vtbl::ACCESS_WEATHER,
    ] {
        assert_eq!(ret_u8(off), 0, "slot {off:#x}");
    }
    assert_eq!(unsafe { vtbl::call_ret_i32(g.this(), vtbl::WANTS_TELEMETRY_UPDATES) }, 2);
    for off in [vtbl::LOAD, vtbl::UNLOAD, vtbl::UPDATE_GRAPHICS, vtbl::ERROR, vtbl::INIT_SCREEN, vtbl::POST_RESET] {
        g.call0(off);
    }
    assert_eq!(g.slot(vtbl::LOAD), g.slot(vtbl::UPDATE_GRAPHICS_V02));
    // Unmapped: every callback returns before touching state.
    g.call0(vtbl::START_SESSION);
    g.call0(vtbl::ENTER_REALTIME);
    assert!(g.sys.st().mappings.is_empty());
}

// ---------------------------------------------------------------------------
// Startup / mappings

#[test]
fn startup_maps_five_buffers() {
    let g = Game::new();
    g.startup();
    let st = g.sys.st();
    let maps: Vec<_> = st.mappings.iter().map(|m| (s(&m.name), m.size, m.sd)).collect();
    assert_eq!(
        maps,
        vec![
            ("$rFactor2SMMP_Telemetry$".into(), 0x3b010, false),
            ("$rFactor2SMMP_Scoring$".into(), 0x12630, false),
            ("$rFactor2SMMP_Rules$".into(), 0xb0d8, false),
            ("$rFactor2SMMP_MultiRules$".into(), 0x9b70, false),
            ("$rFactor2SMMP_Extended$".into(), 0x24e4, false),
        ]
    );
    // The sizes the reading application maps: 241 680 / 75 312.
    assert_eq!(TELEMETRY_MAP_SIZE, 241_680);
    assert_eq!(SCORING_MAP_SIZE, 75_312);
    drop(st);
    assert!(g.plugin().is_mapped);
    unsafe {
        // Every mapping zeroed, then ClearState: version counters 1/1 for
        // telemetry, scoring, rules, extended and multi rules.
        for m in [g.telemetry(), g.scoring(), g.rules(), g.multi_rules(), g.extended()] {
            assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (1, 1));
        }
        let e = g.extended().add(8);
        assert_eq!(bytes(e, 8), b"3.0.1.0\0");
        assert_eq!(rd_u8(e, ext::IS_64BIT), 1);
        assert_eq!(rd_i32(e, ext::SCR_DOUBLE_FILE_TYPE), -1);
        assert_eq!(rd_u8(e, ext::SCR_IS_HOSTED), 0);
        // Payload zero apart from those fields.
        let mut want = vec![0u8; EXTENDED_PAYLOAD_SIZE];
        want[..8].copy_from_slice(b"3.0.1.0\0");
        want[8] = 1;
        want[ext::SCR_DOUBLE_FILE_TYPE..ext::SCR_DOUBLE_FILE_TYPE + 4].copy_from_slice(&[0xff; 4]);
        assert_eq!(bytes(e, EXTENDED_PAYLOAD_SIZE), &want[..]);
        assert!(bytes(g.telemetry().add(8), TELEMETRY_PAYLOAD_SIZE).iter().all(|&b| b == 0));
    }
    // Default config: no debug output, no files.
    assert!(g.sys.st().files.is_empty());
}

#[test]
fn startup_map_globally_uses_security_descriptor() {
    let g = Game::with(|g| g.dedicated_server_map_globally = true);
    g.startup();
    let st = g.sys.st();
    assert!(st.mappings.iter().all(|m| m.sd));
    assert!(st
        .calls
        .contains(&"ConvertStringSecurityDescriptor(D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)(A;OICI;GA;;;WD))".into()));
    // Not dedicated: plain names (no Global\ prefix, no pid).
    assert_eq!(s(&st.mappings[0].name), "$rFactor2SMMP_Telemetry$");
}

#[test]
fn dedicated_server_names() {
    let g = Game::new();
    g.sys.st().module_name = "D:\\srv\\Bin64\\rFactor2 Dedicated.exe".into();
    g.startup();
    assert_eq!(s(&g.sys.st().mappings[0].name), "$rFactor2SMMP_Telemetry$4321");
    let g = Game::with(|g| g.dedicated_server_map_globally = true);
    g.sys.st().module_name = "D:\\srv\\Bin64\\rFactor2 Dedicated.exe".into();
    g.startup();
    let st = g.sys.st();
    assert_eq!(s(&st.mappings[1].name), "Global\\$rFactor2SMMP_Scoring$4321");
    assert!(st.mappings[1].sd);
}

#[test]
fn startup_mapping_failure_stops() {
    let g = Game::with(|g| g.debug_level = 1);
    g.sys.st().fail_mapping = Some("$rFactor2SMMP_Rules$".into());
    g.startup();
    assert!(!g.plugin().is_mapped);
    let log = g.debug_log();
    assert!(log.contains("MappedBuffer<struct rF2Rules>::MapMemoryFile(234) : Failed to create file mapping for file: $rFactor2SMMP_Rules$\r\n"), "{log}");
    assert!(log.contains("MappedBuffer<struct rF2Rules>::Initialize(41) : Failed to map file\r\n"));
    assert!(log.contains("SharedMemoryPlugin::Startup(196) : Failed to initialize rules mapping\r\n"));
    assert_eq!(g.sys.st().mappings.len(), 2);
}

// ---------------------------------------------------------------------------
// Telemetry frames

#[test]
fn telemetry_frame_is_copied_and_closed_when_all_scored_vehicles_arrived() {
    let g = Game::new();
    g.startup();
    let sc = scoring_info(3, 10.0);
    g.call_p(vtbl::UPDATE_SCORING, sc.info.as_ptr());
    let t: Vec<_> = (0..3).map(|i| telem_info(20 + i, 10.5)).collect();
    unsafe {
        let m = g.telemetry();
        g.call_p(vtbl::UPDATE_TELEMETRY, t[0].as_ptr());
        // A new frame was begun: Begin = End + 1 while updating.
        assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (2, 1));
        g.call_p(vtbl::UPDATE_TELEMETRY, t[1].as_ptr());
        g.call_p(vtbl::UPDATE_TELEMETRY, t[2].as_ptr());
        assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (2, 2));
        assert_eq!(rd_i32(m, 8 + TEL_BYTES_HINT), 3 * 0x760 + 8);
        assert_eq!(rd_i32(m, 8 + TEL_NUM_VEHICLES), 3);
        for (k, v) in t.iter().enumerate() {
            assert_eq!(bytes(m.add(0x10 + k * 0x760), 0x760), &v[..]);
        }
        // Same vehicle again inside the frame window: skipped.
        g.call_p(vtbl::UPDATE_TELEMETRY, t[0].as_ptr());
        assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (2, 2));
        // ET + 0.02 >= 0.0199: next frame.
        let n = telem_info(20, 10.52);
        g.call_p(vtbl::UPDATE_TELEMETRY, n.as_ptr());
        assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (3, 2));
        // ET within 0.0199 of the frame start: same frame, min() kept.
        let n2 = telem_info(21, 10.515);
        g.call_p(vtbl::UPDATE_TELEMETRY, n2.as_ptr());
        assert_eq!(g.plugin().last_telemetry_update_et, 10.515);
        assert_eq!(g.plugin().cur_telemetry_vehicle_index, 2);
        // ExitRealtime closes the open frame.
        g.call0(vtbl::EXIT_REALTIME);
        assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (3, 3));
        assert_eq!(rd_i32(m, 8 + TEL_NUM_VEHICLES), 2);
        assert_eq!(rd_i32(m, 8), 2 * 0x760 + 8);
    }
}

#[test]
fn telemetry_nan_delta_stays_in_frame() {
    // comisd/jae: an unordered |delta| takes the same-frame path.
    let g = Game::new();
    g.startup();
    let a = telem_info(1, 5.0);
    g.call_p(vtbl::UPDATE_TELEMETRY, a.as_ptr());
    let b = telem_info(2, f64::NAN);
    g.call_p(vtbl::UPDATE_TELEMETRY, b.as_ptr());
    assert_eq!(g.plugin().cur_telemetry_vehicle_index, 2);
    // minsd(last, NaN) = NaN (second operand).
    assert!(g.plugin().last_telemetry_update_et.is_nan());
}

#[test]
fn damage_tracking() {
    let g = Game::new();
    g.startup();
    unsafe {
        let mut t = telem_info(7, 1.0);
        let set = |t: &mut Vec<u8>, et: f64, impact: f64, mag: f64| {
            t[telem::ELAPSED_TIME..telem::ELAPSED_TIME + 8].copy_from_slice(&et.to_le_bytes());
            t[telem::LAST_IMPACT_ET..telem::LAST_IMPACT_ET + 8].copy_from_slice(&impact.to_le_bytes());
            t[telem::LAST_IMPACT_MAGNITUDE..telem::LAST_IMPACT_MAGNITUDE + 8].copy_from_slice(&mag.to_le_bytes());
        };
        set(&mut t, 1.0, 0.5, 100.0);
        g.call_p(vtbl::UPDATE_TELEMETRY, t.as_ptr());
        set(&mut t, 2.0, 0.5, 100.0); // same impact: not counted again
        g.call_p(vtbl::UPDATE_TELEMETRY, t.as_ptr());
        set(&mut t, 3.0, 2.5, 40.0);
        g.call_p(vtbl::UPDATE_TELEMETRY, t.as_ptr());
        // Extended is flushed by the next scoring update.
        let mut sc = scoring_info(1, 3.0);
        sc.vehicles[..4].copy_from_slice(&7i32.to_le_bytes());
        g.call_p(vtbl::UPDATE_SCORING, sc.info.as_ptr());
        let e = g.extended().add(8);
        let td = ext::TRACKED_DAMAGES + 7 * 16;
        assert_eq!((rd_f64(e, td), rd_f64(e, td + 8)), (100.0, 140.0));
        // Stopped in the pit stall: reset, lastPitStopET = scoring ET.
        let mut sc = scoring_info(1, 4.0);
        sc.vehicles[..4].copy_from_slice(&7i32.to_le_bytes());
        sc.vehicles[vehicle_scoring::PIT_STATE] = 3;
        g.call_p(vtbl::UPDATE_SCORING, sc.info.as_ptr());
        assert_eq!((rd_f64(e, td), rd_f64(e, td + 8)), (0.0, 0.0));
        let dt = g.plugin().ext.p().add(ext::DAMAGE_TRACKING + 7 * 16);
        assert_eq!((rd_f64(dt, 0), rd_f64(dt, 8)), (0.0, 4.0));
        // An impact older than the pit stop is ignored.
        set(&mut t, 5.0, 3.5, 10.0);
        g.call_p(vtbl::UPDATE_TELEMETRY, t.as_ptr());
        assert_eq!(rd_f64(g.plugin().ext.p(), ext::TRACKED_DAMAGES + 7 * 16), 0.0);
        set(&mut t, 6.0, 4.5, 10.0);
        g.call_p(vtbl::UPDATE_TELEMETRY, t.as_ptr());
        assert_eq!(rd_f64(g.plugin().ext.p(), ext::TRACKED_DAMAGES + 7 * 16), 10.0);
    }
}

// ---------------------------------------------------------------------------
// Scoring / rules / multi rules

#[test]
fn scoring_copy_and_clamp() {
    let g = Game::new();
    g.startup();
    unsafe {
        let sc = scoring_info(130, 33.0);
        g.call_p(vtbl::UPDATE_SCORING, sc.info.as_ptr());
        let m = g.scoring();
        assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (2, 2));
        assert_eq!(rd_i32(m, 8), 128 * 0x248 + 0x228);
        assert_eq!(bytes(m.add(0xc), 0x224), &sc.info[..]);
        assert_eq!(bytes(m.add(0x230), 128 * 0x248), &sc.vehicles[..128 * 0x248]);
        assert_eq!(g.plugin().last_scoring_update_et, 33.0);
        // The reading application expects rF2ScoringInfo at +0xc and the vehicles at +0x230.
        assert_eq!(8 + SC_INFO, 0xc);
        assert_eq!(8 + SC_VEHICLES, 0x230);
    }
}

#[test]
fn rules_and_multi_rules_copy() {
    use track_rules as tr;
    let g = Game::new();
    g.startup();
    unsafe {
        let actions: Vec<u8> = (0..3 * tr::ACTION_SIZE).map(|i| i as u8 ^ 0x5a).collect();
        let parts: Vec<u8> = (0..2 * tr::PARTICIPANT_SIZE).map(|i| (i * 11) as u8).collect();
        let mut info: Vec<u8> = (0..tr::SIZE).map(|i| (i * 13) as u8).collect();
        info[tr::NUM_ACTIONS..tr::NUM_ACTIONS + 4].copy_from_slice(&3i32.to_le_bytes());
        info[tr::ACTION..tr::ACTION + 8].copy_from_slice(&(actions.as_ptr() as usize).to_le_bytes());
        info[tr::NUM_PARTICIPANTS..tr::NUM_PARTICIPANTS + 4].copy_from_slice(&2i32.to_le_bytes());
        info[tr::PARTICIPANT..tr::PARTICIPANT + 8].copy_from_slice(&(parts.as_ptr() as usize).to_le_bytes());
        assert_eq!(g.call_p_u8(vtbl::ACCESS_TRACK_RULES, info.as_mut_ptr()), 0);
        let m = g.rules();
        assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (2, 2));
        assert_eq!(rd_i32(m, 8), 2 * 0x14c + 0xad0);
        assert_eq!(bytes(m.add(0xc), tr::SIZE), &info[..]);
        assert_eq!(bytes(m.add(8 + 0x2d0), 3 * 0x10), &actions[..]);
        assert_eq!(bytes(m.add(8 + 0xad0), 2 * 0x14c), &parts[..]);

        use multi_rules as mr;
        let mparts: Vec<u8> = (0..4 * mr::PARTICIPANT_SIZE).map(|i| (i * 17) as u8).collect();
        let mut minfo: Vec<u8> = (0..mr::SIZE).map(|i| (i * 19) as u8).collect();
        minfo[mr::NUM_PARTICIPANTS..mr::NUM_PARTICIPANTS + 4].copy_from_slice(&4i32.to_le_bytes());
        minfo[mr::PARTICIPANT..mr::PARTICIPANT + 8].copy_from_slice(&(mparts.as_ptr() as usize).to_le_bytes());
        assert_eq!(g.call_p_u8(vtbl::ACCESS_MULTI_SESSION_RULES, minfo.as_mut_ptr()), 0);
        let m = g.multi_rules();
        assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (2, 2));
        assert_eq!(rd_i32(m, 8), 4 * 0x134 + 0x168);
        assert_eq!(bytes(m.add(0xc), mr::SIZE), &minfo[..]);
        assert_eq!(bytes(m.add(8 + 0x168), 4 * 0x134), &mparts[..]);
    }
}

// ---------------------------------------------------------------------------
// Extended state

#[test]
fn extended_session_realtime_threads_physics() {
    let g = Game::new();
    g.startup();
    unsafe {
        let sc = scoring_info(2, 1.0);
        let mut info = sc.info.clone();
        info[scoring_info::GAME_PHASE] = 5;
        info[scoring_info::SESSION..scoring_info::SESSION + 4].copy_from_slice(&10i32.to_le_bytes());
        let mut sc2 = scoring_info(2, 1.0);
        sc2.info = info;
        let vp = sc2.vehicles.as_ptr() as usize;
        sc2.info[scoring_info::VEHICLE..scoring_info::VEHICLE + 8].copy_from_slice(&vp.to_le_bytes());
        sc2.vehicles[vehicle_scoring::PLACE] = 3;
        sc2.vehicles[vehicle_scoring::IS_PLAYER] = 1;
        sc2.vehicles[vehicle_scoring::FINISH_STATUS] = 2;
        g.call_p(vtbl::UPDATE_SCORING, sc2.info.as_ptr());
        g.sys.st().ticks = 123_456;
        g.call0(vtbl::START_SESSION);
        let e = g.extended().add(8);
        assert_eq!(rd_u8(e, ext::SESSION_STARTED), 1);
        assert_eq!(rd_u64(e, ext::TICKS_SESSION_STARTED), 123_456);
        assert_eq!(rd_u8(e, ext::ST_GAME_PHASE), 5);
        assert_eq!(rd_i32(e, ext::ST_SESSION), 10);
        assert_eq!(rd_i32(e, ext::ST_NUM_SCORING_VEHICLES), 2);
        assert_eq!(bytes(e.add(ext::ST_VEHICLES), 8), &[10, 0, 0, 0, 3, 1, 2, 0]);
        // StartSession's ClearState wiped scoring.
        assert_eq!(rd_i32(g.scoring(), 8 + SC_INFO + scoring_info::NUM_VEHICLES), 0);
        g.call0(vtbl::ENTER_REALTIME);
        assert_eq!(rd_u8(e, ext::IN_REALTIME_FC), 1);
        g.call_i(vtbl::THREAD_STARTED, 0);
        g.call_i(vtbl::THREAD_STARTED, 1);
        assert_eq!((rd_u8(e, ext::MULTIMEDIA_THREAD_STARTED), rd_u8(e, ext::SIMULATION_THREAD_STARTED)), (1, 1));
        g.call_i(vtbl::THREAD_STOPPING, 1);
        assert_eq!(rd_u8(e, ext::SIMULATION_THREAD_STARTED), 0);
        let mut phys: Vec<u8> = (1..=0x28).collect();
        g.call_p(vtbl::SET_PHYSICS_OPTIONS, phys.as_mut_ptr());
        assert_eq!(bytes(e.add(ext::PHYSICS), 0x28), &phys[..]);
        g.call0(vtbl::EXIT_REALTIME);
        assert_eq!(rd_u8(e, ext::IN_REALTIME_FC), 0);
        g.sys.st().ticks = 999;
        g.call0(vtbl::END_SESSION);
        assert_eq!(rd_u8(e, ext::SESSION_STARTED), 0);
        assert_eq!(rd_u64(e, ext::TICKS_SESSION_ENDED), 999);
        // Without the SCR plugin, messages are never captured.
        let mut msg = [0u8; 0x80];
        msg[..5].copy_from_slice(b"hello");
        assert_eq!(g.call_p_u8(vtbl::WANTS_TO_DISPLAY_MESSAGE, msg.as_mut_ptr()), 0);
        assert_eq!(rd_u8(e, ext::DISPLAYED_MESSAGE), 0);
    }
}

#[test]
fn shutdown_clears_and_releases() {
    let g = Game::new();
    g.startup();
    let sc = scoring_info(1, 1.0);
    g.call_p(vtbl::UPDATE_SCORING, sc.info.as_ptr());
    g.call0(vtbl::SHUTDOWN);
    assert!(!g.plugin().is_mapped);
    unsafe {
        // Each payload memset to 0 inside a Begin/End pair before unmapping.
        let m = g.scoring();
        assert_eq!((rd_u32(m, 0), rd_u32(m, 4)), (3, 3));
        assert!(bytes(m.add(8), SCORING_PAYLOAD_SIZE).iter().all(|&b| b == 0));
        let e = g.extended();
        assert!(bytes(e.add(8), EXTENDED_PAYLOAD_SIZE).iter().all(|&b| b == 0));
    }
    let st = g.sys.st();
    assert_eq!(st.calls.iter().filter(|c| *c == "UnmapViewOfFile").count(), 5);
}

// ---------------------------------------------------------------------------
// Custom plugin variables

#[test]
fn custom_variables() {
    let g = Game::new();
    let mut var = [0u8; 0x88];
    let names = [
        " Enabled",
        "EnableStockCarRulesPlugin",
        "DebugOutputLevel",
        "DebugISIInternals",
        "DedicatedServerMapGlobally",
    ];
    for (i, n) in names.iter().enumerate() {
        var = [0xcc; 0x88];
        var[0] = 0;
        let r = unsafe { vtbl::call_p_ret_u8_i(g.this(), vtbl::GET_CUSTOM_VARIABLE, i as i32, var.as_mut_ptr()) };
        assert_eq!(r, 1);
        assert_eq!(s(&var[..0x80]), *n);
        let settings = if i == 2 { 8 } else { 2 };
        assert_eq!(&var[0x80..0x88], &[settings, 0, 0, 0, 0, 0, 0, 0]);
    }
    assert_eq!(unsafe { vtbl::call_p_ret_u8_i(g.this(), vtbl::GET_CUSTOM_VARIABLE, 5, var.as_mut_ptr()) }, 0);

    let access = |name: &str, v: i32| {
        let mut var = [0u8; 0x88];
        var[..name.len()].copy_from_slice(name.as_bytes());
        var[0x84..0x88].copy_from_slice(&v.to_le_bytes());
        g.call_p(vtbl::ACCESS_CUSTOM_VARIABLE, var.as_ptr());
    };
    g.sys.st().files.insert("UserData\\Log\\RF2SMMP_DebugOutput.txt".into(), b"old".to_vec());
    access("debugoutputlevel", 99);
    assert_eq!(g.cx.g().debug_level, 7);
    assert!(g.sys.file("UserData\\Log\\RF2SMMP_DebugOutput.txt").is_none(), "removed when level != 0");
    access("DebugOutputLevel", -3);
    assert_eq!(g.cx.g().debug_level, 0);
    access("DebugISIInternals", 2);
    access("DedicatedServerMapGlobally", 1);
    access("EnableStockCarRulesPlugin", 1);
    let gl = g.cx.g();
    assert!(gl.debug_isi_internals && gl.dedicated_server_map_globally && gl.enable_stock_car_rules);

    let setting = |name: &str, i: i32| {
        let mut var = [0u8; 0x88];
        var[..name.len()].copy_from_slice(name.as_bytes());
        let mut out = [0x77u8; 0x80];
        unsafe { vtbl::call_p_i_p(g.this(), vtbl::GET_CUSTOM_VARIABLE_SETTING, var.as_mut_ptr(), i, out.as_mut_ptr()) };
        out
    };
    assert_eq!(s(&setting("DebugOutputLevel", 3)), "3%");
    assert_eq!(s(&setting(" Enabled", 1)), "True");
    assert_eq!(s(&setting("DEBUGISIINTERNALS", 0)), "False");
    assert_eq!(setting("Other", 0)[0], 0x77, "unknown caption: untouched");
}

// ---------------------------------------------------------------------------
// Debug output

#[test]
fn debug_log_format_crlf_and_levels() {
    let g = Game::with(|g| g.debug_level = 2);
    g.startup();
    let log = g.debug_log();
    let lines: Vec<&str> = log.split("\r\n").collect();
    assert_eq!(lines[0], "09:05:07.042 TID:0x1a2b  SharedMemoryPlugin::Startup(174) : Starting rFactor 2 Shared Memory Map Plugin 64bit Version: 3.0.1.0");
    assert_eq!(lines[1], "09:05:07.042 TID:0x1a2b  SharedMemoryPlugin::Startup(175) : Configuration:");
    assert_eq!(lines[2], "09:05:07.042 TID:0x1a2b  SharedMemoryPlugin::Startup(176) : EnableStockCarRulesPlugin: 0");
    assert_eq!(lines[3], "09:05:07.042 TID:0x1a2b  SharedMemoryPlugin::Startup(177) : DebugOutputLevel: 2");
    assert!(log.contains("SharedMemoryPlugin::Startup(217) : Files mapped successfully\r\n"));
    assert!(log.contains("SharedMemoryPlugin::Startup(222) : Size of telemetry buffer: 241680 bytes.\r\n"));
    assert!(log.contains("SharedMemoryPlugin::Startup(250) : Size of extended buffer: 9444 bytes.\r\n"));
    // Level 6 messages are filtered.
    g.call0(vtbl::START_SESSION);
    assert!(!g.debug_log().contains("SESSION - Started."));
    assert!(!log.contains('\n') || log.matches('\n').count() == log.matches("\r\n").count());
}

#[test]
fn trace_levels_timing() {
    let g = Game::with(|g| g.debug_level = 7);
    g.startup();
    let sc = scoring_info(1, 1.0);
    g.call_p(vtbl::UPDATE_SCORING, sc.info.as_ptr());
    let t = telem_info(10, 1.5);
    g.call_p(vtbl::UPDATE_TELEMETRY, t.as_ptr());
    let log = g.debug_log();
    // QPC advances 1000 ticks of 10 MHz (100 µs) per call in the mock.
    assert!(
        log.contains("SharedMemoryPlugin::TraceBeginUpdate(707) : SCORING - Begin Update:  Delta since last update:"),
        "{log}"
    );
    assert!(log.contains(
        "SharedMemoryPlugin::ScoringTraceBeginUpdate(690) : SCORING - Scoring ET:1.000000  Telemetry ET:-1.000000\r\n"
    ));
    assert!(log.contains("SharedMemoryPlugin::UpdateScoring(730) : WARNING: Scoring update is ahead of telemetry.\r\n"));
    assert!(log.contains("SharedMemoryPlugin::TelemetryTraceBeginUpdate(513) : TELEMETRY - Begin Update:  ET:1.500000  ET delta:2.500000  Time delta since last update:"));
    assert!(log.contains("SharedMemoryPlugin::TelemetryTraceVehicleAdded(530) : Telemetry added - mID:10  ET:1.500000  Pos Changed:Changed\r\n"));
    assert!(log.contains(
        "SharedMemoryPlugin::TelemetryTraceEndUpdate(547) : TELEMETRY - End Update.  Telemetry chain update took "
    ));
    assert!(log.contains("Vehicles in chain: 1  Version Begin:2  End:2\r\n"));
}

#[test]
fn internals_dumps() {
    let g = Game::with(|g| g.debug_isi_internals = true);
    g.startup();
    let tf = "UserData\\Log\\RF2SMMP_InternalsTelemetryOutput.txt";
    let sf = "UserData\\Log\\RF2SMMP_InternalsScoringOutput.txt";
    assert_eq!(g.sys.file(tf).unwrap(), b"-STARTUP- (version 1.100)\r\n");
    let mut t = vec![0u8; telem::SIZE];
    let put = |t: &mut Vec<u8>, o: usize, v: f64| t[o..o + 8].copy_from_slice(&v.to_le_bytes());
    put(&mut t, telem::DELTA_TIME, 0.01);
    put(&mut t, telem::ELAPSED_TIME, 12.5);
    t[telem::VEHICLE_NAME..telem::VEHICLE_NAME + 3].copy_from_slice(b"Car");
    put(&mut t, telem::LOCAL_VEL + 16, -50.0);
    put(&mut t, telem::ORI + 2 * 24 + 16, 1.0); // ori[2].z
    put(&mut t, telem::ORI, 1.0); // ori[0].x
    put(&mut t, telem::UNFILTERED_THROTTLE, 0.5);
    put(&mut t, telem::WHEELS + telem::WHEEL_SIZE + telem::wheel::ROTATION, 3.0);
    g.call_p(vtbl::UPDATE_TELEMETRY, t.as_ptr());
    let out = String::from_utf8(g.sys.file(tf).unwrap()).unwrap();
    assert!(out.starts_with(
        "-STARTUP- (version 1.100)\r\nDT=0.0100  ET=12.5000\r\nLap=0 StartET=0.000\r\nVehicle=Car\r\nTrack=\r\n"
    ));
    assert!(out.contains("\r\n[ 1.000, 0.000, 0.000]\r\n[ 0.000, 0.000, 0.000]\r\n[ 0.000, 0.000, 1.000]\r\n"));
    assert!(out.contains("UnfilteredThrottle=50.0%\r\n"));
    assert!(out.contains("Wheel=FrontRight\r\n SuspensionDeflection=0.000 RideHeight=0.000\r\n SuspForce=0.0 BrakeTemp=0.0 BrakePressure=0.000\r\n ForwardRotation=-3.0 Camber=0.000\r\n"));
    assert!(out.contains("Wheel=RearLeft\r\n") && out.contains("Wheel=RearRight\r\n"));
    assert!(out.ends_with("Pitch = -0.0 deg, Roll = 0.0 deg\r\nSpeed = 180.0 KPH, 111.9 MPH\r\n\r\n"), "{out}");
    // The cached FILE * was fclose'd and not reset: the next dump is lost.
    let t2 = telem_info(3, 13.0);
    g.call_p(vtbl::UPDATE_TELEMETRY, t2.as_ptr());
    assert_eq!(String::from_utf8(g.sys.file(tf).unwrap()).unwrap(), out);

    let sc = scoring_info(1, 2.0);
    g.call_p(vtbl::UPDATE_SCORING, sc.info.as_ptr());
    let so = String::from_utf8_lossy(&g.sys.file(sf).unwrap()).into_owned();
    assert!(so.contains("START STREAM\r\n<results/>END STREAM\r\n"), "{so}");
    assert!(so.contains("Driver 0: "));
    g.call0(vtbl::START_SESSION);
    assert!(String::from_utf8_lossy(&g.sys.file(sf).unwrap()).ends_with("\r\n\r\n--STARTSESSION--\r\n"));
}

// ---------------------------------------------------------------------------
// StockCarRules hosting

mod scr {
    use super::*;
    use std::sync::Mutex;

    pub static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    fn rec(s: String) {
        CALLS.lock().unwrap().push(s);
    }

    #[repr(C)]
    pub struct Obj {
        vt: *const [*const (); 50],
        info: usize,
    }
    unsafe extern "C" fn nop(_: *mut Obj) {}
    unsafe extern "C" fn yes(_: *mut Obj) -> u8 {
        1
    }
    unsafe extern "C" fn startup(_: *mut Obj, v: i32) {
        rec(format!("Startup({v})"))
    }
    unsafe extern "C" fn access_cv(_: *mut Obj, var: *mut u8) {
        rec(format!("AccessCustomVariable({}={})", super::s(bytes(var, 0x80)), rd_i32(var, 0x84)));
        if let Some(value) = CALLBACK_ERRNO.take() {
            NATIVE_ERRNO.set(Some(value));
        }
    }
    unsafe extern "C" fn track_rules(_: *mut Obj, info: *mut u8) -> u8 {
        rec("AccessTrackRules".into());
        // The SCR plugin writes its messages into the rules.
        *info.add(track_rules::YELLOW_FLAG_STATE) = 1;
        let m = b"Pace car\0";
        core::ptr::copy_nonoverlapping(m.as_ptr(), info.add(track_rules::MESSAGE), m.len());
        let p = rd_ptr(info, track_rules::PARTICIPANT);
        let pm = b"Pass left\0";
        core::ptr::copy_nonoverlapping(pm.as_ptr(), p.add(track_rules::PARTICIPANT_MESSAGE), pm.len());
        1
    }
    unsafe extern "C" fn display(_: *mut Obj, _m: *mut u8) -> u8 {
        1
    }
    unsafe extern "C" fn shutdown(_: *mut Obj) {
        rec("Shutdown".into())
    }
    static mut VT: [*const (); 50] = [core::ptr::null(); 50];
    static mut OBJ: Obj = Obj { vt: core::ptr::null(), info: 0 };
    pub unsafe extern "C" fn get_name() -> *const u8 {
        b"Stock Car Rules\0".as_ptr()
    }
    pub unsafe extern "C" fn get_type() -> i32 {
        3
    }
    pub unsafe extern "C" fn get_version() -> i32 {
        7
    }
    pub unsafe extern "C" fn create() -> *mut u8 {
        let vt = &mut *core::ptr::addr_of_mut!(VT);
        for s in vt.iter_mut() {
            *s = nop as *const ();
        }
        vt[vtbl::STARTUP / 8] = startup as *const ();
        vt[vtbl::SHUTDOWN / 8] = shutdown as *const ();
        vt[vtbl::ACCESS_CUSTOM_VARIABLE / 8] = access_cv as *const ();
        vt[vtbl::ACCESS_TRACK_RULES / 8] = track_rules as *const ();
        vt[vtbl::WANTS_TO_DISPLAY_MESSAGE / 8] = display as *const ();
        vt[vtbl::WANTS_PIT_MENU_ACCESS / 8] = yes as *const ();
        OBJ.vt = vt;
        (core::ptr::addr_of_mut!(OBJ) as *mut u8).add(8)
    }
    pub unsafe extern "C" fn destroy(p: *mut u8) {
        rec(format!("Destroy(+8={})", p == (core::ptr::addr_of_mut!(OBJ) as *mut u8).add(8)));
    }
}

const JSON: &str = "{\r\n  \"rFactor2SharedMemoryMapPlugin64.dll\":{\r\n    \" Enabled\":1,\r\n    \"EnableStockCarRulesPlugin\":1\r\n  },\r\n  \"StockCarRules.dll\":{\r\n    \" Enabled\":0,\r\n    \"DoubleFileType\":2,\r\n    \"Strictness\":75\r\n  }\r\n}\r\n";

fn scr_game(json: &str) -> Game {
    let g = Game::with(|g| {
        g.enable_stock_car_rules = true;
        g.debug_level = 2;
    });
    {
        let mut st = g.sys.st();
        st.files.insert(
            "C:\\Games\\rFactor 2\\UserData\\player\\CustomPluginVariables.JSON".into(),
            json.as_bytes().to_vec(),
        );
        st.libraries.insert(
            "C:\\Games\\rFactor 2\\Bin64\\Plugins\\StockCarRules.dll".into(),
            vec![
                ("GetPluginName".into(), scr::get_name as *const () as usize),
                ("GetPluginType".into(), scr::get_type as *const () as usize),
                ("GetPluginVersion".into(), scr::get_version as *const () as usize),
                ("CreatePluginObject".into(), scr::create as *const () as usize),
                ("DestroyPluginObject".into(), scr::destroy as *const () as usize),
            ],
        );
    }
    g
}

#[test]
fn stock_car_rules_hosting() {
    let _guard = SCR_LOCK.lock().unwrap();
    scr::CALLS.lock().unwrap().clear();
    let g = scr_game(JSON);
    g.startup();
    assert!(g.plugin().host.loaded);
    assert_eq!(
        *scr::CALLS.lock().unwrap(),
        vec![
            "AccessCustomVariable( Enabled=1)",
            "AccessCustomVariable(DoubleFileType=2)",
            "AccessCustomVariable(Strictness=75)",
            "Startup(1100)"
        ]
    );
    unsafe {
        let e = g.extended().add(8);
        assert_eq!(rd_u8(e, ext::SCR_IS_HOSTED), 1);
        assert_eq!(rd_i32(e, ext::SCR_DOUBLE_FILE_TYPE), 2);
    }
    let log = g.debug_log();
    assert!(log.contains("PluginHost::Initialize(119) : Forwarding StockCarRules.dll plugin configuration:\r\n"));
    assert!(log.contains("PluginHost::ForwardPluginConfig(500) : DoubleFileType 2\r\n"));
    assert!(log.contains(
        "PluginHost::Initialize(126) : Successfully loaded StockCarRules.dll plugin.  Name: Stock Car Rules\r\n"
    ));
    // The JSON buffer's '\r's were restored and the buffer freed.
    assert_eq!(g.sys.st().allocs.len(), 1);

    // Track rules: the SCR plugin's messages are captured and kept.
    use track_rules as tr;
    unsafe {
        let mut parts = vec![0u8; 2 * tr::PARTICIPANT_SIZE];
        parts[..4].copy_from_slice(&5i32.to_le_bytes());
        parts[tr::PARTICIPANT_SIZE..tr::PARTICIPANT_SIZE + 4].copy_from_slice(&6i32.to_le_bytes());
        let mut info = vec![0u8; tr::SIZE];
        info[tr::NUM_PARTICIPANTS..tr::NUM_PARTICIPANTS + 4].copy_from_slice(&2i32.to_le_bytes());
        info[tr::PARTICIPANT..tr::PARTICIPANT + 8].copy_from_slice(&(parts.as_ptr() as usize).to_le_bytes());
        assert_eq!(g.call_p_u8(vtbl::ACCESS_TRACK_RULES, info.as_mut_ptr()), 1);
        let m = g.rules().add(8);
        assert_eq!(s(bytes(m.add(4 + tr::MESSAGE), 0x60)), "Pace car");
        assert_eq!(s(bytes(m.add(RU_PARTICIPANTS + tr::PARTICIPANT_MESSAGE), 0x60)), "Pass left");
        // Participant 6 had no message: its slot gets the (empty) capture.
        assert_eq!(rd_u8(m, RU_PARTICIPANTS + tr::PARTICIPANT_SIZE + tr::PARTICIPANT_MESSAGE), 0);
        assert_eq!(s(&g.plugin().participant_track_rules_messages[5]), "Pass left");
        // Displayed messages are captured when the SCR plugin wants them shown.
        let mut msg = [0u8; 0x80];
        msg[..4].copy_from_slice(b"Flag");
        assert_eq!(g.call_p_u8(vtbl::WANTS_TO_DISPLAY_MESSAGE, msg.as_mut_ptr()), 1);
        assert_eq!(s(bytes(g.extended().add(8 + ext::DISPLAYED_MESSAGE), 0x80)), "Flag");
    }
    g.call0(vtbl::SHUTDOWN);
    assert_eq!(
        scr::CALLS.lock().unwrap()[4..],
        ["AccessTrackRules".to_string(), "Shutdown".into(), "Destroy(+8=true)".into()]
    );
    assert!(!g.plugin().host.loaded);
    assert!(g.debug_log().contains("PluginHost::Cleanup(158) : Successfully unloaded StockCarRules.dll plugin.\r\n"));
}

static SCR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn native_shared_errno_receives_plugin_file_open_failure() {
    let _native = NativeErrno::enter(34);
    let g = Game::new();
    assert!(unsafe { crate::host::PluginHost::get_file_contents(g.cx, b"missing.json\0") }.is_null());
    assert_eq!(g.cx.g().errno, 2);
    assert_eq!(NATIVE_ERRNO.get(), Some(2), "failed fopen_s updates the shared CRT state");
}

#[test]
fn native_shared_errno_receives_debug_file_delete_failure() {
    let _native = NativeErrno::enter(34);
    let g = Game::new();
    let mut variable = [0u8; custom_var::SIZE];
    variable[..16].copy_from_slice(b"DebugOutputLevel");
    variable[custom_var::CURRENT_SETTING..custom_var::CURRENT_SETTING + 4].copy_from_slice(&1i32.to_le_bytes());
    unsafe { g.plugin().access_custom_variable(variable.as_mut_ptr()) };
    assert_eq!(g.cx.g().errno, 2);
    assert_eq!(NATIVE_ERRNO.get(), Some(2), "remove failure updates the shared CRT state");
}

#[test]
fn stock_car_rules_not_disabled_in_json() {
    let _guard = SCR_LOCK.lock().unwrap();
    let g = scr_game(&JSON.replace("\" Enabled\":0", "\" Enabled\":1"));
    g.startup();
    assert!(!g.plugin().host.loaded);
    assert!(g.debug_log().contains("PluginHost::Initialize(41) : Error: StockCarRules.dll plugin is not disabled in CustomPluginVariables.JSON\r\n"));
    assert!(!g.sys.st().calls.iter().any(|c| c.starts_with("LoadLibraryExA")));
    unsafe { assert_eq!(rd_u8(g.extended().add(8), ext::SCR_IS_HOSTED), 0) };
}

#[test]
fn forward_plugin_config_parsing() {
    let _guard = SCR_LOCK.lock().unwrap();
    // A non-numeric value ends the number; a too long number fails.
    let g = scr_game(&JSON.replace("\"Strictness\":75", "\"Strictness\":1234567"));
    g.startup();
    assert!(!g.plugin().host.loaded);
    let log = g.debug_log();
    assert!(
        log.contains(
            "PluginHost::ForwardPluginConfig(472) : Exceeded max number length in line:     \"Strictness\":1234567\r\n"
        ),
        "{log}"
    );
    assert!(log.contains("PluginHost::Initialize(121) : Failed to forward StockCarRules.dll plugin configuration.\r\n"));
    assert_eq!(g.sys.st().freed_libraries, 1, "scope guard unloads the library");
    // Missing file.
    let g = Game::with(|g| {
        g.enable_stock_car_rules = true;
        g.debug_level = 1;
    });
    g.startup();
    let log = g.debug_log();
    assert!(log.contains("PluginHost::GetFileContents(349) : fopen_s() failed with: 2\r\n"));
    assert!(log.contains("PluginHost::Initialize(31) : Failed to load CustomPluginVariables.JSON file\r\n"));
}

#[test]
fn is_plugin_disabled_needle() {
    let mut buf = b"\"StockCarRules.dll\":{\r\n    \" Enabled\":0,\r\n}\0".to_vec();
    let mut cfg = core::ptr::null_mut();
    assert!(unsafe { crate::host::PluginHost::is_plugin_disabled(buf.as_mut_ptr(), b"StockCarRules.dll\0", &mut cfg) });
    assert_eq!(cfg as usize - buf.as_ptr() as usize, 1);
    assert_eq!(&buf[..], b"\"StockCarRules.dll\":{\r\n    \" Enabled\":0,\r\n}\0", "CRs restored");
    // The needle needs the space before the quote.
    let mut buf = b"\"StockCarRules.dll\":{\" Enabled\":0}\0".to_vec();
    assert!(!unsafe {
        crate::host::PluginHost::is_plugin_disabled(buf.as_mut_ptr(), b"StockCarRules.dll\0", &mut cfg)
    });
}

// ---------------------------------------------------------------------------
// printf

#[test]
fn printf_subset_matches_crt() {
    let f =
        |fmt: &str, a: &[Arg]| String::from_utf8(cfmt::sprintf::<512>(fmt.as_bytes(), a).as_bytes().to_vec()).unwrap();
    assert_eq!(
        f("%.2d:%.2d:%.2d.%.3d TID:0x%04x  ", &[Arg::Int(9), Arg::Int(5), Arg::Int(59), Arg::Int(7), Arg::Int(0xab)]),
        "09:05:59.007 TID:0x00ab  "
    );
    assert_eq!(f("%04x", &[Arg::Int(0x12345)]), "12345");
    assert_eq!(f("%6.3f|%6.3f", &[Arg::Dbl(1.5), Arg::Dbl(-12.25)]), " 1.500|-12.250");
    assert_eq!(
        f("%.1f %.2f %.3f %.4f", &[Arg::Dbl(0.05), Arg::Dbl(2.675), Arg::Dbl(-0.0), Arg::Dbl(1e-5)]),
        "0.1 2.67 -0.000 0.0000"
    );
    // Ties of exactly representable values round to even (UCRT >= 10.0.19041).
    assert_eq!(f("%.0f %.0f %.1f", &[Arg::Dbl(0.5), Arg::Dbl(1.5), Arg::Dbl(0.25)]), "0 2 0.2");
    assert_eq!(f("%f", &[Arg::Dbl(1.0 / 3.0)]), "0.333333");
    assert_eq!(
        f("%f %f %f", &[Arg::Dbl(f64::INFINITY), Arg::Dbl(f64::NEG_INFINITY), Arg::Dbl(f64::NAN)]),
        "inf -inf nan"
    );
    assert_eq!(f("%f", &[Arg::Dbl(f64::from_bits(0xfff8_0000_0000_0000))]), "-nan(ind)");
    assert_eq!(f("%d%%", &[Arg::Int(3)]), "3%");
    assert_eq!(f("%d %d", &[Arg::Int(-7), Arg::Int(i32::MIN)]), "-7 -2147483648");
    assert_eq!(f("%d", &[Arg::Int(0xffff_fffeu32 as i32)]), "-2");
    assert_eq!(f("%s(%d) : %s %s\n", &[Arg::Str(b"F"), Arg::Int(12), Arg::Str(b"a"), Arg::Str(b"b")]), "F(12) : a b\n");
    assert_eq!(f("%.3f", &[Arg::Dbl((1100f32 * 0.001f32) as f64)]), "1.100");
    assert_eq!(f("%5d|%-5d|%05d", &[Arg::Int(42), Arg::Int(42), Arg::Int(-42)]), "   42|42   |-0042");
}

#[test]
fn crt_helpers() {
    let mut e = 0;
    assert_eq!(crate::crt::atol(b"75", &mut e), 75);
    assert_eq!(crate::crt::atol(b"", &mut e), 0);
    assert_eq!(e, 0);
    assert_eq!(crate::crt::errno_from_win32(2), 2);
    assert_eq!(crate::crt::errno_from_win32(32), 13);
    assert_eq!(crate::crt::errno_from_win32(9999), 22);
    let mut n = [0u8; 20];
    let k = crate::crt::itoa10(0x3b010, &mut n);
    assert_eq!(&n[..k + 1], b"241680\0");
    assert!(crate::crt::stricmp_eq(b"DebugISIInternals", b"debugisiinternals"));
    assert_eq!(mapped_id(-5), 0);
    assert_eq!(mapped_id(513), 1);
}

// ---------------------------------------------------------------------------
// Strings and layout

#[test]
fn layout_matches_srs_rf2_definition() {
    // Offsets inside the mapping, as the reading application uses them.
    assert_eq!(VERSION_BLOCK_SIZE + TEL_NUM_VEHICLES, 0xc);
    assert_eq!(VERSION_BLOCK_SIZE + TEL_VEHICLES, 0x10);
    assert_eq!(telem::SIZE, 1888);
    assert_eq!((telem::LOCAL_VEL, telem::LOCAL_ACCEL, telem::ORI, telem::GEAR), (0xb8, 0xd0, 0xe8, 0x160));
    assert_eq!((telem::ENGINE_RPM, telem::FUEL, telem::ENGINE_MAX_RPM, telem::WHEELS), (0x164, 0x20c, 0x214, 0x350));
    assert_eq!((telem::WHEEL_SIZE, telem::wheel::TERRAIN_NAME), (260, 0xa0));
    assert_eq!(
        (scoring_info::SIZE, scoring_info::GAME_PHASE, scoring_info::SECTOR_FLAG, scoring_info::START_LIGHT),
        (548, 0x6c, 0x6e, 0x71)
    );
    assert_eq!(
        (vehicle_scoring::SIZE, vehicle_scoring::SECTOR, vehicle_scoring::IS_PLAYER, vehicle_scoring::CONTROL),
        (584, 0x66, 0xc4, 0xc5)
    );
}

// ---------------------------------------------------------------------------
// /GS cookie failures of the unbounded sprintf / lstrcatA writes. The plugin
// methods are called directly (not through the extern "C" vtable) so the
// mock's failure can unwind into the test.

/// The message part of the first debug-log line of `func`.
fn traced_msg(log: &str, func: &str) -> String {
    let start = log.find(func).unwrap_or_else(|| panic!("{func} not traced:\n{log}"));
    let rest = &log[start..];
    let rest = &rest[rest.find(" : ").unwrap() + 3..];
    rest[..rest.find("\r\n").unwrap()].to_string()
}

#[test]
fn scoring_trace_cookie_failure_threshold() {
    // ScoringTraceBeginUpdate's char[0x200]: fails exactly when the line plus
    // its NUL runs past 0x200 bytes (the cookie is 0x200 past the buffer).
    let mut seen = (false, false);
    for k in 150..300 {
        let g = Game::with(|g| g.debug_level = 2);
        g.startup();
        // Telemetry ET with 301 integer digits, set below the trace level.
        let t = telem_info(10, 1e300);
        unsafe { g.plugin().update_telemetry(t.as_ptr()) };
        g.cx.g().debug_level = 6;
        let sc = scoring_info(0, 10f64.powi(k));
        let failed = ends_in_gs_failure(|| unsafe { g.plugin().update_scoring(sc.info.as_ptr()) });
        let log = g.debug_log();
        let msg = traced_msg(&log, "SharedMemoryPlugin::ScoringTraceBeginUpdate(690)");
        assert!(msg.starts_with("SCORING - Scoring ET:1"), "{msg}");
        assert_eq!(failed, msg.len() + 1 > 0x200, "k={k} len={}", msg.len());
        if failed {
            // The whole line was traced, then the function's epilogue failed;
            // nothing after it (the ahead-of-telemetry check) ran.
            assert_eq!(g.sys.st().calls.last().unwrap(), "__report_gsfailure");
            assert!(!log.contains("UpdateScoring(730)"));
        }
        seen.0 |= msg.len() + 1 == 0x200 && !failed;
        seen.1 |= msg.len() + 1 == 0x201 && failed;
    }
    assert!(seen.0 && seen.1, "threshold not hit exactly: {seen:?}");
}

#[test]
fn telemetry_begin_trace_cookie_failure() {
    let mut seen = (false, false);
    for k in 150..300 {
        let g = Game::with(|g| g.debug_level = 6);
        g.startup();
        let t = telem_info(10, 10f64.powi(k));
        let failed = ends_in_gs_failure(|| unsafe { g.plugin().update_telemetry(t.as_ptr()) });
        let msg = traced_msg(&g.debug_log(), "SharedMemoryPlugin::TelemetryTraceBeginUpdate(513)");
        assert_eq!(failed, msg.len() + 1 > 0x200, "k={k} len={}", msg.len());
        if failed {
            assert_eq!(g.sys.st().calls.last().unwrap(), "__report_gsfailure");
        }
        seen.0 |= !failed;
        seen.1 |= failed;
    }
    assert!(seen.0 && seen.1);
}

#[test]
fn telemetry_skip_trace_cookie_failure() {
    // First SkipUpdate line: two adjacent char[0x200] before the cookie, so a
    // line of more than 0x200 (but under 0x400) bytes does not fail.
    let g = Game::with(|g| g.debug_level = 2);
    g.startup();
    let t = telem_info(10, 1e300);
    unsafe { g.plugin().update_telemetry(t.as_ptr()) };
    g.cx.g().debug_level = 6;
    assert!(!ends_in_gs_failure(|| unsafe { g.plugin().update_telemetry(t.as_ptr()) }));
    let log = g.debug_log();
    let msg = traced_msg(&log, "SharedMemoryPlugin::TelemetryTraceSkipUpdate(479)");
    assert!(msg.len() + 1 > 0x200 && msg.len() + 1 <= 0x400, "{}", msg.len());
    assert!(!log.contains("TelemetryTraceSkipUpdate(496)"));

    // Position mismatch: the second line (its own char[0x200]) runs over.
    let g = Game::with(|g| g.debug_level = 2);
    g.startup();
    let t = telem_info(10, 1e300);
    unsafe { g.plugin().update_telemetry(t.as_ptr()) };
    g.cx.g().debug_level = 6;
    let mut t2 = t.clone();
    t2[telem::POS..telem::POS + 8].copy_from_slice(&1.25f64.to_le_bytes());
    assert!(ends_in_gs_failure(|| unsafe { g.plugin().update_telemetry(t2.as_ptr()) }));
    let msg = traced_msg(&g.debug_log(), "SharedMemoryPlugin::TelemetryTraceSkipUpdate(496)");
    assert!(msg.starts_with("WARNING - Pos Mismatch on skip update!!!  New ET: 1"));
    assert!(msg.len() + 1 > 0x200);
    assert_eq!(g.sys.st().calls.last().unwrap(), "__report_gsfailure");
}

#[test]
fn plugin_host_initialize_cookie_failure() {
    // char[0x110] + lstrcatA of the 43-character JSON path: a working
    // directory of 229 characters puts the NUL at offset 0x110 (the cookie).
    for (len, fails) in [(228usize, false), (229, true)] {
        let g = Game::with(|g| {
            g.enable_stock_car_rules = true;
            g.debug_level = 2;
        });
        let cwd = format!("C:\\{}", "d".repeat(len - 3));
        g.sys.st().cur_dir = cwd.clone();
        let failed = ends_in_gs_failure(|| unsafe { g.plugin().startup(1100) });
        assert_eq!(failed, fails, "cwd {len}");
        let calls = g.sys.st().calls.clone();
        // The full, unbounded path was opened either way.
        assert!(
            calls.iter().any(|c| c.contains(&format!("{cwd}\\UserData\\player\\CustomPluginVariables.JSON"))),
            "{calls:?}"
        );
        assert!(g
            .debug_log()
            .contains("PluginHost::Initialize(31) : Failed to load CustomPluginVariables.JSON file\r\n"));
        assert_eq!(calls.last().map(|s| s == "__report_gsfailure"), Some(fails));
    }

    // With the plugin hosted from the long directory: it is loaded from the
    // full path and configured, then Initialize's epilogue ends the process
    // before Startup is forwarded to it.
    let _guard = SCR_LOCK.lock().unwrap();
    scr::CALLS.lock().unwrap().clear();
    let g = scr_game(JSON);
    let cwd = format!("C:\\{}", "d".repeat(240 - 3));
    {
        let mut st = g.sys.st();
        st.cur_dir = cwd.clone();
        let json = st.files.remove("C:\\Games\\rFactor 2\\UserData\\player\\CustomPluginVariables.JSON").unwrap();
        st.files.insert(format!("{cwd}\\UserData\\player\\CustomPluginVariables.JSON"), json);
        let lib = st.libraries.remove("C:\\Games\\rFactor 2\\Bin64\\Plugins\\StockCarRules.dll").unwrap();
        st.libraries.insert(format!("{cwd}\\Bin64\\Plugins\\StockCarRules.dll"), lib);
    }
    assert!(ends_in_gs_failure(|| unsafe { g.plugin().startup(1100) }));
    assert!(g.plugin().host.loaded);
    assert_eq!(
        *scr::CALLS.lock().unwrap(),
        vec![
            "AccessCustomVariable( Enabled=1)",
            "AccessCustomVariable(DoubleFileType=2)",
            "AccessCustomVariable(Strictness=75)"
        ]
    );
    assert!(g.sys.st().calls.iter().any(|c| *c == format!("LoadLibraryExA({cwd}\\Bin64\\Plugins\\StockCarRules.dll)")));
    assert!(g.debug_log().contains(
        "PluginHost::Initialize(126) : Successfully loaded StockCarRules.dll plugin.  Name: Stock Car Rules\r\n"
    ));
    assert_eq!(g.sys.st().calls.last().unwrap(), "__report_gsfailure");
}
