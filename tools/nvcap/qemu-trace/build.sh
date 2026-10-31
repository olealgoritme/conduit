#!/usr/bin/env bash
# Build the nvtrace QEMU: conduit's QEMU 11.1.2 (same tarball, same conduit
# patches, same configure options) plus the patches in ./patches.
#
# Usage: build.sh [--dev] [--jobs N]
#   --dev   build the tree in src/ as it is (no re-extract); for development.
# Output: build/qemu-system-x86_64, installed into install/ (PREFIX).
# Never installs over /opt/conduit and never writes into the conduit repo.
set -euo pipefail
shopt -s nullglob

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONDUIT_QEMU="${CONDUIT_QEMU:-/home/xuw/code/conduit/host/qemu}"
QEMU_VERSION=11.1.2
QEMU_SHA256=731b5681e4bb18be313231579b8efd0296c5b015fa36dc533874b639ba838016
WORKDIR="${WORKDIR:-$HERE}"
PREFIX="${PREFIX:-$HERE/install}"
JOBS="${JOBS:-16}"
DEV=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dev) DEV=1; shift ;;
    --jobs) JOBS="$2"; shift 2 ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ $JOBS -le 16 ]] || JOBS=16

SRC_DIR="$WORKDIR/src"
BUILD_DIR="$WORKDIR/build"
TARBALL="$SRC_DIR/qemu-$QEMU_VERSION.tar.xz"
TREE="$SRC_DIR/qemu-$QEMU_VERSION"
mkdir -p "$SRC_DIR" "$BUILD_DIR"

if [[ ! -f "$TARBALL" ]]; then
  cp "$CONDUIT_QEMU/src/qemu-$QEMU_VERSION.tar.xz" "$TARBALL"
fi
echo "$QEMU_SHA256  $TARBALL" | sha256sum -c - >/dev/null

# Patch set = conduit patches (in order) followed by ours.
PATCHES=("$CONDUIT_QEMU"/patches/*.patch "$HERE"/patches/*.patch)
WANT="$(cat "${PATCHES[@]}" | sha256sum | cut -d' ' -f1)"

if [[ $DEV == 0 ]]; then
  if [[ ! -d "$TREE" || "$(cat "$TREE/.nvtrace-stamp" 2>/dev/null)" != "$WANT" ]]; then
    rm -rf "$TREE" "$BUILD_DIR"
    mkdir -p "$BUILD_DIR"
    tar xf "$TARBALL" -C "$SRC_DIR"
    for p in "${PATCHES[@]}"; do
      echo "applying $(basename "$p")"
      patch -d "$TREE" -p1 --forward --quiet < "$p"
    done
    echo "$WANT" > "$TREE/.nvtrace-stamp"
  fi
fi

NPATCH=${#PATCHES[@]}
PKGVERSION="conduit-nvtrace, patches $NPATCH ${WANT:0:8}"
[[ $DEV == 1 ]] && PKGVERSION="$PKGVERSION dev"
CONF_STAMP="nvtrace slirp=enabled audio=enabled tpm=enabled prefix=$PREFIX"

cd "$BUILD_DIR"
if [[ ! -f build.ninja || "$(cat .nvtrace-configured 2>/dev/null)" != "$CONF_STAMP" ]]; then
  # Same options as conduit's build-qemu.sh (audio and slirp enabled).
  nice -n 10 "$TREE/configure" \
    --prefix="$PREFIX" \
    --with-pkgversion="$PKGVERSION" \
    --target-list=x86_64-softmmu \
    --without-default-features \
    --enable-kvm \
    --disable-tcg \
    --enable-vhost-user \
    --enable-vhost-kernel \
    --enable-vhost-net \
    --enable-slirp \
    --enable-pixman \
    --enable-vnc \
    --enable-fdt=system \
    --enable-malloc-trim \
    --enable-tpm \
    --enable-pipewire --enable-pa --audio-drv-list=pipewire,pa \
    --disable-gtk --disable-sdl --disable-spice --disable-opengl \
    --disable-docs \
    --disable-werror
  echo "$CONF_STAMP" > .nvtrace-configured
elif [[ "$(cat .nvtrace-pkgversion 2>/dev/null)" != "$PKGVERSION" ]]; then
  ./pyvenv/bin/meson configure -Dpkgversion="$PKGVERSION" .
fi
echo "$PKGVERSION" > .nvtrace-pkgversion

nice -n 10 ninja -j "$JOBS"
./qemu-system-x86_64 --version
./qemu-system-x86_64 -device vfio-pci,help 2>/dev/null | grep -q x-trap-bars \
  || { echo "x-trap-bars missing from vfio-pci (patches not applied?)" >&2; [[ $DEV == 1 ]] || exit 1; }
nice -n 10 ninja install >/dev/null
echo "installed: $PREFIX/bin/qemu-system-x86_64"
