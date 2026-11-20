//! InternalsPluginV07 virtual table (MSVC x64 layout, 50 slots)
//! and helpers to call a C++ plugin object through it.
//!
//! On x64 Windows a C++ member function is called like a plain Microsoft
//! x64 function with `this` first, which is `extern "C"` there.

/// Byte offsets into the vtable.
pub const DESTRUCTOR: usize = 0x000;
pub const STARTUP: usize = 0x008;
pub const SHUTDOWN: usize = 0x010;
pub const LOAD: usize = 0x018;
pub const UNLOAD: usize = 0x020;
pub const START_SESSION: usize = 0x028;
pub const END_SESSION: usize = 0x030;
pub const ENTER_REALTIME: usize = 0x038;
pub const EXIT_REALTIME: usize = 0x040;
pub const WANTS_SCORING_UPDATES: usize = 0x048;
pub const UPDATE_SCORING: usize = 0x050;
pub const WANTS_TELEMETRY_UPDATES: usize = 0x058;
pub const UPDATE_TELEMETRY: usize = 0x060;
pub const WANTS_GRAPHICS_UPDATES: usize = 0x068;
pub const UPDATE_GRAPHICS: usize = 0x070;
pub const REQUEST_COMMENTARY: usize = 0x078;
pub const HAS_HARDWARE_INPUTS: usize = 0x080;
pub const UPDATE_HARDWARE: usize = 0x088;
pub const ENABLE_HARDWARE: usize = 0x090;
pub const DISABLE_HARDWARE: usize = 0x098;
pub const CHECK_HW_CONTROL: usize = 0x0a0;
pub const FORCE_FEEDBACK: usize = 0x0a8;
pub const ERROR: usize = 0x0b0;
pub const SET_PHYSICS_OPTIONS: usize = 0x0b8;
pub const WANTS_TO_VIEW_VEHICLE: usize = 0x0c0;
pub const UPDATE_GRAPHICS_V02: usize = 0x0c8;
pub const WANTS_TO_DISPLAY_MESSAGE: usize = 0x0d0;
pub const SET_ENVIRONMENT: usize = 0x0d8;
pub const INIT_SCREEN: usize = 0x0e0;
pub const UNINIT_SCREEN: usize = 0x0e8;
pub const DEACTIVATE_SCREEN: usize = 0x0f0;
pub const REACTIVATE_SCREEN: usize = 0x0f8;
pub const RENDER_SCREEN_BEFORE_OVERLAYS: usize = 0x100;
pub const RENDER_SCREEN_AFTER_OVERLAYS: usize = 0x108;
pub const PRE_RESET: usize = 0x110;
pub const POST_RESET: usize = 0x118;
pub const INIT_CUSTOM_CONTROL: usize = 0x120;
pub const WANTS_WEATHER_ACCESS: usize = 0x128;
pub const ACCESS_WEATHER: usize = 0x130;
pub const THREAD_STARTED: usize = 0x138;
pub const THREAD_STOPPING: usize = 0x140;
pub const GET_CUSTOM_VARIABLE: usize = 0x148;
pub const ACCESS_CUSTOM_VARIABLE: usize = 0x150;
pub const GET_CUSTOM_VARIABLE_SETTING: usize = 0x158;
pub const WANTS_MULTI_SESSION_RULES_ACCESS: usize = 0x160;
pub const ACCESS_MULTI_SESSION_RULES: usize = 0x168;
pub const WANTS_TRACK_RULES_ACCESS: usize = 0x170;
pub const ACCESS_TRACK_RULES: usize = 0x178;
pub const WANTS_PIT_MENU_ACCESS: usize = 0x180;
pub const ACCESS_PIT_MENU: usize = 0x188;
pub const SLOTS: usize = 50;

#[inline]
unsafe fn slot(obj: *mut u8, off: usize) -> *const () {
    let vt = *(obj as *const *const *const ());
    *vt.add(off / 8)
}

pub unsafe fn call0(obj: *mut u8, off: usize) {
    let f: unsafe extern "C" fn(*mut u8) = core::mem::transmute(slot(obj, off));
    f(obj)
}
pub unsafe fn call_i32(obj: *mut u8, off: usize, a: i32) {
    let f: unsafe extern "C" fn(*mut u8, i32) = core::mem::transmute(slot(obj, off));
    f(obj, a)
}
pub unsafe fn call_p(obj: *mut u8, off: usize, p: *const u8) {
    let f: unsafe extern "C" fn(*mut u8, *const u8) = core::mem::transmute(slot(obj, off));
    f(obj, p)
}
/// bool-returning slot (only `al` is tested).
pub unsafe fn call_ret_u8(obj: *mut u8, off: usize) -> u8 {
    let f: unsafe extern "C" fn(*mut u8) -> u8 = core::mem::transmute(slot(obj, off));
    f(obj)
}
/// long-returning slot.
pub unsafe fn call_ret_i32(obj: *mut u8, off: usize) -> i32 {
    let f: unsafe extern "C" fn(*mut u8) -> i32 = core::mem::transmute(slot(obj, off));
    f(obj)
}
pub unsafe fn call_p_ret_u8(obj: *mut u8, off: usize, p: *mut u8) -> u8 {
    let f: unsafe extern "C" fn(*mut u8, *mut u8) -> u8 = core::mem::transmute(slot(obj, off));
    f(obj, p)
}
/// `bool f(long i, T &)` (GetCustomVariable).
pub unsafe fn call_p_ret_u8_i(obj: *mut u8, off: usize, i: i32, p: *mut u8) -> u8 {
    let f: unsafe extern "C" fn(*mut u8, i32, *mut u8) -> u8 = core::mem::transmute(slot(obj, off));
    f(obj, i, p)
}
/// `void f(T &, long i, U &)` (GetCustomVariableSetting).
pub unsafe fn call_p_i_p(obj: *mut u8, off: usize, p: *mut u8, i: i32, q: *mut u8) {
    let f: unsafe extern "C" fn(*mut u8, *mut u8, i32, *mut u8) = core::mem::transmute(slot(obj, off));
    f(obj, p, i, q)
}
