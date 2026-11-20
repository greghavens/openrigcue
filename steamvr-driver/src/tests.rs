//! In-process form of tools/harness/harness.cpp: a fake vrserver drives the driver through its C ABI and writes
//! a trace, which is compared with the stored trace of the case (tests/reference/<case>/trace.txt). The traces
//! must be identical, which checks interface requests, property writes, device registration and every
//! DriverPose_t byte for byte; the driver's standard output is compared with the timestamps masked.

use crate::elpp::logger::{LogFile, Storage};
use crate::elpp::{self, config};
use crate::openvr::{DriverPose, Interface, PropertyWrite};
use std::ffi::{c_char, c_void, CStr};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());
static TRACE: Mutex<String> = Mutex::new(String::new());

fn t(line: impl AsRef<str>) {
    let mut tr = TRACE.lock().unwrap();
    tr.push_str(line.as_ref());
    tr.push('\n');
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ------------------------------------------------------------------------------------------------ fakes

#[repr(C)]
struct Fake {
    vtable: *const usize,
    name: &'static str,
    missing: Option<&'static str>,
}

macro_rules! unexpected {
    ($($n:literal),*) => {
        [$({
            unsafe extern "C" fn f(this: *mut Fake) {
                t(format!("UNEXPECTED {} slot {}", (*this).name, $n));
            }
            f as *const () as usize
        }),*]
    };
}

fn generic_vtable() -> &'static [usize; 16] {
    static V: std::sync::OnceLock<[usize; 16]> = std::sync::OnceLock::new();
    V.get_or_init(|| unexpected!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15))
}

static mut G_DEVICE: *mut c_void = std::ptr::null_mut();

unsafe extern "C" fn host_tracked_device_added(
    _this: *mut Fake,
    serial: *const c_char,
    cls: i32,
    drv: *mut c_void,
) -> bool {
    t(format!(
        "host.TrackedDeviceAdded serial=\"{}\" class={} driver={}",
        CStr::from_ptr(serial).to_string_lossy(),
        cls,
        if drv.is_null() { "null" } else { "nonnull" }
    ));
    G_DEVICE = drv;
    true
}

unsafe extern "C" fn host_pose_updated(_this: *mut Fake, id: u32, pose: *const DriverPose, size: u32) {
    let bytes = std::slice::from_raw_parts(pose as *const u8, std::mem::size_of::<DriverPose>());
    t(format!("host.TrackedDevicePoseUpdated id={id} size={size} pose={}", hex(bytes)));
}

fn host_vtable() -> &'static [usize; 16] {
    static V: std::sync::OnceLock<[usize; 16]> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        let mut v = *generic_vtable();
        v[0] = host_tracked_device_added as *const () as usize;
        v[1] = host_pose_updated as *const () as usize;
        v
    })
}

unsafe extern "C" fn props_write_batch(_this: *mut Fake, c: u64, b: *mut PropertyWrite, n: u32) -> i32 {
    t(format!("props.WritePropertyBatch container=0x{c:x} count={n}"));
    for i in 0..n as usize {
        let w = &mut *b.add(i);
        let buf = if w.pv_buffer.is_null() {
            "null".to_string()
        } else {
            hex(std::slice::from_raw_parts(w.pv_buffer as *const u8, w.un_buffer_size as usize))
        };
        t(format!(
            "  prop={} writeType={} size={} tag={} buffer={}",
            w.prop, w.write_type, w.un_buffer_size, w.un_tag, buf
        ));
        w.e_set_error = 0;
        w.e_error = 0;
    }
    0
}

unsafe extern "C" fn props_to_container(_this: *mut Fake, idx: u32) -> u64 {
    t(format!("props.TrackedDeviceToPropertyContainer {idx}"));
    0x1234_5600_0000_0000u64 | u64::from(idx)
}

fn props_vtable() -> &'static [usize; 16] {
    static V: std::sync::OnceLock<[usize; 16]> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        let mut v = *generic_vtable();
        v[1] = props_write_batch as *const () as usize;
        v[3] = props_to_container as *const () as usize;
        v
    })
}

struct Objects {
    host: Fake,
    props: Fake,
    settings: Fake,
    log: Fake,
    manager: Fake,
    resources: Fake,
    watchdog_host: Fake,
    other: Fake,
}

fn objects() -> &'static mut Objects {
    static mut O: Option<Objects> = None;
    // SAFETY: tests run one at a time under LOCK.
    unsafe {
        let o = &mut *std::ptr::addr_of_mut!(O);
        o.get_or_insert_with(|| {
            let g = generic_vtable().as_ptr();
            let mk = |vtable: *const usize, name| Fake { vtable, name, missing: None };
            Objects {
                host: mk(host_vtable().as_ptr(), "IVRServerDriverHost_006"),
                props: mk(props_vtable().as_ptr(), "IVRProperties_001"),
                settings: mk(g, "IVRSettings_003"),
                log: mk(g, "IVRDriverLog_001"),
                manager: mk(g, "IVRDriverManager_001"),
                resources: mk(g, "IVRResources_001"),
                watchdog_host: mk(g, "IVRWatchdogHost_002"),
                other: mk(g, "other"),
            }
        })
    }
}

unsafe extern "C" fn ctx_get_generic_interface(this: *mut Fake, v: *const c_char, err: *mut i32) -> *mut c_void {
    let v = CStr::from_ptr(v).to_str().unwrap();
    let o = objects();
    let mut r: *mut Fake = match v {
        "IVRServerDriverHost_006" => &mut o.host,
        "IVRProperties_001" => &mut o.props,
        "IVRSettings_003" => &mut o.settings,
        "IVRDriverLog_001" => &mut o.log,
        "IVRDriverManager_001" => &mut o.manager,
        "IVRResources_001" => &mut o.resources,
        "IVRWatchdogHost_002" => &mut o.watchdog_host,
        _ => &mut o.other,
    };
    if (*this).missing == Some(v) {
        r = std::ptr::null_mut();
    }
    t(format!(
        "ctx.GetGenericInterface \"{v}\" err_ptr={} -> {}",
        if err.is_null() { "null" } else { "nonnull" },
        if r.is_null() { "null" } else { "object" }
    ));
    if !err.is_null() {
        *err = if r.is_null() { 105 } else { 0 };
    }
    r.cast()
}

unsafe extern "C" fn ctx_get_driver_handle(_this: *mut Fake) -> u64 {
    t("UNEXPECTED ctx.GetDriverHandle");
    0
}

fn ctx_vtable() -> &'static [usize; 2] {
    static V: std::sync::OnceLock<[usize; 2]> = std::sync::OnceLock::new();
    V.get_or_init(|| [ctx_get_generic_interface as *const () as usize, ctx_get_driver_handle as *const () as usize])
}

fn new_ctx(missing: Option<&'static str>) -> Box<Fake> {
    Box::new(Fake { vtable: ctx_vtable().as_ptr(), name: "ctx", missing })
}

// ------------------------------------------------------------------------------------------------ calls

unsafe fn vcall<F: Copy>(obj: *mut c_void, slot: usize) -> F {
    let vt = *(obj as *const *const usize);
    std::mem::transmute_copy(&*vt.add(slot))
}

type Factory = unsafe extern "C" fn(*const c_char, *mut i32) -> *mut c_void;

fn factory(name: &str, rc: Option<&mut i32>) -> *mut c_void {
    let c = std::ffi::CString::new(name).unwrap();
    let f: Factory = crate::HmdDriverFactory;
    // SAFETY: calling the export like vrserver.
    unsafe { f(c.as_ptr(), rc.map_or(std::ptr::null_mut(), |r| r as *mut i32)) }
}

fn provider() -> *mut c_void {
    let mut rc = -1;
    let p = factory("IServerTrackedDeviceProvider_004", Some(&mut rc));
    t(format!("factory IServerTrackedDeviceProvider_004 -> {} rc={rc}", if p.is_null() { "null" } else { "object" }));
    p
}

unsafe fn init(p: *mut c_void, ctx: &mut Fake) -> i32 {
    vcall::<unsafe extern "C" fn(*mut c_void, *mut Interface) -> i32>(p, 0)(p, (ctx as *mut Fake).cast())
}
unsafe fn cleanup(p: *mut c_void) {
    vcall::<unsafe extern "C" fn(*mut c_void)>(p, 1)(p)
}
unsafe fn frame(p: *mut c_void, label: &str) {
    t(format!("RunFrame {label}"));
    vcall::<unsafe extern "C" fn(*mut c_void)>(p, 3)(p)
}
unsafe fn activate(d: *mut c_void, id: u32) -> i32 {
    vcall::<unsafe extern "C" fn(*mut c_void, u32) -> i32>(d, 0)(d, id)
}

/// A named section as the harness creates it (`size` bytes, zero filled).
struct Section(std::fs::File);

impl Section {
    fn create(dir: &Path, name: &str, size: u64) -> Section {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join(crate::section::mirror_file_name(name)))
            .unwrap();
        f.set_len(size).unwrap();
        Section(f)
    }
    fn set(&self, i: usize, v: f64) {
        use std::os::unix::fs::FileExt;
        self.0.write_at(&v.to_le_bytes(), 8 * i as u64).unwrap();
    }
    fn set6(&self, v: [f64; 6]) {
        for (i, x) in v.iter().enumerate() {
            self.set(i, *x);
        }
    }
}

// ------------------------------------------------------------------------------------------------ scenarios

// ------------------------------------------------------------------------------------------------ runner

/// `[LEVEL] YYYY-MM-DD HH:MM:SS: ` -> `[LEVEL] <time>: `
fn mask_times(text: &[u8]) -> String {
    let s = String::from_utf8_lossy(text);
    s.lines()
        .map(
            |l| {
                if l.len() > 28 && l.starts_with('[') {
                    format!("{}<time>{}", &l[..8], &l[27..])
                } else {
                    l.to_string()
                }
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn in_page_error_catch_path_state() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("srs-vr-inpage-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("OPENRIGCUE_TEST_MIRROR_DIR", &dir);
    TRACE.lock().unwrap().clear();
    // SAFETY: test-only reset of the module globals.
    unsafe {
        *crate::context::ctx() = std::mem::zeroed();
        G_DEVICE = std::ptr::null_mut();
    }
    let stdout_path = dir.join("stdout.txt");
    let stdout = std::fs::File::create(&stdout_path).unwrap();
    let log_path = dir.join("driver_motionpose.log");
    let mut storage = Storage::new(config::Typed::default(), LogFile::open(log_path.to_str().unwrap()));
    use std::os::fd::AsRawFd;
    storage.stdout_fd = stdout.as_raw_fd();
    elpp::set_storage(Some(storage));

    // SAFETY: driving the driver like vrserver does; the device is inspected between frames.
    unsafe {
        let prov = provider();
        let mut ctx = new_ctx(None);
        assert_eq!(init(prov, &mut ctx), 0);
        assert_eq!(activate(G_DEVICE, 5), 0);
        let rig = Section::create(&dir, "Local\\SimRacingStudioMotionRigPose", 4096);
        rig.set6([12.5, -30.25, 7.75, 15.0, -4.5, 3.25]);
        frame(prov, "connected");
        let d = &*(G_DEVICE as *const crate::device::Device);
        assert!(d.mover_connected && d.last_submitted.pose_is_valid);
        let (handle, view) = (d.map_file_mover, d.rig_pose);
        assert_ne!(handle, view, "the view is an object of its own");
        let valid_pose = d.last_submitted;

        std::fs::write(dir.join("Local_SimRacingStudioMotionRigPose.in-page-error"), b"").unwrap();
        *libc::__errno_location() = libc::EACCES;
        frame(prov, "in-page-error");
        let d = &*(G_DEVICE as *const crate::device::Device);
        assert!(!d.mover_connected && !d.ovrmc_connected);
        assert_eq!(d.last_submitted.result, crate::openvr::TRACKING_RESULT_CALIBRATING_IN_PROGRESS);
        assert!(!d.last_submitted.pose_is_valid);
        assert_eq!(d.last_submitted.q_rotation, valid_pose.q_rotation);
        // Closing the rig section handle closes the handle only: the view stays open (it is never unmapped).
        assert!(libc::fcntl(view, libc::F_GETFD) >= 0);
        let trace = TRACE.lock().unwrap().clone();
        let last = trace.lines().rev().find(|l| l.contains("pose=")).unwrap();
        let pose = &last[last.find("pose=").unwrap() + 5..];
        assert_eq!(&pose[2 * 0x110..2 * 0x115], "6400000000", "result 100, poseIsValid 0");
        cleanup(prov);
    }
    elpp::shutdown();
    elpp::set_storage(None);
    let out = mask_times(&std::fs::read(&stdout_path).unwrap());
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        [
            "[TRACE] <time>: CMotionPoseControllerDriver()",
            "[INFO ] <time>: Activate motionPoseController",
            "[INFO ] <time>: Successfully connected to Local\\SimRacingStudioMotionRigPose",
            "[ERROR] <time>: MMF failed: Unknown exception ",
            "[ERROR] <time>: Failed to close OVRMC handle! Error: 6",
            "[TRACE] <time>: ~CMotionPoseControllerDriver()",
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn emptied_mirror_keeps_the_view_contents() {
    // An empty mirror is a section whose producer has gone. The driver's open handle and view keep the Windows
    // section alive with its last bytes, and a producer that comes back writes into that same section: the pose
    // stays the last one (valid, result 200), no exception, and follows the producer again once it is back.
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("srs-vr-emptied-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("OPENRIGCUE_TEST_MIRROR_DIR", &dir);
    let rig = Section::create(&dir, "Local\\SimRacingStudioMotionRigPose", 48);
    rig.set6([12.5, -30.25, 7.75, 15.0, -4.5, 3.25]);
    let mut d = crate::device::Device::new();
    let first = d.get_pose().unwrap();
    assert!(first.pose_is_valid && d.mover_connected);
    rig.0.set_len(0).unwrap();
    let kept = d.get_pose().unwrap();
    // Identical but for the device-connected byte (+0x117), which the pose copies from the connected flag before connecting.
    assert_eq!(kept.to_bytes()[..0x117], first.to_bytes()[..0x117]);
    assert!(!first.device_is_connected && kept.device_is_connected);
    // Not connected any more (no handle): an empty mirror is no section, so the open fails.
    assert_eq!(crate::section::open_file_mapping(crate::section::RIG_POSE_SECTION), crate::section::NULL_HANDLE);
    rig.0.set_len(48).unwrap();
    rig.set6([-100.0, 250.5, -12.125, 90.0, 30.0, -45.0]);
    let live = d.get_pose().unwrap();
    assert!(live.pose_is_valid);
    assert_ne!(live.vec_position, first.vec_position);
    std::mem::forget(d);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn device_vtable_has_the_original_slots() {
    // The device's virtual table: slots 0..5, +0x30 scalar deleting destructor (the clean-up calls it with flags 1), +0x38 power-off.
    use crate::device::DeviceVtable;
    use std::mem::{offset_of, size_of};
    assert_eq!(offset_of!(DeviceVtable, get_pose), 0x28);
    assert_eq!(offset_of!(DeviceVtable, scalar_deleting_destructor), 0x30);
    assert_eq!(offset_of!(DeviceVtable, power_off), 0x38);
    assert_eq!(size_of::<DeviceVtable>(), 0x40);
}
