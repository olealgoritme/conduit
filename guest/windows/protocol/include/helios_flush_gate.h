/* The flush gate (HEFL): make the WDDM fence of a D3D11 pfnFlush mean "the GPU work of
 * this flush is done" (guest/windows/docs/flush-gate.md; Rust mirror:
 * protocol/src/flush_gate.rs).
 *
 * Send ONE pfnRenderCb from pfnFlush (NumAllocations = 0, NumPatchLocations = 0,
 * CommandLength = 48, on the device's own hContext) whose command is the record below.
 * Do it only for a device that holds a SHARED_KEYEDMUTEX resource (create or open) and
 * only when the flush recorded work since the previous gate.
 *
 * Gate on the capability, never on the KMD version:
 *   Venus stream point (FLAG_STREAM) and the wire rung: the MAP_READ_LEDGER PROBE reply
 *     (HELIOS_ESCAPE_MAP_READ_LEDGER op 0, out_size) & HELIOS_SCANOUT_CAP_FLUSH_GATE (1u << 5);
 *   RM fence (FLAG_RM_FENCE): NVRM QUERY_CAPS.supported_ops & HELIOS_NVRM_CAP_FLUSH_GATE
 *     (1ull << 34, guest/rmclient/src/helios_nvrm_escape.h).
 * An older KMD does not know the magic: it copies the bytes, returns success, gates
 * nothing and does NOT take an RM fence handle.
 *
 * The KMD never fails the Render for anything about the boundary. What it cannot honour
 * degrades to the wire rung (the packet retires when every transport entry enqueued
 * before the Render has retired, GPU completion included) and is counted (FlGDeg).
 * An RM fence handle in the tail is the KMD's afterwards, attached or not: never Close,
 * EVENT_REGISTER or reuse it. */
#ifndef HELIOS_FLUSH_GATE_H
#define HELIOS_FLUSH_GATE_H
#include <stdint.h>
#include <stddef.h>
#include "helios_rm_fence.h"

#define HELIOS_FLUSH_GATE_MAGIC   0x4C464548u /* 'HEFL' little-endian */
#define HELIOS_FLUSH_GATE_VERSION 1u

/* ctx_id / value / cookie name a point of this process's registered Venus producer
 * stream, the same tuple a present marker carries. ctx_id and cookie nonzero; value 0 =
 * "already complete" (the stream must be live). */
#define HELIOS_FLUSH_GATE_FLAG_STREAM   0x1u
/* fence is an RM fence of this process (FLAG_FENCE, nonzero handle). */
#define HELIOS_FLUSH_GATE_FLAG_RM_FENCE 0x2u
/* flags == 0: the wire rung. No boundary of its own; only a proof for work that already
 * reached the transport, so wait for your own submission thread first. */

#define HELIOS_SCANOUT_CAP_FLUSH_GATE (1u << 5)

struct HeliosFlushGateCmd {
   uint32_t magic, version;
   uint32_t flags;
   uint32_t ctx_id;
   uint32_t value;
   uint32_t reserved; /* zero */
   uint64_t cookie;
   struct HeliosRmFenceTail fence;
};

#if defined(__cplusplus)
static_assert(sizeof(HeliosFlushGateCmd) == 48, "HEFL");
static_assert(offsetof(HeliosFlushGateCmd, cookie) == 24, "HEFL cookie offset");
static_assert(offsetof(HeliosFlushGateCmd, fence) == 32, "HEFL fence offset");
#else
_Static_assert(sizeof(struct HeliosFlushGateCmd) == 48, "HEFL");
_Static_assert(offsetof(struct HeliosFlushGateCmd, cookie) == 24, "HEFL cookie offset");
_Static_assert(offsetof(struct HeliosFlushGateCmd, fence) == 32, "HEFL fence offset");
#endif
#endif
