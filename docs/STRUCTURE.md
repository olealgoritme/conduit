# Repository layout

```
conduit/
├── host/
│   ├── backend/   Rust. Talks to the host NVIDIA driver for each VM, exports
│   │              frames as dma-bufs, sandboxed (seccomp + Landlock).
│   │              (from virtio-nvgpu: device/, protocol/, gen/)
│   ├── viewer/    C. The Wayland window: zero-copy frames, input, overlay.
│   │              (from nvkvm-pv's display broker)
│   ├── stream/    Rust + C. conduit-stream: the VM over the network — a
│   │              GameStream host for Moonlight and the conduit link for
│   │              Conduit viewers; NVENC/NVDEC (docs/STREAMING.md).
│   ├── vmm/       Rust. Small built-in VM runner `conduit-vmm` (from
│   │              nesbox), the fallback when the bundled QEMU is missing.
│   └── qemu/      Build script + patches for the bundled QEMU 11.1, the
│                  default VM runner (`conduit up/view --vmm qemu|builtin`).
├── guest/
│   ├── linux/     C. The guest kernel module, conduit_gpu (virtio GPU, KMS display,
│   │              input, clipboard device). Packaged with DKMS.
│   ├── system/    Files the conduit-guest package installs in the VM: module
│   │              autoload, modprobe.d, sysctl, and its post-install setup.
│   ├── agent/     Python. conduit-clipboard-agent: the desktop session's
│   │              clipboard <-> /dev/conduit-clipboard.
│   └── windows/   Reserved for the Windows guest driver (see ROADMAP).
├── cli/           The `conduit` command (create / view / up / down / stream /
│                  remote; libvirt: attach / detach / libvirt enable, docs/LIBVIRT.md).
├── packaging/
│   ├── deb/       Debian packages: conduit, conduit-guest
│   └── dkms/      dkms.conf for the guest module
├── docs/          ARCHITECTURE, SECURITY, ROADMAP; design/ holds design notes
└── .github/workflows/
    ├── ci.yml         every push: Rust tests, guest module build against
    │                  Ubuntu/Fedora headers, viewer selftests
    ├── abi.yml        weekly: new NVIDIA driver release → regenerate ABI
    │                  tables → open a PR
    └── release.yml    tag → build every package below and attach to the release
```

## Release artifacts (built by release.yml on every tag)

| Artifact | For | Built how |
|---|---|---|
| `conduit_X_amd64.deb` | Ubuntu, Debian, Mint, Pop!_OS | in an Ubuntu 24.04 container |
| `conduit-X.x86_64.rpm` | Fedora, RHEL, openSUSE | in a Fedora container |
| `conduit-X.pkg.tar.zst` | Arch, Manjaro | in an Arch container (+ AUR PKGBUILD) |
| `conduit-X-x86_64-linux.tar.gz` | everything else | backend and VMM static (musl), viewer with its libraries, `install.sh` |
| `conduit-guest_X_all.deb` / `.rpm` | inside the VM | DKMS source package for the guest module |
| `flake.nix` | NixOS | in the repo |

Every package installs to its own prefix (`/opt/conduit` for the bundled QEMU
and VMM), adds the `conduit` command, a desktop entry, and an AppArmor/SELinux
rule for libvirt. It never replaces system QEMU or libvirt.

Licenses stay per component: guest driver GPL-2.0, backend Apache-2.0,
protocol BSD-3-Clause, viewer Apache-2.0 (with its NOTICE), VMM Apache-2.0,
stream host Apache-2.0 (with its NOTICE: vendored ENet, nanors, nv-codec-headers, all MIT).
