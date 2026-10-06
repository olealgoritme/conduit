/* SPDX-License-Identifier: MIT */
/*
 * Generations of the Windows transport: one loss state per generation.
 *
 * A generation is one life of the KMD's NVRM transport as this process sees
 * it: it starts when init accepts QUERY_CAPS (recording the transport's epoch)
 * and ends when the transport is lost. A loss is seen three ways:
 *
 *   - a reply whose header says so (helios_nvrm_reply_is_lost: TRANSPORT_RESET,
 *     or an epoch other than the init epoch; with the KMD's per-image salt this
 *     also catches a driver image reload, whose epoch no longer repeats),
 *   - a D3DKMT status that says the device is gone,
 *   - the process's loss table (helios_kmdmap.h) having moved for any other
 *     reason (a vanished KMD view faulted, the UMD or the Venus ICD saw it).
 *
 * All three end in the same place: the loss table's epoch moves away from the
 * one this generation attached at. That is the only latch. It is per
 * generation, not per process: the next init after a loss starts a new
 * generation that attaches at the table's current epoch, so it is not lost
 * until the table moves again.
 *
 * Nothing of an earlier generation is ever sent to a later transport: ids
 * restart per transport, so an old MUNMAP / UNPIN / EVENT_UNREGISTER could tear
 * down a new object that happens to have the same id.
 *
 * Pure C, no OS calls: tests/test_win_wire.c runs it on any host. The table's
 * epoch itself is helios_kmdmap's; the transport passes it in.
 */
#ifndef CRM_WIN_GEN_H
#define CRM_WIN_GEN_H

#include <stdint.h>

#include "helios_nvrm_escape.h"

struct crm_win_gen {
    uint32_t generation; /* 0 before the first init, then 1, 2, ... */
    int32_t loss_epoch0; /* the loss table's epoch this generation attached at */
    uint64_t init_epoch; /* the transport's epoch from QUERY_CAPS (never 0) */
};

/* Init: accept the transport QUERY_CAPS described, at loss-table epoch
 * `table_epoch`. Epoch 0 is the KMD saying there is no transport (a live one is
 * never 0): refused, and nothing changes, so the next open tries again. */
static inline int crm_win_gen_accept(struct crm_win_gen *g, uint64_t caps_epoch,
                                     int32_t table_epoch)
{
    if (caps_epoch == 0)
        return -1;
    if (g->generation == 0) {
        g->generation = 1;
        g->loss_epoch0 = table_epoch;
    }
    g->init_epoch = caps_epoch;
    return 0;
}

/* Is this generation lost, the loss table now being at `table_epoch`? */
static inline int crm_win_gen_lost(const struct crm_win_gen *g, int32_t table_epoch)
{
    return g->generation != 0 && table_epoch != g->loss_epoch0;
}

/* Is escape buffer `buf` (`size` bytes) an NVRM reply whose header can be
 * judged? The same D3DKMTEscape path also carries the transport's other Helios
 * escapes (Venus holder contexts, blob release, foreign resources), which are
 * shorter than an NVRM header or number their statuses differently: reading
 * their bytes 24..39 as status and epoch would judge garbage. */
static inline int crm_win_gen_judged(const void *buf, uint32_t size)
{
    if (size < HELIOS_NVRM_HEADER_BYTES)
        return 0;
    const HeliosEscapeHeader *e = (const HeliosEscapeHeader *)buf;
    return e->magic == HELIOS_ESCAPE_MAGIC && e->cmd_type == HELIOS_ESCAPE_NVRM;
}

/* Does reply `buf` (`size` bytes, from an escape whose NTSTATUS was success)
 * end this generation? Only NVRM replies are judged (crm_win_gen_judged). */
static inline int crm_win_gen_reply_lost(const struct crm_win_gen *g, const void *buf,
                                         uint32_t size)
{
    return g->generation != 0 && crm_win_gen_judged(buf, size) &&
           helios_nvrm_reply_is_lost(g->init_epoch, (const HeliosNvrmHeader *)buf);
}

/* After a loss: the next generation attaches at the table's current epoch and
 * waits for its own QUERY_CAPS (crm_win_gen_accept). */
static inline void crm_win_gen_restart(struct crm_win_gen *g, int32_t table_epoch)
{
    g->generation++;
    g->loss_epoch0 = table_epoch;
    g->init_epoch = 0;
}

#endif /* CRM_WIN_GEN_H */
