// An allocation whose bytes are whatever the heap holds: the EEPROM code
// passes those bytes to NVS as they are.
// Assembly reads turn the heap bytes into defined Rust scalar outputs before
// the Vec owns them, avoiding an uninitialized-u8 read in the Rust abstract
// machine. The allocation/deallocation use the C allocator.
#[cfg(target_arch = "xtensa")]
fn native_malloc(n: usize) -> Option<alloc::vec::Vec<u8>> {
    if n == 0 {
        return Some(alloc::vec::Vec::new());
    }
    if n > isize::MAX as usize {
        return None;
    }
    unsafe extern "C" {
        fn malloc(n: usize) -> *mut u8;
    }
    unsafe {
        let p = malloc(n);
        if p.is_null() {
            return None;
        }
        for i in 0..n {
            let value: u32;
            core::arch::asm!("l8ui {value},{pointer},0",value=out(reg)value,
                pointer=in(reg)p.add(i),options(readonly,nostack));
            p.add(i).write_volatile(value as u8);
        }
        Some(alloc::vec::Vec::from_raw_parts(p, n, n))
    }
}
