#!/bin/bash
# nvcap: win11 (overlay on the conduit win11 disks) with the RTX 5090 passed through.
# Runs QEMU in the foreground as root; the GPU must already be on vfio-pci (switch.sh).
# SSH: 127.0.0.1:2223. QMP: /run/nvcap/qmp.sock. VNC (emulated VGA): /run/nvcap/vnc.sock.
set -eu
N=/steam/conduit-scratch/nvcap
R=/run/nvcap
mkdir -p $R
# swtpm's AppArmor profile only allows libvirt's paths
TS=/run/libvirt/qemu/swtpm/nvcap.sock
mkdir -p /run/libvirt/qemu/swtpm /var/log/swtpm/libvirt/qemu
swtpm socket --tpm2 --tpmstate dir=/var/lib/libvirt/swtpm/nvcap/tpm2 --ctrl type=unixio,path=$TS \
	--terminate --daemon --log file=/var/log/swtpm/libvirt/qemu/nvcap.log
for i in $(seq 50); do [ -S $TS ] && break; sleep 0.1; done
HV=hv-time,hv-relaxed,hv-vapic,hv-spinlocks=0x1fff,hv-vpindex,hv-runtime,hv-synic,hv-stimer,hv-stimer-direct,hv-reset,hv-frequencies,hv-tlbflush,hv-ipi
exec taskset -c 8-15,24-31 /opt/conduit/bin/qemu-system-x86_64 \
	-name nvcap,debug-threads=on \
	-machine pc-q35-11.1,accel=kvm,smm=on,kernel-irqchip=on,hpet=off \
	-global ICH9-LPC.disable_s3=1 -global ICH9-LPC.disable_s4=1 \
	-cpu host,migratable=off,topoext=on,host-phys-bits=on,$HV \
	-smp 16,sockets=1,cores=8,threads=2 -m 16G -overcommit mem-lock=on \
	-rtc base=localtime,driftfix=slew \
	-drive if=pflash,format=raw,readonly=on,file=/usr/share/conduit/bios/conduit-bios.fd \
	-drive if=pflash,format=raw,file=$N/VARS.fd \
	-object iothread,id=io1 \
	-drive id=c,if=none,format=qcow2,file=$N/c.qcow2,discard=unmap \
	-device virtio-blk-pci,drive=c,iothread=io1,bootindex=1 \
	-drive id=g,if=none,format=qcow2,file=$N/g.qcow2 \
	-device virtio-blk-pci,drive=g,iothread=io1 \
	-netdev user,id=n0,hostfwd=tcp:127.0.0.1:2223-:22 \
	-device virtio-net-pci,netdev=n0,mac=52:54:00:56:cd:60 \
	-chardev socket,id=tpm0,path=$TS -tpmdev emulator,id=tpm0,chardev=tpm0 -device tpm-crb,tpmdev=tpm0 \
	-fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=262144 \
	-device pcie-root-port,id=rp1,chassis=1,slot=1,bus=pcie.0 \
	-device vfio-pci,host=0000:01:00.0,bus=rp1,addr=00.0,multifunction=on \
	-device vfio-pci,host=0000:01:00.1,bus=rp1,addr=00.1 \
	-device usb-ehci,id=ehci -device usb-tablet,bus=ehci.0 \
	-vga std -display none -vnc unix:$R/vnc.sock \
	-qmp unix:$R/qmp.sock,server=on,wait=off \
	-serial none -parallel none
