#!/bin/bash
# test-standalone.sh -- prove the viewer on the host WITHOUT a VM.
#
# Starts the viewer on a private socket, streams a GPU-rendered moving pattern
# from /dev/dri/renderD128 (GBM block-linear buffers, rendered with GLES,
# exported as dma-bufs once) for a few seconds, then stops both and prints the
# per-second fps / latency lines from each side.
#
#   bash host/broker/test-standalone.sh [SECONDS] [HZ] [WxH]
#   NVGPU_TEST_LINEAR=1 ...     use LINEAR buffers instead of block-linear
#   NVGPU_TEST_FULLSCREEN=1 ... start the viewer fullscreen (direct scanout /
#                               tearing only happen fullscreen)
#
# Keeps the run SHORT on purpose and checks that Hyprland survived it.
set -uo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
SECS="${1:-5}"; HZ="${2:-240}"; SIZE="${3:-2560x1440}"
OUT="$(mktemp -d "${TMPDIR:-/tmp}/nvgpu-viewer-test.XXXXXX")"
SOCK="$OUT/viewer.sock"

make -C "$DIR" >/dev/null || exit 1
[ -n "${WAYLAND_DISPLAY:-}" ] || { echo "WAYLAND_DISPLAY not set" >&2; exit 1; }
HYPR_PID="$(pgrep -x Hyprland | head -1 || true)"

VARGS=(--backend wayland --socket "$SOCK" --size "$SIZE" --title "virtio-nvgpu test"
       --present-mode=native --stats --seq-usec)
[ "${NVGPU_TEST_FULLSCREEN:-0}" = 1 ] && VARGS+=(--fullscreen)
"$DIR/nvkvm-display-broker" "${VARGS[@]}" >"$OUT/viewer.log" 2>&1 &
VPID=$!
for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { echo "viewer did not come up:"; cat "$OUT/viewer.log"; exit 1; }

TARGS=(--socket "$SOCK" --size "$SIZE" --hz "$HZ" --seconds "$SECS")
[ "${NVGPU_TEST_LINEAR:-0}" = 1 ] && TARGS+=(--linear)
timeout $((SECS + 10)) "$DIR/nvgpu-scanout-test" "${TARGS[@]}" >"$OUT/source.log" 2>&1
SRC_RC=$?
sleep 0.3
kill "$VPID" 2>/dev/null; wait "$VPID" 2>/dev/null

echo "== source ($OUT/source.log, rc=$SRC_RC)"; cat "$OUT/source.log"
echo "== viewer ($OUT/viewer.log)"
grep -E "stats:|Present:|probing|CAN import|REFUS|refused|tearing|ADVERTISED|GRAB IS|error|failed" "$OUT/viewer.log"

if [ -n "$HYPR_PID" ] && ! kill -0 "$HYPR_PID" 2>/dev/null; then
    echo "!! HYPRLAND DIED during the test. Crash reports: ~/.cache/hyprland/"
    ls -t ~/.cache/hyprland/ 2>/dev/null | head -3
    exit 3
fi
exit $SRC_RC
