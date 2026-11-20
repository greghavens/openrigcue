// Reads of bytes and words in a raw stack frame. The assembly reads return
// the bytes already present on the interrupt stack as register outputs, so no
// uninitialized Rust scalar value is created.
#[inline(always)]
unsafe fn native_stack_byte(pointer: *const u8) -> u8 {
    let value: u32;
    core::arch::asm!("l8ui {value},{pointer},0",value=out(reg)value,pointer=in(reg)pointer,options(readonly,nostack));
    value as u8
}
#[inline(always)]
unsafe fn native_stack_word(pointer: *const u8) -> u32 {
    let value: u32;
    core::arch::asm!("l32i {value},{pointer},0",value=out(reg)value,pointer=in(reg)pointer,options(readonly,nostack));
    value
}
struct NativeCanStorage {
    frame: *mut u8,
}
impl can::FrameStorage for NativeCanStorage {
    #[inline(always)]
    fn fir(&mut self, value: u32) {
        unsafe {
            ptr::write_volatile(self.frame.cast::<u32>(), value);
        }
    }
    #[inline(always)]
    fn msg_id(&mut self, value: u32) {
        unsafe {
            ptr::write_volatile(self.frame.add(4).cast::<u32>(), value);
        }
    }
    #[inline(always)]
    fn data(&mut self, index: usize, value: u8) {
        unsafe {
            ptr::write_volatile(self.frame.add(8 + index), value);
        }
    }
}
