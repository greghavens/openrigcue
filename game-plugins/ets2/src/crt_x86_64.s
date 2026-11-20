// x86_64 part of the DLL start-up (see crt.rs): the
// functions that need SEH scope tables or their caller's frame, with their
// __C_specific_handler scope records. MSVCR110 calls made through a
// `jmp [IAT]` thunk use the thunk's bare name, the ones
// made through the IAT use __imp_.

    .section .text$ets2crt,"xr"

// __security_check_cookie. Unreferenced.
    .p2align 4
    .def ets2_crt_security_check_cookie; .scl 3; .type 32; .endef
ets2_crt_security_check_cookie:
.seh_proc ets2_crt_security_check_cookie
.seh_endprologue
    cmp rcx, qword ptr [rip + {cookie}]
    jne .Lscc_fail
    rol rcx, 16
    test cx, 0xffff
    jne .Lscc_restore
    rep ret
.Lscc_restore:
    ror rcx, 16
.Lscc_fail:
    jmp ets2_crt_report_gsfailure
.seh_endproc

// _DllMainCRTStartup: the image entry point.
    .p2align 4
    .globl _DllMainCRTStartup
    .def _DllMainCRTStartup; .scl 2; .type 32; .endef
_DllMainCRTStartup:
.seh_proc _DllMainCRTStartup
    mov qword ptr [rsp + 8], rbx
    mov qword ptr [rsp + 0x10], rsi
    push rdi
.seh_pushreg rdi
    sub rsp, 0x20
.seh_stackalloc 0x20
.seh_savereg rbx, 0x30
.seh_savereg rsi, 0x38
.seh_endprologue
    mov rdi, r8
    mov ebx, edx
    mov rsi, rcx
    cmp edx, 1
    jne .Lentry_go
    call {init_cookie}
.Lentry_go:
    mov r8, rdi
    mov edx, ebx
    mov rcx, rsi
    mov rbx, qword ptr [rsp + 0x30]
    mov rsi, qword ptr [rsp + 0x38]
    add rsp, 0x20
    pop rdi
    jmp ets2_crt_dll_main_crt_startup
.seh_endproc

// __DllMainCRTStartup, with
//   __try {{ __try {{ ... }} __except (__CppXcptFilter(...)) {{ retcode = 0 }} }}
//   __finally {{ the DllMain reason = -1 }}
    .p2align 4
    .def ets2_crt_dll_main_crt_startup; .scl 3; .type 32; .endef
ets2_crt_dll_main_crt_startup:
.seh_proc ets2_crt_dll_main_crt_startup
.seh_handler __C_specific_handler, @unwind, @except
    mov rax, rsp
    mov qword ptr [rax + 8], rbx
    mov qword ptr [rax + 0x10], rsi
    mov qword ptr [rax + 0x18], rdi
    push r14
.seh_pushreg r14
    sub rsp, 0x30
.seh_stackalloc 0x30
.seh_savereg rbx, 0x40
.seh_savereg rsi, 0x48
.seh_savereg rdi, 0x50
.seh_endprologue
    mov rsi, r8
    mov edi, edx
    mov r14, rcx
    mov ebx, 1
    mov dword ptr [rax - 0x18], ebx
.Ldmcs_finally_begin:
    mov dword ptr [rip + {dllmain_reason}], edx
.Ldmcs_try_begin:
    test edx, edx
    jne .Ldmcs_not_unattached
    cmp dword ptr [rip + {proc_attached}], edx
    jne .Ldmcs_not_unattached
    xor ebx, ebx
    mov dword ptr [rax - 0x18], ebx
    jmp .Ldmcs_leave
.Ldmcs_not_unattached:
    lea eax, [rdx - 1]
    cmp eax, 1
    ja .Ldmcs_call_dllmain
    mov rax, qword ptr [rip + {raw_dll_main}]
    test rax, rax
    je .Ldmcs_no_raw
    call rax
    mov ebx, eax
    mov dword ptr [rsp + 0x20], eax
.Ldmcs_no_raw:
    test ebx, ebx
    je .Ldmcs_leave
    mov r8, rsi
    mov edx, edi
    mov rcx, r14
    call {crt_init}
    mov ebx, eax
    mov dword ptr [rsp + 0x20], eax
    test eax, eax
    je .Ldmcs_leave
.Ldmcs_call_dllmain:
    mov r8, rsi
    mov edx, edi
    mov rcx, r14
    call {dll_main}
    mov ebx, eax
    mov dword ptr [rsp + 0x20], eax
    cmp edi, 1
    jne .Ldmcs_detach
    test eax, eax
    jne .Ldmcs_detach
    mov r8, rsi
    xor edx, edx
    mov rcx, r14
    call {dll_main}
    mov r8, rsi
    xor edx, edx
    mov rcx, r14
    call {crt_init}
    mov rax, qword ptr [rip + {raw_dll_main}]
    test rax, rax
    je .Ldmcs_detach
    mov r8, rsi
    xor edx, edx
    mov rcx, r14
    call rax
.Ldmcs_detach:
    test edi, edi
    je .Ldmcs_crt_term
    cmp edi, 3
    jne .Ldmcs_leave
.Ldmcs_crt_term:
    mov r8, rsi
    mov edx, edi
    mov rcx, r14
    call {crt_init}
    neg eax
    sbb ecx, ecx
    and ecx, ebx
    mov ebx, ecx
    mov dword ptr [rsp + 0x20], ecx
    je .Ldmcs_leave
    mov rax, qword ptr [rip + {raw_dll_main}]
    test rax, rax
    je .Ldmcs_leave
    mov r8, rsi
    mov edx, edi
    mov rcx, r14
    call rax
    mov ebx, eax
    mov dword ptr [rsp + 0x20], eax
.Ldmcs_leave:
    jmp .Ldmcs_finally
.Ldmcs_except:
    xor ebx, ebx
    mov dword ptr [rsp + 0x20], ebx
.Ldmcs_finally:
    mov dword ptr [rip + {dllmain_reason}], 0xffffffff
    mov eax, ebx
    mov rbx, qword ptr [rsp + 0x40]
    mov rsi, qword ptr [rsp + 0x48]
    mov rdi, qword ptr [rsp + 0x50]
    add rsp, 0x30
    pop r14
    ret
.seh_handlerdata
    .long 2
    .rva .Ldmcs_try_begin, .Ldmcs_except, ets2_crt_dmcs_filter, .Ldmcs_except
    .rva .Ldmcs_finally_begin, .Ldmcs_finally, ets2_crt_dmcs_finally
    .long 0
    .section .text$ets2crt,"xr"
.seh_endproc

// __report_gsfailure. Only reached from
// __security_check_cookie, which nothing calls.
    .p2align 4
    .def ets2_crt_report_gsfailure; .scl 3; .type 32; .endef
ets2_crt_report_gsfailure:
.seh_proc ets2_crt_report_gsfailure
    mov qword ptr [rsp + 8], rcx
    sub rsp, 0x38
.seh_stackalloc 0x38
.seh_endprologue
    mov ecx, 0x17
    call IsProcessorFeaturePresent
    test eax, eax
    je .Lgs_no_fastfail
    mov ecx, 2
    int 0x29
.Lgs_no_fastfail:
    lea rcx, [rip + {gs_context}]
    call __crtCapturePreviousContext
    mov rax, qword ptr [rsp + 0x38]
    mov qword ptr [rip + {gs_context} + 0xf8], rax
    lea rax, [rsp + 0x38]
    add rax, 8
    mov qword ptr [rip + {gs_context} + 0x98], rax
    mov rax, qword ptr [rip + {gs_context} + 0xf8]
    mov qword ptr [rip + {gs_record} + 0x10], rax
    mov rax, qword ptr [rsp + 0x40]
    mov qword ptr [rip + {gs_context} + 0x80], rax
    mov dword ptr [rip + {gs_record}], 0xc0000409
    mov dword ptr [rip + {gs_record} + 4], 1
    mov dword ptr [rip + {gs_record} + 0x18], 1
    mov eax, 8
    imul rax, rax, 0
    lea rcx, [rip + {gs_record} + 0x20]
    mov qword ptr [rcx + rax], 2
    mov eax, 8
    imul rax, rax, 0
    mov rcx, qword ptr [rip + {cookie}]
    mov qword ptr [rsp + rax + 0x20], rcx
    mov eax, 8
    imul rax, rax, 1
    mov rcx, qword ptr [rip + {cookie_complement}]
    mov qword ptr [rsp + rax + 0x20], rcx
    lea rcx, [rip + ets2_crt_gs_exception_pointers]
    call {raise_securityfailure}
    add rsp, 0x38
    ret
.seh_endproc

// _IsNonwritableInCurrentImage:
//   __try {{ ... }} __except (code == EXCEPTION_ACCESS_VIOLATION) {{ return 0 }}
    .p2align 4
    .globl ets2_crt_is_nonwritable_in_current_image
    .def ets2_crt_is_nonwritable_in_current_image; .scl 2; .type 32; .endef
ets2_crt_is_nonwritable_in_current_image:
.seh_proc ets2_crt_is_nonwritable_in_current_image
.seh_handler __C_specific_handler, @unwind, @except
    mov qword ptr [rsp + 8], rbx
    push rdi
.seh_pushreg rdi
    sub rsp, 0x20
.seh_stackalloc 0x20
.seh_savereg rbx, 0x30
.seh_endprologue
    mov rbx, rcx
.Linw_try_begin:
    lea rdi, [rip + __ImageBase]
    mov rcx, rdi
    call {validate_image_base}
    test eax, eax
    je .Linw_done
    sub rbx, rdi
    mov rdx, rbx
    mov rcx, rdi
    call {find_pe_section}
    test rax, rax
    je .Linw_done
    mov eax, dword ptr [rax + 0x24]
    shr eax, 0x1f
    not eax
    and eax, 1
    jmp .Linw_done
.Linw_except:
    xor eax, eax
.Linw_done:
    mov rbx, qword ptr [rsp + 0x30]
    add rsp, 0x20
    pop rdi
    ret
.seh_handlerdata
    .long 1
    .rva .Linw_try_begin, .Linw_except, ets2_crt_inw_filter, .Linw_except
    .section .text$ets2crt,"xr"
.seh_endproc

// _onexit, the DLL's own: registers with the DLL's encoded
// table through MSVCR110 __dllonexit under _lock(_EXIT_LOCK1), or with
// MSVCR110's _onexit once the table is gone (-1).
//   __try {{ ... }} __finally {{ _unlock(8) }}
    .p2align 4
    .globl ets2_crt_onexit
    .def ets2_crt_onexit; .scl 2; .type 32; .endef
ets2_crt_onexit:
.seh_proc ets2_crt_onexit
.seh_handler __C_specific_handler, @unwind
    push rbx
.seh_pushreg rbx
    sub rsp, 0x20
.seh_stackalloc 0x20
.seh_endprologue
    mov rbx, rcx
    mov rcx, qword ptr [rip + {onexitbegin}]
    call qword ptr [rip + __imp_DecodePointer]
    mov qword ptr [rsp + 0x38], rax
    cmp rax, -1
    jne .Lonexit_table
    mov rcx, rbx
    call qword ptr [rip + __imp__onexit]
    jmp .Lonexit_ret
.Lonexit_table:
    mov ecx, 8
    call _lock
    nop
.Lonexit_try_begin:
    mov rcx, qword ptr [rip + {onexitbegin}]
    call qword ptr [rip + __imp_DecodePointer]
    mov qword ptr [rsp + 0x38], rax
    mov rcx, qword ptr [rip + {onexitend}]
    call qword ptr [rip + __imp_DecodePointer]
    mov qword ptr [rsp + 0x40], rax
    mov rcx, rbx
    call qword ptr [rip + __imp_EncodePointer]
    mov rcx, rax
    lea r8, [rsp + 0x40]
    lea rdx, [rsp + 0x38]
    call __dllonexit
    mov rbx, rax
    mov rcx, qword ptr [rsp + 0x38]
    call qword ptr [rip + __imp_EncodePointer]
    mov qword ptr [rip + {onexitbegin}], rax
    mov rcx, qword ptr [rsp + 0x40]
    call qword ptr [rip + __imp_EncodePointer]
    mov qword ptr [rip + {onexitend}], rax
.Lonexit_try_end:
    mov ecx, 8
    call _unlock
    mov rax, rbx
.Lonexit_ret:
    add rsp, 0x20
    pop rbx
    ret
.seh_handlerdata
    .long 1
    .rva .Lonexit_try_begin, .Lonexit_try_end, ets2_crt_onexit_finally
    .long 0
    .section .text$ets2crt,"xr"
.seh_endproc

// Filter of __DllMainCRTStartup's __except.
    .p2align 4
    .def ets2_crt_dmcs_filter; .scl 3; .type 32; .endef
ets2_crt_dmcs_filter:
.seh_proc ets2_crt_dmcs_filter
    push rbp
.seh_pushreg rbp
    sub rsp, 0x20
.seh_stackalloc 0x20
.seh_endprologue
    mov rbp, rdx
    mov rdx, rcx
    mov qword ptr [rbp + 0x28], rcx
    mov rax, qword ptr [rcx]
    mov ecx, dword ptr [rax]
    mov dword ptr [rbp + 0x24], ecx
    call __CppXcptFilter
    nop
    add rsp, 0x20
    pop rbp
    ret
.seh_endproc

// __DllMainCRTStartup's __finally.
    .p2align 4
    .def ets2_crt_dmcs_finally; .scl 3; .type 32; .endef
ets2_crt_dmcs_finally:
.seh_proc ets2_crt_dmcs_finally
    push rbp
.seh_pushreg rbp
    sub rsp, 0x20
.seh_stackalloc 0x20
.seh_endprologue
    mov rbp, rdx
    mov dword ptr [rip + {dllmain_reason}], 0xffffffff
    add rsp, 0x20
    pop rbp
    ret
.seh_endproc

// _IsNonwritableInCurrentImage's filter.
    .p2align 4
    .def ets2_crt_inw_filter; .scl 3; .type 32; .endef
ets2_crt_inw_filter:
.seh_proc ets2_crt_inw_filter
    push rbp
.seh_pushreg rbp
    sub rsp, 0x20
.seh_stackalloc 0x20
.seh_endprologue
    mov rbp, rdx
    mov rax, qword ptr [rcx]
    xor ecx, ecx
    cmp dword ptr [rax], 0xc0000005
    sete cl
    mov eax, ecx
    add rsp, 0x20
    pop rbp
    ret
.seh_endproc

// _onexit's __finally.
    .p2align 4
    .def ets2_crt_onexit_finally; .scl 3; .type 32; .endef
ets2_crt_onexit_finally:
.seh_proc ets2_crt_onexit_finally
    push rbp
.seh_pushreg rbp
    sub rsp, 0x20
.seh_stackalloc 0x20
.seh_endprologue
    mov rbp, rdx
    mov ecx, 8
    call _unlock
    nop
    add rsp, 0x20
    pop rbp
    ret
.seh_endproc

    .section .rdata$ets2crt,"dr"
// Debug directory (0x38 bytes): IMAGE_DEBUG_DIRECTORY entries
// for the CodeView record and the VC_FEATURE record below, with a
// fixed time stamp. AddressOfRawData is the RVA; PointerToRawData
// (+0x18) is a file offset, which only the linked image knows, so it is 0
// here and scripts/pe-link.sh fills it in.
    .p2align 2
    .globl ets2_debug_directory
ets2_debug_directory:
    .long 0, 0x55f867da
    .short 0, 0
    .long 2, 0x6e
    .rva .Ldebug_codeview
    .long 0
    .long 0, 0x55f867da
    .short 0, 0
    .long 12, 0x10
    .rva .Ldebug_vc_feature
    .long 0
    .globl ets2_debug_directory_end
ets2_debug_directory_end:

// The exception pointers of a security failure.
    .p2align 3
ets2_crt_gs_exception_pointers:
    .quad {gs_record}
    .quad {gs_context}

// _load_config_used: IMAGE_LOAD_CONFIG_DIRECTORY64, Size 0x70,
// everything zero except SecurityCookie (+0x58).
    .p2align 4
    .globl _load_config_used
_load_config_used:
    .long 0x70
    .zero 0x54
    .quad {cookie}
    .zero 0x10

// CodeView record (0x6e bytes): RSDS, the PDB GUID
// and age, and the PDB path.
.Ldebug_codeview:
    .ascii "RSDS"
    .byte 0xbb, 0x60, 0x4e, 0x7c, 0xe1, 0x0a, 0xc2, 0x4d
    .byte 0x9e, 0x43, 0x98, 0xc4, 0xd7, 0x0b, 0x5d, 0x5e
    .long 1
    .asciz "C:\\Projects\\Software\\ets2-sdk-plugin\\ets2-telemetry\\vs2012\\Release\\ets2-telemetry.pdb"
// VC_FEATURE record (0x10 bytes): Pre-VC++ 11.00 0, C/C++ 15,
// /GS 15, /sdl 0.
    .p2align 2
.Ldebug_vc_feature:
    .long 0, 15, 15, 0
