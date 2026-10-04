# Roadmap

## Linux guests (done)
- [x] Linux guest renders on the host GPU (Vulkan, GL, CUDA, NVENC)
- [x] Zero-copy display in a host Wayland window, up to 240 Hz
- [x] Keyboard, mouse, clipboard and sound
- [x] The VM follows the window size by default; optional scale-to-fit (Ctrl+Alt+R)
- [x] Fullscreen at the monitor's exact mode, direct scanout
- [x] Hardware cursor
- [x] Performance overlay (fps, frame times, latency)
- [x] `conduit` CLI: create / attach / view / up / down / status / logs / doctor
- [x] Packages: .deb, .rpm, Arch, tarball, Nix flake; `conduit-guest` (DKMS)
- [x] QEMU 11.1 (vhost-user shared memory), libvirt / virt-manager, `conduit attach`
- [x] Bundled QEMU 11.1 in `/opt/conduit` for older distros
- [x] New NVIDIA driver releases: CI regenerates the ABI tables weekly
- [x] CUDA managed memory and large pinned host memory
- [x] Streaming to Moonlight, with gamepads
- [x] Conduit's viewer over the network (`conduit remote`), including lossless
- [x] Viewing and streaming the same VM at once
- [x] GPU request tracing (`conduit trace`)

## Next
- [ ] View and stream together: the VM keeps the highest refresh rate any
      client needs (the stream drops frames to its own fps), and the stream
      scales the local window's picture on the GPU instead of changing the
      VM's resolution
- [ ] Lower per-call latency: an ioeventfd for the GPU device's notify register
      in `conduit-vmm`, backend thread placement, a short spin in the event pump
- [ ] Display pacing and fences: forward buffer release, drive the guest vblank
      from the host's presentation feedback, GPU fences across the boundary
      ([KNOWN-ISSUES.md](KNOWN-ISSUES.md))
- [ ] Audio in the stream (Opus from the VM's sound card)
- [ ] GPU hot-plug (`device_del` / `device_add`), so a VM can be snapshotted,
      saved and migrated with the GPU unplugged
- [ ] CUDA processes that survive a snapshot (`cuda-checkpoint` in the guest
      around unplug and re-plug)

## Later
- [ ] Full GPU state in snapshots (record and recreate the VM's RM objects and
      VRAM contents)
- [ ] NVIDIA native context in virglrenderer: stock QEMU's virtio-gpu, crosvm, libkrun
- [ ] Windows guests, two routes:
  - CUDA / NVML / NVENC on a small non-WDDM driver, using NVIDIA's own user-mode libraries
  - graphics through the open stack: NVK (Mesa Vulkan) on RM + DXVK / vkd3d-proton
- [ ] Graphics via Venus (Vulkan forwarding) over Conduit's device ([VENUS.md](VENUS.md))
- [ ] Multiple VMs sharing one GPU with fair scheduling
- [ ] Per-VM GPU selection on multi-GPU hosts
- [ ] Multiple monitors per VM, VRR and HDR
