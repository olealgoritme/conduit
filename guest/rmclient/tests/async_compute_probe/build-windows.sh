#!/bin/sh
# Cross-compile async_compute_probe.exe for Windows x86_64 with MinGW-w64.
#
#   build-windows.sh [REPO_DIR] [OUT_DIR]      (default: this checkout, ./out-win)
#
# librmclient (guest/rmclient: rmclient.c, status.c, both transports) is
# compiled into the exe, so it is one file and needs no librmclient.dll.  The
# Windows transport builds against the vendored WDK headers
# (guest/windows/icd/win-build/wdk-include) and helios_kmdmap.h, as
# guest/rmclient/meson.build does.  Needs gcc-mingw-w64-x86-64 only.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
REPO=${1:-$(cd "$here/../../../.." && pwd)}
OUT=${2:-$here/out-win}
CC=${CC:-x86_64-w64-mingw32-gcc}
RM="$REPO/guest/rmclient"
mkdir -p "$OUT"
"$CC" -std=gnu11 -O2 -Wall -Wno-unused-function \
  -I"$here" -I"$RM/include" -I"$RM/src" \
  -isystem "$REPO/guest/windows/icd/win-build/wdk-include" \
  -I"$REPO/guest/windows/umd_common/bridge" \
  "$here/async_compute_probe.c" \
  "$RM/src/rmclient.c" "$RM/src/status.c" "$RM/src/transport_linux.c" "$RM/src/transport_windows.c" \
  -static-libgcc -o "$OUT/async_compute_probe.exe"
x86_64-w64-mingw32-strip "$OUT/async_compute_probe.exe"
ls -l "$OUT/async_compute_probe.exe"
