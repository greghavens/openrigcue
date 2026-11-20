// i386 part of the DLL start-up (see crt.rs): the
// functions with EH4 (`_except_handler4`) SEH frames, the frame-capturing
// __report_gsfailure, __security_check_cookie and the SEH prolog/epilog
// helpers, with their EH4 scope tables. MSVCR110 calls made through a
// `jmp [IAT]` thunk use the thunk's name, the ones made
// through the IAT use __imp_.

    .section .text$ets2crt,"xr"

// __security_check_cookie, fastcall (ecx).
    .p2align 4
ets2_crt_security_check_cookie:
    cmp ecx, dword ptr [{cookie}]
    jne .Lscc_fail
    rep ret
.Lscc_fail:
    jmp ets2_crt_report_gsfailure

// _DllMainCRTStartup@12: the image entry point.
    .p2align 4
    .globl __DllMainCRTStartup@12
__DllMainCRTStartup@12:
    push ebp
    mov ebp, esp
    cmp dword ptr [ebp + 0xc], 1
    jne .Lentry_go
    call {init_cookie}
.Lentry_go:
    push dword ptr [ebp + 0x10]
    push dword ptr [ebp + 0xc]
    push dword ptr [ebp + 8]
    call ets2_crt_dll_main_crt_startup
    add esp, 0xc
    pop ebp
    ret 0xc

// __DllMainCRTStartup, cdecl, with
//   __try {{ __try {{ ... }} __except (__CppXcptFilter(...)) {{ retcode = 0 }} }}
//   __finally {{ the DllMain reason = -1 }}
    .p2align 4
ets2_crt_dll_main_crt_startup:
    push 0x10
    .byte 0x68                          // push imm32
    .long .Ldmcs_scope
    call ets2_crt_seh_prolog4
    xor eax, eax
    inc eax
    mov esi, eax
    mov dword ptr [ebp - 0x1c], esi
    xor ebx, ebx
    mov dword ptr [ebp - 4], ebx
    mov edi, dword ptr [ebp + 0xc]
    mov dword ptr [{dllmain_reason}], edi
    mov dword ptr [ebp - 4], eax
    test edi, edi
    jne .Ldmcs_not_unattached
    cmp dword ptr [{proc_attached}], edi
    je .Ldmcs_zero
.Ldmcs_not_unattached:
    cmp edi, eax
    je .Ldmcs_attach_or_thread
    cmp edi, 2
    jne .Ldmcs_call_dllmain
.Ldmcs_attach_or_thread:
    mov eax, dword ptr [{raw_dll_main}]
    test eax, eax
    je .Ldmcs_no_raw
    push dword ptr [ebp + 0x10]
    push edi
    push dword ptr [ebp + 8]
    call eax
    mov esi, eax
    mov dword ptr [ebp - 0x1c], esi
.Ldmcs_no_raw:
    test esi, esi
    je .Ldmcs_out
    push dword ptr [ebp + 0x10]
    push edi
    push dword ptr [ebp + 8]
    call {crt_init}
    mov esi, eax
    mov dword ptr [ebp - 0x1c], esi
    test esi, esi
    je .Ldmcs_out
.Ldmcs_call_dllmain:
    push dword ptr [ebp + 0x10]
    push edi
    push dword ptr [ebp + 8]
    call {dll_main}
    mov esi, eax
    mov dword ptr [ebp - 0x1c], esi
    cmp edi, 1
    jne .Ldmcs_detach
    test esi, esi
    jne .Ldmcs_detach
    push dword ptr [ebp + 0x10]
    push ebx
    push dword ptr [ebp + 8]
    call {dll_main}
    push dword ptr [ebp + 0x10]
    push ebx
    push dword ptr [ebp + 8]
    call {crt_init}
    mov eax, dword ptr [{raw_dll_main}]
    test eax, eax
    je .Ldmcs_detach
    push dword ptr [ebp + 0x10]
    push ebx
    push dword ptr [ebp + 8]
    call eax
.Ldmcs_detach:
    test edi, edi
    je .Ldmcs_crt_term
    cmp edi, 3
    jne .Ldmcs_out
.Ldmcs_crt_term:
    push dword ptr [ebp + 0x10]
    push edi
    push dword ptr [ebp + 8]
    call {crt_init}
    neg eax
    sbb eax, eax
    and esi, eax
    mov dword ptr [ebp - 0x1c], esi
    je .Ldmcs_out
    mov eax, dword ptr [{raw_dll_main}]
    test eax, eax
    je .Ldmcs_out
    push dword ptr [ebp + 0x10]
    push edi
    push dword ptr [ebp + 8]
    call eax
    mov esi, eax
    jmp .Ldmcs_store
.Ldmcs_filter:
    mov ecx, dword ptr [ebp - 0x14]
    mov eax, dword ptr [ecx]
    mov eax, dword ptr [eax]
    mov dword ptr [ebp - 0x20], eax
    push ecx
    push eax
    call ___CppXcptFilter
    pop ecx
    pop ecx
    ret
.Ldmcs_handler:
    mov esp, dword ptr [ebp - 0x18]
    xor ebx, ebx
.Ldmcs_zero:
    mov esi, ebx
.Ldmcs_store:
    mov dword ptr [ebp - 0x1c], esi
.Ldmcs_out:
    mov dword ptr [ebp - 4], ebx
    mov dword ptr [ebp - 4], 0xfffffffe
    call .Ldmcs_finally_body
    mov eax, esi
    call ets2_crt_seh_epilog4
    ret
.Ldmcs_finally:
    mov esi, dword ptr [ebp - 0x1c]
.Ldmcs_finally_body:
    mov dword ptr [{dllmain_reason}], 0xffffffff
    ret

// __report_gsfailure. Only reached from
// __security_check_cookie (through _except_handler4_common).
    .p2align 4
ets2_crt_report_gsfailure:
    push ebp
    mov ebp, esp
    sub esp, 0x324
    push 0x17
    call _IsProcessorFeaturePresent@4
    test eax, eax
    je .Lgs_no_fastfail
    push 2
    pop ecx
    int 0x29
.Lgs_no_fastfail:
    mov dword ptr [{gs_context} + 0xb0], eax
    mov dword ptr [{gs_context} + 0xac], ecx
    mov dword ptr [{gs_context} + 0xa8], edx
    mov dword ptr [{gs_context} + 0xa4], ebx
    mov dword ptr [{gs_context} + 0xa0], esi
    mov dword ptr [{gs_context} + 0x9c], edi
    mov word ptr [{gs_context} + 0xc8], ss
    mov word ptr [{gs_context} + 0xbc], cs
    mov word ptr [{gs_context} + 0x98], ds
    mov word ptr [{gs_context} + 0x94], es
    mov word ptr [{gs_context} + 0x90], fs
    mov word ptr [{gs_context} + 0x8c], gs
    pushfd
    pop dword ptr [{gs_context} + 0xc0]
    mov eax, dword ptr [ebp]
    mov dword ptr [{gs_context} + 0xb4], eax
    mov eax, dword ptr [ebp + 4]
    mov dword ptr [{gs_context} + 0xb8], eax
    lea eax, [ebp + 8]
    mov dword ptr [{gs_context} + 0xc4], eax
    mov eax, dword ptr [ebp - 0x324]
    mov dword ptr [{gs_context}], 0x10001
    mov eax, dword ptr [{gs_context} + 0xb8]
    mov dword ptr [{gs_record} + 0xc], eax
    mov dword ptr [{gs_record}], 0xc0000409
    mov dword ptr [{gs_record} + 4], 1
    mov dword ptr [{gs_record} + 0x10], 1
    push 4
    pop eax
    imul eax, eax, 0
    mov dword ptr [eax + {gs_record} + 0x14], 2
    push 4
    pop eax
    imul eax, eax, 0
    mov ecx, dword ptr [{cookie}]
    mov dword ptr [ebp + eax - 8], ecx
    push 4
    pop eax
    shl eax, 0
    mov ecx, dword ptr [{cookie_complement}]
    mov dword ptr [ebp + eax - 8], ecx
    .byte 0x68                          // push imm32
    .long ets2_crt_gs_exception_pointers
    call {raise_securityfailure}
    leave
    ret

// _IsNonwritableInCurrentImage, cdecl, with an inline EH4 frame:
//   __try {{ ... }} __except (code == EXCEPTION_ACCESS_VIOLATION) {{ return 0 }}
    .p2align 4
    .globl _ets2_crt_is_nonwritable_in_current_image
_ets2_crt_is_nonwritable_in_current_image:
    push ebp
    mov ebp, esp
    push 0xfffffffe
    .byte 0x68                          // push imm32
    .long .Linw_scope
    .byte 0x68                          // push imm32
    .long ets2_crt_except_handler4
    mov eax, dword ptr fs:[0]
    push eax
    sub esp, 8
    push ebx
    push esi
    push edi
    mov eax, dword ptr [{cookie}]
    xor dword ptr [ebp - 8], eax
    xor eax, ebp
    push eax
    lea eax, [ebp - 0x10]
    mov dword ptr fs:[0], eax
    mov dword ptr [ebp - 0x18], esp
    mov dword ptr [ebp - 4], 0
    .byte 0x68                          // push imm32
    .long ___ImageBase
    call {validate_image_base}
    add esp, 4
    test eax, eax
    je .Linw_zero
    mov eax, dword ptr [ebp + 8]
    sub eax, offset ___ImageBase
    push eax
    .byte 0x68                          // push imm32
    .long ___ImageBase
    call {find_pe_section}
    add esp, 8
    test eax, eax
    je .Linw_zero
    mov eax, dword ptr [eax + 0x24]
    shr eax, 0x1f
    not eax
    and eax, 1
    mov dword ptr [ebp - 4], 0xfffffffe
    mov ecx, dword ptr [ebp - 0x10]
    mov dword ptr fs:[0], ecx
    pop ecx
    pop edi
    pop esi
    pop ebx
    mov esp, ebp
    pop ebp
    ret
.Linw_filter:
    mov eax, dword ptr [ebp - 0x14]
    mov eax, dword ptr [eax]
    xor ecx, ecx
    cmp dword ptr [eax], 0xc0000005
    sete cl
    mov eax, ecx
    ret
.Linw_handler:
    mov esp, dword ptr [ebp - 0x18]
.Linw_zero:
    mov dword ptr [ebp - 4], 0xfffffffe
    xor eax, eax
    mov ecx, dword ptr [ebp - 0x10]
    mov dword ptr fs:[0], ecx
    pop ecx
    pop edi
    pop esi
    pop ebx
    mov esp, ebp
    pop ebp
    ret

// _onexit, cdecl, the DLL's own (see crt_x86_64.s):
//   __try {{ ... }} __finally {{ _unlock(8) }}
    .p2align 4
    .globl _ets2_crt_onexit
_ets2_crt_onexit:
    push 0x14
    .byte 0x68                          // push imm32
    .long .Lonexit_scope
    call ets2_crt_seh_prolog4
    push dword ptr [{onexitbegin}]
    mov esi, dword ptr [__imp__DecodePointer@4]
    call esi
    mov dword ptr [ebp - 0x1c], eax
    cmp eax, -1
    jne .Lonexit_table
    push dword ptr [ebp + 8]
    call dword ptr [__imp___onexit]
    pop ecx
    jmp .Lonexit_epilog
.Lonexit_table:
    push 8
    call __lock
    pop ecx
    and dword ptr [ebp - 4], 0
    push dword ptr [{onexitbegin}]
    call esi
    mov dword ptr [ebp - 0x1c], eax
    push dword ptr [{onexitend}]
    call esi
    mov dword ptr [ebp - 0x20], eax
    lea eax, [ebp - 0x20]
    push eax
    lea eax, [ebp - 0x1c]
    push eax
    push dword ptr [ebp + 8]
    mov esi, dword ptr [__imp__EncodePointer@4]
    call esi
    push eax
    call ___dllonexit
    add esp, 0xc
    mov edi, eax
    mov dword ptr [ebp - 0x24], edi
    push dword ptr [ebp - 0x1c]
    call esi
    mov dword ptr [{onexitbegin}], eax
    push dword ptr [ebp - 0x20]
    call esi
    mov dword ptr [{onexitend}], eax
    mov dword ptr [ebp - 4], 0xfffffffe
    call .Lonexit_finally_body
    mov eax, edi
.Lonexit_epilog:
    call ets2_crt_seh_epilog4
    ret
.Lonexit_finally:
    mov edi, dword ptr [ebp - 0x24]
.Lonexit_finally_body:
    push 8
    call __unlock
    pop ecx
    ret

// __SEH_prolog4: sets up an EH4 frame.
    .p2align 4
ets2_crt_seh_prolog4:
    .byte 0x68                          // push imm32
    .long ets2_crt_except_handler4
    push dword ptr fs:[0]
    mov eax, dword ptr [esp + 0x10]
    mov dword ptr [esp + 0x10], ebp
    lea ebp, [esp + 0x10]
    sub esp, eax
    push ebx
    push esi
    push edi
    mov eax, dword ptr [{cookie}]
    xor dword ptr [ebp - 4], eax
    xor eax, ebp
    push eax
    mov dword ptr [ebp - 0x18], esp
    push dword ptr [ebp - 8]
    mov eax, dword ptr [ebp - 4]
    mov dword ptr [ebp - 4], 0xfffffffe
    mov dword ptr [ebp - 8], eax
    lea eax, [ebp - 0x10]
    mov dword ptr fs:[0], eax
    ret

// __SEH_epilog4: tears down an EH4 frame.
ets2_crt_seh_epilog4:
    mov ecx, dword ptr [ebp - 0x10]
    mov dword ptr fs:[0], ecx
    pop ecx
    pop edi
    pop edi
    pop esi
    pop ebx
    mov esp, ebp
    pop ebp
    push ecx
    ret

// _except_handler4: MSVCR110 _except_handler4_common with
// this image's cookie and __security_check_cookie.
ets2_crt_except_handler4:
    push ebp
    mov ebp, esp
    push dword ptr [ebp + 0x14]
    push dword ptr [ebp + 0x10]
    push dword ptr [ebp + 0xc]
    push dword ptr [ebp + 8]
    .byte 0x68                          // push imm32
    .long ets2_crt_security_check_cookie
    .byte 0x68                          // push imm32
    .long {cookie}
    call __except_handler4_common
    add esp, 0x18
    pop ebp
    ret

    .section .rdata$ets2crt,"dr"
// Debug directory (0x38 bytes): IMAGE_DEBUG_DIRECTORY entries
// for the CodeView record and the VC_FEATURE record below, with a
// fixed time stamp. AddressOfRawData is the RVA; PointerToRawData
// (+0x18) is a file offset, which only the linked image knows, so it is 0
// here and scripts/pe-link.sh fills it in.
    .p2align 2
    .globl _ets2_debug_directory
_ets2_debug_directory:
    .long 0, 0x55f8677f
    .short 0, 0
    .long 2, 0x6e
    .rva .Ldebug_codeview
    .long 0
    .long 0, 0x55f8677f
    .short 0, 0
    .long 12, 0x10
    .rva .Ldebug_vc_feature
    .long 0
    .globl _ets2_debug_directory_end
_ets2_debug_directory_end:

// The exception pointers of a security failure.
    .p2align 2
ets2_crt_gs_exception_pointers:
    .long {gs_record}
    .long {gs_context}

// _load_config_used: IMAGE_LOAD_CONFIG_DIRECTORY32, Size 0x48,
// SecurityCookie (+0x3c), SEHandlerTable (+0x40) and SEHandlerCount (+0x44).
    .p2align 3
    .globl __load_config_used
__load_config_used:
    .long 0x48
    .zero 0x38
    .long {cookie}
    .long ets2_crt_safe_se_handler_table
    .long 1

// CodeView record (0x6e bytes): RSDS, the PDB GUID
// and age, and the PDB path.
.Ldebug_codeview:
    .ascii "RSDS"
    .byte 0xf4, 0xc9, 0xcf, 0x3b, 0xb2, 0x1b, 0xb5, 0x48
    .byte 0x8b, 0x93, 0xdd, 0x04, 0xa4, 0xe1, 0x76, 0xaf
    .long 5
    .asciz "C:\\Projects\\Software\\ets2-sdk-plugin\\ets2-telemetry\\vs2012\\Release\\ets2-telemetry.pdb"
// VC_FEATURE record (0x10 bytes): Pre-VC++ 11.00 0, C/C++ 16,
// /GS 16, /sdl 0.
    .p2align 2
.Ldebug_vc_feature:
    .long 0, 16, 16, 0

// The SafeSEH handler table: _except_handler4.
    .p2align 2
ets2_crt_safe_se_handler_table:
    .rva ets2_crt_except_handler4

// EH4 scope tables: GSCookieOffset, GSCookieXOROffset, EHCookieOffset,
// EHCookieXOROffset, then {{ EnclosingLevel, Filter, Handler }} records.
    .p2align 2
.Ldmcs_scope:
    .long 0xfffffffe, 0, 0xffffffd0, 0
    .long 0xfffffffe, 0, .Ldmcs_finally
    .long 0, .Ldmcs_filter, .Ldmcs_handler
.Linw_scope:
    .long 0xfffffffe, 0, 0xffffffd8, 0
    .long 0xfffffffe, .Linw_filter, .Linw_handler
.Lonexit_scope:
    .long 0xfffffffe, 0, 0xffffffcc, 0
    .long 0xfffffffe, 0, .Lonexit_finally
