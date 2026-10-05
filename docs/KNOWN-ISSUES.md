# Known issues and limitations

Current limitations, with the intended fix for each. Planned work is in
[ROADMAP.md](ROADMAP.md).

## Backend

| Issue | Effect | Intended fix |
|---|---|---|
| A backend crash is not supervised | The guest keeps stale handles and its GPU calls time out | Stop the VM (or mark it degraded) when the backend dies; in the guest, fail all fds with `-ENODEV` after repeated timeouts |
| The guest gives up on a call after 10 s while the host call continues | An allocation can succeed on the host that the guest never sees (freed only on fd close) | Make the timeout a module parameter; after a timed-out `RM_ALLOC`, send an `RM_FREE` for the guest-chosen handle |
| A driver release without its own ABI tables is refused unless the backend runs with `--allow-nearest-abi` | With that flag it uses the nearest older release's tables, and classes whose sizes changed are refused at first use | Tables for the new release (the weekly ABI workflow); a startup self-check of a few read-only controls |
| `Caps::for_class` only knows the engine classes it lists | A new engine class needs no capability until it is added | Generate the 3D/video class lists per release; refuse unknown engine classes |
| Every guest may open every host GPU (`/dev/nvidia0..15`) | No per-VM GPU selection on multi-GPU hosts | A `--gpu <pci-addr>` option that filters the device list and the Landlock rules |
| `--vram-limit-mib` misses memory RM allocates internally | The limit is approximate (tens of MiB per guest) | Per-channel estimates |

## Display

| Issue | Effect | Intended fix |
|---|---|---|
| The guest ignores buffer release from the host | The guest may render into a buffer the host compositor still reads | Forward `EV_RELEASE` as a guest event; complete a flip only when the previous buffer is released |
| Guest vblank is a free-running timer | Up to a frame of extra latency and phase drift against the host's refresh | Drive the guest vblank from the host's presentation feedback (`EV_FRAME`), keeping the timer as a fallback |
| GPU fences cross the boundary only as semaphore-surface fences ([SYNC.md](SYNC.md)); the legacy PRIME fence ioctls are not served, and a buffer rendered without any fence is still flipped unsynchronised | Old userspace without `supports_semsurf`, or a client using neither explicit nor attached fences, can show a frame before its rendering finished | Serve PRIME_FENCE_* if anything still needs it |
| No damage rectangles | The host recomposites and the encoder re-encodes the whole frame | `drm_plane_enable_fb_damage_clips()`, pass the bounding rectangle with the flip |
| No `GAMMA_LUT` / CTM | Night light and colour profiles in the guest have no effect | Enable colour management on the CRTC and forward the LUT |
| No physical size or EDID (Linux guests; Windows guests get one, [VENUS.md](VENUS.md)) | GNOME picks scale 1 on HiDPI monitors | Report the host output's size, or synthesize an EDID |
| One head | No multi-monitor | Several CRTCs/connectors, one viewer window per head |
| No VRR or HDR properties | No adaptive sync or HDR in the guest | `vrr_capable` / `HDR_OUTPUT_METADATA` forwarded to the viewer |
| No dumb buffers / fbcon | No text console or Plymouth on Conduit's head; firmware and early boot show only through the boot console, which only attached libvirt VMs have ([SCANOUT.md](SCANOUT.md#boot-console)) | Host-backed pitch-linear dumb buffers |

## Guest

- One Conduit GPU device per guest (global classes, fixed device majors).
- `CONFIG_INPUT_UINPUT` is off in `guest/linux/guest-kernel.config` (stock
  distro kernels have it).
- The device's virtio ID, 45, has since been assigned to SPI controllers by
  the virtio spec. Kernels with `spi_virtio` (Arch 7.2) race `conduit_gpu`
  for the device; the `conduit-guest` package blacklists `spi_virtio`
  (`guest/system/modprobe.conf`); a module built by hand needs the same
  blacklist.

## Windows guests (Venus)

Experimental, behind `--venus` ([WINDOWS.md](WINDOWS.md), [VENUS.md](VENUS.md)).

- From a checkout, `make` builds the backend with the `venus` feature but
  not `conduit-venus` (it needs the venus submodules and a local
  virglrenderer); build that by hand ([VENUS.md](VENUS.md)). The packages
  have both.
- `conduit attach` knows only Linux guests: run it with `--guest-later` for a
  Windows VM and install the driver by hand ([WINDOWS.md](WINDOWS.md)). It
  adds no Hyper-V enlightenments, which steady a Windows guest's frame pacing;
  add them to the domain by hand.
- The guest driver is test-signed: Secure Boot off, test-signing on.
- The guest's keyboard and pointer go through the boot console's emulated
  PS/2 keyboard and USB tablet (no gamepads); no clipboard sharing.
- `virglrenderer` needs Conduit's patch (`host/venus/patches`) for usable
  frame rates on NVIDIA; an unpatched build waits about 10 ms per ring fence.
- Region 3 needs QEMU (`conduit-vmm` has fixed BARs).
- A Venus blob carries no layout the host can read back; the scanout's
  modifier is inferred from the blob's size (linear, or NVIDIA block-linear
  when the blob holds the padded rows). An optimal-tiling scanout whose
  height is a whole number of blocks (768, 1024) is taken as linear and shows
  garbage.
- Only `HOST3D` blobs; guest-memory blobs are refused.
- A fence on a ring with no queue bound makes virglrenderer destroy the
  context, which the backend cannot see.

## QEMU

Stock QEMU cannot host the device; `host/qemu/patches` are required (see
`host/qemu/README.md`). Upstreaming 0001 (config size) and 0003 (fixed-address
shared-memory mappings) would make distro QEMU and libvirt usable directly.
