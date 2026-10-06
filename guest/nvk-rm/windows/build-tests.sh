#!/bin/sh
# Cross-compile the NVK-on-RM Vulkan tests for Windows (MinGW-w64).
#
#   guest/nvk-rm/windows/build-tests.sh [OUT_DIR]     (default: ./nvk-tests-win)
#
# Builds icd_smoke.exe, vk_summary.exe, vk_compute_test.exe,
# vk_offscreen_test.exe and vk_scanout_present.exe plus their SPIR-V
# (glslangValidator). MinGW ships no Vulkan import library: one for
# vulkan-1.dll is generated from the Vulkan headers' prototypes (only the
# functions a test calls end up imported, all of them loader exports).
#
# Needs: gcc-mingw-w64-x86-64, the Vulkan headers (VULKAN_INCLUDE, default
# /usr/include; only its vulkan/ and vk_video/ are used), glslangValidator.
#
# In the guest (an elevated session ignores VK_DRIVER_FILES, hence
# VK_DIRECT_DRIVER, see tests/vk_direct_driver.h):
#   set NVK_RM=1
#   set VK_DIRECT_DRIVER=C:\path\to\vulkan_nouveau.dll
#   vk_summary.exe & vk_compute_test.exe compute.spv copy & vk_offscreen_test.exe 100
#   vk_scanout_present.exe 120     (spinning triangle on the scanout, 120 s)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
tests="$here/../tests"
OUT_DIR=${1:-$PWD/nvk-tests-win}
VULKAN_INCLUDE=${VULKAN_INCLUDE:-/usr/include}
CC=${CC:-x86_64-w64-mingw32-gcc}
DLLTOOL=${DLLTOOL:-x86_64-w64-mingw32-dlltool}

mkdir -p "$OUT_DIR/include"
cp -r "$VULKAN_INCLUDE/vulkan" "$VULKAN_INCLUDE/vk_video" "$OUT_DIR/include/"

{
  echo "LIBRARY vulkan-1.dll"
  echo "EXPORTS"
  cat "$VULKAN_INCLUDE/vulkan/vulkan_core.h" "$VULKAN_INCLUDE/vulkan/vulkan_win32.h" |
    sed -n 's/^VKAPI_ATTR [A-Za-z0-9_]* VKAPI_CALL \(vk[A-Za-z0-9]*\)(.*/\1/p' | sort -u
} > "$OUT_DIR/vulkan-1.def"
"$DLLTOOL" -d "$OUT_DIR/vulkan-1.def" -l "$OUT_DIR/libvulkan-1.a"

glslangValidator -V "$tests/compute.comp" -o "$OUT_DIR/compute.spv" >/dev/null
glslangValidator -V "$tests/triangle.vert" -o "$OUT_DIR/triangle.vert.spv" >/dev/null
glslangValidator -V "$tests/triangle.frag" -o "$OUT_DIR/triangle.frag.spv" >/dev/null
glslangValidator -V "$tests/spin.vert" -o "$OUT_DIR/spin.vert.spv" >/dev/null

for t in vk_summary vk_compute_test vk_offscreen_test vk_scanout_present; do
  "$CC" -O1 -Wall -I"$OUT_DIR/include" "$tests/$t.c" -L"$OUT_DIR" -lvulkan-1 -lm -o "$OUT_DIR/$t.exe"
done
"$CC" -O1 -Wall -I"$OUT_DIR/include" "$here/icd_smoke.c" -o "$OUT_DIR/icd_smoke.exe"

rm -rf "$OUT_DIR/include" "$OUT_DIR/vulkan-1.def" "$OUT_DIR/libvulkan-1.a"
ls -l "$OUT_DIR"
