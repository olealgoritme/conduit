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
    └── release.yml    tag → build .debs and attach them to the release
```

Licenses stay per component: guest driver GPL-2.0, backend Apache-2.0,
protocol BSD-3-Clause, viewer Apache-2.0 (with its NOTICE), VMM Apache-2.0.
