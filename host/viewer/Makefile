# SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
#
# conduit-viewer (formerly nvkvm-display-broker)
#
# Backends are compiled in only when their libraries are present, and the build
# PRINTS which ones it kept.  A missing backend must be visible at build time,
# not discovered as a mysterious runtime failure.
#
#   make                 build with whatever is available
#   make check           build, then run selftest.sh
#   sudo make install    -> /usr/local/bin
#
# Note what is NOT here: no libdrm, no EGL, no GL, no libgbm.  The broker turns
# a dma-buf into a wl_buffer or an X pixmap with protocol requests; the import
# happens in the compositor or the X server.  That is the same property the
# VMM gains, one process in.
#
# Debian/Ubuntu build deps:
#   Wayland backend : libwayland-dev wayland-protocols
#   X11 backend     : libxcb1-dev libxcb-dri3-dev libxcb-present-dev
#   X11 scaling     : libxcb-render0-dev     (optional; without it, 1:1 only)
#   X11 rel. motion : libxcb-xinput-dev      (optional; see the report below)

PKG_CONFIG ?= pkg-config
PREFIX     ?= /usr/local
BIN         = conduit-viewer
TESTCLIENT  = nvkvm-broker-testclient
RELAY       = nvkvm-broker-relay
DMASRC      = nvkvm-broker-dmabuf-src

CFLAGS  ?= -O2 -g
CFLAGS  += -std=gnu11 -Wall -Wextra -Wno-unused-parameter \
           -Wformat=2 -Wshadow -Wvla \
           -fstack-protector-strong -D_FORTIFY_SOURCE=2 \
           -D_GNU_SOURCE -I. -Icommon
LDFLAGS += -Wl,-z,relro,-z,now

OBJS = nvkvm_broker.o nb_common.o nb_placeholder.o nb_overlay.o nb_view.o nb_ui.o \
       nb_session_x11.o nb_session_wl.o nb_session_test.o

# ── X11 backend: xcb + DRI3 + Present ─────────────────────────────────────
# DRI3 is what lets a dma-buf become an X pixmap without a GL context, and
# Present is what puts that pixmap on screen and reports back when it is done
# with it.  Without both there is no GL-free X11 path, so the backend is left
# out rather than silently degraded.
X11_PKGS = xcb xcb-dri3 xcb-present
ifeq ($(shell $(PKG_CONFIG) --exists $(X11_PKGS) && echo yes),yes)
HAVE_X11 = 1
CFLAGS  += -DNB_HAVE_X11 $(shell $(PKG_CONFIG) --cflags $(X11_PKGS))
LIBS    += $(shell $(PKG_CONFIG) --libs $(X11_PKGS))
# Optional: XI2 raw motion, the only source of true relative deltas under a
# pointer grab (core motion is clamped at the confine boundary).
# XRender: server-side scaling, so a window that is not the guest's mode still
# fills.  Optional -- without it the X11 backend letterboxes 1:1 as before.
ifeq ($(shell $(PKG_CONFIG) --exists xcb-render && echo yes),yes)
HAVE_XRENDER = 1
CFLAGS  += -DNB_HAVE_XCB_RENDER $(shell $(PKG_CONFIG) --cflags xcb-render)
LIBS    += $(shell $(PKG_CONFIG) --libs xcb-render)
endif
ifeq ($(shell $(PKG_CONFIG) --exists xcb-xinput && echo yes),yes)
HAVE_XI  = 1
CFLAGS  += -DNB_HAVE_XCB_XINPUT $(shell $(PKG_CONFIG) --cflags xcb-xinput)
LIBS    += $(shell $(PKG_CONFIG) --libs xcb-xinput)
endif
endif

# ── Wayland backend ───────────────────────────────────────────────────────
WL_PROTO_DIR := $(shell $(PKG_CONFIG) --variable=pkgdatadir wayland-protocols 2>/dev/null)
WL_SCANNER   := $(shell $(PKG_CONFIG) --variable=wayland_scanner wayland-scanner 2>/dev/null)
ifeq ($(WL_SCANNER),)
WL_SCANNER   := $(shell command -v wayland-scanner 2>/dev/null)
endif

WL_XML = \
  $(WL_PROTO_DIR)/stable/xdg-shell/xdg-shell.xml \
  $(WL_PROTO_DIR)/stable/linux-dmabuf/linux-dmabuf-v1.xml \
  $(WL_PROTO_DIR)/unstable/keyboard-shortcuts-inhibit/keyboard-shortcuts-inhibit-unstable-v1.xml \
  $(WL_PROTO_DIR)/unstable/pointer-constraints/pointer-constraints-unstable-v1.xml \
  $(WL_PROTO_DIR)/unstable/relative-pointer/relative-pointer-unstable-v1.xml \
  $(WL_PROTO_DIR)/stable/presentation-time/presentation-time.xml \
  $(WL_PROTO_DIR)/stable/viewporter/viewporter.xml \
  $(WL_PROTO_DIR)/unstable/xdg-decoration/xdg-decoration-unstable-v1.xml \
  $(WL_PROTO_DIR)/staging/fractional-scale/fractional-scale-v1.xml \
  $(WL_PROTO_DIR)/staging/tearing-control/tearing-control-v1.xml

WL_HDRS = $(notdir $(WL_XML:.xml=-client-protocol.h))
WL_SRCS = $(notdir $(WL_XML:.xml=-protocol.c))
WL_OBJS = $(WL_SRCS:.c=.o)

ifneq ($(WL_PROTO_DIR),)
ifneq ($(WL_SCANNER),)
ifeq ($(shell $(PKG_CONFIG) --exists wayland-client && echo yes),yes)
ifeq ($(shell test -f $(WL_PROTO_DIR)/stable/linux-dmabuf/linux-dmabuf-v1.xml && echo yes),yes)
HAVE_WL  = 1
CFLAGS  += -DNB_HAVE_WAYLAND $(shell $(PKG_CONFIG) --cflags wayland-client)
LIBS    += $(shell $(PKG_CONFIG) --libs wayland-client)
OBJS    += $(WL_OBJS)
endif
endif
endif
endif

# ── UI text: FreeType + fontconfig ────────────────────────────────────────
# The menu (Ctrl+Alt+M) and the on-screen notices draw text with FreeType,
# using whatever sans face fontconfig picks.  Optional: without them the UI
# falls back to the built-in bitmap font.
ifeq ($(shell $(PKG_CONFIG) --exists freetype2 fontconfig && echo yes),yes)
HAVE_FT   = 1
UI_CFLAGS = -DNB_HAVE_FREETYPE $(shell $(PKG_CONFIG) --cflags freetype2 fontconfig)
UI_LIBS   = $(shell $(PKG_CONFIG) --libs freetype2 fontconfig)
CFLAGS   += $(UI_CFLAGS)
LIBS     += $(UI_LIBS)
endif
LIBS     += -lm

ifeq ($(HAVE_X11)$(HAVE_WL),)
$(warning No display backend will be compiled in: only --backend test will work.)
$(warning   Wayland: apt install libwayland-dev wayland-protocols)
$(warning   X11:     apt install libxcb1-dev libxcb-dri3-dev libxcb-present-dev)
endif

# ── test tooling: a real dma-buf producer ─────────────────────────────────
# dmabuf_source is the ONLY thing here that links libgbm, and it is not part
# of the broker.  It exists because testclient sends a memfd, which every real
# backend correctly rejects — so it can prove the validator but can never put
# a pixel on screen.  Built only when gbm is present.
ifeq ($(shell $(PKG_CONFIG) --exists gbm egl glesv2 && echo yes),yes)
HAVE_SCANTEST = 1
endif
ifeq ($(shell $(PKG_CONFIG) --exists gbm && echo yes),yes)
HAVE_GBM = 1
GBM_CFLAGS = $(shell $(PKG_CONFIG) --cflags gbm)
GBM_LIBS   = $(shell $(PKG_CONFIG) --libs gbm)
endif

.SECONDARY:
.PHONY: all install clean report check check-sanitize
SCANTEST    = nvgpu-scanout-test
all: report $(BIN) $(TESTCLIENT) $(RELAY) $(if $(HAVE_GBM),$(DMASRC),) $(if $(HAVE_SCANTEST),$(SCANTEST),)

report:
	@echo "conduit-viewer backends:"
	@echo "  scanout test source : $(if $(HAVE_SCANTEST),yes (gbm+egl+glesv2),NO - apt install libgbm-dev libegl-dev libgles-dev)"
	@echo "  dma-buf test source : $(if $(HAVE_GBM),yes (libgbm),NO - apt install libgbm-dev)"
	@echo "  wayland : $(if $(HAVE_WL),yes,NO - install libwayland-dev wayland-protocols)"
	@echo "  x11     : $(if $(HAVE_X11),yes (dri3+present$(if $(HAVE_XRENDER), +xrender scaling, NO SCALING: install libxcb-render0-dev)),NO - install libxcb1-dev libxcb-dri3-dev libxcb-present-dev)$(if $(HAVE_X11),$(if $(HAVE_XI), + xi2 raw motion, - NO relative motion under grab: install libxcb-xinput-dev),)"
	@echo "  test    : yes (always)"
	@echo "  ui text : $(if $(HAVE_FT),freetype + fontconfig,bitmap font only - apt install libfreetype-dev libfontconfig-dev)"

$(BIN): $(OBJS)
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS) $(LIBS)

# Generated Wayland protocol glue.  Every .o depends on the headers because
# they are generated, and make has no way to know that otherwise.
$(OBJS): $(if $(HAVE_WL),$(WL_HDRS),)

%-client-protocol.h:
	$(WL_SCANNER) client-header $(filter %/$*.xml,$(WL_XML)) $@

%-protocol.c:
	$(WL_SCANNER) private-code $(filter %/$*.xml,$(WL_XML)) $@

#
# HEADER DEPENDENCIES.  Without these, editing a .h rebuilds NOTHING: `make`
# sees every .o newer than its .c and stops.  That is not a slow build, it is a
# WRONG one -- struct nb_sink grew several fields this cycle, and object files
# compiled against different layouts of it link together happily and then
# corrupt each other's memory.  The observable result was four selftest
# failures in the input/grab tests, on a tree whose sources were correct.
#
# -MMD writes a .d beside each .o; -MP adds phony targets so a DELETED header
# does not wedge the build with "no rule to make target".
#
%.o: %.c
	$(CC) $(CFLAGS) -MMD -MP -c -o $@ $<

-include $(wildcard *.d)

# A minimal client, for proving the broker works before involving QEMU.  It is
# also the selftest's attack harness: its --bad-* options are the ways a
# hostile VMM can lie to the privileged side.
$(TESTCLIENT): testclient.c common/nvkvm_broker_proto.h
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

# The sandboxed side, deliberately linked against libc and nothing else — run
# ldd on it inside the container; that link line IS the security claim.
$(RELAY): dmabuf_relay.c common/nvkvm_broker_proto.h
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

$(DMASRC): dmabuf_source.c common/nvkvm_broker_proto.h
	$(CC) $(CFLAGS) $(GBM_CFLAGS) $(if $(HAVE_X11),-DSRC_HAVE_X11,) \
	      -o $@ $< $(LDFLAGS) $(GBM_LIBS) $(if $(HAVE_X11),$(shell $(PKG_CONFIG) --libs xcb xcb-dri3),)

# virtio-nvgpu standalone producer: GBM + EGL/GLES2 render into block-linear
# buffers, streamed over the broker protocol.  TEST TOOLING, like DMASRC.
$(SCANTEST): nvgpu_scanout_test.c common/nvkvm_broker_proto.h
	$(CC) $(CFLAGS) $(shell $(PKG_CONFIG) --cflags gbm egl glesv2) -o $@ $< \
	      $(LDFLAGS) $(shell $(PKG_CONFIG) --libs gbm egl glesv2) -lm

# The UTF-8 validator is extracted from the source it lives in rather than
# copied, so the test cannot quietly drift from the code it is testing.
test/utf8_extracted.inc: nvkvm_broker.c
	sed -n '/^static bool nb_utf8_ok/,/^}/p' $< > $@

test-utf8: test/utf8_extracted.inc test/test_utf8.c
	$(CC) $(CFLAGS) -I test -o $@ test/test_utf8.c

test-overlay: test/test_overlay.c nb_overlay.c nb_placeholder.c nb_overlay.h
	$(CC) $(CFLAGS) -I . -o $@ test/test_overlay.c nb_overlay.c nb_placeholder.c -lm

test-view: test/test_view.c nb_view.c nb_view.h
	$(CC) $(CFLAGS) -I . -o $@ test/test_view.c nb_view.c -lm

test-ui: test/test_ui.c nb_ui.c nb_ui.h nb_view.c nb_view.h nb_placeholder.c
	$(CC) $(CFLAGS) $(UI_CFLAGS) -I . -o $@ test/test_ui.c nb_ui.c nb_view.c nb_placeholder.c $(UI_LIBS) -lm

test-relacc: test/test_relacc.c nb_relacc.h
	$(CC) $(CFLAGS) -I . -o $@ test/test_relacc.c

check: all test-utf8 test-overlay test-relacc test-view test-ui
	./test-utf8
	./test-view
	./test-ui
	./test-relacc
	./test-overlay
	bash selftest.sh
	python3 test/test_adopted_socket.py ./$(BIN)
	python3 test/test_clipboard.py ./$(BIN)
	python3 test/test_clipboard_push.py ./$(BIN)
	python3 test/test_lifecycle.py ./$(BIN)

# Keep the sanitizer invocation reproducible locally and in CI.  A clean
# rebuild matters here: mixing instrumented and ordinary objects links fine
# and produces a test result that did not cover what it claims to cover.
check-sanitize:
	$(MAKE) clean
	$(MAKE) CFLAGS="$(CFLAGS) -O1 -fsanitize=address,undefined -fno-omit-frame-pointer" \
	        LDFLAGS="$(LDFLAGS) -fsanitize=address,undefined" check

install: $(BIN)
	install -d $(DESTDIR)$(PREFIX)/bin
	install -m 0755 $(BIN) $(DESTDIR)$(PREFIX)/bin/$(BIN)

clean:
	rm -f $(BIN) $(TESTCLIENT) $(RELAY) $(DMASRC) $(SCANTEST) *.o *.d \
	      *-client-protocol.h *-protocol.c test-utf8 test-overlay test/utf8_extracted.inc
