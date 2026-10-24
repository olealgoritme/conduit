#!/bin/sh
# SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
#
# check.sh [GLSLANG] -- compile every enhancement program offline, exactly as
# nb_gpuscale.c assembles it (the same files, in the same order, with the same
# GLSL ES fixups as embed.sh), with glslangValidator.  `make check-shaders`.
# No GPU needed.  Exits 77 (skip) when glslangValidator is not installed.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
top=$here/..
glslang=${1:-glslangValidator}
command -v "$glslang" >/dev/null 2>&1 || { echo "check.sh: no $glslang, skipped"; exit 77; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0

fix() {
    tr -d '\r' < "$1" |
    sed -e 's/con3\[2\]=con3\[3\]=0;/con3[2]=con3[3]=0u;/' \
        -e 's/con\[2\]=0;/con[2]=0u;/' \
        -e 's/con\[3\]=0;/con[3]=0u;/' \
        -e 's/const1\[3\]=0;/const1[3]=0u;/'
}

prog() {        # prog NAME STAGE FILE...
    name=$1 stage=$2
    shift 2
    out=$tmp/$name.$stage
    : > "$out"
    for f in "$@"; do
        fix "$top/$f" >> "$out"
    done
    if "$glslang" -S "$stage" "$out" > "$tmp/$name.log" 2>&1; then
        echo "  ok    $name"
    else
        echo "  FAIL  $name"
        sed -n '1,20p' "$tmp/$name.log"
        fail=1
    fi
}

A=third_party/fsr1/ffx_a.h
prog quad      vert shaders/quad.vert
prog copy      frag shaders/es_frag.glsl shaders/copy.frag
prog easu      frag shaders/es_frag.glsl shaders/easu_head.glsl $A third_party/fsr1/ffx_fsr1.h shaders/easu_main.glsl
prog rcas      frag shaders/es_frag.glsl shaders/rcas_head.glsl $A third_party/fsr1/ffx_fsr1.h shaders/rcas_main.glsl
prog fsr1     frag shaders/es_frag.glsl shaders/fsr1_head.glsl $A third_party/fsr1/ffx_fsr1.h shaders/fsr1_main.glsl
prog cas       frag shaders/es_frag.glsl shaders/cas_noscale.glsl shaders/cas_head.glsl $A third_party/cas/ffx_cas.h shaders/cas_main.glsl
prog cas_scale frag shaders/es_frag.glsl shaders/cas_scale.glsl shaders/cas_head.glsl $A third_party/cas/ffx_cas.h shaders/cas_main.glsl
prog nis       comp shaders/es_comp.glsl shaders/nis_head.glsl shaders/nis_scaler_gles.glsl shaders/nis_main.glsl
prog fg_down   comp shaders/es_comp.glsl shaders/fg_down.comp
prog fg_flow   comp shaders/es_comp.glsl shaders/fg_flow.comp
prog fg_interp comp shaders/es_comp.glsl shaders/fg_interp.comp
exit $fail
