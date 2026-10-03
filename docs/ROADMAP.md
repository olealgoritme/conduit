# Roadmap

## Now: make it solid on Linux
- [x] Linux guest renders on the host GPU (Vulkan, GL, CUDA, NVENC)
- [x] Zero-copy display in a host Wayland window, 240 Hz
- [x] Keyboard and mouse
- [ ] Scale-to-fit by default, optional "VM follows window size"
- [ ] Fullscreen = monitor's exact mode, direct scanout
- [ ] Hardware cursor (no cursor lag)
- [ ] Performance overlay (fps, frame times, latency)
- [ ] `conduit` CLI: create / attach / view / up / down / status / logs
- [ ] Packages: `conduit` (host) and `conduit-guest` (DKMS) .debs

## Next: run anywhere
- [ ] QEMU 11.1+ support (vhost-user shared memory), libvirt / virt-manager via `conduit attach`
- [ ] Bundled QEMU 11.1 in `/opt/conduit` for older distros (Ubuntu 24.04)
- [ ] Automatic support for new NVIDIA driver releases (CI regenerates ABI tables)
- [ ] Lower per-call latency (doorbell ioeventfd, wake path)
- [x] CUDA managed memory beyond 64 MiB, pinned host memory beyond 256 MiB (`guest/tests/cuda-mem.c`)

## Later: more guests and VMMs
- [ ] NVIDIA native context in virglrenderer: stock QEMU's virtio-gpu (≈9.2+), crosvm, libkrun
- [ ] Windows guests, two candidate routes:
  - open stack: NVK (Mesa Vulkan) on RM + DXVK, on a virtio-gpu Windows driver (no CUDA)
  - NVIDIA's own Windows user-mode driver on a Conduit WDDM kernel driver (clean-room interop; needs a study phase first)
- [ ] Multiple VMs sharing one GPU with fair scheduling
