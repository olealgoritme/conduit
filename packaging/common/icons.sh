# shellcheck shell=bash
# install_icons DESTROOT: the one place that lays out Conduit's app icon.
#
# Sourced (never run) by packaging/build.sh and flake.nix. Installs
# packaging/common/icons/conduit.svg as DESTROOT/share/icons/hicolor/scalable/apps/conduit.svg
# and every conduit-N.png there as DESTROOT/share/icons/hicolor/NxN/apps/conduit.png.
# The icon name `conduit` is the Icon= of packaging/common/conduit.desktop.
# ICON_SRC (default: icons/ next to this file; flake.nix sets it) names the source dir.
install_icons() {
    local dest=${1:?install_icons DESTROOT} src f n
    src=${ICON_SRC:-$(dirname "${BASH_SOURCE[0]}")/icons}
    local theme="$dest/share/icons/hicolor"
    install -D -m0644 "$src/conduit.svg" "$theme/scalable/apps/conduit.svg"
    for f in "$src"/conduit-*.png; do
        n=${f##*/conduit-}; n=${n%.png}
        install -D -m0644 "$f" "$theme/${n}x${n}/apps/conduit.png"
    done
}
