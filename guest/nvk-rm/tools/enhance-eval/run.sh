#!/bin/sh
# run.sh MESA_DIR [OUT_DIR]: compile the frame-generation shaders of a Mesa
# tree with the series applied, build fg_eval and run both flow tiers.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
enh=${1:?MESA_DIR}/src/vulkan/wsi/enhance
out=${2:-}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for s in fg_down fg_flow fg_interp; do
  glslangValidator -V --target-env vulkan1.1 -S comp -I"$enh" -o "$tmp/$s.spv" "$enh/$s.comp" >/dev/null
done
cc -O2 -Wall -o "$tmp/fg_eval" "$here/fg_eval.c" -lvulkan -lm
for tier in fast quality; do
  echo "== $tier"
  if [ -n "$out" ]; then mkdir -p "$out/$tier"; "$tmp/fg_eval" "$tmp" $tier "$out/$tier"; else "$tmp/fg_eval" "$tmp" $tier; fi
done
