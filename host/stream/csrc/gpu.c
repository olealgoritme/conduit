/* SPDX-License-Identifier: Apache-2.0 */
/*
 * gpu.c -- see gpu.h.
 *
 *   dma-buf ──EGLImage──► GL texture ──one draw──► plane textures (R8 / RG8)
 *            (cached per buffer)        convert,    │ registered with CUDA once
 *                                       scale,      ▼ map + cuMemcpy2D (device→device)
 *                                       cursor      NVENC input surface ──► bitstream
 *
 * EGL runs on the device platform (EGL_EXT_platform_device): no window, no
 * compositor, no GBM needed on the encode side. CUDA, NVENC and NVDEC are
 * loaded from the driver at run time (nv-codec-headers' loader), so building
 * needs no CUDA toolkit and the binary starts (and explains) without them.
 */
#define _GNU_SOURCE
#include "gpu_internal.h"

#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

void cs_err(char *err, size_t errlen, const char *fmt, ...)
{
    va_list ap;

    if (!err || !errlen)
        return;
    va_start(ap, fmt);
    vsnprintf(err, errlen, fmt, ap);
    va_end(ap);
}

uint64_t cs_now_us(void)
{
    struct timespec ts;

    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000ull + (uint64_t)ts.tv_nsec / 1000;
}

const char *cs_cu_err(cs_gpu *g, CUresult r)
{
    const char *s = NULL;

    if (g && g->cu && g->cu->cuGetErrorName)
        g->cu->cuGetErrorName(r, &s);
    return s ? s : "CUDA error";
}

/* ------------------------------------------------------------------ GL setup */

static int load_gl(cs_gpu *g)
{
#define GLF(name) \
    if (!(g->gl.name = (void *)eglGetProcAddress(#name))) return -1;
    GL_FUNCS(GLF)
#undef GLF
    g->gl.EGLImageTargetTexture2DOES =
        (cs_image_target_fn)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!g->gl.EGLImageTargetTexture2DOES)
        return -1;
    return 0;
}

static const char *VS =
    "#version 330 core\n"
    "void main() {\n"
    "  vec2 p = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);\n"
    "  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n"
    "}\n";

/*
 * One shader for every output layout. Coordinates are in MEMORY rows: row 0 is
 * the first row of every buffer (the top of the picture), for the source
 * dma-buf, the plane textures and the CUDA copy alike, so nothing is flipped.
 */
static const char *FS_ENC =
    "#version 330 core\n"
    "uniform sampler2D u_src;\n"
    "uniform sampler2D u_cursor;\n"
    "uniform int u_mode;\n"        /* 0 Y, 1 NV12 UV, 2 YUV444, 3 G/B/R */
    "uniform int u_has_src;\n"
    "uniform int u_exact;\n"
    "uniform vec2 u_src_size;\n"
    "uniform vec4 u_rect;\n"       /* picture rect in output pixels */
    "uniform int u_cur_on;\n"
    "uniform vec4 u_cur;\n"        /* cursor rect in source pixels */
    "uniform vec3 u_ky, u_kcb, u_kcr, u_off;\n"
    "layout(location = 0) out vec4 o0;\n"
    "layout(location = 1) out vec4 o1;\n"
    "layout(location = 2) out vec4 o2;\n"
    "vec3 src_at(vec2 sp) {\n"
    "  vec3 c = u_exact != 0 ? texelFetch(u_src, ivec2(sp), 0).rgb\n"
    "                        : texture(u_src, sp / u_src_size).rgb;\n"
    "  if (u_cur_on != 0) {\n"
    "    vec2 q = sp - u_cur.xy;\n"
    "    if (q.x >= 0.0 && q.y >= 0.0 && q.x < u_cur.z && q.y < u_cur.w) {\n"
    "      vec4 k = texelFetch(u_cursor, ivec2(q), 0);\n"
    "      c = k.rgb + (1.0 - k.a) * c;\n"   /* premultiplied, as KMS cursor planes are */
    "    }\n"
    "  }\n"
    "  return c;\n"
    "}\n"
    "vec3 px(ivec2 d) {\n"
    "  vec2 p = vec2(d) + 0.5;\n"
    "  if (u_has_src == 0 || p.x < u_rect.x || p.y < u_rect.y ||\n"
    "      p.x >= u_rect.x + u_rect.z || p.y >= u_rect.y + u_rect.w) return vec3(0.0);\n"
    "  return src_at((p - u_rect.xy) * u_src_size / u_rect.zw);\n"
    "}\n"
    "void main() {\n"
    "  ivec2 d = ivec2(gl_FragCoord.xy);\n"
    "  if (u_mode == 1) {\n"
    "    ivec2 b = d * 2;\n"
    "    vec3 c = (px(b) + px(b + ivec2(1, 0)) + px(b + ivec2(0, 1)) + px(b + ivec2(1, 1))) * 0.25;\n"
    "    o0 = vec4(dot(u_kcb, c) + u_off.y, dot(u_kcr, c) + u_off.z, 0.0, 1.0);\n"
    "    return;\n"
    "  }\n"
    "  vec3 c = px(d);\n"
    "  if (u_mode == 3) { o0 = vec4(c.g); o1 = vec4(c.b); o2 = vec4(c.r); return; }\n"
    "  o0 = vec4(dot(u_ky, c) + u_off.x);\n"
    "  if (u_mode == 2) { o1 = vec4(dot(u_kcb, c) + u_off.y); o2 = vec4(dot(u_kcr, c) + u_off.z); }\n"
    "}\n";

GLuint cs_gl_program(cs_gpu *g, const char *vs, const char *fs, char *err, size_t errlen)
{
    const char *src[2] = { vs, fs };
    GLenum kind[2] = { GL_VERTEX_SHADER, GL_FRAGMENT_SHADER };
    GLuint sh[2], prog;
    GLint ok;
    char log[1024];

    for (int i = 0; i < 2; i++) {
        sh[i] = g->gl.glCreateShader(kind[i]);
        g->gl.glShaderSource(sh[i], 1, &src[i], NULL);
        g->gl.glCompileShader(sh[i]);
        g->gl.glGetShaderiv(sh[i], GL_COMPILE_STATUS, &ok);
        if (!ok) {
            g->gl.glGetShaderInfoLog(sh[i], sizeof(log), NULL, log);
            cs_err(err, errlen, "shader: %s", log);
            return 0;
        }
    }
    prog = g->gl.glCreateProgram();
    g->gl.glAttachShader(prog, sh[0]);
    g->gl.glAttachShader(prog, sh[1]);
    g->gl.glLinkProgram(prog);
    g->gl.glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    g->gl.glDeleteShader(sh[0]);
    g->gl.glDeleteShader(sh[1]);
    if (!ok) {
        g->gl.glGetProgramInfoLog(prog, sizeof(log), NULL, log);
        cs_err(err, errlen, "program: %s", log);
        return 0;
    }
    return prog;
}

/* ------------------------------------------------------------------ open */

static EGLDeviceEXT pick_device(const char *render_node)
{
    PFNEGLQUERYDEVICESEXTPROC qd = (void *)eglGetProcAddress("eglQueryDevicesEXT");
    PFNEGLQUERYDEVICESTRINGEXTPROC qs = (void *)eglGetProcAddress("eglQueryDeviceStringEXT");
    EGLDeviceEXT devs[16];
    EGLint n = 0;

    if (!qd || !qs || !qd(16, devs, &n))
        return EGL_NO_DEVICE_EXT;
    for (EGLint i = 0; i < n; i++) {
        const char *ext = qs(devs[i], EGL_EXTENSIONS);
        const char *node = NULL;

        if (ext && strstr(ext, "EGL_EXT_device_drm_render_node"))
            node = qs(devs[i], EGL_DRM_RENDER_NODE_FILE_EXT);
        if (!ext || !strstr(ext, "EGL_EXT_device_drm"))
            continue; /* software devices (Mesa's) have no DRM node */
        if (render_node && (!node || strcmp(node, render_node)))
            continue;
        if (!render_node && ext && strstr(ext, "EGL_MESA"))
            continue;
        return devs[i];
    }
    return EGL_NO_DEVICE_EXT;
}

cs_gpu *cs_gpu_open(const char *render_node, char *err, size_t errlen)
{
    cs_gpu *g = calloc(1, sizeof(*g));
    EGLDeviceEXT dev;
    PFNEGLGETPLATFORMDISPLAYEXTPROC gpd;
    CUresult r;

    if (!g)
        return NULL;
    for (int i = 0; i < CS_IMG_SLOTS; i++)
        g->img[i].id = 0;
    g->cur_img = -1;

    dev = pick_device(render_node);
    if (dev == EGL_NO_DEVICE_EXT) {
        cs_err(err, errlen, "no NVIDIA EGL device%s%s (is the NVIDIA driver loaded?)",
               render_node ? " for " : "", render_node ? render_node : "");
        goto fail;
    }
    gpd = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    g->dpy = gpd ? gpd(EGL_PLATFORM_DEVICE_EXT, dev, NULL) : EGL_NO_DISPLAY;
    if (g->dpy == EGL_NO_DISPLAY || !eglInitialize(g->dpy, NULL, NULL)) {
        cs_err(err, errlen, "EGL: cannot initialize the device display (0x%x)", eglGetError());
        goto fail;
    }
    {
        const char *ext = eglQueryString(g->dpy, EGL_EXTENSIONS);
        if (!ext || !strstr(ext, "EGL_EXT_image_dma_buf_import")) {
            cs_err(err, errlen, "EGL: no dma-buf import on this device");
            goto fail;
        }
        g->has_modifiers = strstr(ext, "EGL_EXT_image_dma_buf_import_modifiers") != NULL;
    }
    if (!eglBindAPI(EGL_OPENGL_API)) {
        cs_err(err, errlen, "EGL: no desktop OpenGL");
        goto fail;
    }
    {
        const EGLint attr[] = {
            EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3,
            EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
            EGL_NONE,
        };
        g->ctx = eglCreateContext(g->dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, attr);
    }
    if (g->ctx == EGL_NO_CONTEXT ||
        !eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx)) {
        cs_err(err, errlen, "EGL: cannot create a GL 3.3 context (0x%x)", eglGetError());
        goto fail;
    }
    g->createImage = (void *)eglGetProcAddress("eglCreateImageKHR");
    g->destroyImage = (void *)eglGetProcAddress("eglDestroyImageKHR");
    g->queryModifiers = (void *)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
    if (!g->createImage || !g->destroyImage || load_gl(g)) {
        cs_err(err, errlen, "EGL/GL: missing entry points");
        goto fail;
    }
    g->gl.glGenVertexArrays(1, &g->vao);
    g->gl.glGenFramebuffers(1, &g->fbo);
    g->prog_enc = cs_gl_program(g, VS, FS_ENC, err, errlen);
    if (!g->prog_enc)
        goto fail;

    /* CUDA + codecs, from the driver. */
    if (cuda_load_functions(&g->cu, NULL) < 0) {
        cs_err(err, errlen, "cannot load libcuda.so.1 (NVIDIA driver)");
        goto fail;
    }
    if ((r = g->cu->cuInit(0)) != CUDA_SUCCESS) {
        cs_err(err, errlen, "cuInit: %s", cs_cu_err(g, r));
        goto fail;
    }
    {
        /* The CUDA device that drives this GL context. */
        unsigned int nd = 0;
        CUdevice devs[4];
        if (g->cu->cuGLGetDevices &&
            g->cu->cuGLGetDevices(&nd, devs, 4, CU_GL_DEVICE_LIST_ALL) == CUDA_SUCCESS && nd > 0)
            g->cudev = devs[0];
        else if ((r = g->cu->cuDeviceGet(&g->cudev, 0)) != CUDA_SUCCESS) {
            cs_err(err, errlen, "cuDeviceGet: %s", cs_cu_err(g, r));
            goto fail;
        }
    }
    if ((r = g->cu->cuCtxCreate(&g->cuctx, CU_CTX_SCHED_BLOCKING_SYNC, g->cudev)) != CUDA_SUCCESS) {
        cs_err(err, errlen, "cuCtxCreate: %s", cs_cu_err(g, r));
        goto fail;
    }
    {
        CUcontext dummy;
        g->cu->cuCtxPopCurrent(&dummy);
    }
    if (nvenc_load_functions(&g->nv, NULL) < 0)
        g->nv = NULL; /* encode unavailable; decode may still work */
    if (cuvid_load_functions(&g->cv, NULL) < 0)
        g->cv = NULL;
    if (g->nv) {
        uint32_t ver = 0;
        g->nv->NvEncodeAPIGetMaxSupportedVersion(&ver);
        g->nvenc_ver_ok = ver >= ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION);
        g->nvenc_driver_ver = ver;
        memset(&g->api, 0, sizeof(g->api));
        g->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        if (g->nvenc_ver_ok && g->nv->NvEncodeAPICreateInstance(&g->api) != NV_ENC_SUCCESS)
            g->nvenc_ver_ok = 0;
    }
    return g;
fail:
    cs_gpu_close(g);
    return NULL;
}

static void drop_img(cs_gpu *g, struct cs_img *im)
{
    if (im->tex)
        g->gl.glDeleteTextures(1, &im->tex);
    if (im->img != EGL_NO_IMAGE_KHR)
        g->destroyImage(g->dpy, im->img);
    memset(im, 0, sizeof(*im));
    im->img = EGL_NO_IMAGE_KHR;
}

void cs_gpu_close(cs_gpu *g)
{
    if (!g)
        return;
    if (g->ctx != EGL_NO_CONTEXT && g->ctx)
        eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx);
    if (g->createImage) {
        for (int i = 0; i < CS_IMG_SLOTS; i++)
            if (g->img[i].id)
                drop_img(g, &g->img[i]);
        if (g->cursor.id)
            drop_img(g, &g->cursor);
    }
    if (g->cuctx)
        g->cu->cuCtxDestroy(g->cuctx);
    if (g->cu)
        cuda_free_functions(&g->cu);
    if (g->nv)
        nvenc_free_functions(&g->nv);
    if (g->cv)
        cuvid_free_functions(&g->cv);
    if (g->dpy) {
        eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        if (g->ctx)
            eglDestroyContext(g->dpy, g->ctx);
        eglTerminate(g->dpy);
    }
    free(g);
}

int cs_gpu_format_ok(cs_gpu *g, uint32_t fourcc, uint64_t modifier)
{
    EGLuint64KHR mods[64];
    EGLBoolean ext[64];
    EGLint n = 0;

    if (fourcc != CS_FOURCC_XR24 && fourcc != CS_FOURCC_AR24 &&
        fourcc != CS_FOURCC_XB24 && fourcc != CS_FOURCC_AB24)
        return 0;
    if (modifier == CS_MOD_INVALID)
        return 1;
    if (!g->has_modifiers || !g->queryModifiers)
        return modifier == 0;
    if (!g->queryModifiers(g->dpy, (EGLint)fourcc, 64, mods, ext, &n))
        return 0;
    for (EGLint i = 0; i < n; i++)
        if (mods[i] == modifier)
            return !ext[i];
    return 0;
}

size_t cs_gpu_modifiers(cs_gpu *g, uint32_t fourcc, uint64_t *out, size_t max)
{
    EGLuint64KHR mods[128];
    EGLBoolean ext[128];
    EGLint n = 0;
    size_t k = 0;

    if (!g->has_modifiers || !g->queryModifiers ||
        !g->queryModifiers(g->dpy, (EGLint)fourcc, 128, mods, ext, &n))
        return 0;
    for (EGLint i = 0; i < n && k < max; i++)
        if (!ext[i])
            out[k++] = mods[i];
    return k;
}

/* ------------------------------------------------------------------ import */

EGLImageKHR cs_import(cs_gpu *g, int fd, uint32_t w, uint32_t h, uint32_t stride,
                      uint32_t offset, uint32_t fourcc, uint64_t modifier)
{
    EGLint a[32];
    int i = 0;

    a[i++] = EGL_WIDTH; a[i++] = (EGLint)w;
    a[i++] = EGL_HEIGHT; a[i++] = (EGLint)h;
    a[i++] = EGL_LINUX_DRM_FOURCC_EXT; a[i++] = (EGLint)fourcc;
    a[i++] = EGL_DMA_BUF_PLANE0_FD_EXT; a[i++] = fd;
    a[i++] = EGL_DMA_BUF_PLANE0_OFFSET_EXT; a[i++] = (EGLint)offset;
    a[i++] = EGL_DMA_BUF_PLANE0_PITCH_EXT; a[i++] = (EGLint)stride;
    if (modifier != CS_MOD_INVALID && g->has_modifiers) {
        a[i++] = EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT; a[i++] = (EGLint)(modifier & 0xffffffffu);
        a[i++] = EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT; a[i++] = (EGLint)(modifier >> 32);
    }
    a[i++] = EGL_NONE;
    return g->createImage(g->dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, a);
}

static int import_into(cs_gpu *g, struct cs_img *im, int fd, uint64_t id, uint32_t w,
                       uint32_t h, uint32_t stride, uint32_t offset, uint32_t fourcc,
                       uint64_t modifier, char *err, size_t errlen)
{
    EGLImageKHR img;
    GLenum e;

    if (w == 0 || h == 0 || w > 16384 || h > 16384) {
        cs_err(err, errlen, "bad buffer size %ux%u", w, h);
        return -1;
    }
    img = cs_import(g, fd, w, h, stride, offset, fourcc, modifier);
    if (img == EGL_NO_IMAGE_KHR) {
        cs_err(err, errlen, "EGL refused the dma-buf (%ux%u fourcc %.4s modifier 0x%llx): 0x%x",
               w, h, (const char *)&fourcc, (unsigned long long)modifier, eglGetError());
        return -1;
    }
    if (im->id)
        drop_img(g, im);
    im->img = img;
    im->id = id;
    im->w = w; im->h = h; im->stride = stride; im->offset = offset;
    im->fourcc = fourcc; im->modifier = modifier;
    g->gl.glGenTextures(1, &im->tex);
    g->gl.glBindTexture(GL_TEXTURE_2D, im->tex);
    g->gl.EGLImageTargetTexture2DOES(GL_TEXTURE_2D, img);
    g->gl.glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    g->gl.glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    g->gl.glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    g->gl.glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    if ((e = g->gl.glGetError()) != GL_NO_ERROR) {
        cs_err(err, errlen, "GL could not use the imported buffer (0x%x)", e);
        drop_img(g, im);
        return -1;
    }
    return 0;
}

int cs_gpu_set_frame(cs_gpu *g, int fd, uint64_t id, uint32_t w, uint32_t h,
                     uint32_t stride, uint32_t offset, uint32_t fourcc,
                     uint64_t modifier, char *err, size_t errlen)
{
    int free_slot = -1, lru = 0;

    g->tick++;
    for (int i = 0; i < CS_IMG_SLOTS; i++) {
        struct cs_img *im = &g->img[i];

        if (im->id == id && id) {
            if (im->w == w && im->h == h && im->stride == stride && im->offset == offset &&
                im->fourcc == fourcc && im->modifier == modifier) {
                im->used = g->tick;
                g->cur_img = i;
                return 0;
            }
            /* same buffer id, new layout: re-import in place */
            if (import_into(g, im, fd, id, w, h, stride, offset, fourcc, modifier, err, errlen))
                return -1;
            im->used = g->tick;
            g->cur_img = i;
            return 0;
        }
        if (!im->id && free_slot < 0)
            free_slot = i;
        if (g->img[i].used < g->img[lru].used)
            lru = i;
    }
    if (free_slot < 0)
        free_slot = lru;
    if (import_into(g, &g->img[free_slot], fd, id, w, h, stride, offset, fourcc, modifier,
                    err, errlen))
        return -1;
    g->img[free_slot].used = g->tick;
    g->cur_img = free_slot;
    return 0;
}

void cs_gpu_clear_frame(cs_gpu *g)
{
    g->cur_img = -1;
}

void cs_gpu_frame_size(cs_gpu *g, uint32_t *w, uint32_t *h)
{
    if (g->cur_img < 0) {
        *w = *h = 0;
        return;
    }
    *w = g->img[g->cur_img].w;
    *h = g->img[g->cur_img].h;
}

int cs_gpu_set_cursor(cs_gpu *g, int fd, uint64_t id, uint32_t w, uint32_t h,
                      uint32_t stride, uint32_t offset, uint32_t fourcc,
                      uint64_t modifier, uint32_t hot_x, uint32_t hot_y,
                      char *err, size_t errlen)
{
    if (fd < 0) {
        g->cursor_visible = 0;
        return 0;
    }
    if (!(g->cursor.id == id && id && g->cursor.w == w && g->cursor.h == h &&
          g->cursor.stride == stride && g->cursor.modifier == modifier)) {
        /* The guest may redraw the same cursor buffer: always re-import (it is
         * small and changes rarely) so new content is never missed. */
        if (import_into(g, &g->cursor, fd, id, w, h, stride, offset, fourcc, modifier, err,
                        errlen))
            return -1;
        g->gl.glBindTexture(GL_TEXTURE_2D, g->cursor.tex);
        g->gl.glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        g->gl.glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    }
    g->cursor_hot_x = hot_x;
    g->cursor_hot_y = hot_y;
    g->cursor_visible = 1;
    return 0;
}

/* ------------------------------------------------------------------ colour */

void cs_csc(uint32_t colorspace, uint32_t full_range, float ky[3], float kcb[3], float kcr[3],
            float off[3])
{
    double kr, kb, kg, ys, cs;

    switch (colorspace) {
    case 1: kr = 0.2126; kb = 0.0722; break;
    case 2: kr = 0.2627; kb = 0.0593; break;
    default: kr = 0.299; kb = 0.114; break;
    }
    kg = 1.0 - kr - kb;
    ys = full_range ? 1.0 : 219.0 / 255.0;
    cs = full_range ? 1.0 : 224.0 / 255.0;
    ky[0] = (float)(kr * ys); ky[1] = (float)(kg * ys); ky[2] = (float)(kb * ys);
    kcb[0] = (float)(-kr / (2 * (1 - kb)) * cs);
    kcb[1] = (float)(-kg / (2 * (1 - kb)) * cs);
    kcb[2] = (float)(0.5 * cs);
    kcr[0] = (float)(0.5 * cs);
    kcr[1] = (float)(-kg / (2 * (1 - kr)) * cs);
    kcr[2] = (float)(-kb / (2 * (1 - kr)) * cs);
    off[0] = full_range ? 0.0f : (float)(16.0 / 255.0);
    off[1] = off[2] = (float)(128.0 / 255.0);
}

/* Draw the current picture into `n` attachments of size w x h, mode as in FS_ENC. */
void cs_convert(cs_gpu *g, const GLuint *tex, int n, uint32_t w, uint32_t h, int mode,
                uint32_t out_w, uint32_t out_h, uint32_t colorspace, uint32_t full_range,
                const struct cs_frame_opts *o)
{
    struct gl_fns *gl = &g->gl;
    GLuint p = g->prog_enc;
    GLenum bufs[3] = { GL_COLOR_ATTACHMENT0, GL_COLOR_ATTACHMENT1, GL_COLOR_ATTACHMENT2 };
    float ky[3], kcb[3], kcr[3], off[3];
    struct cs_img *src = g->cur_img >= 0 ? &g->img[g->cur_img] : NULL;

    gl->glBindFramebuffer(GL_FRAMEBUFFER, g->fbo);
    for (int i = 0; i < 3; i++)
        gl->glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0 + i, GL_TEXTURE_2D,
                                   i < n ? tex[i] : 0, 0);
    gl->glDrawBuffers(n, bufs);
    gl->glViewport(0, 0, (GLsizei)w, (GLsizei)h);
    gl->glUseProgram(p);
    gl->glBindVertexArray(g->vao);
    cs_csc(colorspace, full_range, ky, kcb, kcr, off);
    gl->glUniform1i(gl->glGetUniformLocation(p, "u_mode"), mode);
    gl->glUniform3f(gl->glGetUniformLocation(p, "u_ky"), ky[0], ky[1], ky[2]);
    gl->glUniform3f(gl->glGetUniformLocation(p, "u_kcb"), kcb[0], kcb[1], kcb[2]);
    gl->glUniform3f(gl->glGetUniformLocation(p, "u_kcr"), kcr[0], kcr[1], kcr[2]);
    gl->glUniform3f(gl->glGetUniformLocation(p, "u_off"), off[0], off[1], off[2]);
    gl->glUniform1i(gl->glGetUniformLocation(p, "u_src"), 0);
    gl->glUniform1i(gl->glGetUniformLocation(p, "u_cursor"), 1);
    gl->glUniform1i(gl->glGetUniformLocation(p, "u_has_src"), src != NULL);
    if (src) {
        /* Fit the picture into out_w x out_h, aspect kept, centred. */
        double sx = (double)out_w / src->w, sy = (double)out_h / src->h;
        double s = sx < sy ? sx : sy;
        double rw = src->w * s, rh = src->h * s;
        double rx = (out_w - rw) / 2, ry = (out_h - rh) / 2;
        int exact = src->w == out_w && src->h == out_h;

        if (exact) { rx = ry = 0; rw = out_w; rh = out_h; }
        gl->glActiveTexture(GL_TEXTURE0);
        gl->glBindTexture(GL_TEXTURE_2D, src->tex);
        gl->glUniform1i(gl->glGetUniformLocation(p, "u_exact"), exact);
        gl->glUniform2f(gl->glGetUniformLocation(p, "u_src_size"), (float)src->w, (float)src->h);
        gl->glUniform4f(gl->glGetUniformLocation(p, "u_rect"), (float)rx, (float)ry, (float)rw,
                        (float)rh);
        int cur = o && o->cursor_on && g->cursor_visible && g->cursor.id;
        gl->glUniform1i(gl->glGetUniformLocation(p, "u_cur_on"), cur);
        if (cur) {
            gl->glActiveTexture(GL_TEXTURE1);
            gl->glBindTexture(GL_TEXTURE_2D, g->cursor.tex);
            gl->glUniform4f(gl->glGetUniformLocation(p, "u_cur"),
                            (float)o->cursor_x - (float)g->cursor_hot_x,
                            (float)o->cursor_y - (float)g->cursor_hot_y,
                            (float)g->cursor.w, (float)g->cursor.h);
            gl->glActiveTexture(GL_TEXTURE0);
        }
    }
    gl->glDrawArrays(GL_TRIANGLES, 0, 3);
    gl->glBindFramebuffer(GL_FRAMEBUFFER, 0);
}

GLuint cs_plane_tex(cs_gpu *g, GLenum ifmt, GLenum fmt, uint32_t w, uint32_t h)
{
    GLuint t;

    g->gl.glGenTextures(1, &t);
    g->gl.glBindTexture(GL_TEXTURE_2D, t);
    g->gl.glTexImage2D(GL_TEXTURE_2D, 0, (GLint)ifmt, (GLsizei)w, (GLsizei)h, 0, fmt,
                       GL_UNSIGNED_BYTE, NULL);
    g->gl.glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    g->gl.glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    return t;
}
