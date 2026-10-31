#!/bin/bash
# night.sh: one unattended capture run on the real RTX 5090.
#   1. desktop off, 5090 to the nvcap VM (switch.sh vm); tools deployed with a
#      Defender exclusion and checked by hash; the display-dark logon task
#   2. l1   : level 1 (nvtrace): selftest, vk/cuda/d3d11/d3d12 tests, DWM
#   3. l13  : level 3 (nvddi) installed boot-start with the capture profile,
#             reboot, level 1+3
#   4. l123 : VM restarted on the tracing QEMU (level 2), level 1+2+3 from boot
#   5. VM off, 5090 back, desktop back with the monitor off (switch.sh host);
#      decoders write their outputs next to the logs
# Any failure or the time limit ends in step 5. Run as root:
#   setsid night.sh > /dev/null 2>&1 < /dev/null &
# Env:
#   NIGHT_STEPS   steps to run, in order (default "l1 l13 l123"); e.g. "l13 l123"
#                 for a run on a guest that already has nvddi installed
#   NIGHT_TAG     suffix of the output directory (night-<date>-<time><tag>)
#   NIGHT_LIMIT_S global time limit (default 14400)
#   NVCAP_DARK    topology (default) | dpms | off: how the guest keeps the
#                 NVIDIA outputs dark (dark/nvcap-dark.ps1)
#   NVCAP_DIR     the nvcap directory (default /steam/conduit-scratch/nvcap)
#   NVCAP_HOST_USER  host user that owns the SSH key and the tool checkouts
#                 (default: owner of this directory)
#   NVTRACE_DIR, NVDDI_DIR  tool checkouts (default ~HOST_USER/code/{nvtrace,nvddi})
#   NVCAP_GUEST_USER  guest account for SSH (default "Ole Algoritme")
#   NVTRACE_*     passed on to vm-trace.sh (e.g. NVTRACE_DIRTY=0x8)
set -u
N=${NVCAP_DIR:-/steam/conduit-scratch/nvcap}  # VM disks, traces, scripts, dark/
R=/run/nvcap
OUT=$N/night-$(date +%Y%m%d-%H%M)${NIGHT_TAG:-}
STEPS=${NIGHT_STEPS:-l1 l13 l123}
DARK=${NVCAP_DARK:-topology}
HUSER=${NVCAP_HOST_USER:-$(stat -c %U "$N")}
HHOME=$(getent passwd "$HUSER" | cut -d: -f6)
NVT=${NVTRACE_DIR:-$HHOME/code/nvtrace}
NVD=${NVDDI_DIR:-$HHOME/code/nvddi}
GUSER=${NVCAP_GUEST_USER:-Ole Algoritme}
mkdir -p "$OUT" $R
touch "$OUT/start.stamp"           # decode(): traces newer than this are ours
exec > >(tee -a "$OUT/night.log") 2>&1
log() { echo "[$(date +%T)] $*"; }
ssh_() { timeout "${T:-120}" sudo -u "$HUSER" ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=no -p 2223 "$GUSER@127.0.0.1" "$@"; }
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
	bash "$N/switch.sh" host           # also turns the monitor off on the login screen
	log "switch: $(tail -4 $R/switch.log | tr '\n' ' ')"
	log "== decoding"
	decode
	log "== done: $OUT"
}
trap finish EXIT
( sleep "${NIGHT_LIMIT_S:-14400}"; log "TIME LIMIT"; kill -TERM $$ ) & LIMIT=$!
trap 'log "terminated"; exit 1' TERM INT

# ---- tools in the guest -------------------------------------------------------------
# Defender quarantined vktest.exe in an earlier run (CreateProcess error 225 =
# ERROR_VIRUS_INFECTED, then "not found"): exclude the tool folders before the
# copy, re-check every binary by hash before each level, and redeploy if needed.
TOOLFILES="nvtrace/*.exe nvtrace/*.dll nvddi/*.exe nvddi/*.sys"
host_hashes() {
	( cd "$NVT/dist" && sha256sum *.exe *.dll | sed 's#  #  nvtrace/#'
	  cd "$NVD/dist" && sha256sum *.exe *.sys | sed 's#  #  nvddi/#' ) | sort -k2
}
guest_hashes() {
	ps_ "foreach (\$d in 'nvtrace','nvddi') { Get-ChildItem C:\\nvcap\\\$d\\* -Include *.exe,*.dll,*.sys -EA 0 | Get-FileHash -Algorithm SHA256 | % { \$_.Hash.ToLower() + '  ' + \$d + '/' + (Split-Path \$_.Path -Leaf) } }" | sort -k2
}
deploy() {
	log "deploy tools"
	ps_ "New-Item -ItemType Directory -Force C:\\nvcap\\nvtrace,C:\\nvcap\\nvddi,C:\\nvcap\\logs,C:\\nvcap\\dark,C:\\nvddi | Out-Null
if (Get-Command Add-MpPreference -EA 0) {
  # right after boot Defender may not take preferences yet: retry for 60 s
  for (\$i = 0; \$i -lt 12; \$i++) {
    try { Add-MpPreference -ExclusionPath 'C:\\nvcap','C:\\nvddi' -ErrorAction Stop
          if ((Get-MpPreference).ExclusionPath -contains 'C:\\nvcap') { break } } catch { \$e = \$_.Exception.Message }
    Start-Sleep -Seconds 5 }
  if ((Get-MpPreference).ExclusionPath -notcontains 'C:\\nvcap') { 'defender exclusion FAILED: ' + \$e }
  'defender exclusions: ' + ((Get-MpPreference).ExclusionPath -join ', ')
  \$t = Get-MpThreatDetection -EA 0 | Sort-Object InitialDetectionTime | Select-Object -Last 3
  foreach (\$x in \$t) { 'defender detection: ' + \$x.InitialDetectionTime + ' ' + (\$x.Resources -join ' ') }
} else { 'defender cmdlets not available' }"
	tar -C "$NVT/dist" -cf - --exclude=__pycache__ . | ssh_ "tar -xf - -C C:/nvcap/nvtrace"
	tar -C "$NVD/dist" -cf - . | ssh_ "tar -xf - -C C:/nvcap/nvddi"
	tar -C "$N/dark" -cf - nvcap-dark.ps1 | ssh_ "tar -xf - -C C:/nvcap/dark"
	check_tools
}
check_tools() { # 0 = every binary present with the host's hash
	local d; d=$(diff <(host_hashes) <(guest_hashes))
	if [ -n "$d" ]; then
		log "tool check: MISMATCH (missing, changed or quarantined):"; echo "$d" | head -20
		ps_ "Get-MpThreatDetection -EA 0 | Sort-Object InitialDetectionTime | Select-Object -Last 5 | % { 'defender detection: ' + \$_.InitialDetectionTime + ' ' + (\$_.Resources -join ' ') }"
		return 1
	fi
	log "tool check: $(host_hashes | wc -l) binaries ok"
}
ensure_tools() { check_tools >/dev/null 2>&1 || { deploy || log "WARN: tools still differ after redeploy"; }; }

# ---- NVIDIA outputs dark ----------------------------------------------------------
# A logon task and a 5-minute repeat task in the guest run dark/nvcap-dark.ps1;
# dark_now runs it once at once (after every boot) and logs what it found.
dark_install() {
	[ "$DARK" = off ] && return 0
	local cmd="powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\\nvcap\\dark\\nvcap-dark.ps1 -Mode $DARK -Log C:\\nvcap\\dark\\dark.log"
	# the SSH logon's USERDOMAIN is WORKGROUP: take the account from the token
	ps_ "\$ErrorActionPreference = 'Stop'
try {
\$u = [Security.Principal.WindowsIdentity]::GetCurrent().Name
\$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '${cmd#powershell.exe }'
\$p = New-ScheduledTaskPrincipal -UserId \$u -LogonType Interactive -RunLevel Highest
\$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 2) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName nvcap-dark -Force -Action \$a -Principal \$p -Settings \$s -Trigger (New-ScheduledTaskTrigger -AtLogOn -User \$u) | Out-Null
\$r = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) -RepetitionInterval (New-TimeSpan -Minutes 5)
Register-ScheduledTask -TaskName nvcap-dark-repeat -Force -Action \$a -Principal \$p -Settings \$s -Trigger \$r | Out-Null
'dark tasks: ' + ((Get-ScheduledTask -TaskName nvcap-dark* | % { \$_.TaskName + '=' + \$_.State }) -join ' ')
} catch { 'dark tasks FAILED: ' + \$_.Exception.Message }" | tail -3
}
dark_now() { # $1 = label
	[ "$DARK" = off ] && return 0
	T=60 insession "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\\nvcap\\dark\\nvcap-dark.ps1 -Mode $DARK > C:\\nvcap\\logs\\dark-$1.txt 2>&1"
	log "dark ($1): $(T=20 ssh_ "type C:\\nvcap\\logs\\dark-$1.txt" 2>/dev/null | tr -d '\r' | grep -E 'active path|topology|dpms|error' | tr '\n' ' ')"
}

# ---- one level ---------------------------------------------------------------------
fetch() { # $1 = label
	mkdir -p "$OUT/$1"
	T=600 ssh_ "tar -cf - -C C:/ nvcap/logs" 2>/dev/null | tar -x -C "$OUT/$1" 2>/dev/null
	T=600 ssh_ "tar -cf - -C C:/ nvddi" 2>/dev/null | tar -x -C "$OUT/$1" 2>/dev/null
	ps_ "Remove-Item C:\\nvcap\\logs\\* -Recurse -Force -EA 0" >/dev/null
	log "fetched $1: $(du -sh "$OUT/$1" | cut -f1)"
}
tests() { # $1 = label; level 1 under nvinject, nvddi marks if present
	local L=$1
	ensure_tools
	ps_ "New-Item -ItemType Directory -Force C:\\nvcap\\logs\\$L | Out-Null"
	ps_ "if (Test-Path C:\\nvcap\\nvddi\\nvddictl.exe) { C:\\nvcap\\nvddi\\nvddictl.exe status 2>&1 }" | head -5
	T=200 insession "cd /d C:\\nvcap\\nvtrace && nvinject -d C:\\nvcap\\logs\\$L\\st --selftest > C:\\nvcap\\logs\\$L\\selftest.txt 2>&1"
	log "selftest: $(T=20 ssh_ "type C:\\nvcap\\logs\\$L\\selftest.txt" 2>/dev/null | tail -2 | tr -d '\r' | tr '\n' ' ')"
	for t in vktest cudatest d3d11test d3d12test; do
		ps_ "if (Test-Path C:\\nvcap\\nvddi\\nvddictl.exe) { C:\\nvcap\\nvddi\\nvddictl.exe mark $L-$t | Out-Null }"
		# full paths: nvinject must not depend on the working directory
		T=300 insession "cd /d C:\\nvcap\\nvtrace && C:\\nvcap\\nvtrace\\nvinject.exe -d C:\\nvcap\\logs\\$L\\$t -t 240 C:\\nvcap\\nvtrace\\$t.exe > C:\\nvcap\\logs\\$L\\$t.txt 2>&1"
		log "$t: $(T=20 ssh_ "type C:\\nvcap\\logs\\$L\\$t.txt" 2>/dev/null | tr -d '\r' | grep -E 'PASS|FAIL|not found|failed \(' | tail -1)"
	done
	# DWM for 20 s while a test presents
	ps_ "C:\\nvcap\\nvtrace\\nvinject.exe -d C:\\nvcap\\logs\\$L\\dwm --dwm" | tail -2
	T=120 insession "cd /d C:\\nvcap\\nvtrace && d3d11test.exe --frames 600 > C:\\nvcap\\logs\\$L\\dwm-present.txt 2>&1"
	sleep 20
	ps_ "C:\\nvcap\\nvtrace\\nvinject.exe -d C:\\nvcap\\logs\\$L\\dwm --stop" | tail -2
	ps_ "Get-Process dwm -EA 0 | % { 'dwm alive ' + \$_.Id }"
	ps_ "if (Test-Path C:\\nvcap\\nvddi\\nvddictl.exe) { C:\\nvcap\\nvddi\\nvddictl.exe flush | Out-Null }"
	ps_ "Copy-Item C:\\nvcap\\dark\\dark.log C:\\nvcap\\logs\\$L\\ -EA 0"
}
decode() {
	local QT=$N/qemu-trace/tools d f
	for d in "$OUT"/*/; do
		[ -d "$d" ] || continue
		find "$d" -name '*.ntr' | while read -r f; do
			timeout 900 python3 "$NVT/dist/ntrdump.py" "$f" --summary > "$f.summary.txt" 2>&1
			timeout 900 python3 "$NVT/dist/ntrdump.py" "$f" --rm > "$f.rm.txt" 2>&1
		done
		find "$d" -name '*.ndl' | while read -r f; do
			timeout 900 python3 "$NVD/decoder/ndldump.py" "$f" --summary > "$f.summary.txt" 2>&1
		done
	done
	# level 2: the trace files written during this run (rotated sets together)
	find "$N/traces" -newer "$OUT/start.stamp" -name '*.nvt.*' -printf '%h\n' 2>/dev/null | sort -u |
	while read -r d; do
		local b; b=$(basename "$d")
		local fs; fs=$(ls "$d"/*.nvt.* 2>/dev/null | grep -v '\.txt$\|\.json$')
		[ -n "$fs" ] || continue
		cp "$d/info.txt" "$OUT/l2-$b.info.txt" 2>/dev/null
		timeout 1800 python3 "$QT/mmiodump.py" summary $fs > "$OUT/l2-$b.summary.txt" 2>&1
		timeout 1800 python3 "$QT/mmiodump.py" rmlog --tree $fs > "$OUT/l2-$b.rmlog.txt" 2>&1
		timeout 1800 python3 "$QT/mmiodump.py" boot $fs --json "$OUT/l2-$b.boot.json" > "$OUT/l2-$b.boot.txt" 2>&1
		timeout 1800 python3 "$QT/mmiodump.py" flcn $fs > "$OUT/l2-$b.flcn.txt" 2>&1
		timeout 1800 python3 "$QT/mmiodump.py" fsp $fs > "$OUT/l2-$b.fsp.txt" 2>&1
		timeout 1800 python3 "$QT/mmiodump.py" timeline $fs --types note > "$OUT/l2-$b.notes.txt" 2>&1
		log "decoded $b: $(grep -m1 '^GSP:' "$OUT/l2-$b.summary.txt")"
	done
}

step_l1() {
	log "== level 1"
	tests l1
	fetch l1
}
step_l13() {
	log "== install nvddi (test signing, boot start, capture profile) and reboot"
	ps_ "bcdedit /set testsigning on | Out-Null; reg add 'HKLM\\SYSTEM\\CurrentControlSet\\Control\\DeviceGuard\\Scenarios\\HypervisorEnforcedCodeIntegrity' /v Enabled /t REG_DWORD /d 0 /f | Out-Null; certutil -f -addstore Root C:\\nvcap\\nvddi\\nvddi-test.cer | Out-Null; certutil -f -addstore TrustedPublisher C:\\nvcap\\nvddi\\nvddi-test.cer | Out-Null; cd C:\\nvcap\\nvddi; cmd /c install.cmd boot 2>&1 | Select-String 'profile|ERROR' | % { \$_.Line }"
	T=15 ssh_ "shutdown /r /t 0" >/dev/null 2>&1
	sleep 30
	if ! wait_ssh 72; then log "FAIL: guest did not come back after the nvddi reboot"; exit 1; fi
	dark_now l13
	ps_ "C:\\nvcap\\nvddi\\nvddictl.exe status" | tee "$OUT/nvddi-status-l13.txt" | head -8
	if ! grep -qi "attached.*yes" "$OUT/nvddi-status-l13.txt"; then
		log "nvddi not attached at boot: restart-gpu.ps1"
		ps_ "powershell -ExecutionPolicy Bypass -File C:\\nvcap\\nvddi\\restart-gpu.ps1" | tail -3
		ps_ "C:\\nvcap\\nvddi\\nvddictl.exe status" | head -8
	fi
	ps_ "Get-PnpDevice -InstanceId 'PCI\\VEN_10DE&DEV_2B85*' | % { \$_.Status + ' ' + \$_.FriendlyName }"
	log "== level 1+3"
	tests l13
	fetch l13
}
step_l123() {
	log "== restart on the tracing QEMU"
	vm_down
	setsid bash "$N/vm-trace.sh" > $R/qemu-trace.log 2>&1 < /dev/null & echo $! > $R/qemu.pid
	sleep 10
	kill -0 "$(qpid)" 2>/dev/null || { log "FAIL: tracing QEMU exited: $(tail -5 $R/qemu-trace.log)"; exit 1; }
	if ! wait_ssh 120; then log "FAIL: no SSH under the tracing QEMU (slow boot?)"; exit 1; fi
	dark_now l123
	ps_ "if (Test-Path C:\\nvcap\\nvddi\\nvddictl.exe) { C:\\nvcap\\nvddi\\nvddictl.exe status 2>&1 }" | tee "$OUT/nvddi-status-l123.txt" | head -8
	log "== level 1+2+3"
	tests l123
	fetch l123
	cp $R/qemu-trace.log "$OUT/" 2>/dev/null
}

# ---- 0. preconditions --------------------------------------------------------------
pgrep -x cs2 >/dev/null && { log "cs2 is running: not starting"; trap - EXIT; exit 1; }
virsh -c qemu:///session domstate win11 2>/dev/null | grep -q "shut off" || { log "conduit win11 is not off: not starting"; trap - EXIT; exit 1; }
[ "$(df --output=avail -BG /steam | tail -1 | tr -dc 0-9)" -gt 100 ] || { log "less than 100 GB free on /steam"; trap - EXIT; exit 1; }
for s in $STEPS; do
	case $s in l1|l13|l123) ;; *) log "unknown step $s"; trap - EXIT; exit 2;; esac
done
log "steps: $STEPS, dark: $DARK, tools: $NVT $NVD"

# ---- 1. VM with the 5090 -----------------------------------------------------------
log "== switch to the VM"
bash "$N/switch.sh" vm
wait_ssh 12 || { log "FAIL: no SSH"; exit 1; }
deploy || log "WARN: tool check failed after deploy (see above)"
dark_install
dark_now boot

for s in $STEPS; do "step_$s"; done
kill $LIMIT 2>/dev/null
log "== captures done"
exit 0
