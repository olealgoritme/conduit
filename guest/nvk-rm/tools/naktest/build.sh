#!/bin/sh
# Build naktest for Linux (naktest) and Windows (naktest.exe):
#
#   guest/nvk-rm/tools/naktest/build.sh [OUT_DIR] [SEED]
#
# Needs python3, glslangValidator, a C compiler with the Vulkan headers and,
# for the .exe, x86_64-w64-mingw32-gcc (the .exe loads vulkan-1.dll at run
# time, so it needs no import library).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
OUT=${1:-$here/out}
SEED=${2:-1}
mkdir -p "$OUT/gen"
python3 "$here/gen.py" "$OUT/gen" "$SEED"
for f in "$OUT"/gen/*.comp; do
  n=$(basename "$f" .comp)
  glslangValidator -V --target-env vulkan1.1 --vn "spv_$n" -o "$OUT/gen/$n.h" "$f" > /dev/null
done
cp "$here/naktest.c" "$OUT/"
${CC:-cc} -O2 -Wall -Wno-tautological-compare -I"$OUT" -o "$OUT/naktest" "$OUT/naktest.c" -lvulkan
if command -v x86_64-w64-mingw32-gcc > /dev/null; then
  # The Vulkan headers only, not the host's whole include directory
  VKINC=${VULKAN_INCLUDE:-/usr/include}
  mkdir -p "$OUT/inc"
  cp -r "$VKINC/vulkan" "$OUT/inc/"
  [ -d "$VKINC/vk_video" ] && cp -r "$VKINC/vk_video" "$OUT/inc/"
  x86_64-w64-mingw32-gcc -O2 -Wall -Wno-tautological-compare -I"$OUT" -I"$OUT/inc" \
    -o "$OUT/naktest.exe" "$OUT/naktest.c" -static
fi
ls -la "$OUT"/naktest*
