#!/bin/bash
# headless_moderule_test.sh -- per-guest-mode picture settings (nb_view.c
# nb_vstore_guest_mode / nb_vstore_auto_rule) against a real compositor:
# weston on its headless backend, the viewer windowed at 1280x720, and a
# LINEAR dma-buf stream that switches modes A (1280x720) -> B (640x480) -> A.
#
# The settings file remembers Stretch + 21:9 for 640x480 only.  Each mode's
# FIRST frame must already be placed with its own settings (the viewer logs
# "guest mode WxH: first frame ..."): B stretched into the 21:9 area, A
# fitted to the whole window, and A again restored.  The file must come out
# with the same rule.
#
# Needs weston and a render node to allocate from (a non-NVIDIA one is
# preferred: NVIDIA does not import LINEAR).  Exits 0 pass, 1 fail, 77 skip.
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
trap 'kill ${WPID:-} ${BPID:-} 2>/dev/null; [ -n "${KEEP:-}" ] && echo "logs: $WORK" || rm -rf "$WORK"' EXIT
# Never the user's desktop: no inherited display, no compositor IPC.
unset WAYLAND_DISPLAY WAYLAND_SOCKET DISPLAY HYPRLAND_INSTANCE_SIGNATURE SWAYSOCK

cat >view.conf <<'CONF'
scale=fit
filter=linear
area=full
res=native
rule=640x480|scale=stretch filter=linear area=21:9 res=native
CONF

LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
    timeout 120 "$WESTON" --backend=headless --renderer=gl --width=1600 \
    --height=1000 --socket=wl-h --idle-time=0 >weston.log 2>&1 &
WPID=$!
for _ in $(seq 60); do [ -S wl-h ] && break; sleep 0.25; done
[ -S wl-h ] || { tail -12 weston.log; skip "weston did not start"; }
export WAYLAND_DISPLAY="$WORK/wl-h"
case "$WAYLAND_DISPLAY" in
    "$WORK"/*) ;;
    *) echo "FAIL: refusing to start the viewer outside the test compositor"; exit 1 ;;
esac

timeout 90 "$BROKER" --socket b.sock --socket-mode 0600 --backend wayland \
    --size 1280x720 --title t --persist --view-state view.conf \
    --resize=scale --overlay=off >viewer.log 2>&1 &
BPID=$!
for _ in $(seq 60); do [ -S b.sock ] && break; sleep 0.25; done
[ -S b.sock ] || { echo "FAIL: no viewer socket"; tail viewer.log; exit 1; }

for size in 1280x720 640x480 1280x720; do
    timeout 30 "$DMASRC" --node "$NODE" --present b.sock --modifier linear \
        --size "$size" --frames 6 >>src.log 2>&1
    sleep 1
done
kill "$BPID" 2>/dev/null; wait "$BPID" 2>/dev/null
kill "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null

grep -q 'CAN import .* modifier 0x0000000000000000' viewer.log ||
    skip "this compositor did not import LINEAR -- nothing to assert"
mapfile -t L < <(grep 'guest mode .*: first frame' viewer.log)
rc=0
if [ "${#L[@]}" -lt 3 ]; then
    echo "FAIL: ${#L[@]} mode changes logged, expected 3"; rc=1
else
    printf '  %s\n' "${L[@]:0:3}"
    case "${L[0]}" in *"1280x720: first frame 1280x720 at 0,0"*"Fit"*) ;; *) echo "FAIL: A"; rc=1 ;; esac
    case "${L[1]}" in *"640x480: first frame 1280x548 at 0,86"*"Stretch"*"21:9"*"(remembered)"*) ;;
        *) echo "FAIL: B's first frame without its rule"; rc=1 ;; esac
    case "${L[2]}" in *"1280x720: first frame 1280x720 at 0,0"*"Fit"*"(default)"*) ;;
        *) echo "FAIL: A not restored"; rc=1 ;; esac
fi
grep -q '^rule=640x480|scale=stretch .*area=21:9' view.conf ||
    { echo "FAIL: the rule did not survive"; cat view.conf; rc=1; }
grep -q '^scale=fit' view.conf || { echo "FAIL: the default changed"; rc=1; }
[ $rc -eq 0 ] && echo "PASS: each mode's first frame has its own settings"
exit $rc
