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
# the scalers: their shaders need the adapted includes
gen=$tmp/gen
mkdir -p "$gen"
python3 "$enh/adapt.py" sgsr "$enh/third_party/sgsr/sgsr1_shader_mobile_edge_direction.frag" "$gen/sgsr1_adapted.glsl"
python3 "$enh/adapt.py" xbr "$enh/third_party/xbr/xbr-lv2-standalone.slang" "$gen/xbr_lv2_adapted.glsl"
python3 "$enh/adapt.py" nis-coef "$enh/third_party/nis/NIS_Config.h" "$gen/nis_coef.glsl"
for s in enh_scale enh_easu enh_rcas enh_nis enh_cas enh_sgsr enh_xbr; do
  glslangValidator -V --target-env vulkan1.1 -S comp -I"$enh" -I"$gen" -I"$enh/third_party/fsr1" \
    -I"$enh/third_party/cas" -I"$enh/third_party/nis" -o "$tmp/$s.spv" "$enh/$s.comp" >/dev/null
done
: > "$gen/a4k_all.h"
for v in 3d:Anime4K_3DGraphics_Upscale_x2_US.glsl m:Anime4K_Upscale_CNN_x2_M.glsl vl:Anime4K_Upscale_CNN_x2_VL.glsl; do
  n=${v%%:*}; f=${v#*:}
  python3 "$enh/adapt.py" a4k $n "$enh/third_party/anime4k/$f" "$gen/a4k_$n.glsl" "$gen/a4k_${n}_passes.h"
  np=$(sed -n "s/#define A4K_.*_PASSES //p" "$gen/a4k_${n}_passes.h")
  i=0
  while [ $i -lt $np ]; do
    glslangValidator -V --target-env vulkan1.1 -S comp -DA4K_PASS=$i --vn a4k_${n}_${i}_spv \
      -o "$gen/a4k_${n}_${i}_spv.h" "$gen/a4k_$n.glsl" >/dev/null
    echo "#include \"a4k_${n}_${i}_spv.h\"" >> "$gen/a4k_all.h"
    i=$((i + 1))
  done
  echo "#include \"a4k_${n}_passes.h\"" >> "$gen/a4k_all.h"
done
cc -O2 -Wall -DHAVE_A4K -I"$gen" -o "$tmp/scale_eval" "$here/scale_eval.c" -lvulkan -lm
echo "== scalers"
if [ -n "$out" ]; then mkdir -p "$out/scale"; "$tmp/scale_eval" "$tmp" "$out/scale"; else "$tmp/scale_eval" "$tmp"; fi
