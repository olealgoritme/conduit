#!/bin/bash
# vramcap.sh LABEL [SECONDS] [vram-etw.ps1 switches, e.g. -CSwitch]:
# DxgKrnl/DXGI ETW capture of the windowed Present (vram-etw.ps1 in the guest), copy back, per-frame report.
# HEAVEN=1 starts windowed Heaven first (hvwin.sh, 1600x900) and waits 25 s; otherwise the app must be running.
# PROC=Heaven.exe selects the app in the report. Output: $VMTEST_DIR/win/vram-LABEL-HHMMSS/ (report.txt, frames.csv, raw files).
set -u
L=${1:?label}; S=${2:-3}; shift; [ $# -gt 0 ] && shift
H="${WIN_SSH:?set WIN_SSH=user@127.0.0.1 (the guest account)}"
V=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}; T=$V/t
D=$(cd "$(dirname "$0")" && pwd); R=$(cd "$D/../../../.." && pwd)
O=$V/win/vram-$L-$(date +%H%M%S); mkdir -p "$O"
if [ "${HEAVEN:-0}" = 1 ]; then bash "$D/hvwin.sh" direct3d11 "" ; sleep 25; fi
scp -O -q -F "$T/sshcfg" "$D/vram-etw.ps1" "win11g:C:/Users/Public/t/vram-etw.ps1" || { echo "upload failed"; exit 1; }
timeout 900 ssh -p 2222 "$H" "powershell -ExecutionPolicy Bypass -File C:\\Users\\Public\\t\\vram-etw.ps1 -Seconds $S $*" | tr -d '\r' | tee "$O/guest.txt"
for f in vram.xml.zip vram.etl dxgkrnl-manifest.xml dxgi-manifest.xml processes.txt kmd-before.txt kmd-after.txt capture.txt segments.txt vram.wprp; do
  scp -O -q -F "$T/sshcfg" "win11g:C:/Users/Public/t/vram/$f" "$O/" 2>/dev/null || echo "missing $f"
done
python3 "$R/guest/windows/tools/vram_redirection_report.py" "$O" --process "${PROC:-Heaven.exe}" --csv "$O/frames.csv" | tee "$O/report.txt"
echo "saved $O"
