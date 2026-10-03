# Repository layout

```
conduit/
├── host/
│   ├── backend/   Rust. Talks to the host NVIDIA driver for each VM, exports
│   │              frames as dma-bufs, sandboxed (seccomp + Landlock).
│   │              (from virtio-nvgpu: device/, protocol/, gen/)
│   ├── viewer/    C. The Wayland window: zero-copy frames, input, overlay.
│   │              (from nvkvm-pv's display broker)
│   ├── vmm/       Rust. Small built-in VM runner (from nesbox), used when
│   │              QEMU is too old.
│   └── qemu/      Build script + patches for the bundled QEMU 11.1.
├── guest/
│   ├── linux/     C. The guest kernel module (virtio GPU, KMS display,
│   │              input). Packaged with DKMS.
│   └── windows/   Reserved for the Windows guest driver (see ROADMAP).
├── cli/           The `conduit` command (create / attach / view / up / down).
├── packaging/
│   ├── deb/       Debian packages: conduit, conduit-guest
│   └── dkms/      dkms.conf for the guest module
├── docs/          ARCHITECTURE, SECURITY, ROADMAP, developer notes
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
protocol BSD-3-Clause, viewer Apache-2.0 (with its NOTICE), VMM Apache-2.0.
