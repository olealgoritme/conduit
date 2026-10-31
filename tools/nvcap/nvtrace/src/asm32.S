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
