//! Host-side tests. Expected values are in
//! tests/vectors/registrations_{x64,x86}.txt and attribute*.txt;
//! the byte-level end-to-end check is
//! harness/tests/replay.rs.

use core::ffi::{c_char, c_void, CStr};
use std::cell::RefCell;
use std::sync::Mutex;

use crate::layout::{self, off, CHANNELS, MAP_SIZE};
use crate::os::{Handle, Os, MAP_NAME_W};
use crate::plugin::{self, SharedMemory};
use crate::scs::*;

static LOCK: Mutex<()> = Mutex::new(());

#[test]
fn mapping_name_is_the_original_wide_literal() {
    let s: Vec<u16> = "Local\\SimTelemetryETS2\0".encode_utf16().collect();
    assert_eq!(&MAP_NAME_W[..], &s[..]);
    assert_eq!(MAP_SIZE, 0x4000);
}

// ---- mock kernel32 ------------------------------------------------------

#[derive(Debug, PartialEq, Clone)]
enum Call {
    New(usize),
    Create { protect: u32, high: u32, low: u32, name: String },
    LastError,
    Open { access: u32, inherit: bool, name: String },
    Map { handle: usize, access: u32, high: u32, low: u32, bytes: usize },
    Unmap(usize),
    Close(usize),
}

struct Mock {
    calls: RefCell<Vec<Call>>,
    new_fails: bool,
    create: usize,
    error: u32,
    open: usize,
    map_fails: bool,
    buf: *mut u8,
}

// SAFETY: tests serialise on LOCK.
unsafe impl Sync for Mock {}

fn wstr(p: *const u16) -> String {
    let mut v = Vec::new();
    let mut i = 0;
    unsafe {
        while *p.add(i) != 0 {
            v.push(*p.add(i));
            i += 1;
        }
    }
    String::from_utf16(&v).unwrap()
}

impl Os for Mock {
    unsafe fn operator_new(&self, size: usize) -> *mut u8 {
        self.calls.borrow_mut().push(Call::New(size));
        if self.new_fails {
            return core::ptr::null_mut();
        }
        Box::into_raw(vec![0u8; size].into_boxed_slice()) as *mut u8
    }
    unsafe fn create_file_mapping(&self, protect: u32, high: u32, low: u32, name: *const u16) -> Handle {
        self.calls.borrow_mut().push(Call::Create { protect, high, low, name: wstr(name) });
        self.create as Handle
    }
    fn last_error(&self) -> u32 {
        self.calls.borrow_mut().push(Call::LastError);
        self.error
    }
    unsafe fn open_file_mapping(&self, access: u32, inherit: bool, name: *const u16) -> Handle {
        self.calls.borrow_mut().push(Call::Open { access, inherit, name: wstr(name) });
        self.open as Handle
    }
    unsafe fn map_view(&self, handle: Handle, access: u32, high: u32, low: u32, bytes: usize) -> *mut u8 {
        self.calls.borrow_mut().push(Call::Map { handle: handle as usize, access, high, low, bytes });
        if self.map_fails {
            core::ptr::null_mut()
        } else {
            self.buf
        }
    }
    unsafe fn unmap_view(&self, view: *mut u8) {
        self.calls.borrow_mut().push(Call::Unmap(view as usize));
    }
    unsafe fn close_handle(&self, handle: Handle) {
        self.calls.borrow_mut().push(Call::Close(handle as usize));
    }
}

fn mock(f: impl FnOnce(&mut Mock)) -> &'static Mock {
    let buf = Box::into_raw(vec![0xaau8; MAP_SIZE].into_boxed_slice()) as *mut u8;
    let mut m = Mock {
        calls: RefCell::new(Vec::new()),
        new_fails: false,
        create: 0x44,
        error: 0,
        open: 0x88,
        map_fails: false,
        buf,
    };
    f(&mut m);
    Box::leak(Box::new(m))
}

const NAME: &str = "Local\\SimTelemetryETS2";

/// What `operator new` leaves in the object before the ctor runs (its
/// memory is uninitialised; this pattern shows which fields the ctor writes).
const UNWRITTEN: usize = usize::from_ne_bytes([0xcd; core::mem::size_of::<usize>()]);

/// Runs the mapping ctor on a heap object the way init does after
/// `operator new`.
fn open(m: &'static Mock) -> &'static mut SharedMemory {
    let p = Box::into_raw(Box::new(core::mem::MaybeUninit::<SharedMemory>::uninit())) as *mut SharedMemory;
    unsafe {
        core::ptr::write_bytes(p as *mut u8, 0xcd, core::mem::size_of::<SharedMemory>());
        &mut *SharedMemory::construct(p, m)
    }
}

fn create_call() -> Call {
    Call::Create { protect: 4, high: 0, low: 0x4000, name: NAME.into() }
}

#[test]
fn mapping_create_then_map_and_zero() {
    let m = mock(|_| {});
    let sm = open(m);
    assert!(sm.ok);
    assert_eq!(sm.size, 0x4000);
    assert_eq!(sm.view, m.buf);
    assert_eq!(
        *m.calls.borrow(),
        vec![create_call(), Call::Map { handle: 0x44, access: 0xf001f, high: 0, low: 0, bytes: 0x4000 }]
    );
    assert!(unsafe { core::slice::from_raw_parts(m.buf, MAP_SIZE) }.iter().all(|&b| b == 0));
}

#[test]
fn mapping_already_exists_error_opens_by_name() {
    let m = mock(|m| {
        m.create = 0;
        m.error = 0xb7;
    });
    let sm = open(m);
    assert!(sm.ok);
    assert_eq!(
        *m.calls.borrow(),
        vec![
            create_call(),
            Call::LastError,
            Call::Open { access: 0xf001f, inherit: false, name: NAME.into() },
            Call::Map { handle: 0x88, access: 0xf001f, high: 0, low: 0, bytes: 0x4000 },
        ]
    );
}

#[test]
fn mapping_other_create_error_gives_up() {
    let m = mock(|m| {
        m.create = 0;
        m.error = 5;
    });
    let sm = open(m);
    assert!(!sm.ok);
    assert_eq!(*m.calls.borrow(), vec![create_call(), Call::LastError]);
    // Handle written with the failed result, view never written.
    assert!(sm.handle.is_null());
    assert_eq!(sm.view as usize, UNWRITTEN);
}

#[test]
fn mapping_open_failure_gives_up() {
    let m = mock(|m| {
        m.create = 0;
        m.error = 0xb7;
        m.open = 0;
    });
    let sm = open(m);
    assert!(!sm.ok);
    assert_eq!(m.calls.borrow().len(), 3);
    assert!(sm.handle.is_null());
    assert_eq!(sm.view as usize, UNWRITTEN);
}

#[test]
fn mapping_view_failure_closes_handle_but_keeps_it() {
    let m = mock(|m| m.map_fails = true);
    let sm = open(m);
    assert!(!sm.ok);
    assert_eq!(sm.handle as usize, 0x44);
    // MapViewOfFile's NULL is stored before the failure test.
    assert!(sm.view.is_null());
    assert_eq!(m.calls.borrow().last(), Some(&Call::Close(0x44)));
    // Nothing more to release on shutdown: ok is false.
    let p: *mut SharedMemory = sm;
    unsafe { SharedMemory::close(p, m) };
    assert_eq!(m.calls.borrow().len(), 3);
}

// ---- fake SDK -------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Sdk {
    Event(u32),
    Channel { name: String, index: u32, type_: u32, flags: u32, ctx: usize },
}

thread_local! {
    static SDK: RefCell<Vec<Sdk>> = const { RefCell::new(Vec::new()) };
    static REFUSE: RefCell<u32> = const { RefCell::new(0) };
    static CHANNEL_CBS: RefCell<Vec<(String, ScsChannelCallback, ScsContext)>> = const { RefCell::new(Vec::new()) };
    static EVENT_CBS: RefCell<Vec<(u32, ScsEventCallback)>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn sdk_log(_: i32, _: *const c_char) {}
unsafe extern "system" fn sdk_reg_event(event: u32, cb: ScsEventCallback, ctx: ScsContext) -> ScsResult {
    assert!(ctx.is_null());
    SDK.with(|s| s.borrow_mut().push(Sdk::Event(event)));
    EVENT_CBS.with(|s| s.borrow_mut().push((event, cb)));
    if REFUSE.with(|r| *r.borrow()) == event {
        -3
    } else {
        0
    }
}
unsafe extern "system" fn sdk_unreg_event(_: u32) -> ScsResult {
    0
}
unsafe extern "system" fn sdk_reg_channel(
    name: *const c_char,
    index: u32,
    type_: u32,
    flags: u32,
    cb: ScsChannelCallback,
    ctx: ScsContext,
) -> ScsResult {
    let name = CStr::from_ptr(name).to_str().unwrap().to_owned();
    SDK.with(|s| s.borrow_mut().push(Sdk::Channel { name: name.clone(), index, type_, flags, ctx: ctx as usize }));
    CHANNEL_CBS.with(|s| s.borrow_mut().push((name, cb, ctx)));
    0
}
unsafe extern "system" fn sdk_unreg_channel(_: *const c_char, _: u32, _: u32) -> ScsResult {
    0
}

fn params(game_version: u32) -> ScsTelemetryInitParams {
    ScsTelemetryInitParams {
        common: ScsSdkInitParams { game_name: c"ETS2".as_ptr(), game_id: c"eut2".as_ptr(), game_version, log: sdk_log },
        register_for_event: sdk_reg_event,
        unregister_from_event: sdk_unreg_event,
        register_for_channel: sdk_reg_channel,
        unregister_from_channel: sdk_unreg_channel,
    }
}

fn reset_sdk(refuse: u32) {
    SDK.with(|s| s.borrow_mut().clear());
    CHANNEL_CBS.with(|s| s.borrow_mut().clear());
    EVENT_CBS.with(|s| s.borrow_mut().clear());
    REFUSE.with(|r| *r.borrow_mut() = refuse);
}

fn events() -> Vec<u32> {
    SDK.with(|s| s.borrow().iter().filter_map(|e| if let Sdk::Event(n) = e { Some(*n) } else { None }).collect())
}

fn channel_count() -> usize {
    SDK.with(|s| s.borrow().iter().filter(|e| matches!(e, Sdk::Channel { .. })).count())
}

unsafe fn rd32(p: *mut u8, o: usize) -> u32 {
    core::ptr::read_unaligned(p.add(o) as *const u32)
}

#[test]
fn init_rejects_other_api_versions_and_null_params_before_touching_kernel32() {
    let _l = LOCK.lock().unwrap();
    let m = mock(|_| {});
    reset_sdk(0);
    let p = params(0x0001_0012);
    unsafe {
        assert_eq!(plugin::telemetry_init(0x0001_0001, &p, m), -1);
        assert_eq!(plugin::telemetry_init(0, &p, m), -1);
        assert_eq!(plugin::telemetry_init(0x0001_0000, core::ptr::null(), m), -7);
    }
    assert!(m.calls.borrow().is_empty());
    assert!(events().is_empty());
}

#[test]
fn init_fails_without_mapping() {
    let _l = LOCK.lock().unwrap();
    for m in [mock(|m| m.new_fails = true), mock(|m| m.map_fails = true)] {
        reset_sdk(0);
        assert_eq!(unsafe { plugin::telemetry_init(0x10000, &params(0x10012), m) }, -7);
        assert!(events().is_empty());
        let size = core::mem::size_of::<SharedMemory>();
        assert_eq!(m.calls.borrow()[0], Call::New(size));
    }
}

#[test]
fn init_writes_header_registers_everything_and_returns_ok() {
    let _l = LOCK.lock().unwrap();
    let m = mock(|_| {});
    reset_sdk(0);
    assert_eq!(unsafe { plugin::telemetry_init(0x10000, &params((1 << 16) | 0x2e), m) }, 0);
    let t = m.buf;
    unsafe {
        assert_eq!(rd32(t, off::TIME), 0);
        assert_eq!(rd32(t, off::PAUSED), 1);
        assert_eq!(rd32(t, off::REVISION), 5);
        assert_eq!(rd32(t, off::GAME_VERSION_MAJOR), 1);
        assert_eq!(rd32(t, off::GAME_VERSION_MINOR), 0x2e);
        assert_eq!(rd32(t, off::MODEL_OFFSET), 0x3c00);
        assert_eq!(rd32(t, off::MODEL_LENGTH), 0);
        assert_eq!(rd32(t, off::TRAILER_OFFSET), 0x3c40);
        assert_eq!(rd32(t, off::TRAILER_LENGTH), 0);
        let s = core::slice::from_raw_parts(t, MAP_SIZE);
        assert!(s[0x14..0x90].iter().all(|&b| b == 0) && s[0xa0..].iter().all(|&b| b == 0));
    }
    assert_eq!(events(), vec![1, 3, 4, 5]);
    let regs = SDK.with(|s| {
        s.borrow()
            .iter()
            .filter_map(|e| match e {
                Sdk::Channel { name, index, type_, flags, ctx } => Some((name.clone(), *index, *type_, *flags, *ctx)),
                _ => None,
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(regs.len(), 67);
    for (r, c) in regs.iter().zip(CHANNELS.iter()) {
        assert_eq!(
            (r.0.as_str(), r.1, r.2, r.3, r.4),
            (c.name.to_str().unwrap(), 0xffff_ffff, c.value_type, 2, t as usize + c.offset)
        );
    }
    unsafe { plugin::telemetry_shutdown(m) };
    assert_eq!(&m.calls.borrow()[m.calls.borrow().len() - 2..], &[Call::Unmap(t as usize), Call::Close(0x44)]);
    // Second shutdown and DllMain detach: nothing more (ok is cleared).
    unsafe {
        plugin::telemetry_shutdown(m);
        assert_eq!(plugin::dll_main(0, m), 1);
        assert_eq!(plugin::dll_main(1, m), 1);
    }
    assert_eq!(m.calls.borrow().iter().filter(|c| matches!(c, Call::Unmap(_))).count(), 1);
}

#[test]
fn refused_event_short_circuits_but_configuration_is_still_registered() {
    let _l = LOCK.lock().unwrap();
    for (refuse, expect) in [(1u32, vec![1u32, 5]), (3, vec![1, 3, 5]), (4, vec![1, 3, 4, 5]), (5, vec![1, 3, 4, 5])] {
        let m = mock(|_| {});
        reset_sdk(refuse);
        let r = unsafe { plugin::telemetry_init(0x10000, &params(0x10012), m) };
        assert_eq!(events(), expect, "refuse {refuse}");
        if refuse == 5 {
            // configuration's own result is ignored
            assert_eq!(r, 0);
            assert_eq!(channel_count(), 67);
        } else {
            assert_eq!(r, -7);
            assert_eq!(channel_count(), 0);
        }
    }
}

#[test]
fn dll_main_detach_closes_like_shutdown() {
    let _l = LOCK.lock().unwrap();
    let m = mock(|_| {});
    reset_sdk(0);
    assert_eq!(unsafe { plugin::telemetry_init(0x10000, &params(0x10012), m) }, 0);
    assert_eq!(unsafe { plugin::dll_main(2, m) }, 1);
    assert!(!m.calls.borrow().iter().any(|c| matches!(c, Call::Unmap(_))));
    assert_eq!(unsafe { plugin::dll_main(0, m) }, 1);
    assert_eq!(&m.calls.borrow()[m.calls.borrow().len() - 2..], &[Call::Unmap(m.buf as usize), Call::Close(0x44)]);
}

// ---- callbacks --------------------------------------------------------------

#[test]
fn strncpy_pads_and_does_not_terminate_long_strings() {
    let mut d = [0xffu8; 8];
    unsafe { plugin::strncpy(d.as_mut_ptr(), c"ab".as_ptr(), 6) };
    assert_eq!(d, [b'a', b'b', 0, 0, 0, 0, 0xff, 0xff]);
    unsafe { plugin::strncpy(d.as_mut_ptr(), c"abcdefghij".as_ptr(), 6) };
    assert_eq!(d, [b'a', b'b', b'c', b'd', b'e', b'f', 0xff, 0xff]);
}

#[test]
fn frame_start_time_and_job_state_machine() {
    let _l = LOCK.lock().unwrap();
    let m = mock(|_| {});
    reset_sdk(0);
    assert_eq!(unsafe { plugin::telemetry_init(0x10000, &params(0x10012), m) }, 0);
    let t = m.buf;
    let frame = |flags: u32, ts: u64| unsafe {
        let fs = ScsTelemetryFrameStart {
            flags,
            _padding: 0,
            render_time: 0,
            simulation_time: 0,
            paused_simulation_time: ts,
        };
        plugin::on_frame_start(1, &fs as *const _ as *const c_void, core::ptr::null_mut());
    };
    unsafe {
        frame(0, 500); // first frame: base = itself
        assert_eq!(rd32(t, off::TIME), 0);
        frame(0, 800);
        assert_eq!(rd32(t, off::TIME), 300);
        frame(1, 50); // timer restart: base = 0
        assert_eq!(rd32(t, off::TIME), 350);
        frame(0, 40); // going backwards wraps the 64-bit accumulator
        assert_eq!(rd32(t, off::TIME), 340);

        // cruise flag
        core::ptr::write_unaligned(t.add(off::CRUISE_CONTROL) as *mut f32, 1.0);
        frame(0, 40);
        assert_eq!(*t.add(off::CRUISE_CONTROL_ACTIVE), 1);

        // job: income + trailer -> on job; trailer off within 0.1 -> finished
        core::ptr::write_unaligned(t.add(off::JOB_INCOME) as *mut u32, 1);
        *t.add(off::TRAILER_CONNECTED) = 1;
        *t.add(off::SOURCE_CITY) = b'X';
        frame(0, 40);
        assert_eq!(*t.add(off::ON_JOB), 1);
        *t.add(off::TRAILER_CONNECTED) = 0;
        core::ptr::write_unaligned(t.add(off::NAVIGATION_DISTANCE) as *mut f32, 0.1);
        frame(0, 40);
        assert_eq!((*t.add(off::ON_JOB), *t.add(off::JOB_FINISHED)), (0, 1));
        for _ in 0..10 {
            frame(0, 40);
            assert_eq!(*t.add(off::SOURCE_CITY), b'X');
        }
        frame(0, 40);
        assert_eq!(*t.add(off::JOB_FINISHED), 0);
        assert_eq!(rd32(t, off::JOB_INCOME), 0);
        assert_eq!(*t.add(off::SOURCE_CITY), 0);
        plugin::telemetry_shutdown(m);
    }
}

#[test]
fn pause_events() {
    let _l = LOCK.lock().unwrap();
    let m = mock(|_| {});
    reset_sdk(0);
    assert_eq!(unsafe { plugin::telemetry_init(0x10000, &params(0x10012), m) }, 0);
    unsafe {
        plugin::on_pause_change(4, core::ptr::null(), core::ptr::null_mut());
        assert_eq!(rd32(m.buf, off::PAUSED), 0);
        plugin::on_pause_change(3, core::ptr::null(), core::ptr::null_mut());
        assert_eq!(rd32(m.buf, off::PAUSED), 1);
        plugin::telemetry_shutdown(m);
    }
}

#[test]
fn both_job_block_clears_cover_exactly_0xa8_to_0x233() {
    for clear in [plugin::clear_job_block_x64 as unsafe fn(*mut u8), plugin::clear_job_block_i386] {
        #[repr(align(16))]
        struct View([u8; 0x300]);
        let mut b = View([0xee; 0x300]);
        unsafe { clear(b.0.as_mut_ptr()) };
        for (i, &v) in b.0.iter().enumerate() {
            assert_eq!(v, if (0xa8..0x234).contains(&i) { 0 } else { 0xee }, "byte {i:#x}");
        }
    }
}

#[test]
fn layout_constants() {
    assert_eq!(layout::DELIVERY_DISTANCE.to_bits(), 0x3dcc_cccd);
    assert_eq!(off::JOB_BLOCK_END - off::CARGO_MASS, 0x18c);
}
