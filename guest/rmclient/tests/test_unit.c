/* SPDX-License-Identifier: MIT */
/*
 * librmclient unit tests against a fake transport that plays a tiny RM:
 * no GPU, no kernel. Covers handle allocation, escape marshalling, error
 * mapping and CPU-mapping bookkeeping.
 */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "rmclient.h"
#include "rmclient_transport.h"
#include "nv_ioctl_defs.h"

#define NV_ERR_INVALID_CLASS      0x22u
#define NV_ERR_NOT_SUPPORTED      0x56u
#define FAKE_CLIENT               0xc1d00001u
#define FAKE_SYSMEM_OTHER         0x3Fu /* a class the library doesn't know is sysmem */

static int failures, checks;
#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
    fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)
#define CHECK_EQ(a, b) do { long long _a = (long long)(a), _b = (long long)(b); checks++; \
    if (_a != _b) { failures++; fprintf(stderr, "%s:%d: %s == %s failed: 0x%llx vs 0x%llx\n", \
    __FILE__, __LINE__, #a, #b, _a, _b); } } while (0)

/* ---------------------------------------------------------------------- */
/* Fake RM                                                                 */

#define MAXFD 64
#define MAXOBJ 4096

struct fobj { uint32_t h, parent, hclass; int live; uint64_t va; uint64_t data; };

struct fake {
    char version[64];
    int fd_open[MAXFD];
    int32_t fd_node[MAXFD];
    int open_calls;
    /* pending mmap context per fd */
    int fd_ctx[MAXFD];
    uint64_t fd_ctx_len[MAXFD];
    uint64_t fd_ctx_cookie[MAXFD];

    int client_live;
    struct fobj objs[MAXOBJ];
    int nobj;

    /* recorded payloads */
    NVOS64_PARAMETERS last_alloc;
    NVOS54_PARAMETERS last_ctrl;
    NVOS00_PARAMETERS last_free;
    NVOS46_PARAMETERS last_map_dma;
    NVOS47_PARAMETERS last_unmap_dma;
    NVOS34_PARAMETERS last_unmap;
    nv_ioctl_alloc_os_event_t last_event;
    int map_attempts, mmaps, munmaps, unmaps, register_fds, strict_checks;
    NV_MEMORY_ALLOCATION_PARAMS last_virt_params;
    NV0005_ALLOC_PARAMETERS last_event_params;
    nv_ioctl_nvos02_parameters_with_fd last_alloc_memory;
    NvUnixEvent queue[8];
    int queued;
    int64_t live_mmaps;
    uint64_t next_cookie;

    /* fault injection: next escape `fail_nr` returns -fail_errno */
    uint32_t fail_nr;
    int fail_errno;
};

static struct fake F;

static void fake_reset(void)
{
    memset(&F, 0, sizeof(F));
    strcpy(F.version, "610.57.04");
    F.next_cookie = 0x7f0000000000ull;
}

static struct fobj *fobj_find(uint32_t h)
{
    for (int i = 0; i < F.nobj; i++)
        if (F.objs[i].live && F.objs[i].h == h)
            return &F.objs[i];
    return NULL;
}

static int fake_live_objects(void)
{
    int n = 0;
    for (int i = 0; i < F.nobj; i++)
        n += F.objs[i].live;
    return n;
}

static void fobj_free_tree(uint32_t h)
{
    for (int i = 0; i < F.nobj; i++) {
        if (F.objs[i].live && F.objs[i].parent == h) {
            F.objs[i].live = 0;
            fobj_free_tree(F.objs[i].h);
        }
    }
}

static int is_sysmem_class(uint32_t c)
{
    return c == NV01_MEMORY_SYSTEM || c == NV01_MEMORY_SYSTEM_OS_DESCRIPTOR || c == FAKE_SYSMEM_OTHER;
}

static int f_open(void *ctx, int32_t node, int *fd)
{
    (void)ctx;
    F.open_calls++;
    for (int i = 3; i < MAXFD; i++) {
        if (!F.fd_open[i]) {
            F.fd_open[i] = 1;
            F.fd_node[i] = node;
            F.fd_ctx[i] = 0;
            *fd = i;
            return 0;
        }
    }
    return -EMFILE;
}

static void f_close(void *ctx, int fd)
{
    (void)ctx;
    if (fd >= 0 && fd < MAXFD)
        F.fd_open[fd] = 0;
}

static int f_ioctl(void *ctx, int fd, uint32_t nr, void *arg, uint32_t size)
{
    (void)ctx;
    if (fd < 0 || fd >= MAXFD || !F.fd_open[fd])
        return -EBADF;
    if (F.fail_nr && F.fail_nr == nr) {
        F.fail_nr = 0;
        return -F.fail_errno;
    }
    switch (nr) {
    case NV_ESC_CHECK_VERSION_STR: {
        nv_ioctl_rm_api_version_t *v = arg;
        if (size != sizeof(*v)) return -EINVAL;
        v->reply = NV_RM_API_VERSION_REPLY_RECOGNIZED;
        if (v->cmd == NV_RM_API_VERSION_CMD_QUERY) {
            strcpy(v->versionString, F.version);
            return 0;
        }
        F.strict_checks++;
        if (strcmp(v->versionString, F.version) != 0) {
            v->reply = NV_RM_API_VERSION_REPLY_UNRECOGNIZED;
            return -EINVAL;
        }
        return 0;
    }
    case NV_ESC_CARD_INFO: {
        nv_ioctl_card_info_t *ci = arg;
        if (size == 0 || size % sizeof(*ci)) return -EINVAL;
        memset(ci, 0, size);
        ci[0].valid = 1;
        ci[0].gpu_id = 0x100;
        ci[0].minor_number = 0;
        ci[0].pci_info.vendor_id = 0x10de;
        ci[0].pci_info.device_id = 0x2684;
        ci[0].pci_info.domain = 0x10;
        ci[0].pci_info.bus = 1;
        return 0;
    }
    case NV_ESC_REGISTER_FD: {
        nv_ioctl_register_fd_t *r = arg;
        if (size != sizeof(*r)) return -EINVAL;
        if (F.fd_node[fd] == CRM_NODE_CTL || F.fd_node[r->ctl_fd] != CRM_NODE_CTL) return -EINVAL;
        F.register_fds++;
        return 0;
    }
    case NV_ESC_RM_ALLOC: {
        NVOS64_PARAMETERS *p = arg;
        if (size != sizeof(*p)) return -EINVAL;
        if (F.fd_node[fd] != CRM_NODE_CTL) return -EINVAL;
        F.last_alloc = *p;
        if (p->hClass == NV01_ROOT_CLIENT) {
            p->hObjectNew = FAKE_CLIENT;
            F.client_live = 1;
            p->status = NV_OK;
            return 0;
        }
        if (!F.client_live || p->hRoot != FAKE_CLIENT) { p->status = 0x0Eu; /* INVALID_CLIENT */ return 0; }
        if (p->hClass == 0xdead) { p->status = NV_ERR_INVALID_CLASS; return 0; }
        if (p->hObjectParent != FAKE_CLIENT && !fobj_find(p->hObjectParent)) { p->status = NV_ERR_OBJECT_NOT_FOUND; return 0; }
        if (p->hObjectNew == 0 || p->hObjectNew == FAKE_CLIENT || fobj_find(p->hObjectNew)) { p->status = NV_ERR_INVALID_OBJECT_HANDLE; return 0; }
        if (F.nobj == MAXOBJ) { p->status = 0x51u; return 0; }
        uint64_t va = 0, data = 0;
        if (p->hClass == NV50_MEMORY_VIRTUAL) {
            NV_MEMORY_ALLOCATION_PARAMS *vp = (void *)(uintptr_t)p->pAllocParms;
            struct fobj *vas = fobj_find(vp->hVASpace);
            struct fobj *par = fobj_find(p->hObjectParent);
            if (p->paramsSize != sizeof(*vp) || !(vp->flags & NVOS32_ALLOC_FLAGS_VIRTUAL) ||
                !vas || vas->hclass != FERMI_VASPACE_A || !par || par->hclass != NV01_DEVICE_0) {
                p->status = NV_ERR_INVALID_ARGUMENT;
                return 0;
            }
            F.last_virt_params = *vp;
            va = (vp->flags & NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE) ? vp->offset : 0x200000000ull;
            data = vp->hVASpace;
            vp->offset = va; /* RM writes the placement back (in/out params) */
        }
        if (p->hClass == NV01_EVENT_OS_EVENT) {
            NV0005_ALLOC_PARAMETERS *ap = (void *)(uintptr_t)p->pAllocParms;
            F.last_event_params = *ap;
            if (p->paramsSize != sizeof(*ap) || ap->data >= MAXFD || !F.fd_open[ap->data]) {
                p->status = NV_ERR_INVALID_ARGUMENT;
                return 0;
            }
            data = ap->data;
        }
        F.objs[F.nobj++] = (struct fobj){ p->hObjectNew, p->hObjectParent, p->hClass, 1, va, data };
        p->status = NV_OK;
        return 0;
    }
    case NV_ESC_RM_ALLOC_MEMORY: {
        nv_ioctl_nvos02_parameters_with_fd *w = arg;
        NVOS02_PARAMETERS *p = &w->params;
        if (size != sizeof(*w)) return -EINVAL;
        if (F.fd_node[fd] == CRM_NODE_CTL) return -EINVAL; /* NV_ACTUAL_DEVICE_ONLY */
        F.last_alloc_memory = *w;
        struct fobj *par = fobj_find(p->hObjectParent);
        if (!par || par->hclass != NV01_DEVICE_0) { p->status = NV_ERR_OBJECT_NOT_FOUND; return 0; }
        if (p->hClass != NV01_MEMORY_SYSTEM_OS_DESCRIPTOR || !(p->flags & NVOS02_FLAGS_MAPPING_NO_MAP)) {
            p->status = 0x29u; /* INVALID_FLAGS */
            return 0;
        }
        if (p->hObjectNew == 0 || fobj_find(p->hObjectNew)) { p->status = NV_ERR_INVALID_OBJECT_HANDLE; return 0; }
        F.objs[F.nobj++] = (struct fobj){ p->hObjectNew, p->hObjectParent, p->hClass, 1, 0, 0 };
        p->status = NV_OK;
        return 0;
    }
    case NV_ESC_RM_FREE: {
        NVOS00_PARAMETERS *p = arg;
        if (size != sizeof(*p)) return -EINVAL;
        F.last_free = *p;
        if (p->hObjectOld == FAKE_CLIENT) {
            F.client_live = 0;
            for (int i = 0; i < F.nobj; i++) F.objs[i].live = 0;
            p->status = NV_OK;
            return 0;
        }
        struct fobj *o = fobj_find(p->hObjectOld);
        if (!o) { p->status = NV_ERR_OBJECT_NOT_FOUND; return 0; }
        o->live = 0;
        fobj_free_tree(o->h);
        if (o->hclass == FERMI_VASPACE_A)
            for (int i = 0; i < F.nobj; i++)
                if (F.objs[i].live && F.objs[i].hclass == NV50_MEMORY_VIRTUAL && F.objs[i].data == o->h)
                    F.objs[i].live = 0;
        p->status = NV_OK;
        return 0;
    }
    case NV_ESC_RM_CONTROL: {
        NVOS54_PARAMETERS *p = arg;
        if (size != sizeof(*p)) return -EINVAL;
        F.last_ctrl = *p;
        if (p->cmd == NV2080_CTRL_CMD_GPU_GET_NAME_STRING) {
            NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS *n = (void *)(uintptr_t)p->params;
            if (p->paramsSize != sizeof(*n)) { p->status = NV_ERR_INVALID_ARGUMENT; return 0; }
            strcpy((char *)n->gpuNameString.ascii, "Fake GPU");
            p->status = NV_OK;
        } else {
            p->status = NV_ERR_NOT_SUPPORTED;
        }
        return 0;
    }
    case NV_ESC_RM_MAP_MEMORY: {
        nv_ioctl_nvos33_parameters_with_fd *p = arg;
        if (size != sizeof(*p)) return -EINVAL;
        F.map_attempts++;
        struct fobj *m = fobj_find(p->params.hMemory);
        if (!m) { p->params.status = NV_ERR_OBJECT_NOT_FOUND; return 0; }
        int want_ctl = is_sysmem_class(m->hclass);
        int mfd = p->fd;
        if (mfd < 0 || mfd >= MAXFD || !F.fd_open[mfd] ||
            (F.fd_node[mfd] == CRM_NODE_CTL) != want_ctl) {
            p->params.status = NV_ERR_INVALID_ARGUMENT;
            return 0;
        }
        F.fd_ctx[mfd] = 1;
        uint64_t start = p->params.offset & ~0xfffull;
        uint64_t end = (p->params.offset + p->params.length + 0xfff) & ~0xfffull;
        F.fd_ctx_len[mfd] = end - start;
        F.fd_ctx_cookie[mfd] = F.next_cookie;
        p->params.pLinearAddress = F.next_cookie;
        F.next_cookie += 0x100000;
        p->params.status = NV_OK;
        return 0;
    }
    case NV_ESC_RM_UNMAP_MEMORY: {
        NVOS34_PARAMETERS *p = arg;
        if (size != sizeof(*p)) return -EINVAL;
        F.last_unmap = *p;
        F.unmaps++;
        p->status = NV_OK;
        return 0;
    }
    case NV_ESC_RM_MAP_MEMORY_DMA: {
        NVOS46_PARAMETERS *p = arg;
        if (size != sizeof(*p)) return -EINVAL;
        F.last_map_dma = *p;
        struct fobj *d = fobj_find(p->hDma);
        if (!fobj_find(p->hMemory) || !d) { p->status = NV_ERR_OBJECT_NOT_FOUND; return 0; }
        if (d->hclass == FERMI_VASPACE_A) { p->status = NV_ERR_INVALID_OBJECT_HANDLE; return 0; } /* as real RM */
        if (d->hclass == NV50_MEMORY_VIRTUAL)
            p->dmaOffset = d->va + p->dmaOffset;
        else if (p->dmaOffset == 0)
            p->dmaOffset = 0x200000000ull;
        p->status = NV_OK;
        return 0;
    }
    case NV_ESC_RM_UNMAP_MEMORY_DMA: {
        NVOS47_PARAMETERS *p = arg;
        if (size != sizeof(*p)) return -EINVAL;
        F.last_unmap_dma = *p;
        p->status = fobj_find(p->hDma) ? NV_OK : NV_ERR_OBJECT_NOT_FOUND;
        return 0;
    }
    case NV_ESC_RM_GET_EVENT_DATA: {
        NVOS41_PARAMETERS *p = arg;
        if (size != sizeof(*p)) return -EINVAL;
        if (F.queued == 0) { p->status = NV_ERR_OPERATING_SYSTEM; p->MoreEvents = 0; return 0; }
        *(NvUnixEvent *)(uintptr_t)p->pEvent = F.queue[0];
        memmove(F.queue, F.queue + 1, sizeof(F.queue[0]) * (size_t)--F.queued);
        p->MoreEvents = F.queued > 0;
        p->status = NV_OK;
        return 0;
    }
    case NV_ESC_ALLOC_OS_EVENT:
    case NV_ESC_FREE_OS_EVENT: {
        nv_ioctl_alloc_os_event_t *e = arg;
        if (size != sizeof(*e)) return -EINVAL;
        F.last_event = *e;
        e->Status = (e->fd == (uint32_t)fd || nr == NV_ESC_FREE_OS_EVENT) ? NV_OK : NV_ERR_INVALID_ARGUMENT;
        return 0;
    }
    default:
        return -ENOTTY;
    }
}

static int f_mmap(void *ctx, int fd, uint64_t offset, uint64_t length, uint32_t prot, void **ptr)
{
    (void)ctx; (void)prot;
    if (fd < 0 || fd >= MAXFD || !F.fd_open[fd] || !F.fd_ctx[fd]) return -EINVAL;
    if (offset != 0 || length != F.fd_ctx_len[fd]) return -ENXIO;
    void *m = aligned_alloc(4096, (size_t)length);
    if (!m) return -ENOMEM;
    memset(m, 0, (size_t)length);
    F.fd_ctx[fd] = 0;
    F.mmaps++;
    F.live_mmaps++;
    *ptr = m;
    return 0;
}

static int f_munmap(void *ctx, void *ptr, uint64_t length)
{
    (void)ctx; (void)length;
    free(ptr);
    F.munmaps++;
    F.live_mmaps--;
    return 0;
}

static const struct crm_transport fake_transport = {
    .abi = CRM_TRANSPORT_ABI,
    .name = "fake",
    .page_size = 4096,
    .open = f_open,
    .close = f_close,
    .ioctl = f_ioctl,
    .mmap = f_mmap,
    .munmap = f_munmap,
};

static int open_fds(void)
{
    int n = 0;
    for (int i = 0; i < MAXFD; i++)
        n += F.fd_open[i];
    return n;
}

static crm_client *open_client(void)
{
    crm_client *c = NULL;
    int r = crm_open(&c, &fake_transport);
    CHECK_EQ(r, 0);
    return c;
}

/* ---------------------------------------------------------------------- */

static void test_open_close(void)
{
    fake_reset();
    crm_client *c = open_client();
    if (!c) return;
    CHECK_EQ(crm_root(c), FAKE_CLIENT);
    CHECK(strcmp(crm_rm_version(c), "610.57.04") == 0);
    CHECK_EQ(F.strict_checks, 1);
    CHECK_EQ(crm_gpu_count(c), 1);
    uint32_t id = 0, minor = 99, dev = 0;
    CHECK_EQ(crm_gpu_info(c, 0, &id, &minor, &dev), 0);
    CHECK_EQ(id, 0x100);
    CHECK_EQ(minor, 0);
    CHECK_EQ(dev, 0x2684);
    CHECK_EQ(crm_gpu_info(c, 1, &id, &minor, &dev), -EINVAL);
    uint16_t vendor = 0;
    uint32_t domain = 0;
    CHECK_EQ(crm_gpu_pci(c, 0, &domain, NULL, NULL, NULL, &vendor), 0);
    CHECK_EQ(vendor, 0x10de);
    CHECK_EQ(domain, 0x10);
    CHECK_EQ(open_fds(), 2);          /* nvidiactl + nvidia0 */
    CHECK_EQ(F.register_fds, 1);
    CHECK_EQ(F.last_alloc.hClass, NV01_ROOT_CLIENT);
    CHECK_EQ(F.last_alloc.hRoot, 0);
    CHECK_EQ(F.last_alloc.hObjectNew, 0); /* RM picks the client handle */
    crm_close(c);
    CHECK_EQ(F.client_live, 0);
    CHECK_EQ(F.last_free.hObjectOld, FAKE_CLIENT);
    CHECK_EQ(open_fds(), 0);
}

static void test_version_mismatch(void)
{
    fake_reset();
    strcpy(F.version, "999.1.2");
    crm_client *c = NULL;
    CHECK_EQ(crm_open(&c, &fake_transport), -EPROTO);
    CHECK(c == NULL);
    CHECK_EQ(open_fds(), 0);
    CHECK(strcmp(crm_status_name(-EPROTO), "-EPROTO") == 0);

    setenv("CRM_RM_VERSION", "any", 1);
    CHECK_EQ(crm_open(&c, &fake_transport), 0);
    if (c) {
        CHECK(strcmp(crm_rm_version(c), "999.1.2") == 0);
        crm_close(c);
    }
    setenv("CRM_RM_VERSION", "999.1.2", 1);
    CHECK_EQ(crm_open(&c, &fake_transport), 0);
    if (c) crm_close(c);
    unsetenv("CRM_RM_VERSION");

    struct crm_transport t = fake_transport;
    t.flags = CRM_TRANSPORT_NO_VERSION_CHECK;
    F.strict_checks = 0;
    CHECK_EQ(crm_open(&c, &t), 0);
    CHECK_EQ(F.strict_checks, 0);
    if (c) crm_close(c);

    t = fake_transport;
    t.abi = 99;
    CHECK_EQ(crm_open(&c, &t), -EINVAL);
}

static void test_handles_and_marshalling(void)
{
    fake_reset();
    crm_client *c = open_client();
    if (!c) return;

    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof(dp));
    dp.deviceId = 0;
    uint32_t dev = 0;
    CHECK_EQ(crm_alloc(c, crm_root(c), &dev, NV01_DEVICE_0, &dp, sizeof(dp)), 0);
    CHECK(dev != 0 && dev != crm_root(c));
    CHECK_EQ(dev & 0xff000000u, 0x5c000000u);
    CHECK_EQ(F.last_alloc.hRoot, FAKE_CLIENT);
    CHECK_EQ(F.last_alloc.hObjectParent, FAKE_CLIENT);
    CHECK_EQ(F.last_alloc.hObjectNew, dev);
    CHECK_EQ(F.last_alloc.hClass, NV01_DEVICE_0);
    CHECK_EQ(F.last_alloc.pAllocParms, (uint64_t)(uintptr_t)&dp);
    CHECK_EQ(F.last_alloc.paramsSize, sizeof(dp));
    CHECK_EQ(F.last_alloc.pRightsRequested, 0);

    /* parent 0 means the root client */
    uint32_t sub = 0;
    NV2080_ALLOC_PARAMETERS sp = { 0 };
    CHECK_EQ(crm_alloc(c, dev, &sub, NV20_SUBDEVICE_0, &sp, sizeof(sp)), 0);
    CHECK(sub != dev && sub != 0);
    uint32_t other = 0;
    CHECK_EQ(crm_alloc(c, 0, &other, 0x1234, NULL, 0), 0);
    CHECK_EQ(F.last_alloc.hObjectParent, FAKE_CLIENT);
    CHECK_EQ(F.last_alloc.pAllocParms, 0);

    /* caller-chosen handle */
    uint32_t mine = 0x12340001;
    CHECK_EQ(crm_alloc(c, dev, &mine, 0x1234, NULL, 0), 0);
    CHECK_EQ(mine, 0x12340001);
    CHECK_EQ(F.last_alloc.hObjectNew, 0x12340001);
    CHECK_EQ(crm_object_count(c), 4);

    /* reserved handles are never handed out by crm_alloc */
    uint32_t r1 = crm_new_handle(c);
    CHECK(r1 != 0);
    uint32_t h2 = 0;
    CHECK_EQ(crm_alloc(c, 0, &h2, 0x1234, NULL, 0), 0);
    CHECK(h2 != r1);
    uint32_t r1b = r1;
    CHECK_EQ(crm_alloc(c, 0, &r1b, 0x1234, NULL, 0), 0); /* use the reservation */
    CHECK_EQ(r1b, r1);
    uint32_t r2 = crm_new_handle(c);
    crm_release_handle(c, r2);
    CHECK_EQ(crm_object_count(c), 6);

    /* bad arguments */
    CHECK_EQ(crm_alloc(c, 0, NULL, 0x1234, NULL, 0), -EINVAL);
    uint32_t z = 0;
    CHECK_EQ(crm_alloc(c, 0, &z, NV01_ROOT_CLIENT, NULL, 0), -EINVAL);
    CHECK_EQ(crm_alloc(c, 0, &z, 0x1234, NULL, 8), -EINVAL);
    CHECK_EQ(crm_free(c, 0, crm_root(c)), -EINVAL);

    /* many allocations and frees: table growth and tombstones */
    uint32_t hs[1000];
    int ok = 1;
    for (int i = 0; i < 1000; i++) {
        hs[i] = 0;
        ok &= crm_alloc(c, sub, &hs[i], 0x1234, NULL, 0) == 0;
        for (int j = 0; j < i && ok; j += 97)
            ok &= hs[j] != hs[i];
    }
    CHECK(ok);
    CHECK_EQ(crm_object_count(c), 1006);
    for (int i = 0; i < 1000; i += 2)
        ok &= crm_free(c, sub, hs[i]) == 0;
    CHECK(ok);
    CHECK_EQ(crm_object_count(c), 506);
    for (int i = 1; i < 1000; i += 2)
        ok &= crm_free(c, 0, hs[i]) == 0; /* parent looked up */
    CHECK(ok);
    CHECK_EQ(F.last_free.hObjectParent, sub);
    CHECK_EQ(crm_object_count(c), 6);
    CHECK_EQ(fake_live_objects(), 6);

    crm_close(c);
}

static void test_errors(void)
{
    fake_reset();
    crm_client *c = open_client();
    if (!c) return;

    uint32_t h = 0;
    CHECK_EQ(crm_alloc(c, 0, &h, 0xdead, NULL, 0), (int)NV_ERR_INVALID_CLASS);
    CHECK_EQ(h, 0);                       /* untouched on failure */
    CHECK_EQ(crm_object_count(c), 0);
    CHECK(strcmp(crm_status_name(NV_ERR_INVALID_CLASS), "NV_ERR_INVALID_CLASS") == 0);
    CHECK(strcmp(crm_status_string(NV_ERR_INVALID_CLASS), "Given class-id not valid") == 0);
    CHECK(strcmp(crm_status_name(0), "NV_OK") == 0);
    CHECK(strcmp(crm_status_name(0x57), "NV_ERR_OBJECT_NOT_FOUND") == 0);
    CHECK(strcmp(crm_status_name(-ENOENT), "-ENOENT") == 0);
    CHECK(strcmp(crm_status_name(0x7ffffff0), "NV_ERR_UNKNOWN") == 0);

    F.fail_nr = NV_ESC_RM_ALLOC;
    F.fail_errno = EIO;
    CHECK_EQ(crm_alloc(c, 0, &h, 0x1234, NULL, 0), -EIO);
    CHECK_EQ(crm_object_count(c), 0);

    /* a failed alloc releases its picked handle: the next one succeeds */
    CHECK_EQ(crm_alloc(c, 0, &h, 0x1234, NULL, 0), 0);
    CHECK_EQ(crm_object_count(c), 1);

    /* unknown parent */
    uint32_t h2 = 0;
    CHECK_EQ(crm_alloc(c, 0x11111111, &h2, 0x1234, NULL, 0), (int)NV_ERR_OBJECT_NOT_FOUND);

    /* duplicate caller handle */
    uint32_t dup = h;
    CHECK_EQ(crm_alloc(c, 0, &dup, 0x1234, NULL, 0), (int)NV_ERR_INVALID_OBJECT_HANDLE);
    CHECK_EQ(crm_object_count(c), 1);

    /* control: success, RM error, transport error */
    NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS n;
    memset(&n, 0, sizeof(n));
    CHECK_EQ(crm_control(c, h, NV2080_CTRL_CMD_GPU_GET_NAME_STRING, &n, sizeof(n)), 0);
    CHECK(strcmp((char *)n.gpuNameString.ascii, "Fake GPU") == 0);
    CHECK_EQ(F.last_ctrl.hClient, FAKE_CLIENT);
    CHECK_EQ(F.last_ctrl.hObject, h);
    CHECK_EQ(F.last_ctrl.cmd, NV2080_CTRL_CMD_GPU_GET_NAME_STRING);
    CHECK_EQ(F.last_ctrl.params, (uint64_t)(uintptr_t)&n);
    CHECK_EQ(F.last_ctrl.paramsSize, sizeof(n));
    CHECK_EQ(F.last_ctrl.flags, 0);
    CHECK_EQ(crm_control(c, h, 0x12345678, NULL, 0), (int)NV_ERR_NOT_SUPPORTED);
    CHECK_EQ(crm_control(c, 0, 0x12345678, NULL, 0), (int)NV_ERR_NOT_SUPPORTED);
    CHECK_EQ(F.last_ctrl.hObject, FAKE_CLIENT); /* 0 = the client */
    F.fail_nr = NV_ESC_RM_CONTROL;
    F.fail_errno = EFAULT;
    CHECK_EQ(crm_control(c, h, 0x12345678, NULL, 0), -EFAULT);

    /* free errors */
    CHECK_EQ(crm_free(c, 0, 0x22222222), (int)NV_ERR_OBJECT_NOT_FOUND);
    CHECK_EQ(crm_free_quiet(c, 0, 0x22222222), 0);
    F.fail_nr = NV_ESC_RM_FREE;
    F.fail_errno = EIO;
    CHECK_EQ(crm_free(c, 0, h), -EIO);
    CHECK_EQ(crm_object_count(c), 1);     /* still tracked: RM never saw it */
    CHECK_EQ(crm_free(c, 0, h), 0);
    CHECK_EQ(crm_object_count(c), 0);

    /* raw escape */
    nv_ioctl_rm_api_version_t v;
    memset(&v, 0, sizeof(v));
    v.cmd = NV_RM_API_VERSION_CMD_QUERY;
    CHECK_EQ(crm_escape(c, -1, NV_ESC_CHECK_VERSION_STR, &v, sizeof(v)), 0);
    CHECK(strcmp(v.versionString, "610.57.04") == 0);
    CHECK_EQ(crm_escape(c, -1, 0x99, NULL, 0), -ENOTTY);

    crm_close(c);
}

static void test_free_cascade(void)
{
    fake_reset();
    crm_client *c = open_client();
    if (!c) return;
    uint32_t dev = 0, sub = 0, vas = 0, mem = 0, mem2 = 0, other = 0;
    CHECK_EQ(crm_alloc(c, 0, &dev, NV01_DEVICE_0, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &sub, NV20_SUBDEVICE_0, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &vas, FERMI_VASPACE_A, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &mem, NV01_MEMORY_SYSTEM, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, sub, &mem2, 0x1234, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, 0, &other, 0x1234, NULL, 0), 0);
    CHECK_EQ(crm_object_count(c), 6);

    void *p = NULL;
    CHECK_EQ(crm_map_memory(c, dev, mem, 0, 8192, 0, &p), 0);
    CHECK_EQ(crm_mapping_count(c), 1);

    /* freeing the subdevice drops its child only */
    CHECK_EQ(crm_free(c, dev, sub), 0);
    CHECK_EQ(crm_object_count(c), 4);
    /* freeing the device drops everything under it, CPU mappings included */
    CHECK_EQ(crm_free(c, 0, dev), 0);
    CHECK_EQ(crm_object_count(c), 1);
    CHECK_EQ(crm_mapping_count(c), 0);
    CHECK_EQ(F.live_mmaps, 0);
    CHECK_EQ(fake_live_objects(), 1);
    crm_close(c);
}

static void test_cpu_mappings(void)
{
    fake_reset();
    crm_client *c = open_client();
    if (!c) return;
    uint32_t dev = 0, sub = 0, sys = 0, vid = 0, odd = 0;
    NV0080_ALLOC_PARAMETERS dp = { 0 };
    CHECK_EQ(crm_alloc(c, 0, &dev, NV01_DEVICE_0, &dp, sizeof(dp)), 0);
    CHECK_EQ(crm_alloc(c, dev, &sub, NV20_SUBDEVICE_0, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &sys, NV01_MEMORY_SYSTEM, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &vid, NV01_MEMORY_LOCAL_USER, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &odd, FAKE_SYSMEM_OTHER, NULL, 0), 0);
    int base_fds = open_fds();

    /* system memory maps through a fresh control channel, one attempt */
    void *ps = NULL;
    F.map_attempts = 0;
    CHECK_EQ(crm_map_memory(c, dev, sys, 0, 4096 * 3, 0, &ps), 0);
    CHECK(ps != NULL);
    CHECK_EQ(F.map_attempts, 1);
    CHECK_EQ(open_fds(), base_fds + 1); /* the mapping keeps its own channel */
    memset(ps, 0xab, 4096 * 3);

    /* video memory maps through a GPU channel; subdevice handle works too;
     * a sub-page offset comes back as base + offset */
    void *pv = NULL;
    F.map_attempts = 0;
    int regs = F.register_fds;
    CHECK_EQ(crm_map_memory(c, sub, vid, 0x1010, 100, 0, &pv), 0);
    CHECK_EQ(F.map_attempts, 1);
    CHECK_EQ(F.register_fds, regs + 1); /* GPU mapping channels are registered */
    CHECK_EQ(open_fds(), base_fds + 2);
    CHECK_EQ(((uintptr_t)pv) & 0xfff, 0x10);
    uint64_t vid_cookie = F.next_cookie - 0x100000;

    /* unknown sysmem class: GPU channel first, RM says INVALID_ARGUMENT,
     * then the control channel */
    void *po = NULL;
    F.map_attempts = 0;
    CHECK_EQ(crm_map_memory(c, dev, odd, 0, 4096, 0, &po), 0);
    CHECK_EQ(F.map_attempts, 2);
    CHECK_EQ(crm_mapping_count(c), 3);
    CHECK_EQ(F.live_mmaps, 3);

    /* errors */
    void *px = NULL;
    CHECK_EQ(crm_map_memory(c, dev, 0x33333333, 0, 4096, 0, &px), (int)NV_ERR_OBJECT_NOT_FOUND);
    CHECK(px == NULL);
    CHECK_EQ(crm_map_memory(c, dev, sys, 0, 0, 0, &px), -EINVAL);
    F.fail_nr = NV_ESC_RM_MAP_MEMORY;
    F.fail_errno = EIO;
    CHECK_EQ(crm_map_memory(c, dev, sys, 0, 4096, 0, &px), -EIO);
    CHECK_EQ(open_fds(), base_fds + 3);
    CHECK_EQ(crm_mapping_count(c), 3);

    /* unmap: CPU side and RM side with RM's cookie, not the CPU pointer */
    F.unmaps = 0;
    CHECK_EQ(crm_unmap_memory(c, sub, vid, pv, 100, 0), 0);
    CHECK_EQ(F.unmaps, 1);
    CHECK_EQ(F.last_unmap.pLinearAddress, vid_cookie);
    CHECK_EQ(F.last_unmap.hDevice, sub);
    CHECK_EQ(F.last_unmap.hMemory, vid);
    CHECK_EQ(F.last_unmap.hClient, FAKE_CLIENT);
    CHECK_EQ(crm_mapping_count(c), 2);
    CHECK_EQ(open_fds(), base_fds + 2); /* its channel closed on unmap */
    CHECK_EQ(crm_unmap_memory(c, sub, vid, pv, 100, 0), -ENOENT); /* twice */
    CHECK_EQ(crm_unmap_memory(c, dev, sys, po, 4096, 0), -ENOENT); /* wrong memory */
    CHECK_EQ(crm_unmap_memory(c, dev, sys, ps, 4096 * 3, 0), 0);
    CHECK_EQ(crm_mapping_count(c), 1);
    CHECK_EQ(F.live_mmaps, 1);
    CHECK_EQ(open_fds(), base_fds + 1);

    /* crm_close releases what is left */
    crm_close(c);
    CHECK_EQ(F.live_mmaps, 0);
    CHECK_EQ(open_fds(), 0);
}

static void test_gpu_mappings(void)
{
    fake_reset();
    crm_client *c = open_client();
    if (!c) return;
    uint32_t dev = 0, vas = 0, mem = 0;
    CHECK_EQ(crm_alloc(c, 0, &dev, NV01_DEVICE_0, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &vas, FERMI_VASPACE_A, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &mem, NV01_MEMORY_LOCAL_USER, NULL, 0), 0);

    uint64_t va = 0;
    CHECK_EQ(crm_map_dma(c, dev, vas, mem, 0x1000, 0x200000, 0x5, &va), 0);
    CHECK_EQ(va, 0x200000000ull);
    CHECK_EQ(F.last_map_dma.hClient, FAKE_CLIENT);
    CHECK_EQ(F.last_map_dma.hDevice, dev);
    CHECK(F.last_map_dma.hDma != vas);     /* mapped through an NV50_MEMORY_VIRTUAL */
    CHECK_EQ(F.last_virt_params.hVASpace, vas);
    CHECK_EQ(F.last_virt_params.size, 0x200000);
    CHECK_EQ(F.last_virt_params.flags & NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE, 0);
    CHECK_EQ(F.last_map_dma.dmaOffset, 0);  /* relative to the virtual object */
    CHECK_EQ(crm_object_count(c), 4);
    CHECK_EQ(F.last_map_dma.hMemory, mem);
    CHECK_EQ(F.last_map_dma.offset, 0x1000);
    CHECK_EQ(F.last_map_dma.length, 0x200000);
    CHECK_EQ(F.last_map_dma.flags, 0x5);
    CHECK_EQ(F.last_map_dma.flags2, 0);
    CHECK_EQ(F.last_map_dma.kindOverride, 0);

    uint64_t fixed = 0x300000000ull;
    CHECK_EQ(crm_map_dma2(c, dev, vas, mem, 0, 0x1000, 0x8000, 0x3, 0xfe, &fixed), 0);
    CHECK_EQ(fixed, 0x300000000ull);
    CHECK(F.last_virt_params.flags & NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE);
    CHECK_EQ(F.last_virt_params.offset, 0x300000000ull);
    CHECK_EQ(F.last_map_dma.flags2, 0x3);
    CHECK_EQ(F.last_map_dma.kindOverride, 0xfe);

    uint64_t bad = 0x1234;
    size_t before = crm_object_count(c);
    CHECK_EQ(crm_map_dma(c, dev, vas, 0x44444444, 0, 0x1000, 0, &bad), (int)NV_ERR_OBJECT_NOT_FOUND);
    CHECK_EQ(bad, 0x1234); /* untouched on failure */
    CHECK_EQ(crm_object_count(c), before); /* the virtual object was freed */

    CHECK_EQ(crm_unmap_dma(c, dev, vas, mem, 0x7, va), 0);
    CHECK_EQ(F.last_unmap_dma.hClient, FAKE_CLIENT);
    CHECK_EQ(F.last_unmap_dma.hDevice, dev);
    CHECK(F.last_unmap_dma.hDma != vas && F.last_unmap_dma.hDma != 0); /* the per-mapping virtual object */
    CHECK_EQ(F.last_unmap_dma.hMemory, mem);
    CHECK_EQ(F.last_unmap_dma.flags, 0x7);
    CHECK_EQ(F.last_unmap_dma.dmaOffset, va);
    CHECK_EQ(F.last_unmap_dma.size, 0);
    CHECK_EQ(crm_unmap_dma(c, dev, vas, mem, 0, va), -ENOENT); /* twice */
    CHECK_EQ(crm_unmap_dma(c, dev, vas, mem, 0, fixed), 0);
    CHECK_EQ(crm_object_count(c), 3);           /* virtual objects freed again */
    CHECK_EQ(fake_live_objects(), 3);

    /* a VirtualMemory handle goes to RM unchanged, dmaOffset relative */
    NV_MEMORY_ALLOCATION_PARAMS vp;
    memset(&vp, 0, sizeof(vp));
    vp.flags = NVOS32_ALLOC_FLAGS_VIRTUAL;
    vp.size = 1 << 30;
    vp.hVASpace = vas;
    uint32_t virt = 0;
    CHECK_EQ(crm_alloc(c, dev, &virt, NV50_MEMORY_VIRTUAL, &vp, sizeof(vp)), 0);
    CHECK_EQ(vp.offset, 0x200000000ull); /* in/out params come back */
    uint64_t rel = 0x10000;
    CHECK_EQ(crm_map_dma(c, dev, virt, mem, 0, 0x1000, 0, &rel), 0);
    CHECK_EQ(F.last_map_dma.hDma, virt);
    CHECK_EQ(rel, 0x200010000ull);
    CHECK_EQ(crm_unmap_dma(c, dev, virt, mem, 0, rel), 0);
    CHECK_EQ(F.last_unmap_dma.hDma, virt);

    /* bindings die with their VA space */
    uint64_t v3 = 0;
    CHECK_EQ(crm_map_dma(c, dev, vas, mem, 0, 0x1000, 0, &v3), 0);
    CHECK_EQ(crm_free(c, dev, vas), 0);
    CHECK_EQ(crm_unmap_dma(c, dev, vas, mem, 0, v3), (int)NV_ERR_OBJECT_NOT_FOUND);
    CHECK_EQ(crm_object_count(c), 3); /* dev, mem, and the caller's own virt (bookkeeping only) */
    crm_close(c);
}

static void test_os_descriptor(void)
{
    fake_reset();
    crm_client *c = open_client();
    if (!c) return;
    uint32_t dev = 0, mem = 0, vas = 0;
    CHECK_EQ(crm_alloc(c, 0, &dev, NV01_DEVICE_0, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &vas, FERMI_VASPACE_A, NULL, 0), 0);
    static char pages[3 * 4096] __attribute__((aligned(4096)));
    CHECK_EQ(crm_alloc_os_descriptor(c, dev, &mem, pages, sizeof(pages), 0), 0);
    CHECK(mem != 0);
    CHECK_EQ(F.last_alloc_memory.params.hRoot, FAKE_CLIENT);
    CHECK_EQ(F.last_alloc_memory.params.hObjectParent, dev);
    CHECK_EQ(F.last_alloc_memory.params.hClass, NV01_MEMORY_SYSTEM_OS_DESCRIPTOR);
    CHECK_EQ(F.last_alloc_memory.params.pMemory, (uint64_t)(uintptr_t)pages);
    CHECK_EQ(F.last_alloc_memory.params.limit, sizeof(pages) - 1);
    CHECK_EQ(F.last_alloc_memory.params.flags,
             NVOS02_FLAGS_COHERENCY_CACHED | NVOS02_FLAGS_PHYSICALITY_NONCONTIGUOUS |
             NVOS02_FLAGS_MAPPING_NO_MAP);
    CHECK_EQ(F.last_alloc_memory.fd, -1);
    CHECK_EQ(crm_object_count(c), 3);

    /* tracked like any object: maps into a VA space and frees */
    uint64_t va = 0;
    CHECK_EQ(crm_map_dma(c, dev, vas, mem, 0, sizeof(pages), 0, &va), 0);
    CHECK_EQ(crm_unmap_dma(c, dev, vas, mem, 0, va), 0);
    CHECK_EQ(crm_free(c, dev, mem), 0);
    CHECK_EQ(crm_object_count(c), 2);

    /* RM's refusal comes back as its status; the handle is not kept */
    uint32_t bad = 0;
    CHECK_EQ(crm_alloc_os_descriptor(c, dev, &bad, pages, sizeof(pages), 0x1u /* no NO_MAP */), 0x29);
    CHECK_EQ(bad, 0);
    CHECK_EQ(crm_object_count(c), 2);
    CHECK_EQ(crm_alloc_os_descriptor(c, dev, &bad, NULL, 4096, 0), -EINVAL);
    crm_close(c);
}

static void test_events(void)
{
    fake_reset();
    crm_client *c = open_client();
    if (!c) return;
    uint32_t dev = 0;
    CHECK_EQ(crm_alloc(c, 0, &dev, NV01_DEVICE_0, NULL, 0), 0);
    int before = open_fds();
    int efd = -1;
    uint32_t ev = 0;
    CHECK_EQ(crm_event_open(c, dev, 0x10000005, &ev, &efd), 0);
    CHECK(efd >= 0);
    CHECK(ev != 0);
    CHECK_EQ(open_fds(), before + 1);
    if (efd >= 0) CHECK_EQ(F.fd_node[efd], CRM_NODE_CTL);
    CHECK_EQ(F.last_event.hClient, FAKE_CLIENT);
    CHECK_EQ(F.last_event.fd, (uint32_t)efd);
    CHECK_EQ(F.last_alloc.hClass, NV01_EVENT_OS_EVENT);
    CHECK_EQ(F.last_alloc.hObjectParent, dev);
    CHECK_EQ(F.last_event_params.hParentClient, FAKE_CLIENT);
    CHECK_EQ(F.last_event_params.hSrcResource, dev);
    CHECK_EQ(F.last_event_params.hClass, NV01_EVENT_OS_EVENT);
    CHECK_EQ(F.last_event_params.notifyIndex, 0x10000005);
    CHECK_EQ(F.last_event_params.data, (uint64_t)efd);
    CHECK_EQ(crm_object_count(c), 2);

    struct crm_event_data d[2];
    CHECK_EQ(crm_event_drain(c, efd, d, 2), 0);
    F.queue[0] = (NvUnixEvent){ dev, 5, 0x11, 0x2 };
    F.queue[1] = (NvUnixEvent){ dev, 6, 0x22, 0x3 };
    F.queue[2] = (NvUnixEvent){ dev, 7, 0x33, 0x4 };
    F.queued = 3;
    CHECK_EQ(crm_event_drain(c, efd, d, 2), 3); /* all consumed, two stored */
    CHECK_EQ(d[0].object, dev);
    CHECK_EQ(d[0].notify_index, 5);
    CHECK_EQ(d[0].info32, 0x11);
    CHECK_EQ(d[1].notify_index, 6);
    CHECK_EQ(d[1].info16, 0x3);
    CHECK_EQ(F.queued, 0);
    CHECK_EQ(crm_event_drain(c, efd, NULL, 0), 0);

    CHECK_EQ(crm_event_close(c, ev, efd), 0);
    CHECK_EQ(open_fds(), before);
    CHECK_EQ(crm_object_count(c), 1);
    CHECK_EQ(fake_live_objects(), 1);

    /* failure to allocate the event object closes the channel */
    uint32_t ev2 = 0;
    int efd2 = -1;
    CHECK_EQ(crm_event_open(c, 0x77777777, 1, &ev2, &efd2), (int)NV_ERR_OBJECT_NOT_FOUND);
    CHECK_EQ(open_fds(), before);
    crm_close(c);
}

/* A transport that maps memory itself (the Windows KMD shape). */
static int custom_maps, custom_unmaps;
static char custom_buf[8192];
static int c_map(void *ctx, int ctl_fd, const struct crm_map_request *req, void **cpu, uint64_t *cookie)
{
    (void)ctx; (void)ctl_fd;
    if (req->h_client != FAKE_CLIENT || req->length != 100 || req->node_hint != CRM_NODE_CTL)
        return -EINVAL;
    custom_maps++;
    *cpu = custom_buf + req->offset;
    *cookie = 0xabcdef;
    return 0;
}
static int c_unmap(void *ctx, int ctl_fd, const struct crm_map_request *req, void *cpu, uint64_t cookie)
{
    (void)ctx; (void)ctl_fd; (void)req;
    if (cookie == 0xabcdef && cpu == custom_buf + 16)
        custom_unmaps++;
    return 0;
}

static void test_custom_map_transport(void)
{
    fake_reset();
    struct crm_transport t = fake_transport;
    t.mmap = NULL;
    t.munmap = NULL;
    t.map_memory = c_map;
    t.unmap_memory = c_unmap;
    crm_client *c = NULL;
    CHECK_EQ(crm_open(&c, &t), 0);
    if (!c) return;
    uint32_t dev = 0, sys = 0;
    CHECK_EQ(crm_alloc(c, 0, &dev, NV01_DEVICE_0, NULL, 0), 0);
    CHECK_EQ(crm_alloc(c, dev, &sys, NV01_MEMORY_SYSTEM, NULL, 0), 0);
    void *p = NULL;
    CHECK_EQ(crm_map_memory(c, dev, sys, 16, 100, 0, &p), 0);
    CHECK(p == custom_buf + 16);
    CHECK_EQ(custom_maps, 1);
    CHECK_EQ(crm_unmap_memory(c, dev, sys, p, 100, 0), 0);
    CHECK_EQ(custom_unmaps, 1);
    CHECK_EQ(F.last_unmap.pLinearAddress, 0xabcdef);
    crm_close(c);

    t.map_memory = NULL;
    CHECK_EQ(crm_open(&c, &t), -EINVAL); /* neither mmap nor map_memory */
}

int main(void)
{
    unsetenv("CRM_RM_VERSION");
    test_open_close();
    test_version_mismatch();
    test_handles_and_marshalling();
    test_errors();
    test_free_cascade();
    test_cpu_mappings();
    test_gpu_mappings();
    test_events();
    test_os_descriptor();
    test_custom_map_transport();
    printf("rmclient unit tests: %d checks, %d failures\n", checks, failures);
    return failures ? 1 : 0;
}
