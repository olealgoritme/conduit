#!/bin/sh
# Cross-compile NVK with the RM backend for Windows x86_64 or x86 (32-bit,
# for WoW64 applications) with MinGW-w64, plus librmclient.dll, on a Linux host.
#
#   guest/nvk-rm/build-windows.sh [MESA_DIR [BUILD_DIR]]
#
# MESA_DIR   Mesa checkout (cloned if missing). Default: ~/code/mesa-nvk-rm-windows
# BUILD_DIR  meson build directory inside MESA_DIR. Default: build-win
#
# Environment:
#   ARCH          x86_64 (default) or i686 (32-bit DLLs for WoW64 apps such as
#                 Unigine Heaven; needs gcc-mingw-w64-i686 and rustup target
#                 i686-pc-windows-gnu; use a separate BUILD_DIR, e.g. build-win32)
#   MESA_BASE     Mesa commit the series applies to (default below)
#   MESON         meson binary (>= 1.7 for Mesa's Rust; default: meson in PATH)
#   MESON_ARGS    extra `meson setup` arguments for Mesa
#   VIDEO_CODECS  Mesa -Dvideo-codecs (default h264dec; empty for none)
#   MESA_CLC_DIR  directory holding native mesa_clc and vtn_bindgen2 (NVK's
#                 OpenCL kernels are compiled on the build machine). Default:
#                 built from MESA_DIR into BUILD_DIR-host (needs the LLVM/clang
#                 development packages listed in README.md)
#   OUT_DIR       where the Windows files are staged. Default: MESA_DIR/BUILD_DIR/dist
#                 (the unstripped DLLs, for symbolizing CPU profiles, go to
#                 OUT_DIR/debug: tools/etw-symbolize.py in guest/windows)
#   JOBS          ninja -j (default 2)
#   MEMORY_MAX    memory cap for the compile, via systemd-run --user --scope
#                 when available (default 2500M; set empty to run uncapped)
#   GL            1: also build Zink (OpenGL on Vulkan) as Mesa's gallium WGL
#                 ICD, libgallium_wgl.dll, plus Mesa's opengl32.dll for
#                 app-local use; Zink loads the NVK next to it (patch 0033)
#
# Needs: gcc-mingw-w64-x86-64, rustup target x86_64-pc-windows-gnu, bindgen,
# cbindgen, and Mesa's usual Python build modules (mako, yaml).
#
# Result in OUT_DIR: vulkan_nouveau.dll, librmclient.dll, nouveau_icd.json
# (library_path relative to the manifest), imports.txt, exports.txt; with
# GL=1 also libgallium_wgl.dll and opengl32.dll.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
conduit=$(cd "$here/../.." && pwd)
ARCH=${ARCH:-x86_64}
case "$ARCH" in
  x86_64|i686) ;;
  *) echo "nvk-rm: ARCH must be x86_64 or i686" >&2; exit 1 ;;
esac
cross="$here/windows/mingw-$ARCH.ini"
tool=$ARCH-w64-mingw32

MESA_DIR=${1:-$HOME/code/mesa-nvk-rm-windows}
BUILD_DIR=${2:-build-win}
MESA_BASE=${MESA_BASE:-70c4c018cbe5b78a1db7e9413bc7e511b366fd95}
MESA_URL=https://gitlab.freedesktop.org/mesa/mesa.git
MESON=${MESON:-meson}
MESON_ARGS=${MESON_ARGS:-}
JOBS=${JOBS:-2}
MEMORY_MAX=${MEMORY_MAX-2500M}
OUT_DIR=${OUT_DIR:-$MESA_DIR/$BUILD_DIR/dist}
GL=${GL:-0}

capped() {
  if [ -n "$MEMORY_MAX" ] && command -v systemd-run >/dev/null 2>&1; then
    systemd-run --user --scope -q -p MemoryMax="$MEMORY_MAX" -p MemorySwapMax=0 "$@"
  else
    "$@"
  fi
}

# 1. Mesa checkout with the series: patches/ (NVK on RM), patches-windows/
#    (the Windows build), patches-windows-dxvk/ and patches-common/ on a
#    local branch
if [ ! -d "$MESA_DIR/.git" ] && [ ! -f "$MESA_DIR/.git" ]; then
  git clone --depth 200 "$MESA_URL" "$MESA_DIR"
fi
if ! git -C "$MESA_DIR" cat-file -e "$MESA_BASE^{commit}" 2>/dev/null; then
  git -C "$MESA_DIR" fetch --depth 200 origin "$MESA_BASE"
fi
# patches-windows-dxvk/: what a D3D11 game on DXVK needs (32-bit build fix,
# no present wait on Win32, R/B order of the GDI present).
# patches-common/: generic NVK patches (per-draw cost) shared with the Linux
# series, applied after everything else.
# patches/0014-0017 are the Linux series' versions of patches-windows/0022 and
# 0027-0029 (BAR heap, cached sysmem, compression, ZCULL); the Windows stack
# takes patches/ only up to 0013.
series="$(ls "$here"/patches/00*.patch | awk -F/ '{ n = substr($NF, 1, 4) + 0 } n <= 13') $here/patches-windows/*.patch $here/patches-windows-dxvk/*.patch $here/patches-common/*.patch"
# The whole series, in order (the start of each subject: a folded Subject:
# line only holds its beginning), must be what is on top of the base; a tree
# with an older or different series (another branch's patches) is rebuilt.
# shellcheck disable=SC2086
want=$(for p in $series; do sed -n 's/^Subject: \[PATCH[^]]*\] //p' "$p" | head -1 | cut -c1-30; done)
have=$(git -C "$MESA_DIR" log --reverse --format=%s "$MESA_BASE..HEAD" 2>/dev/null | cut -c1-30)
if [ -n "$have" ] && [ "$want" = "$have" ]; then
  echo "nvk-rm: Windows series already applied in $MESA_DIR"
else
  if [ -n "$(git -C "$MESA_DIR" status --porcelain --untracked-files=no)" ]; then
    echo "nvk-rm: $MESA_DIR has uncommitted changes; refusing to switch branches" >&2
    exit 1
  fi
  git -C "$MESA_DIR" checkout -B nvk-rm-windows "$MESA_BASE"
  git -C "$MESA_DIR" am --3way $series
fi

# 2. Native mesa_clc + vtn_bindgen2 (build-machine tools)
if [ -z "${MESA_CLC_DIR:-}" ]; then
  host="$MESA_DIR/$BUILD_DIR-host"
  if [ ! -f "$host/build.ninja" ]; then
    (cd "$MESA_DIR" && "$MESON" setup "$host" \
      -Dvulkan-drivers= -Dgallium-drivers= -Dplatforms= \
      -Dmesa-clc=enabled -Dinstall-mesa-clc=true \
      -Dglx=disabled -Degl=disabled -Dgbm=disabled -Dopengl=false \
      -Dgles1=disabled -Dgles2=disabled -Dbuild-tests=false \
      -Dbuildtype=release)
  fi
  capped ninja -C "$host" -j"$JOBS" src/compiler/clc/mesa_clc src/compiler/spirv/vtn_bindgen2
  MESA_CLC_DIR="$MESA_DIR/$BUILD_DIR-hostbin"
  mkdir -p "$MESA_CLC_DIR"
  ln -sf "$host/src/compiler/clc/mesa_clc" "$host/src/compiler/spirv/vtn_bindgen2" "$MESA_CLC_DIR/"
fi
PATH="$MESA_CLC_DIR:$PATH"
export PATH

# 3. librmclient.dll (Conduit's guest/rmclient, Windows transport over the KMD)
rmc="$MESA_DIR/$BUILD_DIR-rmclient"
if [ ! -f "$rmc/build.ninja" ]; then
  "$MESON" setup "$rmc" "$conduit/guest/rmclient" --cross-file "$cross" -Dbuildtype=release
fi
ninja -C "$rmc" librmclient.dll

# 4. Mesa: NVK only, RM backend only, Windows WSI
pcdir="$MESA_DIR/$BUILD_DIR-pkgconfig"
mkdir -p "$pcdir"
cat > "$pcdir/rmclient.pc" <<EOF
Name: rmclient
Description: librmclient headers (Conduit); NVK loads librmclient.dll at runtime
Version: 0.1.0
Cflags: -I$conduit/guest/rmclient/include -I$conduit/guest/windows/protocol/include
EOF
PKG_CONFIG_LIBDIR=$pcdir
export PKG_CONFIG_LIBDIR

cd "$MESA_DIR"
# Release: -O3, no C asserts (an assert is a dialog box on Windows), NAK
# (Rust) without debug assertions and overflow checks.  BUILDTYPE=
# debugoptimized for debugging.
BUILDTYPE=${BUILDTYPE:-release}
if [ "$BUILDTYPE" = release ]; then NDEBUG=true; else NDEBUG=false; fi
# H.264 decode for patch 35 (NVDEC; off at runtime unless NVK_EXPERIMENTAL=video)
VIDEO_CODECS=${VIDEO_CODECS-h264dec}
if [ "$GL" = 1 ]; then
  gl_opts="-Dgallium-drivers=zink -Dopengl=true"
  gl_targets="src/gallium/targets/wgl/libgallium_wgl.dll src/gallium/targets/libgl-gdi/opengl32.dll"
else
  gl_opts="-Dgallium-drivers= -Dopengl=false"
  gl_targets=
fi
if [ -f "$BUILD_DIR/build.ninja" ]; then
  # Build directories from older versions of this script were debugoptimized
  # shellcheck disable=SC2086
  "$MESON" configure "$BUILD_DIR" -Dbuildtype="$BUILDTYPE" -Db_ndebug="$NDEBUG" \
    -Dvideo-codecs="$VIDEO_CODECS" $gl_opts
else
  # shellcheck disable=SC2086
  "$MESON" setup "$BUILD_DIR" --cross-file "$cross" \
    -Dvulkan-drivers=nouveau -Dnvk-rm=enabled $gl_opts \
    -Dplatforms=windows -Dllvm=disabled \
    -Dmesa-clc=system -Dprecomp-compiler=system \
    -Dvideo-codecs="$VIDEO_CODECS" -Dvulkan-layers= \
    -Degl=disabled -Dgbm=disabled -Dglx=disabled \
    -Dgles1=disabled -Dgles2=disabled \
    -Dshader-cache=enabled -Dzlib=disabled -Dzstd=disabled -Dexpat=disabled \
    -Dxmlconfig=disabled -Dperfetto=false -Dbuild-tests=false \
    -Dbuildtype="$BUILDTYPE" -Db_ndebug="$NDEBUG" \
    $MESON_ARGS
fi
# Build directories from before patch 26 have the shader cache off
if ! grep -q 'ENABLE_SHADER_CACHE' "$BUILD_DIR/build.ninja"; then
  "$MESON" configure "$BUILD_DIR" -Dshader-cache=enabled
fi
# shellcheck disable=SC2086
capped ninja -C "$BUILD_DIR" -j"$JOBS" \
  src/nouveau/vulkan/vulkan_nouveau.dll \
  src/nouveau/vulkan/nouveau_icd.$ARCH.json $gl_targets

# 5. Stage: the DLLs, an ICD manifest pointing next to itself, imports/exports
mkdir -p "$OUT_DIR"
# Stripped copies (a debugoptimized vulkan_nouveau.dll is ~140 MB with DWARF,
# ~18 MB without); the unstripped DLLs stay in the build directories.
"$tool-strip" -o "$OUT_DIR/vulkan_nouveau.dll" "$BUILD_DIR/src/nouveau/vulkan/vulkan_nouveau.dll"
"$tool-strip" -o "$OUT_DIR/librmclient.dll" "$rmc/librmclient.dll"
# The same DLLs unstripped (DWARF), for symbolizing CPU profiles of exactly
# this build; stripping only drops debug sections, so RVAs match.
mkdir -p "$OUT_DIR/debug"
cp "$BUILD_DIR/src/nouveau/vulkan/vulkan_nouveau.dll" "$OUT_DIR/debug/vulkan_nouveau.dll"
cp "$rmc/librmclient.dll" "$OUT_DIR/debug/librmclient.dll"
if [ "$GL" = 1 ]; then
  "$tool-strip" -o "$OUT_DIR/libgallium_wgl.dll" "$BUILD_DIR/src/gallium/targets/wgl/libgallium_wgl.dll"
  "$tool-strip" -o "$OUT_DIR/opengl32.dll" "$BUILD_DIR/src/gallium/targets/libgl-gdi/opengl32.dll"
fi
python3 - "$BUILD_DIR/src/nouveau/vulkan/nouveau_icd.$ARCH.json" "$OUT_DIR/nouveau_icd.json" <<'EOF'
import json, sys
icd = json.load(open(sys.argv[1]))
# The Windows loader resolves a relative path with a separator against the
# manifest's own directory.
icd["ICD"]["library_path"] = ".\\vulkan_nouveau.dll"
json.dump(icd, open(sys.argv[2], "w"), indent=4)
EOF
objdump=$tool-objdump
{
  for dll in "$OUT_DIR"/*.dll; do
    echo "=== $(basename "$dll") ==="
    "$objdump" -p "$dll" | sed -n 's/^[[:space:]]*DLL Name: /DLL Name: /p'
  done
} > "$OUT_DIR/imports.txt"
{
  for dll in "$OUT_DIR"/*.dll; do
    echo "=== $(basename "$dll") ==="
    "$objdump" -p "$dll" | sed -n '/^\[Ordinal\/Name Pointer\] Table/,/^$/p' | sed -n 's/^[[:space:]]*\[ *[0-9]*\] //p'
  done
} > "$OUT_DIR/exports.txt"

# Only system DLLs: nothing from MinGW's runtime may be needed next to them
if grep -i 'DLL Name: lib\(gcc\|stdc++\|winpthread\)' "$OUT_DIR/imports.txt"; then
  echo "nvk-rm: unexpected MinGW runtime dependency (see $OUT_DIR/imports.txt)" >&2
  exit 1
fi

ls -l "$OUT_DIR"
cat "$OUT_DIR/exports.txt"
echo "nvk-rm: Windows build staged in $OUT_DIR"
