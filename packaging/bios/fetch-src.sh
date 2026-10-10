#!/usr/bin/env bash
# Fetch the pinned Ubuntu edk2 source tarballs (packaging/bios/version.sh)
# into DIR, checksums checked. The project's own mirror (release assets of
# the `edk2-src-<deb version>` release, which bios.yml publishes) comes
# first, Launchpad second, so a release build does not depend on Launchpad.
#
# Usage: packaging/bios/fetch-src.sh DIR
# Env:   EDK2_MIRROR  base URL of the mirror ("" skips it)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=packaging/bios/version.sh
. "$HERE/version.sh"

DIR="${1:?usage: fetch-src.sh DIR}"
MIRROR="${EDK2_MIRROR-https://github.com/olealgoritme/conduit/releases/download/edk2-src-${EDK2_DEB_VERSION}}"
LP="https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/edk2/${EDK2_DEB_VERSION}"

mkdir -p "$DIR"
ok() { echo "$2  $1" | sha256sum -c --status; }
fetch() {
  local name="$1" sum="$2" dst="$DIR/$1" base
  ok "$dst" "$sum" 2>/dev/null && return 0
  for base in ${MIRROR:+"$MIRROR"} "$LP"; do
    if curl -fsSL --retry 3 --max-time 600 -o "$dst.part" "$base/$name" && ok "$dst.part" "$sum"; then
      mv "$dst.part" "$dst"
      return 0
    fi
    echo "note: $name from $base failed or did not match its checksum" >&2
  done
  rm -f "$dst.part"
  echo "could not fetch $name (checksum $sum)" >&2
  exit 1
}
fetch "edk2_${EDK2_UPSTREAM}.orig.tar.xz" "$EDK2_ORIG_SHA256"
fetch "edk2_${EDK2_DEB_VERSION}.debian.tar.xz" "$EDK2_DEBIAN_SHA256"
