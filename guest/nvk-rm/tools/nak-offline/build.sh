#!/bin/sh
# Build the offline NAK compiler loop: NVK for Linux (from a Mesa checkout
# with the NVK-on-RM series applied, Linux or Windows stack), nouveau's
# drm-shim, NAK's unit tests, and nvstats.
#
#   guest/nvk-rm/tools/nak-offline/build.sh MESA_DIR [BUILD_DIR]
#
# MESA_DIR   Mesa checkout with the series applied (git am'd)
# BUILD_DIR  meson build directory inside MESA_DIR (default: build-nak-offline)
#
# Environment: MESON (meson >= 1.4), JOBS (ninja -j, default 4).
# No GPU is needed: run.sh preloads the drm-shim, which fakes a nouveau
# device of any chipset, and NVK compiles shaders exactly as on hardware.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
MESA_DIR=$(cd "${1:?usage: build.sh MESA_DIR [BUILD_DIR]}" && pwd)
BUILD_DIR=${2:-build-nak-offline}
MESON=${MESON:-meson}
JOBS=${JOBS:-4}

cd "$MESA_DIR"
if [ ! -f "$BUILD_DIR/build.ninja" ]; then
  "$MESON" setup "$BUILD_DIR" \
    -Dvulkan-drivers=nouveau -Dgallium-drivers= -Dplatforms= \
    -Dvideo-codecs= -Dtools=drm-shim -Dbuild-tests=true \
    -Dbuildtype=release -Db_ndebug=true
fi
nice -n 19 ninja -C "$BUILD_DIR" -j"$JOBS" \
  src/nouveau/vulkan/libvulkan_nouveau.so \
  src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json \
  src/nouveau/drm-shim/libnouveau_noop_drm_shim.so \
  src/nouveau/compiler/nak

cc -O2 -o "$MESA_DIR/$BUILD_DIR/nvstats" "$here/nvstats.c" -lvulkan
echo "nak-offline: built $MESA_DIR/$BUILD_DIR (nvstats, NVK, drm-shim, nak tests)"
