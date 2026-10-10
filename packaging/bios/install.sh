#!/bin/sh
# Conduit BIOS installer for hosts without the distribution package.
#
#   tar xf conduit-bios-*.tar.gz && sudo ./conduit-bios/install.sh
#   sudo ./conduit-bios/install.sh --uninstall
#
# Puts the images in /usr/share/conduit/bios. The libvirt firmware descriptors
# go to /etc/qemu/firmware, only where the stock Debian/Ubuntu OVMF vars
# templates they name exist (the images match that firmware).
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=/usr/share/conduit/bios
DESC=/etc/qemu/firmware

[ "$(id -u)" -eq 0 ] || { echo "Run as root: sudo $0" >&2; exit 1; }

if [ "${1:-}" = --uninstall ]; then
    rm -rf "$DIR"
    rm -f "$DESC"/90-conduit-bios*.json
    rmdir /usr/share/conduit "$DESC" 2>/dev/null || true
    echo "Removed the Conduit BIOS. \`conduit attach VM\` puts a VM back on its stock firmware."
    exit 0
fi

echo "Installing the Conduit BIOS $(cat "$HERE/VERSION") to $DIR"
install -d "$DIR"
install -m0644 "$HERE/conduit-bios.fd" "$HERE/conduit-bios.secboot.fd" "$HERE/VERSION" "$DIR/"
if [ -f /usr/share/OVMF/OVMF_VARS_4M.fd ]; then
    install -d "$DESC"
    install -m0644 "$HERE"/firmware/90-conduit-bios*.json "$DESC/"
else
    echo "note: no /usr/share/OVMF (Debian/Ubuntu ovmf): \`conduit attach\` keeps this host's stock firmware." >&2
fi
echo "Done. \`conduit attach VM\` switches a VM on the stock OVMF image to it."
