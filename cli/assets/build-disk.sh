#!/bin/bash
# build-disk.sh WORKDIR -- build a Conduit VM disk, or convert one to the stock
# kernel. Run as root (conduit runs it through sudo). Everything it needs is in
# WORKDIR:
#   config.env          MODE (create | convert) DISK OWNER_UID OWNER_GID, and for
#                       create: SIZE_BYTES TARBALL DESKTOP GUEST_USER
#                       GUEST_PASSWORD VM_HOSTNAME GUEST_IP HOST_IP
#   guest/              files installed into the VM (see cli/assets/guest)
#   authorized_keys     ssh public key(s) for root and the user (create)
#   conduit-guest.deb   the guest driver package (DKMS source)
#
# The disk is a raw ext4 filesystem (the VM sees it as /dev/vda, no partition
# table), sparse, built from Ubuntu's 24.04 cloud root tarball. It carries
# Ubuntu's stock kernel (linux-image-generic) with its headers; DKMS builds the
# guest driver for it, and again for every kernel update inside the VM.
# Conduit boots that kernel straight from the disk's /boot.
#
# MODE=convert takes an existing disk (for example one that booted a custom
# kernel) and adds the stock kernel, headers, DKMS and conduit-guest to it.
set -euo pipefail

W=${1:?usage: build-disk.sh WORKDIR}
# shellcheck disable=SC1091
. "$W/config.env"
G=$W/guest
MNT=$(mktemp -d /tmp/conduit-build.XXXXXX)
say() { echo "conduit-build: $*"; }

cleanup() {
  set +e
  for m in dev/pts dev proc sys run; do
    mountpoint -q "$MNT/$m" && umount -l "$MNT/$m"
  done
  mountpoint -q "$MNT" && umount "$MNT"
  rmdir "$MNT" 2>/dev/null
}
trap cleanup EXIT
trap 'say "FAILED (line $LINENO). The partial disk is left at $DISK; conduit removes it."' ERR

MODE=${MODE:-create}
[ "$(id -u)" = 0 ] || { echo "build-disk.sh must run as root" >&2; exit 1; }

if [ "$MODE" = create ]; then
  [ -f "$TARBALL" ] || { echo "missing $TARBALL" >&2; exit 1; }
  say "creating a ${SIZE_BYTES}-byte sparse disk"
  rm -f "$DISK"
  truncate -s "$SIZE_BYTES" "$DISK"
  mkfs.ext4 -q -F -L conduit-root "$DISK"
  mount -o loop "$DISK" "$MNT"
  say "unpacking Ubuntu 24.04 (cloud root image)"
  tar -xJf "$TARBALL" -C "$MNT" --numeric-owner --xattrs --xattrs-include='*'
else
  [ -f "$DISK" ] || { echo "missing $DISK" >&2; exit 1; }
  say "checking $DISK"
  e2fsck -p "$DISK" >/dev/null || [ $? -le 1 ]
  mount -o loop "$DISK" "$MNT"
  trap 'say "FAILED (line $LINENO). The disk may be half converted; restore your backup."' ERR
fi

# --- chroot plumbing --------------------------------------------------------
for m in dev dev/pts proc sys run; do mkdir -p "$MNT/$m"; done
mount --bind /dev "$MNT/dev"
mount --bind /dev/pts "$MNT/dev/pts"
mount -t proc proc "$MNT/proc"
mount -t sysfs sys "$MNT/sys"
mount -t tmpfs tmpfs "$MNT/run"
# DNS for apt inside the chroot (restored to systemd-resolved's stub below).
mv "$MNT/etc/resolv.conf" "$MNT/etc/resolv.conf.conduit" 2>/dev/null || true
cat /etc/resolv.conf > "$MNT/etc/resolv.conf"
# Do not start services inside the chroot.
printf '#!/bin/sh\nexit 101\n' > "$MNT/usr/sbin/policy-rc.d"; chmod 755 "$MNT/usr/sbin/policy-rc.d"
in_vm() { chroot "$MNT" /usr/bin/env DEBIAN_FRONTEND=noninteractive LC_ALL=C.UTF-8 "$@"; }

# --- the stock kernel and the guest driver (both modes) --------------------------
# DKMS builds conduit-guest for every installed kernel that has headers, here
# and on every kernel update inside the VM. `uname -r` in this chroot is the
# host's kernel, so the build is asked for by version.
install_kernel_and_driver() {
  say "installing Ubuntu's stock kernel, headers and DKMS"
  in_vm apt-get update -q
  in_vm apt-get install -y -q --no-install-recommends \
    linux-image-generic linux-headers-generic \
    initramfs-tools dkms gcc make kmod
  say "installing the Conduit guest driver (conduit-guest, DKMS)"
  cp "$W/conduit-guest.deb" "$MNT/tmp/conduit-guest.deb"
  in_vm apt-get install -y -q --no-install-recommends /tmp/conduit-guest.deb
  rm -f "$MNT/tmp/conduit-guest.deb"
  local k ok=0
  for k in "$MNT"/lib/modules/*; do
    k=${k##*/}
    # build is an absolute symlink into the VM's /usr/src: test it as a link.
    [ -L "$MNT/lib/modules/$k/build" ] || [ -e "$MNT/lib/modules/$k/build" ] || continue
    in_vm dkms autoinstall -k "$k"
    if ls "$MNT/lib/modules/$k/updates/dkms/"virtio_gpu_nv.ko* >/dev/null 2>&1; then
      say "guest driver built for $k"; ok=1
    fi
  done
  [ "$ok" = 1 ] || { echo "the guest driver did not build for any installed kernel" >&2; exit 1; }
  # Loaded at boot by conduit-guest.service; also listed so udev-less boots load it.
  echo virtio_gpu_nv > "$MNT/etc/modules-load.d/conduit.conf"
  in_vm apt-get clean
}

# The Conduit guest services and settings (both modes).
install_guest_files() {
  install -d "$MNT/mnt/nvidia"
  install -m644 "$G/conduit-guest.service" "$MNT/etc/systemd/system/conduit-guest.service"
  in_vm systemctl enable conduit-guest.service >/dev/null
  install -m644 "$G/99-conduit.rules" "$MNT/etc/udev/rules.d/99-conduit.rules"
  install -m644 "$G/71-conduit-seat.rules" "$MNT/etc/udev/rules.d/71-conduit-seat.rules"
  install -m644 "$G/zz-conduit-nvidia.conf" "$MNT/etc/ld.so.conf.d/zz-conduit-nvidia.conf"
  install -m644 "$G/conduit-nvidia.sh" "$MNT/etc/profile.d/conduit-nvidia.sh"
  install -d "$MNT/etc/environment.d"
  install -m644 "$G/90-conduit-nvidia.conf" "$MNT/etc/environment.d/90-conduit-nvidia.conf"
  # Serial console for `conduit logs NAME vm`: ttyS0 under QEMU, hvc0 built-in.
  in_vm systemctl enable serial-getty@ttyS0.service >/dev/null 2>&1 || true
  in_vm systemctl enable serial-getty@hvc0.service >/dev/null 2>&1 || true
}

finish() {
  rm -f "$MNT/usr/sbin/policy-rc.d"
  rm -f "$MNT/etc/resolv.conf"
  if [ -e "$MNT/etc/resolv.conf.conduit" ] || [ -L "$MNT/etc/resolv.conf.conduit" ]; then
    mv "$MNT/etc/resolv.conf.conduit" "$MNT/etc/resolv.conf"
  else
    ln -s ../run/systemd/resolve/stub-resolv.conf "$MNT/etc/resolv.conf"
  fi
  sync
  cleanup
  trap - EXIT ERR
  chown "$OWNER_UID:$OWNER_GID" "$DISK"
  say "done"
}

if [ "$MODE" = convert ]; then
  install_kernel_and_driver
  install_guest_files
  # A disk made for a custom kernel loads its driver by path; that module
  # cannot load into the stock kernel, and the unit's later steps (the share
  # mount) would never run. conduit-guest.service replaces it.
  if [ -e "$MNT/etc/systemd/system/nvgpu.service" ]; then
    in_vm systemctl disable nvgpu.service >/dev/null 2>&1 || true
    rm -f "$MNT/etc/systemd/system/nvgpu.service"
    in_vm systemctl mask nvgpu.service >/dev/null 2>&1 || true
    say "disabled the old nvgpu.service (conduit-guest.service replaces it)"
    # Drop-ins that made other units depend on it (e.g. gdm) would now keep
    # them from starting at all: point them at conduit-guest.service instead.
    for d in "$MNT"/etc/systemd/system/*.service.d/*.conf; do
      [ -f "$d" ] && grep -q 'nvgpu\.service' "$d" && sed -i 's/nvgpu\.service/conduit-guest.service/g' "$d" \
        && say "repointed $(basename "$(dirname "$d")")/$(basename "$d") to conduit-guest.service"
    done
  fi
  finish
  exit 0
fi

# --- packages -----------------------------------------------------------------
PKGS="openssh-server sudo udev kmod systemd-resolved dbus-user-session
      libglvnd0 libegl1 libgl1 libgles2 libgbm1 libdrm2 libvulkan1 libwayland-client0
      libwayland-server0 libxkbcommon0 vulkan-tools mesa-utils pciutils less"
case "$DESKTOP" in
  gnome) PKGS="$PKGS ubuntu-desktop-minimal gnome-terminal dconf-cli" ;;
  xfce)  PKGS="$PKGS xfce4 xfce4-terminal lightdm lightdm-gtk-greeter xserver-xorg-core xserver-xorg-input-libinput dbus-x11" ;;
  none)  ;;
  *) echo "unknown desktop $DESKTOP" >&2; exit 1 ;;
esac
say "installing packages ($DESKTOP desktop); this downloads a lot and can take 10+ minutes"
in_vm apt-get update -q
# shellcheck disable=SC2086
in_vm apt-get install -y -q --no-install-recommends $PKGS
in_vm apt-get clean
install_kernel_and_driver

# --- the VM's own settings ------------------------------------------------------
say "configuring the VM"
echo "$VM_HOSTNAME" > "$MNT/etc/hostname"
grep -q "127.0.1.1" "$MNT/etc/hosts" || echo "127.0.1.1 $VM_HOSTNAME" >> "$MNT/etc/hosts"
echo "/dev/vda / ext4 rw,errors=remount-ro 0 1" > "$MNT/etc/fstab"

# No cloud: cloud-init would wait for a datasource that never comes; snapd's
# seeding is slow and pointless here.
touch "$MNT/etc/cloud/cloud-init.disabled" 2>/dev/null || true
in_vm systemctl mask snapd.seeded.service systemd-networkd-wait-online.service >/dev/null 2>&1 || true

# Network: static address on the VM's private link, NAT on the host.
rm -f "$MNT"/etc/netplan/*.yaml
sed -e "s/@GUEST_IP@/$GUEST_IP/" -e "s/@HOST_IP@/$HOST_IP/" "$G/10-conduit.network" \
  > "$MNT/etc/systemd/network/10-conduit.network"
in_vm systemctl enable systemd-networkd systemd-resolved >/dev/null
if [ -d "$MNT/etc/NetworkManager/conf.d" ]; then
  install -m644 "$G/nm-unmanaged.conf" "$MNT/etc/NetworkManager/conf.d/10-conduit-unmanaged.conf"
fi

# ssh: keys only.
in_vm ssh-keygen -A >/dev/null
mkdir -p "$MNT/etc/ssh/sshd_config.d"
printf 'PasswordAuthentication no\nPermitRootLogin prohibit-password\n' > "$MNT/etc/ssh/sshd_config.d/10-conduit.conf"
in_vm systemctl enable ssh >/dev/null 2>&1 || true
install -d -m700 "$MNT/root/.ssh"
install -m600 "$W/authorized_keys" "$MNT/root/.ssh/authorized_keys"

# The user: autologin, passwordless sudo, GPU/input groups.
if ! in_vm id "$GUEST_USER" >/dev/null 2>&1; then
  in_vm useradd -m -s /bin/bash -G sudo,video,render,input "$GUEST_USER"
fi
echo "$GUEST_USER:$GUEST_PASSWORD" | in_vm chpasswd
echo "$GUEST_USER ALL=(ALL) NOPASSWD:ALL" > "$MNT/etc/sudoers.d/90-conduit"
chmod 440 "$MNT/etc/sudoers.d/90-conduit"
UH=$MNT/home/$GUEST_USER
install -d -m700 "$UH/.ssh"
install -m600 "$W/authorized_keys" "$UH/.ssh/authorized_keys"
in_vm chown -R "$GUEST_USER:$GUEST_USER" "/home/$GUEST_USER/.ssh"

# --- Conduit guest bits -----------------------------------------------------------
install_guest_files

# --- desktop ------------------------------------------------------------------------
case "$DESKTOP" in
  gnome)
    sed "s/@USER@/$GUEST_USER/" "$G/gdm-custom.conf" > "$MNT/etc/gdm3/custom.conf"
    # GDM disables Wayland on unknown "nvidia" setups; our card is a plain KMS device.
    : > "$MNT/etc/udev/rules.d/61-gdm.rules"
    install -d "$MNT/etc/dconf/db/local.d" "$MNT/etc/dconf/profile"
    install -m644 "$G/dconf-00-conduit" "$MNT/etc/dconf/db/local.d/00-conduit"
    printf 'user-db:user\nsystem-db:local\n' > "$MNT/etc/dconf/profile/user"
    in_vm dconf update
    in_vm systemctl set-default graphical.target >/dev/null
    ;;
  xfce)
    install -d "$MNT/etc/lightdm/lightdm.conf.d"
    sed "s/@USER@/$GUEST_USER/" "$G/lightdm-autologin.conf" > "$MNT/etc/lightdm/lightdm.conf.d/50-conduit.conf"
    in_vm groupadd -f autologin
    in_vm usermod -aG autologin "$GUEST_USER"
    in_vm systemctl set-default graphical.target >/dev/null
    ;;
  none)
    in_vm systemctl set-default multi-user.target >/dev/null
    ;;
esac

# --- finish ---------------------------------------------------------------------------
: > "$MNT/etc/machine-id"   # a fresh id on first boot
finish
