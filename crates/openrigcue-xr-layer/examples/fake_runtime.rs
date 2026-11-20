//! A minimal fake OpenXR *runtime* (cdylib) for `tests/real_loader.rs`: the system's Khronos loader
//! loads it through `XR_RUNTIME_JSON`, with the real layer in between. It tracks one head pose set
//! by the test and records the last submitted projection view; nothing touches a real VR runtime.
//!
//! Built by `cargo test` as `examples/libfake_runtime.so`.

use std::ffi::{c_char, CStr};
use std::ptr;
use std::sync::Mutex;

use openxr_sys as xr;
use xr::{pfn, Handle};

#[derive(Clone, Copy)]
struct State {
    /// The tracked head in STAGE: x, y, z, qx, qy, qz, qw.
    head: [f32; 7],
    /// The pose of the first projection view of the last xrEndFrame, same layout.
    submitted: [f32; 7],
    focused_pending: bool,
    timespec_enabled: bool,
    next_handle: u64,
    /// Reference-space type per space handle (index = handle − 1000), VIEW or not.
    view_spaces: [bool; 64],
}

static STATE: Mutex<State> = Mutex::new(State {
    head: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
    submitted: [0.0; 7],
    focused_pending: false,
    timespec_enabled: false,
    next_handle: 1000,
    view_spaces: [false; 64],
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn posef(a: [f32; 7]) -> xr::Posef {
    xr::Posef {
        orientation: xr::Quaternionf { x: a[3], y: a[4], z: a[5], w: a[6] },
        position: xr::Vector3f { x: a[0], y: a[1], z: a[2] },
    }
}

fn array(p: &xr::Posef) -> [f32; 7] {
    [p.position.x, p.position.y, p.position.z, p.orientation.x, p.orientation.y, p.orientation.z, p.orientation.w]
}

/// Sets the tracked head pose (x, y, z, qx, qy, qz, qw).
///
/// # Safety
/// `pose` points to 7 floats.
#[no_mangle]
pub unsafe extern "C" fn fake_runtime_set_head(pose: *const f32) {
    // SAFETY: guaranteed by the caller.
    let p = unsafe { std::slice::from_raw_parts(pose, 7) };
    state().head.copy_from_slice(p);
}

/// Copies the last submitted projection view pose into `out` (7 floats).
///
/// # Safety
/// `out` points to 7 writable floats.
#[no_mangle]
pub unsafe extern "C" fn fake_runtime_submitted(out: *mut f32) {
    let s = state().submitted;
    // SAFETY: guaranteed by the caller.
    unsafe { ptr::copy_nonoverlapping(s.as_ptr(), out, 7) };
}

/// Whether the instance was created with XR_KHR_convert_timespec_time.
#[no_mangle]
pub extern "C" fn fake_runtime_timespec_enabled() -> bool {
    state().timespec_enabled
}

/// The loader's entry point into the runtime.
///
/// # Safety
/// Called by the loader with valid negotiation structures.
#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "system" fn xrNegotiateLoaderRuntimeInterface(
    _info: *const xr::NegotiateLoaderInfo,
    request: *mut xr::NegotiateRuntimeRequest,
) -> xr::Result {
    // SAFETY: the loader's valid request structure.
    let req = unsafe { &mut *request };
    req.runtime_interface_version = 1;
    req.runtime_api_version = xr::Version::new(1, 0, 0);
    req.get_instance_proc_addr = Some(gipa);
    xr::Result::SUCCESS
}

macro_rules! erase {
    ($f:expr, $ty:ty) => {{
        let f: $ty = $f;
        // SAFETY: type erasure as PFN_xrVoidFunction; callers cast back to `$ty`.
        Some(unsafe { std::mem::transmute::<$ty, pfn::VoidFunction>(f) })
    }};
}

unsafe extern "system" fn gipa(
    _instance: xr::Instance,
    name: *const c_char,
    function: *mut Option<pfn::VoidFunction>,
) -> xr::Result {
    // SAFETY: NUL-terminated name from the loader.
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    let f = match name {
        b"xrGetInstanceProcAddr" => erase!(gipa, pfn::GetInstanceProcAddr),
        b"xrEnumerateInstanceExtensionProperties" => {
            erase!(enumerate_extensions, pfn::EnumerateInstanceExtensionProperties)
        }
        b"xrCreateInstance" => erase!(create_instance, pfn::CreateInstance),
        b"xrDestroyInstance" => erase!(ok_instance, pfn::DestroyInstance),
        b"xrGetInstanceProperties" => erase!(instance_properties, pfn::GetInstanceProperties),
        b"xrPollEvent" => erase!(poll_event, pfn::PollEvent),
        b"xrCreateSession" => erase!(create_session, pfn::CreateSession),
        b"xrDestroySession" => erase!(ok_session, pfn::DestroySession),
        b"xrCreateReferenceSpace" => erase!(create_reference_space, pfn::CreateReferenceSpace),
        b"xrDestroySpace" => erase!(ok_space, pfn::DestroySpace),
        b"xrLocateSpace" => erase!(locate_space, pfn::LocateSpace),
        b"xrWaitFrame" => erase!(wait_frame, pfn::WaitFrame),
        b"xrEndFrame" => erase!(end_frame, pfn::EndFrame),
        b"xrConvertTimeToTimespecTimeKHR" => {
            erase!(convert_time, pfn::ConvertTimeToTimespecTimeKHR)
        }
        _ => None,
    };
    // SAFETY: valid out-pointer.
    unsafe { *function = f };
    if f.is_some() {
        xr::Result::SUCCESS
    } else {
        xr::Result::ERROR_FUNCTION_UNSUPPORTED
    }
}

unsafe extern "system" fn enumerate_extensions(
    _layer: *const c_char,
    capacity: u32,
    count: *mut u32,
    props: *mut xr::ExtensionProperties,
) -> xr::Result {
    // SAFETY: valid out-pointer.
    unsafe { *count = 1 };
    if capacity >= 1 {
        // SAFETY: the caller provides `capacity` elements.
        let p = unsafe { &mut *props };
        for (d, s) in p.extension_name.iter_mut().zip(b"XR_KHR_convert_timespec_time\0") {
            *d = *s as c_char;
        }
        p.extension_version = 1;
    }
    xr::Result::SUCCESS
}

unsafe extern "system" fn create_instance(
    info: *const xr::InstanceCreateInfo,
    instance: *mut xr::Instance,
) -> xr::Result {
    // SAFETY: valid create info with `enabled_extension_count` names.
    let info = unsafe { &*info };
    let enabled = (0..info.enabled_extension_count as usize)
        // SAFETY: as above.
        .any(|i| unsafe { CStr::from_ptr(*info.enabled_extension_names.add(i)) } == c"XR_KHR_convert_timespec_time");
    state().timespec_enabled = enabled;
    // SAFETY: valid out-pointer.
    unsafe { *instance = xr::Instance::from_raw(1) };
    xr::Result::SUCCESS
}

unsafe extern "system" fn ok_instance(_: xr::Instance) -> xr::Result {
    xr::Result::SUCCESS
}

unsafe extern "system" fn ok_session(_: xr::Session) -> xr::Result {
    xr::Result::SUCCESS
}

unsafe extern "system" fn ok_space(_: xr::Space) -> xr::Result {
    xr::Result::SUCCESS
}

unsafe extern "system" fn instance_properties(_: xr::Instance, props: *mut xr::InstanceProperties) -> xr::Result {
    // SAFETY: valid out-structure.
    let p = unsafe { &mut *props };
    for (d, s) in p.runtime_name.iter_mut().zip(b"FakeRuntime\0") {
        *d = *s as c_char;
    }
    xr::Result::SUCCESS
}

unsafe extern "system" fn poll_event(_: xr::Instance, buffer: *mut xr::EventDataBuffer) -> xr::Result {
    let mut s = state();
    if !s.focused_pending {
        return xr::Result::EVENT_UNAVAILABLE;
    }
    s.focused_pending = false;
    let ev = xr::EventDataSessionStateChanged {
        ty: xr::StructureType::EVENT_DATA_SESSION_STATE_CHANGED,
        next: ptr::null(),
        session: xr::Session::from_raw(2),
        state: xr::SessionState::FOCUSED,
        time: xr::Time::from_nanos(0),
    };
    // SAFETY: the event buffer holds any event structure.
    unsafe { ptr::write(buffer as *mut xr::EventDataSessionStateChanged, ev) };
    xr::Result::SUCCESS
}

unsafe extern "system" fn create_session(
    _: xr::Instance,
    _: *const xr::SessionCreateInfo,
    session: *mut xr::Session,
) -> xr::Result {
    state().focused_pending = true;
    // SAFETY: valid out-pointer.
    unsafe { *session = xr::Session::from_raw(2) };
    xr::Result::SUCCESS
}

unsafe extern "system" fn create_reference_space(
    _: xr::Session,
    info: *const xr::ReferenceSpaceCreateInfo,
    space: *mut xr::Space,
) -> xr::Result {
    let mut s = state();
    let h = s.next_handle;
    s.next_handle += 1;
    // SAFETY: valid create info.
    s.view_spaces[(h - 1000) as usize % 64] = unsafe { (*info).reference_space_type } == xr::ReferenceSpaceType::VIEW;
    // SAFETY: valid out-pointer.
    unsafe { *space = xr::Space::from_raw(h) };
    xr::Result::SUCCESS
}

fn is_view(s: &State, space: xr::Space) -> bool {
    s.view_spaces[(space.into_raw().wrapping_sub(1000)) as usize % 64]
}

/// Every non-VIEW space is the STAGE origin; VIEW is the head. Only head-in-world and
/// world-in-world are needed by the test.
unsafe extern "system" fn locate_space(
    space: xr::Space,
    base: xr::Space,
    _: xr::Time,
    loc: *mut xr::SpaceLocation,
) -> xr::Result {
    let s = state();
    let pose = match (is_view(&s, space), is_view(&s, base)) {
        (true, false) => posef(s.head),
        _ => posef([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
    };
    // SAFETY: valid out-structure.
    let loc = unsafe { &mut *loc };
    loc.location_flags = xr::SpaceLocationFlags::from_raw(0b1111);
    loc.pose = pose;
    xr::Result::SUCCESS
}

unsafe extern "system" fn wait_frame(
    _: xr::Session,
    _: *const xr::FrameWaitInfo,
    state_out: *mut xr::FrameState,
) -> xr::Result {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let now = ts.tv_sec * 1_000_000_000 + ts.tv_nsec;
    // SAFETY: valid out-structure.
    let st = unsafe { &mut *state_out };
    st.predicted_display_time = xr::Time::from_nanos(now + 11_000_000);
    st.predicted_display_period = xr::Duration::from_nanos(11_111_111);
    st.should_render = xr::TRUE;
    xr::Result::SUCCESS
}

unsafe extern "system" fn end_frame(_: xr::Session, info: *const xr::FrameEndInfo) -> xr::Result {
    // SAFETY: valid frame with one projection layer (as the test submits).
    let info = unsafe { &*info };
    if info.layer_count > 0 {
        // SAFETY: as above.
        let layer = unsafe { &*(*info.layers as *const xr::CompositionLayerProjection) };
        if layer.ty == xr::StructureType::COMPOSITION_LAYER_PROJECTION && layer.view_count > 0 {
            // SAFETY: `view_count` views.
            state().submitted = array(unsafe { &(*layer.views).pose });
        }
    }
    xr::Result::SUCCESS
}

unsafe extern "system" fn convert_time(_: xr::Instance, t: xr::Time, ts: *mut libc::timespec) -> xr::Result {
    let n = t.as_nanos();
    // SAFETY: valid out-pointer.
    unsafe {
        (*ts).tv_sec = n / 1_000_000_000;
        (*ts).tv_nsec = n % 1_000_000_000;
    }
    xr::Result::SUCCESS
}
