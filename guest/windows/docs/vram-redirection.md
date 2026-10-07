# The redirection surface in GPU memory (lane F): findings, design, staged plan

Status (2026-10-08): phase 1 tooling built; V1 (`VidMmCapsX`) run on hardware (354.1) and NEGATIVE
(section 2.4); V2-V5 machinery (`RedirVram`: the RM VRAM allocation service, the KMD_RM foreign
adoption, the CE VRAM-to-VRAM route, the upload and readback for CPU writers and readers) built behind
default-off knobs, host-tested (kmd-dev gate rc=0, stubcheck: no new errors beyond the stub model's
known gap), never run. Branch `feat/vram-redirection`, which merges `fix/rm-ce-route-review-findings`
(PR #13: the copy-engine channel and route) on top of `feat/rm-copy-engine-present` (PR #2). Because V1
showed that Windows keeps a CPU-visible STAGING surface as the Blt destination, the path to a GPU-only
redirection surface is GDI hardware acceleration on the copy engine (fallback A, 5.4, built on
`feat/vram-redirection-gdi-accel` on top of these modules: section 8 lists the shared surface).

Companion reading: `rm-copy-engine-present.md` (sections 11, 14, 15: the CE channel and the route),
`rm-backed-standard.md` (sections 8 and 13: the census and the Blt destination), `zero-copy-present.md`
(24: `BltAsync`, `GuestBlob`), `kmd-handoff-2026-10.md` (1, 2, 7), `kmd-rm-client.md` (5, 12, 14, 15),
`shared-foreign-surfaces.md`, `../../../docs/dwm-on-nvk.md`, `../../../docs/HANDOFF.md`.

## 0. The short answer

* **The bar.** Heaven D3D11, windowed 1600x900, "Composed: Copy with GPU GDI". Bare metal on the same
  host: about 576 fps. Here: 214-247 fps; Heaven spends about 1.8 ms per frame inside `Present`.
* **Where the time goes (measured before this lane, confirmed per frame by the phase 1 capture).** The
  app thread waits in dxgkrnl between `Lock` and `Unlock` (DxgKrnl events 41 and 42 on Windows 11 26H1)
  on the destination of the Blt, until a Blt DMA packet retires (event 180). The packet retires only
  after the producer's frame, the copy (about 220 µs over PCIe on the copy-engine route) and the guest
  completion path: a GPU round trip through the guest per frame.
* **What the destination is.** A KMD standard buffer (`KmdStandardBuffer`: shadow or staging standard
  allocation, linear, `CpuVisible`, `Cached`) whose authoritative content, in steady state, is guest
  system pages: VidMm moves it from the Venus-window memory segment (id 2) to system memory once and
  keeps it there (`PgTo` 1). DWM never opens it (census, `rm-backed-standard.md` 8 "S-A0 result"); it
  reads those system pages through dxgkrnl's CPU view (`BltNoMirror` froze the window). This is
  Windows' canonical redirection path for an adapter WITHOUT GDI hardware acceleration: GDI and the
  redirected window content live in CPU-visible memory, and every GPU write into it must retire before
  the CPU side may touch it again.
* **What bare metal does differently.** The window's redirection surface is a GPU surface that DWM
  samples on the GPU; the redirected Blt is ordered on the GPU and never waits for a CPU lock.
* **The design (section 5).** Make the redirection surface a GPU-only allocation (not `CpuVisible`,
  never `Lock`ed), backed by RM video memory the KMD allocates on its own RM client, adopted as a
  foreign resource so DWM on NVK imports it by resource id (zero copy, VRAM), and written by the
  redirected Blt with a VRAM-to-VRAM copy on the KMD's RM copy-engine channel. Windows chooses a GPU-only
  redirection surface only if the adapter tells it that GDI allocations need not be CPU visible
  (`DXGK_VIDMMCAPS.NonCpuVisiblePrimary`, WDDM 2.0+) and, depending on what CDD then asks for, gives it
  the copies it needs between CPU-written GDI staging and the GPU-only surfaces. Stage V1 is the
  one-bit experiment that tells which allocations Windows asks for when that cap is set; stages V2-V5
  build the memory, the copy, the import and the CPU readers.
* **What cannot be promised before V1 runs.** That Windows 11 26H1 moves a DXGI blit-model window's
  redirection to a GPU-only surface with that cap alone, without GDI hardware acceleration. If it does
  not, section 5.4 (GDI hardware acceleration limited to copies and fills on the copy engine) and
  section 5.5 (a VRAM shadow behind the same allocation) are the fallbacks; both are written down.

## 1. Background and numbers

| | value | source |
|---|---|---|
| Heaven windowed, bare metal, same host | ~576 fps (1.74 ms) | `docs/HANDOFF.md` |
| here, `GuestBlob=1` | 247 fps (4.05 ms) | `docs/HANDOFF.md` |
| `msInPresentAPI` | ~1.8 ms | PresentMon |
| producer wait (Blt deferred until the producer's frame is done) | ~650 µs p50 | stage timing (PR #4) |
| CE copy 5.76 MB (1600x900x4) into guest pages | ~220-234 µs (PCIe x8 write bound) | PR #2 smoke, route live: 16403/16403 Presents routed |
| VRAM-to-VRAM copy of the same frame on the GPU | ~20 µs (estimate: ~1.5 TB/s copy-engine read+write; to be measured in V3) | |
| Lock -> Unlock on the app thread (events 41 -> 42) | 1.0-1.7 ms | DxgKrnl ETW, `kmd-handoff-2026-10.md` 1 |

The CPU cost of the rest of Heaven's frame (outside `Present`) is about 2.2-2.6 ms here against well under
1.7 ms on bare metal; it is not this lane's subject, but it bounds what removing the wait can give (about
400 fps if the wait goes to zero and nothing else changes).

## 2. Phase 1: confirming the serialisation (ETW)

### 2.1 What is captured

`guest/windows/ci/vmtest/vram-etw.ps1` (run in the guest as administrator; `vramcap.sh` runs it from the
host, copies the result back and calls the report):

* the build's own DxgKrnl, DXGI and Win32k manifests (`wevtutil gp ... /ge /gm /f:xml`): event ids and
  field order move between builds, so the parser keys on field names;
* a WPR profile generated on the spot: DxgKrnl at level 5 with the keywords `Base`, `Profiler`,
  `References`, `Resource`, `Memory` (and `Present`/`GPUScheduler` where the build has them), masks
  read from the manifest; capture state (rundown) on start and on save for `Base|References|Resource|Memory`,
  which is what emits `ReportSegment` (78), `AdapterAllocation`/`DeviceAllocation` DC_Start (35, 38) and
  `ReportCommittedGlobalAllocation` (227); DXGI with every keyword (Present start/stop, 42/43); Win32k
  `Updates|Visualization|Tracing` (the token events PresentMon uses to see DWM take a frame); kernel
  process/thread/loader (optionally `-CSwitch`: context switches and ready-thread);
* the KMD's registry counters before and after (the `blrow.sh` snapshot), the process list, the OS build;
* the ETL itself (opens in WPA/GPUView) and `tracerpt -of XML`, zipped.

The events the report uses (26H1 ids; classified by field names):

| id | event | used for |
|---|---|---|
| DXGI 42/43 | Present start/stop | one frame = one `Present` call of the app thread |
| 166 | Blit (`hSourceAllocation`, `hDestAllocation`, `bRedirectedPresent`) | the destination: the redirection surface. "Copy with GPU GDI" is a Blit with `bRedirectedPresent` 0 followed by PresentHistory model 3 (PresentMon's rule) |
| 171/215 | PresentHistory(Detailed)Start, `Model` | 3 = `D3DKMT_PM_REDIRECTED_BLT` |
| 178/180 | QueuePacket start/stop | the Blt's DMA packet: submitted, retired |
| 41/42, 340/341 | Lock/Unlock (`hAllocationHandle`, `dwFlags`, `uiLockStatus`) | CPU access to an allocation: who (pid/tid) and how long |
| 105/106 | ProfilerStart/Stop (`Function`) | dxgkrnl's own function spans (the CPU-access wait among them) |
| 33/35, 36/38 | AdapterAllocation, DeviceAllocation (+ DC_Start) | handle joins, size, `Flags`, read/write segment sets, preferred segment, section object |
| 78 | ReportSegment | segment id, size, flags, memory segment group |
| 80/227, 73, 53/60, 70, 58, 74 | committed / page-in / transfer / placement / aperture map / evict | which segment the allocation is in, and when it moved |

### 2.2 Run

```text
# windowed Heaven D3D11 1600x900 running (or HEAVEN=1 to start it), watchdog on
WIN_SSH=<guest>@127.0.0.1 bash guest/windows/ci/vmtest/vramcap.sh hv-rmce 3
WIN_SSH=<guest>@127.0.0.1 bash guest/windows/ci/vmtest/vramcap.sh hv-rmce-cs 2 -CSwitch
```

Output: `$VMTEST_DIR/win/vram-LABEL-HHMMSS/` with `report.txt`, `frames.csv` and the raw files. The parser
alone: `guest/windows/tools/vram_redirection_report.py DIR [--process Heaven.exe] [--csv f.csv]`;
`--selftest` checks it on a synthetic trace.

### 2.3 How to read the report

* `redirection surface candidate`: the Blit destination, its size, flags, read/write segment sets
  (bit 0 = segment 1), the segments it was seen in and the last placement. "0 (no segment: system memory
  / not resident)" or an aperture segment means the CPU-visible system-memory case.
* `Lock events by process`: who takes CPU access to it. A `dwm.exe` row means DWM's composition reads
  it with the CPU (through win32k or its own lock); the app's own row is dxgkrnl's CPU-access throttle
  on the Present path.
* Per frame: `present` (the app's `Present` call), `stall` (the longest gap between two consecutive
  events of the app thread inside it) and what brackets it, `retire->wake` (stall end minus the nearest
  QueuePacket retire before it), `the longest stall ends on` (own Blt retire / previous Blt retire / other),
  `blt_pkt` (the frame's own packet, submit to retire), `dstlk` (locks on the destination inside the
  frame), and the profiler spans.
* The hypothesis is confirmed when the stall is bracketed by Lock/Unlock on the redirection surface, ends
  within ~100 µs of a Blt packet retire in most frames, and the surface sits in system memory/aperture.

### 2.4 Results

**V1 on 354.1** (353.1 + V1, windowed Heaven 1600x900, 5120x1440@240, `RmCopyEngine` 0), knob absent and
`VidMmCapsX=0x200`, identical: `PBdStd` 3 (STAGING), `PBdGdi` 0, `PBdSto` 2 (pitched standard buffer),
`PBdKnd` 2, `PBdw`/`PBdh` 1600x900, `PBdPch` 6400, `PBdFmt` 87, `PBdSz` 6619136; `StdNStaging` 1, every
`StdNGdi*` 0; 201 / 188 Presents per second, both "Composed: Copy with GPU GDI". So the destination is
dxgkrnl's STAGING surface (the hypothesis of section 4 holds), and `NonCpuVisiblePrimary` alone does not
make Windows hand out a GPU-only redirection surface.

With `CeRtDirect=1` (route submit at Present): `CeRtDir` 17219/17223, `BltDeferUs` ~0, `CeRtLag` mean
160 µs, `msInPresentAPI` 1.63 -> 1.52 ms, 249 -> 253 Presents per second: the producer's own GPU time and
the staging readback (the CPU lock) remain the limit.

The ETW captures (`hv-rmce`, `hv-rmce-cs`, `hv-venus`) failed on the transport (`vramcap.sh`, fixed in
9b7226d8: ssh/scp through an ssh-config host alias, `DRY=1`); rerun pending.

## 3. What the redirection surface is, here and on bare metal

### 3.1 Windows' model (public documentation)

* Blit model: "contents of the back buffer are copied into the redirection surface on each call to
  Present1"; flip model: DWM composes straight from the shared back buffers
  ([DXGI flip model](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model)).
* Since Windows 7, GDI renders on the CPU into an aperture (CPU-visible system memory) surface and the GPU
  copies the updates into the video-memory redirection surface DWM samples
  ([Comparing Direct2D and GDI](https://learn.microsoft.com/en-us/windows/win32/direct2d/comparing-direct2d-and-gdi)).
  The two halves are the GDI surface types `STAGING_CPUVISIBLE` (CPU, linear, cache-coherent aperture) and
  `TEXTURE` (not CPU visible, shared, "used as a texture during DWM composition")
  ([D3DKMDT_GDISURFACETYPE](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ne-d3dkmdt-_d3dkmdt_gdisurfacetype)).
  The GPU half needs GDI hardware acceleration (`SupportKernelModeCommandBuffer`,
  [GDI hardware acceleration](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gdi-hardware-acceleration)).
* `DXGK_VIDMMCAPS.NonCpuVisiblePrimary` (WDDM 2.0+): "GDI allocations are not required to be CPU visible"
  ([DXGK_VIDMMCAPS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_vidmmcaps)).
  The same page lists the cross-adapter resource rules (aperture only, CPU visible, write-combined,
  linear): that is the shape the `CrossAdaptCaps` path gives a blit-model window here.
* A CPU lock waits for the GPU to finish with the allocation unless `DonotWait`; `IgnoreSync` exists only
  for aperture-placeable allocations
  ([D3DDDICB_LOCKFLAGS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dukmdt/ns-d3dukmdt-_d3dddicb_lockflags)).
  A GPU-only allocation is never locked, so writes to it are ordered on the GPU only (inference).
* Segments: memory segments (VRAM, CPU access directly or through a CPU host aperture), one aperture
  segment, and the implicit system segment 0
  ([GPU segments](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-segments),
  [CPU host aperture](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/cpu-host-aperature),
  [DXGK_SEGMENTFLAGS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_segmentflags)).

Other virtual GPUs, for comparison: VirtualBox's VMSVGA driver puts DEFAULT-usage surfaces in a
CPU-invisible host segment and DYNAMIC/STAGING in a CPU-visible aperture, and its Present turns Blt into
GPU commands
([VBoxMPWddm.cpp](https://github.com/VirtualBox/virtualbox/blob/main/src/VBox/Additions/win/Graphics/Video/mp/wddm/VBoxMPWddm.cpp));
viogpu3d reports a single cache-coherent aperture, every allocation `CpuVisible` in it
([viogpu_adapter.cpp](https://github.com/max8rr8/kvm-guest-drivers-windows/blob/viogpu3d/viogpu/viogpu3d/viogpu_adapter.cpp),
[viogpu_allocation.cpp](https://github.com/max8rr8/kvm-guest-drivers-windows/blob/viogpu3d/viogpu/viogpu3d/viogpu_allocation.cpp)),
i.e. the system-memory redirection this KMD has today; Hyper-V GPU-PV keeps non-CPU-visible allocations
entirely on the host GPU and maps only CPU-visible ones into the guest IO space
([GPU paravirtualization](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization)).

### 3.2 This KMD today (read on this branch)

Segments (`ddi/query_adapter_info.rs`, `ddi/bar_segment.rs`, `ddi/segment_table.rs`; WDDM 2.1 + GpuMmu,
`ddi/wddm_surface.rs`):

| id | what | flags | backing | CPU access |
|---|---|---|---|---|
| 1 | aperture, 1 GiB | Aperture, CacheCoherent | guest system pages | n/a |
| 2 | "BAR" memory segment, `VidMmVramMB` MiB, local budget group | CacheCoherent, SupportsCpuHostAperture, SupportsCachedCpuHostAperture (`BarSegFlags` 0x1C) | the virtio host-visible window: Venus blobs (host memory), **not VRAM** | `DxgkDdiMapCpuHostAperture` maps the allocation's Venus blob at dxgkrnl's offset |

`DXGK_VIDMMCAPS`: `SectionBackedPrimary`, `CrossAdapterResource` with `CrossAdaptCaps`, GpuMmu bits.
`PresentationCaps` 0: **no GDI hardware acceleration** (deliberately; `query_adapter_info.rs` explains the
history). No `DxgkDdiLock`.

The redirection surface: `GetStandardAllocationDriverData` accepts shared primary, shadow, staging and GDI
surfaces; every non-primary one except GDI `TEXTURE` becomes `KmdStandardBuffer`: a Venus host-visible
CACHED buffer, `CpuVisible`, `Cached`, preferred segment 2, supported {2, aperture},
`SystemBackingPolicy::PresentLinearBuffer` (`create_allocation.rs` `build_backing`, `vidmm_placement`).
The census saw the app create and open five shadow and one staging surface and DWM open none. The Blt arm
accepts it as `PresentDestinationDesc::StandardBuffer` by storage class (`display.rs`), with no type check.
In steady state its content is VidMm's system pages (leases in `system_backings`), which the copy writes:
the Venus copy plus CPU mirror, the guest blob (`GuestBlob`), or the copy-engine route's OS descriptor
(`RmCopyEngine=1`).

The KMD's RM client already has what a VRAM surface needs: `alloc_memory` of `NV01_MEMORY_LOCAL_USER`
(`virtio/rm_client.rs`), export to an fd and GEM, a `HELIOS_BLOB_MEM_RM_EXPORT` resource (`rm_foreign`),
adoption of its own imports into a WDDM allocation (`foreign_tables.rs` `adopt_for_allocation`, KMD_RM
creator), `RM_RESOURCE_IMPORT` for openers (NVK imports it as VRAM, which is NVK's assumption anyway), and
the CE channel with GPU mappings of RM memory and dup'd NVK images (`rm_client/ce_channel.rs`, `ce_dup`).
What it lacks for this lane: a vidmem object that is a WDDM allocation's backing (only the level 1-4 ring
surfaces exist, adopted by nothing) and a CE copy whose destination is vidmem.

## 4. Why the redirected Blt serialises here

**Confirmed by `PBdStd` 3 on hardware (2.4); the per-frame Lock rows await the ETW rerun.** The census's one STAGING
surface is the likeliest Blt destination. Microsoft documents exactly this flow for it: dxgkrnl Blts
"from an application's back buffer into the staging surface. The staging surface is then locked and read
by the CPU", and the staging surface is created "when a direct bitblt to the primary surface is not
possible (for example, in multiple-monitor or sprites cases)"
([D3DKMDT_STAGINGSURFACEDATA](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ns-d3dkmdt-_d3dkmdt_stagingsurfacedata)).
The shadow surfaces are CDD's CPU copies of the primary, drawn by the CPU and Blitted to the primary
([D3DKMDT_SHADOWSURFACEDATA](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ns-d3dkmdt-_d3dkmdt_shadowsurfacedata)).
If `PBdStd` reads 3, the `Lock` on the app thread is dxgkrnl itself reading the staging surface back with
the CPU, right after the GPU Blt into it, to move the frame into the window's CPU-side (GDI) redirection
bitmap; the GPU Blt must retire first, which is the wait. That is the readback path Windows takes when the
window's redirection surface cannot be a GPU target of the app's device (`DwmDxGetWindowSharedSurface`
answers `DWM_S_GDI_REDIRECTION_SURFACE` and the app presents with model `REDIRECTED_BLT` into the GDI
redirection surface,
[DwmDxGetWindowSharedSurface](https://learn.microsoft.com/en-us/windows/win32/dwm/dwmdxgetwindowsharedsurface));
here the GDI redirection surface is CPU-visible because the adapter has no GDI hardware acceleration.


(To be finalised against 2.4.) The destination is CPU visible and lives in system memory; the
consumer (DWM's composition of a CPU-GDI redirection surface) reads it with the CPU; dxgkrnl therefore
puts a CPU-access acquire (`Lock`, VidMm "begin CPU access") between consecutive GPU writes into it, and
that acquire waits for the previous write's DMA packet to retire. On bare metal the packet retires a few
tens of microseconds after the producer's frame; here the retire is a guest round trip (producer wait +
PCIe copy + completion), so the app thread pays it every frame. Moving the same CPU-visible allocation
into VRAM would not remove the acquire (the CPU still reads it, now through a write-combined BAR view at
~200 MB/s, `kmd-rm-client.md` 5.2): the allocation must stop being CPU visible.

## 5. Design

### 5.1 Target picture

```text
app (NVK, VRAM back buffer) --Present(Blt)--> KMD Blt arm
   --> CE channel: ACQUIRE producer sem; COPY VRAM->VRAM (~20 us); RELEASE completion   (GPU-ordered)
   --> DMA fence retires on the CE completion, no CPU access anywhere
redirection surface = GPU-only WDDM allocation, backed by KMD RM vidmem (LOCAL_USER), adopted as a
   foreign resource (resid + layout trailer)
DWM (NVK) opens it -> RM_RESOURCE_IMPORT -> samples VRAM directly (zero copy)
CPU readers (PrintWindow, capture, GDI on the window) -> through the CPU-visible staging half or an
   on-demand readback (5.6)
```

### 5.2 Getting Windows to make the redirection surface GPU-only (stage V1)

Windows decides the redirection surface's type from the adapter's caps; the KMD cannot relabel a
`CpuVisible` shadow surface after the fact, because dxgkrnl and DWM have already chosen the CPU path for it.
The documented lever is `DXGK_VIDMMCAPS.NonCpuVisiblePrimary`. What CDD asks for with it set is not
documented for an adapter without GDI acceleration, so V1 measures it:

* knob `VidMmCapsX` (default 0): raw `DXGK_VIDMMCAPS` bits OR'd into the reported word, restricted to an
  accepted mask (today only bit 9, `NonCpuVisiblePrimary`, 0x200); mirrors `VmCapsXEff`, `VmCapsXMsk`
  (at StartDevice) and `VmCapsRep` (the word the caps query reported). Pure logic
  `kmd_logic::vidmm_caps`, same shape as `FlipCapsX`.
* destination census: the Blt arm's sampled Present trace already records the destination's identity
  (`PBdStd` standard allocation type, `PBdGdi` GDI surface type, `PBdw`/`PBdh`, `PBdSto` storage class,
  `PBdKnd`, `PBdst` resource id). V1 adds `PBdSys` next to them: 1 when the destination has system-backing
  leases at that Present (VidMm holds it in system memory), 0 when it is in segment 2 (or untracked).
  `vram-etw.ps1` dumps all of them (`kmd-before.txt`/`kmd-after.txt`). Together with the `StdN*`/`StdO*`
  census they answer: which standard type Heaven's redirection surface is (13.6 Q1 of
  `rm-backed-standard.md`), whether it is system-resident while frames are presented, and with
  `VidMmCapsX=0x200`, whether Windows now asks for GDI `TEXTURE` (or another non-CPU-visible type) and
  whether DWM opens it (`StdOpenPid`).

Code (V1 as built): `kmd_logic/src/vidmm_caps.rs` (`resolve_vidmm_caps`, 5 tests),
`kmd_render/src/adapter/mod.rs` (`AdapterKnobs::vidmm_caps_x`, `vidmm_caps`, the start mirrors),
`kmd_render/src/ddi/query_adapter_info.rs` (`query_driver_caps`: the word goes through
`vidmm_caps(base).reported`, mirrored `VmCapsRep`), `kmd_render/src/diag.rs` (knob name),
`kmd_render/src/ddi/display.rs` (`PBdSys`). With the knob absent the reported word is byte-identical.

Result (2.4): the second row of the table below: the destination stayed the STAGING surface. Next:
5.4. Other documented levers, checked: `CrossAdaptCaps` cannot help (a cross-adapter resource is by
definition aperture-only, CPU visible and write-combined, [DXGK_VIDMMCAPS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_vidmmcaps));
the staging surface exists "when a direct bitblt to the primary surface is not possible", is then
"locked and read by the CPU" ([D3DKMDT_STAGINGSURFACEDATA](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ns-d3dkmdt-_d3dkmdt_stagingsurfacedata)),
and the redirection surface that a direct Blt can target is the GPU `TEXTURE` GDI surface, which CDD
uses with GDI hardware acceleration (`SupportKernelModeCommandBuffer`,
[GDI hardware acceleration](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gdi-hardware-acceleration)).
That is the one documented switch left.

Expected outcomes and what each means:

| with `VidMmCapsX=0x200` | meaning | next |
|---|---|---|
| Heaven's destination becomes a GDI `TEXTURE` (`PBdStd` 4, `PBdGdi` 1, `PBdSto` 1 optimal), `StdOGdiTex` grows with `StdOpenPid` = DWM | Windows composes the window from a GPU-only surface | V2-V4 as written |
| the destination stays a shadow/staging buffer, nothing else changes | the cap does not reach redirection without GDI acceleration | 5.4 |
| the desktop breaks (black GDI windows, `PBCpy` 0xE6 refusals: CDD now Blts from a CPU-visible staging source into GPU-only surfaces) | CDD expects Present Blts staging -> GPU surface | V2 adds that Blt (standard-buffer source), then retry |
| the adapter fails to start | the cap is refused at this WDDM level | knob off; 5.4 |

The knob is read at StartDevice; recovery is `VidMmCapsX` removed plus `pnputil /restart-device`.

### 5.3 The memory: RM vidmem behind a GPU-only allocation (stages V2, V3)

* **Backing.** In `build_backing`, the `KmdOptimalGdiTexture` arm (and any other non-CPU-visible type V1
  names) gets a new first choice behind knob `RedirVram` (default 0): `rm_client::alloc_memory`
  (`NV01_MEMORY_LOCAL_USER`, pitch-linear first, 128-byte aligned pitch as NVK authors it for imports,
  64 KiB pages), exported and imported as a KMD_RM foreign resource (`rm_foreign::import_surface`), adopted
  by the WDDM allocation with a LINEAR layout trailer (the allocation's private data grows to 128 bytes
  for this arm: `PRIV_SIZE` per type, `rm-backed-standard.md` 0 item 8). Fallback on any refusal: today's
  Venus OPTIMAL image, counted.
* **Placement.** Not `CpuVisible`, not `Cached`. Which segment VidMm is told: the honest answer is a
  memory segment backed by VRAM. Two ways:
  1. **(preferred) a third segment, "RM VRAM"**: a memory segment, not CPU visible, no CPU host aperture,
     `LocalBudgetGroup`, size = the host's `--vram-limit-mib` share. Its "pages" are addresses VidMm assigns
     inside a range the KMD never maps; the real memory is the per-allocation RM object, and paging
     operations on it are no-ops for content (the content lives in the RM object and nowhere else),
     exactly like the Venus window segment's tracking allocations today. Must respect the "cpu-host segment
     last" rule (`segment_table.rs`): it goes before segment 2, which renumbers segment 2 -> 3 and touches
     every `MEMORY_SEGMENT_ID` user (`gpummu.rs`), so it is its own reviewed change.
  2. **(first step, no new segment) the aperture only, `EvictionSegmentSet` empty, no CPU access**: what
     the GDI texture arm already declares. VidMm then believes the allocation lives in system pages it
     maps through the aperture, but nothing ever reads those pages (no CPU view, the GPU copy and DWM go to
     the RM object). Paging must not copy content for it (`BAR_DEVICE_OP_SKIPS` already skips it). Risk:
     VidMm's budget accounting charges system memory for VRAM; harmless at 6 MB per window.
* **The copy (V3).** `ce_present::present_push` with the destination VA = the RM vidmem object mapped on
  the CE channel's VA space (`ce_channel::gpu_map_with` with the vidmem kind and big pages, the path
  `ce_dup` uses for NVK images), source = the producer's dup'd image as today, ACQUIRE the producer's
  semaphore, RELEASE completion. The Present's DMA fence retires on the CE completion
  (`complete_ce_blt` without a mirror and without a lease: the destination has no system backing). The
  route's decision (`ce_route::decide`) gets a destination class "RM vidmem" in place of row 7's lease
  check; everything else (the record, the `h_client` rule, strikes, deadlines, bystander discharge)
  is unchanged.
* **Dispatch before the producer's boundary.** With no CPU lock left, the Present can return as soon as
  the copy is queued. The route still waits for the producer's boundary before submitting (15.1 of
  `rm-copy-engine-present.md`); that wait is on the worker, not on the app thread, so it costs latency,
  not throughput. Submitting at Present with the GPU acquire (`CeRtDirect`, 15.13) removes it once a stuck
  acquire can be released.

### 5.4 Fallback A: copy-only GDI acceleration on the copy engine

If `NonCpuVisiblePrimary` alone does not move redirection to `TEXTURE`, the documented switch is GDI
hardware acceleration (`SupportKernelModeCommandBuffer`): CDD then keeps redirection surfaces as GDI
`TEXTURE` and sends the GDI operations as kernel-mode command buffers (`DxgkDdiRenderKm`; BitBlt,
ColorFill, AlphaBlend, StretchBlt, TransparentBlt, ClearType). A driver can accept the subset it can do and
CDD falls back to the CPU path for the rest per surface; the copies and fills map onto the copy engine
(remap unit for formats, `rm-copy-engine-present.md` 12) and the rest stays CPU (on the staging half).
This is a larger change (a RenderKm parser, a GDI command executor on the CE channel, the CPU fallback)
and the earlier KMD CPU blitter was removed for reasons recorded in `query_adapter_info.rs`; it is the
fallback, not the plan.

### 5.5 Fallback B: a VRAM shadow behind the CPU-visible allocation

If Windows keeps a CPU-visible redirection surface whatever the caps, the zero-copy part is lost but the
lock can still be shortened: the copy engine writes the frame VRAM-to-VRAM into a KMD RM vidmem shadow
that DWM on NVK imports, and the guest pages (the CPU view) are written lazily. This needs DWM to sample
the shadow instead of uploading the CPU view, i.e. a UMD-side substitution in `dwm.exe` (the NVK DWM's
upload of that window's content replaced by a GPU copy from the shadow's resid). It is fragile (it depends
on how DWM's composition reads a CPU-GDI surface, not documented) and is written down only as the last
resort.

### 5.6 CPU and GDI readers of a GPU-only redirection surface

With `NonCpuVisiblePrimary`, Windows itself owns the CPU half: GDI renders into CPU-visible staging and
reaches the GPU-only surface through copies the KMD performs (Present Blts staging -> surface, which V2
adds), and readers such as `PrintWindow`, window capture (Windows.Graphics.Capture, BitBlt from the window
DC) and Desktop Duplication go through DWM or through a copy back into CPU-visible staging, which is the
same Blt in the other direction (surface -> staging) on the copy engine. No lazy BAR view of VRAM is
needed or wanted (WC reads at ~200 MB/s). The KMD's `MapCpuHostAperture` must refuse an RM vidmem
allocation loudly (counted) rather than map anything, which is what it does for a non-CPU-visible
allocation already.

### 5.7 How DWM reads it, and the earlier "DWM opens no KMD-made surfaces" finding

The census (340.1) found DWM opening no KMD standard allocation, because the redirection surface was
CPU-visible and DWM's composition reads such surfaces through dxgkrnl's CPU view. With a GPU-only
redirection surface Windows has no CPU view to give DWM, so DWM must open it as a shared texture
(`D3DKMDT_GDISURFACE_TEXTURE`: "created as a shared surface... used as a texture during DWM composition").
On an NVK DWM that open goes through the Helios UMD's `OpenResource`; the KMD's foreign identity and
layout trailer make `nvk_can_open` true and NVK imports the RM object by resource id
(`RM_RESOURCE_IMPORT`). A Venus DWM (`DwmIcd=venus`) cannot import RM vidmem: with `RedirVram` on, the
KMD keeps the Venus OPTIMAL image for a process that is not on NVK (the creator is the app, so the
decision is made at the first open by a Venus device: refuse and fall back to a placeholder until the
next window creation, counted), or `RedirVram` simply requires `DwmIcd=nvk`.

### 5.8 Risks

| risk | where it bites | mitigation |
|---|---|---|
| Windows ignores `NonCpuVisiblePrimary` without GDI acceleration | V1 | measured first; 5.4 |
| CDD with the cap needs Present Blts from CPU-visible staging into GPU surfaces the KMD refuses (0xE6) | desktop black in V1 | V2 adds the arm; V1 is opt-in and recovers with a device restart |
| residency/eviction: VidMm evicts the allocation (budget pressure) and asks for a paging transfer of a surface whose content lives only in RM | V2 | segment choice 5.3; paging content ops skip it (as for the GDI texture today); eviction leaves the RM object alive, page-in is a no-op |
| VRAM budget | many windows (6 MB at 1600x900, 29 MB at 5120x1440) under the host's `--vram-limit-mib` | count `RvBytes`; refuse above a cap and fall back to the Venus image |
| multi-monitor | redirection surfaces of windows spanning outputs | unchanged: one surface per window, the output does not matter |
| DWM restart | DWM's imports die with it; the allocation is the app's | the foreign record's lifetime rules (`shared-foreign-surfaces.md` 3, 6.1) already cover an opener's death |
| device restart / TDR | RM objects of the generation go | `rm_client::forget` sweep; allocations re-created by dxgkrnl after the reset; `RedirVram` refuses until the client is up |
| security | a GPU-only window surface reachable by `RM_DUP_OBJECT` | `NvDupHarden` (the route's `h_client` rule applies to the producer side; the destination is KMD-owned and never handed out as an RM handle, only as a resid to authorized openers) |

### 5.9 V2-V5 as built (`RedirVram`, default 0)

| piece | file | what |
|---|---|---|
| pure rules | `kmd_logic/src/rm_vidmem.rs` | knob, route (only `KmdOptimalGdiTexture`), layout (the ring surfaces' pitch-linear geometry, `rm_client::surface_layout`), RM's answer (`adopt`), the foreign layout, a 1 GiB budget, the channel map table (`MapBook`, keyed by resource id, stale on destroy), fixed handles and VA windows in the channel's client, `vram_copy` / `bounce_copy` plans, the counter list and its writer scan; 11 tests |
| allocation service | `kmd_render/src/virtio/rm_client/vidmem.rs` | its own RM client (level 5's bring-up machine and slot table `rm_sysmem::Svc`), `RM_ALLOC` of `NV01_MEMORY_LOCAL_USER`, export, `GEM_IMPORT_NVKMS`, an `RM_EXPORT` resource WITHOUT `USE_MAPPABLE` (`sysmem::import_resource_flags`), adoption by the WDDM allocation as a KMD_RM foreign resource with a LINEAR layout; `lookup`, `released` (route and channel forget it first, then GEM close and free), `forget` |
| allocation hook | `ddi/create_allocation.rs` | `GetStandardAllocationDriverData` reports 128 bytes of private data for a GDI `TEXTURE` when the knob is on (room for the layout trailer DWM's NVK opener needs, zeroed); `build_backing`'s GDI texture arm asks `vidmem::try_create` first, else today's Venus image |
| channel side | `kmd_render/src/virtio/rm_client/ce_vram.rs` | `ce_surface` (dup from the service's client into the channel's client, map at a fixed window, big pages then system flags), `copy` (VRAM to VRAM, no producer), `wait`, `transfer` (CPU bytes to or from a rectangle through the bounce buffer: RM system memory of the channel's client, CPU-mapped cached, GPU-mapped), teardown hooks in `ce_channel`/`ce_route` |
| generic submit | `ce_channel::submit_build`, `submit_copy` | a push the caller builds, ending with the completion release it is handed; a copy with no producer (acquire of the channel's own page at 0) |
| route | `ddi/ce_present_route.rs` | `try_route_vram`: the decision with `vram_destination_facts` (a VRAM object of the Blt's extent and pitch that fits a window) instead of the lease rule; the destination record's "descriptor" is its `ce_vram` mapping (no leases, no pin, no OS descriptor); the Venus fallback is the copy into the foreign-imported image if Venus imports it, else a submission with no command (`PreparedPresentBltSubmission::none`: the request terminalizes failed, the previous frame stays); `queue_async_blt` accepts an image destination only when it is a VRAM object; `vram_destination_gone` |
| Present | `ddi/vram_redirect.rs`, hook in `display.rs` | every Blt with a VRAM surface on either side, before any descriptor: NVK source -> VRAM (the route, `PBCpy` 7), VRAM -> VRAM (one CE copy, waited), standard buffer -> VRAM (upload of the buffer's authoritative CPU view, `read_standard_buffer`), VRAM -> standard buffer (readback into the blob and its system pages, `write_standard_buffer`); anything else a counted skip (`PBCpy` 8, `RvBltSkip`, `RvBltWhy`) |

Counters (`rm_vidmem::COUNTERS`, all `Rv*`): service `RvKnob RvTry RvOk RvVenus RvWhy RvStage RvFail
RvState RvLive RvBytes(MiB) RvFreed RvBring RvMs RvMsMax RvSoft RvLeak`; channel `RvMapOk RvMapFail
RvMapStat RvMapLive RvMapGive RvXfer RvXferFail RvXferWhy RvXferUs RvXferMax RvCopy RvCopyFail`; Present
`RvBltSeen RvBltRoute RvBltSkip RvBltWhy RvRdBack RvUpload RvGdiFail`. The route's `CeRt*` count the VRAM
Blts with the others. `RvBltWhy`: 1 route refused, 2 no foreign source (`ForeignCopy` 0), 3 no destination
descriptor, 4 format, 5 rectangle (stretch), 6 channel busy or down, 7 copy, 8 read, 9 write, 10 memory,
11 unknown source.

Known limits: the route's destination table has 8 entries (a VRAM destination destroyed with a copy in
flight keeps its entry until the generation ends); `ce_vram` maps 16 objects at a time (LRU); a Venus
DWM cannot import RM video memory (run with `DwmIcd=nvk`); the synchronous upload and readback run on the
Present thread (bounded by `XFER_MS` 250 ms plus `IO_MS` 2 s of RM I/O for the first bounce allocation);
sub-rectangles (`pDstSubRects`) are covered by copying the whole `DstRect`.

## 6. Staged plan and knobs

| stage | what | knob (default) | pass |
|---|---|---|---|
| V0 | phase 1 ETW capture (built) | none | 2.3 criteria; 2.4 filled |
| V1 | `VidMmCapsX` + `PBdSys` (built here) | `VidMmCapsX` (0) | table in 5.2 answered |
| V2 | RM vidmem backing for GDI `TEXTURE` (built, 5.9); Blt staging <-> surface on the CE (built: upload/readback) | `RedirVram` (0) | windows draw, `RvOk` = textures, `RvVenus` 0, DWM imports (`FgRiOk`) |
| V3 | the redirected Blt VRAM->VRAM on the CE route (built) | `RmCopyEngine=1` + `RedirVram=1` + `ForeignCopy=1` | `RvBltRoute` = Blts, `CeRtDone` grows, PresentMon `msInPresentAPI` < 0.2 ms, no Lock on the surface in the ETW capture |
| V4 | submit at Present with the GPU acquire (no worker hop) | `CeRtDirect` | stage timing: present -> CE done < 300 µs |
| V5 | soak, restart-device, DWM restart, multi-window, then defaults | | 10 min stress clean; `RvLeak` 0 |

## 7. Test recipe (V1)

1. Install the build, then in the guest:
   `reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v VidMmCapsX /t REG_DWORD /d 0x200 /f`
   and restart the device (`blrow.sh`-style: knob, `pnputil /restart-device`, DWM and shell restart).
2. Read `VmCapsRep` (must have bit 9: 0x268 with `CrossAdaptCaps` off, 0x278 with it on), `VmCapsXMsk` 0,
   `StdN*`, `StdO*`, `StdOpenPid`, `PBdStd`, `PBdGdi`, `PBdw`, `PBdh`, `PBdSto`, `PBdSys` before and after
   starting windowed Heaven (`hvwin.sh`). `PBd*` are sampled: read them after 10 s of Heaven.
3. Screenshot the desktop (`shot.ps1`): GDI windows (Explorer, Notepad) and Heaven drawn?
4. ETW capture as in 2.2 with label `hv-ncvp`.
5. Remove the knob, restart the device; the same counters with the knob absent are the baseline row.

## 7b. Test recipe (V2-V5, one package)

Only meaningful once Windows hands out GDI `TEXTURE` redirection surfaces, i.e. with fallback A's
`GdiAccel` (its own recipe); before that `RvTry` stays 0 and nothing changes (a cheap regression row).

1. Knobs: `RedirVram=1`, `RmCopyEngine=1`, `CeRtDirect=1`, `ForeignCopy=1`, `BltAsync=1`, `NvDupHarden` its
   default; `HKLM\SOFTWARE\Helios DwmIcd=nvk`; restart the device (`blrow.sh` pattern), then DWM and the shell.
2. Read `RvKnob` 1, `RvState` (phase 2 = up once a texture was made), `RvTry`/`RvOk`/`RvVenus`/`RvWhy`/`RvFail`.
3. Desktop: Explorer, Notepad, a GDI-heavy window drawn (upload: `RvUpload` grows, `RvGdiFail` 0).
4. Windowed Heaven: `RvBltRoute` and `CeRtDone` grow per frame, `RvBltSkip` flat, `PBCpy` 7;
   PresentMon `msInPresentAPI`; ETW capture `hv-vram` (no Lock on the destination).
5. CPU readers: Alt+PrintScreen of Heaven's window, Snipping Tool window capture, a PrintWindow tool:
   `RvRdBack` grows, the image is the frame.
6. Teardown: close Heaven (`RvFreed` grows, `RvLive` back), `taskkill dwm` (DWM restarts, windows redraw),
   restart the device with windows open (`RvLeak` 0).
7. Off row: `RedirVram` removed, restart: `RvTry` 0, everything as before.

## 8. Shared surface for fallback A (copy-only GDI acceleration)

Agreed with the fallback-A branch (`feat/vram-redirection-gdi-accel`): it consumes
`ce_vram::{CeSurface, ce_surface, ce_surface_cached, wait, transfer, copy, Dir}`, keyed by the
allocation's host resource id (`present_alloc_info(..).resource_id`), and
`ce_channel::{submit_build, submit_copy, SubmitError, poll}`; `build_paging_buffer::{read_standard_buffer,
write_standard_buffer}` for CPU-visible GDI surfaces. GDI `TEXTURE` is RM video memory only with
`RedirVram=1`. Every remapped CE launch programs its own remap state. This branch owns `ce_channel.rs`,
`create_allocation.rs`, `display.rs`, `ce_present_route.rs` and the new modules; fallback A owns its
`gdi_accel`/`gdi_exec` files and the RenderKm hooks.

## 9. Sources

Microsoft Learn: [DXGI flip model](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model),
[Comparing Direct2D and GDI](https://learn.microsoft.com/en-us/windows/win32/direct2d/comparing-direct2d-and-gdi),
[D3DKMDT_GDISURFACETYPE](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ne-d3dkmdt-_d3dkmdt_gdisurfacetype),
[GDI hardware acceleration](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gdi-hardware-acceleration),
[DXGK_VIDMMCAPS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_vidmmcaps),
[D3DDDICB_LOCKFLAGS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dukmdt/ns-d3dukmdt-_d3dddicb_lockflags),
[GPU segments](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-segments),
[CPU host aperture](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/cpu-host-aperature),
[DXGK_SEGMENTFLAGS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_segmentflags),
[D3DKMT_PRESENT_MODEL](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/ne-d3dkmthk-_d3dkmt_present_model),
[GPU paravirtualization](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization).
Other drivers: [VirtualBox VBoxMPWddm.cpp](https://github.com/VirtualBox/virtualbox/blob/main/src/VBox/Additions/win/Graphics/Video/mp/wddm/VBoxMPWddm.cpp),
[viogpu3d (kvm-guest-drivers-windows PR 943)](https://github.com/virtio-win/kvm-guest-drivers-windows/pull/943),
[WSL dxgkrnl](https://github.com/microsoft/WSL2-Linux-Kernel/tree/linux-msft-wsl-6.18.y/drivers/hv/dxgkrnl).
ETW layouts: [Windows 11 26H1 DxgKrnl manifest (Windows10EtwEvents)](https://github.com/jdu2600/Windows10EtwEvents/blob/master/manifest/Microsoft-Windows-DxgKrnl.tsv);
PresentMon's present-mode rules: [PresentMonTraceConsumer.cpp](https://github.com/GameTechDev/PresentMon/blob/main/PresentData/PresentMonTraceConsumer.cpp).

## 10. Fallback A as built: GDI acceleration on the copy engine (`GdiAccel`, default 0)

Status: G0 (caps + RenderKm census) and G1 (executor + fence gate) built on
`feat/vram-redirection-gdi-accel`, host-tested where pure (`kmd_logic::gdi_accel`, 17 tests), type-checked
against the stub WDK, never compiled against the real WDK and never run. V1 was negative on hardware
(`PBdStd` 3, `StdNGdiTex` 0 with `VidMmCapsX=0x200`), so this is the main path to a GPU-resident
redirection surface.

### 10.1 What Windows requires (research)

| item | mandatory? | source |
|---|---|---|
| `DXGK_PRESENTATIONCAPS.SupportKernelModeCommandBuffer` | the opt-in; "a required feature starting with WDDM 1.1" for full-graphics and render-only drivers in Microsoft's feature-caps table (certification), but not enforced at load: this KMD loads with it clear (22.22.180.0 A/B) | [GDI Hardware Acceleration](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gdi-hardware-acceleration), [WDDM driver and feature caps](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/wddm-driver-and-feature-caps), [DXGK_PRESENTATIONCAPS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_presentationcaps) |
| a cache-coherent aperture segment | precondition ("report this support only if the cache-coherent GPU aperture segment exists"); segment 1 is one | same |
| `DxgkDdiCreateAllocation`, `DxgkDdiGetStandardAllocationDriverData` (GDI surface types `TEXTURE`, `STAGING_CPUVISIBLE`, `STAGING`, `LOOKUPTABLE`, `EXISTINGSYSMEM`; Pitch returned for the CPU-visible ones) | mandatory | [Setting the Size and Pitch](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/setting-the-size-and-pitch-of-the-memory-allocation), [D3DKMDT_GDISURFACETYPE](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ne-d3dkmdt-_d3dkmdt_gdisurfacetype) |
| `DxgkDdiRenderKm` in `DRIVER_INITIALIZATION_DATA`; the GDI device/context (`GdiDevice`, `GdiContext` flags) | mandatory | [Initialization and DMA Buffer Creation](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/initialization-and-dma-buffer-creation), [DxgkDdiRenderKm](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_renderkm) |
| translate the WHOLE command buffer: BitBlt (1), ColorFill (2), AlphaBlend (3), StretchBlt (4), TransparentBlt (6), ClearTypeBlend (7); Escape (5) ignored | mandatory: the return codes have no per-operation decline | [Specifying GDI Hardware-Accelerated Rendering Operations](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/specifying-gdi-hardware-accelerated-rendering-operations), the `DXGK_GDIARG_*` pages |
| the output patch-location list with every allocation reference | mandatory | DxgkDdiRenderKm remarks |
| `NoSameBitmap*`, `NoSameBitmapOverlapped*`, `NoScreenToScreenBlt`, `NoOverlapScreenBlt` | optional declines (dxgkrnl "will not request") | DXGK_PRESENTATIONCAPS |
| `SupportAllBltRops`, `SupportMirrorStretchBlt`, `SupportMonoStretchBltModes` | optional; clear = CDD sends only the named ROPs, no mirror, no BLACKONWHITE/WHITEONBLACK | same, `DXGK_GDIARG_BITBLT`, `DXGK_GDIARG_STRETCHBLT` |
| `AlignmentShift` (>= 2), `MaxTextureWidthShift`/`HeightShift` | required fields | DXGK_PRESENTATIONCAPS |
| present CDD operations into DWM's UMD-created textures | implied by GDI acceleration (`DriverSupportsCddDwmInterop` is then ignored) | DXGK_PRESENTATIONCAPS |

Public drivers: none implements RenderKm. VirtualBox's WDDM driver reports `NoScreenToScreenBlt |
NoOverlapScreenBlt | AlignmentShift 2 | MaxTexture*Shift 2` without `SupportKernelModeCommandBuffer` and
leaves its GDI surface case commented out
([VBoxMPWddm.cpp](https://github.com/VirtualBox/virtualbox/blob/main/src/VBox/Additions/win/Graphics/Video/mp/wddm/VBoxMPWddm.cpp));
viogpu3d registers no RenderKm and reports `PresentationCaps` 0
([viogpu_adapter.cpp](https://github.com/max8rr8/kvm-guest-drivers-windows/blob/viogpu3d/viogpu/viogpu3d/viogpu_adapter.cpp));
Microsoft's render-only sample driver sets it FALSE and stubs RenderKm
([graphics-driver-samples, RosKmd](https://github.com/microsoft/graphics-driver-samples)); the KMDOD sample
is display-only. The operation semantics this executor implements come from the Learn pages alone
(the formulas of `DXGK_GDIARG_ALPHABLEND`, `_TRANSPARENTBLT`, `_STRETCHBLT`, `_CLEARTYPEBLEND`,
`_BITBLT`). The fill words follow Mesa NVK's `nvk_cmd_fill_memory_ce` (remap `CONST_A`,
[nvk_cmd_copy.c](https://gitlab.freedesktop.org/mesa/mesa/-/blob/main/src/nouveau/vulkan/nvk_cmd_copy.c)).
PresentMon's "Composed: Copy with GPU GDI" is a Blt with `bRedirectedPresent` 0 followed by
PresentHistory model `REDIRECTED_BLT`, independent of these caps
([PresentMonTraceConsumer.cpp](https://github.com/GameTechDev/PresentMon/blob/main/PresentData/PresentMonTraceConsumer.cpp)).

### 10.2 The caps word

`GdiAccel=1` reports `0x0A06C8FC` (`kmd_logic::gdi_accel::ACCEL_CAPS`; bit positions from the C bitfield
order of WDK 10.0.26100.0 `d3dkmddi.h`, NOT the Learn page's "Nth bit" sentences, which are wrong after the
4-bit `AlignmentShift`): `SupportKernelModeCommandBuffer` (bit 2), `NoSameBitmapAlphaBlend`,
`NoSameBitmapStretchBlt`, `NoSameBitmapTransparentBlt`, `NoSameBitmapOverlappedAlphaBlend`,
`NoSameBitmapOverlappedStretchBlt` (3-7), `AlignmentShift` 2 (10-13), `MaxTextureWidthShift` and
`MaxTextureHeightShift` 3 = 16384 (14-16, 17-19), `NoSameBitmapOverlappedBitBlt` (25: a scroll goes back to
CDD; a disjoint copy inside one surface stays ours), `NoTempSurfaceForClearTypeBlend` (27). Clear:
`SupportAllBltRops`, mirror, mono modes, `NoScreenToScreenBlt`/`NoOverlapScreenBlt` (the Present path is
unchanged), the reserved bits. `GdiAccel=2` reports only `DriverSupportsCddDwmInterop` (0x100) and
`GdiAccel=3` only `SupportSoftwareDeviceBitmaps` (0x10000000): one-bit experiments without GDI
acceleration (RenderKm stays the pass-through; `GdiKnob`/`GdiCaps` mirror the word). Knob absent or any
other value: 0, as before.

### 10.3 The pieces

| piece | file | what |
|---|---|---|
| pure rules | `kmd_logic/src/gdi_accel.rs` | caps; the `DXGK_RENDERKM_COMMAND` reader (x64 offsets checked against a C compile of the header's declarations; refusals `Bad`: size, opcode, sub-rects, trailer); `plan` (engine and `Why`); the CE words of a fill (`SET_REMAP_CONST_A/B/COMPONENTS` = `CONST_A` x4, `COMPONENT_SIZE_FOUR`, one component; `LAUNCH_DMA` with `REMAP_ENABLE`, pitch, multi-line, non-pipelined) and of a rectangle copy (`ce_present::copy`, `Remap::None`); the CPU reference executor (ROP3, the named ROPs, AlphaBlend premultiplied `AC_SRC_OVER`, TransparentBlt, StretchBlt with the truncate mapping and mirroring, ClearTypeBlend with and without the gamma table); the private record `"HGDA"`; the timeline |
| caps, RenderKm and RenderGdi | `kmd_render/src/ddi/gdi_accel.rs` | `reported_caps` (query_adapter_info), `note_start` (AdapterKnobs at StartDevice), `note_create_device`/`note_create_context` (census), `render_km` and `render_gdi` (GpuMmu adapters get RenderGdi), both into `translate`: parse, resolve each allocation index through `present_alloc_info` and classify it (VRAM: `vidmem::lookup`; System: a `PitchedStandardBuffer`; else Unreachable), plan, materialise and clip sub-rectangles (inline from the command buffer, or copied from dxgkrnl's kernel pointer), patch list, clear the private data and write the job id, 16-byte DMA marker |
| executor | `kmd_render/src/ddi/gdi_exec.rs` | job table (`commit`, orphans above 256 unclaimed), `admit` at SubmitCommand, `seq_ready` for the fence, `service` on the HPD worker, `discharge_all` at StopDevice |
| seam | `kmd_render/src/ddi/gdi_ce_glue.rs` | the only calls into the V2-V5 modules (section 8): `vidmem::lookup`, `ce_vram::{ce_surface, wait, transfer}`, `ce_channel::submit_build`, `read/write_standard_buffer`; a busy channel retried 4 x 1 ms |
| fence gate | `virtio/gpu/mod.rs` | `WddmPending::gdi_seq`: the immediate-signal path and the FIFO head both wait for `seq_ready`; not rebasable |
| hooks | `submit_command.rs` (RenderKm body, both SubmitCommand entry points), `hpd.rs`, `lifecycle.rs`, `query_adapter_info.rs`, `adapter/mod.rs`, `diag.rs` | one relaxed load each with the knob off |

### 10.4 A GDI command's path

1. CDD -> `DxgkDdiRenderKm` (PASSIVE): the buffer becomes job N (commands, surfaces, engine, clipped
   sub-rectangles); `pDmaBufferPrivateData[0..16]` = `"HGDA"`, version 1, N (the rest of the 112 bytes
   zeroed: a recycled buffer's stale `"HPBL"` Present prefix would otherwise gate this fence on an old copy).
2. `DxgkDdiSubmitCommand` (DISPATCH): `admit(N)`: first time, the next sequence S and a worker wake; a
   preempted replay, the same S; N already executed and gone, no wait. The WDDM entry carries `gdi_seq` S.
3. HPD worker: jobs in S order. Engine `Ce` (SRCCOPY BitBlt, PATCOPY ColorFill, every surface VRAM): one
   push per 10-11 rectangles through `submit_build`, waited at most 100 ms. Engine `Cpu` (everything else,
   or a CE failure): the bounding window of the sub-rectangles read from each surface (VRAM through the
   bounce buffer, a standard buffer through its authoritative CPU view at the command's pitch for
   `STAGING_CPUVISIBLE`), the reference executor over each sub-rectangle, the destination window written
   back (`write_standard_buffer` updates the blob and the system pages VidMm holds). Then the watermark
   moves to S and a completion DPC retires the fence.

Every admitted job completes (CE failure -> CPU; CPU failure -> dropped, counted; StopDevice discharges),
so a GDI fence cannot block the adapter-global FIFO forever.

### 10.5 Counters (`gdi_accel::COUNTERS`, written only by `gdi_accel.rs` and `gdi_exec.rs`)

| counter | meaning |
|---|---|
| `GdiKnob`, `GdiCaps` | knob in force, the PresentationCaps word reported |
| `GdiCmdN`, `GdiOpN` | RenderKm calls, commands parsed |
| `GdiBad`, `GdiBadWhy` | refused command buffers (parsing stopped there), last `Bad` code (1 size, 2 opcode, 3 sub-rects, 4 trailer) |
| `GdiOpMask` | opcodes seen, bit = opcode (2 BitBlt, 4 ColorFill, 8 AlphaBlend, 16 StretchBlt, 32 Escape, 64 TransparentBlt, 128 ClearType) |
| `GdiRopMask` | ROPs seen: BitBlt bit = rop (1 SRCCOPY .. 5 ROP3), ColorFill bit = 8 + rop (1 PATCOPY .. 7 ROP3) |
| `GdiBltN`, `GdiFillN` | commands executed on the copy engine |
| `GdiFall` | commands executed on the CPU |
| `GdiDrop` | commands not executed |
| `GdiWhy`, `GdiMask` | last reason off the copy engine, every reason seen (`1 << code`): 1 ROP, 2 blend, 3 stretch, 4 transparent, 5 ClearType, 6 a system surface, 7 overlap, 8 CE failed (redone on the CPU), 9 unreachable surface, 10 bad index, 11 out of bounds, 12 CPU failed, 13 timeout |
| `GdiJobN`, `GdiAgain`, `GdiDone`, `GdiOrph` | jobs admitted, replays, executed, unclaimed jobs dropped |
| `GdiCeSub`, `GdiRects` | CE pushes, sub-rectangles parsed |
| `GdiUs`, `GdiUsMax` | executor time per job (sum, max, µs) |
| `GdiCls` | surface classes seen: destination bit 0 VRAM, 1 standard buffer, 2 unreachable; sources the same at bits 4-6 |
| `GdiDstRes`, `GdiDstWH` | last destination's resource id and `w << 16 \| h` |
| `GdiRkIn`, `GdiRgIn` | entries into `DxgkDdiRenderKm` / `DxgkDdiRenderGdi` with the knob on, before any parsing |
| `GdiDevN`, `GdiCtxN`, `GdiCtxFl` | GDI devices (`GdiDevice`) and GDI contexts (`GdiContext`) created, counted with the knob off too; the last GDI context's raw `DXGK_CREATECONTEXTFLAGS` (bit 2 `VirtualAddressing`) |

Mirrored at the first RenderKm, every 64th, and after each worker pass that ran a job.

### 10.6 Unverified, and known limits

* **G0 on hardware (356.1, 5120x1440):** Windows accepts the caps (adapter OK, `StdNGdiTex` 18 /
  `StdOGdiTex` 38, `StdNGdiStgCpu` 5, `StdNGdiStg` 1, `StdNGdiLut` 1) but `GdiCmdN` stayed 0: RenderKm
  was never called. The reason: on a GPU-virtual-addressing adapter (this one reports GpuMmu) dxgkrnl's
  `ADAPTER_RENDER::DdiRenderGdi` calls `DxgkDdiRenderGdi` (`DXGKARG_RENDERGDI`: the same
  `DXGK_RENDERKM_COMMAND` stream, no patch lists, the DMA buffer's GPU VA), not RenderKm; the KMD's
  RenderGdi was still the pass-through. Both entry points now share the translation (`render_km`,
  `render_gdi` -> `translate`), and `GdiRgIn`/`GdiRkIn`/`GdiCtxN`/`GdiCtxFl` show which one runs.

* Never run. Whether Windows 11 26H1 still drives GDI acceleration through CDD for an adapter that
  advertises it late (no other public driver does) is the first thing G0's census answers.
* `NumSubRects` 0 is taken as "the destination rectangle alone" (not documented); an external `pSubRects`
  (outside the command buffer) is read as a kernel pointer (documented as needing no try/except).
* The `Rop3` field is read as its low byte.
* Without `RedirVram` a GDI `TEXTURE` is a Venus image: Unreachable, every command on it dropped. G1 needs
  `RedirVram=1` (and the copy-engine channel up) for windows to draw.
* `EXISTINGSYSMEM` surfaces wrap user memory Windows hands to CreateAllocation; today's
  `GetStandardAllocationDriverData` backs every non-TEXTURE GDI surface with a KMD standard buffer, so its
  content is not the user's pages (counted as a standard buffer, executed on the wrong bytes). To fix in
  the allocation arm (owned by the V2 branch) if the census shows Windows using it.
* The CPU path is synchronous on the HPD worker (bounded by the bounce transfer's 250 ms per surface) and
  reads whole bounding windows; a GDI-heavy desktop costs worker time the Present path shares.
* ClearType's gamma row is read from the `LOOKUPTABLE` surface at `Gamma * pitch` (8 bpp, 512 entries);
  if that surface is not a standard buffer the blend runs without gamma.

### 10.7 Test recipe (main session)

G0 rows (census only; with the G1 build the same counters plus execution):

1. `reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v GdiAccel /t REG_DWORD /d 1 /f`;
   for G1 also the 7b knobs (`RedirVram=1`, `RmCopyEngine=1`, `CeRtDirect=1`, `ForeignCopy=1`, `BltAsync=1`,
   `DwmIcd=nvk`). Reboot (the caps are read at AddAdapter), or `pnputil /restart-device` then DWM and the
   shell.
2. Code 0? `GdiKnob` 1, `GdiCaps` 0x0A06C8FC.
3. Before and after Explorer, Notepad, windowed Heaven (10 s): `StdNGdiTex`, `StdOGdiTex`, `StdOpenPid`,
   `PBdStd` (4 = GDI surface), `PBdGdi` (1 = TEXTURE), `RvTry`/`RvOk`, the `Gdi*` row.
4. Screenshot (`shot.ps1`). G0: content wrong or stale is expected. G1: windows drawn; `GdiDrop` flat,
   `GdiBltN`/`GdiFillN` growing, `GdiFall` for blends and text.
5. Heaven: PresentMon `msInPresentAPI`, ETW capture `hv-gdi` (no Lock on the redirection surface).
6. Off row: knob removed, reboot: `GdiKnob` absent or 0, `PresentationCaps` 0 in the 0x01D1 record.

What each G0 outcome means: `GdiCmdN` 0 and `StdNGdiTex` 0 -> Windows ignores the cap here (then 5.5);
`StdNGdiTex` > 0 and `PBdStd` 4 for Heaven -> the redirection surface is GPU-only, G1 with `RedirVram` is
the path; adapter fails to start or a bugcheck in RenderKm -> the counters and the code to the lane.
