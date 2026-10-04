#!/usr/bin/env bash
# Build a minimal QEMU (x86_64-softmmu, KVM, vhost-user, virtio) for Conduit.
#
# QEMU >= 11.1 is required: it is the first release with vhost-user
# VIRTIO Shared Memory Regions (VHOST_USER_PROTOCOL_F_SHMEM, GET_SHMEM_CONFIG,
# BACKEND_SHMEM_MAP/UNMAP) and shmem support in the generic vhost-user
# device (vhost-user-test-device-pci), which conduit-backend needs
# for its window and UVM aperture.
#
# The patches in ./patches are applied unless --stock is given. Stock 11.1
# cannot host the Conduit GPU device (256-byte vhost-user config limit); see README.md.
#
# Usage: build-qemu.sh [--version X.Y.Z] [--prefix DIR] [--no-slirp]
#                      [--no-audio] [--stock] [--jobs N] [--install]
# Env:   QEMU_VERSION, PREFIX, JOBS, WORKDIR (defaults below).
#
# Builds as the calling user under host/qemu/{src,build}; installs only with
# --install (uses sudo if PREFIX is not writable).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
QEMU_VERSION="${QEMU_VERSION:-11.1.2}"
PREFIX="${PREFIX:-/opt/conduit}"
JOBS="${JOBS:-$(nproc)}"
WORKDIR="${WORKDIR:-$HERE}"
SLIRP=enabled
AUDIO=enabled
INSTALL=0
PATCHES=1

# QEMU release signing key (Michael Roth), published on qemu.org/download.
QEMU_KEY_FPR="CEACC9E15534EBABB82D3FA03353C9CEF108B584"
# Pinned SHA-256 of known tarballs (checked in addition to the signature).
declare -A QEMU_SHA256=(
  [11.1.2]=731b5681e4bb18be313231579b8efd0296c5b015fa36dc533874b639ba838016
)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) QEMU_VERSION="$2"; shift 2 ;;
    --prefix)  PREFIX="$2"; shift 2 ;;
    --jobs)    JOBS="$2"; shift 2 ;;
    --no-slirp) SLIRP=disabled; shift ;;
    --no-audio) AUDIO=disabled; shift ;;
    --install) INSTALL=1; shift ;;
    --stock)   PATCHES=0; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

case "$QEMU_VERSION" in
  11.[1-9]*|11.[1-9][0-9]*|1[2-9].*) ;;
  *) echo "QEMU $QEMU_VERSION lacks vhost-user shmem; need >= 11.1" >&2; exit 1 ;;
esac

SRC_DIR="$WORKDIR/src"
BUILD_DIR="$WORKDIR/build"
TARBALL="qemu-$QEMU_VERSION.tar.xz"
URL="https://download.qemu.org/$TARBALL"

mkdir -p "$SRC_DIR" "$BUILD_DIR"
cd "$SRC_DIR"

# ---- fetch ---------------------------------------------------------------
if [[ ! -f "$TARBALL" ]]; then
  curl -fL --retry 3 -o "$TARBALL.part" "$URL"
  mv "$TARBALL.part" "$TARBALL"
fi
[[ -f "$TARBALL.sig" ]] || curl -fL --retry 3 -o "$TARBALL.sig" "$URL.sig"

# ---- verify --------------------------------------------------------------
if [[ -n "${QEMU_SHA256[$QEMU_VERSION]:-}" ]]; then
  echo "${QEMU_SHA256[$QEMU_VERSION]}  $TARBALL" | sha256sum -c -
else
  echo "warning: no pinned sha256 for $QEMU_VERSION; relying on signature" >&2
fi

GNUPGHOME="$(mktemp -d)"; export GNUPGHOME
trap 'rm -rf "$GNUPGHOME"' EXIT
if ! curl -fsS "https://keys.openpgp.org/vks/v1/by-fingerprint/$QEMU_KEY_FPR" \
      | gpg --batch --quiet --import 2>/dev/null; then
  gpg --batch --quiet --keyserver hkps://keyserver.ubuntu.com \
      --recv-keys "$QEMU_KEY_FPR"
fi
gpg --batch --status-fd 1 --verify "$TARBALL.sig" "$TARBALL" 2>/dev/null \
  | grep -q "^\[GNUPG:\] VALIDSIG $QEMU_KEY_FPR " \
  || { echo "signature verification FAILED for $TARBALL" >&2; exit 1; }
echo "signature OK ($QEMU_KEY_FPR)"

# ---- extract + patch -----------------------------------------------------
# The tree is re-extracted whenever the patch set (or --stock) changes, so a
# stamp always describes exactly what is in it.
TREE="qemu-$QEMU_VERSION"
if [[ $PATCHES == 1 ]]; then
  WANT="$(cat "$HERE"/patches/*.patch 2>/dev/null | sha256sum | cut -d' ' -f1)"
else
  WANT=stock
fi
if [[ ! -d "$TREE" || "$(cat "$TREE/.conduit-stamp" 2>/dev/null)" != "$WANT" ]]; then
  # The build dir goes too: tar restores the tarball's old mtimes, so ninja
  # would not notice a file that went back from patched to stock.
  rm -rf "$TREE" "$BUILD_DIR"
  mkdir -p "$BUILD_DIR"
  tar xf "$TARBALL"
  if [[ $PATCHES == 1 ]]; then
    for p in "$HERE"/patches/*.patch; do
      echo "applying $(basename "$p")"
      patch -d "$TREE" -p1 --forward --quiet < "$p"
    done
  fi
  echo "$WANT" > "$TREE/.conduit-stamp"
fi

# ---- configure -----------------------------------------------------------
# --without-default-features turns every optional feature off; re-enable
# only what the Conduit VM needs. No GUI (gtk/sdl/spice/opengl): the guest
# display is the zero-copy nvgpu scanout shown by the Conduit viewer.
# VNC + pixman stay on for a headless firmware/console fallback.
# Audio: the guest gets a virtio-sound card (speakers + mic) played through
# the desktop's PipeWire, or PulseAudio (pipewire-pulse) as a fallback.
# TPM: libvirt domains with <tpm model='tpm-crb'><backend type='emulator'/>
# (the virt-install default; Windows 11 needs a TPM 2.0) need the TPM
# backends and the tpm-crb/tpm-tis devices. The emulator backend talks to an
# external swtpm over a socket, so this adds no library dependency.
# The options are part of the stamp below, so changing them reconfigures.
if [[ $AUDIO == enabled ]]; then
  AUDIO_OPTS=(--enable-pipewire --enable-pa --audio-drv-list=pipewire,pa)
else
  AUDIO_OPTS=(--disable-pipewire --disable-pa --audio-drv-list=)
fi
CONF_STAMP="slirp=$SLIRP audio=$AUDIO tpm=enabled"
cd "$BUILD_DIR"
if [[ ! -f build.ninja || "$(cat .conduit-configured 2>/dev/null)" != "$CONF_STAMP" ]]; then
  "$SRC_DIR/qemu-$QEMU_VERSION/configure" \
    --prefix="$PREFIX" \
    --target-list=x86_64-softmmu \
    --without-default-features \
    --enable-kvm \
    --disable-tcg \
    --enable-vhost-user \
    --enable-vhost-kernel \
    --enable-vhost-net \
    --"$([[ $SLIRP == enabled ]] && echo enable || echo disable)"-slirp \
    --enable-pixman \
    --enable-vnc \
    --enable-fdt=system \
    --enable-malloc-trim \
    --enable-tpm \
    "${AUDIO_OPTS[@]}" \
    --disable-gtk --disable-sdl --disable-spice --disable-opengl \
    --disable-docs \
    --disable-werror
  echo "$CONF_STAMP" > .conduit-configured
fi

# ---- build ---------------------------------------------------------------
nice -n 10 ninja -j "$JOBS"
./qemu-system-x86_64 --version
# QEMU 11.1 names the generic device vhost-user-test-device(-pci).
./qemu-system-x86_64 -device help 2>/dev/null \
  | grep -E '"vhost-user-test-device-pci"' \
  || { echo "vhost-user-test-device-pci missing from build" >&2; exit 1; }
./qemu-system-x86_64 -tpmdev help 2>&1 | grep -qE '^ *emulator ' \
  || { echo "TPM emulator backend missing from build" >&2; exit 1; }
./qemu-system-x86_64 -device help 2>/dev/null | grep '"tpm-crb"' >/dev/null \
  || { echo "tpm-crb device missing from build" >&2; exit 1; }
if [[ $AUDIO == enabled ]]; then
  ./qemu-system-x86_64 -audiodev help 2>/dev/null | grep -qx pipewire \
    || { echo "pipewire audio backend missing from build" >&2; exit 1; }
fi

if [[ $INSTALL == 1 ]]; then
  if [[ -w "$(dirname "$PREFIX")" || -w "$PREFIX" ]]; then
    ninja install
  else
    sudo ninja install
  fi
  echo "installed to $PREFIX/bin/qemu-system-x86_64"
fi
