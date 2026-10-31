#!/bin/bash
# night.sh: one unattended capture run of all three levels on the real RTX 5090.
#   1. desktop off, 5090 to the nvcap VM (switch.sh vm)
#   2. level 1 (nvtrace) runs: selftest, vk/d3d11/d3d12/cuda tests, DWM
#   3. level 3 (nvddi) installed boot-start + reboot; level 1+3 runs
#   4. VM restarted on the tracing QEMU (level 2); level 1+2+3 runs from boot
#   5. VM off, 5090 back, desktop back (switch.sh host); decoders write REPORT inputs
# Any failure or the global time limit ends in step 5. Run as root: setsid night.sh &
set -u
N=/steam/conduit-scratch/nvcap
R=/run/nvcap
OUT=$N/night-$(date +%Y%m%d-%H%M)
mkdir -p $OUT $R
exec > >(tee -a $OUT/night.log) 2>&1
log() { echo "[$(date +%T)] $*"; }
H="Ole Algoritme@127.0.0.1"
ssh_() { timeout "${T:-120}" sudo -u xuw ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=no -p 2223 "$H" "$@"; }
ps_() { # PowerShell in the guest (SSH session, elevated)
	local enc; enc=$(printf '%s' "$1" | iconv -t UTF-16LE | base64 -w0)
	ssh_ "powershell -NoProfile -NonInteractive -EncodedCommand $enc" 2>&1 | tr -d '\r' | grep -v -E '^#< CLIXML|^<Objs'
}
insession() { # $1 = cmd file content, runs in the logged-on desktop session; T = timeout s
	printf '%s\r\n' "@echo off" "$1" > $R/job.cmd
	tar -C $R -cf - job.cmd | ssh_ "tar -xf - -C C:/nvcap"
	ps_ "Set-ExecutionPolicy -Scope Process Bypass -Force; & C:\\Users\\Public\\perf\\run-in-session.ps1 -Dir C:\\nvcap -Command 'call C:\\nvcap\\job.cmd' -TimeoutSec ${T:-180} | Out-Null"
}
wait_ssh() { for i in $(seq ${1:-72}); do ssh_ "echo up" 2>/dev/null | grep -q up && return 0; sleep 5; done; return 1; }
qpid() { cat $R/qemu.pid 2>/dev/null; }
vm_down() { # graceful guest shutdown, then make sure QEMU is gone
	local p; p=$(qpid)
	T=15 ssh_ "shutdown /s /t 0" >/dev/null 2>&1
	for i in $(seq 120); do kill -0 "$p" 2>/dev/null || return 0; sleep 1; done
	log "qemu still up after 120 s: quit"; kill "$p"; sleep 3; kill -9 "$p" 2>/dev/null; return 0
}
finish() {
	trap - EXIT
	log "== back to the host"
	kill -0 "$(qpid)" 2>/dev/null && vm_down
	bash $N/switch.sh host
	log "switch: $(tail -3 $R/switch.log | tr '\n' ' ')"
	log "== decoding"
	decode
	log "== done: $OUT"
}
trap finish EXIT
# global limit: 4 h, then back to the host whatever happens
( sleep 14400; log "TIME LIMIT"; kill -TERM $$ ) & LIMIT=$!
trap 'log "terminated"; exit 1' TERM INT

fetch() { # $1 = label
	mkdir -p $OUT/$1
	T=600 ssh_ "tar -cf - -C C:/ nvcap/logs" 2>/dev/null | tar -x -C $OUT/$1 2>/dev/null
	T=600 ssh_ "tar -cf - -C C:/ nvddi" 2>/dev/null | tar -x -C $OUT/$1 2>/dev/null
	ps_ "Remove-Item C:\\nvcap\\logs\\* -Recurse -Force -EA 0" >/dev/null
	log "fetched $1: $(du -sh $OUT/$1 | cut -f1)"
}
tests() { # $1 = label; level 1 under nvinject, nvddi marks if present
	local L=$1
	ps_ "New-Item -ItemType Directory -Force C:\\nvcap\\logs\\$L | Out-Null"
	ps_ "if (Test-Path C:\\nvcap\\nvddi\\nvddictl.exe) { C:\\nvcap\\nvddi\\nvddictl.exe status }" | head -5
	T=200 insession "cd /d C:\\nvcap\\nvtrace && nvinject -d C:\\nvcap\\logs\\$L\\st --selftest > C:\\nvcap\\logs\\$L\\selftest.txt 2>&1"
	log "selftest: $(T=20 ssh_ "type C:\\nvcap\\logs\\$L\\selftest.txt" 2>/dev/null | tail -2 | tr -d '\r' | tr '\n' ' ')"
	for t in vktest cudatest d3d11test d3d12test; do
		ps_ "if (Test-Path C:\\nvcap\\nvddi\\nvddictl.exe) { C:\\nvcap\\nvddi\\nvddictl.exe mark $L-$t | Out-Null }"
		T=200 insession "cd /d C:\\nvcap\\nvtrace && nvinject -d C:\\nvcap\\logs\\$L\\$t -t 120 $t.exe > C:\\nvcap\\logs\\$L\\$t.txt 2>&1"
		log "$t: $(T=20 ssh_ "type C:\\nvcap\\logs\\$L\\$t.txt" 2>/dev/null | grep -E 'PASS|FAIL' | tail -1 | tr -d '\r')"
	done
	# DWM for 20 s while a test presents
	ps_ "C:\\nvcap\\nvtrace\\nvinject.exe -d C:\\nvcap\\logs\\$L\\dwm --dwm" | tail -2
	T=120 insession "cd /d C:\\nvcap\\nvtrace && d3d11test.exe > C:\\nvcap\\logs\\$L\\dwm-present.txt 2>&1"
	sleep 20
	ps_ "C:\\nvcap\\nvtrace\\nvinject.exe -d C:\\nvcap\\logs\\$L\\dwm --stop" | tail -2
	ps_ "Get-Process dwm -EA 0 | % { 'dwm alive ' + \$_.Id }"
	ps_ "if (Test-Path C:\\nvcap\\nvddi\\nvddictl.exe) { C:\\nvcap\\nvddi\\nvddictl.exe flush | Out-Null }"
}
decode() {
	for d in $OUT/*/; do
		for f in $(find $d -name '*.ntr' 2>/dev/null); do python3 /home/xuw/code/nvtrace/dist/ntrdump.py "$f" --summary > "$f.summary.txt" 2>&1; done
		for f in $(find $d -name '*.ndl' 2>/dev/null); do python3 /home/xuw/code/nvddi/decoder/ndldump.py "$f" --summary > "$f.summary.txt" 2>&1; done
	done
	for f in $(find $N/traces -newer $OUT/night.log -name '*.nvt*' 2>/dev/null | head -3); do
		python3 $N/qemu-trace/tools/mmiodump.py summary "$f" > $OUT/$(basename $f).summary.txt 2>&1
		python3 $N/qemu-trace/tools/mmiodump.py rmlog --tree "$f" > $OUT/$(basename $f).rmlog.txt 2>&1
		python3 $N/qemu-trace/tools/mmiodump.py boot "$f" > $OUT/$(basename $f).boot.json 2>&1
	done
}

# ---- 0. preconditions --------------------------------------------------------------
pgrep -x cs2 >/dev/null && { log "cs2 is running: not starting"; trap - EXIT; exit 1; }
virsh -c qemu:///session domstate win11 2>/dev/null | grep -q "shut off" || { log "conduit win11 is not off: not starting"; trap - EXIT; exit 1; }
[ "$(df --output=avail -BG /steam | tail -1 | tr -dc 0-9)" -gt 100 ] || { log "less than 100 GB free on /steam"; trap - EXIT; exit 1; }

# ---- 1. VM with the 5090 -----------------------------------------------------------
log "== 1. switch to the VM"
bash $N/switch.sh vm
wait_ssh 12 || { log "FAIL: no SSH"; exit 1; }
log "deploy tools"
ps_ "New-Item -ItemType Directory -Force C:\\nvcap\\nvtrace,C:\\nvcap\\nvddi,C:\\nvcap\\logs | Out-Null"
tar -C /home/xuw/code/nvtrace/dist -cf - --exclude=__pycache__ . | ssh_ "tar -xf - -C C:/nvcap/nvtrace"
tar -C /home/xuw/code/nvddi/dist -cf - . | ssh_ "tar -xf - -C C:/nvcap/nvddi"
ps_ "Get-ChildItem C:\\nvcap\\nvtrace,C:\\nvcap\\nvddi | Measure-Object | % Count"

# ---- 2. level 1 alone --------------------------------------------------------------
log "== 2. level 1"
tests l1
fetch l1

# ---- 3. level 3 installed boot-start, level 1+3 ------------------------------------
log "== 3. install nvddi (test signing, boot start) and reboot"
ps_ "bcdedit /set testsigning on | Out-Null; reg add 'HKLM\\SYSTEM\\CurrentControlSet\\Control\\DeviceGuard\\Scenarios\\HypervisorEnforcedCodeIntegrity' /v Enabled /t REG_DWORD /d 0 /f | Out-Null; certutil -f -addstore Root C:\\nvcap\\nvddi\\nvddi-test.cer | Out-Null; certutil -f -addstore TrustedPublisher C:\\nvcap\\nvddi\\nvddi-test.cer | Out-Null; cd C:\\nvcap\\nvddi; cmd /c install.cmd boot 2>&1 | select -last 3"
T=15 ssh_ "shutdown /r /t 0" >/dev/null 2>&1
sleep 30
if ! wait_ssh 72; then log "FAIL: guest did not come back after the nvddi reboot"; exit 1; fi
ps_ "C:\\nvcap\\nvddi\\nvddictl.exe status" | tee $OUT/nvddi-status.txt | head -8
if ! grep -qi "attached.*yes" $OUT/nvddi-status.txt; then
	log "nvddi not attached at boot: restart-gpu.ps1"
	ps_ "powershell -ExecutionPolicy Bypass -File C:\\nvcap\\nvddi\\restart-gpu.ps1" | tail -3
	ps_ "C:\\nvcap\\nvddi\\nvddictl.exe status" | head -8
fi
ps_ "Get-PnpDevice -InstanceId 'PCI\\VEN_10DE&DEV_2B85*' | % { \$_.Status + ' ' + \$_.FriendlyName }"
tests l13
fetch l13

# ---- 4. tracing QEMU (level 2) from boot, level 1+2+3 ------------------------------
log "== 4. restart on the tracing QEMU"
vm_down
setsid bash $N/vm-trace.sh > $R/qemu-trace.log 2>&1 < /dev/null & echo $! > $R/qemu.pid
sleep 10
kill -0 "$(qpid)" 2>/dev/null || { log "FAIL: tracing QEMU exited: $(tail -5 $R/qemu-trace.log)"; exit 1; }
if ! wait_ssh 120; then log "FAIL: no SSH under the tracing QEMU (slow boot?)"; exit 1; fi
ps_ "C:\\nvcap\\nvddi\\nvddictl.exe status" | head -5
tests l123
fetch l123
cp $R/qemu-trace.log $OUT/ 2>/dev/null
kill $LIMIT 2>/dev/null
log "== captures done"
exit 0
