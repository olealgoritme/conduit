#!/bin/sh
# Compile a SPIR-V corpus with NVK + NAK on a faked GPU and write NAK's
# statistics (the VK_KHR_pipeline_executable_properties numbers, the same
# ones NVK_SHADER_STATS prints) per shader.
#
#   guest/nvk-rm/tools/nak-offline/run.sh MESA_DIR/BUILD_DIR CORPUS OUT_DIR
#
# CORPUS holds (paths in the list files are relative to CORPUS):
#   all.txt       SPIR-V files to try as compute shaders (others are skipped)
#   fs-pairs.txt  "fs.spv vs.spv <color kinds> <depth>" lines (genvs.py)
#   vs-pairs.txt  "vs.spv fs.spv <attrs>" lines (genfs.py)
# OUT_DIR gets compute.txt, frag.txt and vert.txt (nvstats format).
#
# Environment passes through, so NAK knobs (NAK_OCC_TARGET, NAK_GPR_CAP, ...)
# apply.  NOUVEAU_CHIPSET selects the GPU (default 1b2: GB202, SM 120).
# NVK_UBO_DESC_CBUF defaults to 0, the Windows default, so the numbers match
# the Windows driver's.
set -eu
B=$(cd "${1:?usage: run.sh BUILD CORPUS OUT_DIR}" && pwd)
CORPUS=$(cd "${2:?CORPUS}" && pwd)
OUT=${3:?OUT_DIR}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

export VK_ICD_FILENAMES="$B/src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json"
export LD_PRELOAD="$B/src/nouveau/drm-shim/libnouveau_noop_drm_shim.so"
export NOUVEAU_CHIPSET="${NOUVEAU_CHIPSET:-1b2}"
export NVK_UBO_DESC_CBUF="${NVK_UBO_DESC_CBUF:-0}"
export MESA_SHADER_CACHE_DISABLE=true

cd "$CORPUS"
# shellcheck disable=SC2046
"$B/nvstats" $(cat all.txt) > "$OUT/compute.txt" 2> "$OUT/compute.err"
[ -f fs-pairs.txt ] && "$B/nvstats" -g fs-pairs.txt > "$OUT/frag.txt" 2> "$OUT/frag.err"
[ -f vs-pairs.txt ] && "$B/nvstats" -v vs-pairs.txt > "$OUT/vert.txt" 2> "$OUT/vert.err"
wc -l "$OUT"/*.txt
