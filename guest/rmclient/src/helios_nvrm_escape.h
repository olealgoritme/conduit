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
  uint64_t supported_ops;         /* out: bit n <=> HELIOS_NVRM_OP_* == n */
  uint32_t supported_event_kinds; /* out: bit n <=> HELIOS_NVRM_EVENT_* == n */
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
 * GetSysFiles 7. Mmap/Munmap use their own ops; everything else is refused. */
#define HELIOS_NVRM_FORWARD_MSG_TYPES                                          \
  ((1u << 1) | (1u << 2) | (1u << 3) | (1u << 6) | (1u << 7))
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
  uint32_t reserved1;  /* in:  zero */
} HeliosNvrmForward;
#define HELIOS_NVRM_FORWARD_BYTES 64u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmForward) == HELIOS_NVRM_FORWARD_BYTES, "Forward");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, req_len) == 40, "req_len");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, resp_cap) == 44, "resp_cap");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, resp_len) == 48, "resp_len");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, timeout_ms) == 52, "timeout_ms");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmForward, pin_id) == 56, "pin_id");

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
#define HELIOS_NVRM_EVENT_TRANSPORT_LOST 2u /* device reset; handle ignored (0) */

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
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmEvent, event_handle) == 48, "event.event_handle");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmEvent, out_state) == 60, "event.out_state");

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
 * id. A pin used by a FORWARD is committed and is released only by Close of its
 * handle, process exit or reset (UNPIN -> PIN_IN_USE). */
typedef struct HeliosNvrmPin {
  HeliosNvrmHeader head;
  uint32_t handle;     /* in:  backend handle the registration is made under */
  uint32_t flags;      /* in:  zero */
  uint64_t user_va;    /* in:  page-aligned start in the caller */
  uint64_t length;     /* in:  bytes, page multiple */
  uint32_t out_pin_id; /* out: for FORWARD.pin_id / UNPIN */
  uint32_t out_npages; /* out */
} HeliosNvrmPin;
#define HELIOS_NVRM_PIN_BYTES 72u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmPin) == HELIOS_NVRM_PIN_BYTES, "Pin");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, user_va) == 48, "pin.user_va");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, length) == 56, "pin.length");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, out_pin_id) == 64, "pin.out_pin_id");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmPin, out_npages) == 68, "pin.out_npages");

typedef struct HeliosNvrmUnpin {
  HeliosNvrmHeader head;
  uint32_t pin_id;
  uint32_t flags; /* zero */
} HeliosNvrmUnpin;
#define HELIOS_NVRM_UNPIN_BYTES 48u
HELIOS_NVRM_STATIC_ASSERT(sizeof(HeliosNvrmUnpin) == HELIOS_NVRM_UNPIN_BYTES, "Unpin");
HELIOS_NVRM_STATIC_ASSERT(offsetof(HeliosNvrmUnpin, pin_id) == 40, "unpin.pin_id");

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

#ifdef __cplusplus
}
#endif

#endif /* HELIOS_NVRM_ESCAPE_H */
