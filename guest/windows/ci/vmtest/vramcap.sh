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
CFG=${WIN_SSH_CONFIG:-$T/sshcfg}; HOSTA=${WIN_SCP_HOST:-win11g}
if [ -f "$CFG" ]; then
  gssh() { ssh -F "$CFG" "$HOSTA" "$@"; }
  gget() { scp -O -q -F "$CFG" "$HOSTA:$1" "$2"; }
  gput() { scp -O -q -F "$CFG" "$1" "$HOSTA:$2"; }
else
  H="${WIN_SSH:?set WIN_SSH_CONFIG + WIN_SCP_HOST, or WIN_SSH=user@127.0.0.1 (the guest account)}"
  gssh() { ssh -p 2222 "$H" "$@"; }
  gget() { scp -O -q -P 2222 "$H:$1" "$2"; }
  gput() { scp -O -q -P 2222 "$1" "$H:$2"; }
fi
D=$(cd "$(dirname "$0")" && pwd); R=$(cd "$D/../../../.." && pwd)
O=$V/win/vram-$L-$(date +%H%M%S); mkdir -p "$O"
if [ "${HEAVEN:-0}" = 1 ]; then bash "$D/hvwin.sh" direct3d11 "" ; sleep 25; fi
gput "$D/vram-etw.ps1" "C:/Users/Public/t/vram-etw.ps1" || { echo "upload failed (config $CFG, host $HOSTA)"; exit 1; }
timeout 900 gssh "powershell -ExecutionPolicy Bypass -File C:\\Users\\Public\\t\\vram-etw.ps1 -Seconds $S $*" | tr -d '\r' | tee "$O/guest.txt"
for f in vram.xml.zip vram.etl dxgkrnl-manifest.xml dxgi-manifest.xml processes.txt kmd-before.txt kmd-after.txt capture.txt segments.txt vram.wprp; do
  gget "C:/Users/Public/t/vram/$f" "$O/" 2>/dev/null || echo "missing $f"
done
python3 "$R/guest/windows/tools/vram_redirection_report.py" "$O" --process "${PROC:-Heaven.exe}" --csv "$O/frames.csv" | tee "$O/report.txt"
echo "saved $O"
