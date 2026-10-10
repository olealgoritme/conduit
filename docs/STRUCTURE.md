# Repository layout

```
conduit/
├── host/
│   ├── backend/   Rust. conduit-backend talks to the host NVIDIA driver for each
│   │              VM, exports frames as dma-bufs, sandboxed (seccomp + Landlock).
│   │              Crates: device/ (conduit-backend, conduit-userspace,
│   │              conduit-sandbox-selftest), protocol/, gen/ (driver ABI tables),
│   │              trace/ (request tracing, docs/TRACING.md).
│   ├── viewer/    C. conduit-viewer, the Wayland/X11 window: zero-copy frames,
│   │              input, overlay.
│   ├── stream/    Rust + C. conduit-stream: the VM over the network — a
│   │              GameStream host for Moonlight and the conduit link for
│   │              Conduit viewers; NVENC/NVDEC (docs/STREAMING.md).
│   ├── vmm/       Rust. Small built-in VM runner `conduit-vmm`, the fallback
│   │              when the bundled QEMU is missing.
│   ├── qemu/      Build script + patches for the bundled QEMU 11.1, the
│   │              default VM runner (`conduit up/view --vmm qemu|builtin`).
│   └── venus/     Rust. conduit-venus: the sandboxed Venus renderer process for
│                  Windows guests (virglrenderer on the host's NVIDIA Vulkan),
│                  its IPC and the Renderer trait the backend uses; own
│                  workspace (docs/VENUS.md).
│                  third_party/: virglrenderer, venus-protocol (submodules);
│                  patches/: Conduit's virglrenderer patches, applied by
│                  build-virglrenderer.sh.
├── guest/
│   ├── linux/     C. The guest kernel module, conduit_gpu (virtio GPU, KMS display,
│   │              input, clipboard device, explicit sync, PCI mirror). Packaged
│   │              with DKMS. test/: plain-C unit tests (`make check`).
│   ├── system/    Files the conduit-guest package installs in the VM: module
│   │              autoload, modprobe.d, sysctl, and its post-install setup.
│   ├── power/     logind drop-in: the power button shuts the VM down.
│   ├── agent/     Python. conduit-clipboard-agent: the desktop session's
│   │              clipboard <-> /dev/conduit-clipboard.
│   ├── tests/     Small in-guest test programs (CUDA memory).
│   ├── rmclient/  C. librmclient (MIT): RM client library for user-mode drivers (NVK).
│   ├── nvk-rm/    Mesa patch series: NVK on RM through librmclient
│   │              (experimental, opt-in with NVK_RM=1), build script, tests.
│   └── windows/   Helios-derived Windows guest components (guest/windows/HELIOS.md):
│                  kmd_render/ + kmd_logic/ (WDDM KMD), protocol/ (escape ABI),
│                  umd/, umd12/, umd_common/ (D3D11/D3D12 UMDs), icd/ (Mesa
│                  Venus ICD build), installer/, packaging/, metadata/, ci/
│                  (windows/: build scripts for windows.yml; vm/: local build
│                  VM), tools/ (probes, helpers),
│                  third_party/ (Mesa, DXVK, vkd3d-proton submodules).
├── cli/           Rust. The `conduit` command (create / view / up / down / stream /
│                  remote / trace; libvirt: attach / detach / libvirt enable,
│                  docs/LIBVIRT.md). cli/assets holds the VM disk build script
│                  and the files it installs in the guest.
├── packaging/     build.sh (stages and packages everything), release.sh (version
│                  bump + tag), nfpm/ (deb, rpm, Arch), deb/ and arch/ (install
│                  scripts, PKGBUILD), rpm/ (spec files), tarball/, dkms/,
│                  common/ (desktop entry, AppArmor, conduit-integrate),
│                  bios/ (Conduit BIOS: edk2 OVMF build, logo, patches)
├── flake.nix      Nix package and app
├── docs/          User and contributor documentation; research/: background
│                  surveys (Windows guest prior art, a thinner Windows path,
│                  NVK on RM)
└── .github/workflows/
    ├── ci.yml         every push: Rust tests, guest module build against
    │                  Ubuntu/Debian/Fedora headers, viewer selftests, agent tests
    ├── abi.yml        weekly: new NVIDIA driver release → regenerate ABI
    │                  tables → open a PR
    ├── release.yml    tag → build every package below and attach to the release
    └── windows.yml    Windows guest stack; on v* tags attaches the driver zip to the release
```

## Release artifacts (built by release.yml on every tag)

| Artifact | For | Built how |
|---|---|---|
| `conduit_X-1_amd64.deb` | Ubuntu, Debian, Mint, Pop!_OS | in an Ubuntu 24.04 container |
| `conduit-X-1.x86_64.rpm` | Fedora, RHEL, openSUSE | in a Fedora container |
| `conduit-X-1-x86_64.pkg.tar.zst` | Arch, Manjaro | in an Arch container (+ `PKGBUILD` for the AUR) |
| `conduit-X-x86_64-linux.tar.gz` | everything else | backend, VMM and CLI static (musl); viewer, stream host and QEMU with their libraries (built on Debian 12); `install.sh` |
| `conduit-guest_X-1_all.deb` / `conduit-guest-X-1.noarch.rpm` / `conduit-guest-X-1-any.pkg.tar.zst` | inside the VM | DKMS source package for the guest module (the Arch one is what `conduit attach` installs on Arch-based guests) |
| `conduit-bios_B-1_all.deb` / `conduit-bios-B-x86_64.tar.gz` | Ubuntu host, optional | Conduit BIOS: edk2 OVMF with the Conduit boot logo, for `conduit attach` (B = edk2 build + revision, `packaging/bios/version.sh`; bios.yml, cached) |
| `flake.nix` | NixOS | in the repo |
| `SHA256SUMS` | | checksums of the files above |
| `conduit-windows-gpu-driver-X.zip`, `SHA256SUMS-windows` | inside a Windows VM | the Windows guest driver (windows.yml) and its checksum |

No release artifact has `conduit-venus` yet: release.yml does not run
`packaging/build.sh venus` (docs/PACKAGING.md).

Every package installs to its own prefix (`/opt/conduit` for the bundled QEMU
and VMM), adds the `conduit` command, a desktop entry, and an AppArmor/SELinux
rule for libvirt. It never replaces system QEMU or libvirt.

Licenses stay per component: guest driver GPL-2.0, backend Apache-2.0,
protocol BSD-3-Clause or GPL-2.0-or-later, viewer Apache-2.0 (with its NOTICE), VMM Apache-2.0,
stream host Apache-2.0 (with its NOTICE: vendored ENet, nanors, nv-codec-headers, all MIT),
Venus renderer Apache-2.0 (virglrenderer MIT), librmclient MIT.
