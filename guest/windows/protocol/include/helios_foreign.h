/* C mirror of HELIOS_ESCAPE_FOREIGN_RESOURCE (guest/windows/protocol/src/foreign.rs).
 *
 * The Rust file is the source of truth; every size and offset below is asserted
 * on both sides. Design and rules: guest/windows/docs/zero-copy-present.md.
 *
 * Little-endian, no pointers, identical for 32-bit (WoW64) and 64-bit callers. */
#ifndef HELIOS_FOREIGN_H
#define HELIOS_FOREIGN_H
#include <stddef.h>
#include <stdint.h>

#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u

#define HELIOS_ESCAPE_FOREIGN_RESOURCE 0x0018u
#define HELIOS_FOREIGN_ABI_VERSION 1u

#define HELIOS_FOREIGN_OP_QUERY_CAPS 1u
#define HELIOS_FOREIGN_OP_IMPORT_RM 2u
/* A GEM handle in the caller's own DRM file for an RM-export resource the caller
 * created or opened (the second-process route: see
 * helios_foreign_rm_resource_import). */
#define HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT 3u

#define HELIOS_FOREIGN_CAP_RM_IMPORT (1u << 0)
/* The KMD lets other processes open an adopted foreign allocation (open
 * identity flag, layout trailer rewritten at every open, host resource kept
 * until the last open closes). KMD-only: independent of the host. */
#define HELIOS_FOREIGN_CAP_SHARED_OPEN (1u << 1)
/* RM_RESOURCE_IMPORT is served end to end (this KMD and a host that serves
 * RmResourceImport). Without it the op answers HELIOS_FOREIGN_ST_UNSUPPORTED. */
#define HELIOS_FOREIGN_CAP_RM_RESOURCE_IMPORT (1u << 2)

/* IMPORT_RM.flags: a helios_foreign_layout follows the 72-byte request
 * (helios_foreign_import_rm_layout, 104 bytes). Not optional: a request without
 * it is refused HELIOS_FOREIGN_ST_BAD_RANGE. */
#define HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT (1u << 0)

/* RM_RESOURCE_IMPORT.out_flags: out_modifier is known. */
#define HELIOS_FOREIGN_RM_RESOURCE_IMPORT_MODIFIER (1u << 0)

#define HELIOS_FOREIGN_ST_OK 0
#define HELIOS_FOREIGN_ST_UNSUPPORTED 1
#define HELIOS_FOREIGN_ST_NOT_OWNED 2
#define HELIOS_FOREIGN_ST_BAD_CONTEXT 3
#define HELIOS_FOREIGN_ST_BAD_RANGE 4
#define HELIOS_FOREIGN_ST_NO_RESOURCES 5
#define HELIOS_FOREIGN_ST_DEVICE_ERROR 6

/* RESOURCE_CREATE_BLOB.blob_mem the KMD sends to the host for IMPORT_RM. */
#define HELIOS_BLOB_MEM_RM_EXPORT 0x80000001u

struct helios_escape_header {
   uint32_t magic, cmd_type, version, size;
};

struct helios_foreign_header {
   struct helios_escape_header hdr; /* cmd_type = HELIOS_ESCAPE_FOREIGN_RESOURCE */
   uint32_t abi_version;            /* in */
   uint32_t op;                     /* in */
   int32_t status;                  /* out: HELIOS_FOREIGN_ST_* */
   uint32_t reserved;               /* in: 0 */
   uint64_t epoch;                  /* out: device generation (as NVRM epoch) */
};

struct helios_foreign_query_caps {
   struct helios_foreign_header head;
   uint64_t supported_ops; /* bit n <=> op n implemented */
   uint32_t caps_flags;    /* HELIOS_FOREIGN_CAP_* */
   uint32_t max_per_owner;
   uint32_t max_total;
   uint32_t reserved0;
   uint64_t max_bytes_per_resource;
   uint64_t max_bytes_per_owner;
   uint32_t live_total;
   uint32_t live_owner;
   uint32_t imported;
   uint32_t refused;
};

struct helios_foreign_import_rm {
   struct helios_foreign_header head;
   uint32_t ctx_id;     /* in: Venus context created by this device */
   uint32_t rm_handle;  /* in: backend handle of a DRM file this device opened */
   uint32_t gem_handle; /* in: GEM handle in that file */
   uint32_t flags;      /* in: HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT, others 0 */
   uint64_t size;       /* in: bytes of the exported object, page multiple */
   uint32_t out_resource_id;
   uint32_t out_host_errno;
};

/* RM_RESOURCE_IMPORT: the caller (a process that opened the adopted allocation,
 * or the device that imported the resource) names a DRM file its own device
 * opened and a resource id; the host makes a GEM object in that file from the
 * resource's dma-buf. NVK then runs DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY on it,
 * NV0000 OS_UNIX_IMPORT_OBJECT_FROM_FD (0x3d06) into its RM client and
 * DRM_IOCTL_GEM_CLOSE, through FORWARD. The same resource on the same file gives
 * the same handle. The KMD refuses with NOT_OWNED (one code) a file that is not
 * the caller's DRM node, an unknown or destroyed resource, and a caller that
 * neither imported nor opened it; zero ids or nonzero flags are BAD_RANGE. */
struct helios_foreign_rm_resource_import {
   struct helios_foreign_header head;
   uint32_t rm_handle;      /* in: backend handle of a DRM file this device opened */
   uint32_t resource_id;    /* in: from the open identity / the IMPORT_RM reply */
   uint32_t flags;          /* in: 0 */
   uint32_t out_gem_handle; /* out: GEM handle in rm_handle's file */
   uint64_t out_size;       /* out: bytes */
   uint64_t out_modifier;   /* out: valid iff out_flags & ..._MODIFIER, else 0 */
   uint32_t out_flags;      /* out: HELIOS_FOREIGN_RM_RESOURCE_IMPORT_* */
   uint32_t out_host_errno; /* out: the host's errno when it refused and said so */
};

/* What is inside the exported object (plane 0). Accepted: fourcc one of
 * DRM_FORMAT_{XRGB,ARGB,XBGR,ABGR}8888; modifier DRM_FORMAT_MOD_LINEAR (0) or
 * 0x0300000000606010 | h, h in 0..=5; width/height 1..=16384; stride a multiple
 * of 4, >= width * 4, <= 1 MiB; offset + stride * rows <= size (a lower bound,
 * not an equality: RM rounds allocations up to 64 KiB). */
struct helios_foreign_layout {
   uint32_t width;    /* in */
   uint32_t height;   /* in */
   uint32_t stride;   /* in: plane 0 pitch in bytes (rowPitch) */
   uint32_t offset;   /* in: plane 0 offset in bytes */
   uint32_t fourcc;   /* in: DRM_FORMAT_* */
   uint32_t reserved; /* in: 0 */
   uint64_t modifier; /* in: DRM_FORMAT_MOD_* */
};

struct helios_foreign_import_rm_layout {
   struct helios_foreign_import_rm base;
   struct helios_foreign_layout layout;
};

_Static_assert(sizeof(struct helios_escape_header) == 16, "hdr");
_Static_assert(sizeof(struct helios_foreign_header) == 40, "head");
_Static_assert(offsetof(struct helios_foreign_header, abi_version) == 16, "abi");
_Static_assert(offsetof(struct helios_foreign_header, op) == 20, "op");
_Static_assert(offsetof(struct helios_foreign_header, status) == 24, "status");
_Static_assert(offsetof(struct helios_foreign_header, reserved) == 28, "reserved");
_Static_assert(offsetof(struct helios_foreign_header, epoch) == 32, "epoch");

_Static_assert(sizeof(struct helios_foreign_query_caps) == 96, "caps size");
_Static_assert(offsetof(struct helios_foreign_query_caps, supported_ops) == 40, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, caps_flags) == 48, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, max_per_owner) == 52, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, max_total) == 56, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, reserved0) == 60, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, max_bytes_per_resource) == 64, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, max_bytes_per_owner) == 72, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, live_total) == 80, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, live_owner) == 84, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, imported) == 88, "caps");
_Static_assert(offsetof(struct helios_foreign_query_caps, refused) == 92, "caps");

_Static_assert(sizeof(struct helios_foreign_import_rm) == 72, "import size");
_Static_assert(offsetof(struct helios_foreign_import_rm, ctx_id) == 40, "import");
_Static_assert(offsetof(struct helios_foreign_import_rm, rm_handle) == 44, "import");
_Static_assert(offsetof(struct helios_foreign_import_rm, gem_handle) == 48, "import");
_Static_assert(offsetof(struct helios_foreign_import_rm, flags) == 52, "import");
_Static_assert(offsetof(struct helios_foreign_import_rm, size) == 56, "import");
_Static_assert(offsetof(struct helios_foreign_import_rm, out_resource_id) == 64, "import");
_Static_assert(offsetof(struct helios_foreign_import_rm, out_host_errno) == 68, "import");

_Static_assert(sizeof(struct helios_foreign_layout) == 32, "layout size");
_Static_assert(offsetof(struct helios_foreign_layout, width) == 0, "layout");
_Static_assert(offsetof(struct helios_foreign_layout, height) == 4, "layout");
_Static_assert(offsetof(struct helios_foreign_layout, stride) == 8, "layout");
_Static_assert(offsetof(struct helios_foreign_layout, offset) == 12, "layout");
_Static_assert(offsetof(struct helios_foreign_layout, fourcc) == 16, "layout");
_Static_assert(offsetof(struct helios_foreign_layout, reserved) == 20, "layout");
_Static_assert(offsetof(struct helios_foreign_layout, modifier) == 24, "layout");

_Static_assert(sizeof(struct helios_foreign_import_rm_layout) == 104, "import+layout size");
_Static_assert(offsetof(struct helios_foreign_import_rm_layout, base) == 0, "import+layout");
_Static_assert(offsetof(struct helios_foreign_import_rm_layout, layout) == 72, "import+layout");

_Static_assert(sizeof(struct helios_foreign_rm_resource_import) == 80, "rri size");
_Static_assert(offsetof(struct helios_foreign_rm_resource_import, rm_handle) == 40, "rri");
_Static_assert(offsetof(struct helios_foreign_rm_resource_import, resource_id) == 44, "rri");
_Static_assert(offsetof(struct helios_foreign_rm_resource_import, flags) == 48, "rri");
_Static_assert(offsetof(struct helios_foreign_rm_resource_import, out_gem_handle) == 52, "rri");
_Static_assert(offsetof(struct helios_foreign_rm_resource_import, out_size) == 56, "rri");
_Static_assert(offsetof(struct helios_foreign_rm_resource_import, out_modifier) == 64, "rri");
_Static_assert(offsetof(struct helios_foreign_rm_resource_import, out_flags) == 72, "rri");
_Static_assert(offsetof(struct helios_foreign_rm_resource_import, out_host_errno) == 76, "rri");

#endif
