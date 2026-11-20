use crate::crt::{scale_count, Clock};
use crate::layout::*;
use crate::libm;
use crate::os::{Handle, Os};
use crate::plugin::{self, Plugin};
use std::cell::{Cell, RefCell};

/// Scripted KERNEL32: one mapping backed by a page-rounded buffer.
struct Fake {
    mem: RefCell<Vec<u8>>,
    log: RefCell<Vec<String>>,
    create_null: Cell<bool>,
    last_error: Cell<u32>,
    open_null: Cell<bool>,
    map_null: Cell<bool>,
    qpc: Cell<i64>,
}

impl Fake {
    fn new() -> Fake {
        Fake {
            mem: RefCell::new(vec![0xcc; 0xa000]),
            log: RefCell::new(vec![]),
            create_null: Cell::new(false),
            last_error: Cell::new(0),
            open_null: Cell::new(false),
            map_null: Cell::new(false),
            qpc: Cell::new(0),
        }
    }
    fn view(&self) -> *mut u8 {
        self.mem.borrow_mut().as_mut_ptr()
    }
}

fn cstr(p: *const u8) -> String {
    unsafe { std::ffi::CStr::from_ptr(p as *const _).to_string_lossy().into_owned() }
}

impl Os for Fake {
    fn create_file_mapping_a(&self, f: Handle, prot: u32, hi: u32, lo: u32, name: *const u8) -> Handle {
        self.log.borrow_mut().push(format!("create {:x} {} {} {:x} {}", f as usize, prot, hi, lo, cstr(name)));
        if self.create_null.get() {
            std::ptr::null_mut()
        } else {
            0x44 as Handle
        }
    }
    fn get_last_error(&self) -> u32 {
        self.log.borrow_mut().push("gle".into());
        self.last_error.get()
    }
    fn open_file_mapping_a(&self, a: u32, i: i32, name: *const u8) -> Handle {
        self.log.borrow_mut().push(format!("open {:x} {} {}", a, i, cstr(name)));
        if self.open_null.get() {
            std::ptr::null_mut()
        } else {
            0x48 as Handle
        }
    }
    fn map_view_of_file(&self, h: Handle, a: u32, hi: u32, lo: u32, n: usize) -> *mut u8 {
        self.log.borrow_mut().push(format!("map {:x} {:x} {} {} {:x}", h as usize, a, hi, lo, n));
        if self.map_null.get() {
            std::ptr::null_mut()
        } else {
            self.view()
        }
    }
    fn unmap_view_of_file(&self, _p: *const u8) -> i32 {
        self.log.borrow_mut().push("unmap".into());
        1
    }
    fn close_handle(&self, h: Handle) -> i32 {
        self.log.borrow_mut().push(format!("close {:x}", h as usize));
        1
    }
    fn qpc(&self) -> Option<i64> {
        Some(self.qpc.get())
    }
    fn qpf(&self) -> Option<i64> {
        Some(10_000_000)
    }
}

fn new_plugin() -> Box<Plugin> {
    // operator new leaves everything but the vfptr uninitialised.
    let mut p: Box<Plugin> = Box::new(unsafe { std::mem::zeroed() });
    p.mapped = 0xcc;
    p
}

#[test]
fn startup_creates_maps_and_clears() {
    let os = Fake::new();
    let mut p = new_plugin();
    unsafe { plugin::startup(&mut *p, &os) };
    assert_eq!(*os.log.borrow(), vec!["create ffffffffffffffff 4 0 9afc $rFactorShared$", "map 44 f001f 0 0 9afc"]);
    assert_eq!(p.mapped, 1);
    let m = os.mem.borrow();
    assert!(m[..MAP_SIZE].iter().all(|&b| b == 0));
    assert!(m[MAP_SIZE..].iter().all(|&b| b == 0xcc));
}

#[test]
fn startup_failure_paths() {
    // create fails with ERROR_ALREADY_EXISTS -> open
    let os = Fake::new();
    os.create_null.set(true);
    os.last_error.set(0xb7);
    let mut p = new_plugin();
    unsafe { plugin::startup(&mut *p, &os) };
    assert_eq!(os.log.borrow()[1..], ["gle", "open f001f 0 $rFactorShared$", "map 48 f001f 0 0 9afc"]);
    assert_eq!(p.mapped, 1);
    // open fails -> not mapped, nothing else
    let os = Fake::new();
    os.create_null.set(true);
    os.last_error.set(0xb7);
    os.open_null.set(true);
    let mut p = new_plugin();
    unsafe { plugin::startup(&mut *p, &os) };
    assert_eq!(os.log.borrow().len(), 3);
    assert_eq!(p.mapped, 0);
    // create fails with another error -> MapViewOfFile(NULL) anyway
    let os = Fake::new();
    os.create_null.set(true);
    os.last_error.set(5);
    os.map_null.set(true);
    let mut p = new_plugin();
    unsafe { plugin::startup(&mut *p, &os) };
    assert_eq!(os.log.borrow()[1..], ["gle", "map 0 f001f 0 0 9afc", "close 0"]);
    assert_eq!(p.mapped, 0);
}

#[test]
fn shutdown_clears_unmaps_closes_and_keeps_fields() {
    let os = Fake::new();
    let mut p = new_plugin();
    unsafe { plugin::startup(&mut *p, &os) };
    os.mem.borrow_mut()[5] = 7;
    os.log.borrow_mut().clear();
    unsafe { plugin::shutdown(&mut *p, &os) };
    assert_eq!(*os.log.borrow(), vec!["unmap", "close 44"]);
    assert_eq!(os.mem.borrow()[5], 0);
    assert_eq!(p.mapped, 0);
    assert_eq!(p.handle as usize, 0x44);
    os.log.borrow_mut().clear();
    os.mem.borrow_mut()[5] = 7;
    unsafe { plugin::shutdown(&mut *p, &os) };
    assert_eq!(*os.log.borrow(), vec!["unmap", "close 44"]);
    assert_eq!(os.mem.borrow()[5], 7);
}

#[test]
fn reset_slot22() {
    let os = Fake::new();
    let mut p = new_plugin();
    unsafe { plugin::startup(&mut *p, &os) };
    p.last_clock = 5;
    p.scoring_clock = 6;
    p.delta = 7;
    os.mem.borrow_mut()[9] = 1;
    unsafe { plugin::reset(&mut *p) };
    assert_eq!((p.last_clock, p.scoring_clock, p.delta, p.session), (0, 0, 0, 1));
    assert_eq!(os.mem.borrow()[9], 0);
}

#[test]
fn clock_matches_ucrt() {
    let os = Fake::new();
    os.qpc.set(1_000);
    let c = Clock::init(&os);
    assert_eq!(c, Clock { freq: 10_000_000, start: 1_000 });
    os.qpc.set(1_000 + 12_345_678);
    assert_eq!(c.clock(&os), 1234);
    os.qpc.set(999);
    assert_eq!(c.clock(&os), -1);
    assert_eq!(Clock::UNSET.clock(&os), -1);
    assert_eq!(scale_count(25_000_000, 10_000_000), 2500);
    os.qpc.set(1_000 + 10_000_000 * 0x8000_0000 / 1000);
    assert_eq!(c.clock(&os), -1);
}

fn ramp(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) | 1).collect()
}

fn put_str(b: &mut [u8], o: usize, s: &[u8]) {
    b[o..o + s.len()].copy_from_slice(s);
    b[o + s.len()] = 0;
}

#[test]
fn telemetry_copies_fields() {
    let os = Fake::new();
    os.qpc.set(0);
    let clk = Clock::init(&os);
    let mut p = new_plugin();
    unsafe { plugin::startup(&mut *p, &os) };
    let mut t = ramp(0x400, 3);
    put_str(&mut t, 0xc, b"car");
    put_str(&mut t, 0x4c, b"track");
    for w in 0..4 {
        put_str(&mut t, 0x17c + w * 0x64 + 0x30, b"tc");
    }
    for (o, v) in [(0x98, 3.0f32), (0x9c, 4.0), (0xa0, 12.0)] {
        t[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    p.scoring_clock = 0;
    os.qpc.set(20_000);
    unsafe { plugin::update_telemetry(&mut *p, &os, &clk, t.as_ptr()) };
    let m = os.mem.borrow();
    let f = |o: usize| f32::from_le_bytes(m[o..o + 4].try_into().unwrap());
    assert_eq!(f(0), 0.002);
    assert_eq!(p.last_clock, 2);
    assert_eq!(f32::from_bits(p.delta), 0.0);
    assert_eq!(m[4..12], t[4..12]);
    assert_eq!(&m[0xc..0x10], b"car\0");
    assert_eq!(&m[0x4c..0x52], b"track\0");
    assert_eq!(m[0x8c..0xec], t[0x8c..0xec]);
    assert_eq!(f(0xec), 13.0);
    assert_eq!(m[0xf0..0x120], t[0xec..0x11c]);
    assert_eq!(m[0x120..0x12b], t[0x11c..0x127]);
    assert_eq!(m[0x12b..0x13f], t[0x128..0x13c]);
    assert_eq!(m[0x13f..0x143], t[0x13c..0x140]);
    assert_eq!(m[0x143..0x147], t[0x140..0x144]);
    assert_eq!(m[0x147], t[0x148]);
    for w in 0..4 {
        let d = 0x148 + w * 0x43;
        let s = 0x17c + w * 0x64;
        assert_eq!(m[d..d + 0x30], t[s..s + 0x30]);
        assert_eq!(&m[d + 0x30..d + 0x33], b"tc\0");
        assert_eq!(m[d + 0x40..d + 0x43], t[s + 0x40..s + 0x43]);
    }
    assert_eq!(m[0x273], p.in_realtime);
}

#[test]
fn scoring_copies_and_extrapolation() {
    let os = Fake::new();
    let clk = Clock::init(&os);
    let mut p = new_plugin();
    unsafe { plugin::startup(&mut *p, &os) };
    p.in_realtime = 1;
    let mut veh = ramp(2 * 0x1ac, 9);
    for i in 0..2 {
        put_str(&mut veh, i * 0x1ac, b"drv");
        put_str(&mut veh, i * 0x1ac + 0x20, b"cls");
        put_str(&mut veh, i * 0x1ac + 0x98, b"pit");
        for (o, v) in [(0xd8, 1.0f32), (0xdc, 2.0), (0xe0, 2.0)] {
            veh[i * 0x1ac + o..i * 0x1ac + o + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    let mut s = ramp(0x200, 5);
    put_str(&mut s, 0x64, b"trk");
    put_str(&mut s, 0x84, b"tr2");
    s[0x58..0x5c].copy_from_slice(&2i32.to_le_bytes());
    s[0x1e8..0x1f0].copy_from_slice(&(veh.as_ptr() as usize).to_le_bytes());
    os.qpc.set(10_000);
    unsafe { plugin::update_scoring(&mut *p, &os, &clk, s.as_ptr()) };
    assert_eq!(p.scoring_clock, 1);
    {
        let m = os.mem.borrow();
        assert_eq!(m[0x254..0x264], s[0x40..0x50]);
        assert_eq!(m[0x264..0x268], s[0x50..0x54]);
        assert_eq!(m[0x268..0x26c], 2i32.to_le_bytes());
        assert_eq!(m[0x26c..0x273], s[0x5c..0x63]);
        assert_eq!(m[0x273], 1);
        assert_eq!(&m[0x274..0x278], b"trk\0");
        assert_eq!(&m[0x294..0x298], b"tr2\0");
        assert_eq!(m[0x2d4..0x2dc], s[0xcc..0xd4]);
        assert_eq!(m[0x2dc..0x2e8], s[0xd4..0xe0]);
        assert_eq!(m[0x2e8..0x2f0], s[0xc4..0xcc]);
        assert_eq!(m[0x2f0..0x2f8], s[0xe0..0xe8]);
        assert_eq!(m[0x2f8..0x2fc], s[0x120..0x124]);
        assert!(m[0x9afc..0x9c2c].iter().all(|&b| b == 0));
        assert!(m[0x9c2c..].iter().all(|&b| b == 0xcc));
        for i in 0..2 {
            let d = 0x2fc + i * 0x130;
            let v = &veh[i * 0x1ac..];
            assert_eq!(&m[d..d + 4], b"drv\0");
            assert_eq!(&m[d + 0x20..d + 0x24], b"cls\0");
            assert_eq!(m[d + 0x60..d + 0x98], v[0x60..0x98]);
            assert_eq!(&m[d + 0x98..d + 0x9c], b"pit\0");
            assert_eq!(m[d + 0xb8..d + 0x12c], v[0xb8..0x12c]);
            assert_eq!(f32::from_le_bytes(m[d + 0x12c..d + 0x130].try_into().unwrap()), 3.0);
        }
        assert!(m[0x2fc + 2 * 0x130..0x9afc].iter().all(|&b| b == 0));
    }
    // Extrapolation with dt = 0.5 s on vehicle 0: set up an identity
    // orientation, a velocity and NaN-free rotation.
    {
        let mut m = os.mem.borrow_mut();
        let mut put = |o: usize, v: f32| m[o..o + 4].copy_from_slice(&v.to_le_bytes());
        for (o, v) in [
            (0x3c8, 1.0f32),
            (0x3cc, 2.0),
            (0x3d0, 3.0),
            (0x3d4, 4.0),
            (0x3d8, 0.0),
            (0x3dc, 3.0),
            (0x3e0, 2.0),
            (0x3e4, 0.0),
            (0x3e8, 0.0),
            (0x3ec, 1.0),
            (0x3f0, 0.0),
            (0x3f4, 0.0),
            (0x3f8, 0.0),
            (0x3fc, 1.0),
            (0x400, 0.0),
            (0x404, 0.0),
            (0x408, 0.0),
            (0x40c, 1.0),
            (0x410, 0.25),
            (0x414, 0.0),
            (0x418, 0.0),
            (0x41c, 0.0),
            (0x420, 0.0),
            (0x424, 0.0),
            (0x360, 10.0),
        ] {
            put(o, v);
        }
        m[0x268..0x26c].copy_from_slice(&1i32.to_le_bytes());
    }
    p.last_clock = 1;
    os.qpc.set(15_000); // clock() = 1 -> 1 -> ... first call
    let t = vec![0u8; 0x400];
    // last_clock 1 and clock() 1 -> dt 0: only copies
    unsafe { plugin::update_telemetry(&mut *p, &os, &clk, t.as_ptr()) };
    p.last_clock = 1;
    os.qpc.set(5_010_000); // clock() = 501 -> dt = 0.5
    unsafe { plugin::update_telemetry(&mut *p, &os, &clk, t.as_ptr()) };
    let m = os.mem.borrow();
    let f = |o: usize| f32::from_le_bytes(m[o..o + 4].try_into().unwrap());
    assert_eq!(f32::from_bits(p.delta), 0.5);
    // velocity += accel * dt * 0.1
    assert_eq!(f(0x3d4), (4.0f64 + 2.0 * 0.5 * 0.1) as f32);
    // position += R.v * dt  (identity rows: (0*0 + 1*v.x) + 0*v.z)
    assert_eq!(f(0x3c8), 1.0 + f(0x3d4) * 0.5);
    assert_eq!(f(0x3d0), 3.0 + 3.0 * 0.5);
    // zero matrix: angles 0 -> rows unchanged (normalised)
    assert_eq!((f(0x3ec), f(0x3f0), f(0x3f4)), (1.0, 0.0, 0.0));
    assert_eq!((f(0x3f8), f(0x3fc), f(0x400)), (0.0, 1.0, 0.0));
    assert_eq!(f(0x428), (f(0x3d4) * f(0x3d4) + 9.0).sqrt());
    assert_eq!(f(0x360), 10.0 - 3.0 * 0.5);
}

#[test]
fn libm_close_to_reference() {
    let mut x = -40.0f64;
    while x < 40.0 {
        for (got, want) in [(libm::cos(x.to_bits()), x.cos()), (libm::sin(x.to_bits()), x.sin())] {
            let g = f64::from_bits(got);
            assert!(
                (g - want).abs() <= 2.0 * f64::EPSILON * want.abs().max(1e-300) + 1e-300 || (g - want).abs() < 1e-16,
                "x={x} got {g} want {want}"
            );
        }
        assert_eq!(f64::from_bits(libm::sqrt((x * x).to_bits())), x.abs());
        x += 0.0137;
    }
}

#[test]
fn libm_special_cases() {
    // tiny arguments
    let t = 1e-80f64; // below 2^-252: the small-argument branches
    assert_eq!(libm::sin(t.to_bits()), t.to_bits());
    assert_eq!(f64::from_bits(libm::cos(t.to_bits())), 1.0);
    assert_eq!(libm::sin(1.0e-310f64.to_bits()), 1.0e-310f64.to_bits());
    assert_eq!(libm::sin(0), 0);
    assert_eq!(libm::sin(0x8000_0000_0000_0000), 0x8000_0000_0000_0000);
    // sqrt: -0, negative, NaN, inf
    assert_eq!(libm::sqrt(0x8000_0000_0000_0000), 0x8000_0000_0000_0000);
    assert_eq!(libm::sqrt((-4.0f64).to_bits()), 0xfff8_0000_0000_0000);
    assert_eq!(libm::sqrt(0x7ff0_0000_0000_0001), 0x7ff8_0000_0000_0001);
    assert_eq!(libm::sqrt(f64::INFINITY.to_bits()), f64::INFINITY.to_bits());
    assert_eq!(libm::sqrt(f64::NEG_INFINITY.to_bits()), 0xfff8_0000_0000_0000);
    // x87 path: infinities -> indefinite, NaN quieted, huge args reduced
    assert_eq!(libm::cos(f64::INFINITY.to_bits()), 0xfff8_0000_0000_0000);
    assert_eq!(libm::sin(f64::NEG_INFINITY.to_bits()), 0xfff8_0000_0000_0000);
    assert_eq!(libm::sin(0x7ff0_0000_0000_0001), 0x7ff8_0000_0000_0001);
    let big = 1e300f64;
    let r = f64::from_bits(libm::sin(big.to_bits()));
    assert!(r.abs() <= 1.0); // fprem1 by an 80-bit constant, not an exact reduction
}

fn x87_sw_after(f: impl FnOnce()) -> u16 {
    let sw: u16;
    unsafe { core::arch::asm!("fnclex", options(nostack, nomem)) };
    f();
    unsafe { core::arch::asm!("fnstsw ax", out("ax") sw, options(nostack, nomem)) };
    sw
}

/// The error handling of the math routines: a masked DOMAIN error
/// (x87 fallback cos/sin of +-inf, x87 sqrt of -inf) leaves IE set in the x87
/// status word.
#[test]
fn x87_domain_error_sets_ie() {
    let sw = x87_sw_after(|| assert_eq!(libm::cos(f64::INFINITY.to_bits()), 0xfff8_0000_0000_0000));
    assert_eq!(sw & 1, 1, "cos(inf) sw {sw:#x}");
    let sw = x87_sw_after(|| assert_eq!(libm::sin(f64::NEG_INFINITY.to_bits()), 0xfff8_0000_0000_0000));
    assert_eq!(sw & 1, 1, "sin(-inf) sw {sw:#x}");
    // sqrt takes the x87 routine only outside the default FP environment
    // (the SSE2 path returns the indefinite without error handling):
    // MXCSR RC = down.
    let mut m: u32 = 0;
    unsafe { core::arch::asm!("stmxcsr [{p}]", p = in(reg) &mut m as *mut u32, options(nostack)) };
    let down = (m & !0x6000) | 0x2000;
    let sw = x87_sw_after(|| {
        unsafe { core::arch::asm!("ldmxcsr [{p}]", p = in(reg) &down as *const u32, options(nostack)) };
        let r = libm::sqrt(f64::NEG_INFINITY.to_bits());
        unsafe { core::arch::asm!("ldmxcsr [{p}]", p = in(reg) &m as *const u32, options(nostack)) };
        assert_eq!(r, 0xfff8_0000_0000_0000);
    });
    assert_eq!(sw & 1, 1, "x87 sqrt(-inf) sw {sw:#x}");
    let sw = x87_sw_after(|| assert_eq!(libm::sqrt(f64::NEG_INFINITY.to_bits()), 0xfff8_0000_0000_0000));
    assert_eq!(sw & 1, 0, "SSE2 sqrt(-inf) sw {sw:#x}");
    // a NaN argument goes through no error handling
    let sw = x87_sw_after(|| assert_eq!(libm::cos(0x7ff8_0000_0000_0001), 0x7ff8_0000_0000_0001));
    assert_eq!(sw & 1, 0, "cos(qNaN) sw {sw:#x}");
}
