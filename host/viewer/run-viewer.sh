#!/bin/bash
# run-viewer.sh -- start the Conduit zero-copy scanout viewer (the
# vendored nvkvm display broker) in the current Wayland session.
#
#   host/viewer/run-viewer.sh                 # listen on $NVGPU_DISPLAY_SOCK
#   NVGPU_VIEWER_FULLSCREEN=1 run-viewer.sh   # start fullscreen
#   NVGPU_VIEWER_RESIZE=scale run-viewer.sh   # keep the guest mode, scale it
#   NVGPU_VIEWER_DIRECT=1 run-viewer.sh       # start in direct mode
#   run-viewer.sh --overlay=windowed          # extra args go to the broker
#
# The backend then connects with:
#   conduit-backend ... --display-socket "$NVGPU_DISPLAY_SOCK"
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/conduit-viewer"
SOCK="${NVGPU_DISPLAY_SOCK:-${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/nvgpu-display.sock}"
SIZE="${NVGPU_VIEWER_SIZE:-2560x1440}"

[ -x "$BIN" ] || make -C "$DIR" >/dev/null
[ -n "${WAYLAND_DISPLAY:-}" ] || { echo "run-viewer: WAYLAND_DISPLAY is not set (run inside the Hyprland session)" >&2; exit 1; }

# A stale socket from a previous run would make bind() fail.
if [ -S "$SOCK" ] && ! fuser -s "$SOCK" 2>/dev/null; then rm -f "$SOCK"; fi

ARGS=(--backend wayland
      --socket "$SOCK"
      --size "$SIZE"
      --title "conduit"
      --present-mode=native      # zero-copy or a loud dropped frame; never shm
      --scale aspect
      --persist                  # survive backend/VM restarts
      --stats)
[ "${NVGPU_VIEWER_FULLSCREEN:-0}" = 1 ] && ARGS+=(--fullscreen)
ARGS+=(--resize="${NVGPU_VIEWER_RESIZE:-guest}")
[ "${NVGPU_VIEWER_DIRECT:-0}" = 1 ] && ARGS+=(--direct-mode=on)
[ -n "${NVGPU_VIEWER_DIRECT_HOOK:-}" ] && ARGS+=(--direct-hook "$NVGPU_VIEWER_DIRECT_HOOK")

echo "run-viewer: listening on $SOCK  (CTRL+ALT+F fullscreen, G grab, O overlay, D direct mode, R resize policy)"
exec "$BIN" "${ARGS[@]}" "$@"
