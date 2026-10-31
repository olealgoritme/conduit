/* x86 helpers for nvtrace (MinGW GAS, cdecl, leading underscores). */
        .intel_syntax noprefix
        .text

/* Entry stub: `mov eax, <nvt_hook*>; jmp _nvt_entry_common`. Arguments are
 * at [esp+4]. nvt_dispatch returns edx:eax = argbytes:result; the stub then
 * pops the stdcall arguments like the original would have. */
        .globl  _nvt_entry_common
_nvt_entry_common:
        push    ebp
        mov     ebp, esp
        lea     ecx, [ebp+8]
        push    dword ptr [ebp+4]
        push    ecx
        push    eax
        call    _nvt_dispatch
        add     esp, 12
        mov     ecx, edx
        pop     ebp
        pop     edx
        add     esp, ecx
        jmp     edx

/* uintptr_t nvt_call_orig(void *fn, const uintptr_t *args, uint32_t nargs)
 * stdcall callee; esp is restored from ebp whatever the callee popped. */
        .globl  _nvt_call_orig
_nvt_call_orig:
        push    ebp
        mov     ebp, esp
        push    esi
        push    edi
        mov     ecx, [ebp+16]
        mov     esi, [ebp+12]
        lea     eax, [ecx*4]
        sub     esp, eax
        mov     edi, esp
        rep movsd
        call    dword ptr [ebp+8]
        lea     esp, [ebp-8]
        pop     edi
        pop     esi
        pop     ebp
        ret

/* int __stdcall nvt_selftest_target{,2,3}(int x): x + 0x1234 / 0x2234 /
 * 0x3234, shaped like win32u stubs (mov eax, imm32 ... ret 4). They start
 * 8-aligned, at 2 mod 8 and at 5 mod 8 (8-byte compare-exchange twice,
 * then the suspended-threads path). */
        .p2align 4
        .globl  _nvt_selftest_target@4
_nvt_selftest_target@4:
        mov     eax, 0x1234
        add     eax, [esp+4]
        ret     4
        .p2align 4
        .byte   0x90, 0x90
        .globl  _nvt_selftest_target2@4
_nvt_selftest_target2@4:
        mov     eax, 0x2234
        add     eax, [esp+4]
        ret     4
        .p2align 4
        .byte   0x90, 0x90, 0x90, 0x90, 0x90
        .globl  _nvt_selftest_target3@4
_nvt_selftest_target3@4:
        mov     eax, 0x3234
        add     eax, [esp+4]
        ret     4
        .p2align 4
