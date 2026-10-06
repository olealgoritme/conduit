# The KMD's own RM client (slice 1)

Status: slice 1 written on `kmd/rm-client` against `044b242` (KMD 22.22.309); slice 2
(sections 12 and 13: the decision on CPU access, the source priority stack, the level 3 ring and
presenter) on `kmd/rm-client-s2` against `bd5bef6`. **Never built, never run**: the KMD cannot
be compiled where this was written. The pure logic is host-tested
(`cargo test` in `guest/windows/kmd_logic`, 38 tests in `rm_client`); the I/O file was
type-checked against a shim that copies the signatures of the code it calls, and read by
hand. Everything marked **UNVERIFIED** needs the hardware run in section 9.

Part of the plan to remove Venus from the Windows guest (`dxvk-on-nvk.md`, "S6", on branch
`research/dxvk-on-nvk`): the KMD's own allocations (`KmdLinearPrimary`,
`KmdOptimalGdiTexture`, `KmdStandardBuffer`, the adapter's LINEAR scanout image) are Venus
blobs today. The design says the KMD "needs its own RM client (the KMD can issue `FORWARD`
itself)" to allocate RM memory for them and flip it with its own `ScanoutFlip`. This is the
first slice of that.

## 1. What slice 1 is, and is not

Behind the registry knob `KmdRmClient` (REG_DWORD, **default 0 = off**), the KMD

1. opens an RM client over the existing host forwarding path, as its own owner;
2. once the display half has bound a VidPn primary, allocates a pitch-linear `XRGB8888`
   video-memory surface of the primary's size from RM, exports it to a DRM GEM handle (the
   route `crm_scanout_smoke` and nvk-rm use), and keeps it for the life of the mode;
3. at level 2, also CPU-maps that surface, paints a test picture and shows it **once**
   through the KMD's own `ScanoutFlip`, with the desktop's host flushes withheld for four
   seconds (the foreign-scanout machinery), after which the desktop comes back by itself.

It is **not** the VidPn primary. The desktop is not drawn into the RM surface: GDI and the
Venus copy still write the Venus primary, and every consumer of an allocation is untouched.
Section 5 says why that was not decidable blind and what the candidates are. The Venus path
is the fallback for everything, and with the knob at 0 nothing here runs and nothing is
written (not a registry value, not an RM message): the only difference from `044b242` is
one atomic load per HPD worker pass (no lock) and one registry read of the knob per transport start.

| `KmdRmClient` | does |
|---|---|
| 0 (default) | nothing |
| 1 | client + surface + GEM import. Invisible: nothing is flipped |
| 2 | 1, plus the kernel view of the surface, the test picture, one flip |

Values above 2 count as 2. The knob is read once per transport generation (so `reg add` +
`pnputil /restart-device` applies it): one atomic holds the level, `u32::MAX` meaning unread, and
`retire_transport` (through `forget`) resets it to unread. At level 0 `service` returns on that one
load, before it asks for the virtio lock.

## 2. Architecture

```
HPD worker (PASSIVE, joined by StopDevice before the transport is retired)
  service()  ->  Client::next(Want)  ->  Step  ->  Io::perform  ->  Client::finish(Step, result)
                 (kmd_logic::rm_client,            (virtio/rm_client.rs)
                  pure, tested)
                                          |
                                          v
              nvrm::forward(owner = DeviceOwner::KMD_RM, MsgHeader | payload)
                 allow-list, ownership tables, quotas, check_ioctl: the same pipe a
                 user-mode RM client uses (docs/nvrm-escape.md)
                                          |
                                          v
              ctrl::raw_roundtrip -> host backend (host/backend/device/src/nvidia) -> RM
```

* `kmd_logic/src/rm_client.rs`: the wire messages and RM payloads (every offset pinned to
  `offsetof` on `guest/rmclient/src/nv_ioctl_defs.h`), the reply parsers, the surface
  geometry rules, the probe picture, and the state machine `Client`. No transport, no clock,
  no lock.
* `kmd_render/src/virtio/rm_client.rs`: a loop that asks `Client::next`, performs the step,
  and reports it. One small function per step (`#[inline(never)]`, so a step's stack buffers
  never add to another's frame: this runs on a worker thread under a deep transport call).
* Hooks in shared files, each one line or a few: `DeviceOwner::KMD_RM` and `from_token`
  (`virtio/gpu/mod.rs`); `retire_begin` / `forget` in `nvrm::close_all_on_host` /
  `retire_transport`; `service` in the HPD loop (`ddi/hpd.rs`); `publish_counters` in
  `publish_nvrm_counters`; the knob name in `diag.rs`; `from_token` in the foreign-scanout
  suppression gate (`adapter/foreign_scanout.rs`).

The client reuses `nvrm::forward` instead of a second table-and-quota implementation: a KMD
`Open` reserves a slot, commits the handle only for a good reply and is refused when the owner
is full; a KMD `Ioctl` is checked against the owner's handle table; a KMD `Close` takes the
entry first. The KMD is, exactly as `nvrm-escape.md` says of the pipe, "a pipe with ownership";
here it is also the client on the other end.

## 3. Ownership, quotas, counters

* **Owner.** `DeviceOwner::KMD_RM` is `NonZeroUsize::MAX`. `hDevice` is a pointer to a boxed
  `DeviceContext`, never all ones, and `DeviceOwner::new` now refuses that value, so no escape
  can present the token. `close_all_for_owner` (DestroyDevice, process exit) is keyed on the
  escaping device and never names it; the transport-wide sweep `close_all_on_host` does, like
  every owner's. `DeviceOwner::from_token` rebuilds an owner from a raw token the KMD itself
  stored (the foreign-scanout state keeps `owner.raw()` as a `u64`), and is the only way to get
  `KMD_RM` back from a number; the suppression gate uses it.
* **Quotas.** Per-owner limits (128 handles, 256 mappings, 256 pins) apply to this owner alone;
  the client holds at most five handles (control, GPU channel, DRM node, plus one export file
  and one map channel at a time) and no pins and no tracked mappings. The 1024 global handle
  slots are shared: a process that fills them makes the client's `Open` fail
  (`Refusal::NoResources`, counted in `NvRef`), which kills the client for the generation and
  leaves Venus in charge. `QUERY_CAPS` is unchanged.
* **The byte quota does not count the client's view.** `host_mmap` checks the per-device byte
  quota of RM-window mappings (a quarter of the window, `nvrm_map_bytes_room`) for the KMD owner,
  but the client's CPU view is never recorded with `push_nvrm_map` (it is not a process's `MMAP`
  and has no user view), so the bytes are neither accumulated against any quota nor counted in
  `NvMapMb`. Accepted: it is one mapping at a time, bounded by the surface (8 MB at 1080p, level 2
  only), and unmapped before the sweep. Left as is.
* **`Nv*` counters.** The client goes through `forward`, so its `Open`/`Close`/`Ioctl`s are
  counted in `NvOpen`/`NvClose`/`NvIoctl` like anyone's. With the client up, `NvOpen - NvClose`
  is 3 more than the user-mode handles (control, GPU, DRM); teardown closes are not counted
  (existing rule), so after a stop `NvOpen - NvClose` stays 3 too. `NvPin`, `NvMap`, events and
  fences are never touched (no pins, no `push_nvrm_map`, no events).
* **`Rm*` counters** (REG_DWORD, mirrored by `rm_client::publish_counters` from
  `publish_nvrm_counters`; written only once the client has run, so a knob-off driver writes
  none of them):

| value | what | healthy at level 1 / 2 |
|---|---|---|
| `RmKnob` | level in force (written only when nonzero) | 1 / 2 |
| `RmStatus` | `Client::status_word` after the last pass: `phase<<28 \| bring-up steps<<20 \| surface stage<<12 \| view stage<<8 \| probe` | `0x10b05000` at level 1 (phase Up, 11 steps, surface Ready); `0x10b05503` at level 2 |
| `RmStep` | the last step STARTED (written before it runs: a hang names itself) | last step of the sequence |
| `RmFail` | `step<<24 \| kind<<16 \| code`, of the last death | **0** |
| `RmDead` | how often the client died | **0** |
| `RmSteps` | steps performed | 16 at level 1, 24 at level 2, per generation |
| `RmBringUp` | bring-ups finished | 1 per transport generation |
| `RmSurf` / `RmSurfFree` | surfaces made / freed | 1 / 0; a mode change adds 1 / 1 |
| `RmGem` | GEM handles imported | = `RmSurf` |
| `RmView` / `RmViewFree` | kernel views made / unmapped | 0 / 0 at level 1; 1 / 0 at level 2 (and `RmViewFree` follows at stop) |
| `RmFillMs` | how long the probe fill took | tens of ms; hundreds means the WC write path is slow |
| `RmRdBad` | probe read-back samples that did not match | **0** |
| `RmProbe` | probe flips shown | 1 per surface at level 2 |
| `RmBusy` | probe sets that found scanout 0 held | 0 |
| `RmClosed` | handles closed after a death | 0 |
| `RmSoft` | undo steps that failed (counted, never retried) | 0 |
| `RmRegFd` | `REGISTER_FD`s the host refused (librmclient ignores these too) | 0 |

`RmFail` decodes with the step numbers of `rm_client::Step` and the kinds `1 Refused` (KMD
policy or quota; the code says which `Refusal`), `2 Transport` (1 timeout, 2 queue full or out of
memory, 3 other, 4 no transport, `0xE0` the generation changed mid-call), `3 Host` (the
host's negative errno), `4 Rm` (the `NV_STATUS`), `5 Parse`, `6 Layout`, `7 Os`, `8 Busy`.

## 4. The sequence, step by step

Every request is `MsgHeader | payload`; an `Ioctl` is `MsgHeader | IoctlReq | data | nested`
with `nested_offset = data_len` (`build_ioctl`; what `crm_wire_ioctl` writes). The host's
allow-list refuses an `RM_ALLOC` whose parameter block is not exactly RM's size for the class
(`host/backend/gen/src/rmallow`), so the sizes are not negotiable. Escape numbers are
`_IOWR('F', nr, data_len)`.

| `RmStep` | step | message | notes |
|---|---|---|---|
| 1 | `OpenCtl` | `Open` device_type 255 | the control file; handle recorded under `KMD_RM` |
| 2 | `VersionQuery` | `NV_ESC_CHECK_VERSION_STR`, cmd `'2'` | what librmclient does first: the host learns the driver version from a successful reply (its ABI profile and allow-list depend on it). The string is kept. A QUERY the host refuses or answers unreadably is **not** fatal (librmclient ignores it too): no string, so step 3 is skipped; only a transport failure or a refused forward ends the client |
| 3 | `VersionStrict` | the same, cmd 0, with that string | a mismatch is RM's `-EINVAL`, reported in the header. Skipped (success) if no string came back |
| 4 | `CardInfo` | `NV_ESC_CARD_INFO` (2304 bytes) | first valid card: `gpu_id`@16, `minor_number`@56 of the 72-byte records |
| 5 | `AllocRoot` | `NV_ESC_RM_ALLOC` NV01_ROOT_CLIENT, `hObjectNew = 0` | RM picks the client handle; read from the reply |
| 6 | `OpenGpu` | `Open` device_type = the minor | the GPU channel `NV01_DEVICE_0` needs open (librmclient opens it right after the root) |
| 7 | `RegisterGpuFd` | `NV_ESC_REGISTER_FD { ctl }` on the channel | refusal ignored, as librmclient does |
| 8 | `AllocDevice` | RM_ALLOC NV01_DEVICE_0 `0x4b4d0001`, 56-byte zero params | |
| 9 | `AllocSubdevice` | RM_ALLOC NV20_SUBDEVICE_0 `0x4b4d0002`, 4 bytes | |
| 10 | `SysFiles` | `GetSysFiles` (reply is a header-less stream, 128 KiB cap) | the DRI section picks the render node: the first whose `dev_info[0]` (the NVIDIA gpu id) equals the card's, else node 0 |
| 11 | `OpenDrm` | `Open` device_type 512 + index | |
| 16 | `AllocMemory` | RM_ALLOC NV01_MEMORY_LOCAL_USER, 128-byte `NV_MEMORY_ALLOCATION_PARAMS` | parameters copied from `crm_scanout_smoke` (nvk-rm's): type IMAGE, `ALIGNMENT_FORCE`, vidmem, 64 KiB pages, `ALLOW_NONCONTIGUOUS` (`attr` 0x19000000, `attr2` 6), alignment 64 KiB. RM's answer (pitch, size) is adopted if it still holds the picture |
| 17 | `OpenExportCh` | `Open` 255 | a fresh control file as the export envelope |
| 18 | `ExportToFd` | RM_CONTROL `NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD` (0x3d05), 24-byte params, `fd` = the export file's backend handle | the host translates the handle at nested offset 16 |
| 19 | `GemImport` | DRM ioctl `0xC0206441` on the DRM node: 32-byte data, 28-byte NVKMS block (`memFd`, layout 1 = pitch) | GEM handle at data offset 24 |
| 20 | `CloseExportCh` | `Close` | nvidia-drm holds its own reference now |
| 21 | `CloseExportChUndo` | `Close` of the same file, stage kept | only when the extent changes while the export file is still open (stages after `OpenExportCh`, before `CloseExportCh`): it goes before `GemClose` / `FreeMemory`. An undo: a failed close is counted (`RmSoft`), never retried |

Level 2 then runs (`RmStep` 32 to 36, 48 to 50) `OpenMapCh` (GPU channel), `RegisterMapFd`, `RmMapMemory` (`NV_ESC_RM_MAP_MEMORY`
with fd, on the control file: librmclient's channel-per-mapping protocol), `HostMmap` (`Mmap` of
the channel, offset 0), `KernelMap` (`MmMapIoSpace`, write-combined, over `RM window base +
offset`), `FillPattern`, `ScanoutSet` (the foreign-scanout state machine with the KMD's token,
lapse 4 s), `ScanoutPresent` (`virtio::foreign_scanout::present`: mints `seq`, sends the 64-byte
flip). Undoing, in reverse (40 to 43): `KernelUnmap`, `HostMunmap`, `RmUnmapMemory`, `CloseMapCh`; a
surface goes by `GemClose` (24; which first ends the KMD's own flip source, so no flip names a closed
GEM), `FreeMemory` (25).

Surface geometry (`surface_layout`): pitch = `width * 4` rounded up to 256 (so RM's answer equals
the request for every width: the 1896-wide mode whose 7584-byte rows once sheared becomes 7680);
size = `pitch * height` rounded up to 64 KiB; extents 64..16384, at most 256 MiB. The flip names
`XRGB8888` (`DRM_FORMAT_XRGB8888`, B,G,R,X in memory: what Windows' BGRX is), modifier linear,
offset 0, stride = the pitch RM reported.

## 5. The primary surface: what is decided and what is not

### 5.1 How the primary is written today

`KmdLinearPrimary` is a Venus HOST3D blob in a BAR (CPU-visible) segment: dxgkrnl's CPU aperture
(`DxgkDdiMapCpuHostAperture`) is served by host-mapping the blob into the Venus window head at the
offset dxgkrnl chose (`ctrl::map_blob_at`), so GDI/win32k raster writes the blob's own bytes, and
paging transfers copy through a transient kernel map of the blob (`build_paging_buffer.rs`). Frames
from DWM reach scanout as a Venus GPU copy into the adapter's dedicated LINEAR scanout image
(`production_linear_scanout`) or by binding the app's own resource; both then go out as
`SET_SCANOUT_BLOB` + `RESOURCE_FLUSH`.

### 5.2 Why an RM surface cannot simply replace it

* **The CPU reaches RM memory only through the RM window** (shared-memory region 1): the host
  chooses the offset (`NV_ESC_RM_MAP_MEMORY` allocates the shm range; `Mmap` reports it), the
  guest cannot. dxgkrnl's aperture addresses are `segment base + page * 4K` inside the Venus
  window head (region 0), at offsets dxgkrnl chose. An RM mapping cannot be placed at those
  addresses, so **dxgkrnl cannot CPU-map an RM surface through the existing segment**, and a WDDM
  allocation backed by one would have no CPU view for GDI at all.
* **The window is write-combined** (the Linux module's default for BAR memory, and what the
  user-mode `MMAP` does): sequential writes are fast; reads are ~200 MB/s (the figure
  recorded in the `AllocCached` knob's comment for WC on this stack). GDI is read-modify-write (ClearType, alpha), so
  a **vidmem surface must not be what GDI writes**.
* NVK-on-RM has no host-visible VRAM (`has_host_visible_vram = false`, `bar_size_B = 0`,
  `nvkmd_rm_pdev.c:360`, `dxvk-on-nvk.md` 3.4): VRAM is for the GPU. Slice 1's CPU view is a
  write-only probe through the BAR window, and whether that window can map vidmem at all on this
  host is the first thing the hardware run decides (step 7 below, `RmFillMs` / `RmRdBad`).

### 5.3 The answer to "how do CPU/GDI writes reach the RM surface"

**In slice 1 they do not**, by decision: the KMD writes the surface only through its own
kernel view, and only the probe does. The production primary needs one of three designs, none
decidable without hardware, listed by what they buy:

| | surface | GDI writes land | DWM frames land | needs |
|---|---|---|---|---|
| A | RM **system** memory, KMD-mapped through the RM window (`NV01_MEMORY_SYSTEM`, 0x3e; the host allow-list serves it with the same 128-byte block) | via paging-transfer CPU copies (the BAR-content-op executor, retargeted at the kernel view) | a CPU copy at present, or A+C | a segment/aperture decision (dxgkrnl cannot map it, see 5.2) |
| B | RM system memory made of **dxgkrnl's own pages** (`NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` through the KMD's pin machinery: page runs of locked pages) | natively: GDI writes guest RAM that RM aliases | a CPU copy, or A+C | whether NVKMS' GEM import and `EXPORT_OBJECT_TO_FD` accept an OS-descriptor object (**UNVERIFIED**; likely not for display) |
| C | RM **vidmem** as the flip target | not at all (a copy feeds it) | a Venus GPU copy into it, zero CPU | host H1/H5: import the RM GEM's dma-buf as Venus memory (`zero-copy-present.md` section 5), then `prepare_optimal_scanout_copy` targets it as it targets the LINEAR image today |

The recommendation is **C for frames and A or B for the GDI-written surface**, i.e. the
primary stays two objects (a CPU-written one, and the flip target a GPU copy fills), which is
what Windows does on bare metal too. **Section 12 settles this** (and replaces A and B by
something sharper). The surface this slice makes is C's flip target; its
allocation, export and flip are what slices 1b and 2 reuse unchanged. Changing the memory kind is
a change of class and `attr` in `mem_alloc_params` and one constant; no payload of the other
steps changes.

### 5.4 Presenting, and scanout restore

The KMD's source is the **foreign-scanout source with the KMD's owner token**: one state cell
(`adapter/foreign_scanout.rs`), the same suppression gate in `queue_active_scanout_refresh_locked`
(only the host `RESOURCE_FLUSH` of the desktop is withheld; binds, WDDM fences, vsync and the read
ledger are untouched), the same sequence numbers (`seq` strictly increasing across every source of
the boot, which the host requires), the same restore: when the source ends, `ReleasePending`
asks for one fresh desktop flush. It ends by the lapse (here: 4 s after the flip, `FsLapse`), by
the close of its DRM file (`foreign_scanout_release_handle`, from `forward`'s `Close` and from the
retire sweep), by `GemClose`, and by transport reset.

Consequences, stated so nobody finds them by surprise:

* While the probe shows, a **user-mode foreign source or a forwarded `ScanoutFlip`** from another
  device is refused (`Busy` / `FORBIDDEN`), and the probe's own `ScanoutSet` finds a user source
  `Busy` (retried on three successive worker passes, then skipped, `RmBusy`; a `Busy` ends the pass, so
  the retry waits for the worker's next wake, which the other source's lapse provides).
* The flip has **no completion**: the host's reply is a bare header and the viewer's release event
  is not forwarded. The probe writes the picture before the flip and never touches the surface
  after it, so no reuse protection is needed; a production source needs N buffers or a release
  signal (`dxvk-on-nvk.md` 3.6).
* A production KMD source has no lapse and no end; it must **yield** to a user-mode source (a
  fullscreen NVK app) and resume when that ends, which the single-source state machine does not
  yet express (it would need a priority for the KMD token and a restore that re-flips the KMD
  surface instead of flushing Venus). That is the first thing slice 1b adds.

## 6. Lifetime and teardown

* **One worker, one writer.** `service` runs on the HPD worker only. StopDevice joins the worker
  (`stop_hpd`) before `retire_transport`; `AdapterContext::drop` does the same before
  `close_all_on_host`. The state `CLIENT` is a leaf spinlock over plain data: never held across a
  round trip, a registry write, an allocation or another lock; never taken under `virtio_lock`,
  nor the reverse. A step copies what it needs out, does its I/O unlocked, and reports under the
  lock, discarding the report (and unmapping a view it made) if the generation changed.
* **Retire.** `close_all_on_host` begins with `retire_begin`: the kernel view is unmapped before
  the host is asked to close the file that holds the mapping (and whether or not the host is
  alive). The sweep then closes the client's handles like any owner's (`Close`, with the sweep's own budget; the client's host mapping is not in the map table, so no `Munmap` is sent for it and the host's close of the channel file is what drops it, **untested**); closing the control file frees every RM client made on it, closing the
  DRM file drops its GEM handles and the dma-bufs exported for them. `retire_transport` ends with
  `forget`, which also unmaps a view recorded in the gap. Nothing is double-closed: a `Close` of a
  handle the sweep already took is `NOT_OWNED`.
* **Bounded at StopDevice.** `stop_hpd` joins the worker for 5 s, twice, and leaks the worker and
  the adapter if it does not exit. So a client caught mid-bring-up (knob on) must not hold it: each
  message is bounded by `TIMEOUT_MS` = 2.5 s (was 10 s), the step loop checks `hpd_stop` before every
  step, and `cleanup` checks it before every `Close` and stops sending after the first transport
  failure. What was not closed stays in the NVRM tables under `KMD_RM`, and the sweep closes it
  (or drops it when the host is gone).
* **Frames.** `service`, `retire_begin`, `forget` (which builds a `Client` by value) and `unmap_view`
  are `#[inline(never)]`, so they add nothing to the frames of `retire_transport`,
  `close_all_on_host`, `dxgkddi_start_device` or the worker.
* **A new transport generation** (`nvrm_epoch`) resets the client to cold (`sync_epoch`), so even a
  missed hook self-heals; surface handles keep counting across generations.
* **No work at init.** Nothing was added to `VirtioGpu::init` or `StartDevice`; the knob is read by
  the worker, the state is a static, so the boot-stack frame budget (`tools/kmd-frame-sizes.ps1`)
  is not touched. Every step function keeps its buffers under ~450 bytes (`CARD_INFO` and
  `GetSysFiles` use `try_reserve_exact` heap vectors).

## 7. Failure and fallback matrix

| where | failure | effect | fallback |
|---|---|---|---|
| knob 0 | none | nothing runs | Venus (as today) |
| bring-up (`RmStep` 1 to 11) | refused, timeout, host errno, RM status, unreadable reply, no card, no DRI node | client **dead** for the generation: view unmapped, every opened handle closed; `RmFail`, `RmDead` | Venus |
| surface (16 to 20) and its free (24, 25) | same, or RM's pitch/size cannot hold the picture | dead, as above | Venus |
| `RegisterGpuFd` / `RegisterMapFd` | host refuses | counted (`RmRegFd`), ignored | continues |
| view (level 2): map channel, `RM_MAP_MEMORY`, host `Mmap`, `MmMapIoSpace` | any | the view is given up and unwound; the client lives | the surface is still valid for a flip |
| probe: fill / set / present | any | the probe is skipped; read-back mismatch is only counted | none needed |
| `ScanoutSet` finds a user source | `Busy` | retried once per worker pass, up to three times | skipped |
| `ScanoutPresent` | host refuses or times out | probe skipped; the KMD's source is released at once (`foreign_scanout_release`), so the desktop flush is restored without waiting for the 4 s lapse | desktop |
| transport stop / replace | `retire_transport` | view unmapped, handles closed, state forgotten | next generation starts cold |
| transport failed | sweep sends nothing | tables cleared; `VirtioGpu::drop` finishes; state forgotten | n/a |
| generation changed mid-step | `send` sees a different epoch | `Transport 0xE0`, result discarded | cold start |
| host table full (`NoResources`) | another process holds the 1024 slots | dead | Venus |
| surface size changes (mode set) | extent differs from the surface's | undo the view, `CloseExportChUndo` if the export file is open, `GemClose`, `FreeMemory`, allocate again; a failed free is a death | Venus |
| StopDevice while a step runs | `hpd_stop` set | no further step or close is started; the sweep closes what is open | n/a |

Nothing consumes the client in production yet, so "dead" costs nothing but the probe. When
something does (slice 1b), the consumer asks `ready_surface()` and keeps using Venus when it is
`None`: the Venus allocation arms are untouched and still run first.

## 8. What is verified here, and what is not

Verified on the host (`cargo test` in `guest/windows/kmd_logic`, 38 new tests): every payload
offset against the values `offsetof` gives on `nv_ioctl_defs.h` (gcc, this branch's header); the
escape numbers; the message envelope against `crm_wire_ioctl`; every reply truncation (none
panics, none parses); the `GetSysFiles` DRI section against the host's writer
(`files.rs::write_dri_section`) including truncations and overflow of the node table; the card
record; the surface geometry (pitch and size rounding, RM's rounding accepted only if it holds
the picture); the state machine: the full level 1 and level 2 sequences, a failure injected at
every one of the 16 required steps (dead, exactly the handles opened so far are closed, once),
sticky death until the generation changes, generation reset, mode-change teardown order, a
failed free, a failed view (unwound, client alive), unwind steps that fail, the probe's `Busy`
budget, and a result that does not carry what its step makes.

Not verified by anything: that the KMD crate compiles (the I/O file was type-checked only against
a shim); every host reaction (the exact allow-list answers, `NV_ESC_RM_MAP_MEMORY` of vidmem,
`Mmap` of that channel, the GEM import of a KMD-exported object, `ScanoutFlip` from a KMD owner);
IRQL behaviour; the retire ordering with a live client; the picture on screen.

## 9. Hardware checklist, in order

Stop at the first step that fails; each says what to read. `reg query
HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`.

1. **Build and boot with the knob absent.** `tools/kmd-frame-sizes.ps1` passes (nothing was added
   to init); the desktop and the existing NVRM smokes behave as before; no `Rm*` value exists.
2. **Prerequisites** (the host and the VM, not the KMD): `NvEvQ = 1`, `NvWinMb` nonzero (level 2
   maps the RM window), the host log's `GET_SYS_FILES: n DRI device(s)` with n >= 1 (the
   backend serves render nodes only with its graphics capability on), and `crm_scanout_smoke` passing in the same VM (it is the
   same route with a user-mode client; if it fails, this will too, for the same reason).
3. **`KmdRmClient=1`, restart the device.** Wait for the desktop's first present. Expect
   `RmBringUp=1`, `RmSurf=1`, `RmGem=1`, `RmDead=0`, `RmFail=0`, `RmStatus=0x10b05000`; host log
   RM_ALLOC classes 0x41, 0x80, 0x2080, 0x40, RM_CONTROL 0x3d05, a DRM ioctl `0xC0206441`; the
   desktop unchanged. If `RmFail` is set: the step number is the table's first column in section 4 and the kind
   says whose answer it was.
4. **Stop and start the device again** (`pnputil /restart-device`). Host log: the three handles
   closed (and the GEM dma-buf forgotten); `RmBringUp` and `RmSurf` become 2 on the next
   generation; nothing in `NvSwept`.
5. **Change the resolution.** `RmSurfFree=1`, `RmSurf=2`, the new surface at the new size.
6. **A failure that must not hurt:** with the transport quiet, run `crm_scanout_smoke` while the
   client is up (both own handles; the 1024 global slots are nowhere near full). Then fill the
   global table from a test (or lower a limit) and watch `RmFail` read `1 Refused / NoResources`
   with the desktop unaffected.
7. **`KmdRmClient=2`, restart the device.** `RmView=1`, `RmFillMs`, `RmRdBad=0`, `RmProbe=1`.
   **On screen:** eight colour bars over two thirds, a grey ramp below, a one-pixel red border,
   for about four seconds, then the desktop returns (`FsSet`, `FsPres`, `FsLapse` +1, `FsRest`
   +1, `FsSupp` > 0 only while it showed). A wrong stride shows as shear or a missing border;
   black means the flip was refused or the GEM is not what was filled. If `RmView=0` with
   `RmDead=0`, the BAR window does not map vidmem here: that is itself the answer to section 5
   (read the `RmStep` it stopped at: `HostMmap` = the host refused, `KernelMap` = `MmMapIoSpace`).
8. **Stop the device while the picture shows** and while the client is mid-bring-up: no bugcheck,
   `RmViewFree` follows, the retire sweep closes the handles, the desktop is back after start.
9. **`FsLapse` hygiene:** `FsSet - FsRel - FsLapse - FsEnd - FsTake` is 0 afterwards.

## 10. Stages

* **Slice 1 (this):** the client, one vidmem surface sized from the primary, export to GEM, the KMD
  as flip source, the probe. Behind the knob; Venus is the fallback and still runs.
* **Slice 1b: make it the VidPn primary's flip target.** Decide 5.3. Give the KMD token priority in
  the foreign-scanout state (user source preempts, KMD resumes and re-flips instead of flushing
  Venus); N surfaces and a release signal (or flip-then-write discipline); the content path.
  Hardware gates: the table in 5.3, `RmFillMs` and `RmRdBad` from step 7.
* **Slice 2: DWM allocations, after S3/S4.** `KmdOptimalGdiTexture` and `KmdStandardBuffer` from RM
  (`create_one` arms), adopted as foreign resources (an internal variant of `IMPORT_RM` that takes
  the KMD's own `(drm, gem)` instead of a device's, needing H1/H4/H5 on the host), so DWM and the
  Venus importers still see a resid; `HeliosWddmAllocMeta` carries the layout. The client grows
  more than one surface and a free list; quotas move from "one" to a table (capacity reserved at
  init: the state is a static now, so this is a constant).
* **Slice 3: Venus removed from the KMD.** Delete the Venus arms of `create_one`, the LINEAR
  scanout image and its GPU copy; `RESOURCE_FLUSH` leaves the display worker; the read ledger is
  fed by a host release signal.

## 11. Open questions

1. **Where the production primary's pixels live and how GDI reaches them** (section 5.3). Needs the
   hardware numbers of step 7 and the host import (H1/H5) before A/B/C can be chosen.
2. **KMD source vs user source**: the priority and the restore rule (5.4). The single-source state
   machine cannot yet say "the KMD source resumes when the app's ends".
3. **DRI node choice.** The match `dev_info[0] == card.gpu_id` assumes the two ids are the same
   number (both are the NVIDIA gpu id, per the host's log line and `nv_ioctl_card_info.gpu_id`);
   unmatched, node 0 is used. A multi-GPU host with the wrong node would fail at the GEM import
   (`RmFail` step 19, `GemImport`), not silently.
4. **`GemImport` of a KMD-exported object** is the same ioctl a user-mode client sends, but the
   export file here is opened by the KMD owner and closed by the KMD right after; the smoke test
   does the same, from user mode (**UNVERIFIED** on Windows).
5. **Vidmem accounting.** The host's `--vram-limit-mib` charges the surface (it is
   `NV01_MEMORY_LOCAL_USER`); 8 MB at 1080p. A guest-wide limit that small would make `AllocMemory`
   answer `NV_ERR_NO_MEMORY` (`RmFail` kind 4, code 0x51).
6. **The 5 s `ScanoutFlip` wait on the HPD worker.** A wedged host holds the display worker for the
   flip's timeout once (the probe only); every other host round trip of the client is bounded by 2.5 s
   and happens a few at a time.

## 12. Slice 2: how CPU data reaches a KMD allocation that lives in RM memory (decided)

Decided from the code, before any of slice 2 was written, and still unmeasured: every number is an
estimate and every "hardware" line is a hypothesis the checklist in section 13.9 tests.

### 12.1 How dxgkrnl reaches the primary today (read, not measured)

* **Two segments** are reported (`ddi/bar_segment.rs`, `build_segment_table`): the *aperture*
  (id 1, system memory: pages dxgkrnl owns, mapped for the GPU by `MapApertureSegment`; the segment
  VidMm carves paging buffers from) and the *BAR memory segment* (id 2) in the **CpuHostAperture
  shape** (`SegmentSpec::bar`), the only memory-segment CPU shape this dxgkrnl accepts (the classic
  `CpuVisible` + `CpuTranslatedAddress` form fails AddAdapter with Code 43, ETW-proven). The BAR
  segment's base is the **Venus window** (shared-memory region 0), `min(window / 2, 1 GiB)` long;
  `configure_window_reserve` keeps the KMD's own blob allocator out of that head.
* **`KmdLinearPrimary` is a Venus HOST3D blob** (`allocate_linear_scanout_image_blob`) placed in
  that segment. When dxgkrnl needs the CPU on it (a `Lock`, win32k GDI raster) it calls
  `DxgkDdiMapCpuHostAperture` with the aperture pages **it chose** inside the window head, and
  the KMD answers by `RESOURCE_MAP_BLOB`-ing the blob **at exactly that window offset**
  (`ddi/cpu_host_aperture.rs` -> `ctrl::map_blob_at`; whole allocation, consecutive pages only,
  anything else refused loudly with `ChE*`). The CPU VAs dxgkrnl builds over `window base + page *
  4 KiB` then read and write the blob's own bytes: one memory for GDI, the host GPU and DWM's
  Venus import. Content ops of paging (`build_paging_buffer.rs`) copy between the system MDL and a
  transient kernel map of the blob.
* **It is fast because it is RAM.** The window is host shared memory, cache-coherent for every
  agent on the same pages, so `AllocCached` (default 1) flags CpuVisible allocations `Cached` and
  dxgkrnl maps WB. The same stack measured write-combined *reads* at ~200 MB/s (36 ms for a 7.8 MiB
  frame, 2026-07-06): GDI is read-modify-write (ClearType, alpha), a WC view cannot be what it
  writes.
* **What reaches scanout.** With DWM composing, frames are Venus GPU copies into the adapter's
  dedicated LINEAR scanout image (`production_linear_scanout`), published as
  `primary_scanout_*` (resource, extent, pitch, allocation size, seqlock) and bound with
  `SET_SCANOUT_BLOB`; the desktop's `RESOURCE_FLUSH` is queued by the HPD worker when the copy's
  completion marks scanout dirty. A DWM image the UMD binds directly (`direct_scanout`) is a
  different resource and is not published as the primary.

### 12.2 Why an RM allocation cannot sit behind that aperture

| | Venus blob (today) | RM memory (this client's) |
|---|---|---|
| who picks the guest-visible address | **dxgkrnl** (aperture page index); the KMD asks the host to map the blob *there* | **the host** (`NV_ESC_RM_MAP_MEMORY` allocates the shm range, `Mmap` reports it) |
| region | 0, the Venus window head: the BAR segment's declared aperture | 1, the RM window (4 GiB, per-device user-map quota window / 4) |
| CPU view | WB over RAM-backed host shmem (host `MAP_INFO`, `AllocCached`) | what the KMD asks `MmMapIoSpace` for: WC (vidmem is BAR1 on the host), reads ~200 MB/s |
| GDI read-modify-write | yes | no |

An RM mapping cannot be placed at dxgkrnl's offsets, so **dxgkrnl cannot CPU-map an RM surface through
the existing segment**, and a WDDM allocation backed by one would have no usable CPU view for GDI at all.

### 12.3 The candidates, and what was found

* **(a) RM SYSTEM memory as the primary** (`NV01_MEMORY_SYSTEM`, or an OS descriptor over pages the
  KMD pins), flipped directly. As the GDI-written surface it has the same placement problem as
  any RM memory (12.2). As a *flip source* the host side has no objection (`handle_scanout_flip`
  PRIME-exports whatever GEM it is given: no memory-type check); whether NVKMS' GEM import and
  `EXPORT_OBJECT_TO_FD` accept a system-memory or OS-descriptor object, and whether the host
  compositor can sample a 29 MB sysmem dma-buf at the display rate over PCIe, is **unknown**. A
  one-constant hardware experiment (`ATTR_LOCATION`, class), listed, not built.
* **(b) A shadow in CPU-coherent memory plus the KMD's own copy into an RM vidmem surface through its
  own write-combined RM-window view, at present time.** Needs nothing of dxgkrnl, nothing of the host.
  **Chosen for this slice** (12.4).
* **(c) An RM system-memory POOL as the CPU-visible segment.** One RM memory object (hundreds of MiB),
  mapped once into region 1 by the host, and the BAR segment's CpuHostAperture declared over *it*
  instead of over the Venus window head. dxgkrnl's chosen aperture offsets are then valid CPU
  addresses by construction (no per-allocation `map_blob_at`), GDI read-modify-write runs at WB
  speed **if the host maps RM sysmem cacheable**, the GPU sees the same bytes (NVK imports a
  sub-range; `ScanoutFlip` has a plane `offset`), and every KMD allocation becomes a sub-range of
  one RM object. This is the destination for `KmdStandardBuffer` and the GDI surfaces. It is also
  the biggest change: `BarSegMode` and the segment base, the paging content ops (in-pool memcpy
  instead of blob maps), the Venus-less arms of `create_one`, a host guarantee of a cached map,
  sub-range import into NVK. **Not this slice**; every one of those is a blind decision today.
* **(d) An OS descriptor over dxgkrnl's own pages.** The pin machinery already builds the page-run
  table from any locked MDL (`build_table`), but locks *user* ranges (`helios_lock_user_pages_seh`):
  the KMD needs a variant for the MDL of `MAP_APERTURE_SEGMENT` (kernel-owned pages), and
  NVKMS import of an OS-descriptor object is unlikely for display. The right tool to make GDI
  memory *GPU-readable* (DWM-on-NVK sampling a redirection surface) if (c) fails; irrelevant to
  scanout.

### 12.4 The decision

1. **CPU-written bytes live in memory dxgkrnl maps natively (today: the Venus blob in the BAR
   segment; destination: the pool of (c)); RM window memory is only ever written, never read, by
   the KMD.** The GDI-written surface is never RM vidmem and never a WC view.
2. **This slice (`KmdRmClient` = 3) leaves every allocation exactly as it is** (Venus blob, dxgkrnl's
   CPU path, `AllocCached`, paging: zero change in what dxgkrnl sees, which is what makes the Venus
   fallback total) and moves **scanout** of the composited desktop to RM: the LINEAR primary the
   display worker already keeps current is the *shadow*; the RM ring is the *flip target*; the KMD
   copies the one into the other at present time with write-combined non-temporal stores and flips
   with its own `ScanoutFlip` (candidate (b)). When the shadow later moves to the pool of (c) only
   the *source address* of that copy changes (and, for pool memory that is itself RM memory, the
   copy can disappear).
3. **Why not allocate the VidPn primary from RM now.** The surface that dxgkrnl and win32k write is
   in neither of the two places an RM allocation can be made visible to them (12.2); the only designs
   that do it ((c), (d)) each need a host decision and a segment/paging rewrite that cannot be
   verified blind, and (c) is the right *destination* for the GDI surfaces, not a primary-only
   change. What can be built blind, tested on the host and reverted by a knob is the scanout half,
   which is also what a user-mode NVK source needs the KMD side of (priority, restore).
4. **Performance, stated so nobody mistakes level 3 for the fast path.** One 5120x1440 XRGB frame is
   29.5 MB (1920x1080: 8.3 MB). 240 whole-frame copies a second would be 7 GB/s of streaming WC
   writes *and* the same in source reads: not a CPU-copy workload. So the presenter paces to 60 Hz
   and coalesces (the newest content wins); a frame costs one pass over both buffers, estimated
   3 to 10 ms of WC writes at 5120x1440 (non-temporal 64-byte bursts) plus the read of the source,
   whose speed is **the** unknown: from a WB-mapped host blob it is at memory speed, from a WC-mapped
   one (host `MAP_INFO`) it is ~200 MB/s, 150 ms a frame, and the path is a demo. `RmCopyMs` /
   `RmCopyMaxMs` measure it. The fast paths are not this copy: a user-mode NVK source flips its own
   RM images with no KMD copy (the priority stack below makes it win), and DWM-on-NVK presents the
   same way (S3/S4/S6) with this ring only the fallback. Dirty rectangles: GDI CPU raster reports
   none to the KMD and the desktop edge at the refresh gate carries none, so the first slice copies
   whole frames; the plan type already takes a row range (`CopyPlan` `y0..y1`) for the two next
   steps, listed in 13.10, band diffing against a guest mirror and the GPU copy engine.
5. **Reuse protection without a host release.** The flip has no completion and the backend does not
   forward `EV_RELEASE` (`docs/SCANOUT.md`, Buffer release), so the ring has two surfaces and a frame
   is never written to the surface flipped last; the residual hazard is a viewer that still samples
   the previous buffer a whole flip interval later (tearing, never corruption). Three surfaces is a
   constant (`RING_SLOTS`); the exact fix is the host forwarding the release.
