#!/bin/bash
# headless_gamescope_test.sh -- the viewer inside gamescope's headless
# backend, a compositor WITHOUT wp_viewporter, fed by the dma-buf test source:
# the plain mode shows the guest's buffers as they are, an upscaler and frame
# generation draw window-size buffers (docs/VIEWER-ENHANCE.md, "Without
# wp_viewporter").  No display, no VM.
#
#   bash headless_gamescope_test.sh [MODE...]     MODE: off fsr nis cas fg2x fg3x
#
# Per mode it asserts from the viewer's --stats lines: off -- frames
# committed by direct attach; an upscaler -- every frame through the GPU
# pass, the asked-for pass ran; fg2x/fg3x -- generated pictures shown
# besides the guest's.  It prints those lines, so the numbers (pass time,
# shown X/s guest + Y/s generated) can be read off too.
#
# GS_NODE: the render node for the frames, the viewer's pass and gamescope
# (default: the first non-NVIDIA one, else the first); GS_SIZE (1920x1080),
# GS_GUEST (1280x720), GS_FRAMES (180), GS_INTERVAL_MS (33: 30 fps, so 2x
# fits under a 60 Hz output).
#
# Exits 0 on pass, 1 on failure, 77 on skip.
set -u

DIR="$(cd "$(dirname "$0")" && pwd)"
BROKER="$DIR/conduit-viewer"
DMASRC="$DIR/nvkvm-broker-dmabuf-src"
SIZE="${GS_SIZE:-1920x1080}"
GUEST="${GS_GUEST:-1280x720}"
FRAMES="${GS_FRAMES:-180}"
IVAL="${GS_INTERVAL_MS:-33}"
MODES=("$@")
[ ${#MODES[@]} -gt 0 ] || MODES=(off fsr fg2x)
rc=0

skip() { echo "SKIP: $*"; exit 77; }

command -v gamescope >/dev/null || skip "gamescope is not installed"
[ -x "$BROKER" ] || skip "viewer not built ($BROKER)"
[ -x "$DMASRC" ] || skip "dma-buf source not built -- needs libgbm at build time"

NODE="${GS_NODE:-}"
if [ -z "$NODE" ]; then
    for n in /dev/dri/renderD*; do
        [ -e "$n" ] || continue
        drv=$(sed -n 's/^DRIVER=//p' "/sys/class/drm/$(basename "$n")/device/uevent" 2>/dev/null)
        [ -z "$NODE" ] && NODE="$n"
        if [ "$drv" != "nvidia" ]; then NODE="$n"; break; fi
    done
fi
[ -n "$NODE" ] || skip "no render node"
sys="/sys/class/drm/$(basename "$NODE")/device"
VKDEV="$(sed 's/^0x//' "$sys/vendor" 2>/dev/null):$(sed 's/^0x//' "$sys/device" 2>/dev/null)"
# gamescope's own GPU: the same one, by its Vulkan driver (--prefer-vk-device
# alone is not always honoured).
case "${VKDEV%%:*}" in
1002) ICD=radeon_icd.json ;;
8086) ICD=intel_icd.json ;;
10de) ICD=nvidia_icd.json ;;
*)    ICD= ;;
esac
for d in /usr/share/vulkan/icd.d /etc/vulkan/icd.d; do
    if [ -n "$ICD" ] && [ -f "$d/$ICD" ] && [ -z "${VK_ICD_FILENAMES:-}" ]; then
        export VK_ICD_FILENAMES="$d/$ICD"
    fi
done
W="${SIZE%x*}"
H="${SIZE#*x}"

WORK="$(mktemp -d)"
chmod 700 "$WORK"
GPID=""
SPID=""
cleanup() {
    [ -n "$SPID" ] && kill "$SPID" 2>/dev/null
    [ -n "$GPID" ] && { kill "$GPID" 2>/dev/null; sleep 1; kill -9 "$GPID" 2>/dev/null; }
    rm -rf "$WORK"
}
trap cleanup EXIT

# Nothing of the session's display may leak in: gamescope's headless backend
# makes its own Wayland socket and hands it to its child.
unset WAYLAND_DISPLAY DISPLAY WAYLAND_SOCKET

run_mode() {
    local mode=$1 args=() log="$WORK/$1.log" sock="$WORK/$1.sock"

    case "$mode" in
    off)  args=() ;;
    fsr|nis|cas) args=(--upscale="$mode" --sharpness=50) ;;
    fg2x) args=(--framegen=2x) ;;
    fg3x) args=(--framegen=3x) ;;
    *) echo "unknown mode $mode"; return 1 ;;
    esac
    echo "== $mode: gamescope --backend headless ${W}x${H}, guest $GUEST, $NODE =="
    XDG_RUNTIME_DIR="$WORK" gamescope --backend headless -W "$W" -H "$H" \
        --prefer-vk-device "$VKDEV" --expose-wayland -- \
        "$BROKER" --socket "$sock" --socket-mode 0600 --backend wayland \
        --fullscreen --size "$SIZE" --stats --gpu "$NODE" "${args[@]}" \
        >"$log" 2>&1 &
    GPID=$!
    for _ in $(seq 80); do [ -S "$sock" ] && break; sleep 0.25; done
    if [ ! -S "$sock" ]; then
        echo "FAIL: the viewer never listened under gamescope"
        tail -15 "$log"
        kill "$GPID" 2>/dev/null; GPID=""
        return 1
    fi
    "$DMASRC" --node "$NODE" --present "$sock" --modifier linear \
              --size "$GUEST" --frames "$FRAMES" --interval-ms "$IVAL" \
              >"$WORK/$mode.src.log" 2>&1 &
    SPID=$!
    sleep $(( FRAMES * IVAL / 1000 + 3 ))
    kill "$SPID" 2>/dev/null; wait "$SPID" 2>/dev/null; SPID=""
    kill "$GPID" 2>/dev/null; sleep 1; kill -9 "$GPID" 2>/dev/null
    wait "$GPID" 2>/dev/null; GPID=""

    grep -E "wp_viewporter|upscaling:|frame generation:|gpu-scale: ready" "$log" | sed 's/^/  /' | head -8
    grep "stats:" "$log" | tail -2 | sed 's/^/  /'
    local commits
    commits=$(grep -o 'stats: commit [0-9.]*' "$log" | awk '{ s += $3 } END { print s + 0 }')
    if [ "${commits%.*}" -lt 1 ]; then
        echo "FAIL: $mode: no frames committed"
        tail -10 "$log"
        return 1
    fi
    case "$mode" in
    off)
        grep -q "path: direct attach" "$log" ||
            { echo "FAIL: off: frames did not go as they are"; return 1; } ;;
    fsr|nis|cas)
        grep -q "gpu-scale [0-9]* frames, pass avg" "$log" ||
            { echo "FAIL: $mode: no frame through the GPU pass"; return 1; }
        if grep -E "upscaling: .* asked, .* runs" "$log" | grep -q "Off runs"; then
            echo "FAIL: $mode: fell back to the plain pass"; return 1
        fi ;;
    fg2x|fg3x)
        local gen
        gen=$(grep -o 'guest + [0-9.]*/s generated' "$log" | awk '{ s += $3 } END { print s + 0 }')
        if [ "${gen%.*}" -lt 1 ]; then
            echo "FAIL: $mode: no generated pictures shown"; return 1
        fi ;;
    esac
    echo "  PASS $mode"
    return 0
}

for m in "${MODES[@]}"; do
    run_mode "$m" || rc=1
done
exit $rc
