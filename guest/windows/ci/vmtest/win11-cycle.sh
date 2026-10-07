#!/usr/bin/env bash
# The one way to restart win11 (every job uses this; no hand-rolled copies).
#   win11-cycle.sh [new-backend-binary]
# Shuts win11 off (verified), stops its backend (verified), optionally installs
# a new conduit-backend (verified by content), starts it with
# `conduit up win11 --venus --display 5120x1440@240`, then checks the running
# backend's binary, its flags, conduit-venus, SSH, and the Helios driver/mode.
# Any failed check prints FAIL and exits non-zero; nothing is skipped silently.
set -u
H="${WIN_SSH:?set WIN_SSH=user@127.0.0.1 (the guest account)}"; MODE=5120x1440@240; NEW=${1:-}
# --verify: run only the post-start checks on the running VM (no shutdown, no start).
VERIFY=0; [ "$NEW" = --verify ] && { VERIFY=1; NEW=; }
fail() { echo "FAIL: $*"; exit 1; }
off() { virsh -c qemu:///session domstate win11 2>/dev/null | grep -q 'shut off'; }
if [ $VERIFY = 0 ]; then
echo "== cycle: shutdown"
# Flush C: first (W: holds only build output, rebuilt anyway; flushing it
# right after a build took minutes).
# Flush every volume first: a forced shutdown (/f) loses file data Windows
# has not written back yet (seen twice on 2026-10-06: a copied test program
# and the KMD's .map came back as zeros after the restart).
if ! off; then
    F=$(timeout 30 ssh -o ConnectTimeout=10 -o BatchMode=yes -p 2222 "$H" 'Write-VolumeCache -DriveLetter C; "C"' 2>&1 | tr -d '\r' | tr '\n' ' ')
    case "$F" in *C*) echo "ok: flushed C:";; *) echo "note: Write-VolumeCache did not return in 30 s; relying on a graceful shutdown to flush";; esac
fi
# Graceful first: a normal shutdown writes back every cached file. Forced
# (/f) only if it has not finished after 90 s.
if ! off; then
    ssh -o ConnectTimeout=10 -o BatchMode=yes -p 2222 "$H" 'shutdown /s /t 0' 2>/dev/null
    for j in $(seq 1 30); do sleep 3; off && break; done
fi
for i in $(seq 1 15); do off && break
    echo "note: graceful shutdown did not finish; forcing"
    ssh -o ConnectTimeout=10 -o BatchMode=yes -p 2222 "$H" 'shutdown /s /f /t 0' 2>/dev/null
    for j in 1 2 3 4 5 6 7; do sleep 3; off && break; done; done
off || fail "win11 did not reach 'shut off'"
echo "ok: win11 shut off"
systemctl --user stop conduit-backend@win11 2>/dev/null
for i in $(seq 1 20); do systemctl --user is-active -q conduit-backend@win11 || break; sleep 1; done
systemctl --user is-active -q conduit-backend@win11 && fail "conduit-backend@win11 still active"
pgrep -f 'conduit-backend.*/conduit/win11/' >/dev/null && fail "a win11 backend process survived"
pgrep -f 'conduit-venus.*win11' >/dev/null && { pkill -f 'conduit-venus.*win11'; sleep 2; }
echo "ok: backend stopped"
if [ -n "$NEW" ]; then
    [ -x "$NEW" ] || fail "no backend binary at $NEW"
    "$NEW" --help 2>&1 | grep -qE -- '--venus($| )' || fail "$NEW has no --venus (built without the venus feature?)"
    sudo -n install -m 755 "$NEW" /opt/conduit/bin/conduit-backend || fail "install"
    cmp -s "$NEW" /opt/conduit/bin/conduit-backend || fail "installed backend differs from $NEW"
    echo "ok: installed backend $(sha256sum /opt/conduit/bin/conduit-backend | cut -c1-12)"
fi
# VENUS_NEW=path: swap conduit-venus too (VM is off here), keeping one backup.
if [ -n "${VENUS_NEW:-}" ]; then
    V=/opt/conduit/bin/conduit-venus
    [ -x "$VENUS_NEW" ] || fail "no conduit-venus at $VENUS_NEW"
    for s in $(nm -D --undefined-only "$VENUS_NEW" | awk '/virgl_/{print $2}'); do
        nm -D --defined-only /opt/conduit/lib/libvirglrenderer.so.1 | grep -q " $s$" || fail "installed virglrenderer lacks $s"
    done
    [ -f $V.pre-import ] || sudo -n cp -p $V $V.pre-import || fail "conduit-venus backup"
    sudo -n install -m 755 "$VENUS_NEW" $V || fail "conduit-venus install"
    cmp -s "$VENUS_NEW" $V || fail "installed conduit-venus differs from $VENUS_NEW"
    echo "ok: installed conduit-venus $(sha256sum $V | cut -c1-12) (backup $V.pre-import)"
fi
# QEMU_NEW=path: swap the QEMU binary too (VM is off here), keeping one backup.
if [ -n "${QEMU_NEW:-}" ]; then
    Q=/opt/conduit/bin/qemu-system-x86_64
    [ -x "$QEMU_NEW" ] || fail "no QEMU binary at $QEMU_NEW"
    "$QEMU_NEW" --version | grep -q 'QEMU emulator version' || fail "$QEMU_NEW does not run"
    "$QEMU_NEW" -device help 2>/dev/null | grep -q vhost-user-test-device-pci || fail "$QEMU_NEW lacks vhost-user-test-device-pci"
    [ -f $Q.pre-0008 ] || sudo -n cp -p $Q $Q.pre-0008 || fail "QEMU backup"
    sudo -n install -m 755 "$QEMU_NEW" $Q || fail "QEMU install"
    cmp -s "$QEMU_NEW" $Q || fail "installed QEMU differs from $QEMU_NEW"
    echo "ok: installed QEMU $(sha256sum $Q | cut -c1-12) (backup $Q.pre-0008)"
fi
# PRESTART="cmd": run while win11 is off and its backend stopped (host
# installs, domain XML changes); a failure aborts before the start.
if [ -n "${PRESTART:-}" ]; then
    echo "== cycle: prestart"
    bash -c "$PRESTART" || fail "prestart: $PRESTART"
fi
# Huge pages: if the domain asks for them, reserve exactly its memory now
# (the VM is off, so its RAM is free) and verify before starting.
if virsh -c qemu:///session dumpxml --inactive win11 | grep -q '<hugepages'; then
    KIB=$(virsh -c qemu:///session dumpxml --inactive win11 | sed -n "s/.*<memory unit='KiB'>\([0-9]*\)<.*/\1/p")
    NEED=$(( KIB / 2048 ))
    sudo -n sh -c 'sync; echo 1 > /proc/sys/vm/compact_memory'
    sudo -n sysctl -q vm.nr_hugepages=$NEED
    GOT=$(awk '/HugePages_Total/{print $2}' /proc/meminfo)
    [ "$GOT" -ge "$NEED" ] || fail "only $GOT of $NEED huge pages reserved"
    echo "ok: $GOT x 2 MiB huge pages reserved for win11"
fi
echo "== cycle: start"
conduit up win11 --venus --display $MODE | tail -1
fi
# The unit runs `conduit _backend`, which starts conduit-venus and then execs
# the backend; wait until the main process IS the backend. Its /proc/PID/exe
# is not readable (the backend sandboxes itself), so the binary is checked by
# its path in argv[0] and the installed file's content (above).
P=0; CMD=
for i in $(seq 1 60); do
    P=$(systemctl --user show -p MainPID --value conduit-backend@win11)
    [ "${P:-0}" != 0 ] && CMD=$(tr '\0' ' ' < /proc/$P/cmdline 2>/dev/null)
    case "$CMD" in /opt/conduit/bin/conduit-backend\ *) break;; esac
    sleep 2
done
case "$CMD" in /opt/conduit/bin/conduit-backend\ *) ;; *) fail "the backend never started (main process: ${CMD:-none})";; esac
case "$CMD" in *--venus*) ;; *) fail "backend runs without --venus: $CMD";; esac
case "$CMD" in *"--display $MODE"*) ;; *) fail "backend runs without --display $MODE: $CMD";; esac
pgrep -f 'conduit-venus' >/dev/null || fail "conduit-venus not running"
QP=$(pgrep -f 'qemu-system-x86_64.*guest=win11' | head -1)
[ -n "$QP" ] || fail "no QEMU process for win11"
# Make the kernel's OOM killer pick anything but the VM.
sudo -n choom -p "$QP" -n -900 >/dev/null && echo "ok: QEMU oom_score_adj $(cat /proc/$QP/oom_score_adj)" || echo "note: could not set QEMU oom_score_adj"
cmp -s /proc/$QP/exe /opt/conduit/bin/qemu-system-x86_64 && echo "ok: QEMU pid $QP runs the installed binary" || echo "note: QEMU pid $QP exe not comparable (permissions)"
echo "ok: backend pid $P, /opt/conduit/bin/conduit-backend $(sha256sum /opt/conduit/bin/conduit-backend | cut -c1-12), --venus, --display $MODE, conduit-venus up"
for i in $(seq 1 60); do ssh -o ConnectTimeout=5 -o BatchMode=yes -p 2222 "$H" exit 2>/dev/null && break; sleep 5; done
ssh -o ConnectTimeout=5 -o BatchMode=yes -p 2222 "$H" exit 2>/dev/null || fail "no SSH after 5 min"
sleep 40
R=$(ssh -o ConnectTimeout=10 -p 2222 "$H" '$r=Get-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render; $v=Get-CimInstance Win32_VideoController | ? Name -match Helios; "$($v.CurrentHorizontalResolution)x$($v.CurrentVerticalResolution)@$($v.CurrentRefreshRate) $($v.DriverVersion) InitStg=$($r.InitStg) StVio=$($r.StVio)"')
echo "guest: $R"
case "$R" in "5120x1440@240 "*"InitStg=7 StVio=0"*) echo "ok: Helios up at $MODE";; *) fail "Helios not up as expected: $R";; esac
ssh -o ConnectTimeout=10 -p 2222 "$H" 'schtasks /create /f /tn ConduitApply /tr C:\Users\Public\apply.cmd /sc once /st 23:59 /it /ru $env:USERNAME | Out-Null; schtasks /run /tn ConduitApply | Out-Null; Start-Sleep 15; schtasks /delete /f /tn ConduitApply | Out-Null; Get-Content C:\Users\Public\display-apply.txt -EA 0 | Select-String "now the only|already|WARNING"'
echo "== cycle: done"
