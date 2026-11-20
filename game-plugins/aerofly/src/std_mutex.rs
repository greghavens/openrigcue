//! The plugin's static mutex (a plain mutex, type 2, with a static
//! initialiser and an exit handler). Nothing in the DLL ever locks it; what
//! is observable is
//! the construction (which sync API it picks, the calls it makes) and the
//! destruction at unload.
//!
//! Layout (0x50 bytes): +0 `int type`, +8 the
//! critical-section object (vptr at +8, its lock at +0x10),
//! +0x48 `long thread_id` = -1, +0x4c `int count` = 0.
//!
//! The critical-section object is chosen by what start-up resolved:
//! * TryAcquireSRWLockExclusive resolved -> win7:
//!   `InitializeSRWLock(+0x10)`; destroy (slot 4) is empty.
//! * else InitializeConditionVariable resolved -> vista:
//!   `InitializeCriticalSectionEx(+0x10, 4000, 0)` or, when that
//!   pointer is NULL, `InitializeCriticalSectionAndSpinCount(+0x10, 4000)`;
//!   destroy = `DeleteCriticalSection(+0x10)`.
//! * else ConcRT: a lock object
//!   at +0x10 (qwords +0,+8,+0x18,+0x28,+0x30 = 0, dword +0x10 = 2,
//!   dwords +0x20,+0x24 = 0); destroy is empty.
//!
//! The mutex destructor calls vtable slot 4.

/// The plain mutex type.
pub const MTX_PLAIN: u32 = 2;
/// spin count of the vista path (0xfa0)
pub const VISTA_SPIN_COUNT: u32 = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Win7,
    /// `ex` = InitializeCriticalSectionEx is available
    Vista {
        ex: bool,
    },
    ConcRT,
}

/// Which critical-section kind the resolved functions select.
pub const fn select(try_acquire_srw: bool, init_condition_variable: bool, init_cs_ex: bool) -> Kind {
    if try_acquire_srw {
        Kind::Win7
    } else if init_condition_variable {
        Kind::Vista { ex: init_cs_ex }
    } else {
        Kind::ConcRT
    }
}

#[cfg(windows)]
pub use self::win::{construct, destroy};

#[cfg(windows)]
mod win {
    use super::*;
    use crate::crt::{
        DeleteCriticalSection, InitializeCriticalSectionAndSpinCount, InitializeCriticalSectionExFn,
        InitializeSRWLockFn,
    };
    use core::ffi::c_void;

    type Slot = unsafe extern "C" fn(*mut u8);

    /// lock / try_lock / try_lock_for / unlock: never called by anything in
    /// the DLL (the mutex is not used).
    unsafe extern "C" fn not_used(_cs: *mut u8) {
        core::arch::asm!("ud2", options(noreturn));
    }
    unsafe extern "C" fn destroy_nothing(_cs: *mut u8) {}
    unsafe extern "C" fn destroy_vista(cs: *mut u8) {
        DeleteCriticalSection(cs.add(8) as *mut c_void);
    }

    static VT_WIN7: [Slot; 5] = [not_used, not_used, not_used, not_used, destroy_nothing];
    static VT_VISTA: [Slot; 5] = [not_used, not_used, not_used, not_used, destroy_vista];
    static VT_CONCRT: [Slot; 5] = [not_used, not_used, not_used, not_used, destroy_nothing];

    #[repr(C, align(8))]
    pub struct Mutex([u8; 0x50]);

    /// The static mutex.
    pub static mut MUTEX: Mutex = Mutex([0u8; 0x50]);

    unsafe fn base() -> *mut u8 {
        core::ptr::addr_of_mut!(MUTEX) as *mut u8
    }

    /// Constructs the mutex in place.
    pub unsafe fn construct() {
        let m = base();
        let cs = m.add(8);
        let api = crate::crt::sync_api();
        let kind = select(
            !api.try_acquire_srw_lock_exclusive.is_null(),
            !api.initialize_condition_variable.is_null(),
            !api.initialize_critical_section_ex.is_null(),
        );
        let lock = cs.add(8) as *mut c_void;
        match kind {
            Kind::Win7 => {
                *(cs as *mut *const Slot) = VT_WIN7.as_ptr();
                let f: InitializeSRWLockFn = core::mem::transmute(api.initialize_srw_lock);
                f(lock);
            }
            Kind::Vista { ex } => {
                *(cs as *mut *const Slot) = VT_VISTA.as_ptr();
                if ex {
                    let f: InitializeCriticalSectionExFn = core::mem::transmute(api.initialize_critical_section_ex);
                    f(lock, VISTA_SPIN_COUNT, 0);
                } else {
                    InitializeCriticalSectionAndSpinCount(lock, VISTA_SPIN_COUNT);
                }
            }
            Kind::ConcRT => {
                *(cs as *mut *const Slot) = VT_CONCRT.as_ptr();
                let c = cs.add(8);
                for o in [0x28usize, 0x30, 0, 8, 0x18] {
                    *(c.add(o) as *mut u64) = 0;
                }
                *(c.add(0x10) as *mut u32) = 2;
                *(c.add(0x20) as *mut u32) = 0;
                *(c.add(0x24) as *mut u32) = 0;
            }
        }
        *(m.add(0x48) as *mut i32) = -1;
        *(m.add(0x4c) as *mut u32) = 0;
        *(m as *mut u32) = MTX_PLAIN;
    }

    /// The exit handler: vtable slot 4 of the critical-section object.
    pub unsafe fn destroy() {
        let cs = base().add(8);
        let vt = *(cs as *const *const Slot);
        (*vt.add(4))(cs);
    }
}
