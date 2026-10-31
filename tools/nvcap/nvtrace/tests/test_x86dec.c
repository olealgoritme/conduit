/* Host unit tests for src/x86dec.c: instruction lengths, Windows syscall stub
 * patterns, and x64 relocation executed natively (Linux x86_64). */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include "x86dec.h"

static int fails, checks;

#define CHECK(cond, ...) do { checks++; if (!(cond)) { fails++; printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)

typedef struct { const char *name; int is64; int len; uint8_t b[16]; } lencase;

static const lencase cases[] = {
    {"mov r10,rcx", 1, 3, {0x4C, 0x8B, 0xD1}},
    {"mov eax,imm32", 1, 5, {0xB8, 0x12, 0x34, 0, 0}},
    {"test byte [abs32],1", 1, 8, {0xF6, 0x04, 0x25, 0x08, 0x03, 0xFE, 0x7F, 0x01}},
    {"jne rel8", 1, 2, {0x75, 0x03}},
    {"syscall", 1, 2, {0x0F, 0x05}},
    {"ret", 1, 1, {0xC3}},
    {"int 2e", 1, 2, {0xCD, 0x2E}},
    {"mov [rsp+8],rbx", 1, 5, {0x48, 0x89, 0x5C, 0x24, 0x08}},
    {"mov r11,rsp", 1, 3, {0x4C, 0x8B, 0xDC}},
    {"push rbp", 1, 1, {0x55}},
    {"push r12", 1, 2, {0x41, 0x54}},
    {"sub rsp,imm32", 1, 7, {0x48, 0x81, 0xEC, 0x00, 0x01, 0, 0}},
    {"sub rsp,imm8", 1, 4, {0x48, 0x83, 0xEC, 0x28}},
    {"lea rbp,[rsp-0x5f]", 1, 5, {0x48, 0x8D, 0x6C, 0x24, 0xA1}},
    {"mov rax,[rip+x]", 1, 7, {0x48, 0x8B, 0x05, 1, 2, 3, 4}},
    {"mov rax,imm64", 1, 10, {0x48, 0xB8, 1, 2, 3, 4, 5, 6, 7, 8}},
    {"mov qword [rip+x],imm32", 1, 11, {0x48, 0xC7, 0x05, 1, 2, 3, 4, 9, 9, 9, 9}},
    {"cmp byte [rip+x],0", 1, 7, {0x80, 0x3D, 1, 2, 3, 4, 0}},
    {"xor eax,eax", 1, 2, {0x33, 0xC0}},
    {"movaps [rsp+0x40],xmm6", 1, 5, {0x0F, 0x29, 0x74, 0x24, 0x40}},
    {"nop dword [rax+rax]", 1, 5, {0x0F, 0x1F, 0x44, 0x00, 0x00}},
    {"jmp [rip+0]", 1, 6, {0xFF, 0x25, 0, 0, 0, 0}},
    {"call rel32", 1, 5, {0xE8, 0, 0, 0, 0}},
    {"jz rel32", 1, 6, {0x0F, 0x84, 0, 0, 0, 0}},
    {"mov ax,imm16", 1, 4, {0x66, 0xB8, 1, 2}},
    {"test eax,imm32", 1, 5, {0xA9, 1, 2, 3, 4}},
    {"test dword [rcx],imm32", 1, 6, {0xF7, 0x01, 1, 2, 3, 4}},
    {"lock cmpxchg [rcx],edx", 1, 4, {0xF0, 0x0F, 0xB1, 0x11}},
    {"mov rax,gs:[0x30]", 1, 9, {0x65, 0x48, 0x8B, 0x04, 0x25, 0x30, 0, 0, 0}},
    {"pshufd xmm0,xmm1,0", 1, 5, {0x66, 0x0F, 0x70, 0xC1, 0x00}},
    {"x86 mov eax,imm32", 0, 5, {0xB8, 1, 0, 0, 0}},
    {"x86 mov edx,imm32", 0, 5, {0xBA, 0x10, 0x20, 0x30, 0x40}},
    {"x86 call edx", 0, 2, {0xFF, 0xD2}},
    {"x86 ret 4", 0, 3, {0xC2, 0x04, 0x00}},
    {"x86 mov edi,edi", 0, 2, {0x8B, 0xFF}},
    {"x86 push ebp", 0, 1, {0x55}},
    {"x86 mov ebp,esp", 0, 2, {0x8B, 0xEC}},
    {"x86 call fs:[0xc0]", 0, 7, {0x64, 0xFF, 0x15, 0xC0, 0, 0, 0}},
    {"x86 mov eax,[0x1234]", 0, 5, {0xA1, 0x34, 0x12, 0, 0}},
    {"x86 inc eax", 0, 1, {0x40}},
    {"x86 sub esp,0x10", 0, 3, {0x83, 0xEC, 0x10}},
    {"x86 lea ecx,[esp+4]", 0, 4, {0x8D, 0x4C, 0x24, 0x04}},
};

static void test_lengths(void)
{
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        x86_insn d;
        int rc = x86_decode(cases[i].b, cases[i].is64, &d);
        CHECK(rc == 0 && d.len == cases[i].len, "%s: rc=%d len=%d want %d", cases[i].name, rc,
              d.len, cases[i].len);
    }
    x86_insn d;
    static const uint8_t vex[] = {0xC5, 0xF8, 0x77};
    CHECK(x86_decode(vex, 1, &d) == -1, "VEX must be refused in x64");
    static const uint8_t rip[] = {0x48, 0x8B, 0x05, 1, 2, 3, 4};
    x86_decode(rip, 1, &d);
    CHECK(d.rip_disp_off == 3, "rip disp offset %d", d.rip_disp_off);
    x86_decode(rip, 0, &d);
    CHECK(d.rip_disp_off == -1, "no rip-relative in x86");
}

/* win32u x64 stub (Windows 10/11) */
static const uint8_t stub64[] = {
    0x4C, 0x8B, 0xD1,                         /* mov r10, rcx */
    0xB8, 0x45, 0x12, 0x00, 0x00,             /* mov eax, 0x1245 */
    0xF6, 0x04, 0x25, 0x08, 0x03, 0xFE, 0x7F, 0x01, /* test byte [7FFE0308h], 1 */
    0x75, 0x03,                               /* jne +3 */
    0x0F, 0x05,                               /* syscall */
    0xC3,                                     /* ret */
    0xCD, 0x2E,                               /* int 2Eh */
    0xC3,                                     /* ret */
};

/* WOW64 x86 stub */
static const uint8_t stub32[] = {
    0xB8, 0x45, 0x12, 0x00, 0x00,             /* mov eax, 0x1245 */
    0xBA, 0x60, 0x50, 0x30, 0x77,             /* mov edx, Wow64SystemServiceCall */
    0xFF, 0xD2,                               /* call edx */
    0xC2, 0x04, 0x00,                         /* ret 4 */
};

static void test_stubs(void)
{
    uint8_t out[128];
    int olen;
    CHECK(x86_stub_syscall_nr(stub64, 1) == 0x1245, "x64 syscall nr");
    CHECK(x86_stub_syscall_nr(stub32, 0) == 0x1245, "x86 syscall nr");
    int c = x86_relocate(stub64, 0x7FFA00001000ull, 14, out, 0x7FFA10000000ull, 1, &olen);
    CHECK(c == 16 && olen == 16 && !memcmp(out, stub64, 16), "x64 stub relocation c=%d olen=%d", c,
          olen);
    c = x86_relocate(stub64, 0x7FFA00001000ull, 5, out, 0x7FFA10000000ull, 1, &olen);
    CHECK(c == 8 && olen == 8 && !memcmp(out, stub64, 8), "x64 stub 5-byte patch relocation c=%d", c);
    c = x86_relocate(stub32, 0x77001000, 5, out, 0x10000000, 0, &olen);
    CHECK(c == 5 && olen == 5 && !memcmp(out, stub32, 5), "x86 stub relocation c=%d", c);
    x86_relocmap map;
    c = x86_relocate_map(stub64, 0x7FFA00001000ull, 5, out, 0x7FFA10000000ull, 1, &olen, &map);
    CHECK(c == 8 && map.n == 2 && x86_relocmap_lookup(&map, 0) == 0 &&
              x86_relocmap_lookup(&map, 3) == 3 && x86_relocmap_lookup(&map, 1) == -1,
          "x64 stub boundary map n=%d", map.n);
    /* jcc rel8 grows to 16 bytes: the instruction after it moves */
    static const uint8_t jshort[] = {0x74, 0x10, 0x90, 0x90, 0x48, 0x89, 0xC8, 0xC3};
    c = x86_relocate_map(jshort, 0x1000, 5, out, 0x2000, 1, &olen, &map);
    CHECK(c == 7 && map.n == 4 && x86_relocmap_lookup(&map, 2) == 16 &&
              x86_relocmap_lookup(&map, 4) == 18,
          "x64 jcc boundary map c=%d n=%d", c, map.n);
    uint32_t ab = 99;
    CHECK(x86_stub_argbytes(stub32, 0, &ab) == 0 && ab == 4, "x86 argbytes %u", ab);
    static const uint8_t stub32b[] = {0xB8, 1, 0, 0, 0, 0x64, 0xFF, 0x15, 0xC0, 0, 0, 0, 0xC2, 0x2C, 0x00};
    CHECK(x86_stub_argbytes(stub32b, 0, &ab) == 0 && ab == 0x2C, "x86 argbytes fs:[c0] %u", ab);
}

static void test_x86_branches(void)
{
    uint8_t out[128];
    int olen;
    /* call rel32 at 0x1000 to 0x2000, then 9 bytes of padding */
    uint8_t code[16] = {0xE8, 0xFB, 0x0F, 0x00, 0x00, 0x90, 0x90};
    int c = x86_relocate(code, 0x1000, 5, out, 0x500000, 0, &olen);
    int32_t rel;
    memcpy(&rel, out + 1, 4);
    CHECK(c == 5 && out[0] == 0xE8 && (uint32_t)(0x500000 + 5 + rel) == 0x2000,
          "x86 call relocation -> %x", (unsigned)(0x500000 + 5 + rel));
    /* je rel8 (+0x10) becomes 0F 84 rel32 */
    uint8_t jcc[8] = {0x74, 0x10, 0x90, 0x90, 0x90};
    c = x86_relocate(jcc, 0x1000, 5, out, 0x500000, 0, &olen);
    memcpy(&rel, out + 2, 4);
    CHECK(c == 5 && out[0] == 0x0F && out[1] == 0x84 && (uint32_t)(0x500000 + 6 + rel) == 0x1012,
          "x86 jcc relocation");
}

/* ---- native execution (x64 host) */
typedef int (*fn_t)(int);

static uint8_t *code_page, *tramp_page;

static fn_t make_tramp(const uint8_t *src_code, size_t len, size_t at, int *ok)
{
    memcpy(code_page + at, src_code, len);
    uint8_t *t = tramp_page + at;
    int olen;
    int c = x86_relocate(code_page + at, (uint64_t)(uintptr_t)(code_page + at), 14, t,
                         (uint64_t)(uintptr_t)t, 1, &olen);
    *ok = c > 0;
    if (c <= 0)
        return NULL;
    x86_emit_abs_jmp(t + olen, (uint64_t)(uintptr_t)(code_page + at + c), 1);
    /* poison the patched region of the original so a wrong jump-back crashes
     * instead of silently passing */
    memset(code_page + at, 0xCC, 14);
    return (fn_t)(void *)t;
}

static void test_exec(void)
{
    code_page = mmap(NULL, 1 << 16, PROT_READ | PROT_WRITE | PROT_EXEC,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    tramp_page = mmap(code_page + (1 << 20), 1 << 16, PROT_READ | PROT_WRITE | PROT_EXEC,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(code_page != MAP_FAILED && tramp_page != MAP_FAILED, "mmap");
    int ok;

    /* 1. RIP-relative lea: returns the address of a data byte */
    static const uint8_t lea[] = {
        0x48, 0x8D, 0x05, 0x10, 0x00, 0x00, 0x00, /* lea rax, [rip+0x10] */
        0x48, 0x83, 0xC0, 0x00,                   /* add rax, 0 */
        0x0F, 0x1F, 0x40, 0x00,                   /* nop dword [rax] */
        0xC3,                                     /* ret */
    };
    memcpy(code_page + 0x100, lea, sizeof(lea));
    long expect = (long)(uintptr_t)(code_page + 0x100 + 7 + 0x10);
    fn_t f = make_tramp(lea, sizeof(lea), 0x100, &ok);
    long got = f ? ((long (*)(void))(void *)f)() : 0;
    CHECK(ok && (uint32_t)got == (uint32_t)expect, "rip-relative lea %lx vs %lx", got, expect);

    /* 2. jcc rel8 inside the relocated region */
    static const uint8_t jcc[] = {
        0x85, 0xFF,                   /* test edi, edi */
        0x74, 0x0C,                   /* je +12 -> 0x10 */
        0xB8, 0x01, 0x00, 0x00, 0x00, /* mov eax, 1 */
        0x83, 0xC0, 0x01,             /* add eax, 1 */
        0x83, 0xC0, 0x01,             /* add eax, 1 */
        0xC3,                         /* ret */
        0xB8, 0x2A, 0x00, 0x00, 0x00, /* mov eax, 42 */
        0xC3,
    };
    f = make_tramp(jcc, sizeof(jcc), 0x200, &ok);
    CHECK(ok && f && f(1) == 3 && f(0) == 42, "jcc relocation");

    /* 3. call rel32 to a helper outside the region */
    static const uint8_t helper[] = {0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3}; /* mov eax,7; ret */
    memcpy(code_page + 0x400, helper, sizeof(helper));
    uint8_t callc[32] = {
        0x48, 0x83, 0xEC, 0x08,       /* sub rsp, 8 (align) */
        0xE8, 0, 0, 0, 0,             /* call helper */
        0x83, 0xC0, 0x01,             /* add eax, 1 */
        0x83, 0xC0, 0x01,             /* add eax, 1 */
        0x48, 0x83, 0xC4, 0x08,       /* add rsp, 8 */
        0xC3,
    };
    int32_t rel = (int32_t)((code_page + 0x400) - (code_page + 0x300 + 9));
    memcpy(callc + 5, &rel, 4);
    f = make_tramp(callc, 20, 0x300, &ok);
    CHECK(ok && f && f(0) == 9, "call relocation");

    /* 4. jmp rel32 as the first instruction (another hooker's patch) */
    uint8_t jmpc[16] = {0xE9, 0, 0, 0, 0, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90};
    rel = (int32_t)((code_page + 0x400) - (code_page + 0x500 + 5));
    memcpy(jmpc + 1, &rel, 4);
    f = make_tramp(jmpc, 16, 0x500, &ok);
    CHECK(ok && f && f(0) == 7, "jmp relocation");

    /* 5. too short: ret inside the patch window must be refused */
    static const uint8_t shortf[] = {0x31, 0xC0, 0xC3, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
                                     0x90, 0x90, 0x90, 0x90, 0x90, 0x90};
    uint8_t out[128];
    int olen;
    CHECK(x86_relocate(shortf, 0x1000, 14, out, 0x2000, 1, &olen) == -1, "short function refused");

    /* 6b. a branch back into the patched bytes must be refused */
    static const uint8_t loopback[] = {0x90, 0x90, 0x90, 0x75, 0xFB, 0x90, 0x90, 0x90}; /* jne -5 -> +0 */
    CHECK(x86_relocate(loopback, 0x1000, 5, out, 0x2000, 1, &olen) == -1, "branch into patch refused");

    /* 6. rip-relative out of +-2GiB range must be refused */
    CHECK(x86_relocate(lea, 0x7FF000000000ull, 14, out, 0x10000000ull, 1, &olen) == -1,
          "far rip-relative refused");
}

int main(void)
{
    test_lengths();
    test_stubs();
    test_x86_branches();
    test_exec();
    printf("x86dec: %d checks, %d failures\n", checks, fails);
    return fails ? 1 : 0;
}
