#!/bin/sh
# Compile every SPIR-V file of a directory on its own, any stage (tessellation
# and geometry included), as VK_EXT_shader_object shader objects: no pipeline
# state and no partner stage needed, for corpora dumped with
# MESA_SPIRV_DUMP_PATH (files named <hash>.<vert|tesc|tese|geom|frag|comp>).
# Statistics come from NVK_SHADER_STATS.
#
#   run-objects.sh BUILD DIR OUT_FILE
#
# OUT_FILE gets "file stage instrs gprs warps static_cycles spills fills" per
# shader, or FAILED.  PAR (default 4) processes in parallel, 200 shaders each.
# Environment passes through, so NAK knobs apply.  Compare two runs with
# objcmp.py.
set -eu
B=$(cd "${1:?usage: run-objects.sh BUILD DIR OUT_FILE}" && pwd)
D=$(cd "${2:?DIR}" && pwd)
OUT=$(realpath -m "${3:?OUT_FILE}")
here=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
export VK_ICD_FILENAMES="$B/src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json"
export LD_PRELOAD="$B/src/nouveau/drm-shim/libnouveau_noop_drm_shim.so"
export NOUVEAU_CHIPSET="${NOUVEAU_CHIPSET:-1b2}"
export NVK_UBO_DESC_CBUF="${NVK_UBO_DESC_CBUF:-0}"
export MESA_SHADER_CACHE_DISABLE=true NVK_SHADER_STATS=1
cd "$D"
ls | grep -E '\.(spv|vert|tesc|tese|geom|frag|comp)$' > "$T/all" || true
split -l 200 "$T/all" "$T/batch."
export B
ls "$T"/batch.* | xargs -P "${PAR:-4}" -I{} sh -c \
  'NVK_SHADER_STATS_FILE={}.st "$B/nvstats" -s $(cat {}) > {}.out 2> {}.err || true'
python3 "$here/objmerge.py" "$T" > "$OUT"
