// The import data of the x64 DLL
// (ets2-telemetry.dll .rdata): the import address table at the start of
// .rdata (0x120 bytes, KERNEL32 then MSVCR110, each list
// zero-terminated), and after the unwind data the import directory
// (0x3c bytes), the two import lookup tables and the
// hint/name and DLL-name strings. ets2-telemetry.ld places the two sections,
// scripts/pe-link.sh points the import and IAT data directories at
// ets2_import_directory / ets2_iat_start.
//
// Every import is defined here (`__imp_X`, and the `jmp [IAT]` thunk `X` for
// the 15 that are called through one),
// so no member of the mingw-w64 import libraries is linked; ets2-telemetry.ld
// asserts that no `.idata$*` section reaches the image.

    .section .ets2iat,"dr"
    .p2align 3
    .globl ets2_iat_start
ets2_iat_start:
    .globl __imp_MapViewOfFile
__imp_MapViewOfFile:
    .rva .Lhn_MapViewOfFile
    .long 0
    .globl __imp_UnmapViewOfFile
__imp_UnmapViewOfFile:
    .rva .Lhn_UnmapViewOfFile
    .long 0
    .globl __imp_GetLastError
__imp_GetLastError:
    .rva .Lhn_GetLastError
    .long 0
    .globl __imp_CreateFileMappingW
__imp_CreateFileMappingW:
    .rva .Lhn_CreateFileMappingW
    .long 0
    .globl __imp_OpenFileMappingW
__imp_OpenFileMappingW:
    .rva .Lhn_OpenFileMappingW
    .long 0
    .globl __imp_CloseHandle
__imp_CloseHandle:
    .rva .Lhn_CloseHandle
    .long 0
    .globl __imp_GetSystemTimeAsFileTime
__imp_GetSystemTimeAsFileTime:
    .rva .Lhn_GetSystemTimeAsFileTime
    .long 0
    .globl __imp_GetCurrentThreadId
__imp_GetCurrentThreadId:
    .rva .Lhn_GetCurrentThreadId
    .long 0
    .globl __imp_QueryPerformanceCounter
__imp_QueryPerformanceCounter:
    .rva .Lhn_QueryPerformanceCounter
    .long 0
    .globl __imp_IsProcessorFeaturePresent
__imp_IsProcessorFeaturePresent:
    .rva .Lhn_IsProcessorFeaturePresent
    .long 0
    .globl __imp_IsDebuggerPresent
__imp_IsDebuggerPresent:
    .rva .Lhn_IsDebuggerPresent
    .long 0
    .globl __imp_DecodePointer
__imp_DecodePointer:
    .rva .Lhn_DecodePointer
    .long 0
    .globl __imp_EncodePointer
__imp_EncodePointer:
    .rva .Lhn_EncodePointer
    .long 0
    .globl __imp_GetTickCount64
__imp_GetTickCount64:
    .rva .Lhn_GetTickCount64
    .long 0
    .quad 0
.Liat_msvcr110:
    .globl __imp__initterm_e
__imp__initterm_e:
    .rva .Lhn__initterm_e
    .long 0
    .globl __imp___C_specific_handler
__imp___C_specific_handler:
    .rva .Lhn___C_specific_handler
    .long 0
    .globl __imp___crt_debugger_hook
__imp___crt_debugger_hook:
    .rva .Lhn___crt_debugger_hook
    .long 0
    .globl __imp__initterm
__imp__initterm:
    .rva .Lhn__initterm
    .long 0
    .globl __imp___crtTerminateProcess
__imp___crtTerminateProcess:
    .rva .Lhn___crtTerminateProcess
    .long 0
    .globl __imp___crtCapturePreviousContext
__imp___crtCapturePreviousContext:
    .rva .Lhn___crtCapturePreviousContext
    .long 0
    .globl __imp__lock
__imp__lock:
    .rva .Lhn__lock
    .long 0
    .globl __imp__unlock
__imp__unlock:
    .rva .Lhn__unlock
    .long 0
    .globl __imp__calloc_crt
__imp__calloc_crt:
    .rva .Lhn__calloc_crt
    .long 0
    .globl __imp___dllonexit
__imp___dllonexit:
    .rva .Lhn___dllonexit
    .long 0
    .globl __imp__onexit
__imp__onexit:
    .rva .Lhn__onexit
    .long 0
    .globl __imp___clean_type_info_names_internal
__imp___clean_type_info_names_internal:
    .rva .Lhn___clean_type_info_names_internal
    .long 0
    .globl __imp__malloc_crt
__imp__malloc_crt:
    .rva .Lhn__malloc_crt
    .long 0
    .globl __imp_free
__imp_free:
    .rva .Lhn_free
    .long 0
    .globl __imp__amsg_exit
__imp__amsg_exit:
    .rva .Lhn__amsg_exit
    .long 0
    .globl __imp___CppXcptFilter
__imp___CppXcptFilter:
    .rva .Lhn___CppXcptFilter
    .long 0
    .globl __imp_strncpy
__imp_strncpy:
    .rva .Lhn_strncpy
    .long 0
    .globl "__imp_??2@YAPEAX_K@Z"
"__imp_??2@YAPEAX_K@Z":
    .rva .Lhn_operator_new
    .long 0
    .globl __imp___crtUnhandledException
__imp___crtUnhandledException:
    .rva .Lhn___crtUnhandledException
    .long 0
    .globl __imp_memset
__imp_memset:
    .rva .Lhn_memset
    .long 0
    .quad 0
    .globl ets2_iat_end
ets2_iat_end:

// Import directory: OriginalFirstThunk, TimeDateStamp,
// ForwarderChain, Name, FirstThunk; KERNEL32.dll, MSVCR110.dll, terminator.
    .section .ets2idata,"dr"
    .p2align 2
    .globl ets2_import_directory
ets2_import_directory:
    .rva .Lilt_kernel32
    .long 0, 0
    .rva .Ldll_kernel32
    .rva ets2_iat_start
    .rva .Lilt_msvcr110
    .long 0, 0
    .rva .Ldll_msvcr110
    .rva .Liat_msvcr110
    .long 0, 0, 0, 0, 0
    .globl ets2_import_directory_end
ets2_import_directory_end:

// Import lookup tables, the IAT's twins.
.Lilt_kernel32:
    .rva .Lhn_MapViewOfFile
    .long 0
    .rva .Lhn_UnmapViewOfFile
    .long 0
    .rva .Lhn_GetLastError
    .long 0
    .rva .Lhn_CreateFileMappingW
    .long 0
    .rva .Lhn_OpenFileMappingW
    .long 0
    .rva .Lhn_CloseHandle
    .long 0
    .rva .Lhn_GetSystemTimeAsFileTime
    .long 0
    .rva .Lhn_GetCurrentThreadId
    .long 0
    .rva .Lhn_QueryPerformanceCounter
    .long 0
    .rva .Lhn_IsProcessorFeaturePresent
    .long 0
    .rva .Lhn_IsDebuggerPresent
    .long 0
    .rva .Lhn_DecodePointer
    .long 0
    .rva .Lhn_EncodePointer
    .long 0
    .rva .Lhn_GetTickCount64
    .long 0
    .quad 0
.Lilt_msvcr110:
    .rva .Lhn__initterm_e
    .long 0
    .rva .Lhn___C_specific_handler
    .long 0
    .rva .Lhn___crt_debugger_hook
    .long 0
    .rva .Lhn__initterm
    .long 0
    .rva .Lhn___crtTerminateProcess
    .long 0
    .rva .Lhn___crtCapturePreviousContext
    .long 0
    .rva .Lhn__lock
    .long 0
    .rva .Lhn__unlock
    .long 0
    .rva .Lhn__calloc_crt
    .long 0
    .rva .Lhn___dllonexit
    .long 0
    .rva .Lhn__onexit
    .long 0
    .rva .Lhn___clean_type_info_names_internal
    .long 0
    .rva .Lhn__malloc_crt
    .long 0
    .rva .Lhn_free
    .long 0
    .rva .Lhn__amsg_exit
    .long 0
    .rva .Lhn___CppXcptFilter
    .long 0
    .rva .Lhn_strncpy
    .long 0
    .rva .Lhn_operator_new
    .long 0
    .rva .Lhn___crtUnhandledException
    .long 0
    .rva .Lhn_memset
    .long 0
    .quad 0

// Hint/name entries and DLL names, each padded
// to an even address.
.Lhn_MapViewOfFile:
    .short 0x03de
    .asciz "MapViewOfFile"
    .p2align 1
.Lhn_UnmapViewOfFile:
    .short 0x05a3
    .asciz "UnmapViewOfFile"
    .p2align 1
.Lhn_GetLastError:
    .short 0x0270
    .asciz "GetLastError"
    .p2align 1
.Lhn_CreateFileMappingW:
    .short 0x00d3
    .asciz "CreateFileMappingW"
    .p2align 1
.Lhn_OpenFileMappingW:
    .short 0x0403
    .asciz "OpenFileMappingW"
    .p2align 1
.Lhn_CloseHandle:
    .short 0x008e
    .asciz "CloseHandle"
    .p2align 1
.Ldll_kernel32:
    .asciz "KERNEL32.dll"
    .p2align 1
.Lhn_operator_new:
    .short 0x0071
    .asciz "??2@YAPEAX_K@Z"
    .p2align 1
.Lhn_strncpy:
    .short 0x0624
    .asciz "strncpy"
    .p2align 1
.Ldll_msvcr110:
    .asciz "MSVCR110.dll"
    .p2align 1
.Lhn___CppXcptFilter:
    .short 0x0161
    .asciz "__CppXcptFilter"
    .p2align 1
.Lhn__amsg_exit:
    .short 0x01e0
    .asciz "_amsg_exit"
    .p2align 1
.Lhn_free:
    .short 0x05a3
    .asciz "free"
    .p2align 1
.Lhn__malloc_crt:
    .short 0x0347
    .asciz "_malloc_crt"
    .p2align 1
.Lhn__initterm:
    .short 0x02c6
    .asciz "_initterm"
    .p2align 1
.Lhn__initterm_e:
    .short 0x02c7
    .asciz "_initterm_e"
    .p2align 1
.Lhn___C_specific_handler:
    .short 0x0160
    .asciz "__C_specific_handler"
    .p2align 1
.Lhn___crt_debugger_hook:
    .short 0x018b
    .asciz "__crt_debugger_hook"
    .p2align 1
.Lhn___crtUnhandledException:
    .short 0x018a
    .asciz "__crtUnhandledException"
    .p2align 1
.Lhn___crtTerminateProcess:
    .short 0x0189
    .asciz "__crtTerminateProcess"
    .p2align 1
.Lhn___crtCapturePreviousContext:
    .short 0x0181
    .asciz "__crtCapturePreviousContext"
    .p2align 1
.Lhn__lock:
    .short 0x0336
    .asciz "_lock"
    .p2align 1
.Lhn__unlock:
    .short 0x049b
    .asciz "_unlock"
    .p2align 1
.Lhn__calloc_crt:
    .short 0x01f6
    .asciz "_calloc_crt"
    .p2align 1
.Lhn___dllonexit:
    .short 0x018d
    .asciz "__dllonexit"
    .p2align 1
.Lhn__onexit:
    .short 0x03dd
    .asciz "_onexit"
    .p2align 1
.Lhn___clean_type_info_names_internal:
    .short 0x017e
    .asciz "__clean_type_info_names_internal"
    .p2align 1
.Lhn_EncodePointer:
    .short 0x0140
    .asciz "EncodePointer"
    .p2align 1
.Lhn_DecodePointer:
    .short 0x0118
    .asciz "DecodePointer"
    .p2align 1
.Lhn_IsDebuggerPresent:
    .short 0x0386
    .asciz "IsDebuggerPresent"
    .p2align 1
.Lhn_IsProcessorFeaturePresent:
    .short 0x038b
    .asciz "IsProcessorFeaturePresent"
    .p2align 1
.Lhn_QueryPerformanceCounter:
    .short 0x043f
    .asciz "QueryPerformanceCounter"
    .p2align 1
.Lhn_GetCurrentThreadId:
    .short 0x022e
    .asciz "GetCurrentThreadId"
    .p2align 1
.Lhn_GetSystemTimeAsFileTime:
    .short 0x02fb
    .asciz "GetSystemTimeAsFileTime"
    .p2align 1
.Lhn_GetTickCount64:
    .short 0x0318
    .asciz "GetTickCount64"
    .p2align 1
.Lhn_memset:
    .short 0x05ed
    .asciz "memset"
    .p2align 1

// The `jmp [IAT]` thunks.
    .section .text$ets2thunk,"xr"
    .p2align 1
    .globl __CppXcptFilter
__CppXcptFilter:
    jmp qword ptr [rip + __imp___CppXcptFilter]
    .globl _amsg_exit
_amsg_exit:
    jmp qword ptr [rip + __imp__amsg_exit]
    .globl _initterm
_initterm:
    jmp qword ptr [rip + __imp__initterm]
    .globl _initterm_e
_initterm_e:
    jmp qword ptr [rip + __imp__initterm_e]
    .globl __C_specific_handler
__C_specific_handler:
    jmp qword ptr [rip + __imp___C_specific_handler]
    .globl __crt_debugger_hook
__crt_debugger_hook:
    jmp qword ptr [rip + __imp___crt_debugger_hook]
    .globl __crtUnhandledException
__crtUnhandledException:
    jmp qword ptr [rip + __imp___crtUnhandledException]
    .globl __crtTerminateProcess
__crtTerminateProcess:
    jmp qword ptr [rip + __imp___crtTerminateProcess]
    .globl __crtCapturePreviousContext
__crtCapturePreviousContext:
    jmp qword ptr [rip + __imp___crtCapturePreviousContext]
    .globl _lock
_lock:
    jmp qword ptr [rip + __imp__lock]
    .globl _unlock
_unlock:
    jmp qword ptr [rip + __imp__unlock]
    .globl __dllonexit
__dllonexit:
    jmp qword ptr [rip + __imp___dllonexit]
    .globl __clean_type_info_names_internal
__clean_type_info_names_internal:
    jmp qword ptr [rip + __imp___clean_type_info_names_internal]
    .globl IsProcessorFeaturePresent
IsProcessorFeaturePresent:
    jmp qword ptr [rip + __imp_IsProcessorFeaturePresent]
// ets2_memset_thunk: the same thunk under a name LLVM does not treat as
// memset (plugin.rs crt_memset), so the plugin's memset calls stay
// separate calls.
    .globl memset
    .globl ets2_memset_thunk
ets2_memset_thunk:
memset:
    jmp qword ptr [rip + __imp_memset]
