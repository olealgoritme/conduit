/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_gpuscale.h -- the fullscreen scaler: one GPU pass that draws the guest's
 * frame, scaled and letterboxed, into a buffer of the OUTPUT's size.
 *
 * Why it exists.  A compositor tears (presents immediately) and scans out
 * directly only for a fullscreen window whose own buffer covers the output
 * with nothing else on top.  A guest running below the output's resolution
 * (a game at 1600x900 on a 5120x1440 screen) gives the viewer a small buffer;
 * scaled by a viewport, or letterboxed on a subsurface, the window no longer
 * qualifies and the compositor composites it at vblank (Hyprland 0.39:
 * CWLSurface::small() compares the BUFFER size with the window, and any
 * subsurface disqualifies).  The scaler restores the qualifying shape: one
 * opaque output-size buffer on the main surface.  The compositor was going to
 * scale the picture anyway; this moves that pass in front of it.
 *
 * Used in fullscreen when the measurement in nb_scanout.h finds it beats
 * showing the guest's buffer zero-copy (or --gpu-scale=on asks for it).
 *
 * EGL, GLES2 and GBM are loaded with dlopen when the scaler is first wanted,
 * so the viewer still has no link-time dependency on them.
 */
#ifndef NB_GPUSCALE_H
#define NB_GPUSCALE_H

#include <stdbool.h>
#include <stdint.h>
#include <sys/types.h>

struct nb_gs;

#define NB_GS_TARGETS 3

/* An output buffer, for the backend to wrap in a wl_buffer. */
struct nb_gs_target {
    int      fd;                /* dma-buf, owned by nb_gs                   */
    uint32_t w, h, stride, offset, fourcc;
    uint64_t modifier;
};

/*
 * Open the scaler on a DRM render node: `node` if given, else the render
 * node of `dev` (the compositor's main device, 0 = unknown), else the first
 * render node that can import (fourcc, mod).  NULL with a reason in why.
 */
struct nb_gs *nb_gs_open(const char *node, dev_t dev, uint32_t fourcc,
                         uint64_t mod, char *why, size_t n);
void nb_gs_close(struct nb_gs *g);
/* Whether this scaler's GPU imports (fourcc, mod) as a source frame. */
bool nb_gs_can_import(struct nb_gs *g, uint32_t fourcc, uint64_t mod);
const char *nb_gs_node(const struct nb_gs *g);

/* Formats/modifiers the compositor accepts, for choosing the target layout:
 * ok(fourcc, mod) says whether a wl_buffer of that pair will be taken. */
typedef bool (*nb_gs_fmt_ok)(void *ctx, uint32_t fourcc, uint64_t mod);

/*
 * (Re)allocate the NB_GS_TARGETS output buffers at w x h (physical pixels).
 * cand_fourcc/cand_mod/n_cand: the (format, modifier) pairs to allocate in,
 * best first and grouped by format (nb_present.h nb_pc_targets); GBM picks
 * among the modifiers of one format at a time.  None fits, or none given:
 * GBM's own choice, kept when ok() takes it, else LINEAR.  Returns the
 * number made (0 on failure, with why).  out[] describes them.
 */
int nb_gs_targets(struct nb_gs *g, uint32_t w, uint32_t h, nb_gs_fmt_ok ok,
                  void *ctx, const uint32_t *cand_fourcc,
                  const uint64_t *cand_mod, unsigned n_cand,
                  struct nb_gs_target out[NB_GS_TARGETS],
                  char *why, size_t n);

/*
 * The last resort when the compositor takes no layout this GPU can render
 * to (NVIDIA renders to no LINEAR buffer, and a compositor on another GPU
 * takes no NVIDIA modifier): plain renderbuffers, each frame read back into
 * memory the caller hands the compositor as wl_shm (XRGB8888, w*4 stride).
 */
int nb_gs_targets_readback(struct nb_gs *g, uint32_t w, uint32_t h,
                           char *why, size_t n);
/* After nb_gs_draw into `target`: copy it to dst (w*h*4 bytes). */
int nb_gs_readback(struct nb_gs *g, int target, void *dst);

/* Source frames, cached by `key` (the backend's buffer slot). */
int  nb_gs_src(struct nb_gs *g, int key, int fd, uint32_t w, uint32_t h,
               uint32_t stride, uint32_t offset, uint32_t fourcc,
               uint64_t mod);
void nb_gs_src_drop(struct nb_gs *g, int key);

/* nb_gs_draw.src_key: the frame-generation history (nb_gs_fg_push) instead
 * of a guest buffer; fg_t says which picture between its two frames. */
#define NB_GS_SRC_FG (-2)

struct nb_gs_draw {
    int    src_key;
    int    target;              /* 0..NB_GS_TARGETS-1                        */
    int    dx, dy, dw, dh;      /* destination, target pixels, y down        */
    double sx, sy, sw, sh;      /* source rectangle, buffer pixels (ignored
                                 * for NB_GS_SRC_FG: the history is cropped) */
    bool   nearest;
    bool   alpha;               /* keep the source's alpha (a cursor image);
                                 * otherwise the target is opaque            */
    /* Picture enhancement (docs/VIEWER-ENHANCE.md); zero = the plain pass. */
    int    upscale;             /* NB_UPSCALE_* (nb_view.h)                  */
    int    sharp;               /* 0..100                                    */
    float  fg_t;                /* NB_GS_SRC_FG: 0 previous .. 1 newest      */
};

/*
 * Draw and wait for the GPU to finish (bounded by timeout_us), so the buffer
 * is complete before the compositor reads it and the source can go back to
 * the guest.  0, or -errno.  gpu_us: submit-to-done time.
 */
int nb_gs_draw(struct nb_gs *g, const struct nb_gs_draw *d,
               unsigned timeout_us, unsigned *gpu_us);

/* Tests: one pixel of a target, 0xRRGGBB (row y counted from the top). */
int nb_gs_read(struct nb_gs *g, int target, int x, int y, uint32_t *rgb);

/*
 * PICTURE ENHANCEMENT (docs/VIEWER-ENHANCE.md): the upscalers and frame
 * generation run in this same context, which is then GLES 3.1 (asked for
 * first, GLES 2 otherwise).  Programs are compiled when first used.
 *
 * With nb_gs_draw.upscale set, a guest frame is first copied into an
 * internal texture (an external dma-buf texture cannot be gathered from),
 * then drawn by the chosen pass: FSR 1 (EASU, then RCAS into the target),
 * NIS (compute, then a 1:1 copy), CAS (sharpened at the guest's size, then
 * scaled).  A pass that cannot run (no GLES 3.1, a shader the driver
 * refuses, a scale it does not do: nb_enhance.h nb_enh_pick) falls back to
 * the plain pass; nb_gs_enh_status says which ran and why.
 */
bool nb_gs_enhance_ok(struct nb_gs *g, const char **why);
/* The pass the last draw used (NB_UPSCALE_*), and why it is not the one
 * asked for (NULL when it is). */
int  nb_gs_enh_status(const struct nb_gs *g, const char **why);

/*
 * FRAME GENERATION (nb_fg.h decides when).  Push a guest frame: it is
 * copied (source rectangle sx..sh) into a two-frame history, its luma
 * pyramid is built and the optical flow from the previous frame estimated
 * (block matching, coarse to fine).  Waits for the GPU (bounded), so the
 * guest buffer may be released after.  1: a pair is ready to interpolate;
 * 0: this is the first frame of a new history (size changed, reset);
 * -errno on failure.  Then nb_gs_draw with src_key NB_GS_SRC_FG.
 */
int  nb_gs_fg_push(struct nb_gs *g, int src_key, double sx, double sy,
                   double sw, double sh, unsigned timeout_us,
                   unsigned *gpu_us);
void nb_gs_fg_reset(struct nb_gs *g);
/* timeout_us 0: submit without waiting; the next nb_gs_draw's wait covers
 * it (one GPU submission per guest frame instead of two). */
/* Whether the history holds a frame to draw. */
bool nb_gs_fg_ready(const struct nb_gs *g);

/*
 * BENCHMARK (conduit-viewer --bench-enhance): every enhancement pass on the
 * GPU at node (NULL: the first that works), an iw x ih source drawn at
 * ow x oh, `iters` times each; the GPU time per pass (timer queries, when
 * the driver has them) and the wall time to the fence, medians, to `out`.
 * No display, no VM.  0, or -1 with the reason printed.
 */
int nb_gs_bench(const char *node, uint32_t iw, uint32_t ih, uint32_t ow,
                uint32_t oh, int iters, void *out_file);

/*
 * GPU PRIORITY OF THE PASS.  The pass sits between the guest's frame and the
 * screen on the GPU the guest is loading, so it asks to be scheduled ahead of
 * it: EGL_IMG_context_priority HIGH by default, EGL_NV_context_priority_
 * realtime when asked.  The driver decides what it grants (NVIDIA's RM
 * requires CAP_SYS_NICE for anything above MEDIUM: SET_INTERLEAVE_LEVEL and
 * MAKE_REALTIME check RS_ACCESS_NICE), and the granted level is read back
 * with eglQueryContext and reported.
 *
 * CONDUIT_VIEWER_GPU_PRIORITY = low | off | medium | high (default) |
 * realtime.  off and medium both create a context without the attribute.
 *
 * With picture enhancement on (docs/VIEWER-ENHANCE.md) the sessions ask for
 * LOW instead (nb_gs_set_prio_default): those passes do more work per frame,
 * and run on the GPU the guest renders on; at LOW they wait for the guest's
 * channels rather than preempt them.  The variable, when set, wins.
 */
enum nb_gs_prio {
    NB_GS_PRIO_UNKNOWN = -1,    /* not reported by the driver               */
    NB_GS_PRIO_LOW = 0,
    NB_GS_PRIO_MEDIUM,
    NB_GS_PRIO_HIGH,
    NB_GS_PRIO_REALTIME,
};

/* The env value (NULL or empty: the default, HIGH); an unknown word is
 * reported through *bad and gives the default. */
enum nb_gs_prio nb_gs_prio_parse(const char *s, bool *bad);
/* The priority asked for when CONDUIT_VIEWER_GPU_PRIORITY is not set, for
 * the next nb_gs_open (HIGH until changed). */
void nb_gs_set_prio_default(enum nb_gs_prio p);
/* What the next nb_gs_open will ask for: the variable, else the default. */
enum nb_gs_prio nb_gs_prio_wanted(void);
const char *nb_gs_prio_name(enum nb_gs_prio p);
/* What was asked for and what the driver says it granted. */
enum nb_gs_prio nb_gs_prio_requested(const struct nb_gs *g);
enum nb_gs_prio nb_gs_prio_granted(const struct nb_gs *g);

#endif /* NB_GPUSCALE_H */
