//! Win64 MSVC-compatible allocation exceptions and a single cleanup frame.
use super::Failure;
use core::ffi::c_void;
use core::ptr::{addr_of, null};

#[repr(C)]
struct Exception {
    vtable: *const c_void,
    what: *const u8,
    owns_message: u64,
}

#[link(name = "VCRUNTIME140", kind = "raw-dylib")]
extern "C" {
    fn _CxxThrowException(object: *mut c_void, info: *const c_void) -> !;
    fn __std_exception_copy(source: *const c_void, target: *mut c_void);
    fn __std_exception_destroy(data: *mut c_void);
}
#[link(name = "VCRUNTIME140", kind = "raw-dylib")]
extern "system" {
    fn __C_specific_handler(
        record: *mut c_void,
        frame: *mut c_void,
        context: *mut c_void,
        dispatcher: *mut c_void,
    ) -> i32;
}
#[link(name = "api-ms-win-crt-heap-l1-1-0", kind = "raw-dylib")]
extern "C" {
    fn free(p: *mut c_void);
}

extern "C" {
    static rf2_vtable_exception: c_void;
    static rf2_vtable_bad_alloc: c_void;
    static rf2_vtable_bad_array_new_length: c_void;
    static rf2_throw_bad_alloc: c_void;
    static rf2_throw_bad_array_new_length: c_void;
    fn rf2_with_cleanup(context: *mut c_void, body: super::Body, cleanup: unsafe extern "C" fn(*mut c_void))
        -> *mut u8;
}

unsafe extern "C" fn copy_exception(target: *mut Exception, source: *const Exception) -> *mut Exception {
    (*target).vtable = addr_of!(rf2_vtable_exception);
    (*target).what = null();
    (*target).owns_message = 0;
    __std_exception_copy(addr_of!((*source).what).cast(), addr_of!((*target).what).cast_mut().cast());
    target
}
unsafe extern "C" fn copy_bad_alloc(target: *mut Exception, source: *const Exception) -> *mut Exception {
    copy_exception(target, source);
    (*target).vtable = addr_of!(rf2_vtable_bad_alloc);
    target
}
unsafe extern "C" fn copy_bad_array_new_length(target: *mut Exception, source: *const Exception) -> *mut Exception {
    copy_exception(target, source);
    (*target).vtable = addr_of!(rf2_vtable_bad_array_new_length);
    target
}
unsafe extern "C" fn unwind_exception(object: *mut Exception) {
    (*object).vtable = addr_of!(rf2_vtable_exception);
    __std_exception_destroy(addr_of!((*object).what).cast_mut().cast());
}
unsafe extern "C" fn destroy_exception(object: *mut Exception, flags: u32) -> *mut Exception {
    unwind_exception(object);
    if flags & 1 != 0 {
        free(object.cast());
    }
    object
}
unsafe extern "C" fn exception_what(object: *const Exception) -> *const u8 {
    if (*object).what.is_null() {
        b"Unknown exception\0".as_ptr()
    } else {
        (*object).what
    }
}
unsafe extern "C" fn type_destroy(object: *mut c_void, flags: u32) -> *mut c_void {
    if flags & 1 != 0 {
        free(object);
    }
    object
}
unsafe extern "system" fn exception_handler(
    record: *mut c_void,
    frame: *mut c_void,
    context: *mut c_void,
    dispatcher: *mut c_void,
) -> i32 {
    __C_specific_handler(record, frame, context, dispatcher)
}

pub(crate) unsafe fn throw(failure: Failure) -> ! {
    let (vtable, what, info) = match failure {
        Failure::BadAlloc => {
            (addr_of!(rf2_vtable_bad_alloc), b"bad allocation\0".as_ptr(), addr_of!(rf2_throw_bad_alloc))
        }
        Failure::BadArrayNewLength => (
            addr_of!(rf2_vtable_bad_array_new_length),
            b"bad array new length\0".as_ptr(),
            addr_of!(rf2_throw_bad_array_new_length),
        ),
    };
    let mut object = Exception { vtable, what, owns_message: 0 };
    _CxxThrowException((&mut object as *mut Exception).cast(), info)
}

pub(super) unsafe fn with_cleanup(
    context: *mut c_void,
    body: super::Body,
    cleanup: unsafe extern "C" fn(*mut c_void),
) -> *mut u8 {
    rf2_with_cleanup(context, body, cleanup)
}

core::arch::global_asm!(
    r#"
.section .rdata,"dr"
.p2align 3
rf2_col_exception:
.long 1,0,0
.rva rf2_td_exception, rf2_chd_exception, rf2_col_exception
.p2align 3
.quad rf2_col_exception
rf2_vtable_exception:
.quad {destroy}, {what}
rf2_chd_exception:
.long 0,0,1
.rva rf2_bases_exception
rf2_bases_exception:
.rva rf2_bcd_exception
.long 0
rf2_bcd_exception:
.rva rf2_td_exception
.long 0,0,-1,0,0x40
.rva rf2_chd_exception
rf2_ct_exception:
.long 0
.rva rf2_td_exception
.long 0,-1,0,0x18
.rva {copy_exception}
.p2align 3
rf2_col_bad_alloc:
.long 1,0,0
.rva rf2_td_bad_alloc, rf2_chd_bad_alloc, rf2_col_bad_alloc
.p2align 3
.quad rf2_col_bad_alloc
rf2_vtable_bad_alloc:
.quad {destroy}, {what}
rf2_chd_bad_alloc:
.long 0,0,2
.rva rf2_bases_bad_alloc
rf2_bases_bad_alloc:
.rva rf2_bcd_bad_alloc
.rva rf2_bcd_exception
.long 0
rf2_bcd_bad_alloc:
.rva rf2_td_bad_alloc
.long 1,0,-1,0,0x40
.rva rf2_chd_bad_alloc
rf2_ct_bad_alloc:
.long 0x10
.rva rf2_td_bad_alloc
.long 0,-1,0,0x18
.rva {copy_bad_alloc}
rf2_cta_bad_alloc:
.long 2
.rva rf2_ct_bad_alloc
.rva rf2_ct_exception
.globl rf2_throw_bad_alloc
rf2_throw_bad_alloc:
.long 0
.rva {unwind}
.long 0
.rva rf2_cta_bad_alloc
.p2align 3
rf2_col_bad_array_new_length:
.long 1,0,0
.rva rf2_td_bad_array_new_length, rf2_chd_bad_array_new_length, rf2_col_bad_array_new_length
.p2align 3
.quad rf2_col_bad_array_new_length
rf2_vtable_bad_array_new_length:
.quad {destroy}, {what}
rf2_chd_bad_array_new_length:
.long 0,0,3
.rva rf2_bases_bad_array_new_length
rf2_bases_bad_array_new_length:
.rva rf2_bcd_bad_array_new_length
.rva rf2_bcd_bad_alloc
.rva rf2_bcd_exception
.long 0
rf2_bcd_bad_array_new_length:
.rva rf2_td_bad_array_new_length
.long 2,0,-1,0,0x40
.rva rf2_chd_bad_array_new_length
rf2_ct_bad_array_new_length:
.long 0
.rva rf2_td_bad_array_new_length
.long 0,-1,0,0x18
.rva {copy_bad_array_new_length}
rf2_cta_bad_array_new_length:
.long 3
.rva rf2_ct_bad_array_new_length
.rva rf2_ct_bad_alloc
.rva rf2_ct_exception
.globl rf2_throw_bad_array_new_length
rf2_throw_bad_array_new_length:
.long 0
.rva {unwind}
.long 0
.rva rf2_cta_bad_array_new_length
.data
.p2align 3
rf2_td_exception:
.quad rf2_type_vtable,0
.asciz ".?AVexception@std@@"
.p2align 3
rf2_td_bad_alloc:
.quad rf2_type_vtable,0
.asciz ".?AVbad_alloc@std@@"
.p2align 3
rf2_td_bad_array_new_length:
.quad rf2_type_vtable,0
.asciz ".?AVbad_array_new_length@std@@"
.section .rdata,"dr"
.p2align 3
rf2_type_vtable:
.quad {type_destroy}
.text
.globl rf2_with_cleanup
rf2_with_cleanup:
.seh_proc rf2_with_cleanup
.seh_handler {handler}, @unwind
push rbx
.seh_pushreg rbx
sub rsp,0x50
.seh_stackalloc 0x50
.seh_endprologue
mov qword ptr [rsp+0x20],rcx
mov qword ptr [rsp+0x28],r8
mov rbx,rdx
.Lrf2_try_begin:
call rbx
mov qword ptr [rsp+0x30],rax
.Lrf2_try_end:
mov rcx,qword ptr [rsp+0x20]
call qword ptr [rsp+0x28]
mov rax,qword ptr [rsp+0x30]
add rsp,0x50
pop rbx
ret
.seh_handlerdata
.long 1
.rva .Lrf2_try_begin, .Lrf2_try_end, rf2_cleanup_unwind
.long 0
.text
.seh_endproc
rf2_cleanup_unwind:
.seh_proc rf2_cleanup_unwind
sub rsp,0x28
.seh_stackalloc 0x28
.seh_endprologue
mov rcx,qword ptr [rdx+0x20]
call qword ptr [rdx+0x28]
add rsp,0x28
ret
.seh_endproc
"#,
    destroy = sym destroy_exception,
    what = sym exception_what,
    type_destroy = sym type_destroy,
    copy_exception = sym copy_exception,
    copy_bad_alloc = sym copy_bad_alloc,
    copy_bad_array_new_length = sym copy_bad_array_new_length,
    unwind = sym unwind_exception,
    handler = sym exception_handler,
);
