#!/bin/bash
# build-disk.sh WORKDIR -- build a Conduit VM disk. Run as root (conduit create
# runs it through sudo). Everything it needs is in WORKDIR:
#   config.env        DISK SIZE_BYTES TARBALL DESKTOP GUEST_USER GUEST_PASSWORD
#                     VM_HOSTNAME GUEST_IP HOST_IP OWNER_UID OWNER_GID
#   guest/            files installed into the VM (see cli/assets/guest)
#   authorized_keys   ssh public key(s) for root and the user
#   virtio_gpu_nv.ko  the guest driver for the kernel Conduit boots
#
# The disk is a raw ext4 filesystem (the VM sees it as /dev/vda, no partition
# table), sparse, built from Ubuntu's 24.04 cloud root tarball.
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

[ "$(id -u)" = 0 ] || { echo "build-disk.sh must run as root" >&2; exit 1; }
[ -f "$TARBALL" ] || { echo "missing $TARBALL" >&2; exit 1; }

say "creating a ${SIZE_BYTES}-byte sparse disk"
rm -f "$DISK"
truncate -s "$SIZE_BYTES" "$DISK"
mkfs.ext4 -q -F -L conduit-root "$DISK"
mount -o loop "$DISK" "$MNT"

say "unpacking Ubuntu 24.04 (cloud root image)"
tar -xJf "$TARBALL" -C "$MNT" --numeric-owner --xattrs --xattrs-include='*'

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

# Serial console for `conduit logs NAME vm`.
in_vm systemctl enable serial-getty@hvc0.service >/dev/null 2>&1 || true

# --- Conduit guest bits -----------------------------------------------------------
say "installing the Conduit guest driver"
install -d "$MNT/opt/conduit-guest" "$MNT/mnt/nvidia"
install -m644 "$W/virtio_gpu_nv.ko" "$MNT/opt/conduit-guest/virtio_gpu_nv.ko"
install -m644 "$G/conduit-guest.service" "$MNT/etc/systemd/system/conduit-guest.service"
in_vm systemctl enable conduit-guest.service >/dev/null
install -m644 "$G/99-conduit.rules" "$MNT/etc/udev/rules.d/99-conduit.rules"
install -m644 "$G/71-conduit-seat.rules" "$MNT/etc/udev/rules.d/71-conduit-seat.rules"
install -m644 "$G/zz-conduit-nvidia.conf" "$MNT/etc/ld.so.conf.d/zz-conduit-nvidia.conf"
install -m644 "$G/conduit-nvidia.sh" "$MNT/etc/profile.d/conduit-nvidia.sh"
install -d "$MNT/etc/environment.d"
install -m644 "$G/90-conduit-nvidia.conf" "$MNT/etc/environment.d/90-conduit-nvidia.conf"

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
rm -f "$MNT/usr/sbin/policy-rc.d"
rm -f "$MNT/etc/resolv.conf"
if [ -e "$MNT/etc/resolv.conf.conduit" ] || [ -L "$MNT/etc/resolv.conf.conduit" ]; then
  mv "$MNT/etc/resolv.conf.conduit" "$MNT/etc/resolv.conf"
else
  ln -s ../run/systemd/resolve/stub-resolv.conf "$MNT/etc/resolv.conf"
fi
: > "$MNT/etc/machine-id"   # a fresh id on first boot
sync
cleanup
trap - EXIT ERR
chown "$OWNER_UID:$OWNER_GID" "$DISK"
say "done"
