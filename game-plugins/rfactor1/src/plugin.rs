//! `SharedMemoryMapPlugin`: the callbacks that publish
//! rFactor's telemetry and scoring in `$rFactorShared$`.

use crate::crt::Clock;
use crate::layout::*;
use crate::libm;
use crate::os::{Handle, Os};
use crate::sse::*;

/// The 0x20-byte object (`operator new(0x20)`, only the vfptr is set).
#[repr(C)]
pub struct Plugin {
    pub vtbl: *const (),
    pub handle: Handle,      // +0x04
    pub view: *mut u8,       // +0x08
    pub mapped: u8,          // +0x0c
    pub _pad0: [u8; 3],
    pub last_clock: i32,     // +0x10 clock() of the previous UpdateTelemetry
    pub delta: u32,          // +0x14 f32 seconds since the previous UpdateTelemetry
    pub scoring_clock: i32,  // +0x18 clock() of the last UpdateScoring
    pub session: u8,         // +0x1c 1 after slot 22, 0 after slot 23 (never read)
    pub in_realtime: u8,     // +0x1d
    pub _pad1: [u8; 2],
}

#[cfg(target_arch = "x86")]
const _: () = assert!(core::mem::size_of::<Plugin>() == PLUGIN_SIZE);

#[inline(always)]
unsafe fn r32(p: *const u8, o: usize) -> u32 {
    core::ptr::read_unaligned(p.add(o) as *const u32)
}
#[inline(always)]
unsafe fn w32(p: *mut u8, o: usize, v: u32) {
    core::ptr::write_unaligned(p.add(o) as *mut u32, v)
}
#[inline(always)]
unsafe fn rf(p: *const u8, o: usize) -> f32 {
    f32::from_bits(r32(p, o))
}
#[inline(always)]
unsafe fn wf(p: *mut u8, o: usize, v: f32) {
    w32(p, o, v.to_bits())
}
#[inline(always)]
unsafe fn cp32(d: *mut u8, dofs: usize, s: *const u8, sofs: usize) {
    w32(d, dofs, r32(s, sofs))
}
#[inline(always)]
unsafe fn cp16(d: *mut u8, dofs: usize, s: *const u8, sofs: usize) {
    core::ptr::write_unaligned(d.add(dofs) as *mut u16, core::ptr::read_unaligned(s.add(sofs) as *const u16))
}
#[inline(always)]
unsafe fn cp8(d: *mut u8, dofs: usize, s: *const u8, sofs: usize) {
    *d.add(dofs) = *s.add(sofs)
}
/// Copies three floats: an
/// 8-byte store of the first two, then a dword store of the third.
/// The stores are one inline-asm block so that the mapping sees exactly
/// two writes (qword at +0, then dword at +8); a plain u64
/// `write_unaligned` is split by LLVM into dword stores in any order.
#[inline(always)]
unsafe fn cp3(d: *mut u8, dofs: usize, s: *const u8, sofs: usize) {
    #[cfg(target_arch = "x86")]
    core::arch::asm!(
        "movss {a}, dword ptr [{s}]",
        "movss {b}, dword ptr [{s}+4]",
        "movss {c}, dword ptr [{s}+8]",
        "unpcklps {a}, {b}",
        "movq qword ptr [{d}], {a}",
        "movss dword ptr [{d}+8], {c}",
        s = in(reg) s.add(sofs), d = in(reg) d.add(dofs),
        a = out(xmm_reg) _, b = out(xmm_reg) _, c = out(xmm_reg) _,
        options(nostack, preserves_flags));
    #[cfg(not(target_arch = "x86"))]
    st3(d, dofs, rf(s, sofs), rf(s, sofs + 4), rf(s, sofs + 8));
}
/// Stores three computed floats:
/// an 8-byte store of the first two, then a dword of the third.
#[inline(always)]
unsafe fn st3(d: *mut u8, dofs: usize, a: f32, b: f32, c: f32) {
    #[cfg(target_arch = "x86")]
    core::arch::asm!(
        "unpcklps {a}, {b}",
        "movq qword ptr [{d}], {a}",
        "movss dword ptr [{d}+8], {c}",
        d = in(reg) d.add(dofs), a = inout(xmm_reg) a => _, b = in(xmm_reg) b, c = in(xmm_reg) c,
        options(nostack, preserves_flags));
    #[cfg(not(target_arch = "x86"))]
    {
        let q = a.to_bits() as u64 | ((b.to_bits() as u64) << 32);
        core::ptr::write_unaligned(d.add(dofs) as *mut u64, q);
        wf(d, dofs + 8, c);
    }
}
/// The view pointer, re-read from the
/// object before (nearly) every store.
#[inline(always)]
unsafe fn view(p: *const Plugin) -> *mut u8 {
    core::ptr::read_volatile(core::ptr::addr_of!((*p).view))
}
/// `rep movsd` (ecx dwords, ascending). No `cld`: this relies on
/// DF = 0 (guaranteed by the ABI, and by Rust on asm entry).
#[inline(always)]
unsafe fn rep_movsd(d: *mut u8, s: *const u8, n: usize) {
    #[cfg(target_arch = "x86")]
    core::arch::asm!("push esi", "mov esi, {s}", "rep movsd", "pop esi", s = in(reg) s, inout("edi") d => _, inout("ecx") n => _);
    #[cfg(not(target_arch = "x86"))]
    for k in 0..n {
        cp32(d, 4 * k, s, 4 * k);
    }
}
/// An unbounded string copy.
#[inline(always)]
unsafe fn strcpy(d: *mut u8, s: *const u8) {
    let mut i = 0usize;
    loop {
        let c = core::ptr::read_volatile(s.add(i));
        core::ptr::write_volatile(d.add(i), c);
        i += 1;
        if c == 0 {
            break;
        }
    }
}
/// The run-time memset (rt.rs).
#[inline(always)]
unsafe fn memset0(d: *mut u8, n: usize) {
    #[cfg(all(windows, target_arch = "x86"))]
    crate::rt::rf1_memset(d, 0, n);
    #[cfg(not(all(windows, target_arch = "x86")))]
    core::ptr::write_bytes(d, 0, n);
}

/// The game's `VehicleScoringInfoV2 *` at ScoringInfoV2 +0x1e8 (a 32-bit
/// pointer; host tests store a full-width pointer there).
#[inline(always)]
unsafe fn scoring_vehicles(info: *const u8) -> *const u8 {
    #[cfg(target_pointer_width = "32")]
    {
        u32::from_le_bytes(core::ptr::read_volatile(info.add(SCORING_VEHICLES_PTR) as *const [u8; 4])) as usize as *const u8
    }
    #[cfg(not(target_pointer_width = "32"))]
    {
        usize::from_le_bytes(core::ptr::read_volatile(info.add(SCORING_VEHICLES_PTR) as *const [u8; core::mem::size_of::<usize>()])) as *const u8
    }
}

/// `f32(sqrt(f64((x*x + y*y) + z*z)))`, in that order.
#[inline(always)]
fn len3(x: f32, y: f32, z: f32) -> f32 {
    let s = addss(addss(mulss(x, x), mulss(y, y)), mulss(z, z));
    cvtsd2ss(f64::from_bits(libm::sqrt(cvtss2sd(s).to_bits())))
}
#[inline(always)]
fn fcos(a: f32) -> f32 {
    cvtsd2ss(f64::from_bits(libm::cos(cvtss2sd(a).to_bits())))
}
#[inline(always)]
fn fsin(a: f32) -> f32 {
    cvtsd2ss(f64::from_bits(libm::sin(cvtss2sd(a).to_bits())))
}

/// Double 0.1 and float 1000.0.
const TENTH: u64 = 0x3fb9_9999_9999_999a;
const THOUSAND: f32 = 1000.0;

/// Slot 6 (Startup): create or open and map `$rFactorShared$`.
pub unsafe fn startup(p: *mut Plugin, os: &dyn Os) {
    (*p).handle = usize::MAX as Handle;
    let h = os.create_file_mapping_a(usize::MAX as Handle, PAGE_READWRITE, 0, MAP_SIZE as u32, MAP_NAME.as_ptr());
    (*p).handle = h;
    if h.is_null() && os.get_last_error() == ERROR_ALREADY_EXISTS {
        let h = os.open_file_mapping_a(MAP_ACCESS, 0, MAP_NAME.as_ptr());
        (*p).handle = h;
        if h.is_null() {
            (*p).mapped = 0;
            return;
        }
    }
    let v = os.map_view_of_file((*p).handle, MAP_ACCESS, 0, 0, MAP_SIZE);
    (*p).view = v;
    if v.is_null() {
        os.close_handle((*p).handle);
        (*p).mapped = 0;
        return;
    }
    (*p).mapped = 1;
    memset0(v, MAP_SIZE);
}

/// Slot 7 (Shutdown). The handle and view fields are left as
/// they are.
pub unsafe fn shutdown(p: *mut Plugin, os: &dyn Os) {
    if (*p).mapped != 0 {
        memset0((*p).view, MAP_SIZE);
    }
    if !(*p).view.is_null() {
        os.unmap_view_of_file((*p).view);
    }
    if !(*p).handle.is_null() {
        os.close_handle((*p).handle);
    }
    (*p).mapped = 0;
}

/// Slot 22: the session start.
pub unsafe fn reset(p: *mut Plugin) {
    if (*p).mapped != 0 {
        memset0((*p).view, MAP_SIZE);
    }
    (*p).last_clock = 0;
    (*p).scoring_clock = 0;
    (*p).delta = 0;
    (*p).session = 1;
}

/// Slot 24 (UpdateTelemetry, TelemInfoV2).
pub unsafe fn update_telemetry(p: *mut Plugin, os: &dyn Os, clk: &Clock, t: *const u8) {
    if (*p).mapped == 0 {
        return;
    }
    if (*p).last_clock == 0 {
        (*p).last_clock = clk.clock(os);
    }
    let c = clk.clock(os);
    (*p).delta = divss(cvtdq2ps(c.wrapping_sub((*p).last_clock)), THOUSAND).to_bits();
    (*p).last_clock = clk.clock(os);
    let c = clk.clock(os);
    let v = || view(p);
    wf(v(), T_DELTA_SINCE_SCORING, divss(cvtdq2ps(c.wrapping_sub((*p).scoring_clock)), THOUSAND));
    cp32(v(), 0x4, t, 0x4);
    cp32(v(), 0x8, t, 0x8);
    strcpy(v().add(0xc), t.add(0xc));
    strcpy(v().add(0x4c), t.add(0x4c));
    // eight three-float groups 0x8c..0xeb: a qword store, then a dword store
    for o in (0x8c..0xec).step_by(12) {
        cp3(v(), o, t, o);
    }
    wf(v(), 0xec, len3(rf(t, 0x98), rf(t, 0x9c), rf(t, 0xa0)));
    for k in 0..12 {
        cp32(v(), 0xf0 + 4 * k, t, 0xec + 4 * k);
    }
    for k in 0..11 {
        cp8(v(), 0x120 + k, t, 0x11c + k);
    }
    cp32(v(), 0x12b, t, 0x128);
    cp32(v(), 0x12f, t, 0x12c);
    cp3(v(), 0x133, t, 0x130);
    for k in 0..4 {
        cp8(v(), 0x13f + k, t, 0x13c + k);
    }
    cp32(v(), 0x143, t, 0x140);
    cp8(v(), 0x147, t, 0x148);
    for w in 0..4 {
        let d = || v().add(T_WHEELS + w * T_WHEEL_STRIDE);
        let s = t.add(TELEM_WHEELS + w * TELEM_WHEEL_STRIDE);
        for k in 0..12 {
            cp32(d(), 4 * k, s, 4 * k);
        }
        strcpy(d().add(0x30), s.add(0x30));
        cp8(d(), 0x40, s, 0x40);
        cp8(d(), 0x41, s, 0x41);
        cp8(d(), 0x42, s, 0x42);
    }
    wf(v(), S_ET, addss(f32::from_bits((*p).delta), rf(v(), S_ET)));
    *v().add(S_IN_REALTIME) = (*p).in_realtime;
    let mut i: i32 = 0;
    if (r32(v(), S_NUM_VEHICLES) as i32) <= 0 {
        return;
    }
    loop {
        extrapolate(p, i as usize * S_VEHICLE_STRIDE);
        i += 1;
        if i >= r32(v(), S_NUM_VEHICLES) as i32 {
            break;
        }
    }
}

/// One extrapolation step on the mapped vehicle at view +
/// `off` (`off` = i * 0x130; the offsets are
/// relative to the view). The view pointer is re-read from the object for
/// every group and once per orientation row.
#[inline(always)]
unsafe fn extrapolate(p: *mut Plugin, off: usize) {
    let dt = || f32::from_bits(core::ptr::read_volatile(core::ptr::addr_of!((*p).delta)));
    let base = || view(p).add(off);
    // f32(f64(src * dt) * 0.1 + f64(dst))
    for (dst, src) in [(0x410, 0x41c), (0x414, 0x420), (0x418, 0x424), (0x3d4, 0x3e0), (0x3d8, 0x3e4), (0x3dc, 0x3e8)] {
        let b = base();
        let m = cvtss2sd(mulss(rf(b, src), dt()));
        wf(b, dst, cvtsd2ss(addsd(mulsd(m, f64::from_bits(TENTH)), cvtss2sd(rf(b, dst)))));
    }
    // position += (orientation rows . velocity) * dt
    let b = base();
    let x = addss(addss(mulss(rf(b, 0x3f0), rf(b, 0x3d8)), mulss(rf(b, 0x3ec), rf(b, 0x3d4))), mulss(rf(b, 0x3f4), rf(b, 0x3dc)));
    wf(b, 0x3c8, addss(mulss(x, dt()), rf(b, 0x3c8)));
    let b = base();
    let y = addss(addss(mulss(rf(b, 0x3fc), rf(b, 0x3d8)), mulss(rf(b, 0x3f8), rf(b, 0x3d4))), mulss(rf(b, 0x400), rf(b, 0x3dc)));
    wf(b, 0x3cc, addss(mulss(y, dt()), rf(b, 0x3cc)));
    let b = base();
    let z = addss(addss(mulss(rf(b, 0x408), rf(b, 0x3d8)), mulss(rf(b, 0x404), rf(b, 0x3d4))), mulss(rf(b, 0x40c), rf(b, 0x3dc)));
    wf(b, 0x3d0, addss(mulss(z, dt()), rf(b, 0x3d0)));
    // The rotation rate times a zero-initialised local matrix:
    // only NaN / infinity survive.
    let b = base();
    let zero = 0.0f32;
    let r0 = rf(b, 0x410);
    let r1 = rf(b, 0x414);
    let r2 = rf(b, 0x418);
    let s14 = addss(addss(mulss(r0, zero), mulss(zero, r1)), mulss(zero, r2));
    let s10 = addss(addss(mulss(zero, r1), mulss(zero, r0)), mulss(zero, r2));
    let s18 = addss(addss(mulss(zero, r1), mulss(zero, r0)), mulss(zero, r2));
    rotate_row(b, 0x3ec, s18, s10, s14, &dt);
    rotate_row(base(), 0x3f8, s18, s10, s14, &dt);
    rotate_row(base(), 0x404, s18, s10, s14, &dt);
    let b = base();
    wf(b, 0x428, len3(rf(b, 0x3d4), rf(b, 0x3d8), rf(b, 0x3dc)));
    let b = base();
    wf(b, 0x360, subss(rf(b, 0x360), mulss(rf(b, 0x3dc), dt())));
}

/// Rotates one orientation row by the three angles and renormalises it
/// (the three rows get the same operations in the same order).
#[inline(always)]
unsafe fn rotate_row(b: *mut u8, row: usize, s18: f32, s10: f32, s14: f32, dt: &dyn Fn() -> f32) {
    let a = mulss(s18, dt());
    let c = fcos(a);
    let s = fsin(a);
    let x = rf(b, row);
    let y = rf(b, row + 4);
    let nx = subss(mulss(x, c), mulss(y, s));
    let ny = addss(mulss(x, s), mulss(y, c));
    let a2 = mulss(s10, dt());
    let s2 = fsin(a2);
    let c2 = fcos(a2);
    let z = rf(b, row + 8);
    let pv = addss(mulss(z, s2), mulss(nx, c2));
    let a3 = mulss(s14, dt());
    let q = subss(mulss(z, c2), mulss(nx, s2));
    let c3 = fcos(a3);
    let s3 = fsin(a3);
    let r = subss(mulss(ny, c3), mulss(q, s3));
    let tv = addss(mulss(ny, s3), mulss(q, c3));
    let l2 = addss(addss(mulss(r, r), mulss(pv, pv)), mulss(tv, tv));
    let l = cvtsd2ss(f64::from_bits(libm::sqrt(cvtss2sd(l2).to_bits())));
    // all three divisions, then a qword store at row and a dword store at row+8
    let (dx, dy, dz) = (divss(pv, l), divss(r, l), divss(tv, l));
    st3(b, row, dx, dy, dz);
}

/// Slot 26 (UpdateScoring, ScoringInfoV2).
pub unsafe fn update_scoring(p: *mut Plugin, os: &dyn Os, clk: &Clock, s: *const u8) {
    if (*p).mapped == 0 {
        return;
    }
    (*p).scoring_clock = clk.clock(os);
    let v = || view(p);
    cp32(v(), 0x254, s, 0x40);
    cp32(v(), 0x258, s, 0x44);
    cp32(v(), 0x25c, s, 0x48);
    cp32(v(), 0x260, s, 0x4c);
    cp32(v(), 0x264, s, 0x50);
    cp32(v(), 0x268, s, 0x58);
    for k in 0..7 {
        cp8(v(), 0x26c + k, s, 0x5c + k);
    }
    *v().add(S_IN_REALTIME) = (*p).in_realtime;
    strcpy(v().add(0x274), s.add(0x64));
    strcpy(v().add(0x294), s.add(0x84));
    cp32(v(), 0x2d4, s, 0xcc);
    cp32(v(), 0x2d8, s, 0xd0);
    cp3(v(), 0x2dc, s, 0xd4);
    cp32(v(), 0x2e8, s, 0xc4);
    cp32(v(), 0x2ec, s, 0xc8);
    cp32(v(), 0x2f0, s, 0xe0);
    cp32(v(), 0x2f4, s, 0xe4);
    cp32(v(), 0x2f8, s, 0x120);
    // one memset of the 0x130-byte local to 0, then a copy of
    // 0x4c dwords local -> view + 0x9afc
    let mut local = core::mem::MaybeUninit::<[u8; S_PAST_END_LEN]>::uninit();
    let local = local.as_mut_ptr() as *mut u8;
    memset0(local, S_PAST_END_LEN);
    rep_movsd(v().add(S_PAST_END), local, S_PAST_END_LEN / 4);
    if (r32(s, SCORING_NUM_VEHICLES) as i32) <= 0 {
        return;
    }
    // Every field group re-reads the VehicleScoringInfoV2 pointer at
    // info + 0x1e8.
    let mut i: i32 = 0;
    loop {
        let o = i as usize * SCORING_VEHICLE_STRIDE;
        let veh = || scoring_vehicles(s).add(o);
        let d = || v().add(S_VEHICLES + i as usize * S_VEHICLE_STRIDE);
        strcpy(d(), veh());
        strcpy(d().add(0x20), veh().add(0x20));
        cp16(d(), 0x60, veh(), 0x60);
        cp8(d(), 0x62, veh(), 0x62);
        cp8(d(), 0x63, veh(), 0x63);
        for f in (0x64..=0x8c).step_by(4) {
            cp32(d(), f, veh(), f);
        }
        cp16(d(), 0x90, veh(), 0x90);
        cp16(d(), 0x92, veh(), 0x92);
        for f in 0x94..0x98 {
            cp8(d(), f, veh(), f);
        }
        strcpy(d().add(0x98), veh().add(0x98));
        for f in (0xb8..=0xc8).step_by(4) {
            cp32(d(), f, veh(), f);
        }
        for f in (0xcc..0x12c).step_by(12) {
            cp3(d(), f, veh(), f);
        }
        let src = veh();
        wf(d(), 0x12c, len3(rf(src, 0xd8), rf(src, 0xdc), rf(src, 0xe0)));
        i += 1;
        if i >= r32(s, SCORING_NUM_VEHICLES) as i32 {
            break;
        }
    }
}
