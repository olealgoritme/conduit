#!/usr/bin/env bash
# packaging/bios/vms-using-bios and install.sh --uninstall: VMs that boot the
# Conduit BIOS are listed (system and session definitions), the package's
# prerm warns but never fails, an upgrade says nothing, and --strict fails.
# Runs without any build artifacts: bash packaging/test/bios-vms.sh
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
S=$ROOT/packaging/bios/vms-using-bios
fail=0
bad() { echo "FAIL: $*" >&2; fail=1; }

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/etc" "$tmp/home/a/.config/libvirt/qemu" "$tmp/home/b"
export CONDUIT_LIBVIRT_SYSTEM_DIR=$tmp/etc CONDUIT_HOMES="$tmp/home/a $tmp/home/b"
stock="<domain><name>plain</name><os><loader type='pflash'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader></os></domain>"
ours="<domain><name>NAME</name><os><loader type='pflash'>/usr/share/conduit/bios/conduit-bios.fd</loader></os></domain>"

echo "$stock" > "$tmp/etc/plain.xml"
out=$("$S" --strict 2>&1) || bad "--strict failed with no VM on the BIOS: $out"
[ -z "$out" ] || bad "printed with no VM on the BIOS: $out"

echo "${ours/NAME/sysvm}" > "$tmp/etc/sysvm.xml"
echo "${ours/NAME/uservm}" > "$tmp/home/a/.config/libvirt/qemu/uservm.xml"
if out=$("$S" --strict 2>&1); then bad "--strict passed with VMs on the BIOS"; fi
for vm in sysvm uservm; do
    grep -q "^  $vm " <<<"$out" || bad "$vm not listed: $out"
done
grep -q plain <<<"$out" && bad "a stock-firmware VM is listed: $out"
grep -q 'conduit attach' <<<"$out" || bad "no way out named: $out"

out=$("$S" remove 2>&1) || bad "prerm 'remove' failed (it must only warn)"
grep -q sysvm <<<"$out" || bad "prerm 'remove' did not warn: $out"
out=$("$S" upgrade 1.0 2>&1) || bad "prerm 'upgrade' failed"
[ -z "$out" ] || bad "prerm 'upgrade' warned: $out"

# install.sh --uninstall stops before removing anything (root check aside).
if grep -q 'vms-using-bios" --strict' "$ROOT/packaging/bios/install.sh"; then :; else
    bad "install.sh --uninstall does not check for VMs on the BIOS"
fi
grep -q 'preremove: "@ROOT@/packaging/bios/vms-using-bios"' "$ROOT/packaging/nfpm/conduit-bios.yaml" \
    || bad "conduit-bios.yaml has no preremove check"

[ "$fail" = 0 ] && echo "bios-vms: ok"
exit "$fail"
