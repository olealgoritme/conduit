#!/bin/sh
# Build dispatch-bench for Linux and Windows (MinGW-w64).
#
#   guest/nvk-rm/tests/perf/dispatch-bench/build.sh [OUT_DIR]
#
# OUT_DIR defaults to ./dispatch-bench-out. Both binaries load the Vulkan
# loader at run time (libvulkan.so.1, vulkan-1.dll), so only the Vulkan
# headers are needed: VULKAN_INCLUDE (default /usr/include). The Windows
# build is skipped when x86_64-w64-mingw32-gcc is missing.
#
# Needs: glslangValidator, xxd, cc.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
OUT_DIR=${1:-$PWD/dispatch-bench-out}
VULKAN_INCLUDE=${VULKAN_INCLUDE:-/usr/include}
CC=${CC:-cc}
MINGW_CC=${MINGW_CC:-x86_64-w64-mingw32-gcc}

mkdir -p "$OUT_DIR/include"
# cs_simple.h / cs_rich.h: the SPIR-V as cs_<name>_spv[] and cs_<name>_spv_len
for s in cs_simple cs_rich; do
  glslangValidator -V --target-env vulkan1.1 -o "$OUT_DIR/$s.spv" "$here/$s.comp" >/dev/null
  (cd "$OUT_DIR" && xxd -i "$s.spv" > "include/$s.h")
done

"$CC" -O2 -Wall -I"$OUT_DIR/include" -I"$VULKAN_INCLUDE" \
  -o "$OUT_DIR/dispatch-bench" "$here/dispatch-bench.c" -ldl
echo "built $OUT_DIR/dispatch-bench"

if command -v "$MINGW_CC" >/dev/null 2>&1; then
  # Only the vulkan/ and vk_video/ headers, not the host's whole /usr/include
  mkdir -p "$OUT_DIR/include-win"
  cp -r "$VULKAN_INCLUDE/vulkan" "$VULKAN_INCLUDE/vk_video" "$OUT_DIR/include-win/"
  "$MINGW_CC" -O2 -Wall -I"$OUT_DIR/include" -I"$OUT_DIR/include-win" \
    -o "$OUT_DIR/dispatch-bench.exe" "$here/dispatch-bench.c" -static-libgcc
  cp "$here/run-guest.cmd" "$OUT_DIR/"
  echo "built $OUT_DIR/dispatch-bench.exe"
else
  echo "no $MINGW_CC: Windows build skipped"
fi
