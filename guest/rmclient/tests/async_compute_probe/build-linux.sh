#!/bin/sh
# Native Linux build (compile check; it runs on any machine with NVIDIA's
# driver at the librmclient release, CRM_RM_VERSION overrides the version check).
#
#   build-linux.sh [REPO_DIR] [OUT_DIR]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
REPO=${1:-$(cd "$here/../../../.." && pwd)}
OUT=${2:-$here/out-linux}
RM="$REPO/guest/rmclient"
mkdir -p "$OUT"
${CC:-cc} -std=gnu11 -O2 -Wall -Wno-unused-function -pthread \
  -I"$here" -I"$RM/include" -I"$RM/src" \
  "$here/async_compute_probe.c" \
  "$RM/src/rmclient.c" "$RM/src/status.c" "$RM/src/transport_linux.c" "$RM/src/transport_windows.c" \
  -o "$OUT/async_compute_probe"
ls -l "$OUT/async_compute_probe"
