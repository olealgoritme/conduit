# Packaging

Every package is built by one script, `packaging/build.sh`. The release
workflow, the RPM spec, the PKGBUILD and the flake all produce the same
layout, so a layout change is made in one place.

## What gets installed

| Path | What |
|---|---|
| `/opt/conduit/bin/conduit` | the CLI |
| `/opt/conduit/bin/conduit-backend` | GPU backend (static musl in release builds) |
| `/opt/conduit/bin/conduit-userspace` | userspace backend tool (`conduit-userspace`, same crate) |
| `/opt/conduit/bin/conduit-viewer` | Wayland/X11 viewer |
| `/opt/conduit/bin/conduit-vmm` | built-in VM runner (static musl in release builds) |
| `/opt/conduit/bin/qemu-system-x86_64`, `share/qemu/` | bundled QEMU 11.1 |
| `/opt/conduit/share/conduit/supported-drivers.txt` | driver releases with backend ABI tables (`conduit doctor`) |
| `/opt/conduit/share/conduit/guest/conduit-guest.deb` | the guest driver package `conduit create` and `conduit stock-kernel` install into VMs (built at stage time with nfpm) |
| `/opt/conduit/libexec/conduit-integrate` | AppArmor/SELinux/desktop hookup (`enable`/`disable`) |
| `/opt/conduit/lib/` | tarball only: the viewer's and QEMU's shared libraries |
| `/opt/conduit/share/doc/conduit/` | LICENSE and every component's LICENSE/NOTICE |
| `/usr/bin/conduit` (packages), `/usr/local/bin/conduit` (tarball) | symlink to the CLI |
| `/usr/share/applications/conduit.desktop` (`/usr/local/share/...` for the tarball) | desktop entry |
| `/etc/apparmor.d/abstractions/conduit` | AppArmor rules for libvirt's QEMU |

The guest package installs `/usr/src/conduit-guest-<version>/` (module source
plus `dkms.conf`); DKMS builds `conduit_gpu.ko` into
`/lib/modules/<kver>/updates/dkms/` for every kernel 6.4 or newer. It also
ships the files in `guest/system/` (module autoload, the modprobe.d entry
that retires the old `virtio_gpu_nv` name, the user-namespace sysctl, and the
setup script every package format runs after install; see
`guest/linux/README.md`).

## Files

```
packaging/
├── build.sh                   the build: deps, rust, viewer, qemu, stage, bundle-libs, guest-src, package
├── nfpm/conduit.yaml          host package: one template -> .deb, .rpm, Arch .pkg.tar.zst
├── nfpm/conduit-guest.yaml    guest DKMS package: .deb, .rpm
├── deb/conduit/               postinst, prerm (used by nfpm for all three formats)
├── deb/conduit-guest/         postinst, prerm (dkms add/install/remove)
├── dkms/dkms.conf             DKMS config, kernel >= 6.4 via BUILD_EXCLUSIVE_KERNEL
├── rpm/conduit.spec           source RPM build (COPR/OBS)
├── rpm/conduit-guest.spec     source RPM for the guest (DKMS, noarch)
├── arch/PKGBUILD              split package: conduit + conduit-guest-dkms (AUR)
├── arch/conduit.install
├── tarball/install.sh         -> /opt/conduit, /usr/local/bin/conduit
├── tarball/uninstall.sh       also installed as /opt/conduit/uninstall.sh
└── common/                    desktop entry, AppArmor abstraction, conduit-integrate
```

## Building locally

```sh
sudo packaging/build.sh deps          # apt, dnf or pacman
packaging/build.sh rust               # static musl; RUST_TARGET=host for a glibc build
packaging/build.sh viewer
packaging/build.sh qemu               # slow; BUNDLE_QEMU=0 to skip
packaging/build.sh stage
packaging/build.sh package deb        # or rpm, archlinux (needs nfpm)

# tarball
LINK_DIR=/usr/local/bin packaging/build.sh stage
packaging/build.sh bundle-libs
packaging/build.sh package tarball

# guest
packaging/build.sh package guest-deb  # or guest-rpm
```

Output goes to `dist/out/`. `VERSION` overrides the version (default:
`git describe`).

Runtime dependencies of the .deb/.rpm/Arch packages are not hand-written:
`build.sh` reads the `NEEDED` libraries of the staged viewer and QEMU and maps
them to Debian/Arch package names (`dpkg -S`, `pacman -Qo`) or RPM soname
requirements (`libfoo.so.1()(64bit)`, which work on Fedora and openSUSE
alike). That is why each format is built in its own distribution's container.

## Contracts with the components

These are assumptions the packaging makes. Change them here and in
`build.sh` together.

**Binary names.** `build.sh` (top) and `flake.nix` hold the cargo/make output names
as variables: `BACKEND_BIN_SRC=conduit-backend` (cargo package `device`,
feature `vhost-user`), `VIEWER_BIN_SRC=conduit-viewer`,
`VMM_BIN_SRC=conduit-vmm` (built with `--no-default-features`),
`CLI_BIN_SRC=conduit`. When a component is renamed, change the variable;
installed names (`conduit-*`) stay.

**QEMU.** `build.sh qemu` runs `host/qemu/build-qemu.sh --prefix /opt/conduit`
with `WORKDIR=dist/qemu-work` (fetch with signature + sha256 check, apply
`host/qemu/patches/`, headless configure, build), deliberately without
`--install`, then runs `DESTDIR=dist/qemu-root ninja -C dist/qemu-work/build install`
itself so no root or sudo is needed. The build needs `gpg`, `curl` and
`patch`; `build.sh deps` installs them. The flake builds the same 11.1.2
tarball (same sha256) with the same patches on top of nixpkgs' QEMU
expression.

**CLI.** `cli/src/paths.rs` finds its helpers as `$CONDUIT_PREFIX/bin/<name>`
(`CONDUIT_PREFIX` defaults to `/opt/conduit`; the flake sets it to its store
path; `CONDUIT_BACKEND`, `CONDUIT_QEMU`, ... override single tools). libvirt
VMs get their backend socket under `/run/conduit/libvirt/<vm>/`; user
sessions use `$XDG_RUNTIME_DIR/conduit/`. The AppArmor rules allow exactly
these. The desktop entry runs `conduit view` with no argument, which should
open the default (or only) VM.

**Rust toolchain.** `rust-toolchain.toml` at the repo root pins 1.90.0 with
the gnu and musl targets for all Rust projects. Don't add a
`rust-toolchain.toml` in a subdirectory: it would win over the root file.

## AppArmor (Ubuntu, Debian, openSUSE)

libvirt confines each VM's QEMU with a generated profile that includes
`abstractions/libvirt-qemu`, which in turn includes
`/etc/apparmor.d/local/abstractions/libvirt-qemu` if it exists. The packages
install `/etc/apparmor.d/abstractions/conduit` and `conduit-integrate enable`
adds one marked line to the local file:

```
include if exists <abstractions/conduit> # added by conduit-integrate
```

The abstraction allows `/opt/conduit/bin/qemu-system-x86_64` (rmix), its
data and libraries, and the backend socket directories. A second marked line
in `local/usr.lib.libvirt.virt-aa-helper` lets libvirt's profile generator
read `/opt/conduit`. `conduit-integrate disable` (run on package removal)
deletes exactly the marked lines. New VMs pick the rules up at start; nothing
needs restarting.

## SELinux (Fedora, RHEL)

`conduit-integrate enable` labels the bundled QEMU so libvirt's `svirt`
domain may execute it:

```sh
semanage fcontext -a -t qemu_exec_t /opt/conduit/bin/qemu-system-x86_64
restorecon -F /opt/conduit/bin/qemu-system-x86_64
```

`semanage` comes from `policycoreutils-python-utils` (a recommended
dependency of the RPM). The backend socket directory is not labelled
automatically: a system-libvirt VM that cannot connect to it shows an AVC
denial (`ausearch -m avc -ts recent`). Until the right type is settled, label
the directory for svirt and report the denial:

```sh
sudo semanage fcontext -a -t svirt_image_t '/run/conduit(/.*)?'
sudo restorecon -R /run/conduit
```

User-session VMs (`qemu:///session`) are not confined by svirt and need
nothing.

## Guest module: DKMS, and akmod

The guest package is DKMS-only on every distribution: one noarch source
package works on Debian, Ubuntu, Fedora (`dkms` is in Fedora), RHEL (EPEL)
and openSUSE. `dkms.conf` limits builds to Linux 6.4+ with a
`BUILD_EXCLUSIVE_KERNEL` regex (works on old DKMS versions, unlike
`BUILD_EXCLUSIVE_KERNEL_MIN`). The postinst builds for the running kernel and
reports, rather than fails, when headers are missing.

An akmod (RPM Fusion style) variant would need a `conduit-guest-kmod.spec`
using `kmodtool`; it is not provided. Add it only if a Fedora user base asks
for it, since DKMS already covers Fedora.

## Arch / AUR

`packaging/arch/PKGBUILD` is a split package (`conduit`,
`conduit-guest-dkms`) that builds from the git tag. Each release attaches a
copy with `pkgver` filled in. To publish on the AUR:

```sh
git clone ssh://aur@aur.archlinux.org/conduit.git aur-conduit
cp PKGBUILD conduit.install aur-conduit/     # from the release
cd aur-conduit && makepkg --printsrcinfo > .SRCINFO
git add . && git commit -m "conduit $pkgver" && git push
```

The AUR needs a public source: while `olealgoritme/conduit` is private,
AUR builds cannot fetch it. Until then use the prebuilt `.pkg.tar.zst` from
the release (`pacman -U`). The PKGBUILD needs `rustup` (it honours
`rust-toolchain.toml`) and network access in `prepare()`/`build()` for crates
and the QEMU tarball.

## Nix

`flake.nix` exposes `packages.default` (a prefix mirroring `/opt/conduit`,
with `conduit` wrapped to `CONDUIT_PREFIX=$out`), `apps.default`, the
individual components, `packages.conduit-guest` (module for
`linuxPackages_latest`) and `nixosModules.guest`. It uses nixpkgs' Rust and
QEMU expression (switched to the 11.1.2 tarball when nixpkgs is older, plus
`host/qemu/patches/`). Commit `flake.lock` after the first `nix flake lock`.

## CI workflows

| Workflow | When | What |
|---|---|---|
| `ci.yml` | push, PR | fmt/clippy/test per Rust project (GPU tests skipped by name), guest module vs Ubuntu 24.04 and Fedora headers, viewer `make check`, actionlint, shellcheck, DKMS package build |
| `abi.yml` | Mondays, manual | new open-gpu-kernel-modules tags / gVisor nvproxy ABIs -> `.github/scripts/abi_update.py` runs the `host/backend/gen` generators -> tests -> PR on `abi/auto` (draft if tests fail). Set secret `ABI_BOT_TOKEN` so CI runs on its PRs. |
| `release.yml` | tag `v*`, manual | static musl Rust binaries once; deb/rpm/Arch/tarball in their own containers with QEMU cached per week; guest .deb/.rpm; checksums, PKGBUILD, GitHub Release (tags only) |

## Repository hygiene

`dist/` (all build output) and `host/qemu/{src,build,build.log}` (local QEMU
builds) belong in `.gitignore`.

## Not covered by CI

CI and `release.yml` build and package every format, but do not run
`nix build` or exercise the AppArmor and SELinux rules against a real libvirt
VM; check those by hand when they change.
