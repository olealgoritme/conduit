#!/bin/bash
# headless_enhance_test.sh -- the picture-enhancement path (an upscaler and
# frame generation, docs/VIEWER-ENHANCE.md) end to end against a real
# compositor, headless: weston's headless backend, the viewer on it with
# --upscale and --framegen, a dma-buf source streaming frames.
#
# Asserts from the viewer's log that the asked-for upscaler is the one that
# ran (not a fallback), that every frame went through the GPU pass, and that
# frame generation showed pictures of its own between the guest's frames.
#
# Like headless_present_test.sh: weston on llvmpipe by default
# (NVKVM_TEST_SOFTWARE=1), or on a real non-NVIDIA GPU with
# NVKVM_TEST_SOFTWARE=0; NVKVM_TEST_NODE picks the allocator's render node;
# ENH_UPSCALE (fsr|nis|cas) and ENH_FRAMEGEN (2x|3x) the modes.
#
# Exits 0 on pass, 1 on failure, 77 on skip (no weston, nothing built).
set -u

DIR="$(cd "$(dirname "$0")" && pwd)"
BROKER="$DIR/conduit-viewer"
DMASRC="$DIR/nvkvm-broker-dmabuf-src"
FRAMES=60
UP="${ENH_UPSCALE:-fsr}"
FG="${ENH_FRAMEGEN:-2x}"
rc=0

skip() { echo "SKIP: $*"; exit 77; }

command -v weston >/dev/null || skip "weston is not installed"
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
trap 'kill ${WPID:-} ${BPID:-} ${SPID:-} 2>/dev/null; rm -rf "$WORK"' EXIT

if [ "${NVKVM_TEST_SOFTWARE:-1}" = 1 ]; then
    export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
fi
echo "== headless weston: --upscale=$UP --framegen=$FG, dma-bufs from $NODE =="

weston --backend=headless --renderer=gl --width=1280 --height=720 \
       --socket=wl-headless --idle-time=0 >"$WORK/weston.log" 2>&1 &
WPID=$!
for _ in $(seq 60); do [ -S "$WORK/wl-headless" ] && break; sleep 0.25; done
[ -S "$WORK/wl-headless" ] || { echo "weston did not start:"; tail -12 "$WORK/weston.log"; exit 77; }
export WAYLAND_DISPLAY=wl-headless

# 640x360 frames in a 1280x720 window: a 2x upscale.
"$BROKER" --socket "$WORK/b.sock" --socket-mode 0600 --allow-user root \
          --backend wayland --size 1280x720 --title enhance-test --stats \
          --render-node "$NODE" --upscale="$UP" --sharpness=50 \
          --framegen="$FG" >"$WORK/broker.log" 2>&1 &
BPID=$!
for _ in $(seq 60); do [ -S "$WORK/b.sock" ] && break; sleep 0.25; done
[ -S "$WORK/b.sock" ] || { echo "FAIL: viewer never created its socket"; tail -12 "$WORK/broker.log"; exit 1; }

"$DMASRC" --node "$NODE" --present "$WORK/b.sock" --modifier linear \
          --size 640x360 --frames "$FRAMES" --interval-ms 33 \
          >"$WORK/src.log" 2>&1 &
SPID=$!
sleep $(( FRAMES / 30 + 6 ))
kill $SPID 2>/dev/null; wait $SPID 2>/dev/null
sleep 1

echo "--- viewer log (enhancement lines) ---"
grep -E 'upscaling|frame generation|framegen|gpu-scale|GPU pass' "$WORK/broker.log" | head -20

if grep -q 'GLES 3.1 needed\|lacks GLES 3.1' "$WORK/broker.log"; then
    skip "this GL has no GLES 3.1"
fi
if ! grep -qE "upscaling: .* asked, .* runs" "$WORK/broker.log"; then
    echo "FAIL: the upscaler never ran"
    rc=1
elif grep -E "upscaling: .* asked, .* runs" "$WORK/broker.log" | grep -q "Off runs"; then
    echo "FAIL: the upscaler fell back to the plain pass"
    rc=1
fi
gen=$(grep -o 'framegen [0-9]x: [0-9]* generated' "$WORK/broker.log" |
      awk '{ s += $3 } END { print s + 0 }')
echo "  generated pictures: $gen"
if [ "$gen" -lt 1 ]; then
    echo "FAIL: frame generation showed no pictures of its own"
    rc=1
fi
[ $rc -eq 0 ] && echo "PASS: $UP ran on every frame, frame generation $FG made $gen pictures"
exit $rc
