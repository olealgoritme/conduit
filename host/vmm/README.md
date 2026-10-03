# conduit-vmm

Conduit's small built-in VM runner: a KVM microVM with the Conduit GPU device
built in, plus virtio disk, network (tap), shared folders (virtiofsd), vsock
and console, all over PCI. Licensed Apache-2.0 (`LICENSE`).

The bundled QEMU 11.1 (`host/qemu`) is the default runner. `conduit-vmm` is
the fallback when that QEMU is missing, or when asked for with
`conduit up|view NAME --vmm builtin`. Compared with QEMU it has no sound and
boots only an uncompressed ELF `vmlinux` (no initrd), so VMs that boot their
own distro kernel need QEMU.

`conduit` writes the JSON config (`cli/src/vm.rs`, `vmm_config`), creates the
tap and starts `conduit-backend` and virtiofsd; you rarely run it by hand:

```sh
conduit-vmm config.json
```

It is derived from [nesbox](https://github.com/nestrilabs/nesbox) (Nestri
Labs), itself built on the rust-vmm crates with ideas from Firecracker, Cloud
Hypervisor and libkrun.

## Build and test

```sh
cd host/vmm
cargo build --release --no-default-features    # as shipped: NVIDIA forwarding only
cargo test --workspace --no-default-features
```

`make vmm` / `make test` at the repo root do the same. The default `virgl`
feature adds a virtio-gpu device for Intel/AMD hosts through virglrenderer
(with the patches in `patches/`); Conduit does not use it.

## Layout

| path | what it does |
|---|---|
| `vmm/src/main.rs` | the `conduit-vmm` binary |
| `vmm/src/config.rs` | the JSON config format |
| `vmm/src/vm.rs`, `boot.rs`, `cpuid.rs`, `acpi.rs`, `power.rs` | KVM setup, boot, CPU topology, ACPI tables, power-off and reset |
| `vmm/src/seccomp.rs`, `isolation.rs` | seccomp filter; reports cgroup/namespace limits in effect |
| `virtio-devices/src/nvgpu.rs` | the Conduit GPU device (vhost-user frontend to `conduit-backend`) |
| `virtio-devices/src/` | block, net, fs, vsock, console |
| `pci/` | PCI configuration space and MSI-X |
