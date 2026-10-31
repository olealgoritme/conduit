#!/bin/sh
# Print the NVK-on-RM patch series, one path per line, in `git am` order:
# patches/ (the RM backend), patches-windows/ (the Windows build and the
# Helios integration), patches-windows-dxvk/ (what DXVK needs on Windows),
# then patches-common/ (generic NVK work). Linux and Windows builds apply the
# same series; the Windows parts build for Linux too.
#
#   guest/nvk-rm/series.sh
#
# Used by build.sh, build-windows.sh and .github/scripts/check-patches.sh.
# One glob per directory: each sorts on its own, so a locale whose collation
# puts "patches-common/" before "patches/" cannot reorder the directories.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
for p in "$here"/patches/*.patch "$here"/patches-windows/*.patch \
         "$here"/patches-windows-dxvk/*.patch "$here"/patches-common/*.patch; do
  [ -f "$p" ] || { echo "nvk-rm: no patches match $p" >&2; exit 1; }
  printf '%s\n' "$p"
done
