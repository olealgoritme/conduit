#!/bin/bash
# /etc/libvirt/hooks/qemu.d/conduit -- installed by `conduit attach`.
# libvirt calls it as: conduit VM_NAME OPERATION SUB-OPERATION ...
# For VMs Conduit was attached to (/etc/conduit/libvirt/VM_NAME.conf), it
# starts the GPU backend before QEMU and stops it after QEMU is gone. The
# backend runs as the desktop user who attached the VM, never as root.
set -u
VM=$1 OP=$2 SUB=${3:-}
CONF=/etc/conduit/libvirt/$VM.conf
[ -f "$CONF" ] || exit 0
# shellcheck disable=SC1090
. "$CONF"
RUN=$(dirname "$CONDUIT_SOCKET")
PIDF=$RUN/backend.pid
LOG=/var/log/conduit-$VM-backend.log

stop_backend() {
  local p
  p=$(cat "$PIDF" 2>/dev/null) || return 0
  # Only signal it if it is still the backend we started.
  if [ -n "$p" ] && [ "$(cat /proc/"$p"/comm 2>/dev/null)" = "$(basename "$CONDUIT_BACKEND" | cut -c1-15)" ]; then
    kill -TERM "$p" 2>/dev/null
    for _ in $(seq 50); do kill -0 "$p" 2>/dev/null || break; sleep 0.1; done
    kill -KILL "$p" 2>/dev/null
  fi
  rm -f "$PIDF" "$CONDUIT_SOCKET"
}

case "$OP/$SUB" in
  prepare/begin)
    stop_backend
    install -d -m 0755 -o "$CONDUIT_USER" "$RUN"
    # setpriv execs (no extra process), so $! is the backend itself.
    setsid setpriv --reuid="$CONDUIT_USER" --regid="$(id -g "$CONDUIT_USER")" --init-groups \
      "$CONDUIT_BACKEND" --socket "$CONDUIT_SOCKET" \
      --caps graphics,video,utility,compute >"$LOG" 2>&1 </dev/null &
    echo $! > "$PIDF"
    for _ in $(seq 100); do [ -S "$CONDUIT_SOCKET" ] && break; sleep 0.1; done
    [ -S "$CONDUIT_SOCKET" ] || { echo "conduit: backend did not start, see $LOG" >&2; exit 1; }
    # QEMU runs as another user (libvirt-qemu / qemu) and must reach the socket.
    chmod 0666 "$CONDUIT_SOCKET"
    ;;
  release/end)
    stop_backend
    ;;
esac
exit 0
