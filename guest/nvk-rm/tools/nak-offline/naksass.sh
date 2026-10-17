#!/bin/sh
# NAK's machine code for corpus shaders, disassembled with nvdisasm like the
# reference driver's (refsass.py), from NAK_DEBUG=print's "Encoded shader".
#
#   naksass.sh BUILD CORPUS OUT_DIR HASH... 
#
# HASH: a shader's file name prefix in CORPUS/shaderdump; fragment shaders
# are compiled with their partner from fs-pairs.txt.  Writes
# OUT_DIR/<hash>.<stage>.bin and .sass.  NAK knobs in the environment apply.
# nvdisasm is $NVDISASM or the one on PATH (CUDA 12.8 or newer for SM120).
set -eu
B=$(cd "${1:?usage: naksass.sh BUILD CORPUS OUT_DIR HASH...}" && pwd)
CORPUS=$(cd "${2:?CORPUS}" && pwd)
mkdir -p "${3:?OUT_DIR}"; OUT=$(cd "$3" && pwd); shift 3
here=$(cd "$(dirname "$0")" && pwd)
ND=${NVDISASM:-nvdisasm}
export VK_ICD_FILENAMES="$B/src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json"
export LD_PRELOAD="$B/src/nouveau/drm-shim/libnouveau_noop_drm_shim.so"
export NOUVEAU_CHIPSET="${NOUVEAU_CHIPSET:-1b2}"
export NVK_UBO_DESC_CBUF="${NVK_UBO_DESC_CBUF:-0}"
export MESA_SHADER_CACHE_DISABLE=true NAK_DEBUG=print
cd "$CORPUS"
for h in "$@"; do
  f=$(ls shaderdump/"$h"*.spv | head -1)
  if grep -q "^$f " fs-pairs.txt; then
    grep "^$f " fs-pairs.txt > "$OUT/$h.pair"
    "$B/nvstats" -g "$OUT/$h.pair" > "$OUT/$h.stats" 2> "$OUT/$h.log"
  else
    "$B/nvstats" "$f" > "$OUT/$h.stats" 2> "$OUT/$h.log"
  fi
  python3 "$here/enc2bin.py" "$OUT/$h.log" "$OUT/$h"
  for b in "$OUT/$h".*.bin; do "$ND" -b SM120 -hex "$b" > "${b%.bin}.sass"; done
  rm -f "$OUT/$h.log"
done
