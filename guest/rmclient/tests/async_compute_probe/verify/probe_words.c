/* The same sequence through the probe's own emitters (copied verbatim by the sed in verify.sh). */
#include <stdio.h>
#include <stdint.h>
#define SUBC_HOST 0u
#define SUBC_COMPUTE 1u
#define CLS_COMPUTE 0xcec0u
#define CMP_SET_OBJECT 0x0000u
#define CMP_INVALIDATE_SKED_CACHES 0x0298u
#define CMP_SET_SHADER_SHARED_MEMORY_WINDOW_A 0x02a0u
#define CMP_SEND_PCAS_A 0x02b4u
#define CMP_SEND_SIGNALING_PCAS2_B 0x02c0u
#define CMP_PCAS_ACTION_INVALIDATE_COPY_SCHEDULE 3u
#define CMP_SET_SHADER_LOCAL_MEMORY_WINDOW_A 0x07b0u
static uint32_t mthd(uint32_t subc, uint32_t m, uint32_t count) { return (1u << 29) | (count << 16) | (subc << 13) | (m >> 2); }
#include "emit_snippets.h"
int main(void)
{
    uint32_t p[64];
    unsigned n = emit_compute_setup(p, 0);
    n = emit_dispatch(p, n, 0x123456789ull);
    for (unsigned i = 0; i < n; i++)
        printf("%08x\n", p[i]);
    return 0;
}
