/* SPDX-License-Identifier: MIT */
/*
 * librmclient core: RM client state, handle allocation, escape marshalling
 * and mapping bookkeeping. OS specifics are in the transport.
 */
#include "rmclient.h"
#include "rmclient_transport.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <threads.h>

#include "nv_ioctl_defs.h"

/* Library-chosen handles: 0x5c000001 upward. RM's own generated handles live
 * elsewhere (clients at 0xc1d00000, internal 0xcaf00000), so they do not
 * collide; a collision with a caller-chosen handle is skipped. */
#define CRM_HANDLE_BASE  0x5c000000u
#define CRM_HANDLE_SPAN  0x00ffffffu

enum { SLOT_EMPTY = 0, SLOT_USED, SLOT_DEAD, SLOT_RESERVED };

struct crm_obj {
    uint32_t handle;
    uint32_t parent;
    uint32_t hclass;
    uint32_t devinst;   /* NV01_DEVICE_0: deviceId from its alloc params */
    uint32_t children;  /* live tracked children */
    uint8_t  state;
};

struct crm_cpu_map {
    uint32_t device;
    uint32_t memory;
    uint32_t flags;
    int32_t  node;
    int      fd;        /* the mapping's own channel, -1 if none */
    void    *ptr;       /* what the caller got */
    void    *base;      /* what mmap returned (page aligned) */
    uint64_t maplen;    /* what mmap mapped */
    uint64_t offset;
    uint64_t length;
    uint64_t cookie;    /* RM's pLinearAddress, for NV_ESC_RM_UNMAP_MEMORY */
};

/* A crm_map_dma into a FERMI_VASPACE_A: the NV50_MEMORY_VIRTUAL the library
 * allocated for it. */
struct crm_va_bind {
    uint32_t vaspace;
    uint32_t virt;
    uint32_t memory;
    uint64_t gpu_va;
};

struct crm_gpu {
    nv_ioctl_card_info_t info;
    int      fd;        /* GPU channel, -1 if it could not be opened */
    int32_t  devinst;   /* RM device instance, -1 until queried */
};

struct crm_client {
    struct crm_transport t;
    mtx_t    lock;
    int      ctl_fd;
    uint32_t root;
    uint32_t next_handle;
    char     rm_version[NV_RM_API_VERSION_STRING_LENGTH];

    struct crm_obj *objs;
    size_t   obj_cap;     /* power of two */
    size_t   obj_used;    /* SLOT_USED */
    size_t   obj_fill;    /* SLOT_USED + SLOT_DEAD + SLOT_RESERVED */
    size_t   obj_reserved;

    struct crm_cpu_map *maps;
    size_t   map_count, map_cap;

    struct crm_va_bind *binds;
    size_t   bind_count, bind_cap;

    struct crm_gpu gpus[NV_MAX_DEVICES];
    int      gpu_count;
};

/* ---------------------------------------------------------------------- */
/* Object table: open addressing keyed by handle.                          */

static size_t hash_handle(uint32_t h)
{
    uint32_t x = h;
    x ^= x >> 16;
    x *= 0x7feb352du;
    x ^= x >> 15;
    x *= 0x846ca68bu;
    x ^= x >> 16;
    return x;
}

static struct crm_obj *obj_find(crm_client *c, uint32_t h)
{
    if (!c->obj_cap || h == 0)
        return NULL;
    size_t mask = c->obj_cap - 1;
    for (size_t i = hash_handle(h) & mask, n = 0; n < c->obj_cap; i = (i + 1) & mask, n++) {
        struct crm_obj *o = &c->objs[i];
        if (o->state == SLOT_EMPTY)
            return NULL;
        if (o->state != SLOT_DEAD && o->handle == h)
            return o;
    }
    return NULL;
}

static int obj_grow(crm_client *c)
{
    size_t ncap = c->obj_cap ? c->obj_cap * 2 : 64;
    /* Rehash in place at the same size when most of the fill is tombstones. */
    if (c->obj_cap && (c->obj_used + c->obj_reserved) * 2 < c->obj_cap)
        ncap = c->obj_cap;
    struct crm_obj *n = calloc(ncap, sizeof(*n));
    if (!n)
        return -ENOMEM;
    size_t mask = ncap - 1;
    for (size_t i = 0; i < c->obj_cap; i++) {
        struct crm_obj *o = &c->objs[i];
        if (o->state != SLOT_USED && o->state != SLOT_RESERVED)
            continue;
        size_t j = hash_handle(o->handle) & mask;
        while (n[j].state != SLOT_EMPTY)
            j = (j + 1) & mask;
        n[j] = *o;
    }
    free(c->objs);
    c->objs = n;
    c->obj_cap = ncap;
    c->obj_fill = c->obj_used + c->obj_reserved;
    return 0;
}

/* Insert (or overwrite) handle h in the given state. */
static struct crm_obj *obj_put(crm_client *c, uint32_t h, uint8_t state)
{
    struct crm_obj *o = obj_find(c, h);
    if (o) {
        if (o->state == SLOT_USED) c->obj_used--;
        if (o->state == SLOT_RESERVED) c->obj_reserved--;
    } else {
        if ((c->obj_fill + 1) * 4 > c->obj_cap * 3 && obj_grow(c) < 0)
            return NULL;
        size_t mask = c->obj_cap - 1;
        size_t i = hash_handle(h) & mask;
        while (c->objs[i].state == SLOT_USED || c->objs[i].state == SLOT_RESERVED)
            i = (i + 1) & mask;
        o = &c->objs[i];
        if (o->state == SLOT_EMPTY)
            c->obj_fill++;
    }
    memset(o, 0, sizeof(*o));
    o->handle = h;
    o->state = state;
    if (state == SLOT_USED) c->obj_used++;
    if (state == SLOT_RESERVED) c->obj_reserved++;
    return o;
}

static void obj_kill(crm_client *c, struct crm_obj *o)
{
    if (o->state == SLOT_USED) c->obj_used--;
    if (o->state == SLOT_RESERVED) c->obj_reserved--;
    o->state = SLOT_DEAD;
}

/* Reserve a fresh handle. Caller holds the lock. 0 on exhaustion/ENOMEM. */
static uint32_t handle_reserve(crm_client *c)
{
    for (uint32_t tries = 0; tries < CRM_HANDLE_SPAN; tries++) {
        uint32_t h = CRM_HANDLE_BASE | (c->next_handle & CRM_HANDLE_SPAN);
        c->next_handle = (c->next_handle + 1) & CRM_HANDLE_SPAN;
        if (c->next_handle == 0)
            c->next_handle = 1;
        if (h == c->root || (h & CRM_HANDLE_SPAN) == 0 || obj_find(c, h))
            continue;
        return obj_put(c, h, SLOT_RESERVED) ? h : 0;
    }
    return 0;
}

static void handle_unreserve(crm_client *c, uint32_t h)
{
    struct crm_obj *o = obj_find(c, h);
    if (o && o->state == SLOT_RESERVED)
        obj_kill(c, o);
}

/* ---------------------------------------------------------------------- */
/* CPU mapping list.                                                       */

static int map_add(crm_client *c, const struct crm_cpu_map *m)
{
    if (c->map_count == c->map_cap) {
        size_t ncap = c->map_cap ? c->map_cap * 2 : 16;
        struct crm_cpu_map *n = realloc(c->maps, ncap * sizeof(*n));
        if (!n)
            return -ENOMEM;
        c->maps = n;
        c->map_cap = ncap;
    }
    c->maps[c->map_count++] = *m;
    return 0;
}

static void map_del(crm_client *c, size_t i)
{
    c->maps[i] = c->maps[--c->map_count];
}

static void map_release_cpu(crm_client *c, const struct crm_cpu_map *m)
{
    if (c->t.unmap_memory) {
        struct crm_map_request req = {
            .h_client = c->root, .h_device = m->device, .h_memory = m->memory,
            .flags = m->flags, .offset = m->offset, .length = m->length,
            .node_hint = m->node,
        };
        c->t.unmap_memory(c->t.ctx, c->ctl_fd, &req, m->ptr, m->cookie);
    } else if (c->t.munmap) {
        c->t.munmap(c->t.ctx, m->base, m->maplen);
    }
    if (m->fd >= 0)
        c->t.close(c->t.ctx, m->fd);
}

static void binds_drop_handle(crm_client *c, uint32_t h)
{
    for (size_t i = 0; i < c->bind_count;) {
        if (c->binds[i].virt == h || c->binds[i].vaspace == h) {
            if (c->binds[i].vaspace == h) {
                /* RM frees VirtualMemory together with its VA space. */
                struct crm_obj *v = obj_find(c, c->binds[i].virt);
                if (v && v->state == SLOT_USED) {
                    struct crm_obj *po = obj_find(c, v->parent);
                    if (po && po->state == SLOT_USED && po->children)
                        po->children--;
                    obj_kill(c, v);
                }
            }
            c->binds[i] = c->binds[--c->bind_count];
        }
        else
            i++;
    }
}

/* Drop the CPU side of every mapping of `memory` (RM already forgot them). */
static void maps_drop_memory(crm_client *c, uint32_t memory)
{
    for (size_t i = 0; i < c->map_count;) {
        if (c->maps[i].memory == memory) {
            map_release_cpu(c, &c->maps[i]);
            map_del(c, i);
        } else {
            i++;
        }
    }
}

/* Forget handle h and every tracked descendant. Caller holds the lock. */
static void forget_subtree(crm_client *c, uint32_t h)
{
    struct crm_obj *o = obj_find(c, h);
    uint32_t parent = 0;
    if (o && o->state == SLOT_USED) {
        parent = o->parent;
        uint32_t *stack = NULL;
        size_t sp = 0, scap = 0;
        int need_scan = o->children > 0;
        obj_kill(c, o);
        maps_drop_memory(c, h);
        binds_drop_handle(c, h);
        if (need_scan) {
            stack = malloc(16 * sizeof(*stack));
            scap = stack ? 16 : 0;
            if (stack)
                stack[sp++] = h;
        }
        while (sp > 0) {
            uint32_t p = stack[--sp];
            for (size_t i = 0; i < c->obj_cap; i++) {
                struct crm_obj *k = &c->objs[i];
                if (k->state != SLOT_USED || k->parent != p)
                    continue;
                uint32_t kh = k->handle;
                int kids = k->children > 0;
                obj_kill(c, k);
                maps_drop_memory(c, kh);
                binds_drop_handle(c, kh);
                if (kids) {
                    if (sp == scap) {
                        uint32_t *ns = realloc(stack, scap * 2 * sizeof(*ns));
                        if (!ns)
                            continue; /* leaks bookkeeping only, never RM state */
                        stack = ns;
                        scap *= 2;
                    }
                    stack[sp++] = kh;
                }
            }
        }
        free(stack);
    } else if (o && o->state == SLOT_RESERVED) {
        obj_kill(c, o);
    }
    if (parent) {
        struct crm_obj *po = obj_find(c, parent);
        if (po && po->state == SLOT_USED && po->children)
            po->children--;
    }
}

/* ---------------------------------------------------------------------- */
/* Escapes.                                                                */

static int esc(crm_client *c, int fd, uint32_t nr, void *arg, uint32_t size)
{
    return c->t.ioctl(c->t.ctx, fd < 0 ? c->ctl_fd : fd, nr, arg, size);
}

static int rm_alloc_raw(crm_client *c, uint32_t root, uint32_t parent, uint32_t h,
                        uint32_t hclass, void *params, uint32_t size, uint32_t *out_h)
{
    NVOS64_PARAMETERS p;
    memset(&p, 0, sizeof(p));
    p.hRoot = root;
    p.hObjectParent = parent;
    p.hObjectNew = h;
    p.hClass = hclass;
    p.pAllocParms = (uint64_t)(uintptr_t)params;
    p.paramsSize = size;
    int r = esc(c, -1, NV_ESC_RM_ALLOC, &p, sizeof(p));
    if (r < 0)
        return r;
    if (p.status != NV_OK)
        return (int)p.status;
    if (out_h)
        *out_h = p.hObjectNew;
    return 0;
}

static int rm_free_raw(crm_client *c, uint32_t parent, uint32_t h)
{
    NVOS00_PARAMETERS p = { .hRoot = c->root, .hObjectParent = parent, .hObjectOld = h };
    int r = esc(c, -1, NV_ESC_RM_FREE, &p, sizeof(p));
    if (r < 0)
        return r;
    return (int)p.status;
}

static int check_version(crm_client *c)
{
    nv_ioctl_rm_api_version_t v;

    memset(&v, 0, sizeof(v));
    v.cmd = NV_RM_API_VERSION_CMD_QUERY;
    if (esc(c, -1, NV_ESC_CHECK_VERSION_STR, &v, sizeof(v)) == 0) {
        memcpy(c->rm_version, v.versionString, sizeof(c->rm_version));
        c->rm_version[sizeof(c->rm_version) - 1] = 0;
    }
    if (c->t.flags & CRM_TRANSPORT_NO_VERSION_CHECK)
        return 0;

    const char *want = getenv("CRM_RM_VERSION");
    if (!want || !*want)
        want = CRM_RM_BUILD_VERSION;
    else if (strcmp(want, "any") == 0)
        want = c->rm_version[0] ? c->rm_version : CRM_RM_BUILD_VERSION;

    memset(&v, 0, sizeof(v));
    v.cmd = NV_RM_API_VERSION_CMD_STRICT;
    snprintf(v.versionString, sizeof(v.versionString), "%s", want);
    int r = esc(c, -1, NV_ESC_CHECK_VERSION_STR, &v, sizeof(v));
    if (r == -EINVAL)
        return -EPROTO; /* RM: client and kernel versions differ */
    if (r < 0)
        return r;
    if (!c->rm_version[0])
        snprintf(c->rm_version, sizeof(c->rm_version), "%s", want);
    return 0;
}

static int read_card_info(crm_client *c)
{
    nv_ioctl_card_info_t *ci = calloc(NV_MAX_DEVICES, sizeof(*ci));
    if (!ci)
        return -ENOMEM;
    int r = esc(c, -1, NV_ESC_CARD_INFO, ci, NV_MAX_DEVICES * sizeof(*ci));
    if (r == 0) {
        c->gpu_count = 0;
        for (int i = 0; i < NV_MAX_DEVICES; i++) {
            if (!ci[i].valid)
                continue;
            struct crm_gpu *g = &c->gpus[c->gpu_count++];
            g->info = ci[i];
            g->fd = -1;
            g->devinst = -1;
        }
    }
    free(ci);
    return r;
}

static void open_gpu_channels(crm_client *c)
{
    for (int i = 0; i < c->gpu_count; i++) {
        struct crm_gpu *g = &c->gpus[i];
        int fd;
        if (c->t.open(c->t.ctx, (int32_t)g->info.minor_number, &fd) < 0)
            continue;
        g->fd = fd;
        if (!(c->t.flags & CRM_TRANSPORT_NO_REGISTER_FD)) {
            /* Ties the GPU channel to our control channel, as libnvidia
             * does. Failure is harmless: the channel still keeps the GPU
             * open for this process, which is what NV01_DEVICE_0 needs. */
            nv_ioctl_register_fd_t reg = { .ctl_fd = c->ctl_fd };
            (void)esc(c, fd, NV_ESC_REGISTER_FD, &reg, sizeof(reg));
        }
    }
}

int crm_open(crm_client **out, const struct crm_transport *transport)
{
    if (!out)
        return -EINVAL;
    *out = NULL;
    if (!transport)
        transport = crm_default_transport();
    if (!transport)
        return -ENOSYS;
    if (transport->abi != CRM_TRANSPORT_ABI || !transport->open || !transport->close ||
        !transport->ioctl)
        return -EINVAL;
    if (!transport->map_memory && (!transport->mmap || !transport->munmap))
        return -EINVAL;

    crm_client *c = calloc(1, sizeof(*c));
    if (!c)
        return -ENOMEM;
    c->t = *transport;
    if (c->t.page_size == 0 || (c->t.page_size & (c->t.page_size - 1)))
        c->t.page_size = 4096;
    c->ctl_fd = -1;
    c->next_handle = 1;
    if (mtx_init(&c->lock, mtx_plain) != thrd_success) {
        free(c);
        return -ENOMEM;
    }

    int r = c->t.open(c->t.ctx, CRM_NODE_CTL, &c->ctl_fd);
    if (r < 0)
        goto fail;
    r = check_version(c);
    if (r < 0)
        goto fail;
    r = read_card_info(c);
    if (r < 0)
        goto fail;
    /* RM picks the client handle (hObjectNew = 0); it is unique system-wide. */
    r = rm_alloc_raw(c, 0, 0, 0, NV01_ROOT_CLIENT, NULL, 0, &c->root);
    if (r != 0)
        goto fail;
    open_gpu_channels(c);
    *out = c;
    return 0;

fail:
    if (c->ctl_fd >= 0)
        c->t.close(c->t.ctx, c->ctl_fd);
    if (c->t.destroy)
        c->t.destroy(c->t.ctx);
    mtx_destroy(&c->lock);
    free(c);
    return r; /* -errno, or a positive NV_STATUS if RM refused the root client */
}

void crm_close(crm_client *c)
{
    if (!c)
        return;
    for (size_t i = 0; i < c->map_count; i++)
        map_release_cpu(c, &c->maps[i]);
    c->map_count = 0;
    if (c->root) {
        NVOS00_PARAMETERS p = { .hRoot = c->root, .hObjectParent = 0, .hObjectOld = c->root };
        (void)esc(c, -1, NV_ESC_RM_FREE, &p, sizeof(p));
    }
    for (int i = 0; i < c->gpu_count; i++)
        if (c->gpus[i].fd >= 0)
            c->t.close(c->t.ctx, c->gpus[i].fd);
    if (c->ctl_fd >= 0)
        c->t.close(c->t.ctx, c->ctl_fd);
    if (c->t.destroy)
        c->t.destroy(c->t.ctx);
    mtx_destroy(&c->lock);
    free(c->maps);
    free(c->binds);
    free(c->objs);
    free(c);
}

uint32_t crm_root(const crm_client *c)
{
    return c ? c->root : 0;
}

const char *crm_rm_version(const crm_client *c)
{
    return c ? c->rm_version : "";
}

int crm_ctl_fd(const crm_client *c)
{
    return c ? c->ctl_fd : -1;
}

uint32_t crm_new_handle(crm_client *c)
{
    if (!c)
        return 0;
    mtx_lock(&c->lock);
    uint32_t h = handle_reserve(c);
    mtx_unlock(&c->lock);
    return h;
}

void crm_release_handle(crm_client *c, uint32_t handle)
{
    if (!c)
        return;
    mtx_lock(&c->lock);
    handle_unreserve(c, handle);
    mtx_unlock(&c->lock);
}

int crm_alloc(crm_client *c, uint32_t parent, uint32_t *object, uint32_t hclass,
              void *params, uint32_t params_size)
{
    if (!c || !object || (params_size && !params))
        return -EINVAL;
    if (hclass == NV01_ROOT_CLIENT || hclass == 0)
        return -EINVAL; /* one root per crm_client; open another client instead */

    uint32_t h = *object;
    int picked = 0;
    if (h == 0) {
        mtx_lock(&c->lock);
        h = handle_reserve(c);
        mtx_unlock(&c->lock);
        if (!h)
            return -ENOMEM;
        picked = 1;
    }

    int r = rm_alloc_raw(c, c->root, parent ? parent : c->root, h, hclass,
                         params, params_size, NULL);

    mtx_lock(&c->lock);
    if (r != 0) {
        if (picked)
            handle_unreserve(c, h);
        mtx_unlock(&c->lock);
        return r;
    }
    struct crm_obj *o = obj_put(c, h, SLOT_USED);
    if (o) {
        o->parent = parent ? parent : c->root;
        o->hclass = hclass;
        o->devinst = 0;
        if (hclass == NV01_DEVICE_0 && params && params_size >= sizeof(uint32_t)) {
            uint32_t devid;
            memcpy(&devid, params, sizeof(devid));
            o->devinst = devid;
        }
        struct crm_obj *po = obj_find(c, o->parent);
        if (po && po->state == SLOT_USED)
            po->children++;
    }
    /* Out of memory for bookkeeping: the object exists in RM regardless and
     * goes away with the client; only crm_object_count undercounts. */
    mtx_unlock(&c->lock);
    *object = h;
    return 0;
}

int crm_free(crm_client *c, uint32_t parent, uint32_t object)
{
    if (!c || object == 0)
        return -EINVAL;
    if (object == c->root)
        return -EINVAL; /* use crm_close */
    if (parent == 0) {
        mtx_lock(&c->lock);
        struct crm_obj *o = obj_find(c, object);
        parent = (o && o->state == SLOT_USED) ? o->parent : c->root;
        mtx_unlock(&c->lock);
    }
    int r = rm_free_raw(c, parent, object);
    if (r != 0)
        return r;
    mtx_lock(&c->lock);
    forget_subtree(c, object);
    mtx_unlock(&c->lock);
    return 0;
}

int crm_free_quiet(crm_client *c, uint32_t parent, uint32_t object)
{
    int r = crm_free(c, parent, object);
    if (r == (int)NV_ERR_OBJECT_NOT_FOUND || r == (int)NV_ERR_INVALID_OBJECT_HANDLE) {
        mtx_lock(&c->lock);
        forget_subtree(c, object);
        mtx_unlock(&c->lock);
        r = 0;
    }
    return r;
}

int crm_control(crm_client *c, uint32_t object, uint32_t cmd, void *params, uint32_t params_size)
{
    if (!c || (params_size && !params))
        return -EINVAL;
    NVOS54_PARAMETERS p;
    memset(&p, 0, sizeof(p));
    p.hClient = c->root;
    p.hObject = object ? object : c->root;
    p.cmd = cmd;
    p.params = (uint64_t)(uintptr_t)params;
    p.paramsSize = params_size;
    int r = esc(c, -1, NV_ESC_RM_CONTROL, &p, sizeof(p));
    if (r < 0)
        return r;
    return (int)p.status;
}

/* ---------------------------------------------------------------------- */
/* CPU mappings.                                                           */

/* Which channel a mapping of `memory` under `device` most likely needs:
 * the control channel for system memory, else the GPU's channel. */
static int32_t map_node_hint(crm_client *c, uint32_t device, uint32_t memory)
{
    uint32_t mclass = 0, devinst = 0;
    int have_dev = 0;

    mtx_lock(&c->lock);
    struct crm_obj *m = obj_find(c, memory);
    if (m && m->state == SLOT_USED)
        mclass = m->hclass;
    uint32_t h = device;
    for (int depth = 0; depth < 8 && h && h != c->root; depth++) {
        struct crm_obj *o = obj_find(c, h);
        if (!o || o->state != SLOT_USED)
            break;
        if (o->hclass == NV01_DEVICE_0) {
            devinst = o->devinst;
            have_dev = 1;
            break;
        }
        h = o->parent;
    }
    mtx_unlock(&c->lock);

    if (mclass == NV01_MEMORY_SYSTEM || mclass == NV01_MEMORY_SYSTEM_OS_DESCRIPTOR)
        return CRM_NODE_CTL;
    if (c->gpu_count == 0)
        return CRM_NODE_CTL;
    if (c->gpu_count == 1 || !have_dev)
        return (int32_t)c->gpus[0].info.minor_number;

    for (int i = 0; i < c->gpu_count; i++) {
        struct crm_gpu *g = &c->gpus[i];
        if (g->devinst < 0) {
            NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS q;
            memset(&q, 0, sizeof(q));
            q.gpuId = g->info.gpu_id;
            if (crm_control(c, c->root, NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2, &q, sizeof(q)) == 0)
                g->devinst = (int32_t)q.deviceInstance;
        }
        if (g->devinst >= 0 && (uint32_t)g->devinst == devinst)
            return (int32_t)g->info.minor_number;
    }
    return (int32_t)c->gpus[0].info.minor_number;
}

static int32_t other_node(crm_client *c, int32_t node)
{
    if (node != CRM_NODE_CTL)
        return CRM_NODE_CTL;
    return c->gpu_count ? (int32_t)c->gpus[0].info.minor_number : CRM_NODE_CTL;
}

static void rm_unmap_cookie(crm_client *c, uint32_t device, uint32_t memory,
                            uint64_t cookie, uint32_t flags, uint32_t *status)
{
    NVOS34_PARAMETERS u;
    memset(&u, 0, sizeof(u));
    u.hClient = c->root;
    u.hDevice = device;
    u.hMemory = memory;
    u.pLinearAddress = cookie;
    u.flags = flags;
    int r = esc(c, -1, NV_ESC_RM_UNMAP_MEMORY, &u, sizeof(u));
    if (status)
        *status = r < 0 ? (uint32_t)r : u.status;
}

int crm_map_memory(crm_client *c, uint32_t device, uint32_t memory, uint64_t offset,
                   uint64_t length, uint32_t flags, void **cpu_ptr)
{
    if (!c || !cpu_ptr || length == 0 || !memory || !device)
        return -EINVAL;
    *cpu_ptr = NULL;

    int32_t hint = map_node_hint(c, device, memory);
    uint32_t prot = CRM_PROT_READ | CRM_PROT_WRITE;
    switch (flags & NVOS33_FLAGS_ACCESS_MASK) {
    case NVOS33_FLAGS_ACCESS_READ_ONLY:  prot = CRM_PROT_READ; break;
    case NVOS33_FLAGS_ACCESS_WRITE_ONLY: prot = CRM_PROT_WRITE; break;
    default: break;
    }

    struct crm_cpu_map m;
    memset(&m, 0, sizeof(m));
    m.device = device;
    m.memory = memory;
    m.flags = flags;
    m.offset = offset;
    m.length = length;
    m.fd = -1;

    if (c->t.map_memory) {
        struct crm_map_request req = {
            .h_client = c->root, .h_device = device, .h_memory = memory,
            .flags = flags, .offset = offset, .length = length, .node_hint = hint,
        };
        int r = c->t.map_memory(c->t.ctx, c->ctl_fd, &req, &m.ptr, &m.cookie);
        if (r != 0)
            return r;
        m.node = hint;
    } else {
        uint64_t pm = (uint64_t)c->t.page_size - 1;
        uint64_t start = offset & ~pm;
        uint64_t end = (offset + length + pm) & ~pm;
        if (end <= start)
            return -EINVAL;
        int32_t nodes[2] = { hint, other_node(c, hint) };
        int attempts = nodes[0] == nodes[1] ? 1 : 2;
        int r = 0;
        for (int a = 0; a < attempts; a++) {
            int fd;
            r = c->t.open(c->t.ctx, nodes[a], &fd);
            if (r < 0)
                return r;
            if (nodes[a] != CRM_NODE_CTL && !(c->t.flags & CRM_TRANSPORT_NO_REGISTER_FD)) {
                nv_ioctl_register_fd_t reg = { .ctl_fd = c->ctl_fd };
                (void)esc(c, fd, NV_ESC_REGISTER_FD, &reg, sizeof(reg));
            }
            nv_ioctl_nvos33_parameters_with_fd p;
            memset(&p, 0, sizeof(p));
            p.params.hClient = c->root;
            p.params.hDevice = device;
            p.params.hMemory = memory;
            p.params.offset = offset;
            p.params.length = length;
            p.params.flags = flags;
            p.fd = fd;
            r = esc(c, -1, NV_ESC_RM_MAP_MEMORY, &p, sizeof(p));
            if (r == 0 && p.params.status != NV_OK)
                r = (int)p.params.status;
            if (r != 0) {
                c->t.close(c->t.ctx, fd);
                /* Wrong channel kind for this memory (system memory maps on
                 * nvidiactl, BAR memory on nvidiaN): RM already undid its
                 * side, try the other kind. */
                if (r == (int)NV_ERR_INVALID_ARGUMENT && a + 1 < attempts)
                    continue;
                return r;
            }
            void *base = NULL;
            r = c->t.mmap(c->t.ctx, fd, 0, end - start, prot, &base);
            if (r < 0) {
                c->t.close(c->t.ctx, fd);
                rm_unmap_cookie(c, device, memory, p.params.pLinearAddress, flags, NULL);
                return r;
            }
            /* One channel per mapping, kept until unmap: RM (and Conduit's
             * backend) tie the mapping to it and refuse a second one. */
            m.fd = fd;
            m.base = base;
            m.maplen = end - start;
            m.ptr = (uint8_t *)base + (offset - start);
            m.cookie = p.params.pLinearAddress;
            m.node = nodes[a];
            break;
        }
    }

    mtx_lock(&c->lock);
    int r = map_add(c, &m);
    mtx_unlock(&c->lock);
    if (r < 0) {
        map_release_cpu(c, &m);
        rm_unmap_cookie(c, device, memory, m.cookie, flags, NULL);
        return r;
    }
    *cpu_ptr = m.ptr;
    return 0;
}

int crm_unmap_memory(crm_client *c, uint32_t device, uint32_t memory, void *cpu_ptr,
                     uint64_t length, uint32_t flags)
{
    if (!c || !cpu_ptr)
        return -EINVAL;
    struct crm_cpu_map m;
    int found = 0;
    mtx_lock(&c->lock);
    for (size_t i = 0; i < c->map_count; i++) {
        if (c->maps[i].ptr == cpu_ptr && c->maps[i].memory == memory &&
            (device == 0 || c->maps[i].device == device)) {
            m = c->maps[i];
            map_del(c, i);
            found = 1;
            break;
        }
    }
    mtx_unlock(&c->lock);
    if (!found)
        return -ENOENT;
    (void)length; /* the recorded length is authoritative */
    int fd = m.fd;
    m.fd = -1;
    map_release_cpu(c, &m);
    uint32_t status = 0;
    rm_unmap_cookie(c, m.device, m.memory, m.cookie, flags ? flags : m.flags, &status);
    if (fd >= 0)
        c->t.close(c->t.ctx, fd);
    return (int)status;
}

/* ---------------------------------------------------------------------- */
/* GPU mappings.                                                           */

static int rm_map_dma(crm_client *c, uint32_t device, uint32_t dma, uint32_t memory,
                      uint64_t offset, uint64_t length, uint32_t flags, uint32_t flags2,
                      uint32_t kind_override, uint64_t *gpu_va)
{
    NVOS46_PARAMETERS p;
    memset(&p, 0, sizeof(p));
    p.hClient = c->root;
    p.hDevice = device;
    p.hDma = dma;
    p.hMemory = memory;
    p.offset = offset;
    p.length = length;
    p.flags = flags;
    p.flags2 = flags2;
    p.kindOverride = kind_override;
    p.dmaOffset = *gpu_va;
    int r = esc(c, -1, NV_ESC_RM_MAP_MEMORY_DMA, &p, sizeof(p));
    if (r < 0)
        return r;
    if (p.status != NV_OK)
        return (int)p.status;
    *gpu_va = p.dmaOffset;
    return 0;
}

static int rm_unmap_dma(crm_client *c, uint32_t device, uint32_t dma, uint32_t memory,
                        uint32_t flags, uint64_t gpu_va)
{
    NVOS47_PARAMETERS p;
    memset(&p, 0, sizeof(p));
    p.hClient = c->root;
    p.hDevice = device;
    p.hDma = dma;
    p.hMemory = memory;
    p.flags = flags;
    p.dmaOffset = gpu_va;
    p.size = 0; /* the whole mapping */
    int r = esc(c, -1, NV_ESC_RM_UNMAP_MEMORY_DMA, &p, sizeof(p));
    if (r < 0)
        return r;
    return (int)p.status;
}

/* If dma is a FERMI_VASPACE_A of this client, return its parent device. */
static uint32_t vaspace_device(crm_client *c, uint32_t dma)
{
    uint32_t dev = 0;
    mtx_lock(&c->lock);
    struct crm_obj *o = obj_find(c, dma);
    if (o && o->state == SLOT_USED && o->hclass == FERMI_VASPACE_A)
        dev = o->parent;
    mtx_unlock(&c->lock);
    return dev;
}

int crm_map_dma2(crm_client *c, uint32_t device, uint32_t dma, uint32_t memory,
                 uint64_t offset, uint64_t length, uint32_t flags, uint32_t flags2,
                 uint32_t kind_override, uint64_t *gpu_va)
{
    if (!c || !gpu_va)
        return -EINVAL;
    uint32_t vas_dev = vaspace_device(c, dma);
    if (!vas_dev)
        return rm_map_dma(c, device, dma, memory, offset, length, flags, flags2,
                          kind_override, gpu_va);
    if (length == 0)
        return -EINVAL;

    /* RM maps only into VirtualMemory or a ctxdma: carve one out of the VA
     * space for this mapping, at *gpu_va if the caller chose an address. */
    NV_MEMORY_ALLOCATION_PARAMS vp;
    memset(&vp, 0, sizeof(vp));
    vp.owner = c->root;
    vp.type = NVOS32_TYPE_IMAGE;
    vp.flags = NVOS32_ALLOC_FLAGS_VIRTUAL;
    vp.size = length;
    vp.hVASpace = dma;
    if (*gpu_va) {
        vp.flags |= NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE;
        vp.offset = *gpu_va;
    }
    uint32_t virt = 0;
    int r = crm_alloc(c, vas_dev, &virt, NV50_MEMORY_VIRTUAL, &vp, sizeof(vp));
    if (r != 0)
        return r;
    uint64_t va = 0; /* relative to the virtual allocation */
    r = rm_map_dma(c, device, virt, memory, offset, length, flags, flags2, kind_override, &va);
    if (r != 0) {
        crm_free(c, vas_dev, virt);
        return r;
    }
    mtx_lock(&c->lock);
    if (c->bind_count == c->bind_cap) {
        size_t ncap = c->bind_cap ? c->bind_cap * 2 : 16;
        struct crm_va_bind *n = realloc(c->binds, ncap * sizeof(*n));
        if (!n) {
            mtx_unlock(&c->lock);
            rm_unmap_dma(c, device, virt, memory, 0, va);
            crm_free(c, vas_dev, virt);
            return -ENOMEM;
        }
        c->binds = n;
        c->bind_cap = ncap;
    }
    c->binds[c->bind_count++] = (struct crm_va_bind){ dma, virt, memory, va };
    mtx_unlock(&c->lock);
    *gpu_va = va;
    return 0;
}

int crm_map_dma(crm_client *c, uint32_t device, uint32_t dma, uint32_t memory,
                uint64_t offset, uint64_t length, uint32_t flags, uint64_t *gpu_va)
{
    return crm_map_dma2(c, device, dma, memory, offset, length, flags, 0, 0, gpu_va);
}

int crm_unmap_dma(crm_client *c, uint32_t device, uint32_t dma, uint32_t memory,
                  uint32_t flags, uint64_t gpu_va)
{
    if (!c)
        return -EINVAL;
    uint32_t vas_dev = vaspace_device(c, dma);
    if (!vas_dev)
        return rm_unmap_dma(c, device, dma, memory, flags, gpu_va);

    uint32_t virt = 0;
    mtx_lock(&c->lock);
    for (size_t i = 0; i < c->bind_count; i++) {
        struct crm_va_bind *b = &c->binds[i];
        if (b->vaspace == dma && b->memory == memory && b->gpu_va == gpu_va) {
            virt = b->virt;
            c->binds[i] = c->binds[--c->bind_count];
            break;
        }
    }
    mtx_unlock(&c->lock);
    if (!virt)
        return -ENOENT;
    int r = rm_unmap_dma(c, device, virt, memory, flags, gpu_va);
    int f = crm_free(c, vas_dev, virt);
    return r ? r : f;
}

/* ---------------------------------------------------------------------- */
/* GPUs, events, raw escapes.                                              */

int crm_gpu_count(crm_client *c)
{
    return c ? c->gpu_count : -EINVAL;
}

int crm_gpu_info(crm_client *c, int index, uint32_t *gpu_id, uint32_t *minor,
                 uint32_t *pci_device_id)
{
    if (!c || index < 0 || index >= c->gpu_count)
        return -EINVAL;
    const nv_ioctl_card_info_t *i = &c->gpus[index].info;
    if (gpu_id) *gpu_id = i->gpu_id;
    if (minor) *minor = i->minor_number;
    if (pci_device_id) *pci_device_id = i->pci_info.device_id;
    return 0;
}

int crm_gpu_pci(crm_client *c, int index, uint32_t *domain, uint8_t *bus,
                uint8_t *slot, uint8_t *function, uint16_t *vendor_id)
{
    if (!c || index < 0 || index >= c->gpu_count)
        return -EINVAL;
    const nv_pci_info_t *p = &c->gpus[index].info.pci_info;
    if (domain) *domain = p->domain;
    if (bus) *bus = p->bus;
    if (slot) *slot = p->slot;
    if (function) *function = p->function;
    if (vendor_id) *vendor_id = p->vendor_id;
    return 0;
}

int crm_event_open(crm_client *c, uint32_t parent, uint32_t notify_index,
                   uint32_t *event_handle, int *event_fd)
{
    if (!c || !event_handle || !event_fd)
        return -EINVAL;
    int fd;
    int r = c->t.open(c->t.ctx, CRM_NODE_CTL, &fd);
    if (r < 0)
        return r;
    nv_ioctl_alloc_os_event_t e = { .hClient = c->root, .hDevice = parent, .fd = (uint32_t)fd };
    r = esc(c, fd, NV_ESC_ALLOC_OS_EVENT, &e, sizeof(e));
    if (r == 0 && e.Status != NV_OK)
        r = (int)e.Status;
    if (r != 0) {
        c->t.close(c->t.ctx, fd);
        return r;
    }
    NV0005_ALLOC_PARAMETERS ap;
    memset(&ap, 0, sizeof(ap));
    ap.hParentClient = c->root;
    ap.hSrcResource = parent;
    ap.hClass = NV01_EVENT_OS_EVENT;
    ap.notifyIndex = notify_index;
    ap.data = (uint64_t)(int64_t)fd;
    uint32_t h = *event_handle;
    r = crm_alloc(c, parent, &h, NV01_EVENT_OS_EVENT, &ap, sizeof(ap));
    if (r != 0) {
        nv_ioctl_free_os_event_t fe = { .hClient = c->root, .hDevice = parent, .fd = (uint32_t)fd };
        (void)esc(c, fd, NV_ESC_FREE_OS_EVENT, &fe, sizeof(fe));
        c->t.close(c->t.ctx, fd);
        return r;
    }
    *event_handle = h;
    *event_fd = fd;
    return 0;
}

int crm_event_close(crm_client *c, uint32_t event_handle, int event_fd)
{
    if (!c || event_fd < 0)
        return -EINVAL;
    uint32_t parent = 0;
    int r = 0;
    if (event_handle) {
        mtx_lock(&c->lock);
        struct crm_obj *o = obj_find(c, event_handle);
        if (o && o->state == SLOT_USED)
            parent = o->parent;
        mtx_unlock(&c->lock);
        r = crm_free_quiet(c, parent, event_handle);
    }
    nv_ioctl_free_os_event_t e = { .hClient = c->root, .hDevice = parent, .fd = (uint32_t)event_fd };
    int f = esc(c, event_fd, NV_ESC_FREE_OS_EVENT, &e, sizeof(e));
    if (f == 0 && e.Status != NV_OK)
        f = (int)e.Status;
    c->t.close(c->t.ctx, event_fd);
    return r ? r : f;
}

int crm_event_drain(crm_client *c, int event_fd, struct crm_event_data *out, int max)
{
    if (!c || event_fd < 0 || max < 0 || (max && !out))
        return -EINVAL;
    int n = 0;
    for (int guard = 0; guard < 4096; guard++) {
        NvUnixEvent ev;
        memset(&ev, 0, sizeof(ev));
        NVOS41_PARAMETERS p;
        memset(&p, 0, sizeof(p));
        p.pEvent = (uint64_t)(uintptr_t)&ev;
        int r = esc(c, event_fd, NV_ESC_RM_GET_EVENT_DATA, &p, sizeof(p));
        if (r < 0)
            return n ? n : r;
        if (p.status != NV_OK)
            break; /* NV_ERR_OPERATING_SYSTEM: nothing (more) queued */
        if (n < max) {
            out[n].object = ev.hObject;
            out[n].notify_index = ev.NotifyIndex;
            out[n].info32 = ev.info32;
            out[n].info16 = ev.info16;
        }
        n++;
        if (!p.MoreEvents)
            break;
    }
    return n;
}

int crm_escape(crm_client *c, int fd, uint32_t nr, void *arg, uint32_t size)
{
    if (!c || (size && !arg))
        return -EINVAL;
    return esc(c, fd, nr, arg, size);
}

size_t crm_object_count(const crm_client *c)
{
    if (!c)
        return 0;
    mtx_t *l = (mtx_t *)&c->lock;
    mtx_lock(l);
    size_t n = c->obj_used;
    mtx_unlock(l);
    return n;
}

size_t crm_mapping_count(const crm_client *c)
{
    if (!c)
        return 0;
    mtx_t *l = (mtx_t *)&c->lock;
    mtx_lock(l);
    size_t n = c->map_count;
    mtx_unlock(l);
    return n;
}
