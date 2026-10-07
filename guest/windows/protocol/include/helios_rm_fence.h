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

/* Fence tail v3 (protocol/src/rm_fence_v3.rs; guest/windows/docs/rm-copy-engine-present.md
 * section 10): an OPTIONAL record behind a FENCE tail that lets the KMD copy the frame on its own
 * copy-engine channel, acquiring the producer's semaphore on the GPU. Placement:
 *   HERF: 0..32 HERF, 32..48 fence tail, 48..72 on-scanout slot ALL ZERO, 72..168 this record
 *         (CommandLength = 168)
 *   HEPR: 0..80 HEPR (reserved |= FLAG_RM_FENCE), 80..96 fence tail, 96..192 this record
 * An older KMD ignores the record (the fence and the Venus copy work as before). Send it only
 * with a FENCE tail; semaphore.value MUST equal rm_fence_value (the copy engine acquires on it).
 * Handles are RM handles of the producer's own client (not backend handles). The source must be
 * uncompressed, one plane, LINEAR or a GB20x block-linear modifier of its element size. */
#define HELIOS_RM_FENCE_TAIL_V3_MAGIC 0x33464548u /* 'HEF3' */
#define HELIOS_RM_FENCE_TAIL_V3_VERSION 3u
#define HELIOS_RM_FENCE_TAIL_V3_BYTES 96u
#define HELIOS_RM_FENCE_TAIL_V3_HERF_OFFSET 72u
#define HELIOS_RM_FENCE_TAIL_V3_HEPR_OFFSET 96u
#define HELIOS_RM_FENCE_TAIL_V3_FLAG_SEMAPHORE 0x1u /* version 3 needs both flags */
#define HELIOS_RM_FENCE_TAIL_V3_FLAG_SOURCE 0x2u
#define HELIOS_RM_COPY_SOURCE_FLAG_COMPRESSED 0x1u /* known; the copy-engine route refuses it */
/* QueryCaps.supported_ops bit 37 (1ull << 37): the KMD reads the record. Send it only then. */
#define HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3 0x2000000000ull

struct HeliosRmSemaphoreLoc {
   uint32_t h_client;  /* the producer's RM client (NVK's device client) */
   uint32_t h_memory;  /* hSemaphoreMem of the timeline's NV_SEMAPHORE_SURFACE */
   uint64_t offset;    /* entry index * entry size; 8-aligned */
   uint64_t value;     /* the value the frame's work releases; nonzero */
};

struct HeliosRmCopySource {
   uint32_t h_client;  /* the producer's RM client */
   uint32_t h_memory;  /* the presented image's RM memory */
   uint64_t offset;    /* plane 0 offset in that memory */
   uint64_t size;      /* bytes of the memory object */
   uint64_t modifier;  /* DRM_FORMAT_MOD_LINEAR or the GB20x block-linear family | h (h <= 5) */
   uint32_t pitch;     /* row pitch in bytes; block-linear: a multiple of 64 */
   uint32_t width;     /* 1..16384 */
   uint32_t height;    /* 1..16384 */
   uint32_t fourcc;    /* DRM_FORMAT_*, one plane */
   uint32_t flags;     /* HELIOS_RM_COPY_SOURCE_FLAG_* */
   uint32_t reserved;  /* zero */
};

struct HeliosRmFenceTailV3 {
   uint32_t magic;     /* HELIOS_RM_FENCE_TAIL_V3_MAGIC */
   uint16_t version;   /* HELIOS_RM_FENCE_TAIL_V3_VERSION */
   uint16_t flags;     /* SEMAPHORE | SOURCE */
   uint32_t bytes;     /* HELIOS_RM_FENCE_TAIL_V3_BYTES */
   uint32_t reserved;  /* zero */
   struct HeliosRmSemaphoreLoc semaphore;
   struct HeliosRmCopySource source;
};

#if defined(__cplusplus)
static_assert(sizeof(HeliosRmSemaphoreLoc) == 24, "semaphore location");
static_assert(offsetof(HeliosRmSemaphoreLoc, offset) == 8, "semaphore offset");
static_assert(offsetof(HeliosRmSemaphoreLoc, value) == 16, "semaphore value");
static_assert(sizeof(HeliosRmCopySource) == 56, "copy source");
static_assert(offsetof(HeliosRmCopySource, offset) == 8, "source offset");
static_assert(offsetof(HeliosRmCopySource, size) == 16, "source size");
static_assert(offsetof(HeliosRmCopySource, modifier) == 24, "source modifier");
static_assert(offsetof(HeliosRmCopySource, pitch) == 32, "source pitch");
static_assert(offsetof(HeliosRmCopySource, fourcc) == 44, "source fourcc");
static_assert(offsetof(HeliosRmCopySource, reserved) == 52, "source reserved");
static_assert(sizeof(HeliosRmFenceTailV3) == 96, "fence tail v3");
static_assert(offsetof(HeliosRmFenceTailV3, bytes) == 8, "v3 bytes");
static_assert(offsetof(HeliosRmFenceTailV3, semaphore) == 16, "v3 semaphore");
static_assert(offsetof(HeliosRmFenceTailV3, source) == 40, "v3 source");
#else
_Static_assert(sizeof(struct HeliosRmSemaphoreLoc) == 24, "semaphore location");
_Static_assert(offsetof(struct HeliosRmSemaphoreLoc, offset) == 8, "semaphore offset");
_Static_assert(offsetof(struct HeliosRmSemaphoreLoc, value) == 16, "semaphore value");
_Static_assert(sizeof(struct HeliosRmCopySource) == 56, "copy source");
_Static_assert(offsetof(struct HeliosRmCopySource, offset) == 8, "source offset");
_Static_assert(offsetof(struct HeliosRmCopySource, size) == 16, "source size");
_Static_assert(offsetof(struct HeliosRmCopySource, modifier) == 24, "source modifier");
_Static_assert(offsetof(struct HeliosRmCopySource, pitch) == 32, "source pitch");
_Static_assert(offsetof(struct HeliosRmCopySource, fourcc) == 44, "source fourcc");
_Static_assert(offsetof(struct HeliosRmCopySource, reserved) == 52, "source reserved");
_Static_assert(sizeof(struct HeliosRmFenceTailV3) == 96, "fence tail v3");
_Static_assert(offsetof(struct HeliosRmFenceTailV3, bytes) == 8, "v3 bytes");
_Static_assert(offsetof(struct HeliosRmFenceTailV3, semaphore) == 16, "v3 semaphore");
_Static_assert(offsetof(struct HeliosRmFenceTailV3, source) == 40, "v3 source");
#endif

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
