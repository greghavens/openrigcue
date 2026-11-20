//! RTTI and C++ exception data of the plugin, in the layout the MSVC C++ ABI
//! defines: every vftable is preceded by its RTTICompleteObjectLocator
//! pointer, TypeDescriptors (.data, `vftable` = &type_info::vftable,
//! `spare` 0) carry the decorated names, and `std::bad_alloc` is thrown with
//! a ThrowInfo / CatchableTypeArray.
//!
//! | class | items |
//! |---|---|
//! | InternalsPluginInfo (2 bases) | `COL_INTERNALS_PLUGIN_INFO` |
//! | PluginObjectInfo (1 base) | `COL_PLUGIN_OBJECT_INFO` |
//! | PluginObject (1 base) | `COL_PLUGIN_OBJECT` |
//! | SharedMemoryMapPlugin (5 bases) | `COL_SHARED_MEMORY_MAP_PLUGIN` |
//! | type_info | `COL_TYPE_INFO`, `TYPE_INFO_VFTABLE` |
//! | std::exception | `COL_EXCEPTION`, `EXCEPTION_VFTABLE` |
//! | std::bad_alloc | `COL_BAD_ALLOC`, `BAD_ALLOC_VFTABLE` |
//! | the bad_alloc ThrowInfo with its catchable types | `TI_BAD_ALLOC` |
//!
//! All BaseClassDescriptors have PMD {0, -1, 0} and attributes 0x40; all
//! signatures, attributes and offsets of COLs and CHDs are 0.

use core::ffi::c_void;
use core::ptr::{addr_of, addr_of_mut};

pub struct S<T>(pub T);
unsafe impl<T> Sync for S<T> {}

#[repr(C)]
pub struct TypeDescriptor<const N: usize> {
    pub vftable: *const (),
    pub spare: *mut c_void,
    pub name: [u8; N],
}

#[repr(C)]
pub struct Bcd {
    pub td: *const (),
    pub num_contained: u32,
    pub mdisp: i32,
    pub pdisp: i32,
    pub vdisp: i32,
    pub attributes: u32,
    pub chd: *const Chd,
}

#[repr(C)]
pub struct Chd {
    pub signature: u32,
    pub attributes: u32,
    pub num_base: u32,
    pub bca: *const *const Bcd,
}

#[repr(C)]
pub struct Col {
    pub signature: u32,
    pub offset: u32,
    pub cd_offset: u32,
    pub td: *const (),
    pub chd: *const Chd,
}

/// A vftable with the COL pointer in front of slot 0.
#[repr(C)]
pub struct Vtbl<const N: usize> {
    pub col: *const Col,
    pub slots: [*const (); N],
}
unsafe impl<const N: usize> Sync for Vtbl<N> {}
impl<const N: usize> Vtbl<N> {
    /// The vfptr stored in objects (address of slot 0).
    pub const fn vfptr(&'static self) -> *const () {
        self.slots.as_ptr() as *const ()
    }
}

const fn name<const N: usize>(s: &[u8]) -> [u8; N] {
    let mut o = [0u8; N];
    let mut i = 0;
    while i < s.len() {
        o[i] = s[i];
        i += 1;
    }
    o
}

macro_rules! td {
    ($id:ident, $s:literal) => {
        pub static mut $id: TypeDescriptor<{ $s.len() + 1 }> = TypeDescriptor {
            vftable: TYPE_INFO_VFTABLE.vfptr(),
            spare: core::ptr::null_mut(),
            name: name($s),
        };
    };
}
macro_rules! bcd {
    ($id:ident, $td:ident, $n:expr, $chd:ident) => {
        static $id: S<Bcd> = S(Bcd {
            td: addr_of!($td) as *const (),
            num_contained: $n,
            mdisp: 0,
            pdisp: -1,
            vdisp: 0,
            attributes: 0x40,
            chd: &$chd.0,
        });
    };
}
macro_rules! chd {
    ($id:ident, $bca:ident, [$($b:ident),*]) => {
        static $bca: S<[*const Bcd; [$(stringify!($b)),*].len()]> = S([$(&$b.0 as *const Bcd),*]);
        static $id: S<Chd> = S(Chd { signature: 0, attributes: 0, num_base: $bca.0.len() as u32, bca: $bca.0.as_ptr() });
    };
}
macro_rules! col {
    ($id:ident, $td:ident, $chd:ident) => {
        pub static $id: S<Col> = S(Col { signature: 0, offset: 0, cd_offset: 0, td: addr_of!($td) as *const (), chd: &$chd.0 });
    };
}

// TypeDescriptors (.data).
td!(TD_PLUGIN_OBJECT_INFO, b".?AVPluginObjectInfo@@");
td!(TD_INTERNALS_PLUGIN_INFO, b".?AVInternalsPluginInfo@@");
td!(TD_PLUGIN_OBJECT, b".?AVPluginObject@@");
td!(TD_INTERNALS_PLUGIN, b".?AVInternalsPlugin@@");
td!(TD_INTERNALS_PLUGIN_V2, b".?AVInternalsPluginV2@@");
td!(TD_INTERNALS_PLUGIN_V3, b".?AVInternalsPluginV3@@");
td!(TD_SHARED_MEMORY_MAP_PLUGIN, b".?AVSharedMemoryMapPlugin@@");
td!(TD_TYPE_INFO, b".?AVtype_info@@");
td!(TD_BAD_ALLOC, b".?AVbad_alloc@std@@");
td!(TD_EXCEPTION, b".?AVexception@std@@");

// PluginObjectInfo / InternalsPluginInfo
bcd!(BCD_POI, TD_PLUGIN_OBJECT_INFO, 0, CHD_POI);
chd!(CHD_POI, BCA_POI, [BCD_POI]);
bcd!(BCD_IPI, TD_INTERNALS_PLUGIN_INFO, 1, CHD_IPI);
chd!(CHD_IPI, BCA_IPI, [BCD_IPI, BCD_POI]);
col!(COL_PLUGIN_OBJECT_INFO, TD_PLUGIN_OBJECT_INFO, CHD_POI);
col!(COL_INTERNALS_PLUGIN_INFO, TD_INTERNALS_PLUGIN_INFO, CHD_IPI);

// PluginObject <- InternalsPlugin <- V2 <- V3 <- SharedMemoryMapPlugin
bcd!(BCD_PO, TD_PLUGIN_OBJECT, 0, CHD_PO);
chd!(CHD_PO, BCA_PO, [BCD_PO]);
bcd!(BCD_IP, TD_INTERNALS_PLUGIN, 1, CHD_IP);
chd!(CHD_IP, BCA_IP, [BCD_IP, BCD_PO]);
bcd!(BCD_IP2, TD_INTERNALS_PLUGIN_V2, 2, CHD_IP2);
chd!(CHD_IP2, BCA_IP2, [BCD_IP2, BCD_IP, BCD_PO]);
bcd!(BCD_IP3, TD_INTERNALS_PLUGIN_V3, 3, CHD_IP3);
chd!(CHD_IP3, BCA_IP3, [BCD_IP3, BCD_IP2, BCD_IP, BCD_PO]);
bcd!(BCD_SMMP, TD_SHARED_MEMORY_MAP_PLUGIN, 4, CHD_SMMP);
chd!(CHD_SMMP, BCA_SMMP, [BCD_SMMP, BCD_IP3, BCD_IP2, BCD_IP, BCD_PO]);
col!(COL_PLUGIN_OBJECT, TD_PLUGIN_OBJECT, CHD_PO);
col!(COL_SHARED_MEMORY_MAP_PLUGIN, TD_SHARED_MEMORY_MAP_PLUGIN, CHD_SMMP);

// type_info
bcd!(BCD_TI, TD_TYPE_INFO, 0, CHD_TI);
chd!(CHD_TI, BCA_TI, [BCD_TI]);
col!(COL_TYPE_INFO, TD_TYPE_INFO, CHD_TI);

// std::exception <- std::bad_alloc
bcd!(BCD_EXC, TD_EXCEPTION, 0, CHD_EXC);
chd!(CHD_EXC, BCA_EXC, [BCD_EXC]);
bcd!(BCD_BA, TD_BAD_ALLOC, 1, CHD_BA);
chd!(CHD_BA, BCA_BA, [BCD_BA, BCD_EXC]);
col!(COL_EXCEPTION, TD_EXCEPTION, CHD_EXC);
col!(COL_BAD_ALLOC, TD_BAD_ALLOC, CHD_BA);

// ---- type_info::vftable ----

/// The `type_info` scalar deleting destructor (vfptr reset, delete
/// 0xc bytes if flags & 1).
unsafe extern "thiscall" fn type_info_sdd(this: *mut *const (), flags: u32) -> *mut *const () {
    *this = TYPE_INFO_VFTABLE.vfptr();
    if flags & 1 != 0 {
        crate::rt::operator_delete(this as *mut c_void);
    }
    this
}
pub static TYPE_INFO_VFTABLE: Vtbl<1> = Vtbl { col: &COL_TYPE_INFO.0, slots: [type_info_sdd as *const ()] };

// ---- std::exception / std::bad_alloc ----

/// The message of a `std::exception` and whether it is owned.
#[repr(C)]
pub struct ExceptionData {
    pub what: *const u8,
    pub do_free: u8,
    pub _pad: [u8; 3],
}
#[repr(C)]
pub struct StdException {
    pub vfptr: *const (),
    pub data: ExceptionData,
}

static UNKNOWN_EXCEPTION: [u8; 18] = *b"Unknown exception\0";
static BAD_ALLOCATION: [u8; 15] = *b"bad allocation\0";

/// Copies exception data, duplicating an owned message.
unsafe fn std_exception_copy(src: *const ExceptionData, dst: *mut ExceptionData) {
    if (*src).do_free == 0 || (*src).what.is_null() {
        (*dst).what = (*src).what;
        (*dst).do_free = 0;
        return;
    }
    let s = (*src).what;
    let mut n = 0usize;
    while *s.add(n) != 0 {
        n += 1;
    }
    let len = n + 1;
    let buf = crate::rt::malloc(len) as *mut u8;
    if !buf.is_null() {
        // copy the message with its terminator
        for i in 0..len {
            *buf.add(i) = *s.add(i);
        }
        (*dst).what = buf;
        (*dst).do_free = 1;
    }
    crate::rt::free(core::ptr::null_mut());
}

/// Frees an owned message and clears the exception data.
unsafe fn std_exception_destroy(d: *mut ExceptionData) {
    if (*d).do_free != 0 {
        crate::rt::free((*d).what as *mut c_void);
    }
    (*d).what = core::ptr::null();
    (*d).do_free = 0;
}

/// `std::exception::exception(const exception &)`.
unsafe extern "thiscall" fn exception_copy(this: *mut StdException, src: *const StdException) -> *mut StdException {
    (*this).vfptr = EXCEPTION_VFTABLE.vfptr();
    (*this).data.what = core::ptr::null();
    *(addr_of_mut!((*this).data.do_free) as *mut u32) = 0;
    std_exception_copy(&(*src).data, &mut (*this).data);
    this
}
/// `std::bad_alloc::bad_alloc(const bad_alloc &)`.
unsafe extern "thiscall" fn bad_alloc_copy(this: *mut StdException, src: *const StdException) -> *mut StdException {
    exception_copy(this, src);
    (*this).vfptr = BAD_ALLOC_VFTABLE.vfptr();
    this
}
/// `std::exception::~exception` (the unwind function of the ThrowInfo).
unsafe extern "thiscall" fn exception_dtor(this: *mut StdException) {
    (*this).vfptr = EXCEPTION_VFTABLE.vfptr();
    std_exception_destroy(&mut (*this).data);
}
/// The scalar deleting destructor of both classes (delete 0xc bytes).
unsafe extern "thiscall" fn exception_sdd(this: *mut StdException, flags: u32) -> *mut StdException {
    (*this).vfptr = EXCEPTION_VFTABLE.vfptr();
    std_exception_destroy(&mut (*this).data);
    if flags & 1 != 0 {
        crate::rt::operator_delete(this as *mut c_void);
    }
    this
}
/// `std::exception::what`.
unsafe extern "thiscall" fn exception_what(this: *const StdException) -> *const u8 {
    let w = (*this).data.what;
    if w.is_null() {
        UNKNOWN_EXCEPTION.as_ptr()
    } else {
        w
    }
}

pub static EXCEPTION_VFTABLE: Vtbl<2> =
    Vtbl { col: &COL_EXCEPTION.0, slots: [exception_sdd as *const (), exception_what as *const ()] };
pub static BAD_ALLOC_VFTABLE: Vtbl<2> =
    Vtbl { col: &COL_BAD_ALLOC.0, slots: [exception_sdd as *const (), exception_what as *const ()] };

/// `std::bad_alloc::bad_alloc()` ("bad allocation").
pub unsafe fn bad_alloc_ctor(this: *mut StdException) {
    (*this).data.what = core::ptr::null();
    *(addr_of_mut!((*this).data.do_free) as *mut u32) = 0;
    (*this).data.what = BAD_ALLOCATION.as_ptr();
    (*this).vfptr = BAD_ALLOC_VFTABLE.vfptr();
}

// ---- ThrowInfo ----

#[repr(C)]
pub struct CatchableType {
    pub properties: u32,
    pub td: *const (),
    pub mdisp: i32,
    pub pdisp: i32,
    pub vdisp: i32,
    pub size: u32,
    pub copy: *const (),
}
#[repr(C)]
pub struct ThrowInfo {
    pub attributes: u32,
    pub unwind: *const (),
    pub forward_compat: *const (),
    pub cta: *const (),
}

/// Catchable type bad_alloc: CT_IsStdBadAlloc (0x10), size 0xc.
static CT_BAD_ALLOC: S<CatchableType> = S(CatchableType {
    properties: 0x10,
    td: addr_of!(TD_BAD_ALLOC) as *const (),
    mdisp: 0,
    pdisp: -1,
    vdisp: 0,
    size: 0xc,
    copy: bad_alloc_copy as *const (),
});
/// Catchable type exception: size 0xc.
static CT_EXCEPTION: S<CatchableType> = S(CatchableType {
    properties: 0,
    td: addr_of!(TD_EXCEPTION) as *const (),
    mdisp: 0,
    pdisp: -1,
    vdisp: 0,
    size: 0xc,
    copy: exception_copy as *const (),
});
/// The catchable type array of bad_alloc.
static CTA_BAD_ALLOC: S<(u32, [*const CatchableType; 2])> = S((2, [&CT_BAD_ALLOC.0, &CT_EXCEPTION.0]));
/// The ThrowInfo of bad_alloc: attributes 0, unwind = the exception destructor.
static TI_BAD_ALLOC: S<ThrowInfo> = S(ThrowInfo {
    attributes: 0,
    unwind: exception_dtor as *const (),
    forward_compat: core::ptr::null(),
    cta: &CTA_BAD_ALLOC.0 as *const _ as *const (),
});

pub fn throw_info_bad_alloc() -> *const ThrowInfo {
    &TI_BAD_ALLOC.0
}
