#!/bin/sh
# Removes a tarball install of Conduit (installed as /opt/conduit/uninstall.sh).
# VMs and their disks live in each user's data directory and are kept.
set -eu
PREFIX=/opt/conduit

[ "$(id -u)" -eq 0 ] || { echo "Run as root: sudo $0" >&2; exit 1; }
[ -d "$PREFIX" ] || { echo "Conduit is not installed in $PREFIX." >&2; exit 0; }

if [ -x "$PREFIX/libexec/conduit-integrate" ]; then
    "$PREFIX/libexec/conduit-integrate" disable || true
fi
[ "$(readlink /usr/local/bin/conduit 2>/dev/null)" = "$PREFIX/bin/conduit" ] && rm -f /usr/local/bin/conduit
rm -f /usr/local/share/applications/conduit.desktop /etc/apparmor.d/abstractions/conduit
rm -f /usr/local/share/icons/hicolor/*/apps/conduit.*
[ -d /usr/local/share/icons/hicolor ] && command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -q -t -f /usr/local/share/icons/hicolor 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database -q /usr/local/share/applications 2>/dev/null || true
rm -rf "$PREFIX"
echo "Conduit removed."
