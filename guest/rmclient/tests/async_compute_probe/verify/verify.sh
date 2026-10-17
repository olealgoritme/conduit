#!/bin/sh
# Compares the probe's compute push words with Mesa's generated macros.
#   verify.sh MESA_DIR MESA_BUILD_DIR
set -eu
here=$(cd "$(dirname "$0")" && pwd)
M=${1:?MESA_DIR}; B=${2:?MESA_BUILD_DIR}
python3 - "$here/.." "$here" <<'PY'
import sys
s=open(sys.argv[1]+'/async_compute_probe.c').read()
a=s.index('static unsigned emit_compute_setup'); b=s.index('static uint32_t *slot(struct chan')
open(sys.argv[2]+'/emit_snippets.h','w').write(s[a:b])
PY
cc -std=gnu11 -w -I"$M/src/nouveau/headers" -I"$B/src/nouveau/headers" -I"$M/src/nouveau/headers/nvidia" -I"$M/src/nouveau/headers/nvidia/classes" -I"$M/src/nouveau/headers/nvidia/classes/hwref" \
   -I"$M/src" -I"$M/include" -I"$B/src" -I"$B/include" "$here/verify_push.c" -o "$here/verify_push"
cc -std=gnu11 -Wall -I"$here" "$here/probe_words.c" -o "$here/probe_words"
"$here/verify_push" > "$here/mesa.txt"
"$here/probe_words" > "$here/probe.txt"
if diff "$here/mesa.txt" "$here/probe.txt"; then echo "push words identical ($(wc -l < "$here/probe.txt") dwords)"; else echo "MISMATCH"; exit 1; fi
