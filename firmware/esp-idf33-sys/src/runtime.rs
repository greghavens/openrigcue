//! Rust allocation and ownership on the pinned C/FreeRTOS runtime.
use core::{
    alloc::{GlobalAlloc, Layout},
    cell::UnsafeCell,
    ffi::c_void,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicPtr, Ordering},
};

struct NativeAllocator;
unsafe impl GlobalAlloc for NativeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // IDF3 multi_heap guarantees four-byte block alignment. Higher Rust
        // alignment uses a private owner header rather than assuming eight.
        if layout.align() <= 4 {
            return crate::malloc(layout.size() as _).cast();
        }
        let Some(total) = layout.size().checked_add(layout.align() - 1 + core::mem::size_of::<usize>()) else {
            return core::ptr::null_mut();
        };
        let base = crate::malloc(total as _).cast::<u8>();
        if base.is_null() {
            return base;
        }
        let aligned =
            ((base as usize + core::mem::size_of::<usize>() + layout.align() - 1) & !(layout.align() - 1)) as *mut u8;
        aligned.sub(core::mem::size_of::<usize>()).cast::<*mut u8>().write(base);
        aligned
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        let base = if layout.align() <= 4 { p } else { p.sub(core::mem::size_of::<usize>()).cast::<*mut u8>().read() };
        crate::free(base.cast());
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.align() <= 4 {
            return crate::realloc(p.cast(), new_size as _).cast();
        }
        let q = self.alloc(Layout::from_size_align_unchecked(new_size, layout.align()));
        if !q.is_null() {
            core::ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
            self.dealloc(p, layout);
        }
        q
    }
}
#[global_allocator]
static ALLOCATOR: NativeAllocator = NativeAllocator;
#[alloc_error_handler]
fn allocation_failure(_layout: Layout) -> ! {
    unsafe { crate::abort() }
}
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    unsafe { crate::abort() }
}

/// Task-only Rust state. This replaces std's IDF5 pthread dependency without
/// holding a critical section across OTA erase/delay or host callbacks.
pub struct Mutex<T> {
    queue: AtomicPtr<c_void>,
    value: UnsafeCell<T>,
}
unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}
impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self { queue: AtomicPtr::new(core::ptr::null_mut()), value: UnsafeCell::new(value) }
    }
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let mut queue = self.queue.load(Ordering::Acquire);
        if queue.is_null() {
            let new = unsafe { crate::xQueueCreateMutex(1) };
            if new.is_null() {
                unsafe { crate::abort() }
            }
            match self.queue.compare_exchange(core::ptr::null_mut(), new.cast(), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => queue = new.cast(),
                Err(existing) => {
                    unsafe { crate::vQueueDelete(new) };
                    queue = existing;
                }
            }
        }
        if unsafe { crate::xQueueSemaphoreTake(queue.cast(), u32::MAX) } != 1 {
            unsafe { crate::abort() }
        }
        MutexGuard { mutex: self, queue }
    }
}
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
    queue: *mut c_void,
}
impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}
impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}
impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        unsafe {
            crate::xQueueGenericSend(self.queue.cast(), core::ptr::null(), 0, 0);
        }
    }
}

/// A mutex on the FreeRTOS portMUX spinlock,
/// including its ISR masking and per-core recursive nesting, not a task mutex.
pub struct CriticalMutex<T> {
    mux: UnsafeCell<crate::portMUX_TYPE>,
    value: UnsafeCell<T>,
}
unsafe impl<T: Send> Sync for CriticalMutex<T> {}
unsafe impl<T: Send> Send for CriticalMutex<T> {}
impl<T> CriticalMutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            mux: UnsafeCell::new(crate::portMUX_TYPE { owner: 0xb33f_ffff, count: 0 }),
            value: UnsafeCell::new(value),
        }
    }
    pub fn lock(&self) -> CriticalGuard<'_, T> {
        unsafe {
            crate::vTaskEnterCritical(self.mux.get());
        }
        CriticalGuard { mutex: self, _same_core: core::marker::PhantomData }
    }
}
pub struct CriticalGuard<'a, T> {
    mutex: &'a CriticalMutex<T>,
    _same_core: core::marker::PhantomData<*mut ()>,
}
impl<T> Deref for CriticalGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}
impl<T> DerefMut for CriticalGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}
impl<T> Drop for CriticalGuard<'_, T> {
    fn drop(&mut self) {
        unsafe {
            crate::vTaskExitCritical(self.mutex.mux.get());
        }
    }
}
