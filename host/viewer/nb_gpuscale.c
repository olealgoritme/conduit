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
 *
 * The context is GLES 3.1 where the driver has it (asked for first), for the
 * picture-enhancement passes at the end of this file; the plain pass is the
 * same GLES 2 shader either way.
 */
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <limits.h>
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
#include "nb_enhance.h"
#include "shaders/nis_coef.h"
#include "nb_shaders.h"         /* generated: shaders/embed.sh */

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
#define GL_TEXTURE_2D             0x0DE1
#define GL_VERSION                0x1F02
#define GL_RGBA                   0x1908
#define GL_UNSIGNED_BYTE          0x1401
#define GL_RGBA8                  0x8058
#define GL_RGBA16F                0x881A
#define GL_RGBA32F                0x8814
#define GL_COMPUTE_SHADER         0x91B9
#define GL_WRITE_ONLY             0x88B9
#define GL_INFO_LOG_LENGTH        0x8B84
#define GL_TEXTURE_FETCH_BARRIER_BIT     0x00000008
#define GL_SHADER_IMAGE_ACCESS_BARRIER_BIT 0x00000020
#define GL_FRAMEBUFFER_BARRIER_BIT       0x00000400
#define GL_TIME_ELAPSED_EXT       0x88BF
#define GL_QUERY_RESULT_EXT       0x8866
#define GL_NO_ERROR               0

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
    EGLContext (*GetCurrentContext)(void);
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
    void (*Enable)(GLenum);
    void (*Scissor)(GLint, GLint, GLsizei, GLsizei);
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
    /* GLES 3.1, optional: the enhancement passes (NULL: not offered) */
    const unsigned char *(*GetString)(GLenum);
    void (*GetProgramInfoLog)(GLuint, GLsizei, GLsizei *, GLchar *);
    void (*FramebufferTexture2D)(GLenum, GLenum, GLenum, GLuint, GLint);
    void (*TexSubImage2D)(GLenum, GLint, GLint, GLint, GLsizei, GLsizei,
                          GLenum, GLenum, const void *);
    void (*TexStorage2D)(GLenum, GLsizei, GLenum, GLsizei, GLsizei);
    void (*Uniform2f)(GLint, GLfloat, GLfloat);
    void (*Uniform2i)(GLint, GLint, GLint);
    void (*Uniform1ui)(GLint, GLuint);
    void (*DispatchCompute)(GLuint, GLuint, GLuint);
    void (*BindImageTexture)(GLuint, GLuint, GLint, GLboolean, GLint, GLenum,
                             GLenum);
    void (*MemoryBarrier)(GLbitfield);
    GLenum (*GlGetError)(void);
    /* EXT_disjoint_timer_query, optional: the benchmark's GPU times */
    void (*GenQueriesEXT)(GLsizei, GLuint *);
    void (*DeleteQueriesEXT)(GLsizei, const GLuint *);
    void (*BeginQueryEXT)(GLenum, GLuint);
    void (*EndQueryEXT)(GLenum);
    void (*GetQueryObjectui64vEXT)(GLuint, GLenum, uint64_t *);
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
#define NB_FG_LEVELS 4          /* luma pyramid: 1/2 .. 1/16 of the frame */

/* An internal texture (immutable storage, one level). */
struct gs_tex {
    GLuint   id;
    uint32_t w, h;
    GLenum   fmt;
};

/* The enhancement programs (enh_progs[] gives their sources). */
enum {
    P_COPY = 0, P_EASU, P_RCAS, P_FSR1, P_CAS, P_CAS_SCALE, P_NIS, P_FG_DOWN,
    P_FG_FLOW, P_FG_INTERP, P_COUNT
};

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
    GLint u_tex, u_rect, u_swap, u_alpha;
    bool readback;              /* targets are plain renderbuffers, read back */
    struct {
        bool     valid;
        struct gbm_bo *bo;
        int      fd;
        EGLImage img;
        GLuint   rb, fbo;
        /* The bars around this destination are black already: the clear
         * is skipped while the picture keeps its place. */
        bool     bars_ok;
        int      bx, by, bw, bh;
    } t[NB_GS_TARGETS];
    uint32_t tw, th;
    struct {
        bool     valid;
        int      key;
        uint32_t w, h;
        EGLImage img;
        GLuint   tex;
        GLuint   tex2d;         /* the same image as a GL_TEXTURE_2D, which
                                 * the passes sample directly; 0: external
                                 * only (nb_gs_src)                       */
    } s[NB_GS_SRC];
    /* picture enhancement */
    bool        es31;           /* GLES 3.1 and every entry point it needs */
    char        enh_why[200];   /* why not, or the last shader failure    */
    struct { GLuint id; bool tried; } pr[P_COUNT];
    GLuint      vs;             /* the quad vertex shader, shared         */
    GLuint      fbo_t;          /* renders into the internal textures     */
    struct gs_tex in_tex, out_tex, cas_tex, coef[2];
    bool        coef_ok;
    int         enh_used;       /* NB_UPSCALE_* the last draw ran          */
    const char *enh_used_why;
    /* frame generation: two frames, their luma pyramids, the flow */
    struct gs_tex hist[2], lum[2][NB_FG_LEVELS], flow[NB_FG_LEVELS], mid;
    int         fg_cur, fg_n, fg_levels;
    uint32_t    fg_w, fg_h;
    bool        fsr_one_pass;   /* CONDUIT_VIEWER_FSR_PASSES=1            */
    bool        no_direct;      /* CONDUIT_VIEWER_ENH_DIRECT=0: always copy */
    bool        bench_q;        /* nb_gs_bench: a timer query is open      */
};

static void enh_down(struct nb_gs *g);
static bool enh_probe(struct nb_gs *g);

/*
 * This instance's context, current on this thread.  A viewer has more than
 * one (the picture's, the guest cursor's copy), and a context is made
 * current at creation only: without this, a call on one would run in the
 * other's context, where its textures and programs do not exist.
 */
static void gs_cur(struct nb_gs *g)
{
    if (g && g->ctx && g->f.GetCurrentContext &&
        g->f.GetCurrentContext() != g->ctx) {
        g->f.MakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx);
    }
}

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
    E(GetCurrentContext, "eglGetCurrentContext");
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
    G(Enable, "glEnable");
    G(Scissor, "glScissor");
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
    /* GLES 3.x / 3.1 (and a few GLES 2 ones only the enhancement passes
     * use): optional, from the library or eglGetProcAddress. */
#define O(field, name) do { *(void **)&f->field = sym(g->h_gles, name); \
        if (!f->field) { *(void **)&f->field = f->GetProcAddress(name); } } while (0)
    O(GetString, "glGetString");
    O(GetProgramInfoLog, "glGetProgramInfoLog");
    O(FramebufferTexture2D, "glFramebufferTexture2D");
    O(TexSubImage2D, "glTexSubImage2D");
    O(TexStorage2D, "glTexStorage2D");
    O(Uniform2f, "glUniform2f");
    O(Uniform2i, "glUniform2i");
    O(Uniform1ui, "glUniform1ui");
    O(DispatchCompute, "glDispatchCompute");
    O(BindImageTexture, "glBindImageTexture");
    O(MemoryBarrier, "glMemoryBarrier");
    O(GlGetError, "glGetError");
    O(GenQueriesEXT, "glGenQueriesEXT");
    O(DeleteQueriesEXT, "glDeleteQueriesEXT");
    O(BeginQueryEXT, "glBeginQueryEXT");
    O(EndQueryEXT, "glEndQueryEXT");
    O(GetQueryObjectui64vEXT, "glGetQueryObjectui64vEXT");
#undef O
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
        { "low", NB_GS_PRIO_LOW },
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

static enum nb_gs_prio prio_default = NB_GS_PRIO_HIGH;

void nb_gs_set_prio_default(enum nb_gs_prio p)
{
    prio_default = p;
}

enum nb_gs_prio nb_gs_prio_wanted(void)
{
    const char *e = getenv("CONDUIT_VIEWER_GPU_PRIORITY");

    return e && *e ? nb_gs_prio_parse(e, NULL) : prio_default;
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
    EGLint attr_pr[] = { EGL_CONTEXT_CLIENT_VERSION, 3,
                         EGL_CONTEXT_PRIORITY_LEVEL_IMG,
                         EGL_CONTEXT_PRIORITY_HIGH_IMG, EGL_NONE };
    EGLint attr[] = { EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE };
    bool bad = false;
    const char *pe = getenv("CONDUIT_VIEWER_GPU_PRIORITY");
    enum nb_gs_prio want = pe && *pe ? nb_gs_prio_parse(pe, &bad)
                                     : prio_default;

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
                "%s is not low|off|medium|high|realtime; using high\n",
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
    /*
     * GLES 3 first (the driver gives the highest compatible version, 3.1+
     * on every current one), for the enhancement passes; GLES 2 when it has
     * none.  The plain pass is a GLES 2 shader and runs in either.
     */
    for (EGLint ver = 3; ver >= 2 && !g->ctx; ver--) {
        attr_pr[1] = attr[1] = ver;
        if (want == NB_GS_PRIO_REALTIME &&
            has_ext(ext, "EGL_NV_context_priority_realtime")) {
            attr_pr[3] = EGL_CONTEXT_PRIORITY_REALTIME_NV;
            g->ctx = f->CreateContext(g->dpy, EGL_NO_CONFIG_KHR,
                                      EGL_NO_CONTEXT, attr_pr);
        }
        if (!g->ctx && want != NB_GS_PRIO_MEDIUM &&
            has_ext(ext, "EGL_IMG_context_priority")) {
            attr_pr[3] = want == NB_GS_PRIO_LOW ? EGL_CONTEXT_PRIORITY_LOW_IMG
                                                : EGL_CONTEXT_PRIORITY_HIGH_IMG;
            g->ctx = f->CreateContext(g->dpy, EGL_NO_CONFIG_KHR,
                                      EGL_NO_CONTEXT, attr_pr);
        }
        if (!g->ctx) {
            g->ctx = f->CreateContext(g->dpy, EGL_NO_CONFIG_KHR,
                                      EGL_NO_CONTEXT, attr);
        }
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
    g->es31 = enh_probe(g);
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
        "uniform float u_alpha;\n"
        "varying vec2 v_tc;\n"
        "void main() {\n"
        "  vec4 t = texture2D(u_tex, v_tc);\n"
        "  gl_FragColor = vec4(mix(t.rgb, t.bgr, u_swap), mix(1.0, t.a, u_alpha));\n"
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
    g->u_alpha = f->GetUniformLocation(g->prog, "u_alpha");
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
        gs_cur(g);
        for (int i = 0; i < NB_GS_SRC; i++) {
            nb_gs_src_drop(g, g->s[i].valid ? g->s[i].key : -1);
        }
        for (int i = 0; i < NB_GS_TARGETS; i++) {
            if (!g->t[i].valid) {
                continue;
            }
            f->DeleteFramebuffers(1, &g->t[i].fbo);
            f->DeleteRenderbuffers(1, &g->t[i].rb);
            if (g->t[i].img) {      /* read-back targets have none */
                f->DestroyImageKHR(g->dpy, g->t[i].img);
            }
        }
        if (g->prog) {
            f->DeleteProgram(g->prog);
        }
        enh_down(g);
        f->MakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        f->DestroyContext(g->dpy, g->ctx);
    }
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        if (g->t[i].valid) {
            /* Read-back targets are plain renderbuffers: no dma-buf, no bo. */
            if (g->t[i].fd >= 0) {
                close(g->t[i].fd);
            }
            if (g->t[i].bo) {
                f->gbm_bo_destroy(g->t[i].bo);
            }
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
    {
        const char *e = getenv("CONDUIT_VIEWER_FSR_PASSES");
        const char *dr = getenv("CONDUIT_VIEWER_ENH_DIRECT");

        g->fsr_one_pass = e && !strcmp(e, "1");
        g->no_direct = dr && !strcmp(dr, "0");
    }
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
                char a[96], b[96], ra[PATH_MAX], rb[PATH_MAX];   /* realpath() writes up to PATH_MAX bytes */

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
    gs_cur(g);
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
    gs_cur(g);
    if (!g->readback || target < 0 || target >= NB_GS_TARGETS ||
        !g->t[target].valid) {
        return -EINVAL;
    }
    g->f.BindFramebuffer(GL_FRAMEBUFFER, g->t[target].fbo);
    /* R and B were swapped by the shader, so GL_RGBA bytes are XRGB8888's
     * B, G, R, X in memory (ARGB8888's B, G, R, A for an alpha draw); rows
     * top-down as everywhere here. */
    g->f.ReadPixels(0, 0, (GLsizei)g->tw, (GLsizei)g->th, 0x1908 /* RGBA */,
                    0x1401 /* UNSIGNED_BYTE */, dst);
    return 0;
}

/* One buffer in `fmt`, GBM choosing among `m` (scanout-capable first). */
static struct gbm_bo *bo_with_mods(struct nb_gs *g, uint32_t w, uint32_t h,
                                   uint32_t fmt, const uint64_t *m, unsigned k)
{
    struct fns *f = &g->f;
    struct gbm_bo *bo = NULL;

    if (!k) {
        return NULL;
    }
    if (f->gbm_bo_create_with_modifiers2) {
        bo = f->gbm_bo_create_with_modifiers2(g->gbm, w, h, fmt, m, k,
                                              GBM_BO_USE_SCANOUT |
                                              GBM_BO_USE_RENDERING);
        if (!bo) {
            /* A layout the display engine cannot scan out is still one the
             * compositor composites. */
            bo = f->gbm_bo_create_with_modifiers2(g->gbm, w, h, fmt, m, k,
                                                  GBM_BO_USE_RENDERING);
        }
    } else if (f->gbm_bo_create_with_modifiers) {
        bo = f->gbm_bo_create_with_modifiers(g->gbm, w, h, fmt, m, k);
    }
    return bo;
}

int nb_gs_targets(struct nb_gs *g, uint32_t w, uint32_t h, nb_gs_fmt_ok ok,
                  void *ctx, const uint32_t *cand_fourcc,
                  const uint64_t *cand_mod, unsigned n_cand,
                  struct nb_gs_target out[NB_GS_TARGETS],
                  char *why, size_t n)
{
    struct fns *f = &g->f;

    gs_cur(g);
    static const uint32_t fmts[2] = { FMT_XRGB8888, FMT_ARGB8888 };

    targets_free(g);
    g->readback = false;
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        struct gbm_bo *bo = NULL;
        uint32_t fmt = 0;
        uint64_t mod;

        /*
         * The candidates first (nb_present.h nb_pc_targets: the layouts the
         * compositor lists, scanout-capable ones first in fullscreen), one
         * format at a time in the order given: GBM picks among exactly the
         * modifiers of that format the compositor takes.
         */
        for (unsigned j = 0; j < n_cand && !bo; ) {
            uint64_t m[48];
            unsigned k = 0, e = j;

            fmt = cand_fourcc[j];
            for (; e < n_cand && cand_fourcc[e] == fmt; e++) {
                if (k < 48 && (!ok || ok(ctx, fmt, cand_mod[e]))) {
                    m[k++] = cand_mod[e];
                }
            }
            bo = bo_with_mods(g, w, h, fmt, m, k);
            j = e;
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

/* The driver lists (fourcc, mod) as importable and not external-only. */
static bool tex2d_ok(struct nb_gs *g, uint32_t fourcc, uint64_t mod)
{
    uint64_t mods[64];
    EGLBoolean ext_only[64];
    EGLint k = 0;

    if (mod == MOD_INVALID || !g->f.QueryDmaBufModifiersEXT ||
        !g->f.QueryDmaBufModifiersEXT(g->dpy, (EGLint)fourcc, 64, mods,
                                      ext_only, &k)) {
        return false;
    }
    for (EGLint i = 0; i < k; i++) {
        if (mods[i] == mod) {
            return !ext_only[i];
        }
    }
    return false;
}

int nb_gs_src(struct nb_gs *g, int key, int fd, uint32_t w, uint32_t h,
              uint32_t stride, uint32_t offset, uint32_t fourcc, uint64_t mod)
{
    gs_cur(g);
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
    g->s[free_i].tex2d = 0;
    /*
     * The same image as an ordinary 2D texture, when the driver says this
     * layout is not external-only: the enhancement passes then read the
     * guest's buffer directly, with no copy (textureGather and texelFetch
     * need a sampler2D).
     */
    if (g->es31 && !g->no_direct && f->GlGetError && tex2d_ok(g, fourcc, mod)) {
        GLuint t2;

        while (f->GlGetError() != GL_NO_ERROR) {
        }
        f->GenTextures(1, &t2);
        f->BindTexture(GL_TEXTURE_2D, t2);
        f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        f->EGLImageTargetTexture2DOES(GL_TEXTURE_2D, g->s[free_i].img);
        if (f->GlGetError() == GL_NO_ERROR) {
            g->s[free_i].tex2d = t2;
        } else {
            f->DeleteTextures(1, &t2);
        }
    }
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
    gs_cur(g);
    for (int i = 0; i < NB_GS_SRC; i++) {
        if (g->s[i].valid && g->s[i].key == key) {
            if (g->s[i].tex2d) {
                g->f.DeleteTextures(1, &g->s[i].tex2d);
            }
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

/* Wait for everything submitted so far (bounded).  0, or -errno. */
static int gs_finish(struct nb_gs *g, unsigned timeout_us, unsigned *gpu_us)
{
    struct fns *f = &g->f;
    EGLSync sync;
    EGLint r;
    uint64_t t0 = now_ns();

    if (g->bench_q) {
        f->EndQueryEXT(GL_TIME_ELAPSED_EXT);
        g->bench_q = false;
    }
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

static int src_index(const struct nb_gs *g, int key)
{
    for (int i = 0; i < NB_GS_SRC; i++) {
        if (g->s[i].valid && g->s[i].key == key) {
            return i;
        }
    }
    return -1;
}

static int enh_frame(struct nb_gs *g, int si, const struct nb_gs_draw *d);

/*
 * Bars: only when the picture leaves some of the buffer uncovered, and only
 * when this target's bars are not black already for this destination (every
 * pass writes inside the destination only, so they stay so).  A 7680x2160
 * target with a 4:3 picture would otherwise clear 33 MB of bars per frame.
 */
static void gs_bars(struct nb_gs *g, const struct nb_gs_draw *d)
{
    struct fns *f = &g->f;
    typeof(g->t[0]) *t = &g->t[d->target];
    bool bars = d->dx > 0 || d->dy > 0 || d->dx + d->dw < (int)g->tw ||
                d->dy + d->dh < (int)g->th;

    if (!bars) {
        t->bars_ok = false;
        return;
    }
    if (t->bars_ok && t->bx == d->dx && t->by == d->dy && t->bw == d->dw &&
        t->bh == d->dh) {
        return;
    }
    f->Viewport(0, 0, (GLsizei)g->tw, (GLsizei)g->th);
    f->ClearColor(0, 0, 0, 1);
    f->Clear(GL_COLOR_BUFFER_BIT);
    t->bars_ok = true;
    t->bx = d->dx;
    t->by = d->dy;
    t->bw = d->dw;
    t->bh = d->dh;
}

int nb_gs_draw(struct nb_gs *g, const struct nb_gs_draw *d,
               unsigned timeout_us, unsigned *gpu_us)
{
    gs_cur(g);
    struct fns *f = &g->f;
    int si = d->src_key == NB_GS_SRC_FG ? -1 : src_index(g, d->src_key);
    bool enh = d->src_key == NB_GS_SRC_FG ||
               (d->upscale > NB_UPSCALE_OFF && !d->alpha && g->es31);

    if ((si < 0 && d->src_key != NB_GS_SRC_FG) || d->target < 0 ||
        d->target >= NB_GS_TARGETS || !g->t[d->target].valid) {
        return -EINVAL;
    }
    if (d->src_key == NB_GS_SRC_FG && (!g->es31 || g->fg_n < 1)) {
        return -EINVAL;
    }
    f->BindFramebuffer(GL_FRAMEBUFFER, g->t[d->target].fbo);
    gs_bars(g, d);
    if (enh) {
        int r = enh_frame(g, si, d);

        if (r < 0) {
            return r;
        }
        return gs_finish(g, timeout_us, gpu_us);
    }
    g->enh_used = NB_UPSCALE_OFF;
    g->enh_used_why = d->upscale > NB_UPSCALE_OFF && !d->alpha
                          ? (g->enh_why[0] ? g->enh_why : "no GLES 3.1")
                          : NULL;
    f->UseProgram(g->prog);     /* an enhancement pass may have run */
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
    f->Uniform1f(g->u_alpha, d->alpha ? 1.0f : 0.0f);
    f->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    return gs_finish(g, timeout_us, gpu_us);
}

int nb_gs_read(struct nb_gs *g, int target, int x, int y, uint32_t *rgb)
{
    gs_cur(g);
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

/* ── picture enhancement (docs/VIEWER-ENHANCE.md) ──────────────────────────── */
/*
 * Everything below runs only when an upscaler or frame generation is on.  The
 * passes render into internal textures (immutable RGBA8 / RGBA16F storage)
 * and end in the same target framebuffer as the plain pass, so the targets,
 * the read-back level and the caller's fence wait are unchanged.
 *
 * Sources: shaders/ and the vendored third_party/ headers, embedded as
 * strings by shaders/embed.sh (nb_shaders.h); shaders/check.sh compiles the
 * same programs offline.
 */

struct enh_src {
    const char *name;
    bool        compute;
    const char *src[6];
};

static const struct enh_src enh_progs[P_COUNT] = {
    [P_COPY]      = { "copy", false, { nb_sh_es_frag, nb_sh_copy } },
    [P_EASU]      = { "FSR 1 EASU", false,
                      { nb_sh_es_frag, nb_sh_easu_head, nb_sh_ffx_a,
                        nb_sh_ffx_fsr1, nb_sh_easu_main } },
    [P_RCAS]      = { "FSR 1 RCAS", false,
                      { nb_sh_es_frag, nb_sh_rcas_head, nb_sh_ffx_a,
                        nb_sh_ffx_fsr1, nb_sh_rcas_main } },
    [P_FSR1]      = { "FSR 1 (one pass)", false,
                      { nb_sh_es_frag, nb_sh_fsr1_head, nb_sh_ffx_a,
                        nb_sh_ffx_fsr1, nb_sh_fsr1_main } },
    [P_CAS]       = { "CAS", false,
                      { nb_sh_es_frag, nb_sh_cas_noscale, nb_sh_cas_head,
                        nb_sh_ffx_a, nb_sh_ffx_cas, nb_sh_cas_main } },
    [P_CAS_SCALE] = { "CAS (scaling)", false,
                      { nb_sh_es_frag, nb_sh_cas_scale, nb_sh_cas_head,
                        nb_sh_ffx_a, nb_sh_ffx_cas, nb_sh_cas_main } },
    [P_NIS]       = { "NIS", true,
                      { nb_sh_es_comp, nb_sh_nis_head, nb_sh_nis_scaler,
                        nb_sh_nis_main } },
    [P_FG_DOWN]   = { "frame generation (pyramid)", true,
                      { nb_sh_es_comp, nb_sh_fg_down } },
    [P_FG_FLOW]   = { "frame generation (flow)", true,
                      { nb_sh_es_comp, nb_sh_fg_flow } },
    [P_FG_INTERP] = { "frame generation (interpolation)", true,
                      { nb_sh_es_comp, nb_sh_fg_interp } },
};

/* GLES 3.1 with every entry point the passes call.  Called once, current. */
static bool enh_probe(struct nb_gs *g)
{
    struct fns *f = &g->f;
    const char *v;
    int maj = 0, min = 0;

    if (!f->GetString || !f->FramebufferTexture2D || !f->TexStorage2D ||
        !f->TexSubImage2D || !f->Uniform2f || !f->Uniform2i ||
        !f->Uniform1ui || !f->DispatchCompute || !f->BindImageTexture ||
        !f->MemoryBarrier || !f->GetProgramInfoLog) {
        say(g->enh_why, sizeof(g->enh_why), "the GL library lacks GLES 3.1 "
            "entry points");
        return false;
    }
    v = (const char *)f->GetString(GL_VERSION);
    if (!v || sscanf(v, "OpenGL ES %d.%d", &maj, &min) != 2 ||
        maj * 10 + min < 31) {
        say(g->enh_why, sizeof(g->enh_why), "the GPU pass is %s, GLES 3.1 "
            "needed", v ? v : "of unknown version");
        return false;
    }
    g->enh_why[0] = '\0';
    return true;
}

bool nb_gs_enhance_ok(struct nb_gs *g, const char **why)
{
    if (why) {
        *why = g && !g->es31 ? g->enh_why : NULL;
    }
    return g && g->es31;
}

int nb_gs_enh_status(const struct nb_gs *g, const char **why)
{
    if (why) {
        *why = g ? g->enh_used_why : NULL;
    }
    return g ? g->enh_used : NB_UPSCALE_OFF;
}

static void tex_free(struct nb_gs *g, struct gs_tex *t)
{
    if (t->id) {
        g->f.DeleteTextures(1, &t->id);
    }
    memset(t, 0, sizeof(*t));
}

/* t at w x h in fmt, (re)made when it is not.  Linear filtering (nearest for
 * RGBA32F, which is not filterable: a linear one would be incomplete). */
static void tex_need(struct nb_gs *g, struct gs_tex *t, uint32_t w,
                     uint32_t h, GLenum fmt)
{
    struct fns *f = &g->f;
    GLint filt = fmt == GL_RGBA32F ? GL_NEAREST : GL_LINEAR;

    if (t->id && t->w == w && t->h == h && t->fmt == fmt) {
        return;
    }
    tex_free(g, t);
    f->GenTextures(1, &t->id);
    f->BindTexture(GL_TEXTURE_2D, t->id);
    f->TexStorage2D(GL_TEXTURE_2D, 1, fmt, (GLsizei)w, (GLsizei)h);
    f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, filt);
    f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, filt);
    f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    t->w = w;
    t->h = h;
    t->fmt = fmt;
}

static void tex_bind(struct nb_gs *g, int unit, const struct gs_tex *t,
                     bool nearest)
{
    struct fns *f = &g->f;
    GLint filt = nearest || t->fmt == GL_RGBA32F ? GL_NEAREST : GL_LINEAR;

    f->ActiveTexture(GL_TEXTURE0 + (GLenum)unit);
    f->BindTexture(GL_TEXTURE_2D, t->id);
    f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, filt);
    f->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, filt);
}

static void enh_down(struct nb_gs *g)
{
    struct fns *f = &g->f;

    for (int i = 0; i < P_COUNT; i++) {
        if (g->pr[i].id) {
            f->DeleteProgram(g->pr[i].id);
        }
        g->pr[i].id = 0;
        g->pr[i].tried = false;
    }
    if (g->vs) {
        f->DeleteShader(g->vs);
        g->vs = 0;
    }
    if (g->fbo_t) {
        f->DeleteFramebuffers(1, &g->fbo_t);
        g->fbo_t = 0;
    }
    tex_free(g, &g->in_tex);
    tex_free(g, &g->out_tex);
    tex_free(g, &g->cas_tex);
    tex_free(g, &g->coef[0]);
    tex_free(g, &g->coef[1]);
    tex_free(g, &g->mid);
    for (int k = 0; k < 2; k++) {
        tex_free(g, &g->hist[k]);
        for (int l = 0; l < NB_FG_LEVELS; l++) {
            tex_free(g, &g->lum[k][l]);
        }
    }
    for (int l = 0; l < NB_FG_LEVELS; l++) {
        tex_free(g, &g->flow[l]);
    }
    g->coef_ok = false;
    g->fg_n = 0;
}

static GLuint enh_shader(struct nb_gs *g, GLenum type, const char *const *src,
                         int n, const char *name)
{
    struct fns *f = &g->f;
    GLuint s = f->CreateShader(type);
    GLint ok = 0;

    f->ShaderSource(s, n, src, NULL);
    f->CompileShader(s);
    f->GetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = "";

        f->GetShaderInfoLog(s, sizeof(log), NULL, log);
        fprintf(stderr, "conduit-viewer: gpu-scale: the %s shader does not "
                "compile here: %s\n", name, log);
        say(g->enh_why, sizeof(g->enh_why), "the %s shader does not compile "
            "on this GPU's driver", name);
        f->DeleteShader(s);
        return 0;
    }
    return s;
}

/* The program k, built the first time it is asked for.  0: unavailable. */
static GLuint enh_prog(struct nb_gs *g, int k)
{
    struct fns *f = &g->f;
    const struct enh_src *e = &enh_progs[k];
    GLuint sh, p;
    GLint ok = 0;
    int n = 0;

    if (g->pr[k].tried) {
        return g->pr[k].id;
    }
    g->pr[k].tried = true;
    if (!g->es31) {
        return 0;
    }
    while (n < 6 && e->src[n]) {
        n++;
    }
    if (!e->compute && !g->vs) {
        static const char *const vsrc[1] = { nb_sh_quad_vert };

        g->vs = enh_shader(g, GL_VERTEX_SHADER, vsrc, 1, "quad");
        if (!g->vs) {
            return 0;
        }
    }
    sh = enh_shader(g, e->compute ? GL_COMPUTE_SHADER : GL_FRAGMENT_SHADER,
                    e->src, n, e->name);
    if (!sh) {
        return 0;
    }
    p = f->CreateProgram();
    f->AttachShader(p, sh);
    if (!e->compute) {
        f->AttachShader(p, g->vs);
    }
    f->LinkProgram(p);
    f->DeleteShader(sh);
    f->GetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1024] = "";

        f->GetProgramInfoLog(p, sizeof(log), NULL, log);
        fprintf(stderr, "conduit-viewer: gpu-scale: the %s program does not "
                "link here: %s\n", e->name, log);
        say(g->enh_why, sizeof(g->enh_why), "the %s program does not link on "
            "this GPU's driver", e->name);
        f->DeleteProgram(p);
        return 0;
    }
    g->pr[k].id = p;
    return p;
}

static GLint U(struct nb_gs *g, GLuint p, const char *name)
{
    return g->f.GetUniformLocation(p, name);
}

/* Render into an internal texture, all of it. */
static void rt_tex(struct nb_gs *g, const struct gs_tex *t)
{
    struct fns *f = &g->f;

    if (!g->fbo_t) {
        f->GenFramebuffers(1, &g->fbo_t);
    }
    f->BindFramebuffer(GL_FRAMEBUFFER, g->fbo_t);
    f->FramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                            GL_TEXTURE_2D, t->id, 0);
    f->Viewport(0, 0, (GLsizei)t->w, (GLsizei)t->h);
}

/* Render into the draw's target, at its destination rectangle. */
static void rt_target(struct nb_gs *g, const struct nb_gs_draw *d)
{
    g->f.BindFramebuffer(GL_FRAMEBUFFER, g->t[d->target].fbo);
    g->f.Viewport(d->dx, d->dy, d->dw, d->dh);
}

/* The plain pass from an internal texture into the current viewport. */
static void copy_draw(struct nb_gs *g, const struct gs_tex *t, bool nearest,
                      float swap)
{
    struct fns *f = &g->f;
    GLuint p = g->pr[P_COPY].id;

    f->UseProgram(p);
    tex_bind(g, 0, t, nearest);
    f->Uniform1i(U(g, p, "u_src"), 0);
    f->Uniform4f(U(g, p, "u_rect"), 0, 0, 1, 1);
    f->Uniform1f(U(g, p, "u_swap"), swap);
    f->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
}

/*
 * The guest's buffer (source slot si, rectangle sx..sh) into an internal
 * RGBA8 texture of the rectangle's size, through the plain GLES 2 program:
 * the one place the external dma-buf texture is read.
 */
static void enh_ingest(struct nb_gs *g, int si, double sx, double sy,
                       double sw, double sh, struct gs_tex *dst)
{
    struct fns *f = &g->f;
    uint32_t w = sw >= 1.0 ? (uint32_t)(sw + 0.5) : 1;
    uint32_t h = sh >= 1.0 ? (uint32_t)(sh + 0.5) : 1;

    tex_need(g, dst, w, h, GL_RGBA8);
    rt_tex(g, dst);
    f->UseProgram(g->prog);
    f->ActiveTexture(GL_TEXTURE0);
    f->BindTexture(GL_TEXTURE_EXTERNAL_OES, g->s[si].tex);
    f->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER,
                     GL_LINEAR);
    f->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER,
                     GL_LINEAR);
    f->Uniform4f(g->u_rect, (GLfloat)(sx / g->s[si].w),
                 (GLfloat)(sy / g->s[si].h), (GLfloat)(sw / g->s[si].w),
                 (GLfloat)(sh / g->s[si].h));
    f->Uniform1f(g->u_swap, 0.0f);
    f->Uniform1f(g->u_alpha, 0.0f);
    f->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
}

static bool nis_coef_up(struct nb_gs *g)
{
    if (g->coef_ok) {
        return true;
    }
    tex_need(g, &g->coef[0], 2, NB_NIS_PHASES, GL_RGBA32F);
    g->f.TexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 2, NB_NIS_PHASES, GL_RGBA,
                       GL_FLOAT, nb_nis_coef_scale);
    tex_need(g, &g->coef[1], 2, NB_NIS_PHASES, GL_RGBA32F);
    g->f.TexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 2, NB_NIS_PHASES, GL_RGBA,
                       GL_FLOAT, nb_nis_coef_usm);
    g->coef_ok = true;
    return true;
}

static void nis_uniforms(struct nb_gs *g, GLuint p, const struct nb_nis_cfg *c)
{
    struct fns *f = &g->f;

#define UF(n) f->Uniform1f(U(g, p, #n), c->n)
#define UU(n) f->Uniform1ui(U(g, p, #n), c->n)
    UF(kDetectRatio); UF(kDetectThres); UF(kMinContrastRatio);
    UF(kRatioNorm); UF(kContrastBoost); UF(kEps); UF(kSharpStartY);
    UF(kSharpScaleY); UF(kSharpStrengthMin); UF(kSharpStrengthScale);
    UF(kSharpLimitMin); UF(kSharpLimitScale); UF(kScaleX); UF(kScaleY);
    UF(kDstNormX); UF(kDstNormY); UF(kSrcNormX); UF(kSrcNormY);
    UU(kInputViewportOriginX); UU(kInputViewportOriginY);
    UU(kInputViewportWidth); UU(kInputViewportHeight);
    UU(kOutputViewportOriginX); UU(kOutputViewportOriginY);
    UU(kOutputViewportWidth); UU(kOutputViewportHeight);
#undef UF
#undef UU
}

/*
 * Draw the internal texture `in` into the target with the upscaler the
 * draw asks for, or the best one that runs here (nb_enh_pick, then what the
 * driver compiled).  0, or -EIO when not even the plain copy runs.
 */
static int enh_draw(struct nb_gs *g, const struct gs_tex *in,
                    const struct nb_gs_draw *d)
{
    struct fns *f = &g->f;
    uint32_t dw = (uint32_t)(d->dw > 0 ? d->dw : 1);
    uint32_t dh = (uint32_t)(d->dh > 0 ? d->dh : 1);
    float swap = g->readback ? 1.0f : 0.0f;
    const char *why = NULL;
    int m = nb_enh_pick(d->upscale, in->w, in->h, dw, dh, &why);
    bool rcas = nb_rcas_on(d->sharp);
    /*
     * FSR: EASU into an output-size texture, then RCAS (two passes), or in
     * one pass with CONDUIT_VIEWER_FSR_PASSES=1 (fsr1_main.glsl: RCAS over
     * EASU computed in place, five EASU evaluations per pixel; it trades the
     * intermediate's memory traffic for ALU and measured 4x slower on an AMD
     * iGPU, so it is not the default).  EASU alone (sharpness 0) is one pass
     * either way.
     */
    bool fsr1 = g->fsr_one_pass && rcas && enh_prog(g, P_FSR1);
    /* CAS scales by itself up to 4x the area (CasSupportScaling). */
    double area = (double)dw * dh / ((double)in->w * in->h);
    bool cas1 = dw >= in->w && dh >= in->h && area <= 4.0 &&
                enh_prog(g, P_CAS_SCALE);
    struct nb_nis_cfg nc = { 0 };
    GLuint p;

    if (!enh_prog(g, P_COPY)) {
        return -EIO;
    }
    if (m == NB_UPSCALE_NIS &&
        (!enh_prog(g, P_NIS) || !nis_coef_up(g) ||
         !nb_nis_config(&nc, nb_sharp_unit(d->sharp), in->w, in->h, dw, dh))) {
        why = g->pr[P_NIS].id ? "NIS: scale out of its range; FSR instead"
                              : "NIS does not run on this GPU's driver; FSR "
                                "instead";
        m = NB_UPSCALE_FSR;
        rcas = nb_rcas_on(d->sharp);
        fsr1 = g->fsr_one_pass && rcas && enh_prog(g, P_FSR1);
    }
    if (m == NB_UPSCALE_FSR && !fsr1 &&
        (!enh_prog(g, P_EASU) || (rcas && !enh_prog(g, P_RCAS)))) {
        why = g->enh_why;
        m = NB_UPSCALE_OFF;
    }
    if (m == NB_UPSCALE_CAS && !cas1 && !enh_prog(g, P_CAS)) {
        why = g->enh_why;
        m = NB_UPSCALE_OFF;
    }
    g->enh_used = m;
    g->enh_used_why = m == d->upscale ? NULL : why;

    switch (m) {
    case NB_UPSCALE_FSR:
        if (fsr1 || !rcas) {
            /* One pass straight into the target: RCAS over EASU computed
             * in place, or EASU alone. */
            p = g->pr[fsr1 ? P_FSR1 : P_EASU].id;
            rt_target(g, d);
            f->UseProgram(p);
            tex_bind(g, 0, in, false);
            f->Uniform1i(U(g, p, "u_src"), 0);
            f->Uniform4f(U(g, p, "u_rect"), 0, 0, 1, 1);
            f->Uniform4f(U(g, p, "u_io"), (GLfloat)in->w, (GLfloat)in->h,
                         (GLfloat)dw, (GLfloat)dh);
            f->Uniform2i(U(g, p, "u_org"), d->dx, d->dy);
            f->Uniform2i(U(g, p, "u_max"), (GLint)dw - 1, (GLint)dh - 1);
            f->Uniform1f(U(g, p, "u_stops"), nb_rcas_stops(d->sharp));
            f->Uniform1f(U(g, p, "u_swap"), swap);
            f->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            break;
        }
        /* Two passes: EASU into an output-size texture, then RCAS 1:1
         * into the target. */
        tex_need(g, &g->out_tex, dw, dh, GL_RGBA8);
        rt_tex(g, &g->out_tex);
        p = g->pr[P_EASU].id;
        f->UseProgram(p);
        tex_bind(g, 0, in, false);
        f->Uniform1i(U(g, p, "u_src"), 0);
        f->Uniform4f(U(g, p, "u_rect"), 0, 0, 1, 1);
        f->Uniform4f(U(g, p, "u_io"), (GLfloat)in->w, (GLfloat)in->h,
                     (GLfloat)dw, (GLfloat)dh);
        f->Uniform2i(U(g, p, "u_org"), 0, 0);
        f->Uniform1f(U(g, p, "u_swap"), 0.0f);
        f->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        rt_target(g, d);
        p = g->pr[P_RCAS].id;
        f->UseProgram(p);
        tex_bind(g, 0, &g->out_tex, true);
        f->Uniform1i(U(g, p, "u_src"), 0);
        f->Uniform4f(U(g, p, "u_rect"), 0, 0, 1, 1);
        f->Uniform2i(U(g, p, "u_org"), d->dx, d->dy);
        f->Uniform2i(U(g, p, "u_max"), (GLint)dw - 1, (GLint)dh - 1);
        f->Uniform1f(U(g, p, "u_stops"), nb_rcas_stops(d->sharp));
        f->Uniform1f(U(g, p, "u_swap"), swap);
        f->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        break;
    case NB_UPSCALE_NIS:
        /*
         * NIS (compute, shared-memory tiles) into an output-size image, then
         * 1:1 into the target: a compute shader cannot write the target's
         * renderbuffer.  The input is read once, in the compute pass.
         */
        tex_need(g, &g->out_tex, dw, dh, GL_RGBA8);
        p = g->pr[P_NIS].id;
        f->UseProgram(p);
        nis_uniforms(g, p, &nc);
        tex_bind(g, 0, in, false);
        tex_bind(g, 1, &g->coef[0], true);
        tex_bind(g, 2, &g->coef[1], true);
        f->Uniform1i(U(g, p, "in_texture"), 0);
        f->Uniform1i(U(g, p, "coef_scaler"), 1);
        f->Uniform1i(U(g, p, "coef_usm"), 2);
        f->BindImageTexture(0, g->out_tex.id, 0, 0, 0, GL_WRITE_ONLY,
                            GL_RGBA8);
        f->DispatchCompute((dw + NB_NIS_BLOCK_W - 1) / NB_NIS_BLOCK_W,
                           (dh + NB_NIS_BLOCK_H - 1) / NB_NIS_BLOCK_H, 1);
        f->MemoryBarrier(GL_TEXTURE_FETCH_BARRIER_BIT |
                         GL_FRAMEBUFFER_BARRIER_BIT);
        rt_target(g, d);
        copy_draw(g, &g->out_tex, true, swap);
        break;
    case NB_UPSCALE_CAS:
        if (cas1) {
            /* Sharpen and scale in one pass into the target. */
            p = g->pr[P_CAS_SCALE].id;
            rt_target(g, d);
            f->UseProgram(p);
            tex_bind(g, 0, in, true);
            f->Uniform1i(U(g, p, "u_src"), 0);
            f->Uniform4f(U(g, p, "u_rect"), 0, 0, 1, 1);
            f->Uniform2i(U(g, p, "u_max"), (GLint)in->w - 1,
                         (GLint)in->h - 1);
            f->Uniform2f(U(g, p, "u_in"), (GLfloat)in->w, (GLfloat)in->h);
            f->Uniform2f(U(g, p, "u_out"), (GLfloat)dw, (GLfloat)dh);
            f->Uniform2i(U(g, p, "u_org"), d->dx, d->dy);
            f->Uniform1f(U(g, p, "u_sharp"), nb_sharp_unit(d->sharp));
            f->Uniform1f(U(g, p, "u_swap"), swap);
            f->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            break;
        }
        /* Above 4x the area or drawn smaller: sharpened at the guest's
         * resolution, then the plain scale. */
        tex_need(g, &g->cas_tex, in->w, in->h, GL_RGBA8);
        rt_tex(g, &g->cas_tex);
        p = g->pr[P_CAS].id;
        f->UseProgram(p);
        tex_bind(g, 0, in, true);
        f->Uniform1i(U(g, p, "u_src"), 0);
        f->Uniform4f(U(g, p, "u_rect"), 0, 0, 1, 1);
        f->Uniform2i(U(g, p, "u_max"), (GLint)in->w - 1, (GLint)in->h - 1);
        f->Uniform2f(U(g, p, "u_in"), (GLfloat)in->w, (GLfloat)in->h);
        f->Uniform2f(U(g, p, "u_out"), (GLfloat)in->w, (GLfloat)in->h);
        f->Uniform2i(U(g, p, "u_org"), 0, 0);
        f->Uniform1f(U(g, p, "u_sharp"), nb_sharp_unit(d->sharp));
        f->Uniform1f(U(g, p, "u_swap"), 0.0f);
        f->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        rt_target(g, d);
        copy_draw(g, &g->cas_tex, d->nearest, swap);
        break;
    default:
        rt_target(g, d);
        copy_draw(g, in, d->nearest, swap);
        break;
    }
    return 0;
}

/* ── frame generation: history, pyramid, flow, interpolation ── */

static void dispatch8(struct nb_gs *g, uint32_t w, uint32_t h)
{
    g->f.DispatchCompute((w + 7) / 8, (h + 7) / 8, 1);
    g->f.MemoryBarrier(GL_TEXTURE_FETCH_BARRIER_BIT |
                       GL_SHADER_IMAGE_ACCESS_BARRIER_BIT);
}

/* Luma pyramid of history slot k: level l is 1/2^(l+1) of the frame. */
static void fg_pyramid(struct nb_gs *g, int k)
{
    struct fns *f = &g->f;
    GLuint p = g->pr[P_FG_DOWN].id;
    const struct gs_tex *src = &g->hist[k];

    f->UseProgram(p);
    f->Uniform1i(U(g, p, "u_src"), 0);
    for (int l = 0; l < g->fg_levels; l++) {
        struct gs_tex *dst = &g->lum[k][l];
        uint32_t w = (src->w + 1) / 2, h = (src->h + 1) / 2;

        tex_need(g, dst, w, h, GL_RGBA16F);
        tex_bind(g, 0, src, false);
        f->Uniform2i(U(g, p, "u_size"), (GLint)w, (GLint)h);
        f->Uniform2f(U(g, p, "u_inv_src"), 1.0f / (GLfloat)src->w,
                     1.0f / (GLfloat)src->h);
        f->Uniform1i(U(g, p, "u_luma"), l == 0);
        f->BindImageTexture(0, dst->id, 0, 0, 0, GL_WRITE_ONLY, GL_RGBA16F);
        dispatch8(g, w, h);
        src = dst;
    }
}

/* Flow from history slot k^1 to k, coarsest level first. */
static void fg_flow(struct nb_gs *g, int k)
{
    struct fns *f = &g->f;
    GLuint p = g->pr[P_FG_FLOW].id;

    f->UseProgram(p);
    f->Uniform1i(U(g, p, "u_prev"), 0);
    f->Uniform1i(U(g, p, "u_cur"), 1);
    f->Uniform1i(U(g, p, "u_coarse"), 2);
    for (int l = g->fg_levels - 1; l >= 0; l--) {
        const struct gs_tex *lv = &g->lum[k][l];
        bool refine = l < g->fg_levels - 1;

        tex_need(g, &g->flow[l], lv->w, lv->h, GL_RGBA16F);
        tex_bind(g, 0, &g->lum[k ^ 1][l], false);
        tex_bind(g, 1, lv, false);
        /* unit 2 is read only when refining; otherwise any other texture
         * (not one bound for writing or sampled here) */
        tex_bind(g, 2, refine ? &g->flow[l + 1] : &g->hist[k], true);
        f->Uniform2i(U(g, p, "u_size"), (GLint)lv->w, (GLint)lv->h);
        f->Uniform2i(U(g, p, "u_csize"),
                     refine ? (GLint)g->flow[l + 1].w : 1,
                     refine ? (GLint)g->flow[l + 1].h : 1);
        f->Uniform2f(U(g, p, "u_inv"), 1.0f / (GLfloat)lv->w,
                     1.0f / (GLfloat)lv->h);
        f->Uniform1f(U(g, p, "u_scale"), (GLfloat)(2u << l));
        f->Uniform1i(U(g, p, "u_radius"), 4);
        f->Uniform1i(U(g, p, "u_refine"), refine);
        f->BindImageTexture(0, g->flow[l].id, 0, 0, 0, GL_WRITE_ONLY,
                            GL_RGBA16F);
        dispatch8(g, lv->w, lv->h);
    }
}

void nb_gs_fg_reset(struct nb_gs *g)
{
    if (g) {
        g->fg_n = 0;
    }
}

bool nb_gs_fg_ready(const struct nb_gs *g)
{
    return g && g->fg_n > 0;
}

int nb_gs_fg_push(struct nb_gs *g, int src_key, double sx, double sy,
                  double sw, double sh, unsigned timeout_us, unsigned *gpu_us)
{
    int si = src_index(g, src_key), k;
    uint32_t w = sw >= 1.0 ? (uint32_t)(sw + 0.5) : 1;
    uint32_t h = sh >= 1.0 ? (uint32_t)(sh + 0.5) : 1;
    int r;

    if (si < 0) {
        return -EINVAL;
    }
    gs_cur(g);
    if (!g->es31 || !enh_prog(g, P_COPY) || !enh_prog(g, P_FG_DOWN) ||
        !enh_prog(g, P_FG_FLOW) || !enh_prog(g, P_FG_INTERP)) {
        return -ENOTSUP;
    }
    if (w != g->fg_w || h != g->fg_h) {
        g->fg_n = 0;            /* a new mode or crop: a new history */
        g->fg_w = w;
        g->fg_h = h;
        g->fg_levels = 1;
        for (uint32_t lw = (w + 1) / 2, lh = (h + 1) / 2;
             g->fg_levels < NB_FG_LEVELS && lw >= 32 && lh >= 32;
             lw = (lw + 1) / 2, lh = (lh + 1) / 2) {
            g->fg_levels++;
        }
    }
    k = g->fg_n ? g->fg_cur ^ 1 : 0;
    enh_ingest(g, si, sx, sy, sw, sh, &g->hist[k]);
    fg_pyramid(g, k);
    if (g->fg_n >= 1) {
        fg_flow(g, k);
    }
    g->fg_cur = k;
    g->fg_n = g->fg_n >= 1 ? 2 : 1;
    if (!timeout_us) {
        /* Submitted; the caller's next draw waits for all of it. */
        g->f.Flush();
        if (gpu_us) {
            *gpu_us = 0;
        }
        return g->fg_n == 2;
    }
    r = gs_finish(g, timeout_us, gpu_us);
    if (r < 0) {
        g->fg_n = 0;
        return r;
    }
    return g->fg_n == 2;
}

/* The picture at phase t: a history frame, or the interpolation into mid. */
static const struct gs_tex *fg_pick(struct nb_gs *g, float t)
{
    struct fns *f = &g->f;
    GLuint p = g->pr[P_FG_INTERP].id;
    const struct gs_tex *a = &g->hist[g->fg_cur ^ 1];
    const struct gs_tex *b = &g->hist[g->fg_cur];

    if (g->fg_n < 2 || t >= 0.999f) {
        return b;
    }
    if (t <= 0.001f) {
        return a;
    }
    tex_need(g, &g->mid, g->fg_w, g->fg_h, GL_RGBA8);
    f->UseProgram(p);
    tex_bind(g, 0, a, false);
    tex_bind(g, 1, b, false);
    tex_bind(g, 2, &g->flow[0], true);
    f->Uniform1i(U(g, p, "u_prev"), 0);
    f->Uniform1i(U(g, p, "u_cur"), 1);
    f->Uniform1i(U(g, p, "u_flow"), 2);
    f->Uniform2i(U(g, p, "u_size"), (GLint)g->fg_w, (GLint)g->fg_h);
    f->Uniform2i(U(g, p, "u_fsize"), (GLint)g->flow[0].w,
                 (GLint)g->flow[0].h);
    f->Uniform2f(U(g, p, "u_inv"), 1.0f / (GLfloat)g->fg_w,
                 1.0f / (GLfloat)g->fg_h);
    f->Uniform1f(U(g, p, "u_fscale"), 2.0f);
    f->Uniform1f(U(g, p, "u_t"), t);
    f->BindImageTexture(0, g->mid.id, 0, 0, 0, GL_WRITE_ONLY, GL_RGBA8);
    dispatch8(g, g->fg_w, g->fg_h);
    return &g->mid;
}

/* One enhanced draw (nb_gs_draw): the source into an internal texture (or
 * the generated picture), then the upscaler into the bound target. */
static int enh_frame(struct nb_gs *g, int si, const struct nb_gs_draw *d)
{
    const struct gs_tex *in;
    struct gs_tex direct;

    if (d->src_key == NB_GS_SRC_FG) {
        in = fg_pick(g, d->fg_t);
    } else if (g->s[si].tex2d && d->sx == 0 && d->sy == 0 &&
               d->sw == g->s[si].w && d->sh == g->s[si].h) {
        /* The guest's buffer itself, sampled in place: no copy. */
        direct = (struct gs_tex){ .id = g->s[si].tex2d, .w = g->s[si].w,
                                  .h = g->s[si].h, .fmt = GL_RGBA8 };
        in = &direct;
    } else {
        enh_ingest(g, si, d->sx, d->sy, d->sw, d->sh, &g->in_tex);
        in = &g->in_tex;
    }
    return enh_draw(g, in, d);
}

/* ── benchmark (conduit-viewer --bench-enhance) ────────────────────────────── */

/* A guest-like source: a GBM buffer in the driver's own layout for
 * rendering (what a guest's frame is), vertical stripes offset by `shift`. */
static struct gbm_bo *bench_src(struct nb_gs *g, uint32_t w, uint32_t h,
                                int key, int shift)
{
    struct fns *f = &g->f;
    struct gbm_bo *bo = NULL;
    EGLImage img = EGL_NO_IMAGE;
    GLuint rb, fbo;
    int fd = -1;

    /*
     * First an explicit modifier the GPU samples in place, as a guest
     * frame's is (block-linear on NVIDIA); GBM's own choice when that buffer
     * cannot be made or imported (a multi-plane layout, say).
     */
    for (int attempt = 0; attempt < 2 && !img; attempt++) {
        if (attempt == 0 && f->QueryDmaBufModifiersEXT &&
            f->gbm_bo_create_with_modifiers) {
            uint64_t mods[64], use[64];
            EGLBoolean ext_only[64];
            EGLint k = 0;
            unsigned nu = 0;

            if (f->QueryDmaBufModifiersEXT(g->dpy, (EGLint)FMT_XRGB8888, 64,
                                           mods, ext_only, &k)) {
                for (EGLint i = 0; i < k; i++) {
                    if (!ext_only[i]) {
                        use[nu++] = mods[i];
                    }
                }
            }
            bo = nu ? f->gbm_bo_create_with_modifiers(g->gbm, w, h,
                                                      FMT_XRGB8888, use, nu)
                    : NULL;
        } else if (attempt == 1) {
            bo = f->gbm_bo_create(g->gbm, w, h, FMT_XRGB8888,
                                  GBM_BO_USE_RENDERING);
        }
        if (!bo) {
            continue;
        }
        fd = f->gbm_bo_get_fd(bo);
        img = fd < 0 ? EGL_NO_IMAGE
                     : import(g, fd, w, h, f->gbm_bo_get_stride(bo),
                              f->gbm_bo_get_offset(bo, 0), FMT_XRGB8888,
                              f->gbm_bo_get_modifier(bo));
        if (!img) {
            if (fd >= 0) {
                close(fd);
            }
            f->gbm_bo_destroy(bo);
            bo = NULL;
            fd = -1;
        }
    }
    if (!img) {
        return NULL;
    }
    f->GenRenderbuffers(1, &rb);
    f->BindRenderbuffer(GL_RENDERBUFFER, rb);
    f->EGLImageTargetRenderbufferStorageOES(GL_RENDERBUFFER, img);
    f->GenFramebuffers(1, &fbo);
    f->BindFramebuffer(GL_FRAMEBUFFER, fbo);
    f->FramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_RENDERBUFFER, rb);
    f->Enable(GL_SCISSOR_TEST);
    for (uint32_t x = 0, i = 0; x < w; x += 48, i++) {
        f->Scissor((GLint)x + shift, 0, 48, (GLsizei)h);
        f->ClearColor((float)(i % 3) * 0.4f, (float)(i % 5) * 0.2f,
                      (float)(i % 7) * 0.15f, 1);
        f->Clear(GL_COLOR_BUFFER_BIT);
    }
    f->Disable(GL_SCISSOR_TEST);
    gs_finish(g, 1000000, NULL);
    f->DeleteFramebuffers(1, &fbo);
    f->DeleteRenderbuffers(1, &rb);
    f->DestroyImageKHR(g->dpy, img);
    nb_gs_src(g, key, fd, w, h, f->gbm_bo_get_stride(bo),
              f->gbm_bo_get_offset(bo, 0), FMT_XRGB8888,
              f->gbm_bo_get_modifier(bo));
    close(fd);
    return bo;
}

static uint64_t bench_med(uint64_t *v, int n)
{
    for (int i = 1; i < n; i++) {
        uint64_t k = v[i];
        int j = i - 1;

        while (j >= 0 && v[j] > k) {
            v[j + 1] = v[j];
            j--;
        }
        v[j + 1] = k;
    }
    return n ? v[n / 2] : 0;
}

struct bench_case {
    const char *name;
    int  kind;                  /* 0 draw, 1 frame-generation push */
    int  upscale, sharp;
    bool one_pass, copy_first, fg;
    float t;
};

int nb_gs_bench(const char *node, uint32_t iw, uint32_t ih, uint32_t ow,
                uint32_t oh, int iters, void *out_file)
{
    static const struct bench_case cases[] = {
        { "plain (bilinear, as without upscaling)", 0, NB_UPSCALE_OFF, 0,
          false, false, false, 0 },
        { "FSR 1 (EASU, RCAS)", 0, NB_UPSCALE_FSR, 50, false, false, false,
          0 },
        { "FSR 1, after a copy (no in-place read)", 0, NB_UPSCALE_FSR, 50,
          false, true, false, 0 },
        { "FSR 1, one pass (FSR_PASSES=1)", 0, NB_UPSCALE_FSR, 50, true,
          false, false, 0 },
        { "FSR 1 sharpness 0 (EASU only)", 0, NB_UPSCALE_FSR, 0, false, false,
          false, 0 },
        { "NIS (compute + 1:1 copy)", 0, NB_UPSCALE_NIS, 50, false, false,
          false, 0 },
        { "CAS", 0, NB_UPSCALE_CAS, 50, false, false, false, 0 },
        { "frame gen: push (copy, pyramid, flow)", 1, 0, 0, false, false,
          true, 0 },
        { "frame gen: picture t=1/2, plain", 0, NB_UPSCALE_OFF, 0, false,
          false, true, 0.5f },
        { "frame gen: picture t=1/2, FSR 1", 0, NB_UPSCALE_FSR, 50, false,
          false, true, 0.5f },
        { "frame gen: newest frame, FSR 1", 0, NB_UPSCALE_FSR, 50, false,
          false, true, 1.0f },
    };
    FILE *out = out_file ? out_file : stdout;
    struct nb_gs_target tg[NB_GS_TARGETS];
    struct gbm_bo *src[2] = { NULL, NULL };
    char why[256] = "";
    struct nb_gs *g;
    uint64_t *gv, *wv;
    GLuint q = 0;
    double k;
    int dw, dh, rc = 0;

    if (iters < 5) {
        iters = 5;
    }
    g = nb_gs_open(node, 0, 0, 0, why, sizeof(why));
    gs_cur(g);
    if (!g) {
        fprintf(out, "bench: no GPU pass: %s\n", why);
        return -1;
    }
    if (!g->es31) {
        fprintf(out, "bench: %s: %s\n", g->node, g->enh_why);
        nb_gs_close(g);
        return -1;
    }
    gv = calloc((size_t)iters, sizeof(*gv));
    wv = calloc((size_t)iters, sizeof(*wv));
    src[0] = bench_src(g, iw, ih, 0, 0);
    src[1] = bench_src(g, iw, ih, 1, 6);
    if (!gv || !wv || !src[0] || !src[1] ||
        nb_gs_targets(g, ow, oh, NULL, NULL, NULL, NULL, 0, tg, why,
                      sizeof(why)) != NB_GS_TARGETS) {
        fprintf(out, "bench: cannot make the buffers: %s\n", why);
        rc = -1;
        goto out;
    }
    if (g->f.GenQueriesEXT && g->f.BeginQueryEXT && g->f.EndQueryEXT &&
        g->f.GetQueryObjectui64vEXT) {
        g->f.GenQueriesEXT(1, &q);
    }
    /* The picture fitted into the output, letterboxed, as the viewer does. */
    k = (double)ow / iw < (double)oh / ih ? (double)ow / iw : (double)oh / ih;
    dw = (int)(iw * k + 0.5);
    dh = (int)(ih * k + 0.5);
    fprintf(out, "bench: %s, GPU context priority %s requested, %s granted; "
            "%ux%u guest frame (modifier 0x%016llx, %s) drawn %dx%d in a "
            "%ux%u output, median of %d\n", g->node,
            nb_gs_prio_name(g->prio_req), nb_gs_prio_name(g->prio_got), iw,
            ih, (unsigned long long)g->f.gbm_bo_get_modifier(src[0]),
            g->s[0].tex2d ? "sampled in place" : "external only: copied",
            dw, dh, ow, oh, iters);
    fprintf(out, "bench: %-40s %10s %10s\n", "pass", "GPU us",
            "wall us");
    nb_gs_fg_reset(g);
    nb_gs_fg_push(g, 0, 0, 0, iw, ih, 1000000, NULL);
    nb_gs_fg_push(g, 1, 0, 0, iw, ih, 1000000, NULL);
    for (size_t c = 0; c < sizeof(cases) / sizeof(cases[0]); c++) {
        const struct bench_case *bc = &cases[c];
        GLuint keep = g->s[0].tex2d;
        bool one = g->fsr_one_pass;
        int n = 0;

        g->fsr_one_pass = bc->one_pass;
        if (bc->copy_first) {
            g->s[0].tex2d = 0;
        }
        for (int i = 0; i < iters + 3; i++) {
            struct nb_gs_draw d = {
                .src_key = bc->fg ? NB_GS_SRC_FG : 0,
                .target = i % NB_GS_TARGETS,
                .dx = ((int)ow - dw) / 2, .dy = ((int)oh - dh) / 2,
                .dw = dw, .dh = dh, .sw = iw, .sh = ih,
                .upscale = bc->upscale, .sharp = bc->sharp, .fg_t = bc->t,
            };
            uint64_t t0 = now_ns(), ns = 0;
            int r;

            if (q) {
                g->f.BeginQueryEXT(GL_TIME_ELAPSED_EXT, q);
                g->bench_q = true;
            }
            r = bc->kind == 1
                    ? nb_gs_fg_push(g, i & 1, 0, 0, iw, ih, 1000000, NULL)
                    : nb_gs_draw(g, &d, 1000000, NULL);
            g->bench_q = false;
            if (q) {
                g->f.GetQueryObjectui64vEXT(q, GL_QUERY_RESULT_EXT, &ns);
            }
            if (r < 0) {
                fprintf(out, "bench: %s failed (%d)\n", bc->name, r);
                break;
            }
            if (i >= 3) {           /* the first draws compile shaders */
                gv[n] = ns;
                wv[n] = now_ns() - t0;
                n++;
            }
        }
        g->s[0].tex2d = keep;
        g->fsr_one_pass = one;
        if (n) {
            uint64_t gm = bench_med(gv, n), wm = bench_med(wv, n);

            if (q) {
                fprintf(out, "bench: %-40s %10.1f %10.1f\n", bc->name,
                        gm / 1000.0, wm / 1000.0);
            } else {
                fprintf(out, "bench: %-40s %10s %10.1f\n", bc->name, "n/a",
                        wm / 1000.0);
            }
        }
    }
    if (!q) {
        fprintf(out, "bench: no EXT_disjoint_timer_query: wall time only "
                "(submission to fence, GPU time plus overhead)\n");
    }
out:
    if (q) {
        g->f.DeleteQueriesEXT(1, &q);
    }
    for (int i = 0; i < 2; i++) {
        nb_gs_src_drop(g, i);
        if (src[i]) {
            g->f.gbm_bo_destroy(src[i]);
        }
    }
    free(gv);
    free(wv);
    nb_gs_close(g);
    return rc;
}
