//! The OpenXR API-layer ABI: loader negotiation, instance creation, the dispatch chain and the
//! intercepted calls. Every `unsafe` operation of the crate is in this file.
//!
//! Each intercepted call first calls down the chain, then adjusts the results inside
//! [`guarded`], which catches panics so none crosses the C ABI; on any internal failure the
//! runtime's own result passes through unchanged. `xrEndFrame` is the exception: it builds the
//! substitute frame first and falls back to the application's frame if that fails.

use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock};

use openrigcue_rigpose::math::Pose;
use openrigcue_rigpose::monotonic_now_ns;
use openxr_sys as xr;
use xr::{pfn, Handle};

use crate::compensation::{Compensator, FileSource};
use crate::config::Settings;
use crate::log;
use crate::motion::{pose_from_xr, pose_to_xr, quat_from_xr, quat_to_xr, vec_from_xr, vec_to_xr, Motion};

/// The layer's name, as in the manifest.
pub const LAYER_NAME: &str = "XR_APILAYER_OPENRIGCUE_motion_compensation";

const TIMESPEC_EXTENSION: &CStr = c"XR_KHR_convert_timespec_time";

// ---------------------------------------------------------------------------------------------
// Bookkeeping
// ---------------------------------------------------------------------------------------------

/// The next layer's (or the runtime's) entry points for one instance.
#[derive(Clone, Copy)]
struct Dispatch {
    get_instance_proc_addr: pfn::GetInstanceProcAddr,
    destroy_instance: Option<pfn::DestroyInstance>,
    get_instance_properties: Option<pfn::GetInstanceProperties>,
    poll_event: Option<pfn::PollEvent>,
    create_session: Option<pfn::CreateSession>,
    destroy_session: Option<pfn::DestroySession>,
    create_reference_space: Option<pfn::CreateReferenceSpace>,
    create_action_space: Option<pfn::CreateActionSpace>,
    destroy_space: Option<pfn::DestroySpace>,
    locate_space: Option<pfn::LocateSpace>,
    locate_spaces: Option<pfn::LocateSpaces>,
    locate_spaces_khr: Option<pfn::LocateSpacesKHR>,
    locate_views: Option<pfn::LocateViews>,
    wait_frame: Option<pfn::WaitFrame>,
    end_frame: Option<pfn::EndFrame>,
    convert_time_to_timespec: Option<pfn::ConvertTimeToTimespecTimeKHR>,
}

struct InstanceCtx {
    handle: xr::Instance,
    dispatch: Dispatch,
    settings: Settings,
}

/// How a space moves relative to the rig.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpaceClass {
    /// Moves with the rig: VIEW spaces, and action spaces (controllers) when they are compensated.
    Attached,
    /// Fixed to the room as far as compensation is concerned. `cacheable` when its location in the
    /// tracking space does not change between reference-space events (reference spaces).
    World { cacheable: bool },
}

#[derive(Debug, Clone, Copy)]
struct SpaceInfo {
    session: u64,
    class: SpaceClass,
    /// A VIEW reference space: its compensated locations are what the application renders with.
    head: bool,
}

/// The time base when `XR_KHR_convert_timespec_time` is not available: the last predicted display
/// time and the monotonic time it was assumed to correspond to.
#[derive(Debug, Default, Clone, Copy)]
struct FallbackClock {
    anchor: Option<(i64, u64)>,
}

struct SessionCtx {
    instance: Arc<InstanceCtx>,
    /// The layer's own tracking space (STAGE, else LOCAL) and its type.
    tracking: Option<(xr::Space, xr::ReferenceSpaceType)>,
    /// The layer's own VIEW space, for calibration.
    view: Option<xr::Space>,
    comp: Mutex<Compensator<FileSource>>,
    clock: Mutex<FallbackClock>,
}

#[derive(Default)]
struct Registry {
    instances: HashMap<u64, Arc<InstanceCtx>>,
    sessions: HashMap<u64, Arc<SessionCtx>>,
    spaces: HashMap<u64, SpaceInfo>,
    /// The most recent next-in-chain `xrGetInstanceProcAddr`, for calls with `XR_NULL_HANDLE`.
    last_next_gipa: Option<pfn::GetInstanceProcAddr>,
}

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(Default::default);

fn registry() -> std::sync::RwLockReadGuard<'static, Registry> {
    REGISTRY.read().unwrap_or_else(|e| e.into_inner())
}

fn registry_mut() -> std::sync::RwLockWriteGuard<'static, Registry> {
    REGISTRY.write().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn instance_ctx(instance: xr::Instance) -> Option<Arc<InstanceCtx>> {
    registry().instances.get(&instance.into_raw()).cloned()
}

fn session_ctx(session: xr::Session) -> Option<Arc<SessionCtx>> {
    registry().sessions.get(&session.into_raw()).cloned()
}

/// Any instance's dispatch, for a call whose handles the layer has not seen (all instances of a
/// process share one runtime).
fn any_instance() -> Option<Arc<InstanceCtx>> {
    registry().instances.values().next().cloned()
}

fn space_info(space: xr::Space) -> Option<SpaceInfo> {
    registry().spaces.get(&space.into_raw()).copied()
}

/// The class of a space; spaces the layer did not see created (for example spatial anchors) are
/// treated as room-fixed and re-located on every use.
fn class_of(space: xr::Space) -> SpaceClass {
    space_info(space).map_or(SpaceClass::World { cacheable: false }, |i| i.class)
}

/// Whether `space` is one of the application's VIEW reference spaces.
fn is_head(space: xr::Space) -> bool {
    space_info(space).is_some_and(|i| i.head)
}

/// Runs the post-processing `f`, catching any panic so it cannot unwind into the caller.
fn guarded(what: &str, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        log::info!("internal error in {what}; results passed through unchanged");
    }
}

#[cfg(test)]
pub(crate) static TEST_SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);

fn instance_settings() -> Settings {
    #[cfg(test)]
    if let Some(s) = lock(&TEST_SETTINGS).clone() {
        return s;
    }
    Settings::from_env()
}

fn identity_posef() -> xr::Posef {
    pose_to_xr(Pose::IDENTITY)
}

// ---------------------------------------------------------------------------------------------
// Loader interface
// ---------------------------------------------------------------------------------------------

/// The loader's entry point into the layer (`xrNegotiateLoaderApiLayerInterface`).
///
/// # Safety
/// Called by the OpenXR loader with pointers to valid negotiation structures (null pointers are
/// rejected).
#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "system" fn xrNegotiateLoaderApiLayerInterface(
    loader_info: *const xr::NegotiateLoaderInfo,
    layer_name: *const c_char,
    request: *mut xr::NegotiateApiLayerRequest,
) -> xr::Result {
    if loader_info.is_null() || request.is_null() {
        return xr::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: non-null, and the loader passes a valid XrNegotiateLoaderInfo it owns for the call.
    let info = unsafe { &*loader_info };
    // SAFETY: non-null, and the loader passes a valid, writable XrNegotiateApiLayerRequest.
    let req = unsafe { &mut *request };
    if !layer_name.is_null() {
        // SAFETY: a non-null layer name from the loader is a NUL-terminated string.
        let name = unsafe { CStr::from_ptr(layer_name) };
        if name.to_bytes() != LAYER_NAME.as_bytes() {
            return xr::Result::ERROR_INITIALIZATION_FAILED;
        }
    }
    match negotiate(info, req) {
        Ok(()) => xr::Result::SUCCESS,
        Err(e) => e,
    }
}

fn negotiate(info: &xr::NegotiateLoaderInfo, req: &mut xr::NegotiateApiLayerRequest) -> Result<(), xr::Result> {
    let fail = Err(xr::Result::ERROR_INITIALIZATION_FAILED);
    if info.struct_type != xr::NegotiateLoaderInfo::TYPE
        || info.struct_version != xr::NegotiateLoaderInfo::VERSION
        || info.struct_size != std::mem::size_of::<xr::NegotiateLoaderInfo>()
        || req.struct_type != xr::NegotiateApiLayerRequest::TYPE
        || req.struct_version != xr::NegotiateApiLayerRequest::VERSION
        || req.struct_size != std::mem::size_of::<xr::NegotiateApiLayerRequest>()
    {
        return fail;
    }
    let ours = xr::CURRENT_LOADER_API_LAYER_VERSION as u32;
    if ours < info.min_interface_version || ours > info.max_interface_version {
        return fail;
    }
    // The layer works with OpenXR 1.0 and 1.1 applications.
    let lowest = xr::Version::new(1, 0, 0);
    if info.max_api_version < lowest || info.min_api_version.major() > 1 {
        return fail;
    }
    req.layer_interface_version = ours;
    req.layer_api_version = xr::CURRENT_API_VERSION.min(info.max_api_version);
    req.get_instance_proc_addr = Some(get_instance_proc_addr);
    req.create_api_layer_instance = Some(create_api_layer_instance);
    Ok(())
}

/// Copies a `count`-element array from a C pointer (empty for null or zero).
///
/// # Safety
/// If `p` is non-null it must point to at least `count` initialised elements.
unsafe fn c_array<'a, T>(p: *const T, count: u32) -> &'a [T] {
    if p.is_null() || count == 0 {
        &[]
    } else {
        // SAFETY: guaranteed by the caller.
        unsafe { std::slice::from_raw_parts(p, count as usize) }
    }
}

/// `xrGetInstanceProcAddr` through `gipa`, cast to the function type `$ty`.
macro_rules! load {
    ($gipa:expr, $instance:expr, $name:literal, $ty:ty) => {{
        let mut f: Option<pfn::VoidFunction> = None;
        // SAFETY: `gipa` is the next layer's xrGetInstanceProcAddr, called with a NUL-terminated
        // name and a valid out-pointer.
        let r = unsafe { ($gipa)($instance, $name.as_ptr(), &mut f) };
        match f {
            Some(f) if r == xr::Result::SUCCESS => {
                // SAFETY: the OpenXR registry defines `$name` with exactly the signature `$ty`;
                // the loader returns it type-erased as PFN_xrVoidFunction (same size and ABI).
                let typed = unsafe { std::mem::transmute::<pfn::VoidFunction, $ty>(f) };
                Some(typed)
            }
            _ => None,
        }
    }};
}

fn load_dispatch(gipa: pfn::GetInstanceProcAddr, instance: xr::Instance, timespec: bool) -> Dispatch {
    Dispatch {
        get_instance_proc_addr: gipa,
        destroy_instance: load!(gipa, instance, c"xrDestroyInstance", pfn::DestroyInstance),
        get_instance_properties: load!(gipa, instance, c"xrGetInstanceProperties", pfn::GetInstanceProperties),
        poll_event: load!(gipa, instance, c"xrPollEvent", pfn::PollEvent),
        create_session: load!(gipa, instance, c"xrCreateSession", pfn::CreateSession),
        destroy_session: load!(gipa, instance, c"xrDestroySession", pfn::DestroySession),
        create_reference_space: load!(gipa, instance, c"xrCreateReferenceSpace", pfn::CreateReferenceSpace),
        create_action_space: load!(gipa, instance, c"xrCreateActionSpace", pfn::CreateActionSpace),
        destroy_space: load!(gipa, instance, c"xrDestroySpace", pfn::DestroySpace),
        locate_space: load!(gipa, instance, c"xrLocateSpace", pfn::LocateSpace),
        locate_spaces: load!(gipa, instance, c"xrLocateSpaces", pfn::LocateSpaces),
        locate_spaces_khr: load!(gipa, instance, c"xrLocateSpacesKHR", pfn::LocateSpacesKHR),
        locate_views: load!(gipa, instance, c"xrLocateViews", pfn::LocateViews),
        wait_frame: load!(gipa, instance, c"xrWaitFrame", pfn::WaitFrame),
        end_frame: load!(gipa, instance, c"xrEndFrame", pfn::EndFrame),
        convert_time_to_timespec: if timespec {
            load!(gipa, instance, c"xrConvertTimeToTimespecTimeKHR", pfn::ConvertTimeToTimespecTimeKHR)
        } else {
            None
        },
    }
}

/// Whether the rest of the chain offers `XR_KHR_convert_timespec_time`.
fn chain_supports_timespec(gipa: pfn::GetInstanceProcAddr) -> bool {
    let Some(enumerate) = load!(
        gipa,
        xr::Instance::NULL,
        c"xrEnumerateInstanceExtensionProperties",
        pfn::EnumerateInstanceExtensionProperties
    ) else {
        return false;
    };
    let mut count = 0u32;
    // SAFETY: capacity 0 with a null array is the size query; `count` is a valid out-pointer.
    if unsafe { enumerate(ptr::null(), 0, &mut count, ptr::null_mut()) } != xr::Result::SUCCESS || count == 0 {
        return false;
    }
    let blank = xr::ExtensionProperties {
        ty: xr::StructureType::EXTENSION_PROPERTIES,
        next: ptr::null_mut(),
        extension_name: [0; xr::MAX_EXTENSION_NAME_SIZE],
        extension_version: 0,
    };
    let mut props = vec![blank; count as usize];
    // SAFETY: `props` has `count` initialised elements with their type set, as the call requires.
    if unsafe { enumerate(ptr::null(), count, &mut count, props.as_mut_ptr()) } != xr::Result::SUCCESS {
        return false;
    }
    props.iter().take(count as usize).any(|p| c_name_eq(&p.extension_name, TIMESPEC_EXTENSION))
}

fn c_name_eq(buf: &[c_char], name: &CStr) -> bool {
    let want = name.to_bytes();
    buf.len() > want.len() && buf.iter().zip(want).all(|(a, b)| *a as u8 == *b) && buf[want.len()] == 0
}

/// `xrCreateApiLayerInstance`: creates the instance down the chain, enabling
/// `XR_KHR_convert_timespec_time` when the runtime has it (so XrTime can be mapped to the pose file's
/// `CLOCK_MONOTONIC`), and records the next layer's entry points.
unsafe extern "system" fn create_api_layer_instance(
    info: *const xr::InstanceCreateInfo,
    layer_info: *const xr::ApiLayerCreateInfo,
    instance: *mut xr::Instance,
) -> xr::Result {
    if info.is_null() || layer_info.is_null() || instance.is_null() {
        return xr::Result::ERROR_VALIDATION_FAILURE;
    }
    // SAFETY: non-null; the loader passes valid structures for the duration of the call.
    let (app_info, li) = unsafe { (&*info, &*layer_info) };
    if li.struct_type != xr::ApiLayerCreateInfo::TYPE || li.next_info.is_null() {
        return xr::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: checked non-null; the loader's chain of XrApiLayerNextInfo outlives the call.
    let next = unsafe { &*li.next_info };
    let (Some(next_gipa), Some(next_create)) = (next.next_get_instance_proc_addr, next.next_create_api_layer_instance)
    else {
        return xr::Result::ERROR_INITIALIZATION_FAILED;
    };
    if next.struct_type != xr::ApiLayerNextInfo::TYPE || !c_name_eq(&next.layer_name, &cstring_of(LAYER_NAME)) {
        log::info!("unexpected layer chain entry; continuing");
    }
    let mut down_layer_info = *li;
    down_layer_info.next_info = next.next;

    // SAFETY: the application's extension list has `enabled_extension_count` valid C strings.
    let app_exts = unsafe { c_array(app_info.enabled_extension_names, app_info.enabled_extension_count) };
    // SAFETY: each entry is a NUL-terminated extension name.
    let app_has_timespec = app_exts.iter().any(|&p| !p.is_null() && unsafe { CStr::from_ptr(p) } == TIMESPEC_EXTENSION);
    let add_timespec = !app_has_timespec && guarded_bool(|| chain_supports_timespec(next_gipa));

    let mut timespec = app_has_timespec;
    let mut result = xr::Result::ERROR_RUNTIME_FAILURE;
    if add_timespec {
        let mut names: Vec<*const c_char> = app_exts.to_vec();
        names.push(TIMESPEC_EXTENSION.as_ptr());
        let mut down_info = *app_info;
        down_info.enabled_extension_count = names.len() as u32;
        down_info.enabled_extension_names = names.as_ptr();
        // SAFETY: `down_info` is the application's create info with a longer extension list that
        // lives (in `names`) until the call returns; `instance` is the caller's out-pointer.
        result = unsafe { next_create(&down_info, &down_layer_info, instance) };
        timespec = result == xr::Result::SUCCESS;
    }
    if result != xr::Result::SUCCESS {
        // Without our extension (not offered, or refused): the application's create info unchanged.
        // SAFETY: forwarding the application's arguments.
        result = unsafe { next_create(info, &down_layer_info, instance) };
    }
    if result != xr::Result::SUCCESS {
        return result;
    }
    // SAFETY: on success the next layer wrote the new handle to `instance`.
    let handle = unsafe { *instance };
    guarded("xrCreateApiLayerInstance", || register_instance(handle, next_gipa, timespec));
    result
}

fn cstring_of(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).expect("no NUL in the layer name")
}

fn guarded_bool(f: impl FnOnce() -> bool) -> bool {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(false)
}

fn register_instance(handle: xr::Instance, gipa: pfn::GetInstanceProcAddr, timespec: bool) {
    let settings = instance_settings();
    log::set_level(settings.log_level);
    let dispatch = load_dispatch(gipa, handle, timespec);
    let runtime = runtime_name(&dispatch, handle);
    log::info!(
        "{LAYER_NAME} {} on {runtime}; pose file {}; time mapping {}",
        env!("CARGO_PKG_VERSION"),
        settings.pose_path.display(),
        if dispatch.convert_time_to_timespec.is_some() { "XR_KHR_convert_timespec_time" } else { "estimated" }
    );
    let ctx = Arc::new(InstanceCtx { handle, dispatch, settings });
    let mut reg = registry_mut();
    reg.last_next_gipa = Some(gipa);
    reg.instances.insert(handle.into_raw(), ctx);
}

fn runtime_name(d: &Dispatch, instance: xr::Instance) -> String {
    let Some(get) = d.get_instance_properties else {
        return "unknown runtime".into();
    };
    let mut props = xr::InstanceProperties {
        ty: xr::StructureType::INSTANCE_PROPERTIES,
        next: ptr::null_mut(),
        runtime_version: xr::Version::new(0, 0, 0),
        runtime_name: [0; xr::MAX_RUNTIME_NAME_SIZE],
    };
    // SAFETY: valid instance and a writable, typed XrInstanceProperties.
    if unsafe { get(instance, &mut props) } != xr::Result::SUCCESS {
        return "unknown runtime".into();
    }
    let bytes: Vec<u8> = props.runtime_name.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    format!("{} {}", String::from_utf8_lossy(&bytes), props.runtime_version)
}

/// `xrGetInstanceProcAddr`: our entry points for the intercepted commands the chain provides,
/// everything else from the next layer.
unsafe extern "system" fn get_instance_proc_addr(
    instance: xr::Instance,
    name: *const c_char,
    function: *mut Option<pfn::VoidFunction>,
) -> xr::Result {
    if name.is_null() || function.is_null() {
        return xr::Result::ERROR_VALIDATION_FAILURE;
    }
    // SAFETY: a non-null name is a NUL-terminated command name.
    let cname = unsafe { CStr::from_ptr(name) };
    if cname == c"xrGetInstanceProcAddr" {
        let ours: pfn::GetInstanceProcAddr = get_instance_proc_addr;
        // SAFETY: storing a function pointer, type-erased as the API requires, into the caller's slot.
        unsafe { *function = Some(std::mem::transmute::<pfn::GetInstanceProcAddr, pfn::VoidFunction>(ours)) };
        return xr::Result::SUCCESS;
    }
    let next_gipa = match instance_ctx(instance) {
        Some(ctx) => ctx.dispatch.get_instance_proc_addr,
        None => match registry().last_next_gipa {
            Some(g) if instance == xr::Instance::NULL => g,
            _ => {
                // SAFETY: valid out-pointer.
                unsafe { *function = None };
                return xr::Result::ERROR_HANDLE_INVALID;
            }
        },
    };
    // SAFETY: forwarding the caller's valid arguments to the next layer.
    let r = unsafe { next_gipa(instance, name, function) };
    // SAFETY: valid out-pointer, written by the next layer.
    let found = unsafe { (*function).is_some() };
    if r != xr::Result::SUCCESS || !found || instance == xr::Instance::NULL {
        return r;
    }
    if let Some(ours) = intercept(cname) {
        // SAFETY: valid out-pointer.
        unsafe { *function = Some(ours) };
    }
    r
}

/// Our implementation of `name`, type-erased.
fn intercept(name: &CStr) -> Option<pfn::VoidFunction> {
    macro_rules! erase {
        ($f:expr, $ty:ty) => {{
            let f: $ty = $f;
            // SAFETY: OpenXR returns every command as PFN_xrVoidFunction; the caller casts it back
            // to `$ty`, the command's real signature.
            Some(unsafe { std::mem::transmute::<$ty, pfn::VoidFunction>(f) })
        }};
    }
    match name.to_bytes() {
        b"xrDestroyInstance" => erase!(destroy_instance, pfn::DestroyInstance),
        b"xrPollEvent" => erase!(poll_event, pfn::PollEvent),
        b"xrCreateSession" => erase!(create_session, pfn::CreateSession),
        b"xrDestroySession" => erase!(destroy_session, pfn::DestroySession),
        b"xrCreateReferenceSpace" => erase!(create_reference_space, pfn::CreateReferenceSpace),
        b"xrCreateActionSpace" => erase!(create_action_space, pfn::CreateActionSpace),
        b"xrDestroySpace" => erase!(destroy_space, pfn::DestroySpace),
        b"xrLocateSpace" => erase!(locate_space, pfn::LocateSpace),
        b"xrLocateSpaces" => erase!(locate_spaces, pfn::LocateSpaces),
        b"xrLocateSpacesKHR" => erase!(locate_spaces_khr, pfn::LocateSpacesKHR),
        b"xrLocateViews" => erase!(locate_views, pfn::LocateViews),
        b"xrWaitFrame" => erase!(wait_frame, pfn::WaitFrame),
        b"xrEndFrame" => erase!(end_frame, pfn::EndFrame),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Instance and session lifetime
// ---------------------------------------------------------------------------------------------

unsafe extern "system" fn destroy_instance(instance: xr::Instance) -> xr::Result {
    let Some(ctx) = instance_ctx(instance) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let sessions: Vec<Arc<SessionCtx>> = {
        let mut reg = registry_mut();
        reg.instances.remove(&instance.into_raw());
        let ids: Vec<u64> =
            reg.sessions.iter().filter(|(_, s)| s.instance.handle == instance).map(|(id, _)| *id).collect();
        reg.spaces.retain(|_, info| !ids.contains(&info.session));
        ids.iter().filter_map(|id| reg.sessions.remove(id)).collect()
    };
    for s in sessions {
        guarded("xrDestroyInstance", || lock(&s.comp).shutdown());
    }
    let Some(down) = ctx.dispatch.destroy_instance else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's handle.
    unsafe { down(instance) }
}

unsafe extern "system" fn create_session(
    instance: xr::Instance,
    info: *const xr::SessionCreateInfo,
    session: *mut xr::Session,
) -> xr::Result {
    let Some(ctx) = instance_ctx(instance) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let Some(down) = ctx.dispatch.create_session else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's arguments.
    let r = unsafe { down(instance, info, session) };
    if r == xr::Result::SUCCESS && !session.is_null() {
        // SAFETY: on success the runtime wrote the session handle.
        let handle = unsafe { *session };
        guarded("xrCreateSession", || register_session(ctx, handle));
    }
    r
}

fn create_internal_space(d: &Dispatch, session: xr::Session, ty: xr::ReferenceSpaceType) -> Option<xr::Space> {
    let create = d.create_reference_space?;
    let info = xr::ReferenceSpaceCreateInfo {
        ty: xr::StructureType::REFERENCE_SPACE_CREATE_INFO,
        next: ptr::null(),
        reference_space_type: ty,
        pose_in_reference_space: identity_posef(),
    };
    let mut space = xr::Space::NULL;
    // SAFETY: valid session, typed create info and out-pointer.
    (unsafe { create(session, &info, &mut space) } == xr::Result::SUCCESS).then_some(space)
}

fn register_session(instance: Arc<InstanceCtx>, handle: xr::Session) {
    let d = &instance.dispatch;
    let tracking = [xr::ReferenceSpaceType::STAGE, xr::ReferenceSpaceType::LOCAL]
        .into_iter()
        .find_map(|ty| create_internal_space(d, handle, ty).map(|s| (s, ty)));
    let view = create_internal_space(d, handle, xr::ReferenceSpaceType::VIEW);
    if tracking.is_none() || view.is_none() || d.locate_space.is_none() {
        log::info!("could not create the layer's reference spaces; this session passes through");
    }
    let settings = instance.settings.clone();
    let source = FileSource::new(settings.pose_path.clone(), settings.max_age_ns);
    let ctx = Arc::new(SessionCtx {
        instance,
        tracking,
        view,
        comp: Mutex::new(Compensator::new(source, settings)),
        clock: Mutex::new(FallbackClock::default()),
    });
    registry_mut().sessions.insert(handle.into_raw(), ctx);
}

unsafe extern "system" fn destroy_session(session: xr::Session) -> xr::Result {
    let ctx = {
        let mut reg = registry_mut();
        reg.spaces.retain(|_, info| info.session != session.into_raw());
        reg.sessions.remove(&session.into_raw())
    };
    let Some(ctx) = ctx else {
        return match any_instance().and_then(|i| i.dispatch.destroy_session) {
            // SAFETY: forwarding the application's handle.
            Some(down) => unsafe { down(session) },
            None => xr::Result::ERROR_HANDLE_INVALID,
        };
    };
    guarded("xrDestroySession", || {
        lock(&ctx.comp).shutdown();
        if let Some(destroy) = ctx.instance.dispatch.destroy_space {
            for space in ctx.tracking.map(|t| t.0).into_iter().chain(ctx.view) {
                // SAFETY: a space the layer created on this session and has not destroyed.
                unsafe { destroy(space) };
            }
        }
    });
    let Some(down) = ctx.instance.dispatch.destroy_session else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's handle.
    unsafe { down(session) }
}

unsafe extern "system" fn create_reference_space(
    session: xr::Session,
    info: *const xr::ReferenceSpaceCreateInfo,
    space: *mut xr::Space,
) -> xr::Result {
    let Some(ctx) = session_ctx(session) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let Some(down) = ctx.instance.dispatch.create_reference_space else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's arguments.
    let r = unsafe { down(session, info, space) };
    if r == xr::Result::SUCCESS && !info.is_null() && !space.is_null() {
        // SAFETY: valid create info (the call succeeded) and the handle the runtime just wrote.
        let (ty, handle) = unsafe { ((*info).reference_space_type, *space) };
        let class = if ty == xr::ReferenceSpaceType::VIEW {
            SpaceClass::Attached
        } else {
            SpaceClass::World { cacheable: true }
        };
        registry_mut().spaces.insert(
            handle.into_raw(),
            SpaceInfo { session: session.into_raw(), class, head: ty == xr::ReferenceSpaceType::VIEW },
        );
    }
    r
}

unsafe extern "system" fn create_action_space(
    session: xr::Session,
    info: *const xr::ActionSpaceCreateInfo,
    space: *mut xr::Space,
) -> xr::Result {
    let Some(ctx) = session_ctx(session) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let Some(down) = ctx.instance.dispatch.create_action_space else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's arguments.
    let r = unsafe { down(session, info, space) };
    if r == xr::Result::SUCCESS && !space.is_null() {
        // SAFETY: the handle the runtime just wrote.
        let handle = unsafe { *space };
        let class = if ctx.instance.settings.compensate_controllers {
            SpaceClass::Attached
        } else {
            SpaceClass::World { cacheable: false }
        };
        registry_mut().spaces.insert(handle.into_raw(), SpaceInfo { session: session.into_raw(), class, head: false });
    }
    r
}

unsafe extern "system" fn destroy_space(space: xr::Space) -> xr::Result {
    let info = registry_mut().spaces.remove(&space.into_raw());
    let ctx = info.and_then(|i| registry().sessions.get(&i.session).cloned());
    if let Some(ctx) = &ctx {
        guarded("xrDestroySpace", || lock(&ctx.comp).forget_space(space.into_raw()));
    }
    let down = ctx
        .map(|c| c.instance.dispatch.destroy_space)
        .unwrap_or_else(|| any_instance().and_then(|i| i.dispatch.destroy_space));
    match down {
        // SAFETY: forwarding the application's handle.
        Some(down) => unsafe { down(space) },
        None => xr::Result::ERROR_HANDLE_INVALID,
    }
}

// ---------------------------------------------------------------------------------------------
// Time and space helpers
// ---------------------------------------------------------------------------------------------

impl SessionCtx {
    /// The `CLOCK_MONOTONIC` nanoseconds of `time`.
    fn to_monotonic(&self, time: xr::Time, now_ns: u64) -> u64 {
        if let Some(convert) = self.instance.dispatch.convert_time_to_timespec {
            let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            // SAFETY: valid instance (it outlives its sessions) and a writable timespec.
            if unsafe { convert(self.instance.handle, time, &mut ts) } == xr::Result::SUCCESS {
                return (ts.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(ts.tv_nsec as u64);
            }
        }
        match lock(&self.clock).anchor {
            Some((xr_anchor, mono_anchor)) => {
                mono_anchor.saturating_add_signed(time.as_nanos().saturating_sub(xr_anchor))
            }
            None => now_ns,
        }
    }

    /// Locates `space` in `base` straight from the runtime (uncompensated).
    fn locate_raw(&self, space: xr::Space, base: xr::Space, time: xr::Time) -> Option<Pose> {
        let locate = self.instance.dispatch.locate_space?;
        let mut loc = xr::SpaceLocation {
            ty: xr::StructureType::SPACE_LOCATION,
            next: ptr::null_mut(),
            location_flags: xr::SpaceLocationFlags::EMPTY,
            pose: identity_posef(),
        };
        // SAFETY: valid handles of this session and a typed, writable XrSpaceLocation.
        let r = unsafe { locate(space, base, time, &mut loc) };
        let valid = xr::SpaceLocationFlags::ORIENTATION_VALID | xr::SpaceLocationFlags::POSITION_VALID;
        (r == xr::Result::SUCCESS && loc.location_flags.contains(valid)).then(|| pose_from_xr(&loc.pose))
    }

    /// `T_from_space` for a room-fixed space, cached for reference spaces.
    fn offset_of(&self, space: xr::Space, class: SpaceClass, time: xr::Time) -> Option<Pose> {
        let (tracking, _) = self.tracking?;
        if space == tracking {
            return Some(Pose::IDENTITY);
        }
        let cacheable = matches!(class, SpaceClass::World { cacheable: true });
        if cacheable {
            if let Some(p) = lock(&self.comp).offset(space.into_raw(), time.as_nanos()) {
                return Some(p);
            }
        }
        let p = self.locate_raw(space, tracking, time)?;
        if cacheable {
            lock(&self.comp).store_offset(space.into_raw(), p, time.as_nanos());
        }
        Some(p)
    }

    /// The compensation for `time` expressed in the room-fixed space `space`: `X⁻¹·K·X`, or `None`
    /// while passing through or when `space` cannot be located.
    fn delta_in(&self, space: xr::Space, class: SpaceClass, time: xr::Time, for_submit: bool) -> Option<Motion> {
        self.delta_parts(space, class, time, for_submit).applied
    }

    /// The tracking-space compensation `K` for `time` and, when it is applied, its form in `space`.
    fn delta_parts(&self, space: xr::Space, class: SpaceClass, time: xr::Time, for_submit: bool) -> DeltaParts {
        let now = monotonic_now_ns();
        let time_ns = self.to_monotonic(time, now);
        let k = {
            let mut comp = lock(&self.comp);
            if for_submit {
                comp.delta_for_submit(time.as_nanos(), time_ns, now)
            } else {
                comp.delta(time.as_nanos(), time_ns, now)
            }
        };
        let applied =
            if k.is_identity() { None } else { self.offset_of(space, class, time).map(|x| k.conjugated_by(x)) };
        DeltaParts { k, applied }
    }

    /// Records what was applied to the head located for `time` (`None`: located uncompensated), so
    /// `xrEndFrame` reverts exactly that.
    fn record_head(&self, time: xr::Time, parts: Option<&DeltaParts>) {
        let k = match parts {
            Some(DeltaParts { k, applied: Some(_) }) => *k,
            _ => Motion::IDENTITY,
        };
        lock(&self.comp).record_applied(time.as_nanos(), k);
    }
}

/// A compensation for one time: `k` in the tracking space, and `applied` in the space it is used
/// in (`None` when nothing is applied: passing through, or that space could not be located).
#[derive(Debug, Clone, Copy)]
struct DeltaParts {
    k: Motion,
    applied: Option<Motion>,
}

/// A located pose with its optional velocity, as views into the output structures.
struct Located<'a> {
    flags: xr::SpaceLocationFlags,
    pose: &'a mut xr::Posef,
    velocity: Option<(xr::SpaceVelocityFlags, &'a mut xr::Vector3f, &'a mut xr::Vector3f)>,
}

impl Located<'_> {
    fn motion(&self) -> Motion {
        let (linear, angular) = match &self.velocity {
            Some((flags, lin, ang)) => (
                if flags.contains(xr::SpaceVelocityFlags::LINEAR_VALID) {
                    vec_from_xr(lin)
                } else {
                    Default::default()
                },
                if flags.contains(xr::SpaceVelocityFlags::ANGULAR_VALID) {
                    vec_from_xr(ang)
                } else {
                    Default::default()
                },
            ),
            None => Default::default(),
        };
        Motion { pose: pose_from_xr(self.pose), linear, angular }
    }

    fn store(&mut self, m: &Motion) {
        *self.pose = pose_to_xr(m.pose);
        if let Some((flags, lin, ang)) = &mut self.velocity {
            if flags.contains(xr::SpaceVelocityFlags::LINEAR_VALID) {
                **lin = vec_to_xr(m.linear);
            }
            if flags.contains(xr::SpaceVelocityFlags::ANGULAR_VALID) {
                **ang = vec_to_xr(m.angular);
            }
        }
    }
}

/// Adjusts one location of `space` relative to `base` at `time`. `base_delta` caches the
/// compensation in `base` across the elements of an `xrLocateSpaces` call.
fn compensate_location(
    ctx: &SessionCtx,
    space: xr::Space,
    base: xr::Space,
    time: xr::Time,
    loc: &mut Located<'_>,
    base_delta: &mut Option<DeltaParts>,
) {
    let head_in_world = is_head(space) && matches!(class_of(base), SpaceClass::World { .. });
    if !loc.flags.contains(xr::SpaceLocationFlags::ORIENTATION_VALID) {
        if head_in_world {
            ctx.record_head(time, None);
        }
        return;
    }
    match (class_of(space), class_of(base)) {
        // A rig-attached space (head, controller) in a room-fixed base: B_from_A' = (X_B⁻¹·K·X_B)·B_from_A.
        (SpaceClass::Attached, base_class @ SpaceClass::World { .. }) => {
            let parts = *base_delta.get_or_insert_with(|| ctx.delta_parts(base, base_class, time, false));
            if let Some(d) = parts.applied {
                loc.store(&d.then(&loc.motion()));
            }
            if head_in_world {
                ctx.record_head(time, Some(&parts));
            }
        }
        // A room-fixed space in a rig-attached base: B_from_A' = B_from_A·(X_A⁻¹·K·X_A)⁻¹.
        (space_class @ SpaceClass::World { .. }, SpaceClass::Attached) => {
            if let Some(d) = ctx.delta_in(space, space_class, time, false) {
                loc.store(&loc.motion().then(&d.inverse()));
            }
        }
        // Both move with the rig, or neither does: the relative pose is unchanged.
        _ => {}
    }
}

/// Finds a structure of type `ty` in an output `next` chain.
///
/// # Safety
/// `next` must be null or the head of a valid chain of OpenXR output structures.
unsafe fn find_in_chain(mut next: *mut std::ffi::c_void, ty: xr::StructureType) -> *mut std::ffi::c_void {
    while !next.is_null() {
        // SAFETY: every OpenXR output structure starts with XrBaseOutStructure (type, next).
        let base = unsafe { &*(next as *const xr::BaseOutStructure) };
        if base.ty == ty {
            return next;
        }
        next = base.next as *mut std::ffi::c_void;
    }
    ptr::null_mut()
}

// ---------------------------------------------------------------------------------------------
// Per-frame calls
// ---------------------------------------------------------------------------------------------

unsafe extern "system" fn wait_frame(
    session: xr::Session,
    info: *const xr::FrameWaitInfo,
    state: *mut xr::FrameState,
) -> xr::Result {
    let Some(ctx) = session_ctx(session) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let Some(down) = ctx.instance.dispatch.wait_frame else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's arguments.
    let r = unsafe { down(session, info, state) };
    if r == xr::Result::SUCCESS && !state.is_null() {
        // SAFETY: on success the runtime filled the application's XrFrameState.
        let display = unsafe { (*state).predicted_display_time };
        guarded("xrWaitFrame", || on_frame(&ctx, display));
    }
    r
}

fn on_frame(ctx: &SessionCtx, display: xr::Time) {
    let now = monotonic_now_ns();
    lock(&ctx.clock).anchor =
        Some((display.as_nanos(), now.saturating_add(ctx.instance.settings.fallback_lookahead_ns)));
    let display_ns = ctx.to_monotonic(display, now);
    if !lock(&ctx.comp).on_frame(now, display.as_nanos()) {
        return;
    }
    let (Some((tracking, _)), Some(view)) = (ctx.tracking, ctx.view) else {
        return;
    };
    if let Some(head) = ctx.locate_raw(view, tracking, display) {
        lock(&ctx.comp).calibrate(head, display_ns, now);
    }
}

unsafe extern "system" fn locate_views(
    session: xr::Session,
    info: *const xr::ViewLocateInfo,
    state: *mut xr::ViewState,
    capacity: u32,
    count: *mut u32,
    views: *mut xr::View,
) -> xr::Result {
    let Some(ctx) = session_ctx(session) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let Some(down) = ctx.instance.dispatch.locate_views else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's arguments.
    let r = unsafe { down(session, info, state, capacity, count, views) };
    if r != xr::Result::SUCCESS
        || info.is_null()
        || state.is_null()
        || count.is_null()
        || views.is_null()
        || capacity == 0
    {
        return r;
    }
    // SAFETY: the call succeeded, so `info` and `state` are valid and the runtime wrote `*count`
    // (≤ capacity) views into the application's array of `capacity` elements.
    let (info, flags, n) = unsafe { (&*info, (*state).view_state_flags, (*count).min(capacity) as usize) };
    // SAFETY: as above; the slice covers only the elements the runtime wrote.
    let views = unsafe { std::slice::from_raw_parts_mut(views, n) };
    guarded("xrLocateViews", || {
        let class = class_of(info.space);
        if class == SpaceClass::Attached {
            // Views relative to the head: nothing to compensate, and nothing a room-fixed
            // projection layer would have been rendered with.
            return;
        }
        if !flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) {
            // Located uncompensated: the submission for this frame must not be reverted either.
            ctx.record_head(info.display_time, None);
            return;
        }
        let parts = ctx.delta_parts(info.space, class, info.display_time, false);
        if let Some(d) = parts.applied {
            for v in views {
                v.pose = pose_to_xr(d.pose.then(pose_from_xr(&v.pose)));
            }
        }
        ctx.record_head(info.display_time, Some(&parts));
    });
    r
}

/// The session a space belongs to, from either handle.
fn session_of(a: xr::Space, b: xr::Space) -> Option<Arc<SessionCtx>> {
    let reg = registry();
    [a, b].iter().find_map(|s| reg.spaces.get(&s.into_raw()).and_then(|i| reg.sessions.get(&i.session).cloned()))
}

unsafe extern "system" fn locate_space(
    space: xr::Space,
    base: xr::Space,
    time: xr::Time,
    location: *mut xr::SpaceLocation,
) -> xr::Result {
    let ctx = session_of(space, base);
    let down = match &ctx {
        Some(c) => c.instance.dispatch.locate_space,
        None => any_instance().and_then(|i| i.dispatch.locate_space),
    };
    let Some(down) = down else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    // SAFETY: forwarding the application's arguments.
    let r = unsafe { down(space, base, time, location) };
    let Some(ctx) = ctx else { return r };
    if r != xr::Result::SUCCESS || location.is_null() {
        return r;
    }
    // SAFETY: the call succeeded on the application's valid, writable XrSpaceLocation.
    let loc = unsafe { &mut *location };
    // SAFETY: the application's `next` chain is a valid chain of output structures.
    let vel = unsafe { find_in_chain(loc.next, xr::StructureType::SPACE_VELOCITY) } as *mut xr::SpaceVelocity;
    // SAFETY: a chained XR_TYPE_SPACE_VELOCITY structure is an XrSpaceVelocity the runtime filled.
    let velocity = unsafe { vel.as_mut() }.map(|v| (v.velocity_flags, &mut v.linear_velocity, &mut v.angular_velocity));
    let mut located = Located { flags: loc.location_flags, pose: &mut loc.pose, velocity };
    guarded("xrLocateSpace", || compensate_location(&ctx, space, base, time, &mut located, &mut None));
    r
}

unsafe extern "system" fn locate_spaces(
    session: xr::Session,
    info: *const xr::SpacesLocateInfo,
    locations: *mut xr::SpaceLocations,
) -> xr::Result {
    let Some(ctx) = session_ctx(session) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let Some(down) = ctx.instance.dispatch.locate_spaces else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's arguments.
    let r = unsafe { down(session, info, locations) };
    // SAFETY: as documented on `compensate_spaces`.
    unsafe { compensate_spaces(&ctx, r, info, locations) };
    r
}

unsafe extern "system" fn locate_spaces_khr(
    session: xr::Session,
    info: *const xr::SpacesLocateInfo,
    locations: *mut xr::SpaceLocations,
) -> xr::Result {
    let Some(ctx) = session_ctx(session) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let Some(down) = ctx.instance.dispatch.locate_spaces_khr else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's arguments.
    let r = unsafe { down(session, info, locations) };
    // SAFETY: as documented on `compensate_spaces`.
    unsafe { compensate_spaces(&ctx, r, info, locations) };
    r
}

/// Post-processes an `xrLocateSpaces` result.
///
/// # Safety
/// `info` and `locations` are the pointers the application passed to the call that returned `r`.
unsafe fn compensate_spaces(
    ctx: &SessionCtx,
    r: xr::Result,
    info: *const xr::SpacesLocateInfo,
    locations: *mut xr::SpaceLocations,
) {
    if r != xr::Result::SUCCESS || info.is_null() || locations.is_null() {
        return;
    }
    // SAFETY: the call succeeded, so both structures are valid.
    let (info, out) = unsafe { (&*info, &mut *locations) };
    let n = info.space_count.min(out.location_count) as usize;
    if n == 0 || out.locations.is_null() || info.spaces.is_null() {
        return;
    }
    // SAFETY: `spaces` has `space_count` handles and `locations` `location_count` elements.
    let spaces = unsafe { std::slice::from_raw_parts(info.spaces, n) };
    // SAFETY: as above; the runtime filled the first `n` locations.
    let locs = unsafe { std::slice::from_raw_parts_mut(out.locations, n) };
    // SAFETY: the application's `next` chain is a valid chain of output structures.
    let vel = unsafe { find_in_chain(out.next, xr::StructureType::SPACE_VELOCITIES) } as *mut xr::SpaceVelocities;
    // SAFETY: a chained XR_TYPE_SPACE_VELOCITIES structure is an XrSpaceVelocities, filled by the runtime.
    let vels: &mut [xr::SpaceVelocityData] = match unsafe { vel.as_mut() } {
        Some(v) if !v.velocities.is_null() && v.velocity_count as usize >= n => {
            // SAFETY: its array has `velocity_count` ≥ n elements.
            unsafe { std::slice::from_raw_parts_mut(v.velocities, n) }
        }
        _ => &mut [],
    };
    guarded("xrLocateSpaces", || {
        let mut base_delta = None;
        let mut vels = vels.iter_mut();
        for (space, loc) in spaces.iter().zip(locs.iter_mut()) {
            let velocity = vels.next().map(|v| (v.velocity_flags, &mut v.linear_velocity, &mut v.angular_velocity));
            let mut located = Located { flags: loc.location_flags, pose: &mut loc.pose, velocity };
            compensate_location(ctx, *space, info.base_space, info.time, &mut located, &mut base_delta);
        }
    });
}

/// Copies of the application's composition layers with reverted poses, kept alive across the
/// down call.
///
/// Each copy is boxed because `layers` points at it: a box keeps its address while the vectors grow.
#[derive(Default)]
#[allow(clippy::vec_box)]
struct SubstituteFrame {
    layers: Vec<*const xr::CompositionLayerBaseHeader>,
    projections: Vec<(Box<xr::CompositionLayerProjection>, Vec<xr::CompositionLayerProjectionView>)>,
    quads: Vec<Box<xr::CompositionLayerQuad>>,
    cylinders: Vec<Box<xr::CompositionLayerCylinderKHR>>,
    equirects: Vec<Box<xr::CompositionLayerEquirectKHR>>,
    equirects2: Vec<Box<xr::CompositionLayerEquirect2KHR>>,
    cubes: Vec<Box<xr::CompositionLayerCubeKHR>>,
}

fn header<T>(layer: &T) -> *const xr::CompositionLayerBaseHeader {
    (layer as *const T).cast()
}

/// Undoes the compensation on a submitted pose: `pose_submitted = (X⁻¹·K·X)⁻¹·pose`.
fn revert(inv: &Pose, p: &xr::Posef) -> xr::Posef {
    pose_to_xr(inv.then(pose_from_xr(p)))
}

/// Builds the frame the compositor sees: every layer in a room-fixed space gets the inverse of the
/// compensation it was rendered with, so the compositor's reprojection works on the real tracked
/// head pose. Returns `None` when nothing needs to change.
///
/// # Safety
/// `info` must be a valid `XrFrameEndInfo` whose layer pointers are valid composition layers of the
/// type in their header.
unsafe fn substitute_frame(ctx: &SessionCtx, info: &xr::FrameEndInfo) -> Option<SubstituteFrame> {
    // SAFETY: the application's array of `layer_count` layer pointers.
    let layers = unsafe { c_array(info.layers, info.layer_count) };
    let mut sub = SubstituteFrame::default();
    let mut changed = false;
    for &layer in layers {
        sub.layers.push(layer);
        if layer.is_null() {
            continue;
        }
        // SAFETY: every composition layer starts with XrCompositionLayerBaseHeader.
        let base = unsafe { &*layer };
        let class = class_of(base.space);
        if class == SpaceClass::Attached {
            continue;
        }
        let Some(d) = ctx.delta_in(base.space, class, info.display_time, true) else {
            continue;
        };
        let inv = d.pose.inverse();
        let slot = sub.layers.last_mut().expect("just pushed");
        match base.ty {
            xr::StructureType::COMPOSITION_LAYER_PROJECTION => {
                // SAFETY: the header's type says this is an XrCompositionLayerProjection.
                let mut p = Box::new(unsafe { *(layer as *const xr::CompositionLayerProjection) });
                // SAFETY: a projection layer has `view_count` valid views.
                let mut views = unsafe { c_array(p.views, p.view_count) }.to_vec();
                for v in &mut views {
                    v.pose = revert(&inv, &v.pose);
                }
                p.views = views.as_ptr();
                *slot = header(&*p);
                sub.projections.push((p, views));
            }
            xr::StructureType::COMPOSITION_LAYER_QUAD => {
                // SAFETY: the header's type says this is an XrCompositionLayerQuad.
                let mut q = Box::new(unsafe { *(layer as *const xr::CompositionLayerQuad) });
                q.pose = revert(&inv, &q.pose);
                *slot = header(&*q);
                sub.quads.push(q);
            }
            xr::StructureType::COMPOSITION_LAYER_CYLINDER_KHR => {
                // SAFETY: the header's type says this is an XrCompositionLayerCylinderKHR.
                let mut c = Box::new(unsafe { *(layer as *const xr::CompositionLayerCylinderKHR) });
                c.pose = revert(&inv, &c.pose);
                *slot = header(&*c);
                sub.cylinders.push(c);
            }
            xr::StructureType::COMPOSITION_LAYER_EQUIRECT_KHR => {
                // SAFETY: the header's type says this is an XrCompositionLayerEquirectKHR.
                let mut e = Box::new(unsafe { *(layer as *const xr::CompositionLayerEquirectKHR) });
                e.pose = revert(&inv, &e.pose);
                *slot = header(&*e);
                sub.equirects.push(e);
            }
            xr::StructureType::COMPOSITION_LAYER_EQUIRECT2_KHR => {
                // SAFETY: the header's type says this is an XrCompositionLayerEquirect2KHR.
                let mut e = Box::new(unsafe { *(layer as *const xr::CompositionLayerEquirect2KHR) });
                e.pose = revert(&inv, &e.pose);
                *slot = header(&*e);
                sub.equirects2.push(e);
            }
            xr::StructureType::COMPOSITION_LAYER_CUBE_KHR => {
                // SAFETY: the header's type says this is an XrCompositionLayerCubeKHR.
                let mut c = Box::new(unsafe { *(layer as *const xr::CompositionLayerCubeKHR) });
                c.orientation = quat_to_xr(inv.rotation * quat_from_xr(&c.orientation));
                *slot = header(&*c);
                sub.cubes.push(c);
            }
            // Layer types without a pose in their space (or unknown ones) pass unchanged.
            _ => continue,
        }
        changed = true;
    }
    changed.then_some(sub)
}

unsafe extern "system" fn end_frame(session: xr::Session, info: *const xr::FrameEndInfo) -> xr::Result {
    let Some(ctx) = session_ctx(session) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let Some(down) = ctx.instance.dispatch.end_frame else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    if info.is_null() {
        // SAFETY: forwarding the application's arguments (the runtime reports the error).
        return unsafe { down(session, info) };
    }
    // SAFETY: non-null application XrFrameEndInfo, valid for the call.
    let app = unsafe { &*info };
    // SAFETY: `app` is the application's valid frame description.
    let sub = catch_unwind(AssertUnwindSafe(|| unsafe { substitute_frame(&ctx, app) })).unwrap_or_else(|_| {
        log::info!("internal error in xrEndFrame; frame submitted unchanged");
        None
    });
    match sub {
        Some(sub) => {
            let mut frame = *app;
            frame.layers = sub.layers.as_ptr();
            // SAFETY: the application's frame with the layer array replaced by copies owned by
            // `sub`, which lives until after the call.
            unsafe { down(session, &frame) }
        }
        // SAFETY: forwarding the application's arguments.
        None => unsafe { down(session, info) },
    }
}

unsafe extern "system" fn poll_event(instance: xr::Instance, buffer: *mut xr::EventDataBuffer) -> xr::Result {
    let Some(ctx) = instance_ctx(instance) else {
        return xr::Result::ERROR_HANDLE_INVALID;
    };
    let Some(down) = ctx.dispatch.poll_event else {
        return xr::Result::ERROR_FUNCTION_UNSUPPORTED;
    };
    // SAFETY: forwarding the application's arguments.
    let r = unsafe { down(instance, buffer) };
    if r != xr::Result::SUCCESS || buffer.is_null() {
        return r;
    }
    // SAFETY: on success the runtime wrote an event into the application's buffer, whose `ty`
    // identifies the event structure stored in it (the buffer is large enough for any event).
    let ty = unsafe { (*buffer).ty };
    match ty {
        xr::StructureType::EVENT_DATA_REFERENCE_SPACE_CHANGE_PENDING => {
            // SAFETY: the type says the buffer holds an XrEventDataReferenceSpaceChangePending.
            let ev = unsafe { &*(buffer as *const xr::EventDataReferenceSpaceChangePending) };
            if let Some(s) = session_ctx(ev.session) {
                guarded("xrPollEvent", || {
                    let tracking_changed = s.tracking.is_some_and(|(_, ty)| ty == ev.reference_space_type);
                    lock(&s.comp).on_reference_space_change(tracking_changed, ev.change_time.as_nanos());
                });
            }
        }
        xr::StructureType::EVENT_DATA_SESSION_STATE_CHANGED => {
            // SAFETY: the type says the buffer holds an XrEventDataSessionStateChanged.
            let ev = unsafe { &*(buffer as *const xr::EventDataSessionStateChanged) };
            if let Some(s) = session_ctx(ev.session) {
                guarded("xrPollEvent", || lock(&s.comp).on_focus(ev.state == xr::SessionState::FOCUSED));
            }
        }
        _ => {}
    }
    r
}

#[cfg(test)]
pub(crate) mod test_access {
    //! Hooks for the in-process tests.
    use super::*;

    pub fn negotiate_entry() -> pfn::NegotiateLoaderApiLayerInterface {
        xrNegotiateLoaderApiLayerInterface
    }

    pub fn status(session: xr::Session) -> Option<crate::compensation::Status> {
        session_ctx(session).map(|s| lock(&s.comp).status())
    }

    pub fn tracking_space(session: xr::Session) -> Option<xr::Space> {
        session_ctx(session).and_then(|s| s.tracking.map(|t| t.0))
    }
}
