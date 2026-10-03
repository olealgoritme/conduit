/* SPDX-License-Identifier: Apache-2.0 */
/* dec.c -- NVDEC decode for the conduit link's client side (see gpu.h). */
#define _GNU_SOURCE
#include "gpu_internal.h"

cs_dec *cs_dec_open(cs_gpu *g, const struct cs_dec_params *p, const uint64_t *modifiers,
                    size_t n, char *err, size_t errlen)
{
    cs_err(err, errlen, "decoding is not built yet");
    return NULL;
}

int cs_dec_decode(cs_dec *d, const uint8_t *data, size_t len, struct cs_dec_frame *out,
                  char *err, size_t errlen)
{
    return -1;
}

void cs_dec_close(cs_dec *d)
{
}
