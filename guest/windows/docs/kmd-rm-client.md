# The KMD's own RM client (slice 1)

Status: slice 1 written on `kmd/rm-client` against `044b242` (KMD 22.22.309); slice 2
(sections 12 to 14: the decision on CPU access, the source priority stack, the level 3 ring and
presenter, the KMD as the creator of foreign resources) on `kmd/rm-client-s2` against `bd5bef6`; section 15
(the primary from RM system memory, `KmdRmClient` = 5) on `kmd/rm-sysmem-primary`, merged onto the v315 line (flush gate, scanout
release event) on `kmd/rm-sysmem-primary-merged` with its cache default and release seam decided (15.5, 15.7, 15.15).
**Never built, never run**: the KMD cannot
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
| 3 | (slice 2, section 13) a ring of two surfaces with their views, and the composited desktop shown through it by the presenter, with Venus as the fallback |
| 4 | (section 14) 3, plus each ring surface imported as a foreign resource under the KMD's own owner (the resid a WDDM allocation adopts) |
| 5 | (section 15) no ring: the VidPn primary itself from RM SYSTEM memory (write-combined by default, Venus as the fallback), mapped by the host into the CPU aperture's window and flipped with the KMD's own `ScanoutFlip` |

Values above 5 count as 5. The knob is read once per transport generation (so `reg add` +
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
  and one map channel at a time; at level 3 a map channel stays open per ring slot, so up to six)
  and no pins and no tracked mappings. Fences the KMD took over for fenced presents
  (`rm-fence-marker.md`) are re-tagged to this same owner but counted apart, against
  `MAX_NVRM_ATTACHED_FENCES` (512, KMD-wide), and are excluded from the owner's 128 count, so
  neither budget can starve the other (13.12). The 1024 global handle
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
  (existing rule), so after a stop `NvOpen - NvClose` stays 3 too. `NvPin`, `NvMap` and events are
  never touched (no pins, no `push_nvrm_map`, no events), and the client never creates, attaches or
  closes a fence: the ones owned by `KMD_RM` are the fenced presents' (counted by the fence counters of `rm-fence-marker.md`).
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
  signal (`dxvk-on-nvk.md` 3.6; the signal now exists, `foreign-scanout.md` "Buffer release").
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
  step, and `cleanup` checks it before the blob release and before every `Close` and stops sending
  after the first transport failure. **Every call inside a step is bounded too**, not only the
  forwarded messages: the level 4 import and release (`rm_foreign`) share one `TIMEOUT_MS` budget
  across all their host commands (`alloc_blob_errno_within`, `release_blob_for_owner_within`,
  a `SweepBudget`), `release_all` is the same budget, the host `Mmap` and its undo take `TIMEOUT_MS`
  (`host_mmap_within`, `release_host_map_within`), and the presenter's source map and flip take 1 s
  each. Before this merge those were 30 s calls (the control queue's ordinary synchronous bound).
  What was not closed stays in the NVRM tables under `KMD_RM`, and the sweep closes it (or drops it
  when the host is gone); a KMD-owned blob not released because the worker was stopping is reclaimed
  by the context destroy and the device reset that follow in StopDevice.
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
  Hardware gates: the table in 5.3, `RmFillMs` and `RmRdBad` from step 7. **Done as far as it can be
  decided blind in slice 2 (sections 12 and 13)**: the decision on 5.3, the priority stack and the
  resume rule, a ring of two surfaces, the copy, the presenter. Left: everything on the hardware
  checklist (13.9) and the Venus-less allocation (12.3 (c)).
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

1. **Where the production primary's pixels live and how GDI reaches them** (section 5.3). Decided in
   section 12 for the CPU side (the pool of 12.3 (c) is the destination; level 3 copies from the
   existing blob); the hardware numbers of 13.9 and the host's cached map of RM sysmem decide (c).
2. **KMD source vs user source**: the priority and the restore rule (5.4). Answered in 13.2 (user
   source > resident KMD source > Venus; the KMD resumes by re-flipping); unverified on hardware.
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
5. **Reuse protection.** The flip has no completion, so the ring has two surfaces and a frame is
   never written to the surface flipped last; the residual hazard is a viewer that still samples
   the previous buffer a whole flip interval later (tearing, never corruption). With the host's
   buffer-release event (`NVGPU_F_SCANOUT_RELEASE`, acked with the display half; `foreign-scanout.md`,
   "Buffer release") the presenter also WAITS for the host to release the surface it will write
   (the book of `kmd_logic::scanout_release`, `Presenter::back_wait_seq`), at most 500 ms
   (`RelRWaits`, `RelRTimeouts`), and a host without the feature keeps the old rotation. Three
   surfaces remains a constant (`RING_SLOTS`) and a way to make the wait rarer.

## 13. Slice 2 as built: the source priority stack, the ring and the presenter (`KmdRmClient` = 3)

Pure logic: `kmd_logic/src/foreign_scanout.rs` (resident source), `rm_client.rs` (ring slots),
`rm_present.rs` (source eligibility, copy plan, ring, presenter); 620 host tests in the crate, including co-simulations of the presenter against the real arbiter (alone, and with the fenced-present queue of `rm-fence-marker.md`: 13.12). I/O:
`kmd_render/src/virtio/rm_present.rs` and the hooks in `adapter/foreign_scanout.rs` and
`virtio/rm_client.rs`. The I/O half (and the `ctrl.rs` / `nvrm.rs` functions it calls that this slice
changed) was type-checked against a harness generated from the REAL module declarations and
signatures of `kmd_render` (module visibility is taken from the real `mod` lines, never typed by
hand: an earlier shim declared a private module `pub` and hid a privacy error) and read by hand;
**it has not been built**.

### 13.1 What level 3 is, and is not

It shows the **composited desktop** through RM. While the LINEAR primary the display worker keeps
current (the adapter's dedicated scanout image, published as `primary_scanout_*`) is what is bound
to scanout 0, the desktop's host flush is withheld and each withheld flush becomes a frame: the
rows of that primary are copied into the ring surface not shown last and flipped with the KMD's own
`ScanoutFlip`. It is **not** the VidPn primary allocated from RM (section 12.4: the allocation is
untouched), it does not cover a DWM image the UMD binds directly (`direct_scanout`, a different
resource: `RmSrcWhy` 2, Venus keeps the screen) or the bootstrap primary before the dedicated image
exists, and it is not the fast path (12.4 item 4).

| `KmdRmClient` | does |
|---|---|
| 0 (default) | nothing |
| 1 | client + one surface + GEM import. Invisible |
| 2 | 1, plus the kernel view, the test picture, one flip for four seconds |
| 3 | the **ring**: two surfaces, each with its kernel view and GEM; no probe; the presenter shows the desktop through it |
| 4 | 3, and each surface also a foreign resource (section 14). Values above 4 count as 4 |

### 13.2 Who owns scanout 0: the source stack

```text
priority:   user-mode source (SCANOUT_SET/PRESENT: NVK, a game)   >   the KMD's resident source (level 3)   >   Venus desktop flush
```

| situation | on screen | desktop `RESOURCE_FLUSH` | what changes it |
|---|---|---|---|
| no source | Venus desktop | flushed | the presenter registers (ring ready and the primary eligible) |
| **resident** (KMD) | the ring's front surface | withheld; every withheld flush is a frame edge for the presenter | the KMD withdraws it (primary no longer eligible, ring not ready, three failures): the desktop is owed one Venus flush. Its DRM file closed, its generation or epoch invalid: same, and the presenter re-registers after a 100 ms pause. A user `SET`: preempted |
| **user** source | the app's image | withheld; the desktop edges are only remembered ("changed") | release, lapse (2 s default), its file closed, its device destroyed, an invalid handle or epoch: **the resident source takes the screen back** (a resume edge); with none registered the desktop is owed a flush as before |
| user source, then another user `SET` | the first keeps it | withheld | the second gets `SCANOUT_BUSY` until the first lapses (unchanged) |
| forwarded `ScanoutFlip` with no `SET` | unchanged | | refused `FORBIDDEN` while any source is live (unchanged; so with level 3 on, an NVK app that still uses forwarded flips must `SET` first) |

`ForeignScanout` (pure) adds `resident_set`, `resident_drop`, `resident`, `resident_foreground`,
`take_resume_owed` and the `SetKind::Preempted` outcome. Every way a source ends funnels through one
function (`foreground_ended`): a user source ending with a resident one registered leaves the state
`Active(resident)` with `resume_owed`; the resident one ending is forgotten and leaves
`ReleasePending`. A resident source never lapses and needs no timed wake; a parked one whose file
closes is forgotten without touching what is on screen; `reset` forgets it; `seq` stays strictly
increasing across all of it.

**Restore, concretely.** `foreign_scanout_restore_desktop` (every end hook) now asks
`take_resume_owed()`: true means a resume edge (`rm_present::note_resume_edge`: an atomic and
`KeSetEvent`, legal at DISPATCH); the presenter answers it by flipping, never by flushing Venus. A
resume after a tenure in which the desktop changed (the gate remembers it: `note_desktop_changed`)
**copies** the newest content into the surface not shown last; after a quiet tenure it re-flips the
front surface unchanged.

### 13.3 The ring and the frame path

* **Client** (`rm_client`, level 3): after bring-up, per slot `AllocMemory`, export, `GemImport`,
  view (`OpenMapCh` .. `KernelMap`); then `Park` (a pure move: the finished working slot is set aside,
  `parked_n` = 1) and again for the second. `RmSteps` is 32 per generation, `RmStatus` `0x11b05500`.
  A new extent tears every slot down (`Unpark` brings a parked one back to be unwound with the same
  steps) before anything new is made; a death closes every slot's files; a view that fails ends the
  ring there (not presentable, no retry, the client lives). `Want.slots()` is 2 at level 3, 1 below.
* **Presenter** (`Presenter::decide`, one act per call, at most three per worker pass): `Register` the
  resident source, `CopyFlip { slot }` (the slot is never the one flipped last), `Reflip { slot }`,
  `Withdraw`, `WaitUntil` (pacing: at most one frame per 16 ms; edges coalesce, the newest content
  wins). The first frame after a registration is always owed. A flip that finds the source yielded
  (a user source took scanout 0 in between) is not a failure; three failures of any kind in a row,
  with no shown frame between, give up for the transport generation: the resident source is withdrawn.
  After a failure (a refused registration, a copy or flip that failed) the presenter itself waits
  100 ms before the next attempt (`WaitUntil`), so the three strikes are never spent in one pass.
* **A frame**: `lease_slot` (under `CLIENT`'s lock: the epoch, the ring whole, the slot's view and
  GEM copied out, `VIEW_LEASED` set), `map_blob_prepare_within` (1 s in all, none started once the
  worker is stopping; a mapped blob needs no round trip) + `MmMapIoSpace` of the primary's blob with
  the host's `MAP_INFO` cache type, `CopyPlan` (every bound of both mappings proven once), a row
  loop of `_mm_loadu_si128` loads and `_mm_stream_si128` stores (a 16-byte aligned destination: the
  pitch is a multiple of 256 and the view starts on a page), `sfence`, unmap, `end_lease`, then
  `present_within` (the same mint and `ScanoutFlip` as user mode's `present`, with a **1 s** bound
  instead of 5 s because the HPD worker flips every frame and StopDevice joins it; it sends direct and
  never queues behind fenced presents, 13.12).
* **Source eligibility** (`source_layout`, from one coherent read of `active_scanout_resource` and the
  `primary_scanout_*` seqlock): something is bound and published, it is the same resource, the extent
  is the ring's, the pitch holds a row, the rows fit the published allocation size.
* **Order in the worker pass**: `rm_client::service` runs after the deferred VidPn programming and
  before the scanout refresh arm, and calls the presenter **twice**: before the client's steps (so a
  ring about to be torn down is withdrawn from scanout before `GemClose` runs under it) and after
  them (so a ring completed this pass is used in it); the second call is skipped while a wake the
  first one asked for (a paced frame, the pause after a failure) is still in the future. A bind to a direct resource in this very pass
  is seen before the refresh arm, so its flush is not withheld.

### 13.4 Failure and fallback

| where | failure | effect | fallback |
|---|---|---|---|
| bring-up, surface, any slot's surface path | as slice 1 (section 7) | client dead for the generation, every slot's files closed | Venus |
| a slot's view (map channel, `RM_MAP_MEMORY`, host `Mmap`, `MmMapIoSpace`) | any | that slot unwound, ring not presentable, client lives | Venus (nothing is registered) |
| registration refused | arbiter | counted as a failure; the next attempt waits 100 ms | Venus |
| source not eligible (`RmSrcWhy`) | a UMD image bound, wrong extent, bad pitch | resident source withdrawn / never registered | Venus |
| source blob cannot be mapped / planned | map or plan failure | the frame is skipped (`RmSrcBad`), counts as a failure | after three, Venus |
| flip refused or timed out (1 s) | host / transport | the frame is skipped (`RmFlipFail`), counts as a failure; the next attempt waits 100 ms | after three (so no sooner than 200 ms later), Venus |
| flip finds the source yielded | a user source took scanout 0 | the frame stays owed; eight in a row count as a failure (a registration that never gets to show) | the user source's end resumes |
| the arbiter ended our registration | file closed, epoch or generation invalid | counted as a failure, re-register after 100 ms | Venus meanwhile (the desktop was owed a flush) |
| mode change | extent differs | withdrawn, ring torn down and rebuilt, registered again | Venus meanwhile |
| StopDevice while the presenter or a step is in a host call | every call of the worker is bounded: a step's allowance is `TIMEOUT_MS` = 2.5 s in all (the foreign import's create + attach + undo, the release's unmap + detach + unref share it), the host `Mmap` 2.5 s, its undo 2.5 s, the source map 1 s, the flip 1 s; `cleanup` skips the blob release once `hpd_stop` is set and otherwise spends at most one step's allowance on it | the worker returns inside the join (5 s, twice); what was not closed stays in the tables and the transport sweep closes it |
| StopDevice / retire | `retire_begin` | views taken, then **wait (2 s) for a frame copy's lease**, then unmapped; `forget` clears the presenter | the transport reset ends the resident source |
| worker stopping | `hpd_stop` | nothing new is started | n/a |

### 13.5 Locking, IRQL, lifetime

* `PRESENTER` is a leaf spinlock over plain data, never held across I/O or another lock; `CLIENT`
  stays a leaf; the frame copy holds **no** lock, only a lease flag set under `CLIENT`'s lock and
  waited for by `retire_begin` / `cleanup` before they unmap (`RmLeaseTmo` counts a timeout: it must
  stay 0, the worker is joined first in every normal path). `STATE` (the arbiter) is a leaf taken at
  most once at a time; `with_virtio` is never called under it.
* The edges (`note_frame_edge`, `note_resume_edge`, `note_desktop_changed`) are atomics and
  `KeSetEvent(Wait = FALSE)`: legal wherever the restore hooks run. Everything else runs on the HPD
  worker at PASSIVE: registry mirrors, `map_blob_prepare` (a host round trip when the blob is not
  mapped yet, no lock held), `MmMapIoSpace`, the flip.
* The source blob can be released by a DestroyAllocation racing the copy (the content mutex does not
  cover the unref); the dedicated LINEAR image is adapter-owned and goes only with the adapter, and
  `resource_is_live` is checked first. The residual window yields one wrong frame, never a fault: a
  read of an unmapped window range is the host's unassigned MMIO.
* **Knob at 0**: `service` returns on its one atomic load before touching any of this. The shared
  paths changed are `foreign_scanout_restore_desktop` (one more leaf-lock `take_resume_owed`, false),
  the gate (one more relaxed load of the knob level when a *user* source is live), the worker wait
  (a clock read only when a source or a paced frame exists), `release_owner` / `release_handle` (one
  lock hold instead of one lock call, the same counters while no resident source exists),
  `forget` (a leaf lock and four atomic stores per retire) and the flip path (`present_within` is
  a new function; `present` keeps its constant and, since the fenced-present merge, its queue check).

### 13.6 Counters (`Rm*`, REG_DWORD, written once the presenter has done something)

| value | what | healthy at level 3 |
|---|---|---|
| `RmPStage` | stage started last: 1 register, 2 withdraw, 3 map source, 4 copy, 5 flip, 6 re-flip | any; a hang names itself |
| `RmPres` | bit 0 registered, bit 1 gave up, bits 8..15 consecutive failures, bits 16.. front surface + 1 | `0x00010001` or `0x00020001` once showing |
| `RmRegs` / `RmWithdrawn` / `RmResEnd` | registrations the arbiter took / withdrawals / times the arbiter ended it | 1 / 0 / 0 per generation, +1 / +1 per mode change |
| `RmGaveUp` | times the presenter gave up | **0** |
| `RmFrames` / `RmReflips` | frames copied and flipped / surfaces re-flipped for a resume | grows with desktop activity / +1 per user source that ended quietly |
| `RmYielded` / `RmPreempted` / `RmResumes` | flips that found the source yielded / user sources that preempted the KMD's / resume edges answered | small |
| `RmFlipFail` / `RmSrcBad` | flips refused or timed out / source maps or plans that failed | **0** |
| `RmCopyMs` / `RmCopyMaxMs` / `RmCopyMB` | last and longest frame copy (ms), megabytes copied in all | the numbers section 12.4 item 4 asks for |
| `RmSrcWhy` | why the primary is not eligible: 0 accepted, 1 nothing bound or published, 2 not the published primary, 3 extent differs, 4 bad pitch, 5 rows exceed the allocation (also written once at every change) | 0 |
| `RmReg` | event record: 1 per accepted registration, 0 per refused one | 1 |
| `RmLeaseTmo` | retire stopped waiting for a frame copy | **0** |

`FsSupp` (withheld flushes) and `FsPres` (flips, the resident source's included) keep counting; the
user-source invariant `FsSet - FsRel - FsLapse - FsEnd - FsTake` stays 0 or 1 because the resident
source's ends are counted in `RmResEnd`, not `FsEnd`.

### 13.7 Verified here, and not

Verified on the host (620 tests in `kmd_logic`): the resident source's every transition (foreground,
preempt, every way a user source ends, the resident one's own ends, parked-file closure, withdraw,
re-registration keeping the generation, a resume cancelled by another user source, reset, `seq`);
the ring's build order, whole teardown on a new extent, the failure of the second slot closing the
first one's files, a given-up view, `take_views`, the exact status word; the presenter's pacing at
60 Hz under a 240 Hz edge stream, yield, resume (copy after a busy tenure, re-flip after a quiet
one), failure counting and giving up, losing the source, the arbiter ending it, the copy plan's
bounds and its execution on buffers; a co-simulation of presenter and arbiter over a whole session.
Type-checked against the generated harness (13 header): the I/O files, the merged adapter and virtio
flip files, the changed `ctrl.rs` functions. **Not verified by anything**: that
`kmd_render` compiles; every host reaction (the ring surfaces' second GEM import in the same DRM
file, flips of alternating GEMs, the viewer's presentation of them, dropped frames under a full
socket); the primary blob's cache attribute and so the copy's speed; the picture on screen; IRQL
behaviour; the retire ordering with a live ring; the lease wait.

### 13.8 What was not decided blind, and where it stops

* **The VidPn primary allocation itself is still a Venus blob**, so Venus is still in the frame
  path (DWM's frames, the GPU copy into the dedicated image, the blob GDI writes). Section 12 says
  why, and what replaces it (the pool of 12.3 (c)); this slice makes the scanout half of S6 real and
  reversible and proves the priority/restore semantics that the NVK sources need regardless.
* **Direct primaries are not covered** (`primary_scanout_*` is not published for them), so a
  fullscreen DWM image keeps Venus' flush; extending `source_layout` to them needs their layout
  published the way the dedicated image's is.

### 13.9 Hardware checklist, in order

Stop at the first step that fails; each says what to read. Same registry key as section 9.

1. **Knob absent.** Nothing changes; no `Rm*` value exists; `tools/kmd-frame-sizes.ps1` passes (no init
   work was added). Then re-run section 9 steps 3 and 7 at levels 1 and 2: `RmStatus` still
   `0x10b05000` / `0x10b05503`.
2. **Prerequisites**: section 9 step 2, a viewer connected, DWM running (the dedicated LINEAR image
   exists only once DWM frames have been copied into it).
3. **`KmdRmClient=3`, restart the device.** `RmBringUp=1`, `RmStatus=0x11b05500`, `RmSteps=32`,
   `RmSurf=2`, `RmGem=2`, `RmView=2`, `RmDead=0`, `RmFail=0`. If `RmView<2` with `RmDead=0` the
   RM window cannot map vidmem here (the answer to the probe's question in 9.7); read `RmStep`.
4. **The presenter engages.** `RmSrcWhy=0`, `RmRegs=1`, `RmPres` bit 0, `RmFrames` grows as the desktop
   changes, `FsSupp` grows, the desktop is visible and updates; `RmSrcBad=0`, `RmFlipFail=0`,
   `RmGaveUp=0`. If the screen is black while `RmFrames` grows: compare with the level 2 picture
   (section 9 step 7: a probe that shows means the flip path is right and the *content* is wrong:
   `RmCopyMs`, the source's cache type, `RmSrcWhy`); a probe that does not show is a host problem.
5. **Record the numbers**: `RmCopyMs`, `RmCopyMaxMs` and frames per second at 1920x1080, then at
   5120x1440@240. These decide the next step in 13.10.
6. **A user source preempts and yields back.** Run `crm_scanout_smoke` / an NVK scanout app: `RmPreempted`
   +1, the app's frames show, `RmFrames` stops growing; when it releases or exits: `RmResumes` +1, the
   desktop is back through the ring (`RmReflips` +1 after a quiet tenure, `RmFrames` +1 if the desktop
   changed), `FsRest` unchanged (no Venus flush), `RmWithdrawn` unchanged. Kill the app: same
   (`close_all_for_owner`). Let it go silent: back after the lapse (`FsLapse` +1).
7. **Mode change.** `RmWithdrawn` +1, `RmSurfFree=2`, `RmSurf=4`, `RmRegs` +1; no frame at a wrong
   extent (no shear).
8. **Device restart with the desktop running.** No bugcheck; `RmViewFree` equals `RmView`;
   `RmLeaseTmo=0`; `RmBringUp` 2 on the next generation.
9. **Soak**: ten minutes of window dragging: `RmFlipFail=0`, `RmSrcBad=0`, `RmGaveUp=0`, the user-source
   invariant of 13.6 is 0.
10. **Experiments that decide section 12**: the flip target in system memory (`ATTR_LOCATION`, class) to
    answer 12.3 (a); the copy with the source's cache attribute noted.

### 13.10 What comes next

1. Measure (step 5). 2. If the copy is the limit: band diffing against a guest-RAM mirror (copy only
   changed row bands; `CopyPlan` already takes `y0..y1`), then the GPU copy engine through an RM
   channel. 3. Direct primaries as sources. 4. The pool of 12.3 (c): it removes the Venus blob from
   the allocation, makes the copy's source RM memory and lets `KmdStandardBuffer` / GDI surfaces be RM
   sub-ranges (slice 3). 5. Three surfaces, or the host forwarding `EV_RELEASE`, to end the reuse
   hazard of 12.4 item 5.

### 13.11 Open questions (new)

1. **The source's cache attribute.** The primary's blob is mapped with the host's `MAP_INFO`; if that
   is write-combined the copy reads at ~200 MB/s and level 3 is a demo. Fix would be a cached host
   memory type for the dedicated LINEAR image (Venus side), or reading it through the pool of (c).
2. **Alternating two GEMs of one DRM file as flip sources**: the backend caches exports by
   `(owner_handle, host_handle)`, so it should be fine; the viewer's handling of an `ATTACH` of one of
   two alternating dma-bufs at 60 Hz is unverified.
3. **Tearing without a release.** If the viewer samples the previous buffer more than one flip
   interval later, a frame tears. Fixed where the host offers `NVGPU_F_SCANOUT_RELEASE` (the
   presenter waits for the surface's release, 500 ms at most); three slots also make it rarer.
4. **No lapse for the resident source.** A wedged HPD worker freezes the screen on the last frame; it
   also serves HPD and the refresh, so nothing else would work either, but the user-source lapse
   cannot rescue it. The worker's own bounded waits are the protection.
5. **NVKMS and system memory** (12.3 (a)), and whether any 5120x1440 sysmem flip is worth having.

### 13.12 The merged state machine: user sources with queued fenced presents, and the resident source

Two features were built apart and meet here. **Fenced presents** (`rm-fence-marker.md`): a user
`SCANOUT_PRESENT` with `RM_FENCE` returns at once; its flip waits in a per-source FIFO
(`ScanoutQueue`, 8 deep) until its fence fires, is sent from the HPD worker (or an escape thread
that finds nothing ahead), and its fence handle, which the KMD took over (owner `KMD_RM`,
`Attach::Scanout`), is closed exactly once. **The resident source** (this document): the KMD's own
ring, flipped by the presenter through the same `ScanoutFlip` machinery, preempted by any user
source and resumed when that one ends. This section is the contract between them. The pure
parts are pinned by `kmd_logic::rm_present::tests::combined` (a model that makes the same calls
the adapter makes, with a wire log and a fence close counter).

**Who runs what.**

| actor | thread / IRQL | touches |
|---|---|---|
| user `SET` / `PRESENT` / `RELEASE`, `Close` | the app's escape, PASSIVE | `STATE` (arbiter), `FENCES` (queue), the pump |
| fence fired | DPC, DISPATCH | the fence table (`fence_note_fired`), wakes the worker |
| lapse poll, the pump (`foreign_fence_service`), the presenter (`rm_client::service`) | HPD worker, PASSIVE, in that order | all of it |
| the refresh gate (`foreign_scanout_suppresses`) | PASSIVE under `scanout_mutex` | `STATE`, raises `FRAME_EDGE` |

Locks: `STATE` and `FENCES` and `PRESENTER` and `CLIENT` are leaves; `virtio_lock` -> `FENCES`;
`STATE` is never taken under `FENCES` or `virtio_lock`; the presenter reads `STATE` before it takes
`PRESENTER` and never holds both.

**The state.** The arbiter's `State` (`Inactive`, `Active(source)`, `ReleasePending`) plus the
resident registration (none, parked, or the foreground source itself) and `resume_owed`; the queue
(entries carry the generation and epoch of the source that minted them); the presenter
(`registered`, owed frame, owed resume, the front slot, the failure count and the pause).

```text
                    resident_set (ring ready, nothing live)             user SET (Preempted)
  Inactive ─────────────────────────────────────► Active(resident) ───────────────────────► Active(user)
     ▲ ▲   user SET (Activated)                    ▲        │                                  │  │
     │ └───────────────────────────────────────────┼────────┼──────────────────────────────────┘  │
     │                                              │        │ withdraw, DRM file closed,          │
     │                                              │        │ invalid generation/epoch            │
     │      desktop_restored (one Venus flush)      │        ▼                                     │
  ReleasePending ◄──────────────────────────────────┼── (resident forgotten)                       │
     ▲                                              │                                              │
     │  user source ends and NO resident registered │   user source ends (release, lapse, file    │
     └──────────────────────────────────────────────┼── closed, owner exit, invalid handle):       │
                                                    │   resident takes the screen back,           │
                                                    └── resume_owed = true ◄──────────────────────┘
  any state ── transport reset ──► Inactive (resident forgotten, queue cleared, no restore owed)
```

**What each event does** (`restore` = `foreign_scanout_restore_desktop`: it first asks
`take_resume_owed()`; true raises `RESUME_EDGE` and the presenter re-flips, false asks for one
Venus desktop refresh; both signal the worker).

| event | arbiter | queue and fences | screen |
|---|---|---|---|
| user `SET`, resident foreground | `Preempted`, new generation, `resume_owed` cleared; the resident registration stays, parked | nothing queued yet | the presenter's next decision is "not foreground": idle, the pause of a past failure is voided; frames stay owed |
| user `SET`, another user live and within its lapse | `Busy`, nothing changes | | |
| user `SET`, another user lapsed | `TookOver`, new generation | the old source's entries are dropped by generation at the next drain, each fence closed once | |
| user `PRESENT` (unfenced), queue empty and pump idle | mint (seq), send, `flip_done` (lapse runs from acceptance) | | the flip |
| user `PRESENT` unfenced, queue busy | mint, enqueue as a ready entry (fence 0) so flips keep their order | | later |
| user `PRESENT` with fence | mint, `fence_attach` (re-tag to `KMD_RM`, counted against `MAX_NVRM_ATTACHED_FENCES` = 512, not the owner's 128), enqueue, pump | the flip is sent when the fence fires; the fence is closed when its entry leaves the queue (sent, superseded by a newer ready one, or dropped) | |
| fence fires | | the worker's next pass drains in order: the leading run of ready entries sends only the newest | |
| **release** (`SCANOUT_RELEASE`) | `foreground_ended`: resident registered -> `Active(resident)` + `resume_owed`; none -> `ReleasePending` | the worker's next pass drains with the live generation = the resident's (or none): every entry of the ended source is dropped and its fence closed | re-flip of the front surface, or a copy if the desktop changed during the tenure (`note_desktop_changed` flagged it); or one Venus flush when nothing is resident |
| **lapse** (`poll` on the worker, or a refused `PRESENT`) | same as release | same. The deadline moves when the host *accepts* a flip (`flip_done`), not when one is minted or queued (`ForeignScanout::present` does not extend it), so a source whose fences are slower than its lapse (2 s by default) is ended while its flips wait, and its queue is dropped | same |
| **file closed** (`release_handle` from a forwarded `Close`) | same | same; the fence handles are separate handles and are closed by the queue's drop, not by the file's `Close` (they are `KMD_RM`'s now) | same |
| **owner exit** (`release_owner` from DestroyDevice / StopDevice) | same | same; `close_all_for_owner` never sees an attached fence (re-tagged) | same |
| invalid handle or epoch (the gate's `invalidate`) | same | same | same |
| **transport reset** (`foreign_scanout_reset`, with the display publication state) | `Inactive`, resident forgotten, `resume_owed` cleared, **no restore**: the display state is rebuilt from scratch | `FENCES.clear()` (counted `FsFDrop`); the entries' fence handles are still in the NVRM table and the transport sweep (`close_all_on_host`) takes each out and closes it: once | the next generation registers the presenter again from cold |
| resident ends (withdraw, DRM file closed under it, invalid generation) | resident forgotten, `ReleasePending`, one Venus flush owed | n/a (the resident never attaches a fence) | Venus |
| presenter: frame due and foreground | `present_within(KMD_RM, drm, gem)`: mint, **send direct** (never queued), `flip_done` | | the copy / re-flip |
| presenter: flip finds the source yielded (`NoSource`) | | | not a failure; the frame stays owed |

**Why the resident's flips never queue.** The queue holds only user sources' entries (and the
level-2 probe's), and the pump drops every entry whose generation is not the live one, so while the
resident is the foreground source the queue holds nothing it must stay behind (stale entries are
dropped, with their fences closed, by the worker pass that precedes the presenter's in the same loop
iteration). `present` (user and probe) queues behind a busy queue to keep order; `present_within` does
not, and sends with the caller's bound (1 s for the presenter).

**Ordering inside one worker pass** (`ddi/hpd.rs`): the lapse poll, then the fenced queue (drop
stale, send ready, close owed), then the VidPn deferral, then `rm_client::service` (presenter, client
steps, presenter again). So a source that ended is dealt with (its queue dropped, its fences closed)
before the presenter's first act of the pass, and an end that raised a resume edge is answered in the
pass that sees it.

**The two races, and why they end right.**

1. *A flip taken off the queue just as its source ends.* The pump drained a ready entry (its fence is
   already closed) on an escape thread or the worker; the source ends, the resident takes the screen
   back and the presenter re-flips; then the late user flip is accepted by the host, after it. The
   host takes frames in arrival order, so the user's frame is on screen. `flip_done` finds the
   flip's generation no longer live and calls `restore`; `take_resume_owed` is already consumed, so
   it asks for a desktop refresh, whose gate finds the resident source live and answers with a
   frame edge: the presenter copies the newest content and flips it after the late one. The wire
   order is `[reflip, user (late), copy]`; Venus is never flushed. (`a_flip_taken_off_the_queue...`.)
2. *A resident flip in flight when a user source sets.* The resident's flip lands after the `SET`
   but before the user's first frame; `flip_done` finds it not live; `resume_owed` was cleared by the
   `SET`, so the refresh's gate sees a user source and only flags "desktop changed". The user's
   first frame replaces it; at the user's end the resume is a copy. Costs nothing but one
   superseded frame.

**Exactly-once close of a fence handle.** Producers of a close: the queue's `drain` (sent,
superseded and dropped entries; `Drain::closes()` names each entry's fence once, and the entry
leaves the queue in the same call) and the transport sweep (every handle still in the table). A drain's
close goes through `fence_want_close`, which acts only on a fence that is `KMD_RM`-owned and attached
and sets `close_wanted` once (`FenceMeta::want_close` returns false the second time); the pop for
the host `Close` removes the table entry first, so a sweep afterwards finds nothing and a `Close`
of a recycled number is never sent from a stale queue entry. Reset clears the queue without closes
and leaves the handles to the sweep. A close the host refused is put back as a `Discard` fence the
sweep closes (no retry loop). None of the resident's code touches a fence, and the RM client's own
cleanup closes only the handles *it* opened (a list), never "everything of `KMD_RM`".

**Handle accounting with both on `KMD_RM`.** The owner token is shared, the budgets are not: the RM
client's opens (control, GPU, DRM, export and map channels: a handful) count against the owner's 128
(`MAX_NVRM_HANDLES_PER_OWNER`), which `reserve_nvrm_handle_slot` computes over the owner's entries
**excluding** attached fences; attached or discarded fences count against `MAX_NVRM_ATTACHED_FENCES`
(512, KMD-wide) and a refused attach is `Full`, never a refusal of the client's `Open`. Both draw on
the 1024-entry table; the only way for the client to be refused is that table being full (documented:
`NoResources`, client dead, Venus). Pinned by the harness tests
the S4 review harness (the real `nvrm_tables.rs` and `rm_gates.rs` compiled against stubs, out of
tree because `kmd_render` cannot host tests), re-run on the merged tree with two tests added
for this merge: `the_rm_clients_cap_and_the_attached_fence_cap_are_independent` (the client at its
128 stays at 128 with 512 user fences attached, and closing one of either frees exactly one
slot of that kind) and `no_sweep_by_owner_reaches_a_fence_the_kmd_took_and_the_sweep_of_all_closes_it_once`.
The foreign-resource table (level 4) is a different table with its own per-owner quota (64
resources) and is not touched by fences.

**Counters.** `FsSet - FsRel - FsLapse - FsEnd - FsTake` is the number of *user* sources live (0 or
1): the resident source is set by `resident_set` (not counted in `FsSet`) and every end of it,
including a `GemClose` of its surface through `foreign_scanout_release`, counts in `RmResEnd`, not
`FsRel` / `FsEnd`. Waiting fenced presents = `FsFQue - FsFSent - FsFSkip - FsFDrop` returns to 0 when
idle; a reset adds the cleared entries to `FsFDrop`.

**Failure pauses.** A registration, a copy or a flip that fails sets the presenter's own pause (100 ms,
`RETRY_AFTER_FAIL_100NS`): `decide` answers `WaitUntil` for an owed frame or resume until it is over,
whichever pass or `service` call asks, so the three strikes that end the presenter for a generation
are always at least 100 ms apart (they could all fall in one worker pass before). A flip that only
found the source yielded is not a failure and is not delayed; a user source on screen voids the pause
(the resume after it starts fresh, the strikes stay counted). The worker's wake for the retry is
`WAKE_AT`; the pass's second presenter call leaves the pass alone while that wake is in the future.

**Not covered by anything but the model.** The adapter's wiring of these calls was type-checked and
read, not run; no hardware has seen a user source with fenced presents over a running ring.

## 14. The KMD's own RM allocations as foreign resources (`KmdRmClient` = 4, and the hooks for the rest)

Requirement (the S6 change list): DWM on NVK must open the KMD's own RM allocations the way it opens
any NVK surface, so each KMD-owned RM allocation needs a **foreign resid**: the KMD is the creator,
exports its RM memory to a GEM handle on its own DRI file (this client already does, section 4),
creates the resource with `RESOURCE_CREATE_BLOB` / `RM_EXPORT` through the `IMPORT_RM` machinery,
records the layout in the foreign table, and the WDDM allocation that backs it **adopts** the resid,
so `OpenAllocation` yields the FOREIGN identity and the layout trailer (`shared-foreign-surfaces.md`
section 2). Present through `ScanoutFlip` from the foreign record is the separate Option B lane: only
its hook points are designed here (14.3); `display.rs` is not touched.

### 14.1 Built: the KMD as the creator of a foreign resource (level 4)

* **`virtio/rm_foreign.rs`** (new): `import_surface` is `foreign::import_rm` for the KMD's own owner.
  Same sequence (gate `rm_import_served`, `validate_request` with a mandatory layout, reserve under one
  lock hold, `alloc_blob_errno` with `HELIOS_BLOB_MEM_RM_EXPORT` and `blob_id = (drm << 32) | gem`,
  commit under one lock hold, with the stale-handle rule), with three differences: the **owner** is
  `DeviceOwner::KMD_RM` (the client's DRI file is recorded under it), the **holder context** is the KMD's
  own Venus context (`adapter.venus_ctx_id()`, not device-owned, so `resolve_owned_ctx` cannot name it:
  `foreign_begin_kmd_import` compares it with the transport generation's instead), and the **layout** is
  the surface's (`surface_foreign_layout`: `XRGB8888`, `MOD_LINEAR`, the RM pitch, offset 0). The KMD's quota is
  its own (the table counts per owner token: 64 resources, 4 GiB). `release_surface` / `release_all` undo it
  through `release_blob_for_owner(KMD_RM, ...)`; a resource a WDDM allocation adopted since is no longer
  the client's (slot owner `None`), so that release is a no-op and the allocation's destroy releases it.
* **Adoption of a KMD-created resource** (`virtio/gpu/foreign_tables.rs::adopt_for_allocation`): the
  creator check used `DeviceOwner::new(creator)`, which refuses the KMD's token on purpose. A creator that
  is `KMD_RM` now proves "holder context" as "the record's context is nonzero" (the import proved it is the
  KMD's own Venus context, and the record dies with the transport generation) and "slot" as "the blob slot
  is `KMD_RM`'s"; the pure table already treated an owner as an opaque token (a new test pins that
  adoption frees the KMD's quota and keeps the record).
* **Client machine** (`kmd_logic::rm_client`): level 4 = level 3 plus, per ring surface, `Step::ForeignImport`
  (right after `CloseExportCh`: the surface is `Ready`, before its view) and, on teardown,
  `Step::ForeignRelease` **before** `GemClose` / `FreeMemory` (the resource names the GEM). A refused import
  is given up for that surface (`share_failed`, never retried, nothing to undo); a failed release is counted
  (`RmSoft`) and advances; the ring is complete (`Park`) only once each surface's import is settled. A death
  reclaims what was made (`release_all`); the transport sweep reclaims the rest. The slot's resid is in
  `SlotInfo::foreign` (and the status word, bits 16 and 17: working and parked slot have one).
* **Why level 4 exists at all**: the ring surfaces are not WDDM allocations, so nothing adopts them; the level
  proves the KMD-owned import end to end on hardware (`RmFgImp`, a resource in the host, the layout in the
  record, the same teardown) and gives the allocation hooks below a tested producer.

Counters: `RmFgImp` / `RmFgRel` / `RmFgErr` (imports made / released / failed), `RmFgErrno` (the last host
errno), `RmFgWhy` (the last failure: 1 gate closed, 2 no KMD Venus context, 3 bad request or not owned, 4 table
or quota, 5 host refused, 6 transport, 7 recorded nothing or raced). The foreign table's own `Fg*`
counters count these resources too (`FgImp`, `FgLive`, `FgRel`).

### 14.2 Not built: the allocation arms (what needs deciding or building, in order)

(Level 5, section 15, builds the primary's arm from system memory and Option B for it; the rest of this list still holds for the other kinds.)

None of the KMD's allocations is RM-backed today (section 12: the CPU-written ones cannot be, until the pool
of 12.3 (c); the GPU-only ones need an RM surface made on demand). The foreign resid is the easy half; the
hooks are:

1. **A surface service.** `CreateAllocation` runs at PASSIVE on the caller's thread, but the RM client's state
   has exactly one mutator, the HPD worker (section 6). A request mailbox (a small static array of tickets
   guarded by the same leaf-lock rule, an event, a bounded wait of 2.5 s like every message of the client)
   lets `create_one` ask the worker for a surface of `(kind, w, h)` and get back `(resid, layout)`; the
   client grows from one working slot to a table of surfaces (the doc's slice 2 item: free list, quotas from
   "one" to a table). The `Step` sequence per surface is the one already tested (`AllocMemory` .. `ForeignImport`);
   `Park`/`Unpark` are the table's moves.
2. **Which allocations.** `KmdLinearPrimary` (pitch-linear vidmem, the memory of this client's surfaces:
   done as a surface, not as an allocation), `KmdOptimalGdiTexture` (GPU-only: pitch-linear first, with
   `MOD_LINEAR` recorded; a block-linear allocation needs the RM kind/attr values nvk-rm uses and the
   modifier `MOD_NVIDIA_BLOCK_LINEAR_BASE | h` in the layout, both **unverified blind**). `KmdStandardBuffer`
   and every other CPU-written allocation stay out until the pool (12.3 (c)); a shadow/staging allocation is
   a sub-range of it, not a surface.
3. **The arm.** A new `Backing` arm (or a branch in the three KMD arms behind the knob, Venus the fallback
   on any failure) that obtains the resid and returns `CreatedBacking { resource_id, foreign:
   ForeignBacking::Adopted(layout), blob_size: NonHostAuthoritative(size), pitch: layout.stride, .. }` after
   `adopt_for_allocation(resource_id, &AdoptRequest { declares_foreign: true, take_ownership: true, ctx_id:
   <the KMD's context>, width, height, pitch, plane_offset: 0, supplied_layout: Some(layout), trailer_room })`
   (the same call `AdoptedUmdResource` makes; the KMD fills the private data a UMD would). The layout trailer
   is written by the existing `write_foreign_layout_trailer`. `HeliosWddmAllocMeta` carries `width`, `height`,
   `pitch` as for any foreign allocation; no new field.
4. **After adoption** everything is the S6 route unchanged: `OpenAllocation` rewrites the identity (FOREIGN
   flag) and the trailer, opens are counted per process, the host resource lives until the last of destroy and
   closes, DWM-on-NVK imports with `RM_RESOURCE_IMPORT` (route R). The KMD must not release an adopted
   resource from the client (14.1: the release no-ops) and the allocation's destroy must be what frees the
   surface: the client's `FreeMemory` / `GemClose` then run when the table entry is retired, which has to wait
   for the destroy (a reference count on the table entry; the host import holds its own reference to the
   memory, so freeing the RM memory first is safe for the host but not for a consumer's RM import of it).

### 14.3 Option B, the present hook points (design; built for the primary by level 5, section 15.7)

(Built since, for the KMD's own sysmem primary by level 5 (15.7) and for ANY adopted foreign resource a user device
imported by `ForeignFlip` (15.18).) Nothing in `display.rs` or `adapter/scanout.rs` was edited when this was designed.
Where the flip lane plugs in:

| where | today | Option B |
|---|---|---|
| `create_allocation::scanout_alloc_info` | returns the resource id, extent, primary address, `direct_scanout` | also whether `foreign_record(resource_id)` is `Some`, with its creator (`KMD_RM` or a device), `rm_handle`, `gem_handle`, layout and size (the record keeps all of them; `foreign_layout` / `foreign_record` already exist for the copy import) |
| `program_vidpn_source_inner`, the `target` match (`direct_scanout` / `production_linear_scanout`) | builds a `ScanoutTarget` the host `SET_SCANOUT_BLOB`s | a third arm for a foreign source: no `ScanoutTarget` and no bind; take the foreign source path instead of `set_scanout_blob` + the `remember_scanout_blob` bookkeeping |
| the flip itself | `SET_SCANOUT_BLOB` then (worker refresh arm) `RESOURCE_FLUSH` | `virtio::foreign_scanout::present_within` with the record's `(owner, handle = rm_handle, gem)`: the arbiter's resident source **updates in place** per flipped allocation (`resident_set` with the allocation's handle and layout keeps its generation), so the desktop suppression, user preemption and the resume rule of section 13 apply unchanged. A record created by a user device needs its owner token (the creator may be gone: then the source is refused and Venus keeps the screen) |
| completion and reuse | `ScanoutFlushToken`, the read ledger per flushed resid, retired by the flush's completion | the host flip has no completion: a ring depth, or the host forwarding `EV_RELEASE`, feeds the ledger (a flip stands in for "the previous buffer is free after the next one is accepted"); until then the conservative rule of 12.4 item 5 |
| `queue_active_scanout_refresh_locked` | the gate withholds the flush for a live source | unchanged: Option B sources are sources |

Decisions Option B needs from the host before it can be written: a release/completion signal (or a depth to
assume), and whether a flip of the same GEM at the display rate is acceptable to the viewer (the same
question as 13.11 item 2).

### 14.4 Hardware checklist for level 4, in order

1. Section 13.9 steps 1 to 5 at level 3 first. 2. **`KmdRmClient=4`, restart the device.** `RmFgImp=2`
   (one per ring surface), `RmFgErr=0`, `RmStatus=0x11b35500`; `RmSteps` 34 (two imports); host log: two
   `RESOURCE_CREATE_BLOB` of blob_mem `0x80000001` for the KMD's DRI handle; `FgImp=2`, `FgLive=2`. If
   `RmFgErr` is nonzero `RmFgWhy` says where (1 gate: the host does not serve the import, config bits 13 and 10;
   5 the host refused: `RmFgErrno`). The desktop is as at level 3 (the import changes nothing on screen).
3. **Mode change**: `RmFgRel=2`, then `RmFgImp=4` after the ring is rebuilt; no `RmSoft`. 4. **Stop and start**:
   `FgLive` returns to 0, `RmFgRel` counts the reclaim, no bugcheck. 5. With a user-mode NVK process open, run
   `RM_RESOURCE_IMPORT` on a resid the KMD made (the open row rule refuses it: nothing adopted it, so
   `FgRiRef` counts it: the expected answer until 14.2 exists).
### 14.5 Bounds

`import_surface` and `release_surface` run on the HPD worker, which StopDevice joins for a bounded
time (section 6), so each is allowed `TIMEOUT_MS` (2.5 s) **in all**: the create, the attach and the
unref of a failed attach (or the unmap, detach and unref of a release) draw on one `SweepBudget`, each
command waiting at most what is left, and with it spent nothing more is sent (`Timeout`; the
transport's sweep reclaims the host side). `release_all` is the same, and is skipped once the worker
is stopping. The allocation arms (14.2) run on the creator's thread, not the worker. Not covered by
the bound: the host's own answer to a command that did arrive late (a created resource whose reply was
lost): it is reclaimed with the context it was created in.

## 15. The primary from RM system memory (`KmdRmClient` = 5, design "A'")

Status: written on `kmd/rm-sysmem-primary` against the v314 line (`70b79dd`), merged onto the v315 line
(`50d0698`: the flush gate `HEFL`, the scanout release event with its release book, `SCANOUT_STATUS` and
the ring waits, S4 fences, S6 sharing, `RM_RESOURCE_IMPORT`, the RM client levels 3 and 4) on
`kmd/rm-sysmem-primary-merged`. Three things the merge settled: **the cache default is write-combined
memory with no alias** (15.5), the level 5 flip is the level 3 presenter with a ring of one and **never
waits for a release** while the **close of a replaced primary's GEM does** (15.7), and the composition with
user sources, the S4 queue and levels 3 and 4 is written down as a state machine (15.15). **Never built,
never run**; the pure logic is host-tested (`kmd_logic::rm_sysmem`, 30 tests, plus 2 in
`foreign_resource`), the I/O files were type-checked against a harness generated from the real module
declarations (15.12). Host side: `feat/rm-export-map-blob` (`1ac5414`), unit-tested, not yet run in a guest.

The idea. Every earlier level left the VidPn primary a Venus blob (host-allocated memory the viewer reads
through a GPU copy into the adapter's LINEAR image) and put RM in front of scanout only. Level 5 makes the
primary itself RM memory: `NV01_MEMORY_SYSTEM` allocated by the KMD's own RM client, exported to a GEM,
created as a foreign (RM-export) Venus resource, adopted by the WDDM allocation, mapped by the host into the
window dxgkrnl's CPU aperture uses (so GDI writes, the paging copies and the GDI executor reach it exactly as
they reach a Venus blob), and shown by the KMD's own `ScanoutFlip`. The display engine cannot scan out system
memory, so the viewer's compositor SAMPLES it: one GPU copy there, no CPU copy here. "Just a blob created by
RM instead of Venus"; nothing else of the aperture / paging / GDI path changes.

| `KmdRmClient` | does |
|---|---|
| 0-4 | unchanged (sections 1, 13, 14); the ring client and presenter run at 3 and 4 only |
| **5** | no ring client, no presenter. The primary (`KmdLinearPrimary`) is allocated from RM system memory when it can be, from Venus when it cannot; an RM primary is flipped as it is |

Sub-knob `KmdRmSysCache` (read at the service's bring-up; 15.5): **0 or absent: write-combined memory, no
alias (the default)**; 1 the same, spelled out; 2 cached memory plus the `Cached` flag on the primary (opt-in
experiment); 3 cached memory under dxgkrnl's write-combined view, a write-back / write-combined alias
(opt-in). Any other value is the default: an unknown value never picks an alias. Values of `KmdRmClient`
above 5 count as 5. With the knob below 5 the primary's creation, its flip and its teardown are the v315
paths (15.15 lists every hook and what it does with the knob off).

**A stale value of 5 or more selects level 5 now.** Before level 5 existed the driver clamped the knob to
level 4, so a box that once had `KmdRmClient` set to 5, 9 or 99 (a typo, an experiment, a key left from a
test) ran level 4. On this build the same key switches the box's desktop primary to RM system memory and
its flip. Look at the key before the first run on any box that has been used for RM experiments (hardware
checklist step 0, 15.13).

### 15.1 The host contract this is written against (`feat/rm-export-map-blob`)

* **Create**: `RESOURCE_CREATE_BLOB`, `blob_mem` `0x80000001` (RM_EXPORT), `blob_flags` `USE_MAPPABLE`. The
  KMD's own creation sends it (`sysmem::import_resource`); the user-mode `IMPORT_RM` ABI sends 0 and cannot ask
  for it. The backend answers `EOPNOTSUPP` for `USE_MAPPABLE` on vidmem or on memory it did not see allocated.
* **What decides "system"**: only what RM writes back at allocation (`attr` `0x2a800000` cached sysmem,
  `0x4a800000` write-combined sysmem, `0x11000000` vidmem), tracked through export and GEM import. So the
  allocation is `NV_ESC_RM_ALLOC` (NVOS64) of class `NV01_MEMORY_SYSTEM` (0x3e), as `crm_alloc` sends it, never
  `VID_HEAP_CONTROL`, and the export and the GEM import go through the backend (`FORWARD`), as slice 1 does.
* **Map**: `RESOURCE_MAP_BLOB` at a page-aligned offset inside the window, length = size rounded to pages,
  placed exactly like a HOST3D blob (so `blob_map_begin` / `blob_remap_begin` apply). One mapping at a time per
  blob. `map_info`: CACHED for cached sysmem, WC for write-combined, UNCACHED otherwise.
* **Unmap** removes the guest view and the range reads ZEROS afterwards (QEMU swaps in fresh anonymous
  memory): never unmap while anything may still read it. UNREF unmaps first.
* **Coherence**: cached sysmem is GPU-snooped; the guest needs no cache maintenance, only to finish its writes
  before a flip (`sfence` for write-combined stores).
* **Measured by the host**: cached reads about 28 GB/s, write-combined reads about 75 MB/s, write-combined
  writes about 28 MB/s. Cached would be the fast choice and is an opt-in only: it puts a write-back alias
  next to dxgkrnl's write-combined view of the same pages (15.5).

### 15.2 Which allocations (decided: the primary only)

`rm_sysmem::route(level, kind)`; the table is a tested function:

| kind | at level 5 | why |
|---|---|---|
| `KmdLinearPrimary` | **RM system memory** | CPU-written by GDI, the memory the screen shows |
| `KmdStandardBuffer { primary: false }` (shadow, staging, GDI staging, the external Present buffer) | Venus (`PresentBuffer`) | not simple. DWM opens it as Venus memory by identity (`venus_memory_id`, `memory_type_index`), and the Present-buffer ABI (`allocate_present_buffer_blob`, `register_present_buffer` / `unregister_present_buffer`, `authorize_present_buffer_open`, the `PresentLinearBuffer` system-backing mirror) is Venus-specific. The only reason to move them is the host's cached map, which the primary needs more |
| `KmdStandardBuffer { primary: true }` | Venus (`OddPrimary`) | the ABI-defensive arm classification never produces |
| `KmdOptimalGdiTexture` | Venus (`NotCpuWritten`) | a tiled, GPU-only image; system memory is for CPU-written bytes. Vidmem (14.2) remains the answer for it |
| adopted UMD resources, raw blobs, tracking | Venus (`NotKmdOwned`) | not the KMD's |

Opening the standard buffers later is a change to `route` plus an arm that does the present-buffer
registration; the creation, the table, the map and the teardown are kind-agnostic (they take a layout).

### 15.3 Decision: create synchronously in `create_one`, not through a worker mailbox

`DxgkDdiCreateAllocation` runs at PASSIVE on the caller's thread. Chosen: **synchronous, on that thread.**

* The primary arm already creates its Venus blob synchronously there (`with_venus_client`, host round trips with
  a 30 s bound). An RM creation is about ten host messages (alloc, open the export file, export, GEM import,
  close it, create the resource and attach it, map, unmap) of a few milliseconds each, plus eleven once per transport
  generation for the bring-up. Each message is bounded by `TIMEOUT_MS` (2.5 s), the whole forward part of a
  creation by ONE 6 s deadline on the interrupt-time clock (`rm_sysmem::CREATE_BUDGET_MS`; `Slow`): the wait
  for another thread's bring-up, the bring-up itself and every step after it share it, and each message waits
  at most what is left of it (none is sent once it is spent). The undo of a failed creation has its own 3 s
  (`UNDO_BUDGET_MS`), so a creation that ran out of time can still give back what it made; a failed creation
  is therefore over in about 9 s.
* A mailbox to the HPD worker has a dependency edge the synchronous call does not: `create_one` would wait on a
  thread that itself calls dxgkrnl (`DxgkCbIndicateChildStatus`, the VidPn programming) and may be blocked on a
  lock dxgkrnl holds while it calls CreateAllocation. A creation that waits only on the control queue, as the
  Venus arm does, cannot take part in that cycle.
* The ring client's state has exactly one mutator (the worker), which is why 14.2 wanted a mailbox. The service
  does not share it: it has its OWN RM client (own control, GPU and DRM files, own `hRoot`; three handles of the
  KMD owner's 128), so there is nothing to serialise but the bring-up. Creations run concurrently, each on a
  reserved table slot; only the first one brings the client up while the others `sleep_ms` for it until their
  own 6 s deadline (the wait is a deadline, not a count of sleeps: a `sleep_ms(1)` lasts a timer tick, about
  15.6 ms, so a count of 5000 of them was 78 s on dxgkrnl's thread).
* Cost: a creation holds its thread for the host round trips, with no lock held (`STATE` is a leaf spinlock
  taken for table moves only; nothing is held across a message). It takes no dxgkrnl lock of its own and is
  never called under the scanout lifecycle lock.

The ring client's `Client` machine is reused for the eleven bring-up steps (driven to `bring_up_done()` on the
creator's thread and then dropped; only the four handles are kept), so no bring-up logic is duplicated.

### 15.4 The sequence, and what each stage undoes

`RmSysStage` is written BEFORE each stage so a hang names itself. Failure in any stage undoes the stages before
it in this order: the export file, the Venus resource, the GEM, the memory; a step that fails is counted
(`RmSysSoft`) and left to the transport sweep (`RmSysLeak`). The undo runs on its own 3 s allowance, not on
what is left of the creation's.

**The slot's RM handle.** The memory's RM handle is `H_BASE + slot`. If the undo cannot make sure RM freed the
memory (the `RM_FREE` failed, or the `RM_ALLOC` timed out or its reply was unreadable, so the host may have
allocated it, and the free was not answered either: `rm_sysmem::mem_may_be_live`), the slot is not freed:
`Svc::abort_leaked` counts the strike and leaves it `Closing` (quarantined) for the rest of the transport
generation, the way a destroy whose free failed does (15.9). A creation that took the slot again would
`RM_ALLOC` the same handle, be refused as a duplicate, and three of those would end the generation's
allocations. An `RM_FREE` that RM answers with `NV_ERR_OBJECT_NOT_FOUND` settles it (nothing is there).

| stage | what | on failure |
|---|---|---|
| 1 admit | route, display half, transport, Venus context, layout; a slot (or the bring-up duty) | Venus, no strike for a policy refusal |
| 2 bring-up | the ring client's eleven steps (`OpenCtl` .. `OpenDrm`) | the files opened are closed; **Dead for the generation** |
| 3 alloc | `RM_ALLOC` `NV01_MEMORY_SYSTEM` (params: `rm_sysmem::params`, `attr` `0x5a000000` write-combined, the default, / `0x3a000000` cached, opt-in; RM writes them back as `0x4a800000` / `0x2a800000`, the host's decision values; `attr2` 1, size, alignment 4096, width/height/pitch 0 as `rm_sysmem_flip.c`) | strike |
| 4-7 | export file, `EXPORT_OBJECT_TO_FD`, `GEM_IMPORT_NVKMS` (pitch layout), close the export file | strike; undo |
| 8 import | `RESOURCE_CREATE_BLOB` `RM_EXPORT` + `USE_MAPPABLE` under `DeviceOwner::KMD_RM` and the KMD's Venus context; the foreign record | strike; undo |
| 9 mark | `foreign_mark_sysmem`: the record may now be mapped | strike; undo |
| 10 trial | map the blob once (KMD window partition), read `map_info`, unmap | strike; undo. Host cannot map, or `map_info` is not the attribute the memory was made with (`RmSysMis`, for the default write-combined memory too): Venus. An unmap the host did not confirm ends the creation with the blob still recorded as mapped and its window range still taken; the undo's blob release (which sees `mapped`) unmaps again and only then returns the range, so no later map can overlap a range the host still has mapped |
| 11 adopt | `adopt_for_allocation` (declares foreign, DEVICE ownership, the layout repeated): the WDDM allocation owns the resource | strike; undo |

Three strikes in a row (a success clears them) stop NEW allocations for the generation (`NoNew`); the live ones,
the client and the flip source stay: their GEMs, memory and the screen depend on them.

**Sizing, and the padded-versus-recorded lesson.** RM system memory is page-granular. The size asked is
`cross_adapter_pitch(width) * height` rounded to a page (1920x1080: pitch 7680, 8,294,400 bytes; 5120x1440:
29,491,200); RM's answer for `size` is adopted if it holds the request (`adopt_size`), rounded to a page, and
THAT number is what the foreign record, the blob table, `CreatedBacking::blob_size` (`HostAuthoritative`),
`ap.size`, VidMm's `info.Size` and the aperture check all see (one value, one rounding), so the aperture page
count, `blob_map_begin`'s map length and the paging clamp agree by construction. VidMm sizing a virtual
transfer larger than what was recorded (measured 0x1E10000 against 0x1C20000 for a 5120x1440 Venus primary)
is cut by `clamp_range` against the recorded size exactly as today (test
`vidmm_sees_the_recorded_size_and_the_padding_is_cut`). The layout recorded is XRGB8888 (DXGI 88, the primary's
format; 87 and 28 map to ARGB / ABGR), `MOD_LINEAR`, stride = the pitch, offset 0.

### 15.5 Decision: the cache attribute, and the alias

dxgkrnl maps a CpuVisible allocation's aperture write-combined unless the allocation carries `Cached`, and
the 36th session found it rejects `Cached` together with the primary. So the primary's own CPU view stays
write-combined whatever memory sits behind it. Cached memory behind that view is a mixed-attribute ALIAS of
the same physical pages: architecturally invalid, never measured on this stack, and the lead engineer's
decision is that it is **not shipped blind**. The default therefore makes no alias; the cached variants are
opt-ins for the run that measures them.

| `KmdRmSysCache` | RM is asked for | host `map_info` must say | dxgkrnl's view | alias | `RmSysCache` / `RmSysAlias` |
|---|---|---|---|---|---|
| **0 or absent (default)**, 1 | write-combined sysmem, `attr` `0x5a000000` (RM writes back `0x4a800000`) | WC (3) | write-combined (no `Cached` flag) | **no** | 3 / 0 |
| 2 (opt-in) | cached sysmem, `attr` `0x3a000000` (`0x2a800000`) | cached (1) | write-back, asked with the `Cached` flag; if dxgkrnl refuses the primary the creation fails (Code 43 class), if `AllocCached=0` takes the flag away it is value 3 | no (yes with `AllocCached=0`, counted) | 1 / 0 |
| 3 (opt-in) | cached sysmem | cached (1) | write-combined | **yes**, counted | 1 / 1 |

* **Default: write-combined memory.** Every view of it agrees: the host maps it write-combined (it reports
  `map_info` WC for coherency 2), the KMD's own kernel maps (paging copies, the GDI executor) follow the host's
  `map_info` through `map_cache_to_mm` (write-combined), dxgkrnl's aperture view is write-combined. It is parity
  with today's primary mapping, whose aperture view is write-combined too (GDI reads of it were already
  write-combined). The cost is the measured write-combined speed: reads about 75 MB/s, writes about 28 MB/s.
* The RM request is what decides "write-combined": `rm_sysmem::params` puts coherency 2 (`NVOS32_ATTR_COHERENCY_WRITE_COMBINE`)
  in bits 31:29 of `attr` and PCI in bits 26:25, so RM answers `0x4a800000`, the value the host's `RmPlacement`
  maps to `MAP_CACHE_WC`; the cached request (coherency 1) is answered `0x2a800000`. Tests pin both words, the bit
  positions the host decodes, and that only value 2 and 3 ever send the cached one.
* **The trial map's safety check applies to the default**: the memory is made with the attribute the knob
  chose, the trial map reads the host's `map_info`, and a host that reports anything else (cached or uncached
  for the write-combined memory the default asked for, as for a cached request) is refused: the allocation is
  given up and Venus makes the primary (`RmSysMis`, `Why::Cache` 19). Tested for every knob value
  (`the_trial_check_guards_the_default_and_every_opt_in`).
* Value 2 sets the `Cached` flag only on a primary this service made (`primary_cached_flag(resource_id)` asks the
  service's table), never on a UMD-adopted foreign primary.
* Before a flip the worker issues `sfence` (this core's write-combined buffers; dxgkrnl's WC stores from other
  cores drain on their own and the viewer samples on its own schedule: a stale frame, not corruption). With the
  write-combined default this is the common case, not the exception.
* What the hardware run decides (15.13 steps 2 and 7): that the default works and shows (no alias, so nothing
  to compare it with), and then whether the CPU read speed of value 3 (and 2) is worth the alias: stale lines
  or tearing in a window drag with value 3 against a clean picture with the default is the answer. On a KVM host
  without non-coherent DMA the guest PAT is ignored (IPAT) and the alias would be inert; on a host that honours it
  it is invalid and the symptom is stale lines.

### 15.6 `MAP_BLOB` for sysmem foreign records, and the paths that use it

Today every `MAP_BLOB` of a foreign resource is refused (`blob_map_begin`, `blob_remap_begin`,
`FgMapRf`). Now `ForeignTable::cpu_mappable(resid)` (the record's `sysmem` bit, set only by the KMD's own
service between import and adoption: `mark_sysmem` requires the creator to be `KMD_RM` and the resource not yet
adopted) lifts the refusal for that one record; vidmem, user-imported and every other foreign resource keep it.
User-mode escapes cannot reach it (they filter on a device owner; an adopted slot's owner is `None`).

Item 4, the paths that map or copy through the aperture mapping, read for WC or Venus assumptions:

* `DxgkDdiMapCpuHostAperture` (`ddi/cpu_host_aperture.rs`): `map_blob_at` -> `blob_remap_begin` -> the same
  `RESOURCE_MAP_BLOB` at dxgkrnl's offset; the size check uses `alloc.size` (the recorded size, 15.4).
  `bar_eligible` is true (`HostAuthoritative`). The existing mapping at another offset is unmapped first
  (the content is the host memory's, so a remap preserves it; the zeros the host leaves behind are in a
  range nothing maps afterwards).
* `BuildPagingBuffer` content ops (`bar_virtual_transfer`, `bar_transfer`, `bar_fill`) all go through
  `with_blob_bytes`: `map_blob_prepare(Any)` plus `MmMapIoSpace` with the cache type of the host's `map_info`
  (`map_cache_to_mm(prep.map_cache)`), never a fixed one. Nothing assumes WC. The system side is the ordinary
  RAM MDL (`MmCached`), unrelated to the blob. The allocation's `system_backing_policy` is `None` (the
  Present-buffer mirror and its page leases are not involved).
* The GDI executor resolves the blob by resource id with `OwnerFilter::Any` (the adopted slot is KMD-owned):
  works as for a Venus blob. The escape `MAP_BLOB` (a UMD mapping it) is owner-scoped and stays refused.
* Nothing in the aperture / paging / GDI code calls a Venus operation on the primary (`venus_image_id` and
  `venus_memory_id` are 0; destroy skips `destroy_image` / `free_memory_blob`, which are gated on them).

### 15.7 Option B: the flip from the foreign record (`virtio/rm_client/sysmem_flip.rs`)

Hook points (14.3's table, as built):

| where | what |
|---|---|
| `program_vidpn_source_inner`, after the extent check | `sysmem_flip::program(resid, address, w, h)`. An allocation whose record is an adopted RM sysmem record (and the level is 5): no `ScanoutTarget`, no `SET_SCANOUT_BLOB`, no GPU copy, no dedicated image. It records the target (resid, DRM file, GEM, layout), stores `active_scanout_*`, publishes the displayed primary (`publish_bound_primary`, as a bind does), ends the leases (`Cancelled`: nothing reads this memory through a lease; the flip is not one), updates a registered resident source's layout in place, raises the frame edge and returns `Programmed`. `host_bound_scanout_resource` is NOT set: a Venus flush of it would be refused loudly (`RfUnb`) instead of being sent for a resource with no scanout. Any other allocation: `NotOurs` and the Venus path below runs unchanged, after `other_source` forgets a previous RM target (the worker then withdraws the resident source, and the desktop flush returns) |
| `rm_client::service` (HPD worker), level 5 | `sysmem_flip::service`: the level 3 presenter's state machine with a ring of ONE (`Presenter::new(1)`; "copy" is nothing, the memory is the primary): register the resident source (`foreign_scanout_resident_set`, owner `KMD_RM`, the service's DRM file), flip the GEM on a frame edge (`present_within`, 1 s, direct) paced to the MODE's refresh period (15.16), re-flip on a resume edge, withdraw when no target. The presenter's inputs come from `rm_sysmem::flip_inputs`: a ring of one never asks the release book (below) |
| the suppression gate (unchanged) | the desktop flush the arbiter withholds is a frame edge (`note_frame_edge`), exactly as at level 3; the refresh machinery reaches the gate because `active_scanout_resource` names the RM primary |
| `release_allocation_resource` (via `sysmem::released`) | the target is forgotten first (`target_gone`), so no flip names a GEM about to be closed; then GEM close, `RM_FREE` |

Priority, preemption, restore: the resident source of the arbiter, unchanged (13.2). A user source preempts it
at once; its end resumes the RM primary with a re-flip (never a Venus flush: there is no Venus image to flush).
A resume needs no copy: the memory is live, so the desktop-changed flag a parked level 3 source keeps
(`note_desktop_changed`, ring levels only) is not needed and not raised at level 5. Three consecutive failures (a refused flip, a refused registration, eight yielded flips with no user
source) withdraw the source and, unlike level 3, **start over five seconds later** (`RmSysGaveUp`): a primary with
no Venus image has no desktop for Venus to show, so withdrawing for good would only freeze the screen. The five
seconds are a gate at the top of the worker's pass (`RESTART_AT`, `rm_sysmem::restart_pause`): a reset presenter
registers at its very next look, and every desktop frame edge wakes the worker, so without the gate the
register, flip, fail, withdraw cycle would run on every edge. During the pause nothing is registered or flipped
(the edges stay owed; a primary programmed meanwhile is shown when the pause ends).
The S4 fence queue is not involved: a resident flip is sent direct and never queues behind fenced presents
(13.12 states why); `present_within` is the same call.

**The release seam (decided; wired in one place).** The host's `ScanoutReleased` (`foreign-scanout.md`, "Buffer release", has the book's
rules) says a replaced buffer is no longer read. Level 5 flips are entered in the same book as every other flip
(`present_within` mints and `send` marks them), and two questions follow.

* **Does a flip wait for a release? No, never, and it cannot be made to.** The buffer a level 5 flip shows is
  either the one already on the scanout (a re-flip, the whole life of a ring of one) or ANOTHER primary (a mode
  change). The book never releases the buffer on the scanout ("the buffer on the scanout is never released; a
  buffer flipped again is released again later"): a re-flip supersedes the older flips of the same buffer, but its
  own `seq` stays live for as long as it is shown, so a wait for it would last its 500 ms limit on every frame for
  nothing. And the flip of another primary is exactly what makes the host release the previous one: a wait
  before it could only wait for its own effect. The presenter agrees: `Presenter::back_wait_seq` is `None` for a
  one-surface ring, and the driver passes `release_tracked = false` (`rm_sysmem::flip_inputs`), so even a real
  "not released" answer could not hold a frame. Tested with a presenter that is given the worst answers
  (`a_ring_of_one_never_waits_for_a_release_that_cannot_come`, 50 consecutive re-flips).
* **Does anything wait? Yes: giving the memory back.** `sysmem::released` (DestroyAllocation, or the last close
  of a destroyed-while-open allocation) closes the GEM and frees the RM memory. A primary the screen has MOVED
  OFF (the host was told to show another buffer after it) is closed only once the host released it, so a viewer
  that still samples it is not left with memory RM took back. `FlipLog` (`rm_sysmem`) remembers the newest flip's
  GEM and the buffer that flip replaced, with the time the replacing flip was taken; `close_gate` is the level 3
  ring's rule (`scanout_release::ring_wait`): held until `ScanoutReleased` retires the replaced buffer's last
  `seq` (`is_done`), at most **500 ms from the replacing flip** (the host overrules a client that holds it after
  500 ms; `RmSysRelTmo` counts the close that went ahead at the limit), polled every 2 ms by `wait_released`
  (the interrupt side wakes the HPD worker, not this thread), ended at once by StopDevice. It waits for
  NOTHING when the GEM is the buffer on the scanout (never released: the log says it is current), was never
  flipped, is older than the one replaced buffer the log keeps, or the host's releases are not tracked
  (`is_done` is true then). A user source that replaced our buffer on the host is not in the log: the close then
  does not wait (as before this change). The host resource is already unreferenced when `released` runs and the
  host's own dma-buf reference keeps the pages valid; the wait only orders the GEM close and RM free after the
  viewer's release. Cost: a DestroyAllocation of a replaced primary can take up to 500 ms on dxgkrnl's thread when
  the viewer sits on it; the common case is none (the release arrives milliseconds after the replacing flip).

The flip's own bookkeeping: `LAST_SEQ` / `RmSysSeq` is the last flip the host took; `RmSysRelWait` counts closes
that waited. Tests: `a_reflipped_buffer_is_never_something_a_close_waits_for`,
`a_replaced_primary_is_waited_for_until_the_host_releases_it_or_the_limit`,
`the_log_follows_a_buffer_that_comes_back_and_forgets_a_closed_one`,
`the_book_agrees_with_the_log_on_what_can_be_waited_for` (the real `ReleaseBook`).

### 15.8 What reaches the screen, and what does not (read this before the first run)

* GDI writes into the primary reach the viewer through flips on frame edges. The host viewer commits only when
  it is sent a `ScanoutFlip` (no re-sampling, no damage message), so every change of the primary has to end in a
  flip of the same GEM: 15.16 lists every edge kind, the mode-rate pacing and what covers a CPU write that no
  event reports (a short decaying tail, and the opt-in `KmdRmSysPollMs` heartbeat). Before 15.16 only the
  programming and the refresh gate's edges existed at level 5; the present blit, the windowed blit and the
  paging writes raised none.
* **DWM's composition into this primary** (the UMD's windowed present blits into the primary by identity)
  rides the host's Venus import of an RM-export resource. The identity record of a STANDARD allocation cannot
  carry the FOREIGN flag today (only DEVICE_MEMORY does), so a UMD opening it sees plain Venus memory with
  `memory_type_index` 0. Whether that import works is the host's and the UMD's; **unverified**, and the
  likeliest first failure of a DWM session at level 5. Without DWM (safe mode, the logon screen's GDI) the path
  is complete.
* There is no Venus fallback for an RM primary that exists: its Venus image is `0`, so the primary-to-LINEAR GPU
  copy cannot run. A primary that cannot be flipped stays on its last frame. The fallback is TOTAL for
  allocations that are not yet made (every failure of 15.4 gives Venus) and for the next primary after a
  failure; making an existing RM primary fall back (a CPU copy of the sysmem into the dedicated LINEAR image,
  or recreating the allocation) is not built (15.14, question 3).
* The create-time layout trailer needs 128 bytes of private data and a standard allocation has 96
  (`GetStandardAllocationDriverData`): the adoption is told room exists (the KMD is the creator, there is no
  creator to promise a trailer to) and `write_foreign_layout_trailer` writes it only where there is room
  (`FgOpNoRm` counts the opens that find none). Openers read the layout from the meta (stride, extent).

### 15.9 Failure and fallback matrix

| where | failure | effect | fallback |
|---|---|---|---|
| knob 0..4 | none | the v315 behaviour: `create_one`'s arm one atomic load (`route` says `KnobOff`), `release_allocation_resource` one, the two display hooks one each; byte for byte what the tip does otherwise (15.15) | Venus (as before) |
| route / display half / transport / context / layout | refused | `RmSysVenus`, `RmSysWhy`; no strike | Venus |
| bring-up | any message fails | files closed; service **Dead** for the generation (`RmSysFail` stage 2) | Venus for every allocation of the generation |
| bring-up by another thread | not finished by this creation's 6 s deadline | `BringUpBusy` | Venus for this one |
| alloc / export / import / trial / adopt | any | undone (15.4); one strike; `RmSysFail` stage and kind | Venus for this one; three strikes in a row: no new RM allocations this generation (`NoNew`) |
| creation slower than 6 s (the deadline covers the wait for a bring-up, the bring-up and the steps) | `Slow` | as a failure; the undo has its own 3 s | Venus |
| the undo cannot confirm the memory's `RM_FREE` (or an `RM_ALLOC` that timed out is not known to be absent) | `RmSysLeak`; the slot stays `Closing` | strike; the slot and its RM handle are not reused this generation | Venus for this one |
| the trial's unmap is not confirmed | `RmSysSoft`, `RmSysTrialF` | the blob stays recorded as mapped; the undo unmaps and then frees the window range | Venus for this one |
| host map_info is not the requested attribute | refused (`RmSysMis`) | undone, strike | Venus |
| table full (32 live) | `TableFull` | no strike | Venus |
| flip refused / times out | counted (`RmSysFlipFail`) | paced retry 100 ms; three in a row: withdraw, restart in 5 s | the screen keeps its last frame meanwhile |
| a user source takes scanout 0 | `NoSource` | yielded, parked; resumes by re-flip | n/a |
| the shown allocation is destroyed | `target_gone` | resident source withdrawn by the worker (the arbiter owes the desktop a flush), GEM closed, memory freed | the next primary |
| a Venus source is programmed | `other_source` | the target is forgotten, the resident source withdrawn | Venus desktop flush |
| destroy of a primary the screen moved off, the host has not released it | `RmSysRelWait` +1; the close is held, polled every 2 ms (15.7) | released by the host: closed at once; 500 ms after the replacing flip: closed anyway (`RmSysRelTmo`); StopDevice: closed at once | n/a |
| destroy of the shown primary, one never flipped, releases not tracked | no wait | closed at once (the host never releases the buffer on the scanout) | n/a |
| destroy: GEM close or free fails | counted (`RmSysSoft`, `RmSysLeak`) | the transport sweep closes the KMD owner's files (the control file's close frees the RM client) | n/a |
| transport reset / StopDevice | `forget` | table, target, presenter cleared; the sweep closed the host side; a stale allocation's destroy is skipped by `is_current_generation` | next generation starts cold |
| **mid-session death of the service** | bring-up cannot die later (it is a one-shot); a creation failing leaves the live allocations alone | nothing already made is torn down: its GEM, memory and flip source stay valid; only NEW allocations go to Venus | decided: never close handles under live allocations |

### 15.10 Locking, IRQL, lifetime

* `STATE` (service) and `TARGET`, `PRES`, `FLIPS` (flip) are leaf spinlocks over plain data, never held across a host
  message, a wait, an allocation, a registry write or another lock, and not nested. `LIVE` mirrors the
  live count so every Venus allocation's destroy costs one load.
* Creation: PASSIVE on dxgkrnl's thread; no lock held; it takes no dxgkrnl lock and the scanout lifecycle lock is
  never held when it runs (create_one does not take it; `destroy_allocation_ctx` calls `released` after its
  lifecycle section ended). The programming hook runs under the lifecycle lock but sends nothing and takes no
  `STATE` lock across I/O; the flips are the worker's, with no lock.
* Order on destroy: `retire_scanout_allocation` (clears `active_scanout_*` if it names it) -> host resource
  unref (`release_allocation_resource`) -> `target_gone` -> GEM close -> `RM_FREE`. A last close that releases a
  destroyed-while-open allocation takes the same path, so the RM objects outlive every opener.
* A flip racing a destroy: the flip re-reads the live slot before it sends; a flip already on its way when the
  GEM is closed is refused by the host and counted (one failed flip, never a strike by itself).
* Stack: every message step is `#[inline(never)]` with its buffers (the ring client's rule); the chain is
  `create_one` -> `build_backing` -> `try_create_primary` -> `create_primary` -> `build` -> `build_steps` -> a step.
  `tools/kmd-frame-sizes.ps1` must be run on a build.

### 15.11 Counters (`RmSys*`, REG_DWORD, written once the service was asked for something; at level 5 `RmKnob` is 5)

| value | what | healthy |
|---|---|---|
| `RmSysTry` / `RmSysOk` / `RmSysVenus` | creations asked / made / given to Venus | 1 / 1 / 0 per primary |
| `RmSysWhy` | the last reason for Venus (`Why::code`: 1 knob, 2 present buffer, 3 not CPU-written, 4 not KMD's, 5 odd primary, 6 no display, 7 no transport, 8 no context, 9 format, 10 extent, 11 size, 12 dead, 13 no-new, 14 bring-up busy, 15 table full, 16 bring-up, 17 alloc, 18 import, 19 cache, 20 trial, 21 adopt, 22 slow) | 0 |
| `RmSysStage` | the stage started last (1 admit, 2 bring-up, 3 alloc, 4 open export, 5 export, 6 GEM import, 7 close export, 8 import, 9 mark, 10 trial, 11 adopt, 20 GEM close, 21 free, 22 undo, 23 waiting for a release before the GEM close) | any |
| `RmSysFail` | `stage << 24 \| kind << 16 \| code` of the last failure (kinds as `RmFail`) | 0 |
| `RmSysState` | `phase << 28 \| strikes << 24 \| live` (phase 0 cold, 1 bringing up, 2 up, 3 no-new, 4 dead) | `0x2000_0001` with the primary alive |
| `RmSysLive` / `RmSysFreed` | allocations alive / released | 1 / 0 |
| `RmSysBring` | bring-ups done | 1 per generation |
| `RmSysMs` / `RmSysMsMax` | last / longest creation, ms | a few tens (bring-up included) |
| `RmSysTrial` / `RmSysTrialF` | trial maps that worked / failed | 1 / 0 |
| `RmSysCache` | the last `map_info` nibble (1 cached, 2 uncached, 3 WC) | 3 by default (write-combined memory); 1 with `KmdRmSysCache` = 2 or 3 |
| `RmSysMis` / `RmSysAlias` | creations refused for the wrong attribute / whose memory and dxgkrnl's view differ | 0 / 0 by default (no alias); `RmSysAlias` 1 only with `KmdRmSysCache` = 3 (or 2 when the `Cached` flag was not applied) |
| `RmSysSoft` / `RmSysLeak` | undo steps that failed / allocations left to the sweep | 0 / 0 |
| `RmSysRelWait` / `RmSysRelTmo` | GEM closes that waited for the host's release of a replaced primary / that went ahead at the 500 ms limit | 0 or small / 0 |
| `RmSysProg` / `RmSysProgBad` | primaries programmed / refused (a layout that is not the mode's) | grows / 0 |
| `RmSysRegs` / `RmSysWithdrawn` / `RmSysGaveUp` | registrations / withdrawals / giving-ups | 1 / 0 / 0 |
| `RmSysFrames` / `RmSysReflips` / `RmSysYielded` / `RmSysFlipFail` | flips for an edge / for a resume / that found the source yielded / refused | grows / small / small / 0 |
| `RmSysPres` / `RmSysSeq` | the presenter word (bit 0 registered, bit 1 gave up, bits 8.. failures) / the last flip's `seq` | 1 / grows |
| `RmSysEdges` | frame edges raised (all kinds) | grows with desktop activity |
| `RmSysEdProg` / `RmSysEdBlt` / `RmSysEdWBlt` / `RmSysEdPag` / `RmSysEdRef` | by kind (15.16): programmed / present blit / windowed blit done / paging write / the refresh gate (markers, restore: the rest of the total) | each grows with its source |
| `RmSysEdOther` | events that named another allocation than the shown primary | grows (most paging and blits) |
| `RmSysCoal` | edges that got no flip of their own (folded into one flip per refresh) | grows under load |
| `RmSysTail` / `RmSysPoll` | flips of the tail after the last reported change / of the `KmdRmSysPollMs` heartbeat | up to 5 per burst / 0 unless the knob is set |
| `RmSysIvl` / `RmSysPollMs` | the flip interval in 100 ns (the mode's refresh period: 41667 at 240 Hz, 166667 at 60 Hz) / the heartbeat knob | the mode's / 0 |
| `RmSysBltCpu` / `RmSysBltBytes` / `RmSysBltUs` / `RmSysBltMaxUs` / `RmSysBltSkip` / `RmSysBltWhy` / `RmSysBltStg` | the Blt CPU-copy fallback (15.17): Presents copied / MiB written / microseconds / the slowest one / skipped with success / the last skip reason / the staging image's resource id | grow with the Blt presents / grows / the average is milliseconds / under about a second / 0 / 0 / one id |

`FgMapRf` must stay 0 for the primary (the map is no longer refused for it); `FsSupp` grows with the withheld
flushes; `RfUnb` is expected to count only if a Venus flush of the RM primary is ever attempted.

### 15.12 Verified here, and not

Verified on the host (`cargo test` in `guest/windows/kmd_logic`, 789 tests with the Blt fallback of 15.17; in `guest/windows/protocol`, 30):
which kinds go to RM; sizes and the page-granular rule against the aperture count and the paging clamp; the RM
parameter block byte for byte (`attr` words against `nvos.h` 610.57.04's bit positions, `0x3a000000` /
`0x5a000000`, and the written-back `0x2a800000` / `0x4a800000` the host decides from); RM's rounded answer; the
cache policy for every knob value (the default is write-combined with no alias; only the opt-in values alias; an
unknown value is the default) and the host-nibble check against every knob value; the service state machine
(bring-up once, waiters, dead for a generation, three strikes, the bounded table, slot reuse only after free,
generation reset, stale commit and free ignored; a slot whose memory may still be live is quarantined, never
found, taken or reused, strikes count, a new generation cleans it; what a failed `RM_ALLOC` / `RM_FREE` leaves
open, by failure kind); the creation's and the undo's budgets (6 s and 3 s, per-message caps, a wait on the deadline
ends on time whatever a sleep costs); the target book; the flip against the REAL arbiter (register,
flip, 60 Hz pacing, a user source preempting and the resume, a new primary updating the resident layout in place,
the shown primary going, three refused flips, and the five-second restart pause: a gate on the clock that holds back
hundreds of frame-edge wakes and opens at the restart time) with the presenter inputs the driver passes (`flip_inputs`, the v315
`release_tracked` / `back_released` fields); a ring of one never held by a release; the release seam against the
REAL `ReleaseBook`; the record's `sysmem` bit (only the KMD's un-adopted import can be marked; an adopted
unmarked record is never a source).

Type-checked (`cargo check`, no codegen) against a harness generated from the REAL module declarations: module
visibility from the real `mod` lines (never typed by hand), signatures cut from the real sources, and the files
under test included unchanged: `rm_client.rs` (as a directory module over its real children), `rm_client/sysmem.rs`,
`rm_client/sysmem_flip.rs`, `rm_client/sysmem_blt.rs` (15.17), `rm_present.rs`, `rm_foreign.rs`, `virtio/foreign_scanout.rs`,
`virtio/scanout_release.rs`, `adapter/foreign_scanout.rs`. A deliberately wrong path is rejected by it (checked).
Every `crate::` / `super::` path of those files and of `create_allocation.rs`, `display.rs`, `ctrl.rs`,
`foreign_tables.rs` and `resource_tables.rs` was resolved against the real `mod` lines and item visibilities
(0 problems). The edits to the large existing files are small hooks, read against the real definitions; they
are not compiled. The v315 line itself did not compile before this merge: its protocol crate carries a
compile-time assertion that named the wrong bit for `HELIOS_NVRM_CAP_SCANOUT_RELEASE` (fixed in this branch,
`>> 35`; `cargo test` in `protocol` now passes). **Not verified by anything**: that `kmd_render` compiles; every
host reaction (`USE_MAPPABLE` create, map and unmap, `map_info`, the GEM import of system memory, the flip of
it, the compositor's sampling, `ScanoutReleased` for a level 5 flip); IRQL behaviour; dxgkrnl's reaction to an
adopted standard allocation; DWM's import (15.8); the 500 ms close wait on dxgkrnl's thread; the opt-in cached
variants (15.5). Also not run: the wiring of the deadline into the messages (`Io::limit`, the budgeted unmap in the
trial), the quarantine call on a failed creation, and the restart gate in the worker's pass (type-checked and read
only; their pure decisions are the tested part).

### 15.13 Hardware checklist, in order

Stop at the first step that fails. Prerequisites: the host with `feat/rm-export-map-blob` (and the 4 GiB window), a
viewer connected, section 9 step 2's list, **levels 2 and 3 working** (they prove the RM client, the export, the
GEM import and `ScanoutFlip` from a KMD owner on this VM), and `tools/kmd-frame-sizes.ps1` passing. Nothing
here sets `KmdRmSysCache` before step 7: steps 1 to 6 run on the default, write-combined memory with no alias.

0. **Check the key first.** Read `KmdRmClient` (and `KmdRmSysCache`) on the box before installing this build.
   A stale value of 5 or more (an old experiment, a typo, 99) used to clamp to level 4 and now selects level 5:
   the box's desktop primary moves to RM memory and its flip on the first boot of the new driver. On a box that
   is not meant to run level 5, delete the value or set it to the level wanted; on a box that is, the value
   is step 2's, set deliberately. The build's identity is 22.22.317.0, so an installed 316 or older is replaced
   (the INF `DriverVer` and the image's `FILEVERSION` come from `driver-version.env`).

1. **Knob absent**: nothing changes; no `Rm*` value; frame sizes pass.
2. **`KmdRmClient=5`, restart the device, no DWM yet** (or boot to the logon screen). This is also the cache
   check of the default: write-combined, no alias. Expect `RmSysTry=1`, `RmSysOk=1`, `RmSysVenus=0`,
   `RmSysState=0x20000001`, `RmSysFail=0`, `RmSysTrial=1`, **`RmSysCache=3`, `RmSysAlias=0`, `RmSysMis=0`**;
   host log: RM_ALLOC class 0x3e with `attr` 0x5a000000, RM_CONTROL 0x3d05, a DRM ioctl 0xC0206441,
   `RESOURCE_CREATE_BLOB` 0x80000001 with `USE_MAPPABLE`, a map and an unmap. `RmSysFail` names the stage (15.11)
   and kind. `RmSysVenus` with `RmSysWhy` 20 or 19 means the host cannot map it or reports another attribute
   than write-combined (`RmSysCache` says which): the host's answer.
3. **The primary maps**: `ChMn` (aperture maps) grows, `ChEm=0`, `ChEp=0`, `FgMapRf=0`; `RmSysLive=1`.
4. **It is shown**: `RmSysProg>=1`, `RmSysRegs=1`, `RmSysFrames>=1`, `RmSysFlipFail=0`; the host log shows a
   `ScanoutFlip` for the KMD's DRM handle; the logon screen / desktop is visible through the viewer. Black:
   compare with level 2's probe (a probe that shows means the flip path is right and the content is the
   question).
5. **GDI updates show**: move a window under GDI; `RmSysFrames` grows with `FsSupp`; the picture follows. If
   the viewer shows the first frame only, the edges are not reaching the gate (`SaCnt`, `RfCnt`) or the viewer
   needs the flip as the damage signal and the edges are the limit.
6. **Baseline CPU speed of the default**: time a read of the primary through GDI (a full-screen `BitBlt` /
   `GetDIBits` from the screen DC, 20 repetitions) and a full-screen GDI fill; note both. They are the numbers
   step 7 is compared with (the host measured write-combined reads at about 75 MB/s).
7. **The opt-in cached variants, only now that the default picture is proven.** `KmdRmSysCache=3`, restart the
   device: expect `RmSysCache=1`, `RmSysAlias=1`; compare the step 6 read speed, and look for stale lines or
   tearing in a window drag (the alias's symptom). Then `KmdRmSysCache=2`: `RmSysAlias=0` if dxgkrnl took the
   `Cached` flag (the 36th session saw it rejected: watch `StdType` / Code 43 class failures at the primary's
   creation, and `AllocCached=0` turns it into value 3). Put the knob back to absent afterwards. The default
   changes only on this evidence.
8. **Preemption**: `crm_scanout_smoke` / an NVK scanout app: `FsSet`+1, `RmSysYielded`, the app's frames show;
   on release `RmSysReflips`+1 and the desktop is back with no Venus flush (`FsRest` unchanged).
9. **Mode change**: a new primary is created (`RmSysOk`+1), programmed (`RmSysProg`+1), the old destroyed
   (`RmSysFreed`+1, `RmSysLive` back to 1), no shear (`RmSysProgBad=0`). With the host's release events on
   (`RelRecv` grows) the old primary's close may have waited: `RmSysRelWait` 0 or 1, `RmSysRelTmo=0`; a
   `RmSysRelTmo` is the viewer sitting on a replaced buffer for 500 ms.
10. **Fallback**: with the host's map disabled (or `RmSysCache` forced wrong), `RmSysVenus` grows with `RmSysWhy`
    set and the desktop is the Venus desktop; after three such creations `RmSysState` shows `no-new`.
11. **Teardown**: stop the device with the primary shown, and while a creation is in flight: no bugcheck,
    `RmSysLeak=0` after a normal destroy, the sweep closes the rest; `FgLive` returns to 0.
    **Time bounds** (not run): with the host's reply to the bring-up held back, a creation ends with `RmSysWhy`
    `BringUp`/`Slow` within about 6 s plus the undo's 3 s (`RmSysMsMax`), and a second creation made meanwhile
    ends at its own 6 s with `BringUpBusy`, not after a minute; with the host's `RESOURCE_UNMAP_BLOB` refused,
    `RmSysTrialF` grows, the creation goes to Venus, and a later map of another primary does not overlap the
    first one's range.
12. **DWM** (15.8): a session with DWM: whether the windowed present reaches the primary is the host's and
    the UMD's; `RmSysFlipFail`, `Pb*` / `Vk*` counters and the host log say where it stops.
13. **Soak**: ten minutes of window dragging with `RmSysFlipFail=0`, `RmSysGaveUp=0`, `RmSysSoft=0`,
    `RmSysRelTmo=0`. Then the give-up path: with the host refusing `ScanoutFlip`, `RmSysGaveUp` grows by one
    per five seconds (three failed flips each, 100 ms apart), not per frame edge.
14. **A Blt workload** (the CPU-copy fallback, 15.17: steps 13a to 13c there). Only after steps 2 to 5 have shown
    the picture and its flips: a Blt present into the RM primary used to fail the Present, and now counts in
    `RmSysBlt*`.

### 15.14 Open questions

1. **The alias** (15.5): the default avoids it. Whether dxgkrnl's write-combined view of cached memory is
   harmless on this stack, and whether the CPU read speed it buys is worth the risk, is what step 7 of 15.13
   measures; until then the cached variants stay opt-in.
2. **Whether the viewer needs a flip per change** or samples the attached buffer continuously: the host session's
   answer is that it commits only on a `ScanoutFlip` (15.16), so the edge-driven flips ARE the damage signal; what
   is open is only the cost of a refresh flip on the real viewer (checklist step 11).
3. **An existing RM primary has no Venus fallback** (15.8; the KMD's own Blt present into it is now answered by the
   CPU copy of 15.17, not this). The seam is `program`'s `Retry` / the presenter's
   restart; the candidates are a CPU copy sysmem -> the dedicated LINEAR image (the default sysmem is write-combined: reads
   are slow, about 75 MB/s; this wants a cached opt-in, 15.5) through the existing bind and flush, or asking dxgkrnl to recreate the primary.
4. **DWM into an RM primary** (15.8): the host's Venus import of RM-export memory by resource id, the
   STANDARD identity not carrying the FOREIGN flag, `memory_type_index` 0.
5. **The standard buffers** (15.2): worth moving once 1 and 4 are answered; the cost is the Present-buffer
   registration in an RM arm.
6. **The release event** (15.7, 15.15): decided: the flip never waits, the close of a replaced primary does. Open: whether
   the host really sends `ScanoutReleased` for a level 5 flip's replaced buffer on this viewer (checklist step 9),
   and whether a user source that replaced our buffer should enter the flip log (today the close then does not wait).
7. **Host size rounding**: RM may report a larger `size` than asked (the spike asked 64 KiB multiples); the KMD
   adopts it, and the host's "size <= object" check is then against the adopted value. Checked in 15.16 (the GEM
   import and the resource import use the one adopted size).

### 15.15 The merged state machine: level 5 on the v315 line

Level 5 was built against v314 and meets, in this merge, four things built apart from it: the scanout release
event and its book (`virtio/scanout_release.rs`, `kmd_logic::scanout_release`), user sources with the S4 fence
queue (13.12), the RM client levels 3 and 4, and the foreign-resource table of S6 / `RM_RESOURCE_IMPORT`. This
section is the contract between them, in the shape of 13.12.

**Who runs what.**

| actor | thread / IRQL | touches |
|---|---|---|
| `CreateAllocation` of the primary | dxgkrnl's thread, PASSIVE, no lock | `sysmem::try_create_primary`: `STATE` (leaf), the service's own RM client, the foreign table, the Venus window |
| `SetVidPnSourceAddress` (the display hook) | PASSIVE, under the scanout lifecycle lock | `sysmem_flip::program`: `TARGET` (leaf), `active_scanout_*`, the leases; sends nothing, waits for nothing |
| `DestroyAllocation` / the last close | PASSIVE, no lock | `release_allocation_resource` -> `sysmem::released`: `target_gone`, then `wait_released` (reads the release book, sleeps 2 ms steps, at most 500 ms from the replacing flip), GEM close, `RM_FREE` |
| user `SET` / `PRESENT` / `RELEASE`, `Close` | the app's escape, PASSIVE | the arbiter `STATE`, the fence queue, the pump: unchanged by level 5 |
| fence fired, `ScanoutReleased` | DPC | the fence table; the release book (`BOOK`, leaf), which wakes the HPD worker for a KMD-owner flip (a level 5 worker pass then finds nothing owed) |
| the refresh gate (`foreign_scanout_suppresses`) | PASSIVE under `scanout_mutex` | raises `FRAME_EDGE` when the resident source is on screen (the withheld desktop flush IS the frame edge) |
| lapse poll, the pump, `rm_client::service` | HPD worker, PASSIVE, in that order | at level 5 `service` goes to `sysmem_flip::service` and nothing else of the ring client runs |

Locks: `STATE` (service), `TARGET`, `PRES`, `FLIPS` are leaf spinlocks over plain data, never held across a host
message, a wait or another lock, and never nested. The release book's `BOOK` is a leaf too; `wait_released` takes it
(through `is_done`) for one read at a time and holds nothing while it sleeps. The v315 order `virtio_lock` ->
`BOOK` (the DPC) is untouched. `program` runs under the lifecycle lock and takes only leaves.

**Levels 3 / 4 against 5: who claims what.** `KmdRmClient` is read once per transport generation
(`KNOB_LEVEL`, reset by `forget`), so within a generation exactly one of two worlds exists:

| | levels 1 to 4 | level 5 |
|---|---|---|
| RM client | the ring client `CLIENT`, driven by the HPD worker's steps (11 bring-up steps, surfaces, views) | the service's OWN client (`sysmem::STATE`), brought up once on the creator's thread; `rm_client::service` returns before it would step `CLIENT`, so `CLIENT` stays `Cold` |
| KMD owner's handles (`DeviceOwner::KMD_RM`, 128) | the ring client's | three of the service's plus one transient export file per creation; the two sets never coexist |
| foreign records of the KMD owner | level 4's ring surfaces | the primary's sysmem record (`sysmem` bit) |
| worker passes | `rm_present::service` (twice a pass) and the client's steps | `sysmem_flip::service` (one call) |
| shared statics | `FRAME_EDGE`, `RESUME_EDGE`, `WAKE_AT` (`rm_present`) | the same three, through `take_edges` / `set_wake_at` / `clear_wake_at`; `PRESENTER` is not used |
| counters | `Rm*`, `RmP*` | `RmKnob`=5 and `RmSys*` (`publish_counters` returns after them) |
| `ring_level_on` | true at 3 and 4 only | false: the ring's desktop-changed flag is not raised (a resume needs no frame, the memory is live) |
| a value above 5 | counts as 5 (before this branch, above 4 counted as 4: a service key left at 5 or more now selects level 5) | |

A knob change takes effect with the next transport generation (`reg add` + `pnputil /restart-device`). `forget`
(from `retire_transport`) clears both worlds (`CLIENT`, `rm_present::reset`, `sysmem::forget`, which resets the
service, the target, the presenter and the flip log), so a generation that changes level starts cold and no
state of the other level survives.

**The state of the screen's source at level 5.** The arbiter's `State` and the resident registration are 13.12's;
level 5 adds the target (which RM primary is shown), the presenter of one surface, and the flip log.

```text
               program(RM primary)               worker: Register, first flip
 Venus desktop ───────────────────► target set ───────────────────────────────► Active(resident)
      ▲                                                                           │      ▲
      │  program(Venus allocation) / the shown primary destroyed /                │      │ the user source ends:
      │  three refused flips: worker Withdraw, the arbiter owes one               │      │ resume_owed -> RESUME_EDGE
      │  Venus desktop flush                                                      │      │ -> Reflip{0}, the SAME GEM
      └───────────────────────────────────────────────────────────────────────────┤      │
                                                         user SET (Preempted)     ▼      │
                                                                              Active(user)
     any state ── transport reset ──► forgotten (service, target, presenter, flip log, release book, arbiter)
```

**What each event does** (`flip` = `present_within(KMD_RM, drm, gem)`, direct, never queued behind fenced presents):

| event | arbiter | release book / flip log | screen |
|---|---|---|---|
| first RM primary programmed | resident registered by the worker | flip entered and marked sent; log: current = (gem, seq) | the primary, by flip |
| desktop refresh withheld by the gate (frame edge) | unchanged | a re-flip of the SAME buffer: the older flips of it are superseded (done; the book wakes the HPD worker for it, and the pass finds nothing owed), the newest stays live for as long as it is shown; log: current's seq moves. **No wait** | re-flip, paced to the mode's refresh (15.16) |
| new primary programmed (mode change) | resident layout updated in place (generation kept) | flip of ANOTHER buffer: the old buffer's newest flip starts ageing and awaits `ScanoutReleased`; log: previous = (old gem, its seq, now), current = new. **No wait** | the new primary |
| the replaced primary destroyed | unchanged | `close_wait(old)` = Replaced: `wait_released` holds the GEM close until the book says done or 500 ms after the replacing flip (`RmSysRelWait` / `RmSysRelTmo`), then GEM close, RM free; log forgets it | unchanged |
| the shown primary destroyed | worker withdraws (target gone first); the arbiter owes one Venus desktop flush | `close_wait` = Free: no wait (the shown buffer is never released); the book's entry for the closed GEM is left to age | the Venus desktop (when a Venus primary exists) |
| a Venus allocation programmed | worker withdraws (`other_source`) | log untouched | Venus path, byte for byte v315 |
| user `SET` (Preempted) | resident parked | nothing | the app; the presenter sees no foreground and idles; a flip in flight mints no flip (`NoSource` = yielded) |
| user `PRESENT` / `RM_FENCE` | S4 queue of the user's entries (8 deep, unchanged) | user flips entered in the book as in v315; the host's release of OUR buffer, replaced by theirs, is matched by (handle, gem) | the app |
| user source ends | `take_resume_owed()` true: `RESUME_EDGE` | our re-flip: the same buffer again, so the log is unchanged | the primary: **a re-flip, not a Venus flush and no copy**; `FsRest` unchanged |
| three refused flips (`RmSysGaveUp`) | resident withdrawn | log untouched | stays on the last frame; the presenter starts over 5 s later (no Venus image to fall back to) |
| transport reset | everything forgotten | `scanout_release::reset()` (tracking off until `StartDevice`), `sysmem::forget()` | cold start |

The composition rules, stated so a review can check them: a user source always wins scanout 0 and level 5 never
sends while one holds it; the S4 queue holds only user entries (the pump drops entries of a dead generation), so a
level 5 flip never queues and never waits behind a fence; the resume is a re-flip of the shown primary and needs
no copy and no Venus flush; the flip never waits for a release (a ring of one has nothing to wait for), the GEM
close of a replaced primary waits at most 500 ms; the primary's foreign record is mappable only because the KMD's
own service marked it `sysmem` between import and adoption (a user-mode `IMPORT_RM` or `RM_RESOURCE_IMPORT`
creates records with the bit off and no escape can set it), so S6 sharing and the user imports of v315 are
unchanged.

**With `KmdRmClient` below 5 the primary's creation, flip and teardown are the v315 paths.** Every hook level 5
adds, read as a diff against the v315 tip (`50d0698`), and what it does with the knob off:

| file | the hook | knob < 5 |
|---|---|---|
| `ddi/create_allocation.rs`, `build_backing`, `KmdLinearPrimary` arm | `if let Some(rm) = sysmem::try_create_primary(..) { return Ok(..) }` before the unchanged Venus `match` | `route(level, ..)` answers `KnobOff` and the call returns `None` before anything is counted or sent: one `knob_level()` (an atomic load; the first call of a transport generation does the registry read the HPD worker makes anyway) |
| `ddi/create_allocation.rs`, `create_one`, the `Cached` flag | `adapter.alloc_cached() && (placement.cached \|\| rm_primary_cached)` | `rm_primary_cached` needs an adopted primary AND a live slot of the service: false, so the condition is v315's `alloc_cached() && placement.cached` |
| `ddi/display.rs`, `program_vidpn_source_inner`, after the extent check | `sysmem_flip::program(..)`; `NotOurs` -> `other_source(..)` | both return at their first line on `!sysmem_level_on()` (one relaxed load); the match falls through to the v315 code, which is unchanged |
| `virtio/ctrl.rs`, `release_allocation_resource` | `sysmem::released(..)` at the end | returns on `LIVE == 0` (one relaxed load) |
| `virtio/gpu/resource_tables.rs`, `blob_map_begin` / `blob_remap_begin` | `foreign.contains(id) && !foreign.cpu_mappable(id)` | `cpu_mappable` is the record's `sysmem` bit, which only the service sets: false for every record, so the refusal is v315's |
| `kmd_logic/foreign_resource.rs` | `Entry::sysmem`, set to `false` by the one constructor; `mark_sysmem`, `cpu_mappable`, `sysmem_source` | never set |
| `virtio/rm_client.rs` | `ring_level_on` is `3..=4` (was `>= 3`, with the knob capped at 4: the same set); `read_knob` caps at 5 (was 4); `service`: level 5 goes to `sysmem_flip::service`; `publish_counters`: level 5 prints its own; `forget`: `sysmem::forget()` | levels 0 to 4 behave as before; only a value of 5 or more (not 4) changes meaning |
| `virtio/rm_present.rs` | `take_edges`, `set_wake_at`, `clear_wake_at` | new functions, no existing line changed |
| `diag.rs` | the `KmdRmSysCache` knob name and the `KmdRmClient` comment | none |

**What is deliberately not shared.** Level 5 does not use the level 3 presenter's release waits
(`back_released`, `RelRWaits`): their only caller is the ring. It does not queue behind the S4 fences. It does not
touch `host_bound_scanout_resource`: a Venus flush of the RM primary is refused loudly (`RfUnb`) rather than sent
for a resource with no scanout, which is also why the suppression gate (checked before that refusal) is what keeps
the desktop flush away while the resident source is on screen.


### 15.16 Re-flipping on change: the edges, the pacing, the dirty-unknown window, the size rule, the opener (decided)

Written on `kmd/level5-refresh` against v317. The pure decisions are `kmd_logic::rm_refresh` (20 tests) and the
presenter's `set_min_interval`; the I/O is `sysmem_flip::primary_changed` / `service`. Not built into a driver, never
run; 15.12's rules about what is verified apply.

**The host fact this answers.** The host viewer commits (attach, full damage, `wl_surface_commit`) only when it
receives a `ScanoutFlip`: it does not re-sample every vsync and there is no damage message. A flip that names the
SAME resource again is the cheap refresh (the PRIME export is cached by `(owner, handle)`, the viewer reuses its
`wl_buffer` when inode and geometry match: a dup, one `sendmsg`, an `lseek`, one compositor commit; Venus does the
same for every `RESOURCE_FLUSH` of its scanout resource). So CPU writes into the sysmem primary show only if a flip
follows them.

**What the Venus path relies on (read, v317).** Nothing watches the bytes. A dirty edge exists only where the driver
is told: (1) `SetVidPnSourceAddress` (the MMIO flip contract; the DMA-flip contract arms the same programming at
submit): the dedicated-LINEAR fallback queues a GPU copy primary -> image whose completion DPC sets
`scanout_refresh_pending` (`ScanoutNotify`), the direct bind arms `arm_bind_refresh`; (2) the UMD's present marker
(`HeliosPresentRefreshCmd` / `HeliosPresentRenderCmd` in `DxgkDdiRender`, `arm_present_marker_refresh`): identity-free
or naming the active resource it is queued at once (`QueueImmediate`) and is promoted by the used-ring DPC when its
Venus boundary retires (`take_ready_scanout_refresh`), else it waits for the buffer's bind; both set
`scanout_refresh_pending` and wake the HPD worker, which runs `queue_active_scanout_refresh` (gate, ownership gate,
`RESOURCE_FLUSH`); (3) the restore after a user source ended. There is NO dirty-rect mechanism: nothing reads
`DXGKARG_PRESENT` move or dirty rects (grep: none); the D4b "snapshot" is a UMD image substituted as the bind
target, not a damage record; the read ledger counts host reads per resource to protect buffer reuse, it does not say
what changed; `WddmDirtyRects` does not exist. The `HeliosPresentRefreshCmd` that `dxgkddi_present` itself writes
into its DMA buffer has no consumer (`SubmitCommand` reads only the private data): it is a record, not an edge.
When GDI (or dxgkrnl's software cursor) writes the primary through the CPU aperture mapping and no present follows,
the Venus path does nothing at all.

**The edges, every kind** (`rm_refresh::Edge`; "level 5 today" is v317 before this change):

| edge | source | context | level 5 today | now |
|---|---|---|---|---|
| `SetVidPnSourceAddress` of the RM primary (each MMIO / DMA flip, a mode change, the same allocation again) | `program_vidpn_source_inner` -> `sysmem_flip::program` (the DIRQL half only defers to the worker) | worker, PASSIVE | **yes** (`note_frame_edge`) | yes, `Edge::Programmed`; `SHOWN_RESID` is published here |
| UMD present marker naming the primary or nothing; the bind of a completed present; a refresh retried after `Busy` | `arm_present_marker_refresh` / `take_ready_scanout_refresh` -> `scanout_refresh_pending` -> worker `queue_active_scanout_refresh` -> the suppression gate | marker DISPATCH/PASSIVE, ready edge DPC, gate PASSIVE | **yes**, through the gate (`foreign_scanout_suppresses` raises the frame edge) while the resident source is registered; an edge that arrives unregistered is dropped (`RfUnb`) and covered by the registration's first frame | unchanged (counted as `RmSysEdRef`, the rest of the total) |
| bind edges (`arm_bind_refresh*`, the DPC fast bind) | display.rs, interrupt.rs | DPC / worker | n/a: an RM primary is never bound (`program` returns first) | n/a |
| the restore after a user source ended | `foreign_scanout_restore_desktop` | any | **yes**: `RESUME_EDGE` -> `Reflip` (resident owed), else the gate edge | unchanged |
| `DxgkDdiPresent` Blt into the primary | `dxgkddi_present_inner`, legacy arm | PASSIVE | **no** (nothing arms a refresh from the Present itself) | yes, `Edge::PresentBlt`, after the fence wait. **Fires only once the blit can target the RM primary (below): today the arm refuses it first** |
| two-phase windowed blit into the primary completes | `service_windowed_blt`, after the mirror | HPD worker, PASSIVE | **no** (Render skips the marker for a snapshot blit; the completion arms nothing) | yes, `Edge::WindowedBlt` (same caveat) |
| `DxgkDdiPresent` Flip | `dxgkddi_present_inner` flip arm | PASSIVE | via `SetVidPnSourceAddress`, the first row | unchanged |
| `DxgkDdiPresentDisplayOnly` | not registered (render + display driver) | n/a | n/a | n/a |
| ColorFill, MoveRects, rotation in `DxgkDdiPresent` | accepted as no-ops (the driver writes no content for them) | PASSIVE | no content changes | none to flip for |
| a `BuildPagingBuffer` page-in, fill or virtual transfer INTO the primary | `bar_transfer`, `bar_fill`, `bar_virtual_transfer_inner` | PASSIVE | **no** | yes, `Edge::Paging` (the `alloc.resource_id` is compared with the shown one) |
| a paging eviction (primary -> system) | same | PASSIVE | no change of content | none |
| a registration's first frame, a resume | `Act::Register`, `RESUME_EDGE` | worker | yes | unchanged |
| **a CPU write through the aperture mapping with no DDI call** (GDI, dxgkrnl's software cursor, a `Lock`) | none: dxgkrnl maps the aperture once and writes it | none | **no, and none can be seen** | the tail and the opt-in heartbeat, below |

`primary_changed(adapter, edge, resid)` is the one door for the three new kinds and for `Programmed`: it compares
`resid` with `SHOWN_RESID` (one atomic load; an event that names another allocation, the common case, costs
`RmSysEdOther` and nothing else) and raises the same frame edge the gate raises. The decision is
`rm_refresh::judge`, tested for every kind against shown / other / none / level off.

**The CPU write nobody reports: what is possible, and what was chosen.** Detecting it is not possible: the bytes can
only be read back through the write-combined mapping (about 75 MB/s: one 5120x1440 frame is 390 ms, and a sparse
sample misses a caret), the PTE dirty bits belong to dxgkrnl's mapping, and no DDI is called. Two things are
possible, and both are bounded:

1. **The tail (always on).** After a flip that was owed to a reported change, the primary is "dirty-unknown" for a
   moment: GDI drawing that trails the present that started the frame, a write that raced the flip's own read. The
   refresher re-flips at +50, +100, +200, +400 and +800 ms (`TAIL_100NS`, 1.55 s, five flips) after the last
   such flip and then STOPS; a new reported change restarts it, so a desktop that is being presented to never
   reaches it, and an idle desktop has no wake at all after the fifth (`Refresher::next_due` is `None`; tested for
   an hour of passes).
2. **The heartbeat (opt-in, `KmdRmSysPollMs`, default 0 = off).** For a session whose writes never come with an
   event (a GDI-only desktop: safe mode, no DWM), a fixed period (50 to 5000 ms) re-flip while an RM primary is shown.
   It costs a flip per period for as long as the desktop exists, which is why it is not the default: with DWM every
   change comes with a present, a paging write or a marker. Hook for a better answer if the hardware run shows one:
   a periodic poll gated by an "active" proxy would need an input-activity signal the KMD does not have.

Chosen over "poll while the desktop is active": the KMD has no activity signal that is not itself one of the
edges above, so an "active" gate would be the tail again. Chosen over "hook dxgkrnl's present that follows": there is
none for GDI CPU writes (that is the problem), and for DWM the present is already an edge.

**Pacing: at most one flip per vblank at the current mode rate.** `adapter.effective_refresh_mhz()` (the committed
VidPn target mode's rate, else the host EDID's preferred, 60 Hz when only a size was given; the value the vsync
heartbeat runs on, without its `VsyncRateMhz` debug override) gives the period through `vsync_deadline::period_100ns`; `flip_interval_100ns` clamps it to
2 ms .. 100 ms and the worker passes it to the presenter (`Presenter::set_min_interval`) before each decision. The
ring levels never call it and keep the 16 ms constant.

| mode | interval (100 ns) | at most per second |
|---|---|---|
| 5120x1440 @ 240 Hz | 41 667 (4.17 ms) | 240 |
| 360 Hz | 27 778 | 360 |
| 144 Hz | 69 444 | 144 |
| 60 Hz, unknown rate | 166 667 | 60 |
| 59.94 Hz | 166 834 | 59 |
| above 500 Hz / below 10 Hz | 20 000 / 1 000 000 (clamped) | 500 / 10 |

Edges closer than the interval coalesce into the one flip that is owed (the presenter holds the frame until
`last_flip + interval` and answers `WaitUntil`), and the LAST edge is always shown by that trailing flip (tested:
an edge every 250 us for 100 ms at 240 Hz gives at most `100 ms / 4.17 ms + 2` flips, never closer than the
interval, and the final edge is flipped). `RmSysCoal` counts the edges that got no flip of their own (edges minus
flips owed to edges), `RmSysIvl` records the interval in use. The worker stays non-spinning: one timed wait, and
the earliest of the presenter's pacing deadline and the refresher's next moment wins (`set_wake_at_min`);
`next_due` never returns a moment that is already due, so a wake that the pass could not serve cannot repeat. The
flip itself is a synchronous round trip on the worker (up to `FLIP_TIMEOUT_MS`), measured from its end, so a slow
host lowers the rate by itself. The timed wait is `foreign_scanout_wait_100ns`'s: at least 1 ms and at most 1 s
(a longer moment, the 5 s heartbeat, is reached in 1 s steps), and the kernel rounds a timeout to its timer
resolution, so with a lone edge the trailing flip of a 240 Hz burst can be late by up to a timer tick (15.6 ms at the
default resolution); under a stream of edges every edge wakes the worker and the flip lands within one edge
spacing of its due time.

**The release book under repeated flips of one buffer** (read, and tested against the REAL `ReleaseBook`:
`hammering_one_buffer_with_re_flips_leaks_nothing_and_never_moves_the_floor_wrongly`, 20 000 flips at 240 Hz).
Each flip is entered (`minted`) and marked (`sent`); `sent` finishes the older flips of the same `(handle, gem)`
(`superseded`), so at most two entries of the source are live (the shown flip, and the one in flight). The book is
a fixed array of 32 recycled in place (the oldest `Done` entry is overwritten): it never grows and never evicts a
live entry (`RelEvict` stays 0). The floor of the source is `first_live - 1`, rises by exactly one per flip and is
never above the live flip (the shown buffer is never released, so a wait for it would last its limit: the flip never
waits, `release_tracked = false`). A mode change flips another GEM; the host's one `ScanoutReleased` for the old
buffer retires its newest seq, a late duplicate matches with 0 newly done, and a flip the host refused is `gone`
(done) without holding the floor. `FlipLog` (the GEM close's rule) keeps the shown buffer's `Free` through any number
of re-flips. Nothing to fix.

**The size rule (checked against the host's answer).** `lseek(SEEK_END)` of the dma-buf is the bound (larger: ERANGE;
equal or smaller passes), MAPPABLE needs `page_align(size) <= hostmem_len`, `SET_SCANOUT_BLOB` needs
`offset + stride * height <= size`, and a size rounded up to 64 KiB passes only if the GEM import used the same
rounded size. `sysmem.rs` takes ONE number after the `RM_ALLOC` reply, `size = rm_sysmem::adopt_size(lay.size,
reported)` (RM's own rounding, page-granular, never below the request), and hands that same variable to
`gem_import` (the NVKMS import's `mem_size`), to `foreign_layout` (the layout is proven to fit it), to
`validate_request` / `foreign_begin_kmd_import` and to the resource creation (`alloc_blob_errno_within`); the
record, `adopted_size` and every later user (VidMm, the aperture count, the blob mapping) read the record's copy of
it. The RM request itself carries `lay.size` (the picture, page-rounded), which RM may only round UP; the dma-buf is
what RM made, so the adopted size is never larger than it. No code change was needed;
`one_rounded_size_serves_the_gem_import_the_import_and_the_flip` pins what the number is for 1920x1080,
1896x1030, 5120x1440, 1366x768 and 3840x2160 with RM answering 0, the request, the request rounded to 64 KiB and
one page more.

**The STANDARD open identity and the Venus-side opener.** What an opener must do with the RM primary: it is a
dma-buf of system memory, not Venus memory, and the Vulkan import must be an image with
`VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` and an explicit layout (the same path the KMD's own foreign copy takes,
`virtio/venus/foreign_copy.rs`):

| field | value for the level 5 primary | where it is |
|---|---|---|
| modifier | `DRM_FORMAT_MOD_LINEAR` (0): the primary is pitch-linear | implied (the KMD creates nothing else); the trailer's `modifier` when present |
| plane 0 offset | 0 | meta `plane_offset` |
| row pitch | `width * 4` rounded up to 256 | meta `pitch` |
| extent, format | the mode's, BGRA / BGRX / RGBA | meta `width` / `height` / `dxgi_format` (fourcc: 28 `AB24`, 87 `AR24`, 88 `XR24`) |
| memory size | the host-verified adopted size | identity `venus_alloc_size` (= `blob_size`) |
| memory type index | not consulted (0 in the identity): the importing device picks it | |
| attach | by resource id to the opener's Venus context (the host confirms any context may attach an RM-export resource) | identity `resource_id` |

Done here (small, additive, safe): the KMD now tells a STANDARD opener that the allocation is that dma-buf. The
STANDARD identity's `reserved[0]` gets bit 1, `HELIOS_WDDM_STANDARD_CONTRACT_FOREIGN_SYSMEM`
(`HeliosWddmOpenIdentity::foreign_sysmem_primary()`), written by `write_open_identity` from the KMD's own record
(`ident.foreign`: set at creation and at an open that hit the foreign table, never from creator data), beside the
dedicated-buffer bit 0 which a primary never has. The DEVICE_MEMORY FOREIGN flag could not be reused (same bit
value, other kind), the identity version stays 2 and a reader that knows only bit 0 sees what it saw before.
`protocol` tests pin the bit, the kind discrimination and that the other readers are unmoved.

**Not done, and exactly what is missing:**

1. **The layout trailer for a STANDARD allocation.** The trailer is at private-data offset 96 and needs 128 bytes;
   `GetStandardAllocationDriverData` reports 96 for every standard type (`PRIV_SIZE`), so `write_foreign_layout_trailer`
   finds no room (`FgOpNoRm` counts it, the adoption is told room exists because the KMD is the creator). Reporting
   128 for `SHAREDPRIMARYSURFACE` alone is a one-line change but alters the private-data size of the boot primary
   for every configuration; not needed, because the table above is derivable from the meta, and not done blind.
2. **The UMD.** `umd/src/forward/resource.rs` `open_resource` imports every STANDARD identity as an ordinary OPTIMAL
   opaque-fd image (`open_texture2d`, "the .38 regression" comment forbids DRM-modifier rebuilds for DWM imports).
   It must branch on `foreign_sysmem_primary()` to the modifier import with the table above. Out of this branch's
   scope (`umd/` is not touched); until it does, a DWM process that opens the primary imports the wrong shape.
3. **The KMD's own blit into the primary** (answered since by the CPU-copy fallback of 15.17: the paragraph below
   describes the failure it removes, and the `PresentDestinationDesc` arm it proposes is still the GPU alternative).
   The Present Blt arm treats a `PitchedStandardBuffer` destination as a
   registered Present buffer: `begin_present_buffer_write_legacy` answers `NotFound` for the RM primary (it has
   `SystemBackingPolicy::None`, no `present_buffer_syncs` slot), so `PBOwn 0xE1` and `STATUS_DEVICE_NOT_READY` come
   first; the two-phase path would strand a request whose `complete_present_buffer_gpu_write` is false. The fix is a
   `PresentDestinationDesc` arm for a foreign destination (a modifier image with `TRANSFER_DST`, imported like
   `new_foreign_dma_buf`) selected by `PresentAllocInfo::foreign`, which the open path sets from the trailer (point 1)
   or would set from the identity bit. That is Venus-client work I cannot compile or test here. Until it exists the
   `PresentBlt` and `WindowedBlt` edges have nothing to fire for, and DWM composes into Venus buffers, which
   programs a Venus allocation and withdraws the RM source (the first row of the 15.15 table).

**IRQL and locks of the new code.** `primary_changed` takes no lock and does no I/O: atomics, `KeSetEvent(Wait =
FALSE)` through `signal_hpd`; legal at any IRQL up to DISPATCH, and used at PASSIVE only today (the present DDI,
`BuildPagingBuffer`, the HPD worker). The edge that already came from a DPC path (the marker's ready edge,
`request_scanout_refresh_for`) is unchanged: it sets `scanout_refresh_pending` and the event, and the gate (PASSIVE,
under `scanout_mutex`) raises the frame edge. `PRES` (now with the refresher) is still a leaf spinlock never held across
I/O; `read_poll_ms` is a registry read on the worker (PASSIVE), once per generation. `publish_counters` takes `PRES`
for one read (PASSIVE). No hook runs under `virtio_lock`.

**Hardware checklist additions** (after 15.13 step 10): 11. with a GDI-only session (safe mode) at
`KmdRmSysPollMs` 0, type into a GDI window: expect the screen to show it only within the tail after an event, and
`RmSysEdges` flat otherwise; set the knob to 100 and expect `RmSysPoll` to grow at 10 per second and the typing to
show. 12. at 5120x1440 / 240 Hz, drag a GDI window: `RmSysIvl` 41667, `RmSysFrames` at most 240 per second,
`RmSysCoal` large, the picture current at the end of the drag (the trailing flip). 13. idle for a minute: `RmSysFrames`,
`RmSysTail` and `RmSysPoll` do not move after the tail (the worker sleeps). 14. a DWM session: which of
`RmSysEdProg` / `RmSysEdRef` / `RmSysEdPag` / `RmSysEdBlt` grow, and that `RmSysEdOther` dominates (the counters are
the answer to "which edges does a real desktop produce"). 15. the cost of a refresh flip on the real viewer (CPU of
the compositor at 240 flips per second of an unchanged picture).

**Untested here, all of it**: that any edge fires on a real dxgkrnl; that the viewer shows a re-flip of an
unchanged GEM (the host's account is code, not a run); the timing of the tail against real GDI traffic; the
heartbeat's cost; the compile of the three hooks in `display.rs`, `build_paging_buffer.rs` and `create_allocation.rs`
(read against the real definitions; the files `sysmem_flip.rs`, `rm_present.rs` and the rest of 15.12's list were
type-checked in the generated harness); that the STANDARD identity's new bit is ignored by the shipped UMD.

### 15.17 The CPU-copy fallback for a Blt into the level 5 RM primary (built; compiled by nothing, run by nothing)

Status: written on `kmd/level5-blt-cpu2` over v318 (`5fe6e8d`), after `kmd/level5-blt-cpu`'s design notes (the earlier
text of this section). A Present Blt whose destination is the RM system-memory primary no longer fails with
`STATUS_DEVICE_NOT_READY`: it is copied by the CPU, or counted and skipped with success. The pure half is host-tested
(`kmd_logic::rm_blt`, 39 tests); the I/O halves were type-checked against generated harnesses (15.12); nothing was
compiled as a whole or run. This closes open point 3 of 15.14 only for the KMD's own Blt arm (the DWM import,
15.8, is another question) and "Not done" item 3 of 15.16 for its first sentence; the `WindowedBlt` edge still has
nothing to fire for.

**How the Blt present works for an ordinary (Venus) primary** (read, `ddi/display.rs` `dxgkddi_present_inner`).
`DxgkDdiPresent` is documented PASSIVE_LEVEL. `DXGKARG_PRESENT` carries `DstRect` and `SrcRect` (`RECT`),
`SubRectCnt` / `pDstSubRects` (sub-rectangles in destination space) and the two allocation-list entries; it has no
move rectangles (those are `DXGKARG_PRESENT_DISPLAYONLY`). The KMD's Blt arm (flag bit 0) reads NONE of the rects: it
requires source and destination to have the same extent and copies the whole surface. The source must be a
`DEVICE_MEMORY` allocation (anything else is `PBCpy 0xE6`, `STATUS_INVALID_PARAMETER`); the destination's
`PresentAllocationStorage` picks the copy:

| allocation | `kind` / `storage` | as a source today | as a destination today |
|---|---|---|---|
| Venus-backed OPTIMAL D3D11/DXVK image (GPU-only tiled memory) | `DEVICE_MEMORY` / `OptimalOpaqueFdImage` | imported once (`ensure_present_image`), copied by a reusable command | an image-to-image copy or blit |
| direct-scanout image, KMD GDI texture (`KmdOptimalGdiTexture`) | `DEVICE_MEMORY` + DIRECT_SCANOUT, or `STANDARD` + OPTIMAL_GDI_TEXTURE / `OptimalCrossContextImage` | the first only | an image-to-image copy or blit |
| adopted foreign (NVK-on-RM) image | `DEVICE_MEMORY` with a layout record | `new_foreign_dma_buf` | not a destination |
| CPU-visible linear blob (a Present buffer, a GDI surface, the Venus primary) | `STANDARD` / `PitchedStandardBuffer` | refused (`0xE6`) | a registered Present buffer: `begin_present_buffer_write_legacy`, an image-to-buffer copy (`record_reusable_present_blt`), the wire fence waited for, the mirror into system backing, `Edge::PresentBlt` |
| typed WindowedBlt snapshot | a stashed `SnapshotDescriptor` | the two-phase queue (`queue_windowed_blt`, a deferred submit after SubmitCommand admits it) | the same destinations |

The completion the Present path expects: the GPU fence of the copy is merged into the DMA buffer's private data
(`PresentSubmissionPrivate::merge_fence`) so SubmitCommand retires the DMA fence behind it; then the tail writes the
refresh marker into the DMA buffer. The adapter's LINEAR scanout image (`allocate_linear_scanout_image_blob`,
`dedicated_scanout_*`, `prepare_optimal_scanout_copy` / `PreparedImageCopy`) is not part of the Blt arm: it is what
`SetVidPnSourceAddress` copies an OPTIMAL primary into for the viewer. The RM primary has no Venus object at all
(`venus_image_id` and `venus_memory_id` are 0), which is why the legacy arm cannot reach it.

**What was built.**

1. **The hook** (`display.rs`): before the legacy Blt arm, `sysmem_blt::primary(adapter, destination.resource_id)`
   (one relaxed load with the knob below 5; at level 5 `foreign_sysmem_source`: an adopted RM sysmem record). If it
   is `Some`, `present_blt_to_rm_primary` runs INSTEAD of the legacy arm (the old arm is now `} else if present_flags
   & 1 != 0 {`: the only line of the old code that changed; everything else in `display.rs` is added). It first does
   what dxgkrnl's protocol needs and the legacy arm did before any host work (a DMA buffer and private data big enough,
   `validate_patch_capacity`: an insufficient-buffer retry cannot copy twice), refuses a source handle that names no
   allocation (`STATUS_INVALID_PARAMETER`, nothing to read), and otherwise ALWAYS returns success.
2. **Source = a Venus-backed image** (`Source::Image`: the ordinary OPTIMAL image, the cross-context GDI texture, the
   foreign image; the descriptor is built exactly as the legacy arm builds it): `VenusClient::rm_blt_copy_to_stage`
   GPU-copies the whole source into a private LINEAR host-visible BGRA STAGING image (a second
   `allocate_linear_scanout_image_blob`, not the adapter's dedicated one: that one is published as the primary scanout
   (`primary_scanout_*`, `SET_SCANOUT_BLOB`) and sharing it would publish the wrong identity). The reusable command is
   the scanout copy's own (`record_reusable_image_copy`, or `record_reusable_converted_image_copy` through a BGRA
   conversion image when the source's Vulkan format is not B8G8R8A8_UNORM), baked once per (source, stage) pair in
   the same `present_blits` cache as an ordinary Present BLT, so `release_present_blits_for_resource` of the source
   releases it with the rest; the import is the cached `ensure_present_image`; the submit is
   `submit_venus_async_present` with no destination buffer (ring 1, no scanout notify). The GPU copy is the whole
   image (the machinery has no rect-limited command; the reusable command is what makes the steady state one
   enqueue). The caller waits for the wire fence with `wait_fence` (5 s) OUTSIDE the Venus mutex. The stage grows
   (never shrinks) when a larger source appears; a replaced one stays until the Venus context is torn down, bounded
   at 8 allocation attempts.
3. **Source = a CPU-visible allocation** (`Source::Cpu`: a `STANDARD` / `PitchedStandardBuffer` allocation, its
   authoritative pitch, a 32-bit 8-bit-per-channel format): read through its own blob mapping, no GPU, no stage.
4. **The row copy** (both arms): `rm_blt::plan` turns (`DstRect`, `SrcRect`, `SubRectCnt`, `pDstSubRects`) into at
   most 32 rects, clipped to the primary and to what the source has; each rect is cut into row bands of at most
   about 4 MiB of either surface; for each band the source window and the primary window are mapped
   (`map_blob_prepare`, idempotent: the primary is normally already mapped at dxgkrnl's aperture offset, and the same
   pages are viewed), `MmMapIoSpace`d with the cache attribute of the HOST's `map_info` for that blob
   (`map_cache_to_mm`; for the primary that is the attribute it was created and trial-mapped with, 15.5: no alias is
   made), the rows copied with `rm_present::copy_row` (streaming stores where 16-byte aligned), `sfence` on THIS core
   (its write-combined buffers: the flip worker's own `sfence` cannot drain them), unmapped. A pixel-order mismatch
   (an RGBA source into an `ABGR8888` primary or the reverse) is a byte swap of each pixel (`rm_blt::swap_rb_row`).
5. **Then**, if any bytes were written, `primary_changed(adapter, Edge::PresentBlt, primary)` once per Present: the
   re-flip is scheduled by 15.16's machinery. The GPU copy's fence, which is already complete, is merged into the
   private data (a record that names no pending work, instead of one a recycled DMA buffer left); a Present with no GPU
   fence merges 0.
6. **Never fail** (`rm_blt::Skip`, `RmSysBltWhy` is the last code): 1 layout (the primary is not a 32-bit format of
   known order, or a surface does not hold its rows), 2 snapshot, 3 source format, 4 source kind, 5 source is the
   destination, 6 no memory for the stage or the cache, 7 GPU copy not submitted, 8 GPU copy not complete in 5 s, 9
   staging image busy for 2 s, 10 source not mappable, 11 primary not mappable, 12 the rect arithmetic found a layout
   that does not hold the copy, 13 no Venus client. Each is a counted success with a stale picture. A typed WindowedBlt
   snapshot source is skipped (2): its content is ready only when its stream boundary is, which a synchronous copy
   cannot wait for; the two-phase path has no RM destination (open question 2).

**The rect rules** (all tested in `rm_blt`): an EMPTY `DstRect` (all zero, inverted) is the whole primary (the legacy
arm never read a rect, so an unset one must keep meaning that); a `DstRect` partly outside is clamped, entirely
outside copies nothing and succeeds; `SubRectCnt` 0 is the whole clamped `DstRect`; otherwise each sub-rect is clipped
to it and the survivors copied (all empty or outside: nothing, not everything); more than 32 survivors, or a count
over 4096, fall back to their bounding box / the whole `DstRect` (more bytes, never fewer); `SrcRect` places the
source by its origin delta from `DstRect` when both are non-empty and the same size, otherwise the source is read at
the same coordinates (what a full-surface copy did); everything is clamped so no byte outside either surface is
addressed (`Surface::valid`, a last proof in `finish`). 5120x1440, pitch 20480: a full frame is 29,491,200 bytes (28
MiB), one 400x40 text line 64,000.

**Counters** (`RmSysBlt*`, mirrored by `sysmem::publish_counters`, which `publish_nvrm_counters` reaches through
`virtio::rm_client::publish_counters` at level 5; written once the fallback ran; names at most 14 characters):

| value | what | healthy |
|---|---|---|
| `RmSysBltCpu` | Presents the fallback answered with a copy (also one with nothing to copy) | grows with the Blt presents |
| `RmSysBltBytes` | cumulative MiB of pixels written to the primary (a skipped Present adds what it wrote before it stopped) | grows with the dirty area |
| `RmSysBltUs` / `RmSysBltMaxUs` | cumulative microseconds of the whole fallback (the GPU wait included) over the copied Presents / the slowest one (both saturate at `u32::MAX`) | the average (`Us / Cpu`) is the cost of a Present; a max near 1 s is a full-frame write at 28 MB/s |
| `RmSysBltSkip` / `RmSysBltWhy` | Presents answered with success without a complete copy / the last reason (above) | 0 / 0 |
| `RmSysBltStg` | the staging image's resource id, written when it is made (`0x80000000` plus the attempts once the 8 are used up) | one id per boot |

`PBCpy` is 3 after a CPU copy and 4 after a skip (written when the value changes: no per-Present registry write).

**Locking and IRQL.** PASSIVE (the Present DDI). The Venus mutex is taken ALONE, for the submit, exactly as the legacy Blt
arm takes it (it takes no scanout mutex: the lock order scanout -> Venus -> virtio is untouched, and nothing here
needs the first); the fence wait, the mapping and the row copy run with no lock and no spinlock held. The only
thing held across them is `STAGE_BUSY`, an atomic flag serializing users of the staging image (two Presents on two
contexts must not share the stage between their GPU copy and their read); a Present that finds it taken sleeps in
1 ms slices for at most 2 s and then skips (9). `primary_changed` takes no lock (atomics, `KeSetEvent`).

**Cost model.** Per Present: the rects, not the frame, on the CPU; the GPU copy of the whole source into the stage
(GPU time, one enqueue and one fence wait: the Present thread blocks for it, as the legacy arm does for a Present
buffer). Reads from the stage and writes to the primary run at the mapping's speed: the host measured write-combined
reads at about 75 MB/s and writes at about 28 MB/s, so a full 5120x1440 frame is about 0.4 s to read and 1 s to write
(rows are read and written one after the other, so the two add), a 400x40 line about 2 ms, and each band costs two
`MmMapIoSpace` / unmap pairs (about 4 MiB each at most). The stage's memory is host-visible Vulkan memory: whether its `map_info` is cached (fast
reads) or write-combined is the host's (`RmSysCache` for the primary, the same nibble logic). A Present that carries
no rects (DstRect unset) is a full-frame copy: `RmSysBltBytes / RmSysBltCpu` is the measure of whether real
Presents carry rects.

**Verified here:** `kmd_logic::rm_blt` against a model of the row loop (39 tests: empty and inverted rects, rects
partly and entirely outside, extreme `RECT` values, `SubRectCnt` 0, a list longer than 32 and over 4096, a short
list, the source placement and its clamps, pitch 20480 at 5120x1440, the band split covering every row once, map
windows page-aligned and inside the mapping, the byte order of every format, the swap, the accounting); the I/O
halves type-check (borrowck included) in three generated harnesses whose stubs carry the REAL signatures cut from the
real sources: `sysmem_blt.rs` as a real child of `rm_client.rs` (module visibility from the real `mod` lines), the new
`present.rs` block against stubs of the `VenusClient` members it uses, and `present_blt_to_rm_primary` plus the real
hook text (cut unchanged out of `dxgkddi_present_inner`) in a function whose parameters have the types the real locals
have. `git diff -w` of `display.rs` against v318 removes one line (the `if present_flags & 1 != 0 {` that became the
`else if`): the Venus path is byte-identical for every destination that is not an adopted RM primary.

**Not verified by anything, and open:**

1. That `kmd_render` compiles (as before: bindgen types, `DXGKARG_PRESENT`'s real field types), and that any of it
   runs: the GPU copy into a second LINEAR image, `map_blob_prepare` of the stage, `MmMapIoSpace` sub-ranges, the
   fence wait, the swizzle, the cost numbers.
2. **The source-kind mapping is read from the allocation identity, not run.** A STANDARD `OptimalCrossContextImage`
   source (the KMD's GDI texture) was never a legal source of the legacy arm; here it is imported as a
   cross-context dma-buf image exactly like a destination of the same kind is. Whether the host accepts it as a
   source, and whether DWM/GDI Presents ever name a CPU-visible STANDARD source, is for the run (`RmSysBltWhy` 3, 4,
   7 and 10 are the signals).
3. **WindowedBlt snapshot sources are skipped.** At level 5 a DWM present that carries a snapshot into the primary
   leaves the previous picture; the fix is the two-phase path with an RM destination (a `PresentDestinationDesc` arm
   that finishes with this copy after the producer boundary), which is Venus-client work with no destination object
   to hang it on.
4. **A CPU-visible source that is not resident.** The Present DDI runs before residency is effective
   (`SubmitCommand` admits it): a pitched source paged out to system memory is read from its blob, which is stale. The
   legacy arm has the same ordering for a GPU source.
5. **The primary's mapping can move.** The view is the host's `map_info` of whatever offset the blob is mapped at; a
   remap by `MapCpuHostAperture` while a band is mapped is not excluded (the paging copies hold a content mutex that
   this path does not take).
6. **The staging image is whole-image and grows only**; a replaced stage and the blits baked against it stay until
   teardown (bounded: 8 attempts, the blit cache's own ceiling).
7. **The `ABGR8888` (RGBA) primary and RGBA sources** go through a byte swap that was tested only on vectors.

**Hardware checklist** (after 15.13 step 13, only once steps 2 to 5 pass: the picture and its flips are proven
first). 13a. **A Blt workload at level 5:** run an app that presents by Blt (a windowed DXVK/D3D11 swapchain composed by
the legacy path, or a GDI redirection test) for a minute. Expect `RmSysBltCpu` growing with the Blt presents,
`RmSysBltSkip=0` (otherwise `RmSysBltWhy` names the arm), `PBCpy=3`, no `PBCpy 0xE5` and no `STATUS_DEVICE_NOT_READY`
from `PBRet`, `RmSysEdBlt` growing with `RmSysBltCpu`, the window's content visible in the viewer, and
`RmSysBltUs / RmSysBltCpu` in the low milliseconds (a full-frame average is the sign that Presents carry no rects:
read `RmSysBltBytes`). 13b. **A window drag with a Blt app:** the frame rate of the app against `RmSysBltMaxUs`; if
the rects are the whole frame the CPU copy bounds the frame rate at about one per second and the answer is the cached
opt-in (15.5) or the Venus fallback of 15.14 point 3. 13c. **Two apps at once:** no `RmSysBltWhy` 9.


### 15.18 Option B for foreign allocations (`ForeignFlip`: the flip of an allocation a user-mode device imported; built, compiled by nothing, run by nothing)

Status: written on `kmd/option-b-foreign` against v319 (`dcc265c`). Gated by the service-key knob `ForeignFlip`
(REG_DWORD under `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`, **default 0 = off, and then every path is the
one v319 runs**: one relaxed load in the display hook and in the worker, nothing written to the service key).
Pure logic: `kmd_logic::foreign_flip` (19 tests, 808 in the crate). I/O: `virtio/foreign_flip.rs`. Small hooks in
`display.rs`, `adapter/scanout.rs`, `adapter/foreign_scanout.rs`, `virtio/nvrm.rs`, `ddi/hpd.rs`, `ddi/submit_command.rs`,
`rm_client/sysmem_flip.rs`.

#### 15.18.1 The question, and the plain answer

The question (from the DWM-on-NVK tester): once DWM runs, level 5 shows nothing of DWM's picture, because DWM flips its own
swap-chain buffers. If those buffers are NVK memory adopted by the KMD (`IMPORT_RM`, then a DEVICE_MEMORY allocation with
the FOREIGN identity), **is the KMD's flip of such an allocation the same as the existing foreign scanout path, or does it
need the level 5 presenter?**

Answer. **Neither as they stood, and both pieces are reused.**

* The existing foreign scanout path (`SCANOUT_SET` / `PRESENT`, `virtio/foreign_scanout.rs`) is driven by the NVK process's
  own escapes. A WDDM flip (`SetVidPnSourceAddress`, or the DMA-buffer flip of `PresentFlipPrivate`) never reaches it: with
  DWM forced to WDDM-flip-only nobody calls those escapes, so the arbiter has no source and `program_vidpn_source_inner`
  takes the Venus path (the KMD copy of the foreign resource into the adapter's scan-out image, section 11 of
  `zero-copy-present.md`, or nothing useful for a placeholder).
* The level 5 flip (`rm_client/sysmem_flip.rs`, 15.7) is exactly the hook that was wanted, but it only recognises
  `foreign_sysmem_source`: a record that is the KMD's own RM system memory (the `sysmem` bit, set by the KMD's own service)
  and it registers the arbiter's resident source under the KMD's own owner token and DRM file. A user-imported record is
  `NotOurs` to it by construction (and the record forgot who imported it at adoption: `creator` becomes `None`).
* So the generic flip is a THIRD arm after the level 5 one, with the same shape (no `ScanoutTarget`, no bind, no
  `SET_SCANOUT_BLOB`; the arbiter's resident source; `present_within`; a ring-of-one presenter on the HPD worker) and
  four things the level 5 arm never needed: the importer's token (the flip must name the importer's DRM file, and the
  arbiter proves ownership of it), a record that remembers it, a poison rule for the file number the host may reuse, and
  a class of resident source apart from the KMD's. **It does not need level 5**: it runs at `KmdRmClient` 0, 1, 2 or 5;
  levels 3 and 4 own the resident source with their ring and the arm refuses, counted.

#### 15.18.2 What existed and what was missing

| piece | existed | missing (now built) |
|---|---|---|
| allocation -> record | `foreign_record` / `foreign_layout` (layout, size); `foreign_sysmem_source` (sysmem only) | the importer's token, DRM file, GEM and the record's life for ANY adopted record (`Entry::origin`, `ForeignTable::flip_record`, `VirtioGpu::foreign_flip_record`). `scanout_alloc_info` needs no change: the arm looks the record up by `source.resource_id`, as the level 5 arm does |
| hook in `program_vidpn_source_inner` | the level 5 arm (`NotOurs` for anything else, then `other_source`) | a second arm after it: `foreign_flip::program` |
| arbiter | `resident_set` with any owner token; `present` / `mint` prove the owner's file; ends on `release_handle`, `release_owner`, an invalid handle, a transport reset | the resident accessors could not tell a KMD resident from a user device's: `resident_state_of` / `resident_drop_of` (class-aware), so the level 5 presenter cannot withdraw the other class's source and vice versa |
| presenter | `rm_present::Presenter` with a ring of one, `rm_sysmem::flip_inputs`, the restart pause | a second instance (`virtio/foreign_flip.rs`) with its own edge flag, so the one pass in which the level 5 service stands down cannot take its edge |
| file-close hazard | none (the KMD's own file is its own) | `ForeignTable::file_closed` / `owner_closed` poison every record made from a closed file; hooks in `foreign_scanout_release_handle` / `_release_owner`; the arm refuses a poisoned record and drops a shown one |
| completion / reuse | `ScanoutFlushToken`, the read ledger (Venus flush); the host release book enters every `present_within` flip | decided: the conservative rule (15.18.5); nothing waits |
| gate | none | the knob `ForeignFlip`, counters `Ff*` |

#### 15.18.3 What is built

**The hook.** In `program_vidpn_source_inner`, after the extent check and after the level 5 arm declined:

```text
foreign_flip::program(adapter, source.resource_id, source.primary_address, width, height)
    NotOurs | Refused  -> foreign_flip::other_source()  (forget a previously shown target), Venus path below, unchanged
    Ok                 -> return Programmed
```

and `Programmed::Ok` of the level 5 arm calls `foreign_flip::other_source` first (a KMD primary replaces a shown foreign
allocation). Both flip contracts end here: the MMIO flip (`pDmaBuffer == NULL`: `SetVidPnSourceAddress`, which DWM's
interval-1 presents use) and the DMA-buffer flip (interval 0: `PresentFlipPrivate` rides the DMA buffer, `arm_dma_flip`
stashes the allocation and `process_deferred_vidpn_source_address` calls the same `program_vidpn_source`).

**The decision** (`kmd_logic::foreign_flip::decide`, a pure table; first match wins):

| # | condition | verdict | counted |
|---|---|---|---|
| 1 | knob off | `Off`: the Venus path | nothing |
| 2 | no foreign record (a plain Venus allocation, or a STANDARD placeholder with no identity) | `NotForeign`: Venus | `FfNoRec` |
| 3 | the record is the KMD's own sysmem | `Sysmem`: the level 5 arm's | nothing |
| 4 | `KmdRmClient` not read yet / 3 or 4 | refuse `LevelUnread` (2) / `RingLevel` (1) | `FfRef`, `FfRef02` / `FfRef01` |
| 5 | no transport / display half off / host lacks the RM import (config bits 13 and 10) | refuse `NoTransport` (3) / `NoDisplay` (4) / `HostCap` (5) | `FfRef03` to `FfRef05` |
| 6 | not adopted / destroyed / the importer closed the file (poisoned) / the KMD's own vidmem import | refuse `NotAdopted` (6) / `Destroyed` (7) / `FileClosed` (8) / `KmdOwned` (9) | `FfRef06` to `FfRef09` |
| 7 | the recorded layout is not a `ScanoutFlip` layout (extent under 64, format) / its extent is not the mode's | refuse `BadLayout` (10) / `Extent` (11) | `FfRef10`, `FfRef11` |
| 8 | the importer's `rm_handle` is not that device's DRM file in this transport generation | refuse `OwnerGone` (12) | `FfRef12` |
| 9 | otherwise | `Take(target)`: `{resid, owner = the importer's token, drm = rm_handle, gem, epoch, layout}` | `FfProg` |

`FfWhy` is the last reason's code. A refusal always lands on the v319 Venus path (the KMD copy of the foreign resource,
`ForeignCopy`). The record's layout is mandatory at import and validated against the size, so "layout missing" is the layout
the flip cannot carry (row 7) or a record that does not exist (row 2).

**Taking an allocation** (`take`, PASSIVE, under the scanout lifecycle lock, sends nothing): the target book
(`Change::New`, `Same`, `Moved` = another allocation of the same device, `Reowned` = another device's); the shown
resource id (one atomic, for `other_source`, `target_gone` and `holds_screen`); `active_scanout_resource` / `_wh`,
`publish_bound_primary(primary_address)` and the end of the leases (`Cancelled`), exactly as the level 5 arm; the host is NOT
bound (`host_bound_scanout_resource` stays), so a Venus flush of the foreign resource, if one were ever attempted, is
refused loudly (`RfUnb`); a registered resident source learns the allocation **in place**
(`foreign_scanout_resident_set(owner, drm, epoch, layout)`: the arbiter keeps the generation of a source of the same
owner, so a flip in flight stays valid; a different owner gets a new generation and the old owner's flip is refused
`NoSource`, tested against the real arbiter); a frame is owed (`OWED`, plus the shared frame edge `note_frame_edge`).

**The worker** (`foreign_flip::service`, called in `hpd.rs` right after `rm_client::service`): the level 3 presenter's
state machine with a ring of one and `rm_sysmem::flip_inputs`: register the resident source under the importer's token
(`Act::Register`), flip on a frame edge (`present_within(owner, drm, gem, 1 s)`, direct, never queued behind fenced
presents), paced to the mode's refresh period (`rm_refresh::flip_interval_100ns`; the newest allocation wins, and the
buffers in between are never shown, which is harmless: dxgkrnl is told the address at programming), re-flip the CURRENT
target on a resume edge, withdraw when the target goes, three failures in a row withdraw and restart five seconds later
(the level 5 constants). No refresh tail or heartbeat (`Refresher`): the memory is GPU-rendered and every change is a flip.

**Desktop suppression, user preemption, lapse, resume** are the arbiter's, unchanged (13.2, 13.12), with the importer as
the resident source's owner:

| event | result (tested against the real `ForeignScanout` in `kmd_logic::foreign_flip::tests`) |
|---|---|
| the resident source is foreground | `suppress_desktop` answers it (`resident`, owner = the importer): the Venus desktop flush is withheld (`FsSupp`), the withheld flush is a frame edge |
| a user `SCANOUT_SET` by another device | `Preempted`: the resident registration is parked, no wire flip, the book still follows DWM's flips |
| that user source releases, lapses (`poll`), has its file closed or its device exit | the resident source takes scanout 0 back, `resume_owed`, a re-flip of the NEWEST allocation (not the one flipped last), no Venus flush |
| the importer itself `SCANOUT_SET`s | `Updated` in place; its end resumes the resident source |
| the importer closes the DRM file / its device is destroyed | the source ends (`release_handle` / `release_owner`; parked: forgotten), the desktop is owed one flush; the records made from that file are poisoned and the shown allocation dropped (`FfPoison`, `FfGone`) |
| the shown allocation is destroyed | `retire_scanout_allocation` -> `target_gone` (`FfGone`): the worker withdraws the source |
| transport reset | `foreign_scanout_reset` ends the source with no restore; `foreign_flip::forget` (from `retire_transport`) clears the target, the presenter and the knob read |
| a Venus allocation or the level 5 primary is programmed | `other_source`: the worker withdraws only this class's source (`resident_drop_of(false)`) |

`RmResEnd` counts the end of ANY resident source, this arm's included; `FsSet - FsRel - FsLapse - FsEnd - FsTake` stays
the number of user sources.

#### 15.18.4 Which allocations, which flips (what the KMD sees)

* The DWM-on-NVK swap-chain allocation is `DEVICE_MEMORY` (kind 1) with `blob_mem 0x80000001`, `adopt_resource_id` = the
  `IMPORT_RM` id, the HFLY trailer at offset 96, `MISC_PRIMARY`, never `MISC_DIRECT_SCANOUT` (`direct_scanout` false, so
  the Venus path takes `production_linear_scanout`). The arm keys on the RECORD, not on the allocation's kind or flags:
  any adopted foreign record is a candidate. Flip-model BIND_PRESENT buffers take the same route. A STANDARD placeholder
  (no identity: gate closed, not 32 bpp, a suballocation, `NvkPlaceholderAllocations=1`) has no record: `FfNoRec`, Venus,
  as today (a black placeholder is what it is).
* `Flags.Primary` allocations reach the arm through `SetVidPnSourceAddress` (the MMIO flip, `pDmaBuffer == NULL`) or through
  the DMA-buffer flip (`PresentFlipPrivate` + `arm_dma_flip`): both call `program_vidpn_source`. **`PresentMultiPlaneOverlay`
  is not seen at all**: the driver does not register the MPO3 KMD interface (`wddm_surface.rs`, `query_adapter_info.rs`:
  `SupportMultiPlaneOverlay` stays 0), so dxgkrnl does not take the MPO flip route to this KMD; if a present with
  `FlipWithMultiPlaneOverlay` ever arrived, `PresentPayload::MultiPlaneOverlay` is refused in `display.rs` and it never
  reaches this arm. A frame gate on the UMD's `PresentMultiplaneOverlay` is therefore harmless to the KMD; DWM's flips are
  the two contracts above.
* The lifetimes: the WDDM allocation belongs to DWM's D3D11 device; the record's importer and holder context belong to
  librmclient's per-process D3DKMT device (`g_ctx`), which survives DWM device recreation and dies with `dwm.exe`. After
  adoption the blob slot is KMD-owned. The flip names the importer's DRM file and a GEM that is per VkDevice / per memory
  in it, so the arm needs the importer alive AND that file open, which is what 15.18.6 is about.

#### 15.18.5 Completion and reuse: the conservative rule

Decided: **12.4 item 5 / 14.3, not the host's `ScanoutReleased` (msg 28).** The host event is usable for a user client
(`SCANOUT_STATUS`) and for a ring presenter that owns its images; here the buffers belong to dxgkrnl and DWM, and the only
way to hold them until the host released the previous flip is to hold the displayed-address publication
(`publish_bound_primary`), which 22.22.217 retired as measured inert and which would cost up to a refresh interval per
frame. So: the flip has no completion; the address is published at programming; the swap chain's depth (DWM's 3-deep
desktop chain) is the protection; the residual hazard is a viewer that still samples the previous buffer a whole flip
interval later (tearing, never corruption). The host's release book still sees every flip (`present_within` mints and sends
through it; `RelMatch` grows); it is not read. Whether the KMD should read it is checklist step 8.

Ordering against the NVK rendering is NOT something this arm provides: it flips when dxgkrnl names the allocation, as the
level 5 arm does. For the DMA-buffer flip the flip's fence retires behind the programming; for the MMIO flip dxgkrnl names
the allocation after its fence. That the NVK work for the frame is complete by then is the UMD's contract (the
"CPU-complete" present marker of `zero-copy-present.md` 10.4: the UMD waits on the CPU for the frame's NVK timeline point
before it presents); this arm adds no wait. A violation shows as an older frame's content, not as corruption.

#### 15.18.6 The wire carries `(owner_handle, host_handle = GEM)`, not a resource id: the hazard, the safe default, and what a resid flip needs

The host `ScanoutFlip` (msg 20, `HELIOS_NVRM_SCANOUT_FLIP_BYTES` = 64): `scanout, owner_handle, host_handle (the GEM),
width, height, stride, offset, fourcc, modifier, seq, reserved[4]`. The host looks the GEM up in the DRM file
`owner_handle`. Consequences for a record whose importer is a user device:

* if the importer's VkDevice goes (DWM recreates its device) NVK closes that DRM file while dxgkrnl may still show the old
  primary; the flip would name a file that is gone, or a file NUMBER the host has reused for another file;
* the host's resource import (`RESOURCE_CREATE_BLOB` / `RM_EXPORT`) holds its own reference to the memory, so the picture
  itself would be fine; only the route to name it is lost.

**Safe default, built.** Trust the pair `(rm_handle, gem)` only while all of these hold, and refuse (Venus, counted) or
drop (withdraw the source) when one stops holding:

1. the record's importer still holds `rm_handle` as its DRM file in this generation, re-read at programming (`FfRef12`) and
   again by `present_within`'s `mint` at every flip (a refusal `NotOwned` / `Forbidden` drops the target: `FfStale`,
   `FfGone`, and poisons the record);
2. the NVRM epoch is unchanged (`target_ready`, and the arbiter's `flip.epoch != epoch` check);
3. **no Close of that file number has happened since the record was made.** The NVRM handle table has no per-open serial,
   so ownership alone cannot tell "the file the record was made from" from "a newer file with the same number". Every
   forwarded `Close`, `DestroyDevice` and the transport sweep call `foreign_scanout_release_handle` / `_release_owner`; the
   hooks there poison every record with `origin == owner && rm_handle == handle` (or every record of the owner) and drop
   the shown allocation if it is one of them (`FfPoison`, `FfGone`). A reused number makes NEW records, which are not
   poisoned (tested). The hooks need the knob read: the first close after the knob is on reads it (`FfKnob`);
4. the allocation is not destroyed (`target_gone`; `Destroyed` for a destroy deferred to the last close).

**What a resid flip would change on the host** (not done; `host/` is not touched here). The flip would not need the file at
all: a `ScanoutFlip` variant (a flag in `reserved[0]`, e.g. `FLIP_FLAG_RESOURCE`) where `host_handle` is the Venus RESOURCE
id the KMD already holds (`Entry::resource_id`; the host's own import has its own reference), `owner_handle = 0`, and the
layout words as today (or none: the host has the resource's recorded layout from `IMPORT_RM`). The host looks the resource
up in the device's resource table, takes its dma-buf / image as it does for any imported resource, and answers like a flip.
On the KMD side: `Target` would carry the resource id instead of `(drm, gem)`; `present_within` would mint against a
resident source whose `handle` is a sentinel (the arbiter proves "the caller's file" with `nvrm_handle_device_type`, which
a resid source has no use for: a resource-kind source in `foreign_scanout` and a `mint` that checks the KMD's own record
instead); the file poisoning, `OwnerGone` and the `release_handle` end of the source would go; `SCANOUT_RELEASED` events
would key on the resource. Until then the safe default above runs, and its cost is exactly its refusals: a primary shown
from a closed file is dropped (the screen holds its last flip until the next programming takes the Venus path) instead of
shown.

#### 15.18.7 Failure and fallback matrix

| where | failure | effect | fallback |
|---|---|---|---|
| knob 0 / absent | none | the v319 behaviour (one load per hook, nothing written) | Venus (as before) |
| `decide` refuses (15.18.3) | counted `FfRef`, `FfWhy`, `FfRef<NN>` | no target; `other_source` | Venus path for this programming |
| no record | `FfNoRec` | none | Venus (placeholders, plain allocations) |
| registration refused by the arbiter | `FsRef`; the presenter pauses 100 ms; three strikes | withdraw, restart in 5 s (`FfGaveUp`) | the next programming is looked at afresh |
| flip refused / times out | `FfFlipFail`; paced retry 100 ms; three in a row withdraw | the screen keeps its last frame meanwhile | next programming |
| flip finds the source yielded | `FfYielded` (not a failure; eight in a row are one) | parked behind a user source | re-flip on resume |
| the importer's file is not its own at flip time | `FfStale`, `FfGone`; the record is poisoned | the worker withdraws the source, the desktop is owed one flush | the next programming (a new allocation, or the Venus path for the poisoned one) |
| importer closes the file / its device exits | `FfPoison`, `FfGone` | as above | as above |
| the shown allocation is destroyed | `FfGone` | withdraw | next programming |
| transport reset | `forget` | everything cleared, knob re-read | cold start |
| level 3 / 4 | refused, `FfRef01` | the ring presenter keeps its resident source | Venus |

A withdrawn source leaves the desktop owed one Venus flush of `active_scanout_resource`, which names the foreign resource
and is not bound on the host: `RfUnb` counts it, as for the level 5 primary. The screen shows the last flip until the next
programming; this is the same "no Venus image to show" limit as 15.8 and the reason the presenter restarts instead of giving
up for good.

#### 15.18.8 Locking, IRQL

`TARGET` and `PRES` are leaf spinlocks over plain data, never held across I/O or another lock, never together; the
arbiter's `STATE` is taken only through the adapter methods after both are released. `program` runs at PASSIVE under the
scanout lifecycle lock (it calls `with_virtio` for the facts, after `rm_import_served`'s own hold, and the adapter methods;
it sends nothing), `target_gone` from `retire_scanout_allocation_locked` (one load, then `TARGET`), the close hooks at
PASSIVE with no lock held (`with_virtio` for the poison, then `TARGET`), the flips from the HPD worker with no lock held.
The knob is read at PASSIVE (the first `program` or close hook of the generation). The level 5 service leaves the shared
frame and resume edges alone while `foreign_flip::holds_screen()` and only stands down; its resident accessors are
class-aware (`resident_state_of(true)`, `resident_drop_of(true)`), which with only KMD sources registered answer exactly
what they did.

#### 15.18.9 Counters (`Ff*`, REG_DWORD, at most 13 characters, written by the throttled mirror once the knob was on and an allocation was seen)

| value | what | healthy |
|---|---|---|
| `FfKnob` | the knob's value (also written when read, if nonzero) | 1 |
| `FfProg` / `FfSame` / `FfMoved` / `FfReowned` | allocations taken / the same again / another of the same device / of another device | grows / grows / grows / 0 or 1 per device |
| `FfNoRec` | programmed with no foreign record | the GDI primary before DWM, placeholders |
| `FfRef`, `FfWhy`, `FfRef01` to `FfRef12` | refused to Venus / last reason / per reason (15.18.3) | 0 |
| `FfRegs` / `FfWithdrawn` / `FfGaveUp` | registrations / withdrawals / giving-ups | 1 / 0 / 0 |
| `FfFrames` / `FfReflips` / `FfYielded` / `FfFlipFail` | flips for an edge / for a resume / that found the source yielded / refused | grows / small / small / 0 |
| `FfStale` / `FfGone` / `FfPoison` | flips refused because the importer's file is not its own / shown allocations dropped / records poisoned by a close | 0 / 0 / 0 until a device or file closes |
| `FfPres` / `FfSeq` / `FfEdges` | the presenter word (bit 0 registered, bit 1 gave up, bits 8.. failures) / the last flip's `seq` / frames owed | 1 / grows / grows |
| `FfReg` | the last registration's answer, written when it happens | 1 |

`FfProg` counts every `SetVidPnSourceAddress` / DMA flip shown through this arm; `FfFrames + FfReflips` is the number of
host flips, at most one per refresh period (the difference is coalescing).

#### 15.18.10 Verified here, and not

Verified on the host: `cargo test` in `guest/windows/kmd_logic` (808 tests) and `guest/windows/protocol` (30, unchanged).
New and checked there, against the REAL arbiter, presenter and foreign table: the decision table (every row of 15.18.3,
including knob off beating every other fault, the level rows, KMD-owned vs user-owned, owner gone, a layout the flip cannot
carry, extent, destroyed / poisoned / not adopted); the target book (same / moved / re-owned, destroyed, file closed, owner
closed); the generation rule (the same allocation again and a move keep the resident source's generation, a new owner gets
a new one and the old owner's flip is refused); pacing and "newest allocation wins"; user preemption, release and LAPSE
both resuming with a re-flip of the newest allocation and no Venus flush; the importer's own `SET` as `Updated`; close and
owner exit (foreground and parked); the two classes of resident source never withdrawing each other's; the poison of
records by file close and by device exit, with a reused file number making an unpoisoned record; the counter names (at most
13, unique, `Ff` used nowhere else in `kmd_render`, and the set the driver writes equals the list). Type- and borrow-checked
(`cargo check`, no codegen) against a harness generated from the REAL module declarations (visibility from the real `mod`
lines, signatures cut from the real sources, the files under test included unchanged: `virtio/foreign_flip.rs`,
`rm_client.rs` as a directory module with `sysmem.rs` and `sysmem_flip.rs`, `rm_present.rs`, `rm_foreign.rs`,
`foreign_scanout.rs`, `scanout_release.rs`, `adapter/foreign_scanout.rs`); a deliberately wrong path (`rm_client::knob_level`,
private) is rejected by it. Every `crate::` / `super::` path of the touched large files (`display.rs`, `scanout.rs`,
`hpd.rs`, `nvrm.rs`, `submit_command.rs`, `foreign_tables.rs`) resolves against the real `mod` lines (0 problems). The hooks
in those large files are small and were read against the real definitions; they are not compiled.

**Not verified by anything**: that `kmd_render` compiles; every host reaction (a `ScanoutFlip` naming a GEM of a user
device's DRM file at the display rate, the compositor sampling it, the host's behaviour when the file closes); IRQL
behaviour; that the DWM-on-NVK UMD's allocations match the record (extent equal to the mode's, 32 bpp, `MISC_PRIMARY`);
dxgkrnl's reaction to a flip that is shown with no bind (the level 5 question, answered there only for a GDI primary); the
present-marker ordering (15.18.5); that the poison hooks always run before the host reuses a file number (the hook runs after
the host's reply to the Close; the ordering of that against another thread's `Open` on the same device is not proved).

#### 15.18.11 Hardware checklist, in order

Prerequisites: the DWM-on-NVK UMD presents through the WDDM flip only (no `SCANOUT_SET` escapes); the host serves the RM
import (`FgImp` counts imports); if `KmdRmClient=5` is in use, 15.13 steps 2 to 5 pass.

1. **Baseline, knob absent.** `ForeignFlip` absent, restart the device, run DWM-on-NVK: note what the screen shows and the
   flip counters (`Pb*`, `Vk*`, `Sn*`, `FsSupp`, `RfUnb`). No `Ff*` value exists. This is the v319 behaviour; if it differs,
   stop.
2. **Check the environment.** `KmdRmClient` absent, 0, 1, 2 or 5 (NOT 3 or 4: the arm refuses, `FfRef01`). The imports work
   without the arm: `FgImp` and `FgAdo` grow, `FgRefA=0`, `FgLive` = the swap chain's buffers.
3. **Turn it on.** `reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v ForeignFlip /t REG_DWORD /d 1`,
   then restart the display adapter's device (the knob is read once per transport generation). `FfKnob=1`.
4. **Is the arm seeing the allocations?** With DWM running: `FfProg` grows (one per flip), `FfNoRec` small and not growing
   (the GDI primary and placeholders only), `FfRef=0`. If `FfRef` grows, `FfWhy` and `FfRef01` to `FfRef12` say why:
   8 `FileClosed` or 12 `OwnerGone` means the importer's DRM file is not open any more at the flip (the UMD / librmclient
   must keep the file open for the life of the swap chain, or the resid flip of 15.18.6 is needed); 10 or 11 mean the
   record's layout is not the mode's; 2 means the worker has not read `KmdRmClient` yet (retry); 5 is a host without the
   import; 9 the KMD's own record; 1 a ring level is set.
5. **Is it shown?** `FfRegs=1`, `FfFrames>=1` and growing at the refresh rate, `FfFlipFail=0`, `FfYielded` small, `FfPres=1`;
   the host log shows `ScanoutFlip` for the importer's DRM handle (DWM's librmclient file) with the swap chain's layout
   (modifier `0x0300000000606010`-class); the viewer shows the desktop. Black with `FfSeq` growing: the flip path is right and
   the frame is the UMD's, or the order of 15.18.5. `FfFrames` growing with no host `ScanoutFlip` at all: the host refused
   (`FsErr`).
6. **Source updates in place.** `FfMoved` grows with `FfProg` and `FfReowned` stays 0 (one importing device); `FsSet`
   unchanged (the arm is never a user source); `FsSupp` grows with the withheld Venus flushes.
7. **Preemption and resume.** Run an NVK scanout app (`crm_scanout_smoke`): `FsSet`+1, no flips from this arm while it
   runs (`FfYielded` small), its frames show; on release `FfReflips`+1 and the desktop is back with no Venus flush (`FsRest`
   unchanged). Kill a silent one: the lapse (2 s) hands the screen back, `FsLapse`+1, `FfReflips`+1.
8. **Tearing / reuse** (15.18.5): drag a window over a moving video for a minute and count visible tears; read `RelMatch`
   and `RelDrop` (the host's releases for these flips). A tear rate that matters is the case for reading the release
   book (hold the displayed-address publication until the replaced flip is released): report it with `RelRecv`.
9. **DWM restart / device recreation** (the 15.18.6 hazard): kill and restart `dwm.exe`, or change the mode so DWM
   recreates its swap chain. Expect `FfPoison` to grow (the importer's file closed), `FfGone`+1 if the primary was shown,
   a brief frozen screen, then `FfProg` growing again with `FfRef` unchanged for the NEW buffers. `FfStale` > 0 means a flip
   reached the host with a file that was not the importer's (the hook missed it): report the counter and the host log. No
   bugcheck, no `RfUnb` growth beyond a few.
10. **Mode change**: a new swap chain at the new mode: `FfProg` grows; `FfRef11` counts only stale buffers of the old mode.
11. **Fallback**: `ForeignFlip=0` again after a device restart restores step 1's behaviour exactly (no `Ff*` growth). With
    `KmdRmClient=3`: `FfRef01` grows and the desktop is the Venus / ring desktop.
12. **Soak**: ten minutes of normal desktop use: `FfFlipFail=0`, `FfGaveUp=0`, `FfStale=0`.

#### 15.18.12 Risks and open questions

1. **The file the flip names** (15.18.6): the safe default refuses and drops; whether NVK keeps the DRM file open for the
   life of DWM's swap chain (it should: the GEM is per memory in that file) is the UMD's, and checklist step 4's `FfRef08` /
   `FfRef12` answers it. If it does not, the resid flip is the only route and needs the host change described there.
2. **Ordering against the NVK frame** (15.18.5) is the UMD's contract; this arm has no wait.
3. **Reuse / tearing** (15.18.5) rests on the swap chain depth.
4. **The handle-reuse window** (15.18.10): a per-open serial in the NVRM handle table would close it for good.
5. **The knob's lazy read is at PASSIVE only**; a `program` that ever ran at DIRQL would be a bug (it cannot today:
   `program_vidpn_source_inner` runs under the scanout lifecycle lock).
6. **A withdrawn source owes the Venus desktop a flush** of an unbound foreign resource (`RfUnb`): the screen keeps its last
   flip. A copy-based fallback for an existing foreign primary is not built (the same limit as 15.14 point 3).
