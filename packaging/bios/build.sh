#!/usr/bin/env bash
# Build the Conduit BIOS: the distro's edk2 OVMF firmware with the Conduit boot
# logo in place of the TianoCore one, and the firmware display started in the
# VM's native mode (patches/).
#
# The firmware has to match the stock OVMF variant a VM already uses, so that
# the VM's existing NVRAM vars file (Secure Boot keys, boot entries) and swtpm
# state keep working when `conduit attach` swaps the loader. To get there this
# script builds from the exact Ubuntu edk2 source package (upstream tarball +
# Ubuntu patch series) with the same build flags as its debian/rules:
#
#   conduit-bios.fd          == OVMF_CODE_4M.fd          (4 MB, TPM2, no SB)
#   conduit-bios.secboot.fd  == OVMF_CODE_4M.secboot.fd  (4 MB, TPM2, SB, SMM)
#
# Both use the stock OVMF_VARS_4M*.fd layout, so existing vars files are used as is.
#
# Usage: packaging/bios/build.sh [OUT_DIR]
# Env:   WORK_DIR  scratch directory (default: target/conduit-bios)
#        JOBS      parallel build jobs (default: 4)
# Needs: gcc, make, nasm, iasl (acpica-tools), uuid-dev, python3, curl, xz.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
# shellcheck source=packaging/bios/version.sh
. "$HERE/version.sh"

OUT_DIR="$(realpath -m "${1:-$REPO/target/conduit-bios/out}")"
WORK_DIR="$(realpath -m "${WORK_DIR:-$REPO/target/conduit-bios}")"
JOBS="${JOBS:-4}"

LP="https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/edk2/${EDK2_DEB_VERSION}"
ORIG="edk2_${EDK2_UPSTREAM}.orig.tar.xz"
DEBIAN="edk2_${EDK2_DEB_VERSION}.debian.tar.xz"

mkdir -p "$WORK_DIR/dl" "$OUT_DIR"
fetch() {
  local name="$1" sum="$2"
  local dst="$WORK_DIR/dl/$name"
  if [ ! -f "$dst" ] || ! echo "$sum  $dst" | sha256sum -c --status; then
    curl -fsSL --retry 3 --max-time 600 -o "$dst.part" "$LP/$name"
    mv "$dst.part" "$dst"
  fi
  echo "$sum  $dst" | sha256sum -c --status || { echo "checksum mismatch: $name" >&2; exit 1; }
}
fetch "$ORIG" "$EDK2_ORIG_SHA256"
fetch "$DEBIAN" "$EDK2_DEBIAN_SHA256"

SRC="$WORK_DIR/edk2-${EDK2_DEB_VERSION}"
rm -rf "$SRC"
mkdir -p "$SRC"
tar -xf "$WORK_DIR/dl/$ORIG" -C "$SRC" --strip-components=1
tar -xf "$WORK_DIR/dl/$DEBIAN" -C "$SRC"

# Apply the Ubuntu patch series (lines starting with # are disabled there too).
cd "$SRC"
while read -r p _; do
  case "$p" in ''|'#'*) continue ;; esac
  patch -p1 --quiet --forward -i "debian/patches/$p"
done < debian/patches/series

# Conduit's changes: the boot logo (LogoDxe -> BootLogoLib -> BGRT) and the
# small patches in packaging/bios/patches (firmware mode = the VM's native mode,
# no boot progress text over the logo).
cp "$HERE/Logo.bmp" MdeModulePkg/Logo/Logo.bmp
for p in "$HERE"/patches/*.patch; do
  patch -p1 --quiet --forward -i "$p"
done

export SOURCE_DATE_EPOCH
SOURCE_DATE_EPOCH="$(date -d "$(dpkg-parsechangelog -l debian/changelog -S Date 2>/dev/null \
  || sed -n 's/^ -- .*>  //p' debian/changelog | head -n1)" +%s)"
RELEASE_DATE="$(date -u -d "@$SOURCE_DATE_EPOCH" +%m/%d/%Y)"

# Flags copied from debian/rules (the PCD strings exactly as make hands them to build) (COMMON_FLAGS, OVMF_4M_FLAGS, OVMF_4M_SECBOOT_FLAGS).
PCD_FLAGS=(
  --pcd "PcdFirmwareVendor=LConduit BIOS (Ubuntu distribution of EDK II)\\0"
  --pcd "PcdFirmwareVersionString=L${CONDUIT_BIOS_VERSION}\\0"
  --pcd "PcdFirmwareReleaseDateString=L${RELEASE_DATE}\\0"
)
COMMON_FLAGS=(-DCC_MEASUREMENT_ENABLE=TRUE -DNETWORK_HTTP_BOOT_ENABLE=TRUE
  -DNETWORK_IP6_ENABLE=TRUE -DNETWORK_TLS_ENABLE "${PCD_FLAGS[@]}")
OVMF_4M_FLAGS=("${COMMON_FLAGS[@]}" -DTPM2_ENABLE=TRUE -DFD_SIZE_4MB)
OVMF_4M_SECBOOT_FLAGS=("${OVMF_4M_FLAGS[@]}" -DBUILD_SHELL=FALSE
  -DSECURE_BOOT_ENABLE=TRUE -DSMM_REQUIRE=TRUE)

export PYTHON3_ENABLE=TRUE
unset WORKSPACE EDK_TOOLS_PATH CONF_PATH ECP_SOURCE EDK_SOURCE EFI_SOURCE
set +u --  # edksetup.sh reads the positional parameters
# shellcheck disable=SC1091
. ./edksetup.sh
set -u
make -C BaseTools ARCH=X64 -j"$JOBS"

ovmf() {
  local out="$1"; shift
  rm -rf Build/OvmfX64
  build -a X64 -t GCC5 -p OvmfPkg/OvmfPkgX64.dsc -b RELEASE -n "$JOBS" "$@"
  cp Build/OvmfX64/RELEASE_GCC5/FV/OVMF_CODE.fd "$OUT_DIR/$out"
}
ovmf conduit-bios.fd "${OVMF_4M_FLAGS[@]}"
cp Build/OvmfX64/RELEASE_GCC5/FV/OVMF_VARS.fd "$WORK_DIR/OVMF_VARS_4M.fd"
ovmf conduit-bios.secboot.fd "${OVMF_4M_SECBOOT_FLAGS[@]}"

# The vars template we would have produced must be byte-identical in layout to
# the stock one (same size); the code images are drop-in for the stock 4M ones.
for f in conduit-bios.fd conduit-bios.secboot.fd; do
  size="$(stat -c %s "$OUT_DIR/$f")"
  [ "$size" = 3653632 ] || { echo "$f: unexpected size $size (want 3653632, 4 MB OVMF_CODE)" >&2; exit 1; }
done
[ "$(stat -c %s "$WORK_DIR/OVMF_VARS_4M.fd")" = 540672 ] || { echo "unexpected OVMF_VARS size" >&2; exit 1; }

# edk2's licenses (BSD-2-Clause-Patent, plus OpenSSL, Brotli, ...), as Ubuntu lists them.
cp debian/copyright "$OUT_DIR/copyright"
echo "$CONDUIT_BIOS_VERSION" > "$OUT_DIR/VERSION"
( cd "$OUT_DIR" && sha256sum conduit-bios.fd conduit-bios.secboot.fd > SHA256SUMS )
echo "Conduit BIOS $CONDUIT_BIOS_VERSION -> $OUT_DIR"
