/* SPDX-License-Identifier: MIT */
/*
 * rm_sysmem_flip: can RM SYSTEM memory be a scanout/flip source the way a
 * Conduit ScanoutFlip uses one?  (Linux host only; spike/sysmem-flip-source.)
 *
 * For each kind of RM memory it runs the chain a guest flip of that memory
 * would take on the host:
 *
 *   RM memory object, the (y << 16) | x pattern written by the CPU in the
 *   layout under test (LINEAR, or NVIDIA block-linear 2D h = 5 as NIL lays it
 *   out)
 *   -> NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD
 *   -> DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY on the NVIDIA render node (NVK's
 *      NVKMS surface parameters, as nvk-rm and the KMD's export do)
 *   -> PRIME (what the backend sends the viewer)
 *   -> EGL import on the NVIDIA GPU (what the viewer's compositor does) and
 *      a GPU draw sampling it, read back and checked at several pixels.
 *
 * Kinds:
 *   vidmem     NV01_MEMORY_LOCAL_USER (the reference; what NVK images are)
 *   sysmem     NV01_MEMORY_SYSTEM, PCI, cached
 *   sysmem-wc  NV01_MEMORY_SYSTEM, PCI, write-combined
 *   osdesc     NV01_MEMORY_SYSTEM_OS_DESCRIPTOR over pages this process
 *              allocated itself (crm_alloc_pages: anonymous, populated,
 *              MADV_DONTFORK) and RM pins, written through our own pointer
 *
 * It also times CPU reads through the RM CPU mapping (not for osdesc, which is
 * NO_MAP: our own pages are read instead) and through the dma-buf's mmap.
 *
 * Needs a guest/rmclient with the NV0000 OS_UNIX export controls
 * (feat/nvk-rm-windows-transport).  Build (from guest/rmclient, after make):
 *   cc -std=gnu11 -O2 -Wall -Iinclude -Isrc -I/usr/include/libdrm \
 *      ../nvk-rm/tests/rm_sysmem_flip.c build-make/librmclient.a \
 *      -ldrm -lEGL -lGLESv2 -lgbm -pthread -o build-make/rm_sysmem_flip
 * Run:
 *   build-make/rm_sysmem_flip [vidmem|sysmem|sysmem-wc|osdesc|udmabuf|all] [linear|bl5|all] [rm|dmabuf]
 *   (dmabuf: write the pattern through a writable mmap of the dma-buf, as a
 *   guest writes an RM-export blob mapped into region 3, then sample it)
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <errno.h>
#include <fcntl.h>
#include <gbm.h>
#include <inttypes.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <drm_fourcc.h>
#include <xf86drm.h>

#include "nv_ioctl_defs.h"
#include "rmclient.h"

#define W 1920u
#define H 1080u
#define BPP 4u

/* ---- nvidia-drm / NVKMS (nvidia-drm-ioctl.h, nvkms-kapi-private.h) ------ */

struct drm_nvidia_gem_import_nvkms_memory_params {
    uint64_t mem_size;
    uint64_t nvkms_params_ptr;
    uint64_t nvkms_params_size;
    uint32_t handle;
    uint32_t pad;
};
struct nvkms_kapi_priv_import_memory_params {
    int32_t memFd;
    uint32_t layout; /* 0 block linear, 1 pitch */
    uint32_t log2_gobs_x, log2_gobs_y, log2_gobs_z;
    uint32_t pitch_in_blocks;
    uint8_t generic_memory;
    uint8_t pad[3];
};
_Static_assert(sizeof(struct drm_nvidia_gem_import_nvkms_memory_params) == 32, "GEM_IMPORT");
_Static_assert(sizeof(struct nvkms_kapi_priv_import_memory_params) == 28, "NVKMS import");
#define DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY \
    DRM_IOWR(DRM_COMMAND_BASE + 0x01, struct drm_nvidia_gem_import_nvkms_memory_params)

#define NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS 0x3u
#define NVOS32_ATTR2_ZBC_PREFER_NO_ZBC 0x2u

static int failures;
/* argv[3] == "dmabuf": write the pattern through the dma-buf mmap. */
static bool via_dmabuf;

static void report(bool ok, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void report(bool ok, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    printf("[%s] ", ok ? " ok " : "FAIL");
    vprintf(fmt, ap);
    printf("\n");
    va_end(ap);
    fflush(stdout);
    if (!ok)
        failures++;
}

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

static double read_mbps(const volatile uint8_t *p, uint64_t size)
{
    volatile uint64_t sum = 0;
    double t0 = now();
    for (uint64_t o = 0; o < size; o += 8)
        sum += *(const volatile uint64_t *)(p + o);
    return size / (now() - t0) / 1e6;
}

static inline uint32_t pat(uint32_t x, uint32_t y) { return (y << 16) | x; }

/* ---- layouts (as rm_export_exec.c) ----------------------------------------- */

struct layout {
    const char *name;
    uint64_t modifier;
    bool block_linear;
    uint32_t log2_gob_h;
    uint32_t pitch;
    uint64_t size;
};

static inline uint32_t gob_off(uint32_t xb, uint32_t y)
{
    return (xb / 32) * 256 + (y / 4) * 128 + ((xb % 32) / 16) * 64 + (y % 4) * 16 + (xb % 16);
}

static inline uint64_t px_off(const struct layout *l, uint32_t x, uint32_t y)
{
    if (!l->block_linear)
        return (uint64_t)y * l->pitch + (uint64_t)x * BPP;
    const uint32_t xb = x * BPP;
    const uint32_t gobs_x = l->pitch / 64;
    const uint32_t bh = 8u << l->log2_gob_h;
    const uint64_t block = (uint64_t)(y / bh) * gobs_x + xb / 64;
    const uint32_t gob_in_block = (y % bh) / 8;
    return block * (512u << l->log2_gob_h) + gob_in_block * 512u + gob_off(xb % 64, y % 8);
}

static void layout_init(struct layout *l, const char *name)
{
    memset(l, 0, sizeof *l);
    l->name = name;
    if (!strcmp(name, "linear")) {
        l->modifier = DRM_FORMAT_MOD_LINEAR;
        l->pitch = W * BPP;
        l->size = (uint64_t)l->pitch * H;
    } else {
        l->modifier = DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 5);
        l->block_linear = true;
        l->log2_gob_h = 5;
        l->pitch = (W * BPP + 63) & ~63u;
        const uint32_t bh = 8u << l->log2_gob_h;
        const uint32_t brows = (H + bh - 1) / bh;
        l->size = (uint64_t)(l->pitch / 64) * brows * (512u << l->log2_gob_h);
    }
}

/* ---- RM ------------------------------------------------------------------- */

struct rm {
    crm_client *c;
    uint32_t dev, sub;
    int drm_fd;
};

struct surf {
    uint32_t mem;
    uint64_t size;
    volatile uint8_t *cpu; /* RM CPU mapping, or our own pages (osdesc) */
    void *own;             /* osdesc: our pages */
    bool rm_mapped;
    uint32_t gem;
    int dmabuf_fd;
};

static int rm_alloc(struct rm *rm, const char *kind, uint64_t want, struct surf *s)
{
    memset(s, 0, sizeof *s);
    s->dmabuf_fd = -1;
    s->size = (want + 0xffff) & ~0xffffull;
    int r;
    if (!strcmp(kind, "osdesc")) {
        r = crm_alloc_pages(rm->c, s->size, &s->own);
        report(r == 0, "  crm_alloc_pages 0x%" PRIx64 " B (anonymous, populated) (%d)", s->size, r);
        if (r)
            return r;
        r = crm_alloc_os_descriptor(rm->c, rm->dev, &s->mem, s->own, s->size, 0);
        report(r == 0,
               "  RM alloc NV01_MEMORY_SYSTEM_OS_DESCRIPTOR over them (PCI|CACHED|NONCONTIG|NO_MAP) "
               "-> 0x%08x (%s)",
               s->mem, crm_status_name(r));
        if (r)
            return r;
        s->cpu = s->own;
        return 0;
    }
    NV_MEMORY_ALLOCATION_PARAMS mp;
    memset(&mp, 0, sizeof mp);
    mp.owner = crm_root(rm->c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.flags = NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE;
    mp.size = s->size;
    uint32_t cls;
    if (!strcmp(kind, "vidmem")) {
        cls = NV01_MEMORY_LOCAL_USER;
        mp.attr = (NVOS32_ATTR_LOCATION_VIDMEM << NVOS32_ATTR_LOCATION_SHIFT) |
                  (NVOS32_ATTR_PAGE_SIZE_BIG << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
                  (NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
        mp.attr2 = NVOS32_ATTR2_ZBC_PREFER_NO_ZBC |
                   (NVOS32_ATTR2_GPU_CACHEABLE_YES << NVOS32_ATTR2_GPU_CACHEABLE_SHIFT);
        mp.alignment = 64 * 1024;
    } else {
        cls = NV01_MEMORY_SYSTEM;
        bool wc = !strcmp(kind, "sysmem-wc");
        mp.attr = (NVOS32_ATTR_LOCATION_PCI << NVOS32_ATTR_LOCATION_SHIFT) |
                  ((wc ? NVOS32_ATTR_COHERENCY_WRITE_COMBINE : NVOS32_ATTR_COHERENCY_CACHED)
                   << NVOS32_ATTR_COHERENCY_SHIFT) |
                  (NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
        mp.attr2 = NVOS32_ATTR2_ZBC_PREFER_NO_ZBC;
        mp.alignment = 4096;
    }
    uint32_t attr_in = mp.attr;
    r = crm_alloc(rm->c, rm->dev, &s->mem, cls, &mp, sizeof mp);
    printf("  attr in 0x%08x out 0x%08x (coherency %u -> %u)\n", attr_in, mp.attr, attr_in >> 29, mp.attr >> 29);
    report(r == 0, "  RM alloc class 0x%x 0x%" PRIx64 " B -> 0x%08x (%s)", cls, s->size, s->mem,
           crm_status_name(r));
    if (r)
        return r;
    void *p = NULL;
    r = crm_map_memory(rm->c, rm->dev, s->mem, 0, s->size, 0, &p);
    report(r == 0, "  RM CPU map (%s)", crm_status_name(r));
    if (r)
        return r;
    s->cpu = p;
    s->rm_mapped = true;
    return 0;
}

static int rm_export(struct rm *rm, const struct layout *l, struct surf *s)
{
    int ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    if (ctl < 0)
        return -errno;
    NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS ex;
    memset(&ex, 0, sizeof ex);
    ex.object.type = NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TYPE_RM;
    ex.object.data.rmObject.hDevice = rm->dev;
    ex.object.data.rmObject.hParent = rm->dev;
    ex.object.data.rmObject.hObject = s->mem;
    ex.fd = ctl;
    int r = crm_control(rm->c, crm_root(rm->c), NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD, &ex,
                        sizeof ex);
    report(r == 0, "  NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD (%s)", crm_status_name(r));
    if (r) {
        close(ctl);
        return r;
    }
    struct nvkms_kapi_priv_import_memory_params nv;
    memset(&nv, 0, sizeof nv);
    nv.memFd = ctl;
    nv.layout = l->block_linear ? 0 : 1;
    nv.log2_gobs_y = l->block_linear ? l->log2_gob_h : 0;
    nv.generic_memory = l->block_linear;
    struct drm_nvidia_gem_import_nvkms_memory_params imp;
    memset(&imp, 0, sizeof imp);
    imp.mem_size = s->size;
    imp.nvkms_params_ptr = (uint64_t)(uintptr_t)&nv;
    imp.nvkms_params_size = sizeof nv;
    r = drmIoctl(rm->drm_fd, DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY, &imp) ? -errno : 0;
    close(ctl);
    report(r == 0 && imp.handle, "  DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY -> GEM %u (%s)", imp.handle,
           r ? strerror(-r) : "ok");
    if (r)
        return r;
    s->gem = imp.handle;
    r = drmPrimeHandleToFD(rm->drm_fd, s->gem, DRM_CLOEXEC | DRM_RDWR, &s->dmabuf_fd) ? -errno : 0;
    report(r == 0, "  PRIME handle -> dma-buf (%s)", r ? strerror(-r) : "ok");
    return r;
}

static void rm_free(struct rm *rm, struct surf *s)
{
    if (s->dmabuf_fd >= 0)
        close(s->dmabuf_fd);
    if (s->gem) {
        struct drm_gem_close gc = { .handle = s->gem };
        drmIoctl(rm->drm_fd, DRM_IOCTL_GEM_CLOSE, &gc);
    }
    if (s->rm_mapped)
        crm_unmap_memory(rm->c, rm->dev, s->mem, (void *)s->cpu, s->size, 0);
    if (s->mem)
        crm_free(rm->c, rm->dev, s->mem);
    if (s->own)
        crm_free_pages(rm->c, s->own, s->size);
    memset(s, 0, sizeof *s);
}

/* ---- the compositor's side: EGL import and a GPU draw sampling it --------- */

struct gl {
    struct gbm_device *gbm;
    EGLDisplay dpy;
    EGLContext ctx;
    GLuint prog, rt, fb;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC tex_image;
};

static int gl_init(struct gl *g, int drm_fd)
{
    g->gbm = gbm_create_device(drm_fd);
    PFNEGLGETPLATFORMDISPLAYEXTPROC gpd = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    g->dpy = gpd(EGL_PLATFORM_GBM_KHR, g->gbm, NULL);
    if (!eglInitialize(g->dpy, NULL, NULL))
        return -1;
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint ca[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    g->ctx = eglCreateContext(g->dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ca);
    if (!g->ctx || !eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx))
        return -1;
    g->tex_image = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    const char *vs = "attribute vec2 p; varying vec2 uv;"
                     "void main(){ uv = p * 0.5 + 0.5; gl_Position = vec4(p, 0., 1.); }";
    const char *fs = "#extension GL_OES_EGL_image_external : require\n"
                     "precision highp float; varying vec2 uv; uniform samplerExternalOES s;"
                     "void main(){ gl_FragColor = texture2D(s, uv); }";
    GLuint sv = glCreateShader(GL_VERTEX_SHADER), sf = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(sv, 1, &vs, NULL);
    glCompileShader(sv);
    glShaderSource(sf, 1, &fs, NULL);
    glCompileShader(sf);
    g->prog = glCreateProgram();
    glAttachShader(g->prog, sv);
    glAttachShader(g->prog, sf);
    glBindAttribLocation(g->prog, 0, "p");
    glLinkProgram(g->prog);
    glGenTextures(1, &g->rt);
    glBindTexture(GL_TEXTURE_2D, g->rt);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, W, H, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &g->fb);
    glBindFramebuffer(GL_FRAMEBUFFER, g->fb);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, g->rt, 0);
    return glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE ? 0 : -1;
}

static void gl_check(struct gl *g, const struct layout *l, int dmabuf)
{
    EGLAttrib a[] = { EGL_WIDTH, W, EGL_HEIGHT, H, EGL_LINUX_DRM_FOURCC_EXT, DRM_FORMAT_XRGB8888,
                      EGL_DMA_BUF_PLANE0_FD_EXT, dmabuf, EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
                      EGL_DMA_BUF_PLANE0_PITCH_EXT, l->pitch,
                      EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (EGLAttrib)(l->modifier & 0xffffffff),
                      EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLAttrib)(l->modifier >> 32),
                      EGL_NONE };
    EGLImage img = eglCreateImage(g->dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, a);
    report(img != EGL_NO_IMAGE, "  eglCreateImage(LINUX_DMA_BUF, modifier 0x%016" PRIx64 ") (0x%x)",
           l->modifier, eglGetError());
    if (img == EGL_NO_IMAGE)
        return;
    GLuint t;
    glGenTextures(1, &t);
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, t);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    g->tex_image(GL_TEXTURE_EXTERNAL_OES, img);
    glBindFramebuffer(GL_FRAMEBUFFER, g->fb);
    glUseProgram(g->prog);
    static const float q[] = { -1, -1, 1, -1, -1, 1, 1, 1 };
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, q);
    glEnableVertexAttribArray(0);
    glViewport(0, 0, W, H);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    /* The whole picture back, compared pixel by pixel (24 bits: the pattern
     * puts y's low byte in red, x in green/blue). */
    uint8_t *px = malloc((size_t)W * H * 4);
    glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
    uint64_t bad = 0;
    for (uint32_t y = 0; y < H; y++)
        for (uint32_t x = 0; x < W; x++) {
            const uint8_t *p = px + ((size_t)y * W + x) * 4;
            uint32_t v = (uint32_t)p[0] << 16 | (uint32_t)p[1] << 8 | p[2];
            if (v != (pat(x, y) & 0xffffff))
                bad++;
        }
    free(px);
    report(bad == 0 && glGetError() == GL_NO_ERROR,
           "  GPU draw sampling it: %" PRIu64 " of %u pixels differ", bad, W * H);
    glDeleteTextures(1, &t);
    eglDestroyImage(g->dpy, img);
}

/* udmabuf: ordinary memfd pages (what QEMU's guest RAM is with a memfd
 * backend) made a dma-buf by /dev/udmabuf, no RM at all, handed to the
 * compositor as is: the alternative route for a primary in guest RAM. */
struct udmabuf_create {
    uint32_t memfd, flags;
    uint64_t offset, size;
};
#define UDMABUF_CREATE _IOW('u', 0x42, struct udmabuf_create)

static void run_udmabuf(struct rm *rm, struct gl *g, const char *lay)
{
    struct layout l;
    layout_init(&l, lay);
    printf("== udmabuf, %s\n", lay);
    uint64_t size = (l.size + 4095) & ~4095ull;
    int mfd = memfd_create("primary", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (mfd < 0 || ftruncate(mfd, (off_t)size) || fcntl(mfd, F_ADD_SEALS, F_SEAL_SHRINK)) {
        report(false, "  memfd: %s", strerror(errno));
        return;
    }
    volatile uint8_t *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    for (uint32_t y = 0; y < H; y++)
        for (uint32_t x = 0; x < W; x++)
            *(volatile uint32_t *)(p + px_off(&l, x, y)) = pat(x, y);
    int dev = open("/dev/udmabuf", O_RDWR | O_CLOEXEC);
    struct udmabuf_create c = { .memfd = (uint32_t)mfd, .flags = 1 /* CLOEXEC */, .size = size };
    int buf = dev >= 0 ? ioctl(dev, UDMABUF_CREATE, &c) : -1;
    report(buf >= 0, "  UDMABUF_CREATE over the memfd (%s)", buf >= 0 ? "ok" : strerror(errno));
    if (buf >= 0) {
        uint32_t gem = 0;
        int r = drmPrimeFDToHandle(rm->drm_fd, buf, &gem) ? -errno : 0;
        printf("  (nvidia-drm PRIME_FD_TO_HANDLE of it: %s)\n", r ? strerror(-r) : "ok");
        if (!r) {
            struct drm_gem_close gc = { .handle = gem };
            drmIoctl(rm->drm_fd, DRM_IOCTL_GEM_CLOSE, &gc);
        }
        gl_check(g, &l, buf);
        close(buf);
    }
    if (dev >= 0)
        close(dev);
    munmap((void *)p, size);
    close(mfd);
}

static void run(struct rm *rm, struct gl *g, const char *kind, const char *lay)
{
    struct layout l;
    layout_init(&l, lay);
    if (!strcmp(kind, "udmabuf")) {
        run_udmabuf(rm, g, lay);
        return;
    }
    printf("== %s, %s\n", kind, lay);
    struct surf s;
    if (rm_alloc(rm, kind, l.size, &s) == 0) {
        for (uint64_t o = 0; o < s.size; o += 4)
            *(volatile uint32_t *)(s.cpu + o) = 0xdeadbeefu;
        for (uint32_t y = 0; y < H && !via_dmabuf; y++)
            for (uint32_t x = 0; x < W; x++)
                *(volatile uint32_t *)(s.cpu + px_off(&l, x, y)) = pat(x, y);
        printf("  CPU read through %s: %.0f MB/s\n", s.rm_mapped ? "the RM mapping" : "our pages",
               read_mbps(s.cpu, s.size));
        /* What nvidia-drm's GEM init asks of a sysmem object through NVKMS
         * (GetMemoryPages, nvkms-kapi.c): NV003E_CTRL_CMD_GET_SURFACE_NUM_PHYS_PAGES.
         * RM serves it only for SystemMemory (class 0x3e). */
        uint32_t np = 0;
        int cr = crm_control(rm->c, s.mem, 0x3e0102 /* GET_SURFACE_NUM_PHYS_PAGES */, &np,
                             sizeof np);
        printf("  NV003E_CTRL_CMD_GET_SURFACE_NUM_PHYS_PAGES on it: %s, %u pages\n",
               crm_status_name(cr), np);
        /* What the backend asks to tell sysmem from vidmem and its CPU
         * caching: NV0041_CTRL_CMD_GET_SURFACE_INFO, PHYS_ATTR (coherency in
         * 31:29) and ADDR_SPACE_TYPE (1 sysmem, 2 vidmem). */
        struct { uint32_t index, data; } si[2] = { { 8, 0 }, { 9, 0 } };
        struct { uint32_t n, pad; uint64_t list; } sp = { 2, 0, (uint64_t)(uintptr_t)si };
        cr = crm_control(rm->c, s.mem, 0x410110, &sp, sizeof sp);
        printf("  NV0041_CTRL_CMD_GET_SURFACE_INFO: %s, phys_attr 0x%08x (coherency %u), "
               "addr space %u\n",
               crm_status_name(cr), si[0].data, si[0].data >> 29, si[1].data);
        /* The CPU caching RM gives a mapping of it: NV_ESC_RM_MAP_MEMORY of
         * one page on a fresh control-file descriptor (never mmapped), the
         * caching type read from the returned flags (23:25), then unmapped. */
        {
            int pfd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
            nv_ioctl_nvos33_parameters_with_fd mp;
            memset(&mp, 0, sizeof mp);
            mp.params.hClient = crm_root(rm->c);
            mp.params.hDevice = rm->dev;
            mp.params.hMemory = s.mem;
            mp.params.length = 4096;
            mp.fd = pfd;
            int er = crm_escape(rm->c, crm_ctl_fd(rm->c), NV_ESC_RM_MAP_MEMORY, &mp, sizeof mp);
            printf("  RM_MAP_MEMORY probe: escape %d, status %s, caching type %u\n", er,
                   crm_status_name((int)mp.params.status), (mp.params.flags >> 23) & 7);
            if (er == 0 && mp.params.status == 0) {
                NVOS34_PARAMETERS up;
                memset(&up, 0, sizeof up);
                up.hClient = mp.params.hClient;
                up.hDevice = rm->dev;
                up.hMemory = s.mem;
                up.pLinearAddress = mp.params.pLinearAddress;
                er = crm_escape(rm->c, crm_ctl_fd(rm->c), 0x4F, &up, sizeof up);
                printf("  RM_UNMAP_MEMORY: escape %d, status %s\n", er,
                       crm_status_name((int)up.status));
            }
            close(pfd);
        }
        if (rm_export(rm, &l, &s) == 0) {
            void *m = mmap(NULL, s.size, PROT_READ, MAP_SHARED, s.dmabuf_fd, 0);
            if (m != MAP_FAILED) {
                printf("  CPU read through the dma-buf mmap: %.0f MB/s (first touch)", read_mbps(m, s.size));
                printf(", %.0f MB/s (mapped)\n", read_mbps(m, s.size));
                munmap(m, s.size);
            } else {
                printf("  dma-buf mmap: %s\n", strerror(errno));
            }
            /* "dmabuf": the pattern is written through a shared, writable
             * mmap of the dma-buf -- what QEMU places in region 3 for an
             * RM-export blob the guest maps (RESOURCE_MAP_BLOB) -- and the
             * GPU samples it with no flush of any kind in between. */
            void *wm = MAP_FAILED;
            if (via_dmabuf) {
                wm = mmap(NULL, s.size, PROT_READ | PROT_WRITE, MAP_SHARED, s.dmabuf_fd, 0);
                report(wm != MAP_FAILED, "  writable dma-buf mmap (%s)",
                       wm != MAP_FAILED ? "ok" : strerror(errno));
                if (wm != MAP_FAILED) {
                    volatile uint8_t *d = wm;
                    double t0 = now();
                    for (uint32_t y = 0; y < H; y++)
                        for (uint32_t x = 0; x < W; x++)
                            *(volatile uint32_t *)(d + px_off(&l, x, y)) = pat(x, y);
                    printf("  pattern written through it: %.0f MB/s, then read: %.0f MB/s\n",
                           (double)W * H * 4 / (now() - t0) / 1e6, read_mbps(d, s.size));
                }
            }
            gl_check(g, &l, s.dmabuf_fd);
            if (wm != MAP_FAILED)
                munmap(wm, s.size);
        }
    }
    rm_free(rm, &s);
}

int main(int argc, char **argv)
{
    const char *kinds[] = { "vidmem", "sysmem", "sysmem-wc", "osdesc", "udmabuf" };
    const char *lays[] = { "linear", "bl5" };
    const char *want_kind = argc > 1 ? argv[1] : "all";
    const char *want_lay = argc > 2 ? argv[2] : "all";
    via_dmabuf = argc > 3 && !strcmp(argv[3], "dmabuf");

    struct rm rm = { .drm_fd = -1 };
    int r = crm_open(&rm.c, NULL);
    report(r == 0, "crm_open (%s)", crm_status_name(r));
    if (r)
        return 1;
    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof dp);
    r = crm_alloc(rm.c, crm_root(rm.c), &rm.dev, NV01_DEVICE_0, &dp, sizeof dp);
    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    if (!r)
        r = crm_alloc(rm.c, rm.dev, &rm.sub, NV20_SUBDEVICE_0, &sp, sizeof sp);
    report(r == 0, "RM device + subdevice (%s)", crm_status_name(r));
    if (r)
        return 1;
    /* The NVIDIA render node (the backend's, the compositor's GPU). */
    for (int i = 128; i < 136 && rm.drm_fd < 0; i++) {
        char path[32];
        snprintf(path, sizeof path, "/dev/dri/renderD%d", i);
        int fd = open(path, O_RDWR | O_CLOEXEC);
        if (fd < 0)
            continue;
        drmVersionPtr v = drmGetVersion(fd);
        if (v && !strcmp(v->name, "nvidia-drm"))
            rm.drm_fd = fd;
        else
            close(fd);
        drmFreeVersion(v);
    }
    report(rm.drm_fd >= 0, "nvidia-drm render node");
    if (rm.drm_fd < 0)
        return 1;
    struct gl g;
    memset(&g, 0, sizeof g);
    report(gl_init(&g, rm.drm_fd) == 0, "EGL/GLES on it");

    for (unsigned k = 0; k < 5; k++)
        for (unsigned j = 0; j < 2; j++)
            if ((!strcmp(want_kind, "all") || !strcmp(want_kind, kinds[k])) &&
                (!strcmp(want_lay, "all") || !strcmp(want_lay, lays[j])))
                run(&rm, &g, kinds[k], lays[j]);

    close(rm.drm_fd);
    crm_free(rm.c, rm.dev, rm.sub);
    crm_free(rm.c, crm_root(rm.c), rm.dev);
    crm_close(rm.c);
    printf("%d failure(s)\n", failures);
    return failures ? 1 : 0;
}
