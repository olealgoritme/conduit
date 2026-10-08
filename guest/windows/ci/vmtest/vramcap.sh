#!/bin/bash
# vramcap.sh LABEL [SECONDS] [vram-etw.ps1 switches, e.g. -CSwitch]:
# DxgKrnl/DXGI ETW capture of the windowed Present (vram-etw.ps1 in the guest), copy back, per-frame report.
# HEAVEN=1 starts windowed Heaven first (hvwin.sh, 1600x900) and waits 25 s; otherwise the app must be running.
# PROC=Heaven.exe selects the app in the report. Output: $VMTEST_DIR/win/vram-LABEL-HHMMSS/ (report.txt, frames.csv, raw files).
# Transport: WIN_SSH_CONFIG (an ssh config file, default $VMTEST_DIR/t/sshcfg) and WIN_SCP_HOST (a Host alias in it,
# default win11g) carry both ssh and scp, so a guest account with a space in its name works; without a config file,
# WIN_SSH=user@127.0.0.1 on port 2222 is used for ssh and scp (destination quoted).
set -u
L=${1:?label}; S=${2:-3}; shift; [ $# -gt 0 ] && shift
V=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}; T=$V/t
D=$(cd "$(dirname "$0")" && pwd); R=$(cd "$D/../../../.." && pwd)
CFG=${WIN_SSH_CONFIG:-$T/sshcfg}; HOSTA=${WIN_SCP_HOST:-win11g}
# Commands as arrays (`timeout` runs programs, not shell functions). DRY=1 prints them instead.
if [ -f "$CFG" ]; then
  SSH=(ssh -F "$CFG" "$HOSTA"); SCP=(scp -O -q -F "$CFG"); RH="$HOSTA"
else
  H="${WIN_SSH:?set WIN_SSH_CONFIG + WIN_SCP_HOST, or WIN_SSH=user@127.0.0.1 (the guest account)}"
  SSH=(ssh -p 2222 "$H"); SCP=(scp -O -q -P 2222); RH="$H"
fi
run() { if [ "${DRY:-0}" = 1 ]; then printf '%q ' "$@"; echo; else "$@"; fi; }
O=$V/win/vram-$L-$(date +%H%M%S); mkdir -p "$O"
if [ "${HEAVEN:-0}" = 1 ] && [ "${DRY:-0}" != 1 ]; then bash "$D/hvwin.sh" direct3d11 "" ; sleep 25; fi
run "${SCP[@]}" "$D/vram-etw.ps1" "$RH:C:/Users/Public/t/vram-etw.ps1" || { echo "upload failed (config $CFG, host $RH)"; exit 1; }
run timeout 900 "${SSH[@]}" "powershell -ExecutionPolicy Bypass -File C:\\Users\\Public\\t\\vram-etw.ps1 -Seconds $S $*" | tr -d '\r' | tee "$O/guest.txt"
for f in vram.xml.zip vram.etl dxgkrnl-manifest.xml dxgi-manifest.xml processes.txt kmd-before.txt kmd-after.txt capture.txt segments.txt vram.wprp; do
  run "${SCP[@]}" "$RH:C:/Users/Public/t/vram/$f" "$O/" 2>/dev/null || echo "missing $f"
done
[ "${DRY:-0}" = 1 ] && exit 0
python3 "$R/guest/windows/tools/vram_redirection_report.py" "$O" --process "${PROC:-Heaven.exe}" --csv "$O/frames.csv" | tee "$O/report.txt"
echo "saved $O"
