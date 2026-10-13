#!/bin/bash
# headless_copy_test.sh -- frames in a layout the compositor does not take
# are shown by the viewer's GPU copy (nb_blit.h), windowed and fullscreen,
# instead of being rejected.
#
# weston on its headless backend with the software renderer (llvmpipe), which
# takes LINEAR only, and a dma-buf stream allocated on a hardware render node
# with a tiled modifier (COPY_TEST_MODIFIER; the default is an AMD GFX9+
# 64K_S_X layout).  The viewer must answer the
# format query YES (by a GPU copy), commit every frame, and say so in --stats.
#
# Needs weston and a non-NVIDIA render node that allocates that modifier.  Exits 0 on pass, 1 on failure, 77 on skip.
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
        if [ "$drv" != "nvidia" ]; then NODE="$n"; break; fi
    done
fi
[ -n "$NODE" ] || skip "no non-NVIDIA render node"

WORK="$(mktemp -d)"
export XDG_RUNTIME_DIR="$WORK"
chmod 700 "$WORK"
cd "$WORK" || exit 1
trap 'kill ${WPID:-} ${BPID:-} 2>/dev/null; [ -n "${KEEP:-}" ] && echo "logs: $WORK" || rm -rf "$WORK"' EXIT
# Never the user's desktop: no inherited display, no compositor IPC.
unset WAYLAND_DISPLAY WAYLAND_SOCKET DISPLAY HYPRLAND_INSTANCE_SIGNATURE SWAYSOCK

# run LOG [viewer args...]: a fresh weston, the viewer, 30 frames, stop.
run() {
    local log=$1; shift
    rm -f wl-h wl-h.lock b.sock
    LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
        timeout 120 "$WESTON" --backend=headless --renderer=gl --width=1280 \
        --height=720 --socket=wl-h --idle-time=0 >weston.log 2>&1 &
    WPID=$!
    for _ in $(seq 60); do [ -S wl-h ] && break; sleep 0.25; done
    [ -S wl-h ] || { tail -12 weston.log; skip "weston did not start"; }
    export WAYLAND_DISPLAY="$WORK/wl-h"
    case "$WAYLAND_DISPLAY" in
        "$WORK"/*) ;;
        *) echo "FAIL: refusing to start the viewer outside the test compositor"; exit 1 ;;
    esac
    timeout 60 "$BROKER" --socket b.sock --socket-mode 0600 --backend wayland \
        --size 640x480 --title t --stats --render-node "$NODE" "$@" \
        >"$log" 2>&1 &
    BPID=$!
    for _ in $(seq 60); do [ -S b.sock ] && break; sleep 0.25; done
    timeout 60 "$DMASRC" --node "$NODE" --present b.sock --modifier "${COPY_TEST_MODIFIER:-0x0200000000401b03}" \
        --size 640x480 --frames 30 >src.log 2>&1
    sleep 1
    kill "$BPID" 2>/dev/null; wait "$BPID" 2>/dev/null
    kill "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null
    unset WAYLAND_DISPLAY
}

check() {
    local log=$1 what=$2 commits
    if grep -q 'GPU copy: .* is not taken by the compositor' "$log"; then
        commits=$(grep -o 'stats: commit [0-9.]*' "$log" | awk '{s+=$3} END {print s+0}')
        if grep -q 'not advertised by this display' "$log"; then
            echo "FAIL: $what: frames rejected"; rc=1
        elif ! grep -q 'path: GPU copy' "$log"; then
            echo "FAIL: $what: --stats does not name the GPU copy"; rc=1
        elif [ "${commits%.*}" -lt 10 ]; then
            echo "FAIL: $what: only ${commits} frames/s committed"; rc=1
        else
            echo "  $what: GPU copy, frames committed"
        fi
    elif grep -q "CAN import\|direct attach" "$log"; then
        skip "the compositor takes this node's default layout: nothing to copy"
    else
        echo "FAIL: $what: no GPU copy"; grep -iE 'error|copy' "$log" | head -5; rc=1
    fi
}

# cross LOG: weston rendering on the non-NVIDIA GPU (Mesa), frames
# rendered on NVIDIA in its block-linear layout: the live case of a desktop
# on an integrated GPU.  The NVIDIA GPU renders to no layout that compositor
# takes, so the copy must fall back to reading back into shared memory.
cross() {
    local log=$1 nv="" pci
    for n in /dev/dri/renderD*; do
        [ "$(sed -n 's/^DRIVER=//p' "/sys/class/drm/$(basename "$n")/device/uevent" 2>/dev/null)" = nvidia ] && nv=$n
    done
    [ -n "$nv" ] || return 1
    pci=$(basename "$(readlink -f "/sys/class/drm/$(basename "$NODE")/device")")
    rm -f wl-h wl-h.lock b.sock
    __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json \
    DRI_PRIME="pci-$(echo "$pci" | tr ':.' '__')" \
        timeout 120 "$WESTON" --backend=headless --renderer=gl --width=1280 \
        --height=720 --socket=wl-h --idle-time=0 >weston.log 2>&1 &
    WPID=$!
    for _ in $(seq 60); do [ -S wl-h ] && break; sleep 0.25; done
    [ -S wl-h ] || return 1
    export WAYLAND_DISPLAY="$WORK/wl-h"
    timeout 60 "$BROKER" --socket b.sock --socket-mode 0600 --backend wayland \
        --size 640x480 --title t --stats >"$log" 2>&1 &
    BPID=$!
    for _ in $(seq 60); do [ -S b.sock ] && break; sleep 0.25; done
    timeout 60 "$DMASRC" --node "$nv" --present b.sock \
        --modifier "${NV_TEST_MODIFIER:-0x0300000000606014}" \
        --size 640x480 --frames 30 >src.log 2>&1
    sleep 1
    kill "$BPID" 2>/dev/null; wait "$BPID" 2>/dev/null
    kill "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null
    unset WAYLAND_DISPLAY
    return 0
}

rc=0
run windowed.log
check windowed.log windowed
run fullscreen.log --fullscreen --sync=vsync
check fullscreen.log "fullscreen vsync"
if cross cross.log; then
    if grep -q 'read-back targets' cross.log && grep -q 'path: GPU copy' cross.log &&
       ! grep -q 'not advertised by this display' cross.log; then
        echo "  desktop on the other GPU: GPU copy read back, frames committed"
    elif grep -q 'path: GPU copy' cross.log; then
        echo "  desktop on the other GPU: GPU copy, frames committed"
    else
        echo "FAIL: desktop on the other GPU: frames not shown"
        grep -iE 'error|copy|read-back' cross.log | head -5; rc=1
    fi
else
    echo "  (no NVIDIA + other GPU pair: cross-GPU case skipped)"
fi
[ $rc -eq 0 ] && echo "PASS: untakeable layouts are shown by the GPU copy"
exit $rc
