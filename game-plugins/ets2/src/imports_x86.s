// The import data of the i386 DLL
// (ets2-telemetry_32.dll .rdata): the import address table at the start of
// .rdata (0x8c bytes, KERNEL32 then MSVCR110, each list
// zero-terminated), and after the scope tables the import directory
// (0x3c bytes), the two import lookup tables and the
// hint/name and DLL-name strings. ets2-telemetry.ld places the two sections,
// scripts/pe-link.sh points the import and IAT data directories at
// _ets2_import_directory / _ets2_iat_start.
//
// Every import is defined here (`__imp_` + decorated name, and the
// `jmp [IAT]` thunk for the 14 that are called through one),
// so no member of the mingw-w64 import libraries is
// linked; ets2-telemetry.ld asserts that no `.idata$*` section reaches the
// image.

    .section .ets2iat,"dr"
    .p2align 2
    .globl _ets2_iat_start
_ets2_iat_start:
    .globl "__imp__MapViewOfFile@20"
"__imp__MapViewOfFile@20":
    .rva .Lhn_MapViewOfFile
    .globl "__imp__UnmapViewOfFile@4"
"__imp__UnmapViewOfFile@4":
    .rva .Lhn_UnmapViewOfFile
    .globl "__imp__GetLastError@0"
"__imp__GetLastError@0":
    .rva .Lhn_GetLastError
    .globl "__imp__CreateFileMappingW@24"
"__imp__CreateFileMappingW@24":
    .rva .Lhn_CreateFileMappingW
    .globl "__imp__OpenFileMappingW@12"
"__imp__OpenFileMappingW@12":
    .rva .Lhn_OpenFileMappingW
    .globl "__imp__CloseHandle@4"
"__imp__CloseHandle@4":
    .rva .Lhn_CloseHandle
    .globl "__imp__GetSystemTimeAsFileTime@4"
"__imp__GetSystemTimeAsFileTime@4":
    .rva .Lhn_GetSystemTimeAsFileTime
    .globl "__imp__GetCurrentThreadId@0"
"__imp__GetCurrentThreadId@0":
    .rva .Lhn_GetCurrentThreadId
    .globl "__imp__QueryPerformanceCounter@4"
"__imp__QueryPerformanceCounter@4":
    .rva .Lhn_QueryPerformanceCounter
    .globl "__imp__IsProcessorFeaturePresent@4"
"__imp__IsProcessorFeaturePresent@4":
    .rva .Lhn_IsProcessorFeaturePresent
    .globl "__imp__IsDebuggerPresent@0"
"__imp__IsDebuggerPresent@0":
    .rva .Lhn_IsDebuggerPresent
    .globl "__imp__DecodePointer@4"
"__imp__DecodePointer@4":
    .rva .Lhn_DecodePointer
    .globl "__imp__EncodePointer@4"
"__imp__EncodePointer@4":
    .rva .Lhn_EncodePointer
    .globl "__imp__GetTickCount64@0"
"__imp__GetTickCount64@0":
    .rva .Lhn_GetTickCount64
    .long 0
.Liat_msvcr110:
    .globl __imp___initterm_e
__imp___initterm_e:
    .rva .Lhn__initterm_e
    .globl __imp___crt_debugger_hook
__imp___crt_debugger_hook:
    .rva .Lhn__crt_debugger_hook
    .globl __imp___initterm
__imp___initterm:
    .rva .Lhn__initterm
    .globl __imp____crtTerminateProcess
__imp____crtTerminateProcess:
    .rva .Lhn___crtTerminateProcess
    .globl __imp___lock
__imp___lock:
    .rva .Lhn__lock
    .globl __imp___unlock
__imp___unlock:
    .rva .Lhn__unlock
    .globl __imp___calloc_crt
__imp___calloc_crt:
    .rva .Lhn__calloc_crt
    .globl __imp____dllonexit
__imp____dllonexit:
    .rva .Lhn___dllonexit
    .globl __imp___onexit
__imp___onexit:
    .rva .Lhn__onexit
    .globl __imp____clean_type_info_names_internal
__imp____clean_type_info_names_internal:
    .rva .Lhn___clean_type_info_names_internal
    .globl __imp___except_handler4_common
__imp___except_handler4_common:
    .rva .Lhn__except_handler4_common
    .globl __imp___malloc_crt
__imp___malloc_crt:
    .rva .Lhn__malloc_crt
    .globl __imp__free
__imp__free:
    .rva .Lhn_free
    .globl __imp___amsg_exit
__imp___amsg_exit:
    .rva .Lhn__amsg_exit
    .globl __imp____CppXcptFilter
__imp____CppXcptFilter:
    .rva .Lhn___CppXcptFilter
    .globl __imp__strncpy
__imp__strncpy:
    .rva .Lhn_strncpy
    .globl "__imp_??2@YAPAXI@Z"
"__imp_??2@YAPAXI@Z":
    .rva .Lhn_operator_new
    .globl __imp____crtUnhandledException
__imp____crtUnhandledException:
    .rva .Lhn___crtUnhandledException
    .globl __imp__memset
__imp__memset:
    .rva .Lhn_memset
    .long 0
    .globl _ets2_iat_end
_ets2_iat_end:

// Import directory: OriginalFirstThunk, TimeDateStamp,
// ForwarderChain, Name, FirstThunk; KERNEL32.dll, MSVCR110.dll, terminator.
    .section .ets2idata,"dr"
    .p2align 2
    .globl _ets2_import_directory
_ets2_import_directory:
    .rva .Lilt_kernel32
    .long 0, 0
    .rva .Ldll_kernel32
    .rva _ets2_iat_start
    .rva .Lilt_msvcr110
    .long 0, 0
    .rva .Ldll_msvcr110
    .rva .Liat_msvcr110
    .long 0, 0, 0, 0, 0
    .globl _ets2_import_directory_end
_ets2_import_directory_end:

// Import lookup tables, the IAT's twins.
.Lilt_kernel32:
    .rva .Lhn_MapViewOfFile
    .rva .Lhn_UnmapViewOfFile
    .rva .Lhn_GetLastError
    .rva .Lhn_CreateFileMappingW
    .rva .Lhn_OpenFileMappingW
    .rva .Lhn_CloseHandle
    .rva .Lhn_GetSystemTimeAsFileTime
    .rva .Lhn_GetCurrentThreadId
    .rva .Lhn_QueryPerformanceCounter
    .rva .Lhn_IsProcessorFeaturePresent
    .rva .Lhn_IsDebuggerPresent
    .rva .Lhn_DecodePointer
    .rva .Lhn_EncodePointer
    .rva .Lhn_GetTickCount64
    .long 0
.Lilt_msvcr110:
    .rva .Lhn__initterm_e
    .rva .Lhn__crt_debugger_hook
    .rva .Lhn__initterm
    .rva .Lhn___crtTerminateProcess
    .rva .Lhn__lock
    .rva .Lhn__unlock
    .rva .Lhn__calloc_crt
    .rva .Lhn___dllonexit
    .rva .Lhn__onexit
    .rva .Lhn___clean_type_info_names_internal
    .rva .Lhn__except_handler4_common
    .rva .Lhn__malloc_crt
    .rva .Lhn_free
    .rva .Lhn__amsg_exit
    .rva .Lhn___CppXcptFilter
    .rva .Lhn_strncpy
    .rva .Lhn_operator_new
    .rva .Lhn___crtUnhandledException
    .rva .Lhn_memset
    .long 0

// Hint/name entries and DLL names, each padded to
// an even address.
.Lhn_MapViewOfFile:
    .short 0x03db
    .asciz "MapViewOfFile"
    .p2align 1
.Lhn_UnmapViewOfFile:
    .short 0x0593
    .asciz "UnmapViewOfFile"
    .p2align 1
.Lhn_GetLastError:
    .short 0x026a
    .asciz "GetLastError"
    .p2align 1
.Lhn_CreateFileMappingW:
    .short 0x00d3
    .asciz "CreateFileMappingW"
    .p2align 1
.Lhn_OpenFileMappingW:
    .short 0x0400
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
    .asciz "??2@YAPAXI@Z"
    .p2align 1
.Lhn_strncpy:
    .short 0x064d
    .asciz "strncpy"
    .p2align 1
.Ldll_msvcr110:
    .asciz "MSVCR110.dll"
    .p2align 1
.Lhn___CppXcptFilter:
    .short 0x0173
    .asciz "__CppXcptFilter"
    .p2align 1
.Lhn__amsg_exit:
    .short 0x0205
    .asciz "_amsg_exit"
    .p2align 1
.Lhn_free:
    .short 0x05d4
    .asciz "free"
    .p2align 1
.Lhn__malloc_crt:
    .short 0x037d
    .asciz "_malloc_crt"
    .p2align 1
.Lhn__initterm:
    .short 0x02ee
    .asciz "_initterm"
    .p2align 1
.Lhn__initterm_e:
    .short 0x02ef
    .asciz "_initterm_e"
    .p2align 1
.Lhn__crt_debugger_hook:
    .short 0x023b
    .asciz "_crt_debugger_hook"
    .p2align 1
.Lhn___crtUnhandledException:
    .short 0x019a
    .asciz "__crtUnhandledException"
    .p2align 1
.Lhn___crtTerminateProcess:
    .short 0x0199
    .asciz "__crtTerminateProcess"
    .p2align 1
.Lhn__lock:
    .short 0x036c
    .asciz "_lock"
    .p2align 1
.Lhn__unlock:
    .short 0x04d6
    .asciz "_unlock"
    .p2align 1
.Lhn__calloc_crt:
    .short 0x021b
    .asciz "_calloc_crt"
    .p2align 1
.Lhn___dllonexit:
    .short 0x019c
    .asciz "__dllonexit"
    .p2align 1
.Lhn__onexit:
    .short 0x0412
    .asciz "_onexit"
    .p2align 1
.Lhn___clean_type_info_names_internal:
    .short 0x018f
    .asciz "__clean_type_info_names_internal"
    .p2align 1
.Lhn__except_handler4_common:
    .short 0x0260
    .asciz "_except_handler4_common"
    .p2align 1
.Lhn_EncodePointer:
    .short 0x013c
    .asciz "EncodePointer"
    .p2align 1
.Lhn_DecodePointer:
    .short 0x0117
    .asciz "DecodePointer"
    .p2align 1
.Lhn_IsDebuggerPresent:
    .short 0x0383
    .asciz "IsDebuggerPresent"
    .p2align 1
.Lhn_IsProcessorFeaturePresent:
    .short 0x0388
    .asciz "IsProcessorFeaturePresent"
    .p2align 1
.Lhn_QueryPerformanceCounter:
    .short 0x043c
    .asciz "QueryPerformanceCounter"
    .p2align 1
.Lhn_GetCurrentThreadId:
    .short 0x0228
    .asciz "GetCurrentThreadId"
    .p2align 1
.Lhn_GetSystemTimeAsFileTime:
    .short 0x02f4
    .asciz "GetSystemTimeAsFileTime"
    .p2align 1
.Lhn_GetTickCount64:
    .short 0x0311
    .asciz "GetTickCount64"
    .p2align 1
.Lhn_memset:
    .short 0x061c
    .asciz "memset"
    .p2align 1

// The `jmp [IAT]` thunks.
    .section .text$ets2thunk,"xr"
    .p2align 1
    .globl ___CppXcptFilter
___CppXcptFilter:
    jmp dword ptr [__imp____CppXcptFilter]
    .globl __amsg_exit
__amsg_exit:
    jmp dword ptr [__imp___amsg_exit]
    .globl __initterm
__initterm:
    jmp dword ptr [__imp___initterm]
    .globl __initterm_e
__initterm_e:
    jmp dword ptr [__imp___initterm_e]
    .globl __crt_debugger_hook
__crt_debugger_hook:
    jmp dword ptr [__imp___crt_debugger_hook]
    .globl ___crtUnhandledException
___crtUnhandledException:
    jmp dword ptr [__imp____crtUnhandledException]
    .globl ___crtTerminateProcess
___crtTerminateProcess:
    jmp dword ptr [__imp____crtTerminateProcess]
    .globl __lock
__lock:
    jmp dword ptr [__imp___lock]
    .globl __unlock
__unlock:
    jmp dword ptr [__imp___unlock]
    .globl ___dllonexit
___dllonexit:
    jmp dword ptr [__imp____dllonexit]
    .globl ___clean_type_info_names_internal
___clean_type_info_names_internal:
    jmp dword ptr [__imp____clean_type_info_names_internal]
    .globl __except_handler4_common
__except_handler4_common:
    jmp dword ptr [__imp___except_handler4_common]
    .globl "_IsProcessorFeaturePresent@4"
"_IsProcessorFeaturePresent@4":
    jmp dword ptr ["__imp__IsProcessorFeaturePresent@4"]
// _ets2_memset_thunk: the same thunk under a name LLVM does not treat as
// memset (plugin.rs crt_memset), so the plugin's memset calls stay
// separate calls.
    .globl _memset
    .globl _ets2_memset_thunk
_ets2_memset_thunk:
_memset:
    jmp dword ptr [__imp__memset]
