#!/bin/sh
# Cross-compile NVK with the RM backend for Windows x86_64 (MinGW-w64), plus
# librmclient.dll, on a Linux host.
#
#   guest/nvk-rm/build-windows.sh [MESA_DIR [BUILD_DIR]]
#
# MESA_DIR   Mesa checkout (cloned if missing). Default: ~/code/mesa-nvk-rm-windows
# BUILD_DIR  meson build directory inside MESA_DIR. Default: build-win
#
# Environment:
#   MESA_BASE     Mesa commit the series applies to (default below)
#   MESON         meson binary (>= 1.7 for Mesa's Rust; default: meson in PATH)
#   MESON_ARGS    extra `meson setup` arguments for Mesa
#   MESA_CLC_DIR  directory holding native mesa_clc and vtn_bindgen2 (NVK's
#                 OpenCL kernels are compiled on the build machine). Default:
#                 built from MESA_DIR into BUILD_DIR-host (needs the LLVM/clang
#                 development packages listed in README.md)
#   OUT_DIR       where the Windows files are staged. Default: MESA_DIR/BUILD_DIR/dist
#   JOBS          ninja -j (default 2)
#   MEMORY_MAX    memory cap for the compile, via systemd-run --user --scope
#                 when available (default 2500M; set empty to run uncapped)
#
# Needs: gcc-mingw-w64-x86-64, rustup target x86_64-pc-windows-gnu, bindgen,
# cbindgen, and Mesa's usual Python build modules (mako, yaml).
#
# Result in OUT_DIR: vulkan_nouveau.dll, librmclient.dll, nouveau_icd.json
# (library_path relative to the manifest), imports.txt, exports.txt.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
conduit=$(cd "$here/../.." && pwd)
cross="$here/windows/mingw-x86_64.ini"

MESA_DIR=${1:-$HOME/code/mesa-nvk-rm-windows}
BUILD_DIR=${2:-build-win}
MESA_BASE=${MESA_BASE:-70c4c018cbe5b78a1db7e9413bc7e511b366fd95}
MESA_URL=https://gitlab.freedesktop.org/mesa/mesa.git
MESON=${MESON:-meson}
MESON_ARGS=${MESON_ARGS:-}
JOBS=${JOBS:-2}
MEMORY_MAX=${MEMORY_MAX-2500M}
OUT_DIR=${OUT_DIR:-$MESA_DIR/$BUILD_DIR/dist}

capped() {
  if [ -n "$MEMORY_MAX" ] && command -v systemd-run >/dev/null 2>&1; then
    systemd-run --user --scope -q -p MemoryMax="$MEMORY_MAX" -p MemorySwapMax=0 "$@"
  else
    "$@"
  fi
}

# 1. Mesa checkout with both series: patches/ (NVK on RM) and
#    patches-windows/ (the Windows build) on a local branch
if [ ! -d "$MESA_DIR/.git" ] && [ ! -f "$MESA_DIR/.git" ]; then
  git clone --depth 200 "$MESA_URL" "$MESA_DIR"
fi
if ! git -C "$MESA_DIR" cat-file -e "$MESA_BASE^{commit}" 2>/dev/null; then
  git -C "$MESA_DIR" fetch --depth 200 origin "$MESA_BASE"
fi
last_subject=$(sed -n 's/^Subject: \[PATCH[^]]*\] //p' "$(ls "$here"/patches-windows/*.patch | tail -1)")
if git -C "$MESA_DIR" log --format=%s "$MESA_BASE..HEAD" 2>/dev/null | grep -qxF "$last_subject"; then
  echo "nvk-rm: Windows series already applied in $MESA_DIR"
else
  if [ -n "$(git -C "$MESA_DIR" status --porcelain --untracked-files=no)" ]; then
    echo "nvk-rm: $MESA_DIR has uncommitted changes; refusing to switch branches" >&2
    exit 1
  fi
  git -C "$MESA_DIR" checkout -B nvk-rm-windows "$MESA_BASE"
  git -C "$MESA_DIR" am --3way "$here"/patches/*.patch "$here"/patches-windows/*.patch
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

# 3. librmclient.dll (Conduit's guest/rmclient, Windows stub transport)
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
Cflags: -I$conduit/guest/rmclient/include
EOF
PKG_CONFIG_LIBDIR=$pcdir
export PKG_CONFIG_LIBDIR

cd "$MESA_DIR"
if [ ! -f "$BUILD_DIR/build.ninja" ]; then
  # shellcheck disable=SC2086
  "$MESON" setup "$BUILD_DIR" --cross-file "$cross" \
    -Dvulkan-drivers=nouveau -Dnvk-rm=enabled -Dgallium-drivers= \
    -Dplatforms=windows -Dllvm=disabled \
    -Dmesa-clc=system -Dprecomp-compiler=system \
    -Dvideo-codecs= -Dvulkan-layers= \
    -Degl=disabled -Dgbm=disabled -Dglx=disabled -Dopengl=false \
    -Dgles1=disabled -Dgles2=disabled \
    -Dshader-cache=disabled -Dzlib=disabled -Dzstd=disabled -Dexpat=disabled \
    -Dxmlconfig=disabled -Dperfetto=false -Dbuild-tests=false \
    -Dbuildtype=debugoptimized \
    $MESON_ARGS
fi
capped ninja -C "$BUILD_DIR" -j"$JOBS" \
  src/nouveau/vulkan/vulkan_nouveau.dll \
  src/nouveau/vulkan/nouveau_icd.x86_64.json

# 5. Stage: the DLLs, an ICD manifest pointing next to itself, imports/exports
mkdir -p "$OUT_DIR"
# Stripped copies (debugoptimized vulkan_nouveau.dll is ~140 MB with DWARF,
# ~18 MB without); the unstripped DLLs stay in the build directories.
x86_64-w64-mingw32-strip -o "$OUT_DIR/vulkan_nouveau.dll" "$BUILD_DIR/src/nouveau/vulkan/vulkan_nouveau.dll"
x86_64-w64-mingw32-strip -o "$OUT_DIR/librmclient.dll" "$rmc/librmclient.dll"
python3 - "$BUILD_DIR/src/nouveau/vulkan/nouveau_icd.x86_64.json" "$OUT_DIR/nouveau_icd.json" <<'EOF'
import json, sys
icd = json.load(open(sys.argv[1]))
# The Windows loader resolves a relative path with a separator against the
# manifest's own directory.
icd["ICD"]["library_path"] = ".\\vulkan_nouveau.dll"
json.dump(icd, open(sys.argv[2], "w"), indent=4)
EOF
objdump=x86_64-w64-mingw32-objdump
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
