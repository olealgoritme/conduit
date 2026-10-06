/* RM fence present markers (guest/windows/docs/rm-fence-marker.md; Rust mirror:
 * protocol/src/rm_fence.rs). The NVRM-side bits (SCANOUT_PRESENT flag, statuses,
 * QueryCaps capability bits) are in guest/rmclient/src/helios_nvrm_escape.h.
 *
 * A fence is a backend handle returned by a forwarded SEMSURF_FENCE_CREATE (nvidia-drm
 * 0x55, low 16 bits of the ioctl number 0x6455). The KMD takes it over when a carrier
 * is accepted: never Close / EVENT_REGISTER / reuse it afterwards. Gate every record
 * below on QueryCaps.supported_ops & HELIOS_NVRM_CAP_PRESENT_FENCE (1ull << 33).
 *
 * Refusals: SCANOUT_PRESENT and HE12 v4 report one (the call fails) and leave the
 * handle yours. A HERF / HEPR tail does NOT: its Render returns success whatever
 * happened to the tail, so for ANY parsed tail whose handle is a fence of the
 * presenting process the KMD takes the handle and closes it, attached as the
 * present's marker or not (both markers, a partial stream tail, no room). Do not
 * Close a handle you put in such a tail. */
#ifndef HELIOS_RM_FENCE_H
#define HELIOS_RM_FENCE_H
#include <stdint.h>
#include <stddef.h>

#define HELIOS_RM_FENCE_TAIL_FLAG_FENCE    0x1u /* rm_fence_handle is a fence to attach */
#define HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE 0x2u /* HE12 only, no handle: nothing to wait for */

/* The 16 bytes appended to a present marker. */
struct HeliosRmFenceTail {
   uint32_t rm_fence_handle;
   uint32_t flags;          /* HELIOS_RM_FENCE_TAIL_FLAG_* ; unknown bits refuse the record */
   uint64_t rm_fence_value; /* DIAGNOSTIC ONLY (the value is baked into the fence) */
};

/* HERF (HeliosPresentRefreshCmd, magic 0x46524548 'HERF', version 1), 32 -> 48 bytes.
 * Stream tail (present_ctx_id/value/cookie) MUST be zero when a fence is carried. The
 * tail is read only when CommandLength >= 48. NO version bump (an older KMD ignores
 * the extra bytes; a bumped version would be ignored whole). */
struct HeliosPresentRefreshCmdFence {
   uint32_t magic, version;
   uint32_t source_index, destination_index; /* reserved-zero */
   uint32_t present_ctx_id, present_value;
   uint64_t present_cookie;
   struct HeliosRmFenceTail fence;
};

/* HEPR (HeliosPresentRenderCmd), 80 -> 96 bytes: present.reserved |=
 * HELIOS_PRESENT_PRIVATE_FLAG_RM_FENCE and CommandLength >= 96; stream tail zero. */
#define HELIOS_PRESENT_PRIVATE_FLAG_RM_FENCE 0x8u
/* (the 80-byte HeliosPresentRenderCmd is defined by the existing UMD structs) */

/* HE12 (HeliosD3D12SubmitCmd, magic 0x32314548 'HE12'), version 4, 48 bytes.
 * Fence variant: ctx_id = value = cookie = gpu_wire_fence = 0, fence.flags = FENCE,
 * fence.rm_fence_handle != 0. CPU-complete variant: the same zeros, fence.flags =
 * COMPLETE, handle 0. An older KMD REFUSES version 4 (Render fails): gate it. */
#define HELIOS_D3D12_SUBMIT_VERSION_V4 4u
struct HeliosD3D12SubmitCmdV4 {
   uint32_t magic, version;
   uint32_t ctx_id, value;
   uint64_t cookie, gpu_wire_fence;
   struct HeliosRmFenceTail fence;
};

#if defined(__cplusplus)
static_assert(sizeof(HeliosRmFenceTail) == 16, "fence tail");
static_assert(sizeof(HeliosPresentRefreshCmdFence) == 48, "HERF fence");
static_assert(offsetof(HeliosPresentRefreshCmdFence, fence) == 32, "HERF fence offset");
static_assert(sizeof(HeliosD3D12SubmitCmdV4) == 48, "HE12 v4");
static_assert(offsetof(HeliosD3D12SubmitCmdV4, fence) == 32, "HE12 v4 fence offset");
#else
_Static_assert(sizeof(struct HeliosRmFenceTail) == 16, "fence tail");
_Static_assert(sizeof(struct HeliosPresentRefreshCmdFence) == 48, "HERF fence");
_Static_assert(offsetof(struct HeliosPresentRefreshCmdFence, fence) == 32, "HERF fence offset");
_Static_assert(sizeof(struct HeliosD3D12SubmitCmdV4) == 48, "HE12 v4");
_Static_assert(offsetof(struct HeliosD3D12SubmitCmdV4, fence) == 32, "HE12 v4 fence offset");
#endif
#endif
