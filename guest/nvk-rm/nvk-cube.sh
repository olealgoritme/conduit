#!/bin/bash
# vkcube on NVK over RM (Conduit), not NVIDIA's own Vulkan driver.
#
#   ~/nvk-cube.sh [--sw] [--wsi xcb|wayland] [other vkcube arguments]
#
# Default: zero-copy presentation. NVK's swapchain images stay in VRAM
# (block-linear, NVIDIA DRM format modifiers) and go to Xwayland / the
# Wayland compositor as dma-bufs through Conduit's nvidia-drm node.
# --sw: Mesa's software WSI instead (a CPU copy per frame, the first-run path).
# Run from an ssh shell it shows on the desktop session (DISPLAY=:0).
set -u

export NVK_RM=1
# The desktop session sets VK_DRIVER_FILES (NVIDIA's ICD), which wins over
# VK_ICD_FILENAMES: set both.
export VK_DRIVER_FILES=$HOME/nvk-prefix/share/vulkan/icd.d/nouveau_icd.x86_64.json
export VK_ICD_FILENAMES=$VK_DRIVER_FILES
export NVK_RMCLIENT_LIB=$HOME/nvk-rm/rmclient/build-make/librmclient.so

wsi=xcb
args=()
mode="zero-copy (dma-buf, NVIDIA block-linear modifiers)"
while [ $# -gt 0 ]; do
  case $1 in
    --sw) export MESA_VK_WSI_DEBUG=sw; mode="software (CPU copy per frame)" ;;
    --wsi) wsi=$2; shift ;;
    --wsi=*) wsi=${1#--wsi=} ;;
    *) args+=("$1") ;;
  esac
  shift
done

# From ssh: use the desktop session's displays
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
if [ -z "${DISPLAY:-}" ]; then
  export DISPLAY=:0
  auth=$(ls "$XDG_RUNTIME_DIR"/.mutter-Xwaylandauth.* 2>/dev/null | head -1)
  [ -n "$auth" ] && export XAUTHORITY=$auth
fi
export WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-0}

vulkaninfo --summary 2>/dev/null | grep -m1 deviceName
echo "presentation: $mode, WSI $wsi"
exec vkcube --wsi "$wsi" "${args[@]}"
