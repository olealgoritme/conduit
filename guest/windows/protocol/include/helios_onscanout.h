/* The "already on scanout" present tag (HOSC; guest/windows/docs/zero-copy-present.md,
 * "Already-on-scanout present tag"; Rust mirror: protocol/src/onscanout.rs).
 *
 * A producer that already put this frame on scanout with the user foreign-scanout source
 * (HELIOS_NVRM_OP_SCANOUT_SET / SCANOUT_PRESENT) but must still call pfnPresentCb (the D3D11
 * frame-latency wait) tags the present so the KMD does not copy the frame into the window's
 * redirection surface. The KMD verifies the claim against its own state and, if it cannot,
 * does the ordinary Blt: a wrong tag costs only this window's update.
 *
 * CARRIER: the tail of the HERF command (helios_rm_fence.h, struct HeliosPresentRefreshCmdFence)
 * that the UMD submits with pfnRenderCb immediately before pfnPresentCb, on the same hContext.
 * NOT pfnPresentCb's pPrivateDriverData: dxgkrnl does not forward it to DxgkDdiPresent.
 * CommandLength = 72, all little-endian:
 *    0..48  struct HeliosPresentRefreshCmdFence (the fence slot at 32..48 zero unless an RM
 *           fence is attached)
 *   48..72  struct HeliosOnScanoutTag
 * The tag is read only when CommandLength >= 72 and the 4 bytes at 48 are not zero. An older KMD
 * ignores it (and does the Blt), so it may always be sent.
 *
 * VALID WHEN (all): a user foreign-scanout source (SCANOUT_SET) is live and not lapsed;
 * generation is its out_generation; the presenting process is the process that owns it;
 * 0 < sequence <= the newest SCANOUT_PRESENT of that source and not more than
 * HELIOS_ONSCANOUT_MAX_LAG behind it; the present is a whole-surface windowed Blt (no
 * ColorFill, no sub-rects) with no snapshot attached; resource_id is 0 or the Blt source's. */
#ifndef HELIOS_ONSCANOUT_H
#define HELIOS_ONSCANOUT_H
#include <stdint.h>
#include <stddef.h>
#include "helios_rm_fence.h"

#define HELIOS_ONSCANOUT_MAGIC       0x43534F48u /* 'HOSC' little-endian */
#define HELIOS_ONSCANOUT_VERSION     1u
#define HELIOS_ONSCANOUT_HERF_OFFSET 48u
#define HELIOS_ONSCANOUT_HERF_BYTES  72u
#define HELIOS_ONSCANOUT_MAX_LAG     256u

struct HeliosOnScanoutTag {
   uint32_t magic;       /* HELIOS_ONSCANOUT_MAGIC */
   uint16_t version;     /* HELIOS_ONSCANOUT_VERSION */
   uint16_t flags;       /* zero (nonzero is rejected) */
   uint64_t sequence;    /* SCANOUT_PRESENT out_seq of this frame, nonzero */
   uint32_t generation;  /* SCANOUT_SET out_generation of the live source, nonzero */
   uint32_t resource_id; /* the Blt source's Helios resource id, or 0 = not stated */
};

struct HeliosPresentRefreshCmdOnScanout {
   struct HeliosPresentRefreshCmdFence base; /* 48 */
   struct HeliosOnScanoutTag tag;            /* +24 = 72 */
};

#if defined(__cplusplus)
static_assert(sizeof(HeliosOnScanoutTag) == 24, "HOSC");
static_assert(offsetof(HeliosOnScanoutTag, sequence) == 8, "HOSC sequence offset");
static_assert(offsetof(HeliosOnScanoutTag, generation) == 16, "HOSC generation offset");
static_assert(offsetof(HeliosOnScanoutTag, resource_id) == 20, "HOSC resource_id offset");
static_assert(sizeof(HeliosPresentRefreshCmdOnScanout) == HELIOS_ONSCANOUT_HERF_BYTES, "HERF+HOSC");
static_assert(offsetof(HeliosPresentRefreshCmdOnScanout, tag) == HELIOS_ONSCANOUT_HERF_OFFSET, "HOSC at 48");
#else
_Static_assert(sizeof(struct HeliosOnScanoutTag) == 24, "HOSC");
_Static_assert(offsetof(struct HeliosOnScanoutTag, sequence) == 8, "HOSC sequence offset");
_Static_assert(offsetof(struct HeliosOnScanoutTag, generation) == 16, "HOSC generation offset");
_Static_assert(offsetof(struct HeliosOnScanoutTag, resource_id) == 20, "HOSC resource_id offset");
_Static_assert(sizeof(struct HeliosPresentRefreshCmdOnScanout) == HELIOS_ONSCANOUT_HERF_BYTES, "HERF+HOSC");
_Static_assert(offsetof(struct HeliosPresentRefreshCmdOnScanout, tag) == HELIOS_ONSCANOUT_HERF_OFFSET, "HOSC at 48");
#endif
#endif
