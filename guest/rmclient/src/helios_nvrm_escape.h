/*
 * helios_nvrm_escape.h -- C mirror of guest/windows/protocol/src/nvrm.rs.
 *
 * The RM-forwarding escape of the Conduit Helios Windows KMD: one
 * D3DKMTEscape verb (HELIOS_ESCAPE_NVRM) with sub-operations, carrying
 * Conduit's NVIDIA-RM messages (host/backend/protocol: Open/Close/Ioctl/
 * GetProcFiles/GetSysFiles, plus Mmap/Munmap/EventReady handled by dedicated
 * ops) between a user-mode RM client and the host through the KMD.
 *
 * THE RUST FILE IS THE SOURCE OF TRUTH and carries the full contract (what the
 * KMD does and does not interpret, result layers, event semantics, pin
 * lifetime, WoW64 rules). This header must not drift from it: every struct size
 * and field offset is checked below with _Static_assert, and the same numbers
 * are asserted in nvrm.rs.
 *
 * Rules in one place:
 *   - little-endian, natural alignment, padding-free, every struct 8-aligned;
 *   - no pointers, no HANDLE/size_t/long in any struct: addresses are uint64_t,
 *     an OS HANDLE is a uint64_t ZERO-extended from a 32-bit process, so a
 *     WoW64 caller and a 64-bit caller use the identical layout;
 *   - SECURITY: user mode never supplies or sees a physical address. FORWARD
 *     refuses an Ioctl whose deep_ptr_offset is HELIOS_NVRM_DEEP_PAGE_RUNS[_INDIRECT];
 *     PIN returns only an opaque pin_id and FORWARD.pin_id makes the KMD splice
 *     its own page-run table into the request (see the Rust file);
 *   - the escape's NTSTATUS is the transport verdict (STATUS_NOT_IMPLEMENTED =
 *     older KMD without the verb), HeliosNvrmHeader.status is the KMD verdict,
 *     the RM result is inside the forwarded reply bytes and is never read by
 *     the KMD.
 *
 * Usage (FORWARD):
 *   size_t req_off  = HELIOS_NVRM_FORWARD_BYTES;
 *   size_t resp_off = helios_nvrm_forward_resp_offset(req_len);
 *   size_t total    = resp_off + resp_cap;
 *   buf = calloc(1, total);
 *   helios_nvrm_init(&f->head, HELIOS_NVRM_OP_FORWARD, total);
 *   f->req_len = req_len; f->resp_cap = resp_cap;
 *   memcpy(buf + req_off, msg_header_and_payload, req_len);
 *   D3DKMTEscape(...private data = buf, size = total...);
 *   // then: NTSTATUS, f->head.status, f->resp_len, reply at buf + resp_off.
 */
#ifndef HELIOS_NVRM_ESCAPE_H
#define HELIOS_NVRM_ESCAPE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifdef __cplusplus
#define HELIOS_NVRM_STATIC_ASSERT(c, m) static_assert(c, m)
#else
#define HELIOS_NVRM_STATIC_ASSERT(c, m) _Static_assert(c, m)
#endif

/* ---- the ordinary Helios escape header (protocol/src/escape.rs) ---------- */
#ifndef HELIOS_ESCAPE_HEADER_DEFINED
#define HELIOS_ESCAPE_HEADER_DEFINED
#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u
typedef struct HeliosEscapeHeader {
  uint32_t magic;    /* HELIOS_ESCAPE_MAGIC */
  uint32_t cmd_type; /* HELIOS_ESCAPE_* */
  uint32_t version;  /* HELIOS_ESCAPE_VERSION */
  uint32_t size;     /* total escape buffer bytes */
} HeliosEscapeHeader;
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosEscapeHeader) == 16, "HeliosEscapeHeader size");
#endif

#define HELIOS_ESCAPE_NVRM 0x0016u
#define HELIOS_NVRM_ABI_VERSION 1u
#define HELIOS_NVRM_MAX_BUFFER (1024u * 1024u)

/* ---- ops ------------------------------------------------------------------ */
#define HELIOS_NVRM_OP_QUERY_CAPS 1u
#define HELIOS_NVRM_OP_FORWARD 2u
#define HELIOS_NVRM_OP_MMAP 3u
#define HELIOS_NVRM_OP_MUNMAP 4u
#define HELIOS_NVRM_OP_EVENT_REGISTER 5u
#define HELIOS_NVRM_OP_EVENT_UNREGISTER 6u
#define HELIOS_NVRM_OP_PIN 7u
#define HELIOS_NVRM_OP_UNPIN 8u

/* ---- HeliosNvrmHeader.status ---------------------------------------------- */
#define HELIOS_NVRM_ST_OK 0
#define HELIOS_NVRM_ST_NOT_OWNED 1
#define HELIOS_NVRM_ST_MSG_TYPE_REFUSED 2
#define HELIOS_NVRM_ST_DEVICE_ERROR 3
#define HELIOS_NVRM_ST_TRANSPORT_RESET 4
#define HELIOS_NVRM_ST_TOO_SCATTERED 5
#define HELIOS_NVRM_ST_NO_RESOURCES 6
#define HELIOS_NVRM_ST_UNSUPPORTED 7
#define HELIOS_NVRM_ST_TIMEOUT 8
#define HELIOS_NVRM_ST_RESP_TRUNCATED 9
#define HELIOS_NVRM_ST_BAD_RANGE 10
#define HELIOS_NVRM_ST_FORBIDDEN 11    /* page-run deep block / bad pin_id request */
#define HELIOS_NVRM_ST_PIN_IN_USE 12   /* UNPIN of a pin a FORWARD has used */

/* ---- common header (first 40 bytes of every buffer) ----------------------- */
typedef struct HeliosNvrmHeader {
  HeliosEscapeHeader hdr; /* cmd_type = HELIOS_ESCAPE_NVRM, size = total */
  uint32_t abi_version;   /* in:  HELIOS_NVRM_ABI_VERSION */
  uint32_t op;            /* in:  HELIOS_NVRM_OP_* */
  int32_t status;         /* out: HELIOS_NVRM_ST_* */
  uint32_t reserved;      /* in/out: zero */
  uint64_t epoch;         /* out: device generation; a change = reopen */
} HeliosNvrmHeader;
#define HELIOS_NVRM_HEADER_BYTES 40u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmHeader) == HELIOS_NVRM_HEADER_BYTES, "hdr");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmHeader, abi_version) == 16, "abi_version");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmHeader, op) == 20, "op");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmHeader, status) == 24, "status");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmHeader, epoch) == 32, "epoch");

/* ---- QUERY_CAPS (88 bytes) ------------------------------------------------- */
typedef struct HeliosNvrmQueryCaps {
  HeliosNvrmHeader head;
  uint32_t max_buffer_bytes;      /* out */
  uint32_t default_timeout_ms;    /* out: FORWARD default for timeout_ms == 0 */
  uint64_t supported_ops;         /* out: bit n <=> HELIOS_NVRM_OP_* == n (ops 5, 6
                                     only while events are usable); bits 32..63 are
                                     capabilities (HELIOS_NVRM_CAP_*) */
  uint32_t supported_event_kinds; /* out: bit n <=> HELIOS_NVRM_EVENT_* == n;
                                     0 when events are not usable */
  uint32_t supported_cache_types; /* out: bit n <=> HELIOS_NVRM_CACHE_* == n */
  uint32_t device_features;       /* out: virtio config `features` (NVGPU_CFG_*) */
  uint32_t max_handles;           /* out: per process */
  uint32_t max_mappings;          /* out: per process */
  uint32_t max_pins;              /* out: per process */
  uint32_t max_pin_pages;         /* out */
  uint32_t pin_deep_kinds;        /* out: HELIOS_NVRM_PIN_DEEP_BIT_* */
} HeliosNvrmQueryCaps;
#define HELIOS_NVRM_PIN_DEEP_BIT_DIRECT (1u << 0)
#define HELIOS_NVRM_PIN_DEEP_BIT_INDIRECT (1u << 1)
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmQueryCaps) == 88, "QueryCaps size");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmQueryCaps, supported_ops) == 48, "supported_ops");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmQueryCaps, pin_deep_kinds) == 84, "pin_deep_kinds");

/* ---- FORWARD (64 bytes + request + response area) ------------------------- */
/* host MsgType values FORWARD accepts: Open 1, Close 2, Ioctl 3, GetProcFiles 6,
 * GetSysFiles 7, ScanoutFlip 20 (zero-copy present of a GEM object in a DRM-node
 * file the caller opened: scanout 0, 64-byte payload). Mmap/Munmap use their own
 * ops; everything else is refused. */
#define HELIOS_NVRM_FORWARD_MSG_TYPES                                          \
  ((1u << 1) | (1u << 2) | (1u << 3) | (1u << 6) | (1u << 7) | (1u << 20))
#define HELIOS_NVRM_SCANOUT_FLIP_BYTES 64u
#define HELIOS_NVRM_MSG_HEADER_BYTES 16u

typedef struct HeliosNvrmForward {
  HeliosNvrmHeader head;
  uint32_t req_len;    /* in:  request bytes after the struct (MsgHeader|payload) */
  uint32_t resp_cap;   /* in:  response-area bytes after the 8-aligned request */
  uint32_t resp_len;   /* out: valid reply bytes at the start of the response area */
  uint32_t timeout_ms; /* in:  0 = default_timeout_ms */
  uint32_t pin_id;     /* in:  PIN id whose KMD-built page-run table rides this
                        *      Ioctl (request deep_ptr_offset/deep_len must be 0),
                        *      or 0 */
  uint32_t rm_status_off; /* in: with pin_id, the byte offset in the reply's data
                           *     block (after MsgHeader + the 12-byte IoctlResp)
                           *     of the 32-bit RM status; the pin is kept only if
                           *     it and the host status are 0. Zero otherwise. */
} HeliosNvrmForward;
#define HELIOS_NVRM_FORWARD_BYTES 64u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmForward) == HELIOS_NVRM_FORWARD_BYTES, "Forward");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, req_len) == 40, "req_len");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, resp_cap) == 44, "resp_cap");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, resp_len) == 48, "resp_len");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, timeout_ms) == 52, "timeout_ms");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, pin_id) == 56, "pin_id");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, rm_status_off) == 60, "rm_status_off");

#define HELIOS_NVRM_FORWARD_REQ_OFFSET HELIOS_NVRM_FORWARD_BYTES
static inline size_t helios_nvrm_forward_resp_offset(uint32_t req_len) {
  return (size_t)HELIOS_NVRM_FORWARD_BYTES + (((size_t)req_len + 7u) & ~(size_t)7u);
}

/* ---- MMAP / MUNMAP --------------------------------------------------------- */
#define HELIOS_NVRM_PROT_READ 1u
#define HELIOS_NVRM_PROT_WRITE 2u
/* DEFAULT lets the KMD choose and is write-combined, like the Linux module. */
#define HELIOS_NVRM_CACHE_DEFAULT 0u
#define HELIOS_NVRM_CACHE_UC 1u
#define HELIOS_NVRM_CACHE_WC 2u
#define HELIOS_NVRM_CACHE_WB 3u

typedef struct HeliosNvrmMmap {
  HeliosNvrmHeader head;
  uint32_t handle;          /* in:  backend handle the mmap offset belongs to */
  uint32_t prot;            /* in:  HELIOS_NVRM_PROT_* */
  uint64_t offset;          /* in:  RM mmap cookie, passed to the host unchanged */
  uint64_t size;            /* in:  bytes (page multiple); out: bytes mapped */
  uint32_t cache_request;   /* in:  HELIOS_NVRM_CACHE_* */
  uint32_t cache_effective; /* out: cache type used (never DEFAULT) */
  uint64_t out_user_va;     /* out: user VA of the mapping */
  uint32_t out_mapping_id;  /* out: host mapping id, for MUNMAP */
  uint32_t flags;           /* in:  zero. out: the host's errno (positive) when
                             *      status == DEVICE_ERROR because it refused */
} HeliosNvrmMmap;
#define HELIOS_NVRM_MMAP_BYTES 88u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmMmap) == HELIOS_NVRM_MMAP_BYTES, "Mmap");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmMmap, handle) == 40, "mmap.handle");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmMmap, offset) == 48, "mmap.offset");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmMmap, size) == 56, "mmap.size");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmMmap, cache_request) == 64, "mmap.cache_request");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmMmap, out_user_va) == 72, "mmap.out_user_va");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmMmap, out_mapping_id) == 80, "mmap.out_mapping_id");

typedef struct HeliosNvrmMunmap {
  HeliosNvrmHeader head;
  uint32_t mapping_id;
  uint32_t flags; /* zero */
} HeliosNvrmMunmap;
#define HELIOS_NVRM_MUNMAP_BYTES 48u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmMunmap) == HELIOS_NVRM_MUNMAP_BYTES, "Munmap");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmMunmap, mapping_id) == 40, "munmap.mapping_id");

/* ---- events (persistent, level-triggered, no lost wakeup) ------------------ */
#define HELIOS_NVRM_EVENT_READY 1u          /* host EventReady for `handle` */
#define HELIOS_NVRM_EVENT_TRANSPORT_LOST 2u /* device reset; handle ignored (0);
                                               wakes EVERY registration */
/* A flip of your scanout source can be reused (see SCANOUT_STATUS below); handle
 * ignored (0). Only where HELIOS_NVRM_CAP_SCANOUT_RELEASE is set. Signalled whenever
 * out_released_seq may have advanced; also wakes with TRANSPORT_LOST. */
#define HELIOS_NVRM_EVENT_SCANOUT_RELEASED 3u
/* what QUERY_CAPS.supported_event_kinds reports while events are usable */
#define HELIOS_NVRM_EVENT_KINDS_ALL                                            \
  ((1u << HELIOS_NVRM_EVENT_READY) | (1u << HELIOS_NVRM_EVENT_TRANSPORT_LOST))
/* ... plus this bit on a device with buffer releases */
#define HELIOS_NVRM_EVENT_KINDS_SCANOUT_RELEASE                                \
  (1u << HELIOS_NVRM_EVENT_SCANOUT_RELEASED)
/* EVENT_REGISTER needs the KMD's event queue (virtqueue 1) to be up; if it is
 * not, REGISTER answers HELIOS_NVRM_ST_UNSUPPORTED and supported_event_kinds is
 * 0. No feature bit is involved (the KMD never acks the input bit). A registration is keyed (process, handle, kind);
 * registering it again replaces the event (STATE_REPLACED). A transport that has
 * already failed answers HELIOS_NVRM_ST_TRANSPORT_RESET. UNREGISTER ignores
 * event_handle and does not signal. Full contract: the Rust file. */

#define HELIOS_NVRM_EVENT_STATE_REGISTERED 1u
#define HELIOS_NVRM_EVENT_STATE_REPLACED 2u
#define HELIOS_NVRM_EVENT_STATE_LATCHED_SIGNALED 3u
#define HELIOS_NVRM_EVENT_STATE_UNREGISTERED 4u
#define HELIOS_NVRM_EVENT_STATE_NOT_FOUND 5u

typedef struct HeliosNvrmEvent {
  HeliosNvrmHeader head;
  uint32_t handle;       /* in:  backend handle (0 for TRANSPORT_LOST) */
  uint32_t kind;         /* in:  HELIOS_NVRM_EVENT_* */
  uint64_t event_handle; /* in:  usermode event HANDLE, ZERO-extended */
  uint32_t flags;        /* in:  zero */
  uint32_t out_state;    /* out: HELIOS_NVRM_EVENT_STATE_* */
} HeliosNvrmEvent;
#define HELIOS_NVRM_EVENT_BYTES 64u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmEvent) == HELIOS_NVRM_EVENT_BYTES, "Event");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmEvent, handle) == 40, "event.handle");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmEvent, kind) == 44, "event.kind");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmEvent, event_handle) == 48, "event.event_handle");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmEvent, flags) == 56, "event.flags");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmEvent, out_state) == 60, "event.out_state");
HELIOS_NVRM_STATIC_ASSERT(HELIOS_NVRM_EVENT_KINDS_ALL == 6u, "event kinds");
HELIOS_NVRM_STATIC_ASSERT(HELIOS_NVRM_EVENT_KINDS_SCANOUT_RELEASE == 8u, "event kinds (release)");

/* ---- PIN / UNPIN: memory registered by CPU address ------------------------- */
/* IoctlReq.deep_ptr_offset values the host reads as a page-run table (host
 * pageruns.rs PAGE_RUNS / _INDIRECT). NEVER send these: FORWARD refuses them;
 * only the KMD writes them, for a pin_id. */
#define HELIOS_NVRM_DEEP_PAGE_RUNS 0xFFFFFFFEu
#define HELIOS_NVRM_DEEP_PAGE_RUNS_INDIRECT 0xFFFFFFFDu
#define HELIOS_NVRM_PAGE_RUNS_MAX 1024u
/* u32 runs, u32 reserved, runs x { u64 gpa, u64 len } */
#define HELIOS_NVRM_PAGE_RUNS_DIRECT_BYTES (8u + HELIOS_NVRM_PAGE_RUNS_MAX * 16u)

/* The KMD locks the pages and keeps the page-run table; user mode gets only an
 * id. A pin a FORWARD has used is released only by the KMD: at once if the
 * registration failed (see rm_status_off), when an RM_FREE succeeds for
 * (h_root, h_object) or for h_root, or at Close of its handle, process exit or
 * reset (UNPIN of a used pin -> PIN_IN_USE). h_root / h_object are the client and
 * memory object handles the registration will create. */
typedef struct HeliosNvrmPin {
  HeliosNvrmHeader head;
  uint32_t handle;     /* in:  backend handle the registration is made under */
  uint32_t flags;      /* in:  zero */
  uint64_t user_va;    /* in:  page-aligned start in the caller */
  uint64_t length;     /* in:  bytes, page multiple */
  uint32_t h_root;     /* in:  RM client handle of the registration */
  uint32_t h_object;   /* in:  memory object handle it will create */
  uint32_t out_pin_id; /* out: for FORWARD.pin_id / UNPIN */
  uint32_t out_npages; /* out */
} HeliosNvrmPin;
#define HELIOS_NVRM_PIN_BYTES 80u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmPin) == HELIOS_NVRM_PIN_BYTES, "Pin");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, user_va) == 48, "pin.user_va");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, length) == 56, "pin.length");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, h_root) == 64, "pin.h_root");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, h_object) == 68, "pin.h_object");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, out_pin_id) == 72, "pin.out_pin_id");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, out_npages) == 76, "pin.out_npages");

typedef struct HeliosNvrmUnpin {
  HeliosNvrmHeader head;
  uint32_t pin_id;
  uint32_t flags; /* zero */
} HeliosNvrmUnpin;
#define HELIOS_NVRM_UNPIN_BYTES 48u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmUnpin) == HELIOS_NVRM_UNPIN_BYTES, "Unpin");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmUnpin, pin_id) == 40, "unpin.pin_id");

/* ---- SCANOUT_SET / SCANOUT_PRESENT / SCANOUT_RELEASE: foreign scanout source -
 * Own scanout 0 and show host GEM objects on it through the KMD (protocol/src/
 * nvrm_scanout.rs has the contract). While a source is live the KMD withholds the
 * desktop's own flushes of scanout 0 (so the two do not alternate), sends the
 * ScanoutFlip itself with a seq it mints, and gives scanout 0 back (with one fresh
 * desktop flush) on RELEASE, close of the DRM file, process exit, device reset, or
 * `lapse_ms` without a PRESENT. Advertised in QueryCaps.supported_ops bits 9..11.
 * SET: BAD_RANGE for a bad layout, NOT_OWNED / FORBIDDEN for the handle (as for a
 * forwarded ScanoutFlip), SCANOUT_BUSY while another device's source is live.
 * PRESENT: NO_SOURCE when the caller has none live on that handle (SET again). */
#define HELIOS_NVRM_OP_SCANOUT_SET 9u
#define HELIOS_NVRM_OP_SCANOUT_PRESENT 10u
#define HELIOS_NVRM_OP_SCANOUT_RELEASE 11u
#define HELIOS_NVRM_ST_SCANOUT_BUSY 13
#define HELIOS_NVRM_ST_NO_SOURCE 14

/* ---- RM fence presents (guest/windows/docs/rm-fence-marker.md) --------------
 * SCANOUT_PRESENT with HELIOS_NVRM_SCANOUT_PRESENT_FLAG_RM_FENCE: rm_fence_handle
 * is a backend handle from a forwarded SEMSURF_FENCE_CREATE (0x6455) that the KMD
 * TAKES OVER (never Close / EVENT_REGISTER / reuse it after status OK). The flip
 * is sent when the fence fires; out_seq is returned at once. Refusals leave the
 * handle the caller's: NOT_OWNED (not yours), FORBIDDEN (yours, not a fence),
 * FENCE_ATTACHED, QUEUE_FULL (HELIOS_NVRM_SCANOUT_FENCE_DEPTH waiting), NO_SOURCE,
 * UNSUPPORTED. Probe QueryCaps.supported_ops for the capability bits (>= 32, so
 * they cannot collide with op numbers). */
#define HELIOS_NVRM_SCANOUT_PRESENT_FLAG_RM_FENCE 1u
#define HELIOS_NVRM_ST_FENCE_ATTACHED 15
#define HELIOS_NVRM_ST_QUEUE_FULL 16
#define HELIOS_NVRM_SCANOUT_FENCE_DEPTH 8u
#define HELIOS_NVRM_CAP_SCANOUT_FENCE (1ull << 32)
#define HELIOS_NVRM_CAP_PRESENT_FENCE (1ull << 33)
/* The WDDM flush gate (HEFL, protocol/include/helios_flush_gate.h) honours its RM fence variant. */
#define HELIOS_NVRM_CAP_FLUSH_GATE (1ull << 34)

/* ---- buffer release: SCANOUT_STATUS + HELIOS_NVRM_EVENT_SCANOUT_RELEASED -----
 * (guest/windows/docs/foreign-scanout.md "Buffer release"; protocol/src/nvrm_scanout.rs)
 * Where the KMD acked the host's NVGPU_F_SCANOUT_RELEASE, QueryCaps.supported_ops has
 * HELIOS_NVRM_CAP_SCANOUT_RELEASE (and op bit 12, event kind bit 3). Then SCANOUT_STATUS
 * tells which presented images the host is done with: an image whose latest present
 * returned out_seq == P may be written again once out_released_seq >= P (never true for
 * the image on screen). Without the cap keep the old rule (rm-fence-marker.md: with N >= 3
 * images, not before present P+1 returned and its fence fired). Wait without polling:
 * EVENT_REGISTER kind SCANOUT_RELEASED (handle 0) once; per image: auto-reset (or reset)
 * the event, SCANOUT_STATUS, and only if out_released_seq < P wait (with a timeout), then
 * SCANOUT_STATUS again. The event also wakes on TRANSPORT_LOST. */
#define HELIOS_NVRM_OP_SCANOUT_STATUS 12u
#define HELIOS_NVRM_CAP_SCANOUT_RELEASE (1ull << 34)

typedef struct HeliosNvrmScanoutSet {
  HeliosNvrmHeader head;
  uint32_t handle;         /* in:  backend handle of a DRM-node file (device_type >= 512) */
  uint32_t flags;          /* in:  zero */
  uint32_t width;          /* in:  64..16384 */
  uint32_t height;         /* in:  64..16384 */
  uint32_t stride;         /* in:  plane 0 pitch, >= width * 4, <= 1 MiB */
  uint32_t offset;         /* in:  plane 0 offset */
  uint32_t fourcc;         /* in:  DRM_FORMAT_{XRGB,ARGB,XBGR,ABGR}8888 */
  uint32_t lapse_ms;       /* in:  0 = 2000, clamped 100..30000; out: in effect */
  uint64_t modifier;       /* in:  DRM_FORMAT_MOD_* (block-linear allowed) */
  uint32_t out_generation; /* out: nonzero source id */
  uint32_t reserved;       /* in:  zero */
} HeliosNvrmScanoutSet;
#define HELIOS_NVRM_SCANOUT_SET_BYTES 88u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmScanoutSet) == HELIOS_NVRM_SCANOUT_SET_BYTES, "ScanoutSet");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, handle) == 40, "sset.handle");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, flags) == 44, "sset.flags");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, width) == 48, "sset.width");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, height) == 52, "sset.height");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, stride) == 56, "sset.stride");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, offset) == 60, "sset.offset");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, fourcc) == 64, "sset.fourcc");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, lapse_ms) == 68, "sset.lapse_ms");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, modifier) == 72, "sset.modifier");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, out_generation) == 80, "sset.out_generation");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutSet, reserved) == 84, "sset.reserved");

typedef struct HeliosNvrmScanoutPresent {
  HeliosNvrmHeader head;
  uint32_t handle;   /* in:  the handle given to SET */
  uint32_t gem;      /* in:  GEM handle (in that DRM file) of the image to show */
  uint32_t flags;    /* in:  HELIOS_NVRM_SCANOUT_PRESENT_FLAG_RM_FENCE or zero */
  uint32_t rm_fence_handle; /* in: with the flag, a fence handle (forwarded
                               SEMSURF_FENCE_CREATE) the KMD takes over; zero without */
  uint64_t out_seq;  /* out: the seq the KMD put in the ScanoutFlip */
} HeliosNvrmScanoutPresent;
#define HELIOS_NVRM_SCANOUT_PRESENT_BYTES 64u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmScanoutPresent) == HELIOS_NVRM_SCANOUT_PRESENT_BYTES, "ScanoutPresent");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutPresent, handle) == 40, "spres.handle");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutPresent, gem) == 44, "spres.gem");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutPresent, flags) == 48, "spres.flags");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutPresent, rm_fence_handle) == 52, "spres.rm_fence_handle");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutPresent, out_seq) == 56, "spres.out_seq");

typedef struct HeliosNvrmScanoutStatus {
  HeliosNvrmHeader head;
  uint32_t handle;            /* in:  the handle given to SCANOUT_SET (a DRM-node handle of yours) */
  uint32_t flags;             /* in:  zero */
  uint64_t out_released_seq;  /* out: every flip of `handle` with seq <= this is done (0 = none) */
  uint64_t out_last_seq;      /* out: the newest seq the KMD remembers for `handle` */
} HeliosNvrmScanoutStatus;
#define HELIOS_NVRM_SCANOUT_STATUS_BYTES 64u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmScanoutStatus) == HELIOS_NVRM_SCANOUT_STATUS_BYTES, "ScanoutStatus");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutStatus, handle) == 40, "sstat.handle");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutStatus, flags) == 44, "sstat.flags");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutStatus, out_released_seq) == 48, "sstat.out_released_seq");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutStatus, out_last_seq) == 56, "sstat.out_last_seq");

typedef struct HeliosNvrmScanoutRelease {
  HeliosNvrmHeader head;
  uint32_t handle; /* in: the handle given to SET, or 0 for "whatever I hold" */
  uint32_t flags;  /* in: zero */
} HeliosNvrmScanoutRelease;
#define HELIOS_NVRM_SCANOUT_RELEASE_BYTES 48u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmScanoutRelease) == HELIOS_NVRM_SCANOUT_RELEASE_BYTES, "ScanoutRelease");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutRelease, handle) == 40, "srel.handle");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmScanoutRelease, flags) == 44, "srel.flags");

/* Fill the common header for `op`; `total` is the whole buffer size. */
static inline void helios_nvrm_init(HeliosNvrmHeader *h, uint32_t op, uint32_t total) {
  h->hdr.magic = HELIOS_ESCAPE_MAGIC;
  h->hdr.cmd_type = HELIOS_ESCAPE_NVRM;
  h->hdr.version = HELIOS_ESCAPE_VERSION;
  h->hdr.size = total;
  h->abi_version = HELIOS_NVRM_ABI_VERSION;
  h->op = op;
  h->status = 0;
  h->reserved = 0;
  h->epoch = 0;
}

/* ---- client rules (not wire: how a client must read the replies) ----------
 *
 * The transport a client initialised against is gone when ANY reply says so:
 *   - HELIOS_NVRM_ST_TRANSPORT_RESET (EVENT_REGISTER on a failed transport), or
 *   - `epoch` differs from the one QUERY_CAPS gave at init. The epoch is the
 *     transport instance's generation: it changes at every StartDevice, and
 *     reads 0 when there is no transport at all (a live one is never 0). Every
 *     handle, mapping, pin and event of the earlier generation is gone, so the
 *     process must reopen; a client that cannot (librmclient) treats the device
 *     as lost for good.
 * The header is valid whenever the escape's NTSTATUS was success (the KMD writes
 * `epoch` on every success return, including a nonzero `status`). */
static inline int helios_nvrm_reply_is_lost(uint64_t init_epoch, const HeliosNvrmHeader *h) {
  return h->status == HELIOS_NVRM_ST_TRANSPORT_RESET || h->epoch != init_epoch;
}

/* NTSTATUS values of a failed escape that mean the device is gone, not that the
 * request was bad: STATUS_DEVICE_NOT_READY (EVENT_REGISTER / UNREGISTER with no
 * transport) and STATUS_DEVICE_REMOVED (the adapter was removed). */
static inline int helios_nvrm_ntstatus_is_lost(int32_t ntstatus) {
  const uint32_t s = (uint32_t)ntstatus;
  return s == 0xC00000A3u || s == 0xC00002B6u;
}

#ifdef __cplusplus
}
#endif

#endif /* HELIOS_NVRM_ESCAPE_H */
