/* SPDX-License-Identifier: Apache-2.0 */
/*
 * nvgpu_scanout_test.c -- standalone producer for the virtio-nvgpu viewer.
 *
 * Stands in for the vhost-user-nvgpu backend WITHOUT a VM: allocates a ring of
 * GBM buffers on the host GPU (default /dev/dri/renderD128), renders a moving
 * pattern into them ON THE GPU (EGL/GLES2, so NVIDIA block-linear buffers work
 * -- gbm_bo_map cannot write those), exports each as a dma-buf ONCE and streams
 * ATTACH+COMMIT to the broker exactly as the backend does (broker wire
 * protocol v2, docs/broker-protocol.md).
 *
 * cmd.seq carries CLOCK_MONOTONIC microseconds (low 32 bits); start the viewer
 * with `--stats --seq-usec` and it reports true send->commit and
 * send->present latency.
 *
 * TEST TOOLING ONLY.  The viewer itself links no EGL, GL or GBM.
 *
 *   nvgpu-scanout-test --socket PATH [--node /dev/dri/renderD128]
 *                      [--size 2560x1440] [--hz 240] [--seconds 10]
 *                      [--bufs 3] [--linear] [--pace-frame] [--print-input]
 */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>

#include <gbm.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>

#include "common/nvkvm_broker_proto.h"

#define FOURCC_XR24 0x34325258u    /* DRM_FORMAT_XRGB8888 */
#define MAXB 8

struct buf {
    struct gbm_bo *bo;
    int      fd;
    uint64_t id;            /* dma-buf inode == the broker's buffer id */
    uint32_t stride, offset;
    uint64_t modifier;
    EGLImageKHR img;
    GLuint   tex, fbo;
    bool     held;          /* committed, no RELEASE yet */
};

static uint64_t now_ns(void)
{
    struct timespec ts;

    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static void die(const char *m)
{
    fprintf(stderr, "nvgpu-scanout-test: %s\n", m);
    exit(1);
}

static int send_cmd(int s, const struct nvkvm_broker_cmd *c, int fd)
{
    char cbuf[CMSG_SPACE(sizeof(int))];
    struct iovec iov = { .iov_base = (void *)c, .iov_len = sizeof(*c) };
    struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1 };

    if (fd >= 0) {
        struct cmsghdr *cm;

        memset(cbuf, 0, sizeof(cbuf));
        msg.msg_control = cbuf;
        msg.msg_controllen = sizeof(cbuf);
        cm = CMSG_FIRSTHDR(&msg);
        cm->cmsg_level = SOL_SOCKET;
        cm->cmsg_type = SCM_RIGHTS;
        cm->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(cm), &fd, sizeof(int));
    }
    return sendmsg(s, &msg, MSG_NOSIGNAL) == (ssize_t)sizeof(*c) ? 0 : -1;
}

int main(int argc, char **argv)
{
    const char *sock_path = NULL, *node = "/dev/dri/renderD128";
    unsigned w = 2560, h = 1440, hz = 240, secs = 10, nb = 3;
    bool linear = false, pace_frame = false, print_input = false;
    struct buf b[MAXB];
    int i;

    for (i = 1; i < argc; i++) {
        const char *a = argv[i], *v = i + 1 < argc ? argv[i + 1] : NULL;

        if (!strcmp(a, "--socket") && v)        { sock_path = v; i++; }
        else if (!strcmp(a, "--node") && v)     { node = v; i++; }
        else if (!strcmp(a, "--size") && v)     { sscanf(v, "%ux%u", &w, &h); i++; }
        else if (!strcmp(a, "--hz") && v)       { hz = (unsigned)atoi(v); i++; }
        else if (!strcmp(a, "--seconds") && v)  { secs = (unsigned)atoi(v); i++; }
        else if (!strcmp(a, "--bufs") && v)     { nb = (unsigned)atoi(v); i++; }
        else if (!strcmp(a, "--linear"))        { linear = true; }
        else if (!strcmp(a, "--pace-frame"))    { pace_frame = true; }
        else if (!strcmp(a, "--print-input"))   { print_input = true; }
        else {
            fprintf(stderr,
                    "usage: %s --socket PATH [--node /dev/dri/renderDN] "
                    "[--size WxH] [--hz N] [--seconds N] [--bufs 2..8] "
                    "[--linear] [--pace-frame] [--print-input]\n", argv[0]);
            return 2;
        }
    }
    if (!sock_path || nb < 2 || nb > MAXB || !hz || !w || !h) {
        die("bad arguments (need --socket; --bufs 2..8)");
    }

    /* ── GPU: GBM device, EGL on it, surfaceless GLES2 context ── */
    int drm = open(node, O_RDWR | O_CLOEXEC);
    if (drm < 0) {
        die("cannot open render node");
    }
    struct gbm_device *gbm = gbm_create_device(drm);
    if (!gbm) {
        die("gbm_create_device failed");
    }
    printf("gbm backend: %s on %s\n", gbm_device_get_backend_name(gbm), node);

    PFNEGLGETPLATFORMDISPLAYEXTPROC getpd =
        (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    PFNEGLCREATEIMAGEKHRPROC create_image =
        (void *)eglGetProcAddress("eglCreateImageKHR");
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC img_tex =
        (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!getpd || !create_image || !img_tex) {
        die("missing EGL/GLES extension entry points");
    }
    EGLDisplay dpy = getpd(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
        die("EGL on GBM failed");
    }
    eglBindAPI(EGL_OPENGL_ES_API);
    static const EGLint ctx_attr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT,
                                      ctx_attr);
    if (ctx == EGL_NO_CONTEXT ||
        !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        die("surfaceless GLES2 context failed");
    }
    printf("GL_RENDERER: %s\n", (const char *)glGetString(GL_RENDERER));

    /* ── the buffer ring: allocated and exported ONCE ── */
    for (i = 0; i < (int)nb; i++) {
        struct stat st;
        uint64_t lin = 0;   /* DRM_FORMAT_MOD_LINEAR */

        memset(&b[i], 0, sizeof(b[i]));
        if (linear) {
            b[i].bo = gbm_bo_create_with_modifiers(gbm, w, h, FOURCC_XR24,
                                                   &lin, 1);
        } else {
            b[i].bo = gbm_bo_create(gbm, w, h, FOURCC_XR24,
                                    GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT);
        }
        if (!b[i].bo) {
            die("gbm_bo_create failed");
        }
        b[i].fd = gbm_bo_get_fd(b[i].bo);
        b[i].stride = gbm_bo_get_stride(b[i].bo);
        b[i].offset = gbm_bo_get_offset(b[i].bo, 0);
        b[i].modifier = gbm_bo_get_modifier(b[i].bo);
        if (b[i].fd < 0 || fstat(b[i].fd, &st) < 0) {
            die("dma-buf export failed");
        }
        b[i].id = (uint64_t)st.st_ino;

        EGLint ia[] = {
            EGL_WIDTH, (EGLint)w, EGL_HEIGHT, (EGLint)h,
            EGL_LINUX_DRM_FOURCC_EXT, (EGLint)FOURCC_XR24,
            EGL_DMA_BUF_PLANE0_FD_EXT, b[i].fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLint)b[i].offset,
            EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)b[i].stride,
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (EGLint)(b[i].modifier & 0xffffffffu),
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLint)(b[i].modifier >> 32),
            EGL_NONE,
        };
        b[i].img = create_image(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT,
                                NULL, ia);
        if (b[i].img == EGL_NO_IMAGE_KHR) {
            die("eglCreateImage of our own dma-buf failed");
        }
        glGenTextures(1, &b[i].tex);
        glBindTexture(GL_TEXTURE_2D, b[i].tex);
        img_tex(GL_TEXTURE_2D, b[i].img);
        glGenFramebuffers(1, &b[i].fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, b[i].fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, b[i].tex, 0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) !=
            GL_FRAMEBUFFER_COMPLETE) {
            die("FBO on the dma-buf is incomplete");
        }
        printf("buf %d: %ux%u XR24 stride %u offset %u modifier 0x%016llx "
               "id %llu\n", i, w, h, b[i].stride, b[i].offset,
               (unsigned long long)b[i].modifier, (unsigned long long)b[i].id);
    }

    /* ── connect, HELLO ── */
    struct sockaddr_un sa = { .sun_family = AF_UNIX };
    int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    snprintf(sa.sun_path, sizeof(sa.sun_path), "%s", sock_path);
    if (s < 0 || connect(s, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
        die("cannot connect to the viewer socket");
    }
    struct nvkvm_broker_pkt pkt;
    if (read(s, &pkt, sizeof(pkt)) != (ssize_t)sizeof(pkt) ||
        pkt.type != NVKVM_BROKER_EV_HELLO) {
        die("no HELLO from the viewer");
    }
    printf("HELLO: proto %u caps 0x%04x\n", pkt.w0, pkt.w1);
    fcntl(s, F_SETFL, fcntl(s, F_GETFL) | O_NONBLOCK);

    /* ── stream ── */
    const uint64_t period = 1000000000ull / hz;
    const uint64_t t_start = now_ns();
    uint64_t next = t_start, t_sec = t_start;
    unsigned sent = 0, frames_ev = 0, releases = 0, reuse_held = 0, inputs = 0;
    uint64_t render_ns = 0, frame_no = 0;
    int cur = -1;
    bool frame_ready = true;
    size_t rxl = 0;
    unsigned char rx[sizeof(pkt)];

    for (;;) {
        uint64_t t = now_ns();

        if (t - t_start >= (uint64_t)secs * 1000000000ull) {
            break;
        }
        /* drain events */
        for (;;) {
            ssize_t n = read(s, rx + rxl, sizeof(rx) - rxl);

            if (n <= 0) {
                if (n == 0) {
                    die("viewer closed the connection");
                }
                break;
            }
            rxl += (size_t)n;
            if (rxl < sizeof(rx)) {
                continue;
            }
            rxl = 0;
            memcpy(&pkt, rx, sizeof(pkt));
            switch (pkt.type) {
            case NVKVM_BROKER_EV_FRAME:
                frames_ev++;
                frame_ready = true;
                break;
            case NVKVM_BROKER_EV_RELEASE: {
                uint64_t id = (uint64_t)pkt.w0 | ((uint64_t)pkt.w1 << 32);

                releases++;
                for (i = 0; i < (int)nb; i++) {
                    if (b[i].id == id) {
                        b[i].held = false;
                    }
                }
                break;
            }
            case NVKVM_BROKER_EV_SURFACE:
                printf("SURFACE %dx%d\n", pkt.x, pkt.y);
                break;
            case NVKVM_BROKER_EV_BYE:
                printf("BYE %d\n", pkt.x);
                return 0;
            case NVKVM_BROKER_EV_KEY: case NVKVM_BROKER_EV_BTN:
            case NVKVM_BROKER_EV_ABS: case NVKVM_BROKER_EV_REL:
            case NVKVM_BROKER_EV_WHEEL:
                inputs++;
                if (print_input) {
                    printf("input type %u x %d y %d w0 %u w1 %u flags 0x%x\n",
                           pkt.type, pkt.x, pkt.y, pkt.w0, pkt.w1, pkt.flags);
                }
                break;
            default:
                break;
            }
        }

        if (t >= next && (!pace_frame || frame_ready)) {
            int pick = -1, k;
            uint64_t r0 = now_ns();

            /* next buffer that is neither on screen nor held by the display */
            for (k = 1; k <= (int)nb; k++) {
                int c = (cur + k) % (int)nb;

                if (c != cur && !b[c].held) {
                    pick = c;
                    break;
                }
            }
            if (pick < 0) {
                pick = (cur + 1) % (int)nb;
                reuse_held++;
            }
            /* moving pattern: hue-cycling background + a sweeping bar pair */
            {
                float ph = (float)frame_no / (float)hz;
                unsigned bx = (unsigned)(frame_no * 8 % w);
                unsigned by = (unsigned)(frame_no * 4 % h);

                glBindFramebuffer(GL_FRAMEBUFFER, b[pick].fbo);
                glViewport(0, 0, (GLsizei)w, (GLsizei)h);
                glDisable(GL_SCISSOR_TEST);
                glClearColor(0.5f + 0.5f * sinf(ph * 2.0f),
                             0.5f + 0.5f * sinf(ph * 2.0f + 2.094f),
                             0.5f + 0.5f * sinf(ph * 2.0f + 4.189f), 1.0f);
                glClear(GL_COLOR_BUFFER_BIT);
                glEnable(GL_SCISSOR_TEST);
                glClearColor(1, 1, 1, 1);
                glScissor((GLint)bx, 0, 48, (GLsizei)h);
                glClear(GL_COLOR_BUFFER_BIT);
                glClearColor(0, 0, 0, 1);
                glScissor(0, (GLint)by, (GLsizei)w, 24);
                glClear(GL_COLOR_BUFFER_BIT);
                glDisable(GL_SCISSOR_TEST);
                glFinish();     /* the frame is complete before it is sent */
            }
            render_ns += now_ns() - r0;

            struct nvkvm_broker_cmd c;
            uint32_t ts_us = (uint32_t)(now_ns() / 1000u);

            memset(&c, 0, sizeof(c));
            c.type = NVKVM_BROKER_CMD_ATTACH;
            c.width = w; c.height = h;
            c.stride = b[pick].stride; c.offset = b[pick].offset;
            c.fourcc = FOURCC_XR24; c.modifier = b[pick].modifier;
            c.seq = ts_us;
            if (send_cmd(s, &c, b[pick].fd) < 0) {
                if (errno == EAGAIN) {
                    goto skip;      /* viewer backlogged: drop, never block */
                }
                die("ATTACH send failed");
            }
            memset(&c, 0, sizeof(c));
            c.type = NVKVM_BROKER_CMD_COMMIT;
            if (send_cmd(s, &c, -1) < 0 && errno != EAGAIN) {
                die("COMMIT send failed");
            }
            b[pick].held = true;
            cur = pick;
            sent++;
            frame_ready = false;
skip:
            frame_no++;
            next += period;
            if (next < t) {
                next = t + period;  /* fell behind: do not burst to catch up */
            }
        }

        t = now_ns();
        if (t - t_sec >= 1000000000ull) {
            double d = (double)(t - t_sec) / 1e9;

            printf("sent %.1f fps | viewer FRAME %.1f/s | RELEASE %.1f/s | "
                   "render+finish avg %.0fus | reuse-held %u | input %u\n",
                   sent / d, frames_ev / d, releases / d,
                   sent ? (double)render_ns / sent / 1e3 : 0.0, reuse_held,
                   inputs);
            fflush(stdout);
            sent = frames_ev = releases = reuse_held = inputs = 0;
            render_ns = 0;
            t_sec = t;
        }

        /* sleep until the next frame or an event, whichever comes first */
        {
            struct pollfd p = { .fd = s, .events = POLLIN };
            uint64_t n2 = now_ns();
            int tmo = next > n2 ? (int)((next - n2) / 1000000u) : 0;

            if (pace_frame && !frame_ready && next <= n2) {
                poll(&p, 1, 100);   /* waiting for the viewer's FRAME */
            } else if (tmo == 0 && next > n2) {
                struct timespec ts = { 0, (long)(next - n2) };

                nanosleep(&ts, NULL);
            } else {
                poll(&p, 1, tmo);
            }
        }
    }
    printf("done\n");
    return 0;
}
