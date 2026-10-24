#!/usr/bin/env bash
# Build DXVK-NVAPI (nvapi64.dll, nvapi.dll) for Windows guests with Conduit's
# patches (third_party/patches/dxvk-nvapi), cross-compiled with MinGW on Linux.
# NGX (DLSS) asks NvAPI for the GPU; see guest/nvk-rm/docs/dlss.md.
#
#   build-dxvk-nvapi.sh [--arch 64|32|both] [--out DIR] [--work DIR] [--src REPO] [--jobs N]
#
# --src   git URL or local clone to clone from (default upstream); submodules
#         always come from their upstream URLs.
# --work  build folder (default ${TMPDIR:-/tmp}/conduit-dxvk-nvapi); reused.
# --out   where the DLLs go (default <work>/out).
# Needs git, meson, ninja and x86_64-w64-mingw32-g++ (i686-w64-mingw32-g++ for 32).
set -euo pipefail

UPSTREAM=https://github.com/jp7677/dxvk-nvapi.git
COMMIT=0f26995d8126617d4d99f62c9571715fc7765da1 # v0.9.2-14 (2026-09-28)

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
patches=$here/../third_party/patches/dxvk-nvapi

arch=64
src=$UPSTREAM
work=${TMPDIR:-/tmp}/conduit-dxvk-nvapi
out=
jobs=2
while [ $# -gt 0 ]; do
    case $1 in
        --arch) arch=$2; shift 2 ;;
        --src) src=$2; shift 2 ;;
        --work) work=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        --jobs) jobs=$2; shift 2 ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
case $arch in 64|32|both) ;; *) echo "--arch is 64, 32 or both" >&2; exit 2 ;; esac
out=${out:-$work/out}

mkdir -p "$work" "$out"
tree=$work/dxvk-nvapi
if [ ! -d "$tree/.git" ]; then
    git clone --quiet "$src" "$tree"
fi
git -C "$tree" fetch --quiet origin "$COMMIT" 2>/dev/null || true
git -C "$tree" checkout --quiet --force --detach "$COMMIT"
git -C "$tree" clean --quiet -fdx --exclude=build-*/
git -C "$tree" submodule update --quiet --init --depth 1
for p in "$patches"/*.patch; do
    git -C "$tree" apply --whitespace=nowarn "$p"
done
echo "dxvk-nvapi $COMMIT + $(ls "$patches"/*.patch | wc -l) Conduit patches"

build() { # $1 = 64|32
    local cross=$tree/build-win$1.txt dir=$tree/build-win$1-helios
    meson setup --reconfigure --cross-file "$cross" --buildtype release \
        -Denable_tests=false "$dir" "$tree" >/dev/null 2>&1 ||
        meson setup --wipe --cross-file "$cross" --buildtype release \
            -Denable_tests=false "$dir" "$tree"
    ninja -C "$dir" -j "$jobs"
    if [ "$1" = 64 ]; then
        cp "$dir/src/nvapi64.dll" "$out/"
    else
        cp "$dir/src/nvapi.dll" "$out/"
    fi
}
[ "$arch" = 32 ] || build 64
[ "$arch" = 64 ] || build 32
ls -l "$out"
