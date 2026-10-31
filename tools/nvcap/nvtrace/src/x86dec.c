/* x86/x64 instruction length decoder and relocation. See x86dec.h. */
#include "x86dec.h"
#include <string.h>

/* one-byte opcodes that take a ModRM byte */
static int op1_modrm(uint8_t op, int is64)
{
    if (op < 0x40)
        return (op & 7) < 4;            /* ALU r/m forms (0x0F and prefixes handled earlier) */
    switch (op) {
    case 0x62: return !is64;            /* bound (x86); EVEX in x64 is refused earlier */
    case 0x63: case 0x69: case 0x6B:
    case 0x80: case 0x81: case 0x82: case 0x83: case 0x84: case 0x85: case 0x86:
    case 0x87: case 0x88: case 0x89: case 0x8A: case 0x8B: case 0x8C: case 0x8D:
    case 0x8E: case 0x8F:
    case 0xC0: case 0xC1: case 0xC6: case 0xC7:
    case 0xD0: case 0xD1: case 0xD2: case 0xD3:
    case 0xD8: case 0xD9: case 0xDA: case 0xDB: case 0xDC: case 0xDD: case 0xDE: case 0xDF:
    case 0xF6: case 0xF7: case 0xFE: case 0xFF:
        return 1;
    case 0xC4: case 0xC5:
        return !is64;                   /* les/lds (x86) */
    }
    return 0;
}

/* two-byte (0F xx) opcodes without ModRM */
static int op2_no_modrm(uint8_t op)
{
    switch (op) {
    case 0x05: case 0x06: case 0x07: case 0x08: case 0x09: case 0x0B: case 0x0E:
    case 0x30: case 0x31: case 0x32: case 0x33: case 0x34: case 0x35: case 0x37:
    case 0x77: case 0xA0: case 0xA1: case 0xA2: case 0xA8: case 0xA9: case 0xAA:
        return 1;
    }
    if (op >= 0x80 && op <= 0x8F)       /* jcc rel32 */
        return 1;
    if (op >= 0xC8 && op <= 0xCF)       /* bswap */
        return 1;
    return 0;
}

int x86_decode(const uint8_t *p, int is64, x86_insn *d)
{
    const uint8_t *s = p;
    int opsize16 = 0, addr16 = 0, rexw = 0, imm = 0;
    uint8_t op;

    memset(d, 0, sizeof(*d));
    d->rip_disp_off = -1;
    d->rel_off = -1;

    for (;;) {
        uint8_t b = *p;
        if (b == 0x66) { opsize16 = 1; p++; continue; }
        if (b == 0x67) { addr16 = 1; p++; continue; }
        if (b == 0xF0 || b == 0xF2 || b == 0xF3 || b == 0x2E || b == 0x36 ||
            b == 0x3E || b == 0x26 || b == 0x64 || b == 0x65) { p++; continue; }
        break;
    }
    if (p - s > 4 || addr16)
        return -1;
    if (is64 && (*p & 0xF0) == 0x40) {
        rexw = (*p & 8) != 0;
        p++;
    }

    op = *p++;
    d->opcode = op;
    if (op == 0x0F) {
        uint8_t op2 = *p++;
        d->two_byte = 1;
        d->opcode = op2;
        if (op2 == 0x38) {
            p++;
            d->has_modrm = 1;
        } else if (op2 == 0x3A) {
            p++;
            d->has_modrm = 1;
            imm = 1;
        } else if (op2 == 0x0F) {
            return -1;                  /* 3DNow! */
        } else if (op2_no_modrm(op2)) {
            if (op2 >= 0x80 && op2 <= 0x8F) {
                d->kind = X86_JCC;
                d->rel_size = 4;
                imm = 4;
            }
        } else {
            d->has_modrm = 1;
            switch (op2) {
            case 0x70: case 0x71: case 0x72: case 0x73:
            case 0xA4: case 0xAC: case 0xBA: case 0xC2: case 0xC4: case 0xC5: case 0xC6:
                imm = 1;
                break;
            }
        }
    } else {
        int immz = opsize16 ? 2 : 4;
        if (is64 && (op == 0x62 || op == 0xC4 || op == 0xC5 || op == 0xD4 || op == 0xD5 ||
                     op == 0xEA || op == 0x9A || (op >= 0x06 && op <= 0x07) || op == 0x0E ||
                     (op >= 0x16 && op <= 0x17) || (op >= 0x1E && op <= 0x1F) ||
                     op == 0x27 || op == 0x2F || op == 0x37 || op == 0x3F || op == 0x60 ||
                     op == 0x61 || op == 0xCE))
            return -1;                  /* invalid in 64-bit mode, or VEX/EVEX */
        d->has_modrm = op1_modrm(op, is64);
        switch (op) {
        case 0x04: case 0x0C: case 0x14: case 0x1C: case 0x24: case 0x2C: case 0x34: case 0x3C:
        case 0x6A: case 0x6B: case 0x80: case 0x82: case 0x83: case 0xA8:
        case 0xC0: case 0xC1: case 0xC6: case 0xCD: case 0xE4: case 0xE5: case 0xE6: case 0xE7:
        case 0xD4: case 0xD5:
            imm = 1;
            break;
        case 0x05: case 0x0D: case 0x15: case 0x1D: case 0x25: case 0x2D: case 0x35: case 0x3D:
        case 0x68: case 0x69: case 0x81: case 0xA9: case 0xC7:
            imm = immz;
            break;
        case 0xC2: case 0xCA:
            imm = 2;
            break;
        case 0xC8:
            imm = 3;
            break;
        case 0xA0: case 0xA1: case 0xA2: case 0xA3:
            imm = is64 ? 8 : 4;
            break;
        case 0xEA: case 0x9A:
            imm = opsize16 ? 4 : 6;
            break;
        }
        if (op >= 0xB0 && op <= 0xB7)
            imm = 1;
        if (op >= 0xB8 && op <= 0xBF)
            imm = rexw ? 8 : immz;
        if (op >= 0x70 && op <= 0x7F) { d->kind = X86_JCC; d->rel_size = 1; imm = 1; }
        if (op == 0xEB) { d->kind = X86_JMP; d->rel_size = 1; imm = 1; }
        if (op == 0xE9) { d->kind = X86_JMP; d->rel_size = 4; imm = 4; }
        if (op == 0xE8) { d->kind = X86_CALL; d->rel_size = 4; imm = 4; }
        if (op >= 0xE0 && op <= 0xE3) { d->kind = X86_LOOP; d->rel_size = 1; imm = 1; }
        if (op == 0xC3 || op == 0xC2 || op == 0xCB || op == 0xCA)
            d->kind = X86_RET;
        if ((op == 0xE8 || op == 0xE9) && opsize16)
            return -1;
    }

    if (d->has_modrm) {
        uint8_t modrm = *p++;
        int mod = modrm >> 6, rm = modrm & 7, reg = (modrm >> 3) & 7;
        if (!d->two_byte && (op == 0xF6 || op == 0xF7) && reg < 2)
            imm = (op == 0xF6) ? 1 : (opsize16 ? 2 : 4);
        if (mod != 3) {
            if (rm == 4) {
                uint8_t sib = *p++;
                if ((sib & 7) == 5 && mod == 0)
                    p += 4;
            } else if (rm == 5 && mod == 0) {
                if (is64)
                    d->rip_disp_off = (int)(p - s);
                p += 4;
            }
            if (mod == 1)
                p += 1;
            else if (mod == 2)
                p += 4;
        }
    }
    if (d->kind == X86_JMP || d->kind == X86_CALL || d->kind == X86_JCC || d->kind == X86_LOOP)
        d->rel_off = (int)(p - s);
    p += imm;
    d->len = (int)(p - s);
    return d->len > 15 ? -1 : 0;
}

uint8_t *x86_emit_abs_jmp(uint8_t *o, uint64_t to, int is64)
{
    if (is64) {
        o[0] = 0xFF;                    /* jmp qword [rip+0] */
        o[1] = 0x25;
        memset(o + 2, 0, 4);
        memcpy(o + 6, &to, 8);
        return o + 14;
    }
    return o;                           /* x86: caller uses emit_rel32 */
}

static uint8_t *emit_rel32(uint8_t *o, uint64_t o_ip, uint8_t opc, uint64_t to)
{
    int32_t rel = (int32_t)(uint32_t)(to - (o_ip + 5));
    o[0] = opc;
    memcpy(o + 1, &rel, 4);
    return o + 5;
}

int x86_relocate(const uint8_t *src, uint64_t src_ip, int need, uint8_t *dst, uint64_t dst_ip,
                 int is64, int *dst_len)
{
    return x86_relocate_map(src, src_ip, need, dst, dst_ip, is64, dst_len, NULL);
}

int x86_relocmap_lookup(const x86_relocmap *map, int off)
{
    for (int i = 0; i < map->n; i++)
        if (map->src_off[i] == off)
            return map->dst_off[i];
    return -1;
}

int x86_relocate_map(const uint8_t *src, uint64_t src_ip, int need, uint8_t *dst, uint64_t dst_ip,
                     int is64, int *dst_len, x86_relocmap *map)
{
    int off = 0;
    uint8_t *o = dst;
    if (map)
        map->n = 0;
    uint64_t targets[32];
    int ntargets = 0;
#define OIP ((uint64_t)(dst_ip + (uint64_t)(o - dst)))
    while (off < need) {
        x86_insn d;
        const uint8_t *ip = src + off;
        if (x86_decode(ip, is64, &d))
            return -1;
        if (map) {
            if (map->n >= X86_RELOC_MAXMAP || off > 255 || o - dst > 255)
                return -1;
            map->src_off[map->n] = (uint8_t)off;
            map->dst_off[map->n] = (uint8_t)(o - dst);
            map->n++;
        }
        uint64_t next = src_ip + (uint64_t)off + (uint64_t)d.len;
        if (d.kind == X86_LOOP)
            return -1;
        if (d.kind == X86_RET && off + d.len < need)
            return -1;                  /* function too short to patch */
        if (d.kind == X86_JMP || d.kind == X86_CALL || d.kind == X86_JCC) {
            int64_t rel = d.rel_size == 1 ? (int8_t)ip[d.rel_off]
                                          : (int32_t)((uint32_t)ip[d.rel_off] |
                                                      ((uint32_t)ip[d.rel_off + 1] << 8) |
                                                      ((uint32_t)ip[d.rel_off + 2] << 16) |
                                                      ((uint32_t)ip[d.rel_off + 3] << 24));
            uint64_t to = next + (uint64_t)rel;
            if (!is64)
                to &= 0xFFFFFFFFu;
            if (ntargets < 32)
                targets[ntargets++] = to;
            if (d.kind == X86_JMP) {
                if (is64)
                    o = x86_emit_abs_jmp(o, to, 1);
                else
                    o = emit_rel32(o, OIP, 0xE9, to);
            } else if (d.kind == X86_CALL) {
                if (is64) {
                    /* call qword [rip+2]; jmp +8; dq to */
                    o[0] = 0xFF; o[1] = 0x15; o[2] = 2; o[3] = 0; o[4] = 0; o[5] = 0;
                    o[6] = 0xEB; o[7] = 0x08;
                    memcpy(o + 8, &to, 8);
                    o += 16;
                } else {
                    o = emit_rel32(o, OIP, 0xE8, to);
                }
            } else {
                uint8_t cc = d.two_byte ? (uint8_t)(d.opcode - 0x80) : (uint8_t)(d.opcode - 0x70);
                if (is64) {
                    /* j!cc +14; jmp qword [rip+0]; dq to */
                    o[0] = (uint8_t)(0x70 | (cc ^ 1));
                    o[1] = 14;
                    o = x86_emit_abs_jmp(o + 2, to, 1);
                } else {
                    int32_t rel32 = (int32_t)(uint32_t)(to - (OIP + 6));
                    o[0] = 0x0F;
                    o[1] = (uint8_t)(0x80 | cc);
                    memcpy(o + 2, &rel32, 4);
                    o += 6;
                }
            }
        } else {
            memcpy(o, ip, d.len);
            if (d.rip_disp_off >= 0) {
                int32_t disp;
                memcpy(&disp, ip + d.rip_disp_off, 4);
                uint64_t to = next + (int64_t)disp;
                if (ntargets < 32)
                    targets[ntargets++] = to;
                int64_t nd = (int64_t)(to - (OIP + (uint64_t)d.len));
                if (nd > INT32_MAX || nd < INT32_MIN)
                    return -1;
                int32_t nd32 = (int32_t)nd;
                memcpy(o + d.rip_disp_off, &nd32, 4);
            }
            o += d.len;
        }
        off += d.len;
    }
#undef OIP
    /* a branch or operand that points into the bytes being replaced would
     * see the patch, not the original code */
    for (int i = 0; i < ntargets; i++)
        if (targets[i] >= src_ip && targets[i] < src_ip + (uint64_t)off)
            return -1;
    *dst_len = (int)(o - dst);
    return off;
}

uint32_t x86_stub_syscall_nr(const uint8_t *p, int is64)
{
    uint32_t nr;
    if (is64) {
        /* 4C 8B D1 B8 imm32: mov r10, rcx; mov eax, imm32 */
        if (p[0] == 0x4C && p[1] == 0x8B && p[2] == 0xD1 && p[3] == 0xB8) {
            memcpy(&nr, p + 4, 4);
            return nr;
        }
    } else if (p[0] == 0xB8) {
        memcpy(&nr, p + 1, 4);
        return nr;
    }
    return 0xFFFFFFFFu;
}

int x86_stub_argbytes(const uint8_t *p, int is64, uint32_t *argbytes)
{
    int off = 0;
    for (int i = 0; i < 12 && off < 48; i++) {
        x86_insn d;
        if (x86_decode(p + off, is64, &d))
            return -1;
        if (!d.two_byte && d.opcode == 0xC2 && d.len >= 3) {
            *argbytes = (uint32_t)p[off + d.len - 2] | ((uint32_t)p[off + d.len - 1] << 8);
            return 0;
        }
        if (!d.two_byte && d.opcode == 0xC3) {
            *argbytes = 0;
            return 0;
        }
        if (d.kind == X86_JMP || d.kind == X86_JCC || d.kind == X86_LOOP)
            return -1;
        off += d.len;
    }
    return -1;
}
