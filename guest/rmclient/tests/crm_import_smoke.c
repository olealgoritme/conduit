/* SPDX-License-Identifier: MIT */
/*
 * crm_import_smoke: the Helios KMD's HELIOS_ESCAPE_FOREIGN_RESOURCE / IMPORT_RM
 * verb on a Windows guest, with nothing but librmclient's Windows transport.
 *
 *   QUERY_CAPS (CAP_RM_IMPORT clear -> SKIP, exit 0) ->
 *   crm_open -> device, subdevice -> NV01_MEMORY_LOCAL_USER, pitch linear
 *   1920x1080 XRGB8888 -> NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD ->
 *   DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY on a host DRM render node (as
 *   crm_scanout_smoke) -> HELIOS_ESCAPE_CTX_CREATE (Venus) ->
 *   IMPORT_RM {ctx, DRM node, GEM, size, layout}: ST_OK, nonzero resource id ->
 *   RELEASE_BLOB (a second one must fail) ->
 *   deliberate mistakes, each with the documented status and host errno:
 *     unknown GEM handle            ST_NOT_OWNED  ENOENT (host)
 *     size larger than the object   ST_BAD_RANGE  ERANGE (host)
 *     a non-DRI channel as rm_handle ST_NOT_OWNED  none or EBADF
 *     no layout flag (72 bytes)     ST_BAD_RANGE  none (KMD, before the host)
 *     layout with a bad modifier    ST_BAD_RANGE  none (KMD, before the host)
 *     a context this device did not create  ST_BAD_CONTEXT  none
 *   -> CTX_DESTROY, GEM close, free, close.
 *
 * The verb and its rules: guest/windows/docs/zero-copy-present.md (sections 3.2,
 * 10.1 and "Gate opened"); the ABI is the KMD's own
 * guest/windows/protocol/include/helios_foreign.h. It is sent with
 * D3DKMTEscape like the NVRM escapes, through crm_win_escape_raw, so the RM
 * handles and the Venus context belong to the same D3DKMT device.
 *
 * Usage: crm_import_smoke [dri_index=0]
 * Exit: 0 passed or skipped, 1 failed, 77 not on Windows.
 */
#include <errno.h>
#include <inttypes.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "rmclient.h"
#include "rmclient_transport.h"
#include "nv_ioctl_defs.h"
#include "helios_foreign.h"

#if defined(_WIN32)

#define W 1920u
#define H 1080u
#define STRIDE (W * 4u)

/* nvidia-drm GEM import, as in crm_scanout_smoke. */
struct drm_nvidia_gem_import_nvkms_memory_params {
    uint64_t mem_size;
    uint64_t nvkms_params_ptr;
    uint64_t nvkms_params_size;
    uint32_t handle;
    uint32_t pad;
};
struct nvkms_kapi_priv_import_memory_params {
    int32_t memFd;
    uint32_t layout; /* NvKmsSurfaceMemoryLayout: 0 block linear, 1 pitch */
    uint32_t log2_gobs_x, log2_gobs_y, log2_gobs_z;
    uint32_t pitch_in_blocks;
    uint8_t generic_memory;
    uint8_t pad[3];
};
_Static_assert(sizeof(struct drm_nvidia_gem_import_nvkms_memory_params) == 32, "GEM_IMPORT");
_Static_assert(sizeof(struct nvkms_kapi_priv_import_memory_params) == 28, "NVKMS import");

#define DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY 0xC0206441u /* DRM_IOWR(0x40 + 0x01, 32) */
#define DRM_IOCTL_GEM_CLOSE 0x40086409u                      /* DRM_IOW(0x09, 8) */
#define DRM_FORMAT_XRGB8888 0x34325258u
#define DRM_FORMAT_MOD_LINEAR 0ull
/* DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(h) is 0x0300000000606010 | h, h in 0..=5. */
#define MOD_NV_BLOCK_LINEAR_BASE 0x0300000000606010ull

#define NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS 0x3u
#define NVOS32_ATTR2_ZBC_PREFER_NO_ZBC 0x2u /* 1:0 */

/* The Linux errnos the host reports in out_host_errno (not mingw's errno.h). */
#define HOST_ENOENT 2u
#define HOST_EBADF 9u
#define HOST_ERANGE 34u

/* The other Helios escapes this test needs (guest/windows/protocol/src/escape.rs). */
#define HELIOS_ESCAPE_CTX_CREATE 0x0002u
#define HELIOS_ESCAPE_CTX_DESTROY 0x0003u
#define HELIOS_ESCAPE_RELEASE_BLOB 0x0008u
#define VIRTIO_GPU_CAPSET_VENUS 4u

struct escape_ctx_create {
    struct helios_escape_header hdr;
    uint32_t capset_id;  /* in */
    uint32_t out_ctx_id; /* out */
};
struct escape_ctx_destroy {
    struct helios_escape_header hdr;
    uint32_t ctx_id;
    uint32_t padding;
};
struct escape_release_blob {
    struct helios_escape_header hdr;
    uint32_t ctx_id;
    uint32_t resource_id;
    uint32_t flags;
    uint32_t padding;
};
_Static_assert(sizeof(struct escape_ctx_create) == 24, "CTX_CREATE");
_Static_assert(offsetof(struct escape_ctx_create, capset_id) == 16, "CTX_CREATE");
_Static_assert(offsetof(struct escape_ctx_create, out_ctx_id) == 20, "CTX_CREATE");
_Static_assert(sizeof(struct escape_ctx_destroy) == 24, "CTX_DESTROY");
_Static_assert(offsetof(struct escape_ctx_destroy, ctx_id) == 16, "CTX_DESTROY");
_Static_assert(sizeof(struct escape_release_blob) == 32, "RELEASE_BLOB");
_Static_assert(offsetof(struct escape_release_blob, ctx_id) == 16, "RELEASE_BLOB");
_Static_assert(offsetof(struct escape_release_blob, resource_id) == 20, "RELEASE_BLOB");

/* The sentinels a reply must overwrite: an untouched field is a failure, not 0. */
#define SENT_STATUS ((int32_t)0x5a5a5a5a)
#define SENT_U32 0xdeadbeefu

static unsigned fails;
static char first_fail[512];

static void fail_note(const char *fmt, ...)
{
    char t[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(t, sizeof t, fmt, ap);
    va_end(ap);
    fails++;
    if (first_fail[0] == 0)
        snprintf(first_fail, sizeof first_fail, "%s", t);
}

/* Print one verdict line; a failure also records the first failing step. */
static int report(int ok, const char *what, const char *expect)
{
    if (ok) {
        printf("[ ok ] %s\n", what);
    } else {
        printf("[FAIL] %s%s%s\n", what, expect ? "  -- expected " : "", expect ? expect : "");
        fail_note("%s%s%s", what, expect ? "  -- expected " : "", expect ? expect : "");
    }
    fflush(stdout);
    return ok ? 0 : -1;
}

static int step(const char *what, int r)
{
    char t[300];
    if (r == 0) {
        printf("[ ok ] %s\n", what);
    } else {
        snprintf(t, sizeof t, "%s: %s (%d / 0x%x) %s", what, crm_status_name(r), r, (unsigned)r,
                 crm_status_string(r));
        printf("[FAIL] %s\n", t);
        fail_note("%s", t);
    }
    fflush(stdout);
    return r;
}

static const char *fst_name(int32_t s)
{
    switch (s) {
    case HELIOS_FOREIGN_ST_OK: return "ST_OK";
    case HELIOS_FOREIGN_ST_UNSUPPORTED: return "ST_UNSUPPORTED";
    case HELIOS_FOREIGN_ST_NOT_OWNED: return "ST_NOT_OWNED";
    case HELIOS_FOREIGN_ST_BAD_CONTEXT: return "ST_BAD_CONTEXT";
    case HELIOS_FOREIGN_ST_BAD_RANGE: return "ST_BAD_RANGE";
    case HELIOS_FOREIGN_ST_NO_RESOURCES: return "ST_NO_RESOURCES";
    case HELIOS_FOREIGN_ST_DEVICE_ERROR: return "ST_DEVICE_ERROR";
    case SENT_STATUS: return "(not written)";
    default: return "ST_?";
    }
}

static void hdr_init(struct helios_escape_header *h, uint32_t cmd, uint32_t size)
{
    h->magic = HELIOS_ESCAPE_MAGIC;
    h->cmd_type = cmd;
    h->version = HELIOS_ESCAPE_VERSION;
    h->size = size;
}

/* ---- the verbs ------------------------------------------------------------ */

static int query_caps(struct helios_foreign_query_caps *q, int32_t *nt)
{
    memset(q, 0, sizeof *q);
    hdr_init(&q->head.hdr, HELIOS_ESCAPE_FOREIGN_RESOURCE, sizeof *q);
    q->head.abi_version = HELIOS_FOREIGN_ABI_VERSION;
    q->head.op = HELIOS_FOREIGN_OP_QUERY_CAPS;
    q->head.status = SENT_STATUS;
    return crm_win_escape_raw(q, sizeof *q, nt);
}

static int ctx_create(uint32_t *ctx, int32_t *nt)
{
    struct escape_ctx_create e;
    memset(&e, 0, sizeof e);
    hdr_init(&e.hdr, HELIOS_ESCAPE_CTX_CREATE, sizeof e);
    e.capset_id = VIRTIO_GPU_CAPSET_VENUS;
    e.out_ctx_id = 0;
    const int r = crm_win_escape_raw(&e, sizeof e, nt);
    *ctx = r == 0 ? e.out_ctx_id : 0;
    return r;
}

static int ctx_destroy(uint32_t ctx, int32_t *nt)
{
    struct escape_ctx_destroy e;
    memset(&e, 0, sizeof e);
    hdr_init(&e.hdr, HELIOS_ESCAPE_CTX_DESTROY, sizeof e);
    e.ctx_id = ctx;
    return crm_win_escape_raw(&e, sizeof e, nt);
}

static int release_blob(uint32_t ctx, uint32_t resid, int32_t *nt)
{
    struct escape_release_blob e;
    memset(&e, 0, sizeof e);
    hdr_init(&e.hdr, HELIOS_ESCAPE_RELEASE_BLOB, sizeof e);
    e.ctx_id = ctx;
    e.resource_id = resid;
    return crm_win_escape_raw(&e, sizeof e, nt);
}

struct imp_out {
    int rc;          /* crm_win_escape_raw */
    int32_t nt;      /* the escape's NTSTATUS */
    int32_t status;  /* HELIOS_FOREIGN_ST_* */
    uint32_t resid;  /* out_resource_id */
    uint32_t host_errno;
    uint64_t epoch;
};

/* One IMPORT_RM. lay != NULL: the 104-byte request with `flags` as given (the
 * caller sets the layout bit); lay == NULL: the 72-byte form. */
static void import_rm(struct imp_out *o, uint32_t ctx, uint32_t rm, uint32_t gem, uint32_t flags,
                      uint64_t size, const struct helios_foreign_layout *lay)
{
    struct helios_foreign_import_rm_layout req;
    memset(&req, 0, sizeof req);
    const uint32_t bytes = lay ? (uint32_t)sizeof req : (uint32_t)sizeof req.base;
    hdr_init(&req.base.head.hdr, HELIOS_ESCAPE_FOREIGN_RESOURCE, bytes);
    req.base.head.abi_version = HELIOS_FOREIGN_ABI_VERSION;
    req.base.head.op = HELIOS_FOREIGN_OP_IMPORT_RM;
    req.base.head.status = SENT_STATUS;
    req.base.ctx_id = ctx;
    req.base.rm_handle = rm;
    req.base.gem_handle = gem;
    req.base.flags = flags;
    req.base.size = size;
    req.base.out_resource_id = SENT_U32;
    req.base.out_host_errno = SENT_U32;
    if (lay)
        req.layout = *lay;
    memset(o, 0, sizeof *o);
    o->rc = crm_win_escape_raw(&req, bytes, &o->nt);
    o->status = req.base.head.status;
    o->resid = req.base.out_resource_id;
    o->host_errno = req.base.out_host_errno;
    o->epoch = req.base.head.epoch;
}

/* ---- one RM memory object as a GEM object on a DRM node ------------------ */

/* RM memory -> GEM handle on the DRM node, as crm_scanout_smoke. */
static int export_to_gem(crm_client *c, uint32_t dev, uint32_t mem, uint64_t size, int drm_fd,
                         uint32_t *gem)
{
    int ctl = -1;
    int r = crm_win_open_device(CRM_WIN_DEV_CTL, &ctl);
    char w[160];
    snprintf(w, sizeof w, "open a fresh control channel for the export (handle %d)", ctl);
    if (step(w, r))
        return r;

    NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS ex;
    memset(&ex, 0, sizeof ex);
    ex.object.type = NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TYPE_RM;
    ex.object.data.rmObject.hDevice = dev;
    ex.object.data.rmObject.hParent = dev;
    ex.object.data.rmObject.hObject = mem;
    ex.fd = ctl;
    r = crm_control(c, crm_root(c), NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD, &ex, sizeof ex);
    if (step("NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD", r))
        goto out;

    struct nvkms_kapi_priv_import_memory_params nv;
    memset(&nv, 0, sizeof nv);
    nv.memFd = ctl;
    nv.layout = 1; /* pitch */
    struct drm_nvidia_gem_import_nvkms_memory_params imp;
    memset(&imp, 0, sizeof imp);
    imp.mem_size = size;
    imp.nvkms_params_ptr = (uint64_t)(uintptr_t)&nv;
    imp.nvkms_params_size = sizeof nv;
    r = crm_win_ioctl(drm_fd, DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY, &imp, sizeof imp, &nv,
                      sizeof nv);
    if (r == 0 && imp.handle == 0)
        r = -EIO;
    snprintf(w, sizeof w, "DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY -> GEM handle %u", imp.handle);
    if (step(w, r) == 0)
        *gem = imp.handle;
out:
    /* nvidia-drm holds its own reference now; the channel was the envelope. */
    crm_win_close_device(ctl);
    return r;
}

/* ---- the cases ------------------------------------------------------------- */

static uint32_t stray_resid; /* an import a refusal case got by mistake: released at the end */
static uint32_t stray_ctx;

/* One import that must be refused with `want_status` and a host errno of
 * `errno_a` (or `errno_b`), and no resource id. */
static void expect_refused(const char *name, uint32_t ctx, uint32_t rm, uint32_t gem,
                           uint32_t flags, uint64_t size,
                           const struct helios_foreign_layout *lay, int32_t want_status,
                           uint32_t errno_a, uint32_t errno_b)
{
    struct imp_out o;
    import_rm(&o, ctx, rm, gem, flags, size, lay);
    char w[400], e[160];
    snprintf(w, sizeof w,
             "%s: %s (%d), host errno %u, resource id %u, escape rc %d NTSTATUS 0x%08x", name,
             fst_name(o.status), o.status, o.host_errno, o.resid, o.rc, (unsigned)o.nt);
    if (errno_a == errno_b)
        snprintf(e, sizeof e, "%s, host errno %u, resource id 0", fst_name(want_status), errno_a);
    else
        snprintf(e, sizeof e, "%s, host errno %u or %u, resource id 0", fst_name(want_status),
                 errno_a, errno_b);
    const int ok = o.rc == 0 && o.status == want_status &&
                   (o.host_errno == errno_a || o.host_errno == errno_b) && o.resid == 0;
    report(ok, w, e);
    if (o.rc == 0 && o.status == HELIOS_FOREIGN_ST_OK && o.resid != 0 && o.resid != SENT_U32) {
        /* The KMD admitted it: do not leave it behind. */
        int32_t nt = 0;
        const int r = release_blob(ctx, o.resid, &nt);
        printf("       released the unexpected resource %u: rc %d NTSTATUS 0x%08x\n", o.resid, r,
               (unsigned)nt);
        if (r)
            stray_resid = o.resid, stray_ctx = ctx;
    }
}

int main(int argc, char **argv)
{
    const unsigned dri = argc > 1 ? (unsigned)strtoul(argv[1], NULL, 0) : 0;

    crm_client *c = NULL;
    uint32_t dev = 0, sub = 0, mem = 0, gem = 0, ctx = 0, resid = 0;
    uint64_t mem_size = 0;
    int drm_fd = -1, probe_fd = -1, skipped = 0;
    char w[500], e[200];
    struct helios_foreign_query_caps caps0, caps1;
    int32_t nt = 0;
    int r;

    if (step("crm_open", crm_open(&c, NULL)))
        return 1;
    printf("       RM version %s, root client 0x%08x\n", crm_rm_version(c), crm_root(c));

    /* (1) QUERY_CAPS: is the host half installed? */
    r = query_caps(&caps0, &nt);
    if (r == -ENOSYS) {
        printf("SKIP: the KMD has no HELIOS_ESCAPE_FOREIGN_RESOURCE (NTSTATUS 0x%08x)\n",
               (unsigned)nt);
        skipped = 1;
        goto out;
    }
    snprintf(w, sizeof w,
             "FOREIGN_RESOURCE QUERY_CAPS: escape rc %d NTSTATUS 0x%08x, status %s (%d), epoch "
             "%" PRIu64,
             r, (unsigned)nt, fst_name(caps0.head.status), caps0.head.status, caps0.head.epoch);
    if (report(r == 0 && caps0.head.status == HELIOS_FOREIGN_ST_OK, w, "rc 0, ST_OK"))
        goto out;
    printf("       supported_ops 0x%" PRIx64 ", caps_flags 0x%x (CAP_RM_IMPORT %s), live %u (this "
           "device %u), imported %u, refused %u\n       limits: %u per device, %u total, %" PRIu64
           " bytes per resource, %" PRIu64 " per device\n",
           caps0.supported_ops, caps0.caps_flags,
           (caps0.caps_flags & HELIOS_FOREIGN_CAP_RM_IMPORT) ? "set" : "clear", caps0.live_total,
           caps0.live_owner, caps0.imported, caps0.refused, caps0.max_per_owner, caps0.max_total,
           caps0.max_bytes_per_resource, caps0.max_bytes_per_owner);
    if (report((caps0.supported_ops & (1u << HELIOS_FOREIGN_OP_IMPORT_RM)) != 0,
               "QUERY_CAPS lists IMPORT_RM in supported_ops", "bit 2 set"))
        goto out;
    if (!(caps0.caps_flags & HELIOS_FOREIGN_CAP_RM_IMPORT)) {
        printf("SKIP: the host does not serve the import yet (CAP_RM_IMPORT clear)\n");
        skipped = 1;
        goto out;
    }

    /* (2) vidmem -> GEM object on a DRM node. */
    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof dp);
    if (step("alloc NV01_DEVICE_0", crm_alloc(c, crm_root(c), &dev, NV01_DEVICE_0, &dp, sizeof dp)))
        goto cleanup;
    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    if (step("alloc NV20_SUBDEVICE_0", crm_alloc(c, dev, &sub, NV20_SUBDEVICE_0, &sp, sizeof sp)))
        goto cleanup;

    r = crm_win_open_device(CRM_WIN_DEV_DRI_BASE + dri, &drm_fd);
    snprintf(w, sizeof w, "open DRM render node %u (Open device_type %u) -> handle %d", dri,
             CRM_WIN_DEV_DRI_BASE + dri, drm_fd);
    if (step(w, r))
        goto cleanup;

    {
        const uint64_t want = ((uint64_t)STRIDE * H + 0xffff) & ~0xffffull;
        NV_MEMORY_ALLOCATION_PARAMS mp;
        memset(&mp, 0, sizeof mp);
        mp.owner = crm_root(c);
        mp.type = NVOS32_TYPE_IMAGE;
        mp.flags = NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE;
        mp.width = W;
        mp.height = H;
        mp.pitch = (int32_t)STRIDE;
        /* What nvk-rm allocates VRAM with: 64 KiB pages, not necessarily contiguous. */
        mp.attr = (NVOS32_ATTR_LOCATION_VIDMEM << NVOS32_ATTR_LOCATION_SHIFT) |
                  (NVOS32_ATTR_PAGE_SIZE_BIG << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
                  (NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
        mp.attr2 = NVOS32_ATTR2_ZBC_PREFER_NO_ZBC |
                   (NVOS32_ATTR2_GPU_CACHEABLE_YES << NVOS32_ATTR2_GPU_CACHEABLE_SHIFT);
        mp.size = want;
        mp.alignment = 64 * 1024;
        snprintf(w, sizeof w, "alloc NV01_MEMORY_LOCAL_USER %ux%u pitch %u (%" PRIu64 " bytes)", W,
                 H, STRIDE, want);
        if (step(w, crm_alloc(c, dev, &mem, NV01_MEMORY_LOCAL_USER, &mp, sizeof mp)))
            goto cleanup;
        mem_size = mp.size;
        printf("       memory 0x%08x, size 0x%" PRIx64 " (%" PRIu64 " bytes)\n", mem, mem_size,
               mem_size);
    }
    if (export_to_gem(c, dev, mem, mem_size, drm_fd, &gem))
        goto cleanup;

    /* A channel that is not a DRM file, for the ownership refusal below. */
    r = crm_win_open_device(CRM_WIN_DEV_CTL, &probe_fd);
    snprintf(w, sizeof w, "open a control channel as the non-DRI handle (handle %d)", probe_fd);
    if (step(w, r))
        goto cleanup;

    /* (3) a Venus context of this device. */
    r = ctx_create(&ctx, &nt);
    snprintf(w, sizeof w, "HELIOS_ESCAPE_CTX_CREATE (capset %u, Venus) -> context %u, rc %d "
             "NTSTATUS 0x%08x", VIRTIO_GPU_CAPSET_VENUS, ctx, r, (unsigned)nt);
    if (report(r == 0 && ctx != 0, w, "rc 0, nonzero context id"))
        goto cleanup;

    /* (4) IMPORT_RM with the layout of what was allocated. */
    const struct helios_foreign_layout good = {
        .width = W,
        .height = H,
        .stride = STRIDE,
        .offset = 0,
        .fourcc = DRM_FORMAT_XRGB8888,
        .reserved = 0,
        .modifier = DRM_FORMAT_MOD_LINEAR,
    };
    {
        struct imp_out o;
        import_rm(&o, ctx, (uint32_t)drm_fd, gem, HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT, mem_size,
                  &good);
        snprintf(w, sizeof w,
                 "IMPORT_RM (ctx %u, DRM node %d, GEM %u, size 0x%" PRIx64 ", %ux%u XRGB8888 "
                 "linear stride %u): %s (%d), host errno %u, resource id %u, escape rc %d "
                 "NTSTATUS 0x%08x, epoch %" PRIu64,
                 ctx, drm_fd, gem, mem_size, W, H, STRIDE, fst_name(o.status), o.status,
                 o.host_errno, o.resid, o.rc, (unsigned)o.nt, o.epoch);
        const int ok = o.rc == 0 && o.status == HELIOS_FOREIGN_ST_OK && o.resid != 0 &&
                       o.resid != SENT_U32 && o.host_errno == 0;
        if (!ok && o.rc == 0 && o.status == HELIOS_FOREIGN_ST_OK && o.resid != 0 &&
            o.resid != SENT_U32)
            resid = o.resid; /* admitted with a stray errno: still release it */
        if (report(ok, w, "rc 0, ST_OK, host errno 0, nonzero resource id"))
            goto cleanup;
        resid = o.resid;
        printf("       resource id %u\n", resid);
    }

    /* The table saw it. */
    r = query_caps(&caps1, &nt);
    snprintf(w, sizeof w, "QUERY_CAPS after the import: live %u (was %u), this device %u (was "
             "%u), imported %u (was %u)", caps1.live_total, caps0.live_total, caps1.live_owner,
             caps0.live_owner, caps1.imported, caps0.imported);
    snprintf(e, sizeof e, "this device %u, imported %u", caps0.live_owner + 1, caps0.imported + 1);
    report(r == 0 && caps1.live_owner == caps0.live_owner + 1 &&
               caps1.imported == caps0.imported + 1,
           w, e);

    /* Release it; a second release must fail. */
    {
        const uint32_t released = resid;
        r = release_blob(ctx, released, &nt);
        snprintf(w, sizeof w, "RELEASE_BLOB (ctx %u, resource %u): rc %d NTSTATUS 0x%08x", ctx,
                 released, r, (unsigned)nt);
        if (report(r == 0, w, "rc 0"))
            goto cleanup; /* still held: the cleanup tries again */
        resid = 0;

        r = release_blob(ctx, released, &nt);
        snprintf(w, sizeof w, "second RELEASE_BLOB (ctx %u, resource %u): rc %d NTSTATUS 0x%08x",
                 ctx, released, r, (unsigned)nt);
        report(r != 0, w, "the escape to fail (the resource is gone)");
    }
    r = query_caps(&caps1, &nt);
    snprintf(w, sizeof w, "QUERY_CAPS after the release: this device %u (was %u)",
             caps1.live_owner, caps0.live_owner);
    snprintf(e, sizeof e, "this device %u", caps0.live_owner);
    report(r == 0 && caps1.live_owner == caps0.live_owner, w, e);

    /* (5) Deliberate mistakes: the documented status AND host errno of each. */
    {
        const uint32_t lf = HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT;
        const uint32_t drm = (uint32_t)drm_fd;
        struct helios_foreign_layout bad_mod = good;
        bad_mod.modifier = MOD_NV_BLOCK_LINEAR_BASE | 6; /* h is 0..=5 */

        /* The host refuses; its errno comes back. */
        expect_refused("unknown GEM handle", ctx, drm, 0x00ffff00u, lf, mem_size, &good,
                       HELIOS_FOREIGN_ST_NOT_OWNED, HOST_ENOENT, HOST_ENOENT);
        expect_refused("size larger than the object", ctx, drm, gem, lf, mem_size * 4, &good,
                       HELIOS_FOREIGN_ST_BAD_RANGE, HOST_ERANGE, HOST_ERANGE);
        /* The KMD refuses before the host is asked (errno 0), or the host says EBADF. */
        expect_refused("non-DRI channel as rm_handle", ctx, (uint32_t)probe_fd, gem, lf, mem_size,
                       &good, HELIOS_FOREIGN_ST_NOT_OWNED, 0, HOST_EBADF);
        expect_refused("context this device did not create", 0x00ffff00u, drm, gem, lf, mem_size,
                       &good, HELIOS_FOREIGN_ST_BAD_CONTEXT, 0, 0);
        /* Validated by the KMD, no host traffic (errno 0). */
        expect_refused("no layout flag (72-byte request)", ctx, drm, gem, 0, mem_size, NULL,
                       HELIOS_FOREIGN_ST_BAD_RANGE, 0, 0);
        expect_refused("layout with a bad modifier", ctx, drm, gem, lf, mem_size, &bad_mod,
                       HELIOS_FOREIGN_ST_BAD_RANGE, 0, 0);
    }
    r = query_caps(&caps1, &nt);
    snprintf(w, sizeof w, "QUERY_CAPS after the refusals: this device %u, refused %u (was %u)",
             caps1.live_owner, caps1.refused, caps0.refused);
    snprintf(e, sizeof e, "this device %u, nothing left behind", caps0.live_owner);
    report(r == 0 && caps1.live_owner == caps0.live_owner, w, e);

cleanup:
    /* (6) Everything, in the order that keeps each step legal. */
    if (resid) {
        r = release_blob(ctx, resid, &nt);
        snprintf(w, sizeof w, "RELEASE_BLOB (ctx %u, resource %u) rc %d NTSTATUS 0x%08x", ctx,
                 resid, r, (unsigned)nt);
        step(w, r);
    }
    if (stray_resid) {
        r = release_blob(stray_ctx, stray_resid, &nt);
        snprintf(w, sizeof w, "RELEASE_BLOB of the unexpected resource %u rc %d NTSTATUS 0x%08x",
                 stray_resid, r, (unsigned)nt);
        step(w, r);
    }
    if (ctx) {
        r = ctx_destroy(ctx, &nt);
        snprintf(w, sizeof w, "HELIOS_ESCAPE_CTX_DESTROY (context %u) rc %d NTSTATUS 0x%08x", ctx,
                 r, (unsigned)nt);
        step(w, r);
    }
    if (probe_fd >= 0) {
        crm_win_close_device(probe_fd);
        printf("[ ok ] close the control channel\n");
    }
    if (gem) {
        struct {
            uint32_t handle, pad;
        } gc = { gem, 0 };
        snprintf(w, sizeof w, "DRM_IOCTL_GEM_CLOSE %u", gem);
        step(w, crm_win_ioctl(drm_fd, DRM_IOCTL_GEM_CLOSE, &gc, sizeof gc, NULL, 0));
    }
    if (mem)
        step("free the vidmem", crm_free(c, dev, mem));
    if (drm_fd >= 0) {
        crm_win_close_device(drm_fd);
        printf("[ ok ] close DRM render node\n");
    }
    if (sub)
        step("free subdevice", crm_free(c, dev, sub));
    if (dev)
        step("free device", crm_free(c, crm_root(c), dev));
out:
    printf("       objects still tracked: %zu, CPU mappings: %zu\n", crm_object_count(c),
           crm_mapping_count(c));
    crm_close(c);
    printf("[ ok ] crm_close\n");
    if (skipped) {
        printf("IMPORT SMOKE SKIPPED\n");
        return 0;
    }
    if (fails) {
        printf("%u check(s) failed; first failing step:\n  %s\n", fails, first_fail);
        printf("IMPORT SMOKE FAILED\n");
        return 1;
    }
    printf("IMPORT SMOKE PASSED\n");
    return 0;
}

#else /* !_WIN32 */

/* The KMD verbs exist on Windows only. Exit 77: skipped. */
int main(void)
{
    printf("crm_import_smoke: Windows only (HELIOS_ESCAPE_FOREIGN_RESOURCE is a Helios KMD verb)\n");
    return 77;
}

#endif
