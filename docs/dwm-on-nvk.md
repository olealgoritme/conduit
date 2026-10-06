# DWM on NVK-on-RM (stage S6, "DWM on NVK")

Goal: `dwm.exe` runs its D3D11 device on DXVK over NVK-on-RM through the Helios UMD. Its flip-chain
buffers then live in RM memory, and the KMD flips them to the host with zero copies. Today DWM runs on
Venus (built-in deny-list, `umd_common/bridge/bridge_icd_backend.cpp`). This document lists what works,
what is missing with an owner for each gap, and the test sequence. Background:
`guest/windows/docs/shared-surfaces.md` (section 8 is the first design), `shared-foreign-surfaces.md`,
`zero-copy-present.md` (section 10), `kmd-rm-client.md` (15.7, 15.8), `foreign-scanout.md`.

## 1. What DWM does with its device (measured on 22.22.319.2, Venus, 5120x1440@240)

From `C:\ProgramData\Helios\umd-<dwm pid>.log`:

* **Swap chain.** Three `pPrimaryDesc` primaries plus three plain present buffers, all 5120x1440
  `B8G8R8A8` (fmt 87), bind 0xa8. DWM presents with `Present1`, one surface each time, flags 0x2
  (flip), `hDst` = 0. It also calls `RotateResourceIdentities` (legacy DXGI flip). It makes no MPO
  calls (`GetMultiplaneOverlayCaps` is never asked) and no `Blt` in steady state.
* **Opens.** About 35 `OpenResource` calls in the first minutes:
  * windows' surfaces: 1920x1080, 1024x1024, 704x704 and 800x704, fmt 87 and 65 (A8);
  * a 32x32 A8 surface;
  * surfaces of NVK apps (foreign resource ids; today a blank placeholder because `ForeignImport` is
    off);
  * KMD-made standard allocations (mem_type 0).
  Nearly all of them are Venus resources.

## 2. What already works

| piece | state |
|---|---|
| ICD choice per process, Venus fallback at device creation (`note_nvk_failed`, `helios_dxvk_create_device` retries on Venus in the same call) | shipped |
| NVK texture with `pPrimaryDesc` / `BIND_PRESENT`: dedicated NVK memory, `IMPORT_RM` on the per-process holder context, WDDM allocation adopting it (`DEVICE_MEMORY`, `blob_mem` 0x80000001, 128-byte private data with the layout trailer at 96, `Flags.Primary` + `VidPnSourceId`, meta `MISC_PRIMARY`) | shipped (`finish_wddm_tex2d_nvk`); never tried for DWM's primaries |
| NVK to NVK shared surfaces (KMD op 3 `RM_RESOURCE_IMPORT`, host msg 31, DXVK patch 0002, NVK 0031/0038) | works |
| WDDM present of an NVK frame with an RM fence marker (`HEPR` tail) or a CPU wait | works (apps) |
| NVK apps' buffers on scanout through the foreign-scanout arbiter | works (4400 fps) |
| Host `RmResourceImport` of a Venus blob whose renderer export is a dma-buf (`EINVAL` for `OPAQUE_FD`) | written (`feat/rm-export-map-blob`, `venus/rm.rs` `rm_resource`) |

## 3. Who owns DWM's swap-chain allocation (answered to the KMD session)

* **The WDDM allocation**: DWM's own D3D11 device creates it through `pfnAllocateCb` and destroys it at
  `DestroyResource` or device teardown. dxgkrnl may hold the last flipped primary past that.
* **The foreign record, holder context and DRM files**: their owner is librmclient's private D3DKMT
  device (`g_ctx` in `guest/rmclient/src/transport_windows.c`), one per process and never closed
  before the process exits. It survives D3D11 device recreation inside one `dwm.exe` and dies with the
  process.
* **The record's `(rm_handle, gem)`**: these belong to the NVK `VkDevice`'s DRM file and the
  `VkDeviceMemory`. NVK closes them when that device or memory goes, while the allocation may still be
  the one on screen. After adoption the blob slot is KMD-owned, and the host import keeps the memory.

## 4. Gaps, each with its fix and owner

### 4.1 Selection and failure safety (UMD; done on this branch)

* `HKLM\SOFTWARE\Helios!DwmIcd` (REG_SZ), read only in `dwm.exe`. `nvk` puts DWM on NVK past `Icd` and
  the deny-lists; `venus` or absent (the default) changes nothing.
* Crash-loop guard: `%ProgramData%\Helios\dwm-nvk-starts.txt` holds the times of recent DWM starts on
  NVK. When `DwmNvkMaxStarts` (default 2; 0 = off) starts fall inside `DwmNvkGuardSeconds` (default
  600), the next start goes to Venus and the log says why. A DWM that stays up never starts again, so a
  healthy session never trips it, and the window ages out on its own.
* Device creation failure on NVK: Venus for this process (existing).
* **Open: a mixed process.** If a later device of the same DWM fails on NVK, the process is latched to
  Venus while the earlier devices stay on NVK. Decide whether DWM should fail that creation instead, so
  that DWM restarts and the guard counts it.

### 4.2 Composition inputs (the hard part)

| source | today on an NVK DWM | fix | owner |
|---|---|---|---|
| NVK app surfaces (foreign ids) | imported by resource id | none | done |
| Venus app surfaces (deny-listed browsers, video, shell apps) | NVK open refused. On this branch DWM gets a **blank placeholder** instead of an `E_FAIL` that would kill it, so the window is black | (a) Venus UMD/DXVK: ordinary shared images use `DMA_BUF` export with DRM-modifier tiling (today `OPAQUE_FD`, `0001-helios-nvk-backend-and-foreign-import.patch`), and the modifier, stride and offset go into the WDDM meta / layout trailer. (b) KMD: op 3 accepts a non-foreign resource the caller's process opened (today `NoSuchResource`); the host decides dma-buf vs `EINVAL`. (c) DXVK patch 0002 and the bridge's `nvk_can_open`: import a Venus dma-buf with the meta's layout | UMD + DXVK, KMD, host (deploy the `feat/rm-export-map-blob` `rm_resource`) |
| KMD-made surfaces (GDI redirection / standard allocations, `KmdOptimalGdiTexture`, shared primary) | Venus blobs; placeholder (black) | KMD: either dma-buf with an explicit modifier (then 4.2 (b) covers them), or RM memory with a foreign id per allocation (`shared-surfaces.md` 7 item 4, level 5 style) | KMD |
| Non-32 bpp surfaces (NV12/P010 video, fp16 HDR, 10:10:10:2) | NVK refuses a resource id (`memory_res_id`: 32 bpp only) | multi-plane and other bpp in `IMPORT_RM` / the layout record / NVK 0025 | NVK + KMD |

**Can the deny-list shrink instead?** Only partly. An all-NVK desktop still needs the following on
Venus or new work: GDI and KMD surfaces (KMD row above); DXR titles (`NvkDenyList12`; NVK reports no
DXR); and video and HDR (non-32 bpp shared surfaces). Browsers also need cross-process keyed mutex
(the hand-off ledger in progress) and NV12 sharing. So the Venus to NVK import route (4.2 (a) to (c))
is needed for the transition. Shrinking the deny-list is the end state, not the first step.

### 4.3 Flipping DWM's buffers (KMD; the `ForeignFlip` arm in progress)

KMD needs, precisely:

1. **K1 `ForeignFlip`**. In `program_vidpn_source_inner`, an allocation that adopted a foreign id
   (kind `DEVICE_MEMORY`, `blob_mem` RM-export, primary flag set) is flipped through the arbiter with
   the record's layout, not with `SET_SCANOUT_BLOB`. Before every flip, re-check that the record's DRM
   file is still its owner's and the NVRM epoch is unchanged. If they are stale, fall back with a
   counter. Better still, key the flip on the host import (resource id), which holds the memory for
   the record's whole life.
2. **K2 ordering**. A DWM present carries an RM fence marker (`HEPR`/`HERF`, cap bit 33) when the
   KMD offers it, or the UMD has already waited on the CPU. The flip must go out only after that
   fence (`rm-fence-marker.md`), never before.
3. **K3 completion to dxgkrnl**. Flip-done / vsync for the new address (`DxgkCbNotifyInterrupt`
   `CRTC_VSYNC`) must be reported for foreign flips exactly as for Venus flips. Otherwise DWM's
   present queue stalls. When the host tracks releases (`ScanoutReleased`), report the old buffer free
   only once the host released it, or accept tearing and count it.
4. **K4 teardown**. When `dwm.exe` dies, its DRM files close while its last primary is on screen.
   The arbiter must give scanout back (the KMD's own primary or the desktop flush), and the next DWM's
   primaries take over. This must not wedge on a stale source.
5. **K5 (from 4.2)**: op 3 for opened Venus resources; KMD-made surfaces as dma-buf or RM.

Until K1 ships, DWM's flips of NVK primaries go through Venus `SET_SCANOUT_BLOB` on an RM-export
resource. Whether the host shows that is part of experiment T2.

### 4.4 DWM's other device use

* **GDI-redirected windows and the cursor**: KMD-made surfaces (4.2). The hardware cursor goes
  through `DxgkDdiSetPointerShape` and is not DWM's device; a software cursor is composed from a
  surface DWM opens.
* **MPO**: DWM never asks for MPO caps here. `dxgi_present_mpo` has no NVK frame gate (no fence
  marker or CPU wait). Add one before MPO is offered on NVK (UMD).
* **Independent flip** of a fullscreen NVK app: the app's own buffers through the arbiter (works
  today for NVK apps). Under an NVK DWM the arbiter must arbitrate between DWM's foreign flips (K1) and
  the app's source: the app preempts, and the DWM flip resumes (KMD, 13.2 rules).
* **Present path in the UMD**: on NVK, DWM never uses NVK's own scanout source (`nvk_present_frame`
  forces the WDDM flip for `dwm.exe`; done on this branch). Otherwise most frames would bypass
  dxgkrnl.

## 5. Test sequence

Run each step only in a quiet window agreed with the install agent. Always end with `DwmIcd` removed
(or `venus`), the original `Icd` / `NvkAllowList`, a DWM restart, and DWM up with the same pid for
30 s.

* **T0 baseline.** Venus DWM: its pid, KMD counters (`HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`),
  the tail of the backend log.
* **T1 (319.2, registry only).** `Icd` removed and `NvkAllowList=dwm.exe`, then `taskkill dwm`. Record
  the UMD log of the new DWM, Application event 1000, the counters and the backend log. Expected stop:
  the first `OpenResource` of a Venus surface fails, and dwmcore exits.
* **T2 (this branch's UMD, `DwmIcd=nvk`).** DWM comes up with placeholders. Do its primaries get
  foreign ids (`nvk resource id: vr=0`), and do Venus `SET_SCANOUT_BLOB` flips of them show anything?
* **T3 (+ KMD `ForeignFlip=1`).** Flips via the arbiter: the desktop is visible, with frame rate and
  host flip counters, and the RM fence ordering is checked.
* **T4.** A windowed NVK D3D11 app is composed by the NVK DWM (NVK to NVK import).
* **T5 (after 4.2).** A Venus browser window and a GDI window (notepad) composed by the NVK DWM.
* **T6 failure.**
  * `NvkIcdPath` pointing at a missing file for one run: DWM on Venus, "no NVK ICD".
  * Three DWM restarts within 10 min: the third start is Venus by the guard.
  * A TDR while on NVK: DWM recreates its device.
* **T7 soak.** 30 min of desktop use: a mode change, the lock screen (LogonUI is deny-listed: a
  Venus surface under an NVK DWM), sleep of the viewer.
