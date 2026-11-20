//! An in-process stand-in for vrserver: fake `IVRDriverContext`, `IVRServerDriverHost` and
//! `IVRProperties` objects whose vtables live in read-only pages (like vrserver's GNU_RELRO
//! `.data.rel.ro`), driven through the driver's real `HmdDriverFactory` and provider vtable.
//!
//! Everything here is process-global (as in vrserver), so it is one sequential test.

use std::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use openrigcue_rigpose::{
    monotonic_now_ns, ConsumerKind, CorOffset, RigPoseReader, RigPoseWriter, RigSample, RigValues,
};
use openrigcue_steamvr_driver::compensate::DeviceKind;
use openrigcue_steamvr_driver::hook::mapping_protection;
use openrigcue_steamvr_driver::openvr::{device_class, DriverPose, PropertyRead};
use openrigcue_steamvr_driver::provider::{self, PoseProcessor};
use openrigcue_steamvr_driver::HmdDriverFactory;

// --- fake objects -----------------------------------------------------------------------------

#[repr(C)]
struct FakeObject {
    vptr: *const usize,
}

/// A vtable of `entries` in its own page, made read-only after writing.
fn read_only_vtable(entries: &[usize]) -> *mut usize {
    // SAFETY: anonymous private mapping of one page; written before being made read-only.
    unsafe {
        let page = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert_ne!(page, libc::MAP_FAILED);
        let table = page as *mut usize;
        for (i, e) in entries.iter().enumerate() {
            *table.add(i) = *e;
        }
        assert_eq!(libc::mprotect(page, 4096, libc::PROT_READ), 0);
        table
    }
}

/// What another hooking tool would do: rewrite a slot in a read-only page.
fn patch_word(address: *mut usize, value: usize) {
    let page = (address as usize & !4095) as *mut c_void;
    // SAFETY: test-owned page from `read_only_vtable`.
    unsafe {
        assert_eq!(libc::mprotect(page, 4096, libc::PROT_READ | libc::PROT_WRITE), 0);
        *address = value;
        assert_eq!(libc::mprotect(page, 4096, libc::PROT_READ), 0);
    }
}

fn slot(object: &FakeObject, n: usize) -> usize {
    // SAFETY: test vtables have at least n + 1 entries.
    unsafe { *object.vptr.add(n) }
}

fn addr<T>(f: T) -> usize
where
    T: Copy,
{
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<usize>());
    // SAFETY: only used with function pointers.
    unsafe { std::mem::transmute_copy(&f) }
}

#[derive(Debug, Clone, PartialEq)]
struct Received {
    host: &'static str,
    device: u32,
    x: f64,
}

static RECEIVED: Mutex<Vec<Received>> = Mutex::new(Vec::new());
static CALL_ORDER: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static LOG_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record(host: &'static str, device: u32, pose: *const DriverPose) {
    // SAFETY: the detour forwards a valid pose pointer.
    let x = unsafe { (*pose).vec_position[0] };
    RECEIVED.lock().unwrap().push(Received { host, device, x });
    CALL_ORDER.lock().unwrap().push(host);
}

unsafe extern "C" fn original_pose_a(_this: *mut c_void, device: u32, pose: *const DriverPose, _size: u32) {
    record("A", device, pose);
}

unsafe extern "C" fn original_pose_b(_this: *mut c_void, device: u32, pose: *const DriverPose, _size: u32) {
    record("B", device, pose);
}

unsafe extern "C" fn original_pose_c(_this: *mut c_void, device: u32, pose: *const DriverPose, _size: u32) {
    record("C", device, pose);
}

type PoseFn = unsafe extern "C" fn(*mut c_void, u32, *const DriverPose, u32);

/// Another tool that hooked host B before us.
unsafe extern "C" fn early_tool_hook(this: *mut c_void, device: u32, pose: *const DriverPose, size: u32) {
    CALL_ORDER.lock().unwrap().push("early-tool");
    // SAFETY: forwards to the real function.
    unsafe { original_pose_b(this, device, pose, size) }
}

/// Another tool that hooks host C after us, chaining to whatever it found (our detour).
static LATE_TOOL_NEXT: AtomicUsize = AtomicUsize::new(0);
unsafe extern "C" fn late_tool_hook(this: *mut c_void, device: u32, pose: *const DriverPose, size: u32) {
    CALL_ORDER.lock().unwrap().push("late-tool");
    // SAFETY: LATE_TOOL_NEXT holds the slot value the tool replaced.
    unsafe {
        let next: PoseFn = std::mem::transmute::<usize, PoseFn>(LATE_TOOL_NEXT.load(Ordering::SeqCst));
        next(this, device, pose, size)
    }
}

/// What vrserver's version-005 host does on a pose update: forward, through
/// the vtable, to slot 1 of the host object it wraps (here host A, the "latest" host).
unsafe extern "C" fn wrapper_pose(_this: *mut c_void, device: u32, pose: *const DriverPose, size: u32) {
    CALL_ORDER.lock().unwrap().push("wrapper");
    let inner: &FakeObject = &FAKES.get().unwrap().host_a;
    // SAFETY: virtual call to slot 1 of the wrapped host, as the real wrapper does.
    unsafe {
        let next: PoseFn = std::mem::transmute::<usize, PoseFn>(slot(inner, 1));
        next((inner as *const FakeObject).cast_mut().cast(), device, pose, size)
    }
}

unsafe extern "C" fn fake_log(_this: *mut c_void, message: *const c_char) {
    // SAFETY: the driver passes a NUL-terminated string.
    let line = unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned();
    LOG_LINES.lock().unwrap().push(line);
}

unsafe extern "C" fn fake_to_container(_this: *mut c_void, device: u32) -> u64 {
    u64::from(device) + 100
}

unsafe extern "C" fn fake_read_property_batch(
    _this: *mut c_void,
    container: u64,
    batch: *mut PropertyRead,
    count: u32,
) -> i32 {
    assert_eq!(count, 1);
    // SAFETY: the driver passes one PropertyRead with an i32 buffer.
    let read = unsafe { &mut *batch };
    assert_eq!(read.prop, 1029);
    let class = match container - 100 {
        1 => device_class::CONTROLLER,
        2 => device_class::TRACKING_REFERENCE,
        3 => device_class::GENERIC_TRACKER,
        _ => {
            read.e_error = 1;
            return 1;
        }
    };
    // SAFETY: buffer of un_buffer_size (4) bytes.
    unsafe { *(read.pv_buffer as *mut i32) = class };
    read.un_tag = 2;
    read.e_error = 0;
    0
}

struct Fakes {
    context: Box<FakeObject>,
    host_a: Box<FakeObject>,
    host_b: Box<FakeObject>,
    host_c: Box<FakeObject>,
    host_wrapper: Box<FakeObject>,
    properties: Box<FakeObject>,
    log: Box<FakeObject>,
}

static FAKES: std::sync::OnceLock<Fakes> = std::sync::OnceLock::new();

// SAFETY: the fakes are immutable after construction (vtable pages are only rewritten through
// the raw pointers, never through these references).
unsafe impl Sync for Fakes {}
// SAFETY: as above.
unsafe impl Send for Fakes {}

unsafe extern "C" fn fake_get_generic_interface(
    _this: *mut c_void,
    version: *const c_char,
    error: *mut i32,
) -> *mut c_void {
    // SAFETY: NUL-terminated by contract.
    let name = unsafe { CStr::from_ptr(version) }.to_str().unwrap();
    let f = FAKES.get().unwrap();
    let object: &FakeObject = match name {
        "IVRServerDriverHost_006" => &f.host_a,
        "IVRServerDriverHost_005" if CONTEXT_OFFERS_WRAPPER.load(Ordering::SeqCst) == 1 => &f.host_wrapper,
        "IVRServerDriverHost_005" => &f.host_b,
        "IVRServerDriverHost_004" if CONTEXT_OFFERS_004.load(Ordering::SeqCst) == 1 => &f.host_c,
        "IVRProperties_001" => &f.properties,
        "IVRDriverLog_001" => &f.log,
        _ => {
            if !error.is_null() {
                // SAFETY: caller-provided.
                unsafe { *error = 105 };
            }
            return std::ptr::null_mut();
        }
    };
    (object as *const FakeObject).cast_mut().cast()
}

/// Host 004 only appears later, as if a driver loaded after us asked for it.
static CONTEXT_OFFERS_004: AtomicUsize = AtomicUsize::new(0);
/// Later, IVRServerDriverHost_005 is a wrapper around host A (as in the real vrserver).
static CONTEXT_OFFERS_WRAPPER: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn fake_get_driver_handle(_this: *mut c_void) -> u64 {
    1
}

fn host(pose_fn: usize) -> Box<FakeObject> {
    // Slots: TrackedDeviceAdded, TrackedDevicePoseUpdated, then the rest unused.
    Box::new(FakeObject { vptr: read_only_vtable(&[0, pose_fn, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]) })
}

fn build_fakes() -> &'static Fakes {
    FAKES.get_or_init(|| Fakes {
        context: Box::new(FakeObject {
            vptr: read_only_vtable(&[
                addr(fake_get_generic_interface as unsafe extern "C" fn(_, _, _) -> _),
                addr(fake_get_driver_handle as unsafe extern "C" fn(_) -> _),
            ]),
        }),
        host_a: host(addr(original_pose_a as PoseFn)),
        host_b: host(addr(early_tool_hook as PoseFn)),
        host_c: host(addr(original_pose_c as PoseFn)),
        host_wrapper: host(addr(wrapper_pose as PoseFn)),
        properties: Box::new(FakeObject {
            vptr: read_only_vtable(&[
                addr(fake_read_property_batch as unsafe extern "C" fn(_, _, _, _) -> _),
                0,
                0,
                addr(fake_to_container as unsafe extern "C" fn(_, _) -> _),
            ]),
        }),
        log: Box::new(FakeObject { vptr: read_only_vtable(&[addr(fake_log as unsafe extern "C" fn(_, _))]) }),
    })
}

// --- driving the driver like vrserver does ----------------------------------------------------

struct Provider(*mut c_void);

impl Provider {
    fn fns(&self) -> *const usize {
        // SAFETY: the provider object's first word is its vptr.
        unsafe { *(self.0 as *const *const usize) }
    }
    fn init(&self, context: &FakeObject) -> i32 {
        // SAFETY: slot 0 is Init(IVRDriverContext*).
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 = std::mem::transmute(*self.fns());
            f(self.0, (context as *const FakeObject).cast_mut().cast())
        }
    }
    fn cleanup(&self) {
        // SAFETY: slot 1 is Cleanup().
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(*self.fns().add(1));
            f(self.0)
        }
    }
    fn interface_versions(&self) -> Vec<String> {
        // SAFETY: slot 2 is GetInterfaceVersions(), a NULL-terminated array of C strings.
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void) -> *const *const c_char = std::mem::transmute(*self.fns().add(2));
            let mut list = f(self.0);
            let mut out = Vec::new();
            while !(*list).is_null() {
                out.push(CStr::from_ptr(*list).to_string_lossy().into_owned());
                list = list.add(1);
            }
            out
        }
    }
    fn run_frame(&self) {
        // SAFETY: slot 3 is RunFrame().
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(*self.fns().add(3));
            f(self.0)
        }
    }
}

/// What a tracking driver does: call TrackedDevicePoseUpdated through the host's vtable.
fn report_pose(host: &FakeObject, device: u32, x: f64, size: u32) {
    let pose = DriverPose { vec_position: [x, 1.2, 0.0], ..DriverPose::default() };
    // SAFETY: slot 1 is TrackedDevicePoseUpdated; `pose` outlives the call.
    unsafe {
        let f: PoseFn = std::mem::transmute::<usize, PoseFn>(slot(host, 1));
        f((host as *const FakeObject).cast_mut().cast(), device, &pose, size);
    }
}

fn last_received() -> Received {
    RECEIVED.lock().unwrap().last().cloned().expect("a pose reached the original")
}

fn take_order() -> Vec<&'static str> {
    std::mem::take(&mut *CALL_ORDER.lock().unwrap())
}

/// Records the device kind and shifts x by 1 for any device it is asked to compensate.
struct ShiftProcessor {
    kinds: Mutex<Vec<(u32, DeviceKind)>>,
}

impl PoseProcessor for ShiftProcessor {
    fn process(&self, device: u32, kind: DeviceKind, pose: &mut DriverPose, _now_ns: u64) -> bool {
        self.kinds.lock().unwrap().push((device, kind));
        if kind == DeviceKind::Other {
            return false;
        }
        pose.vec_position[0] += 1.0;
        true
    }
}

static SHIFT: ShiftProcessor = ShiftProcessor { kinds: Mutex::new(Vec::new()) };

const POSE_SIZE: u32 = std::mem::size_of::<DriverPose>() as u32;

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("openrigcue-fake-vrserver-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn driver_in_a_fake_vrserver() {
    let dir = temp_dir();
    let rig_path = dir.join("rig-pose");
    // Keep the test away from the real /dev/shm and ~/.config.
    std::env::set_var("OPENRIGCUE_RIG_POSE", &rig_path);
    std::env::set_var("OPENRIGCUE_STEAMVR_CONFIG", dir.join("steamvr-driver.toml"));
    let f = build_fakes();
    let original_a = slot(&f.host_a, 1);
    let original_b = slot(&f.host_b, 1);
    let original_context = slot(&f.context, 0);

    // Factory: only the provider interface is offered.
    let mut rc = 0;
    // SAFETY: NUL-terminated name and a writable return code, as vrserver passes them.
    let none = unsafe { HmdDriverFactory(c"IVRWatchdogProvider_001".as_ptr(), &mut rc) };
    assert!(none.is_null());
    assert_eq!(rc, 105);
    // SAFETY: as above.
    let provider = Provider(unsafe { HmdDriverFactory(c"IServerTrackedDeviceProvider_004".as_ptr(), &mut rc) });
    assert!(!provider.0.is_null());
    let versions = provider.interface_versions();
    assert!(versions.iter().any(|v| v == "IServerTrackedDeviceProvider_004"));
    assert_eq!(versions.len(), 12);

    // Init hooks the context and both host classes; the pages stay read-only.
    assert_eq!(provider.init(&f.context), 0);
    assert_ne!(slot(&f.context, 0), original_context);
    assert_ne!(slot(&f.host_a, 1), original_a);
    assert_ne!(slot(&f.host_b, 1), original_b);
    for object in [&f.context, &f.host_a, &f.host_b] {
        assert_eq!(mapping_protection(object.vptr as usize).unwrap(), libc::PROT_READ);
    }
    let hooks = provider::hook_state();
    assert_eq!((hooks.host_hooks, hooks.context_hooks), (2, 1));

    // Pose updates reach the host with the processor's change; classes come from IVRProperties.
    provider::install_processor(&SHIFT);
    for (device, expect_x) in [(0, 1.5), (1, 1.5), (2, 0.5), (3, 1.5)] {
        report_pose(&f.host_a, device, 0.5, POSE_SIZE);
        assert_eq!(last_received(), Received { host: "A", device, x: expect_x });
    }
    let kinds = std::mem::take(&mut *SHIFT.kinds.lock().unwrap());
    assert_eq!(
        kinds,
        vec![(0, DeviceKind::Hmd), (1, DeviceKind::Controller), (2, DeviceKind::Other), (3, DeviceKind::Tracker)]
    );
    // An unknown pose struct size is forwarded untouched.
    report_pose(&f.host_a, 0, 0.5, POSE_SIZE + 8);
    assert_eq!(last_received().x, 0.5);
    take_order();

    // Host B was hooked by another tool first: we chain in front of it.
    report_pose(&f.host_b, 0, 0.0, POSE_SIZE);
    assert_eq!(take_order(), vec!["early-tool", "B"]);
    assert_eq!(last_received().x, 1.0);

    // A host class handed out later is hooked by the context detour.
    CONTEXT_OFFERS_004.store(1, Ordering::SeqCst);
    let original_c = slot(&f.host_c, 1);
    let mut err = 0;
    // SAFETY: slot 0 of the context vtable is GetGenericInterface (now our detour).
    let got = unsafe {
        let gi: unsafe extern "C" fn(*mut c_void, *const c_char, *mut i32) -> *mut c_void =
            std::mem::transmute::<usize, _>(slot(&f.context, 0));
        gi((&*f.context as *const FakeObject).cast_mut().cast(), c"IVRServerDriverHost_004".as_ptr(), &mut err)
    };
    assert_eq!(got, (&*f.host_c as *const FakeObject).cast_mut().cast());
    let our_detour = slot(&f.host_c, 1);
    assert_ne!(our_detour, original_c);
    report_pose(&f.host_c, 1, 0.0, POSE_SIZE);
    assert_eq!(last_received(), Received { host: "C", device: 1, x: 1.0 });

    // Then a late tool hooks C on top of us.
    LATE_TOOL_NEXT.store(our_detour, Ordering::SeqCst);
    // SAFETY: slot 1 of host C's vtable.
    patch_word(unsafe { (f.host_c.vptr as *mut usize).add(1) }, addr(late_tool_hook as PoseFn));
    take_order();
    report_pose(&f.host_c, 1, 0.0, POSE_SIZE);
    assert_eq!(take_order(), vec!["late-tool", "C"]);
    assert_eq!(last_received().x, 1.0);

    // A wrapper host class (vrserver's 005/004 forward virtually into the latest host): both its
    // slot and the inner host's slot hold our detour, yet the pose is compensated exactly once.
    CONTEXT_OFFERS_WRAPPER.store(1, Ordering::SeqCst);
    let original_wrapper = slot(&f.host_wrapper, 1);
    // SAFETY: slot 0 of the context vtable is GetGenericInterface (our detour).
    let got = unsafe {
        let gi: unsafe extern "C" fn(*mut c_void, *const c_char, *mut i32) -> *mut c_void =
            std::mem::transmute::<usize, _>(slot(&f.context, 0));
        gi((&*f.context as *const FakeObject).cast_mut().cast(), c"IVRServerDriverHost_005".as_ptr(), &mut err)
    };
    assert_eq!(got, (&*f.host_wrapper as *const FakeObject).cast_mut().cast());
    assert_ne!(slot(&f.host_wrapper, 1), original_wrapper, "wrapper hooked");
    take_order();
    report_pose(&f.host_wrapper, 0, 0.0, POSE_SIZE);
    assert_eq!(take_order(), vec!["wrapper", "A"]);
    assert_eq!(last_received(), Received { host: "A", device: 0, x: 1.0 }, "compensated once, not twice");
    // The guard is per call: a direct pose to the inner host afterwards is compensated as usual.
    report_pose(&f.host_a, 0, 0.0, POSE_SIZE);
    assert_eq!(last_received().x, 1.0);
    SHIFT.kinds.lock().unwrap().clear();
    take_order();

    // The real processor with the real rig-pose contract: no file yet, so no change.
    let driver = provider::driver_processor().expect("Init created the runtime");
    provider::install_processor(driver);
    report_pose(&f.host_a, 0, 0.5, POSE_SIZE);
    assert_eq!(last_received().x, 0.5);

    // The daemon starts publishing: within a housekeeping cycle the driver opens the file, and the
    // headset pose is calibrated and compensated; the driver takes the active-consumer claim.
    let mut writer = RigPoseWriter::create(&rig_path).unwrap();
    let observer = RigPoseReader::open(&rig_path).unwrap();
    let publish = |writer: &mut RigPoseWriter, roll: f64| {
        let sample = RigSample {
            timestamp_ns: monotonic_now_ns(),
            values: RigValues { roll, ..RigValues::ZERO },
            // CoR 0.6 m below the eyes.
            cor_offset: CorOffset { forward: 0.0, down: 0.6, right: 0.0 },
            enabled: true,
            ..RigSample::default()
        };
        writer.publish(&sample).unwrap();
    };
    publish(&mut writer, 0.0);
    wait_for("the headset pose to be compensated", || {
        publish(&mut writer, 0.0);
        report_pose(&f.host_a, 0, 0.5, POSE_SIZE);
        driver.compensator.calibration().is_some()
    });
    // Rig rolled 10°: the head (same tracked pose) is swung back about the CoR 0.6 m below it,
    // sideways by about 0.6·sin 10° ≈ 0.10 m.
    publish(&mut writer, 10.0);
    report_pose(&f.host_a, 0, 0.5, POSE_SIZE);
    let shift = (last_received().x - 0.5).abs();
    assert!((shift - 0.6 * 10f64.to_radians().sin()).abs() < 0.01, "shift {shift}");
    wait_for("the driver's claim", || {
        report_pose(&f.host_a, 0, 0.5, POSE_SIZE);
        matches!(observer.consumer(), Ok(Some(c)) if c.kind == ConsumerKind::STEAMVR_DRIVER)
    });
    let status = dir.join("steamvr-driver.status");
    wait_for("the status file", || std::fs::read_to_string(&status).is_ok_and(|s| s.contains("calibrated=true")));

    // Log lines are written through IVRDriverLog on RunFrame.
    provider.run_frame();
    assert!(LOG_LINES.lock().unwrap().iter().any(|l| l.contains("hooked IVRServerDriverHost_006")));

    // Cleanup restores what still holds our detour and leaves the late tool's chain intact.
    provider.cleanup();
    assert_eq!(slot(&f.host_a, 1), original_a);
    assert_eq!(slot(&f.host_b, 1), original_b);
    assert_eq!(slot(&f.context, 0), original_context);
    assert_eq!(slot(&f.host_c, 1), addr(late_tool_hook as PoseFn));
    assert_eq!(slot(&f.host_wrapper, 1), original_wrapper);
    assert_eq!(mapping_protection(f.host_a.vptr as usize).unwrap(), libc::PROT_READ);
    assert_eq!(observer.consumer().unwrap(), None, "claim released on Cleanup");
    take_order();
    report_pose(&f.host_c, 1, 0.0, POSE_SIZE);
    assert_eq!(take_order(), vec!["late-tool", "C"]);
    assert_eq!(last_received().x, 0.0, "inactive after Cleanup: passes through");

    // SteamVR may Init the driver again: hooks come back, and C (still chained) is not re-hooked,
    // so nothing forwards to itself.
    assert_eq!(provider.init(&f.context), 0);
    provider::install_processor(&SHIFT);
    assert_ne!(slot(&f.host_a, 1), original_a);
    report_pose(&f.host_c, 1, 0.0, POSE_SIZE);
    assert_eq!(take_order(), vec!["late-tool", "C"]);
    assert_eq!(last_received().x, 1.0);
    report_pose(&f.host_a, 0, 0.0, POSE_SIZE);
    assert_eq!(last_received(), Received { host: "A", device: 0, x: 1.0 });
    provider.cleanup();
    assert_eq!(slot(&f.host_a, 1), original_a);
    let _ = std::fs::remove_dir_all(&dir);
}
