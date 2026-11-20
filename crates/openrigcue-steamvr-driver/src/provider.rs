//! The C++-ABI face of the driver: `HmdDriverFactory`, the `IServerTrackedDeviceProvider` object
//! vrserver drives, and the two detours that sit in vrserver's vtables.
//!
//! Flow (method of OpenVR-MotionCompensation / OpenVR-InputEmulator and OpenVR-SpaceCalibrator,
//! reimplemented): in `Init` the driver
//! 1. pins its own library (`RTLD_NODELETE`), since vtables will point into it;
//! 2. hooks `IVRDriverContext::GetGenericInterface` (slot 0) of the context it was given; the
//!    detour hooks every `IVRServerDriverHost_004/005/006` object any driver asks for afterwards;
//! 3. asks for each of those host versions itself, so the host classes are hooked even if the
//!    device drivers obtained their host pointers before this driver loaded;
//! 4. starts its housekeeping thread.
//!
//! The `TrackedDevicePoseUpdated` detour (slot 1) copies the pose, lets the compensator change the
//! copy, and forwards the copy (or the untouched original) to the function it replaced.
//!
//! Hard rules for the detours: they never unwind across the ABI (`catch_unwind`), never block,
//! never allocate on the normal path, and always forward the call.

use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_rigpose::monotonic_now_ns;

use crate::compensate::{Compensator, DeviceKind, Outcome};
use crate::config::Settings;
use crate::hook::{HookTable, Installed};
use crate::openvr::{
    self, device_class, init_error, slot, version, DriverLogFn, DriverPose, GetGenericInterfaceFn, PropertyRead,
    ReadPropertyBatchFn, TrackedDevicePoseUpdatedFn, TrackedDeviceToPropertyContainerFn,
};
use crate::shm::{HookState, Housekeeper, ShmRigSource};

// ---------------------------------------------------------------------------------------------
// Pose processing (safe side)

/// What the pose detour hands each update to. The driver installs [`DriverProcessor`]; tests can
/// install their own.
pub trait PoseProcessor: Sync {
    /// Changes `pose` in place; returns true if it changed anything.
    fn process(&self, device: u32, kind: DeviceKind, pose: &mut DriverPose, now_ns: u64) -> bool;
}

/// The production processor: the compensator over the shared rig-pose file.
pub struct DriverProcessor {
    pub compensator: Compensator<&'static ShmRigSource>,
}

impl PoseProcessor for DriverProcessor {
    fn process(&self, _device: u32, kind: DeviceKind, pose: &mut DriverPose, now_ns: u64) -> bool {
        matches!(self.compensator.process(kind, pose, now_ns), Outcome::Compensated | Outcome::Fading)
    }
}

static PROCESSOR: AtomicPtr<&'static dyn PoseProcessor> = AtomicPtr::new(std::ptr::null_mut());

/// Routes pose updates to `processor` from now on. The previous processor is leaked on purpose: a
/// pose thread may still be using it.
pub fn install_processor(processor: &'static dyn PoseProcessor) {
    let boxed: &'static mut &'static dyn PoseProcessor = Box::leak(Box::new(processor));
    PROCESSOR.store(boxed, Ordering::Release);
}

fn processor() -> Option<&'static dyn PoseProcessor> {
    let p = PROCESSOR.load(Ordering::Acquire);
    // SAFETY: non-null values come only from `install_processor`, which leaks a
    // `&'static dyn PoseProcessor`; they are never freed.
    (!p.is_null()).then(|| unsafe { *p })
}

/// Whether the detours modify poses (between `Init` and `Cleanup`).
static ACTIVE: AtomicBool = AtomicBool::new(false);
static POSE_CALLS: AtomicU64 = AtomicU64::new(0);
static LAST_HMD_POSE_NS: AtomicU64 = AtomicU64::new(0);

static CONTEXT_HOOKS: HookTable = HookTable::new(slot::CONTEXT_GET_GENERIC_INTERFACE);
static HOST_HOOKS: HookTable = HookTable::new(slot::HOST_TRACKED_DEVICE_POSE_UPDATED);

pub fn hook_state() -> HookState {
    HookState {
        host_hooks: HOST_HOOKS.hooked_vtables(),
        context_hooks: CONTEXT_HOOKS.hooked_vtables(),
        last_hmd_pose_ns: LAST_HMD_POSE_NS.load(Ordering::Relaxed),
        pose_calls: POSE_CALLS.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------------------------
// Device classes, via IVRProperties

const MAX_DEVICES: usize = 64;
const CLASS_UNKNOWN: i32 = -1;
/// Give up asking for a device's class after this many failed queries (treat it as "other").
const CLASS_QUERY_ATTEMPTS: u32 = 200;

static PROPERTIES: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CLASSES: [AtomicI32; MAX_DEVICES] = [const { AtomicI32::new(CLASS_UNKNOWN) }; MAX_DEVICES];
static CLASS_QUERIES: [AtomicU32; MAX_DEVICES] = [const { AtomicU32::new(0) }; MAX_DEVICES];

fn kind_of_class(class: i32) -> DeviceKind {
    match class {
        device_class::HMD => DeviceKind::Hmd,
        device_class::CONTROLLER => DeviceKind::Controller,
        device_class::GENERIC_TRACKER => DeviceKind::Tracker,
        _ => DeviceKind::Other,
    }
}

fn device_kind(device: u32) -> DeviceKind {
    if device == openvr::TRACKED_DEVICE_INDEX_HMD {
        return DeviceKind::Hmd;
    }
    let Some(i) = usize::try_from(device).ok().filter(|i| *i < MAX_DEVICES) else {
        return DeviceKind::Other;
    };
    let cached = CLASSES[i].load(Ordering::Relaxed);
    if cached != CLASS_UNKNOWN {
        return kind_of_class(cached);
    }
    let properties = PROPERTIES.load(Ordering::Acquire);
    if properties.is_null() {
        return DeviceKind::Other;
    }
    // SAFETY: `properties` is the IVRProperties_001 object vrserver returned from
    // GetGenericInterface; it lives as long as vrserver.
    match unsafe { query_device_class(properties, device) } {
        Some(class) => {
            CLASSES[i].store(class, Ordering::Relaxed);
            kind_of_class(class)
        }
        None => {
            if CLASS_QUERIES[i].fetch_add(1, Ordering::Relaxed) >= CLASS_QUERY_ATTEMPTS {
                CLASSES[i].store(device_class::INVALID, Ordering::Relaxed);
            }
            DeviceKind::Other
        }
    }
}

/// `Prop_DeviceClass_Int32` of `device`, through `IVRProperties_001`.
///
/// # Safety
/// `properties` must be a live `vr::IVRProperties` (version 001) object.
unsafe fn query_device_class(properties: *mut c_void, device: u32) -> Option<i32> {
    // SAFETY: slots 0 and 3 of IVRProperties_001 are ReadPropertyBatch and
    // TrackedDeviceToPropertyContainer; the buffer outlives the call.
    unsafe {
        let to_container: TrackedDeviceToPropertyContainerFn = std::mem::transmute::<usize, _>(openvr::vtable_entry(
            properties,
            slot::PROPERTIES_TRACKED_DEVICE_TO_PROPERTY_CONTAINER,
        ));
        let container = to_container(properties, device);
        if container == 0 {
            return None;
        }
        let mut value: i32 = 0;
        let mut read = PropertyRead {
            prop: openvr::PROP_DEVICE_CLASS_INT32,
            pv_buffer: (&mut value as *mut i32).cast(),
            un_buffer_size: std::mem::size_of::<i32>() as u32,
            un_tag: 0,
            un_required_buffer_size: 0,
            e_error: 0,
        };
        let read_batch: ReadPropertyBatchFn =
            std::mem::transmute::<usize, _>(openvr::vtable_entry(properties, slot::PROPERTIES_READ_PROPERTY_BATCH));
        read_batch(properties, container, &mut read, 1);
        (read.e_error == openvr::TRACKED_PROP_SUCCESS && read.un_tag == openvr::INT32_PROPERTY_TAG).then_some(value)
    }
}

// ---------------------------------------------------------------------------------------------
// Logging: queued from any thread, written to vrserver's log from RunFrame (vrserver's thread).

static DRIVER_LOG: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static LOG_QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn log(message: impl Into<String>) {
    let mut queue = LOG_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    if queue.len() < 256 {
        queue.push(message.into());
    }
}

fn flush_log() {
    let Ok(mut queue) = LOG_QUEUE.try_lock() else {
        return;
    };
    if queue.is_empty() {
        return;
    }
    let lines = std::mem::take(&mut *queue);
    drop(queue);
    let logger = DRIVER_LOG.load(Ordering::Acquire);
    for line in lines {
        let text = format!("openrigcue: {}\n", line.replace('\0', " "));
        if logger.is_null() {
            eprint!("{text}");
            continue;
        }
        let Ok(c) = CString::new(text) else { continue };
        // SAFETY: `logger` is the IVRDriverLog_001 object from GetGenericInterface; slot 0 is
        // Log(const char*), and `c` outlives the call.
        unsafe {
            let f: DriverLogFn = std::mem::transmute::<usize, _>(openvr::vtable_entry(logger, slot::DRIVER_LOG_LOG));
            f(logger, c.as_ptr());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The detours

thread_local! {
    /// How many pose detours are active on this thread. vrserver's older host classes are thin
    /// wrappers: a pose update through the 005 host calls slot 1 of the
    /// host it wraps through that object's vtable, and the 004 wrapper forwards into the 005 one.
    /// With all three vtables hooked, a
    /// pose from a 004/005 driver re-enters the detour on the same thread; only the outermost
    /// call compensates, so every pose update is compensated exactly once.
    static POSE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Marks a pose detour as active on this thread for as long as it lives.
struct DepthGuard {
    outermost: bool,
}

impl DepthGuard {
    fn enter() -> Self {
        let outermost = POSE_DEPTH.with(|d| {
            let depth = d.get();
            d.set(depth.saturating_add(1));
            depth == 0
        });
        DepthGuard { outermost }
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        POSE_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Replaces `IVRServerDriverHost::TrackedDevicePoseUpdated` (slot 1) in hooked host vtables.
unsafe extern "C" fn pose_updated_detour(this: *mut c_void, device: u32, pose: *const DriverPose, size: u32) {
    // SAFETY: vrserver calls this through the vtable of a live host object `this`.
    let Some(original) = (unsafe { HOST_HOOKS.original_for(this) }) else {
        // Unreachable: entries are published before a slot points here. Dropping one pose update
        // is the only safe option.
        return;
    };
    // SAFETY: the entry holds what was in this very slot, so it has this exact signature.
    let original = unsafe { std::mem::transmute::<usize, TrackedDevicePoseUpdatedFn>(original) };
    let depth = DepthGuard::enter();
    if !depth.outermost {
        // A wrapper host forwarding into a hooked inner host: the outer detour already
        // compensated this pose (or decided not to). Forward untouched.
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { original(this, device, pose, size) };
        return;
    }
    POSE_CALLS.fetch_add(1, Ordering::Relaxed);

    // Only a pose struct of exactly the layout we know is copied; a different size means a
    // different struct version, which is forwarded untouched.
    if ACTIVE.load(Ordering::Acquire) && !pose.is_null() && size as usize == std::mem::size_of::<DriverPose>() {
        let modified = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `pose` points at a DriverPose_t of `size` bytes for the duration of the call.
            let mut copy = unsafe { *pose };
            let kind = device_kind(device);
            let now_ns = monotonic_now_ns();
            if kind == DeviceKind::Hmd {
                LAST_HMD_POSE_NS.store(now_ns, Ordering::Relaxed);
            }
            let changed = processor().is_some_and(|p| p.process(device, kind, &mut copy, now_ns));
            changed.then_some(copy)
        }));
        if let Ok(Some(copy)) = modified {
            // SAFETY: forwarding to the replaced function with a valid pose of the same size.
            unsafe { original(this, device, &copy, size) };
            return;
        }
    }
    // SAFETY: forwarding the caller's own arguments unchanged.
    unsafe { original(this, device, pose, size) }
}

/// Replaces `IVRDriverContext::GetGenericInterface` (slot 0) in hooked context vtables.
unsafe extern "C" fn get_generic_interface_detour(
    this: *mut c_void,
    version: *const c_char,
    error: *mut i32,
) -> *mut c_void {
    // SAFETY: vrserver calls this through the vtable of a live context object `this`.
    let Some(original) = (unsafe { CONTEXT_HOOKS.original_for(this) }) else {
        if !error.is_null() {
            // SAFETY: the caller passed a writable EVRInitError* (or null, checked above).
            unsafe { *error = init_error::INIT_INTERNAL };
        }
        return std::ptr::null_mut();
    };
    // SAFETY: the entry holds what was in this very slot, so it has this exact signature.
    let original = unsafe { std::mem::transmute::<usize, GetGenericInterfaceFn>(original) };
    // SAFETY: forwarding the caller's arguments unchanged.
    let interface = unsafe { original(this, version, error) };
    if !interface.is_null() && !version.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `version` is the caller's NUL-terminated interface name, `interface` the
            // object vrserver just returned for it.
            unsafe { hook_if_pose_host(version, interface) }
        }));
    }
    interface
}

fn pose_detour_address() -> usize {
    pose_updated_detour as *const () as usize
}

fn context_detour_address() -> usize {
    get_generic_interface_detour as *const () as usize
}

/// Hooks `interface` if `version` names a host interface whose slot 1 is TrackedDevicePoseUpdated.
///
/// # Safety
/// `version` must be a NUL-terminated string and `interface` the live object returned for it.
unsafe fn hook_if_pose_host(version: *const c_char, interface: *mut c_void) {
    // SAFETY: caller contract.
    let name = unsafe { CStr::from_ptr(version) }.to_bytes_with_nul();
    if !version::POSE_HOSTS.contains(&name) {
        return;
    }
    let shown = String::from_utf8_lossy(&name[..name.len() - 1]).into_owned();
    // SAFETY: `interface` is a live IVRServerDriverHost of a version whose slot 1 has the detour's
    // signature; the detour forwards through HOST_HOOKS; the library is pinned.
    match unsafe { HOST_HOOKS.install(interface, pose_detour_address()) } {
        Ok(Installed::Hooked { vtable, .. }) => {
            log(format!("hooked {shown}::TrackedDevicePoseUpdated (vtable {vtable:#x})"))
        }
        Ok(Installed::AlreadyHooked { .. }) => {}
        Err(e) => log(format!("cannot hook {shown}: {e}; poses from it are not compensated")),
    }
}

// ---------------------------------------------------------------------------------------------
// The IServerTrackedDeviceProvider_004 object

#[repr(C)]
struct ProviderFns {
    init: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32,
    cleanup: unsafe extern "C" fn(*mut c_void),
    get_interface_versions: unsafe extern "C" fn(*mut c_void) -> *const *const c_char,
    run_frame: unsafe extern "C" fn(*mut c_void),
    should_block_standby_mode: unsafe extern "C" fn(*mut c_void) -> bool,
    enter_standby: unsafe extern "C" fn(*mut c_void),
    leave_standby: unsafe extern "C" fn(*mut c_void),
}

/// An Itanium vtable: offset-to-top and RTTI words, then the address point with the functions.
#[repr(C)]
struct ProviderVtable {
    offset_to_top: isize,
    type_info: *const c_void,
    functions: ProviderFns,
}

#[repr(C)]
struct ProviderObject {
    vptr: *const ProviderFns,
}

#[repr(C)]
struct VersionList([*const c_char; openvr::INTERFACE_VERSIONS.len() + 1]);

// SAFETY: all three hold only pointers to immutable statics.
unsafe impl Sync for ProviderVtable {}
// SAFETY: see above.
unsafe impl Sync for ProviderObject {}
// SAFETY: see above.
unsafe impl Sync for VersionList {}

static PROVIDER_VTABLE: ProviderVtable = ProviderVtable {
    offset_to_top: 0,
    type_info: std::ptr::null(),
    functions: ProviderFns {
        init: provider_init,
        cleanup: provider_cleanup,
        get_interface_versions: provider_get_interface_versions,
        run_frame: provider_run_frame,
        should_block_standby_mode: provider_should_block_standby_mode,
        enter_standby: provider_noop,
        leave_standby: provider_noop,
    },
};

static PROVIDER: ProviderObject = ProviderObject { vptr: &PROVIDER_VTABLE.functions };

static VERSIONS: VersionList = {
    let mut list = [std::ptr::null(); openvr::INTERFACE_VERSIONS.len() + 1];
    let mut i = 0;
    while i < openvr::INTERFACE_VERSIONS.len() {
        list[i] = openvr::INTERFACE_VERSIONS[i].as_ptr().cast();
        i += 1;
    }
    VersionList(list)
};

/// The SteamVR driver entry point.
///
/// # Safety
/// Called by vrserver with a NUL-terminated interface name and a writable (or null) return code.
#[no_mangle]
#[allow(non_snake_case)]
pub unsafe extern "C" fn HmdDriverFactory(interface_name: *const c_char, return_code: *mut i32) -> *mut c_void {
    let wanted = !interface_name.is_null()
        // SAFETY: vrserver passes a NUL-terminated string (checked non-null).
        && unsafe { CStr::from_ptr(interface_name) }.to_bytes_with_nul() == version::SERVER_TRACKED_DEVICE_PROVIDER;
    if wanted {
        return (&PROVIDER as *const ProviderObject).cast_mut().cast();
    }
    if !return_code.is_null() {
        // SAFETY: non-null, provided by the caller for this purpose.
        unsafe { *return_code = init_error::INIT_INTERFACE_NOT_FOUND };
    }
    std::ptr::null_mut()
}

struct Housekeeping {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

static RUNTIME: OnceLock<&'static DriverProcessor> = OnceLock::new();
static HOUSEKEEPING: Mutex<Option<Housekeeping>> = Mutex::new(None);

unsafe extern "C" fn provider_init(_this: *mut c_void, context: *mut c_void) -> i32 {
    // SAFETY: vrserver passes its live IVRDriverContext.
    catch_unwind(AssertUnwindSafe(|| unsafe { init(context) })).unwrap_or(init_error::INIT_INTERNAL)
}

/// # Safety
/// `context` must be vrserver's live `IVRDriverContext` (or null).
unsafe fn init(context: *mut c_void) -> i32 {
    if context.is_null() {
        return init_error::INIT_INTERNAL;
    }
    pin_library();

    // SAFETY: slot 0 of IVRDriverContext is GetGenericInterface. If an earlier Init hooked it,
    // this is our detour, which forwards.
    let get_interface = |name: &'static [u8]| -> *mut c_void {
        let mut err = 0;
        // SAFETY: as above; `name` is NUL-terminated and `err` writable.
        unsafe {
            let f: GetGenericInterfaceFn =
                std::mem::transmute::<usize, _>(openvr::vtable_entry(context, slot::CONTEXT_GET_GENERIC_INTERFACE));
            f(context, name.as_ptr().cast(), &mut err)
        }
    };
    DRIVER_LOG.store(get_interface(version::DRIVER_LOG), Ordering::Release);
    PROPERTIES.store(get_interface(version::PROPERTIES), Ordering::Release);
    log(format!("openrigcue-steamvr-driver {} starting", env!("CARGO_PKG_VERSION")));

    let runtime = *RUNTIME.get_or_init(|| {
        let settings_path = Settings::default_path();
        let settings = settings_path.as_deref().map_or(Ok(Settings::default()), Settings::load).unwrap_or_else(|e| {
            log(format!("settings: {e}; using defaults"));
            Settings::default()
        });
        let source: &'static ShmRigSource = Box::leak(Box::new(ShmRigSource::new(openrigcue_rigpose::default_path())));
        Box::leak(Box::new(DriverProcessor { compensator: Compensator::new(source, settings) }))
    });
    install_processor(runtime);

    // SAFETY: `context` is a live IVRDriverContext; the detour has slot 0's signature, forwards
    // through CONTEXT_HOOKS, and the library is pinned.
    match unsafe { CONTEXT_HOOKS.install(context, context_detour_address()) } {
        Ok(Installed::Hooked { .. }) => log("hooked IVRDriverContext::GetGenericInterface"),
        Ok(Installed::AlreadyHooked { .. }) => {}
        Err(e) => log(format!("cannot hook IVRDriverContext: {e}; relying on direct host hooks")),
    }
    for host in version::POSE_HOSTS {
        let interface = get_interface(host);
        if !interface.is_null() {
            // SAFETY: `interface` was just returned for `host`, a NUL-terminated name. Idempotent if
            // the context detour already hooked it.
            unsafe { hook_if_pose_host(host.as_ptr().cast(), interface) };
        }
    }
    let hooks = hook_state();
    if hooks.host_hooks == 0 {
        log("no IVRServerDriverHost could be hooked: motion compensation is inactive");
    }
    ACTIVE.store(true, Ordering::Release);
    start_housekeeping(runtime, Settings::default_path());
    init_error::NONE
}

fn start_housekeeping(runtime: &'static DriverProcessor, settings_path: Option<std::path::PathBuf>) {
    let mut slot = HOUSEKEEPING.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_some() {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let spawned = std::thread::Builder::new().name("openrigcue-housekeeping".into()).spawn(move || {
        let mut keeper = Housekeeper::new(settings_path);
        while !thread_stop.load(Ordering::Acquire) {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                keeper.tick(&runtime.compensator, hook_state(), monotonic_now_ns());
                for m in keeper.messages.drain(..) {
                    log(m);
                }
            }));
            std::thread::sleep(Duration::from_millis(100));
        }
        keeper.shutdown(runtime.compensator.source());
    });
    match spawned {
        Ok(thread) => *slot = Some(Housekeeping { stop, thread }),
        Err(e) => log(format!("cannot start the housekeeping thread: {e}")),
    }
}

unsafe extern "C" fn provider_cleanup(_this: *mut c_void) {
    let _ = catch_unwind(|| {
        ACTIVE.store(false, Ordering::Release);
        if let Some(h) = HOUSEKEEPING.lock().unwrap_or_else(|p| p.into_inner()).take() {
            h.stop.store(true, Ordering::Release);
            let _ = h.thread.join();
        }
        // SAFETY: the hooked vtables belong to vrserver, which outlives its drivers.
        let host = unsafe { HOST_HOOKS.uninstall_all(pose_detour_address()) };
        // SAFETY: as above.
        let context = unsafe { CONTEXT_HOOKS.uninstall_all(context_detour_address()) };
        log(format!(
            "stopped: restored {} host and {} context slots; {} left chained under other tools' hooks",
            host.restored,
            context.restored,
            host.left_chained + context.left_chained
        ));
        flush_log();
    });
}

unsafe extern "C" fn provider_get_interface_versions(_this: *mut c_void) -> *const *const c_char {
    VERSIONS.0.as_ptr()
}

unsafe extern "C" fn provider_run_frame(_this: *mut c_void) {
    let _ = catch_unwind(flush_log);
}

unsafe extern "C" fn provider_should_block_standby_mode(_this: *mut c_void) -> bool {
    false
}

unsafe extern "C" fn provider_noop(_this: *mut c_void) {}

/// Keeps this library mapped for the life of the process: hooked vtables point into it, and
/// another tool may have chained onto our detours.
fn pin_library() {
    let mut info = libc::Dl_info {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    let anchor = pose_updated_detour as *const c_void;
    // SAFETY: dladdr only inspects the loader's tables; `info` is writable.
    let found = unsafe { libc::dladdr(anchor, &mut info) } != 0 && !info.dli_fname.is_null();
    if !found {
        log("dladdr failed; the library is not pinned");
        return;
    }
    // SAFETY: re-opening an already loaded object (RTLD_NOLOAD) only bumps its reference count
    // and marks it NODELETE; the handle is intentionally never closed.
    let handle = unsafe { libc::dlopen(info.dli_fname, libc::RTLD_NOW | libc::RTLD_NOLOAD | libc::RTLD_NODELETE) };
    if handle.is_null() {
        log("dlopen(RTLD_NODELETE) failed; the library is not pinned");
    }
}

/// For tests: the device class cache is process-global.
#[doc(hidden)]
pub fn reset_device_classes() {
    for (c, q) in CLASSES.iter().zip(&CLASS_QUERIES) {
        c.store(CLASS_UNKNOWN, Ordering::Relaxed);
        q.store(0, Ordering::Relaxed);
    }
}

/// For tests: the processor the driver installed at `Init`.
#[doc(hidden)]
pub fn driver_processor() -> Option<&'static DriverProcessor> {
    RUNTIME.get().copied()
}

/// For tests: write queued log lines now.
#[doc(hidden)]
pub fn flush_log_now() {
    flush_log();
}
