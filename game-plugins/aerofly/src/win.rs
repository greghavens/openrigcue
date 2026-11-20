//! Windows-only runtime pieces of the no_std DLL: the static message
//! vectors, the user DllMain and the panic handler. The entry point, the
//! C++ static initialisers and the exit handlers are in scrt.rs.

use core::cell::UnsafeCell;
use core::ffi::c_void;

use crate::msvc_vector::MessageVector;

pub struct StaticVector(UnsafeCell<MessageVector>);
unsafe impl Sync for StaticVector {}

impl StaticVector {
    const fn new() -> Self {
        StaticVector(UnsafeCell::new(MessageVector::new()))
    }
    /// # Safety
    /// Only from Update (which the game never re-enters) and load/unload.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get(&self) -> &mut MessageVector {
        &mut *self.0.get()
    }
}

/// The received-message list Update fills.
/// It has no lock: Aerofly calls Update from a single thread.
pub static MESSAGES: StaticVector = StaticVector::new();
/// Two more vectors that
/// nothing fills; only their exit handlers run.
pub static UNUSED_VECTOR_240: StaticVector = StaticVector::new();
pub static UNUSED_VECTOR_258: StaticVector = StaticVector::new();

/// The module handle of this DLL.
pub static mut AEROFLY_HINSTANCE: *mut c_void = core::ptr::null_mut();

const DLL_PROCESS_ATTACH: u32 = 1;

/// The user DllMain, called by `scrt::dllmain_dispatch` for
/// every reason: it stores the HINSTANCE when reason == DLL_PROCESS_ATTACH
/// and returns TRUE.
pub unsafe extern "system" fn dll_main(hinstance: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // nothing reads it; volatile keeps the store
        core::ptr::write_volatile(core::ptr::addr_of_mut!(AEROFLY_HINSTANCE), hinstance);
    }
    1
}

/// No reachable panic exists in the plugin; if one happened, terminate
/// with an illegal instruction trap.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("ud2") };
    }
}
