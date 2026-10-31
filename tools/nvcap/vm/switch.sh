#!/bin/bash
# switch.sh vm   : stop the desktop (display manager), give the RTX 5090 to vfio-pci, boot nvcap (vm.sh).
#                  Any failure before the VM is up restores the desktop on its own.
# switch.sh host : shut nvcap down, give the 5090 back to the NVIDIA driver, start the display manager.
# Run as root. Log: /run/nvcap/switch.log
set -u
GPU=0000:01:00.0 AUD=0000:01:00.1
N=/steam/conduit-scratch/nvcap R=/run/nvcap
mkdir -p $R
exec >>$R/switch.log 2>&1
log() { echo "[$(date +%T)] $*"; }

nv_users() { fuser /dev/nvidia0 /dev/nvidiactl /dev/nvidia-modeset /dev/nvidia-uvm /dev/dri/by-path/pci-$GPU-card 2>/dev/null | tr -s ' '; }

to_host() {
	log "to host"
	if pid=$(cat $R/qemu.pid 2>/dev/null) && kill -0 "$pid" 2>/dev/null; then
		timeout 10 sudo -u xuw ssh -o BatchMode=yes -o ConnectTimeout=5 -p 2223 "Ole Algoritme@127.0.0.1" "shutdown /s /t 0" >/dev/null 2>&1 ||
			echo '{"execute":"qmp_capabilities"}{"execute":"system_powerdown"}' | timeout 5 socat - UNIX-CONNECT:$R/qmp.sock >/dev/null 2>&1
		for i in $(seq 120); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
		kill -0 "$pid" 2>/dev/null && { log "qemu still up after 120 s: quit"; kill "$pid"; sleep 3; kill -9 "$pid" 2>/dev/null; }
	fi
	for d in $GPU $AUD; do
		echo > /sys/bus/pci/devices/$d/driver_override
		[ -e /sys/bus/pci/devices/$d/driver ] && echo $d > /sys/bus/pci/devices/$d/driver/unbind
	done
	modprobe nvidia && modprobe nvidia_modeset && modprobe nvidia_drm && modprobe nvidia_uvm
	echo $GPU > /sys/bus/pci/drivers_probe; echo $AUD > /sys/bus/pci/drivers_probe
	systemctl start nvidia-persistenced
	log "5090: $(lspci -k -s 01:00.0 | grep 'in use')"
	nvidia-smi -L || log "nvidia-smi failed"
	systemctl start display-manager
	log "display manager started"
}

to_vm() {
	log "to vm"
	systemctl stop display-manager
	# The session's GPU users end with it; give them 30 s, then end what is left.
	for i in $(seq 30); do [ -z "$(nv_users)" ] && break; sleep 1; done
	if [ -n "$(nv_users)" ]; then
		log "still on the GPU: $(for p in $(nv_users); do printf '%s(%s) ' $p "$(cat /proc/$p/comm 2>/dev/null)"; done)"
		for p in $(nv_users); do kill $p 2>/dev/null; done; sleep 3
		for p in $(nv_users); do kill -9 $p 2>/dev/null; done; sleep 1
	fi
	systemctl stop nvidia-persistenced
	if ! timeout 60 modprobe -r nvidia_uvm nvidia_drm nvidia_modeset nvidia; then
		log "FAIL: NVIDIA modules would not unload"; to_host; return 1
	fi
	modprobe vfio-pci
	for d in $GPU $AUD; do
		echo vfio-pci > /sys/bus/pci/devices/$d/driver_override
		[ -e /sys/bus/pci/devices/$d/driver ] && echo $d > /sys/bus/pci/devices/$d/driver/unbind
		echo $d > /sys/bus/pci/drivers_probe
	done
	log "5090: $(lspci -k -s 01:00.0 | grep 'in use')"
	if ! lspci -k -s 01:00.0 | grep -q 'in use: vfio-pci'; then log "FAIL: not on vfio-pci"; to_host; return 1; fi
	setsid bash $N/vm.sh >$R/qemu.log 2>&1 < /dev/null & echo $! > $R/qemu.pid
	sleep 5
	if ! kill -0 "$(cat $R/qemu.pid)" 2>/dev/null; then log "FAIL: qemu exited: $(tail -5 $R/qemu.log)"; to_host; return 1; fi
	log "qemu pid $(cat $R/qemu.pid) up"
	for i in $(seq 60); do
		timeout 8 sudo -u xuw ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=no -p 2223 "Ole Algoritme@127.0.0.1" "echo up" 2>/dev/null | grep -q up && { log "SSH up after $((i*5)) s"; return 0; }
		kill -0 "$(cat $R/qemu.pid)" 2>/dev/null || { log "FAIL: qemu died: $(tail -5 $R/qemu.log)"; to_host; return 1; }
		sleep 5
	done
	log "WARN: no SSH after 5 min; VM left running (VNC $R/vnc.sock)"
}

case "${1:-}" in
vm) to_vm ;;
host) to_host ;;
*) echo "usage: switch.sh vm|host" >&2; exit 2 ;;
esac
