/* Cross-checks the probe's hand-encoded compute methods against Mesa's own
 * generated push macros (the ones NVK uses).  Build: see verify.sh. */
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include "nv_push.h"
#include "nv_push_cla0c0.h"
#include "nv_push_clb1c0.h"
#include "nv_push_clc3c0.h"
#include "nv_push_clc6c0.h"

int main(void)
{
    uint32_t buf[64];
    struct nv_push push;
    nv_push_init(&push, buf, 64, 0xff);
    struct nv_push *p = &push;
    P_MTHD(p, NVA0C0, SET_OBJECT);
    P_NVA0C0_SET_OBJECT(p, { .class_id = 0xcec0, .engine_id = 0 });
    P_MTHD(p, NVC3C0, SET_SHADER_SHARED_MEMORY_WINDOW_A);
    P_NVC3C0_SET_SHADER_SHARED_MEMORY_WINDOW_A(p, 1ull);
    P_NVC3C0_SET_SHADER_SHARED_MEMORY_WINDOW_B(p, 0);
    P_MTHD(p, NVC3C0, SET_SHADER_LOCAL_MEMORY_WINDOW_A);
    P_NVC3C0_SET_SHADER_LOCAL_MEMORY_WINDOW_A(p, 0);
    P_NVC3C0_SET_SHADER_LOCAL_MEMORY_WINDOW_B(p, 0xff000000u);
    P_IMMD(p, NVB1C0, INVALIDATE_SKED_CACHES, 0);
    P_MTHD(p, NVA0C0, SEND_PCAS_A);
    P_NVA0C0_SEND_PCAS_A(p, 0x123456789ull >> 8);
    P_IMMD(p, NVC6C0, SEND_SIGNALING_PCAS2_B, PCAS_ACTION_INVALIDATE_COPY_SCHEDULE);
    for (uint32_t *w = buf; w < push.end; w++)
        printf("%08x\n", *w);
    return 0;
}
