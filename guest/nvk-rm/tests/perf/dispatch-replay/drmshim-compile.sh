#!/bin/bash
# Compile a capture's shader with a Linux NVK build under the nouveau drm-shim (no GPU): checks that the
# pipeline builds and, with NAK_DEBUG=print in the environment, prints NAK's code.
#   drmshim-compile.sh CAPTURE_DIR MESA_BUILD_DIR [CHIPSET]
# MESA_BUILD_DIR is a Mesa build with -Dvulkan-drivers=nouveau and the drm-shim built
# (src/nouveau/drm-shim, src/nouveau/vulkan/nouveau_devenv_icd.*.json). CHIPSET defaults to 1b2 (GB202).
set -eu
cap=${1:?capture dir}; build=${2:?Mesa build dir}; chip=${3:-1b2}
here=$(cd "$(dirname "$0")" && pwd)
LD_PRELOAD=$build/src/nouveau/drm-shim/libnouveau_noop_drm_shim.so NOUVEAU_CHIPSET=$chip \
VK_ICD_FILENAMES=$build/src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json NVK_I_WANT_A_BROKEN_VULKAN_DRIVER=1 \
    "$here/dispatch-replay" --compile-only --dev NVK "$cap"
