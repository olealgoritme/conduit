#!/usr/bin/env bash
# The app icon set: install_icons lays out the svg and nine PNG sizes, the
# desktop entry names the installed icon, and every package format carries it.
# Runs without any build artifacts: bash packaging/test/icons.sh
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
PKG=$ROOT/packaging
SIZES="16 22 24 32 48 64 128 256 512"
fail=0
bad() { echo "FAIL: $*" >&2; fail=1; }

# shellcheck source=../common/icons.sh
. "$PKG/common/icons.sh"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
install_icons "$tmp/usr"
theme=$tmp/usr/share/icons/hicolor

grep -q '<svg' "$theme/scalable/apps/conduit.svg" 2>/dev/null || bad "scalable/apps/conduit.svg missing"
for n in $SIZES; do
    f=$theme/${n}x${n}/apps/conduit.png
    [ -f "$f" ] || { bad "${n}x${n}/apps/conduit.png missing"; continue; }
    file "$f" | grep -q "PNG image data, $n x $n," || bad "$f is not a ${n}x${n} PNG"
done
[ "$(find "$theme" -type f | wc -l)" -eq $(( $(echo $SIZES | wc -w) + 1 )) ] \
    || bad "install_icons installed unexpected files: $(find "$theme" -type f | wc -l)"

# The desktop entry's Icon= is the installed file stem.
icon=$(sed -n 's/^Icon=//p' "$PKG/common/conduit.desktop")
[ "$icon" = conduit ] && [ -f "$theme/scalable/apps/$icon.svg" ] || bad "Icon=$icon is not an installed icon"

# Every format that lists or installs files mentions the icon set.
for f in packaging/nfpm/conduit.yaml:usr/share/icons/hicolor \
         packaging/rpm/conduit.spec:icons/hicolor \
         packaging/arch/PKGBUILD:usr/share/icons/hicolor \
         flake.nix:install_icons \
         packaging/build.sh:install_icons \
         packaging/tarball/install.sh:icons/hicolor \
         packaging/tarball/uninstall.sh:icons/hicolor \
         packaging/common/conduit-integrate:gtk-update-icon-cache; do
    grep -q "${f#*:}" "$ROOT/${f%%:*}" || bad "${f%%:*} does not mention ${f#*:}"
done

[ "$fail" = 0 ] && echo "icons: ok"
exit "$fail"
