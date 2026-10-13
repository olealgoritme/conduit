/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_gpuscale.c -- see nb_gpuscale.h.
 *
 * EGL on GBM, a surfaceless GLES2 context, guest dma-bufs imported as
 * EGLImages (GL_TEXTURE_EXTERNAL_OES, which takes any modifier the driver
 * imports), output buffers allocated by GBM for scanout and rendered through
 * an FBO.  One clear and one textured quad per frame, then an EGL fence the
 * caller waits on: no compositor here does explicit sync with us, so the
 * buffer must be complete before it is committed.
 *
 * Every library is dlopen'ed: a viewer that never goes fullscreen at a
 * non-native size never loads a GL driver.
 */
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>

#include "nb_gpuscale.h"
#include "nb_gpusel.h"

/* ── the slice of EGL / GLES2 / GBM used here, spelled locally ────────── */

typedef void *EGLDisplay, *EGLContext, *EGLConfig, *EGLSurface, *EGLImage,
             *EGLSync, *EGLClientBuffer;
typedef int32_t EGLint;
typedef unsigned EGLBoolean, EGLenum;
typedef intptr_t EGLAttrib;
typedef uint64_t EGLTime;

#define EGL_NONE                        0x3038
#define EGL_TRUE                        1
#define EGL_EXTENSIONS                  0x3055
#define EGL_OPENGL_ES_API               0x30A0
#define EGL_CONTEXT_CLIENT_VERSION      0x3098
#define EGL_CONTEXT_PRIORITY_LEVEL_IMG  0x3100
#define EGL_CONTEXT_PRIORITY_HIGH_IMG   0x3101
#define EGL_CONTEXT_PRIORITY_MEDIUM_IMG 0x3102
#define EGL_CONTEXT_PRIORITY_LOW_IMG    0x3103
#define EGL_CONTEXT_PRIORITY_REALTIME_NV 0x3357
#define EGL_PLATFORM_GBM_KHR            0x31D7
#define EGL_LINUX_DMA_BUF_EXT           0x3270
#define EGL_WIDTH                       0x3057
#define EGL_HEIGHT                      0x3056
#define EGL_LINUX_DRM_FOURCC_EXT        0x3271
#define EGL_DMA_BUF_PLANE0_FD_EXT       0x3272
#define EGL_DMA_BUF_PLANE0_OFFSET_EXT   0x3273
#define EGL_DMA_BUF_PLANE0_PITCH_EXT    0x3274
#define EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT 0x3443
#define EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT 0x3444
#define EGL_IMAGE_PRESERVED_KHR         0x30D2
#define EGL_SYNC_FENCE_KHR              0x30F9
#define EGL_SYNC_FLUSH_COMMANDS_BIT_KHR 0x0001
#define EGL_TIMEOUT_EXPIRED_KHR         0x30F5
#define EGL_CONDITION_SATISFIED_KHR     0x30F6
#define EGL_NO_CONTEXT                  ((EGLContext)0)
#define EGL_NO_SURFACE                  ((EGLSurface)0)
#define EGL_NO_CONFIG_KHR               ((EGLConfig)0)
#define EGL_NO_IMAGE                    ((EGLImage)0)
#define EGL_NO_SYNC                     ((EGLSync)0)

typedef unsigned GLenum, GLuint, GLbitfield;
typedef int GLint, GLsizei;
typedef float GLfloat;
typedef char GLchar;
typedef unsigned char GLboolean;

#define GL_TEXTURE_EXTERNAL_OES   0x8D65
#define GL_TEXTURE_MIN_FILTER     0x2801
#define GL_TEXTURE_MAG_FILTER     0x2800
#define GL_TEXTURE_WRAP_S         0x2802
#define GL_TEXTURE_WRAP_T         0x2803
#define GL_CLAMP_TO_EDGE          0x812F
#define GL_NEAREST                0x2600
#define GL_LINEAR                 0x2601
#define GL_FRAMEBUFFER            0x8D40
#define GL_RENDERBUFFER           0x8D41
#define GL_COLOR_ATTACHMENT0      0x8CE0
#define GL_FRAMEBUFFER_COMPLETE   0x8CD5
#define GL_COLOR_BUFFER_BIT       0x4000
#define GL_VERTEX_SHADER          0x8B31
#define GL_FRAGMENT_SHADER        0x8B30
#define GL_COMPILE_STATUS         0x8B81
#define GL_LINK_STATUS            0x8B82
#define GL_FLOAT                  0x1406
#define GL_TRIANGLE_STRIP         0x0005
#define GL_TEXTURE0               0x84C0
#define GL_SCISSOR_TEST           0x0C11
#define GL_BLEND                  0x0BE2

struct gbm_device;
struct gbm_bo;
union gbm_bo_handle { void *ptr; int32_t s32; uint32_t u32; int64_t s64; uint64_t u64; };
#define GBM_BO_USE_SCANOUT   (1 << 0)
#define GBM_BO_USE_RENDERING (1 << 2)

#define FOURCC(a, b, c, d) ((uint32_t)(a) | ((uint32_t)(b) << 8) | \
                            ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
#define FMT_XRGB8888 FOURCC('X', 'R', '2', '4')
#define FMT_ARGB8888 FOURCC('A', 'R', '2', '4')
#define MOD_INVALID  0x00ffffffffffffffull
#define MOD_LINEAR   0ull

struct fns {
    /* EGL */
    void *(*GetProcAddress)(const char *);
    EGLDisplay (*GetPlatformDisplayEXT)(EGLenum, void *, const EGLint *);
    EGLBoolean (*Initialize)(EGLDisplay, EGLint *, EGLint *);
    EGLBoolean (*Terminate)(EGLDisplay);
    const char *(*QueryString)(EGLDisplay, EGLint);
    EGLBoolean (*BindAPI)(EGLenum);
    EGLContext (*CreateContext)(EGLDisplay, EGLConfig, EGLContext, const EGLint *);
    EGLBoolean (*DestroyContext)(EGLDisplay, EGLContext);
    EGLBoolean (*MakeCurrent)(EGLDisplay, EGLSurface, EGLSurface, EGLContext);
    EGLBoolean (*QueryContext)(EGLDisplay, EGLContext, EGLint, EGLint *);
    EGLint (*GetError)(void);
    EGLImage (*CreateImageKHR)(EGLDisplay, EGLContext, EGLenum, EGLClientBuffer, const EGLint *);
    EGLBoolean (*DestroyImageKHR)(EGLDisplay, EGLImage);
    EGLSync (*CreateSyncKHR)(EGLDisplay, EGLenum, const EGLint *);
    EGLBoolean (*DestroySyncKHR)(EGLDisplay, EGLSync);
    EGLint (*ClientWaitSyncKHR)(EGLDisplay, EGLSync, EGLint, EGLTime);
    EGLBoolean (*QueryDmaBufModifiersEXT)(EGLDisplay, EGLint, EGLint, uint64_t *, EGLBoolean *, EGLint *);
    /* GLES2 */
    void (*EGLImageTargetTexture2DOES)(GLenum, void *);
    void (*EGLImageTargetRenderbufferStorageOES)(GLenum, void *);
    void (*GenTextures)(GLsizei, GLuint *);
    void (*DeleteTextures)(GLsizei, const GLuint *);
    void (*BindTexture)(GLenum, GLuint);
    void (*ActiveTexture)(GLenum);
    void (*TexParameteri)(GLenum, GLenum, GLint);
    void (*GenFramebuffers)(GLsizei, GLuint *);
    void (*DeleteFramebuffers)(GLsizei, const GLuint *);
    void (*BindFramebuffer)(GLenum, GLuint);
    void (*FramebufferRenderbuffer)(GLenum, GLenum, GLenum, GLuint);
    GLenum (*CheckFramebufferStatus)(GLenum);
    void (*GenRenderbuffers)(GLsizei, GLuint *);
    void (*DeleteRenderbuffers)(GLsizei, const GLuint *);
    void (*BindRenderbuffer)(GLenum, GLuint);
    void (*Viewport)(GLint, GLint, GLsizei, GLsizei);
    void (*ClearColor)(GLfloat, GLfloat, GLfloat, GLfloat);
    void (*Clear)(GLbitfield);
    void (*Disable)(GLenum);
    GLuint (*CreateShader)(GLenum);
    void (*ShaderSource)(GLuint, GLsizei, const GLchar *const *, const GLint *);
    void (*CompileShader)(GLuint);
    void (*GetShaderiv)(GLuint, GLenum, GLint *);
    void (*GetShaderInfoLog)(GLuint, GLsizei, GLsizei *, GLchar *);
    void (*DeleteShader)(GLuint);
    GLuint (*CreateProgram)(void);
    void (*AttachShader)(GLuint, GLuint);
    void (*BindAttribLocation)(GLuint, GLuint, const GLchar *);
    void (*LinkProgram)(GLuint);
    void (*GetProgramiv)(GLuint, GLenum, GLint *);
    void (*DeleteProgram)(GLuint);
    void (*UseProgram)(GLuint);
    GLint (*GetUniformLocation)(GLuint, const GLchar *);
    void (*Uniform1i)(GLint, GLint);
    void (*Uniform4f)(GLint, GLfloat, GLfloat, GLfloat, GLfloat);
    void (*VertexAttribPointer)(GLuint, GLint, GLenum, GLboolean, GLsizei, const void *);
    void (*EnableVertexAttribArray)(GLuint);
    void (*DrawArrays)(GLenum, GLint, GLsizei);
    void (*Flush)(void);
    void (*ReadPixels)(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, void *);
    void (*RenderbufferStorage)(GLenum, GLenum, GLsizei, GLsizei);
    void (*Uniform1f)(GLint, GLfloat);
    /* GBM */
    struct gbm_device *(*gbm_create_device)(int);
    void (*gbm_device_destroy)(struct gbm_device *);
    struct gbm_bo *(*gbm_bo_create)(struct gbm_device *, uint32_t, uint32_t, uint32_t, uint32_t);
    struct gbm_bo *(*gbm_bo_create_with_modifiers)(struct gbm_device *, uint32_t, uint32_t, uint32_t, const uint64_t *, unsigned);
    struct gbm_bo *(*gbm_bo_create_with_modifiers2)(struct gbm_device *, uint32_t, uint32_t, uint32_t, const uint64_t *, unsigned, uint32_t);
    void (*gbm_bo_destroy)(struct gbm_bo *);
    int (*gbm_bo_get_fd)(struct gbm_bo *);
    uint32_t (*gbm_bo_get_stride)(struct gbm_bo *);
    uint32_t (*gbm_bo_get_offset)(struct gbm_bo *, int);
    uint64_t (*gbm_bo_get_modifier)(struct gbm_bo *);
};

#define NB_GS_SRC 8

struct nb_gs {
    struct fns f;
    void *h_egl, *h_gles, *h_gbm;
    int drm_fd;
    char node[64];
    struct gbm_device *gbm;
    EGLDisplay dpy;
    EGLContext ctx;
    enum nb_gs_prio prio_req, prio_got;
    GLuint prog;
    GLint u_tex, u_rect, u_swap;
    bool readback;              /* targets are plain renderbuffers, read back */
    struct {
        bool     valid;
        struct gbm_bo *bo;
        int      fd;
        EGLImage img;
        GLuint   rb, fbo;
    } t[NB_GS_TARGETS];
    uint32_t tw, th;
    struct {
        bool     valid;
        int      key;
        uint32_t w, h;
        EGLImage img;
        GLuint   tex;
    } s[NB_GS_SRC];
};

static void say(char *why, size_t n, const char *fmt, ...)
{
    va_list ap;

    if (!why || !n) {
        return;
    }
    va_start(ap, fmt);
    vsnprintf(why, n, fmt, ap);
    va_end(ap);
}

static void *sym(void *h, const char *name)
{
    return h ? dlsym(h, name) : NULL;
}

static bool load(struct nb_gs *g, char *why, size_t n)
{
    struct fns *f = &g->f;

    g->h_egl = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
    g->h_gles = dlopen("libGLESv2.so.2", RTLD_NOW | RTLD_LOCAL);
    g->h_gbm = dlopen("libgbm.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!g->h_egl || !g->h_gles || !g->h_gbm) {
        say(why, n, "cannot load %s", !g->h_egl ? "libEGL.so.1" :
            !g->h_gles ? "libGLESv2.so.2" : "libgbm.so.1");
        return false;
    }
#define E(field, name) do { *(void **)&f->field = sym(g->h_egl, name); \
        if (!f->field) { say(why, n, "libEGL lacks %s", name); return false; } } while (0)
    E(GetProcAddress, "eglGetProcAddress");
    E(Initialize, "eglInitialize");
    E(Terminate, "eglTerminate");
    E(QueryString, "eglQueryString");
    E(BindAPI, "eglBindAPI");
    E(CreateContext, "eglCreateContext");
    E(DestroyContext, "eglDestroyContext");
    E(MakeCurrent, "eglMakeCurrent");
    E(QueryContext, "eglQueryContext");
    E(GetError, "eglGetError");
#undef E
#define X(field, name) do { *(void **)&f->field = f->GetProcAddress(name); \
        if (!f->field) { say(why, n, "EGL lacks %s", name); return false; } } while (0)
    X(GetPlatformDisplayEXT, "eglGetPlatformDisplayEXT");
    X(CreateImageKHR, "eglCreateImageKHR");
    X(DestroyImageKHR, "eglDestroyImageKHR");
    X(CreateSyncKHR, "eglCreateSyncKHR");
    X(DestroySyncKHR, "eglDestroySyncKHR");
    X(ClientWaitSyncKHR, "eglClientWaitSyncKHR");
    X(EGLImageTargetTexture2DOES, "glEGLImageTargetTexture2DOES");
    X(EGLImageTargetRenderbufferStorageOES, "glEGLImageTargetRenderbufferStorageOES");
#undef X
    *(void **)&f->QueryDmaBufModifiersEXT =
        f->GetProcAddress("eglQueryDmaBufModifiersEXT");
#define G(field, name) do { *(void **)&f->field = sym(g->h_gles, name); \
        if (!f->field) { say(why, n, "libGLESv2 lacks %s", name); return false; } } while (0)
    G(GenTextures, "glGenTextures");
    G(DeleteTextures, "glDeleteTextures");
    G(BindTexture, "glBindTexture");
    G(ActiveTexture, "glActiveTexture");
    G(TexParameteri, "glTexParameteri");
    G(GenFramebuffers, "glGenFramebuffers");
    G(DeleteFramebuffers, "glDeleteFramebuffers");
    G(BindFramebuffer, "glBindFramebuffer");
    G(FramebufferRenderbuffer, "glFramebufferRenderbuffer");
    G(CheckFramebufferStatus, "glCheckFramebufferStatus");
    G(GenRenderbuffers, "glGenRenderbuffers");
    G(DeleteRenderbuffers, "glDeleteRenderbuffers");
    G(BindRenderbuffer, "glBindRenderbuffer");
    G(Viewport, "glViewport");
    G(ClearColor, "glClearColor");
    G(Clear, "glClear");
    G(Disable, "glDisable");
    G(CreateShader, "glCreateShader");
    G(ShaderSource, "glShaderSource");
    G(CompileShader, "glCompileShader");
    G(GetShaderiv, "glGetShaderiv");
    G(GetShaderInfoLog, "glGetShaderInfoLog");
    G(DeleteShader, "glDeleteShader");
    G(CreateProgram, "glCreateProgram");
    G(AttachShader, "glAttachShader");
    G(BindAttribLocation, "glBindAttribLocation");
    G(LinkProgram, "glLinkProgram");
    G(GetProgramiv, "glGetProgramiv");
    G(DeleteProgram, "glDeleteProgram");
    G(UseProgram, "glUseProgram");
    G(GetUniformLocation, "glGetUniformLocation");
    G(Uniform1i, "glUniform1i");
    G(Uniform4f, "glUniform4f");
    G(VertexAttribPointer, "glVertexAttribPointer");
    G(EnableVertexAttribArray, "glEnableVertexAttribArray");
    G(DrawArrays, "glDrawArrays");
    G(Flush, "glFlush");
    G(ReadPixels, "glReadPixels");
    G(RenderbufferStorage, "glRenderbufferStorage");
    G(Uniform1f, "glUniform1f");
#undef G
#define B(field, name) do { *(void **)&f->field = sym(g->h_gbm, name); \
        if (!f->field) { say(why, n, "libgbm lacks %s", name); return false; } } while (0)
    B(gbm_create_device, "gbm_create_device");
    B(gbm_device_destroy, "gbm_device_destroy");
    B(gbm_bo_create, "gbm_bo_create");
    B(gbm_bo_destroy, "gbm_bo_destroy");
    B(gbm_bo_get_fd, "gbm_bo_get_fd");
    B(gbm_bo_get_stride, "gbm_bo_get_stride");
    B(gbm_bo_get_offset, "gbm_bo_get_offset");
    B(gbm_bo_get_modifier, "gbm_bo_get_modifier");
#undef B
    *(void **)&f->gbm_bo_create_with_modifiers =
        sym(g->h_gbm, "gbm_bo_create_with_modifiers");
    /* Optional (Mesa 21.3+): the same with usage flags. */
    *(void **)&f->gbm_bo_create_with_modifiers2 =
        sym(g->h_gbm, "gbm_bo_create_with_modifiers2");
    return true;
}

static bool has_ext(const char *list, const char *ext)
{
    size_t n = strlen(ext);

    while (list && (list = strstr(list, ext))) {
        if (list[n] == ' ' || list[n] == '\0') {
            return true;
        }
        list += n;
    }
    return false;
}

enum nb_gs_prio nb_gs_prio_parse(const char *s, bool *bad)
{
    static const struct { const char *name; enum nb_gs_prio p; } t[] = {
        { "off", NB_GS_PRIO_MEDIUM }, { "medium", NB_GS_PRIO_MEDIUM },
        { "default", NB_GS_PRIO_MEDIUM }, { "high", NB_GS_PRIO_HIGH },
        { "realtime", NB_GS_PRIO_REALTIME }, { "rt", NB_GS_PRIO_REALTIME },
    };

    if (bad) {
        *bad = false;
    }
    if (!s || !*s) {
        return NB_GS_PRIO_HIGH;
    }
    for (size_t i = 0; i < sizeof(t) / sizeof(t[0]); i++) {
        if (!strcasecmp(s, t[i].name)) {
            return t[i].p;
        }
    }
    if (bad) {
        *bad = true;
    }
    return NB_GS_PRIO_HIGH;
}

const char *nb_gs_prio_name(enum nb_gs_prio p)
{
    switch (p) {
    case NB_GS_PRIO_LOW:      return "low";
    case NB_GS_PRIO_MEDIUM:   return "medium";
    case NB_GS_PRIO_HIGH:     return "high";
    case NB_GS_PRIO_REALTIME: return "realtime";
    default:                  return "unknown";
    }
}

enum nb_gs_prio nb_gs_prio_requested(const struct nb_gs *g)
{
    return g ? g->prio_req : NB_GS_PRIO_UNKNOWN;
}

enum nb_gs_prio nb_gs_prio_granted(const struct nb_gs *g)
{
    return g ? g->prio_got : NB_GS_PRIO_UNKNOWN;
}

static enum nb_gs_prio prio_from_egl(EGLint v)
{
    switch (v) {
    case EGL_CONTEXT_PRIORITY_REALTIME_NV: return NB_GS_PRIO_REALTIME;
    case EGL_CONTEXT_PRIORITY_HIGH_IMG:    return NB_GS_PRIO_HIGH;
    case EGL_CONTEXT_PRIORITY_MEDIUM_IMG:  return NB_GS_PRIO_MEDIUM;
    case EGL_CONTEXT_PRIORITY_LOW_IMG:     return NB_GS_PRIO_LOW;
    default:                               return NB_GS_PRIO_UNKNOWN;
    }
}

/* EGL + context on an open render node.  false with why. */
static bool egl_up(struct nb_gs *g, char *why, size_t n)
{
    struct fns *f = &g->f;
    const char *ext;
    EGLint major, minor;
    EGLint attr_pr[] = { EGL_CONTEXT_CLIENT_VERSION, 2,
                         EGL_CONTEXT_PRIORITY_LEVEL_IMG,
                         EGL_CONTEXT_PRIORITY_HIGH_IMG, EGL_NONE };
    EGLint attr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    bool bad = false;
    enum nb_gs_prio want =
        nb_gs_prio_parse(getenv("CONDUIT_VIEWER_GPU_PRIORITY"), &bad);

    g->gbm = f->gbm_create_device(g->drm_fd);
    if (!g->gbm) {
        say(why, n, "gbm_create_device(%s) failed", g->node);
        return false;
    }
    g->dpy = f->GetPlatformDisplayEXT(EGL_PLATFORM_GBM_KHR, g->gbm, NULL);
    if (!g->dpy || !f->Initialize(g->dpy, &major, &minor)) {
        say(why, n, "no EGL display on %s (0x%x)", g->node, f->GetError());
        g->dpy = NULL;
        return false;
    }
    ext = f->QueryString(g->dpy, EGL_EXTENSIONS);
    if (!has_ext(ext, "EGL_EXT_image_dma_buf_import") ||
        !has_ext(ext, "EGL_KHR_surfaceless_context") ||
        !has_ext(ext, "EGL_KHR_no_config_context")) {
        say(why, n, "EGL on %s lacks dma-buf import or surfaceless contexts",
            g->node);
        return false;
    }
    if (!f->BindAPI(EGL_OPENGL_ES_API)) {
        say(why, n, "eglBindAPI(GLES) failed");
        return false;
    }
    if (bad) {
        fprintf(stderr, "conduit-viewer: gpu-scale: CONDUIT_VIEWER_GPU_PRIORITY="
                "%s is not off|medium|high|realtime; using high\n",
                getenv("CONDUIT_VIEWER_GPU_PRIORITY"));
    }
    /*
     * Above-normal priority where the driver offers it: the pass sits
     * between the guest's frame and the screen, and shares the GPU with the
     * guest.  Realtime first when asked (EGL_NV_context_priority_realtime),
     * then HIGH, then none: a driver may refuse the attribute outright
     * instead of granting less.
     */
    g->prio_req = want;
    g->prio_got = NB_GS_PRIO_UNKNOWN;
    if (want == NB_GS_PRIO_REALTIME &&
        has_ext(ext, "EGL_NV_context_priority_realtime")) {
        attr_pr[3] = EGL_CONTEXT_PRIORITY_REALTIME_NV;
        g->ctx = f->CreateContext(g->dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT,
                                  attr_pr);
    }
    if (!g->ctx && want >= NB_GS_PRIO_HIGH &&
        has_ext(ext, "EGL_IMG_context_priority")) {
        attr_pr[3] = EGL_CONTEXT_PRIORITY_HIGH_IMG;
        g->ctx = f->CreateContext(g->dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT,
                                  attr_pr);
    }
    if (!g->ctx) {
        g->ctx = f->CreateContext(g->dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT,
                                  attr);
    }
    if (!g->ctx || !f->MakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE,
                                   g->ctx)) {
        say(why, n, "no GLES2 context on %s (0x%x)", g->node, f->GetError());
        return false;
    }
    /* What the driver granted, which the extension says the query reports
     * (it may be less than asked, silently). */
    if (has_ext(ext, "EGL_IMG_context_priority")) {
        EGLint v = 0;

        if (f->QueryContext(g->dpy, g->ctx, EGL_CONTEXT_PRIORITY_LEVEL_IMG,
                            &v)) {
            g->prio_got = prio_from_egl(v);
        }
    } else {
        g->prio_got = NB_GS_PRIO_MEDIUM;    /* no priorities at all */
    }
    return true;
}

static GLuint shader(struct nb_gs *g, GLenum type, const char *src)
{
    struct fns *f = &g->f;
    GLuint s = f->CreateShader(type);
    GLint ok = 0;

    f->ShaderSource(s, 1, &src, NULL);
    f->CompileShader(s);
    f->GetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        f->DeleteShader(s);
        return 0;
    }
    return s;
}

static bool prog_up(struct nb_gs *g, char *why, size_t n)
{
    struct fns *f = &g->f;
    /* a_pos in 0..1 over the destination; the source rectangle in texture
     * coordinates arrives as u_rect (x, y, w, h). */
    static const char *vs =
        "attribute vec2 a_pos;\n"
        "uniform vec4 u_rect;\n"
        "varying vec2 v_tc;\n"
        "void main() {\n"
        "  v_tc = u_rect.xy + a_pos * u_rect.zw;\n"
        "  gl_Position = vec4(a_pos * 2.0 - 1.0, 0.0, 1.0);\n"
        "}\n";
    static const char *fs =
        "#extension GL_OES_EGL_image_external : require\n"
        "precision mediump float;\n"
        "uniform samplerExternalOES u_tex;\n"
        "uniform float u_swap;\n"
        "varying vec2 v_tc;\n"
        "void main() {\n"
        "  vec3 c = texture2D(u_tex, v_tc).rgb;\n"
        "  gl_FragColor = vec4(mix(c, c.bgr, u_swap), 1.0);\n"
        "}\n";
    static const GLfloat quad[] = { 0, 0, 1, 0, 0, 1, 1, 1 };
    GLuint v = shader(g, GL_VERTEX_SHADER, vs);
    GLuint fr = shader(g, GL_FRAGMENT_SHADER, fs);
    GLint ok = 0;

    if (!v || !fr) {
        say(why, n, "shader compile failed (GL_OES_EGL_image_external?)");
        return false;
    }
    g->prog = f->CreateProgram();
    f->AttachShader(g->prog, v);
    f->AttachShader(g->prog, fr);
    f->BindAttribLocation(g->prog, 0, "a_pos");
    f->LinkProgram(g->prog);
    f->DeleteShader(v);
    f->DeleteShader(fr);
    f->GetProgramiv(g->prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        say(why, n, "shader link failed");
        return false;
    }
    f->UseProgram(g->prog);
    g->u_tex = f->GetUniformLocation(g->prog, "u_tex");
    g->u_rect = f->GetUniformLocation(g->prog, "u_rect");
    g->u_swap = f->GetUniformLocation(g->prog, "u_swap");
    f->Uniform1i(g->u_tex, 0);
    /* Client-side vertex array: GLES2 allows it, and it is 32 bytes. */
    f->VertexAttribPointer(0, 2, GL_FLOAT, 0, 0, quad);
    f->EnableVertexAttribArray(0);
    f->Disable(GL_BLEND);
    f->Disable(GL_SCISSOR_TEST);
    return true;
}

static bool can_import(struct nb_gs *g, uint32_t fourcc, uint64_t mod)
{
    uint64_t mods[64];
    EGLBoolean ext_only[64];
    EGLint k = 0;

    if (mod == MOD_INVALID || !g->f.QueryDmaBufModifiersEXT) {
        return true;            /* cannot ask; the import will tell */
    }
    if (!g->f.QueryDmaBufModifiersEXT(g->dpy, (EGLint)fourcc, 64, mods,
                                      ext_only, &k)) {
        return false;
    }
    for (EGLint i = 0; i < k; i++) {
        if (mods[i] == mod) {
            return true;
        }
    }
    return false;
}

static void gs_down(struct nb_gs *g)
{
    struct fns *f = &g->f;

    if (g->dpy && g->ctx) {
        for (int i = 0; i < NB_GS_SRC; i++) {
            nb_gs_src_drop(g, g->s[i].valid ? g->s[i].key : -1);
        }
        for (int i = 0; i < NB_GS_TARGETS; i++) {
            if (!g->t[i].valid) {
                continue;
            }
            f->DeleteFramebuffers(1, &g->t[i].fbo);
            f->DeleteRenderbuffers(1, &g->t[i].rb);
            f->DestroyImageKHR(g->dpy, g->t[i].img);
        }
        if (g->prog) {
            f->DeleteProgram(g->prog);
        }
        f->MakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        f->DestroyContext(g->dpy, g->ctx);
    }
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        if (g->t[i].valid) {
            close(g->t[i].fd);
            f->gbm_bo_destroy(g->t[i].bo);
            g->t[i].valid = false;
        }
    }
    if (g->dpy) {
        f->Terminate(g->dpy);
    }
    if (g->gbm) {
        f->gbm_device_destroy(g->gbm);
    }
    if (g->drm_fd >= 0) {
        close(g->drm_fd);
    }
    g->dpy = g->ctx = NULL;
    g->gbm = NULL;
    g->prog = 0;
    g->drm_fd = -1;
}

static bool try_node(struct nb_gs *g, const char *path, uint32_t fourcc,
                     uint64_t mod, char *why, size_t n)
{
    snprintf(g->node, sizeof(g->node), "%s", path);
    g->drm_fd = open(path, O_RDWR | O_CLOEXEC);
    if (g->drm_fd < 0) {
        say(why, n, "%s: %s", path, strerror(errno));
        return false;
    }
    if (!egl_up(g, why, n) || !prog_up(g, why, n)) {
        gs_down(g);
        return false;
    }
    if (fourcc && !can_import(g, fourcc, mod)) {
        say(why, n, "%s cannot import %.4s modifier 0x%016llx", path,
            (const char *)&fourcc, (unsigned long long)mod);
        gs_down(g);
        return false;
    }
    return true;
}

struct nb_gs *nb_gs_open(const char *node, dev_t dev, uint32_t fourcc,
                         uint64_t mod, char *why, size_t n)
{
    struct nb_gs *g = calloc(1, sizeof(*g));
    char path[300];

    if (!g) {
        say(why, n, "out of memory");
        return NULL;
    }
    g->drm_fd = -1;
    if (!load(g, why, n)) {
        nb_gs_close(g);
        return NULL;
    }
    if (node && *node) {
        if (try_node(g, node, fourcc, mod, why, n)) {
            return g;
        }
        nb_gs_close(g);
        return NULL;
    }
    /* The compositor's own device first: its buffers are what it imports
     * cheapest, and the guest's frames live there on a single-GPU host. */
    if (dev) {
        DIR *d = opendir("/dev/dri");
        struct dirent *e;

        while (d && (e = readdir(d))) {
            struct stat st;

            if (strncmp(e->d_name, "renderD", 7)) {
                continue;
            }
            snprintf(path, sizeof(path), "/dev/dri/%s", e->d_name);
            /* Same device as the card node `dev`: compare the sysfs device. */
            if (stat(path, &st) == 0) {
                char a[96], b[96], ra[256], rb[256];

                snprintf(a, sizeof(a), "/sys/dev/char/%u:%u/device",
                         major(st.st_rdev), minor(st.st_rdev));
                snprintf(b, sizeof(b), "/sys/dev/char/%u:%u/device",
                         major(dev), minor(dev));
                if (realpath(a, ra) && realpath(b, rb) && !strcmp(ra, rb) &&
                    try_node(g, path, fourcc, mod, why, n)) {
                    closedir(d);
                    return g;
                }
            }
        }
        if (d) {
            closedir(d);
        }
    }
    /*
     * --gpu auto (nb_gpusel.h): the GPU of the frame's own vendor first --
     * it made the frame and imports its layout natively -- then the rest,
     * by PCI facts rather than node numbers.
     */
    {
        struct nb_gpu_node nodes[NB_GPU_MAX];
        int order[NB_GPU_MAX];
        int k = nb_gpusel_scan(NULL, nodes, NB_GPU_MAX);

        nb_gpu_order(nodes, k, mod, order);
        for (int i = 0; i < k; i++) {
            if (access(nodes[order[i]].path, F_OK) == 0 &&
                try_node(g, nodes[order[i]].path, fourcc, mod, why, n)) {
                return g;
            }
        }
        if (!k) {
            /* No sysfs (a container): the node numbers, in order. */
            for (int i = 128; i < 136; i++) {
                snprintf(path, sizeof(path), "/dev/dri/renderD%d", i);
                if (access(path, F_OK) == 0 &&
                    try_node(g, path, fourcc, mod, why, n)) {
                    return g;
                }
            }
        }
    }
    nb_gs_close(g);
    return NULL;
}

bool nb_gs_can_import(struct nb_gs *g, uint32_t fourcc, uint64_t mod)
{
    return g && can_import(g, fourcc, mod);
}

void nb_gs_close(struct nb_gs *g)
{
    if (!g) {
        return;
    }
    gs_down(g);
    /* The libraries stay loaded: a driver's dlclose is not always safe, and
     * a second open in the same process reuses them. */
    free(g);
}

const char *nb_gs_node(const struct nb_gs *g)
{
    return g ? g->node : "";
}

static EGLImage import(struct nb_gs *g, int fd, uint32_t w, uint32_t h,
                       uint32_t stride, uint32_t offset, uint32_t fourcc,
                       uint64_t mod)
{
    EGLint a[32];
    int k = 0;

    a[k++] = EGL_WIDTH;                      a[k++] = (EGLint)w;
    a[k++] = EGL_HEIGHT;                     a[k++] = (EGLint)h;
    a[k++] = EGL_LINUX_DRM_FOURCC_EXT;       a[k++] = (EGLint)fourcc;
    a[k++] = EGL_DMA_BUF_PLANE0_FD_EXT;      a[k++] = fd;
    a[k++] = EGL_DMA_BUF_PLANE0_OFFSET_EXT;  a[k++] = (EGLint)offset;
    a[k++] = EGL_DMA_BUF_PLANE0_PITCH_EXT;   a[k++] = (EGLint)stride;
    if (mod != MOD_INVALID) {
        a[k++] = EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT;
        a[k++] = (EGLint)(mod & 0xffffffffu);
        a[k++] = EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT;
        a[k++] = (EGLint)(mod >> 32);
    }
    a[k++] = EGL_IMAGE_PRESERVED_KHR;        a[k++] = EGL_TRUE;
    a[k] = EGL_NONE;
    return g->f.CreateImageKHR(g->dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT,
                               NULL, a);
}

static void targets_free(struct nb_gs *g)
{
    struct fns *f = &g->f;

    for (int i = 0; i < NB_GS_TARGETS; i++) {
        if (!g->t[i].valid) {
            continue;
        }
        f->DeleteFramebuffers(1, &g->t[i].fbo);
        f->DeleteRenderbuffers(1, &g->t[i].rb);
        if (g->t[i].img) {
            f->DestroyImageKHR(g->dpy, g->t[i].img);
        }
        if (g->t[i].fd >= 0) {
            close(g->t[i].fd);
        }
        if (g->t[i].bo) {
            f->gbm_bo_destroy(g->t[i].bo);
        }
        memset(&g->t[i], 0, sizeof(g->t[i]));
    }
    g->tw = g->th = 0;
}

int nb_gs_targets_readback(struct nb_gs *g, uint32_t w, uint32_t h,
                           char *why, size_t n)
{
    struct fns *f = &g->f;

    targets_free(g);
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        g->t[i].fd = -1;
        f->GenRenderbuffers(1, &g->t[i].rb);
        f->BindRenderbuffer(GL_RENDERBUFFER, g->t[i].rb);
        f->RenderbufferStorage(GL_RENDERBUFFER, 0x8058 /* GL_RGBA8_OES */,
                               (GLsizei)w, (GLsizei)h);
        f->GenFramebuffers(1, &g->t[i].fbo);
        f->BindFramebuffer(GL_FRAMEBUFFER, g->t[i].fbo);
        f->FramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                   GL_RENDERBUFFER, g->t[i].rb);
        g->t[i].valid = true;
        if (f->CheckFramebufferStatus(GL_FRAMEBUFFER) !=
            GL_FRAMEBUFFER_COMPLETE) {
            say(why, n, "the scaler's read-back framebuffer is incomplete");
            targets_free(g);
            return 0;
        }
    }
    g->tw = w;
    g->th = h;
    g->readback = true;
    return NB_GS_TARGETS;
}

int nb_gs_readback(struct nb_gs *g, int target, void *dst)
{
    if (!g->readback || target < 0 || target >= NB_GS_TARGETS ||
        !g->t[target].valid) {
        return -EINVAL;
    }
    g->f.BindFramebuffer(GL_FRAMEBUFFER, g->t[target].fbo);
    /* R and B were swapped by the shader, so GL_RGBA bytes are XRGB8888's
     * B, G, R, X in memory; rows top-down as everywhere here. */
    g->f.ReadPixels(0, 0, (GLsizei)g->tw, (GLsizei)g->th, 0x1908 /* RGBA */,
                    0x1401 /* UNSIGNED_BYTE */, dst);
    return 0;
}

int nb_gs_targets(struct nb_gs *g, uint32_t w, uint32_t h, nb_gs_fmt_ok ok,
                  void *ctx, const uint64_t *scan_mods, unsigned n_scan,
                  struct nb_gs_target out[NB_GS_TARGETS],
                  char *why, size_t n)
{
    struct fns *f = &g->f;
    static const uint32_t fmts[2] = { FMT_XRGB8888, FMT_ARGB8888 };

    targets_free(g);
    g->readback = false;
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        struct gbm_bo *bo = NULL;
        uint32_t fmt = 0;
        uint64_t mod;

        /*
         * The compositor's scanout tranche first, when it named one: GBM
         * picks among exactly the XRGB modifiers a plane takes, so the
         * output buffer is not only importable but scannable.
         */
        if (n_scan && (f->gbm_bo_create_with_modifiers2 ||
                       f->gbm_bo_create_with_modifiers)) {
            uint64_t m[32];
            unsigned k = 0;

            for (unsigned j = 0; j < n_scan && k < 32; j++) {
                if (!ok || ok(ctx, FMT_XRGB8888, scan_mods[j])) {
                    m[k++] = scan_mods[j];
                }
            }
            if (k) {
                fmt = FMT_XRGB8888;
                bo = f->gbm_bo_create_with_modifiers2
                         ? f->gbm_bo_create_with_modifiers2(
                               g->gbm, w, h, fmt, m, k,
                               GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING)
                         : f->gbm_bo_create_with_modifiers(g->gbm, w, h, fmt,
                                                           m, k);
            }
        }
        /* Opaque XRGB first (a plane takes it without blending), and only
         * a layout the compositor said it takes. */
        for (int k = 0; k < 2 && !bo; k++) {
            fmt = fmts[k];
            bo = f->gbm_bo_create(g->gbm, w, h, fmt,
                                  GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING);
            if (bo && ok && !ok(ctx, fmt, f->gbm_bo_get_modifier(bo))) {
                uint64_t lin = MOD_LINEAR;

                f->gbm_bo_destroy(bo);
                bo = NULL;
                /* The compositor refused GBM's choice: LINEAR, which every
                 * dma-buf compositor takes (and costs scanout nothing on
                 * most display engines at this size). */
                if (f->gbm_bo_create_with_modifiers && ok(ctx, fmt, lin)) {
                    bo = f->gbm_bo_create_with_modifiers(g->gbm, w, h, fmt,
                                                         &lin, 1);
                }
            }
        }
        if (!bo) {
            say(why, n, "cannot allocate a %ux%u scanout buffer the "
                "compositor accepts", w, h);
            targets_free(g);
            return 0;
        }
        mod = f->gbm_bo_get_modifier(bo);
        g->t[i].bo = bo;
        g->t[i].fd = f->gbm_bo_get_fd(bo);
        out[i] = (struct nb_gs_target){
            .fd = g->t[i].fd, .w = w, .h = h,
            .stride = f->gbm_bo_get_stride(bo),
            .offset = f->gbm_bo_get_offset(bo, 0),
            .fourcc = fmt, .modifier = mod,
        };
        g->t[i].img = g->t[i].fd >= 0
                          ? import(g, g->t[i].fd, w, h, out[i].stride,
                                   out[i].offset, fmt, mod)
                          : EGL_NO_IMAGE;
        if (!g->t[i].img) {
            say(why, n, "cannot import the scaler's own buffer (0x%x)",
                f->GetError());
            if (g->t[i].fd >= 0) {
                close(g->t[i].fd);
            }
            f->gbm_bo_destroy(bo);
            memset(&g->t[i], 0, sizeof(g->t[i]));
            targets_free(g);
            return 0;
        }
        f->GenRenderbuffers(1, &g->t[i].rb);
        f->BindRenderbuffer(GL_RENDERBUFFER, g->t[i].rb);
        f->EGLImageTargetRenderbufferStorageOES(GL_RENDERBUFFER, g->t[i].img);
        f->GenFramebuffers(1, &g->t[i].fbo);
        f->BindFramebuffer(GL_FRAMEBUFFER, g->t[i].fbo);
        f->FramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                   GL_RENDERBUFFER, g->t[i].rb);
        g->t[i].valid = true;
        if (f->CheckFramebufferStatus(GL_FRAMEBUFFER) !=
            GL_FRAMEBUFFER_COMPLETE) {
            say(why, n, "the scaler's framebuffer is incomplete");
            targets_free(g);
            return 0;
        }
    }
    g->tw = w;
    g->th = h;
    return NB_GS_TARGETS;
}

int nb_gs_src(struct nb_gs *g, int key, int fd, uint32_t w, uint32_t h,
              uint32_t stride, uint32_t offset, uint32_t fourcc, uint64_t mod)
{
    struct fns *f = &g->f;
    int free_i = -1;

    for (int i = 0; i < NB_GS_SRC; i++) {
        if (g->s[i].valid && g->s[i].key == key) {
            return i;
        }
        if (!g->s[i].valid && free_i < 0) {
            free_i = i;
        }
    }
    if (free_i < 0 || fd < 0) {
        return -1;
    }
    g->s[free_i].img = import(g, fd, w, h, stride, offset, fourcc, mod);
    if (!g->s[free_i].img) {
        return -1;
    }
    f->GenTextures(1, &g->s[free_i].tex);
    f->BindTexture(GL_TEXTURE_EXTERNAL_OES, g->s[free_i].tex);
    f->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    f->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    f->EGLImageTargetTexture2DOES(GL_TEXTURE_EXTERNAL_OES, g->s[free_i].img);
    g->s[free_i].valid = true;
    g->s[free_i].key = key;
    g->s[free_i].w = w;
    g->s[free_i].h = h;
    return free_i;
}

void nb_gs_src_drop(struct nb_gs *g, int key)
{
    if (!g || key < 0) {
        return;
    }
    for (int i = 0; i < NB_GS_SRC; i++) {
        if (g->s[i].valid && g->s[i].key == key) {
            g->f.DeleteTextures(1, &g->s[i].tex);
            g->f.DestroyImageKHR(g->dpy, g->s[i].img);
            memset(&g->s[i], 0, sizeof(g->s[i]));
        }
    }
}

static uint64_t now_ns(void)
{
    struct timespec ts;

    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

int nb_gs_draw(struct nb_gs *g, const struct nb_gs_draw *d,
               unsigned timeout_us, unsigned *gpu_us)
{
    struct fns *f = &g->f;
    int si = -1;
    EGLSync sync;
    EGLint r;
    uint64_t t0;

    for (int i = 0; i < NB_GS_SRC; i++) {
        if (g->s[i].valid && g->s[i].key == d->src_key) {
            si = i;
        }
    }
    if (si < 0 || d->target < 0 || d->target >= NB_GS_TARGETS ||
        !g->t[d->target].valid) {
        return -EINVAL;
    }
    f->BindFramebuffer(GL_FRAMEBUFFER, g->t[d->target].fbo);
    /* Bars: only when the picture leaves some of the buffer uncovered. */
    if (d->dx > 0 || d->dy > 0 || d->dx + d->dw < (int)g->tw ||
        d->dy + d->dh < (int)g->th) {
        f->Viewport(0, 0, (GLsizei)g->tw, (GLsizei)g->th);
        f->ClearColor(0, 0, 0, 1);
        f->Clear(GL_COLOR_BUFFER_BIT);
    }
    /*
     * Rows are addressed top-down in both buffers (row 0 of a dma-buf is
     * GL's y = 0, in a texture and in a framebuffer alike), so the
     * destination is placed with dy as is and nothing is flipped.
     */
    f->Viewport(d->dx, d->dy, d->dw, d->dh);
    f->ActiveTexture(GL_TEXTURE0);
    f->BindTexture(GL_TEXTURE_EXTERNAL_OES, g->s[si].tex);
    f->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER,
                     d->nearest ? GL_NEAREST : GL_LINEAR);
    f->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER,
                     d->nearest ? GL_NEAREST : GL_LINEAR);
    /* The source rectangle, buffer pixels -> texture coordinates. */
    f->Uniform4f(g->u_rect, (GLfloat)(d->sx / g->s[si].w),
                 (GLfloat)(d->sy / g->s[si].h), (GLfloat)(d->sw / g->s[si].w),
                 (GLfloat)(d->sh / g->s[si].h));
    f->Uniform1f(g->u_swap, g->readback ? 1.0f : 0.0f);
    f->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    t0 = now_ns();
    sync = f->CreateSyncKHR(g->dpy, EGL_SYNC_FENCE_KHR, NULL);
    if (!sync) {
        f->Flush();
        return -EIO;
    }
    r = f->ClientWaitSyncKHR(g->dpy, sync, EGL_SYNC_FLUSH_COMMANDS_BIT_KHR,
                             (EGLTime)timeout_us * 1000u);
    f->DestroySyncKHR(g->dpy, sync);
    if (gpu_us) {
        *gpu_us = (unsigned)((now_ns() - t0) / 1000u);
    }
    if (r == EGL_TIMEOUT_EXPIRED_KHR) {
        return -ETIMEDOUT;
    }
    return r == EGL_CONDITION_SATISFIED_KHR ? 0 : -EIO;
}

int nb_gs_read(struct nb_gs *g, int target, int x, int y, uint32_t *rgb)
{
    uint8_t px[4] = { 0 };

    if (target < 0 || target >= NB_GS_TARGETS || !g->t[target].valid) {
        return -EINVAL;
    }
    g->f.BindFramebuffer(GL_FRAMEBUFFER, g->t[target].fbo);
    g->f.ReadPixels(x, y, 1, 1, 0x1908 /* GL_RGBA */, 0x1401 /* UNSIGNED_BYTE */,
                    px);
    *rgb = ((uint32_t)px[0] << 16) | ((uint32_t)px[1] << 8) | px[2];
    return 0;
}
