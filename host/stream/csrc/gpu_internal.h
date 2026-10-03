/* SPDX-License-Identifier: Apache-2.0 */
#ifndef CS_GPU_INTERNAL_H
#define CS_GPU_INTERNAL_H

#include <EGL/egl.h>
#include <EGL/eglext.h>
#define GL_GLEXT_PROTOTYPES 1
#include <GL/glcorearb.h>

/* nv-codec-headers: run-time loader for libcuda / libnvidia-encode / libnvcuvid. */
#define FFNV_LOG_FUNC(logctx, msg, ...) ((void)0)
#define FFNV_DEBUG_LOG_FUNC(logctx, msg, ...) ((void)0)
#include <ffnvcodec/dynlink_loader.h>

#include "gpu.h"

#define CS_FOURCC(a, b, c, d) \
    ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
#define CS_FOURCC_XR24 CS_FOURCC('X', 'R', '2', '4')
#define CS_FOURCC_AR24 CS_FOURCC('A', 'R', '2', '4')
#define CS_FOURCC_XB24 CS_FOURCC('X', 'B', '2', '4')
#define CS_FOURCC_AB24 CS_FOURCC('A', 'B', '2', '4')
#define CS_MOD_INVALID 0x00ffffffffffffffull
#define CS_MOD_LINEAR 0ull

#define CS_IMG_SLOTS 8

typedef void (*cs_image_target_fn)(GLenum, void *);

#define GL_FUNCS(X) \
    X(glGetError) X(glGenTextures) X(glDeleteTextures) X(glBindTexture) X(glTexParameteri) \
    X(glTexImage2D) X(glTexSubImage2D) X(glActiveTexture) X(glGenFramebuffers) \
    X(glDeleteFramebuffers) X(glBindFramebuffer) X(glFramebufferTexture2D) \
    X(glCheckFramebufferStatus) X(glDrawBuffers) X(glViewport) X(glCreateShader) \
    X(glShaderSource) X(glCompileShader) X(glGetShaderiv) X(glGetShaderInfoLog) \
    X(glDeleteShader) X(glCreateProgram) X(glAttachShader) X(glLinkProgram) \
    X(glGetProgramiv) X(glGetProgramInfoLog) X(glUseProgram) X(glGetUniformLocation) \
    X(glUniform1i) X(glUniform2f) X(glUniform3f) X(glUniform4f) X(glGenVertexArrays) \
    X(glBindVertexArray) X(glDrawArrays) X(glFinish) X(glFlush) X(glReadPixels) \
    X(glPixelStorei) X(glReadBuffer)

struct gl_fns {
    __typeof__(&glGetError) glGetError;
    __typeof__(&glGenTextures) glGenTextures;
    __typeof__(&glDeleteTextures) glDeleteTextures;
    __typeof__(&glBindTexture) glBindTexture;
    __typeof__(&glTexParameteri) glTexParameteri;
    __typeof__(&glTexImage2D) glTexImage2D;
    __typeof__(&glTexSubImage2D) glTexSubImage2D;
    __typeof__(&glActiveTexture) glActiveTexture;
    __typeof__(&glGenFramebuffers) glGenFramebuffers;
    __typeof__(&glDeleteFramebuffers) glDeleteFramebuffers;
    __typeof__(&glBindFramebuffer) glBindFramebuffer;
    __typeof__(&glFramebufferTexture2D) glFramebufferTexture2D;
    __typeof__(&glCheckFramebufferStatus) glCheckFramebufferStatus;
    __typeof__(&glDrawBuffers) glDrawBuffers;
    __typeof__(&glViewport) glViewport;
    __typeof__(&glCreateShader) glCreateShader;
    __typeof__(&glShaderSource) glShaderSource;
    __typeof__(&glCompileShader) glCompileShader;
    __typeof__(&glGetShaderiv) glGetShaderiv;
    __typeof__(&glGetShaderInfoLog) glGetShaderInfoLog;
    __typeof__(&glDeleteShader) glDeleteShader;
    __typeof__(&glCreateProgram) glCreateProgram;
    __typeof__(&glAttachShader) glAttachShader;
    __typeof__(&glLinkProgram) glLinkProgram;
    __typeof__(&glGetProgramiv) glGetProgramiv;
    __typeof__(&glGetProgramInfoLog) glGetProgramInfoLog;
    __typeof__(&glUseProgram) glUseProgram;
    __typeof__(&glGetUniformLocation) glGetUniformLocation;
    __typeof__(&glUniform1i) glUniform1i;
    __typeof__(&glUniform2f) glUniform2f;
    __typeof__(&glUniform3f) glUniform3f;
    __typeof__(&glUniform4f) glUniform4f;
    __typeof__(&glGenVertexArrays) glGenVertexArrays;
    __typeof__(&glBindVertexArray) glBindVertexArray;
    __typeof__(&glDrawArrays) glDrawArrays;
    __typeof__(&glFinish) glFinish;
    __typeof__(&glFlush) glFlush;
    __typeof__(&glReadPixels) glReadPixels;
    __typeof__(&glPixelStorei) glPixelStorei;
    __typeof__(&glReadBuffer) glReadBuffer;
    cs_image_target_fn EGLImageTargetTexture2DOES;
};

struct cs_img {
    uint64_t id;
    EGLImageKHR img;
    GLuint tex;
    uint32_t w, h, stride, offset, fourcc;
    uint64_t modifier;
    uint64_t used;
};

struct cs_gpu {
    EGLDisplay dpy;
    EGLContext ctx;
    int has_modifiers;
    PFNEGLCREATEIMAGEKHRPROC createImage;
    PFNEGLDESTROYIMAGEKHRPROC destroyImage;
    PFNEGLQUERYDMABUFMODIFIERSEXTPROC queryModifiers;
    struct gl_fns gl;
    GLuint vao, fbo, prog_enc, prog_dec;

    struct cs_img img[CS_IMG_SLOTS];
    int cur_img;
    uint64_t tick;
    struct cs_img cursor;
    int cursor_visible;
    uint32_t cursor_hot_x, cursor_hot_y;

    CudaFunctions *cu;
    NvencFunctions *nv;
    CuvidFunctions *cv;
    CUdevice cudev;
    CUcontext cuctx;
    NV_ENCODE_API_FUNCTION_LIST api;
    int nvenc_ver_ok;
    uint32_t nvenc_driver_ver;
};

void cs_err(char *err, size_t errlen, const char *fmt, ...) __attribute__((format(printf, 3, 4)));
uint64_t cs_now_us(void);
const char *cs_cu_err(cs_gpu *g, CUresult r);
GLuint cs_gl_program(cs_gpu *g, const char *vs, const char *fs, char *err, size_t errlen);
GLuint cs_plane_tex(cs_gpu *g, GLenum ifmt, GLenum fmt, uint32_t w, uint32_t h);
EGLImageKHR cs_import(cs_gpu *g, int fd, uint32_t w, uint32_t h, uint32_t stride,
                      uint32_t offset, uint32_t fourcc, uint64_t modifier);
void cs_csc(uint32_t colorspace, uint32_t full_range, float ky[3], float kcb[3], float kcr[3],
            float off[3]);
void cs_convert(cs_gpu *g, const GLuint *tex, int n, uint32_t w, uint32_t h, int mode,
                uint32_t out_w, uint32_t out_h, uint32_t colorspace, uint32_t full_range,
                const struct cs_frame_opts *o);

#endif
