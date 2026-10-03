/* SPDX-License-Identifier: Apache-2.0 */
/*
 * enc.c -- NVENC session fed from CUDA device memory.
 *
 * Per frame: one GL draw into the plane textures (gpu.c cs_convert), map them
 * in CUDA, device-to-device copy into the registered NVENC input surface,
 * encode synchronously, copy the bitstream out. The input surface keeps the
 * last picture, so a repeat or an IDR on request re-encodes it without the
 * guest's buffer (which the guest may already be drawing into again).
 */
#define _GNU_SOURCE
#include "gpu_internal.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum { LAYOUT_NV12, LAYOUT_444, LAYOUT_GBR };

struct cs_enc {
    cs_gpu *g;
    struct cs_enc_params p;
    int layout;
    GLuint tex[3];
    int ntex;
    CUgraphicsResource res[3];
    CUdeviceptr in;
    size_t pitch;
    void *enc;
    NV_ENC_REGISTERED_PTR reg;
    NV_ENC_OUTPUT_PTR bs;
    NV_ENC_BUFFER_FORMAT fmt;
    uint64_t frames;
    int rfi_ok;
    uint8_t *out;
    size_t out_cap;
};

static GUID codec_guid(uint32_t c)
{
    return c == CS_CODEC_AV1 ? NV_ENC_CODEC_AV1_GUID
         : c == CS_CODEC_HEVC ? NV_ENC_CODEC_HEVC_GUID
                              : NV_ENC_CODEC_H264_GUID;
}

static GUID preset_guid(uint32_t p)
{
    switch (p) {
    case 1: return NV_ENC_PRESET_P1_GUID;
    case 2: return NV_ENC_PRESET_P2_GUID;
    case 3: return NV_ENC_PRESET_P3_GUID;
    case 5: return NV_ENC_PRESET_P5_GUID;
    case 6: return NV_ENC_PRESET_P6_GUID;
    case 7: return NV_ENC_PRESET_P7_GUID;
    default: return NV_ENC_PRESET_P4_GUID;
    }
}

static const char *nv_err(cs_enc *e, NVENCSTATUS s)
{
    static __thread char buf[256];
    const char *m = e && e->enc && e->g->api.nvEncGetLastErrorString
                        ? e->g->api.nvEncGetLastErrorString(e->enc) : NULL;
    snprintf(buf, sizeof(buf), "NVENC error %d%s%s", (int)s, m && *m ? ": " : "", m ? m : "");
    return buf;
}

static int cap(cs_gpu *g, void *enc, GUID codec, NV_ENC_CAPS what)
{
    NV_ENC_CAPS_PARAM cp = { .version = NV_ENC_CAPS_PARAM_VER, .capsToQuery = what };
    int v = 0;

    if (g->api.nvEncGetEncodeCaps(enc, codec, &cp, &v) != NV_ENC_SUCCESS)
        return 0;
    return v;
}

static void *open_session(cs_gpu *g)
{
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS op = {
        .version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
        .deviceType = NV_ENC_DEVICE_TYPE_CUDA,
        .device = g->cuctx,
        .apiVersion = NVENCAPI_VERSION,
    };
    void *enc = NULL;

    if (g->api.nvEncOpenEncodeSessionEx(&op, &enc) != NV_ENC_SUCCESS)
        return NULL;
    return enc;
}

uint32_t cs_gpu_codec_caps(cs_gpu *g, int codec)
{
    CUcontext dummy;
    uint32_t r = 0;
    void *enc;
    GUID guids[16];
    uint32_t n = 0;
    GUID cg = codec_guid((uint32_t)codec);

    if (!g->nv || !g->nvenc_ver_ok)
        return 0;
    g->cu->cuCtxPushCurrent(g->cuctx);
    enc = open_session(g);
    if (enc) {
        g->api.nvEncGetEncodeGUIDs(enc, guids, 16, &n);
        for (uint32_t i = 0; i < n; i++)
            if (!memcmp(&guids[i], &cg, sizeof(GUID)))
                r |= 1;
        if (r) {
            if (cap(g, enc, cg, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE))
                r |= 2;
            if (cap(g, enc, cg, NV_ENC_CAPS_SUPPORT_LOSSLESS_ENCODE))
                r |= 4;
            if (cap(g, enc, cg, NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION))
                r |= 8;
            if (cap(g, enc, cg, NV_ENC_CAPS_SUPPORT_10BIT_ENCODE))
                r |= 16;
            r |= ((uint32_t)cap(g, enc, cg, NV_ENC_CAPS_WIDTH_MAX) / 16) << 16;
        }
        g->api.nvEncDestroyEncoder(enc);
    }
    g->cu->cuCtxPopCurrent(&dummy);
    return r;
}

static void set_color(cs_enc *e, NV_ENC_CONFIG *c)
{
    const struct cs_enc_params *p = &e->p;
    NV_ENC_VUI_COLOR_PRIMARIES prim;
    NV_ENC_VUI_TRANSFER_CHARACTERISTIC trc;
    NV_ENC_VUI_MATRIX_COEFFS mat;
    int full = p->full_range;

    switch (p->colorspace) {
    case 1:
        prim = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        trc = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        mat = NV_ENC_VUI_MATRIX_COEFFS_BT709;
        break;
    case 2:
        prim = NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
        trc = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT2020_10;
        mat = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
        break;
    default:
        prim = NV_ENC_VUI_COLOR_PRIMARIES_SMPTE170M;
        trc = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE170M;
        mat = NV_ENC_VUI_MATRIX_COEFFS_SMPTE170M;
        break;
    }
    if (p->lossless) {
        prim = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        trc = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB;
        mat = NV_ENC_VUI_MATRIX_COEFFS_RGB; /* planes are G, B, R */
        full = 1;
    }
    if (p->codec == CS_CODEC_AV1) {
        NV_ENC_CONFIG_AV1 *a = &c->encodeCodecConfig.av1Config;
        a->colorPrimaries = prim;
        a->transferCharacteristics = trc;
        a->matrixCoefficients = mat;
        a->colorRange = full;
        return;
    }
    NV_ENC_CONFIG_H264_VUI_PARAMETERS *v = p->codec == CS_CODEC_HEVC
                                               ? &c->encodeCodecConfig.hevcConfig.hevcVUIParameters
                                               : &c->encodeCodecConfig.h264Config.h264VUIParameters;
    v->videoSignalTypePresentFlag = 1;
    v->videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    v->videoFullRangeFlag = full;
    v->colourDescriptionPresentFlag = 1;
    v->colourPrimaries = prim;
    v->transferCharacteristics = trc;
    v->colourMatrix = mat;
    /* No reordering: lets every decoder output each frame at once. */
    v->bitstreamRestrictionFlag = 1;
}

cs_enc *cs_enc_open(cs_gpu *g, const struct cs_enc_params *req, char *err, size_t errlen)
{
    const struct cs_enc_params *p;
    cs_enc *e;
    CUcontext dummy;
    CUresult cr;
    NVENCSTATUS s;
    GUID cg = codec_guid(req->codec);
    NV_ENC_PRESET_CONFIG pc = { .version = NV_ENC_PRESET_CONFIG_VER,
                                .presetCfg = { .version = NV_ENC_CONFIG_VER } };
    NV_ENC_CONFIG cfg;
    NV_ENC_INITIALIZE_PARAMS ip;
    NV_ENC_TUNING_INFO tune;
    uint32_t preset = req->preset;
    uint32_t w = req->width & ~1u, h = req->height & ~1u;

    if (!g->nv) {
        cs_err(err, errlen, "NVENC is not available (libnvidia-encode.so.1 not found)");
        return NULL;
    }
    if (!g->nvenc_ver_ok) {
        cs_err(err, errlen, "the NVIDIA driver's NVENC is too old (API %u.%u, need %u.%u)",
               g->nvenc_driver_ver >> 4, g->nvenc_driver_ver & 15, NVENCAPI_MAJOR_VERSION,
               NVENCAPI_MINOR_VERSION);
        return NULL;
    }
    if (w < 64 || h < 64 || w > 8192 || h > 8192) {
        cs_err(err, errlen, "unsupported stream size %ux%u", w, h);
        return NULL;
    }
    e = calloc(1, sizeof(*e));
    if (!e)
        return NULL;
    e->g = g;
    e->p = *req;
    p = &e->p;
    e->p.width = w;
    e->p.height = h;
    if (e->p.fps == 0)
        e->p.fps = 60;
    if (e->p.lossless)
        e->p.chroma444 = 1;
    e->layout = e->p.lossless ? LAYOUT_GBR : e->p.chroma444 ? LAYOUT_444 : LAYOUT_NV12;
    e->fmt = e->layout == LAYOUT_NV12 ? NV_ENC_BUFFER_FORMAT_NV12 : NV_ENC_BUFFER_FORMAT_YUV444;

    eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx);
    g->cu->cuCtxPushCurrent(g->cuctx);

    /* Plane textures the conversion draws into, registered with CUDA once. */
    if (e->layout == LAYOUT_NV12) {
        e->tex[0] = cs_plane_tex(g, GL_R8, GL_RED, w, h);
        e->tex[1] = cs_plane_tex(g, GL_RG8, GL_RG, w / 2, h / 2);
        e->ntex = 2;
    } else {
        for (int i = 0; i < 3; i++)
            e->tex[i] = cs_plane_tex(g, GL_R8, GL_RED, w, h);
        e->ntex = 3;
    }
    for (int i = 0; i < e->ntex; i++) {
        cr = g->cu->cuGraphicsGLRegisterImage(&e->res[i], e->tex[i], GL_TEXTURE_2D,
                                              CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY);
        if (cr != CUDA_SUCCESS) {
            cs_err(err, errlen, "cuGraphicsGLRegisterImage: %s", cs_cu_err(g, cr));
            goto fail;
        }
    }
    {
        size_t rows = e->layout == LAYOUT_NV12 ? h + h / 2 : 3 * (size_t)h;
        e->pitch = (w + 255) & ~255u;
        cr = g->cu->cuMemAlloc(&e->in, e->pitch * rows);
        if (cr != CUDA_SUCCESS) {
            cs_err(err, errlen, "cuMemAlloc: %s", cs_cu_err(g, cr));
            goto fail;
        }
        /* Start black so the first IDR (sent before the guest's first frame) is sane. */
        g->cu->cuMemsetD8Async(e->in, e->layout == LAYOUT_GBR || p->full_range ? 0 : 16,
                               e->pitch * h, 0);
        if (e->layout != LAYOUT_GBR)
            g->cu->cuMemsetD8Async(e->in + e->pitch * h, 128, e->pitch * (rows - h), 0);
        else
            g->cu->cuMemsetD8Async(e->in + e->pitch * h, 0, e->pitch * (rows - h), 0);
        g->cu->cuStreamSynchronize(0);
    }

    e->enc = open_session(g);
    if (!e->enc) {
        cs_err(err, errlen, "cannot open an NVENC session (too many sessions?)");
        goto fail;
    }
    if (p->chroma444 && !cap(g, e->enc, cg, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE)) {
        cs_err(err, errlen, "this GPU cannot encode 4:4:4 with this codec");
        goto fail;
    }
    if (p->lossless && !cap(g, e->enc, cg, NV_ENC_CAPS_SUPPORT_LOSSLESS_ENCODE)) {
        cs_err(err, errlen, "this GPU cannot encode lossless with this codec");
        goto fail;
    }
    e->rfi_ok = cap(g, e->enc, cg, NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION);

    if (preset == 0) {
        /* Pixel rate decides: P1 keeps 5120x1440@240 (1.8 Gpix/s) well inside a frame time. */
        uint64_t rate = (uint64_t)w * h * e->p.fps;
        preset = rate > 1000000000ull ? 1 : rate > 500000000ull ? 2 : 4;
    }
    tune = p->lossless ? NV_ENC_TUNING_INFO_LOSSLESS : NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    s = g->api.nvEncGetEncodePresetConfigEx(e->enc, cg, preset_guid(preset), tune, &pc);
    if (s != NV_ENC_SUCCESS) {
        cs_err(err, errlen, "preset: %s", nv_err(e, s));
        goto fail;
    }
    cfg = pc.presetCfg;
    cfg.version = NV_ENC_CONFIG_VER;
    cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
    cfg.frameIntervalP = 1; /* no B-frames */
    if (!p->lossless) {
        uint32_t bps = p->bitrate_kbps ? p->bitrate_kbps * 1000u : 20000000u;
        cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
        cfg.rcParams.averageBitRate = bps;
        cfg.rcParams.maxBitRate = bps;
        /* One frame of VBV: no frame waits behind a big one. */
        cfg.rcParams.vbvBufferSize = bps / e->p.fps;
        cfg.rcParams.vbvInitialDelay = bps / e->p.fps;
        cfg.rcParams.zeroReorderDelay = 1;
        cfg.rcParams.enableAQ = 0;
        cfg.rcParams.multiPass = preset <= 2 ? NV_ENC_MULTI_PASS_DISABLED
                                             : NV_ENC_TWO_PASS_QUARTER_RESOLUTION;
    }
    switch (p->codec) {
    case CS_CODEC_H264: {
        NV_ENC_CONFIG_H264 *c = &cfg.encodeCodecConfig.h264Config;
        cfg.profileGUID = p->chroma444 ? NV_ENC_H264_PROFILE_HIGH_444_GUID
                                       : NV_ENC_H264_PROFILE_HIGH_GUID;
        c->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        c->repeatSPSPPS = 1;
        c->chromaFormatIDC = p->chroma444 ? 3 : 1;
        if (p->lossless)
            c->qpPrimeYZeroTransformBypassFlag = 1;
        if (p->max_ref_frames)
            c->maxNumRefFrames = p->max_ref_frames;
        if (p->slices > 1) {
            c->sliceMode = 3;
            c->sliceModeData = p->slices;
        }
        if (p->intra_refresh) {
            c->enableIntraRefresh = 1;
            c->intraRefreshPeriod = e->p.fps * 2;
            c->intraRefreshCnt = e->p.fps / 10 + 1;
        }
        break;
    }
    case CS_CODEC_HEVC: {
        NV_ENC_CONFIG_HEVC *c = &cfg.encodeCodecConfig.hevcConfig;
        cfg.profileGUID = p->chroma444 ? NV_ENC_HEVC_PROFILE_FREXT_GUID
                                       : NV_ENC_HEVC_PROFILE_MAIN_GUID;
        c->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        c->repeatSPSPPS = 1;
        c->chromaFormatIDC = p->chroma444 ? 3 : 1;
        if (p->max_ref_frames)
            c->maxNumRefFramesInDPB = p->max_ref_frames;
        if (p->slices > 1) {
            c->sliceMode = 3;
            c->sliceModeData = p->slices;
        }
        if (p->intra_refresh) {
            c->enableIntraRefresh = 1;
            c->intraRefreshPeriod = e->p.fps * 2;
            c->intraRefreshCnt = e->p.fps / 10 + 1;
        }
        break;
    }
    case CS_CODEC_AV1: {
        NV_ENC_CONFIG_AV1 *c = &cfg.encodeCodecConfig.av1Config;
        cfg.profileGUID = NV_ENC_AV1_PROFILE_MAIN_GUID;
        c->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        c->repeatSeqHdr = 1;
        c->chromaFormatIDC = p->chroma444 ? 3 : 1;
        c->outputAnnexBFormat = 0;
        if (p->max_ref_frames)
            c->maxNumRefFramesInDPB = p->max_ref_frames;
        if (p->intra_refresh) {
            c->enableIntraRefresh = 1;
            c->intraRefreshPeriod = e->p.fps * 2;
            c->intraRefreshCnt = e->p.fps / 10 + 1;
        }
        break;
    }
    }
    set_color(e, &cfg);

    memset(&ip, 0, sizeof(ip));
    ip.version = NV_ENC_INITIALIZE_PARAMS_VER;
    ip.encodeGUID = cg;
    ip.presetGUID = preset_guid(preset);
    ip.tuningInfo = tune;
    ip.encodeWidth = ip.darWidth = w;
    ip.encodeHeight = ip.darHeight = h;
    ip.frameRateNum = e->p.fps;
    ip.frameRateDen = 1;
    ip.enablePTD = 1;
    ip.encodeConfig = &cfg;
    /* Big frames: let the driver split each one across the NVENC engines. */
    if (p->codec != CS_CODEC_H264 && (uint64_t)w * h * e->p.fps > 1000000000ull)
        ip.splitEncodeMode = NV_ENC_SPLIT_AUTO_FORCED_MODE;
    s = g->api.nvEncInitializeEncoder(e->enc, &ip);
    if (s != NV_ENC_SUCCESS && ip.splitEncodeMode) {
        ip.splitEncodeMode = NV_ENC_SPLIT_AUTO_MODE;
        s = g->api.nvEncInitializeEncoder(e->enc, &ip);
    }
    if (s != NV_ENC_SUCCESS) {
        cs_err(err, errlen, "initialize: %s", nv_err(e, s));
        goto fail;
    }
    {
        NV_ENC_REGISTER_RESOURCE rr = {
            .version = NV_ENC_REGISTER_RESOURCE_VER,
            .resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            .width = w,
            .height = h,
            .pitch = (uint32_t)e->pitch,
            .resourceToRegister = (void *)(uintptr_t)e->in,
            .bufferFormat = e->fmt,
            .bufferUsage = NV_ENC_INPUT_IMAGE,
        };
        s = g->api.nvEncRegisterResource(e->enc, &rr);
        if (s != NV_ENC_SUCCESS) {
            cs_err(err, errlen, "register input: %s", nv_err(e, s));
            goto fail;
        }
        e->reg = rr.registeredResource;
    }
    {
        NV_ENC_CREATE_BITSTREAM_BUFFER cb = { .version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER };
        s = g->api.nvEncCreateBitstreamBuffer(e->enc, &cb);
        if (s != NV_ENC_SUCCESS) {
            cs_err(err, errlen, "bitstream buffer: %s", nv_err(e, s));
            goto fail;
        }
        e->bs = cb.bitstreamBuffer;
    }
    g->cu->cuCtxPopCurrent(&dummy);
    return e;
fail:
    g->cu->cuCtxPopCurrent(&dummy);
    cs_enc_close(e);
    return NULL;
}

static int copy_planes(cs_enc *e, char *err, size_t errlen)
{
    cs_gpu *g = e->g;
    CUresult cr;
    uint32_t w = e->p.width, h = e->p.height;

    cr = g->cu->cuGraphicsMapResources((unsigned)e->ntex, e->res, 0);
    if (cr != CUDA_SUCCESS) {
        cs_err(err, errlen, "map planes: %s", cs_cu_err(g, cr));
        return -1;
    }
    for (int i = 0; i < e->ntex; i++) {
        CUarray arr;
        CUDA_MEMCPY2D m;

        cr = g->cu->cuGraphicsSubResourceGetMappedArray(&arr, e->res[i], 0, 0);
        if (cr != CUDA_SUCCESS)
            break;
        memset(&m, 0, sizeof(m));
        m.srcMemoryType = CU_MEMORYTYPE_ARRAY;
        m.srcArray = arr;
        m.dstMemoryType = CU_MEMORYTYPE_DEVICE;
        m.dstDevice = e->in + e->pitch * h * (size_t)i;
        m.dstPitch = e->pitch;
        m.WidthInBytes = w; /* NV12 UV: w/2 texels of 2 bytes */
        m.Height = (e->layout == LAYOUT_NV12 && i == 1) ? h / 2 : h;
        cr = g->cu->cuMemcpy2DAsync(&m, 0);
        if (cr != CUDA_SUCCESS)
            break;
    }
    if (cr == CUDA_SUCCESS)
        cr = g->cu->cuStreamSynchronize(0);
    g->cu->cuGraphicsUnmapResources((unsigned)e->ntex, e->res, 0);
    if (cr != CUDA_SUCCESS) {
        cs_err(err, errlen, "plane copy: %s", cs_cu_err(g, cr));
        return -1;
    }
    return 0;
}

int cs_enc_encode(cs_enc *e, const struct cs_frame_opts *o, struct cs_packet *out, char *err,
                  size_t errlen)
{
    cs_gpu *g = e->g;
    CUcontext dummy;
    NVENCSTATUS s;
    NV_ENC_MAP_INPUT_RESOURCE mi = { .version = NV_ENC_MAP_INPUT_RESOURCE_VER };
    NV_ENC_PIC_PARAMS pp;
    NV_ENC_LOCK_BITSTREAM lb = { .version = NV_ENC_LOCK_BITSTREAM_VER };
    uint64_t t0 = cs_now_us();
    int ret = -1;

    memset(out, 0, sizeof(*out));
    eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx);
    g->cu->cuCtxPushCurrent(g->cuctx);
    if (o->render) {
        int mode = e->layout == LAYOUT_GBR ? 3 : e->layout == LAYOUT_444 ? 2 : 0;
        cs_convert(g, e->tex, e->layout == LAYOUT_NV12 ? 1 : 3, e->p.width, e->p.height, mode,
                   e->p.width, e->p.height, e->p.colorspace, e->p.full_range, o);
        if (e->layout == LAYOUT_NV12)
            cs_convert(g, &e->tex[1], 1, e->p.width / 2, e->p.height / 2, 1, e->p.width,
                       e->p.height, e->p.colorspace, e->p.full_range, o);
        /* The guest may reuse its buffer as soon as we return: make sure the
         * draw that read it has finished, not just been queued. */
        g->gl.glFinish();
        if (copy_planes(e, err, errlen))
            goto out;
    }
    mi.registeredResource = e->reg;
    s = g->api.nvEncMapInputResource(e->enc, &mi);
    if (s != NV_ENC_SUCCESS) {
        cs_err(err, errlen, "map input: %s", nv_err(e, s));
        goto out;
    }
    memset(&pp, 0, sizeof(pp));
    pp.version = NV_ENC_PIC_PARAMS_VER;
    pp.inputWidth = e->p.width;
    pp.inputHeight = e->p.height;
    pp.inputPitch = (uint32_t)e->pitch;
    pp.inputBuffer = mi.mappedResource;
    pp.bufferFmt = mi.mappedBufferFmt;
    pp.outputBitstream = e->bs;
    pp.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    pp.inputTimeStamp = ++e->frames;
    pp.frameIdx = (uint32_t)e->frames;
    if (o->force_idr || e->frames == 1)
        pp.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    s = g->api.nvEncEncodePicture(e->enc, &pp);
    if (s != NV_ENC_SUCCESS) {
        g->api.nvEncUnmapInputResource(e->enc, mi.mappedResource);
        cs_err(err, errlen, "encode: %s", nv_err(e, s));
        goto out;
    }
    lb.outputBitstream = e->bs;
    s = g->api.nvEncLockBitstream(e->enc, &lb);
    if (s != NV_ENC_SUCCESS) {
        g->api.nvEncUnmapInputResource(e->enc, mi.mappedResource);
        cs_err(err, errlen, "lock bitstream: %s", nv_err(e, s));
        goto out;
    }
    if (lb.bitstreamSizeInBytes > e->out_cap) {
        size_t cap = lb.bitstreamSizeInBytes + (lb.bitstreamSizeInBytes >> 2);
        uint8_t *n = realloc(e->out, cap);
        if (!n) {
            g->api.nvEncUnlockBitstream(e->enc, e->bs);
            g->api.nvEncUnmapInputResource(e->enc, mi.mappedResource);
            cs_err(err, errlen, "out of memory");
            goto out;
        }
        e->out = n;
        e->out_cap = cap;
    }
    memcpy(e->out, lb.bitstreamBufferPtr, lb.bitstreamSizeInBytes);
    out->data = e->out;
    out->len = lb.bitstreamSizeInBytes;
    out->idr = lb.pictureType == NV_ENC_PIC_TYPE_IDR || lb.pictureType == NV_ENC_PIC_TYPE_I;
    out->frame_index = e->frames;
    g->api.nvEncUnlockBitstream(e->enc, e->bs);
    g->api.nvEncUnmapInputResource(e->enc, mi.mappedResource);
    out->encode_us = (uint32_t)(cs_now_us() - t0);
    ret = 0;
out:
    g->cu->cuCtxPopCurrent(&dummy);
    return ret;
}

int cs_enc_invalidate(cs_enc *e, uint64_t first, uint64_t last)
{
    CUcontext dummy;
    int ret = 0;

    if (!e->rfi_ok || last < first || last - first > 16 || last > e->frames)
        return -1;
    e->g->cu->cuCtxPushCurrent(e->g->cuctx);
    for (uint64_t f = first; f <= last; f++)
        if (e->g->api.nvEncInvalidateRefFrames(e->enc, f) != NV_ENC_SUCCESS)
            ret = -1;
    e->g->cu->cuCtxPopCurrent(&dummy);
    return ret;
}

void cs_enc_close(cs_enc *e)
{
    cs_gpu *g;
    CUcontext dummy;

    if (!e)
        return;
    g = e->g;
    eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx);
    g->cu->cuCtxPushCurrent(g->cuctx);
    if (e->enc) {
        if (e->bs)
            g->api.nvEncDestroyBitstreamBuffer(e->enc, e->bs);
        if (e->reg)
            g->api.nvEncUnregisterResource(e->enc, e->reg);
        g->api.nvEncDestroyEncoder(e->enc);
    }
    for (int i = 0; i < e->ntex; i++)
        if (e->res[i])
            g->cu->cuGraphicsUnregisterResource(e->res[i]);
    if (e->in)
        g->cu->cuMemFree(e->in);
    g->cu->cuCtxPopCurrent(&dummy);
    if (e->ntex)
        g->gl.glDeleteTextures(e->ntex, e->tex);
    free(e->out);
    free(e);
}
