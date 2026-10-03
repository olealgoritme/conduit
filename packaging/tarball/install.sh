#!/bin/sh
# Conduit installer for distributions without a native package.
#
#   tar xf conduit-*-x86_64-linux.tar.gz && sudo ./conduit/install.sh
#
# Installs everything into /opt/conduit (replacing an earlier tarball install),
# links /usr/local/bin/conduit, adds a desktop entry under /usr/local/share and
# the AppArmor/SELinux rules for libvirt. Undo with: sudo /opt/conduit/uninstall.sh
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
PREFIX=/opt/conduit

[ "$(id -u)" -eq 0 ] || { echo "Run as root: sudo $0" >&2; exit 1; }
[ "$(uname -m)" = x86_64 ] || { echo "Conduit needs an x86-64 host." >&2; exit 1; }
[ -d "$HERE/root/conduit/bin" ] || { echo "Run install.sh from the unpacked tarball." >&2; exit 1; }

# A distro package owns /opt/conduit when it is installed; do not fight it.
for q in "dpkg -S $PREFIX/bin/conduit" "rpm -qf $PREFIX/bin/conduit" "pacman -Qo $PREFIX/bin/conduit"; do
    if command -v "${q%% *}" >/dev/null 2>&1 && $q >/dev/null 2>&1; then
        echo "Conduit is installed from a distribution package; remove that first." >&2
        exit 1
    fi
done

[ -e /dev/kvm ] || echo "warning: /dev/kvm not found; enable VT-x/AMD-V in the BIOS." >&2

echo "Installing Conduit $(cat "$HERE/VERSION") to $PREFIX"
if [ -x "$PREFIX/libexec/conduit-integrate" ]; then
    "$PREFIX/libexec/conduit-integrate" disable >/dev/null || true
fi
# Copy next to the old tree, then swap, so a failed copy leaves the old install.
rm -rf "$PREFIX.new"
cp -a "$HERE/root/conduit" "$PREFIX.new"
rm -rf "$PREFIX"
mv "$PREFIX.new" "$PREFIX"
chown -R root:root "$PREFIX"

install -d /usr/local/bin /usr/local/share/applications
ln -sfn "$PREFIX/bin/conduit" /usr/local/bin/conduit
install -m0644 "$PREFIX/share/conduit/conduit.desktop" /usr/local/share/applications/conduit.desktop
if [ -d /etc/apparmor.d ]; then
    install -d /etc/apparmor.d/abstractions
    install -m0644 "$PREFIX/share/conduit/apparmor/conduit" /etc/apparmor.d/abstractions/conduit
fi
"$PREFIX/libexec/conduit-integrate" enable

echo "Done. Try: conduit create myvm && conduit view myvm"
