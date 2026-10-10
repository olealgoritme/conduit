# Known issues and limitations

Current limitations, with the intended fix for each. Planned work is in
[ROADMAP.md](ROADMAP.md).

## Backend

| Issue | Effect | Intended fix |
|---|---|---|
| A backend crash is not supervised | The guest keeps stale handles and its GPU calls time out | Stop the VM (or mark it degraded) when the backend dies; in the guest, fail all fds with `-ENODEV` after repeated timeouts |
| The guest gives up on a call after 10 s while the host call continues | An allocation can succeed on the host that the guest never sees (freed only on fd close) | Make the timeout a module parameter; after a timed-out `RM_ALLOC`, send an `RM_FREE` for the guest-chosen handle |
| A driver release without its own ABI tables is refused unless the backend runs with `--allow-nearest-abi` | With that flag the RM pointer table, RM allowlist, UVM table, video-memory table, OS-descriptor table and escape sizes use the nearest older release's, and classes whose sizes changed are refused at first use. The GET_DEV_INFO layout and the NVKMS command table never fall back (their layouts are not monotonic across releases, so an older one is a guess): with no table of its own a release is served only NVKMS ALLOC/FREE_DEVICE, the host's DRM nodes are not asked what they are, and no render node is offered to the guest (one log line names the missing table) | Tables for the new release (the weekly ABI workflow); a startup self-check of a few read-only controls |
| The escape-size list reaches the guest as the last section of GET_SYS_FILES, and the earlier sections stop the read when one is absent | A backend with no RM allowlist for its release (section 3 absent) ends the read before the size list, so the guest checks no size and the backend alone does; a backend with the list and a guest from before it simply never reads it | Give each section a length so an absent one is skipped rather than ending the read |
| The guest holds at most 128 escape-size records | A release with more is read whole but applied not at all (one `dev_warn`); the backend still refuses a wrong size | Size the array from the section's count |
| `Caps::for_class` only knows the engine classes it lists | A new engine class needs no capability until it is added | Generate the 3D/video class lists per release; refuse unknown engine classes |
| Every guest may open every host GPU (`/dev/nvidia0..15`) | No per-VM GPU selection on multi-GPU hosts | A `--gpu <pci-addr>` option that filters the device list and the Landlock rules |
| The closed NVIDIA kernel modules and branches before 580 are untested | Releases with tables are accepted with a `conduit doctor` warning, and the backend starts in safe mode (`gpu.safe_mode auto`: a 2 GiB video-memory limit, or less if you set a smaller `gpu.vram_limit_mib`, and 1 s clamps on blocking calls) until you set `gpu.safe_mode false`. The closed modules stub dma-buf export and import (CUDA dma-buf does not work in a guest), do not charge UVM system memory to the memory cgroup, and have not been run with Conduit | Run the checks in [GPU-SUPPORT.md](GPU-SUPPORT.md) per release and generation |
| A new driver release needs hand edits after its tables are generated | The `PROFILES` lists of `abi::devinfo` and `abi::nvkms` and the includes in `guest/linux/nvgpu_devinfo.h` (the generator writes the table files and their `pub mod`, not these). A missed `PROFILES` entry keeps the release refused, and `accepted_releases_match_the_cli_list` fails on it; nothing checks the guest header's includes | A test that every `guest/linux/devinfo/*.h` is included by `nvgpu_devinfo.h`; generate both `PROFILES` lists |
| `UNMAP_MEMORY` of an address the backend has no mapping for is logged and ignored | Seen at process exit on the 4070 SUPER (565.77): a bookkeeping gap between the guest's mmap and the backend's records | Find which mappings are not recorded; account for them or refuse |
| `SYS_PARAMS` is answered with zeroed parameters (success) when the host returns EBUSY | The guest's copy of the host's system parameters is empty for the second and later clients | Return the first answer, cached |
| The default video-memory limit (safe mode, or `gpu.vram_limit_mib auto`; safe mode itself is capped at 2 GiB regardless) comes from a PCI-id table | Cards not in `cli/src/protect.rs` get a fixed 4 GiB limit | Have the backend ask RM for the card's memory once it has the GPU open |
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

## Windows guests

Experimental, behind `--venus` ([WINDOWS.md](WINDOWS.md), [VENUS.md](VENUS.md)).
Rendering is on NVK-on-RM; Venus is the fallback for processes the policy
keeps off NVK.

- One display mode at a time, set by `conduit up --display`,
  `conduit view NAME WxH@HZ` or the viewer's window size. The driver does not
  scale, so a game rendering below that mode does not fill the screen.
- Games need the viewer's mouse grab (`Ctrl+Alt+G`) for mouse look; without
  it the guest gets absolute tablet positions.
- Independent flip is off by default (`IndepFlip=0`, `DirectFlipSupport=0`):
  full-screen games are composed by DWM. With `IndepFlip=1` the hardware
  cursor comes on too (`HwCursor` follows `IndepFlip` unless set).

- From a checkout, `make` builds the backend with the `venus` feature but
  not `conduit-venus` (it needs the venus submodules and a local
  virglrenderer); build that by hand ([VENUS.md](VENUS.md)). The packages
  have both.
- `conduit attach` recognizes a Windows VM and adds the Hyper-V
  enlightenments, but does not install its guest driver: install the Helios
  package inside the VM by hand ([WINDOWS.md](WINDOWS.md)).
- The guest driver is test-signed: Secure Boot off, test-signing on.
- Counter-Strike 2: in some launches a strip of stray geometry (a long brown
  "beam") comes out of the player model in the main menu and stays for that
  process; restarting the game clears it. Not reproducible on demand yet;
  the null-vertex-buffer path (NVK patch 0041) is ruled out by tests.
- GDI read-back of an app's surface (`GetDC` + `BitBlt` from a D3D11
  GDI-compatible texture) returns part of the page dark in about 2 of 100
  attempts.
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
- Guest-memory blobs only with `conduit config set venus.guest_blobs true`
  (opt-in, [VENUS.md](VENUS.md) "Guest-memory blobs"); otherwise only
  `HOST3D` blobs.
- A fence on a ring with no queue bound makes virglrenderer destroy the
  context, which the backend cannot see.

## QEMU

Stock QEMU cannot host the device; `host/qemu/patches` are required (see
`host/qemu/README.md`). Upstreaming 0001 (config size) and 0003 (fixed-address
shared-memory mappings) would make distro QEMU and libvirt usable directly.
