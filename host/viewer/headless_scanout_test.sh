#!/bin/bash
# headless_scanout_test.sh -- the fullscreen path decision (nb_scanout.h)
# against a real compositor: weston on its headless backend, the viewer
# fullscreen at 1280x720, and a 640x480 LINEAR dma-buf stream, so the
# picture is scaled and letterboxed.
#
# Weston's headless output has no planes, so nothing is ever scanned out
# directly: the viewer must measure the zero-copy path, then the GPU pass,
# then settle on one ("scanout path: ... [measured]") and report the path and
# the blockers in its --stats line.  Then the same with a 1280x720 stream:
# 1:1, which is zero-copy by rule, without measuring.
#
# Needs weston and a render node to allocate from (a non-NVIDIA one is
# preferred: NVIDIA does not import LINEAR, see headless_present_test.sh).
# Exits 0 on pass, 1 on failure, 77 on skip.
set -u

DIR="$(cd "$(dirname "$0")" && pwd)"
BROKER="$DIR/conduit-viewer"
DMASRC="$DIR/nvkvm-broker-dmabuf-src"
WESTON="${WESTON:-weston}"

skip() { echo "SKIP: $*"; exit 77; }

command -v "$WESTON" >/dev/null || skip "weston is not installed"
[ -x "$BROKER" ] || skip "viewer not built ($BROKER)"
[ -x "$DMASRC" ] || skip "dma-buf source not built -- needs libgbm at build time"

NODE="${NVKVM_TEST_NODE:-}"
if [ -z "$NODE" ]; then
    for n in /dev/dri/renderD*; do
        [ -e "$n" ] || continue
        drv=$(sed -n 's/^DRIVER=//p' "/sys/class/drm/$(basename "$n")/device/uevent" 2>/dev/null)
        [ -z "$NODE" ] && NODE="$n"
        if [ "$drv" != "nvidia" ]; then NODE="$n"; break; fi
    done
fi
[ -n "$NODE" ] || skip "no render node to allocate a dma-buf from"

WORK="$(mktemp -d)"
export XDG_RUNTIME_DIR="$WORK"
chmod 700 "$WORK"
cd "$WORK" || exit 1
trap 'kill ${WPID:-} ${BPID:-} 2>/dev/null; rm -rf "$WORK"' EXIT
# Never the user's desktop: no inherited display, no compositor IPC.
unset WAYLAND_DISPLAY WAYLAND_SOCKET DISPLAY HYPRLAND_INSTANCE_SIGNATURE SWAYSOCK

# One weston per run: weston 13's kiosk shell does not always survive a
# fullscreen client going away.
weston_up() {
    rm -f wl-h wl-h.lock
    LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
        timeout 120 "$WESTON" --backend=headless --renderer=gl --width=1280 \
        --height=720 --socket=wl-h --idle-time=0 --shell=kiosk-shell.so \
        >weston.log 2>&1 &
    WPID=$!
    for _ in $(seq 60); do [ -S wl-h ] && break; sleep 0.25; done
    [ -S wl-h ] || { tail -12 weston.log; skip "weston did not start"; }
    # Absolute: libwayland resolves nothing else.
    export WAYLAND_DISPLAY="$WORK/wl-h"
}

# run SIZE FRAMES LOG: the viewer fullscreen, one stream, then stop.
run() {
    weston_up
    case "$WAYLAND_DISPLAY" in
        "$WORK"/*) ;;
        *) echo "FAIL: refusing to start the viewer outside the test compositor"; exit 1 ;;
    esac
    timeout 60 "$BROKER" --socket b.sock --socket-mode 0600 --backend wayland \
        --size "$1" --title t --fullscreen --stats --sync=immediate \
        --render-node "$NODE" >"$3" 2>&1 &
    BPID=$!
    for _ in $(seq 60); do [ -S b.sock ] && break; sleep 0.25; done
    timeout 60 "$DMASRC" --node "$NODE" --present b.sock --modifier linear \
        --size "$1" --frames "$2" >src.log 2>&1
    sleep 1
    kill "$BPID" 2>/dev/null
    wait "$BPID" 2>/dev/null
    kill "$WPID" 2>/dev/null
    wait "$WPID" 2>/dev/null
    rm -f b.sock
}

rc=0
run 640x480 40 scaled.log
grep -q 'CAN import .* modifier 0x0000000000000000' scaled.log ||
    skip "this compositor did not import LINEAR -- nothing to assert"
if grep -q 'scanout path: .* \[measured\]' scaled.log; then
    echo "  scaled: $(grep -o 'scanout path: .* \[measured\].*' scaled.log | tail -1 | cut -c1-140)"
else
    echo "FAIL: scaled picture: no measured path decision"
    grep 'scanout path' scaled.log | head -5
    rc=1
fi
grep -q 'stats: .*| path: ' scaled.log ||
    { echo "FAIL: the --stats line has no path"; rc=1; }
grep -q 'measuring the GPU pass' scaled.log ||
    { echo "FAIL: composited zero-copy did not lead to measuring the GPU pass"; rc=1; }

run 1280x720 12 exact.log
if grep -q 'scanout path: zero-copy \[rule\] - 1:1' exact.log &&
   ! grep -q 'measuring' exact.log; then
    echo "  1:1   : zero-copy by rule, not measured"
else
    echo "FAIL: 1:1 picture was not zero-copy by rule"
    grep 'scanout path' exact.log | head -5
    rc=1
fi
[ $rc -eq 0 ] && echo "PASS: the path is measured when scaled and fixed at 1:1"
exit $rc
