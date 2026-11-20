//! The C++ exceptions the plugin can throw out of its own code, built and
//! raised in the MSVC C++ ABI so that the game can catch them (Windows only).
//!
//! Throw sites:
//! * bad_alloc: operator new found no memory
//!   (the only one reachable: a failed HeapAlloc while the message vector
//!   grows or the name table is built). The object holds
//!   its vptr, the message "bad allocation" and a zero qword at +0x10.
//! * bad_array_new_length, with the message "bad array new length".
//! * length_error("vector too long"): the message is a heap copy.
//!
//! The throw: params {0x19930520, object,
//! ThrowInfo, image base from RtlPcToFileHeader(ThrowInfo)} and
//! `RaiseException(0xE06D7363, EXCEPTION_NONCONTINUABLE, 4, params)`.
//!
//! The RTTI and throw metadata below are in the x64 form of that ABI
//! (image-relative 32-bit references): type
//! descriptors in .data with `type_info`'s vftable, complete object
//! locators (signature 1, self RVA), class hierarchy descriptors and base
//! class arrays (0-terminated), base class descriptors (PMD 0,-1,0,
//! attributes 0x40), catchable types (size 0x18; bad_alloc's has property
//! 0x10 `CT_IsStdBadAlloc`), catchable type arrays and ThrowInfos
//! (attributes 0, unwind = the exception destructor).
#![allow(non_upper_case_globals)]

use core::ffi::c_void;
use core::ptr::{addr_of, null, null_mut};

use crate::crt::{RaiseException, RtlPcToFileHeader};
use crate::msvc_heap::{free, malloc};

/// `std::exception` / derived objects: vptr, then the message pointer and
/// the flag that says whether the message is owned.
#[repr(C)]
pub struct StdException {
    vptr: *const c_void,
    what: *const u8,
    do_free: u64, // bool + padding; the constructors clear the whole qword
}

#[repr(C)]
struct ExceptionData {
    what: *const u8,
    do_free: u8,
}

const EH_EXCEPTION_NUMBER: u32 = 0xE06D_7363;
const EH_MAGIC_NUMBER1: usize = 0x1993_0520;
const EH_PURE_MAGIC_NUMBER1: usize = 0x0199_4000;
const EXCEPTION_NONCONTINUABLE: u32 = 1;
const TI_IS_PURE: u32 = 0x8;
const TI_IS_WINRT: u32 = 0x10;

static BAD_ALLOCATION: &[u8] = b"bad allocation\0";
static BAD_ARRAY_NEW_LENGTH: &[u8] = b"bad array new length\0";
static VECTOR_TOO_LONG: &[u8] = b"vector too long\0";
static UNKNOWN_EXCEPTION: &[u8] = b"Unknown exception\0";

extern "C" {
    static aerofly_eh_vft_exception: c_void;
    static aerofly_eh_vft_bad_alloc: c_void;
    static aerofly_eh_vft_bad_array_new_length: c_void;
    static aerofly_eh_vft_logic_error: c_void;
    static aerofly_eh_vft_length_error: c_void;
    static aerofly_eh_vft_type_info: c_void;
    static aerofly_eh_ti_bad_alloc: c_void;
    static aerofly_eh_ti_bad_array_new_length: c_void;
    static aerofly_eh_ti_length_error: c_void;
}

core::arch::global_asm!(
    // ---- vftables (COL pointer first, then the slots) --------------------
    ".section .rdata,\"dr\"",
    ".p2align 3",
    ".quad aerofly_eh_col_exception",
    ".globl aerofly_eh_vft_exception",
    "aerofly_eh_vft_exception:",
    ".quad {sdd}",
    ".quad {what}",
    ".quad aerofly_eh_col_bad_alloc",
    ".globl aerofly_eh_vft_bad_alloc",
    "aerofly_eh_vft_bad_alloc:",
    ".quad {sdd}",
    ".quad {what}",
    ".quad aerofly_eh_col_bad_array_new_length",
    ".globl aerofly_eh_vft_bad_array_new_length",
    "aerofly_eh_vft_bad_array_new_length:",
    ".quad {sdd}",
    ".quad {what}",
    ".quad aerofly_eh_col_logic_error",
    ".globl aerofly_eh_vft_logic_error",
    "aerofly_eh_vft_logic_error:",
    ".quad {sdd}",
    ".quad {what}",
    ".quad aerofly_eh_col_length_error",
    ".globl aerofly_eh_vft_length_error",
    "aerofly_eh_vft_length_error:",
    ".quad {sdd}",
    ".quad {what}",
    ".quad aerofly_eh_col_type_info",
    ".globl aerofly_eh_vft_type_info",
    "aerofly_eh_vft_type_info:",
    ".quad {ti_sdd}",
    // ---- complete object locators {sig, offset, cdOffset, TD, CHD, self} --
    ".p2align 2",
    "aerofly_eh_col_exception:",
    ".long 1, 0, 0",
    ".rva aerofly_eh_td_exception, aerofly_eh_chd_exception, aerofly_eh_col_exception",
    "aerofly_eh_col_bad_alloc:",
    ".long 1, 0, 0",
    ".rva aerofly_eh_td_bad_alloc, aerofly_eh_chd_bad_alloc, aerofly_eh_col_bad_alloc",
    "aerofly_eh_col_bad_array_new_length:",
    ".long 1, 0, 0",
    ".rva aerofly_eh_td_bad_array_new_length, aerofly_eh_chd_bad_array_new_length, aerofly_eh_col_bad_array_new_length",
    "aerofly_eh_col_logic_error:",
    ".long 1, 0, 0",
    ".rva aerofly_eh_td_logic_error, aerofly_eh_chd_logic_error, aerofly_eh_col_logic_error",
    "aerofly_eh_col_length_error:",
    ".long 1, 0, 0",
    ".rva aerofly_eh_td_length_error, aerofly_eh_chd_length_error, aerofly_eh_col_length_error",
    "aerofly_eh_col_type_info:",
    ".long 1, 0, 0",
    ".rva aerofly_eh_td_type_info, aerofly_eh_chd_type_info, aerofly_eh_col_type_info",
    // ---- class hierarchy descriptors {sig 0, attr 0, nbases, BCA} --------
    "aerofly_eh_chd_exception:",
    ".long 0, 0, 1",
    ".rva aerofly_eh_bca_exception",
    "aerofly_eh_bca_exception:",
    ".rva aerofly_eh_bcd_exception",
    ".long 0",
    "aerofly_eh_chd_bad_alloc:",
    ".long 0, 0, 2",
    ".rva aerofly_eh_bca_bad_alloc",
    "aerofly_eh_bca_bad_alloc:",
    ".rva aerofly_eh_bcd_bad_alloc, aerofly_eh_bcd_exception",
    ".long 0",
    "aerofly_eh_chd_bad_array_new_length:",
    ".long 0, 0, 3",
    ".rva aerofly_eh_bca_bad_array_new_length",
    "aerofly_eh_bca_bad_array_new_length:",
    ".rva aerofly_eh_bcd_bad_array_new_length, aerofly_eh_bcd_bad_alloc, aerofly_eh_bcd_exception",
    ".long 0",
    "aerofly_eh_chd_logic_error:",
    ".long 0, 0, 2",
    ".rva aerofly_eh_bca_logic_error",
    "aerofly_eh_bca_logic_error:",
    ".rva aerofly_eh_bcd_logic_error, aerofly_eh_bcd_exception",
    ".long 0",
    "aerofly_eh_chd_length_error:",
    ".long 0, 0, 3",
    ".rva aerofly_eh_bca_length_error",
    "aerofly_eh_bca_length_error:",
    ".rva aerofly_eh_bcd_length_error, aerofly_eh_bcd_logic_error, aerofly_eh_bcd_exception",
    ".long 0",
    "aerofly_eh_chd_type_info:",
    ".long 0, 0, 1",
    ".rva aerofly_eh_bca_type_info",
    "aerofly_eh_bca_type_info:",
    ".rva aerofly_eh_bcd_type_info",
    ".long 0",
    // ---- base class descriptors {TD, ncontained, mdisp, pdisp, vdisp, attr, CHD}
    "aerofly_eh_bcd_exception:",
    ".rva aerofly_eh_td_exception",
    ".long 0, 0, -1, 0, 0x40",
    ".rva aerofly_eh_chd_exception",
    "aerofly_eh_bcd_bad_alloc:",
    ".rva aerofly_eh_td_bad_alloc",
    ".long 1, 0, -1, 0, 0x40",
    ".rva aerofly_eh_chd_bad_alloc",
    "aerofly_eh_bcd_bad_array_new_length:",
    ".rva aerofly_eh_td_bad_array_new_length",
    ".long 2, 0, -1, 0, 0x40",
    ".rva aerofly_eh_chd_bad_array_new_length",
    "aerofly_eh_bcd_logic_error:",
    ".rva aerofly_eh_td_logic_error",
    ".long 1, 0, -1, 0, 0x40",
    ".rva aerofly_eh_chd_logic_error",
    "aerofly_eh_bcd_length_error:",
    ".rva aerofly_eh_td_length_error",
    ".long 2, 0, -1, 0, 0x40",
    ".rva aerofly_eh_chd_length_error",
    "aerofly_eh_bcd_type_info:",
    ".rva aerofly_eh_td_type_info",
    ".long 0, 0, -1, 0, 0x40",
    ".rva aerofly_eh_chd_type_info",
    // ---- catchable types {props, TD, mdisp, pdisp, vdisp, size, copy} ----
    "aerofly_eh_ct_exception:",
    ".long 0",
    ".rva aerofly_eh_td_exception",
    ".long 0, -1, 0, 0x18",
    ".rva {copy_exception}",
    "aerofly_eh_ct_bad_alloc:",
    ".long 0x10",
    ".rva aerofly_eh_td_bad_alloc",
    ".long 0, -1, 0, 0x18",
    ".rva {copy_bad_alloc}",
    "aerofly_eh_ct_bad_array_new_length:",
    ".long 0",
    ".rva aerofly_eh_td_bad_array_new_length",
    ".long 0, -1, 0, 0x18",
    ".rva {copy_bad_array_new_length}",
    "aerofly_eh_ct_logic_error:",
    ".long 0",
    ".rva aerofly_eh_td_logic_error",
    ".long 0, -1, 0, 0x18",
    ".rva {copy_logic_error}",
    "aerofly_eh_ct_length_error:",
    ".long 0",
    ".rva aerofly_eh_td_length_error",
    ".long 0, -1, 0, 0x18",
    ".rva {copy_length_error}",
    // ---- catchable type arrays and ThrowInfos {attr, unwind, fwd, CTA} ---
    "aerofly_eh_cta_bad_alloc:",
    ".long 2",
    ".rva aerofly_eh_ct_bad_alloc, aerofly_eh_ct_exception",
    "aerofly_eh_cta_bad_array_new_length:",
    ".long 3",
    ".rva aerofly_eh_ct_bad_array_new_length, aerofly_eh_ct_bad_alloc, aerofly_eh_ct_exception",
    "aerofly_eh_cta_length_error:",
    ".long 3",
    ".rva aerofly_eh_ct_length_error, aerofly_eh_ct_logic_error, aerofly_eh_ct_exception",
    ".globl aerofly_eh_ti_bad_alloc",
    "aerofly_eh_ti_bad_alloc:",
    ".long 0",
    ".rva {dtor}",
    ".long 0",
    ".rva aerofly_eh_cta_bad_alloc",
    ".globl aerofly_eh_ti_bad_array_new_length",
    "aerofly_eh_ti_bad_array_new_length:",
    ".long 0",
    ".rva {dtor}",
    ".long 0",
    ".rva aerofly_eh_cta_bad_array_new_length",
    ".globl aerofly_eh_ti_length_error",
    "aerofly_eh_ti_length_error:",
    ".long 0",
    ".rva {dtor}",
    ".long 0",
    ".rva aerofly_eh_cta_length_error",
    // ---- type descriptors {type_info vftable, spare, name} (.data) -------
    ".data",
    ".p2align 3",
    "aerofly_eh_td_exception:",
    ".quad aerofly_eh_vft_type_info, 0",
    ".asciz \".?AVexception@std@@\"",
    ".p2align 3",
    "aerofly_eh_td_bad_alloc:",
    ".quad aerofly_eh_vft_type_info, 0",
    ".asciz \".?AVbad_alloc@std@@\"",
    ".p2align 3",
    "aerofly_eh_td_bad_array_new_length:",
    ".quad aerofly_eh_vft_type_info, 0",
    ".asciz \".?AVbad_array_new_length@std@@\"",
    ".p2align 3",
    "aerofly_eh_td_logic_error:",
    ".quad aerofly_eh_vft_type_info, 0",
    ".asciz \".?AVlogic_error@std@@\"",
    ".p2align 3",
    "aerofly_eh_td_length_error:",
    ".quad aerofly_eh_vft_type_info, 0",
    ".asciz \".?AVlength_error@std@@\"",
    ".p2align 3",
    "aerofly_eh_td_type_info:",
    ".quad aerofly_eh_vft_type_info, 0",
    ".asciz \".?AVtype_info@@\"",
    ".text",
    sdd = sym exception_scalar_deleting_destructor,
    what = sym exception_what,
    ti_sdd = sym type_info_scalar_deleting_destructor,
    dtor = sym exception_destructor,
    copy_exception = sym exception_copy,
    copy_bad_alloc = sym bad_alloc_copy,
    copy_bad_array_new_length = sym bad_array_new_length_copy,
    copy_logic_error = sym logic_error_copy,
    copy_length_error = sym length_error_copy,
);

fn vft(p: &'static c_void) -> *const c_void {
    p as *const c_void
}

// ---- exception data ------------------------------------------------------

/// Copies exception data, duplicating an owned message.
unsafe fn std_exception_copy(from: *const ExceptionData, to: *mut ExceptionData) {
    if (*from).do_free != 0 && !(*from).what.is_null() {
        let src = (*from).what;
        let mut n = 0usize;
        while *src.add(n) != 0 {
            n += 1;
        }
        let count = n + 1;
        let mut buffer = malloc(count);
        if !buffer.is_null() {
            // the message and its NUL fit exactly
            core::ptr::copy_nonoverlapping(src, buffer, count);
            (*to).do_free = 1;
            (*to).what = buffer;
            buffer = null_mut();
        }
        free(buffer);
    } else {
        (*to).what = (*from).what;
        (*to).do_free = 0;
    }
}

/// Frees an owned message and clears the exception data.
unsafe fn std_exception_destroy(data: *mut ExceptionData) {
    if (*data).do_free != 0 {
        free((*data).what as *mut u8);
    }
    (*data).what = null();
    (*data).do_free = 0;
}

unsafe fn data(e: *mut StdException) -> *mut ExceptionData {
    addr_of!((*e).what) as *mut ExceptionData
}

/// Copy constructor body shared by all catchable types: vptr = exception's,
/// 16 bytes at +8 cleared, then the data copy; the derived ones then
/// store their own vptr.
unsafe fn copy_into(dst: *mut StdException, src: *const StdException, own: *const c_void) -> *mut StdException {
    (*dst).vptr = vft(&aerofly_eh_vft_exception);
    (*dst).what = null();
    (*dst).do_free = 0;
    std_exception_copy(addr_of!((*src).what) as *const ExceptionData, data(dst));
    if !own.is_null() {
        (*dst).vptr = own;
    }
    dst
}

/// `std::exception::exception(const exception&)`
unsafe extern "C" fn exception_copy(dst: *mut StdException, src: *const StdException) -> *mut StdException {
    copy_into(dst, src, null())
}

/// `std::bad_alloc::bad_alloc(const bad_alloc&)`
unsafe extern "C" fn bad_alloc_copy(dst: *mut StdException, src: *const StdException) -> *mut StdException {
    copy_into(dst, src, vft(&aerofly_eh_vft_bad_alloc))
}

/// `std::bad_array_new_length::bad_array_new_length(const&)`
unsafe extern "C" fn bad_array_new_length_copy(dst: *mut StdException, src: *const StdException) -> *mut StdException {
    copy_into(dst, src, vft(&aerofly_eh_vft_bad_array_new_length))
}

/// `std::logic_error::logic_error(const logic_error&)`
unsafe extern "C" fn logic_error_copy(dst: *mut StdException, src: *const StdException) -> *mut StdException {
    copy_into(dst, src, vft(&aerofly_eh_vft_logic_error))
}

/// `std::length_error::length_error(const length_error&)`
unsafe extern "C" fn length_error_copy(dst: *mut StdException, src: *const StdException) -> *mut StdException {
    copy_into(dst, src, vft(&aerofly_eh_vft_length_error))
}

/// `std::exception::~exception` (the ThrowInfo unwind
/// function of all three throws).
unsafe extern "C" fn exception_destructor(this: *mut StdException) {
    (*this).vptr = vft(&aerofly_eh_vft_exception);
    std_exception_destroy(data(this));
}

/// vftable slot 0 of every exception class: scalar deleting
/// destructor; bit 0 of `flags` frees the 0x18-byte object.
unsafe extern "C" fn exception_scalar_deleting_destructor(this: *mut StdException, flags: u32) -> *mut StdException {
    (*this).vptr = vft(&aerofly_eh_vft_exception);
    std_exception_destroy(data(this));
    if flags & 1 != 0 {
        free(this as *mut u8); // operator delete(this, 0x18)
    }
    this
}

/// vftable slot 1: `what()`.
unsafe extern "C" fn exception_what(this: *const StdException) -> *const u8 {
    let w = (*this).what;
    if w.is_null() {
        UNKNOWN_EXCEPTION.as_ptr()
    } else {
        w
    }
}

/// `type_info`'s scalar deleting destructor.
unsafe extern "C" fn type_info_scalar_deleting_destructor(this: *mut *const c_void, flags: u32) -> *mut *const c_void {
    *this = vft(&aerofly_eh_vft_type_info);
    if flags & 1 != 0 {
        free(this as *mut u8); // operator delete(this, 0x18)
    }
    this
}

// ---- throwing -------------------------------------------------------------

/// Throws a C++ exception with one of the ThrowInfos above (attributes 0).
unsafe fn cxx_throw(object: *mut StdException, throw_info: *const c_void) -> ! {
    let attributes = *(throw_info as *const u32);
    let mut base: *mut c_void = null_mut();
    base = RtlPcToFileHeader(throw_info, &mut base);
    debug_assert!(attributes & TI_IS_WINRT == 0);
    let magic = if attributes & TI_IS_PURE != 0 || base.is_null() { EH_PURE_MAGIC_NUMBER1 } else { EH_MAGIC_NUMBER1 };
    let params: [usize; 4] = [magic, object as usize, throw_info as usize, base as usize];
    RaiseException(EH_EXCEPTION_NUMBER, EXCEPTION_NONCONTINUABLE, 4, params.as_ptr());
    // a non-continuable exception never comes back
    loop {
        core::arch::asm!("int3");
    }
}

/// Throws `std::bad_alloc`.
#[inline(never)]
pub unsafe fn throw_bad_alloc() -> ! {
    let mut e = StdException { vptr: vft(&aerofly_eh_vft_bad_alloc), what: BAD_ALLOCATION.as_ptr(), do_free: 0 };
    cxx_throw(&mut e, addr_of!(aerofly_eh_ti_bad_alloc))
}

/// Throws `std::bad_array_new_length`.
#[inline(never)]
pub unsafe fn throw_bad_array_new_length() -> ! {
    let mut e = StdException {
        vptr: vft(&aerofly_eh_vft_bad_array_new_length),
        what: BAD_ARRAY_NEW_LENGTH.as_ptr(),
        do_free: 0,
    };
    cxx_throw(&mut e, addr_of!(aerofly_eh_ti_bad_array_new_length))
}

/// Throws `std::length_error("vector too long")`:
/// the message is copied to the heap.
#[inline(never)]
pub unsafe fn throw_length_error_vector_too_long() -> ! {
    let mut e = StdException { vptr: vft(&aerofly_eh_vft_exception), what: null(), do_free: 0 };
    let init = ExceptionData { what: VECTOR_TOO_LONG.as_ptr(), do_free: 1 };
    std_exception_copy(&init, data(&mut e));
    e.vptr = vft(&aerofly_eh_vft_length_error);
    cxx_throw(&mut e, addr_of!(aerofly_eh_ti_length_error))
}
