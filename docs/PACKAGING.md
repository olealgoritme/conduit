# Packaging

Every package is built by one script, `packaging/build.sh`. The release
workflow, the RPM spec, the PKGBUILD and the flake all produce the same
layout, so a layout change is made in one place.

## What gets installed

| Path | What |
|---|---|
| `/opt/conduit/bin/conduit` | the CLI |
| `/opt/conduit/bin/conduit-backend` | GPU backend (static musl in release builds) |
| `/opt/conduit/bin/conduit-userspace` | stages the host's NVIDIA user-space files for the guest (same crate as the backend) |
| `/opt/conduit/bin/conduit-viewer` | Wayland/X11 viewer |
| `/opt/conduit/bin/conduit-vmm` | built-in VM runner (static musl in release builds) |
| `/opt/conduit/bin/conduit-stream` | stream host for Moonlight and `conduit remote` |
| `/opt/conduit/bin/qemu-system-x86_64`, `share/qemu/` | bundled QEMU 11.1 |
| `/opt/conduit/share/conduit/supported-drivers.txt` | driver releases with backend ABI tables (`conduit doctor`) |
| `/opt/conduit/share/conduit/guest/conduit-guest.deb` | the guest driver package `conduit create` and `conduit stock-kernel` install into VMs (built at stage time with nfpm) |
| `/opt/conduit/share/conduit/guest/conduit-guest.pkg.tar.zst` | the same, as an Arch package, for `conduit attach` on Arch-based guests (built at stage time with nfpm) |
| `/opt/conduit/libexec/conduit-integrate` | AppArmor/SELinux/desktop hookup (`enable`/`disable`) |
| `/opt/conduit/lib/` | `libvirglrenderer.so.1` for `conduit-venus`; in the tarball also the viewer's, stream host's and QEMU's shared libraries |
| `/opt/conduit/share/doc/conduit/` | LICENSE and every component's LICENSE/NOTICE |
| `/usr/bin/conduit` (packages), `/usr/local/bin/conduit` (tarball) | symlink to the CLI |
| `/usr/share/applications/conduit.desktop` (`/usr/local/share/...` for the tarball) | desktop entry |
| `/usr/share/icons/hicolor/scalable/apps/conduit.svg`, `/usr/share/icons/hicolor/<N>x<N>/apps/conduit.png` for N = 16, 22, 24, 32, 48, 64, 128, 256, 512 (`/usr/local/share/...` for the tarball) | app icon, `Icon=conduit` in the desktop entry |
| `/etc/apparmor.d/abstractions/conduit` | AppArmor rules for libvirt's QEMU |

The icon set is `packaging/common/icons/conduit.svg` and its PNG renders
`conduit-<N>.png` (committed, so builds need no SVG renderer; regenerate with
`rsvg-convert -w N -h N conduit.svg -o conduit-N.png`). `install_icons` in
`packaging/common/icons.sh` is the only code that lays them out; `build.sh
stage`, the tarball staging and the flake all call it, and the RPM spec,
nfpm template and PKGBUILD list the result. `packaging/test/icons.sh` checks it.

`conduit-venus`, the Venus renderer for `--venus` (experimental,
docs/VENUS.md), is packaged in `/opt/conduit/bin` with its own virglrenderer
(Venus only, built by `host/venus/build-virglrenderer.sh`) in
`/opt/conduit/lib/libvirglrenderer.so.1`, found through a RUNPATH of
`$ORIGIN/../lib` (`packaging/build.sh venus`; `build-virglrenderer.sh`
applies `host/venus/patches/` to the pinned submodule first). The packaged
backend is built with its `venus` feature. The release workflow, the RPM
spec, the PKGBUILD, the flake and `make package` all build it, so `--venus`
works from every install. It needs the two submodules
`host/venus/third_party/{virglrenderer,venus-protocol}` (the release
workflow checks out only those two; the RPM spec fetches them as Source1/2
and the PKGBUILD and flake as git sources, at the pinned commits, which CI
checks against the submodules). virglrenderer `dlopen()`s the host's Vulkan
loader (`libvulkan.so.1`), so the packages depend on it explicitly
(`libvulkan1`, `libvulkan.so.1()(64bit)`, `vulkan-icd-loader`) and the
tarball never bundles it. A stage without `build.sh venus` still packages,
with a warning, and `--venus` does not work from that install.

The guest package installs `/usr/src/conduit-guest-<version>/` (module source
plus `dkms.conf`); DKMS builds `conduit_gpu.ko` into
`/lib/modules/<kver>/updates/dkms/` for every kernel 6.4 or newer. It also
ships the files in `guest/system/` (module autoload, the modprobe.d entry
that retires the old `virtio_gpu_nv` name and blacklists `spi_virtio`, the
user-namespace sysctl, and the
setup script every package format runs after install; see
`guest/linux/README.md`).

## Files

```
packaging/
├── build.sh                   the build: deps, rust, viewer, stream, venus, qemu, stage, bundle-libs, guest-src, package
├── nfpm/conduit.yaml          host package: one template -> .deb, .rpm, Arch .pkg.tar.zst
├── nfpm/conduit-guest.yaml    guest DKMS package: .deb, .rpm, Arch .pkg.tar.zst
├── deb/conduit/               postinst, prerm (used by nfpm for all three formats)
├── deb/conduit-guest/         postinst, prerm (dkms add/install/remove; all three guest formats)
├── dkms/dkms.conf             DKMS config, kernel >= 6.4 via BUILD_EXCLUSIVE_KERNEL
├── rpm/conduit.spec           source RPM build (COPR/OBS)
├── rpm/conduit-guest.spec     source RPM for the guest (DKMS, noarch)
├── arch/PKGBUILD              split package: conduit + conduit-guest-dkms (AUR)
├── arch/conduit.install, arch/conduit-guest.install
├── release.sh                 version bump + tag (make release / release-minor / release-major)
├── tarball/install.sh         -> /opt/conduit, /usr/local/bin/conduit
├── tarball/uninstall.sh       also installed as /opt/conduit/uninstall.sh
└── common/                    desktop entry, icons/ (svg + PNG renders) and icons.sh (`install_icons`), AppArmor abstraction, conduit-integrate
```

## Building locally

```sh
sudo packaging/build.sh deps          # apt, dnf or pacman
packaging/build.sh rust               # static musl; RUST_TARGET=host for a glibc build
packaging/build.sh viewer
packaging/build.sh stream
packaging/build.sh venus              # conduit-venus + virglrenderer (needs the venus submodules; meson, Vulkan and libdrm headers, python3 mako/yaml: `deps` installs them)
packaging/build.sh qemu               # slow; BUNDLE_QEMU=0 to skip
packaging/build.sh stage
packaging/build.sh package deb        # or rpm, archlinux (needs nfpm)

# tarball
LINK_DIR=/usr/local/bin packaging/build.sh stage
packaging/build.sh bundle-libs
packaging/build.sh package tarball

# guest
packaging/build.sh package guest-deb  # or guest-rpm, guest-arch

# Conduit BIOS (gcc, nasm, iasl/acpica-tools, uuid-dev, python3)
packaging/build.sh bios               # -> dist/bios
packaging/build.sh package bios-deb   # or bios-rpm, bios-arch, bios-tarball
```

The Conduit BIOS (`packaging/bios/build.sh`) downloads Ubuntu's `edk2` source
package pinned in `packaging/bios/version.sh` (checksums checked), applies its
patch series, swaps in `packaging/bios/Logo.bmp` (rendered by
`make-logo.py`) and `packaging/bios/patches`, and builds `OVMF_CODE_4M.fd` and
`OVMF_CODE_4M.secboot.fd` with the flags of Ubuntu's `debian/rules`, so the
images take the stock varstores. Its package version is the edk2 build plus
`CONDUIT_BIOS_REV`; bump that when the logo, patches or script change, and
the edk2 pin to follow Ubuntu's `ovmf`.

Output goes to `dist/out/`. `VERSION` overrides the version (default:
`git describe`).

Runtime dependencies of the .deb/.rpm/Arch packages are not hand-written:
`build.sh` reads the `NEEDED` libraries of the staged viewer and QEMU and maps
them to Debian/Arch package names (`dpkg -S`, `pacman -Qo`) or RPM soname
requirements (`libfoo.so.1()(64bit)`, which work on Fedora and openSUSE
alike), plus the Vulkan loader, which virglrenderer `dlopen()`s, when
`conduit-venus` is staged. That is why each format is built in its own
distribution's container.

## Contracts with the components

These are assumptions the packaging makes. Change them here and in
`build.sh` together.

**Binary names.** `build.sh` (top) holds the cargo/make output names
as variables (`flake.nix` has its own copies): `BACKEND_BIN_SRC=conduit-backend` (cargo package `device`,
features `BACKEND_FEATURES=vhost-user,venus`), `USERSPACE_BIN_SRC=conduit-userspace`, `VIEWER_BIN_SRC=conduit-viewer`,
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
VMs of the system libvirt get their sockets under `/run/conduit/<vm>/`; user
sessions use `$XDG_RUNTIME_DIR/conduit/<vm>/`. The AppArmor rules allow exactly
these. The generic desktop entry runs `conduit view` with no argument, which
opens the only VM, or the most recently used one (`conduit view`/`up` touch
`~/.local/share/conduit/vms/NAME/last-used`); the per-VM entries
(`~/.local/share/applications/conduit-NAME.desktop`) name their VM.

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
and openSUSE; Arch gets the same contents as `conduit-guest-*-any.pkg.tar.zst`
(`package guest-arch`, nfpm: the postinst runs as `post_install`/`post_upgrade`,
and pacman's own dkms hooks build the module as well). `dkms.conf` limits builds to Linux 6.4+ with a
`BUILD_EXCLUSIVE_KERNEL` regex (works on old DKMS versions, unlike
`BUILD_EXCLUSIVE_KERNEL_MIN`). The postinst builds for the running kernel and
reports, rather than fails, when headers are missing.

The source package carries every directory of `guest/linux` except `test/`
(the generated per-release tables: `gen/`, `rmctrl/`, `devinfo/`, ...), and
`build.sh guest-src` fails when a quoted `#include` of the sources does not
resolve inside the package, since the in-tree build cannot notice a missing
directory.

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
individual components (the backend with `vhost-user,venus`, like
`build.sh`; `venus` is `conduit-venus` with `virglrenderer`, built from the
pinned virglrenderer and venus-protocol revisions with `host/venus/patches/`,
the loader's path on virglrenderer's RUNPATH for its `dlopen()` of
`libvulkan.so.1`), `packages.conduit-guest` (module for
`linuxPackages_latest`) and `nixosModules.guest`. It uses nixpkgs' Rust and
QEMU expression (switched to the 11.1.2 tarball when nixpkgs is older, plus
`host/qemu/patches/`). Commit `flake.lock` after the first `nix flake lock`.

## CI workflows

| Workflow | When | What |
|---|---|---|
| `ci.yml` | push, PR, manual | fmt/clippy/test per Rust project (backend, VMM, `host/venus` without its `renderer` feature, CLI, stream host; GPU tests skipped by name), guest module vs Ubuntu 24.04 (GA and HWE), Debian 13 and Fedora headers plus its plain-C unit tests, viewer `make check`, guest agent unit tests, actionlint, shellcheck, venus submodule pins in `flake.nix` and the RPM spec, DKMS package build |
| `abi.yml` | Mondays, manual | new open-gpu-kernel-modules tags / gVisor nvproxy ABIs -> `.github/scripts/abi_update.py` runs the `host/backend/gen` generators -> tests -> PR on `abi/auto` (draft if tests fail). Set secret `ABI_BOT_TOKEN` so CI runs on its PRs. |
| `release.yml` | tag `v*`, manual | static musl Rust binaries once; deb/rpm/Arch/tarball in their own containers (viewer, stream host, `conduit-venus` from the venus submodules) with QEMU cached per week; guest .deb/.rpm/.pkg.tar.zst; checksums, PKGBUILD, GitHub Release (tags only) |
| `bios.yml` | changes under `packaging/bios`, called by `release.yml` | the Conduit BIOS on Ubuntu 24.04, cached per `packaging/bios/**` content; conduit-bios .deb/.rpm/.pkg.tar.zst/tarball |
| `windows.yml` | `v*` tags (the `release` job waits up to 3 hours for the release.yml release, then attaches the driver zip and its checksum as `SHA256SUMS-windows`; `SHA256SUMS` covers release.yml's files) and by hand (about an hour on Windows runners) | the Windows guest stack from `guest/windows` (WDDM driver, D3D11/12 UMDs, Mesa Venus ICD, loaders, installer); the package per configuration as an artifact, and the Release driver folder (`conduit-windows-gpu-driver-<version>.zip`: signed INF/SYS/CAT, UMDs, NVK/Zink files, test certificate, `packaging/windows/install.ps1`) as the release asset. The driver alone also builds in a local Windows VM (`guest/windows/ci/vm/README.md`) |

## Repository hygiene

`dist/` (all build output) and `host/qemu/{src,build,build.log}` (local QEMU
builds) belong in `.gitignore`.

## Not covered by CI

CI and `release.yml` build and package every format, but do not run
`nix build` or exercise the AppArmor and SELinux rules against a real libvirt
VM; check those by hand when they change.
