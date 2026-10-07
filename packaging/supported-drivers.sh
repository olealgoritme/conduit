#!/bin/sh
# packaging/supported-drivers.sh [GEN_SRC]: the NVIDIA driver releases the
# backend accepts, one per line, ascending.
#
# The backend refuses a host release unless the RM pointer table (rmctrl), RM
# allowlist (rmallow), UVM command table (uvm), video-memory table (vidmem)
# and GET_DEV_INFO layout (devinfo)
# are all that release's own (NvidiaBackend::inexact_tables). So a release is
# supported when every one of those directories has a vX_Y_Z.rs for it.
# build.sh writes this into share/conduit/supported-drivers.txt for
# `conduit doctor`; tests in cli/ and the backend check it against both.
set -eu
GEN=${1:-$(dirname "$0")/../host/backend/gen/src}
TABLES="rmctrl rmallow uvm vidmem devinfo nvkms"

releases() {
    for f in "$GEN/$1"/v*_*_*.rs; do
        [ -e "$f" ] || continue
        basename "$f" .rs | sed -e 's/^v//' -e 's/_/./g'
    done
}

n=0
for t in $TABLES; do
    [ -d "$GEN/$t" ] || { echo "supported-drivers.sh: no $GEN/$t" >&2; exit 1; }
    n=$((n + 1))
done
for t in $TABLES; do
    releases "$t"
done | sort | uniq -c | awk -v n="$n" '$1 == n { print $2 }' | sort -V
