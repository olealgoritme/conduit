#!/usr/bin/env bash
# Compile a public shader corpus through NVK and NAK on faked GPUs (nouveau's
# drm-shim) and compare NAK's statistics with a baseline build.
#
#   .github/scripts/nak-shaders.sh HEAD_BUILD [BASE_BUILD]
#
# HEAD_BUILD, BASE_BUILD  directories nak-build.sh staged (the baseline is
#                         optional; without it only totals are reported)
#
# The corpus, compiled to SPIR-V with glslangValidator:
#   nak    guest/nvk-rm/tests/nak-corpus/ (NAK-specific cases)
#   tests  guest/nvk-rm/tests/*.comp, *.vert, *.frag
#   dxvk, vkd3d  the built-in GLSL shaders of the DXVK and vkd3d-proton
#          submodules (guest/windows/third_party), when they are checked out
# Game and benchmark shaders are not part of it.
#
# Each shader is compiled alone as a VK_EXT_shader_object shader object
# (tools/nak-offline/run-objects.sh, one process per shader) for every
# chipset in CHIPSETS, and once more on the first with NAK_DEBUG=spill, which
# makes NAK spill nearly everything. The statistics are NVK's own
# (NVK_SHADER_STATS): instructions, GPRs, warps per SM, static cycles,
# spills and fills.
#
# Environment: OUT_DIR (default ./nak-shaders-out), CHIPSETS (default
# "1b2 192": GB202/SM120 and AD102/SM89), GLSLANG (default glslangValidator),
# DXVK_DIR and VKD3D_DIR (other DXVK and vkd3d-proton checkouts).
# Writes OUT_DIR/summary.md and appends it to $GITHUB_STEP_SUMMARY when set.
# Exits 1 when a shader of our own does not compile to SPIR-V, or when NAK
# fails any shader in HEAD_BUILD.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
head_build=$(cd "${1:?usage: nak-shaders.sh HEAD_BUILD [BASE_BUILD]}" && pwd)
base_build=${2:-}
[ -z "$base_build" ] || base_build=$(cd "$base_build" && pwd)
out=${OUT_DIR:-$PWD/nak-shaders-out}
read -r -a chipsets <<< "${CHIPSETS:-1b2 192}"
GLSLANG=${GLSLANG:-glslangValidator}
tools=$root/guest/nvk-rm/tools/nak-offline

rm -rf "$out"
mkdir -p "$out/corpus" "$out/glslang"
corpus=$out/corpus
status=0

# compile GROUP REQUIRED FILE...: SPIR-V named GROUP.NAME.STAGE in the corpus
compile() {
  local group=$1 required=$2 f name
  shift 2
  for f in "$@"; do
    name=$group.$(basename "$f")
    # NVK has no VK_EXT_shader_stencil_export; vkd3d-proton uses its
    # *_no_export variants there
    if grep -q gl_FragStencilRef "$f"; then
      echo "$name (stencil export)" >> "$out/skipped.txt"
      continue
    fi
    if ! "$GLSLANG" -V --quiet --target-env vulkan1.3 -I"$(dirname "$f")" \
         -o "$corpus/$name" "$f" > "$out/glslang/$name.log" 2>&1; then
      rm -f "$corpus/$name"
      echo "$name" >> "$out/glslang-failed.txt"
      if [ "$required" = 1 ]; then
        echo "::error file=${f#"$root"/}::does not compile to SPIR-V"
        status=1
      fi
      cat "$out/glslang/$name.log"
    fi
  done
}

shaders_in() { # dir...: GLSL shader sources below the directories
  find "$@" -type f \( -name '*.comp' -o -name '*.vert' -o -name '*.frag' \
       -o -name '*.geom' -o -name '*.tesc' -o -name '*.tese' \) | LC_ALL=C sort
}

mapfile -t files < <(shaders_in "$root/guest/nvk-rm/tests/nak-corpus")
compile nak 1 "${files[@]}"
mapfile -t files < <(find "$root/guest/nvk-rm/tests" -maxdepth 1 -type f \
                       \( -name '*.comp' -o -name '*.vert' -o -name '*.frag' \) | LC_ALL=C sort)
compile tests 1 "${files[@]}"
dxvk=${DXVK_DIR:-$root/guest/windows/third_party/dxvk}
vkd3d=${VKD3D_DIR:-$root/guest/windows/third_party/vkd3d-proton}
if [ -d "$dxvk/src" ]; then
  mapfile -t dirs < <(find "$dxvk/src" -type d -name shaders)
  mapfile -t files < <(shaders_in "${dirs[@]}")
  compile dxvk 0 "${files[@]}"
fi
if [ -d "$vkd3d/libs/vkd3d/shaders" ]; then
  mapfile -t files < <(shaders_in "$vkd3d/libs/vkd3d/shaders")
  compile vkd3d 0 "${files[@]}"
fi
echo "nak-shaders: $(find "$corpus" -type f | wc -l) shaders in the corpus"

# run BUILD NAME [ENV...]: statistics of every corpus shader, one process each
run() {
  local build=$1 name=$2
  shift 2
  env "$@" BATCH=1 PAR="$(nproc)" sh "$tools/run-objects.sh" "$build" "$corpus" "$out/$name.txt"
  echo "nak-shaders: $name: $(grep -c FAILED "$out/$name.txt" || true) failed of $(wc -l < "$out/$name.txt")"
}
for chip in "${chipsets[@]}"; do
  run "$head_build" "head-$chip" NOUVEAU_CHIPSET="$chip"
  [ -z "$base_build" ] || run "$base_build" "base-$chip" NOUVEAU_CHIPSET="$chip"
done
run "$head_build" "head-spill-${chipsets[0]}" NOUVEAU_CHIPSET="${chipsets[0]}" NAK_DEBUG=spill

# Why each failure failed: the shader alone, with its stderr (NAK's panic
# message or NVK's error)
: > "$out/failures.txt"
for f in "$out"/head-*.txt; do
  run_name=$(basename "$f" .txt)
  chip=${run_name##*-}
  extra=()
  case $run_name in head-spill-*) extra=(NAK_DEBUG=spill) ;; esac
  while read -r shader _; do
    {
      echo "### $shader ($run_name)"
      (cd "$corpus" && env "${extra[@]}" NOUVEAU_CHIPSET="$chip" NVK_UBO_DESC_CBUF=0 \
         MESA_SHADER_CACHE_DISABLE=true \
         VK_ICD_FILENAMES="$head_build/src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json" \
         LD_PRELOAD="$head_build/src/nouveau/drm-shim/libnouveau_noop_drm_shim.so" \
         timeout 120 "$head_build/nvstats" -s "$shader" 2>&1 | tail -n 15) || true
    } >> "$out/failures.txt"
    echo "::error::NAK: $shader does not compile ($run_name)"
    status=1
  done < <(grep FAILED "$f" || true)
done

python3 "$root/.github/scripts/nak-stats.py" "$out" "$head_build" "$base_build" "${chipsets[@]}" > "$out/summary.md"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  cat "$out/summary.md" >> "$GITHUB_STEP_SUMMARY"
fi
cat "$out/summary.md"
exit "$status"
