/* SPDX-License-Identifier: Apache-2.0 */
/*
 * gpu.h -- the GPU half of conduit-stream: dma-buf import, colour conversion,
 * NVENC encode and NVDEC decode. Everything here runs on the GPU; the only
 * bytes that reach the CPU are bitstreams.
 *
 * Threading: a cs_gpu and everything made from it belong to ONE thread (its
 * EGL context is current there). The Rust side guarantees that.
 */
#ifndef CS_GPU_H
#define CS_GPU_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct cs_gpu cs_gpu;
typedef struct cs_enc cs_enc;
typedef struct cs_dec cs_dec;

enum { CS_CODEC_H264 = 0, CS_CODEC_HEVC = 1, CS_CODEC_AV1 = 2 };

/* Open the GPU: EGL on the NVIDIA device (no window), GL 3.3 core, a CUDA
 * context, and the NVENC/NVDEC entry points (loaded from the driver at run
 * time). `render_node` may be NULL (first NVIDIA device). */
cs_gpu *cs_gpu_open(const char *render_node, char *err, size_t errlen);
void cs_gpu_close(cs_gpu *g);

/* 1 if a dma-buf of this (fourcc, modifier) can be imported. */
int cs_gpu_format_ok(cs_gpu *g, uint32_t fourcc, uint64_t modifier);

/* Importable modifiers for `fourcc` (not external-only); returns the count. */
size_t cs_gpu_modifiers(cs_gpu *g, uint32_t fourcc, uint64_t *out, size_t max);

/* What a codec can do here: bit 0 supported, bit 1 4:4:4, bit 2 lossless,
 * bit 3 reference-frame invalidation, bit 4 10-bit. Bits 16..31: max width/16. */
uint32_t cs_gpu_codec_caps(cs_gpu *g, int codec);

/* Make `fd` (a dma-buf; not consumed) the current picture. `id` identifies
 * the buffer (its inode): the import is cached per id. 0 on success. */
int cs_gpu_set_frame(cs_gpu *g, int fd, uint64_t id, uint32_t w, uint32_t h,
                     uint32_t stride, uint32_t offset, uint32_t fourcc,
                     uint64_t modifier, char *err, size_t errlen);
/* No picture (black). */
void cs_gpu_clear_frame(cs_gpu *g);
/* Size of the current picture (0x0 if none). */
void cs_gpu_frame_size(cs_gpu *g, uint32_t *w, uint32_t *h);

/* The guest's cursor image: fd < 0 hides it. Hotspot in image pixels. */
int cs_gpu_set_cursor(cs_gpu *g, int fd, uint64_t id, uint32_t w, uint32_t h,
                      uint32_t stride, uint32_t offset, uint32_t fourcc,
                      uint64_t modifier, uint32_t hot_x, uint32_t hot_y,
                      char *err, size_t errlen);

struct cs_enc_params {
    uint32_t codec;         /* CS_CODEC_* */
    uint32_t width, height; /* stream size (even) */
    uint32_t fps;
    uint32_t bitrate_kbps;
    uint32_t chroma444;     /* 1: 4:4:4 */
    uint32_t lossless;      /* 1: lossless, RGB as G/B/R planes (implies 4:4:4) */
    uint32_t colorspace;    /* 0 BT.601, 1 BT.709, 2 BT.2020 */
    uint32_t full_range;
    uint32_t preset;        /* NVENC P1..P7; 0 = choose by pixel rate */
    uint32_t max_ref_frames;/* 0 = encoder default */
    uint32_t slices;        /* slices per frame, 0/1 = one */
    uint32_t intra_refresh; /* 1: intra refresh instead of IDR recovery */
};

struct cs_frame_opts {
    int32_t render;        /* 1: convert the current picture; 0: re-encode the last input */
    int32_t force_idr;
    int32_t cursor_on;     /* blend the cursor at (cursor_x, cursor_y) = pointer position in picture pixels */
    int32_t cursor_x, cursor_y;
};

struct cs_packet {
    const uint8_t *data;   /* valid until the next encode/close */
    size_t len;
    uint64_t frame_index;  /* counts every encoded frame from 1 */
    int32_t idr;
    uint32_t encode_us;    /* convert + copy + encode, microseconds */
};

cs_enc *cs_enc_open(cs_gpu *g, const struct cs_enc_params *p, char *err, size_t errlen);
int cs_enc_encode(cs_enc *e, const struct cs_frame_opts *o, struct cs_packet *out,
                  char *err, size_t errlen);
/* Invalidate frames [first,last] as references (RFI). 0 ok, -1 not possible (send an IDR). */
int cs_enc_invalidate(cs_enc *e, uint64_t first, uint64_t last);
void cs_enc_close(cs_enc *e);

/* ---- decode (the conduit link's client side) ---- */

struct cs_dec_params {
    uint32_t codec;
    uint32_t width, height;
    uint32_t chroma444;
    uint32_t lossless;      /* G/B/R planes */
    uint32_t colorspace;
    uint32_t full_range;
};

struct cs_dec_frame {
    int fd;                 /* dma-buf of the output picture; owned by the decoder, do not close */
    uint64_t id;
    uint32_t width, height, stride, offset, fourcc;
    uint64_t modifier;
};

/* Output buffers are allocated with GBM on `render_node` with one of
 * `modifiers` (n may be 0: LINEAR). */
cs_dec *cs_dec_open(cs_gpu *g, const struct cs_dec_params *p, const uint64_t *modifiers,
                    size_t n, char *err, size_t errlen);
/* Decode one access unit. Returns 1 and fills `out` when a picture is ready,
 * 0 when not yet, -1 on error. */
int cs_dec_decode(cs_dec *d, const uint8_t *data, size_t len, struct cs_dec_frame *out,
                  char *err, size_t errlen);
void cs_dec_close(cs_dec *d);

#ifdef __cplusplus
}
#endif

#endif
