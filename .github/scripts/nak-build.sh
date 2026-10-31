#!/usr/bin/env bash
# Build what the NAK shader check (nak-shaders.sh) runs: NVK for Linux with
# the RM backend, from Mesa with a Conduit tree's NVK-on-RM series applied,
# plus nouveau's drm-shim, NAK's unit tests and nvstats.
#
#   .github/scripts/nak-build.sh TREE OUT_DIR    build into OUT_DIR
#   .github/scripts/nak-build.sh --key TREE      print the cache key
#
# TREE     a Conduit checkout; its guest/nvk-rm/series.sh and the MESA_BASE
#          its build.sh pins are what gets built
# OUT_DIR  receives the files, in the build directory layout
#          guest/nvk-rm/tools/nak-offline/run-objects.sh expects:
#            src/nouveau/vulkan/libvulkan_nouveau.so
#            src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json
#            src/nouveau/drm-shim/libnouveau_noop_drm_shim.so
#            nvstats, nak-tests (NAK's unit test binary), series.txt
#
# The meson options and nvstats come from the checkout this script is in,
# only the series from TREE, so a baseline tree is built the same way. The
# key covers MESA_BASE, the series' contents in order, this script and
# nvstats.c.
#
# Environment: WORK_DIR (Mesa checkout and build directory; default a
# temporary directory), MESA_URL, MESON (meson >= 1.7), JOBS (ninja -j,
# default nproc), LLVM_CONFIG (llvm-config of the LLVM to build mesa_clc
# with; default: meson's choice).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
nvstats_c=$root/guest/nvk-rm/tools/nak-offline/nvstats.c

mesa_base() { # tree
  sed -n 's/^MESA_BASE=\${MESA_BASE:-\([0-9a-f]\{40\}\)}$/\1/p' "$1/guest/nvk-rm/build.sh"
}
series() { # tree
  sh "$1/guest/nvk-rm/series.sh"
}

if [ "${1:-}" = --key ]; then
  tree=$(cd "${2:?usage: nak-build.sh --key TREE}" && pwd)
  base=$(mesa_base "$tree")
  [ -n "$base" ] || { echo "no MESA_BASE in $tree/guest/nvk-rm/build.sh" >&2; exit 1; }
  hash=$( { series "$tree" | xargs -d '\n' cat; cat "${BASH_SOURCE[0]}" "$nvstats_c"; } | sha256sum | cut -c1-16)
  echo "${base:0:12}-$hash"
  exit 0
fi

tree=$(cd "${1:?usage: nak-build.sh TREE OUT_DIR}" && pwd)
out=${2:?usage: nak-build.sh TREE OUT_DIR}
mkdir -p "$out"
out=$(cd "$out" && pwd)
work=${WORK_DIR:-$(mktemp -d)}
mkdir -p "$work"
mesa=$work/mesa
build=$work/build
MESA_URL=${MESA_URL:-https://gitlab.freedesktop.org/mesa/mesa.git}
MESON=${MESON:-meson}
JOBS=${JOBS:-$(nproc)}

base=$(mesa_base "$tree")
[ -n "$base" ] || { echo "no MESA_BASE in $tree/guest/nvk-rm/build.sh" >&2; exit 1; }
mapfile -t patches < <(series "$tree")

# git am commits on the scratch checkout; nothing is pushed.
export GIT_AUTHOR_NAME="Conduit CI" GIT_AUTHOR_EMAIL=ci@conduit.invalid
export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME" GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

# 1. Mesa at MESA_BASE with the series, as build.sh applies it
if [ ! -d "$mesa/.git" ]; then
  git init -q "$mesa"
fi
git -C "$mesa" am --abort >/dev/null 2>&1 || true
if ! git -C "$mesa" cat-file -e "$base^{commit}" 2>/dev/null; then
  git -C "$mesa" fetch -q --depth 1 --no-tags "$MESA_URL" "$base"
fi
git -C "$mesa" checkout -q -f --detach "$base"
git -C "$mesa" clean -q -fdx
if ! git -C "$mesa" am -q --3way "${patches[@]}"; then
  echo "::error::the NVK-on-RM series does not apply on Mesa $base"
  exit 1
fi
echo "nak-build: Mesa $base + ${#patches[@]} patches"

# 2. Configure: NVK with the RM backend (the Linux build's -Dnvk-rm=enabled),
#    the drm-shim, NAK's tests. Rust debug assertions (b_ndebug=false) catch
#    more compiler invariants; C asserts stay off (-DNDEBUG), as the
#    drm-shim's fake GB202 trips a winsys assert release builds compile out
#    (tools/nak-offline/README.md, "Debug-assertion build").
#    rmclient.h from the tree, as build.sh takes it.
pcdir=$work/pkgconfig
mkdir -p "$pcdir"
cat > "$pcdir/rmclient.pc" <<EOF
Name: rmclient
Description: librmclient headers (Conduit)
Version: 0.1.0
Cflags: -I$tree/guest/rmclient/include
EOF
export PKG_CONFIG_PATH="$pcdir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
native=()
if [ -n "${LLVM_CONFIG:-}" ]; then
  printf "[binaries]\nllvm-config = '%s'\n" "$LLVM_CONFIG" > "$work/native.ini"
  native=(--native-file "$work/native.ini")
fi
rm -rf "$build"
(cd "$mesa" && "$MESON" setup "$build" "${native[@]}" \
  -Dvulkan-drivers=nouveau -Dgallium-drivers= -Dplatforms= \
  -Dvideo-codecs= -Dtools=drm-shim -Dbuild-tests=true \
  -Dnvk-rm=enabled \
  -Dbuildtype=debugoptimized -Db_ndebug=false \
  -Dc_args=-DNDEBUG -Dcpp_args=-DNDEBUG)

# 3. Build
ninja -C "$build" -j"$JOBS" \
  src/nouveau/vulkan/libvulkan_nouveau.so \
  src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json \
  src/nouveau/drm-shim/libnouveau_noop_drm_shim.so \
  src/nouveau/compiler/nak

# 4. Stage
mkdir -p "$out/src/nouveau/vulkan" "$out/src/nouveau/drm-shim"
cp "$build/src/nouveau/vulkan/libvulkan_nouveau.so" "$out/src/nouveau/vulkan/"
# The manifest names the library relative to itself, so the directory can move
python3 - "$build/src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json" \
  "$out/src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json" <<'EOF'
import json, sys
m = json.load(open(sys.argv[1]))
m["ICD"]["library_path"] = "./libvulkan_nouveau.so"
json.dump(m, open(sys.argv[2], "w"), indent=2)
EOF
cp "$build/src/nouveau/drm-shim/libnouveau_noop_drm_shim.so" "$out/src/nouveau/drm-shim/"
cp "$build/src/nouveau/compiler/nak" "$out/nak-tests"
cc -O2 -Wall -o "$out/nvstats" "$nvstats_c" -lvulkan
# debugoptimized carries full debug info (hundreds of MB); panics name their
# source line without it
strip --strip-debug "$out/src/nouveau/vulkan/libvulkan_nouveau.so" \
  "$out/src/nouveau/drm-shim/libnouveau_noop_drm_shim.so" "$out/nak-tests"
git -C "$mesa" log --format='%h %s' "$base..HEAD" > "$out/series.txt"
echo "$base" > "$out/mesa-base.txt"
echo "nak-build: staged in $out"
