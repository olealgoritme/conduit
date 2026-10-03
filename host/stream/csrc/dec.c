/* SPDX-License-Identifier: Apache-2.0 */
/*
 * dec.c -- NVDEC decode for the conduit link's client side.
 *
 *   bitstream ──cuvid parser──► NVDEC ──map──► CUDA planes ──cuMemcpy2D──► GL plane textures
 *                                                                             │ one draw: YUV (or G/B/R) → RGB
 *                                                                             ▼
 *                                          GBM buffer (dma-buf, a modifier the viewer accepts) ──► viewer
 *
 * Output buffers rotate (CS_DEC_OUT of them); the caller hands each one to the
 * viewer and must not get a buffer back while the viewer still reads it, which
 * it ensures by tracking the viewer's RELEASE events (see link.rs).
 */
#define _GNU_SOURCE
#include "gpu_internal.h"

#include <fcntl.h>
#include <stdio.h>
#include <gbm.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define CS_DEC_OUT 4

struct out_buf {
    struct gbm_bo *bo;
    int fd;
    uint64_t id;
    uint32_t stride, offset;
    uint64_t modifier;
    EGLImageKHR img;
    GLuint tex, fbo;
};

struct cs_dec {
    cs_gpu *g;
    struct cs_dec_params p;
    int drm_fd;
    struct gbm_device *gbm;
    uint64_t mods[32];
    size_t nmods;
    CUvideoparser parser;
    CUvideodecoder dec;
    cudaVideoSurfaceFormat fmt;
    uint32_t w, h;          /* display size */
    GLuint plane[3];
    CUgraphicsResource res[3];
    int nplanes;
    struct out_buf out[CS_DEC_OUT];
    int next_out;
    int have_frame;
    struct cs_dec_frame frame;
    char err[256];
    int failed;
    GLuint prog;
};

static const char *VS =
    "#version 330 core\n"
    "void main() {\n"
    "  vec2 p = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);\n"
    "  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n"
    "}\n";

/* Rows in memory order, as everywhere in this module (row 0 = top). */
static const char *FS_DEC =
    "#version 330 core\n"
    "uniform sampler2D u_y, u_u, u_v;\n"
    "uniform int u_layout;\n"       /* 0 NV12 (u = RG), 1 planar 4:4:4, 2 G/B/R */
    "uniform vec3 u_off;\n"         /* y, cb, cr offsets */
    "uniform vec2 u_scale;\n"       /* y, c scale */
    "uniform vec3 u_k;\n"           /* kr, kg, kb */
    "out vec4 o;\n"
    "void main() {\n"
    "  ivec2 d = ivec2(gl_FragCoord.xy);\n"
    "  float y = texelFetch(u_y, d, 0).r;\n"
    "  if (u_layout == 2) {\n"
    "    o = vec4(texelFetch(u_v, d, 0).r, y, texelFetch(u_u, d, 0).r, 1.0);\n"
    "    return;\n"
    "  }\n"
    "  vec2 c = u_layout == 0 ? texelFetch(u_u, d / 2, 0).rg\n"
    "                         : vec2(texelFetch(u_u, d, 0).r, texelFetch(u_v, d, 0).r);\n"
    "  float Y = (y - u_off.x) / u_scale.x;\n"
    "  float cb = (c.x - u_off.y) / u_scale.y, cr = (c.y - u_off.z) / u_scale.y;\n"
    "  float r = Y + 2.0 * (1.0 - u_k.x) * cr;\n"
    "  float b = Y + 2.0 * (1.0 - u_k.z) * cb;\n"
    "  float g = (Y - u_k.x * r - u_k.z * b) / u_k.y;\n"
    "  o = vec4(clamp(vec3(r, g, b), 0.0, 1.0), 1.0);\n"
    "}\n";

static int out_alloc(cs_dec *d, char *err, size_t errlen)
{
    cs_gpu *g = d->g;

    for (int i = 0; i < CS_DEC_OUT; i++) {
        struct out_buf *o = &d->out[i];
        struct stat st;

        if (d->nmods)
            o->bo = gbm_bo_create_with_modifiers2(d->gbm, d->w, d->h, GBM_FORMAT_XRGB8888,
                                                  d->mods, (unsigned)d->nmods,
                                                  GBM_BO_USE_RENDERING);
        if (!o->bo)
            o->bo = gbm_bo_create(d->gbm, d->w, d->h, GBM_FORMAT_XRGB8888,
                                  GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
        if (!o->bo) {
            cs_err(err, errlen, "cannot allocate a %ux%u output buffer", d->w, d->h);
            return -1;
        }
        o->fd = gbm_bo_get_fd(o->bo);
        o->stride = gbm_bo_get_stride(o->bo);
        o->offset = gbm_bo_get_offset(o->bo, 0);
        o->modifier = gbm_bo_get_modifier(o->bo);
        o->id = fstat(o->fd, &st) == 0 ? (uint64_t)st.st_ino : (uint64_t)i + 1;
        o->img = cs_import(g, o->fd, d->w, d->h, o->stride, o->offset, CS_FOURCC_XR24,
                           o->modifier);
        if (o->img == EGL_NO_IMAGE_KHR) {
            cs_err(err, errlen, "EGL cannot render into the output buffer (0x%x)", eglGetError());
            return -1;
        }
        g->gl.glGenTextures(1, &o->tex);
        g->gl.glBindTexture(GL_TEXTURE_2D, o->tex);
        g->gl.EGLImageTargetTexture2DOES(GL_TEXTURE_2D, o->img);
        g->gl.glGenFramebuffers(1, &o->fbo);
        g->gl.glBindFramebuffer(GL_FRAMEBUFFER, o->fbo);
        g->gl.glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, o->tex, 0);
        if (g->gl.glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
            cs_err(err, errlen, "output buffer is not renderable");
            return -1;
        }
        g->gl.glBindFramebuffer(GL_FRAMEBUFFER, 0);
    }
    return 0;
}

static void out_free(cs_dec *d)
{
    cs_gpu *g = d->g;

    for (int i = 0; i < CS_DEC_OUT; i++) {
        struct out_buf *o = &d->out[i];
        if (o->fbo)
            g->gl.glDeleteFramebuffers(1, &o->fbo);
        if (o->tex)
            g->gl.glDeleteTextures(1, &o->tex);
        if (o->img)
            g->destroyImage(g->dpy, o->img);
        if (o->fd > 0)
            close(o->fd);
        if (o->bo)
            gbm_bo_destroy(o->bo);
        memset(o, 0, sizeof(*o));
    }
}

static void planes_free(cs_dec *d)
{
    for (int i = 0; i < d->nplanes; i++) {
        if (d->res[i])
            d->g->cu->cuGraphicsUnregisterResource(d->res[i]);
        d->res[i] = NULL;
    }
    if (d->nplanes)
        d->g->gl.glDeleteTextures(d->nplanes, d->plane);
    d->nplanes = 0;
}

static int CUDAAPI on_sequence(void *ud, CUVIDEOFORMAT *f)
{
    cs_dec *d = ud;
    cs_gpu *g = d->g;
    CUVIDDECODECREATEINFO ci;
    unsigned w = (unsigned)(f->display_area.right - f->display_area.left);
    unsigned h = (unsigned)(f->display_area.bottom - f->display_area.top);
    int n = f->min_num_decode_surfaces + 2;
    CUresult r;

    if (f->bit_depth_luma_minus8) {
        snprintf(d->err, sizeof(d->err), "10-bit streams are not supported");
        d->failed = 1;
        return 0;
    }
    if (d->dec) {
        if (w == d->w && h == d->h)
            return n;
        g->cv->cuvidDestroyDecoder(d->dec);
        d->dec = NULL;
    }
    memset(&ci, 0, sizeof(ci));
    ci.CodecType = f->codec;
    ci.ChromaFormat = f->chroma_format;
    ci.OutputFormat = f->chroma_format == cudaVideoChromaFormat_444 ? cudaVideoSurfaceFormat_YUV444
                                                                    : cudaVideoSurfaceFormat_NV12;
    ci.ulWidth = f->coded_width;
    ci.ulHeight = f->coded_height;
    ci.ulMaxWidth = f->coded_width;
    ci.ulMaxHeight = f->coded_height;
    ci.ulTargetWidth = w;
    ci.ulTargetHeight = h;
    ci.display_area.left = (short)f->display_area.left;
    ci.display_area.top = (short)f->display_area.top;
    ci.display_area.right = (short)f->display_area.right;
    ci.display_area.bottom = (short)f->display_area.bottom;
    ci.ulNumDecodeSurfaces = (tcu_ulong)n;
    ci.ulNumOutputSurfaces = 2;
    ci.DeinterlaceMode = cudaVideoDeinterlaceMode_Weave;
    ci.ulCreationFlags = cudaVideoCreate_PreferCUVID;
    r = g->cv->cuvidCreateDecoder(&d->dec, &ci);
    if (r != CUDA_SUCCESS) {
        snprintf(d->err, sizeof(d->err), "cuvidCreateDecoder %ux%u: %s", w, h, cs_cu_err(g, r));
        d->failed = 1;
        return 0;
    }
    d->fmt = ci.OutputFormat;

    if (w != d->w || h != d->h || !d->nplanes) {
        char e[256];
        planes_free(d);
        out_free(d);
        d->w = w;
        d->h = h;
        if (d->fmt == cudaVideoSurfaceFormat_NV12) {
            d->plane[0] = cs_plane_tex(g, GL_R8, GL_RED, w, h);
            d->plane[1] = cs_plane_tex(g, GL_RG8, GL_RG, w / 2, h / 2);
            d->nplanes = 2;
        } else {
            for (int i = 0; i < 3; i++)
                d->plane[i] = cs_plane_tex(g, GL_R8, GL_RED, w, h);
            d->nplanes = 3;
        }
        for (int i = 0; i < d->nplanes; i++) {
            r = g->cu->cuGraphicsGLRegisterImage(&d->res[i], d->plane[i], GL_TEXTURE_2D,
                                                 CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD);
            if (r != CUDA_SUCCESS) {
                snprintf(d->err, sizeof(d->err), "register plane: %s", cs_cu_err(g, r));
                d->failed = 1;
                return 0;
            }
        }
        if (out_alloc(d, e, sizeof(e))) {
            snprintf(d->err, sizeof(d->err), "%s", e);
            d->failed = 1;
            return 0;
        }
    }
    return n;
}

static int CUDAAPI on_decode(void *ud, CUVIDPICPARAMS *pp)
{
    cs_dec *d = ud;
    CUresult r = d->g->cv->cuvidDecodePicture(d->dec, pp);

    if (r != CUDA_SUCCESS) {
        snprintf(d->err, sizeof(d->err), "decode: %s", cs_cu_err(d->g, r));
        d->failed = 1;
        return 0;
    }
    return 1;
}

static void render(cs_dec *d, struct out_buf *o)
{
    cs_gpu *g = d->g;
    struct gl_fns *gl = &g->gl;
    float ky[3], kcb[3], kcr[3], off[3];
    double kr, kb;
    int layout = d->p.lossless ? 2 : d->fmt == cudaVideoSurfaceFormat_NV12 ? 0 : 1;

    cs_csc(d->p.colorspace, d->p.full_range, ky, kcb, kcr, off);
    switch (d->p.colorspace) {
    case 1: kr = 0.2126; kb = 0.0722; break;
    case 2: kr = 0.2627; kb = 0.0593; break;
    default: kr = 0.299; kb = 0.114; break;
    }
    gl->glBindFramebuffer(GL_FRAMEBUFFER, o->fbo);
    {
        GLenum b = GL_COLOR_ATTACHMENT0;
        gl->glDrawBuffers(1, &b);
    }
    gl->glViewport(0, 0, (GLsizei)d->w, (GLsizei)d->h);
    gl->glUseProgram(d->prog);
    gl->glBindVertexArray(g->vao);
    for (int i = 0; i < 3; i++) {
        gl->glActiveTexture(GL_TEXTURE0 + (GLenum)i);
        gl->glBindTexture(GL_TEXTURE_2D, d->plane[i < d->nplanes ? i : 0]);
    }
    gl->glUniform1i(gl->glGetUniformLocation(d->prog, "u_y"), 0);
    gl->glUniform1i(gl->glGetUniformLocation(d->prog, "u_u"), 1);
    gl->glUniform1i(gl->glGetUniformLocation(d->prog, "u_v"), 2);
    gl->glUniform1i(gl->glGetUniformLocation(d->prog, "u_layout"), layout);
    gl->glUniform3f(gl->glGetUniformLocation(d->prog, "u_off"), off[0], off[1], off[2]);
    gl->glUniform2f(gl->glGetUniformLocation(d->prog, "u_scale"),
                    d->p.full_range ? 1.0f : 219.0f / 255.0f,
                    d->p.full_range ? 1.0f : 224.0f / 255.0f);
    gl->glUniform3f(gl->glGetUniformLocation(d->prog, "u_k"), (float)kr, (float)(1 - kr - kb),
                    (float)kb);
    gl->glDrawArrays(GL_TRIANGLES, 0, 3);
    gl->glActiveTexture(GL_TEXTURE0);
    gl->glBindFramebuffer(GL_FRAMEBUFFER, 0);
    /* The viewer imports the buffer as soon as we return: it must be finished. */
    gl->glFinish();
}

static int CUDAAPI on_display(void *ud, CUVIDPARSERDISPINFO *di)
{
    cs_dec *d = ud;
    cs_gpu *g = d->g;
    CUVIDPROCPARAMS vp;
    unsigned long long dptr = 0;
    unsigned int pitch = 0;
    CUresult r;
    struct out_buf *o;

    if (!di || d->failed)
        return 1;
    memset(&vp, 0, sizeof(vp));
    vp.progressive_frame = di->progressive_frame;
    vp.top_field_first = di->top_field_first;
    r = g->cv->cuvidMapVideoFrame(d->dec, di->picture_index, &dptr, &pitch, &vp);
    if (r != CUDA_SUCCESS) {
        snprintf(d->err, sizeof(d->err), "map frame: %s", cs_cu_err(g, r));
        d->failed = 1;
        return 0;
    }
    r = g->cu->cuGraphicsMapResources((unsigned)d->nplanes, d->res, 0);
    if (r == CUDA_SUCCESS) {
        for (int i = 0; i < d->nplanes && r == CUDA_SUCCESS; i++) {
            CUarray arr;
            CUDA_MEMCPY2D m;
            /* NVDEC surfaces: luma rows, then chroma, at the decoder's (aligned) height. */
            size_t plane_rows = (size_t)((d->h + 1) & ~1u);

            r = g->cu->cuGraphicsSubResourceGetMappedArray(&arr, d->res[i], 0, 0);
            if (r != CUDA_SUCCESS)
                break;
            memset(&m, 0, sizeof(m));
            m.srcMemoryType = CU_MEMORYTYPE_DEVICE;
            m.srcDevice = (CUdeviceptr)(dptr + (unsigned long long)pitch * plane_rows * (unsigned)i);
            m.srcPitch = pitch;
            m.dstMemoryType = CU_MEMORYTYPE_ARRAY;
            m.dstArray = arr;
            m.WidthInBytes = d->w;
            m.Height = (d->fmt == cudaVideoSurfaceFormat_NV12 && i == 1) ? d->h / 2 : d->h;
            r = g->cu->cuMemcpy2DAsync(&m, 0);
        }
        if (r == CUDA_SUCCESS)
            r = g->cu->cuStreamSynchronize(0);
        g->cu->cuGraphicsUnmapResources((unsigned)d->nplanes, d->res, 0);
    }
    g->cv->cuvidUnmapVideoFrame(d->dec, dptr);
    if (r != CUDA_SUCCESS) {
        snprintf(d->err, sizeof(d->err), "plane copy: %s", cs_cu_err(g, r));
        d->failed = 1;
        return 0;
    }
    o = &d->out[d->next_out];
    d->next_out = (d->next_out + 1) % CS_DEC_OUT;
    render(d, o);
    d->frame.fd = o->fd;
    d->frame.id = o->id;
    d->frame.width = d->w;
    d->frame.height = d->h;
    d->frame.stride = o->stride;
    d->frame.offset = o->offset;
    d->frame.fourcc = CS_FOURCC_XR24;
    d->frame.modifier = o->modifier;
    d->have_frame = 1;
    return 1;
}

cs_dec *cs_dec_open(cs_gpu *g, const struct cs_dec_params *p, const uint64_t *modifiers,
                    size_t n, char *err, size_t errlen)
{
    cs_dec *d;
    CUVIDPARSERPARAMS pp;
    CUcontext dummy;
    CUresult r;

    if (!g->cv) {
        cs_err(err, errlen, "NVDEC is not available (libnvcuvid.so.1 not found)");
        return NULL;
    }
    d = calloc(1, sizeof(*d));
    if (!d)
        return NULL;
    d->g = g;
    d->p = *p;
    for (size_t i = 0; i < n && i < 32; i++)
        d->mods[d->nmods++] = modifiers[i];
    d->drm_fd = open(getenv("CONDUIT_RENDER_NODE") ? getenv("CONDUIT_RENDER_NODE")
                                                    : "/dev/dri/renderD128",
                     O_RDWR | O_CLOEXEC);
    if (d->drm_fd < 0 || !(d->gbm = gbm_create_device(d->drm_fd))) {
        cs_err(err, errlen, "cannot open the GPU's render node for buffers");
        goto fail;
    }
    eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx);
    d->prog = cs_gl_program(g, VS, FS_DEC, err, errlen);
    if (!d->prog)
        goto fail;
    memset(&pp, 0, sizeof(pp));
    pp.CodecType = p->codec == CS_CODEC_AV1 ? cudaVideoCodec_AV1
                 : p->codec == CS_CODEC_HEVC ? cudaVideoCodec_HEVC
                                             : cudaVideoCodec_H264;
    pp.ulMaxNumDecodeSurfaces = 8;
    pp.ulMaxDisplayDelay = 0; /* every frame out as soon as it is decoded */
    pp.pUserData = d;
    pp.pfnSequenceCallback = on_sequence;
    pp.pfnDecodePicture = on_decode;
    pp.pfnDisplayPicture = on_display;
    g->cu->cuCtxPushCurrent(g->cuctx);
    r = g->cv->cuvidCreateVideoParser(&d->parser, &pp);
    g->cu->cuCtxPopCurrent(&dummy);
    if (r != CUDA_SUCCESS) {
        cs_err(err, errlen, "cuvidCreateVideoParser: %s", cs_cu_err(g, r));
        goto fail;
    }
    return d;
fail:
    cs_dec_close(d);
    return NULL;
}

int cs_dec_decode(cs_dec *d, const uint8_t *data, size_t len, struct cs_dec_frame *out,
                  char *err, size_t errlen)
{
    cs_gpu *g = d->g;
    CUVIDSOURCEDATAPACKET pk;
    CUcontext dummy;
    CUresult r;

    eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx);
    memset(&pk, 0, sizeof(pk));
    pk.payload = data;
    pk.payload_size = (tcu_ulong)len;
    pk.flags = CUVID_PKT_ENDOFPICTURE;
    d->have_frame = 0;
    g->cu->cuCtxPushCurrent(g->cuctx);
    r = g->cv->cuvidParseVideoData(d->parser, &pk);
    g->cu->cuCtxPopCurrent(&dummy);
    if (r != CUDA_SUCCESS || d->failed) {
        cs_err(err, errlen, "%s", d->failed ? d->err : cs_cu_err(g, r));
        d->failed = 0;
        return -1;
    }
    if (!d->have_frame)
        return 0;
    *out = d->frame;
    return 1;
}

void cs_dec_close(cs_dec *d)
{
    cs_gpu *g;
    CUcontext dummy;

    if (!d)
        return;
    g = d->g;
    eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx);
    g->cu->cuCtxPushCurrent(g->cuctx);
    if (d->parser)
        g->cv->cuvidDestroyVideoParser(d->parser);
    if (d->dec)
        g->cv->cuvidDestroyDecoder(d->dec);
    planes_free(d);
    g->cu->cuCtxPopCurrent(&dummy);
    out_free(d);
    if (d->gbm)
        gbm_device_destroy(d->gbm);
    if (d->drm_fd > 0)
        close(d->drm_fd);
    free(d);
}
