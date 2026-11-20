//! The behaviour of the plugin: mapping object, init/shutdown, DllMain
//! and every SCS callback, for both the x64 build (`ets2-telemetry.dll`) and
//! the i386 build (`ets2-telemetry_32.dll`).
//!
//! This works on raw pointers into the mapping view. Some spots are
//! unchecked: the copy of the model/trailer id, the signed ratio
//! index, and attribute handlers without a NULL test.

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void, CStr};
use core::ptr;

use crate::layout::{self, off, AttrAction, ChannelKind, ATTRIBUTES, CHANNELS, MAP_SIZE};
use crate::os::{Handle, Os, MAP_NAME_W};
use crate::scs::*;

/// The mapping object (0x28 bytes on x64, 0x14 bytes on i386, from `operator new`):
/// name +0x0, size +0x8/+0x4, handle +0x10/+0x8, view +0x18/+0xc, ok
/// +0x20/+0x10. Which `Os` to call is kept outside the object
/// (`Globals::os`), so the object holds only those fields.
#[repr(C)]
pub struct SharedMemory {
    pub name: *const u16,
    pub size: u32,
    pub handle: Handle,
    pub view: *mut u8,
    pub ok: bool,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(core::mem::size_of::<SharedMemory>() == 0x28 && core::mem::offset_of!(SharedMemory, ok) == 0x20);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<SharedMemory>() == 0x14 && core::mem::offset_of!(SharedMemory, ok) == 0x10);

impl SharedMemory {
    /// The constructor, run on the object
    /// `operator new` returned. It writes the fields of
    /// that object in place: size, ok and name before CreateFileMappingW
    /// (x64 order size, ok, name; i386 size, name, ok), handle after each
    /// call that returns one, view only after MapViewOfFile. On the
    /// create-error and open-failure paths view is never written and keeps
    /// whatever `operator new` left there (nothing reads it: ok is 0).
    pub unsafe fn construct(this: *mut SharedMemory, os: &dyn Os) -> *mut SharedMemory {
        let size = ptr::addr_of_mut!((*this).size);
        let ok = ptr::addr_of_mut!((*this).ok);
        let name = ptr::addr_of_mut!((*this).name);
        let handle = ptr::addr_of_mut!((*this).handle);
        let view = ptr::addr_of_mut!((*this).view);
        size.write_volatile(MAP_SIZE as u32);
        #[cfg(not(target_pointer_width = "32"))]
        {
            ok.write_volatile(false);
            name.write_volatile(MAP_NAME_W.as_ptr());
        }
        #[cfg(target_pointer_width = "32")]
        {
            name.write_volatile(MAP_NAME_W.as_ptr());
            ok.write_volatile(false);
        }
        let h = os.create_file_mapping(layout::PAGE_READWRITE, 0, MAP_SIZE as u32, MAP_NAME_W.as_ptr());
        handle.write_volatile(h);
        if h.is_null() {
            if os.last_error() != layout::ERROR_ALREADY_EXISTS {
                return this;
            }
            // The name is read back from the object.
            let h = os.open_file_mapping(layout::FILE_MAP_ALL_ACCESS, false, name.read_volatile());
            handle.write_volatile(h);
            if h.is_null() {
                return this;
            }
        }
        // The handle is read back from the object.
        let v = os.map_view(handle.read_volatile(), layout::FILE_MAP_ALL_ACCESS, 0, 0, MAP_SIZE);
        view.write_volatile(v);
        if v.is_null() {
            os.close_handle(handle.read_volatile());
            return this;
        }
        crt_memset(v, 0, MAP_SIZE);
        ok.write_volatile(true);
        this
    }

    /// Body shared by scs_telemetry_shutdown and the DllMain
    /// detach. The handle/view fields are left as they are.
    pub unsafe fn close(this: *mut SharedMemory, os: &dyn Os) {
        if (*this).ok {
            if !(*this).view.is_null() {
                os.unmap_view((*this).view);
            }
            if !(*this).handle.is_null() {
                os.close_handle((*this).handle);
            }
        }
        (*this).ok = false;
    }
}

/// The plugin's globals: mapping object, view pointer,
/// accumulated time, last
/// paused_simulation_time (initially -1) and the
/// delivered-job frame counter. Nothing else: the kernel32
/// to call (or a test double) is passed to init/shutdown/DllMain.
pub struct Globals {
    pub mapping: *mut SharedMemory,
    pub telemetry: *mut u8,
    pub timestamp: u64,
    pub last_timestamp: u64,
    pub finished_frames: i32,
}

pub struct GlobalCell(UnsafeCell<Globals>);
// SAFETY: the SCS games call the plugin from one thread.
unsafe impl Sync for GlobalCell {}

pub static GLOBALS: GlobalCell = GlobalCell(UnsafeCell::new(Globals {
    mapping: ptr::null_mut(),
    telemetry: ptr::null_mut(),
    timestamp: 0,
    last_timestamp: u64::MAX,
    finished_frames: 0,
}));

#[inline]
pub fn g() -> *mut Globals {
    GLOBALS.0.get()
}

#[inline]
unsafe fn wr32(base: *mut u8, offset: usize, v: u32) {
    ptr::write_unaligned(base.add(offset) as *mut u32, v)
}
#[inline]
unsafe fn rd32(base: *mut u8, offset: usize) -> u32 {
    ptr::read_unaligned(base.add(offset) as *const u32)
}
#[inline]
unsafe fn rdf32(base: *mut u8, offset: usize) -> f32 {
    f32::from_bits(rd32(base, offset))
}
/// A store the compiler may neither merge with its neighbours nor reorder,
/// for the places where the store sequence into the shared view
/// is fixed (another process may read the view at any time). Every
/// such offset is a multiple of the store size and the view is page-aligned.
#[inline(always)]
unsafe fn st<T: Copy>(base: *mut u8, offset: usize, v: T) {
    ptr::write_volatile(base.add(offset) as *mut T, v)
}
#[inline(always)]
unsafe fn st32(base: *mut u8, offset: usize, v: u32) {
    st(base, offset, v)
}
#[cfg(not(target_pointer_width = "32"))]
#[inline(always)]
unsafe fn st64(base: *mut u8, offset: usize, v: u64) {
    st(base, offset, v)
}

// MSVCR110 `memset`, called through its `jmp [__imp_memset]` thunk.
// The thunk is reached by
// a second label (src/imports_*.s `ets2_memset_thunk`) so that LLVM does not
// recognise the call as memset and fold, merge or inline the
// separate calls. Only the release DLL has those import tables (crt.rs);
// builds with std (harness, tests, debug) use the C library's memset.
#[cfg(all(windows, not(any(test, feature = "std", debug_assertions))))]
extern "C" {
    #[link_name = "ets2_memset_thunk"]
    fn memset_thunk(dst: *mut c_void, c: i32, n: usize) -> *mut c_void;
}

/// `memset(dst, c, n)` as one call of MSVCR110's memset (on the host, the
/// same effect).
#[inline(always)]
pub unsafe fn crt_memset(dst: *mut u8, c: i32, n: usize) {
    #[cfg(all(windows, not(any(test, feature = "std", debug_assertions))))]
    memset_thunk(dst as *mut c_void, c, n);
    #[cfg(not(all(windows, not(any(test, feature = "std", debug_assertions)))))]
    ptr::write_bytes(dst, c as u8, n);
}

// `comiss a, b` (a signalling compare, used by the three float tests;
// LLVM's `>`/`>=` give `ucomiss`). False for NaN.
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64 as simd;
#[cfg(target_arch = "x86")]
use core::arch::x86 as simd;

/// `comiss a, b; seta`: a > b, false if unordered.
#[inline(always)]
fn comiss_gt(a: f32, b: f32) -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    unsafe {
        simd::_mm_comigt_ss(simd::_mm_set_ss(a), simd::_mm_set_ss(b)) != 0
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        a > b
    }
}

/// `comiss a, b; jb`: not taken when a >= b, taken if unordered.
#[inline(always)]
fn comiss_ge(a: f32, b: f32) -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    unsafe {
        simd::_mm_comige_ss(simd::_mm_set_ss(a), simd::_mm_set_ss(b)) != 0
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        a >= b
    }
}

/// The job-block clear of frame start (bytes 0xa8..0x234), x64 form:
/// no memset call, a qword at 0x12c (income +
/// delivery time), the dword at 0xa8 (cargo mass), then qwords over the
/// cargo id/name (0xac..0x12b) and the four city/company strings
/// (0x134..0x233), in that order.
///
/// The qwords are not 8-aligned (0x12c, 0xac, ...), which a Rust store may
/// not do, so on x86_64 they are instructions in one asm
/// block; elsewhere (non-x86 hosts) the same bytes are written.
pub unsafe fn clear_job_block_x64(t: *mut u8) {
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!(
        "mov qword ptr [{t} + 0x12c], {z}",
        "mov dword ptr [{t} + 0xa8], {z:e}",
        "mov qword ptr [{t} + 0xac], {z}",
        "mov qword ptr [{t} + 0xb4], {z}",
        "mov qword ptr [{t} + 0xbc], {z}",
        "mov qword ptr [{t} + 0xc4], {z}",
        "mov qword ptr [{t} + 0xcc], {z}",
        "mov qword ptr [{t} + 0xd4], {z}",
        "mov qword ptr [{t} + 0xdc], {z}",
        "mov qword ptr [{t} + 0xe4], {z}",
        "mov qword ptr [{t} + 0xec], {z}",
        "mov qword ptr [{t} + 0xf4], {z}",
        "mov qword ptr [{t} + 0xfc], {z}",
        "mov qword ptr [{t} + 0x104], {z}",
        "mov qword ptr [{t} + 0x10c], {z}",
        "mov qword ptr [{t} + 0x114], {z}",
        "mov qword ptr [{t} + 0x11c], {z}",
        "mov qword ptr [{t} + 0x124], {z}",
        "mov qword ptr [{t} + 0x134], {z}",
        "mov qword ptr [{t} + 0x13c], {z}",
        "mov qword ptr [{t} + 0x144], {z}",
        "mov qword ptr [{t} + 0x14c], {z}",
        "mov qword ptr [{t} + 0x154], {z}",
        "mov qword ptr [{t} + 0x15c], {z}",
        "mov qword ptr [{t} + 0x164], {z}",
        "mov qword ptr [{t} + 0x16c], {z}",
        "mov qword ptr [{t} + 0x174], {z}",
        "mov qword ptr [{t} + 0x17c], {z}",
        "mov qword ptr [{t} + 0x184], {z}",
        "mov qword ptr [{t} + 0x18c], {z}",
        "mov qword ptr [{t} + 0x194], {z}",
        "mov qword ptr [{t} + 0x19c], {z}",
        "mov qword ptr [{t} + 0x1a4], {z}",
        "mov qword ptr [{t} + 0x1ac], {z}",
        "mov qword ptr [{t} + 0x1b4], {z}",
        "mov qword ptr [{t} + 0x1bc], {z}",
        "mov qword ptr [{t} + 0x1c4], {z}",
        "mov qword ptr [{t} + 0x1cc], {z}",
        "mov qword ptr [{t} + 0x1d4], {z}",
        "mov qword ptr [{t} + 0x1dc], {z}",
        "mov qword ptr [{t} + 0x1e4], {z}",
        "mov qword ptr [{t} + 0x1ec], {z}",
        "mov qword ptr [{t} + 0x1f4], {z}",
        "mov qword ptr [{t} + 0x1fc], {z}",
        "mov qword ptr [{t} + 0x204], {z}",
        "mov qword ptr [{t} + 0x20c], {z}",
        "mov qword ptr [{t} + 0x214], {z}",
        "mov qword ptr [{t} + 0x21c], {z}",
        "mov qword ptr [{t} + 0x224], {z}",
        "mov qword ptr [{t} + 0x22c], {z}",
        t = in(reg) t,
        z = in(reg) 0u64,
        options(nostack, preserves_flags),
    );
    #[cfg(not(target_arch = "x86_64"))]
    {
        ptr::write_unaligned(t.add(0x12c) as *mut u64, 0);
        ptr::write_unaligned(t.add(0xa8) as *mut u32, 0);
        for o in (0xac..0x12c).step_by(8).chain((0x134..0x234).step_by(8)) {
            ptr::write_unaligned(t.add(o) as *mut u64, 0);
        }
    }
}

/// The job-block clear, i386 form: dwords at 0x12c,
/// 0x130 and 0xa8, then one 64-byte memset per string (0xac, 0xec, 0x134,
/// 0x174, 0x1b4, 0x1f4), six MSVCR110 memset calls.
pub unsafe fn clear_job_block_i386(t: *mut u8) {
    st32(t, off::JOB_INCOME, 0);
    st32(t, off::JOB_DELIVERY_TIME, 0);
    st32(t, off::CARGO_MASS, 0);
    crt_memset(t.add(off::CARGO_ID), 0, 0x40);
    crt_memset(t.add(off::CARGO_NAME), 0, 0x40);
    crt_memset(t.add(off::SOURCE_CITY), 0, 0x40);
    crt_memset(t.add(off::DESTINATION_CITY), 0, 0x40);
    crt_memset(t.add(off::SOURCE_COMPANY), 0, 0x40);
    crt_memset(t.add(off::DESTINATION_COMPANY), 0, 0x40);
}

// MSVCR110 `strncpy`, imported and
// called through the IAT; on the host (tests) the C
// library's, same semantics.
#[cfg(windows)]
crate::os::iat_import!(
    "msvcr110",
    "C",
    fn crt_strncpy(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char,
    "strncpy",
    "strncpy"
);
#[cfg(not(windows))]
extern "C" {
    #[link_name = "strncpy"]
    fn crt_strncpy(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char;
}

/// `strncpy(dst, src, n)`: copy up to n bytes, zero-fill the rest, no
/// terminator when the source is n bytes or longer.
pub unsafe fn strncpy(dst: *mut u8, src: *const c_char, n: usize) {
    crt_strncpy(dst as *mut c_char, src, n);
}

/// The string equality of the configuration handler:
/// byte compare up to and including the first NUL.
unsafe fn str_eq(a: *const c_char, b: &CStr) -> bool {
    let a = a as *const u8;
    let b = b.as_ptr() as *const u8;
    let mut i = 0;
    loop {
        let c = *a.add(i);
        if c != *b.add(i) {
            return false;
        }
        if c == 0 {
            return true;
        }
        i += 1;
    }
}

/// The string copy of the id handlers; returns the length.
unsafe fn strcpy_len(dst: *mut u8, src: *const c_char) -> u32 {
    let src = src as *const u8;
    let mut i = 0usize;
    loop {
        let c = *src.add(i);
        *dst.add(i) = c;
        if c == 0 {
            break;
        }
        i += 1;
    }
    i as u32
}

/// scs_telemetry_init: creates the mapping and registers the events and channels.
pub unsafe fn telemetry_init(version: u32, params: *const ScsTelemetryInitParams, os: &'static dyn Os) -> ScsResult {
    let g = g();
    if version != SCS_TELEMETRY_VERSION_1_00 {
        return SCS_RESULT_UNSUPPORTED;
    }
    if params.is_null() {
        return SCS_RESULT_GENERIC_ERROR;
    }
    // operator new(0x28) (i386 0x14); a failed allocation leaves the global
    // NULL. A previous object is not freed.
    let obj = os.operator_new(core::mem::size_of::<SharedMemory>()) as *mut SharedMemory;
    (*g).mapping = if obj.is_null() { ptr::null_mut() } else { SharedMemory::construct(obj, os) };
    if (*g).mapping.is_null() || !(*(*g).mapping).ok {
        return SCS_RESULT_GENERIC_ERROR;
    }
    let t = (*(*g).mapping).view;
    (*g).telemetry = t;
    if t.is_null() {
        return SCS_RESULT_GENERIC_ERROR;
    }
    // A second full memset (the constructor did one), then the header in a
    // fixed store order (x64 writes offset+length as one qword each,
    // i386 the two offsets, then the two lengths).
    crt_memset(t, 0, MAP_SIZE);
    let p = &*params;
    st32(t, off::PAUSED, 1);
    st32(t, off::TIME, 0);
    st32(t, off::REVISION, layout::PLUGIN_REVISION);
    st32(t, off::GAME_VERSION_MAJOR, p.common.game_version >> 16);
    st32(t, off::GAME_VERSION_MINOR, p.common.game_version & 0xffff);
    #[cfg(not(target_pointer_width = "32"))]
    {
        st64(t, off::MODEL_OFFSET, layout::MODEL_STRING_OFFSET as u64);
        st64(t, off::TRAILER_OFFSET, layout::TRAILER_STRING_OFFSET as u64);
    }
    #[cfg(target_pointer_width = "32")]
    {
        st32(t, off::MODEL_OFFSET, layout::MODEL_STRING_OFFSET);
        st32(t, off::TRAILER_OFFSET, layout::TRAILER_STRING_OFFSET);
        st32(t, off::MODEL_LENGTH, 0);
        st32(t, off::TRAILER_LENGTH, 0);
    }

    let events_ok = (p.register_for_event)(SCS_TELEMETRY_EVENT_FRAME_START, on_frame_start, ptr::null_mut()) == SCS_RESULT_OK
        && (p.register_for_event)(SCS_TELEMETRY_EVENT_PAUSED, on_pause_change, ptr::null_mut()) == SCS_RESULT_OK
        && (p.register_for_event)(SCS_TELEMETRY_EVENT_STARTED, on_pause_change, ptr::null_mut()) == SCS_RESULT_OK;
    // Registered whatever the result above, and its own result is ignored.
    (p.register_for_event)(SCS_TELEMETRY_EVENT_CONFIGURATION, on_configuration, ptr::null_mut());
    if !events_ok {
        return SCS_RESULT_GENERIC_ERROR;
    }
    for c in CHANNELS.iter() {
        let cb: ScsChannelCallback = match c.kind {
            ChannelKind::Word => on_channel_word,
            ChannelKind::Bool => on_channel_bool,
            ChannelKind::FVector => on_channel_fvector,
            ChannelKind::DPlacement => on_channel_dplacement,
        };
        // Results ignored (the duplicate truck.electric.enabled may be refused).
        (p.register_for_channel)(
            c.name.as_ptr(),
            SCS_U32_NIL,
            c.value_type,
            SCS_TELEMETRY_CHANNEL_FLAG_NO_VALUE,
            cb,
            (*g).telemetry.add(c.offset) as ScsContext,
        );
    }
    (*g).timestamp = 0;
    (*g).last_timestamp = u64::MAX;
    SCS_RESULT_OK
}

/// scs_telemetry_shutdown: tests only
/// the mapping global. The globals keep their pointers.
pub unsafe fn telemetry_shutdown(os: &dyn Os) {
    let m = (*g()).mapping;
    if !m.is_null() {
        SharedMemory::close(m, os);
    }
}

/// DllMain: closes the mapping on
/// DLL_PROCESS_DETACH (0) and returns TRUE for every reason.
pub unsafe fn dll_main(reason: u32, os: &dyn Os) -> i32 {
    if reason != 0 {
        return 1;
    }
    telemetry_shutdown(os);
    1
}

/// The frame-start callback.
pub unsafe extern "system" fn on_frame_start(_event: u32, event_info: *const c_void, _context: ScsContext) {
    let g = g();
    let info = &*(event_info as *const ScsTelemetryFrameStart);
    let now = info.paused_simulation_time;
    let mut last = (*g).last_timestamp;
    if last == u64::MAX {
        last = now;
    }
    if info.flags & SCS_TELEMETRY_FRAME_START_FLAG_TIMER_RESTART != 0 {
        last = 0;
    }
    (*g).last_timestamp = now;
    (*g).timestamp = (*g).timestamp.wrapping_add(now.wrapping_sub(last));
    let t = (*g).telemetry;
    if t.is_null() {
        return;
    }
    wr32(t, off::TIME, (*g).timestamp as u32);
    // Cruise control is active when its value is greater than 0.
    *t.add(off::CRUISE_CONTROL_ACTIVE) = comiss_gt(rdf32(t, off::CRUISE_CONTROL), 0.0) as u8;

    if *t.add(off::ON_JOB) == 1 && *t.add(off::TRAILER_CONNECTED) == 0 {
        let distance = rdf32(t, off::NAVIGATION_DISTANCE);
        // The distance is at most 0.1 and at least 0: false for NaN.
        if comiss_ge(layout::DELIVERY_DISTANCE, distance) && comiss_ge(distance, 0.0) {
            // One 16-bit store: on_job = 0, job_finished = 1.
            st(t, off::ON_JOB, 0x100u16);
            (*g).finished_frames = 0;
            return;
        }
    }
    if *t.add(off::JOB_FINISHED) != 0 {
        (*g).finished_frames = (*g).finished_frames.wrapping_add(1);
        if (*g).finished_frames > layout::JOB_CLEAR_FRAMES {
            #[cfg(not(target_pointer_width = "32"))]
            clear_job_block_x64(t);
            #[cfg(target_pointer_width = "32")]
            clear_job_block_i386(t);
            *t.add(off::JOB_FINISHED) = 0;
        }
        return;
    }
    if rd32(t, off::JOB_INCOME) != 0 && *t.add(off::TRAILER_CONNECTED) != 0 {
        *t.add(off::ON_JOB) = 1;
    }
}

/// The paused / started callback.
pub unsafe extern "system" fn on_pause_change(event: u32, _event_info: *const c_void, _context: ScsContext) {
    let t = (*g()).telemetry;
    if !t.is_null() {
        wr32(t, off::PAUSED, (event == SCS_TELEMETRY_EVENT_PAUSED) as u32);
    }
}

/// The configuration callback.
pub unsafe extern "system" fn on_configuration(_event: u32, event_info: *const c_void, _context: ScsContext) {
    let info = &*(event_info as *const ScsTelemetryConfiguration);
    let mut attr = info.attributes;
    while !(*attr).name.is_null() {
        let name = (*attr).name;
        if let Some(a) = ATTRIBUTES.iter().find(|a| str_eq(name, a.name)) {
            apply_attribute(a.action, attr);
        }
        attr = attr.add(1);
    }
}

unsafe fn apply_attribute(action: AttrAction, attr: *const ScsNamedValue) {
    let g = g();
    let t = (*g).telemetry;
    let data = &(*attr).value.value;
    match action {
        AttrAction::Str64(o) => {
            if !t.is_null() {
                strncpy(t.add(o), data.value_string, 64);
            }
        }
        AttrAction::WordUnchecked(o) => wr32(t, o, data.value_u32),
        AttrAction::Word(o) => {
            if !t.is_null() {
                wr32(t, o, data.value_u32);
            }
        }
        AttrAction::ModelId => {
            let s = data.value_string;
            if *(s as *const u8) == b'v' {
                let base = (*(*g).mapping).view;
                let dst = base.offset(rd32(t, off::MODEL_OFFSET) as i32 as isize);
                let len = strcpy_len(dst, s);
                wr32(t, off::MODEL_LENGTH, len);
            }
        }
        AttrAction::TrailerId => {
            let base = (*(*g).mapping).view;
            let dst = base.offset(rd32(t, off::TRAILER_OFFSET) as i32 as isize);
            let len = strcpy_len(dst, data.value_string);
            wr32(t, off::TRAILER_LENGTH, len);
        }
        AttrAction::Ratio(o) => {
            if !t.is_null() {
                let i = (*attr).index as i32;
                if i < 24 {
                    let at = t.wrapping_offset((o as isize).wrapping_add((i as isize).wrapping_mul(4)));
                    ptr::write_unaligned(at as *mut u32, data.value_u32);
                }
            }
        }
    }
}

/// The callback of float, s32 and u32 channels.
pub unsafe extern "system" fn on_channel_word(_n: *const c_char, _i: u32, value: *const ScsValue, context: ScsContext) {
    if !value.is_null() {
        ptr::write_unaligned(context as *mut u32, (*value).value.value_u32);
    }
}

/// The callback of bool channels.
pub unsafe extern "system" fn on_channel_bool(_n: *const c_char, _i: u32, value: *const ScsValue, context: ScsContext) {
    if context.is_null() {
        return;
    }
    *(context as *mut u8) = if value.is_null() { 0 } else { ((*value).value.value_bool != 0) as u8 };
}

/// The callback of fvector channels.
pub unsafe extern "system" fn on_channel_fvector(_n: *const c_char, _i: u32, value: *const ScsValue, context: ScsContext) {
    if !value.is_null() {
        let v = (*value).value.value_fvector;
        let d = context as *mut u32;
        ptr::write_unaligned(d, v[0].to_bits());
        ptr::write_unaligned(d.add(1), v[1].to_bits());
        ptr::write_unaligned(d.add(2), v[2].to_bits());
    }
}

/// The callback of dplacement channels (each coordinate narrowed to f32,
/// euler copied).
pub unsafe extern "system" fn on_channel_dplacement(_n: *const c_char, _i: u32, value: *const ScsValue, context: ScsContext) {
    if !value.is_null() {
        let p = (*value).value.value_dplacement;
        let d = context as *mut f32;
        ptr::write_unaligned(d, p.position[0] as f32);
        ptr::write_unaligned(d.add(1), p.position[1] as f32);
        ptr::write_unaligned(d.add(2), p.position[2] as f32);
        ptr::write_unaligned(d.add(3), p.orientation[0]);
        ptr::write_unaligned(d.add(4), p.orientation[1]);
        ptr::write_unaligned(d.add(5), p.orientation[2]);
    }
}
