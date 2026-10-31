/* x86/x64 instruction length decoder and prologue relocation for inline
 * hooks. Pure code (no OS calls) so it can be unit-tested on any host. */
#ifndef X86DEC_H
#define X86DEC_H

#include <stdint.h>

typedef struct x86_insn {
    int len;
    int has_modrm;
    int rip_disp_off;   /* offset of a RIP-relative disp32, -1 if none */
    int rel_off;        /* offset of a relative branch immediate, -1 if none */
    int rel_size;       /* 1 or 4 */
    int kind;           /* X86_* */
    uint8_t opcode;     /* last opcode byte */
    int two_byte;       /* 0F-prefixed opcode */
} x86_insn;

enum {
    X86_PLAIN = 0,
    X86_JMP = 1,        /* jmp rel8/rel32 */
    X86_CALL = 2,       /* call rel32 */
    X86_JCC = 3,        /* jcc rel8/rel32 */
    X86_LOOP = 4,       /* loop/jcxz: not relocatable */
    X86_RET = 5,
};

/* 0 on success; -1 for encodings the decoder refuses (VEX/EVEX, 16-bit
 * addressing, far branches in x64, ...). */
int x86_decode(const uint8_t *p, int is64, x86_insn *d);

/* Absolute jump at o (x64: jmp [rip+0]; dq to = 14 bytes; x86: jmp rel32 =
 * 5 bytes, relative to where o executes). Returns the end. */
uint8_t *x86_emit_abs_jmp(uint8_t *o, uint64_t to, int is64);

/* Copy whole instructions starting at src (which executes at src_ip) into
 * dst (which will execute at dst_ip) until at least `need` bytes are
 * covered, fixing RIP-relative operands and relative branches. Returns the
 * number of source bytes consumed, or -1. *dst_len = bytes written.
 * The caller appends a jump back to src_ip + consumed. */
int x86_relocate(const uint8_t *src, uint64_t src_ip, int need, uint8_t *dst, uint64_t dst_ip,
                 int is64, int *dst_len);

/* Syscall number from a stub's `mov eax, imm32` (x64: after mov r10, rcx),
 * or 0xFFFFFFFF if p is not a syscall stub. */
uint32_t x86_stub_syscall_nr(const uint8_t *p, int is64);

/* x86 stdcall stubs: bytes popped by the `ret imm16` that ends the stub.
 * 0 on success. */
int x86_stub_argbytes(const uint8_t *p, int is64, uint32_t *argbytes);

/* Upper bound of bytes x86_relocate may write for `need` source bytes. */
#define X86_RELOC_MAX(need) ((need) * 4 + 64)

#endif
