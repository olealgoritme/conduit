#!/bin/bash
# stages.sh VMNAME [SECS]: per-frame stage timing of the Windows present paths (docs/TRACING.md
# "Frame stage timing"). Collects the host's stage stamps (backend and conduit-venus) and the guest
# driver's StgRing for SECS seconds (default 10), joins them by frame id and prints the stage table.
# The guest side needs the KMD knob StageTrace=1 (read at StartDevice: restart the device after
# setting it). Keeps the raw collection and a Perfetto trace in $VMTEST_DIR/win/stages-<time>/.
set -euo pipefail
VM=${1:?usage: stages.sh VMNAME [SECS]}; SECS=${2:-10}
H="${WIN_SSH:?set WIN_SSH=user@127.0.0.1 (the guest account)}"
D=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}
OUT=$D/win/stages-$(date +%Y%m%d-%H%M%S)
mkdir -p "$OUT"
conduit trace "$VM" stages --duration "$SECS" \
    --guest-cmd "timeout 10 ssh -p 2222 \"$H\" 'reg query HKLM\\SYSTEM\\CurrentControlSet\\Services\\helios_kmd_render /v StgRing'" \
    --save "$OUT" --perfetto "$OUT/stages.json" | tee "$OUT/table.txt"
echo "saved: $OUT (reanalyse: conduit trace stages $OUT; open stages.json in ui.perfetto.dev)"
