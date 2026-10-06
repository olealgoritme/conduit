#!/bin/sh
# Apply the NVK-on-RM patch series to Mesa and build NVK with it.
#
#   guest/nvk-rm/build.sh [MESA_DIR [BUILD_DIR]]
#
# MESA_DIR   Mesa checkout (cloned if missing). Default: ~/code/mesa-nvk-rm
# BUILD_DIR  meson build directory inside MESA_DIR. Default: build-rm
#
# Environment:
#   MESA_BASE       Mesa commit the series applies to (default below)
#   MESON           meson binary (needs >= 1.4; default: meson in PATH)
#   MESON_ARGS      extra `meson setup` arguments
#   BUILDTYPE       meson buildtype (default release, with b_ndebug=true)
#   RMCLIENT_INCLUDE  directory holding rmclient.h (default: guest/rmclient/include
#                   in this tree if present; otherwise Mesa uses the copy in
#                   the patch, which is identical)
#
# The result is MESA_DIR/BUILD_DIR/src/nouveau/vulkan/libvulkan_nouveau.so
# and nouveau_devenv_icd.x86_64.json next to it. See README.md for running it.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
conduit=$(cd "$here/../.." && pwd)

MESA_DIR=${1:-$HOME/code/mesa-nvk-rm}
BUILD_DIR=${2:-build-rm}
MESA_BASE=${MESA_BASE:-70c4c018cbe5b78a1db7e9413bc7e511b366fd95}
MESA_URL=https://gitlab.freedesktop.org/mesa/mesa.git
MESON=${MESON:-meson}
MESON_ARGS=${MESON_ARGS:-}

if [ -z "${RMCLIENT_INCLUDE:-}" ] && [ -f "$conduit/guest/rmclient/include/rmclient.h" ]; then
  RMCLIENT_INCLUDE=$conduit/guest/rmclient/include
fi

# 1. Mesa checkout at the base commit
if [ ! -d "$MESA_DIR/.git" ]; then
  git clone --depth 200 "$MESA_URL" "$MESA_DIR"
fi
if ! git -C "$MESA_DIR" cat-file -e "$MESA_BASE^{commit}" 2>/dev/null; then
  git -C "$MESA_DIR" fetch --depth 200 origin "$MESA_BASE"
fi

# 2. Apply the series on a local branch, unless it is already there:
#    patches/ (NVK on RM), then patches-common/ (generic NVK patches that
#    apply on top of the Windows stack as well)
series=$(ls "$here"/patches/*.patch "$here"/patches-common/*.patch)
last_patch=$(printf "%s\n" $series | tail -1)
last_subject=$(awk '/^Subject: /{s = $0; while ((getline l) > 0 && l ~ /^ /) s = s l;
                    sub(/^Subject: \[PATCH[^]]*\] /, "", s); print s; exit}' "$last_patch")
if git -C "$MESA_DIR" log --format=%s "$MESA_BASE..HEAD" 2>/dev/null | grep -qxF "$last_subject"; then
  echo "nvk-rm: series already applied in $MESA_DIR"
else
  if [ -n "$(git -C "$MESA_DIR" status --porcelain --untracked-files=no)" ]; then
    echo "nvk-rm: $MESA_DIR has uncommitted changes; refusing to switch branches" >&2
    exit 1
  fi
  git -C "$MESA_DIR" checkout -B nvk-rm "$MESA_BASE"
  # shellcheck disable=SC2086
  git -C "$MESA_DIR" am --3way $series
fi

# 3. Point meson at Conduit's rmclient.h through a throwaway pkg-config file
if [ -n "${RMCLIENT_INCLUDE:-}" ]; then
  pcdir="$MESA_DIR/$BUILD_DIR-pkgconfig"
  mkdir -p "$pcdir"
  cat > "$pcdir/rmclient.pc" <<EOF
Name: rmclient
Description: librmclient headers (Conduit); NVK dlopen()s the library at runtime
Version: 0.1.0
Cflags: -I$RMCLIENT_INCLUDE
EOF
  PKG_CONFIG_PATH="$pcdir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
  export PKG_CONFIG_PATH
fi

# 4. Configure and build only NVK
cd "$MESA_DIR"
# Release: -O3, no C asserts (b_ndebug), and NAK (Rust) without debug
# assertions or overflow checks.  debugoptimized keeps all three on, which
# costs CPU time per draw and per shader compile (and on Windows an assert
# is a dialog).  BUILDTYPE=debugoptimized for debugging.
BUILDTYPE=${BUILDTYPE:-release}
if [ "$BUILDTYPE" = release ]; then NDEBUG=true; else NDEBUG=false; fi
if [ ! -f "$BUILD_DIR/build.ninja" ]; then
  # shellcheck disable=SC2086
  "$MESON" setup "$BUILD_DIR" \
    -Dvulkan-drivers=nouveau \
    -Dgallium-drivers= \
    -Dnvk-rm=enabled \
    -Dbuildtype="$BUILDTYPE" \
    -Db_ndebug="$NDEBUG" \
    $MESON_ARGS
else
  # Existing build directories from older versions of this script were
  # debugoptimized; bring them in line.
  "$MESON" configure "$BUILD_DIR" -Dbuildtype="$BUILDTYPE" -Db_ndebug="$NDEBUG"
fi
ninja -C "$BUILD_DIR" \
  src/nouveau/vulkan/libvulkan_nouveau.so \
  src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json

echo "nvk-rm: built $MESA_DIR/$BUILD_DIR/src/nouveau/vulkan/libvulkan_nouveau.so"
