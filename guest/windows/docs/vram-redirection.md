# The redirection surface in GPU memory (lane F): findings, design, staged plan

Status: phase 1 tooling built and handed to the hardware session; phase 2 design written; stage V1
(`VidMmCapsX` caps knob, default off, and the `PBdSys` counter) built, host-tested (kmd-dev gate rc=0,
stubcheck: no new errors), never run on hardware. Branch
`feat/vram-redirection`, based on `feat/rm-copy-engine-present` (PR #2), because the later stages
copy on the KMD's own RM copy-engine channel. Results of the hardware captures go in section 2.4.

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

Pending: main session's captures (`hv-rmce`, `hv-rmce-cs`, optionally `hv-venus`).

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

**Leading hypothesis (to be settled by `PBdStd` and the Lock rows of 2.4).** The census's one STAGING
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

## 6. Staged plan and knobs

| stage | what | knob (default) | pass |
|---|---|---|---|
| V0 | phase 1 ETW capture (built) | none | 2.3 criteria; 2.4 filled |
| V1 | `VidMmCapsX` + `PBdSys` (built here) | `VidMmCapsX` (0) | table in 5.2 answered |
| V2 | RM vidmem backing for the GPU-only GDI surface type V1 names; Blt staging <-> surface on the CE channel | `RedirVram` (0) | windows draw, `RvMade` = surfaces, `RvFall` 0, DWM imports (`FgRiOk`) |
| V3 | the redirected Blt VRAM->VRAM on the CE route | `RmCopyEngine=1` + `RedirVram=1` | `CeRtOk` = Blts, PresentMon `msInPresentAPI` < 0.2 ms, no Lock on the surface in the ETW capture |
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

## 8. Sources

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
