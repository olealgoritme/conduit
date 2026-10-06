# Shared surfaces between NVK processes, and DWM on NVK (stage S6 groundwork)

Status (2026-10-06): design plus implementation of the NVK, librmclient, DXVK, UMD and host
halves of NVK↔NVK shared surfaces; the KMD half is specified in section 7 (its open/lifetime
part exists on `kmd/s6-shared-foreign`, not released). Section 8 is the design for running DWM
itself on NVK. Read `dxvk-on-nvk.md` (sections 3.6, 3.7, S6 on `research/dxvk-on-nvk`),
`zero-copy-present.md` and `shared-foreign-surfaces.md` (`kmd/s6-shared-foreign`) first.

| piece | where | state |
|---|---|---|
| host verb `RmResourceImport` (MsgType 31) | `feat/s6-backend` (`host/backend/device/src/nvidia/rm_resource.rs`, docs/VENUS.md) | done, unit-tested; route verified on the host driver |
| librmclient `crm_win_rm_resource_import`, `crm_dup_object` | `guest/rmclient` (this branch) | done (KMD op 3 of `FOREIGN_RESOURCE`); DUP tested in win11 |
| NVK patch 0031 | `guest/nvk-rm/patches-windows/0031-*.patch` | done, builds; tested in win11 up to the host verb |
| DXVK patch 0002 | `third_party/patches/dxvk/0002-helios-nvk-shared-import.patch` | written, applies on 0001; not compiled |
| UMD bridge | `umd/bridge/dxvk_bridge.cpp` `open_ddi_texture2d` | written; not compiled (no driver build was run) |
| KMD | section 7 | open half on `kmd/s6-shared-foreign`; `FOREIGN_RESOURCE` op 3 `RM_RESOURCE_IMPORT` on `kmd/rm-resource-import` (v313) |

## 1. The problem

A D3D11 app on NVK (Helios UMD → DXVK → NVK on RM) opens a surface another NVK process created:
DXGI shared handle, NT handle, `OpenSharedResource[1]`, a keyed-mutex texture. Both processes
must map the same RM memory. Each NVK process has its own RM client (librmclient over the KMD's
NVRM escapes), so the creator's `(hClient, hMemory)` means nothing in the opener's client.

The creator already has a name for the memory that every component accepts: the Helios
resource id (resid) minted by `IMPORT_RM` (S3), adopted by the WDDM allocation, written into
every opener's `HeliosWddmOpenIdentity` by `DxgkDdiOpenAllocation`. The question is only how the
opener turns a resid back into RM memory of its own client.

## 2. Candidates, and what the hardware stack actually does

| route | how | result |
|---|---|---|
| A. `NV_ESC_RM_DUP_OBJECT` across clients | opener dups `(hClientSrc, hMemorySrc)` into its client | **works today, end to end, with no change anywhere** (`guest/rmclient/tests/crm_share_smoke.c`, win11, KMD 22.22.311.0): the opener CPU- and GPU-maps the creator's VRAM and sysmem, sees its pattern, the creator sees the opener's writes; the dup survives the creator's free and the creator's process exit; a dup from a non-existent client is refused. The host passes it because every RM client of a VM lives in the one backend process |
| B. creator's export fd (0x3d05) handed to the opener, opener 0x3d06 | fd is a backend handle of the creator's device | works by accident for the same reason |
| C. resid → host → GEM on the opener's render node → opener's client | backend PRIME-imports the dma-buf it holds for the resid on a DRM file of the opener; opener `GEM_EXPORT_NVKMS_MEMORY` + 0x3d06 | **chosen**; verified on the host driver (section 6) |

A and B are rejected as the production route: they need the creator's RM handles (or fds) in
another process, which the KMD refuses to hand out (`shared-foreign-surfaces.md` §1: "the resid
is the only route"). They also show a hole: the KMD does not inspect NVOS55 `hClientSrc` or
the fds inside 0x3d05/0x3d06/NVKMS payloads, so any process can map any other process's RM
memory today if it guesses the handles (client handles are sequential, `0xc1d0xxxx`, memory
handles start at `0x5c000001`). Section 7 lists the check.

C keeps D1 of dxvk-on-nvk.md (the resid is the only cross-component buffer name), needs no RM
handle in another process, and reuses three things that already exist: the backend's dma-buf of
every RM-export resource, the forwarded nvidia-drm ioctls, and NVK's Linux dma-buf import
(`nvkmd_rm_drm.c` `nvkmd_rm_import_dma_buf`).

## 3. The route, step by step

```
creator A (NVK)                         KMD                    backend (host)              opener B (NVK)
alloc RM memory (Helios export)
0x3d05 → GEM_IMPORT_NVKMS on A's DRI
IMPORT_RM(ctx, A's DRI, gem, layout) ─► mint resid R ─────────► RESOURCE_CREATE_BLOB(RM_EXPORT)
                                                                holds dma-buf(R), modifier
CreateAllocation adopts R
                                         DxgkDdiOpenAllocation(B)  ◄───────────────────────── OpenResource
                                         identity{R, foreign}, layout trailer ─────────────►  UMD open_resource
                                                                                              DXVK: OPTIMAL image, same desc
                                                                                              vkAllocateMemory(+IMPORT_MEMORY_RESOURCE_INFO R)
                                         FOREIGN op 3 ◄────────────────────────────────────── crm_win_rm_resource_import(B's DRI, R)
                                         check: B opened R, msg 31 ► PRIME_FD_TO_HANDLE(dma-buf(R)) on B's DRI file
                                                                    reply {gem, size, modifier} ─► 
                                                                                              GEM_EXPORT_NVKMS_MEMORY(gem, memFd = B's ctl)
                                                                                              0x3d06 into B's client → hMemory_B
                                                                                              GEM_CLOSE(gem); GPU-map; bind image
```

- **Layout.** NVK on Windows has no `VK_EXT_image_drm_format_modifier` (its RM backend reports
  `has_alloc_tiled = false` there). The opener's DXVK builds the image from the same D3D
  description (OPTIMAL), which NIL lays out identically for the same format/extent; NVK then
  **checks** (patch 0031, `nvk_helios_check_import_layout`) that the image's modifier, row pitch
  and plane offset equal the open's layout trailer, and that the host's modifier for R agrees.
  A mismatch is `VK_ERROR_INVALID_EXTERNAL_HANDLE`, never a silently misread surface. Imported
  memory is shared, so images bound to it get their own VA with the uncompressed kind.
- **Lifetime.** Once 0x3d06 succeeded, B's RM handle keeps the memory alive by itself (RM
  refcount; the GEM handle is closed at once). The resid's lifetime is the KMD's
  (`kmd/s6-shared-foreign`: host resource until the last of creator destroy and every open's
  close). The memory owns no resid release: `helios_res_adopted = true` on import.
- **Resource id reuse.** The imported memory answers `memory_res_id` with R, so B can present
  or re-share the surface by the same id.
- **Limits.** 32 bpp BGRA/RGBA 2D single-plane surfaces, as `IMPORT_RM` itself (the formats
  the foreign layout can name); VRAM only (NVK allocates exportable images there). Other
  formats (R16F, NV12 video surfaces) need `IMPORT_RM` to carry a NIL layout descriptor instead
  of a fourcc/modifier pair; until then such apps stay on the deny-list.

## 4. Sync and keyed mutex between processes

The D3D11 keyed mutex is the Microsoft runtime's (DXVK's own `DxvkKeyedMutex` is stripped by
the UMD: `api_misc_flags` turns `SHARED_KEYEDMUTEX` into plain `SHARED`). `AcquireSync` blocks
on the CPU until the key is released; `ReleaseSync` flushes the context (`pfnFlush`) and
releases with a fence value that dxgkrnl orders against the context's DMA buffers. NVK's GPU
work is not in those DMA buffers, so dxgkrnl cannot order it.

- **v1 (needed before shipping, UMD only):** in `pfnFlush` of an NVK device that holds a live
  shared allocation created or opened with `SHARED_KEYEDMUTEX`, flush the immediate context and
  wait for the submission on the CPU before returning. The releaser's GPU writes are then
  complete before the runtime's `D3DKMTReleaseKeyedMutex2`, and the acquirer's later NVK work
  starts after its `AcquireSync` returned. Same CPU-complete rule as S3's presents. Scope it to
  keyed-mutex resources: plain DXGI-shared surfaces have no defined cross-process order beyond
  flush, and a per-flush wait for every flip-model app would cost frames. Not implemented on
  this branch (`umd/src/forward/transfer.rs` `flush`, with a per-device count of live keyed-mutex
  resources kept in `resource.rs` create/open/destroy).
- **S4 (GPU-side):** the flush records an RM semaphore point and submits a DMA buffer carrying
  an RM-fence boundary (`rm-fence-marker.md` on `kmd/s4-rm-fence-marker`: HE12 v4 / HERF / HEPR
  tails). dxgkrnl then retires the releaser's DMA buffer only when the RM semaphore reached the
  value, so the keyed-mutex fence is correct without a CPU wait on the releaser. The acquirer
  still issues NVK work dxgkrnl does not see: it needs a GPU wait on the releaser's semaphore.
  Proposal for S4/S6: per shared resource, the KMD records the last writer's `(RM semaphore
  surface resid, value)` at each release boundary; the acquirer's NVK imports the semaphore
  surface once by the same RmResourceImport route (it is RM memory) and inserts a semaphore
  acquire before the first use after `AcquireSync`. Until then, keep the v1 CPU rule on the
  acquire side too (wait for nothing: v1 already completes the releaser).

**Implemented (flush gate, `docs/flush-gate.md`, KMD `kmd/flush-completion` 4fc8df4, v315):**
`umd/src/forward/transfer.rs` `flush_gate`. On every flush (and NVK present) of a device holding
a created or opened `MISC_SHARED` non-`BIND_PRESENT` resource, when DXVK recorded work since the
previous gate (CS sequence number), one 48-byte `HEFL` through `pfnRenderCb` on the device's
context, no allocations: NVK `RM_FENCE` (NVRM caps bit 34, the S4 queue fence after the
submission thread drained); Venus `STREAM` (scanout probe bit 5; a point of the present stream
signalled behind the recorded work by DXVK patch 0003 `HeliosSignalFlushPoint`, the stream
registered without a producer allocation if no present did it first), else the wire rung after
the submission thread. Without a carrier the CPU wait above remains (NVK by default, Venus with
`HELIOS_KEYED_FLUSH_WAIT=1`). A failed packet never fails the flush. `d3d11_share keyed-load`
prints the KMD's `FlGRec FlGStrm FlGFnc FlGWire FlGDeg` deltas.

## 5. Implementation on this branch

- **librmclient**: `crm_win_rm_resource_import(rm_handle, resource_id, &gem, &size, &modifier,
  &flags, &host_errno)`: `HELIOS_ESCAPE_FOREIGN_RESOURCE` op 3 (`RM_RESOURCE_IMPORT`, 80-byte
  `helios_foreign_rm_resource_import`; the KMD checks that this process opened the resource or
  this device imported it, then sends the backend's msg 31). `-ENOSYS` while the KMD lacks the
  op (QUERY_CAPS `supported_ops` bit 3, learnt lazily) or the gate is closed (cap bit 2). `crm_dup_object` (NVOS55) with unit tests and `crm_share_smoke`: kept as
  RM API coverage and as the proof of the KMD hole; NVK does not use it.
- **NVK patch 0031** (applies on 0023 alone or on nvk-rm/integration after 0029, before
  `patches-windows-dxvk/`; ordered as `5c7e2b8 → 0031 → dxvk 0001..0004`):
  `HELIOS_ICD_CAP_SHARED_IMPORT`, `HELIOS_STRUCTURE_TYPE_IMPORT_MEMORY_RESOURCE_INFO`
  (1000384004), `nvkmd_rm_mem_import_resource`, layout check. `NVK_HELIOS_SHARED_IMPORT=0`
  disables it. The `helios_icd_api` table is unchanged (no new entry: the resid already
  travels).
- **DXVK patch 0002**: on NVK, an import with `D3D11_HELIOS_IMPORT_INFO::Foreign` and a resid is
  allowed; dedicated memory with `HeliosImportMemoryResourceInfo` (resid, `AllocSize` = the
  open's `venus_alloc_size` (the KMD's verified size), modifier, stride, offset). Every other
  import on NVK still throws.
- **UMD**: `open_ddi_texture2d` admits NVK opens of foreign surfaces when the ICD has
  `HELIOS_ICD_CAP_SHARED_IMPORT`; refuses (logged) otherwise. `open_resource` already parses the
  layout trailer and passes it.
- **Backend** (`feat/s6-backend`): `RmResourceImport`, `NVGPU_CFG_RM_RESOURCE_IMPORT = 1 << 14`
  (set with `NVGPU_CFG_RM_IMPORT`), docs/VENUS.md "RM-export resources in a second process".

## 6. Tests and results

| test | where | result |
|---|---|---|
| `crm_share_smoke` (two processes, librmclient, DUP_OBJECT) | win11, `C:\Users\Public\s6\rmc`, KMD 311, today's backend | all pass: VRAM and sysmem shared both ways, dup outlives creator free and creator exit, bogus client refused |
| `rm_export_exec` + `rm_reimport_check` (the route of section 3 without a guest) | host, RTX 5090, 610.57.04 | LINEAR and block-linear h=5 1920×1080: 0 of 2073600 pixels differ through a second RM client |
| `vk_dmabuf_to_rm dmabuf` (host Vulkan memory → RM client, spike X4) | host | exact; `opaque`: `PRIME_FD_TO_HANDLE` EBADF |
| backend unit test `an_rm_resource_becomes_a_gem_handle_on_another_render_node` | `cargo test -p device --features venus` | pass (342 lib tests) |
| `helios_share_test` (two NVK processes, no UMD) | win11, `C:\Users\Public\s6\nvk`, NVK s6 build | creator: caps 0x17, export image, IMPORT_RM resid with layout; opener: wrong-layout import refused; real import stops at RM_RESOURCE_IMPORT: KMD 311 has no op 3 (`-ENOSYS`), raw FORWARD of msg 31 is refused (`-EPERM`) |
| `test_unit`, `test_win_wire` | host | pass |

What is left to prove on the guest, with KMD v313 and a backend carrying `feat/s6-backend`:
a D3D11 two-process sample (`OpenSharedResource` / NT handle + keyed mutex) on NVK through the
UMD, which is the only process pair the KMD lets through (the opener must have opened the
allocation; `helios_share_test` without a WDDM open is refused `NOT_OWNED` by design and stays
useful for the creator half and the layout check).

## 7. KMD change list (for the KMD session)

1. **RM_RESOURCE_IMPORT** — done on `kmd/rm-resource-import` (v313): `FOREIGN_RESOURCE` op 3,
   cap bit 2 (gated by config bit 14), checks the caller's DRI file and that the caller created
   or opened the resource, sends msg 31; raw FORWARD of msg 31 stays refused.
2. **Close sweep**: the returned GEM handle belongs to B's DRI file; nothing to track for
   correctness (the backend forgets on GEM_CLOSE / file Close), but recording `(device, file,
   gem)` lets the per-device sweep close leaked ones.
3. **Payload handle checks** (hardening, now concrete): in `MSG_IOCTL`, refuse NVOS55
   (`NV_ESC_RM_DUP_OBJECT`, nr 0x34) whose `hClientSrc` is not a client created on one of the
   caller's own ctl handles (snoop `NV01_ROOT` allocation replies per owner, as the backend's
   `note_clients` does); refuse 0x3d05/0x3d06 and NVKMS `memFd` values that are not the
   caller's own backend handles. Without this, section 2's route A works for any process.
4. **Primary / KMD-owned allocations for DWM on NVK** (section 8): with `KmdRmClient=1`, mint
   a foreign resid for each KMD-owned RM allocation (the KMD is the creator: export to a GEM
   handle on the KMD's own DRI file, RESOURCE_CREATE_BLOB RM_EXPORT, layout record), so DWM's
   NVK opens them by the same route.
5. **Option B flip**: `program_vidpn_source_inner` sends `ScanoutFlip` for an allocation that
   adopted a foreign resid (layout from the foreign record), no `SET_SCANOUT_BLOB`.

## 8. DWM on NVK (design)

**Goal.** DWM's D3D11 device runs DXVK on NVK; the desktop is composed and scanned out from RM
memory; Venus is no longer needed for the desktop.

1. **Selection.** Keep the deny-list mechanism; drop `dwm.exe` from it behind a knob
   (`HKLM\SOFTWARE\Helios!DwmIcd = nvk|venus`, default `venus` until the gates below pass).
   The UMD's `IcdBackend` choice is per process already.
2. **What DWM renders into.** Its flip chain (the "primary" swapchain DXGI makes on the
   output) is created by DWM's own D3D11 device: on NVK these are dedicated exportable NVK
   images (Helios export request), each with a foreign resid adopted by its WDDM allocation —
   S3's path unchanged. The KMD-owned allocations that DWM also touches (shared primary,
   shadow and staging standard allocations, `KmdOptimalGdiTexture`, `KmdStandardBuffer`) come
   from the KMD's own RM client (slice 1 of `kmd-rm-client.md`, knob `KmdRmClient`), with a
   foreign resid each (section 7 item 4) so DWM opens them like any NVK surface. GDI paging
   (`build_paging_buffer.rs` transfers) goes through CPU mappings of that RM memory (BAR1) or
   later the copy engine.
3. **Composition inputs.**
   - NVK apps' surfaces: sections 3-5 (resid → GEM → DWM's RM client).
   - Venus apps' surfaces during the transition (deny-listed browsers, video, interop apps):
     **needed** — DWM composes every window. Spike X4 says the reverse direction is cheap on
     the host: memory NVIDIA's Vulkan driver exports as **DMA_BUF** imports into an RM client
     through the same nvidia-drm calls (`vk_dmabuf_to_rm`, exact), **OPAQUE_FD** does not.
     Proposal: (a) Venus processes allocate shared images with DMA_BUF export and an explicit
     DRM modifier (the scanout-image path already does); (b) the backend extends
     `RmResourceImport` to Venus blobs whose renderer export is a dma-buf (today it answers
     `EINVAL` for non-RM-export resources) and reports the modifier the creator chose; (c) DWM's
     NVK imports them as in section 3 (layout from the open's meta for Venus surfaces, which then
     must carry the modifier: a Venus-side `memory_layout` in `helios_icd_interface_v2`, already
     planned in dxvk-on-nvk.md 3.2). For Venus surfaces that are not dma-buf-exportable (OPTIMAL
     opaque-fd images) the fallback is a GPU copy by the KMD's own Venus context into an RM-backed
     foreign resource per update, only for legacy producers.
4. **Present.** DWM's flips land in `DxgkDdiSetVidPnSourceAddress` / `DxgkDdiPresent` as now;
   for an allocation that adopted a foreign resid the KMD sends `ScanoutFlip` with the recorded
   layout (Option B, section 7 item 5). Pacing: v1 CPU-complete (DWM's UMD waits for NVK before
   the flip, as S3 apps do); S4 replaces it with the RM-fence boundary on the flip. Needs the
   host's flip completion/release (dxvk-on-nvk.md 3.6: `handle_scanout_flip` answers a bare
   header, `EV_RELEASE` not forwarded) so the KMD's READ LEDGER knows when a buffer is free.
5. **Failure fallback.** DWM restarts itself after a crash (and Windows falls back to basic
   display after repeated failures). The UMD keeps a per-boot crash counter for `dwm.exe` in
   `HKLM\SOFTWARE\Helios\Runtime!DwmNvkFailures`: incremented when DWM's device is created on
   NVK, cleared after N minutes of healthy composition (a timer in the UMD, or the KMD noting
   60 s of flips); when it reaches 2, the next DWM start chooses Venus (and logs why). Device
   removal (TDR on NVK work, `VK_ERROR_DEVICE_LOST`) counts as a failure too. Venus stays
   installed and initialised until this has been stable across a release.
6. **Order of work.** (1) NVK↔NVK sharing end to end (this document) with a D3D11 two-process
   test; (2) Venus→NVK import (3c) and the DMA_BUF change on the Venus side; (3) KMD RM client
   allocations with resids and the Option B flip; (4) DWM on NVK behind the knob, Venus fallback;
   (5) S4 fences on DWM's flips; (6) retire Venus for the desktop.

## 9. What the host must provide

- `RmResourceImport` deployed (`feat/s6-backend`).
- For DWM on NVK: `RmResourceImport` for dma-buf-exported Venus blobs (3b), a flip completion
  or release event for `ScanoutFlip`, and the layout carried per GEM object (exists for
  RM-export imports; also needed for the KMD's own RM allocations).
- Nothing new for NVK↔NVK sync in v1; S4's SEMSURF path for the GPU-side version.
