# Roadmap

## Linux guests (done)
- [x] Linux guest renders on the host GPU (Vulkan, GL, CUDA, NVENC)
- [x] Zero-copy display in a host Wayland window, up to 240 Hz
- [x] Keyboard, mouse, clipboard and sound
- [x] Scale-to-fit by default, optional "VM follows window size"
- [x] Fullscreen at the monitor's exact mode, direct scanout
- [x] Hardware cursor
- [x] Performance overlay (fps, frame times, latency)
- [x] `conduit` CLI: create / attach / view / up / down / status / logs
- [x] Packages: .deb, .rpm, Arch, tarball, Nix flake; `conduit-guest` (DKMS)
- [x] QEMU 11.1 (vhost-user shared memory), libvirt / virt-manager, `conduit attach`
- [x] Bundled QEMU 11.1 in `/opt/conduit` for older distros
- [x] New NVIDIA driver releases: CI regenerates the ABI tables weekly
- [x] CUDA managed memory and large pinned host memory
- [x] Streaming to Moonlight

## Next
- [ ] Lower per-call latency (doorbell ioeventfd, wake path; see [design/LATENCY.md](design/LATENCY.md))
- [ ] Display pacing and fences (see [design/KNOWN-ISSUES.md](design/KNOWN-ISSUES.md))
- [ ] Snapshots / save / migration: GPU hot-plug first, then CUDA checkpoint, then full GPU state ([design/SNAPSHOTS.md](design/SNAPSHOTS.md))

## Later
- [ ] NVIDIA native context in virglrenderer: stock QEMU's virtio-gpu, crosvm, libkrun ([design/IDEAS.md](design/IDEAS.md))
- [ ] Windows guests ([design/WINDOWS.md](design/WINDOWS.md)), two routes:
  - CUDA / NVML / NVENC on a small non-WDDM driver, using NVIDIA's own user-mode libraries
  - graphics through the open stack: NVK (Mesa Vulkan) on RM + DXVK / vkd3d-proton
- [ ] Multiple VMs sharing one GPU with fair scheduling
