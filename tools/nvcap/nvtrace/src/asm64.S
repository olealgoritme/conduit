/* x64 helpers for nvtrace (MinGW GAS, MS x64 ABI). */
        .intel_syntax noprefix
        .text

/* Every hook's entry stub does `mov rax, <nvt_hook*>; jmp nvt_entry_common`.
 * Spill the four register arguments into the caller's home space so that all
 * arguments are contiguous at [entry rsp + 8], then call
 * nvt_dispatch(hook, args, return_address) and return its result. */
        .globl  nvt_entry_common
        .def    nvt_entry_common; .scl 2; .type 32; .endef
        .seh_proc nvt_entry_common
nvt_entry_common:
        mov     [rsp+8], rcx
        mov     [rsp+16], rdx
        mov     [rsp+24], r8
        mov     [rsp+32], r9
        sub     rsp, 40
        .seh_stackalloc 40
        .seh_endprologue
        mov     rcx, rax
        lea     rdx, [rsp+48]
        mov     r8, [rsp+40]
        call    nvt_dispatch
        add     rsp, 40
        ret
        .seh_endproc

/* uintptr_t nvt_call_orig(void *fn, const uintptr_t *args, uint32_t nargs)
 * Calls fn with nargs arguments taken from args. At least 4 slots are copied
 * (the caller's spilled register arguments are always there). */
        .globl  nvt_call_orig
        .def    nvt_call_orig; .scl 2; .type 32; .endef
        .seh_proc nvt_call_orig
nvt_call_orig:
        push    rbp
        .seh_pushreg rbp
        push    rsi
        .seh_pushreg rsi
        push    rdi
        .seh_pushreg rdi
        mov     rbp, rsp
        .seh_setframe rbp, 0
        .seh_endprologue
        mov     r10, rcx
        mov     rsi, rdx
        mov     eax, r8d
        cmp     eax, 4
        jae     1f
        mov     eax, 4
1:      mov     ecx, eax
        lea     rax, [rax*8+15]
        and     rax, -16
        sub     rsp, rax
        mov     rdi, rsp
        rep movsq
        mov     rcx, [rsp]
        mov     rdx, [rsp+8]
        mov     r8, [rsp+16]
        mov     r9, [rsp+24]
        call    r10
        lea     rsp, [rbp]
        pop     rdi
        pop     rsi
        pop     rbp
        ret
        .seh_endproc

/* int nvt_selftest_target{,2,3}(int x): return x + 0x1234 / 0x2234 /
 * 0x3234. Shaped like ntdll/win32u syscall stubs (mov r10, rcx; mov eax,
 * imm32) so nvinject --selftest can hook them while other threads run
 * through them. The three start 8-aligned, at 4 mod 16 and at 13 mod 16,
 * which exercises the 8-byte, 16-byte and suspended-threads patch paths. */
        .p2align 5
        .globl  nvt_selftest_target
        .def    nvt_selftest_target; .scl 2; .type 32; .endef
nvt_selftest_target:
        .byte   0x4C, 0x8B, 0xD1                /* mov r10, rcx */
        mov     eax, 0x1234
        add     eax, r10d
        ret
        .p2align 5
        .byte   0x90, 0x90, 0x90, 0x90
        .globl  nvt_selftest_target2
        .def    nvt_selftest_target2; .scl 2; .type 32; .endef
nvt_selftest_target2:
        .byte   0x4C, 0x8B, 0xD1
        mov     eax, 0x2234
        add     eax, r10d
        ret
        .p2align 5
        .byte   0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90
        .globl  nvt_selftest_target3
        .def    nvt_selftest_target3; .scl 2; .type 32; .endef
nvt_selftest_target3:
        .byte   0x4C, 0x8B, 0xD1
        mov     eax, 0x3234
        add     eax, r10d
        ret
        .p2align 4
