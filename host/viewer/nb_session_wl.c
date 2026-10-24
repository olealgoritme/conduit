/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_session_wl.c — the Wayland backend.
 *
 * DISPLAY.  An ordinary xdg_toplevel window.  The guest's scanout dma-buf
 * arrives from the VMM as an fd; we wrap it in a wl_buffer through
 * zwp_linux_dmabuf_v1 and attach it to our wl_surface.  Nothing is imported
 * into a GL context, nothing is blitted, and this file contains no GL call:
 * the compositor takes the guest's own allocation as the surface's content.
 *
 * That is one blit fewer than QEMU's GTK/SDL path, which imports the same
 * dma-buf as a texture and glBlitFramebuffer's it into a window framebuffer it
 * owns.  It is also the only arrangement under which the frame can reach a
 * hardware plane at all: a compositor promotes a surface to DIRECT SCANOUT
 * when it is fullscreen, opaque, unoccluded and its format/modifier suits the
 * plane.  That cannot be requested — only qualified for — which is why
 * CTRL+ALT+F is worth having.
 *
 * FORMATS.  The set of acceptable (fourcc, modifier) pairs is whatever the
 * compositor advertised on zwp_linux_dmabuf_v1, and nothing else.  We bind at
 * version 3 deliberately: from version 4 the compositor is required NOT to
 * send `format`/`modifier` events (feedback objects replace them), and a
 * broker that binds 4 would end up with an empty advertised set and reject
 * every frame.
 *
 * INPUT.  Three optional protocols, each independently absent:
 *   zwp_keyboard_shortcuts_inhibit_manager_v1  keyboard, including the
 *                                              compositor's own bindings
 *   zwp_pointer_constraints_v1 (locked pointer) pointer lock under grab
 *   zwp_relative_pointer_manager_v1             relative deltas under grab
 * Missing ones are reported at startup, reflected in the capability bits and
 * spelled out in grab_caveat — a grab that quietly leaks Super to the
 * compositor is exactly the failure a user finds mid-game.
 *
 * FOCUS.  wl_keyboard.leave is the focus-out signal.  It is what makes the
 * grab safe to offer at all: without it the grab would be unescapable, and an
 * unescapable grab is a keylogger.
 */
#include <errno.h>
#include <math.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/wait.h>
#include <time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include "nvkvm_broker.h"
#include "nb_overlay.h"
#include "nb_relacc.h"
#include "nb_gpuscale.h"
#include "nb_sprite.h"
#include "nb_pace.h"
#include "nb_latch.h"
#include "nb_blit.h"
#include "nb_scanout.h"
#include "nb_present.h"
#include "nb_xsync.h"
#include "nb_vmpause.h"
#include <sys/timerfd.h>

#ifndef NB_HAVE_WAYLAND

struct nb_session *nb_session_wayland(const struct nb_config *cfg)
{
    (void)cfg;
    nb_err("the Wayland backend was not compiled in (wayland-client or "
           "wayland-protocols missing at build time)");
    return NULL;
}

#else /* NB_HAVE_WAYLAND */

#include <linux/input-event-codes.h>
#include <wayland-client.h>

#include "keyboard-shortcuts-inhibit-unstable-v1-client-protocol.h"
#include "linux-dmabuf-v1-client-protocol.h"
#include "pointer-constraints-unstable-v1-client-protocol.h"
#include "relative-pointer-unstable-v1-client-protocol.h"
#include "presentation-time-client-protocol.h"
#include "viewporter-client-protocol.h"
#include "fractional-scale-v1-client-protocol.h"
#include "nb_ui.h"
#include "tearing-control-v1-client-protocol.h"
#include "content-type-v1-client-protocol.h"
#include "single-pixel-buffer-v1-client-protocol.h"
#include "xdg-decoration-unstable-v1-client-protocol.h"
#include "xdg-shell-client-protocol.h"
#ifdef NB_HAVE_WL_SYNCOBJ
#include "linux-drm-syncobj-v1-client-protocol.h"
#else
struct wp_linux_drm_syncobj_manager_v1;
struct wp_linux_drm_syncobj_surface_v1;
struct wp_linux_drm_syncobj_timeline_v1;
#endif

/* Defined here rather than pulled from libdrm: with the DRM-lease design gone
 * this backend has no other reason to link libdrm, and one constant is not
 * worth a dependency. */
#define NB_DRM_FORMAT_MOD_INVALID  0x00ffffffffffffffULL

#define NB_TB_H     28

/*
 * Minimum CONTENT size, and the reason it has to exist.
 *
 * Nothing set a floor: set_min_size was never called (it appears only in
 * the generated xdg-shell stub) and the X11 backend set no XSizeHints. A
 * compositor is therefore free to configure the toplevel arbitrarily
 * small, and top_configure() subtracts NB_TB_H for the title bar and then
 * clamps anything <= 0 to 1 -- so any configure at or below 28px high
 * produced a ONE PIXEL content area. OBSERVED 2026-09-02: the broker
 * window starts collapsed and has to be resized by hand.
 *
 * The clamp itself was not wrong -- a zero-height surface is invalid --
 * it just had no floor to clamp to.
 */
#define NB_MIN_W    320
#define NB_MIN_H    240
#define NB_TB_BTN   34          /* width of one button, from the right edge  */

/* The close-confirmation overlay. */
#define NB_DLG_W     560
#define NB_DLG_BTN_H  40
#define NB_DLG_GAP     8
#define NB_DLG_PAD    18
#define NB_DLG_N       4        /* acpi / force / display-only / cancel       */
#define NB_DLG_TITLE  26
#define NB_DLG_H  (NB_DLG_PAD * 2 + NB_DLG_TITLE + NB_DLG_GAP + \
                   NB_DLG_N * (NB_DLG_BTN_H + NB_DLG_GAP))

static const char *const nb_dlg_label[NB_DLG_N] = {
    "SHUT DOWN THE GUEST (ACPI)",
    "FORCE OFF THE VM",
    "CLOSE THE DISPLAY ONLY",
    "CANCEL",
};
#define NB_CUR_N     5          /* arrow + four resize cursors            */
/*
 * Px of grabbable edge OUTSIDE the window.  Invisible and outside the visible
 * bounds, so width costs nothing but the ease of hitting it -- and 8 was hard
 * to hit, which is the whole purpose of the thing.
 */
#define NB_BORDER    14
#define NB_TITLE_MAX ((size_t)48)
/* Cursor slots: 0 arrow, 1 horizontal, 2 vertical, 3 NW-SE, 4 NE-SW. */
#define NB_CUR_ARROW 0
#define NB_CUR_EW    1
#define NB_CUR_NS    2
#define NB_CUR_NWSE  3
#define NB_CUR_NESW  4

static const uint32_t nb_bd_edge[8] = {
    XDG_TOPLEVEL_RESIZE_EDGE_TOP,
    XDG_TOPLEVEL_RESIZE_EDGE_BOTTOM,
    XDG_TOPLEVEL_RESIZE_EDGE_LEFT,
    XDG_TOPLEVEL_RESIZE_EDGE_RIGHT,
    XDG_TOPLEVEL_RESIZE_EDGE_TOP_LEFT,
    XDG_TOPLEVEL_RESIZE_EDGE_TOP_RIGHT,
    XDG_TOPLEVEL_RESIZE_EDGE_BOTTOM_LEFT,
    XDG_TOPLEVEL_RESIZE_EDGE_BOTTOM_RIGHT,
};
static const int nb_bd_cursor[8] = {
    NB_CUR_NS, NB_CUR_NS, NB_CUR_EW, NB_CUR_EW,
    NB_CUR_NWSE, NB_CUR_NESW, NB_CUR_NESW, NB_CUR_NWSE,
};

struct nb_wl_buf {
    bool      valid;
    uint64_t  id;                   /* dma-buf inode                          */
    struct wl_buffer *buf;
    uint32_t  w, h, stride, offset, fourcc;
    uint64_t  modifier;
    uint64_t  used;                 /* LRU tick                               */
    struct nb_wl *owner;
    /*
     * HELD BY THE COMPOSITOR.  Set when the buffer is committed to the
     * surface, cleared by wl_buffer.release.  A Wayland client must not touch
     * a buffer between those two points -- and here the party that touches it
     * is the GUEST, several processes away, which never sees the release.
     * Tracking it is what makes "the guest overwrote a buffer the compositor
     * was still reading" an observable event instead of a theory.
     */
    bool      held;
    uint32_t  seq;                  /* client frame counter at last commit    */
    uint64_t  commits;
    uint64_t  recv_ns;              /* virtio-nvgpu: when its ATTACH arrived  */
    int       fd;                   /* dup of the dma-buf for the fullscreen
                                     * scaler, -1 none (shm)                 */
    /* COPY-ONLY: the compositor does not take this layout, so there is no
     * wl_buffer (buf NULL) and every frame goes through the GPU pass
     * (nb_blit.h). */
    bool      copy;
    bool      shm;                  /* a wl_shm buffer (the boot console)    */
};

/*
 * EXPLICIT SYNC, one timeline: the viewer's import of it (a DRM syncobj
 * handle on nb_xs's render node) and the compositor's.
 */
struct nb_wl_tl {
    uint32_t h;
    struct wp_linux_drm_syncobj_timeline_v1 *tl;
};

/* What was attached to a surface and still needs its acquire/release points
 * (set right before the surface's commit, see wl_surf_commit()). */
enum { NB_XA_NONE = 0, NB_XA_GUEST, NB_XA_GT };
struct nb_wl_xa {
    int      kind;                  /* NB_XA_*                               */
    int      idx;                   /* guest slot or GPU-copy target         */
    uint64_t acq;                   /* acquire point already on the timeline,
                                     * 0 = make one at the commit            */
};

/* Distinct pairs whose importability we remember.  Two occur in practice:
 * the guest's own tiling and the LINEAR fallback. */
/* DRM_FORMAT_ARGB8888, spelled locally like the XR24 one. */
#define NB_FOURCC_AR24_LOCAL 0x34325241u
#define NB_FOURCC_XR24_LOCAL 0x34325258u

#define NB_PROVEN_SLOTS 4

struct nb_wl {
    struct wl_display    *dpy;
    struct wl_registry   *reg;
    struct wl_compositor *comp;
    struct wl_seat       *seat;
    struct wl_keyboard   *kbd;
    struct wl_pointer    *ptr;
    struct wl_surface    *surf;
    struct xdg_wm_base   *wm_base;
    struct xdg_surface   *xdg_surf;
    struct xdg_toplevel  *toplevel;
    struct zwp_linux_dmabuf_v1 *dmabuf;
    uint32_t              dmabuf_ver;

    /*
     * PAIRS PROVEN BY IMPORT, not by advertisement.
     *
     * A compositor can advertise a (fourcc, modifier) its own driver then
     * refuses to bind.  MEASURED on GNOME/Mutter with NVIDIA 595.84: it
     * advertises XR24 + LINEAR and answers the import with
     *   [destroyed object]: error 7: failed to import supplied dmabufs:
     *   Could not bind the given EGLImage to a CoglTexture2D
     * -- which is a WAYLAND PROTOCOL ERROR, so the connection dies and the
     * broker exits.  Under a restart policy that is a spin: OBSERVED 15
     * restarts in a few minutes on the physical box.
     *
     * So the first buffer of an unproven pair goes through the ASYNCHRONOUS
     * zwp_linux_buffer_params_v1.create, which reports failure as a `failed`
     * EVENT instead of killing the connection.  Once a pair is proven, every
     * later buffer uses create_immed, which is why the roundtrip is paid once
     * per pair and never on the steady-state frame path -- the objection the
     * create_immed comment below is about.
     */
    struct {
        uint32_t fourcc;
        uint64_t mod;
        int      state;       /* 0 unproven, 1 importable, -1 refused */
        bool     probing;
    } proven[NB_PROVEN_SLOTS];
    uint32_t probe_fourcc;   /* the pair the outstanding probe is about */
    uint64_t probe_mod;
    /* ...and there is at most ONE, because the two fields above are how its
     * answer is attributed.  See wl_attach(). */
    bool     probe_inflight;
    /* A probe that failed and the client has not been told about yet. */
    bool     probe_bad_pending;
    uint32_t probe_bad_fourcc;
    uint64_t probe_bad_mod;
    /* Only ever used for the idle placeholder.  The guest path never touches
     * shm — its frames are dma-bufs and are never copied. */
    struct wl_shm        *shm;
    struct wl_buffer     *idle_buf;
    void                 *idle_px;
    size_t                idle_sz;
    int                   idle_w, idle_h;
    bool                  idle_wanted;   /* show it as soon as configured */
    bool                  client_attached; /* a VMM is connected, frames or not */
    uint64_t              last_frame_ms;   /* when we last paced the VMM        */
    bool                  idle_shown;

    struct zwp_keyboard_shortcuts_inhibit_manager_v1 *inhibit_mgr;
    struct zwp_keyboard_shortcuts_inhibitor_v1       *inhibitor;
    struct zwp_pointer_constraints_v1                *constraints;
    struct zwp_locked_pointer_v1                     *lock;
    struct zwp_relative_pointer_manager_v1           *relptr_mgr;
    struct zwp_relative_pointer_v1                   *relptr;
    struct nb_relacc                                  relacc;  /* sub-pixel carry */

    /* Window chrome.  Without this the toplevel is undecorated on Mutter:
     * no title bar, no close button, no resize grips — and the --title we
     * were given renders nowhere. */
    struct zxdg_decoration_manager_v1     *deco_mgr;
    struct zxdg_toplevel_decoration_v1    *deco;

    /* Resize.  A surface's size is its buffer's size unless a viewport says
     * otherwise; wp_viewporter is what decouples "the window the user
     * dragged" from "the resolution the guest is scanning out". */
    struct wp_viewporter *viewporter;
    struct wp_viewport   *viewport;

    /*
     * wp_presentation is the ONLY way a Wayland client learns whether its
     * buffer reached a hardware plane: the `presented` event carries
     * KIND_ZERO_COPY, which is set exactly when the compositor scanned the
     * client's own buffer out instead of compositing a copy of it.  That is
     * the same question the X11 backend answers with Present: FLIP / COPY,
     * and until this was wired up the Wayland side could not answer it at all.
     */
    struct wp_presentation *presentation;
    int                     last_scanout;   /* -1 unknown, 0 copy, 1 zero-copy */
    uint32_t                pres_clock;     /* clockid_t of presented events   */

    /*
     * virtio-nvgpu: TEARING CONTROL.  With the ASYNC hint a compositor that
     * allows tearing (Hyprland: general:allow_tearing = true plus an
     * `immediate` window rule) flips our buffer the moment it is committed
     * instead of at the next vblank -- the lowest latency a window can get.
     * It is a HINT: without the compositor-side opt-in it changes nothing.
     */
    struct wp_tearing_control_manager_v1 *tearing_mgr;
    struct wp_tearing_control_v1         *tearing;
    struct wp_tearing_control_v1         *cs_tearing;  /* the picture subsurface */
    /*
     * CONTENT TYPE "game" while presenting immediately: newer Hyprland keys
     * direct scanout (render:direct_scanout = 2) and VRR (misc:vrr = 3) off
     * it, KWin its fullscreen latency policy.  A hint like tearing control.
     */
    struct wp_content_type_manager_v1    *ctype_mgr;
    struct wp_content_type_v1            *ctype, *cs_ctype;
    /*
     * HYPRLAND.  Its tearing needs an `immediate` window rule on top of the
     * client's hint; the viewer sets that on its OWN window at runtime
     * (`setprop pid:N immediate 0|1 lock` over the IPC socket), never in a
     * config file.  hypr_want/hypr_sent: -1 unknown.
     */
    const char *hypr_sig;
    int         hypr_want, hypr_sent;
    bool        hypr_failed;
    /* The letterbox's black pixel, when the compositor offers it as a
     * solid colour rather than a buffer to sample (see wl_blk_build()). */
    struct wp_single_pixel_buffer_manager_v1 *spb_mgr;

    /*
     * virtio-nvgpu: --stats.  Per-second fps and latency, all CLOCK_MONOTONIC:
     *   recv->commit    ATTACH read off the socket -> wl_surface_commit
     *   send->commit    the sender's own timestamp (cmd.seq, --seq-usec)
     *   commit->present wl_surface_commit -> wp_presentation `presented`
     *   send->present   end to end, sender -> photons-ish (scanout start)
     */
    uint64_t st_t0;
    unsigned st_commits, st_presented, st_discarded, st_zc;
    uint64_t st_rc_sum, st_rc_max;          /* ns */
    uint64_t st_sc_sum; unsigned st_sc_n;   /* us */
    uint64_t st_cp_sum, st_cp_max; unsigned st_cp_n; /* ns */
    uint64_t st_sp_sum; unsigned st_sp_n;   /* us */
    /* Distributions for the stats line: commit->present samples, and the
     * interval between consecutive presents (frame pacing), in us. */
#define NB_ST_SAMPLES 1024
    uint32_t st_cp_s[NB_ST_SAMPLES]; unsigned st_cp_k;
    uint32_t st_pi_s[NB_ST_SAMPLES]; unsigned st_pi_k;
    uint64_t st_last_pres;                  /* ns, last presented time   */
    struct nb_pres_ctx *pc_last;            /* feedback of the latest commit */

    /*
     * THE OUTPUT'S SCALE, IN 120THS -- and it is not cosmetic.
     *
     * A Wayland surface is measured in LOGICAL pixels.  On this GNOME the
     * output is 3840x2160 at scale 1.5, so a fullscreen surface measures
     * 2560x1440 -- and telling the guest to render 2560x1440 would produce a
     * buffer that does NOT cover the 3840x2160 output, which is exactly the
     * condition the compositor requires before it will scan it out directly.
     * The guest must be told the PHYSICAL size.
     *
     * 120 is the protocol's fixed-point base, and 120 (i.e. 1.0) is the right
     * default: a compositor that does not offer wp_fractional_scale_v1 is
     * telling us logical and physical are the same thing.
     */
    struct wp_fractional_scale_manager_v1 *frac_mgr;
    struct wp_fractional_scale_v1         *frac;
    uint32_t                               scale_120;

    /* Client-side title bar.  A SUBSURFACE ABOVE the content, at negative y,
     * so it never overlaps the guest's picture -- an occluded surface is not a
     * scanout candidate, and the whole point of this backend is that it can
     * be one. */
    struct wl_subcompositor *subcomp;
    struct wl_surface       *tb_surf;
    struct wl_subsurface    *tb_sub;
    /*
     * THE CLOSE CONFIRMATION.  A subsurface above the content, drawn with the
     * same shm + 5x7 font the title bar and placeholder already use -- no
     * toolkit, no new dependency, and the hit-testing is the same shape as the
     * title bar's three buttons.
     *
     * It does NOT decide anything about the VM.  Three of its four choices
     * send a message and let QEMU act; the fourth (close the display only) is
     * purely local to the broker and touches no guest.  The privileged process
     * still holds no VM policy -- it asks the human which message to send.
     */
    struct wl_surface       *dlg_surf;
    struct wl_subsurface    *dlg_sub;
    struct wl_buffer        *dlg_buf;
    void                    *dlg_px;
    size_t                   dlg_sz;
    bool                     dlg_open;
    /* We have already sent a close request and are still here.  The broker
     * cannot know WHY -- that is the VMM's and the guest's business -- but
     * "you already asked and the window is still open" is its own history and
     * is worth saying, because the QEMU log where the reason appears is not
     * something the person clicking the button can see. */
    bool                     close_asked;
    bool                     ptr_on_dlg;
    bool                     ptr_on_content;   /* over the guest's picture */
    bool                     cur_img_on;       /* ...and the host pointer shows
                                                * an image there (not NULL) */
    int                      dlg_hover, dlg_press;
    int                      dlg_px_x, dlg_px_y;
    struct wl_buffer        *tb_buf;
    void                    *tb_px;
    size_t                   tb_sz;
    int                      tb_w;
    bool                     ptr_on_tb;
    bool                     tb_mapped;
    int                      tb_hover;      /* button under the pointer, -1 */
    int                      tb_press;      /* button held down, -1 for none */
    /* Our own pointer cursor, for when the pointer is on our chrome.  Drawn
     * here rather than loaded: libwayland-cursor would be a new dependency
     * and a theme lookup, for one arrow that is only ever seen over a 28px
     * bar. */
    struct wl_surface       *cur_surf[NB_CUR_N];
    /* Per-cursor hotspot, in SURFACE coordinates (so already magnified).  It
     * used to be a single hardcoded 6,5 for all four resize cursors while
     * cur_make() took the real ones and dropped them -- so the glyph was drawn
     * offset from the actual pointer, and for the 7px-wide bottom cursor 6 was
     * nearly off the image.  Aiming with a cursor that lies about where it
     * points is most of "the resize edge is hard to hit". */
    int                      cur_hx[NB_CUR_N], cur_hy[NB_CUR_N];
    struct wl_buffer        *cur_buf[NB_CUR_N];
    void                    *cur_px[NB_CUR_N];
    size_t                   cur_sz[NB_CUR_N];

    /*
     * RESIZE BORDERS.  A Wayland client has no window edges: the compositor
     * draws no frame and the surface stops exactly where the content does, so
     * there is nothing outside it to grab.  These eight surfaces ARE the
     * edges -- invisible (a single transparent pixel stretched by a viewport),
     * present only to take a pointer and turn a drag into
     * xdg_toplevel_resize().
     */
    /*
     * LETTERBOX BACKGROUND.  In aspect mode the viewport destination is
     * smaller than the window in one dimension, and Wayland has no background
     * colour -- something has to actually occupy the remainder or the desktop
     * shows through.  This was one opaque black pixel stretched by its own
     * viewport around the main surface; it is no longer mapped (the main
     * surface is the bars now, see cs_surf).
     */
    struct wl_surface    *bg_surf;
    struct wl_subsurface *bg_sub;
    struct wp_viewport   *bg_vp;
    struct wl_buffer     *bg_buf;
    void                 *bg_px;
    bool                  bg_mapped;
    int                   off_x, off_y;   /* content offset inside the window */

    /*
     * What bd_layout() last positioned the borders from.  They are children of
     * the CONTENT surface and offset by the letterbox, so they depend on
     * off_x/off_y -- which bg_layout() computes from inside wl_viewport_apply(),
     * on the per-frame path as well as the resize path.  Recording the inputs
     * lets bd_layout() be called from both and do nothing when nothing moved.
     */
    int                   bd_at_ox, bd_at_oy, bd_at_w, bd_at_h;
    bool                  bd_at_tb, bd_at_fs, bd_at_valid;
    struct wl_surface    *bd_surf[8];
    struct wl_subsurface *bd_sub[8];
    struct wp_viewport   *bd_vp[8];
    struct wl_buffer     *bd_buf;       /* one transparent pixel, shared */
    void                 *bd_px;
    int                   bd_hot;       /* border under the pointer, -1 none */
    int                   fit_w, fit_h; /* aspect-fit size when scaling      */
    int                   scale_mode;   /* 0 never, 1 always, 2 auto         */
    bool                     maximized;
    int                      tb_px_x, tb_px_y;   /* pointer, title-bar-local */
    uint32_t                 last_serial;
    char                     title[128];
    bool                     quit;

    /*
     * CLIPBOARD.  wl_data_device is core Wayland, so no extra protocol XML.
     *
     * The compositor only delivers a selection offer to the FOCUSED client,
     * which is not a limitation to work around -- it is the same rule the
     * clipboard policy states, enforced a layer down.  wlr-data-control would
     * read the selection while unfocused; GNOME does not offer it, and wanting
     * it would mean the design had drifted.
     */
    struct wl_data_device_manager *ddm;
    struct wl_data_device         *ddev;
    struct wl_data_offer          *offer;      /* current selection, if text */
    int                            offer_text; /* best text mime it has, as a
                                                * rank (nb_wl_text_mime), 0 = none */
    /* data_offer precedes the selection/enter event that says what the object
     * is for.  Keep that candidate separate so an ignored DnD offer cannot
     * replace the clipboard selection. */
    struct wl_data_offer          *pending_offer;
    int                            pending_offer_text;
    bool                           offer_own;         /* selection is ours */
    bool                           pending_offer_own;
    struct wl_data_source         *source;     /* ours, when we own it       */
    char                          *src_text;   /* what we would send         */
    size_t                         src_len;
    int                            fetch_fd;   /* pipe being read, or -1     */
    uint64_t                       fetch_generation;
    uint64_t                       fetch_deadline_ms;  /* see wl_tick_fetch */
    uint64_t                       clip_notice_until;  /* title-bar notice   */
    /*
     * A guest copy is only PUT ON THE HOST CLIPBOARD while this window has
     * keyboard focus.  Wayland enforces half of that anyway -- setting a
     * selection needs a serial from a real input event -- but the policy is
     * the point: a background VM must not be able to reach across and replace
     * what you just copied somewhere else.
     *
     * Dropping it instead would be worse than it sounds: the guest believes
     * the copy succeeded, so nothing re-sends it, and the text only reappears
     * if the user copies something ELSE in the guest.  So hold the last one
     * and apply it on focus-in.
     */
    bool                           focused;
    char                          *clip_pending;
    size_t                         clip_pending_len;
    char                          *fetch_buf;  /* fetch_cap + 1, per fetch   */
    size_t                         fetch_cap;
    size_t                         fetch_len;
    int                            fetch_pfd;  /* index in w->pfd, or -1     */
    /*
     * Serving OUR selection (the guest's text) to a host requester.  Up to
     * 1 MiB does not fit a pipe in one write, and the requester paces the
     * read, so what does not go at once is finished from the poll loop --
     * never by blocking the one thread that also carries the keyboard.
     */
#define NB_WL_MAX_SENDS 4
    struct {
        int    fd;                  /* -1 = free slot                      */
        char  *buf;                 /* the remainder, owned                */
        size_t len, off;
        int    pfd;                 /* index in w->pfd, or -1              */
    } send[NB_WL_MAX_SENDS];

    struct nb_formats formats;

    /* The sink is not reachable from Wayland callbacks otherwise.  NULL until
     * the first dispatch(), so every callback must tolerate that. */
    struct nb_sink *sink;
    struct nb_session *sess;
    /* The pollfd main filled for us, so dispatch can tell whether the
     * compositor fd is actually readable.  Valid only between a
     * wl_pollfds() and the dispatch() that follows it. */
    struct pollfd *pfd;
    bool   flush_blocked;       /* the compositor socket would not take it all */

    struct nb_wl_buf bufs[NB_MAX_BUFS];
    uint64_t tick;
    int      pending;           /* index of the ATTACHed-but-not-COMMITted buf */
    int      current;           /* index of the buffer the surface holds       */
    bool     frame_inflight;
    /*
     * THE COMMIT GATE (nb_pace.h).  A frame the compositor cannot use yet
     * (socket backed up, enough commits ahead of the frame callback) stays
     * staged in `pending`; a newer ATTACH replaces it and releases it, and
     * the staged one goes out when the gate opens (wl_commit_staged()).
     */
    struct nb_pace pace;
    bool     commit_staged;
    /*
     * --sync=latch (nb_latch.h): at most one commit per output refresh, the
     * newest frame, at a learned point before the vblank it is aimed at.  The
     * window opening is a CLOCK_MONOTONIC timerfd (poll() alone has
     * millisecond resolution, a quarter of a 240 Hz refresh).
     */
    struct nb_latch latch;
    int      latch_tfd;         /* -1 until first needed                    */
    bool     latch_armed;
    int      latch_pfd;         /* index in the pollfds, -1 when not polled */
    uint64_t latch_open_ns;     /* when the current window opened           */
    /* CONDUIT_VIEWER_FRAMELOG: one line per frame event (see wl_flog()). */
    FILE    *flog;
    uint64_t flog_flush_ns;
    bool     nudge_pending;     /* a cursor nudge the gate held (nb_pace.h) */
    unsigned st_nudges;
    uint64_t n_reuse_inflight;  /* commits of a buffer the compositor still
                                 * held -- see wl_commit()                     */

    /* THREE sizes, deliberately not one:
     *   buf_w/buf_h   the guest's scanout buffer — the resolution it chose
     *   win_w/win_h   what the compositor last configured for the window
     *   surf_w/surf_h what the surface currently measures, which is win_*
     *                 when a viewport is scaling and buf_* when it is not
     * Conflating them is what makes a user's window drag turn into a guest
     * mode switch, which is not what dragging a window means. */
    int    buf_w, buf_h;
    int    win_w, win_h;
    int    wnd_w, wnd_h;        /* last WINDOWED content size (0 = none yet) */
    int    surf_w, surf_h;
    /* A FOURTH, and it is the only one EV_SURFACE may ever carry: the window's
     * CONTENT size -- win_* less the decorations we draw.  surf_* cannot serve
     * because, per the note above, it becomes buf_* whenever no viewport is
     * scaling: reporting it then tells the guest to render the size it just
     * rendered, and on a fullscreen window the two disagree, so they trade
     * places forever.  OBSERVED on a 4K display at scale 1.5: the guest flipped
     * between 1920x1080 and 3760x2118 about every two seconds, indefinitely.
     * (2160 - 42 is the 28px title bar at 1.5x, hence the odd number.) */
    int    cont_w, cont_h;
    /* The last hint actually sent, so an unchanged one is not resent. */
    unsigned hint_w, hint_h;
    /* The host output's refresh in MILLIHERTZ, learned from presentation
     * feedback.  0 until the first frame has been presented -- there is no
     * other source: the broker does not bind wl_output. */
    unsigned refresh_mhz;
    uint32_t deco_mode;
    bool   grabbed;
    bool   fullscreen;
    bool   configured;
    /* xdg_toplevel.configure, recorded until xdg_surface.configure. */
    bool    tc_pending, tc_fs, tc_max;
    int32_t tc_w, tc_h;

    /*
     * virtio-nvgpu VIEWER STATE.
     *
     * Window geometry, set EXPLICITLY.  Without it a compositor derives the
     * window from the bounding box of every subsurface -- including the
     * invisible resize borders hanging NB_BORDER px outside and the letterbox
     * background -- and Hyprland then placed and clipped the content by that
     * box: REPORTED as smeared columns down the left of the picture, a black
     * strip, and the host desktop showing through on the right.
     */
    int      geo_x, geo_y, geo_w, geo_h;
    bool     geo_valid;

    /*
     * THE PICTURE'S SURFACE.  Hyprland crops a window's MAIN surface by its
     * window geometry (to cut client-side shadows), computing texture
     * coordinates from geometry / surface size -- so any part of the window
     * the main surface does not cover reads outside the texture: smeared edge
     * columns, and the desktop showing through.  So the main surface ALWAYS
     * covers the whole window:
     *   exact fit  -> the guest's buffer IS the main surface (and fullscreen
     *                 at the output's mode can be scanned out directly)
     *   letterbox  -> the main surface is one black pixel stretched over the
     *                 window, and the guest's buffer goes on cs_surf, a
     *                 desynchronised subsurface at (off_x, off_y)
     *
     * SUBSURFACE STACKING: by creation order only, never by
     * wl_subsurface.place_above/place_below.  The protocol puts a new
     * subsurface on top of its parent and of every sibling made before it,
     * which is exactly the order wanted here: cs_surf first, then the
     * overlay, the UI layers and the close dialog as they are first needed.
     * Restacking requests are where compositors differ: one in wide use
     * matches the sibling a placement names against the wrong list, so every
     * placement lands at the bottom of the stack -- with the resize borders
     * placed below the parent first, the picture and the overlay ended up
     * under the opaque black main surface and the window was black.  Creation
     * order needs no request at all, so there is nothing to get wrong.
     */
    struct wl_surface    *cs_surf;
    struct wl_subsurface *cs_sub;
    struct wp_viewport   *cs_vp;
    bool                  in_sub;
    int                   cs_at_x, cs_at_y;
    struct wl_buffer     *blk_buf;      /* 1x1 opaque black */
    void                 *blk_px;
    int                   main_vw, main_vh;   /* main's destination, in_sub */
    int                   tgt_ow, tgt_oh;     /* opaque region last set     */
    bool                  main_dirty;   /* parent state awaits a commit    */
    struct wl_surface    *ptr_surf;     /* what the pointer last entered   */
    bool     bd_disabled;       /* server-side decorations: no own borders */

    /* Mode hints (EV_MODE_HINT): what was last asked, and a pending one. */
    int      resize_mode;       /* NB_RESIZE_*                              */
    unsigned hint_align;
    bool     mh_valid;
    unsigned mh_w, mh_h, mh_hz, mh_why;
    uint64_t mh_due_ms;         /* debounced windowed hint, 0 = none        */

    /* Frame statistics: the title (2/s) and the overlay (4/s). */
    struct nb_fstats fs;
    struct nb_fsnap  snap;
    uint64_t fs_next_ms, title_next_ms;
    char     title_cur[256];
    char     tb_stats[64];      /* ASCII digest for our own title bar      */
    bool     stats_active;

    /* The overlay: a small shm subsurface over the top-left of the window. */
    struct wl_surface    *ov_surf;
    struct wl_subsurface *ov_sub;
    struct wl_buffer     *ov_buf[2];
    bool                  ov_busy[2];
    void                 *ov_px;
    size_t                ov_sz;
    unsigned              ov_w, ov_h, ov_sc;
    int                   ov_at_x, ov_at_y;
    int                   ov_mode;      /* NB_OVERLAY_*                     */
    bool                  ov_user;      /* CTRL+ALT+O                       */
    bool                  ov_mapped;

    /* Direct mode (CTRL+ALT+D). */
    bool        direct;
    const char *direct_hook;
    bool        tearing_async;  /* what was last requested */
    bool        has_content;    /* a buffer was committed: the window maps */

    /*
     * THE FULLSCREEN SCALER (nb_gpuscale.h).  In fullscreen with immediate
     * presentation, a picture that is not an exact fit is drawn by one GPU
     * pass into an output-size buffer on the main surface, the only shape a
     * compositor tears or scans out.  gs_mode: --gpu-scale (NB_GS_*).
     */
    int             gs_mode;
    struct nb_gs   *gs;
    bool            gs_dead;        /* failed: never again this session    */
    bool            gs_frame;       /* this commit is a scaled frame       */
    bool            gs_dropped;     /* ...that had no free target: skipped */
    bool            gs_shown;       /* the main surface shows a target     */
    bool            gs_logged;
    struct nb_wl_gt {
        struct nb_wl *owner;
        struct wl_buffer *buf;
        bool ready, busy, failed;
        unsigned gen;               /* bumped on reallocation */
    } gt[NB_GS_TARGETS];
    uint32_t        gt_w, gt_h;
    unsigned        gt_fb_gen;      /* fb_gen the targets were made for */
    int             gt_last;
    /* READ-BACK targets: the compositor takes no layout the scaler's GPU
     * renders to, so each frame is read back into this shm (nb_gpuscale.h
     * nb_gs_targets_readback).  gt_want_rb: the compositor refused the
     * dma-buf targets, use read-back from the next allocation. */
    bool            gt_rb, gt_want_rb;
    void           *gt_shm;
    size_t          gt_shm_sz;
    uint64_t        st_gs_sum, st_gs_max; unsigned st_gs_n, st_gs_drop;
    unsigned        st_held;        /* commits the gate held back       */
    /* The GPU copy for layouts the compositor does not take (nb_blit.h):
     * what the viewer's GPU said per pair, and frames copied this second. */
#define NB_COPY_CACHE 8
    struct { uint32_t fourcc; uint64_t mod; bool ok; } copy_v[NB_COPY_CACHE];
    unsigned        copy_n;
    unsigned        st_copy;
    bool            copy_told;
    /* Which path a fullscreen frame takes, measured (nb_scanout.h). */
    struct nb_so    so;
    /*
     * THE PRESENT LEVEL (nb_present.h): zero-copy, gpu-copy or readback, and
     * how far --present-mode lets a hard failure take it.
     */
    struct nb_pc    pc;
    bool            pc_noimport_told;
    /*
     * EXPLICIT SYNC (wp_linux_drm_syncobj_v1, nb_xsync.h).  A surface that
     * carries a dma-buf gets a syncobj extension; one about to carry
     * anything else (the letterbox's black pixel, shared memory) loses it
     * first, because only dma-bufs are guaranteed to take explicit sync.
     * Acquire points share one timeline; each buffer the compositor may hold
     * has its own release timeline (nb_present.h nb_rel).  While a surface
     * has the extension, wl_buffer.release is undefined for it: a buffer is
     * free when its release point is signalled.
     */
    struct wp_linux_drm_syncobj_manager_v1 *xs_mgr;
    struct nb_xs   *xs;
    bool            xs_off;         /* not offered, turned off, or failed  */
    struct wp_linux_drm_syncobj_surface_v1 *xs_surf[2];  /* main, picture */
    struct nb_wl_xa xs_pend[2];
    struct nb_wl_tl xs_acq;
    uint64_t        xs_acq_pt;
    struct nb_wl_tl xs_buf_tl[NB_MAX_BUFS];
    struct nb_wl_tl xs_gt_tl[NB_GS_TARGETS];
    struct nb_rel   rel[NB_MAX_BUFS];       /* per guest slot, never reset */
    struct nb_rel   gt_rel[NB_GS_TARGETS];
    uint64_t        buf_acq[NB_MAX_BUFS];   /* acquire point of its content */
    uint64_t        gt_acq[NB_GS_TARGETS];
    int             xs_pfd;         /* index in the pollfds, -1 none       */
    unsigned        st_xs;          /* commits explicitly synced, per second */
    bool            xs_dmabuf_fence;/* the acquire uses the dma-buf's fence */

    /*
     * DMA-BUF FEEDBACK (zwp_linux_dmabuf_v1 v4) for the main surface: the
     * compositor's tranches, one of them flagged SCANOUT when it could put
     * this surface on a plane with those formats.  A SECOND binding of the
     * global, at version 4, used only for get_surface_feedback: the format
     * set itself still comes from the version 3 binding (w->dmabuf), whose
     * format/modifier events version 4 no longer sends.
     */
    struct zwp_linux_dmabuf_v1          *dmabuf4;
    struct zwp_linux_dmabuf_feedback_v1 *dfb;
    void           *fb_tab;         /* the format table, mmap'ed       */
    size_t          fb_tab_sz;
    struct nb_so_fmts fb_tr_fmts;   /* the tranche being received      */
    uint32_t        fb_tr_flags;
    dev_t           fb_tr_dev;      /* this tranche's target device, 0 unknown */
    struct nb_so_fmts fb_pend_scan; /* scanout tranches of this batch  */
    bool            fb_pend_has;
    unsigned        fb_pend_tr;
    struct nb_so_fmts fb_scan;      /* the last complete batch         */
    bool            fb_has_scan, fb_got;
    /* The other tranches (the compositor's main device: what it imports to
     * render with), where the GPU copy allocates (nb_pc_targets). */
    struct nb_so_fmts fb_pend_render, fb_render;
    bool            fb_pend_has_render, fb_has_render;
    dev_t           fb_main_dev;    /* the compositor's main device, 0 unknown */
    bool            gt_fs;          /* targets were made for fullscreen    */
    /* Every pair the compositor advertised, before the guest-format and
     * --present-mode filters: tells "advertised nothing" from "nothing we
     * can use". */
    unsigned        n_adv_raw;
    unsigned        fb_tr;
    unsigned        fb_gen;         /* bumped when the scanout set changes */
    /* WLR_NO_HARDWARE_CURSORS=1 in our environment: the session draws
     * cursors in software (reported, never acted on). */
    bool            env_sw_cursor;

    /* The guest's cursor, as the host pointer's image (CMD_CURSOR). */
    struct wl_surface  *gc_surf;
    struct wp_viewport *gc_vp;
    struct {
        bool      valid;
        uint64_t  id, used;
        uint32_t  w, h, stride, offset;
        uint64_t  mod;
        struct wl_buffer *buf;
    } gc[4];
    int       gc_cur;           /* slot on the cursor surface, -1 none     */
    bool      gc_on;            /* the guest shows a cursor                */
    unsigned  gc_hx, gc_hy;     /* hotspot, guest pixels                   */
    int       gc_lhx, gc_lhy;   /* ...and in surface (logical) pixels      */
    int       gc_k1000;         /* scale it was laid out at, x1000         */
    uint32_t  gc_gen;
    uint64_t  gc_tick;
    bool      gc_told_fail;
    /* A display that does not take the cursor's LINEAR ARGB8888 dma-buf
     * (Hyprland 0.47 on NVIDIA lists only tiled layouts): the viewer's GPU
     * copies each new image into shared memory.  Opened when first needed. */
    struct nb_gs *gc_gs;
    uint32_t  gc_gs_w, gc_gs_h;     /* its read-back targets' size         */
    bool      gc_gs_failed;         /* no GPU here imports the cursor      */
    bool      gc_broken;            /* the newest image could not be shown */
    uint32_t  ptr_serial;           /* the pointer's last enter: set_cursor */
    bool      ptr_on_pic;           /* last cur_apply: over the picture    */
    /*
     * Under grab the host pointer is locked and shows nothing, so the viewer
     * draws the guest cursor itself, at the position the guest reports
     * (CMD_CURSOR_POS): `spr_surf` holds the cursor's buffer, a desync
     * subsurface of `spr_anchor` -- a 1x1 transparent desync subsurface of
     * the main surface at 0,0 -- so a move is a commit of the anchor alone,
     * never of the picture.  Mapped only while it is drawn; no input.
     */
    struct wl_surface    *spr_anchor, *spr_surf;
    struct wl_subsurface *spr_anchor_sub, *spr_sub;
    struct wp_viewport   *spr_vp;
    struct wl_buffer     *spr_anchor_buf;
    struct wl_buffer     *spr_buf;      /* attached to spr_surf now        */
    struct nb_rect        spr_at;       /* where it was put, window coords */
    bool      spr_mapped;
    bool      spr_kick;             /* (un)mapped: the picture must repaint */
    struct nb_rect spr_dmg;         /* ...this much of it, window coords   */
    bool      gpos_valid;           /* a guest pointer position is known   */
    bool      gpos_seen;            /* the guest reported one (this client)*/
    int       gpos_x, gpos_y;       /* ...in guest pixels, the hotspot's   */

    /*
     * Display settings (nb_view.h) and where they last put the picture.
     * `place` is what the picture's viewport was given, so the pointer is
     * mapped through exactly the rectangle that is on screen.
     */
    const struct nb_config *cfg;
    struct nb_vstore vst;
    struct nb_place  place;
    bool             vp_src[2];     /* source rect set: [0] main, [1] cs   */

    /* The viewer's own UI layers (nb_ui.h): subsurfaces with shm buffers. */
    struct nb_ui     ui;
    struct nb_vmpause vmp;          /* the menu's Pause / Resume       */
    struct nb_wl_layer *ly_menu, *ly_note, *ly_btn;
    uint64_t note_until_ms;         /* notice visible until, 0 = hidden    */
    char     note_txt[64];
    double   btn_alpha;             /* the top-edge button's fade, 0..1    */
    bool     btn_want, btn_hot;
    uint64_t btn_last_ms, btn_seen_ms;
    bool     ptr_on_menu, ptr_on_btn;
    double   ptr_wx, ptr_wy;        /* last pointer, window coordinates     */
    /* The picture the guest pointer was last placed through: when it moves
     * or changes size under a still pointer, the pointer is sent again. */
    struct nb_rect ptr_pic;
    int      ptr_bw, ptr_bh;
    /* The area outline while the menu is open: 4 edges + 4 corners, one
     * shared 1x1 green buffer stretched by viewports. */
    struct wl_surface    *ed_surf[8];
    struct wl_subsurface *ed_sub[8];
    struct wp_viewport   *ed_vp[8];
    struct wl_buffer     *ed_buf;
    void                 *ed_px;
    bool                  ed_mapped;
    struct nb_rect        ed_at;
    int                   ui_cur;   /* NB_UI_CUR_* shown, -1 = not ours    */
    bool                  rehint_pending;   /* a mode hint is owed      */
    uint64_t              rehint_due_ms;    /* ...sent then, 0 = not yet */
    /* The guest's mode changed and its rule changed the layout (re-placed
     * from the UI tick, not the frame path), or the mode list changed. */
    bool                  rule_due;
    char                  rule_note[64];
    bool                  modes_dirty;
};

/* One UI layer: a desynchronised subsurface of the main surface. */
struct nb_wl_layer {
    struct wl_surface    *surf;
    struct wl_subsurface *sub;
    struct wp_viewport   *vp;
    struct wl_buffer     *buf[2];
    bool                  busy[2];
    void                 *px;
    size_t                sz;
    int                   pw, ph;   /* buffer, physical pixels              */
    int                   lw, lh;   /* on screen, logical                   */
    int                   x, y;
    bool                  mapped, input;
};

/* Forward: the title bar lives further down, but wl_commit sizes it. */
static void tb_update(struct nb_wl *w, int width);
/* Forward: the cursor is built lazily, on first entry to our own chrome. */
static void cur_build(struct nb_wl *w);
/* Forward: hide or show the host cursor over the content, per what is on it. */
static void cur_apply(struct nb_wl *w, uint32_t serial);
/* Forward: under grab, draw the guest cursor (or stop drawing it), and make
 * the compositor repaint when it appeared or went. */
static void spr_update(struct nb_wl *w);
static void spr_kick_now(struct nb_wl *w);
/* Forward: the clipboard notice needs a clock and lives with the clipboard. */
static uint64_t nb_now_ms_wl(void);
/* Forward: the invisible resize borders, laid out on every size change. */
static void bd_build(struct nb_wl *w);
/* Forward: the letterbox bars, sized from the content rect wl_viewport_apply
 * just computed.  Defined next to the rest of the shm chrome. */
static void bg_layout(struct nb_wl *w, int dw, int dh);
/* Forward: EV_SURFACE reports PHYSICAL pixels; defined with the scale code. */
static void wl_report_surface(struct nb_wl *w, int lw, int lh);
static void bd_layout(struct nb_wl *w);
static int  bd_index(const struct nb_wl *w, const struct wl_surface *s);
/* Forward: the idle placeholder is released by wl_commit once a real frame is
 * on screen; defined with the rest of the placeholder code. */
static void wl_idle_drop(struct nb_wl *w);
/* Forward: presentation feedback is requested by wl_commit, defined below. */
extern const struct wp_presentation_feedback_listener nb_pres_listener;
/* Forward: virtio-nvgpu viewer pieces, defined after the registry. */
static void wl_geom_apply(struct nb_wl *w);
static void top_apply(struct nb_wl *w);
static void wl_mode_hint(struct nb_wl *w, bool immediate);
static void ov_sync(struct nb_wl *w);
static void ov_paint_commit(struct nb_wl *w);
static void wl_title_update(struct nb_wl *w, bool force);
static bool gc_layout(struct nb_wl *w, bool force);
/* Forward: the display settings and the viewer's own UI, defined after the
 * hotkeys. */
static const struct nb_ui_env *wl_ui_env(struct nb_wl *w);
static void wl_ui_do(struct nb_wl *w, unsigned bits);
static void wl_ui_layer_motion(struct nb_wl *w, double lx, double ly);
static void wl_btn_track(struct nb_wl *w, double wy);
static void wl_ptr_to_guest(struct nb_wl *w);
static void wl_ptr_follow(struct nb_wl *w);
static void wl_ui_reflow(struct nb_wl *w);
static int  wl_tick_ui(struct nb_wl *w, int next);
static void wl_menu_set(struct nb_wl *w, bool open);
static void wl_rehint(struct nb_wl *w);
static void wl_ed_show(struct nb_wl *w, bool on);
static void wl_ed_build(struct nb_wl *w);
static void wl_hypr_sync(struct nb_wl *w);
static bool wl_async_wanted(const struct nb_wl *w);
static void wl_sync_changed(struct nb_wl *w, const char *why);
/* Forward: the fullscreen path decision (nb_scanout.h), defined with the
 * scaler. */
static int wl_guest_scanout(const struct nb_wl *w, const struct nb_wl_buf *sl);
static const char *wl_so_shape(const struct nb_wl *w);

/* ── virtio-nvgpu: per-frame presentation context and --stats ─────────── */

struct nb_pres_ctx {
    struct nb_wl *w;
    uint64_t commit_ns;     /* CLOCK_MONOTONIC at wl_surface_commit          */
    uint32_t seq;           /* cmd.seq of the committed frame                */
    uint64_t start_ns;      /* the guest's flip (or our receipt of it)       */
    uint64_t recv_ns;       /* our receipt of the frame                      */
    uint64_t aimed;         /* --sync=latch: the vblank it was aimed at       */
    unsigned so_gen;        /* nb_scanout probe generation at commit         */
    int      so_path;       /* NB_SO_ZC or NB_SO_GPU: how it was shown       */
};

/*
 * CONDUIT_VIEWER_FRAMELOG=FILE: per-frame timing, CLOCK_MONOTONIC ns, for
 * frame-to-photon analysis (joins with the backend's CONDUIT_FRAME_PROBE log
 * by seq):
 *   C seq recv flip commit aimed mode   committed (flip 0 if unstamped, aimed 0
 *                                       unless --sync=latch; mode i/v/l)
 *   P seq present refresh flags         wp_presentation presented
 *   D seq                               discarded (replaced before scanout)
 *   X seq recv                          staged, replaced before it was committed
 */
static void wl_flog(struct nb_wl *w, const char *fmt, ...)
    __attribute__((format(printf, 2, 3)));
static void wl_flog(struct nb_wl *w, const char *fmt, ...)
{
    va_list ap;
    uint64_t now;

    if (!w->flog) {
        return;
    }
    va_start(ap, fmt);
    vfprintf(w->flog, fmt, ap);
    va_end(ap);
    now = nb_mono_ns();
    if (now - w->flog_flush_ns > 1000000000ull) {
        fflush(w->flog);
        w->flog_flush_ns = now;
    }
}

/*
 * The guest's flip time, from cmd.seq when the client stamps it with its
 * CLOCK_MONOTONIC microseconds (low 32 bits): widened against our own receipt
 * time, which is a few microseconds later on the same clock.  0 when the
 * client does not stamp, or the stamp is not plausibly recent.
 */
static uint64_t nb_flip_ns(const struct nb_wl *w, const struct nb_wl_buf *sl)
{
    uint64_t recv_us;
    uint32_t back;

    if (!sl->recv_ns || !nb_sink_seq_usec(w->sink)) {
        return 0;
    }
    recv_us = sl->recv_ns / 1000u;
    back = (uint32_t)recv_us - sl->seq;          /* mod 2^32 */
    if (back > 1000000u) {
        return 0;
    }
    return (recv_us - back) * 1000u;
}

static uint32_t nb_mono_us32(void)
{
    return (uint32_t)(nb_mono_ns() / 1000u);
}

static int nb_u32_cmp(const void *a, const void *b)
{
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;

    return x < y ? -1 : x > y;
}

/* p50 / p99 of n samples (sorted in place), us. */
static void nb_pct(uint32_t *v, unsigned n, uint32_t *p50, uint32_t *p99)
{
    *p50 = *p99 = 0;
    if (!n) {
        return;
    }
    qsort(v, n, sizeof(*v), nb_u32_cmp);
    *p50 = v[n / 2];
    *p99 = v[(n * 99) / 100 < n ? (n * 99) / 100 : n - 1];
}

static void nb_stats_sample(uint32_t *v, unsigned *k, uint64_t ns)
{
    if (*k < NB_ST_SAMPLES) {
        v[(*k)++] = ns / 1000u > UINT32_MAX ? UINT32_MAX : (uint32_t)(ns / 1000u);
    }
}

static bool wl_latch_on(const struct nb_wl *w);
/* The presentation mode for the stats line; latch mode with its state. */
static const char *wl_mode_name(const struct nb_wl *w, char *b, size_t n)
{
    const struct nb_latch *l = &w->latch;

    if (!wl_latch_on(w)) {
        return w->tearing_async ? "immediate" : "vsync";
    }
    snprintf(b, n, "latch lead %.2fms budget %.2fms on-time %llu late %llu repeats %llu%s",
             l->lead / 1e6, l->budget / 1e6, (unsigned long long)l->on_time,
             (unsigned long long)l->late, (unsigned long long)l->repeats,
             nb_latch_clocked(l) ? "" : " (no vblank clock yet)");
    return b;
}

/*
 * DIRECT SCANOUT, FROM OUR SIDE.  What a compositor needs from a client to
 * put its buffer on a plane is the same everywhere (wlroots, KWin, Mutter,
 * Hyprland, X compositors unredirecting): one opaque surface, fullscreen,
 * nothing mapped above or below it, a buffer exactly the output's size with
 * no scaling or crop, and no cursor image over it (a software cursor has to
 * be composited in).  This lists the ones the viewer itself is breaking
 * right now; "none" means the compositor chose to composite for reasons of
 * its own (its configuration, tearing, a format it cannot scan out).
 * Generic by construction: it inspects only our own surface state.
 */
static void wl_scanout_blockers(const struct nb_wl *w, char *b, size_t n)
{
    size_t k = 0;
    unsigned s120 = w->viewporter && w->scale_120 ? w->scale_120 : 120;
    int pw = (int)(((long)w->win_w * s120 + 60) / 120);
    int ph = (int)(((long)w->win_h * s120 + 60) / 120);
    int bw = 0, bh = 0;

#define BLK(cond, what) do { if ((cond) && k < n) { \
        k += (size_t)snprintf(b + k, n - k, "%s%s", k ? ", " : "", what); \
        if (k >= n) k = n - 1; } } while (0)
    b[0] = '\0';
    if (w->gs_shown) {
        bw = (int)w->gt_w;
        bh = (int)w->gt_h;
    } else if (!w->in_sub && w->current >= 0 && w->bufs[w->current].valid) {
        bw = w->place.cropped ? 0 : (int)w->bufs[w->current].w;
        bh = w->place.cropped ? 0 : (int)w->bufs[w->current].h;
    }
    BLK(!w->fullscreen, "not fullscreen");
    BLK(w->in_sub, "picture letterboxed on a subsurface");
    BLK(!w->in_sub && (bw != pw || bh != ph), "buffer is not the output's size");
    BLK(w->tb_mapped, "title bar mapped");
    BLK(w->ov_mapped, "stats overlay mapped");
    BLK(w->ly_note && w->ly_note->mapped, "notice mapped");
    BLK((w->ly_menu && w->ly_menu->mapped) || w->ui.open || w->dlg_open,
        "menu or dialog open");
    BLK(w->ly_btn && w->ly_btn->mapped, "menu button mapped");
    BLK(w->ptr_on_content && w->cur_img_on, "a cursor image over the picture");
    BLK(w->spr_mapped, "the guest cursor drawn over the picture (grab)");
    if (!w->gs_shown && w->current >= 0 && w->bufs[w->current].valid &&
        wl_guest_scanout(w, &w->bufs[w->current]) == 0) {
        BLK(true, "the guest's format/modifier is not in the compositor's "
                  "scanout tranche");
    }
#undef BLK
    if (!k) {
        snprintf(b, n, "none on our side (compositor policy%s)",
                 w->env_sw_cursor ? "; WLR_NO_HARDWARE_CURSORS is set: a "
                 "software cursor on the output prevents it" : "");
    }
}

static void nb_stats_flush(struct nb_wl *w, uint64_t now)
{
    if (!w->st_t0) {
        w->st_t0 = now;
        return;
    }
    if (now - w->st_t0 < 1000000000ull) {
        return;
    }
    if (nb_stats && (w->st_commits || w->st_presented)) {
        double secs = (double)(now - w->st_t0) / 1e9;
        char sc[48] = "", sp[48] = "";

        if (w->st_sc_n) {
            snprintf(sc, sizeof(sc), " send->commit %.0fus",
                     (double)w->st_sc_sum / w->st_sc_n);
        }
        if (w->st_sp_n) {
            snprintf(sp, sizeof(sp), " send->present %.0fus",
                     (double)w->st_sp_sum / w->st_sp_n);
        }
        uint32_t cp50, cp99, pi50, pi99;
        double pi_mean = 0, pi_sd = 0;
        unsigned i;
        char gs[200] = "";

        if (w->st_gs_n || w->st_gs_drop) {
            snprintf(gs, sizeof(gs), " | gpu-scale %u frames, pass avg %.0fus "
                     "max %lluus, %u dropped | path: %s on %s%s", w->st_gs_n,
                     w->st_gs_n ? (double)w->st_gs_sum / w->st_gs_n : 0.0,
                     (unsigned long long)w->st_gs_max, w->st_gs_drop,
                     w->st_copy ? "GPU copy (the compositor does not take "
                                  "the guest's format/modifier)"
                                : "GPU pass (fullscreen scaler)",
                     w->gs ? nb_gs_node(w->gs) : "?",
                     w->gt_rb ? ", read back into shared memory" : "");
        } else if (w->st_commits) {
            snprintf(gs, sizeof(gs), " | path: direct attach");
        }

        for (i = 0; i < w->st_pi_k; i++) {
            pi_mean += w->st_pi_s[i];
        }
        if (w->st_pi_k) {
            pi_mean /= w->st_pi_k;
            for (i = 0; i < w->st_pi_k; i++) {
                double d = w->st_pi_s[i] - pi_mean;

                pi_sd += d * d;
            }
            pi_sd = sqrt(pi_sd / w->st_pi_k);
        }
        nb_pct(w->st_cp_s, w->st_cp_k, &cp50, &cp99);
        nb_pct(w->st_pi_s, w->st_pi_k, &pi50, &pi99);
        char lm[96];

        char blk[320] = "";
        char path[320];

        /* Only when something was composited: a direct-scanout second
         * has nothing to explain. */
        if (w->st_presented > w->st_zc) {
            char why[300];

            wl_scanout_blockers(w, why, sizeof(why));
            snprintf(blk, sizeof(blk), " | composited, blockers: %s", why);
        }
        snprintf(path, sizeof(path), " | path: %s [%s: %s]", wl_so_shape(w),
                 nb_so_phase_name(w->so.phase), w->so.why);
        nb_log("stats: commit %.1f fps, presented %.1f fps (%u direct scanout, "
               "%.0f%%), superseded %u | recv->commit avg %.0fus max %.0fus |%s "
               "commit->present avg %.0fus p50 %uus p99 %uus max %.0fus |%s "
               "| present interval avg %.0fus sd %.0fus p50 %uus p99 %uus "
               "| %s, held %u, cursor nudges %u%s%s%s",
               w->st_commits / secs, w->st_presented / secs, w->st_zc,
               w->st_presented ? 100.0 * w->st_zc / w->st_presented : 0.0,
               w->st_discarded,
               w->st_commits ? (double)w->st_rc_sum / w->st_commits / 1e3 : 0.0,
               (double)w->st_rc_max / 1e3, sc,
               w->st_cp_n ? (double)w->st_cp_sum / w->st_cp_n / 1e3 : 0.0,
               cp50, cp99, (double)w->st_cp_max / 1e3, sp,
               pi_mean, pi_sd, pi50, pi99,
               wl_mode_name(w, lm, sizeof(lm)), w->st_held,
               w->st_nudges, gs, path, blk);
    }
    w->st_held = 0;
    w->st_nudges = 0;
    w->st_gs_sum = w->st_gs_max = 0;
    w->st_gs_n = w->st_gs_drop = 0;
    w->st_copy = 0;
    w->st_cp_k = w->st_pi_k = 0;
    w->st_t0 = now;
    w->st_commits = w->st_presented = w->st_discarded = w->st_zc = 0;
    w->st_rc_sum = w->st_rc_max = 0;
    w->st_sc_sum = 0; w->st_sc_n = 0;
    w->st_cp_sum = w->st_cp_max = 0; w->st_cp_n = 0;
    w->st_sp_sum = 0; w->st_sp_n = 0;
}

static void nb_stats_commit(struct nb_wl *w, const struct nb_wl_buf *sl)
{
    uint64_t now = nb_mono_ns();

    if (w->pc_last) {
        w->pc_last->commit_ns = now;
        w->pc_last = NULL;
    }
    w->st_commits++;
    if (sl->recv_ns && now >= sl->recv_ns) {
        uint64_t d = now - sl->recv_ns;

        w->st_rc_sum += d;
        if (d > w->st_rc_max) {
            w->st_rc_max = d;
        }
    }
    nb_fs_commit(&w->fs, nb_flip_ns(w, sl), sl->recv_ns, now);
    if (nb_sink_seq_usec(w->sink)) {
        uint32_t d = nb_mono_us32() - sl->seq;   /* mod 2^32 */

        if (d < 1000000u) {                      /* ignore garbage/wrap */
            w->st_sc_sum += d;
            w->st_sc_n++;
        }
    }
    nb_stats_flush(w, now);
}

/* ── buffers ─────────────────────────────────────────────────────────────── */

static void buf_release(void *data, struct wl_buffer *b)
{
    struct nb_wl_buf *slot = data;
    (void)b;

    /*
     * The compositor has finished reading this buffer.  Telling the client is
     * what lets it recycle without guessing; it is advisory (nvkvm's guest
     * cycles its own bos regardless), which is why a missed release only costs
     * the client an optimisation.
     *
     * With NVKVM_BROKER_CAP_RELEASE_SEQ it is exact: `seq` is the newest
     * ATTACH of this buffer, which this release covers (attach and commit
     * happen back to back, so the newest ATTACH is the newest commit).
     *
     * Not for an explicitly synced commit: then the event is undefined and
     * the release point says when (wl_xs_scan).
     */
    if (!nb_rel_event(&slot->owner->rel[slot - slot->owner->bufs])) {
        return;
    }
    slot->held = false;
    if (slot->owner->sink) {
        nb_sink_release(slot->owner->sink, slot->id, slot->seq);
    }
}
static const struct wl_buffer_listener buf_listener = { .release = buf_release };

static void wl_buf_destroy(struct nb_wl *w, int i)
{
    if (!w->bufs[i].valid) {
        return;
    }
    if (w->bufs[i].buf) {
        wl_buffer_destroy(w->bufs[i].buf);
    }
    if (w->gs) {
        nb_gs_src_drop(w->gs, i);
    }
    if (w->bufs[i].fd >= 0) {
        close(w->bufs[i].fd);
    }
    memset(&w->bufs[i], 0, sizeof(w->bufs[i]));
    /* No release comes for a destroyed wl_buffer; the slot's timeline and
     * its point counter stay for the next buffer. */
    nb_rel_forget(&w->rel[i]);
    w->buf_acq[i] = 0;
}

/*
 * Evict a cache slot for a new import.  Destroyed while the compositor still
 * held it, no wl_buffer.release will come for it any more.  A victim is never
 * the buffer on the surface and prefers buffers already released, so the
 * compositor is done with it at its next repaint: say so now rather than
 * never (NVKVM_BROKER_CAP_RELEASE_SEQ promises a release per buffer).
 */
static void wl_buf_evict(struct nb_wl *w, int i)
{
    if (w->bufs[i].valid && w->bufs[i].held && w->sink) {
        nb_sink_release(w->sink, w->bufs[i].id, w->bufs[i].seq);
    }
    wl_buf_destroy(w, i);
}

/* ops->hold_release: the compositor still reads an earlier commit of `id`,
 * so its coming release covers the dropped ATTACH `seq` too. */
static bool wl_hold_release(struct nb_session *s, uint64_t id, uint32_t seq)
{
    struct nb_wl *w = s->priv;
    int i;

    for (i = 0; i < NB_MAX_BUFS; i++) {
        if (w->bufs[i].valid && w->bufs[i].held && w->bufs[i].id == id) {
            w->bufs[i].seq = seq;
            return true;
        }
    }
    return false;
}

/* An ATTACH this backend accepted but will not show (a probe): released at
 * once unless an earlier commit of the buffer is still held. */
static void wl_drop_attach(struct nb_session *s, const struct nb_buf_desc *d)
{
    if (!(s->caps & NVKVM_BROKER_CAP_RELEASE_SEQ) || !s->sink) {
        return;
    }
    if (!wl_hold_release(s, d->id, d->seq)) {
        nb_sink_release(s->sink, d->id, d->seq);
    }
}

/* ── explicit sync (wp_linux_drm_syncobj_v1, nb_xsync.h) ─────────────────── */
/*
 * WHAT A COMMIT CARRIES.  A dma-buf attached to a surface with the syncobj
 * extension needs an acquire point (the compositor waits for it before it
 * reads) and a release point (it signals that when it is done), set in the
 * same commit cycle.  They are set right before the commit, in
 * wl_surf_commit(), from what wl_attach_sf() recorded: setting them at the
 * attach would leave a release point nobody commits when a relayout attaches
 * again before the commit, and that buffer would never be released.
 *
 *   acquire  a guest frame: the dma-buf's pending writes (nb_xs_from_dmabuf);
 *            the frame is also complete at handover for a Linux guest, whose
 *            KMS head waits for its plane fences before the flip.  A GPU-copy
 *            frame: the pass's fence when the driver exports one, else the
 *            pass was waited for and the point is signalled from the CPU.
 *            Committing the same content again (a relayout, a cursor nudge)
 *            reuses its point.
 *   release  one timeline per buffer; wl_xs_scan() reads them when the
 *            eventfd fires (or from the tick), and that is when a guest
 *            buffer goes back to the guest and a copy target is free.
 */

/* Explicit sync for new commits: offered, wanted, and not given up. */
static bool wl_xs_on(const struct nb_wl *w)
{
    return w->xs_mgr && !w->xs_off && w->cfg && w->cfg->explicit_sync;
}

static struct wl_surface *wl_xs_sf(const struct nb_wl *w, int k)
{
    return k ? w->cs_surf : w->surf;
}

/* Something is waiting on a point: the watcher must keep looking. */
static bool wl_xs_waiting(const struct nb_wl *w)
{
    if (!w->xs) {
        return false;
    }
    for (int i = 0; i < NB_MAX_BUFS; i++) {
        if (nb_rel_waiting(&w->rel[i])) {
            return true;
        }
    }
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        if (nb_rel_waiting(&w->gt_rel[i])) {
            return true;
        }
    }
    return false;
}

/*
 * Points that have been reached: guest buffers back to the guest, copy
 * targets free again.  One SYNCOBJ_QUERY for everything that waits.
 */
static void wl_xs_scan(struct nb_wl *w)
{
    uint32_t h[NB_MAX_BUFS + NB_GS_TARGETS];
    uint64_t v[NB_MAX_BUFS + NB_GS_TARGETS];
    int who[NB_MAX_BUFS + NB_GS_TARGETS];
    unsigned n = 0;

    if (!w->xs) {
        return;
    }
    nb_xs_drain(w->xs);
    for (int i = 0; i < NB_MAX_BUFS; i++) {
        if (nb_rel_waiting(&w->rel[i]) && w->xs_buf_tl[i].h) {
            h[n] = w->xs_buf_tl[i].h;
            who[n++] = i;
        }
    }
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        if (nb_rel_waiting(&w->gt_rel[i]) && w->xs_gt_tl[i].h) {
            h[n] = w->xs_gt_tl[i].h;
            who[n++] = 100 + i;
        }
    }
    if (!n || nb_xs_query(w->xs, h, v, n) < 0) {
        return;
    }
    for (unsigned k = 0; k < n; k++) {
        if (who[k] < 100) {
            int i = who[k];

            if (nb_rel_signaled(&w->rel[i], v[k]) && w->bufs[i].valid) {
                w->bufs[i].held = false;
                if (w->sink) {
                    nb_sink_release(w->sink, w->bufs[i].id, w->bufs[i].seq);
                }
            }
        } else if (nb_rel_signaled(&w->gt_rel[who[k] - 100], v[k])) {
            w->gt[who[k] - 100].busy = false;
        }
    }
}

#ifdef NB_HAVE_WL_SYNCOBJ
/*
 * Give up explicit sync for new commits (an ioctl or an import failed).  The
 * surfaces lose their extension now, so their next commit is implicitly
 * synced; points already committed are still watched (w->xs stays open).
 */
static void wl_xs_fail(struct nb_wl *w, const char *why)
{
    if (!w->xs_off) {
        nb_err("explicit sync: off for this session: %s; the compositor "
               "syncs implicitly from now", why);
    }
    w->xs_off = true;
    for (int k = 0; k < 2; k++) {
        if (w->xs_surf[k]) {
            wp_linux_drm_syncobj_surface_v1_destroy(w->xs_surf[k]);
            w->xs_surf[k] = NULL;
        }
    }
}

/* A timeline the compositor knows too.  0, or -1. */
static int wl_xs_tl(struct nb_wl *w, struct nb_wl_tl *t)
{
    int fd = -1;

    if (t->tl) {
        return 0;
    }
    if (nb_xs_timeline(w->xs, &t->h, &fd) < 0) {
        t->h = 0;
        return -1;
    }
    /* libwayland duplicates the fd when it marshals the request. */
    t->tl = wp_linux_drm_syncobj_manager_v1_import_timeline(w->xs_mgr, fd);
    close(fd);
    if (!t->tl) {
        nb_xs_destroy(w->xs, t->h);
        t->h = 0;
        return -1;
    }
    return 0;
}

/* The render node and the acquire timeline, made on first use. */
static bool wl_xs_ready(struct nb_wl *w)
{
    char why[200] = "";

    if (!wl_xs_on(w)) {
        return false;
    }
    if (w->xs && w->xs_acq.tl) {
        return true;
    }
    if (!w->xs) {
        w->xs = nb_xs_open(NULL, w->fb_main_dev, why, sizeof(why));
        if (!w->xs) {
            nb_log("explicit sync: not used (%s); the compositor syncs "
                   "implicitly", why[0] ? why : "no render node");
            w->xs_off = true;
            return false;
        }
    }
    if (wl_xs_tl(w, &w->xs_acq) < 0) {
        wl_xs_fail(w, "cannot create the acquire timeline");
        return false;
    }
    nb_log("explicit sync: on (wp_linux_drm_syncobj_v1), timelines on %s, "
           "release points %s", nb_xs_node(w->xs),
           nb_xs_eventfd(w->xs) >= 0 ? "watched with an eventfd"
                                     : "polled every millisecond (no "
                                       "DRM_IOCTL_SYNCOBJ_EVENTFD)");
    return true;
}

/* Give surface k the syncobj extension (a dma-buf goes on it) or take it
 * away (anything else does). */
static void wl_xs_surface(struct nb_wl *w, int k, bool want)
{
    if (want && wl_xs_ready(w) && wl_xs_sf(w, k)) {
        if (!w->xs_surf[k]) {
            w->xs_surf[k] = wp_linux_drm_syncobj_manager_v1_get_surface(
                                w->xs_mgr, wl_xs_sf(w, k));
        }
    } else if (w->xs_surf[k]) {
        wp_linux_drm_syncobj_surface_v1_destroy(w->xs_surf[k]);
        w->xs_surf[k] = NULL;
    }
}
/* Everything explicit sync made, at the end of the session. */
static void wl_xs_tl_drop(struct nb_wl *w, struct nb_wl_tl *t)
{
    if (t->tl) {
        wp_linux_drm_syncobj_timeline_v1_destroy(t->tl);
    }
    nb_xs_destroy(w->xs, t->h);
    memset(t, 0, sizeof(*t));
}

static void wl_xs_close(struct nb_wl *w)
{
    for (int k = 0; k < 2; k++) {
        if (w->xs_surf[k]) {
            wp_linux_drm_syncobj_surface_v1_destroy(w->xs_surf[k]);
            w->xs_surf[k] = NULL;
        }
    }
    wl_xs_tl_drop(w, &w->xs_acq);
    for (int i = 0; i < NB_MAX_BUFS; i++) {
        wl_xs_tl_drop(w, &w->xs_buf_tl[i]);
    }
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        wl_xs_tl_drop(w, &w->xs_gt_tl[i]);
    }
    if (w->xs_mgr) {
        wp_linux_drm_syncobj_manager_v1_destroy(w->xs_mgr);
        w->xs_mgr = NULL;
    }
    nb_xs_close(w->xs);
    w->xs = NULL;
}
#else
static void wl_xs_close(struct nb_wl *w)
{
    (void)w;
}
static void wl_xs_surface(struct nb_wl *w, int k, bool want)
{
    (void)w; (void)k; (void)want;
}
static bool wl_xs_ready(struct nb_wl *w)
{
    (void)w;
    return false;
}
#endif

/*
 * Attach `b` to surface k (0 the main surface, 1 the picture's) and record
 * what it is, for the points wl_surf_commit() sets.  `dma`: the buffer is a
 * dma-buf (only those take explicit sync).  `acq`: the content's acquire
 * point when it already has one (0: made at the commit).
 */
static void wl_attach_sf(struct nb_wl *w, int k, struct wl_buffer *b,
                         int kind, int idx, bool dma, uint64_t acq)
{
    struct wl_surface *sf = wl_xs_sf(w, k);

    if (!sf) {
        return;
    }
    wl_xs_surface(w, k, b && dma && kind != NB_XA_NONE);
    wl_surface_attach(sf, b, 0, 0);
    w->xs_pend[k] = (struct nb_wl_xa){
        .kind = b ? kind : NB_XA_NONE, .idx = idx, .acq = acq,
    };
}

/* Right before a commit of `sf`: the points for what was attached to it. */
static void wl_xs_flush(struct nb_wl *w, struct wl_surface *sf)
{
    int k = sf && sf == w->surf ? 0 : sf && sf == w->cs_surf ? 1 : -1;
    struct nb_wl_xa a;
    struct nb_rel *rel;
    uint64_t *acqp;
    struct nb_wl_tl *tl;
    int dmabuf = -1;

    if (k < 0 || w->xs_pend[k].kind == NB_XA_NONE) {
        return;
    }
    a = w->xs_pend[k];
    w->xs_pend[k].kind = NB_XA_NONE;
    if (a.kind == NB_XA_GUEST) {
        rel = &w->rel[a.idx];
        acqp = &w->buf_acq[a.idx];
        tl = &w->xs_buf_tl[a.idx];
        dmabuf = w->bufs[a.idx].valid ? w->bufs[a.idx].fd : -1;
    } else {
        rel = &w->gt_rel[a.idx];
        acqp = &w->gt_acq[a.idx];
        tl = &w->xs_gt_tl[a.idx];
    }
#ifdef NB_HAVE_WL_SYNCOBJ
    if (w->xs_surf[k]) {
        uint64_t acq = a.acq, rp;
        int r = 0;

        if (!acq) {
            acq = ++w->xs_acq_pt;
            r = dmabuf >= 0
                    ? nb_xs_from_dmabuf(w->xs, w->xs_acq.h, acq, dmabuf)
                    : nb_xs_signal(w->xs, w->xs_acq.h, acq);
            if (r == 1 && !w->xs_dmabuf_fence) {
                w->xs_dmabuf_fence = true;
                nb_log("explicit sync: a guest frame's acquire point carries "
                       "the dma-buf's own fence");
            }
        }
        if (r >= 0 && wl_xs_tl(w, tl) == 0) {
            *acqp = acq;
            rp = nb_rel_commit(rel, true);
            wp_linux_drm_syncobj_surface_v1_set_acquire_point(
                w->xs_surf[k], w->xs_acq.tl, (uint32_t)(acq >> 32),
                (uint32_t)acq);
            wp_linux_drm_syncobj_surface_v1_set_release_point(
                w->xs_surf[k], tl->tl, (uint32_t)(rp >> 32), (uint32_t)rp);
            (void)nb_xs_watch(w->xs, tl->h, rp);
            w->st_xs++;
            return;
        }
        wl_xs_fail(w, r < 0 ? "the acquire point could not be set"
                            : "cannot create a release timeline");
        /* Committed without points now: content the GPU may still be
         * writing (the copy's unwaited pass) must be done first. */
        if (a.acq) {
            (void)nb_xs_wait(w->xs, w->xs_acq.h, a.acq, 50000);
        }
    }
#else
    (void)tl;
    (void)dmabuf;
#endif
    *acqp = 0;
    nb_rel_commit(rel, false);
}

/* Every commit of the main surface or the picture's goes through here. */
static void wl_surf_commit(struct nb_wl *w, struct wl_surface *sf)
{
    wl_xs_flush(w, sf);
    wl_surface_commit(sf);
}

/* ── the present level (nb_present.h) ─────────────────────────────────────── */

/* Say where frames go now, once per change (the one log line --present-mode
 * promises). */
static void wl_pc_report(struct nb_wl *w)
{
    if (!nb_pc_take_change(&w->pc)) {
        return;
    }
    nb_log("present: %s (--present-mode=%s): %s", nb_pl_name(w->pc.level),
           nb_pm_name(w->pc.mode), w->pc.why);
}

/* A hard failure at `at`: one level down when the mode allows it. */
static void wl_pc_fail(struct nb_wl *w, enum nb_pl at, const char *why)
{
    if (nb_pc_fail(&w->pc, at, "%s", why)) {
        if (nb_pc_forces_readback(&w->pc)) {
            w->gt_want_rb = true;
            w->gt_w = 0;            /* reallocate at the next frame */
        }
        wl_pc_report(w);
        if (w->sink) {
            nb_sink_refresh(w->sink);   /* the current frame, at the new level */
        }
    }
}

/* ── ops: format policy ──────────────────────────────────────────────────── */

static bool wl_format_ok(struct nb_session *s, uint32_t fourcc, uint64_t mod)
{
    struct nb_wl *w = s->priv;

    /*
     * HARDENING 3, verbatim: what the GPU advertises, not a list we wrote.
     * When the compositor could only tell us formats (dmabuf version 2, no
     * `modifier` event) the only thing it has committed to is the implicit
     * layout, so an explicit modifier is not something it agreed to accept.
     */
    if (!(s->caps & NVKVM_BROKER_CAP_MODIFIERS)) {
        return mod == NB_DRM_FORMAT_MOD_INVALID &&
               nb_formats_has(&w->formats, fourcc, NB_DRM_FORMAT_MOD_INVALID);
    }
    return nb_formats_has(&w->formats, fourcc, mod);
}

/*
 * ops->copy_ok: can the GPU pass show this pair although the compositor does
 * not take it?  The scaler is opened on the first question (it dlopen()s
 * EGL/GBM), on --render-node or the first render node that imports the pair;
 * the verdict is cached per pair.
 */
static bool wl_copy_ok(struct nb_session *s, uint32_t fourcc, uint64_t mod)
{
    struct nb_wl *w = s->priv;
    char why[200] = "";
    bool ok;

    /*
     * --gpu-scale=off turns off the fullscreen SCALER (a choice between two
     * ways of showing a frame the compositor takes); a frame it does not
     * take, or a level that copies everything, has no other way to the
     * screen, so it is not asked here.  The read-back level needs no
     * dma-buf protocol at all.
     */
    if (!nb_copy_allowed(nb_pc_copy_allowed(&w->pc) && !nb_linear_only,
                         true, w->gs_dead, w->viewport != NULL,
                         w->dmabuf != NULL || nb_pc_forces_readback(&w->pc))) {
        return false;
    }
    for (unsigned i = 0; i < w->copy_n; i++) {
        if (w->copy_v[i].fourcc == fourcc && w->copy_v[i].mod == mod) {
            return w->copy_v[i].ok;
        }
    }
    if (!w->gs) {
        const char *node = w->cfg ? w->cfg->render_node : NULL;

        w->gs = nb_gs_open(node, 0, fourcc, mod, why, sizeof(why));
        if (!w->gs && node) {
            /* The chosen GPU cannot import this layout: any GPU that can,
             * rather than a black window. */
            nb_err("viewer GPU: %s (--gpu / viewer.gpu); trying the others",
                   why);
            w->gs = nb_gs_open(NULL, 0, fourcc, mod, why, sizeof(why));
        }
        if (w->gs) {
            nb_log("gpu-scale: ready on %s, GPU context priority %s "
                   "requested, %s granted (CONDUIT_VIEWER_GPU_PRIORITY)",
                   nb_gs_node(w->gs),
                   nb_gs_prio_name(nb_gs_prio_requested(w->gs)),
                   nb_gs_prio_name(nb_gs_prio_granted(w->gs)));
        }
    }
    ok = w->gs && nb_gs_can_import(w->gs, fourcc, mod);
    nb_log("GPU copy: %.4s modifier 0x%016llx %s", (const char *)&fourcc,
           (unsigned long long)mod,
           ok ? "is not taken by the compositor; the viewer's GPU pass "
                "copies these frames into a buffer it does take"
              : why[0] ? why : "cannot be imported by the viewer's GPU either");
    if (w->copy_n < NB_COPY_CACHE) {
        w->copy_v[w->copy_n].fourcc = fourcc;
        w->copy_v[w->copy_n].mod = mod;
        w->copy_v[w->copy_n].ok = ok;
        w->copy_n++;
    }
    return ok;
}

/*
 * At the gpu-copy and readback levels every dma-buf frame goes through the
 * GPU pass -- when the viewer's GPU can import it.  When it cannot there is
 * no copy to make, and the frame is shown zero-copy if the compositor takes
 * it (said once).
 */
static bool wl_force_copy(struct nb_wl *w, const struct nb_buf_desc *d)
{
    if (!nb_pc_forces_copy(&w->pc) || d->is_shm) {
        return false;
    }
    if (wl_copy_ok(w->sess, d->fourcc, d->modifier)) {
        return true;
    }
    if (!w->pc_noimport_told) {
        w->pc_noimport_told = true;
        nb_err("present: level %s, but the viewer's GPU cannot import %.4s "
               "modifier 0x%016llx; showing those frames zero-copy",
               nb_pl_name(w->pc.level), (const char *)&d->fourcc,
               (unsigned long long)d->modifier);
    }
    return false;
}

/* ── ops: attach / commit ────────────────────────────────────────────────── */

/* Slot for this pair, allocating one if there is room. */
static int wl_pair_slot(struct nb_wl *w, uint32_t fourcc, uint64_t mod,
                        bool create)
{
    int free_slot = -1;

    for (int i = 0; i < NB_PROVEN_SLOTS; i++) {
        if (w->proven[i].fourcc == fourcc && w->proven[i].mod == mod &&
            (w->proven[i].state || w->proven[i].probing)) {
            return i;
        }
        if (free_slot < 0 && !w->proven[i].state && !w->proven[i].probing) {
            free_slot = i;
        }
    }
    if (!create || free_slot < 0) {
        return -1;
    }
    w->proven[free_slot].fourcc = fourcc;
    w->proven[free_slot].mod    = mod;
    return free_slot;
}

/*
 * The probe answered.  `created` means this display really can import the pair;
 * `failed` means it advertised something it cannot bind, and the whole point of
 * probing is that we learn this WITHOUT the connection being torn down.
 *
 * The probe buffer itself is discarded either way.  Showing it would mean
 * threading a deferred attach through the commit path (and holding a buffer
 * already released to the guest), so on `created` the client is asked for its
 * current frame again (EV_REFRESH) instead.  Without that, the frame dropped
 * here is lost for good when it was the only one: a viewer that (re)connects
 * to an idle guest got exactly one frame, the probe, and kept showing
 * "WAITING FOR THE VM" until the guest next drew something.
 */
static void probe_created(void *data, struct zwp_linux_buffer_params_v1 *params,
                          struct wl_buffer *buf)
{
    struct nb_wl *w = data;
    int i = wl_pair_slot(w, w->probe_fourcc, w->probe_mod, false);

    w->probe_inflight = false;
    if (i >= 0) {
        w->proven[i].state   = 1;
        w->proven[i].probing = false;
        nb_log("this display CAN import %.4s modifier 0x%016llx — proven by "
               "import, not by advertisement",
               (const char *)&w->proven[i].fourcc,
               (unsigned long long)w->proven[i].mod);
    }
    wl_buffer_destroy(buf);
    zwp_linux_buffer_params_v1_destroy(params);
    /* The pair is proven: the frame the probe swallowed (and any dropped
     * while it was out) can be shown now. */
    if (w->sink) {
        nb_sink_refresh(w->sink);
    }
}

static void probe_failed(void *data, struct zwp_linux_buffer_params_v1 *params)
{
    struct nb_wl *w = data;
    int i = wl_pair_slot(w, w->probe_fourcc, w->probe_mod, false);

    w->probe_inflight = false;
    if (i >= 0) {
        w->proven[i].state   = -1;
        w->proven[i].probing = false;
        nb_err("this display ADVERTISED %.4s modifier 0x%016llx and then "
               "refused to import it.  Frames in that layout will be rejected "
               "rather than retried; the VMM is being told so it can send "
               "something else.",
               (const char *)&w->proven[i].fourcc,
               (unsigned long long)w->proven[i].mod);
        w->probe_bad_pending = true;
        w->probe_bad_fourcc  = w->proven[i].fourcc;
        w->probe_bad_mod     = w->proven[i].mod;
        wl_pc_fail(w, NB_PL_ZC, "the compositor refused to import the "
                   "guest's buffer");
    }
    zwp_linux_buffer_params_v1_destroy(params);
}

static const struct zwp_linux_buffer_params_v1_listener probe_listener = {
    .created = probe_created,
    .failed  = probe_failed,
};

static int wl_attach(struct nb_session *s, const struct nb_buf_desc *d)
{
    struct nb_wl *w = s->priv;
    struct zwp_linux_buffer_params_v1 *params;
    struct wl_buffer *b;
    int i, victim = -1;
    uint64_t oldest = UINT64_MAX;

    w->tick++;

    /* A staged buffer that was never committed is dropped by this ATTACH: it
     * will not be shown, so it is released now unless the compositor still
     * holds an earlier commit of it (then that release covers it). */
    if (w->pending >= 0) {
        struct nb_wl_buf *old = &w->bufs[w->pending];

        w->pending = -1;
        if (old->valid) {
            wl_flog(w, "X %u %llu\n", old->seq, (unsigned long long)old->recv_ns);
        }
        if (old->valid && !old->held && w->sink &&
            (s->caps & NVKVM_BROKER_CAP_RELEASE_SEQ)) {
            nb_sink_release(w->sink, old->id, old->seq);
        }
    }

    /* Already imported?  Same reasoning as the import cache in
     * nvkvm_present_egl.c: the guest cycles a handful of scanout bos, and the
     * dma-buf inode is their stable identity across dup(2) and SCM_RIGHTS. */
    for (i = 0; i < NB_MAX_BUFS; i++) {
        struct nb_wl_buf *sl = &w->bufs[i];

        if (sl->valid && sl->id == d->id && sl->w == d->width &&
            sl->h == d->height && sl->stride == d->stride &&
            sl->offset == d->offset && sl->fourcc == d->fourcc &&
            sl->modifier == d->modifier &&
            /* Imported for zero-copy before the level dropped: made again
             * as a copy (the old slot ages out of the cache). */
            (sl->shm || (sl->copy ? !w->gs_dead : !wl_force_copy(w, d)))) {
            sl->used = w->tick;
            sl->seq  = d->seq;
            sl->recv_ns = d->recv_ns;
            w->pending = i;
            return 0;
        }
    }

    /* Pick a victim.  Never the buffer the surface is holding and never the
     * one already staged, or the compositor is left reading freed content. */
    for (i = 0; i < NB_MAX_BUFS; i++) {
        if (i == w->current || i == w->pending) {
            continue;
        }
        if (!w->bufs[i].valid) {
            victim = i;
            break;
        }
        /* A buffer the compositor has released goes before one it still
         * holds (whose release would be lost with its wl_buffer). */
        if ((w->bufs[i].used | (w->bufs[i].held ? 1ull << 63 : 0)) < oldest) {
            oldest = w->bufs[i].used | (w->bufs[i].held ? 1ull << 63 : 0);
            victim = i;
        }
    }
    if (victim < 0) {
        return -ENOSPC;         /* cannot happen with NB_MAX_BUFS >= 3 */
    }

    /*
     * TIER 3: SHARED MEMORY.  The VMM sends a memfd when the dma-buf tiers have
     * been refused.  wl_shm is a core Wayland global -- every compositor must
     * implement it -- so this cannot fail for want of import support, which is
     * exactly why it is the floor under a display that advertises a modifier it
     * will not bind.  It costs the compositor an upload per frame; that is the
     * price of universality and the reason the VMM tries it last.
     *
     * No modifier is involved and none was checked: shm is linear by
     * definition, and the geometry bounds nb_validate_desc() already enforced
     * are what keep the compositor from reading past the mapping.
     */
    if (d->is_shm) {
        struct wl_shm_pool *pool;
        struct wl_buffer *sb;
        size_t need = (size_t)d->stride * d->height + d->offset;

        if (!w->shm) {
            nb_err("ATTACH: this compositor offers no wl_shm, so there is no "
                   "buffer type left to try");
            return -EINVAL;
        }
        pool = wl_shm_create_pool(w->shm, d->fd, (int32_t)need);
        if (!pool) {
            return -EIO;
        }
        sb = wl_shm_pool_create_buffer(pool, (int32_t)d->offset,
                                       (int32_t)d->width, (int32_t)d->height,
                                       (int32_t)d->stride,
                                       d->fourcc == NB_FOURCC_AR24_LOCAL
                                           ? WL_SHM_FORMAT_ARGB8888
                                           : WL_SHM_FORMAT_XRGB8888);
        wl_shm_pool_destroy(pool);
        if (!sb) {
            return -EIO;
        }
        {
            static bool told;

            if (!told) {
                told = true;
                nb_log("presenting through wl_shm: the display refused every "
                       "dma-buf layout offered, so frames are shared as plain "
                       "memory.  This works everywhere and costs the "
                       "compositor one upload per frame.");
            }
        }
        wl_buf_evict(w, victim);
        w->bufs[victim] = (struct nb_wl_buf){
            .valid = true, .id = d->id, .buf = sb,
            .w = d->width, .h = d->height, .stride = d->stride,
            .offset = d->offset, .fourcc = d->fourcc, .modifier = d->modifier,
            .used = w->tick, .owner = w, .seq = d->seq, .fd = -1,
            .shm = true,
        };
        wl_buffer_add_listener(sb, &buf_listener, &w->bufs[victim]);
        w->pending = victim;
        return 0;
    }

    /*
     * DIRECT OR COPY (nb_blit.h).  A pair the compositor does not advertise,
     * or advertised and then refused on import, is shown through the GPU
     * pass when the viewer's GPU imports it: no wl_buffer is made of it.
     */
    {
        bool adv = wl_format_ok(s, d->fourcc, d->modifier);
        int ps0 = adv ? wl_pair_slot(w, d->fourcc, d->modifier, false) : -1;
        int proven = ps0 >= 0 ? w->proven[ps0].state : 0;
        enum nb_att how = nb_att_decide(adv, proven, false);

        if (how != NB_ATT_DIRECT || wl_force_copy(w, d)) {
            how = nb_att_decide(adv && !wl_force_copy(w, d), proven,
                                wl_copy_ok(s, d->fourcc, d->modifier));
        }
        if (how == NB_ATT_REJECT) {
            return -EINVAL;
        }
        if (how == NB_ATT_COPY) {
            int fd2 = fcntl(d->fd, F_DUPFD_CLOEXEC, 3);

            if (fd2 < 0) {
                return -errno;
            }
            if (!w->copy_told) {
                w->copy_told = true;
                nb_log("picture: %.4s modifier 0x%016llx frames are copied by "
                       "the GPU pass (%s), in every window mode",
                       (const char *)&d->fourcc,
                       (unsigned long long)d->modifier,
                       wl_force_copy(w, d)
                           ? nb_pc_forces_readback(&w->pc)
                                 ? "present level readback"
                                 : "present level gpu-copy"
                       : adv ? "the compositor refused this layout"
                             : "the compositor does not advertise this layout");
            }
            wl_buf_evict(w, victim);
            w->bufs[victim] = (struct nb_wl_buf){
                .valid = true, .id = d->id, .buf = NULL, .copy = true,
                .w = d->width, .h = d->height, .stride = d->stride,
                .offset = d->offset, .fourcc = d->fourcc,
                .modifier = d->modifier, .used = w->tick, .owner = w,
                .seq = d->seq, .recv_ns = d->recv_ns, .fd = fd2,
            };
            w->pending = victim;
            return 0;
        }
    }

    /*
     * ADVERTISED IS NOT IMPORTABLE.  Gate on what this display has actually
     * proven it can bind; see the `proven` table's comment for the Mutter case
     * that makes this necessary.
     */
    {
        int ps = wl_pair_slot(w, d->fourcc, d->modifier, true);

        /*
         * NO SLOT LEFT IS A REJECTION, NOT A DARE.  AUDIT 2026-08-27 S-6:
         * falling through from here reached create_immed with a pair nothing
         * had vouched for -- and create_immed reports a refusal as a WAYLAND
         * PROTOCOL ERROR, which kills the connection and exits the broker.
         * That is the exact outcome the proven[] table was added to avoid, so
         * running out of table entries must not re-open it.  The display
         * advertises dozens of pairs and the client picks which four to spend,
         * so this is client-reachable in four commands.
         */
        if (ps < 0) {
            return -EINVAL;
        }
        if (w->proven[ps].state == -1) {
            return -EINVAL;         /* known-refused: reject, do not retry */
        }
        if (w->proven[ps].probing) {
            wl_drop_attach(s, d);   /* a probe is in flight; nothing to show */
            return 0;
        }
        /*
         * ONE PROBE AT A TIME.  probe_fourcc/probe_mod are a single slot --
         * "the pair the outstanding probe is about", singular -- and the
         * `created`/`failed` events carry no identity of their own, so two
         * probes in flight means both answers are filed against whichever pair
         * was started last.  The other pair's slot then stays `probing`
         * forever: it never displays, and it never frees its entry either,
         * which is how a client reaches the ps < 0 case above.
         */
        if (w->probe_inflight) {
            wl_drop_attach(s, d);
            return 0;
        }
        if (w->proven[ps].state == 0) {
            /* First buffer of this pair: probe it asynchronously, so a refusal
             * arrives as an event rather than as a fatal protocol error. */
            struct zwp_linux_buffer_params_v1 *pp =
                zwp_linux_dmabuf_v1_create_params(w->dmabuf);

            if (!pp) {
                return -EIO;
            }
            zwp_linux_buffer_params_v1_add(pp, d->fd, 0, d->offset, d->stride,
                                           (uint32_t)(d->modifier >> 32),
                                           (uint32_t)(d->modifier & 0xffffffffu));
            zwp_linux_buffer_params_v1_add_listener(pp, &probe_listener, w);
            w->probe_fourcc      = d->fourcc;
            w->probe_mod         = d->modifier;
            w->proven[ps].probing = true;
            w->probe_inflight     = true;
            zwp_linux_buffer_params_v1_create(pp, (int32_t)d->width,
                                              (int32_t)d->height, d->fourcc,
                                              0 /* flags */);
            nb_log("probing whether this display can import %.4s modifier "
                   "0x%016llx (one frame is dropped to find out)",
                   (const char *)&d->fourcc,
                   (unsigned long long)d->modifier);
            w->pending = -1;
            wl_drop_attach(s, d);   /* the probe buffer is never shown */
            return 0;
        }
    }

    params = zwp_linux_dmabuf_v1_create_params(w->dmabuf);
    if (!params) {
        return -EIO;
    }
    zwp_linux_buffer_params_v1_add(params, d->fd, 0, d->offset, d->stride,
                                   (uint32_t)(d->modifier >> 32),
                                   (uint32_t)(d->modifier & 0xffffffffu));
    /*
     * create_immed, not create: `create` answers asynchronously, and waiting
     * for the answer means a roundtrip to the compositor inside the path that
     * also carries input.  Input must never block on rendering — this project
     * shipped that bug once already.  create_immed reports failure as a
     * protocol error on the params object instead, which we surface as a lost
     * display rather than as a stall.
     */
    b = zwp_linux_buffer_params_v1_create_immed(params, (int32_t)d->width,
                                                (int32_t)d->height, d->fourcc,
                                                0 /* flags */);
    zwp_linux_buffer_params_v1_destroy(params);
    if (!b) {
        return -EIO;
    }

    wl_buf_evict(w, victim);
    w->bufs[victim] = (struct nb_wl_buf){
        .valid = true, .id = d->id, .buf = b,
        .w = d->width, .h = d->height, .stride = d->stride,
        .offset = d->offset, .fourcc = d->fourcc, .modifier = d->modifier,
        .used = w->tick, .owner = w, .seq = d->seq, .recv_ns = d->recv_ns,
        /* Kept for the fullscreen scaler, which imports it on the GPU when
         * the picture is not 1:1, and for explicit sync, which takes the
         * acquire point from its fence.  A dup of an fd we hold anyway. */
        .fd = w->gs_mode == NB_GS_OFF && !wl_xs_on(w)
                  ? -1 : fcntl(d->fd, F_DUPFD_CLOEXEC, 3),
    };
    wl_buffer_add_listener(b, &buf_listener, &w->bufs[victim]);
    w->pending = victim;
    return 0;
}

static void frame_done(void *data, struct wl_callback *cb, uint32_t t);
static const struct wl_callback_listener frame_listener = { .done = frame_done };

static void frame_done(void *data, struct wl_callback *cb, uint32_t t)
{
    struct nb_wl *w = data;
    (void)t;

    wl_callback_destroy(cb);
    w->frame_inflight = false;
    nb_pace_frame_done(&w->pace);
    if (w->sink) {
        w->last_frame_ms = nb_now_ms_wl();
        nb_sink_frame(w->sink);
    }
}

/* ── --sync=latch: latest frame per refresh, late latch (nb_latch.h) ────── */
/* Direct mode (CTRL+ALT+D) is the lowest-latency immediate path and wins. */
static bool wl_latch_on(const struct nb_wl *w)
{
    return w->vst.sync == NB_SYNC_LATCH && !w->direct;
}

/* May the staged frame be committed now?  The latch window in latch mode
 * (and never into a full socket), the frame-callback gate otherwise. */
static bool wl_gate_open(struct nb_wl *w, uint64_t now)
{
    if (!wl_latch_on(w)) {
        return nb_pace_may_commit(&w->pace, now);
    }
    if (w->pace.blocked || !nb_latch_may_commit(&w->latch, now)) {
        return false;
    }
    w->latch_open_ns = nb_latch_clocked(&w->latch) ? nb_latch_wake(&w->latch) : now;
    return true;
}

/* Arm the timerfd for the next window opening while a frame is staged. */
static void wl_latch_arm(struct nb_wl *w)
{
    uint64_t now, due, at;
    struct itimerspec its = { 0 };

    if (!wl_latch_on(w)) {
        return;
    }
    now = nb_mono_ns();
    due = nb_latch_due_ns(&w->latch, now);
    if (!due) {
        return;                 /* open now (held only by a full socket) */
    }
    if (w->latch_tfd < 0) {
        w->latch_tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC);
        if (w->latch_tfd < 0) {
            return;             /* wl_tick's millisecond timeout still runs */
        }
    }
    at = now + due;
    its.it_value.tv_sec = (time_t)(at / 1000000000ull);
    its.it_value.tv_nsec = (long)(at % 1000000000ull);
    if (timerfd_settime(w->latch_tfd, TFD_TIMER_ABSTIME, &its, NULL) == 0) {
        w->latch_armed = true;
    }
}

/*
 * THE ONE PLACE THE VIEWPORT DESTINATION IS DECIDED.
 *
 * It used to be decided in two -- once in wl_commit() for a new frame, once in
 * top_configure() for a resize -- and the two disagreed in exactly one case,
 * which is the case a user hits constantly:
 *
 *   top_configure() only touched the viewport when SCALING WAS WANTED
 *   (scale_mode 1, or scale_mode 2 while fullscreen).  Leaving fullscreen
 *   clears w->fullscreen, so the condition went false and the destination was
 *   left AT THE FULLSCREEN SIZE.  The frame stayed 3840x2160 while the window
 *   frame shrank, so the guest's desktop spilled across the whole screen with
 *   the host visible in a strip down the side.
 *
 * Entering and leaving fullscreen are the same operation with different
 * numbers, so they now run the same code.  A destination that equals the
 * buffer is UNSET rather than set 1:1, because a viewport scaling by exactly
 * one is still a viewport and a compositor deciding whether a surface may go
 * straight to a hardware plane is entitled to refuse anything carrying one.
 *
 * Returns the resulting surface size in out_w / out_h.
 */
/*
 * The 1x1 opaque black buffer the main surface shows under a letterbox,
 * stretched over the whole window by the main viewport.  A single-pixel
 * buffer where the compositor has one (a solid colour it can draw or put on
 * a plane without sampling a texture), else one pixel of shm.
 */
static bool wl_blk_build(struct nb_wl *w)
{
    struct wl_shm_pool *pool;
    uint32_t *px;
    int fd;

    if (w->blk_buf) {
        return true;
    }
    if (w->spb_mgr) {
        w->blk_buf = wp_single_pixel_buffer_manager_v1_create_u32_rgba_buffer(
            w->spb_mgr, 0, 0, 0, UINT32_MAX);
        if (w->blk_buf) {
            return true;
        }
    }
    if (!w->shm) {
        return false;
    }
    fd = memfd_create("nvgpu-viewer-black", MFD_CLOEXEC);
    if (fd < 0) {
        return false;
    }
    if (ftruncate(fd, 4) < 0) {
        close(fd);
        return false;
    }
    px = mmap(NULL, 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (px == MAP_FAILED) {
        close(fd);
        return false;
    }
    *px = 0xff000000u;
    pool = wl_shm_create_pool(w->shm, fd, 4);
    close(fd);
    if (!pool) {
        munmap(px, 4);
        return false;
    }
    w->blk_buf = wl_shm_pool_create_buffer(pool, 0, 1, 1, 4,
                                           WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);
    if (!w->blk_buf) {
        munmap(px, 4);
        return false;
    }
    w->blk_px = px;
    return true;
}

static void wl_opaque(struct nb_wl *w, struct wl_surface *sf, int ow, int oh)
{
    struct wl_region *r = wl_compositor_create_region(w->comp);

    if (r) {
        wl_region_add(r, 0, 0, ow, oh);
        wl_surface_set_opaque_region(sf, r);
        wl_region_destroy(r);
    }
}

/* Leave the letterbox: the picture's subsurface is emptied and the main
 * surface takes buffers again.  The caller attaches what it shows. */
static void wl_leave_sub(struct nb_wl *w)
{
    if (!w->in_sub) {
        return;
    }
    w->in_sub = false;
    wl_attach_sf(w, 1, NULL, NB_XA_NONE, -1, false, 0);
    wl_surf_commit(w, w->cs_surf);
    w->cs_at_x = w->cs_at_y = INT32_MIN;
    w->main_vw = w->main_vh = 0;
    w->tgt_ow = w->tgt_oh = 0;
}


/* ── the fullscreen scaler (nb_gpuscale.h) ───────────────────────────────── */

static void gt_release(void *data, struct wl_buffer *b)
{
    struct nb_wl_gt *t = data;

    (void)b;
    /* Undefined under explicit sync: then the release point frees it. */
    if (nb_rel_event(&t->owner->gt_rel[t - t->owner->gt])) {
        t->busy = false;
    }
}
static const struct wl_buffer_listener gt_listener = { .release = gt_release };

/* A create request in flight; its answer may arrive after a reallocation. */
struct gt_req {
    struct nb_wl_gt *t;
    unsigned gen;
};

static void gt_created(void *data, struct zwp_linux_buffer_params_v1 *params,
                       struct wl_buffer *buf)
{
    struct gt_req *rq = data;
    struct nb_wl_gt *t = rq->t;
    bool stale = rq->gen != t->gen;

    free(rq);
    zwp_linux_buffer_params_v1_destroy(params);
    if (stale || t->buf || t->failed) { /* superseded by a reallocation */
        wl_buffer_destroy(buf);
        return;
    }
    t->buf = buf;
    t->ready = true;
    wl_buffer_add_listener(buf, &gt_listener, t);
}

static void gt_failed(void *data, struct zwp_linux_buffer_params_v1 *params)
{
    struct gt_req *rq = data;
    struct nb_wl_gt *t = rq->t;
    bool stale = rq->gen != t->gen;

    free(rq);
    zwp_linux_buffer_params_v1_destroy(params);
    if (stale) {
        return;
    }
    t->failed = true;
    if (!t->owner) {
        return;
    }
    /*
     * Not the end: the same pass, read back into shared memory, which every
     * compositor takes.  Every target of the batch answers; only the first
     * says so.  Read-back targets are wl_shm and never come here, so this
     * never turns the pass off.
     */
    if (!t->owner->gt_want_rb) {
        nb_err("GPU copy: the compositor refused the copy's dma-buf; reading "
               "the pass back into shared memory instead");
    }
    t->owner->gt_want_rb = true;
    t->owner->gt_w = 0;         /* reallocate at the next frame */
    wl_pc_fail(t->owner, NB_PL_COPY, "the compositor refused the GPU copy's "
               "buffers");
}
static const struct zwp_linux_buffer_params_v1_listener gt_params_listener = {
    .created = gt_created,
    .failed  = gt_failed,
};

static void gt_drop(struct nb_wl *w)
{
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        if (w->gt[i].buf) {
            wl_buffer_destroy(w->gt[i].buf);
        }
        w->gt[i] = (struct nb_wl_gt){ .owner = w, .gen = w->gt[i].gen + 1 };
        nb_rel_forget(&w->gt_rel[i]);
        w->gt_acq[i] = 0;
    }
    if (w->gt_shm) {
        munmap(w->gt_shm, w->gt_shm_sz);
        w->gt_shm = NULL;
        w->gt_shm_sz = 0;
    }
    w->gt_rb = false;
    w->gt_w = w->gt_h = 0;
}

/* Read-back targets: NB_GS_TARGETS XRGB8888 wl_shm buffers in one memfd. */
static bool gt_shm_make(struct nb_wl *w, int pw, int ph)
{
    size_t one = (size_t)pw * (size_t)ph * 4u;
    size_t sz = one * NB_GS_TARGETS;
    struct wl_shm_pool *pool;
    int fd;

    if (!w->shm) {
        return false;
    }
    fd = memfd_create("conduit-viewer-copy", MFD_CLOEXEC);
    if (fd < 0) {
        return false;
    }
    if (ftruncate(fd, (off_t)sz) < 0) {
        close(fd);
        return false;
    }
    w->gt_shm = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (w->gt_shm == MAP_FAILED) {
        w->gt_shm = NULL;
        close(fd);
        return false;
    }
    w->gt_shm_sz = sz;
    pool = wl_shm_create_pool(w->shm, fd, (int32_t)sz);
    close(fd);
    if (!pool) {
        return false;
    }
    for (int i = 0; i < NB_GS_TARGETS; i++) {
        w->gt[i].buf = wl_shm_pool_create_buffer(pool, (int32_t)(one * i),
                                                 pw, ph, pw * 4,
                                                 WL_SHM_FORMAT_XRGB8888);
        if (!w->gt[i].buf) {
            wl_shm_pool_destroy(pool);
            return false;
        }
        wl_buffer_add_listener(w->gt[i].buf, &gt_listener, &w->gt[i]);
        w->gt[i].ready = true;
    }
    wl_shm_pool_destroy(pool);
    w->gt_rb = true;
    return true;
}

static void wl_gs_close(struct nb_wl *w)
{
    gt_drop(w);
    if (w->gs) {
        nb_gs_close(w->gs);
        w->gs = NULL;
    }
}

static bool gs_fmt_ok(void *ctx, uint32_t fourcc, uint64_t mod)
{
    struct nb_wl *w = ctx;

    return nb_formats_has(&w->formats, fourcc, mod) ||
           (w->fb_has_render && nb_so_fmts_has(&w->fb_render, fourcc, mod));
}

static void wl_gs_disable(struct nb_wl *w, const char *why)
{
    w->gs_dead = true;
    nb_err("gpu-scale: off for this session: %s.  The compositor scales the "
           "picture, which on some compositors (Hyprland) means vsync in "
           "fullscreen; a 1:1 guest mode is unaffected", why);
    if (nb_pc_forces_copy(&w->pc)) {
        nb_err("present: %s needs the GPU pass, which failed; frames the "
               "compositor takes as they are are shown zero-copy now",
               nb_pl_name(w->pc.level));
    }
    if (w->sink) {
        nb_sink_refresh(w->sink);
    }
}

/*
 * Whether the scaler COULD take this frame: fullscreen, a dma-buf, a viewport
 * to undo, and a scaler that has not failed.  Whether it does is
 * wl_so_decide()'s call (nb_scanout.h).
 */
static bool wl_gs_eligible(const struct nb_wl *w, int slot, const char **why)
{
    const struct nb_wl_buf *sl = slot >= 0 && slot < NB_MAX_BUFS
                                     ? &w->bufs[slot] : NULL;

    *why = NULL;
    if (w->gs_mode == NB_GS_OFF) {
        *why = "--gpu-scale=off";
    } else if (w->gs_dead) {
        *why = "the GPU pass failed earlier this session";
    } else if (!w->viewport) {
        *why = "no wp_viewporter";
    } else if (!sl || !sl->valid || sl->fd < 0) {
        *why = "not a dma-buf frame";
    }
    return *why == NULL && w->fullscreen;
}

/* The picture is the buffer, 1:1, covering the whole window. */
static bool wl_pic_exact(const struct nb_wl *w, int slot, int pw, int ph)
{
    const struct nb_wl_buf *sl;

    if (slot < 0 || slot >= NB_MAX_BUFS || !w->bufs[slot].valid) {
        return false;
    }
    sl = &w->bufs[slot];
    return (int)sl->w == pw && (int)sl->h == ph && !w->place.cropped &&
           w->place.vis.x == 0 && w->place.vis.y == 0 &&
           w->place.vis.w == w->win_w && w->place.vis.h == w->win_h;
}

/* Feedback verdict on a guest buffer: 1 scanout-capable, 0 not, -1 unknown. */
static int wl_guest_scanout(const struct nb_wl *w, const struct nb_wl_buf *sl)
{
    return nb_so_guest_scanout(&w->fb_scan, w->fb_has_scan, sl->fourcc,
                               nb_fourcc_opaque_twin(sl->fourcc),
                               sl->modifier);
}

static const char *wl_so_shape(const struct nb_wl *w)
{
    if (w->gs_shown) {
        return "GPU pass into an output-size buffer";
    }
    if (w->in_sub) {
        return "zero-copy, letterboxed on a subsurface";
    }
    if (w->current >= 0 && w->bufs[w->current].valid &&
        (int)w->bufs[w->current].w == w->tgt_ow &&
        (int)w->bufs[w->current].h == w->tgt_oh && !w->place.cropped) {
        return "zero-copy 1:1";
    }
    return "zero-copy, scaled by a viewport";
}

/*
 * THE FULLSCREEN PATH DECISION, every frame and every relayout: the inputs
 * go to nb_so_decide(), which applies the fixed rules and otherwise measures
 * the zero-copy path against the GPU pass (nb_scanout.h).  The layout key
 * restarts the measurement when anything that shapes the picture changes.
 */
static enum nb_so_path wl_so_decide(struct nb_wl *w, int slot, int pw, int ph)
{
    const struct nb_wl_buf *sl = slot >= 0 && slot < NB_MAX_BUFS &&
                                 w->bufs[slot].valid ? &w->bufs[slot] : NULL;
    struct nb_so_in in = {0};
    uint64_t key = 0;
    enum nb_so_path p;

    in.fullscreen = w->fullscreen;
    in.gs_ok = wl_gs_eligible(w, slot, &in.gs_off);
    in.gs_forced = w->gs_mode == NB_GS_ON;
    in.tear_or_latch = wl_async_wanted(w) || wl_latch_on(w);
    in.exact = wl_pic_exact(w, slot, pw, ph);
    in.nearest_scaled = !in.exact &&
                        w->vst.cur.filter == NB_FILTER_NEAREST;
    in.can_measure = w->presentation && w->pres_clock != UINT32_MAX;
    in.guest_scanout = sl ? wl_guest_scanout(w, sl) : -1;

    key = nb_so_mix(key, (uint64_t)w->fullscreen | (uint64_t)wl_async_wanted(w) << 1 |
                         (uint64_t)wl_latch_on(w) << 2 |
                         (uint64_t)w->place.cropped << 3 |
                         (uint64_t)(w->vst.cur.filter & 0xff) << 8 |
                         (uint64_t)(in.guest_scanout + 1) << 16 |
                         (uint64_t)(w->gs_mode & 0xff) << 24);
    key = nb_so_mix(key, (uint64_t)(uint32_t)pw << 32 | (uint32_t)ph);
    key = nb_so_mix(key, (uint64_t)(uint32_t)w->place.vis.x << 32 |
                         (uint32_t)w->place.vis.y);
    key = nb_so_mix(key, (uint64_t)(uint32_t)w->place.vis.w << 32 |
                         (uint32_t)w->place.vis.h);
    if (sl) {
        key = nb_so_mix(key, (uint64_t)sl->w << 32 | sl->h);
        key = nb_so_mix(key, (uint64_t)sl->fourcc);
        key = nb_so_mix(key, sl->modifier);
    }
    p = nb_so_decide(&w->so, &in, key, nb_mono_ns());
    if (nb_so_take_change(&w->so)) {
        nb_log("scanout path: %s [%s] - %s",
               p == NB_SO_GPU ? "GPU pass" : "zero-copy",
               nb_so_phase_name(w->so.phase), w->so.why);
    }
    return p;
}

/* Make sure the scaler and its targets exist at pw x ph.  false: not now
 * (targets still being created) or never (gs_dead). */
static bool wl_gs_ready(struct nb_wl *w, const struct nb_wl_buf *sl, int pw,
                        int ph)
{
    struct nb_gs_target tg[NB_GS_TARGETS];
    char why[200] = "";

    if (!w->gs) {
        w->gs = nb_gs_open(w->cfg ? w->cfg->render_node : NULL, 0, sl->fourcc,
                           sl->modifier, why, sizeof(why));
        if (!w->gs) {
            wl_gs_disable(w, why[0] ? why : "no GPU");
            return false;
        }
        nb_log("gpu-scale: ready on %s, GPU context priority %s requested, "
               "%s granted (CONDUIT_VIEWER_GPU_PRIORITY)", nb_gs_node(w->gs),
               nb_gs_prio_name(nb_gs_prio_requested(w->gs)),
               nb_gs_prio_name(nb_gs_prio_granted(w->gs)));
    }
    if (w->gt_w != (uint32_t)pw || w->gt_h != (uint32_t)ph ||
        w->gt_fb_gen != w->fb_gen || w->gt_fs != w->fullscreen) {
        struct nb_so_fmts v3;
        struct nb_pc_cand cand;
        int k;

        /* The layouts to allocate in, best first (nb_pc_targets): the
         * compositor's render tranche, the scanout one first in fullscreen;
         * the plain modifier list without feedback. */
        memset(&v3, 0, sizeof(v3));
        for (unsigned i = 0; i < w->formats.n; i++) {
            nb_so_fmts_add(&v3, w->formats.e[i].fourcc,
                           w->formats.e[i].modifier);
        }
        nb_pc_targets(w->fb_has_scan ? &w->fb_scan : NULL, w->fullscreen,
                      &w->fb_render, w->fb_has_render, &v3, &cand);
        gt_drop(w);
        w->gt_fb_gen = w->fb_gen;
        w->gt_fs = w->fullscreen;
        k = w->gt_want_rb || !w->dmabuf
                ? 0
                : nb_gs_targets(w->gs, (uint32_t)pw, (uint32_t)ph,
                                gs_fmt_ok, w, cand.fourcc, cand.mod, cand.n,
                                tg, why, sizeof(why));
        if (k != NB_GS_TARGETS) {
            /*
             * No dma-buf layout this GPU renders to that the compositor
             * takes (an NVIDIA GPU and a compositor on another GPU): the
             * pass into plain renderbuffers, read back into wl_shm.
             */
            char why2[200] = "";

            if (nb_gs_targets_readback(w->gs, (uint32_t)pw, (uint32_t)ph,
                                       why2, sizeof(why2)) != NB_GS_TARGETS ||
                !gt_shm_make(w, pw, ph)) {
                gt_drop(w);
                wl_gs_disable(w, why2[0] ? why2 : why[0] ? why
                                                         : "no output buffers");
                return false;
            }
            w->gt_w = (uint32_t)pw;
            w->gt_h = (uint32_t)ph;
            nb_log("gpu-scale: %dx%d read-back targets in shared memory on %s "
                   "(%s): one copy through memory per frame", pw, ph,
                   nb_gs_node(w->gs),
                   w->gt_want_rb ? "the compositor refused the dma-buf ones"
                                 : why[0] ? why : "no dma-buf target");
            return true;
        }
        for (int i = 0; i < NB_GS_TARGETS; i++) {
            struct zwp_linux_buffer_params_v1 *pp =
                zwp_linux_dmabuf_v1_create_params(w->dmabuf);
            struct gt_req *rq = malloc(sizeof(*rq));

            if (!pp || !rq) {
                free(rq);
                if (pp) {
                    zwp_linux_buffer_params_v1_destroy(pp);
                }
                wl_gs_disable(w, "zwp_linux_dmabuf_v1 create_params failed");
                return false;
            }
            rq->t = &w->gt[i];
            rq->gen = w->gt[i].gen;
            zwp_linux_buffer_params_v1_add(pp, tg[i].fd, 0, tg[i].offset,
                                           tg[i].stride,
                                           (uint32_t)(tg[i].modifier >> 32),
                                           (uint32_t)tg[i].modifier);
            zwp_linux_buffer_params_v1_add_listener(pp, &gt_params_listener,
                                                    rq);
            zwp_linux_buffer_params_v1_create(pp, (int32_t)tg[i].w,
                                              (int32_t)tg[i].h, tg[i].fourcc,
                                              0);
        }
        w->gt_w = (uint32_t)pw;
        w->gt_h = (uint32_t)ph;
        nb_log("gpu-scale: %dx%d %.4s modifier 0x%016llx output buffers, "
               "%s (the compositor answers before the first is used)", pw, ph,
               (const char *)&tg[0].fourcc,
               (unsigned long long)tg[0].modifier,
               !w->fb_has_scan ? "no scanout tranche to check against"
               : nb_so_fmts_has(&w->fb_scan, tg[0].fourcc, tg[0].modifier)
                   ? "in the compositor's scanout tranche"
                   : "NOT in the compositor's scanout tranche");
        wl_display_flush(w->dpy);
        return false;
    }
    return true;
}

/*
 * Draw the guest's frame in `slot` into a free target and attach it to the
 * main surface.  1: shown.  0: dropped this time (no free target yet; the
 * previous picture stays).  -1: not possible, use the compositor's scaling.
 */
static int wl_gs_present(struct nb_wl *w, int slot, int pw, int ph)
{
    struct nb_wl_buf *sl = &w->bufs[slot];
    const struct nb_place *p = &w->place;
    struct nb_gs_draw d;
    unsigned gpu_us = 0;
    double kx = (double)pw / (w->win_w > 0 ? w->win_w : 1);
    double ky = (double)ph / (w->win_h > 0 ? w->win_h : 1);
    int t = -1, r;

    if (!wl_gs_ready(w, sl, pw, ph)) {
        return w->gs_dead ? -1 : (w->gs_shown ? 0 : -1);
    }
    for (int k = 1; k <= NB_GS_TARGETS; k++) {
        int i = (w->gt_last + k) % NB_GS_TARGETS;

        if (w->gt[i].ready && !w->gt[i].busy) {
            t = i;
            break;
        }
    }
    if (t < 0) {
        w->st_gs_drop++;
        return w->gs_shown ? 0 : -1;
    }
    if (nb_gs_src(w->gs, slot, sl->fd, sl->w, sl->h, sl->stride, sl->offset,
                  sl->fourcc, sl->modifier) < 0) {
        wl_gs_disable(w, "the GPU cannot import the guest's frame");
        return -1;
    }
    d = (struct nb_gs_draw){
        .src_key = slot, .target = t,
        .dx = (int)(p->vis.x * kx + 0.5), .dy = (int)(p->vis.y * ky + 0.5),
        .dw = (int)(p->vis.w * kx + 0.5), .dh = (int)(p->vis.h * ky + 0.5),
        .sx = p->sw > 0 ? p->sx : 0, .sy = p->sh > 0 ? p->sy : 0,
        .sw = p->sw > 0 ? p->sw : sl->w, .sh = p->sh > 0 ? p->sh : sl->h,
        .nearest = w->vst.cur.filter == NB_FILTER_NEAREST,
    };
    r = nb_gs_draw(w->gs, &d, 50000, &gpu_us);
    if (r < 0) {
        wl_gs_disable(w, r == -ETIMEDOUT ? "the GPU pass took over 50 ms"
                                         : "the GPU pass failed");
        return -1;
    }
    if (w->gt_rb && w->gt_shm) {
        uint64_t r0 = nb_mono_ns();

        if (nb_gs_readback(w->gs, t, (char *)w->gt_shm +
                           (size_t)t * (w->gt_shm_sz / NB_GS_TARGETS)) < 0) {
            wl_gs_disable(w, "the read-back failed");
            return -1;
        }
        gpu_us += (unsigned)((nb_mono_ns() - r0) / 1000u);  /* in the cost */
    }
    w->st_gs_sum += gpu_us;
    w->st_gs_n++;
    if (sl->copy) {
        w->st_copy++;
    }
    if (gpu_us > w->st_gs_max) {
        w->st_gs_max = gpu_us;
    }
    w->gt_last = t;
    /* The main surface: one opaque output-size buffer, nothing on top. */
    wl_leave_sub(w);
    if (w->vp_src[0]) {
        wp_viewport_set_source(w->viewport, wl_fixed_from_int(-1),
                               wl_fixed_from_int(-1), wl_fixed_from_int(-1),
                               wl_fixed_from_int(-1));
        w->vp_src[0] = false;
    }
    if (pw == w->win_w && ph == w->win_h) {
        wp_viewport_set_destination(w->viewport, -1, -1);
    } else {
        wp_viewport_set_destination(w->viewport, w->win_w, w->win_h);
    }
    /* The pass was waited for: the acquire point is signalled at the
     * commit (acq 0). */
    wl_attach_sf(w, 0, w->gt[t].buf, NB_XA_GT, t, !w->gt_rb, 0);
    w->gt[t].busy = true;
    if (!w->gs_shown || w->tgt_ow != w->win_w || w->tgt_oh != w->win_h) {
        w->tgt_ow = w->win_w;
        w->tgt_oh = w->win_h;
        wl_opaque(w, w->surf, w->win_w, w->win_h);
    }
    if (!w->gs_shown) {
        w->gs_shown = true;
        w->frame_inflight = false;
        nb_log("picture: GPU-scaled into a %dx%d buffer on the main surface "
               "(guest %ux%u -> %dx%d at %d,%d%s): %s", pw, ph, sl->w,
               sl->h, d.dw, d.dh, d.dx, d.dy,
               d.nearest ? ", sharp" : "",
               sl->copy ? "a copy, because the compositor does not take the "
                          "guest's format/modifier"
                        : w->so.why);
    }
    return 1;
}

static struct wl_surface *wl_viewport_apply(struct nb_wl *w, int bw, int bh,
                                            struct wl_buffer *buf,
                                            bool new_frame,
                                            int *out_w, int *out_h)
{
    /*
     * A RESIZE IS A VIEWPORT, NOT A RE-MODE.  All three modes are a viewport
     * destination and an offset: no buffer is reallocated and no round trip
     * is made.  Whether the guest is asked to change mode is wl_mode_hint()'s
     * decision (fullscreen, or --resize=guest), taken from host state only.
     */
    int dw, dh, ox, oy;

    if (bw <= 0 || bh <= 0) {
        bw = w->win_w > 0 ? w->win_w : 1;
        bh = w->win_h > 0 ? w->win_h : 1;
    }
    /*
     * The arithmetic lives in nb_view.c (test/test_view.c): the output area,
     * the scale mode, and -- for a picture larger than its area at 1:1 -- the
     * buffer rectangle that is visible.  dw x dh is what is drawn, at
     * off_x/off_y; fit_* is the whole picture's size, which is the scale the
     * guest cursor is drawn at.
     */
    nb_view_place(&w->vst.cur, bw, bh, w->win_w, w->win_h, w->scale_120,
                  &w->place);
    dw = w->place.vis.w;
    dh = w->place.vis.h;
    (void)ox; (void)oy;
    w->fit_w = w->place.pic.w;
    w->fit_h = w->place.pic.h;
    bg_layout(w, dw, dh);
    w->off_x = w->place.vis.x;
    w->off_y = w->place.vis.y;
    if (w->ui.open) {
        wl_ed_show(w, true);    /* compare-only unless the area moved */
    }

    /*
     * FULLSCREEN + IMMEDIATE, NOT 1:1: the scaler draws it into an
     * output-size buffer on the main surface (nb_gpuscale.h).  Anything it
     * cannot do falls through to the compositor's scaling below.
     */
    bool left_gs = false;
    {
        int slot = new_frame ? w->pending : w->current;
        unsigned s120 = w->viewporter && w->scale_120 ? w->scale_120 : 120;
        int pw = (int)(((long)w->win_w * s120 + 60) / 120);
        int ph = (int)(((long)w->win_h * s120 + 60) / 120);

        const char *gs_off;
        bool eligible = wl_gs_eligible(w, slot, &gs_off);
        enum nb_so_path path = wl_so_decide(w, slot, pw, ph);

        /* A copy-only frame (nb_blit.h) always goes through the pass, in
         * every window mode; any other only when the decision says so. */
        bool copy_only = slot >= 0 && slot < NB_MAX_BUFS &&
                         w->bufs[slot].valid && w->bufs[slot].copy;
        /* --gpu-scale=off turns off the scaler, not the copy: a copy-only
         * frame has no other way to the screen. */
        bool usable = !w->gs_dead && w->viewport;

        w->gs_frame = false;
        w->gs_dropped = false;
        if ((copy_only && usable) || (eligible && path == NB_SO_GPU)) {
            int r = wl_gs_present(w, slot, pw, ph);

            if (r >= 0) {
                w->gs_frame = true;
                w->gs_dropped = r == 0;
                wl_geom_apply(w);
                *out_w = w->win_w;
                *out_h = w->win_h;
                return w->surf;
            }
        }
        if (w->gs_shown) {
            w->gs_shown = false;
            left_gs = true;
            w->tgt_ow = w->tgt_oh = 0;
            w->main_vw = w->main_vh = 0;
            nb_log("picture: zero-copy again (%s)", w->so.why);
        }
    }

    /*
     * WHICH SURFACE CARRIES THE PICTURE -- see cs_surf.  Exact fit: the main
     * surface, so a fullscreen guest at the output's mode stays a direct
     * scanout candidate.  Anything else: the subsurface, over a black main
     * surface that covers the whole window.
     */
    {
        bool want_sub = w->cs_surf && w->cs_vp && w->win_w > 0 &&
                        w->win_h > 0 &&
                        nb_view_has_bars(&w->place, w->win_w, w->win_h) &&
                        wl_blk_build(w);
        bool crop = w->place.cropped;
        struct wl_buffer *cur = w->current >= 0 && w->bufs[w->current].valid
                                    ? w->bufs[w->current].buf : NULL;
        struct wl_buffer *show = buf ? buf : cur;
        struct wl_surface *target;
        struct wp_viewport *vp;
        bool switched = left_gs;

        if (want_sub && !w->in_sub) {
            w->in_sub = true;
            switched = true;
            /* The main surface is about to show a 1x1 buffer: a source rect
             * left over from a cropped picture would be a protocol error. */
            if (w->vp_src[0] && w->viewport) {
                wp_viewport_set_source(w->viewport, wl_fixed_from_int(-1),
                                       wl_fixed_from_int(-1),
                                       wl_fixed_from_int(-1),
                                       wl_fixed_from_int(-1));
                w->vp_src[0] = false;
            }
            wl_attach_sf(w, 0, w->blk_buf, NB_XA_NONE, -1, false, 0);
            wl_surface_damage_buffer(w->surf, 0, 0, 1, 1);
            w->main_vw = w->main_vh = 0;
            w->tgt_ow = w->tgt_oh = 0;
        } else if (!want_sub && w->in_sub) {
            wl_leave_sub(w);
            switched = true;
        }
        target = w->in_sub ? w->cs_surf : w->surf;
        vp = w->in_sub ? w->cs_vp : w->viewport;
        if (show && (buf || switched)) {
            int si = buf ? w->pending : w->current;

            /* A new frame gets a new acquire point; the same content
             * attached again (a switch of surface) keeps its own. */
            wl_attach_sf(w, w->in_sub ? 1 : 0, show, NB_XA_GUEST, si,
                         !w->bufs[si].shm, buf ? 0 : w->buf_acq[si]);
        }
        if (w->in_sub) {
            if (w->main_vw != w->win_w || w->main_vh != w->win_h) {
                w->main_vw = w->win_w;
                w->main_vh = w->win_h;
                wp_viewport_set_destination(w->viewport, w->win_w, w->win_h);
                wl_opaque(w, w->surf, w->win_w, w->win_h);
                w->main_dirty = true;
            }
            if (w->cs_at_x != w->off_x || w->cs_at_y != w->off_y) {
                w->cs_at_x = w->off_x;
                w->cs_at_y = w->off_y;
                wl_subsurface_set_position(w->cs_sub, w->off_x, w->off_y);
                w->main_dirty = true;   /* position is the parent's state */
            }
        }
        if (vp) {
            bool *src_on = &w->vp_src[w->in_sub ? 1 : 0];

            /* 1:1 larger than the area: show only the visible part. */
            if (crop) {
                wp_viewport_set_source(vp, wl_fixed_from_double(w->place.sx),
                                       wl_fixed_from_double(w->place.sy),
                                       wl_fixed_from_double(w->place.sw),
                                       wl_fixed_from_double(w->place.sh));
                *src_on = true;
            } else if (*src_on) {
                wp_viewport_set_source(vp, wl_fixed_from_int(-1),
                                       wl_fixed_from_int(-1),
                                       wl_fixed_from_int(-1),
                                       wl_fixed_from_int(-1));
                *src_on = false;
            }
            if (dw == bw && dh == bh && !crop) {
                wp_viewport_set_destination(vp, -1, -1);
            } else {
                wp_viewport_set_destination(vp, dw, dh);
            }
        }
        /* Opaque: a compositor will not put a surface it might have to blend
         * on a plane, and the guest's 32-bit formats carry no meaningful
         * alpha.  Sticky state, so only on a change. */
        if (dw != w->tgt_ow || dh != w->tgt_oh || switched) {
            w->tgt_ow = dw;
            w->tgt_oh = dh;
            wl_opaque(w, target, dw, dh);
        }
        if (switched) {
            /* A frame callback armed on the other surface may never fire
             * (an emptied subsurface is not drawn); re-arm on this one. */
            w->frame_inflight = false;
            nb_log("picture: %s (%dx%d in a %dx%d window)",
                   w->in_sub ? "letterboxed on its own subsurface"
                             : "on the main surface, exact fit",
                   dw, dh, w->win_w, w->win_h);
        }
        wl_geom_apply(w);
        *out_w = dw;
        *out_h = dh;
        return target;
    }
}

/* After committing the picture's surface: commit the main surface too if the
 * letterbox changed parent state (its size, the picture's position). */
static void wl_main_settle(struct nb_wl *w)
{
    if (w->main_dirty) {
        w->main_dirty = false;
        if (w->in_sub) {
            wl_surf_commit(w, w->surf);
        }
    }
}

static int wl_commit(struct nb_session *s, struct nb_sink *sink)
{
    struct nb_wl *w = s->priv;
    struct nb_wl_buf *sl;
    struct wl_callback *cb;
    struct wl_surface *target = w->surf;
    int new_w, new_h;

    /*
     * A probe came back refused.  Told here rather than from the listener
     * because this is where the sink is in scope, and it is called often
     * enough that the news is never more than a frame late.
     */
    if (w->probe_bad_pending) {
        w->probe_bad_pending = false;
        nb_sink_format_verdict(sink, w->probe_bad_fourcc, w->probe_bad_mod,
                               wl_copy_ok(s, w->probe_bad_fourcc,
                                          w->probe_bad_mod));
    }

    if (w->pending < 0) {
        return -ENOENT;         /* COMMIT with nothing attached; not fatal */
    }
    sl = &w->bufs[w->pending];
    if (!sl->valid) {
        w->pending = -1;
        return -ENOENT;
    }
    /*
     * FLOOD GATE.  Held back, not dropped: the frame stays staged until the
     * compositor can take it, and a newer one replaces it.  Writing every
     * frame of a guest flipping at thousands per second fills the compositor
     * socket, and libwayland makes a full socket a fatal error.
     */
    if (!wl_gate_open(w, nb_mono_ns())) {
        w->pace.deferred++;
        w->st_held++;
        w->commit_staged = true;
        wl_latch_arm(w);
        return 0;
    }
    w->commit_staged = false;
    w->nudge_pending = false;   /* this frame repaints the cursor too */

    /*
     * THE FRAME-GLITCH DETECTOR.
     *
     * `held` means the compositor took this buffer and has not sent
     * wl_buffer.release for it.  Seeing it set here, for a buffer that is NOT
     * the one currently on the surface, means the guest has cycled its whole
     * scanout ring and come back to a buffer the compositor is still reading
     * -- so the guest has been rendering into it underneath the compositor.
     * That is exactly the "light, intermittent, stale-looking frame" a user
     * reports, and it is a real correctness bug rather than a cosmetic one.
     *
     * It is DETECTED and counted here, not fixed here: the broker cannot stop
     * the guest from drawing.  The fix is backpressure, and it has to reach
     * the guest -- see nb_sink_release() and the relay.
     */
    if (sl->held && w->current != w->pending) {
        w->n_reuse_inflight++;
        if (nb_trace_frames || w->n_reuse_inflight <= 8 ||
            (w->n_reuse_inflight % 256) == 0) {
            nb_log("frame: REUSE-IN-FLIGHT seq=%u buf=%llu slot=%d "
                   "(the compositor never released it; %llu so far)",
                   sl->seq, (unsigned long long)sl->id, w->pending,
                   (unsigned long long)w->n_reuse_inflight);
        }
    }
    if (nb_trace_frames) {
        int i, held = 0;

        for (i = 0; i < NB_MAX_BUFS; i++) {
            if (w->bufs[i].valid && w->bufs[i].held) {
                held++;
            }
        }
        nb_log("frame: commit seq=%u buf=%llu slot=%d prev-slot=%d held=%d",
               sl->seq, (unsigned long long)sl->id, w->pending, w->current,
               held);
    }

    /* A real frame supersedes the placeholder; drop its mapping rather than
     * hold a megabyte of shm for the life of the VM.
     *
     * The DROP happens after the commit below, not here -- see there.  This
     * comment described the drop for a long time while the code only cleared
     * the two flags, so the mapping and its wl_buffer were in fact held for
     * the life of the VM. */
    if (w->idle_wanted) {
        w->idle_wanted = false;
        w->idle_shown = false;
    }
    w->buf_w = (int)sl->w;
    w->buf_h = (int)sl->h;
    /*
     * A NEW GUEST MODE: its per-mode rule (nb_vstore_guest_mode) goes into
     * the layout here, before wl_viewport_apply() places this frame, so the
     * mode's first frame is already shown with it.  The notice and the menu
     * follow from the UI tick.
     */
    bool new_mode = sl->w != nb_guest_w || sl->h != nb_guest_h;

    if (new_mode) {
        if ((nb_guest_mode_changed(&w->vst, sl->w, sl->h, w->rule_note,
                                   sizeof(w->rule_note)) & NB_VIEW_CH_LAYOUT) ||
            w->rule_note[0]) {
            w->rule_due = true;
        }
        w->modes_dirty = true;
    }
    /*
     * With a viewport the surface measures the WINDOW, not the buffer, and the
     * compositor scales the guest's frame into it.  That is what makes the
     * window resizable without the guest re-moding: dragging a window is a
     * host gesture and has nothing to say about the resolution the guest chose
     * to scan out.  Without wp_viewporter the surface is the buffer, as
     * before, and the window is whatever size the guest picked.
     */
    if (w->viewport) {
        if (w->win_w <= 0 || w->win_h <= 0) {
            w->win_w = (int)sl->w;
            w->win_h = (int)sl->h;
            tb_update(w, w->win_w);
        }
        /*
         * SCALING POLICY, applied by wl_viewport_apply() so that a new frame
         * and a resize can never disagree about it: the WHOLE guest picture,
         * fitted per --scale (aspect by default, letterboxed), on whichever
         * surface wl_viewport_apply() says -- the main one for an exact fit,
         * the picture subsurface under a letterbox.  It also attaches.
         *
         * Whether the guest is asked to re-mode is wl_mode_hint()'s business
         * (fullscreen: the output's mode); this is what is shown meanwhile.
         */
        target = wl_viewport_apply(w, (int)sl->w, (int)sl->h, sl->buf, true,
                                   &new_w, &new_h);
        if (new_mode) {
            char sum[64];

            /* What the mode's FIRST frame was placed with (the rule, if
             * any, is already in force): checkable from the log. */
            nb_view_summary(&w->vst.cur, sum, sizeof(sum));
            nb_log("guest mode %ux%u: first frame %dx%d at %d,%d in %dx%d "
                   "(%s)%s%s", sl->w, sl->h, w->place.vis.w, w->place.vis.h,
                   w->place.vis.x, w->place.vis.y, w->win_w, w->win_h, sum,
                   w->rule_note[0] ? " - " : "", w->rule_note);
        }
        /* A copy-only frame the GPU pass did not draw (no free output
         * buffer yet, or the pass just failed) has nothing else to show:
         * skipped like a dropped scaled frame, never attached. */
        if (w->gs_dropped || (sl->copy && !w->gs_frame)) {
            /* Every scaler buffer is still on screen or queued: skip this
             * frame (the next one replaces it) and give it back now. */
            if (w->sink && !wl_hold_release(s, sl->id, sl->seq)) {
                nb_sink_release(w->sink, sl->id, sl->seq);
            }
            w->pending = -1;
            return 0;
        }
        /* The guest cursor is drawn at the picture's scale; a re-mode or a
         * resize changes it.  Compare-only when nothing moved. */
        if (w->gc_on && gc_layout(w, false)) {
            cur_apply(w, w->last_serial);
        }
        /* ...and so is the one the viewer draws under grab (compare-only
         * when nothing moved). */
        if (w->spr_mapped || w->grabbed) {
            spr_update(w);
        }
    } else {
        wl_attach_sf(w, 0, sl->buf, NB_XA_GUEST, w->pending, !sl->shm, 0);
        new_w = (int)sl->w;
        new_h = (int)sl->h;
        /* No viewporter: the surface is the buffer.  An opaque region is a
         * precondition for direct scanout; sticky, so only on a change. */
        if (new_w != w->surf_w || new_h != w->surf_h) {
            wl_opaque(w, w->surf, new_w, new_h);
        }
    }
    if (w->gs_frame) {
        wl_surface_damage_buffer(target, 0, 0, INT32_MAX, INT32_MAX);
    } else {
        wl_surface_damage_buffer(target, 0, 0, (int32_t)sl->w,
                                 (int32_t)sl->h);
    }
    if (w->presentation) {
        struct wp_presentation_feedback *fb =
            wp_presentation_feedback(w->presentation, target);
        struct nb_pres_ctx *pc = calloc(1, sizeof(*pc));

        if (fb && pc) {
            uint64_t fl = nb_flip_ns(w, sl);

            pc->w = w;
            pc->seq = sl->seq;
            pc->start_ns = fl ? fl : sl->recv_ns;
            pc->recv_ns = sl->recv_ns;
            pc->so_gen = w->so.gen;
            pc->so_path = w->gs_frame ? NB_SO_GPU : NB_SO_ZC;
            wp_presentation_feedback_add_listener(fb, &nb_pres_listener, pc);
            w->pc_last = pc;    /* commit_ns filled in right after commit */
        } else {
            free(pc);
            if (fb) {
                wp_presentation_feedback_destroy(fb);
            }
            w->pc_last = NULL;
        }
    }
    {
        bool armed_now = false;

        if (!w->frame_inflight) {
            cb = wl_surface_frame(target);
            if (cb) {
                wl_callback_add_listener(cb, &frame_listener, w);
                w->frame_inflight = true;
                armed_now = true;
            }
        }
        nb_pace_committed(&w->pace, nb_mono_ns(), armed_now);
    }
    /*
     * The letterbox offsets the resize borders hang off are computed by
     * bg_layout() inside the wl_viewport_apply() above, so re-place them here
     * rather than only on the resize path -- otherwise the FIRST frame, the one
     * that introduces the letterbox, leaves them where xdg_configure put them.
     * Guarded on its inputs, so a steady stream of same-size frames pays only
     * the compare.
     */
    bd_layout(w);
    wl_surf_commit(w, target);
    wl_main_settle(w);          /* the letterbox's parent state, if it moved */
    /* The drawn cursor came or went in this frame: the area it covers may lie
     * over the bars, outside the frame's damage. */
    spr_kick_now(w);
    if (!w->has_content) {
        w->has_content = true;  /* mapped now: a window property can stick */
        wl_hypr_sync(w);
    }
    if (w->viewport) {
        wl_ptr_follow(w);       /* compare-only unless the picture moved */
    }
    nb_pace_flushed(&w->pace, wl_display_flush(w->dpy) < 0 &&
                              errno == EAGAIN);  /* out NOW: see FLUSH below */
    {
        struct nb_pres_ctx *pc_now = w->pc_last;   /* nb_stats_commit clears it */
        uint64_t now, aimed = 0;

        nb_stats_commit(w, sl);
        now = nb_mono_ns();

        if (wl_latch_on(w)) {
            /* Cost: from the window opening (or the frame's arrival, if it
             * came later) to the flush -- the GPU scaling pass included. */
            uint64_t from = w->latch_open_ns > sl->recv_ns ? w->latch_open_ns
                                                           : sl->recv_ns;

            aimed = nb_latch_committed(&w->latch, now,
                                       from && now > from ? now - from : 0);
            if (pc_now) {
                pc_now->aimed = aimed;
            }
        }
        wl_flog(w, "C %u %llu %llu %llu %llu %c\n", sl->seq,
                (unsigned long long)sl->recv_ns,
                (unsigned long long)nb_flip_ns(w, sl),
                (unsigned long long)now, (unsigned long long)aimed,
                wl_latch_on(w) ? 'l' : w->tearing_async ? 'i' : 'v');
    }

    /*
     * NOW drop the idle placeholder, and only now.
     *
     * Its mapping and wl_buffer used to survive the whole life of the VM
     * despite the comment above claiming otherwise -- around a megabyte of shm
     * plus a protocol object, per broker, held for a picture that has been off
     * screen since the first guest frame.
     *
     * After wl_surface_commit(), for the same reason wl_idle_make() gives for
     * destroying in that order: the compositor processes attach+commit before
     * it sees the destroy, so the placeholder cannot be yanked out from under
     * a frame it is still scanning out.  Dropping it BEFORE the attach would
     * do exactly that.
     *
     * Safe to repeat and safe to lose: wl_idle_drop() is idempotent, and
     * ops->show_idle re-makes the buffer if the VM goes away again.
     */
    if (w->idle_buf) {
        wl_idle_drop(w);
    }

    if (w->gs_frame) {
        /* Scaled: the GPU pass has finished reading the guest's buffer
         * (nb_gs_draw waited), and the compositor never saw it.  Back to
         * the guest at once -- unless an earlier commit of it, from before
         * the scaler took over, is still on screen. */
        if (w->sink && !wl_hold_release(s, sl->id, sl->seq)) {
            nb_sink_release(w->sink, sl->id, sl->seq);
        }
    } else {
        sl->held = true;
    }
    sl->commits++;
    if (w->current < 0) {
        /* First guest frame after the placeholder: the guest draws the cursor
         * from here on, so ours has to go even though the pointer never left. */
        w->current = w->pending;
        cur_apply(w, w->last_serial);
    }
    w->current = w->pending;
    w->pending = -1;
    /*
     * RECORD WHAT WE PRESENTED.  DO NOT REPORT IT.
     *
     * EV_SURFACE is an INSTRUCTION -- "guest, render this size" -- and it must
     * only ever carry what the WINDOW can show.  What arrives here is the size
     * the guest actually sent, which is an OBSERVATION.  Sending it back told
     * the guest to render the size it had just rendered, and on a fullscreen
     * window the two disagree, so they traded places forever:
     *
     *   configure: the window can show 3760x2118  -> "render 3760x2118"
     *   commit:    the guest's buffer is 1920x1080 -> "render 1920x1080"
     *   configure: the window can still show 3760x2118 -> ...
     *
     * OBSERVED on a 4K display at scale 1.5: the guest flipped between
     * 1920x1080 and 3760x2118 about every two seconds, forever.  (2160 - 42 is
     * the 28px title bar at 1.5x, which is why the number looks arbitrary.)
     *
     * This is the same rule the WINDOW handler states one layer up -- a guest
     * re-mode does not move the user's window -- applied to the buffer layer:
     * what the guest did is not an instruction to the guest.
     */
    w->surf_w = new_w;
    w->surf_h = new_h;
    /*
     * FLUSH, or the frame sits in libwayland's outgoing buffer.
     *
     * wl_surface_commit() only QUEUES the request; nothing reaches the
     * compositor until the connection is flushed.  Every other request this
     * backend issues is followed by a flush, and the present path -- the one
     * that runs most often -- was the one that was not.
     *
     * A busy guest hid it completely: the next frame's flush pushed the
     * previous one out microseconds later, so the lag was invisible.  It only
     * showed on an IDLE desktop, where a single event produces exactly one
     * frame and nothing follows to flush it.  REPORTED as: type a character in
     * Konsole and nothing appears, then move the mouse and it appears -- the
     * mouse motion was doing the flush the commit should have done.
     */
    wl_display_flush(w->dpy);
    return 0;
}

/*
 * THE CURSOR NUDGE (nb_pace.h): the current picture committed again, so a
 * compositor that repaints a tearing output only on our commits (Hyprland)
 * redraws its software cursor at the pointer's new position.  Only over a
 * fullscreen, immediately presented picture with the host pointer visible,
 * never faster than twice the refresh, through the same gate as frames.
 */
static bool wl_nudge_wanted(const struct nb_wl *w)
{
    /* No image (NULL cursor, always so under grab, unless the viewer draws
     * the guest's): nothing for the compositor to repaint, and a commit
     * would only cost it work. */
    return nb_nudge_wanted(w->tearing_async, w->fullscreen,
                           (w->ptr_on_content && w->cur_img_on &&
                            !w->grabbed && !w->ui.open) || w->spr_mapped,
                           w->current >= 0);
}

static uint64_t wl_nudge_min_ns(const struct nb_wl *w)
{
    /* half a refresh period; 1 ms until the refresh is known */
    return w->refresh_mhz ? 500000000000ull / w->refresh_mhz : 1000000ull;
}

static void wl_nudge(struct nb_wl *w)
{
    uint64_t now = nb_mono_ns();
    struct wl_surface *sf;
    struct wl_buffer *b;
    bool armed = false;

    if (!wl_nudge_wanted(w)) {
        w->nudge_pending = false;
        return;
    }
    if (w->commit_staged && w->pending >= 0) {
        w->nudge_pending = false;       /* the staged frame repaints anyway */
        return;
    }
    if (!nb_nudge_may(&w->pace, now, wl_nudge_min_ns(w))) {
        w->nudge_pending = true;
        return;
    }
    if (w->gs_shown) {
        sf = w->surf;
        b = w->gt[w->gt_last].buf;
    } else {
        const struct nb_wl_buf *sl = &w->bufs[w->current];

        /* Only a buffer the compositor still holds: once released it is the
         * guest's again and may be half drawn. */
        if (!sl->valid || !sl->held) {
            w->nudge_pending = false;
            return;
        }
        sf = w->in_sub ? w->cs_surf : w->surf;
        b = sl->buf;
    }
    if (!sf || !b) {
        w->nudge_pending = false;
        return;
    }
    if (w->gs_shown) {
        wl_attach_sf(w, 0, b, NB_XA_GT, w->gt_last, !w->gt_rb,
                     w->gt_acq[w->gt_last]);
    } else {
        wl_attach_sf(w, sf == w->cs_surf ? 1 : 0, b, NB_XA_GUEST, w->current,
                     !w->bufs[w->current].shm, w->buf_acq[w->current]);
    }
    wl_surface_damage_buffer(sf, 0, 0, 1, 1);
    if (!w->frame_inflight) {
        struct wl_callback *cb = wl_surface_frame(sf);

        if (cb) {
            wl_callback_add_listener(cb, &frame_listener, w);
            w->frame_inflight = true;
            armed = true;
        }
    }
    wl_surf_commit(w, sf);
    nb_pace_committed(&w->pace, now, armed);
    nb_pace_flushed(&w->pace, wl_display_flush(w->dpy) < 0 && errno == EAGAIN);
    w->nudge_pending = false;
    w->st_nudges++;
}

/* Commit the staged frame the gate held back, if the gate is open now; else
 * a cursor nudge that was held. */
static void wl_commit_staged(struct nb_wl *w)
{
    if (w->commit_staged && w->pending >= 0 && w->sess && w->sink &&
        wl_gate_open(w, nb_mono_ns())) {
        (void)wl_commit(w->sess, w->sink);
        return;
    }
    if (w->commit_staged && w->pending >= 0 && !w->latch_armed) {
        wl_latch_arm(w);        /* the window moved on: wait for the next one */
    }
    if (w->nudge_pending) {
        wl_nudge(w);
    }
}

/* ── the client-side title bar ───────────────────────────────────────────── */
/*
 * GNOME'S MUTTER DOES NOT ADVERTISE zxdg_decoration_manager_v1 AT ALL
 * (verified on GNOME Shell 50.1, 2026-08-24): server-side decorations are not
 * something it can be asked for, so a toplevel that draws none arrives bare --
 * no title bar, no close button, and the --title nobody renders.  On a
 * compositor that DOES offer the protocol we ask for server-side and use it;
 * this is the fallback for the one every desktop Linux user is running.
 *
 * Placed at y = -NB_TB_H, so the bar sits ABOVE the content rather than on top
 * of it.  That is not cosmetic: a subsurface overlapping the main surface
 * occludes it, and an occluded surface cannot be promoted to a hardware plane.
 * The whole reason this backend hands the guest's buffer over untouched is to
 * stay a scanout candidate, and decorations must not spend that.
 */
         /* drawn characters of --title in the bar */

/* Buttons, counted from the right: 0 = close, 1 = maximize, 2 = minimize.
 * Returns -1 for "not on a button", i.e. somewhere to drag the window by. */
static int tb_hit(const struct nb_wl *w, int x)
{
    int i;

    for (i = 0; i < 3; i++) {
        if (x >= w->tb_w - (i + 1) * NB_TB_BTN &&
            x <  w->tb_w - i * NB_TB_BTN) {
            return i;
        }
    }
    return -1;
}

static void tb_drop(struct nb_wl *w)
{
    if (w->tb_buf) {
        wl_buffer_destroy(w->tb_buf);
        w->tb_buf = NULL;
    }
    if (w->tb_px) {
        munmap(w->tb_px, w->tb_sz);
        w->tb_px = NULL;
        w->tb_sz = 0;
    }
    w->tb_w = 0;
}

static void tb_paint(struct nb_wl *w, int width)
{
    uint32_t *px = w->tb_px;
    const uint32_t bg  = w->grabbed ? 0xff43301aU : 0xff1b2027U;
    const uint32_t fg  = 0xffe6edf3U;
    const uint32_t hot = 0xffffb454U;
    const char *txt;
    char composed[256];
    bool hot_txt;
    unsigned tw, scale = 2, x;

    if (!px) {
        return;
    }
    nb_placeholder_fill(px, (unsigned)width, NB_TB_H, (unsigned)width, bg);
    /* one-pixel bottom rule, so the bar reads as chrome and not as content */
    for (x = 0; x < (unsigned)width; x++) {
        px[(size_t)(NB_TB_H - 1) * width + x] = 0xff2b3947U;
    }

    /*
     * UNDER GRAB THE TITLE IS THE ONLY WAY OUT.  CTRL+ALT+G is the sole
     * release gesture by design -- every other key is going to the guest -- so
     * the bar says so, in the accent colour, for as long as the grab is held.
     */
    {
        /*
         * STATUS AND TITLE, not status INSTEAD OF title.  Losing the window
         * name is a real cost when several are open -- but a status line is
         * only worth having if it is legible, so when both do not fit the
         * status keeps the space and the title is what goes.
         */
        const char *status = NULL;

        if (w->clip_notice_until && nb_now_ms_wl() < w->clip_notice_until) {
            status = "THE VM CHANGED YOUR CLIPBOARD";
        } else {
            w->clip_notice_until = 0;
            if (w->grabbed) {
                status = "GRABBED - CTRL+ALT+G TO RELEASE";
            } else if (w->vmp.paused) {
                status = NB_VM_PAUSED_STATUS;
            }
        }
        hot_txt = status != NULL;
        if (status && w->title[0]) {
            const unsigned avail = (unsigned)(width - 3 * NB_TB_BTN - 16);

            snprintf(composed, sizeof composed, "%s - %s", status, w->title);
            txt = nb_placeholder_text_w(composed, scale) <= avail
                      ? composed : status;
        } else if (!status && w->tb_stats[0]) {
            const unsigned avail = (unsigned)(width - 3 * NB_TB_BTN - 16);

            snprintf(composed, sizeof composed, "%s - %s", w->title,
                     w->tb_stats);
            txt = nb_placeholder_text_w(composed, scale) <= avail
                      ? composed : w->title;
        } else {
            txt = status ? status : w->title;
        }
    }
    tw = nb_placeholder_text_w(txt, scale);
    while (scale > 1 && tw > (unsigned)(width - 3 * NB_TB_BTN - 16)) {
        scale--;
        tw = nb_placeholder_text_w(txt, scale);
    }
    nb_placeholder_text(px, (unsigned)width, NB_TB_H, (unsigned)width,
                        10, (NB_TB_H - 7 * scale) / 2, txt, scale,
                        hot_txt ? hot : fg);

    /*
     * Three buttons at the right, in the order every desktop puts them:
     * minimize, maximize, close.  Drawn as pixels rather than glyphs -- they
     * are not letters and the 5x7 font has no box-drawing in it.
     */
    {
        int b;

        for (b = 0; b < 3; b++) {
            int cx = width - (b + 1) * NB_TB_BTN + (NB_TB_BTN - 11) / 2;
            int cy = (NB_TB_H - 11) / 2;
            int k;

            if (cx < 0) {
                continue;
            }
            /* Hover feedback.  Without it there is no way to tell a button
             * from a decoration until you have already clicked it. */
            if (b == w->tb_press) {
                int hx0 = width - (b + 1) * NB_TB_BTN;
                int hy, hx;

                for (hy = 0; hy < NB_TB_H - 1; hy++) {
                    for (hx = hx0; hx < hx0 + NB_TB_BTN && hx < width; hx++) {
                        if (hx >= 0) {
                            px[(size_t)hy * width + hx] =
                                b == 0 ? 0xffe05a4aU : 0xff7d8c9bU;
                        }
                    }
                }
            } else if (b == w->tb_hover) {
                int hx0 = width - (b + 1) * NB_TB_BTN;
                int hy, hx;

                for (hy = 0; hy < NB_TB_H - 1; hy++) {
                    for (hx = hx0; hx < hx0 + NB_TB_BTN && hx < width; hx++) {
                        if (hx >= 0) {
                            px[(size_t)hy * width + hx] =
                                b == 0 ? 0xffc0392bU : 0xff53606eU;
                        }
                    }
                }
            }
            if (b == 0) {                       /* close: an X */
                for (k = 0; k < 11; k++) {
                    px[(size_t)(cy + k) * width + cx + k] = fg;
                    px[(size_t)(cy + k) * width + cx + 10 - k] = fg;
                }
            } else if (b == 1) {                /* maximize: a box */
                for (k = 0; k < 11; k++) {
                    px[(size_t)cy * width + cx + k] = fg;
                    px[(size_t)(cy + 10) * width + cx + k] = fg;
                    px[(size_t)(cy + k) * width + cx] = fg;
                    px[(size_t)(cy + k) * width + cx + 10] = fg;
                }
            } else {                            /* minimize: a rule */
                for (k = 0; k < 11; k++) {
                    px[(size_t)(cy + 9) * width + cx + k] = fg;
                    px[(size_t)(cy + 10) * width + cx + k] = fg;
                }
            }
        }
    }
}

static void tb_update(struct nb_wl *w, int width)
{
    struct wl_shm_pool *pool;
    size_t stride, sz;
    void *px;
    int fd;

    if (!w->tb_surf || !w->shm) {
        return;
    }
    /*
     * NO CHROME IN FULLSCREEN.  Every other application hides its decorations
     * there and so must this one -- and it is not only cosmetic: fullscreen is
     * the state in which the content surface can reach a hardware plane, and a
     * mapped subsurface hanging off it is one more reason for a compositor to
     * decline.  Attaching a NULL buffer unmaps the bar outright.
     */
    if (w->fullscreen) {
        if (w->tb_mapped) {
            wl_surface_attach(w->tb_surf, NULL, 0, 0);
            wl_surface_commit(w->tb_surf);
            w->tb_mapped = false;
            wl_geom_apply(w);
            wl_surf_commit(w, w->surf);
            wl_display_flush(w->dpy);
        }
        return;
    }
    if (width <= 3 * NB_TB_BTN + 20) {
        return;
    }
    if (width != w->tb_w) {
        stride = (size_t)width * 4;
        sz = stride * NB_TB_H;
        fd = memfd_create("conduit-viewer-titlebar", MFD_CLOEXEC);
        if (fd < 0) {
            return;
        }
        if (ftruncate(fd, (off_t)sz) < 0) {
            close(fd);
            return;
        }
        px = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (px == MAP_FAILED) {
            close(fd);
            return;
        }
        pool = wl_shm_create_pool(w->shm, fd, (int32_t)sz);
        close(fd);
        if (!pool) {
            munmap(px, sz);
            return;
        }
        tb_drop(w);
        w->tb_buf = wl_shm_pool_create_buffer(pool, 0, width, NB_TB_H,
                                              (int32_t)stride,
                                              WL_SHM_FORMAT_XRGB8888);
        wl_shm_pool_destroy(pool);
        if (!w->tb_buf) {
            munmap(px, sz);
            return;
        }
        w->tb_px = px;
        w->tb_sz = sz;
        w->tb_w  = width;
    }
    tb_paint(w, w->tb_w);
    if (!w->tb_mapped) {
        nb_log("title bar: mapped, %dx%d at the top of the window",
               w->tb_w, NB_TB_H);
    }
    w->tb_mapped = true;
    /* Sit at the WINDOW's top-left, which is off_* up-left of the content
     * when aspect mode is drawing bars. */
    if (w->tb_sub) {
        wl_subsurface_set_position(w->tb_sub, 0, -NB_TB_H);
    }
    wl_surface_attach(w->tb_surf, w->tb_buf, 0, 0);
    wl_surface_damage_buffer(w->tb_surf, 0, 0, w->tb_w, NB_TB_H);
    wl_surface_commit(w->tb_surf);
    /* The window is the content PLUS the bar sitting above it, and PLUS any
     * letterbox bars around it. */
    wl_geom_apply(w);
    wl_surf_commit(w, w->surf);
    wl_display_flush(w->dpy);
}

/* ── clipboard ───────────────────────────────────────────────────────────── */

#define NB_CLIP_MIME "text/plain;charset=utf-8"

static uint64_t nb_now_ms_wl(void)
{
    struct timespec ts;

    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000u + (uint64_t)(ts.tv_nsec / 1000000);
}

/*
 * Text types we read the host selection as, best first.  Not every program
 * offers the canonical one: X11 programs through XWayland and some toolkits
 * offer only UTF8_STRING or text/plain, and such a copy used to be skipped
 * without a word.  Whatever arrives is validated as UTF-8 before it goes on.
 */
static const char *const nb_wl_text_mimes[] = {
    NB_CLIP_MIME, "UTF8_STRING", "text/plain", "TEXT", "STRING",
};
#define NB_WL_TEXT_MIMES (int)(sizeof(nb_wl_text_mimes) / sizeof(nb_wl_text_mimes[0]))

/* The rank of `mime` (higher is better), 0 if it is not text we take. */
static int nb_wl_text_rank(const char *mime)
{
    for (int i = 0; i < NB_WL_TEXT_MIMES; i++) {
        if (!strcmp(mime, nb_wl_text_mimes[i])) {
            return NB_WL_TEXT_MIMES - i;
        }
    }
    return 0;
}

/* Private MIME type our own data source also offers. A selection offer that
 * carries it is ours (the guest's text); one without it belongs to someone
 * else. This does not rely on wl_data_source.cancelled, which Hyprland 0.39
 * did not deliver when another client took the selection -- the viewer then
 * kept believing it owned the clipboard and never sent host copies. */
#define NB_WL_OWN_MIME "application/x-conduit-own"

static const char *nb_wl_text_mime(int rank)
{
    return rank > 0 && rank <= NB_WL_TEXT_MIMES
        ? nb_wl_text_mimes[NB_WL_TEXT_MIMES - rank] : NB_CLIP_MIME;
}

static void doffer_offer(void *d, struct wl_data_offer *o, const char *mime)
{
    struct nb_wl *w = d;
    int rank = nb_wl_text_rank(mime);

    if (!strcmp(mime, NB_WL_OWN_MIME)) {
        if (w->offer == o) {
            w->offer_own = true;
        } else if (w->pending_offer == o) {
            w->pending_offer_own = true;
        }
        return;
    }
    if (w->offer == o && rank > w->offer_text) {
        w->offer_text = rank;
    } else if (w->pending_offer == o && rank > w->pending_offer_text) {
        w->pending_offer_text = rank;
    }
}
static void doffer_source_actions(void *d, struct wl_data_offer *o, uint32_t a) {}
static void doffer_action(void *d, struct wl_data_offer *o, uint32_t a) {}
static const struct wl_data_offer_listener doffer_listener = {
    .offer = doffer_offer,
    .source_actions = doffer_source_actions,
    .action = doffer_action,
};

static void ddev_data_offer(void *d, struct wl_data_device *dev,
                            struct wl_data_offer *o)
{
    struct nb_wl *w = d;

    /* This event does not say whether `o` is a selection or drag-and-drop
     * offer.  Association comes in selection()/enter(), so replacing the
     * current selection here confuses the two object lifetimes. */
    if (w->pending_offer && w->pending_offer != w->offer) {
        wl_data_offer_destroy(w->pending_offer);
    }
    w->pending_offer = o;
    w->pending_offer_text = 0;
    w->pending_offer_own = false;
    wl_data_offer_add_listener(o, &doffer_listener, w);
}
static void ddev_selection(void *d, struct wl_data_device *dev,
                           struct wl_data_offer *o)
{
    struct nb_wl *w = d;

    if (!o) {
        if (w->offer) {
            wl_data_offer_destroy(w->offer);
        }
        w->offer = NULL;
        w->offer_text = 0;
        w->offer_own = false;
        return;
    }
    /* The offer we were told about in data_offer is now THE selection. */
    if (w->offer && w->offer != o) {
        wl_data_offer_destroy(w->offer);
    }
    w->offer = o;
    if (w->pending_offer == o) {
        w->offer_text = w->pending_offer_text;
        w->offer_own = w->pending_offer_own;
        w->pending_offer = NULL;
        w->pending_offer_text = 0;
        w->pending_offer_own = false;
    } else {
        w->offer_own = false;
        /* The protocol promises data_offer first.  Fail this offer closed if a
         * compositor violates that ordering; it has no recorded MIME set. */
        w->offer_text = 0;
    }
    /* Someone else's selection: ours is gone even if `cancelled` never came. */
    if (!w->offer_own && w->source) {
        wl_data_source_destroy(w->source);
        w->source = NULL;
        free(w->src_text);
        w->src_text = NULL;
        w->src_len = 0;
    }
    /* Conduit push modes: a new host selection, seen only while focused --
     * the core decides whether it goes to the guest.  Compositors differ in
     * whether this comes before or after wl_keyboard.enter (Hyprland may send
     * it first); focus-in fetches too, and the core drops repeats by
     * content, so either order sends each new text exactly once. */
    if (!w->offer_text) {
        nb_log("clipboard: the new host selection has no text type; not "
               "sent to the VM");
    } else if (w->sink) {
        nb_sink_host_clipboard_changed(w->sink);
    }
}
static void ddev_enter(void *d, struct wl_data_device *v, uint32_t s,
                       struct wl_surface *su, wl_fixed_t x, wl_fixed_t y,
                       struct wl_data_offer *o)
{
    struct nb_wl *w = d;

    /* Drag-and-drop is deliberately unsupported.  Destroy its offer now that
     * the association is known, without disturbing the clipboard selection. */
    if (o && o == w->pending_offer) {
        wl_data_offer_destroy(o);
        w->pending_offer = NULL;
        w->pending_offer_text = 0;
    }
}
static void ddev_leave(void *d, struct wl_data_device *v) {}
static void ddev_motion(void *d, struct wl_data_device *v, uint32_t t,
                        wl_fixed_t x, wl_fixed_t y) {}
static void ddev_drop(void *d, struct wl_data_device *v) {}
static const struct wl_data_device_listener ddev_listener = {
    .data_offer = ddev_data_offer,
    .enter = ddev_enter,
    .leave = ddev_leave,
    .motion = ddev_motion,
    .drop = ddev_drop,
    .selection = ddev_selection,
};

/* ── guest -> host: we become the selection owner ────────────────────────── */

static void dsrc_send(void *d, struct wl_data_source *src, const char *mime,
                      int fd)
{
    struct nb_wl *w = d;
    size_t off = 0;
    int flags, slot = -1;

    if (!w->src_text || strcmp(mime, NB_CLIP_MIME)) {
        close(fd);
        return;
    }
    /* The requester owns this fd.  It must not be allowed to stop the single
     * display/input loop by declining to read: nonblocking, and whatever does
     * not fit now is finished from the poll loop (wl_send_pump). */
    flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0) {
        nb_err("clipboard: cannot make a host requester's selection fd "
               "nonblocking: %s; refusing that request", strerror(errno));
        close(fd);
        return;
    }
    while (off < w->src_len) {
        ssize_t n = write(fd, w->src_text + off, w->src_len - off);

        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
            break;
        }
        if (n <= 0) {
            close(fd);
            return;
        }
        off += (size_t)n;
    }
    if (off == w->src_len) {
        close(fd);
        return;
    }
    for (int i = 0; i < NB_WL_MAX_SENDS; i++) {
        if (w->send[i].fd < 0) {
            slot = i;
            break;
        }
    }
    if (slot < 0) {
        nb_log("clipboard: too many host requesters reading at once; "
               "refusing one");
        close(fd);
        return;
    }
    w->send[slot].buf = malloc(w->src_len - off);
    if (!w->send[slot].buf) {
        close(fd);
        return;
    }
    memcpy(w->send[slot].buf, w->src_text + off, w->src_len - off);
    w->send[slot].len = w->src_len - off;
    w->send[slot].off = 0;
    w->send[slot].fd = fd;
    w->send[slot].pfd = -1;
}

static void wl_send_drop(struct nb_wl *w, int i)
{
    close(w->send[i].fd);
    free(w->send[i].buf);
    w->send[i].fd = -1;
    w->send[i].buf = NULL;
    w->send[i].len = w->send[i].off = 0;
    w->send[i].pfd = -1;
}

/* Continue a selection transfer the requester is now ready for. */
static void wl_send_pump(struct nb_wl *w, int i)
{
    while (w->send[i].off < w->send[i].len) {
        ssize_t n = write(w->send[i].fd, w->send[i].buf + w->send[i].off,
                          w->send[i].len - w->send[i].off);

        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
            return;
        }
        if (n <= 0) {
            break;
        }
        w->send[i].off += (size_t)n;
    }
    wl_send_drop(w, i);
}

static void dsrc_cancelled(void *d, struct wl_data_source *src)
{
    struct nb_wl *w = d;

    /* Someone else took the selection.  Ours is dead; drop it so we do not
     * keep the guest's text alive longer than it is on the clipboard. */
    if (w->source == src) {
        wl_data_source_destroy(src);
        w->source = NULL;
        free(w->src_text);
        w->src_text = NULL;
        w->src_len = 0;
    }
}
static void dsrc_target(void *d, struct wl_data_source *s, const char *m) {}
static void dsrc_dnd_drop(void *d, struct wl_data_source *s) {}
static void dsrc_dnd_finished(void *d, struct wl_data_source *s) {}
static void dsrc_action(void *d, struct wl_data_source *s, uint32_t a) {}
static const struct wl_data_source_listener dsrc_listener = {
    .target = dsrc_target,
    .send = dsrc_send,
    .cancelled = dsrc_cancelled,
    .dnd_drop_performed = dsrc_dnd_drop,
    .dnd_finished = dsrc_dnd_finished,
    .action = dsrc_action,
};

static int wl_set_clipboard(struct nb_session *s, const char *text, size_t len);
static void wl_notify_clipboard(struct nb_session *s);

/* Put a held guest copy on the clipboard now that the window has focus. */
static void wl_clip_flush_pending(struct nb_wl *w)
{
    char *text;
    size_t len;

    if (!w->clip_pending || !w->focused) {
        return;
    }
    /* Detach first: wl_set_clipboard() is about to be re-entered and must not
     * see the pending copy again, or focus-in would loop. */
    text = w->clip_pending; len = w->clip_pending_len;
    w->clip_pending = NULL; w->clip_pending_len = 0;
    if (w->sess && wl_set_clipboard(w->sess, text, len) == 0) {
        nb_log("the VM's clipboard text is now on yours (it was held while "
               "the window was not focused)");
        wl_notify_clipboard(w->sess);
    }
    free(text);
}

static int wl_set_clipboard(struct nb_session *s, const char *text, size_t len)
{
    struct nb_wl *w = s->priv;
    char *copy;

    if (!w->ddm || !w->ddev || !w->seat) {
        return -ENOTSUP;
    }
    copy = malloc(len + 1);
    if (!copy) {
        return -ENOMEM;
    }
    memcpy(copy, text, len);
    copy[len] = '\0';

    /*
     * NOT FOCUSED: hold it, do not apply it.  Wayland would refuse the
     * selection anyway (no input serial to justify it), so the alternative is
     * not "apply it regardless" -- it is "fail silently and lose the copy".
     * Only the most recent one is kept; a clipboard has room for one thing.
     */
    if (!w->focused) {
        free(w->clip_pending);
        w->clip_pending = copy;
        w->clip_pending_len = len;
        return 1;               /* HELD -- see the ops contract */
    }

    if (w->source) {
        wl_data_source_destroy(w->source);
        w->source = NULL;
    }
    free(w->src_text);
    w->src_text = copy;
    w->src_len = len;

    w->source = wl_data_device_manager_create_data_source(w->ddm);
    if (!w->source) {
        return -EIO;
    }
    wl_data_source_add_listener(w->source, &dsrc_listener, w);
    wl_data_source_offer(w->source, NB_CLIP_MIME);
    wl_data_source_offer(w->source, NB_WL_OWN_MIME);
    /* The serial must be one from a real input event; the compositor rejects
     * a fabricated one, which is the protocol enforcing "only a focused client
     * that has seen input may take the selection". */
    wl_data_device_set_selection(w->ddev, w->source, w->last_serial);
    wl_display_flush(w->dpy);
    return 0;
}

/* ── host -> guest: read the selection, asynchronously ───────────────────── */

static int wl_fetch_clipboard(struct nb_session *s, struct nb_sink *sink,
                              uint64_t generation)
{
    struct nb_wl *w = s->priv;
    int fds[2];

    if (w->fetch_fd >= 0) {
        return -EBUSY;              /* one at a time */
    }
    if (!w->offer) {
        nb_sink_clip_skip(sink, "the host clipboard is empty (no selection "
                          "offered to this window yet)");
        return -ENOENT;
    }
    if (!w->offer_text) {
        nb_sink_clip_skip(sink, "the host clipboard holds no text");
        return -ENOENT;
    }
    if (w->offer_own) {
        /* The selection is OURS -- the guest's own text.  Reading it back
         * through the compositor would only echo it. */
        nb_sink_clip_skip(sink, "the host clipboard is the VM's own copy");
        return -ENOENT;
    }
    w->fetch_cap = nb_sink_clip_cap(sink);
    free(w->fetch_buf);
    w->fetch_buf = malloc(w->fetch_cap + 1u);
    if (!w->fetch_buf) {
        w->fetch_cap = 0;
        return -ENOMEM;
    }
    if (pipe2(fds, O_CLOEXEC | O_NONBLOCK) < 0) {
        return -errno;
    }
    wl_data_offer_receive(w->offer, nb_wl_text_mime(w->offer_text), fds[1]);
    close(fds[1]);                  /* the compositor owns its end now */
    wl_display_flush(w->dpy);
    w->fetch_fd = fds[0];
    w->fetch_len = 0;
    w->fetch_generation = generation;
    w->fetch_deadline_ms = nb_now_ms_wl() + NB_CLIP_FETCH_TIMEOUT_MS;
    return 0;
}

/*
 * Drain the selection pipe.  Returns true when the transfer is finished (for
 * any reason), so the caller can release the held paste keystroke exactly once.
 */
static bool wl_fetch_pump(struct nb_wl *w, struct nb_sink *sink)
{
    for (;;) {
        ssize_t n = read(w->fetch_fd, w->fetch_buf + w->fetch_len,
                         w->fetch_cap + 1u - w->fetch_len);

        if (n > 0) {
            w->fetch_len += (size_t)n;
            w->fetch_deadline_ms = nb_now_ms_wl() + NB_CLIP_FETCH_TIMEOUT_MS;
            if (w->fetch_len > w->fetch_cap) {
                nb_log("clipboard: host selection is larger than the %zu-byte "
                       "cap; not sending it", w->fetch_cap);
                nb_sink_clip_finish(sink, w->fetch_generation, false);
                break;
            }
            continue;
        }
        if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
            return false;           /* more later */
        }
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n < 0) {
            nb_log("clipboard: reading the host selection failed: %s",
                   strerror(errno));
        }
        /* n == 0: EOF, the whole selection is here. */
        if (n == 0 && w->fetch_len > 0) {
            bool sent = nb_sink_send_clipboard(sink, w->fetch_generation,
                                               w->fetch_buf, w->fetch_len);

            nb_sink_clip_finish(sink, w->fetch_generation, sent);
        } else {
            nb_sink_clip_finish(sink, w->fetch_generation, false);
        }
        break;
    }
    close(w->fetch_fd);
    w->fetch_fd = -1;
    w->fetch_len = 0;
    w->fetch_generation = 0;
    free(w->fetch_buf);
    w->fetch_buf = NULL;
    w->fetch_cap = 0;
    return true;
}

static void wl_client_detach(struct nb_session *s, uint64_t generation)
{
    struct nb_wl *w = s->priv;

    if (w->fetch_fd >= 0 && w->fetch_generation == generation) {
        close(w->fetch_fd);
        w->fetch_fd = -1;
        w->fetch_len = 0;
        w->fetch_generation = 0;
        free(w->fetch_buf);
        w->fetch_buf = NULL;
        w->fetch_cap = 0;
    }
    /*
     * DROP THE TEXT WE ARE HOLDING FOR A VM THAT HAS GONE (audit 2026-08-29).
     *
     * nb_client_state_reset() exists so a later VMM cannot inherit paste state,
     * and this copy is exactly that — a guest's clipboard content, accepted but
     * never applied because the window was not focused.  Kept here it survives
     * the disconnect, and the next focus-in puts a departed VM's text on the
     * user's clipboard, attributed by the notice to whatever VM is on screen
     * now.  Already-applied text (src_text, and the wl_data_source that serves
     * it) is NOT dropped: that is the user's clipboard now.
     */
    free(w->clip_pending);
    w->clip_pending = NULL;
    w->clip_pending_len = 0;
    /* The warning/title state described the departed VM, not the persistent
     * host window. */
    w->clip_notice_until = 0;
    /* virtio-nvgpu: everything below described the departed VM.  An import
     * still in flight for it must not be shown when it lands. */
    w->gc_gen++;
    w->gc_on = false;
    w->gc_broken = false;
    w->gc_cur = -1;
    w->gpos_valid = w->gpos_seen = false;
    for (int i = 0; i < 4; i++) {
        if (w->gc[i].buf) {
            wl_buffer_destroy(w->gc[i].buf);
        }
        memset(&w->gc[i], 0, sizeof(w->gc[i]));
    }
    cur_apply(w, w->last_serial);
    w->mh_valid = false;
    w->mh_due_ms = 0;
    w->stats_active = false;
    nb_fs_reset(&w->fs);
    memset(&w->snap, 0, sizeof(w->snap));
    w->snap.direct = -1;
    w->tb_stats[0] = '\0';
    wl_title_update(w, true);
    if (w->ov_mapped) {
        ov_paint_commit(w);
    }
    tb_update(w, w->tb_w > 0 ? w->tb_w : w->win_w);
}

/* ── the output's fractional scale ───────────────────────────────────────── */
/*
 * EV_SURFACE carries what the GUEST should render, which is physical pixels --
 * see the scale_120 comment.  Everything else in this file works in logical
 * pixels, so the conversion lives here and nowhere else.
 */
static void wl_report_surface(struct nb_wl *w, int lw, int lh)
{
    unsigned s = w->scale_120 ? w->scale_120 : 120;

    if (!w->sink || lw <= 0 || lh <= 0) {
        return;
    }
    /*
     * Sync the fullscreen flag HERE, on the packet the client actually keys
     * its mode switch off, rather than only on the transition.  Fullscreen can
     * be true before any transition is observed -- `--fullscreen` sets it at
     * startup, so top_configure's was_fs check never fires and the flag stayed
     * false forever, which silently disabled the whole ui_info path.  The
     * setter is idempotent, so calling it on every report costs a compare.
     */
    nb_sink_set_fullscreen(w->sink, w->fullscreen);

    /*
     * THE ONE PLACE A HINT IS EMITTED, and it is reached only from host events
     * -- a geometry change or a scale change.  Nothing a guest does gets here.
     */
    unsigned pw, ph;

    switch (nb_res_mode) {
    case NB_RES_NONE:
        return;                 /* suggest nothing at all */
    case NB_RES_FIXED:
        pw = nb_res_w;
        ph = nb_res_h;
        break;
    default:
        /* Native: the output area in physical pixels -- the window, unless
         * a smaller area was chosen, which the guest then fills 1:1. */
        nb_view_guest_mode(&w->vst.cur, lw, lh, s, &pw, &ph);
        break;
    }

    /*
     * ROUND THE WIDTH DOWN TO A MULTIPLE OF 8.
     *
     * The guest synthesises the mode with drm_cvt_mode(), which applies
     * CVT_H_GRANULARITY = 8, so a width that is not a multiple of 8 cannot be
     * honoured exactly.  Exact is what matters: a compositor promotes a surface
     * to a hardware plane only when it covers the output, so missing by four
     * pixels silently costs direct scanout.  Down, never up, so the suggestion
     * always fits the window we measured.
     */
    if (w->hint_align > 1) {
        pw -= pw % w->hint_align;
    }
    if (pw == 0 || ph == 0) {
        return;
    }

    /*
     * Idempotent.  A configure that did not change the content size, or a
     * fixed mode re-reporting the same numbers, is not news -- and a hint the
     * guest has already acted on is a re-mode it does not need.
     */
    if (pw == w->hint_w && ph == w->hint_h) {
        return;
    }
    w->hint_w = pw;
    w->hint_h = ph;
    /*
     * What we LEARNED is w->refresh_mhz; what we SAY is this.  Kept apart so
     * the log still shows the real host rate even when an operator has pinned
     * a different one.
     */
    {
        unsigned say = w->refresh_mhz;

        if (nb_hz_mode == NB_HZ_NONE) {
            say = 0;
        } else if (nb_hz_mode == NB_HZ_FIXED) {
            say = nb_hz_fixed;
        }
        nb_sink_surface(w->sink, pw, ph, say);
    }
}


static void frac_scale(void *d, struct wp_fractional_scale_v1 *f,
                       uint32_t scale)
{
    struct nb_wl *w = d;

    (void)f;
    if (scale == 0 || scale == w->scale_120) {
        return;
    }
    w->scale_120 = scale;
    nb_log("output scale is %u.%03u — the guest is told PHYSICAL pixels, so a "
           "fullscreen guest buffer can cover the output",
           scale / 120, (scale % 120) * 1000 / 120);
    /* Re-report at the new scale: the size in logical pixels has not changed,
     * but what the guest should render has.  cont_*, not surf_* -- see the
     * field's comment for why reporting surf_* oscillates. */
    wl_report_surface(w, w->cont_w, w->cont_h);
    wl_mode_hint(w, true);
}
static const struct wp_fractional_scale_v1_listener frac_listener = {
    .preferred_scale = frac_scale,
};

/*
 * The host cursor over the CONTENT surface, hidden or shown by what is
 * actually on it.  Called on pointer entry and again whenever the content
 * changes underneath a pointer that is already inside -- the placeholder
 * appearing or the guest's first frame arriving sends no enter event, so
 * without this the cursor keeps whatever state it had.
 */
/*
 * wl_pointer.set_cursor, then the cursor surface committed again with its
 * buffer and full damage.  A compositor may take a cursor's pixels only from
 * a commit made while the surface HAS the cursor role -- Hyprland copies them
 * then (Compositor.cpp updateCursorShm) and on NVIDIA draws the cursor from
 * that copy -- so an image committed before its first set_cursor is shown
 * transparent there.  The extra commit of an unchanged shm buffer is harmless
 * everywhere else.
 */
static void ptr_cursor(struct nb_wl *w, struct wl_pointer *p, uint32_t serial,
                       struct wl_surface *sf, struct wl_buffer *buf,
                       int32_t hx, int32_t hy)
{
    /*
     * The serial set_cursor takes is the pointer ENTER's.  `last_serial` may
     * be a key's (CTRL+ALT+G itself), which a compositor is entitled to
     * refuse -- weston does, and the pointer kept its image under grab.
     */
    wl_pointer_set_cursor(p, w->ptr_serial ? w->ptr_serial : serial, sf, hx,
                          hy);
    if (sf && buf) {
        wl_surface_attach(sf, buf, 0, 0);
        wl_surface_damage_buffer(sf, 0, 0, INT32_MAX, INT32_MAX);
        wl_surface_commit(sf);
    }
}

/* The pointer (window coordinates) is over the drawn guest picture, not
 * over the bars around it. */
static bool wl_ptr_on_pic(const struct nb_wl *w)
{
    const struct nb_rect *v = &w->place.vis;

    return w->current >= 0 && w->ptr_wx >= v->x && w->ptr_wx < v->x + v->w &&
           w->ptr_wy >= v->y && w->ptr_wy < v->y + v->h;
}

static void cur_apply(struct nb_wl *w, uint32_t serial)
{
    /* Whatever changed may change the cursor drawn under grab too. */
    spr_update(w);
    if (!w->ptr || !w->ptr_on_content) {
        return;
    }
    w->ptr_on_pic = wl_ptr_on_pic(w);
    cur_build(w);
    if (w->ui.open && !w->dlg_open) {
        /* The menu is open: the area editor's handles. */
        static const int map[NB_UI_CUR_COUNT] = {
            [NB_UI_CUR_ARROW] = NB_CUR_ARROW, [NB_UI_CUR_MOVE] = NB_CUR_ARROW,
            [NB_UI_CUR_N] = NB_CUR_NS, [NB_UI_CUR_S] = NB_CUR_NS,
            [NB_UI_CUR_W] = NB_CUR_EW, [NB_UI_CUR_E] = NB_CUR_EW,
            [NB_UI_CUR_NW] = NB_CUR_NWSE, [NB_UI_CUR_SE] = NB_CUR_NWSE,
            [NB_UI_CUR_NE] = NB_CUR_NESW, [NB_UI_CUR_SW] = NB_CUR_NESW,
            [NB_UI_CUR_TEXT] = NB_CUR_ARROW,
        };
        int c = nb_ui_area_cursor(&w->ui);
        int k = c >= 0 && c < NB_UI_CUR_COUNT ? map[c] : NB_CUR_ARROW;

        w->ui_cur = c;
        ptr_cursor(w, w->ptr, serial, w->cur_surf[k], w->cur_buf[k],
                   w->cur_hx[k], w->cur_hy[k]);
        w->cur_img_on = true;
        return;
    }
    if (w->dlg_open) {
        /*
         * THE DIALOG IS HOST UI AND ALWAYS HAS A POINTER, exactly like the
         * title bar.  It can be up WHILE a guest buffer is on the surface, so
         * it has to be tested before the guest-content case below -- otherwise
         * the user is asked a question with four buttons and given no cursor
         * to answer it with.
         */
        ptr_cursor(w, w->ptr, serial, w->cur_surf[NB_CUR_ARROW],
                   w->cur_buf[NB_CUR_ARROW], 0, 0);
        w->cur_img_on = true;
    } else if (w->current >= 0 && !w->ptr_on_pic && !w->grabbed) {
        /* The bars around the picture: nothing of the guest's is drawn
         * there, so the host pointer is. */
        ptr_cursor(w, w->ptr, serial, w->cur_surf[NB_CUR_ARROW],
                   w->cur_buf[NB_CUR_ARROW], 0, 0);
        w->cur_img_on = true;
    } else if (w->current >= 0) {
        /*
         * Guest content.  With a guest cursor plane (CMD_CURSOR) its image IS
         * the host pointer's, so it moves at host speed with no round trip --
         * except under grab, where the guest drives a relative pointer the
         * host cannot place, so it is hidden.  Without one the guest
         * composites its own cursor into the frame, and ours must go.
         */
        /*
         * NULL, not a transparent image: with no cursor image a compositor
         * has no cursor plane or software cursor to draw over the picture,
         * which is one of the conditions for scanning it out directly
         * (wlroots' wlr_output_is_direct_scanout_allowed() refuses while a
         * software cursor is visible).  Under grab it is always NULL.
         */
        if (w->gc_on && !w->grabbed && w->gc_broken) {
            /* The guest shows a cursor this viewer could not copy: ours,
             * rather than none at all or the guest's previous shape. */
            ptr_cursor(w, w->ptr, serial, w->cur_surf[NB_CUR_ARROW],
                       w->cur_buf[NB_CUR_ARROW], 0, 0);
            w->cur_img_on = true;
        } else if (w->gc_on && !w->grabbed && w->gc_cur >= 0 && w->gc_surf) {
            ptr_cursor(w, w->ptr, serial, w->gc_surf, w->gc[w->gc_cur].buf,
                       w->gc_lhx, w->gc_lhy);
            w->cur_img_on = true;
        } else {
            wl_pointer_set_cursor(w->ptr, w->ptr_serial ? w->ptr_serial
                                                        : serial, NULL, 0, 0);
            w->cur_img_on = false;
        }
    } else {
        /* The placeholder.  Nothing is drawing a cursor into it. */
        ptr_cursor(w, w->ptr, serial, w->cur_surf[NB_CUR_ARROW],
                   w->cur_buf[NB_CUR_ARROW], 0, 0);
        w->cur_img_on = true;
    }
}

/* ── the letterbox background ────────────────────────────────────────────── */
/* Unused since the letterbox moved onto the main surface (see cs_surf);
 * kept for the upstream merge. */
__attribute__((unused)) static void bg_build(struct nb_wl *w)
{
    struct wl_shm_pool *pool;
    uint32_t *px;
    int fd;

    if (w->bg_surf || !w->shm || !w->subcomp || !w->comp || !w->viewporter) {
        return;
    }
    fd = memfd_create("conduit-viewer-bg", MFD_CLOEXEC);
    if (fd < 0) {
        return;
    }
    if (ftruncate(fd, 4) < 0) {
        close(fd);
        return;
    }
    px = mmap(NULL, 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (px == MAP_FAILED) {
        close(fd);
        return;
    }
    *px = 0xff000000u;          /* opaque black: the conventional bar colour */
    w->bg_px = px;
    pool = wl_shm_create_pool(w->shm, fd, 4);
    if (pool) {
        w->bg_buf = wl_shm_pool_create_buffer(pool, 0, 1, 1, 4,
                                              WL_SHM_FORMAT_ARGB8888);
        wl_shm_pool_destroy(pool);
    }
    close(fd);
    if (!w->bg_buf) {
        return;
    }
    w->bg_surf = wl_compositor_create_surface(w->comp);
    if (!w->bg_surf) {
        return;
    }
    w->bg_sub = wl_subcompositor_get_subsurface(w->subcomp, w->bg_surf,
                                                w->surf);
    if (!w->bg_sub) {
        return;
    }
    /* No restacking request (see SUBSURFACE STACKING). */
    wl_subsurface_set_desync(w->bg_sub);
    w->bg_vp = wp_viewporter_get_viewport(w->viewporter, w->bg_surf);
}

/*
 * Show or hide the bars.  `dw`/`dh` is the content rectangle just computed;
 * anything the window has left over is bar.  Called from the same place the
 * viewport destination is set, so the two can never disagree.
 */
static void bg_layout(struct nb_wl *w, int dw, int dh)
{
    bool want = w->win_w > dw || w->win_h > dh;

    w->off_x = want && w->win_w > dw ? (w->win_w - dw) / 2 : 0;
    w->off_y = want && w->win_h > dh ? (w->win_h - dh) / 2 : 0;

    /* virtio-nvgpu: the bars are the main surface itself now (see cs_surf);
     * the old below-the-parent bar surface is never mapped -- it hung OUTSIDE
     * the main surface, which is exactly what Hyprland cannot render. */
    want = false;
    /* No legacy bar surface: nothing to map, but the picture's offset above
     * still applies -- zeroing it here pinned letterboxed frames to the
     * top-left of the window. */
    if (!w->bg_surf || !w->bg_vp) {
        return;
    }
    if (!want) {
        if (w->bg_mapped) {
            wl_surface_attach(w->bg_surf, NULL, 0, 0);
            wl_surface_commit(w->bg_surf);
            w->bg_mapped = false;
        }
        return;
    }
    /* The background sits at the window origin; the content (the parent
     * surface) is inset by off_*, so the background starts that far up-left
     * of it. */
    wl_subsurface_set_position(w->bg_sub, -w->off_x, -w->off_y);
    wp_viewport_set_destination(w->bg_vp, w->win_w, w->win_h);
    wl_surface_attach(w->bg_surf, w->bg_buf, 0, 0);
    wl_surface_damage_buffer(w->bg_surf, 0, 0, 1, 1);
    wl_surface_commit(w->bg_surf);
    w->bg_mapped = true;
}

/* ── the close confirmation ──────────────────────────────────────────────── */
/*
 * WHY THIS IS IN THE PRIVILEGED PROCESS AT ALL, stated plainly because the
 * original design said it should not be.
 *
 * The objection to a dialog in the broker was that the broker holds the
 * keyboard grab and knows nothing about VMs, so it must not DECIDE what
 * closing a window does to one.  It still does not.  Each choice below either
 * selects which fixed-size message to send -- QEMU decides what a powerdown or
 * a force-off means -- or does something purely local to the broker's own
 * window.  What is new is drawing and hit-testing, and the title bar's three
 * buttons already do both, so this is not a new class of code in here.
 *
 * The alternative, a helper process spawned to draw it, buys isolation for the
 * drawing but costs a process, an IPC and a way for the dialog to disagree
 * with the window it belongs to.  Not worth it for four rectangles.
 */
static int dlg_hit(const struct nb_wl *w, int x, int y)
{
    int i;

    if (x < NB_DLG_PAD || x >= NB_DLG_W - NB_DLG_PAD) {
        return -1;
    }
    for (i = 0; i < NB_DLG_N; i++) {
        int top = NB_DLG_PAD + NB_DLG_TITLE + NB_DLG_GAP +
                  i * (NB_DLG_BTN_H + NB_DLG_GAP);

        if (y >= top && y < top + NB_DLG_BTN_H) {
            return i;
        }
    }
    return -1;
}

static void dlg_paint(struct nb_wl *w)
{
    uint32_t *px = w->dlg_px;
    unsigned stride_px = NB_DLG_W;
    int i;

    if (!px) {
        return;
    }
    nb_placeholder_fill(px, NB_DLG_W, NB_DLG_H, stride_px, 0xff1c1c22u);
    /* A border, so it reads as a thing on top rather than a hole in the guest. */
    for (i = 0; i < NB_DLG_W; i++) {
        px[i] = 0xff5a5a6eu;
        px[(NB_DLG_H - 1) * stride_px + i] = 0xff5a5a6eu;
    }
    for (i = 0; i < NB_DLG_H; i++) {
        px[i * stride_px] = 0xff5a5a6eu;
        px[i * stride_px + NB_DLG_W - 1] = 0xff5a5a6eu;
    }
    nb_placeholder_text(px, NB_DLG_W, NB_DLG_H, stride_px, NB_DLG_PAD,
                        NB_DLG_PAD,
                        w->close_asked ? "STILL RUNNING - ASK AGAIN?"
                                       : "CLOSE THE DISPLAY?",
                        2, w->close_asked ? 0xffffd080u : 0xffffffffu);

    for (i = 0; i < NB_DLG_N; i++) {
        int top = NB_DLG_PAD + NB_DLG_TITLE + NB_DLG_GAP +
                  i * (NB_DLG_BTN_H + NB_DLG_GAP);
        int bw = NB_DLG_W - 2 * NB_DLG_PAD;
        uint32_t bg;
        unsigned tw;
        int x, y;

        bg = (i == w->dlg_press) ? 0xff4a4a64u
           : (i == w->dlg_hover) ? 0xff3c3c50u
                                 : 0xff2e2e3au;
        for (y = top; y < top + NB_DLG_BTN_H; y++) {
            for (x = NB_DLG_PAD; x < NB_DLG_PAD + bw; x++) {
                px[y * (int)stride_px + x] = bg;
            }
        }
        tw = nb_placeholder_text_w(nb_dlg_label[i], 2);
        nb_placeholder_text(px, NB_DLG_W, NB_DLG_H, stride_px,
                            (unsigned)(NB_DLG_PAD + (bw - (int)tw) / 2),
                            (unsigned)(top + (NB_DLG_BTN_H - 14) / 2),
                            nb_dlg_label[i], 2,
                            i == 1 ? 0xffff9090u : 0xffe8e8f0u);
    }
}

static void dlg_hide(struct nb_wl *w)
{
    if (!w->dlg_open) {
        return;
    }
    w->dlg_open = false;
    w->dlg_hover = w->dlg_press = -1;
    w->ptr_on_dlg = false;
    cur_apply(w, w->last_serial);   /* back to whatever the content wants */
    if (w->dlg_surf) {
        wl_surface_attach(w->dlg_surf, NULL, 0, 0);
        wl_surface_commit(w->dlg_surf);
        wl_surf_commit(w, w->surf);
        wl_display_flush(w->dpy);
    }
    nb_log("close: dismissed");
}

static void dlg_commit(struct nb_wl *w)
{
    if (!w->dlg_surf || !w->dlg_buf) {
        return;
    }
    dlg_paint(w);
    /* Centre it on the window as the compositor last configured it. */
    if (w->dlg_sub) {
        int cw = w->win_w > 0 ? w->win_w : NB_DLG_W;
        int ch = w->win_h > 0 ? w->win_h : NB_DLG_H;

        wl_subsurface_set_position(w->dlg_sub, (cw - NB_DLG_W) / 2,
                                   (ch - NB_DLG_H) / 2);
    }
    wl_surface_attach(w->dlg_surf, w->dlg_buf, 0, 0);
    wl_surface_damage_buffer(w->dlg_surf, 0, 0, NB_DLG_W, NB_DLG_H);
    wl_surface_commit(w->dlg_surf);
    wl_surf_commit(w, w->surf);
    wl_display_flush(w->dpy);
}

static void dlg_show(struct nb_wl *w)
{
    /* virtio-nvgpu: closing the window closes the viewer only. The VM keeps
     * running and a viewer started again reattaches; no dialog. */
    if (!getenv("NVGPU_VIEWER_CLOSE_DIALOG")) {
        nb_log("close: closing the display only; the VM keeps running");
        w->quit = true;
        return;
    }
    struct wl_shm_pool *pool;
    size_t stride, sz;
    int fd;

    if (w->dlg_open) {
        return;
    }
    /*
     * NOTHING TO ASK ABOUT WITH NO VM.  Over the placeholder there is no guest
     * to shut down and no client to defer the policy to, so the four-way
     * question is meaningless -- X just closes the display, which is what it
     * has always meant when nobody is connected.
     */
    if (!nb_sink_has_client(w->sink)) {
        nb_log("close: no VM is connected, so there is nothing to ask about");
        w->quit = true;
        return;
    }
    if (!w->shm || !w->subcomp || !w->comp) {
        w->quit = true;         /* no way to ask; the old behaviour */
        return;
    }
    /*
     * DROP THE GRAB BEFORE SHOWING HOST UI.  A dialog the user cannot click
     * because the pointer is locked to the guest, in front of a guest that is
     * swallowing the keyboard, is exactly the state a grab is hardest to get
     * out of.  Done before the dialog maps, not after.
     */
    nb_sink_force_ungrab(w->sink);
    if (!w->dlg_surf) {
        w->dlg_surf = wl_compositor_create_surface(w->comp);
        if (!w->dlg_surf) {
            w->quit = true;
            return;
        }
        w->dlg_sub = wl_subcompositor_get_subsurface(w->subcomp, w->dlg_surf,
                                                     w->surf);
        if (!w->dlg_sub) {
            w->quit = true;
            return;
        }
        /* Made last, so it is on top (see SUBSURFACE STACKING). */
        wl_subsurface_set_desync(w->dlg_sub);
    }
    if (!w->dlg_buf) {
        stride = (size_t)NB_DLG_W * 4;
        sz = stride * NB_DLG_H;
        fd = memfd_create("conduit-viewer-close", MFD_CLOEXEC);
        if (fd < 0) {
            w->quit = true;
            return;
        }
        if (ftruncate(fd, (off_t)sz) < 0) {
            close(fd);
            w->quit = true;
            return;
        }
        w->dlg_px = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (w->dlg_px == MAP_FAILED) {
            w->dlg_px = NULL;
            close(fd);
            w->quit = true;
            return;
        }
        w->dlg_sz = sz;
        pool = wl_shm_create_pool(w->shm, fd, (int32_t)sz);
        if (pool) {
            w->dlg_buf = wl_shm_pool_create_buffer(pool, 0, NB_DLG_W,
                                                   NB_DLG_H, (int32_t)stride,
                                                   WL_SHM_FORMAT_ARGB8888);
            wl_shm_pool_destroy(pool);
        }
        close(fd);
        if (!w->dlg_buf) {
            w->quit = true;
            return;
        }
    }
    w->dlg_open = true;
    w->dlg_hover = w->dlg_press = -1;
    cur_apply(w, w->last_serial);   /* give the pointer back over the content */
    nb_log("close: asking what to do (ACPI / force off / display only / cancel)");
    dlg_commit(w);
}

/* One of the four choices was taken. */
static void dlg_choose(struct nb_wl *w, int choice)
{
    switch (choice) {
    case 0:
        nb_log("close: ACPI powerdown requested%s",
               w->close_asked ? " (again; the guest has not acted on the last "
                                "one -- FORCE OFF is the way out)" : "");
        if (!nb_sink_close_request(w->sink, NVKVM_BROKER_CLOSE_POWERDOWN)) {
            w->quit = true;
        } else {
            w->close_asked = true;
        }
        break;
    case 1:
        nb_log("close: force off requested");
        if (!nb_sink_close_request(w->sink, NVKVM_BROKER_CLOSE_FORCE)) {
            w->quit = true;
        }
        break;
    case 2:
        /* Purely local: the VM is not told and keeps running.  The socket
         * closes, and a broker started again on the same path reattaches. */
        nb_log("close: closing the display only; the VM keeps running");
        w->quit = true;
        break;
    default:
        break;
    }
    dlg_hide(w);
}

/* ── the idle placeholder ────────────────────────────────────────────────── */
/*
 * The surface has no content until the client's first ATTACH+COMMIT, so an
 * idle broker is an unmapped window: it sits in the dock and shows nothing
 * when clicked.  "Waiting for a VM" and "crashed" then look identical, which
 * is the first thing a user meets.  We paint our own frame instead.
 *
 * shm, not dma-buf, on purpose: this is the one thing the broker draws itself,
 * it must work before any GPU buffer exists, and wl_shm is the only allocator
 * every compositor is required to have.  It costs one page-aligned mapping
 * that is freed the moment a guest frame replaces it.
 */
static void wl_idle_drop(struct nb_wl *w)
{
    if (w->idle_buf) {
        wl_buffer_destroy(w->idle_buf);
        w->idle_buf = NULL;
    }
    if (w->idle_px) {
        munmap(w->idle_px, w->idle_sz);
        w->idle_px = NULL;
        w->idle_sz = 0;
    }
    w->idle_w = w->idle_h = 0;
}

static int wl_idle_make(struct nb_wl *w, int wd, int ht)
{
    struct wl_shm_pool *pool;
    size_t stride = (size_t)wd * 4;
    size_t sz = stride * (size_t)ht;
    void *px;
    int fd;

    if (!w->shm || wd <= 0 || ht <= 0) {
        return -ENOTSUP;
    }
    fd = memfd_create("conduit-viewer-idle", MFD_CLOEXEC);
    if (fd < 0) {
        return -errno;
    }
    if (ftruncate(fd, (off_t)sz) < 0) {
        close(fd);
        return -errno;
    }
    px = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (px == MAP_FAILED) {
        close(fd);
        return -errno;
    }
    pool = wl_shm_create_pool(w->shm, fd, (int32_t)sz);
    close(fd);
    if (!pool) {
        munmap(px, sz);
        return -EIO;
    }
    /*
     * Destroy the previous buffer only AFTER the new one exists, and rely on
     * request ordering for the swap: the compositor processes attach+commit
     * before it sees the destroy, so the old buffer is never yanked out from
     * under a frame it is still scanning out.
     */
    wl_idle_drop(w);
    w->idle_buf = wl_shm_pool_create_buffer(pool, 0, wd, ht, (int32_t)stride,
                                            WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);
    if (!w->idle_buf) {
        munmap(px, sz);
        return -EIO;
    }
    w->idle_px = px;
    w->idle_sz = sz;
    w->idle_w  = wd;
    w->idle_h  = ht;
    if (w->client_attached) {
        nb_placeholder_paint(px, (unsigned)wd, (unsigned)ht, (unsigned)wd,
                             "VM ATTACHED - NO PICTURE YET",
                             "THE GUEST HAS NOT PRESENTED A FRAME");
    } else {
        nb_placeholder_paint(px, (unsigned)wd, (unsigned)ht, (unsigned)wd,
                             "CONDUIT", "WAITING FOR THE VM");
    }
    return 0;
}

/* ops->dismiss_dialog: the broker took a state that contradicts the dialog. */
/*
 * ops->notify_clipboard: the guest just replaced the host clipboard.
 *
 * VISIBILITY IS THE CONTROL for this direction.  A guest that can silently
 * swap what you are about to paste into a host terminal is the actual attack;
 * a log line is good for forensics but nobody is reading stderr, so it also
 * goes where the user is already looking.
 */
static void wl_notify_clipboard(struct nb_session *s)
{
    struct nb_wl *w = s->priv;

    w->clip_notice_until = nb_now_ms_wl() + 4000u;
    tb_update(w, w->tb_w > 0 ? w->tb_w : w->win_w);
    wl_display_flush(w->dpy);
}

/*
 * Clock-driven work.  Only the title-bar notice qualifies today: it expires on
 * a deadline, not on an event, and the main loop otherwise sleeps in poll()
 * forever -- which is why a "transient" notice used to sit there until an
 * unrelated event happened to redraw the bar.
 */
/* Same contract as the X11 backend: a client that attaches after the edges
 * happened is told the current state rather than left guessing. */
static int wl_show_idle(struct nb_session *s);

static void wl_resync(struct nb_session *s)
{
    struct nb_wl *w = s->priv;

    /* Same trap as the X11 backend: resync runs at ATTACH, which can be before
     * the first dispatch that assigns w->sink, and then this returns having
     * re-emitted nothing.  Take it from the session. */
    if (!w->sink) {
        w->sink = s->sink;
    }
    if (!w->sink) {
        return;
    }
    nb_sink_focus(w->sink, w->focused);
    wl_clip_flush_pending(w);
    /*
     * The client missed the window's size too: the handshake carried whatever
     * the session last recorded, which before any hint is the --size we
     * started with.  Re-report what the window really is, and the mode we
     * want -- a client that connects while we are fullscreen must still be
     * asked for the output's mode.
     */
    if (w->cont_w > 0 && w->cont_h > 0) {
        w->hint_w = w->hint_h = 0;
        wl_report_surface(w, w->cont_w, w->cont_h);
    }
    w->mh_valid = false;
    wl_mode_hint(w, true);
    w->stats_active = true;
    w->fs_next_ms = 0;
    /*
     * Name the state the user is actually looking at.  A guest that is attached
     * but has never presented is not "waiting for a VM" -- the VM is right
     * there and silent, which is a different thing to go and debug.
     */
    w->client_attached = true;
    if (w->idle_wanted || w->idle_shown) {
        wl_show_idle(s);
    }
}

/* How long to wait before pacing the VMM unprompted. */
#define NB_WL_PACE_MS 100

static int wl_tick_clip(struct nb_session *s);

static int wl_tick_viewer(struct nb_wl *w, int next);

static int wl_tick_fetch(struct nb_wl *w, int next);

static int wl_tick(struct nb_session *s)
{
    struct nb_wl *w = s->priv;
    int r = wl_tick_fetch(w, wl_tick_clip(s));

    r = wl_tick_viewer(w, r);
    /*
     * Release points without an eventfd to watch them (a kernel before
     * DRM_IOCTL_SYNCOBJ_EVENTFD): looked at every millisecond while one is
     * outstanding.
     */
    if (wl_xs_waiting(w) && nb_xs_eventfd(w->xs) < 0) {
        wl_xs_scan(w);
        if (wl_xs_waiting(w) && (r < 0 || r > 1)) {
            r = 1;
        }
    }
    /* A staged frame held by an overdue frame callback: look again then. */
    wl_commit_staged(w);
    if ((w->commit_staged && w->pending >= 0) || w->nudge_pending) {
        uint64_t now = nb_mono_ns();
        uint64_t due = w->commit_staged && w->pending >= 0
                           ? nb_pace_due_ns(&w->pace, now)
                           : nb_nudge_due_ns(&w->pace, now, wl_nudge_min_ns(w));

        if (due) {
            int ms = (int)((due + 999999) / 1000000);

            if (r < 0 || ms < r) {
                r = ms;
            }
        }
    }
    if (w->commit_staged && w->pending >= 0 && wl_latch_on(w)) {
        /* The timerfd is polled only from the next pollfds(): this iteration
         * gets a millisecond timeout as a backstop. */
        uint64_t due = nb_latch_due_ns(&w->latch, nb_mono_ns());
        int ms = (int)((due + 999999) / 1000000);

        wl_latch_arm(w);
        if (due && (r < 0 || ms < r)) {   /* due 0: a full socket; POLLOUT wakes us */
            r = ms;
        }
    }
    return r;
}

static int wl_tick_clip(struct nb_session *s)
{
    struct nb_wl *w = s->priv;
    uint64_t now;

    /*
     * THE SAME PACING WATCHDOG THE X11 BACKEND CARRIES, for the same reason:
     * here FRAME comes from a wl_surface frame callback, and that callback is
     * armed inside wl_commit().  No commit, no callback, no FRAME, no commit --
     * a closed loop that cannot restart itself once a token is lost, and a
     * guest rebooting from inside is enough to lose one (the VMM's socket is
     * per-QEMU, so it never re-handshakes and is never re-primed).
     *
     * A compositor's vblank fires whether or not anything was drawn; match it.
     * Slow on purpose -- in the steady state the frame callbacks pace at the
     * compositor's own rate and this must not add frames on top of them.
     */
    if (w->sink) {
        now = nb_now_ms_wl();
        if (now - w->last_frame_ms >= NB_WL_PACE_MS) {
            w->last_frame_ms = now;
            nb_sink_frame(w->sink);
        }
        if (!w->clip_notice_until) {
            return NB_WL_PACE_MS;
        }
    }

    if (!w->clip_notice_until) {
        return -1;
    }
    now = nb_now_ms_wl();
    if (now >= w->clip_notice_until) {
        w->clip_notice_until = 0;
        tb_update(w, w->tb_w > 0 ? w->tb_w : w->win_w);
        wl_display_flush(w->dpy);
        return -1;
    }
    return (int)(w->clip_notice_until - now);
}

static void wl_dismiss_dialog(struct nb_session *s)
{
    struct nb_wl *w = s->priv;

    /* Also clears the "you already asked" memory: it is per-connection, and
     * the two callers are a client going away and the user choosing a
     * contradictory mode -- after either, the next close starts fresh. */
    w->close_asked = false;
    dlg_hide(w);
}

static int wl_show_idle(struct nb_session *s)
{
    struct nb_wl *w = s->priv;
    int wd = w->win_w > 0 ? w->win_w : (w->surf_w > 0 ? w->surf_w : 1280);
    int ht = w->win_h > 0 ? w->win_h : (w->surf_h > 0 ? w->surf_h : 800);
    int r;

    w->idle_wanted = true;
    if (!w->shm) {
        /* Said once: a compositor with no wl_shm is legal but startling. */
        if (!w->idle_shown) {
            w->idle_shown = true;
            nb_log("no wl_shm: the window stays blank until the VM's first "
                   "frame");
        }
        return -ENOTSUP;
    }
    if (!w->configured) {
        return 0;               /* xdg_configure will call us back */
    }
    if (!w->idle_buf || w->idle_w != wd || w->idle_h != ht) {
        r = wl_idle_make(w, wd, ht);
        if (r < 0) {
            nb_err("could not make the placeholder buffer: %s", strerror(-r));
            return r;
        }
    }
    wl_leave_sub(w);            /* the placeholder is the main surface */
    wl_attach_sf(w, 0, w->idle_buf, NB_XA_NONE, -1, false, 0);
    if (w->viewport) {
        wp_viewport_set_destination(w->viewport, wd, ht);
    }
    wl_opaque(w, w->surf, wd, ht);
    w->tgt_ow = w->tgt_oh = 0;
    /* The placeholder fills the window: no letterbox, so no stale offsets
     * from the last guest frame may survive into the window geometry. */
    bg_layout(w, w->win_w > 0 ? w->win_w : wd, w->win_h > 0 ? w->win_h : ht);
    wl_geom_apply(w);
    wl_surface_damage_buffer(w->surf, 0, 0, wd, ht);
    wl_surf_commit(w, w->surf);
    wl_display_flush(w->dpy);
    w->current = -1;
    cur_apply(w, w->last_serial);   /* placeholder now: give the cursor back */
    if (!w->idle_shown) {
        nb_log("no client yet: showing the placeholder (%dx%d)", wd, ht);
        w->idle_shown = true;
    }
    return 0;
}

static int wl_resize(struct nb_session *s, unsigned wd, unsigned ht)
{
    struct nb_wl *w = s->priv;

    /*
     * On Wayland a surface's size IS its buffer's size, so there is nothing to
     * request: the next ATTACH at the new geometry resizes the window, and
     * EV_SURFACE then reports what actually happened.  Recorded so the window
     * has a sensible size before the first frame arrives.
     */
    w->surf_w = (int)wd;
    w->surf_h = (int)ht;
    return 0;
}

/* ── seat ────────────────────────────────────────────────────────────────── */

static void kbd_keymap(void *d, struct wl_keyboard *k, uint32_t f, int fd,
                       uint32_t sz)
{
    (void)d; (void)k; (void)f; (void)sz;
    /* We forward evdev keycodes, not symbols, so the keymap is the guest's
     * business.  Closing it is required by the protocol. */
    close(fd);
}
static void kbd_enter(void *d, struct wl_keyboard *k, uint32_t serial,
                      struct wl_surface *s, struct wl_array *keys)
{
    struct nb_wl *w = d;
    (void)k; (void)s; (void)keys;
    /*
     * KEYBOARD SERIALS COUNT TOO.  wl_data_device.set_selection needs a serial
     * from a real input event, and only ptr_enter/ptr_button used to record
     * one -- so a user working from the keyboard claimed the selection with
     * whatever stale pointer serial was lying around, and the compositor
     * silently ignored it.  That is why a guest copy "sometimes" did not
     * reach the host clipboard, and why it failed most reliably right after a
     * paste: Ctrl+V and Ctrl+C are both keyboard events, so neither refreshed
     * the serial, and the one still on file had already been spent.
     */
    w->last_serial = serial;
    w->focused = true;
    if (w->sink) {
        /* Focus-in fetches the host clipboard (nb_sink_focus); also when the
         * core already believed it was focused, so no keyboard enter is ever
         * a missed push. Repeats are dropped by content. */
        bool was = nb_sink_focused(w->sink);

        nb_sink_focus(w->sink, true);
        if (was) {
            nb_sink_host_clipboard_changed(w->sink);
        }
    }
    wl_clip_flush_pending(w);
    spr_update(w);              /* the drawn cursor, back under a grab */
    spr_kick_now(w);
}
static void kbd_leave(void *d, struct wl_keyboard *k, uint32_t serial,
                      struct wl_surface *s)
{
    struct nb_wl *w = d;
    (void)k; (void)serial; (void)s;
    w->focused = false;
    /* The property that stops the grab being a keylogger. */
    if (w->sink) {
        nb_sink_focus(w->sink, false);
    }
    /* Unfocused, the guest gets no input: no guest cursor drawn either. */
    spr_update(w);
    spr_kick_now(w);
}
static void kbd_key(void *d, struct wl_keyboard *k, uint32_t serial,
                    uint32_t time, uint32_t key, uint32_t state)
{
    struct nb_wl *w = d;
    (void)k; (void)time;

    w->last_serial = serial;    /* see kbd_enter: set_selection needs a fresh one */

    /*
     * MODAL.  While the confirmation is up, no key reaches the guest --
     * including the hotkeys, which nb_sink_key() would otherwise interpret.
     * A dialog the guest can type through is not a dialog.
     */
    if (w->dlg_open) {
        if (state != WL_KEYBOARD_KEY_STATE_PRESSED) {
            return;
        }
        switch (key) {
        case KEY_ESC:
            dlg_hide(w);
            break;
        case KEY_ENTER:
        case KEY_KPENTER:
            dlg_choose(w, 0);   /* the graceful one is the default */
            break;
        default:
            break;
        }
        return;
    }
    /*
     * THE MENU HAS THE KEYBOARD while it is open: nothing reaches the guest.
     * The sink still tracks the modifiers, so CTRL+ALT+M closes it and the
     * chords work again the moment it is gone.
     */
    if (w->ui.open && w->sink) {
        bool down = state == WL_KEYBOARD_KEY_STATE_PRESSED;

        nb_sink_key_latch(w->sink, key, down);
        if (down && w->sink->ctrl_down && w->sink->alt_down &&
            (key == KEY_M || key == KEY_F || key == KEY_O)) {
            if (key == KEY_M) {
                wl_menu_set(w, false);
            } else if (key == KEY_F) {
                nb_sink_toggle_fullscreen(w->sink);
            } else {
                wl_ui_do(w, NB_UI_STATS);
            }
            return;
        }
        if (key != KEY_LEFTCTRL && key != KEY_RIGHTCTRL &&
            key != KEY_LEFTALT && key != KEY_RIGHTALT) {
            wl_ui_do(w, nb_ui_key(&w->ui, wl_ui_env(w), key,
                                  w->sink->shift_down, down));
        }
        return;
    }
    /* wl_keyboard.key carries the evdev code directly — no translation. */
    if (w->sink) {
        nb_sink_key(w->sink, key, state == WL_KEYBOARD_KEY_STATE_PRESSED);
    }
}
static void kbd_mods(void *d, struct wl_keyboard *k, uint32_t serial,
                     uint32_t dep, uint32_t lat, uint32_t lock, uint32_t grp)
{
    (void)d; (void)k; (void)serial; (void)dep; (void)lat; (void)lock; (void)grp;
    /* Ignored on purpose: the sink derives modifier state from the same key
     * stream the client sees, so the two cannot disagree. */
}
static void kbd_repeat(void *d, struct wl_keyboard *k, int32_t r, int32_t dly)
{
    (void)d; (void)k; (void)r; (void)dly;
}
static const struct wl_keyboard_listener kbd_listener = {
    .keymap = kbd_keymap, .enter = kbd_enter, .leave = kbd_leave,
    .key = kbd_key, .modifiers = kbd_mods, .repeat_info = kbd_repeat,
};

static void ptr_enter(void *d, struct wl_pointer *p, uint32_t serial,
                      struct wl_surface *s, wl_fixed_t x, wl_fixed_t y)
{
    struct nb_wl *w = d;

    w->last_serial = serial;
    w->ptr_serial = serial;
    w->ptr_surf = s;
    /*
     * The title bar is a surface of ours too, and the pointer entering IT is
     * not the pointer entering the guest.  Leave the host cursor alone there
     * (the user is reaching for the close button and needs to see it) and
     * forward nothing to the guest.
     */
    cur_build(w);
    w->bd_hot = bd_index(w, s);
    if (w->bd_hot >= 0) {
        w->ptr_on_content = false;
        {
            const int c = nb_bd_cursor[w->bd_hot];

            ptr_cursor(w, p, serial, w->cur_surf[c], w->cur_buf[c],
                       w->cur_hx[c], w->cur_hy[c]);
        }
        return;
    }
    w->ptr_on_menu = s && w->ly_menu && s == w->ly_menu->surf;
    w->ptr_on_btn = s && w->ly_btn && s == w->ly_btn->surf;
    if (w->ptr_on_menu || w->ptr_on_btn) {
        /* Our own UI: an arrow, and nothing reaches the guest. */
        w->ptr_on_dlg = false;
        w->ptr_on_tb = false;
        w->ptr_on_content = false;
        w->ui_cur = -1;
        ptr_cursor(w, p, serial, w->cur_surf[NB_CUR_ARROW],
                   w->cur_buf[NB_CUR_ARROW], 0, 0);
        if (w->sink) {
            nb_sink_pointer(w->sink, false);
        }
        if (w->ptr_on_btn) {
            wl_ui_layer_motion(w, wl_fixed_to_double(x),
                               wl_fixed_to_double(y));
        }
        return;
    }
    if (s && s == w->dlg_surf) {
        w->ptr_on_dlg = true;
        w->ptr_on_tb = false;
        w->ptr_on_content = false;
        ptr_cursor(w, p, serial, w->cur_surf[NB_CUR_ARROW],
                   w->cur_buf[NB_CUR_ARROW], 0, 0);
        return;
    }
    w->ptr_on_dlg = false;
    if (s && s == w->tb_surf) {
        w->ptr_on_tb = true;
        w->ptr_on_content = false;
        ptr_cursor(w, p, serial, w->cur_surf[NB_CUR_ARROW],
                   w->cur_buf[NB_CUR_ARROW], 0, 0);
        return;
    }
    w->ptr_on_tb = false;
    /*
     * HIDE THE HOST CURSOR over the guest's picture -- but ONLY over the
     * guest's picture.  The guest composites its own cursor into the scanout it
     * hands us (see wl_commit), so leaving the host one visible draws two, a
     * few pixels apart, and the one that responds is not the one the user is
     * looking at.
     *
     * THE PLACEHOLDER IS NOT THE GUEST.  Nothing is compositing a cursor into
     * it, so hiding the host one there just leaves the window with no pointer
     * at all -- which is what happens before a VM connects, exactly when a
     * person is most likely to be moving the mouse around wondering whether
     * the thing is alive.
     *
     * A NULL surface is the protocol's "no cursor", and its scope is right by
     * construction: set_cursor applies while the pointer is over OUR surface,
     * and the title bar is a different surface of ours that sets its own, so
     * the arrow returns on its own at the window chrome.
     */
    w->ptr_on_content = true;
    /*
     * Where it entered is a position like any motion's.  A compositor sends
     * enter alone when the pointer crosses from the bars onto the picture's
     * subsurface, or when the picture appears under a still pointer; until
     * the next motion the guest pointer would stay where it was, and a click
     * land there.  Known before cur_apply: over the bars it is an arrow.
     */
    w->ptr_wx = wl_fixed_to_double(x);
    w->ptr_wy = wl_fixed_to_double(y);
    if (w->in_sub && s == w->cs_surf) {
        w->ptr_wx += w->off_x;
        w->ptr_wy += w->off_y;
    }
    cur_apply(w, serial);
    spr_kick_now(w);            /* the drawn cursor, back under a grab */
    if (w->sink) {
        nb_sink_pointer(w->sink, true);
    }
    if (!w->ui.open) {
        wl_ptr_to_guest(w);
    }
}
static void ptr_leave(void *d, struct wl_pointer *p, uint32_t serial,
                      struct wl_surface *s)
{
    struct nb_wl *w = d;

    w->ptr_on_content = false;
    /* The pointer is not over the guest: neither is the drawn cursor. */
    spr_update(w);
    spr_kick_now(w);

    if (w->ptr_on_menu || w->ptr_on_btn) {
        if (w->ptr_on_menu) {
            wl_ui_do(w, nb_ui_panel_leave(&w->ui));
        }
        if (w->btn_hot) {
            w->btn_hot = false;
            w->btn_last_ms = 0;     /* repaint on the next tick */
        }
        w->ptr_on_menu = w->ptr_on_btn = false;
        return;
    }
    if (bd_index(w, s) >= 0) {
        w->bd_hot = -1;
        return;
    }
    if (s && s == w->tb_surf) {
        w->ptr_on_tb = false;
        if (w->tb_hover != -1) {
            w->tb_hover = -1;
            tb_update(w, w->tb_w);
        }
        return;
    }
    (void)p; (void)serial; (void)s;
    if (w->sink) {
        nb_sink_pointer(w->sink, false);
    }
}
static void ptr_motion(void *d, struct wl_pointer *p, uint32_t t,
                       wl_fixed_t x, wl_fixed_t y)
{
    struct nb_wl *w = d;
    (void)p; (void)t;
    if (w->bd_hot >= 0) {
        return;                 /* chrome, not guest input */
    }
    if (w->ptr_on_dlg) {
        int was = w->dlg_hover;

        w->dlg_px_x = wl_fixed_to_int(x);
        w->dlg_px_y = wl_fixed_to_int(y);
        w->dlg_hover = dlg_hit(w, w->dlg_px_x, w->dlg_px_y);
        if (w->dlg_hover != was) {
            dlg_commit(w);
        }
        return;                 /* the overlay, not guest input */
    }
    if (w->ptr_on_tb) {
        int was = w->tb_hover;

        w->tb_px_x = wl_fixed_to_int(x);
        w->tb_px_y = wl_fixed_to_int(y);
        w->tb_hover = w->tb_w > 0 ? tb_hit(w, w->tb_px_x) : -1;
        if (w->tb_hover != was) {
            tb_update(w, w->tb_w);
        }
        return;                 /* chrome, not guest input */
    }
    {
        double wx = wl_fixed_to_double(x), wy = wl_fixed_to_double(y);

        /*
         * To WINDOW coordinates.  The main surface is the window; the
         * picture's subsurface sits at off_*.  Over the bars the pointer is
         * on the main surface.
         */
        if (w->ptr_on_btn || w->ptr_on_menu) {
            wl_ui_layer_motion(w, wx, wy);      /* layer-local */
            return;
        }
        if (w->in_sub && w->ptr_surf == w->cs_surf) {
            wx += w->off_x;
            wy += w->off_y;
        }
        w->ptr_wx = wx;
        w->ptr_wy = wy;
        wl_btn_track(w, wy);
        if (!w->ui.open && wl_ptr_on_pic(w) != w->ptr_on_pic) {
            cur_apply(w, w->last_serial);       /* picture <-> bars */
        }
        if (w->ui.open) {
            wl_ui_do(w, nb_ui_area_motion(&w->ui, wl_ui_env(w), wx, wy));
            return;
        }
        wl_ptr_to_guest(w);
        wl_nudge(w);            /* the compositor's cursor, while tearing */
    }
}
static void ptr_button(void *d, struct wl_pointer *p, uint32_t serial,
                       uint32_t t, uint32_t button, uint32_t state)
{
    struct nb_wl *w = d;

    w->last_serial = serial;
    if (w->ptr_on_btn) {
        /* The top-edge button: release over it opens the menu. */
        if (!state) {
            wl_menu_set(w, !w->ui.open);
        }
        return;
    }
    if (w->ptr_on_menu) {
        if (button == BTN_LEFT) {
            wl_ui_do(w, nb_ui_panel_button(&w->ui, wl_ui_env(w), state != 0));
        }
        return;
    }
    if (w->ui.open && !w->dlg_open && w->bd_hot < 0 && !w->ptr_on_tb) {
        /* The menu has the pointer: a drag edits the output area. */
        if (button == BTN_LEFT) {
            wl_ui_do(w, nb_ui_area_button(&w->ui, wl_ui_env(w), state != 0));
        }
        return;
    }
    if (w->ptr_on_dlg) {
        /* PRESS ARMS, RELEASE ACTS, same rule as the title bar: none of these
         * four is an action anyone should trigger by a mis-click. */
        int hit = dlg_hit(w, w->dlg_px_x, w->dlg_px_y);

        if (state) {
            w->dlg_press = hit;
            dlg_commit(w);
            return;
        }
        if (w->dlg_press >= 0 && hit == w->dlg_press) {
            int choice = w->dlg_press;

            w->dlg_press = -1;
            dlg_choose(w, choice);
            return;
        }
        w->dlg_press = -1;
        dlg_commit(w);
        return;
    }
    if (w->dlg_open) {
        /*
         * A CLICK OUTSIDE CANCELS.  Standard modal behaviour, and it is what
         * stops the dialog ever being a trap: whatever else is confusing,
         * clicking away from it always works.  The click is consumed, never
         * forwarded -- it was aimed at host UI, not at the guest.
         */
        if (state) {
            dlg_hide(w);
        }
        return;
    }
    if (w->bd_hot >= 0) {
        if (state && w->toplevel && w->seat) {
            /* Logged with the serial because a compositor SILENTLY IGNORES a
             * resize carrying a stale one -- no error, no event, nothing.
             * Without this line "I never asked" and "I asked and was ignored"
             * look identical from out here. */
            nb_log("resize: edge %d requested with serial %u (window %dx%d)",
                   nb_bd_edge[w->bd_hot], serial, w->win_w, w->win_h);
            xdg_toplevel_resize(w->toplevel, w->seat, serial,
                                nb_bd_edge[w->bd_hot]);
            wl_display_flush(w->dpy);
        }
        return;
    }
    /*
     * A click on the title bar is a window-management gesture, never guest
     * input: the close box quits, anything else asks the compositor to move
     * the window (xdg_toplevel_move is the only way a Wayland client can --
     * it has no idea where its window is).
     */
    if (w->ptr_on_tb) {
        /*
         * PRESS ARMS, RELEASE ACTS -- and only if the release is over the
         * same button.  Acting on press means a mis-click cannot be taken
         * back, and "close" is not an action anyone should be able to trigger
         * by accident on a window holding a running VM.  Dragging is the
         * exception: a window move has to begin on press or there is nothing
         * to drag.
         */
        int hit = w->tb_w > 0 ? tb_hit(w, w->tb_px_x) : -1;

        if (state) {
            w->tb_press = hit;
            if (hit < 0 && w->toplevel && w->seat) {
                xdg_toplevel_move(w->toplevel, w->seat, serial);
            }
            tb_update(w, w->tb_w);
            wl_display_flush(w->dpy);
            return;
        }
        if (w->tb_press >= 0 && hit == w->tb_press) {
            switch (hit) {
            case 0:
                /*
                 * Report it and keep running.  The VMM owns the policy -- it
                 * is the only party that knows there is a guest here and what
                 * closing its display should do to it.  If nobody is connected
                 * there is no policy to defer to, so we simply quit.
                 */
                nb_log("title bar: close");
                dlg_show(w);
                break;
            case 1:
                if (w->maximized) {
                    xdg_toplevel_unset_maximized(w->toplevel);
                } else {
                    xdg_toplevel_set_maximized(w->toplevel);
                }
                break;
            case 2:
                xdg_toplevel_set_minimized(w->toplevel);
                break;
            default:
                break;
            }
        }
        w->tb_press = -1;
        tb_update(w, w->tb_w);
        wl_display_flush(w->dpy);
        return;
    }
    (void)p; (void)serial; (void)t;
    /* wl_pointer.button carries the evdev BTN_* code directly. */
    if (w->sink) {
        nb_sink_btn(w->sink, button, state == WL_POINTER_BUTTON_STATE_PRESSED);
    }
}
static void ptr_axis(void *d, struct wl_pointer *p, uint32_t t, uint32_t axis,
                     wl_fixed_t value)
{
    struct nb_wl *w = d;
    int v = wl_fixed_to_int(value);
    (void)p; (void)t;

    if (w->ptr_on_menu) {
        if (axis == WL_POINTER_AXIS_VERTICAL_SCROLL && v) {
            wl_ui_do(w, nb_ui_panel_wheel(&w->ui, wl_ui_env(w),
                                          v > 0 ? 1 : -1));
        }
        return;
    }
    if (w->ui.open || w->ptr_on_btn || !w->sink) {
        return;
    }
    /* Wayland axis is "down is positive"; evdev REL_WHEEL is "up is positive". */
    if (axis == WL_POINTER_AXIS_VERTICAL_SCROLL) {
        nb_sink_wheel(w->sink, v > 0 ? -1 : (v < 0 ? 1 : 0), 0);
    } else {
        nb_sink_wheel(w->sink, 0, v > 0 ? 1 : (v < 0 ? -1 : 0));
    }
}
static void ptr_frame(void *d, struct wl_pointer *p) { (void)d; (void)p; }
static void ptr_axis_src(void *d, struct wl_pointer *p, uint32_t s)
{ (void)d; (void)p; (void)s; }
static void ptr_axis_stop(void *d, struct wl_pointer *p, uint32_t t,
                          uint32_t a) { (void)d; (void)p; (void)t; (void)a; }
static void ptr_axis_disc(void *d, struct wl_pointer *p, uint32_t a, int32_t v)
{ (void)d; (void)p; (void)a; (void)v; }
static const struct wl_pointer_listener ptr_listener = {
    .enter = ptr_enter, .leave = ptr_leave, .motion = ptr_motion,
    .button = ptr_button, .axis = ptr_axis, .frame = ptr_frame,
    .axis_source = ptr_axis_src, .axis_stop = ptr_axis_stop,
    .axis_discrete = ptr_axis_disc,
};

static void relptr_motion(void *d, struct zwp_relative_pointer_v1 *r,
                          uint32_t th, uint32_t tl,
                          wl_fixed_t dx, wl_fixed_t dy,
                          wl_fixed_t udx, wl_fixed_t udy)
{
    struct nb_wl *w = d;
    (void)r; (void)th; (void)tl; (void)dx; (void)dy;
    /* Unaccelerated deltas: a guest applies its own acceleration.  Whole
     * pixels go now; the fraction is carried (nb_relacc.h). */
    if (w->sink) {
        int px, py;

        nb_relacc_add(&w->relacc, udx, udy, &px, &py);
        nb_sink_rel(w->sink, px, py);
    }
}
static const struct zwp_relative_pointer_v1_listener relptr_listener = {
    .relative_motion = relptr_motion,
};

static void seat_caps(void *d, struct wl_seat *s, uint32_t caps)
{
    struct nb_wl *w = d;

    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !w->kbd) {
        w->kbd = wl_seat_get_keyboard(s);
        wl_keyboard_add_listener(w->kbd, &kbd_listener, w);
    }
    if ((caps & WL_SEAT_CAPABILITY_POINTER) && !w->ptr) {
        w->ptr = wl_seat_get_pointer(s);
        wl_pointer_add_listener(w->ptr, &ptr_listener, w);
    }
}
static void seat_name(void *d, struct wl_seat *s, const char *n)
{ (void)d; (void)s; (void)n; }
static const struct wl_seat_listener seat_listener = {
    .capabilities = seat_caps, .name = seat_name,
};

/* ── zwp_linux_dmabuf_feedback_v1: the main surface's scanout tranche ───── */
/*
 * Only the SCANOUT-flagged tranches are kept: they are the compositor saying
 * "a buffer in one of these layouts could go on a plane as it is".  A
 * compositor that never names one (it does not hint planes) leaves the
 * question open, which nb_so_guest_scanout() reports as unknown rather than
 * no.  Each batch ends with `done` and replaces the previous one.
 */
static void fb_table(void *d, struct zwp_linux_dmabuf_feedback_v1 *f,
                     int32_t fd, uint32_t size)
{
    struct nb_wl *w = d;
    void *m;

    (void)f;
    if (w->fb_tab) {
        munmap(w->fb_tab, w->fb_tab_sz);
        w->fb_tab = NULL;
        w->fb_tab_sz = 0;
    }
    m = size ? mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0) : MAP_FAILED;
    close(fd);
    if (m != MAP_FAILED) {
        w->fb_tab = m;
        w->fb_tab_sz = size;
    }
}
static void fb_main_device(void *d, struct zwp_linux_dmabuf_feedback_v1 *f,
                           struct wl_array *dev)
{
    struct nb_wl *w = d;

    (void)f;
    /* A dev_t: where explicit sync makes its timelines. */
    if (dev && dev->size == sizeof(dev_t)) {
        memcpy(&w->fb_main_dev, dev->data, sizeof(dev_t));
    }
}
static void fb_tr_device(void *d, struct zwp_linux_dmabuf_feedback_v1 *f,
                         struct wl_array *dev)
{
    struct nb_wl *w = d;

    (void)f;
    if (dev && dev->size == sizeof(dev_t)) {
        memcpy(&w->fb_tr_dev, dev->data, sizeof(dev_t));
    }
}
static void fb_tr_formats(void *d, struct zwp_linux_dmabuf_feedback_v1 *f,
                          struct wl_array *idx)
{
    struct nb_wl *w = d;
    const uint16_t *i;
    /* The table: 16 bytes per entry, u32 format, u32 padding, u64 modifier. */
    const uint8_t *tab = w->fb_tab;
    size_t n = w->fb_tab_sz / 16;

    (void)f;
    if (!tab) {
        return;
    }
    wl_array_for_each(i, idx) {
        uint32_t fourcc;
        uint64_t mod;

        if (*i >= n) {
            continue;
        }
        memcpy(&fourcc, tab + (size_t)*i * 16, 4);
        memcpy(&mod, tab + (size_t)*i * 16 + 8, 8);
        /* Only formats a guest frame can have (the rest cannot matter). */
        if (nb_fourcc_bpp(fourcc)) {
            nb_so_fmts_add(&w->fb_tr_fmts, fourcc, mod);
        }
    }
}
static void fb_tr_flags(void *d, struct zwp_linux_dmabuf_feedback_v1 *f,
                        uint32_t flags)
{
    struct nb_wl *w = d;

    (void)f;
    w->fb_tr_flags = flags;
}
static void fb_tr_done(void *d, struct zwp_linux_dmabuf_feedback_v1 *f)
{
    struct nb_wl *w = d;

    (void)f;
    w->fb_pend_tr++;
    if (w->fb_tr_flags & ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_FLAGS_SCANOUT) {
        w->fb_pend_has = true;
        for (unsigned k = 0; k < w->fb_tr_fmts.n; k++) {
            nb_so_fmts_add(&w->fb_pend_scan, w->fb_tr_fmts.e[k].fourcc,
                           w->fb_tr_fmts.e[k].mod);
        }
    } else if (!w->fb_tr_dev || !w->fb_main_dev ||
               w->fb_tr_dev == w->fb_main_dev) {
        /* What the compositor imports to render with, on its main device
         * (another device's tranche lists what only that GPU takes): where
         * the GPU copy allocates its buffers (nb_present.h nb_pc_targets). */
        w->fb_pend_has_render = true;
        for (unsigned k = 0; k < w->fb_tr_fmts.n; k++) {
            nb_so_fmts_add(&w->fb_pend_render, w->fb_tr_fmts.e[k].fourcc,
                           w->fb_tr_fmts.e[k].mod);
        }
    }
    memset(&w->fb_tr_fmts, 0, sizeof(w->fb_tr_fmts));
    w->fb_tr_flags = 0;
    w->fb_tr_dev = 0;
}
static void fb_done(void *d, struct zwp_linux_dmabuf_feedback_v1 *f)
{
    struct nb_wl *w = d;
    bool changed = !w->fb_got || w->fb_has_scan != w->fb_pend_has ||
                   w->fb_scan.n != w->fb_pend_scan.n ||
                   memcmp(w->fb_scan.e, w->fb_pend_scan.e,
                          w->fb_scan.n * sizeof(w->fb_scan.e[0]));
    bool rchanged = w->fb_has_render != w->fb_pend_has_render ||
                    w->fb_render.n != w->fb_pend_render.n ||
                    memcmp(w->fb_render.e, w->fb_pend_render.e,
                           w->fb_render.n * sizeof(w->fb_render.e[0]));

    (void)f;
    w->fb_got = true;
    w->fb_has_scan = w->fb_pend_has;
    w->fb_scan = w->fb_pend_scan;
    w->fb_has_render = w->fb_pend_has_render;
    w->fb_render = w->fb_pend_render;
    w->fb_tr = w->fb_pend_tr;
    memset(&w->fb_pend_scan, 0, sizeof(w->fb_pend_scan));
    memset(&w->fb_pend_render, 0, sizeof(w->fb_pend_render));
    w->fb_pend_has = false;
    w->fb_pend_has_render = false;
    w->fb_pend_tr = 0;
    if (rchanged && !changed) {
        w->fb_gen++;            /* the copy's targets follow the render set */
    }
    if (changed) {
        uint64_t m[4];

        w->fb_gen++;
        unsigned k = nb_so_fmts_mods(&w->fb_scan, NB_FOURCC_XR24_LOCAL, m, 4);

        if (!w->fb_has_scan) {
            nb_log("dma-buf feedback: %u tranche%s, none flagged scanout (the "
                   "compositor gives no plane hint for this surface now)",
                   w->fb_tr, w->fb_tr == 1 ? "" : "s");
        } else {
            char ms[96] = "";
            size_t o = 0;

            for (unsigned j = 0; j < k && o < sizeof(ms); j++) {
                o += (size_t)snprintf(ms + o, sizeof(ms) - o, "%s0x%llx",
                                      j ? ", " : "", (unsigned long long)m[j]);
            }
            nb_log("dma-buf feedback: scanout tranche, %u pair%s%s; XR24 "
                   "modifiers it takes: %s", w->fb_scan.n,
                   w->fb_scan.n == 1 ? "" : "s",
                   w->fb_scan.overflowed ? " (truncated)" : "",
                   k ? ms : "none");
        }
    }
}
static const struct zwp_linux_dmabuf_feedback_v1_listener dfb_listener = {
    .done = fb_done,
    .format_table = fb_table,
    .main_device = fb_main_device,
    .tranche_done = fb_tr_done,
    .tranche_target_device = fb_tr_device,
    .tranche_formats = fb_tr_formats,
    .tranche_flags = fb_tr_flags,
};

/* ── zwp_linux_dmabuf_v1: the advertised format set ──────────────────────── */

static void dmabuf_format(void *d, struct zwp_linux_dmabuf_v1 *z, uint32_t fmt)
{
    struct nb_wl *w = d;
    (void)z;
    w->n_adv_raw++;
    /* Version 1/2 only ever say "this format, implicit layout". */
    nb_formats_add(&w->formats, fmt, NB_DRM_FORMAT_MOD_INVALID);
}
static void dmabuf_modifier(void *d, struct zwp_linux_dmabuf_v1 *z,
                            uint32_t fmt, uint32_t hi, uint32_t lo)
{
    struct nb_wl *w = d;
    (void)z;
    w->n_adv_raw++;
    nb_formats_add(&w->formats, fmt, ((uint64_t)hi << 32) | lo);
}
static const struct zwp_linux_dmabuf_v1_listener dmabuf_listener = {
    .format = dmabuf_format, .modifier = dmabuf_modifier,
};

/* ── xdg-shell ───────────────────────────────────────────────────────────── */

static void wm_ping(void *d, struct xdg_wm_base *b, uint32_t serial)
{
    (void)d;
    xdg_wm_base_pong(b, serial);
}
static const struct xdg_wm_base_listener wm_listener = { .ping = wm_ping };

static void xdg_configure(void *d, struct xdg_surface *s, uint32_t serial)
{
    struct nb_wl *w = d;

    xdg_surface_ack_configure(s, serial);
    w->configured = true;
    if (w->tc_pending) {
        w->tc_pending = false;
        top_apply(w);
    }
    /* Do NOT commit a buffer here: the first real content arrives as an
     * ATTACH+COMMIT from the client.  But the main surface IS committed,
     * always: that commit is what makes the ack (and the size it acked)
     * take effect -- see top_configure().  Under a letterbox the main
     * surface is the black background, so this is cheap; on an exact fit
     * it re-commits the current frame's state unchanged. */
    tb_update(w, w->win_w > 0 ? w->win_w : w->surf_w);
    bd_layout(w);
    if (w->idle_wanted && w->current < 0) {
        wl_show_idle(w->sess);
    } else {
        wl_surf_commit(w, w->surf);
    }
    ov_sync(w);                 /* first configure maps it */
}
static const struct xdg_surface_listener xdg_listener = {
    .configure = xdg_configure,
};

/*
 * The content size to use when the window leaves fullscreen and the compositor
 * either leaves the choice to us (configure 0x0) or offers a shape that is
 * nothing like the guest's -- a portrait strip for a landscape desktop, which
 * showed up as a tall, narrow window with the picture letterboxed in the
 * middle.  The last windowed size is the answer; with none (the window
 * started fullscreen) it is the guest's aspect at a modest width.
 */
static void wl_unfs_size(const struct nb_wl *w, int *wd, int *ht)
{
    int bw = w->buf_w > 0 ? w->buf_w : 16;
    int bh = w->buf_h > 0 ? w->buf_h : 9;

    if (w->wnd_w >= NB_MIN_W && w->wnd_h >= NB_MIN_H) {
        *wd = w->wnd_w;
        *ht = w->wnd_h;
        return;
    }
    *wd = bw < 1600 ? bw : 1600;
    *ht = (int)((long)*wd * bh / bw);
    if (*ht < NB_MIN_H) {
        *ht = NB_MIN_H;
    }
}

/* True when a window of wd x ht is far from the guest's aspect (over 2:1). */
static bool wl_odd_aspect(const struct nb_wl *w, int wd, int ht)
{
    if (w->buf_w <= 0 || w->buf_h <= 0 || wd <= 0 || ht <= 0) {
        return false;
    }
    return (long)wd * w->buf_h * 2 < (long)ht * w->buf_w ||
           (long)wd * w->buf_h > (long)ht * w->buf_w * 2;
}

/*
 * xdg_toplevel.configure only RECORDS the new state; xdg_configure() applies
 * it, after xdg_surface.ack_configure.  The order matters: a compositor takes
 * the acked size as the window's size only on the next commit of the MAIN
 * surface after the ack.  Applying here commits first and acks after, and in
 * a letterbox nothing commits the main surface again (the picture is on its
 * own subsurface) -- so Hyprland kept the old size for the window and placed
 * the one-pixel black background by it: off to one side, or off screen, with
 * the desktop showing through the bars.
 */
static void top_configure(void *d, struct xdg_toplevel *t, int32_t wd,
                          int32_t ht, struct wl_array *states)
{
    struct nb_wl *w = d;
    uint32_t *st;
    (void)t;

    w->tc_w = wd;
    w->tc_h = ht;
    w->tc_fs = false;
    w->tc_max = false;
    if (states) {
        wl_array_for_each(st, states) {
            if (*st == XDG_TOPLEVEL_STATE_FULLSCREEN) {
                w->tc_fs = true;
            } else if (*st == XDG_TOPLEVEL_STATE_MAXIMIZED) {
                w->tc_max = true;
            }
        }
    }
    w->tc_pending = true;
}

/* The toplevel state top_configure() recorded, applied after the ack. */
static void top_apply(struct nb_wl *w)
{
    int32_t wd = w->tc_w, ht = w->tc_h;
    bool was_fs = w->fullscreen;
    bool left_fs, chosen = false;   /* chosen: wd/ht already content size */

    /*
     * BELIEVE THE COMPOSITOR, not our own request.  Fullscreen and maximized
     * can both be entered without us asking (a compositor keybinding, a
     * double-click on the bar we do not draw), and the title bar has to
     * disappear in fullscreen however it was reached.
     */
    w->fullscreen = w->tc_fs;
    w->maximized = w->tc_max;
    if (was_fs != w->fullscreen) {
        nb_log("window is %s", w->fullscreen ? "FULLSCREEN (chrome hidden)"
                                             : "windowed");
        /* Report what actually happened, not what we asked for: the client
         * keys a guest mode switch off this flag. */
        if (w->sink) {
            nb_sink_set_fullscreen(w->sink, w->fullscreen);
        }
        tb_update(w, w->win_w > 0 ? w->win_w : w->surf_w);
        ov_sync(w);
        wl_sync_changed(w, NULL);
        if (w->fullscreen && w->ov_mapped) {
            nb_log("overlay: drawn over the fullscreen guest, so the compositor "
                   "has to composite (no direct scanout).  CTRL+ALT+O hides "
                   "it, CTRL+ALT+D enters direct mode; the numbers stay in "
                   "the window title.");
        }
    }
    left_fs = was_fs && !w->fullscreen;
    if (wd <= 0 || ht <= 0) {
        if (was_fs != w->fullscreen) {
            wl_mode_hint(w, true);
        }
        if (!left_fs || w->maximized) {
            return;             /* "pick your own size" */
        }
        /* Leaving fullscreen with the size left to us: keeping win_w/win_h
         * would keep the fullscreen (or a stale) shape. */
        wl_unfs_size(w, &wd, &ht);
        chosen = true;
        nb_log("unfullscreen: client picks %dx%d", wd, ht);
    } else if (left_fs && !w->maximized &&
               wl_odd_aspect(w, wd, w->tb_mapped ? ht - NB_TB_H : ht)) {
        int cw, ch;

        wl_unfs_size(w, &cw, &ch);
        nb_log("unfullscreen: compositor offered %dx%d, using %dx%d", wd, ht,
               cw, ch);
        wd = cw;
        ht = ch;
        chosen = true;
    }
    /*
     * xdg_toplevel.configure reports the size of the WINDOW GEOMETRY, and our
     * geometry is the content PLUS the title bar (set_window_geometry with
     * y = -NB_TB_H, height = win_h + NB_TB_H).  So the content is what is
     * left after taking the bar back off.
     *
     * Feeding `ht` straight into win_h -- which is what this did first --
     * makes the window grow by NB_TB_H on every configure: we ask for
     * ht + NB_TB_H, the compositor obliges and tells us, we ask for
     * ht + 2*NB_TB_H, and dragging the width alone was enough to run the
     * height off the screen.  Observed 2026-08-24.
     */
    if (w->tb_mapped && !chosen) {
        ht -= NB_TB_H;
    }
    if (ht < NB_MIN_H) {
        ht = NB_MIN_H;          /* was `ht = 1` -- a collapsed window */
    }
    if (wd < NB_MIN_W) {
        wd = NB_MIN_W;
    }
    /*
     * A configure that changes nothing is worth skipping -- but ONLY if the
     * fullscreen state did not change too.  Leaving fullscreen back onto a
     * window that already happened to be this size still has to re-run the
     * geometry below, because the scaling policy is keyed on `fullscreen`, not
     * on the size.  Returning here on that edge is how the stale-viewport bug
     * would survive the fix for it.
     */
    if (wd == w->win_w && ht == w->win_h && was_fs == w->fullscreen) {
        return;
    }
    {
        int old_w = w->win_w, old_h = w->win_h;

        w->win_w = wd;
        w->win_h = ht;
        if (!w->fullscreen && !w->maximized) {
            w->wnd_w = wd;
            w->wnd_h = ht;
        }
        /* EV_SURFACE carries the WINDOW (content) size -- not the fitted
         * picture, which in aspect mode is the old mode's aspect and would
         * lock the guest to it -- and the mode hint follows the same size.
         * Debounced only for a windowed drag in --resize=guest; a fullscreen
         * edge is one deliberate action and goes at once. */
        if (old_w != wd || old_h != ht || was_fs != w->fullscreen) {
            bool now = w->fullscreen || was_fs != w->fullscreen;

            if (wd != w->cont_w || ht != w->cont_h) {
                w->cont_w = wd;
                w->cont_h = ht;
                if (w->sink) {
                    wl_report_surface(w, wd, ht);
                }
            }
            wl_mode_hint(w, now);
        }
    }
    /*
     * The window changed -- dragged, maximised, or fullscreened and back.  All
     * of those are the same operation with different numbers, so they run the
     * same code as a new frame does: wl_viewport_apply() decides the
     * destination, and it is called UNCONDITIONALLY rather than only when
     * scaling is wanted, because "scaling is no longer wanted" is precisely
     * the transition that has to reset the destination.
     *
     * Whether the guest is ASKED to change resolution is wl_mode_hint()'s
     * business above; the frame on screen is fitted here either way, so a
     * guest that has not switched (yet, or ever) is shown whole.
     */
    if (w->viewport && w->current >= 0) {
        int dw, dh;
        struct wl_surface *pic = wl_viewport_apply(w, w->buf_w, w->buf_h,
                                                   NULL, false, &dw, &dh);

        wl_surface_damage_buffer(pic, 0, 0,
                                 w->gs_frame ? INT32_MAX : w->buf_w,
                                 w->gs_frame ? INT32_MAX : w->buf_h);
        wl_surf_commit(w, pic);
        wl_main_settle(w);
        if (w->gc_on && gc_layout(w, false)) {
            cur_apply(w, w->last_serial);
        }
        wd = dw;
        ht = dh;
    }
    wl_geom_apply(w);
    /* AFTER the viewport, because wl_viewport_apply() may snap win_* back to
     * the buffer size in the 1:1 case and set_window_geometry has to agree
     * with the surface we just committed. */
    tb_update(w, w->win_w);
    bd_layout(w);
    wl_display_flush(w->dpy);
    if (w->current < 0 && w->idle_wanted) {
        wl_show_idle(w->sess);  /* repaint the placeholder at the new size */
        return;
    }
    /* What the pointer's absolute range is measured against: the picture. */
    w->surf_w = wd;
    w->surf_h = ht;
    if (w->ov_mapped) {
        ov_paint_commit(w);     /* follows the window's corner */
    }
    wl_ui_reflow(w);
}
/*
 * xdg_toplevel.close -- the compositor asking us to go away (Alt+F4, a close
 * from a window list, a session ending).  Same meaning as our own X button and
 * therefore the same path: this used to be a no-op, so those gestures did
 * nothing at all.
 */
static void top_close(void *d, struct xdg_toplevel *t)
{
    struct nb_wl *w = d;

    (void)t;
    nb_log("the compositor asked the window to close");
    dlg_show(w);
}
static const struct xdg_toplevel_listener top_listener = {
    .configure = top_configure, .close = top_close,
};

/* ── our own pointer cursor ──────────────────────────────────────────────── */
/*
 * A Wayland client owns the cursor image while the pointer is over its
 * surfaces, and "owns" includes the case where it sets none: the cursor simply
 * keeps whatever the last client set.  So hiding it over the guest's picture
 * (which composites its own) means we MUST put it back over our own title bar,
 * or it stays invisible -- including when the pointer arrives on the bar
 * directly from outside the window, which never touches the content surface at
 * all.
 */


static void cur_make(struct nb_wl *w, int slot, const char *const *art,
                     int aw, int ah, int hx, int hy, int mag);

static void cur_build(struct nb_wl *w)
{
    /* Plain arrow: 1 = black outline, 2 = white fill, space = transparent. */
    static const char *const arrow[] = {
        "1           ", "11          ", "121         ", "1221        ",
        "12221       ", "122221      ", "1222221     ", "12222221    ",
        "122222221   ", "1222222221  ", "12222222221 ", "122222111111",
        "1221221     ", "121 1221    ", "11  1221    ", "1    1221   ",
        "     1221   ", "      121   ", "      11    ",
    };
    /* Double-headed arrows for the resize edges. */
    static const char *const ew[] = {
        "   1     1   ", "  11     11  ", " 121     121 ",
        "12222222222221", "121     121 ", "  11     11  ",
        "   1     1   ",
    };
    static const char *const ns[] = {
        "   1   ", "  111  ", " 12121 ", "1211121", "  121  ",
        "  121  ", "  121  ", "  121  ", "1211121", " 12121 ",
        "  111  ", "   1   ",
    };
    static const char *const nwse[] = {
        "1111111    ", "1222211    ", "122211     ", "1221221    ",
        "121 1221   ", "11   1221  ", "1     1221 ", "       122211",
        "        11221", "         1221", "     1111111",
    };
    static const char *const nesw[] = {
        "    1111111", "    1122221", "     112221", "    1221221",
        "   1221 121", "  1221   11", " 1221     1", "112211      ",
        "12211       ", "1221        ", "1111111     ",
    };

    if (w->cur_buf[NB_CUR_ARROW]) {
        return;
    }
    cur_make(w, NB_CUR_ARROW, arrow, 12, 19, 0, 0, 1);
    cur_make(w, NB_CUR_EW,    ew,   14,  7, 7, 3, 2);
    cur_make(w, NB_CUR_NS,    ns,    7, 12, 3, 6, 2);
    cur_make(w, NB_CUR_NWSE,  nwse, 13, 11, 6, 5, 2);
    cur_make(w, NB_CUR_NESW,  nesw, 11, 11, 5, 5, 2);
}

/*
 * One cursor image, drawn rather than loaded.  libwayland-cursor plus an
 * XCURSOR theme lookup would be a new dependency and a filesystem search, for
 * five small bitmaps that are only ever seen over this broker's own chrome.
 */
static void cur_make(struct nb_wl *w, int slot, const char *const *art,
                     int aw, int ah, int hx, int hy, int mag)
{
    /*
     * `mag` is nearest-neighbour magnification, per cursor on purpose.  The
     * resize art is a 7x12-ish bitmap that reads as a speck next to a normal
     * ~24px theme cursor -- the bottom-edge one worst, being the smallest --
     * so those go 2x.  The plain arrow is already 12x19 and looked oversized
     * doubled, so it stays 1x.  Either way: no theme lookup, no new
     * dependency.
     */
    if (mag < 1) { mag = 1; }
    struct wl_shm_pool *pool;
    const int bwd = aw * mag, bht = ah * mag;
    size_t stride = (size_t)bwd * 4, sz = stride * (size_t)bht;
    uint32_t *px;
    int fd, x, y;

    if (!w->shm || !w->comp || slot < 0 || slot >= NB_CUR_N) {
        return;
    }
    fd = memfd_create("conduit-viewer-cursor", MFD_CLOEXEC);
    if (fd < 0) {
        return;
    }
    if (ftruncate(fd, (off_t)sz) < 0) {
        close(fd);
        return;
    }
    px = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (px == MAP_FAILED) {
        close(fd);
        return;
    }
    for (y = 0; y < bht; y++) {
        const char *row = art[y / mag];
        int len = (int)strlen(row);

        for (x = 0; x < bwd; x++) {
            int sx = x / mag;
            char c = sx < len ? row[sx] : ' ';

            px[(size_t)y * bwd + x] = c == '1' ? 0xff000000U
                                    : c == '2' ? 0xffffffffU
                                    : 0x00000000U;
        }
    }
    pool = wl_shm_create_pool(w->shm, fd, (int32_t)sz);
    close(fd);
    if (!pool) {
        munmap(px, sz);
        return;
    }
    w->cur_hx[slot] = hx * mag;
    w->cur_hy[slot] = hy * mag;
    w->cur_buf[slot] = wl_shm_pool_create_buffer(pool, 0, bwd, bht,
                                                 (int32_t)stride,
                                                 WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool);
    if (!w->cur_buf[slot]) {
        munmap(px, sz);
        return;
    }
    w->cur_px[slot] = px;
    w->cur_sz[slot] = sz;
    w->cur_surf[slot] = wl_compositor_create_surface(w->comp);
    if (w->cur_surf[slot]) {
        wl_surface_attach(w->cur_surf[slot], w->cur_buf[slot], 0, 0);
        wl_surface_damage_buffer(w->cur_surf[slot], 0, 0, aw, ah);
        wl_surface_commit(w->cur_surf[slot]);
    }
}

/* ── resize borders ──────────────────────────────────────────────────────── */
/*
 * Eight invisible subsurfaces around the window -- four edges, four corners.
 * They exist ONLY to receive a pointer: each is a single fully transparent
 * pixel stretched to its rectangle by a viewport, with an empty opaque region
 * (so it never occludes anything, and in particular never disqualifies the
 * content surface from direct scanout) and a full input region.
 *
 * A press on one becomes xdg_toplevel_resize(), which is the only way a
 * Wayland client can be resized by a drag: it does not know where its window
 * is on screen, so it hands the gesture to the compositor along with the edge
 * and the compositor does the rest.
 */


static void bd_geometry(const struct nb_wl *w, int i, int *x, int *y,
                        int *bw, int *bh)
{
    /*
     * THESE ARE CHILDREN OF THE CONTENT SURFACE, so their positions are in
     * CONTENT-local coordinates -- and the content is not the window.  When
     * --scale aspect is drawing letterbox bars the content sits off_x/off_y
     * in from the window's edge, so a border placed at content x=0 lands
     * off_x inside the window instead of on its edge.
     *
     * OBSERVED: the resize edges were wrong ONLY on the axis that had
     * whitespace, and by exactly the bar's width.  The title bar already
     * compensates the same way (-off_x, -off_y - NB_TB_H); the borders did
     * not.
     */
    const int tb   = w->tb_mapped ? NB_TB_H : 0;
    /* virtio-nvgpu: the main surface is the window now (see cs_surf). */
    const int left = 0;                         /* window's left edge   */
    const int top  = -tb;                       /* window's top, incl. the bar */
    int ww = w->win_w > 0 ? w->win_w : 1;
    int wh = (w->win_h > 0 ? w->win_h : 1) + tb;
    const int b = NB_BORDER;

    switch (i) {
    case 0: *x = left;     *y = top - b;  *bw = ww; *bh = b;  break; /* top    */
    case 1: *x = left;     *y = top + wh; *bw = ww; *bh = b;  break; /* bottom */
    case 2: *x = left - b; *y = top;      *bw = b;  *bh = wh; break; /* left   */
    case 3: *x = left + ww; *y = top;     *bw = b;  *bh = wh; break; /* right  */
    case 4: *x = left - b; *y = top - b;  *bw = b;  *bh = b;  break;
    case 5: *x = left + ww; *y = top - b; *bw = b;  *bh = b;  break;
    case 6: *x = left - b; *y = top + wh; *bw = b;  *bh = b;  break;
    default:*x = left + ww; *y = top + wh; *bw = b; *bh = b;  break;
    }
}

static void bd_layout(struct nb_wl *w)
{
    int i, x, y, bw, bh;

    if (!w->bd_buf || w->bd_disabled) {
        return;
    }
    /*
     * Idempotence, so the per-frame caller costs six int compares.  Without
     * that caller the borders were placed once at xdg_configure -- when off_x
     * and off_y are still 0 because no frame has arrived to letterbox -- and
     * nothing moved them until the first resize.  That is why the grab areas
     * started out displaced and silently corrected themselves after one
     * maximise.
     */
    if (w->bd_at_valid
        && w->bd_at_ox == w->off_x && w->bd_at_oy == w->off_y
        && w->bd_at_w  == w->win_w && w->bd_at_h  == w->win_h
        && w->bd_at_tb == w->tb_mapped && w->bd_at_fs == w->fullscreen) {
        return;
    }
    w->bd_at_ox = w->off_x; w->bd_at_oy = w->off_y;
    w->bd_at_w  = w->win_w; w->bd_at_h  = w->win_h;
    w->bd_at_tb = w->tb_mapped; w->bd_at_fs = w->fullscreen;
    w->bd_at_valid = true;
    /*
     * UNMAP IN FULLSCREEN, like the title bar.  There is nothing to resize a
     * fullscreen window by, and -- the reason that matters here -- fullscreen
     * is the state in which the content surface can reach a hardware plane.
     * Eight extra mapped subsurfaces hanging off it are eight more reasons
     * for a compositor to decline, for chrome nobody can use.
     */
    if (w->fullscreen) {
        for (i = 0; i < 8; i++) {
            if (w->bd_surf[i]) {
                wl_surface_attach(w->bd_surf[i], NULL, 0, 0);
                wl_surface_commit(w->bd_surf[i]);
            }
        }
        wl_surf_commit(w, w->surf);
        wl_display_flush(w->dpy);
        return;
    }
    for (i = 0; i < 8; i++) {
        if (!w->bd_surf[i]) {
            continue;
        }
        bd_geometry(w, i, &x, &y, &bw, &bh);
        if (bw <= 0 || bh <= 0) {
            continue;
        }
        wl_subsurface_set_position(w->bd_sub[i], x, y);
        if (w->bd_vp[i]) {
            wp_viewport_set_destination(w->bd_vp[i], bw, bh);
        }
        wl_surface_attach(w->bd_surf[i], w->bd_buf, 0, 0);
        wl_surface_damage_buffer(w->bd_surf[i], 0, 0, 1, 1);
        wl_surface_commit(w->bd_surf[i]);
    }
    wl_surf_commit(w, w->surf);
    wl_display_flush(w->dpy);
}

static void bd_build(struct nb_wl *w)
{
    struct wl_shm_pool *pool;
    uint32_t *px;
    int fd, i;

    if (w->bd_buf || !w->shm || !w->subcomp || !w->viewporter) {
        return;
    }
    fd = memfd_create("conduit-viewer-border", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, 4) < 0) {
        if (fd >= 0) {
            close(fd);
        }
        return;
    }
    px = mmap(NULL, 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (px == MAP_FAILED) {
        close(fd);
        return;
    }
    *px = 0x00000000U;                  /* fully transparent */
    pool = wl_shm_create_pool(w->shm, fd, 4);
    close(fd);
    if (!pool) {
        munmap(px, 4);
        return;
    }
    w->bd_buf = wl_shm_pool_create_buffer(pool, 0, 1, 1, 4,
                                          WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool);
    if (!w->bd_buf) {
        munmap(px, 4);
        return;
    }
    w->bd_px = px;
    for (i = 0; i < 8; i++) {
        struct wl_region *none;

        w->bd_surf[i] = wl_compositor_create_surface(w->comp);
        if (!w->bd_surf[i]) {
            continue;
        }
        w->bd_sub[i] = wl_subcompositor_get_subsurface(w->subcomp,
                                                       w->bd_surf[i], w->surf);
        w->bd_vp[i] = wp_viewporter_get_viewport(w->viewporter, w->bd_surf[i]);
        if (w->bd_sub[i]) {
            /* Outside the main surface, so stacking does not matter. */
            wl_subsurface_set_desync(w->bd_sub[i]);
        }
        none = wl_compositor_create_region(w->comp);
        if (none) {
            wl_surface_set_opaque_region(w->bd_surf[i], none);
            wl_region_destroy(none);
        }
    }
    nb_log("resize borders: %d px of grabbable edge around the window",
           NB_BORDER);
}

static int bd_index(const struct nb_wl *w, const struct wl_surface *s)
{
    int i;

    for (i = 0; i < 8; i++) {
        if (w->bd_surf[i] == s) {
            return i;
        }
    }
    return -1;
}

/* ── presentation feedback: FLIP or COPY ─────────────────────────────────── */
static void pres_discarded(void *d, struct wp_presentation_feedback *f)
{
    struct nb_pres_ctx *pc = d;

    /* LATEST-FRAME-WINS, observed: a commit the compositor replaced with a
     * newer one before it ever reached the screen.  Nothing queued it. */
    if (pc->w->pc_last == pc) {
        pc->w->pc_last = NULL;
    }
    wl_flog(pc->w, "D %u\n", pc->seq);
    pc->w->st_discarded++;
    nb_fs_superseded(&pc->w->fs);
    free(pc);
    wp_presentation_feedback_destroy(f);
}
static void pres_sync_output(void *d, struct wp_presentation_feedback *f,
                             struct wl_output *o)
{ (void)d; (void)f; (void)o; }
static void pres_presented(void *d, struct wp_presentation_feedback *f,
                           uint32_t tv_sec_hi, uint32_t tv_sec_lo,
                           uint32_t tv_nsec, uint32_t refresh,
                           uint32_t sh, uint32_t sl, uint32_t flags)
{
    struct nb_pres_ctx *pc = d;
    struct nb_wl *w = pc->w;
    int zc = (flags & WP_PRESENTATION_FEEDBACK_KIND_ZERO_COPY) ? 1 : 0;

    (void)sh; (void)sl;
    wp_presentation_feedback_destroy(f);
    if (w->pres_clock == CLOCK_MONOTONIC) {
        uint64_t tp = ((((uint64_t)tv_sec_hi << 32) | tv_sec_lo) * 1000000000ull) +
                      tv_nsec;

        /* The vblank clock for --sync=latch, learned in every mode so a
         * switch to it starts clocked. */
        nb_latch_presented(&w->latch, tp, refresh,
                           (flags & WP_PRESENTATION_FEEDBACK_KIND_VSYNC) != 0,
                           pc->aimed);
        wl_flog(w, "P %u %llu %u %u\n", pc->seq, (unsigned long long)tp,
                refresh, flags);
    }

    /* The client's guest completes the flip on this (EV_PRESENTED): the
     * frame is on the screen now, not when it was committed. */
    if (w->sink) {
        uint64_t tp = 0;

        if (w->pres_clock == CLOCK_MONOTONIC) {
            tp = ((((uint64_t)tv_sec_hi << 32) | tv_sec_lo) * 1000000000ull) +
                 tv_nsec;
        }
        nb_sink_presented(w->sink, pc->seq, flags, tp);
    }

    /*
     * The fullscreen path measurement (nb_scanout.h): receive -> on screen,
     * the GPU pass included when it ran.  A compositor on another clock
     * (weston: CLOCK_MONOTONIC_RAW) is brought onto ours by reading both
     * clocks now; the two calls are a microsecond apart.
     */
    if (w->pres_clock != UINT32_MAX && pc->recv_ns) {
        uint64_t t = ((((uint64_t)tv_sec_hi << 32) | tv_sec_lo) *
                      1000000000ull) + tv_nsec;

        if (w->pres_clock != CLOCK_MONOTONIC) {
            struct timespec a;

            if (clock_gettime((clockid_t)w->pres_clock, &a) == 0) {
                uint64_t pn = (uint64_t)a.tv_sec * 1000000000ull +
                              (uint64_t)a.tv_nsec;

                t = t + nb_mono_ns() - pn;
            } else {
                t = 0;
            }
        }
        if (t >= pc->recv_ns) {
            nb_so_presented(&w->so, pc->so_gen, (enum nb_so_path)pc->so_path,
                            zc != 0, t - pc->recv_ns);
        }
    }

    /* virtio-nvgpu --stats: commit -> presented, and sender -> presented. */
    w->st_presented++;
    w->st_zc += (unsigned)zc;
    if (w->pres_clock != CLOCK_MONOTONIC || !pc->commit_ns) {
        nb_fs_presented(&w->fs, 0, 0, 0, zc);   /* counted, not timed */
    }
    if (w->pres_clock == CLOCK_MONOTONIC && pc->commit_ns) {
        uint64_t t = ((((uint64_t)tv_sec_hi << 32) | tv_sec_lo) *
                      1000000000ull) + tv_nsec;

        nb_fs_presented(&w->fs, pc->start_ns, pc->commit_ns, t, zc);

        if (w->st_last_pres && t > w->st_last_pres &&
            t - w->st_last_pres < 1000000000ull) {
            nb_stats_sample(w->st_pi_s, &w->st_pi_k, t - w->st_last_pres);
        }
        w->st_last_pres = t;
        if (t >= pc->commit_ns) {
            uint64_t dd = t - pc->commit_ns;

            nb_stats_sample(w->st_cp_s, &w->st_cp_k, dd);
            w->st_cp_sum += dd;
            w->st_cp_n++;
            if (dd > w->st_cp_max) {
                w->st_cp_max = dd;
            }
        }
        if (nb_sink_seq_usec(w->sink)) {
            uint32_t dus = (uint32_t)(t / 1000u) - pc->seq;

            if (dus < 1000000u) {
                w->st_sp_sum += dus;
                w->st_sp_n++;
            }
        }
    }
    if (w->pc_last == pc) {
        w->pc_last = NULL;
    }
    free(pc);
    nb_stats_flush(w, nb_mono_ns());

    /*
     * THE HOST'S REFRESH, which the guest otherwise cannot know.
     *
     * Its virtual head is pinned by the kms_hz module parameter (default 60)
     * while the host may be running at 144, so the guest renders at a rate
     * nothing asked for and neither side is aware.  `refresh` here is the
     * output's nominal frame interval in NANOSECONDS; QemuUIInfo.refresh_rate
     * and our EV_SURFACE both use MILLIHERTZ, so 16666666ns -> 60000.
     *
     * Compared at whole-Hz granularity before re-hinting: the value is nominal
     * rather than measured and should be stable, but a hint per frame would be
     * a re-mode per frame, and that is the shape of bug this file has already
     * had twice.
     */
    if (refresh > 0) {
        unsigned mhz = (unsigned)(1000000000000ULL / refresh);

        if (nb_hz_mode == NB_HZ_AUTO && mhz / 1000 != w->refresh_mhz / 1000) {
            w->refresh_mhz = mhz;
            nb_log("host output refresh is %u.%03u Hz — telling the guest, so "
                   "it need not stay at its built-in default",
                   mhz / 1000, mhz % 1000);
            /* Including the first learn: the guest was told 0 ("keep yours")
             * until now, and in fullscreen it should match the output. */
            if (w->sink && w->cont_w > 0) {
                w->hint_w = 0;          /* force the idempotence check open */
                wl_report_surface(w, w->cont_w, w->cont_h);
            }
            wl_mode_hint(w, true);
        }
    }
    if (zc != w->last_scanout) {
        w->last_scanout = zc;
        /*
         * Deliberately worded like the X11 backend's line, because it is the
         * same fact: FLIP means the guest's own buffer is on a hardware plane
         * and nothing copied it.
         */
        nb_log("Present: %s%s  (refresh %u.%03u ms)",
               zc ? "FLIP" : "COPY",
               zc ? " - the guest's buffer is scanned out directly"
                  : " - the compositor is compositing it",
               refresh / 1000000, (refresh % 1000000) / 1000);
    }
}
const struct wp_presentation_feedback_listener nb_pres_listener = {
    .sync_output = pres_sync_output,
    .presented   = pres_presented,
    .discarded   = pres_discarded,
};

/* ── decorations ─────────────────────────────────────────────────────────── */
/*
 * A Wayland toplevel has no chrome unless somebody draws it.  With neither
 * client-side decorations nor a server-side request the window arrives bare:
 * no title bar, no close button, nothing to drag a corner by — and the title
 * we set is never rendered anywhere, so --title looks broken.  We ask the
 * compositor to decorate, and we BELIEVE ITS ANSWER rather than the request:
 * a compositor is allowed to reply client-side, and then a chrome-less window
 * is what the user gets and they should be told why.
 */
static void deco_configure(void *d, struct zxdg_toplevel_decoration_v1 *dec,
                           uint32_t mode)
{
    struct nb_wl *w = d;
    (void)dec;

    if (mode == ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE) {
        nb_log("window decorations: server-side (title bar, close, resize)");
        /* The compositor resizes the window by its own border; our invisible
         * grab strips outside the window would only widen the surface tree
         * the compositor has to reason about. */
        if (!w->bd_disabled) {
            w->bd_disabled = true;
            for (int i = 0; i < 8; i++) {
                if (w->bd_surf[i]) {
                    wl_surface_attach(w->bd_surf[i], NULL, 0, 0);
                    wl_surface_commit(w->bd_surf[i]);
                }
            }
        }
    } else {
        nb_log("window decorations: the compositor insists on CLIENT-side, "
               "and this broker draws none — expect a bare window with no "
               "title bar and no resize grips.");
    }
    w->deco_mode = mode;
}
static const struct zxdg_toplevel_decoration_v1_listener deco_listener = {
    .configure = deco_configure,
};

static void pres_clock_id(void *d, struct wp_presentation *p, uint32_t clk)
{
    struct nb_wl *w = d;
    (void)p;

    w->pres_clock = clk;
}
static const struct wp_presentation_listener pres_clock_listener = {
    .clock_id = pres_clock_id,
};

/* ── registry ────────────────────────────────────────────────────────────── */

static void reg_global(void *data, struct wl_registry *r, uint32_t name,
                       const char *iface, uint32_t ver)
{
    struct nb_wl *w = data;

#define BIND(I, V) wl_registry_bind(r, name, &I, (ver < (V)) ? ver : (V))
    if (!strcmp(iface, wl_compositor_interface.name)) {
        w->comp = BIND(wl_compositor_interface, 4);
    } else if (!strcmp(iface, wl_seat_interface.name) && !w->seat) {
        w->seat = BIND(wl_seat_interface, 5);
        wl_seat_add_listener(w->seat, &seat_listener, w);
    } else if (!strcmp(iface, zxdg_decoration_manager_v1_interface.name)) {
        w->deco_mgr = BIND(zxdg_decoration_manager_v1_interface, 1);
    } else if (!strcmp(iface, wl_subcompositor_interface.name)) {
        w->subcomp = BIND(wl_subcompositor_interface, 1);
    } else if (!strcmp(iface, wp_presentation_interface.name)) {
        w->presentation = BIND(wp_presentation_interface, 1);
        w->pres_clock = UINT32_MAX;
        wp_presentation_add_listener(w->presentation, &pres_clock_listener, w);
    } else if (!strcmp(iface, wp_tearing_control_manager_v1_interface.name)) {
        w->tearing_mgr = BIND(wp_tearing_control_manager_v1_interface, 1);
    } else if (!strcmp(iface, wp_content_type_manager_v1_interface.name)) {
        w->ctype_mgr = BIND(wp_content_type_manager_v1_interface, 1);
    } else if (!strcmp(iface,
                       wp_single_pixel_buffer_manager_v1_interface.name)) {
        w->spb_mgr = BIND(wp_single_pixel_buffer_manager_v1_interface, 1);
    } else if (!strcmp(iface, wp_fractional_scale_manager_v1_interface.name)) {
        w->frac_mgr = BIND(wp_fractional_scale_manager_v1_interface, 1);
    } else if (!strcmp(iface, wl_data_device_manager_interface.name)) {
        w->ddm = BIND(wl_data_device_manager_interface, 3);
    } else if (!strcmp(iface, wp_viewporter_interface.name)) {
        w->viewporter = BIND(wp_viewporter_interface, 1);
    } else if (!strcmp(iface, wl_shm_interface.name) && !w->shm) {
        w->shm = BIND(wl_shm_interface, 1);
    } else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        w->wm_base = BIND(xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(w->wm_base, &wm_listener, w);
    } else if (!strcmp(iface, zwp_linux_dmabuf_v1_interface.name) &&
               !w->dmabuf) {
        /*
         * Cap at 3 ON PURPOSE.  From version 4 the compositor MUST NOT send
         * the `format`/`modifier` events — the per-surface feedback object
         * replaces them — so binding 4 would leave the advertised set empty
         * and every ATTACH would be rejected as an unadvertised format.
         */
        w->dmabuf_ver = ver < 3 ? ver : 3;
        w->dmabuf = wl_registry_bind(r, name, &zwp_linux_dmabuf_v1_interface,
                                     w->dmabuf_ver);
        zwp_linux_dmabuf_v1_add_listener(w->dmabuf, &dmabuf_listener, w);
        /* And a version 4 binding of the same global for the surface's
         * dma-buf feedback (scanout tranches); see dmabuf4. */
        if (ver >= 4) {
            w->dmabuf4 = wl_registry_bind(r, name,
                                          &zwp_linux_dmabuf_v1_interface, 4);
        }
    } else if (!strcmp(iface,
               zwp_keyboard_shortcuts_inhibit_manager_v1_interface.name)) {
        w->inhibit_mgr =
            BIND(zwp_keyboard_shortcuts_inhibit_manager_v1_interface, 1);
    } else if (!strcmp(iface, zwp_pointer_constraints_v1_interface.name)) {
        w->constraints = BIND(zwp_pointer_constraints_v1_interface, 1);
    } else if (!strcmp(iface,
               zwp_relative_pointer_manager_v1_interface.name)) {
        w->relptr_mgr = BIND(zwp_relative_pointer_manager_v1_interface, 1);
#ifdef NB_HAVE_WL_SYNCOBJ
    } else if (!strcmp(iface,
               wp_linux_drm_syncobj_manager_v1_interface.name)) {
        w->xs_mgr = BIND(wp_linux_drm_syncobj_manager_v1_interface, 1);
#endif
    }
#undef BIND
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t name)
{ (void)d; (void)r; (void)name; }
static const struct wl_registry_listener reg_listener = {
    .global = reg_global, .global_remove = reg_remove,
};

/* ── virtio-nvgpu: window geometry ───────────────────────────────────────── */
/*
 * THE ONE PLACE THE WINDOW GEOMETRY IS SET, from the same numbers the
 * letterbox and the title bar are placed by.  The content surface is the
 * origin; the window starts off_* up-left of it (the letterbox) and, with our
 * own title bar, NB_TB_H above that.  Everything else hanging off the surface
 * -- the invisible resize strips, the overlay -- is outside or on top of the
 * window and must not widen it.
 */
static void wl_geom_apply(struct nb_wl *w)
{
    int x, y, gw, gh;

    if (!w->xdg_surf || w->win_w <= 0 || w->win_h <= 0) {
        return;
    }
    /* The main surface covers the window exactly (see cs_surf), so the
     * geometry never reaches outside it. */
    x = 0;
    y = 0;
    gw = w->win_w;
    gh = w->win_h;
    if (w->tb_mapped && !w->fullscreen) {
        y -= NB_TB_H;
        gh += NB_TB_H;
        if (w->tb_w > 0) {
            gw = w->tb_w;
        }
    }
    if (w->geo_valid && x == w->geo_x && y == w->geo_y && gw == w->geo_w &&
        gh == w->geo_h) {
        return;
    }
    w->geo_valid = true;
    w->geo_x = x;
    w->geo_y = y;
    w->geo_w = gw;
    w->geo_h = gh;
    xdg_surface_set_window_geometry(w->xdg_surf, x, y, gw, gh);
}

/* ── virtio-nvgpu: mode hints (EV_MODE_HINT) ─────────────────────────────── */

#define NB_HINT_DEBOUNCE_MS 150

/*
 * What mode should the guest be in?  Decided from HOST state only -- the
 * window, the output, the user's --resize choice -- never from a guest frame,
 * so there is no loop to oscillate in.
 *
 *   fullscreen          the output's exact mode, in buffer pixels: 1:1 and
 *                       covering the output is the compositor's condition for
 *                       direct scanout
 *   windowed, scale     (default) "your configured mode": the full guest
 *                       picture is fitted into whatever window the WM gives
 *   windowed, guest     the window's own size, debounced while dragging
 */
static void wl_mode_hint(struct nb_wl *w, bool immediate)
{
    unsigned s = w->scale_120 ? w->scale_120 : 120;
    unsigned hw = 0, hh = 0, hz, why;
    const char *whys[] = { "restore the configured mode", "fullscreen",
                           "follow the window", "fixed --resolution" };

    if (!w->sink || !nb_sink_has_client(w->sink) ||
        !(w->sess->caps & NVKVM_BROKER_CAP_MODE_HINTS) ||
        nb_res_mode == NB_RES_NONE) {
        w->mh_due_ms = 0;
        return;
    }
    hz = nb_hz_mode == NB_HZ_NONE ? 0
       : nb_hz_mode == NB_HZ_FIXED ? nb_hz_fixed : w->refresh_mhz;
    if (nb_res_mode == NB_RES_FIXED) {
        hw = nb_res_w;
        hh = nb_res_h;
        why = NVKVM_BROKER_HINT_FIXED;
    } else if (w->fullscreen || w->resize_mode == NB_RESIZE_GUEST) {
        if (w->win_w <= 0 || w->win_h <= 0) {
            return;
        }
        nb_view_guest_mode(&w->vst.cur, w->win_w, w->win_h, s, &hw, &hh);
        if (w->hint_align > 1) {
            hw -= hw % w->hint_align;
        }
        if (!hw || !hh || hw > NVKVM_BROKER_MAX_DIM ||
            hh > NVKVM_BROKER_MAX_DIM) {
            return;
        }
        why = w->fullscreen ? NVKVM_BROKER_HINT_FULLSCREEN
                            : NVKVM_BROKER_HINT_WINDOW;
    } else {
        hz = 0;                 /* the configured rate, too */
        why = NVKVM_BROKER_HINT_RESTORE;
    }
    if (w->mh_valid && hw == w->mh_w && hh == w->mh_h && hz == w->mh_hz &&
        why == w->mh_why) {
        w->mh_due_ms = 0;       /* a drag that came back where it started */
        return;
    }
    if (!immediate) {
        w->mh_due_ms = nb_now_ms_wl() + NB_HINT_DEBOUNCE_MS;
        return;
    }
    w->mh_due_ms = 0;
    w->mh_valid = true;
    w->mh_w = hw;
    w->mh_h = hh;
    w->mh_hz = hz;
    w->mh_why = why;
    if (why == NVKVM_BROKER_HINT_RESTORE) {
        nb_log("mode hint: %s", whys[why]);
    } else {
        nb_log("mode hint: %ux%u@%u.%03u (%s)", hw, hh, hz / 1000, hz % 1000,
               whys[why < 4 ? why : 0]);
    }
    nb_sink_mode_hint(w->sink, hw, hh, hz, why);
}

/* ── virtio-nvgpu: the title, always ─────────────────────────────────────── */

static void wl_title_update(struct nb_wl *w, bool force)
{
    char t[sizeof(w->title_cur)];
    char base[sizeof(w->title) + 16];
    bool flip = nb_sink_seq_usec(w->sink);

    if (!w->toplevel) {
        return;
    }
    /* Paused (the menu's Pause): said in the name too, for task bars. */
    snprintf(base, sizeof(base), "%s%s", w->vmp.paused ? "Paused - " : "",
             w->title);
    /*
     * No live numbers in the title while fullscreen: nobody sees it there, and
     * every title change makes the compositor re-run its window rules for us
     * (Hyprland: all of them, twice a second, logged) -- work on the frame
     * path's thread of a compositor that is meant to be scanning us out.
     */
    if (w->stats_active && !w->fullscreen) {
        nb_fs_title(t, sizeof(t), base, (unsigned)w->buf_w,
                    (unsigned)w->buf_h, w->refresh_mhz, &w->snap, flip);
        if (w->snap.frames) {
            snprintf(w->tb_stats, sizeof(w->tb_stats), "%.0f FPS %.1f MS %s",
                     w->snap.fps, w->snap.ft_avg,
                     w->snap.direct > 0 ? "DIRECT"
                     : w->snap.direct == 0 ? "COMPOSITED" : "");
        } else {
            w->tb_stats[0] = '\0';
        }
    } else {
        snprintf(t, sizeof(t), "%s", base);
    }
    if (!force && !strcmp(t, w->title_cur)) {
        return;
    }
    snprintf(w->title_cur, sizeof(w->title_cur), "%s", t);
    xdg_toplevel_set_title(w->toplevel, t);
    if (w->tb_mapped) {
        tb_update(w, w->tb_w);
    }
}

/* ── virtio-nvgpu: the overlay ───────────────────────────────────────────── */
/*
 * A subsurface over the window's top-left corner, desynchronised from the
 * content so repainting it never touches the frame path.  Two shm buffers so
 * one can be painted while the compositor still reads the other; if both are
 * held the repaint is skipped -- it is a quarter-second readout, not a frame.
 * An empty input region keeps the pointer going to the guest underneath.
 *
 * It occludes the content, and an occluded surface is not a direct-scanout
 * candidate: that is why direct mode hides it and --overlay=windowed exists.
 */
static void ov_release(void *data, struct wl_buffer *b)
{
    struct nb_wl *w = data;

    for (int i = 0; i < 2; i++) {
        if (w->ov_buf[i] == b) {
            w->ov_busy[i] = false;
        }
    }
}
static const struct wl_buffer_listener ov_buf_listener = {
    .release = ov_release,
};

static bool ov_want(const struct nb_wl *w)
{
    if (!w->ov_user || w->direct || !w->configured || !w->subcomp ||
        !w->shm || !w->comp) {
        return false;
    }
    if (w->fullscreen && w->ov_mode != NB_OVERLAY_ALWAYS) {
        return false;
    }
    return true;
}

static void ov_drop_buffers(struct nb_wl *w)
{
    for (int i = 0; i < 2; i++) {
        if (w->ov_buf[i]) {
            wl_buffer_destroy(w->ov_buf[i]);
            w->ov_buf[i] = NULL;
        }
        w->ov_busy[i] = false;
    }
    if (w->ov_px) {
        munmap(w->ov_px, w->ov_sz);
        w->ov_px = NULL;
        w->ov_sz = 0;
    }
}

static bool ov_build(struct nb_wl *w, unsigned sc)
{
    struct wl_shm_pool *pool;
    unsigned ow, oh;
    size_t one;
    void *px;
    int fd;

    if (!w->ov_surf) {
        struct wl_region *none;

        w->ov_surf = wl_compositor_create_surface(w->comp);
        if (!w->ov_surf) {
            return false;
        }
        w->ov_sub = wl_subcompositor_get_subsurface(w->subcomp, w->ov_surf,
                                                    w->surf);
        if (!w->ov_sub) {
            return false;
        }
        /* Above the picture's own subsurface because it is made after it
         * (see SUBSURFACE STACKING). */
        wl_subsurface_set_desync(w->ov_sub);
        none = wl_compositor_create_region(w->comp);
        if (none) {
            wl_surface_set_input_region(w->ov_surf, none);
            wl_region_destroy(none);
        }
        w->ov_at_x = w->ov_at_y = INT32_MIN;
    }
    if (w->ov_px && w->ov_sc == sc) {
        return true;
    }
    ov_drop_buffers(w);
    nb_ov_size(sc, &ow, &oh);
    one = (size_t)ow * oh * 4;
    fd = memfd_create("nvgpu-viewer-overlay", MFD_CLOEXEC);
    if (fd < 0) {
        return false;
    }
    if (ftruncate(fd, (off_t)(one * 2)) < 0) {
        close(fd);
        return false;
    }
    px = mmap(NULL, one * 2, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (px == MAP_FAILED) {
        close(fd);
        return false;
    }
    pool = wl_shm_create_pool(w->shm, fd, (int32_t)(one * 2));
    close(fd);
    if (!pool) {
        munmap(px, one * 2);
        return false;
    }
    for (int i = 0; i < 2; i++) {
        w->ov_buf[i] = wl_shm_pool_create_buffer(pool, (int32_t)(one * i),
                                                 (int32_t)ow, (int32_t)oh,
                                                 (int32_t)ow * 4,
                                                 WL_SHM_FORMAT_ARGB8888);
        if (w->ov_buf[i]) {
            wl_buffer_add_listener(w->ov_buf[i], &ov_buf_listener, w);
        }
    }
    wl_shm_pool_destroy(pool);
    w->ov_px = px;
    w->ov_sz = one * 2;
    w->ov_w = ow;
    w->ov_h = oh;
    w->ov_sc = sc;
    return w->ov_buf[0] && w->ov_buf[1];
}

static void ov_paint_commit(struct nb_wl *w)
{
    static uint32_t graph[512];
    struct nb_ov_info in = {0};
    unsigned cols;
    int i, x, y;

    if (!w->ov_mapped || !w->ov_px) {
        return;
    }
    i = !w->ov_busy[0] ? 0 : !w->ov_busy[1] ? 1 : -1;
    if (i < 0) {
        return;                 /* both still on screen: skip this one */
    }
    in.mode_w = w->current >= 0 ? (unsigned)w->buf_w : 0;
    in.mode_h = w->current >= 0 ? (unsigned)w->buf_h : 0;
    in.refresh_mhz = w->refresh_mhz;
    in.scale = w->buf_w > 0 && w->fit_w > 0
                   ? (double)w->fit_w * (w->scale_120 ? w->scale_120 : 120) /
                         120.0 / w->buf_w
                   : 1.0;
    in.tearing = w->tearing_async;
    in.direct_mode = w->direct;
    in.guest_resize = w->resize_mode == NB_RESIZE_GUEST;
    in.flip_known = nb_sink_seq_usec(w->sink);
    in.host_cursor = w->gc_on && !w->grabbed;
    in.viewer_cursor = w->spr_mapped;
    {
        static char gpu[40];
        const char *n = w->gs ? strrchr(nb_gs_node(w->gs), '/') : NULL;
        bool copy = w->current >= 0 && w->bufs[w->current].copy;

        in.path = copy ? "GPU COPY" : w->gs_shown ? "GPU SCALE" : "DIRECT";
        snprintf(gpu, sizeof(gpu), "%s", n ? n + 1 : "");
        for (char *p = gpu; *p; p++) {
            *p = (char)(*p >= 'a' && *p <= 'z' ? *p - 32 : *p);
        }
        in.gpu = gpu;
    }
    cols = (w->ov_w - 8 * w->ov_sc) / w->ov_sc;
    if (cols > sizeof(graph) / sizeof(graph[0])) {
        cols = sizeof(graph) / sizeof(graph[0]);
    }
    nb_fs_graph(&w->fs, nb_mono_ns(), 2000, graph, cols);
    nb_ov_paint((uint32_t *)((char *)w->ov_px + (size_t)i * w->ov_w * w->ov_h * 4),
                w->ov_w, w->ov_h, w->ov_sc, &w->snap, &in, graph, cols);
    /* The window's corner: the main surface's, always inside it. */
    x = 8;
    y = 8;
    if (x != w->ov_at_x || y != w->ov_at_y) {
        w->ov_at_x = x;
        w->ov_at_y = y;
        wl_subsurface_set_position(w->ov_sub, x, y);  /* on the next frame */
    }
    wl_surface_attach(w->ov_surf, w->ov_buf[i], 0, 0);
    wl_surface_damage_buffer(w->ov_surf, 0, 0, (int32_t)w->ov_w,
                             (int32_t)w->ov_h);
    wl_surface_commit(w->ov_surf);
    w->ov_busy[i] = true;
}

static void ov_sync(struct nb_wl *w)
{
    bool want = ov_want(w);
    unsigned sc = w->win_w > 0 && w->win_w < 900 ? 1 : 2;

    if (want) {
        if (!ov_build(w, sc)) {
            return;
        }
        if (!w->ov_mapped) {
            w->ov_mapped = true;
            nb_log("overlay: on (CTRL+ALT+O toggles)");
        }
        ov_paint_commit(w);
        /* The subsurface's position is parent state: commit the parent so a
         * first map or a move does not wait for the next guest frame. */
        wl_surf_commit(w, w->surf);
    } else if (w->ov_mapped) {
        w->ov_mapped = false;
        if (w->ov_surf) {
            wl_surface_attach(w->ov_surf, NULL, 0, 0);
            wl_surface_commit(w->ov_surf);
        }
        nb_log("overlay: off");
    }
    wl_display_flush(w->dpy);
}

/* ── virtio-nvgpu: the clock-driven half ─────────────────────────────────── */

static int wl_tick_min(int a, uint64_t due_ms, uint64_t now)
{
    int b = due_ms > now ? (int)(due_ms - now) : 0;

    return a < 0 || b < a ? b : a;
}

/*
 * A HOST CLIPBOARD OWNER THAT NEVER ANSWERS.  The selection is read through a
 * pipe the owning program writes; one that never writes nor closes it (hung,
 * or simply slow to notice) used to leave the paste chord held -- every later
 * Ctrl+V swallowed -- and every later fetch -EBUSY, for good.  Give up after
 * NB_CLIP_FETCH_TIMEOUT_MS without progress: release the chord, drop the pipe.
 */
static int wl_tick_fetch(struct nb_wl *w, int next)
{
    uint64_t now;

    if (w->fetch_fd < 0) {
        return next;
    }
    now = nb_now_ms_wl();
    if (now < w->fetch_deadline_ms) {
        return wl_tick_min(next, w->fetch_deadline_ms, now);
    }
    nb_log("clipboard: the program holding the host clipboard sent nothing "
           "for %u ms; giving up on it", NB_CLIP_FETCH_TIMEOUT_MS);
    if (w->sink) {
        nb_sink_clip_finish(w->sink, w->fetch_generation, false);
    }
    close(w->fetch_fd);
    w->fetch_fd = -1;
    w->fetch_len = 0;
    w->fetch_generation = 0;
    free(w->fetch_buf);
    w->fetch_buf = NULL;
    w->fetch_cap = 0;
    return next;
}

static int wl_tick_viewer(struct nb_wl *w, int next)
{
    uint64_t now = nb_now_ms_wl();
    bool flush = false;

    next = wl_tick_ui(w, next);

    if (w->mh_due_ms) {
        if (now >= w->mh_due_ms) {
            wl_mode_hint(w, true);
            flush = true;
        } else {
            next = wl_tick_min(next, w->mh_due_ms, now);
        }
    }
    if (w->stats_active && w->sink && nb_sink_has_client(w->sink)) {
        if (now >= w->fs_next_ms) {
            uint64_t ns = nb_mono_ns();

            w->fs_next_ms = now + NB_FS_BUCKET_MS;
            nb_fs_roll(&w->fs, ns, w->sink->n_input, w->sink->n_reject);
            nb_fs_snapshot(&w->fs, ns, w->last_scanout, &w->snap);
            if (w->ov_mapped) {
                ov_paint_commit(w);
            }
            if (now >= w->title_next_ms) {
                w->title_next_ms = now + 500;
                wl_title_update(w, false);
            }
            flush = true;
        }
        next = wl_tick_min(next, w->fs_next_ms, now);
    }
    if (flush) {
        wl_display_flush(w->dpy);
    }
    return next;
}

/* ── virtio-nvgpu: direct mode ───────────────────────────────────────────── */

/* `CMD on|off` through /bin/sh, not waited for: a double fork, so there is no
 * child of ours to reap and nothing the hook does can stall the input loop. */
static void nb_run_hook(const char *cmd, const char *arg)
{
    char line[1024];
    pid_t p;

    if (!cmd || !*cmd) {
        return;
    }
    snprintf(line, sizeof(line), "%s %s", cmd, arg);
    p = fork();
    if (p < 0) {
        nb_err("direct-hook: fork: %s", strerror(errno));
        return;
    }
    if (p == 0) {
        sigset_t none;
        pid_t q = fork();

        if (q != 0) {
            _exit(q < 0 ? 1 : 0);
        }
        /* The broker blocks its signals for signalfd; the hook must not
         * inherit that, nor SIGPIPE ignored. */
        sigemptyset(&none);
        sigprocmask(SIG_SETMASK, &none, NULL);
        signal(SIGPIPE, SIG_DFL);
        {
            int fd = open("/dev/null", O_RDONLY | O_CLOEXEC);

            if (fd >= 0) {
                dup2(fd, STDIN_FILENO);
            }
        }
        execl("/bin/sh", "sh", "-c", line, (char *)NULL);
        _exit(127);
    }
    while (waitpid(p, NULL, 0) < 0 && errno == EINTR) {
    }
    nb_log("direct-hook: ran `%s`", line);
}

/*
 * HYPRLAND: the `immediate` window property on our own window, over the IPC
 * socket ($XDG_RUNTIME_DIR/hypr/SIG/.socket.sock, /tmp/hypr/SIG/... before
 * 0.40).  Hyprland only tears a window that has it (a window rule or this),
 * whatever the client's tearing hint says.  `lock` keeps a dynamic rule
 * re-evaluation (fullscreen toggles re-run the rules) from resetting it.
 * Blocking with a short timeout: it runs on a fullscreen / sync change only,
 * and Hyprland answers a local request in well under a millisecond.
 */
static int wl_hypr_request(const char *sig, const char *req, char *ans,
                           size_t n)
{
    const char *rt = getenv("XDG_RUNTIME_DIR");
    char path[2][160];
    int k = 0, i, fd = -1;
    struct sockaddr_un sa;
    struct timeval tv = { 0, 250000 };
    ssize_t got = 0;

    if (rt && *rt) {
        snprintf(path[k++], sizeof(path[0]), "%s/hypr/%s/.socket.sock", rt,
                 sig);
    }
    snprintf(path[k++], sizeof(path[0]), "/tmp/hypr/%s/.socket.sock", sig);
    for (i = 0; i < k; i++) {
        if (strlen(path[i]) >= sizeof(sa.sun_path)) {
            continue;
        }
        fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
        if (fd < 0) {
            return -errno;
        }
        memset(&sa, 0, sizeof(sa));
        sa.sun_family = AF_UNIX;
        memcpy(sa.sun_path, path[i], strlen(path[i]) + 1);
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
        if (connect(fd, (struct sockaddr *)&sa, sizeof(sa)) == 0) {
            break;
        }
        close(fd);
        fd = -1;
    }
    if (fd < 0) {
        return -ENOENT;
    }
    if (send(fd, req, strlen(req), MSG_NOSIGNAL) != (ssize_t)strlen(req)) {
        close(fd);
        return -EIO;
    }
    while ((size_t)got + 1 < n) {
        ssize_t r = read(fd, ans + got, n - 1 - (size_t)got);

        if (r <= 0) {
            break;
        }
        got += r;
    }
    ans[got > 0 ? got : 0] = '\0';
    close(fd);
    return 0;
}

static void wl_hypr_sync(struct nb_wl *w)
{
    char req[96], ans[128];
    int r;

    if (!w->hypr_sig || w->hypr_failed || !w->has_content ||
        w->hypr_want < 0 || w->hypr_want == w->hypr_sent) {
        return;
    }
    snprintf(req, sizeof(req), "setprop pid:%d immediate %d lock",
             (int)getpid(), w->hypr_want);
    r = wl_hypr_request(w->hypr_sig, req, ans, sizeof(ans));
    w->hypr_sent = w->hypr_want;
    if (r < 0 || strncmp(ans, "ok", 2) != 0) {
        /* Not fatal and not retried: an older or newer Hyprland that spells
         * it differently still honours a window rule. */
        w->hypr_failed = true;
        nb_log("hyprland: could not set `immediate` on this window (%s); "
               "add `windowrulev2 = immediate, class:^(conduit-viewer)$` to "
               "the Hyprland config for tearing",
               r < 0 ? strerror(-r) : ans);
        return;
    }
    nb_log("hyprland: `immediate` %s on this window (%s)",
           w->hypr_want ? "ON" : "off",
           w->hypr_want ? "tears when fullscreen and alone on the output; "
                          "needs general:allow_tearing = true"
                        : "presented at vblank");
}

/*
 * PRESENTATION SYNC.  Immediate (tearing) or at vblank, per --sync / the
 * menu (nb_sync_*), fullscreen state and direct mode.  Applied as:
 *   wp_tearing_control_v1  ASYNC / VSYNC on the main and picture surfaces
 *   wp_content_type_v1     "game" while immediate
 *   Hyprland               the window's `immediate` property
 * All of it is double-buffered surface state that takes effect with the
 * next content commit, which in a running guest is the next frame.
 */
static bool wl_async_wanted(const struct nb_wl *w)
{
    if (w->vst.sync == NB_SYNC_VSYNC || w->vst.sync == NB_SYNC_LATCH) {
        return false;           /* latch aims at vblanks: never tear */
    }
    return w->direct || nb_sync_immediate(w->vst.sync, w->fullscreen);
}

static void wl_hint_surface(struct nb_wl *w, struct wl_surface *sf,
                            struct wp_tearing_control_v1 **tc,
                            struct wp_content_type_v1 **ct, bool async)
{
    if (!sf) {
        return;
    }
    if (w->tearing_mgr) {
        if (!*tc) {
            *tc = wp_tearing_control_manager_v1_get_tearing_control(
                w->tearing_mgr, sf);
        }
        if (*tc) {
            wp_tearing_control_v1_set_presentation_hint(
                *tc, async ? WP_TEARING_CONTROL_V1_PRESENTATION_HINT_ASYNC
                           : WP_TEARING_CONTROL_V1_PRESENTATION_HINT_VSYNC);
        }
    }
    if (w->ctype_mgr) {
        if (!*ct) {
            *ct = wp_content_type_manager_v1_get_surface_content_type(
                w->ctype_mgr, sf);
        }
        if (*ct) {
            wp_content_type_v1_set_content_type(
                *ct, async ? WP_CONTENT_TYPE_V1_TYPE_GAME
                           : WP_CONTENT_TYPE_V1_TYPE_NONE);
        }
    }
}

static void wl_tearing_apply(struct nb_wl *w)
{
    bool async = wl_async_wanted(w);

    wl_hint_surface(w, w->surf, &w->tearing, &w->ctype, async);
    wl_hint_surface(w, w->cs_surf, &w->cs_tearing, &w->cs_ctype, async);
    w->tearing_async = async && w->tearing_mgr;
    if (w->hypr_sig) {
        w->hypr_want = async ? 1 : 0;
        wl_hypr_sync(w);
    }
}

/* A sync-relevant state changed (fullscreen, direct mode, the setting):
 * re-apply, and latch it on the surfaces now rather than with the next
 * frame, which an idle guest may never send. */
static void wl_sync_changed(struct nb_wl *w, const char *why)
{
    bool was = w->tearing_async;

    wl_tearing_apply(w);
    if (w->configured) {
        if (w->in_sub && w->cs_surf) {
            wl_surf_commit(w, w->cs_surf);
        }
        wl_surf_commit(w, w->surf);
        wl_display_flush(w->dpy);
    }
    if (was != w->tearing_async || why) {
        nb_log("presentation: %s (%s, sync=%s%s)",
               w->tearing_async ? "IMMEDIATE (tearing allowed)" : "vsync",
               w->fullscreen ? "fullscreen" : "windowed",
               nb_sync_name(w->vst.sync), w->direct ? ", direct mode" : "");
    }
}

static void wl_direct_set(struct nb_wl *w, bool on)
{
    w->direct = on;
    wl_sync_changed(w, NULL);
    ov_sync(w);
    /* Fullscreen is where 1:1 matters; ask again so a guest that drifted is
     * put back on the output's exact mode. */
    w->mh_valid = false;
    wl_mode_hint(w, true);
    nb_log("direct mode %s: overlay %s, tearing %s%s", on ? "ON" : "off",
           ov_want(w) ? "shown" : "hidden",
           w->tearing_async ? "ASYNC" : "VSYNC",
           on && !w->fullscreen ? " (direct scanout needs fullscreen: "
                                  "CTRL+ALT+F)" : "");
    nb_run_hook(w->direct_hook, on ? "on" : "off");
    w->title_next_ms = 0;
    wl_display_flush(w->dpy);
}

/* CMD_MODES completed a new list: the open menu shows it. */
static void wl_modes(struct nb_session *s)
{
    struct nb_wl *w = s->priv;

    w->modes_dirty = true;
}

static bool wl_hotkey(struct nb_session *s, unsigned code)
{
    struct nb_wl *w = s->priv;

    switch (code) {
    case KEY_O:
        w->ov_user = !w->ov_user;
        /* Asked for in fullscreen: show it there too (it costs the tearing
         * and direct scanout that need the picture alone on the output). */
        if (w->ov_user && w->fullscreen && w->ov_mode == NB_OVERLAY_WINDOWED) {
            w->ov_mode = NB_OVERLAY_ALWAYS;
        }
        ov_sync(w);
        if (!w->ov_user || w->ov_mapped) {
            return true;
        }
        nb_log("overlay: wanted, but hidden by %s",
               w->direct ? "direct mode (CTRL+ALT+D)"
                         : "--overlay=windowed in fullscreen");
        return true;
    case KEY_D:
        wl_direct_set(w, !w->direct);
        return true;
    case KEY_M:
        wl_menu_set(w, !w->ui.open);
        return true;
    default: {
        char label[64];
        int ch = nb_view_hotkey(&w->vst, code, w->sink && w->sink->shift_down,
                                w->win_w, w->win_h, w->scale_120, label,
                                sizeof(label));
        unsigned bits = NB_UI_SAVE | NB_UI_NOTICE | NB_UI_REDRAW;

        if (ch < 0) {
            return false;
        }
        if (ch & NB_VIEW_CH_LAYOUT) {
            bits |= NB_UI_VIEW;
        }
        if (ch & NB_VIEW_CH_RES) {
            bits |= NB_UI_RES;
        }
        snprintf(w->ui.notice, sizeof(w->ui.notice), "%s", label);
        nb_log("display: %s", label);
        wl_ui_do(w, bits);
        return true;
    }
    }
}

/* ── display settings and the viewer's own UI ────────────────────────────── */
/*
 * NOTHING HERE IS ON THE FRAME PATH.  The picture stays one buffer on one
 * surface whose viewport does the scaling; a settings change re-places it
 * (a viewport destination, a subsurface position) and that is all.  The
 * menu, the notice and the top-edge button are separate subsurfaces with
 * their own small shm buffers, painted only when their state changes and
 * unmapped when not shown -- so with the menu closed nothing extra exists.
 *
 * Only core protocol is used for the layers (wl_subcompositor, wl_shm), with
 * wp_viewporter -- already required for scaling -- to give them crisp
 * physical-pixel buffers at a fractional output scale.
 */
static const struct nb_ui_env *wl_ui_env(struct nb_wl *w)
{
    static struct nb_ui_env e;

    e.win_w = w->win_w > 0 ? w->win_w : 1;
    e.win_h = w->win_h > 0 ? w->win_h : 1;
    e.s120 = w->viewporter && w->scale_120 ? w->scale_120 : 120;
    e.buf_w = w->current >= 0 ? w->buf_w : 0;
    e.buf_h = w->current >= 0 ? w->buf_h : 0;
    e.stats_on = w->ov_user;
    e.fullscreen = w->fullscreen;
    e.immediate = w->tearing_async;
    /* A compositor chooses its own filter; the fullscreen scaler is ours. */
    e.nearest_ok = w->gs_shown;
    e.vm_actions = w->cfg && (w->cfg->vm_shutdown_cmd || w->cfg->vm_reboot_cmd);
    e.vm_pause = w->cfg && w->cfg->vm_pause_cmd && w->cfg->vm_resume_cmd;
    e.vm_paused = w->vmp.paused;
    e.vm_pause_busy = nb_vmpause_busy(&w->vmp);
    e.translucent = true;
    e.modes = &nb_mode_rx.cur;
    return &e;
}

static void ly_release(void *data, struct wl_buffer *b)
{
    struct nb_wl_layer *L = data;

    for (int i = 0; i < 2; i++) {
        if (L->buf[i] == b) {
            L->busy[i] = false;
        }
    }
}
static const struct wl_buffer_listener ly_buf_listener = {
    .release = ly_release,
};

static struct nb_wl_layer *wl_layer_new(struct nb_wl *w, bool input)
{
    struct nb_wl_layer *L;

    if (!w->comp || !w->subcomp || !w->shm) {
        return NULL;
    }
    L = calloc(1, sizeof(*L));
    if (!L) {
        return NULL;
    }
    L->input = input;
    L->surf = wl_compositor_create_surface(w->comp);
    L->sub = L->surf ? wl_subcompositor_get_subsurface(w->subcomp, L->surf,
                                                       w->surf) : NULL;
    if (!L->sub) {
        if (L->surf) {
            wl_surface_destroy(L->surf);
        }
        free(L);
        return NULL;
    }
    wl_subsurface_set_desync(L->sub);   /* on top: made last */
    if (w->viewporter) {
        L->vp = wp_viewporter_get_viewport(w->viewporter, L->surf);
    }
    if (!input) {
        struct wl_region *none = wl_compositor_create_region(w->comp);

        if (none) {
            wl_surface_set_input_region(L->surf, none);
            wl_region_destroy(none);
        }
    }
    L->x = L->y = INT32_MIN;
    return L;
}

static void wl_layer_drop_buffers(struct nb_wl_layer *L)
{
    for (int i = 0; i < 2; i++) {
        if (L->buf[i]) {
            wl_buffer_destroy(L->buf[i]);
            L->buf[i] = NULL;
        }
        L->busy[i] = false;
    }
    if (L->px) {
        munmap(L->px, L->sz);
        L->px = NULL;
        L->sz = 0;
    }
    L->pw = L->ph = 0;
}

/* A free buffer of pw x ph to paint into; NULL when both are on screen. */
static uint32_t *wl_layer_begin(struct nb_wl *w, struct nb_wl_layer *L,
                                int pw, int ph, int *idx)
{
    if (pw != L->pw || ph != L->ph) {
        struct wl_shm_pool *pool;
        size_t one = (size_t)pw * (size_t)ph * 4;
        void *px;
        int fd;

        if (pw <= 0 || ph <= 0 || pw > 8192 || ph > 8192) {
            return NULL;
        }
        if (L->busy[0] || L->busy[1]) {
            /* Never unmap memory the compositor may still read. */
            return NULL;
        }
        wl_layer_drop_buffers(L);
        fd = memfd_create("conduit-viewer-ui", MFD_CLOEXEC);
        if (fd < 0) {
            return NULL;
        }
        if (ftruncate(fd, (off_t)(one * 2)) < 0) {
            close(fd);
            return NULL;
        }
        px = mmap(NULL, one * 2, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (px == MAP_FAILED) {
            close(fd);
            return NULL;
        }
        pool = wl_shm_create_pool(w->shm, fd, (int32_t)(one * 2));
        close(fd);
        if (!pool) {
            munmap(px, one * 2);
            return NULL;
        }
        for (int i = 0; i < 2; i++) {
            L->buf[i] = wl_shm_pool_create_buffer(pool, (int32_t)(one * i),
                                                  pw, ph, pw * 4,
                                                  WL_SHM_FORMAT_ARGB8888);
            if (L->buf[i]) {
                wl_buffer_add_listener(L->buf[i], &ly_buf_listener, L);
            }
        }
        wl_shm_pool_destroy(pool);
        L->px = px;
        L->sz = one * 2;
        L->pw = pw;
        L->ph = ph;
        if (!L->buf[0] || !L->buf[1]) {
            wl_layer_drop_buffers(L);
            return NULL;
        }
    }
    *idx = !L->busy[0] ? 0 : !L->busy[1] ? 1 : -1;
    if (*idx < 0) {
        return NULL;
    }
    return (uint32_t *)((char *)L->px + (size_t)*idx * (size_t)L->pw *
                        (size_t)L->ph * 4);
}

static void wl_layer_show(struct nb_wl *w, struct nb_wl_layer *L, int idx,
                          int x, int y, int lw, int lh)
{
    wl_surface_attach(L->surf, L->buf[idx], 0, 0);
    wl_surface_damage_buffer(L->surf, 0, 0, L->pw, L->ph);
    if (L->vp) {
        wp_viewport_set_destination(L->vp, lw, lh);
    }
    if (L->input && (lw != L->lw || lh != L->lh || !L->mapped)) {
        struct wl_region *r = wl_compositor_create_region(w->comp);

        if (r) {
            wl_region_add(r, 0, 0, lw, lh);
            wl_surface_set_input_region(L->surf, r);
            wl_region_destroy(r);
        }
    }
    L->lw = lw;
    L->lh = lh;
    wl_surface_commit(L->surf);
    L->busy[idx] = true;
    if (x != L->x || y != L->y || !L->mapped) {
        L->x = x;
        L->y = y;
        wl_subsurface_set_position(L->sub, x, y);
        wl_surf_commit(w, w->surf);     /* position is the parent's state */
    }
    L->mapped = true;
}

static void wl_layer_hide(struct nb_wl *w, struct nb_wl_layer *L)
{
    (void)w;
    if (!L || !L->mapped) {
        return;
    }
    wl_surface_attach(L->surf, NULL, 0, 0);
    wl_surface_commit(L->surf);
    L->mapped = false;
}

/* ── the notice ── */
static void wl_note_paint(struct nb_wl *w)
{
    const struct nb_ui_env *e = wl_ui_env(w);
    int lw, lh, pw, ph, idx;
    uint32_t *px;

    if (!w->ly_note) {
        w->ly_note = wl_layer_new(w, false);
        if (!w->ly_note) {
            return;
        }
    }
    nb_ui_notice_size(w->note_txt, &lw, &lh);
    pw = nb_ui_phys(lw, e->s120);
    ph = nb_ui_phys(lh, e->s120);
    px = wl_layer_begin(w, w->ly_note, pw, ph, &idx);
    if (!px) {
        return;
    }
    nb_ui_paint_notice(w->note_txt, e->s120, px, pw, ph, pw, false);
    wl_layer_show(w, w->ly_note, idx, (e->win_w - lw) / 2,
                  e->win_h > lh + 48 ? 24 : 0, lw, lh);
}

static void wl_note_show(struct nb_wl *w, const char *txt)
{
    snprintf(w->note_txt, sizeof(w->note_txt), "%s", txt);
    w->note_until_ms = nb_now_ms_wl() + 1600;
    wl_note_paint(w);
}

/* ── the menu ── */
static void wl_menu_paint(struct nb_wl *w)
{
    const struct nb_ui_env *e = wl_ui_env(w);
    struct nb_rect r;
    int lw, lh, pw, ph, idx;
    uint32_t *px;

    if (!w->ui.open) {
        return;
    }
    if (!w->ly_menu) {
        wl_ed_build(w);             /* below the menu: made first */
        w->ly_menu = wl_layer_new(w, true);
        if (!w->ly_menu) {
            return;
        }
    }
    nb_ui_layout(&w->ui, e, &lw, &lh);
    nb_ui_panel_rect(&w->ui, e, &r);
    pw = nb_ui_phys(r.w, e->s120);
    ph = nb_ui_phys(r.h, e->s120);
    px = wl_layer_begin(w, w->ly_menu, pw, ph, &idx);
    if (!px) {
        return;
    }
    nb_ui_paint_panel(&w->ui, e, px, pw, ph, pw);
    wl_layer_show(w, w->ly_menu, idx, r.x, r.y, r.w, r.h);
}

/*
 * The outline's eight surfaces, made once.  Before the menu's layer, which
 * must stack above them (SUBSURFACE STACKING: creation order).
 */
static void wl_ed_build(struct nb_wl *w)
{
    int i;

    if (!w->viewporter || !w->comp || !w->subcomp || !w->shm) {
        return;
    }
    if (!w->ed_buf) {
        struct wl_shm_pool *pool;
        uint32_t *px;
        int fd = memfd_create("conduit-viewer-edge", MFD_CLOEXEC);

        if (fd < 0 || ftruncate(fd, 4) < 0) {
            if (fd >= 0) {
                close(fd);
            }
            return;
        }
        px = mmap(NULL, 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (px == MAP_FAILED) {
            close(fd);
            return;
        }
        *px = 0xff3fb950u;      /* the Conduit green */
        pool = wl_shm_create_pool(w->shm, fd, 4);
        close(fd);
        if (!pool) {
            munmap(px, 4);
            return;
        }
        w->ed_buf = wl_shm_pool_create_buffer(pool, 0, 1, 1, 4,
                                              WL_SHM_FORMAT_ARGB8888);
        wl_shm_pool_destroy(pool);
        w->ed_px = px;
        for (i = 0; i < 8 && w->ed_buf; i++) {
            struct wl_region *none;

            w->ed_surf[i] = wl_compositor_create_surface(w->comp);
            w->ed_sub[i] = wl_subcompositor_get_subsurface(w->subcomp,
                                                           w->ed_surf[i],
                                                           w->surf);
            wl_subsurface_set_desync(w->ed_sub[i]);
            w->ed_vp[i] = wp_viewporter_get_viewport(w->viewporter,
                                                     w->ed_surf[i]);
            none = wl_compositor_create_region(w->comp);
            if (none) {
                wl_surface_set_input_region(w->ed_surf[i], none);
                wl_region_destroy(none);
            }
        }
    }
}

/* The output area's outline while the menu is open. */
static void wl_ed_show(struct nb_wl *w, bool on)
{
    struct nb_rect a;
    int i;

    if (on) {
        wl_ed_build(w);
    }
    if (!w->ed_buf) {
        return;
    }
    if (!on) {
        if (w->ed_mapped) {
            for (i = 0; i < 8; i++) {
                wl_surface_attach(w->ed_surf[i], NULL, 0, 0);
                wl_surface_commit(w->ed_surf[i]);
            }
            w->ed_mapped = false;
        }
        return;
    }
    a = w->place.area;
    if (w->ed_mapped && !memcmp(&a, &w->ed_at, sizeof(a))) {
        return;
    }
    {
        const int t = 2, c = 12;
        const int r[8][4] = {
            { a.x, a.y, a.w, t }, { a.x, a.y + a.h - t, a.w, t },
            { a.x, a.y, t, a.h }, { a.x + a.w - t, a.y, t, a.h },
            { a.x, a.y, c, c / 2 }, { a.x + a.w - c, a.y, c, c / 2 },
            { a.x, a.y + a.h - c / 2, c, c / 2 },
            { a.x + a.w - c, a.y + a.h - c / 2, c, c / 2 },
        };

        for (i = 0; i < 8; i++) {
            wl_subsurface_set_position(w->ed_sub[i], r[i][0], r[i][1]);
            wp_viewport_set_destination(w->ed_vp[i], r[i][2] > 0 ? r[i][2] : 1,
                                        r[i][3] > 0 ? r[i][3] : 1);
            wl_surface_attach(w->ed_surf[i], w->ed_buf, 0, 0);
            wl_surface_damage_buffer(w->ed_surf[i], 0, 0, 1, 1);
            wl_surface_commit(w->ed_surf[i]);
        }
    }
    w->ed_at = a;
    w->ed_mapped = true;
    wl_surf_commit(w, w->surf);
}

/* ── the top-edge button ── */
static void wl_btn_paint(struct nb_wl *w)
{
    const struct nb_ui_env *e = wl_ui_env(w);
    int lw, lh, pw, ph, idx;
    uint32_t *px;

    if (w->btn_alpha <= 0.0) {
        wl_layer_hide(w, w->ly_btn);
        return;
    }
    if (!w->ly_btn) {
        w->ly_btn = wl_layer_new(w, true);
        if (!w->ly_btn) {
            return;
        }
    }
    nb_ui_button_size(&lw, &lh);
    pw = nb_ui_phys(lw, e->s120);
    ph = nb_ui_phys(lh, e->s120);
    px = wl_layer_begin(w, w->ly_btn, pw, ph, &idx);
    if (!px) {
        return;
    }
    nb_ui_paint_button(e->s120, px, pw, ph, pw, w->btn_alpha, w->btn_hot,
                       false);
    wl_layer_show(w, w->ly_btn, idx, (e->win_w - lw) / 2, 4, lw, lh);
}

static void wl_btn_track(struct nb_wl *w, double wy)
{
    uint64_t now = nb_now_ms_wl();
    bool want = !w->ui.open && !w->grabbed && wy < NB_UI_BUTTON_ZONE;

    if (want) {
        w->btn_seen_ms = now;
        if (!w->btn_want) {
            w->btn_want = true;
            w->btn_last_ms = 0;
        }
    }
}

static void wl_ui_layer_motion(struct nb_wl *w, double lx, double ly)
{
    if (w->ptr_on_menu) {
        wl_ui_do(w, nb_ui_panel_motion(&w->ui, wl_ui_env(w), lx, ly));
    } else if (w->ptr_on_btn) {
        w->btn_seen_ms = nb_now_ms_wl();
        if (!w->btn_hot) {
            w->btn_hot = true;
            wl_btn_paint(w);
            wl_display_flush(w->dpy);
        }
    }
}

/* The clock half: the notice's timeout and the button's fade. */
static int wl_tick_ui(struct nb_wl *w, int next)
{
    uint64_t now = nb_now_ms_wl();
    bool flush = false;

    if (w->rule_due) {
        w->rule_due = false;
        w->modes_dirty = false;
        snprintf(w->ui.notice, sizeof(w->ui.notice), "%s", w->rule_note);
        wl_ui_do(w, NB_UI_VIEW | NB_UI_REDRAW |
                        (w->rule_note[0] ? NB_UI_NOTICE : 0));
        flush = true;
    } else if (w->modes_dirty) {
        w->modes_dirty = false;
        if (w->ui.open) {
            wl_ui_do(w, NB_UI_REDRAW);
            flush = true;
        }
    }

    /* The menu's Pause / Resume command, reaped when it ends. */
    if (nb_vmpause_busy(&w->vmp)) {
        if (nb_vm_pause_reap(&w->vmp, w->sink, w->ui.notice,
                             sizeof(w->ui.notice))) {
            if (w->ui.open) {
                wl_ui_do(w, NB_UI_REDRAW | NB_UI_NOTICE);
            } else {
                wl_note_show(w, w->ui.notice);
            }
            wl_title_update(w, true);
            flush = true;
        } else {
            next = wl_tick_min(next, now + NB_VMPAUSE_POLL_MS, now);
        }
    }

    if (w->rehint_due_ms) {
        if (now >= w->rehint_due_ms) {
            w->rehint_due_ms = 0;
            wl_rehint(w);
            flush = true;
        } else {
            next = wl_tick_min(next, w->rehint_due_ms, now);
        }
    }
    if (w->note_until_ms) {
        if (now >= w->note_until_ms) {
            w->note_until_ms = 0;
            wl_layer_hide(w, w->ly_note);
            flush = true;
        } else {
            next = wl_tick_min(next, w->note_until_ms, now);
        }
    }
    /* The button stays while the pointer is near it, then fades. */
    if (w->btn_want && !w->ptr_on_btn && now - w->btn_seen_ms > 900) {
        w->btn_want = false;
        w->btn_last_ms = 0;
    }
    if (w->btn_want && (w->ui.open || w->grabbed)) {
        w->btn_want = false;
    }
    {
        double target = w->btn_want ? 1.0 : 0.0;

        if (w->btn_alpha != target) {
            double dt = w->btn_last_ms ? (double)(now - w->btn_last_ms) : 16;
            double step = dt / 160.0;

            w->btn_alpha += target > w->btn_alpha ? step : -step;
            if ((target > 0 && w->btn_alpha > 1.0) ||
                (target == 0 && w->btn_alpha < 0.0)) {
                w->btn_alpha = target;
            }
            w->btn_last_ms = now;
            wl_btn_paint(w);
            flush = true;
            if (w->btn_alpha != target) {
                next = wl_tick_min(next, now + 16, now);
            }
        } else {
            w->btn_last_ms = now;
        }
        if (w->btn_want && !w->ptr_on_btn) {
            next = wl_tick_min(next, w->btn_seen_ms + 901, now);
        }
    }
    if (flush) {
        wl_display_flush(w->dpy);
    }
    return next;
}

/* The pointer, mapped into the guest through the rectangle on screen. */
static void wl_ptr_to_guest(struct nb_wl *w)
{
    int bw = w->current >= 0 && w->buf_w > 0 ? w->buf_w : w->win_w;
    int bh = w->current >= 0 && w->buf_h > 0 ? w->buf_h : w->win_h;
    int gx, gy;

    if (!w->sink || bw <= 0 || bh <= 0) {
        return;
    }
    if (w->current < 0) {
        nb_view_place(&w->vst.cur, bw, bh, w->win_w, w->win_h, w->scale_120,
                      &w->place);
    }
    nb_view_map(&w->place, bw, bh, w->ptr_wx, w->ptr_wy, &gx, &gy);
    nb_sink_abs(w->sink, gx, gy, (unsigned)bw, (unsigned)bh);
}

/*
 * The picture moved or changed size (a new guest mode, scale or area) while
 * the pointer stayed where it is: the same window point is another guest
 * pixel now, and the guest may have moved its pointer itself -- a compositor
 * puts it back at the centre of an output that was replugged for a new mode.
 * Without this the cursor is drawn under the host pointer while a click lands
 * wherever the guest left it, until the mouse moves.
 */
static void wl_ptr_follow(struct nb_wl *w)
{
    int gx, gy;

    if (w->place.pic.x == w->ptr_pic.x && w->place.pic.y == w->ptr_pic.y &&
        w->place.pic.w == w->ptr_pic.w && w->place.pic.h == w->ptr_pic.h &&
        w->buf_w == w->ptr_bw && w->buf_h == w->ptr_bh) {
        return;
    }
    w->ptr_pic = w->place.pic;
    w->ptr_bw = w->buf_w;
    w->ptr_bh = w->buf_h;
    if (!w->sink || !w->ptr_on_content || w->ui.open || w->current < 0 ||
        w->buf_w <= 0 || w->buf_h <= 0) {
        return;
    }
    nb_view_map(&w->place, w->buf_w, w->buf_h, w->ptr_wx, w->ptr_wy, &gx, &gy);
    nb_sink_abs_resync(w->sink, gx, gy, (unsigned)w->buf_w,
                       (unsigned)w->buf_h);
}

/* Re-place the picture after a settings change: a viewport and a position. */
static void wl_relayout(struct nb_wl *w)
{
    if (w->viewport && w->current >= 0) {
        int dw, dh;
        struct wl_surface *pic = wl_viewport_apply(w, w->buf_w, w->buf_h,
                                                   NULL, false, &dw, &dh);

        wl_surface_damage_buffer(pic, 0, 0,
                                 w->gs_frame ? INT32_MAX : w->buf_w,
                                 w->gs_frame ? INT32_MAX : w->buf_h);
        wl_surf_commit(w, pic);
        wl_main_settle(w);
        if (w->gc_on && gc_layout(w, false)) {
            cur_apply(w, w->last_serial);
        }
        spr_update(w);
        spr_kick_now(w);
        w->surf_w = dw;
        w->surf_h = dh;
        wl_ptr_follow(w);
    }
    bd_layout(w);
    if (w->ui.open) {
        wl_ed_show(w, true);
    }
}

/* Ask the guest again: the resolution, or the area a native one follows. */
static void wl_rehint(struct nb_wl *w)
{
    nb_view_apply_res(&w->vst.cur);
    w->hint_w = w->hint_h = 0;
    if (w->cont_w > 0 && w->cont_h > 0) {
        wl_report_surface(w, w->cont_w, w->cont_h);
    }
    w->mh_valid = false;
    wl_mode_hint(w, true);
}

static void wl_menu_set(struct nb_wl *w, bool open)
{
    if (open == w->ui.open) {
        return;
    }
    if (open) {
        /* The guest stops getting input: release what it holds, and give
         * the pointer back to the host. */
        if (w->sink) {
            if (w->grabbed) {
                nb_sink_force_ungrab(w->sink);
            }
            nb_sink_release_keys(w->sink);
        }
        w->btn_want = false;
        w->btn_alpha = 0;
        wl_layer_hide(w, w->ly_btn);
        nb_log("menu: open");
    } else {
        nb_log("menu: closed");
    }
    wl_ui_do(w, nb_ui_set_open(&w->ui, wl_ui_env(w), open));
    if (open) {
        /* The host pointer, at once and wherever it is: the menu is used
         * with it, and a still pointer gets no motion to bring it back. */
        cur_apply(w, w->last_serial);
    }
}

static void wl_ui_do(struct nb_wl *w, unsigned bits)
{
    /*
     * Remembered for the guest mode in force (nb_vstore_auto_rule), once an
     * area drag is over (its steps are not each written to the file).
     */
    if ((bits & (NB_UI_VIEW | NB_UI_SAVE)) && w->ui.drag < 0 &&
        nb_vstore_auto_rule(&w->vst)) {
        bits |= NB_UI_SAVE;
    }
    if (bits & NB_UI_VIEW) {
        wl_relayout(w);
    }
    /*
     * A new guest mode is a re-mode in the guest, so it is debounced: held
     * while an area drag is in progress, then sent once things settle.
     */
    if (bits & NB_UI_RES) {
        w->rehint_pending = true;
    }
    if (w->rehint_pending && w->ui.drag < 0) {
        w->rehint_pending = false;
        w->rehint_due_ms = nb_now_ms_wl() + 250;
    }
    if (bits & NB_UI_SAVE) {
        nb_vstore_persist(&w->vst, w->cfg);
    }
    if (bits & (NB_UI_MODE_ADD | NB_UI_MODE_DEL)) {
        nb_sink_mode_edit(w->sink, w->ui.mode_w, w->ui.mode_h,
                          (bits & NB_UI_MODE_ADD) != 0);
    }
    if (bits & NB_UI_STATS) {
        w->ov_user = !w->ov_user;
        if (w->ov_user && w->fullscreen &&
            w->ov_mode == NB_OVERLAY_WINDOWED) {
            w->ov_mode = NB_OVERLAY_ALWAYS;
        }
        ov_sync(w);
        bits |= NB_UI_REDRAW;
    }
    if (bits & NB_UI_SYNC) {
        wl_sync_changed(w, "menu");
        wl_relayout(w);         /* the scaler follows the sync setting */
    }
    if ((bits & NB_UI_FULLSCREEN) && w->sink) {
        nb_sink_toggle_fullscreen(w->sink);
    }
    if ((bits & NB_UI_VM_SHUTDOWN) && w->cfg) {
        nb_run_detached(w->cfg->vm_shutdown_cmd);
    }
    if ((bits & NB_UI_VM_REBOOT) && w->cfg) {
        nb_run_detached(w->cfg->vm_reboot_cmd);
    }
    if ((bits & (NB_UI_VM_PAUSE | NB_UI_VM_RESUME)) &&
        !nb_vm_pause_ask(&w->vmp, w->sink, w->cfg, (bits & NB_UI_VM_PAUSE) != 0,
                         w->ui.notice, sizeof(w->ui.notice))) {
        bits |= NB_UI_NOTICE;
    }
    if (bits & NB_UI_CLOSE || !w->ui.open) {
        wl_layer_hide(w, w->ly_menu);
        wl_ed_show(w, false);
        if (w->ptr_on_content || w->ui_cur >= 0) {
            w->ui_cur = -1;
            cur_apply(w, w->last_serial);
            if (w->ptr_on_content) {
                wl_ptr_to_guest(w);
            }
        }
    } else {
        if (bits & (NB_UI_REDRAW | NB_UI_VIEW)) {
            wl_menu_paint(w);
        }
        wl_ed_show(w, true);
        if ((bits & NB_UI_CURSOR) && !w->ptr_on_menu) {
            cur_apply(w, w->last_serial);
        }
    }
    if (bits & NB_UI_NOTICE) {
        wl_note_show(w, w->ui.notice);
    }
    if (w->ov_mapped && (bits & (NB_UI_VIEW | NB_UI_RES))) {
        ov_paint_commit(w);
    }
    wl_display_flush(w->dpy);
}

/* The window changed size or scale: move what is shown with it. */
static void wl_ui_reflow(struct nb_wl *w)
{
    static bool opened;

    if (!opened && w->cfg && w->cfg->open_menu && w->win_w > 0) {
        opened = true;
        wl_menu_set(w, true);
    }
    if (w->ui.open) {
        wl_menu_paint(w);
        wl_ed_show(w, true);
    }
    if (w->note_until_ms) {
        wl_note_paint(w);
    }
    if (w->ly_btn && w->ly_btn->mapped) {
        wl_btn_paint(w);
    }
}

/* ── virtio-nvgpu: the guest's cursor as the host pointer ────────────────── */
/*
 * CMD_CURSOR hands us the guest cursor plane's dma-buf.  It becomes a
 * wl_buffer on a cursor surface of our own, which wl_pointer.set_cursor puts
 * under the HOST pointer while that pointer is over the guest's picture -- so
 * the cursor moves at the compositor's speed and the guest is never in the
 * loop for a move.
 *
 * Imported with the ASYNCHRONOUS create (an import refusal is then an event,
 * not a fatal protocol error) and cached by dma-buf inode: a guest compositor
 * cycles a few cursor buffers, and a cursor changes rarely.  Drawn at the
 * picture's own scale (guest pixel -> window pixel) through a viewport, so it
 * is the size the guest meant relative to its desktop.
 */
struct nb_gc_req {
    struct nb_wl *w;
    uint32_t gen;
    uint64_t id;
    uint32_t cw, ch, stride, offset, fourcc;
    uint64_t mod;
    unsigned hx, hy;
    int fd;                     /* our dup, for the copy if the import fails */
};

/* Lay the current image out at the picture's scale.  Returns true when the
 * logical hotspot changed, i.e. set_cursor has to be repeated. */
static bool gc_layout(struct nb_wl *w, bool force)
{
    double kx = 1.0, ky = 1.0;
    int k1000, lw, lh, hx, hy;

    if (w->gc_cur < 0 || !w->gc_surf || !w->gc[w->gc_cur].valid) {
        return false;
    }
    /* The picture's own scale, per axis (stretch differs between them), so
     * the cursor is drawn by the same transform as the frame. */
    if (w->buf_w > 0 && w->buf_h > 0) {
        nb_view_cursor_scale(&w->place, w->buf_w, w->buf_h, &kx, &ky);
    }
    k1000 = (int)(kx * 1000.0 + 0.5) * 65536 + (int)(ky * 1000.0 + 0.5);
    if (!force && k1000 == w->gc_k1000) {
        return false;
    }
    w->gc_k1000 = k1000;
    lw = (int)(w->gc[w->gc_cur].w * kx + 0.5);
    lh = (int)(w->gc[w->gc_cur].h * ky + 0.5);
    hx = (int)(w->gc_hx * kx + 0.5);
    hy = (int)(w->gc_hy * ky + 0.5);
    if (lw < 1) { lw = 1; }
    if (lh < 1) { lh = 1; }
    wl_surface_attach(w->gc_surf, w->gc[w->gc_cur].buf, 0, 0);
    if (w->gc_vp) {
        if (k1000 == 1000 * 65536 + 1000) {
            wp_viewport_set_destination(w->gc_vp, -1, -1);
        } else {
            wp_viewport_set_destination(w->gc_vp, lw, lh);
        }
    }
    wl_surface_damage_buffer(w->gc_surf, 0, 0, (int32_t)w->gc[w->gc_cur].w,
                             (int32_t)w->gc[w->gc_cur].h);
    wl_surface_commit(w->gc_surf);
    if (force || hx != w->gc_lhx || hy != w->gc_lhy) {
        w->gc_lhx = hx;
        w->gc_lhy = hy;
        return true;
    }
    return force;
}

static void gc_show(struct nb_wl *w, int slot, unsigned hx, unsigned hy)
{
    w->gc_cur = slot;
    w->gc_hx = hx;
    w->gc_hy = hy;
    w->gc_on = true;
    w->gc_broken = false;
    w->gc[slot].used = ++w->gc_tick;
    gc_layout(w, true);
    cur_apply(w, w->last_serial);
    spr_kick_now(w);
    wl_display_flush(w->dpy);
    wl_nudge(w);                /* a new cursor image needs a repaint too */
}

/* The cache slot a new image goes to: a free one, else the one shown
 * longest ago, never the one on screen.  -1: none. */
static int gc_slot(struct nb_wl *w)
{
    int i, victim = -1;
    uint64_t oldest = UINT64_MAX;

    for (i = 0; i < 4; i++) {
        if (i == w->gc_cur) {
            continue;           /* never the one on screen */
        }
        if (!w->gc[i].valid) {
            return i;
        }
        if (w->gc[i].used < oldest) {
            oldest = w->gc[i].used;
            victim = i;
        }
    }
    return victim;
}

/* The GPU that copies cursor images, opened the first time one is needed. */
static bool gc_gs_ready(struct nb_wl *w)
{
    const char *node = w->cfg ? w->cfg->render_node : NULL;
    char why[200] = "";

    if (w->gc_gs || w->gc_gs_failed) {
        return w->gc_gs != NULL;
    }
    w->gc_gs = nb_gs_open(node, 0, NB_FOURCC_AR24_LOCAL, 0, why, sizeof(why));
    if (!w->gc_gs && node) {
        w->gc_gs = nb_gs_open(NULL, 0, NB_FOURCC_AR24_LOCAL, 0, why,
                              sizeof(why));
    }
    if (w->gc_gs && !nb_gs_can_import(w->gc_gs, NB_FOURCC_AR24_LOCAL, 0)) {
        snprintf(why, sizeof(why), "%s imports no LINEAR ARGB8888",
                 nb_gs_node(w->gc_gs));
        nb_gs_close(w->gc_gs);
        w->gc_gs = NULL;
    }
    if (!w->gc_gs) {
        w->gc_gs_failed = true;
        nb_err("guest cursor: no GPU here copies the guest's cursor image "
               "(%s)", why[0] ? why : "no render node");
        return false;
    }
    nb_log("guest cursor: copied by %s into shared memory when its shape "
           "changes (the display takes no LINEAR ARGB8888 dma-buf); moves "
           "are the host pointer's", nb_gs_node(w->gc_gs));
    return true;
}

/*
 * The display does not take the cursor's dma-buf: draw it 1:1, alpha kept,
 * with the viewer's GPU and read it back into a wl_shm buffer -- the one
 * kind every compositor takes.  Only a new shape comes here (a cursor is at
 * most 256x256); a move is the host pointer's and copies nothing.  The image
 * is copied every time, never matched in the cache: a guest may redraw a
 * cursor in place.
 */
static int gc_copy(struct nb_wl *w, int fd, uint32_t cw, uint32_t ch,
                   uint32_t stride, uint32_t offset, uint32_t fourcc,
                   uint64_t mod, unsigned hx, unsigned hy)
{
    struct nb_gs_draw dr = {
        .src_key = 0, .target = 0, .dw = (int)cw, .dh = (int)ch,
        .sw = cw, .sh = ch, .nearest = true, .alpha = true,
    };
    struct wl_shm_pool *pool;
    struct wl_buffer *buf;
    char why[200] = "";
    size_t size = (size_t)cw * ch * 4;
    void *px;
    int mfd, slot, r;

    if (!w->shm || !gc_gs_ready(w)) {
        return -ENOTSUP;
    }
    if (cw != w->gc_gs_w || ch != w->gc_gs_h) {
        w->gc_gs_w = w->gc_gs_h = 0;
        if (nb_gs_targets_readback(w->gc_gs, cw, ch, why, sizeof(why)) <= 0) {
            nb_err("guest cursor: %ux%u copy target: %s", cw, ch, why);
            return -EIO;
        }
        w->gc_gs_w = cw;
        w->gc_gs_h = ch;
    }
    nb_gs_src_drop(w->gc_gs, 0);
    r = nb_gs_src(w->gc_gs, 0, fd, cw, ch, stride, offset, fourcc, mod);
    if (r == 0) {
        r = nb_gs_draw(w->gc_gs, &dr, 50000, NULL);
    }
    nb_gs_src_drop(w->gc_gs, 0);
    if (r != 0) {
        nb_err("guest cursor: GPU copy of the %ux%u image failed (%d)", cw, ch,
               r);
        return r;
    }
    slot = gc_slot(w);
    if (slot < 0) {
        return -EBUSY;
    }
    mfd = memfd_create("conduit-viewer-cursor", MFD_CLOEXEC);
    if (mfd < 0) {
        return -errno;
    }
    if (ftruncate(mfd, (off_t)size) < 0) {
        close(mfd);
        return -EIO;
    }
    px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if (px == MAP_FAILED) {
        close(mfd);
        return -ENOMEM;
    }
    r = nb_gs_readback(w->gc_gs, 0, px);
    munmap(px, size);
    if (r != 0) {
        close(mfd);
        return r;
    }
    pool = wl_shm_create_pool(w->shm, mfd, (int32_t)size);
    close(mfd);
    if (!pool) {
        return -ENOMEM;
    }
    buf = wl_shm_pool_create_buffer(pool, 0, (int32_t)cw, (int32_t)ch,
                                    (int32_t)(cw * 4), WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool);
    if (!buf) {
        return -ENOMEM;
    }
    if (w->gc[slot].buf) {
        wl_buffer_destroy(w->gc[slot].buf);
    }
    /* id 0: never a cache hit (a dma-buf's id is its inode). */
    w->gc[slot] = (typeof(w->gc[slot])){
        .valid = true, .used = ++w->gc_tick, .w = cw, .h = ch,
        .stride = cw * 4, .mod = 0, .buf = buf,
    };
    gc_show(w, slot, hx, hy);
    return 0;
}

/* The newest image cannot be shown: the guest still shows a cursor, so the
 * host pointer is the viewer's own arrow until the next image arrives. */
static void gc_broke(struct nb_wl *w)
{
    w->gc_on = true;
    w->gc_broken = true;
    cur_apply(w, w->last_serial);
    wl_display_flush(w->dpy);
}

static void gc_created(void *data, struct zwp_linux_buffer_params_v1 *params,
                       struct wl_buffer *buf)
{
    struct nb_gc_req *r = data;
    struct nb_wl *w = r->w;
    int victim;

    zwp_linux_buffer_params_v1_destroy(params);
    close(r->fd);
    victim = gc_slot(w);
    if (victim < 0) {
        wl_buffer_destroy(buf);
        free(r);
        return;
    }
    if (w->gc[victim].buf) {
        wl_buffer_destroy(w->gc[victim].buf);
    }
    w->gc[victim] = (typeof(w->gc[victim])){
        .valid = true, .id = r->id, .used = ++w->gc_tick,
        .w = r->cw, .h = r->ch, .stride = r->stride, .offset = r->offset,
        .mod = r->mod, .buf = buf,
    };
    /* Only the newest request is shown; an older one that lands late is
     * merely cached. */
    if (r->gen == w->gc_gen) {
        gc_show(w, victim, r->hx, r->hy);
    }
    free(r);
}

static void gc_failed(void *data, struct zwp_linux_buffer_params_v1 *params)
{
    struct nb_gc_req *r = data;
    struct nb_wl *w = r->w;

    zwp_linux_buffer_params_v1_destroy(params);
    /* Advertised but refused: copy from now on (wl_cursor asks), and this
     * image too when it is still the newest. */
    if (!w->gc_told_fail) {
        w->gc_told_fail = true;
        nb_err("cursor: the compositor refused to import the guest's cursor "
               "image (%ux%u AR24 modifier 0x%016llx); copying it instead.  "
               "Said once.", r->cw, r->ch, (unsigned long long)r->mod);
    }
    if (r->gen == w->gc_gen &&
        gc_copy(w, r->fd, r->cw, r->ch, r->stride, r->offset, r->fourcc,
                r->mod, r->hx, r->hy) != 0) {
        nb_err("cursor: the guest's cursor cannot be shown; the viewer's own "
               "arrow stands in for it");
        gc_broke(w);
    }
    close(r->fd);
    free(r);
}

static const struct zwp_linux_buffer_params_v1_listener gc_listener = {
    .created = gc_created,
    .failed  = gc_failed,
};

static int wl_cursor(struct nb_session *s, const struct nb_buf_desc *d,
                     unsigned hx, unsigned hy)
{
    struct nb_wl *w = s->priv;
    struct zwp_linux_buffer_params_v1 *pp;
    struct nb_gc_req *r;
    int i;

    w->gc_gen++;
    if (!d) {
        if (w->gc_on) {
            w->gc_on = false;
            w->gc_broken = false;
            cur_apply(w, w->last_serial);
            spr_kick_now(w);
            wl_display_flush(w->dpy);
        }
        return 0;
    }
    if (!w->gc_surf) {
        w->gc_surf = wl_compositor_create_surface(w->comp);
        if (!w->gc_surf) {
            return -ENOMEM;
        }
        if (w->viewporter) {
            w->gc_vp = wp_viewporter_get_viewport(w->viewporter, w->gc_surf);
        }
    }
    /* The EXACT pair: a cursor needs its alpha, so no opaque twin.  A
     * display that does not take it (or refused it once) gets a copy. */
    if (w->gc_told_fail || !wl_format_ok(s, d->fourcc, d->modifier)) {
        int rc = gc_copy(w, d->fd, d->width, d->height, d->stride,
                         d->offset, d->fourcc, d->modifier, hx, hy);

        if (rc != 0) {
            gc_broke(w);
        }
        return rc;
    }
    for (i = 0; i < 4; i++) {
        if (w->gc[i].valid && w->gc[i].id == d->id && w->gc[i].w == d->width &&
            w->gc[i].h == d->height && w->gc[i].stride == d->stride &&
            w->gc[i].offset == d->offset && w->gc[i].mod == d->modifier) {
            gc_show(w, i, hx, hy);
            return 0;
        }
    }
    r = calloc(1, sizeof(*r));
    if (!r) {
        return -ENOMEM;
    }
    pp = zwp_linux_dmabuf_v1_create_params(w->dmabuf);
    if (!pp) {
        free(r);
        return -EIO;
    }
    *r = (struct nb_gc_req){
        .w = w, .gen = w->gc_gen, .id = d->id, .cw = d->width,
        .ch = d->height, .stride = d->stride, .offset = d->offset,
        .fourcc = d->fourcc, .mod = d->modifier, .hx = hx, .hy = hy,
        .fd = fcntl(d->fd, F_DUPFD_CLOEXEC, 0),
    };
    if (r->fd < 0) {
        free(r);
        return -errno;
    }
    zwp_linux_buffer_params_v1_add(pp, d->fd, 0, d->offset, d->stride,
                                   (uint32_t)(d->modifier >> 32),
                                   (uint32_t)(d->modifier & 0xffffffffu));
    zwp_linux_buffer_params_v1_add_listener(pp, &gc_listener, r);
    zwp_linux_buffer_params_v1_create(pp, (int32_t)d->width,
                                      (int32_t)d->height, d->fourcc, 0);
    wl_display_flush(w->dpy);
    return 0;
}

/* ── under grab: the guest cursor, drawn by the viewer ───────────────────── */
/*
 * Grabbed, the host pointer is locked (zwp_locked_pointer_v1) and shows no
 * image, and the guest's cursor is in no frame either: a guest with a
 * hardware cursor leaves it to the host.  So the viewer draws it, at the
 * position the guest reports (CMD_CURSOR_POS), on its own subsurface over
 * the picture -- composited by the compositor, nothing copied per frame, and
 * the picture's commits untouched.  Only while the guest shows a cursor: a
 * game that hides it (relative mouse) gets nothing drawn, and the subsurface
 * is unmapped so the picture can be scanned out directly again.
 */

/* The anchor's 1x1 fully transparent buffer. */
static struct wl_buffer *spr_clear_buf(struct nb_wl *w)
{
    struct wl_shm_pool *pool;
    struct wl_buffer *b;
    int fd;

    if (w->spb_mgr) {
        b = wp_single_pixel_buffer_manager_v1_create_u32_rgba_buffer(
            w->spb_mgr, 0, 0, 0, 0);
        if (b) {
            return b;
        }
    }
    if (!w->shm) {
        return NULL;
    }
    fd = memfd_create("conduit-viewer-clear", MFD_CLOEXEC);
    if (fd < 0) {
        return NULL;
    }
    if (ftruncate(fd, 4) < 0) {     /* zero-filled: ARGB 0, transparent */
        close(fd);
        return NULL;
    }
    pool = wl_shm_create_pool(w->shm, fd, 4);
    close(fd);
    if (!pool) {
        return NULL;
    }
    b = wl_shm_pool_create_buffer(pool, 0, 1, 1, 4, WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool);
    return b;
}

static void spr_destroy(struct nb_wl *w)
{
    if (w->spr_vp) {
        wp_viewport_destroy(w->spr_vp);
    }
    if (w->spr_sub) {
        wl_subsurface_destroy(w->spr_sub);
    }
    if (w->spr_surf) {
        wl_surface_destroy(w->spr_surf);
    }
    if (w->spr_anchor_sub) {
        wl_subsurface_destroy(w->spr_anchor_sub);
    }
    if (w->spr_anchor) {
        wl_surface_destroy(w->spr_anchor);
    }
    if (w->spr_anchor_buf) {
        wl_buffer_destroy(w->spr_anchor_buf);
    }
    w->spr_vp = NULL;
    w->spr_sub = w->spr_anchor_sub = NULL;
    w->spr_surf = w->spr_anchor = NULL;
    w->spr_anchor_buf = w->spr_buf = NULL;
    w->spr_mapped = false;
}

/*
 * Made once, right after the picture's own subsurface: stacked above the
 * picture by creation order (SUBSURFACE STACKING), and linked into the main
 * surface by its next commit long before a grab needs it.  Unmapped (no
 * buffer) until then, so it costs nothing.
 */
static bool spr_build(struct nb_wl *w)
{
    struct wl_region *none;

    if (w->spr_surf) {
        return true;
    }
    if (!w->comp || !w->subcomp || !w->viewporter || !w->surf) {
        return false;
    }
    w->spr_anchor = wl_compositor_create_surface(w->comp);
    w->spr_surf = wl_compositor_create_surface(w->comp);
    if (w->spr_anchor) {
        w->spr_anchor_sub = wl_subcompositor_get_subsurface(
            w->subcomp, w->spr_anchor, w->surf);
    }
    if (w->spr_surf && w->spr_anchor) {
        w->spr_sub = wl_subcompositor_get_subsurface(w->subcomp, w->spr_surf,
                                                     w->spr_anchor);
    }
    w->spr_anchor_buf = spr_clear_buf(w);
    if (!w->spr_anchor_sub || !w->spr_sub || !w->spr_anchor_buf) {
        spr_destroy(w);
        return false;
    }
    /* Both apply their own commits: a move is one commit of the anchor. */
    wl_subsurface_set_desync(w->spr_anchor_sub);
    wl_subsurface_set_desync(w->spr_sub);
    w->spr_vp = wp_viewporter_get_viewport(w->viewporter, w->spr_surf);
    /* Pointer events go through to the picture underneath. */
    none = wl_compositor_create_region(w->comp);
    if (none) {
        wl_surface_set_input_region(w->spr_anchor, none);
        wl_surface_set_input_region(w->spr_surf, none);
        wl_region_destroy(none);
    }
    return true;
}

static void spr_update(struct nb_wl *w)
{
    struct wl_buffer *buf;
    struct nb_rect r;
    struct nb_spr_in in = {
        .grabbed = w->grabbed,
        .focused = w->focused,
        .ptr_in = w->ptr_on_content,
        .shown = w->gc_on && !w->gc_broken,
        .image = w->gc_cur >= 0 && w->gc[w->gc_cur].valid &&
                 w->gc[w->gc_cur].buf && w->spr_surf && w->spr_vp,
        .pos = w->gpos_valid,
        .frame = w->current >= 0 && w->buf_w > 0 && w->buf_h > 0,
        .ui = w->ui.open || w->dlg_open,
    };
    bool want = nb_spr_want(&in);

    if (want) {
        want = nb_view_cursor_rect(&w->place, w->buf_w, w->buf_h, w->gpos_x,
                                   w->gpos_y, (int)w->gc_hx, (int)w->gc_hy,
                                   (int)w->gc[w->gc_cur].w,
                                   (int)w->gc[w->gc_cur].h, &r);
    }
    if (!want) {
        if (w->spr_mapped) {
            /*
             * Both surfaces lose their buffer, the cursor first: a
             * compositor may draw a subsurface whose parent was unmapped
             * as long as it still has a buffer.  Then the area it covered
             * is damaged on the window's own surface (spr_kick_now): one
             * that repaints only what a commit damages -- Hyprland -- kept
             * the cursor's last pixels on screen after the ungrab.
             */
            wl_surface_attach(w->spr_surf, NULL, 0, 0);
            wl_surface_commit(w->spr_surf);
            wl_surface_attach(w->spr_anchor, NULL, 0, 0);
            wl_surface_commit(w->spr_anchor);
            w->spr_mapped = false;
            w->spr_buf = NULL;
            w->spr_kick = true;
            nb_spr_damage_add(&w->spr_dmg, &w->spr_at);
            wl_display_flush(w->dpy);
        }
        return;
    }
    buf = w->gc[w->gc_cur].buf;
    if (!w->spr_mapped || buf != w->spr_buf || r.w != w->spr_at.w ||
        r.h != w->spr_at.h) {
        wl_surface_attach(w->spr_surf, buf, 0, 0);
        wp_viewport_set_destination(w->spr_vp, r.w, r.h);
        wl_surface_damage_buffer(w->spr_surf, 0, 0, INT32_MAX, INT32_MAX);
        wl_surface_commit(w->spr_surf);
        w->spr_buf = buf;
    }
    if (!w->spr_mapped || r.x != w->spr_at.x || r.y != w->spr_at.y) {
        /* The cursor's position is its parent's state: the anchor's commit
         * applies it, and nothing else. */
        wl_subsurface_set_position(w->spr_sub, r.x, r.y);
        if (!w->spr_mapped) {
            wl_surface_attach(w->spr_anchor, w->spr_anchor_buf, 0, 0);
            wl_surface_damage_buffer(w->spr_anchor, 0, 0, 1, 1);
            w->spr_kick = true;
            nb_spr_damage_add(&w->spr_dmg, &r);
        }
        wl_surface_commit(w->spr_anchor);
        w->spr_mapped = true;
    }
    w->spr_at = r;
    wl_display_flush(w->dpy);
}

/*
 * The drawn cursor appeared or went: damage the main surface and commit it,
 * nothing attached.  The cursor's own (desync) commits are applied, but not
 * every compositor repaints for them -- weston does not for a subsurface's
 * first map until something else damages the output -- and a commit of the
 * window's own surface is a repaint everywhere.  Never from inside the frame
 * path (that commit repaints anyway and clears the flag), and nothing is
 * attached, so the picture's buffer and its sync points are not touched.
 */
static void spr_kick_now(struct nb_wl *w)
{
    struct nb_rect d = w->spr_dmg;

    if (!w->spr_kick || !w->surf) {
        return;
    }
    w->spr_kick = false;
    w->spr_dmg = (struct nb_rect){ 0, 0, 0, 0 };
    /* Surface coordinates: the window's, which the cursor was placed in. */
    if (d.w > 0 && d.h > 0) {
        wl_surface_damage(w->surf, d.x, d.y, d.w, d.h);
    } else {
        wl_surface_damage(w->surf, 0, 0, 1, 1);
    }
    wl_surface_commit(w->surf);
    wl_display_flush(w->dpy);
}

/* CMD_CURSOR_POS: where the guest's pointer is now (under grab). */
static void wl_cursor_pos(struct nb_session *s, int x, int y)
{
    struct nb_wl *w = s->priv;

    w->gpos_x = x;
    w->gpos_y = y;
    w->gpos_valid = true;
    w->gpos_seen = true;
    spr_update(w);
    spr_kick_now(w);
    wl_nudge(w);    /* a move while tearing (see wl_nudge_wanted) */
}

/* ── ops: event loop ─────────────────────────────────────────────────────── */

static int wl_pollfds(struct nb_session *s, struct pollfd *out, int max)
{
    struct nb_wl *w = s->priv;
    int r, n;

    if (max < 1) {
        return 0;
    }
    /*
     * Flush before the poll, non-blocking.  wl_display_flush() returns -1 with
     * EAGAIN when the compositor socket is full; we then ask poll() for
     * POLLOUT and try again, rather than looping on write.  This is the rule
     * "input must never block on rendering": the same thread carries both, so
     * a blocking flush to a wedged compositor would stall the keyboard.
     */
    r = wl_display_flush(w->dpy);
    w->flush_blocked = (r < 0 && errno == EAGAIN);
    nb_pace_flushed(&w->pace, w->flush_blocked);

    out[0].fd = wl_display_get_fd(w->dpy);
    out[0].events = POLLIN | (w->flush_blocked ? POLLOUT : 0);
    out[0].revents = 0;
    w->pfd = out;
    n = 1;
    w->fetch_pfd = -1;
    if (w->fetch_fd >= 0 && n < max) {
        /* The selection pipe.  Polled rather than read inline: the transfer is
         * the compositor's to pace, and blocking on it here would stall input
         * for as long as the other client takes to write. */
        out[n].fd = w->fetch_fd;
        out[n].events = POLLIN;
        out[n].revents = 0;
        w->fetch_pfd = n++;
    }
    for (int i = 0; i < NB_WL_MAX_SENDS; i++) {
        w->send[i].pfd = -1;
        if (w->send[i].fd >= 0 && n < max) {
            out[n].fd = w->send[i].fd;
            out[n].events = POLLOUT;
            out[n].revents = 0;
            w->send[i].pfd = n++;
        }
    }
    w->latch_pfd = -1;
    if (w->latch_armed && w->latch_tfd >= 0 && n < max) {
        out[n].fd = w->latch_tfd;
        out[n].events = POLLIN;
        out[n].revents = 0;
        w->latch_pfd = n++;
    }
    /* Explicit sync: a release point reached (nb_xsync.h). */
    w->xs_pfd = -1;
    if (nb_xs_eventfd(w->xs) >= 0 && wl_xs_waiting(w) && n < max) {
        out[n].fd = nb_xs_eventfd(w->xs);
        out[n].events = POLLIN;
        out[n].revents = 0;
        w->xs_pfd = n++;
    }
    return n;
}

/*
 * The main loop calls dispatch() on every wakeup, not only when the compositor
 * fd is readable — the client socket or a signal get us here too.
 * wl_display_read_events() BLOCKS, so it may only be called when poll actually
 * said POLLIN; otherwise the broker would stall on the compositor and stop
 * forwarding input, which is exactly the coupling this design forbids.
 */
static int wl_dispatch_session(struct nb_session *s, struct nb_sink *sink)
{
    struct nb_wl *w = s->priv;
    bool readable = w->pfd && (w->pfd->revents & (POLLIN | POLLHUP | POLLERR));

    w->sink = sink;

    /*
     * CLIPBOARD, both halves, before the Wayland dispatch below -- the pipe is
     * a separate fd and its readiness has nothing to do with the compositor's.
     */
    if (w->fetch_fd >= 0 && w->pfd && w->fetch_pfd > 0 &&
        (w->pfd[w->fetch_pfd].revents & (POLLIN | POLLHUP))) {
        (void)wl_fetch_pump(w, sink);
    }
    for (int i = 0; i < NB_WL_MAX_SENDS; i++) {
        if (w->send[i].fd >= 0 && w->pfd && w->send[i].pfd > 0) {
            short re = w->pfd[w->send[i].pfd].revents;

            if (re & (POLLERR | POLLHUP | POLLNVAL)) {
                wl_send_drop(w, i);
            } else if (re & POLLOUT) {
                wl_send_pump(w, i);
            }
        }
    }

    if (w->pfd && (w->pfd->revents & POLLOUT)) {
        nb_pace_flushed(&w->pace, wl_display_flush(w->dpy) < 0 &&
                                  errno == EAGAIN);
    }
    if (w->pfd && w->latch_pfd > 0 && (w->pfd[w->latch_pfd].revents & POLLIN)) {
        uint64_t n;

        if (read(w->latch_tfd, &n, sizeof(n)) < 0) {
            /* EAGAIN: already consumed; nothing else can fail here */
        }
        w->latch_armed = false;
        wl_commit_staged(w);    /* the latch window opened */
    }
    if (w->pfd && w->xs_pfd >= 0 && (w->pfd[w->xs_pfd].revents & POLLIN)) {
        wl_xs_scan(w);          /* buffers back to the guest, targets free */
    }
    if (wl_display_prepare_read(w->dpy) != 0) {
        /* Another queue already has events buffered; drain those first. */
        if (wl_display_dispatch_pending(w->dpy) < 0) {
            return -EIO;
        }
        wl_commit_staged(w);
        return 0;
    }
    if (!readable) {
        wl_display_cancel_read(w->dpy);
        if (wl_display_dispatch_pending(w->dpy) < 0) {
            return -EIO;
        }
        wl_commit_staged(w);
        return 0;
    }
    if (wl_display_read_events(w->dpy) < 0) {
        return -EIO;
    }
    if (wl_display_dispatch_pending(w->dpy) < 0) {
        return -EIO;
    }
    if (w->quit) {
        /* The close box.  Same shape as the display going away: main() says
         * goodbye to the client and tears the session down. */
        return -ECONNRESET;
    }
    wl_commit_staged(w);        /* a frame callback may have opened the gate */
    return 0;
}

/*
 * The compositor answers a grab request with events, not with a return code,
 * and nothing here used to listen for them.  A capability that advertises
 * true and then silently declines is exactly the failure this project keeps
 * finding, so both objects are now listened to and every transition is
 * logged: what was requested, and what the compositor actually did with it.
 */
static void inhibitor_active(void *d, struct zwp_keyboard_shortcuts_inhibitor_v1 *i)
{
    (void)d; (void)i;
    nb_log("grab: shortcuts inhibitor ACTIVE — compositor bindings "
           "(Super, Alt+Tab, ...) now reach the guest");
}
static void inhibitor_inactive(void *d, struct zwp_keyboard_shortcuts_inhibitor_v1 *i)
{
    (void)d; (void)i;
    nb_log("grab: shortcuts inhibitor INACTIVE — the compositor keeps its own "
           "bindings; they will NOT reach the guest");
}
static const struct zwp_keyboard_shortcuts_inhibitor_v1_listener inhibitor_listener = {
    .active = inhibitor_active, .inactive = inhibitor_inactive,
};

static void lockptr_locked(void *d, struct zwp_locked_pointer_v1 *l)
{
    (void)d; (void)l;
    nb_log("grab: pointer LOCKED to the window");
}
static void lockptr_unlocked(void *d, struct zwp_locked_pointer_v1 *l)
{
    (void)d; (void)l;
    nb_log("grab: pointer lock RELEASED by the compositor");
}
static const struct zwp_locked_pointer_v1_listener lockptr_listener = {
    .locked = lockptr_locked, .unlocked = lockptr_unlocked,
};

static int wl_set_grab(struct nb_session *s, bool on)
{
    struct nb_wl *w = s->priv;

    if (w->grabbed == on) {
        return 0;
    }
    nb_log("grab %s: requesting%s%s%s", on ? "ON" : "off",
           w->inhibit_mgr ? " shortcuts-inhibit" : " (no shortcuts-inhibit)",
           w->constraints  ? " pointer-lock"     : " (no pointer-lock)",
           w->relptr_mgr   ? " relative-pointer" : " (no relative-pointer)");
    if (on) {
        if (!w->ptr) {
            nb_log("grab: no wl_pointer on this seat — neither the pointer "
                   "lock nor relative motion can be requested at all");
        }
        if (w->inhibit_mgr && !w->inhibitor) {
            w->inhibitor =
                zwp_keyboard_shortcuts_inhibit_manager_v1_inhibit_shortcuts(
                    w->inhibit_mgr, w->surf, w->seat);
            if (w->inhibitor) {
                zwp_keyboard_shortcuts_inhibitor_v1_add_listener(
                    w->inhibitor, &inhibitor_listener, w);
            }
        }
        if (w->constraints && w->ptr && !w->lock) {
            w->lock = zwp_pointer_constraints_v1_lock_pointer(
                w->constraints, w->surf, w->ptr, NULL,
                ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_PERSISTENT);
            if (w->lock) {
                zwp_locked_pointer_v1_add_listener(w->lock, &lockptr_listener,
                                                   w);
            }
        }
        if (w->relptr_mgr && w->ptr && !w->relptr) {
            w->relptr = zwp_relative_pointer_manager_v1_get_relative_pointer(
                w->relptr_mgr, w->ptr);
            zwp_relative_pointer_v1_add_listener(w->relptr,
                                                 &relptr_listener, w);
        }
    } else {
        if (w->inhibitor) {
            zwp_keyboard_shortcuts_inhibitor_v1_destroy(w->inhibitor);
            w->inhibitor = NULL;
        }
        if (w->lock) {
            zwp_locked_pointer_v1_destroy(w->lock);
            w->lock = NULL;
        }
        if (w->relptr) {
            zwp_relative_pointer_v1_destroy(w->relptr);
            w->relptr = NULL;
        }
    }
    w->grabbed = on;
    /*
     * Where the guest cursor is when the pointer locks: where the host
     * pointer is, since the guest followed it until now.  Only for a guest
     * that reports its position (it did once on this connection) -- for one
     * that does not, a cursor drawn there would stand still while the real
     * one moves.  Released, the host pointer drives the guest again.
     */
    w->gpos_valid = false;
    if (on && w->gpos_seen && w->ptr_on_content && w->current >= 0 &&
        w->buf_w > 0 && w->buf_h > 0) {
        nb_view_map(&w->place, w->buf_w, w->buf_h, w->ptr_wx, w->ptr_wy,
                    &w->gpos_x, &w->gpos_y);
        w->gpos_valid = true;
    }
    /* Grabbed, the guest cursor leaves the host pointer for the viewer's
     * own drawing of it; released, it is the host pointer's image again. */
    cur_apply(w, w->last_serial);
    spr_kick_now(w);
    /* The bar is the only thing on screen that can say how to get out. */
    tb_update(w, w->tb_w > 0 ? w->tb_w : w->win_w);
    /*
     * Flush NOW.  Everything above is a queued request, and the only other
     * flush in this backend is the POLLOUT path, which does not run until the
     * compositor socket has something to say.  Without this the grab is issued
     * on the wire whenever the next frame happens to flush — which on an idle
     * guest is "eventually", and looks exactly like a grab that does nothing.
     */
    wl_display_flush(w->dpy);
    return 0;
}

static int wl_set_fullscreen(struct nb_session *s, bool on)
{
    struct nb_wl *w = s->priv;

    if (!w->toplevel) {
        return -ENOTSUP;
    }
    /*
     * Fullscreen is where the frame can actually reach a hardware plane: a
     * compositor promotes an opaque, unoccluded, correctly formatted surface
     * covering an output to DIRECT SCANOUT.  It cannot be requested, only
     * qualified for — this is the request for the precondition.
     */
    if (on) {
        xdg_toplevel_set_fullscreen(w->toplevel, NULL);
    } else {
        xdg_toplevel_unset_fullscreen(w->toplevel);
    }
    w->fullscreen = on;
    return 0;
}

static void wl_close_session(struct nb_session *s)
{
    struct nb_wl *w = s->priv;
    int i;

    if (!w) {
        return;
    }
    if (w->latch_tfd >= 0) {
        close(w->latch_tfd);
        w->latch_tfd = -1;
    }
    if (w->flog) {
        fclose(w->flog);
        w->flog = NULL;
    }
    wl_set_grab(s, false);
    if (w->fetch_fd >= 0) {
        close(w->fetch_fd);
    }
    free(w->fetch_buf);
    w->fetch_buf = NULL;
    for (i = 0; i < NB_WL_MAX_SENDS; i++) {
        if (w->send[i].fd >= 0) {
            wl_send_drop(w, i);
        }
    }
    if (w->pending_offer && w->pending_offer != w->offer) {
        wl_data_offer_destroy(w->pending_offer);
    }
    if (w->offer) {
        wl_data_offer_destroy(w->offer);
    }
    if (w->source) {
        wl_data_source_destroy(w->source);
    }
    free(w->src_text);
    free(w->clip_pending);
    w->clip_pending = NULL;
    for (i = 0; i < NB_MAX_BUFS; i++) {
        wl_buf_destroy(w, i);
    }
    wl_gs_close(w);
    wl_xs_close(w);
    if (w->dfb) {
        zwp_linux_dmabuf_feedback_v1_destroy(w->dfb);
        w->dfb = NULL;
    }
    if (w->fb_tab) {
        munmap(w->fb_tab, w->fb_tab_sz);
        w->fb_tab = NULL;
    }
    if (w->dmabuf4) {
        zwp_linux_dmabuf_v1_destroy(w->dmabuf4);
        w->dmabuf4 = NULL;
    }
    wl_idle_drop(w);
    tb_drop(w);
    ov_drop_buffers(w);
    if (w->ov_sub) {
        wl_subsurface_destroy(w->ov_sub);
    }
    if (w->ov_surf) {
        wl_surface_destroy(w->ov_surf);
    }
    for (i = 0; i < 4; i++) {
        if (w->gc[i].buf) {
            wl_buffer_destroy(w->gc[i].buf);
        }
    }
    if (w->gc_gs) {
        nb_gs_close(w->gc_gs);
    }
    if (w->gc_vp) {
        wp_viewport_destroy(w->gc_vp);
    }
    if (w->gc_surf) {
        wl_surface_destroy(w->gc_surf);
    }
    spr_destroy(w);
    if (w->direct) {
        nb_run_hook(w->direct_hook, "off");    /* leave the host as found */
    }
    for (i = 0; i < NB_CUR_N; i++) {
        if (w->cur_buf[i]) {
            wl_buffer_destroy(w->cur_buf[i]);
        }
        if (w->cur_px[i]) {
            munmap(w->cur_px[i], w->cur_sz[i]);
        }
        if (w->cur_surf[i]) {
            wl_surface_destroy(w->cur_surf[i]);
        }
    }
    for (i = 0; i < 8; i++) {
        if (w->bd_vp[i]) {
            wp_viewport_destroy(w->bd_vp[i]);
        }
        if (w->bd_sub[i]) {
            wl_subsurface_destroy(w->bd_sub[i]);
        }
        if (w->bd_surf[i]) {
            wl_surface_destroy(w->bd_surf[i]);
        }
    }
    if (w->bd_buf) {
        wl_buffer_destroy(w->bd_buf);
        w->bd_buf = NULL;
    }
    if (w->bd_px) {
        munmap(w->bd_px, 4);
        w->bd_px = NULL;
    }
    if (w->tb_sub) {
        wl_subsurface_destroy(w->tb_sub);
        w->tb_sub = NULL;
    }
    if (w->tb_surf) {
        wl_surface_destroy(w->tb_surf);
        w->tb_surf = NULL;
    }
    if (w->tearing) {
        wp_tearing_control_v1_destroy(w->tearing);
        w->tearing = NULL;
    }
    if (w->cs_tearing) {
        wp_tearing_control_v1_destroy(w->cs_tearing);
        w->cs_tearing = NULL;
    }
    if (w->ctype) {
        wp_content_type_v1_destroy(w->ctype);
        w->ctype = NULL;
    }
    if (w->cs_ctype) {
        wp_content_type_v1_destroy(w->cs_ctype);
        w->cs_ctype = NULL;
    }
    if (w->ctype_mgr) {
        wp_content_type_manager_v1_destroy(w->ctype_mgr);
        w->ctype_mgr = NULL;
    }
    if (w->tearing_mgr) {
        wp_tearing_control_manager_v1_destroy(w->tearing_mgr);
        w->tearing_mgr = NULL;
    }
    if (w->cs_vp) {
        wp_viewport_destroy(w->cs_vp);
    }
    if (w->cs_sub) {
        wl_subsurface_destroy(w->cs_sub);
    }
    if (w->cs_surf) {
        wl_surface_destroy(w->cs_surf);
    }
    if (w->blk_buf) {
        wl_buffer_destroy(w->blk_buf);
    }
    if (w->blk_px) {
        munmap(w->blk_px, 4);
    }
    if (w->spb_mgr) {
        wp_single_pixel_buffer_manager_v1_destroy(w->spb_mgr);
        w->spb_mgr = NULL;
    }
    if (w->viewport) {
        wp_viewport_destroy(w->viewport);
        w->viewport = NULL;
    }
    if (w->deco) {
        zxdg_toplevel_decoration_v1_destroy(w->deco);
        w->deco = NULL;
    }
    if (w->dpy) {
        wl_display_flush(w->dpy);
        wl_display_disconnect(w->dpy);
    }
    free(w);
    s->priv = NULL;
    free(s);
}

/*
 * The compositor cannot take a dma-buf at all (no zwp_linux_dmabuf_v1, a
 * version without create_immed, or no pair a guest frame can use).  The
 * read-back level needs none of that -- only wl_shm, which is core -- so a
 * mode that may fall that far (auto, copy, readback, shm) goes there; an
 * explicit native or linear asked for dma-buf and fails loudly instead.
 * Returns true when presentation can go on.
 */
static bool wl_dmabuf_unusable(struct nb_wl *w, const char *why)
{
    if (!w->shm || !nb_pc_floor(&w->pc, why)) {
        return false;
    }
    w->gt_want_rb = true;
    return true;
}

static int wl_open(struct nb_session *s, const struct nb_config *cfg)
{
    struct nb_wl *w = s->priv;
    int missing = 0;

    w->sess = s;
    w->surf_w = (int)cfg->win_w;
    w->surf_h = (int)cfg->win_h;
    w->pending = -1;
    w->current = -1;
    w->last_scanout = -1;
    nb_so_init(&w->so);
    nb_pc_init(&w->pc, nb_tier);
    /* The read-back level reads every copy back from the start. */
    w->gt_want_rb = nb_pc_forces_readback(&w->pc);
    /* 0 is a valid button index, so "none" has to be spelled -1 explicitly:
     * a calloc'd struct would otherwise open with the close button drawn as
     * hovered AND held. */
    w->tb_hover = -1;
    w->tb_press = -1;
    w->bd_hot = -1;
    w->scale_mode = cfg->scale_mode;
    w->cfg = cfg;
    nb_vstore_setup(&w->vst, cfg);
    nb_ui_init(&w->ui, &w->vst);
    w->ui_cur = -1;
    /* virtio-nvgpu viewer state */
    w->ov_mode = cfg->overlay_mode;
    w->ov_user = cfg->overlay_mode != NB_OVERLAY_OFF;
    w->gs_mode = cfg->gpu_scale;
    for (int gi = 0; gi < NB_GS_TARGETS; gi++) {
        w->gt[gi].owner = w;
    }
    w->direct = cfg->direct_mode;
    w->direct_hook = cfg->direct_hook;
    w->hint_align = cfg->hint_align ? cfg->hint_align : 1;
    w->resize_mode = cfg->resize_mode;
    w->gc_cur = -1;
    w->snap.direct = -1;

    w->dpy = wl_display_connect(NULL);
    if (!w->dpy) {
        nb_err("wl_display_connect failed (WAYLAND_DISPLAY=%s)",
               getenv("WAYLAND_DISPLAY") ? getenv("WAYLAND_DISPLAY") : "");
        return -ENOENT;
    }
    w->reg = wl_display_get_registry(w->dpy);
    wl_registry_add_listener(w->reg, &reg_listener, w);
    wl_display_roundtrip(w->dpy);   /* globals */
    wl_display_roundtrip(w->dpy);   /* the dmabuf format/modifier burst */
    /*
     * The data device needs BOTH the seat and the manager, and registry order
     * is not guaranteed -- so it is created here, after the globals are in,
     * rather than in whichever handler happened to run second.
     */
    if (w->ddm && w->seat) {
        w->ddev = wl_data_device_manager_get_data_device(w->ddm, w->seat);
        if (w->ddev) {
            wl_data_device_add_listener(w->ddev, &ddev_listener, w);
        }
    }
    if (!w->ddev) {
        nb_log("no wl_data_device: clipboard is unavailable on this "
               "compositor, whatever --clipboard says");
    }
    if (w->ddev) {
        s->clipboard_caps = NB_SESSION_CLIP_G2H | NB_SESSION_CLIP_H2G;
    }

    if (!w->comp || !w->wm_base) {
        nb_err("the compositor is missing %s%s— there is no window to make",
               w->comp ? "" : "wl_compositor ",
               w->wm_base ? "" : "xdg_wm_base ");
        goto fail;
    }
    if (!w->seat) {
        /*
         * No seat means no input and, critically, no wl_keyboard.leave — so
         * focus loss is unobservable and grab must not be offered.  Not fatal:
         * a display with no input is still a display, and saying so beats
         * refusing to start with a message about a global nobody has heard of.
         */
        nb_err("this compositor advertises no wl_seat: there will be NO "
               "keyboard, NO pointer and NO grab. Display only.");
    }
    /*
     * WHAT THE COMPOSITOR TAKES.  The read-back level (and --present-mode
     * shm, which advertises no dma-buf on purpose) needs only wl_shm; every
     * other level needs zwp_linux_dmabuf_v1 version 2+ and at least one pair
     * a guest frame can have.  "Advertised nothing" and "advertised nothing
     * this mode can use" are told apart: the first is the compositor, the
     * second is the format filter or --present-mode=linear.
     */
    if (!w->shm && nb_pc_forces_readback(&w->pc)) {
        nb_err("--present-mode=%s needs wl_shm, and this compositor offers "
               "none.  wl_shm is core Wayland; without it nothing can be "
               "shown.", nb_pm_name(w->pc.mode));
        goto fail;
    }
    if (!nb_pc_forces_readback(&w->pc)) {
        char why[200] = "";

        if (!w->dmabuf) {
            snprintf(why, sizeof(why), "this compositor does not implement "
                     "zwp_linux_dmabuf_v1");
        } else if (w->dmabuf_ver < 2) {
            snprintf(why, sizeof(why), "zwp_linux_dmabuf_v1 version %u has "
                     "no create_immed (version 2+ is needed)", w->dmabuf_ver);
        } else if (w->formats.n == 0 && w->n_adv_raw == 0) {
            snprintf(why, sizeof(why), "the compositor advertised no dma-buf "
                     "formats");
        } else if (w->formats.n == 0) {
            snprintf(why, sizeof(why), "the compositor advertised %u dma-buf "
                     "(format, modifier) pairs, none of them XRGB/ARGB 8888%s",
                     w->n_adv_raw,
                     nb_tier == NB_TIER_LINEAR || nb_linear_only
                         ? " with the LINEAR modifier (--present-mode=linear)"
                         : "");
        }
        if (why[0]) {
            if (!wl_dmabuf_unusable(w, why)) {
                nb_err("%s, so the guest's buffer cannot be shown as it is "
                       "(--present-mode=%s).  --present-mode=auto or readback "
                       "reads frames back into shared memory instead.", why,
                       nb_pm_name(w->pc.mode));
                goto fail;
            }
        }
    }
    wl_pc_report(w);

    /* The window. */
    w->scale_120 = 120;         /* until the compositor says otherwise */
    w->fetch_fd = -1;
    w->surf = wl_compositor_create_surface(w->comp);
    if (w->surf && w->frac_mgr) {
        w->frac = wp_fractional_scale_manager_v1_get_fractional_scale(
                      w->frac_mgr, w->surf);
        if (w->frac) {
            wp_fractional_scale_v1_add_listener(w->frac, &frac_listener, w);
        }
    }
    w->xdg_surf = xdg_wm_base_get_xdg_surface(w->wm_base, w->surf);
    xdg_surface_add_listener(w->xdg_surf, &xdg_listener, w);
    w->toplevel = xdg_surface_get_toplevel(w->xdg_surf);
    xdg_toplevel_add_listener(w->toplevel, &top_listener, w);
    xdg_toplevel_set_title(w->toplevel, cfg->title);
    xdg_toplevel_set_app_id(w->toplevel, "conduit-viewer");
    /* Tell the compositor the floor rather than only clamping after the
     * fact: a min size it knows about stops the small configure being sent,
     * and makes the resize edges stop at something usable. The geometry we
     * set includes the title bar, so the minimum must too. */
    xdg_toplevel_set_min_size(w->toplevel, NB_MIN_W, NB_MIN_H + NB_TB_H);
    if (w->deco_mgr) {
        w->deco = zxdg_decoration_manager_v1_get_toplevel_decoration(
            w->deco_mgr, w->toplevel);
        if (w->deco) {
            zxdg_toplevel_decoration_v1_add_listener(w->deco, &deco_listener,
                                                     w);
            zxdg_toplevel_decoration_v1_set_mode(
                w->deco, ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE);
        }
    } else {
        nb_log("no zxdg_decoration_manager_v1: the window will have no title "
               "bar, no close button and no resize grips, and --title will "
               "render nowhere.  That is the compositor's choice, not a bug "
               "here.");
    }
    /*
     * --title is the base title, and it is the user's to choose.  Clamped to
     * NB_TITLE_MAX drawn characters: the bar has three buttons on the right
     * and a fixed-width font, and a title long enough to run under them is
     * worse than a truncated one.
     */
    {
        const char *t = cfg->title && *cfg->title ? cfg->title : "Conduit";

        if (strlen(t) > NB_TITLE_MAX) {
            snprintf(w->title, sizeof(w->title), "%.*s...",
                     (int)NB_TITLE_MAX - 3, t);
            nb_log("--title truncated to %zu characters: \"%s\"",
                   NB_TITLE_MAX, w->title);
        } else {
            snprintf(w->title, sizeof(w->title), "%s", t);
        }
    }
    /*
     * Draw our own title bar when -- and only when -- the compositor will not.
     * Mutter does not implement zxdg_decoration_manager_v1 (GNOME Shell 50.1,
     * verified 2026-08-24), so on GNOME this is the only way the window gets a
     * title, buttons, or anything to drag it by.
     */
    if (!w->deco_mgr && w->subcomp && w->shm) {
        w->tb_surf = wl_compositor_create_surface(w->comp);
        if (w->tb_surf) {
            struct wl_region *op = wl_compositor_create_region(w->comp);

            w->tb_sub = wl_subcompositor_get_subsurface(w->subcomp,
                                                        w->tb_surf, w->surf);
            if (w->tb_sub) {
                /*
                 * ABOVE the content, at negative y, NOT overlapping it.  The
                 * guest's picture stays whole -- the window is simply
                 * NB_TB_H taller than the guest's resolution, which is what
                 * a title bar is -- and, because nothing occludes the content
                 * surface, it remains a direct-scanout candidate even while
                 * windowed.  An overlay bar would forfeit that for chrome.
                 */
                wl_subsurface_set_position(w->tb_sub, 0, -NB_TB_H);
                wl_subsurface_set_desync(w->tb_sub);
                nb_log("drawing a client-side title bar (%d px): this "
                       "compositor offers no server-side decorations",
                       NB_TB_H);
            }
            if (op) {
                wl_region_add(op, 0, 0, 1 << 20, NB_TB_H);
                wl_surface_set_opaque_region(w->tb_surf, op);
                wl_region_destroy(op);
            }
        }
    }
    bd_build(w);
    if (cfg->fullscreen) {
        /*
         * Asking at startup, before the first configure, is the only way to
         * get a fullscreen window without a keystroke -- which is what makes
         * the direct-scanout question measurable from a script instead of
         * from somebody's fingers.
         */
        xdg_toplevel_set_fullscreen(w->toplevel, NULL);
        w->fullscreen = true;
        nb_log("starting fullscreen (--fullscreen)");
    }
    if (w->direct) {
        nb_log("direct mode ON at startup (--direct-mode=on): overlay hidden");
        nb_run_hook(w->direct_hook, "on");
    }
    if (w->dmabuf4 && w->surf) {
        w->dfb = zwp_linux_dmabuf_v1_get_surface_feedback(w->dmabuf4, w->surf);
        if (w->dfb) {
            zwp_linux_dmabuf_feedback_v1_add_listener(w->dfb, &dfb_listener, w);
        }
    } else {
        nb_log("dma-buf feedback: not offered (zwp_linux_dmabuf_v1 below "
               "version 4): no scanout tranche to check the guest's format "
               "against");
    }
    {
        const char *swc = getenv("WLR_NO_HARDWARE_CURSORS");

        w->env_sw_cursor = swc && *swc && strcmp(swc, "0") != 0;
        if (w->env_sw_cursor) {
            nb_log("WLR_NO_HARDWARE_CURSORS=%s is set in this session: a "
                   "compositor that honours it draws the pointer in software, "
                   "and a visible software cursor rules out direct scanout.  "
                   "Over the fullscreen picture the viewer hides the host "
                   "pointer when the guest draws its own (and under grab)", swc);
        }
    }
    if (w->viewporter) {
        w->viewport = wp_viewporter_get_viewport(w->viewporter, w->surf);
        if (w->subcomp) {
            /* The picture's surface under a letterbox; see cs_surf. */
            w->cs_surf = wl_compositor_create_surface(w->comp);
            if (w->cs_surf) {
                w->cs_sub = wl_subcompositor_get_subsurface(w->subcomp,
                                                            w->cs_surf, w->surf);
                w->cs_vp = wp_viewporter_get_viewport(w->viewporter,
                                                      w->cs_surf);
            }
            if (w->cs_sub) {
                /* The first subsurface that overlaps the picture area:
                 * everything made later is stacked above it. */
                wl_subsurface_set_desync(w->cs_sub);
                /* The guest cursor under grab: right above the picture. */
                (void)spr_build(w);
            }
            w->cs_at_x = w->cs_at_y = INT32_MIN;
        }
    } else {
        nb_log("no wp_viewporter: the window is fixed at the guest's own "
               "resolution and cannot be resized independently of it");
    }
    {
        const char *sig = getenv("HYPRLAND_INSTANCE_SIGNATURE");

        w->hypr_want = w->hypr_sent = -1;
        if (sig && *sig && !strchr(sig, '/')) {
            w->hypr_sig = sig;
        }
    }
    wl_tearing_apply(w);
    nb_log("presentation: sync=%s -> %s now (%s; tearing-control %s, "
           "content-type %s%s)", nb_sync_name(w->vst.sync),
           w->tearing_async ? "IMMEDIATE" : "vsync",
           w->vst.sync == NB_SYNC_FULLSCREEN
               ? "immediate in fullscreen, vsync in a window"
               : w->vst.sync == NB_SYNC_IMMEDIATE ? "always immediate"
                                                  : "always vsync",
           w->tearing_mgr ? "yes" : "NOT OFFERED: the compositor decides",
           w->ctype_mgr ? "yes" : "no",
           w->hypr_sig ? ", Hyprland: sets this window's `immediate`" : "");
    w->win_w = w->surf_w;
    w->win_h = w->surf_h;
    w->wnd_w = w->win_w;            /* what unfullscreen returns to, until a
                                     * windowed configure says otherwise */
    w->wnd_h = w->win_h;
    wl_surf_commit(w, w->surf);
    wl_display_roundtrip(w->dpy);   /* configure, and the decoration mode */

    s->width = (uint32_t)w->surf_w;
    s->height = (uint32_t)w->surf_h;
    s->caps = NVKVM_BROKER_CAP_FULLSCREEN | NVKVM_BROKER_CAP_DMABUF |
              NVKVM_BROKER_CAP_RELEASE | NVKVM_BROKER_CAP_RELEASE_SEQ;
    /* Presentation feedback is what EV_PRESENTED reports. */
    if (w->presentation) {
        s->caps |= NVKVM_BROKER_CAP_PRESENTED;
    }
    if (w->seat) {
        s->caps |= NVKVM_BROKER_CAP_KEYBOARD | NVKVM_BROKER_CAP_ABS_POINTER |
                   NVKVM_BROKER_CAP_FOCUS_EVENTS;
    }
    /* virtio-nvgpu: mode hints always; the guest cursor when there is a
     * pointer to put it under and the display takes a LINEAR ARGB8888
     * dma-buf (what the guest's cursor plane is). */
    s->caps |= NVKVM_BROKER_CAP_MODE_HINTS | NVKVM_BROKER_CAP_MODE_LIST;
    if (w->seat && ((w->dmabuf && w->dmabuf_ver >= 3 &&
                     nb_formats_has(&w->formats, NB_FOURCC_AR24_LOCAL, 0)) ||
                    (w->shm && gc_gs_ready(w)))) {
        s->caps |= NVKVM_BROKER_CAP_CURSOR;
        /* Under grab it draws the guest cursor itself, where the guest
         * says it is. */
        if (w->spr_surf) {
            s->caps |= NVKVM_BROKER_CAP_CURSOR_POS;
        }
    } else {
        nb_log("guest cursor: not offered (needs a pointer, and a display "
               "that imports LINEAR ARGB8888 dma-bufs or a GPU that copies "
               "them); the guest draws its own");
    }
    if (w->dmabuf_ver >= 3) {
        s->caps |= NVKVM_BROKER_CAP_MODIFIERS;
    } else {
        nb_log("zwp_linux_dmabuf_v1 is version 2: no explicit modifiers, so "
               "only implicitly-modified buffers will be accepted");
    }
    if (w->relptr_mgr && w->seat) {
        s->caps |= NVKVM_BROKER_CAP_REL_POINTER;
    } else {
        missing++;
    }
    if (w->constraints && w->seat) {
        s->caps |= NVKVM_BROKER_CAP_POINTER_LOCK;
    } else {
        missing++;
    }
    if (w->inhibit_mgr && w->seat) {
        s->caps |= NVKVM_BROKER_CAP_TOTAL_GRAB;
    } else {
        missing++;
    }

    /* Say exactly what is missing, at startup, in one line. */
    if (missing) {
        snprintf(s->grab_caveat, sizeof(s->grab_caveat),
                 "compositor is missing: %s%s%s",
                 w->inhibit_mgr ? ""
                     : "keyboard-shortcuts-inhibit (Super and other compositor "
                       "shortcuts WILL still fire under grab); ",
                 w->constraints ? "" : "pointer-constraints (pointer NOT "
                                       "confined); ",
                 w->relptr_mgr ? "" : "relative-pointer (NO relative motion "
                                      "under grab); ");
    }
    nb_formats_log(&w->formats, "the compositor");

    /*
     * ACCEPT SHARED MEMORY TOO.
     *
     * wl_shm is a core Wayland global that every compositor must implement, so
     * a shm buffer is the one thing a display CANNOT refuse.  That makes it the
     * universal last tier for the VMM, after a display has advertised a
     * modifier it then would not import -- MEASURED on GNOME/Mutter with
     * NVIDIA 595.84, which advertises XR24+LINEAR through
     * eglQueryDmaBufModifiersEXT and answers the import with "Could not bind
     * the given EGLImage to a CoglTexture2D".
     *
     * It costs the compositor an upload per frame, which is why it is the LAST
     * tier and not the first.  The VMM only reaches for it when the dma-buf
     * tiers have been refused.
     *
     * accept_SHM, not accept_MEMFD.  AUDIT 2026-08-27 S-1: this line used to
     * set accept_memfd, which is the validator's "a memfd may stand in for a
     * dma-buf ANYWHERE" switch -- so making the shm tier work here also
     * deleted the dma-buf proof (design §3 rule 4) from every ordinary frame,
     * and a memfd sent WITHOUT F_SHM went to
     * zwp_linux_buffer_params_v1_add() as though it were one.
     */
    /* Conduit: an F_SHM frame is always accepted.  The guest's own frames are
     * dma-bufs and stay zero-copy (or a loud dropped frame) in the native and
     * linear tiers; the backend sends shared memory only for its boot console
     * -- the VM's firmware and boot screens, before the guest driver shows
     * anything -- which has no dma-buf to offer. */
    s->accept_shm = true;
    return 0;

fail:
    if (w->dpy) {
        wl_display_disconnect(w->dpy);
        w->dpy = NULL;
    }
    return -ENOTSUP;
}

static const struct nb_session_ops wl_ops = {
    .name = "wayland",
    .open = wl_open,
    .close = wl_close_session,
    .pollfds = wl_pollfds,
    .dispatch = wl_dispatch_session,
    .set_grab = wl_set_grab,
    .set_fullscreen = wl_set_fullscreen,
    .format_ok = wl_format_ok,
    .copy_ok = wl_copy_ok,
    .attach = wl_attach,
    .commit = wl_commit,
    .resize = wl_resize,
    .client_detach = wl_client_detach,
    .show_idle = wl_show_idle,
    .dismiss_dialog = wl_dismiss_dialog,
    .set_clipboard = wl_set_clipboard,
    .tick = wl_tick,
    .resync = wl_resync,
    .notify_clipboard = wl_notify_clipboard,
    .fetch_clipboard = wl_fetch_clipboard,
    .cursor = wl_cursor,
    .cursor_pos = wl_cursor_pos,
    .hotkey = wl_hotkey,
    .hold_release = wl_hold_release,
    .modes = wl_modes,
};

struct nb_session *nb_session_wayland(const struct nb_config *cfg)
{
    struct nb_session *s = calloc(1, sizeof(*s));
    struct nb_wl *w = calloc(1, sizeof(*w));

    if (!s || !w) {
        free(s);
        free(w);
        return NULL;
    }
    s->ops = &wl_ops;
    s->priv = w;
    w->fetch_fd = -1;
    w->fetch_pfd = -1;
    w->latch_tfd = -1;
    w->latch_pfd = -1;
    w->xs_pfd = -1;
    nb_latch_init(&w->latch);
    {
        const char *fl = getenv("CONDUIT_VIEWER_FRAMELOG");

        if (fl && *fl) {
            w->flog = fopen(fl, "we");
            if (w->flog) {
                setvbuf(w->flog, NULL, _IOFBF, 1 << 16);
                nb_log("frame log: writing %s", fl);
            } else {
                nb_log("frame log: %s: %s", fl, strerror(errno));
            }
        }
    }
    for (int i = 0; i < NB_WL_MAX_SENDS; i++) {
        w->send[i].fd = -1;
        w->send[i].pfd = -1;
    }

    if (wl_ops.open(s, cfg) != 0) {
        free(w);
        free(s);
        return NULL;
    }
    return s;
}

#endif /* NB_HAVE_WAYLAND */
