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
* Crash-loop guard (decision in `umd_common/bridge/bridge_dwm_guard.h`, host test
  `bridge_dwm_guard_test.cpp`, run by CI). It counts only DWM starts on NVK that **never got healthy**:
  * `%ProgramData%\Helios\dwm-nvk-starts.txt` holds the pending starts, one per line: the time of
    the choice, the DWM's pid and creation time, and the UMD build (a hash of the UMD's driver-store
    folder, which is new for every driver package).
  * A DWM on NVK that is still presenting `DwmNvkHealthySeconds` (default 30) after its choice deletes
    the file and logs "DWM on NVK is healthy". Ordinary restarts, the restarts a live driver update
    causes, and a reboot after either therefore never add up to a crash loop.
  * A start counts only if the same UMD build made it, in another DWM process (a DWM that loads a new
    copy of the UMD during a live update is not a restart), inside `DwmNvkGuardSeconds` (default
    600). Starts of another build are dropped, so installing a new driver starts a fresh history.
  * When `DwmNvkMaxStarts` (default 2; 0 = off) pending starts count, the next start goes to Venus, is
    not recorded, and the log says why. The window ages out on its own.
  * Pending starts survive a reboot: a DWM start that takes the machine down (a bugcheck) still
    counts on the next boot, where pids often repeat (the creation time tells the processes apart).
  * **Venus must work for the fallback.** If Venus cannot create the device of a DWM the guard sent to
    Venus, that DWM moves to NVK after all (`dwm_guard_venus_failed`, logged loudly) instead of failing
    at every start.
* Device creation failure on NVK: Venus for this process (existing).
* **Open: a mixed process.** If a later device of the same DWM fails on NVK, the process is latched to
  Venus while the earlier devices stay on NVK. Decide whether DWM should fail that creation instead, so
  that DWM restarts and the guard counts it.

### 4.2 Composition inputs: the main route is "nearly everything on NVK"

DWM composes every window, so an NVK DWM must open every surface. The main route is to shrink the
deny-list until nearly every process runs on NVK. Video decode, D3D12 and OpenGL (Zink) all run on NVK
now, so DWM's inputs become NVK surfaces, which it imports by resource id (works). Venus-to-NVK
cross-import (4.2.3) remains the fallback for what stays on Venus.

**Order:**

1. With DWM still on Venus, set `ForeignImport=1` (the Venus DWM composes NVK surfaces; today the
   default 0 gives a blank placeholder).
2. Move entries off the deny-list one category at a time with `NvkAllowList`.
3. Then turn DWM itself to NVK (`DwmIcd=nvk`).

#### 4.2.1 The built-in deny-list today, entry by entry

Source: `kBuiltinDeny`, `umd_common/bridge/bridge_icd_backend.cpp`. Its stated reason: "DWM and the
shell compose everything else (S6 moves them); the rest open or produce surfaces shared with Venus
processes (video, browsers, overlays, capture), which an NVK process cannot import". Every entry is
there because of **cross-process sharing between an NVK and a Venus process**, not because NVK cannot
render for it. Once both ends of every share are NVK, that reason is gone. What is left are the limits
of NVK-to-NVK sharing, listed as "blocker" in the table and in 4.2.2.

| entries | why listed | can move now? | blocker | test |
|---|---|---|---|---|
| `dwm.exe` | the compositor | with this branch (`DwmIcd=nvk`) | 4.3 (`ForeignFlip`), 4.2.2 | T2/T3 |
| `csrss.exe`, `winlogon.exe`, `fontdrvhost.exe`, `rdpclip.exe` | system processes; they rarely or never create a D3D11 device | yes | none known (GDI does not go through the UMD) | allow; boot to desktop; no `umd-<pid>.log` for them, or a clean one |
| `explorer.exe`, `sihost.exe`, `shellexperiencehost.exe`, `shellhost.exe`, `startmenuexperiencehost.exe`, `searchhost.exe`, `searchapp.exe`, `textinputhost.exe`, `widgets.exe`, `widgetservice.exe`, `phoneexperiencehost.exe`, `crossdeviceresume.exe`, `applicationframehost.exe`, `systemsettings.exe`, `runtimebroker.exe`, `dllhost.exe` | the shell: XAML / DirectComposition surfaces DWM opens; `applicationframehost` and `dllhost` (thumbnails) share surfaces with other processes | after A8 resource ids | **8 bpp surfaces**: T1 shows DWM opening `A8_UNORM` (fmt 65) shell surfaces (800x704, 704x704, 32x32). NVK mints ids for 32 bpp only, and the KMD's layout record takes only the four 32 bpp fourccs | allow one exe, restart it, open the start menu / search / taskbar; DWM log: `foreign` opens, no placeholder |
| `logonui.exe`, `consent.exe`, `lockapp.exe` | secure-desktop and lock-screen UI composed by DWM | after the shell | same as the shell; a failure here blocks logon or UAC, so move these last of the shell | lock (Win+L), a UAC prompt, unlock |
| `taskmgr.exe`, `mmc.exe` | interop-heavy (DirectComposition, hosted surfaces) | yes, to try | probably none; DComp swap chains are 32 bpp | open both; their graphs and windows compose |
| `msedge.exe`, `msedgewebview2.exe`, `chrome.exe`, `firefox.exe`, `brave.exe`, `opera.exe` | multi-process GPU sharing (renderer to GPU process to DComp), keyed mutex, NV12 video | not yet | (1) cross-process keyed mutex ordering: the S6 hand-off ledger, in progress; (2) **NV12/P010 shared textures** for video (decode, VideoProcessor, DComp video overlays): no ids for non-32 bpp or multi-plane; (3) fp16 for HDR video | allow one browser; scroll a page (32 bpp sharing), then play H.264 video; GPU process and DWM logs |
| `teams.exe`, `ms-teams.exe`, `discord.exe`, `slack.exe`, `spotify.exe`, `code.exe`, `steamwebhelper.exe`, `epicwebhelper.exe`, `cefsharp.browsersubprocess.exe` | Chromium/Electron/CEF: same model as browsers | with the browsers | same as browsers | same as browsers |
| `vlc.exe`, `mpc-hc64.exe`, `mpc-be64.exe`, `video.ui.exe`, `microsoft.photos.exe`, `photos.exe` | D3D11VA decode, NV12/YUV swap chains, MF / DComp video | partly: a player that presents RGB through its own swap chain can move now (NVK has H.264 decode; other codecs fall back to software) | NV12/YUV swap chains or shared NV12 surfaces, as for browsers | play H.264 and HEVC in each; frame drops, DWM opens |
| `obs64.exe`, `obs32.exe` | capture: Desktop Duplication (opens DWM's output) and game-capture shared textures | after DWM on NVK | duplication of an NVK DWM's primary (a foreign resource; 32 bpp, should import); game capture is then NVK to NVK | display capture, window capture, game capture of an NVK D3D11 app |
| `nvidia share.exe` | GeForce overlay; no NVIDIA driver in the guest | remove the entry | none | n/a |

Expected leftovers on Venus:

* **DXR / ray-tracing titles** (`NvkDenyList12`, D3D12 only; NVK reports no DXR).
* Until 8 bpp, multi-plane and fp16 resource ids exist: browsers, Electron apps and video apps for
  video and HDR content.
* The secure desktop until the shell has soaked.

#### 4.2.0 The desktop follows DWM (UMD, `feat/s6-fast-handoff`)

An NVK DWM shows a blank placeholder for every surface a Venus process made: with `DwmIcd=nvk`
and `Icd=venus` the Start menu, search and notification centre came up gray (DWM log: "not
importable: blank placeholder" for 1312x384, 704x704, 800x704, 832x896, 1024x1024, 32x32). So
while DWM runs on NVK, every D3D11 process goes to NVK as well:

* The DWM that chose NVK through `DwmIcd=nvk` (crash-loop guard passed) creates the named event
  `Local\HeliosDwmOnNvk` (DACL: everyone and restricted/AppContainer tokens may wait; low label)
  and closes it if NVK fails for it; it dies with that DWM.
* Any other D3D11 process whose registry says `DwmIcd=nvk` and that finds the marker (an
  `ERROR_ACCESS_DENIED` open counts as found) takes NVK. `Icd=venus` and the built-in deny-list do
  not apply then (their reason, NVK and Venus processes cannot share surfaces, is what they would
  cause); an explicit `NvkDenyList` still keeps a process on Venus.
  `DesktopFollowsDwm=0` (REG_DWORD) turns it off. D3D12 is unchanged (already NVK by default).
* NVK apps' presents go to DWM (composed by resource id) rather than straight to scanout 0 while
  the marker is there, as with `ForeignImport=1`; otherwise an NVK app took the whole screen and
  DWM's own flips showed through about once a second. This holds for D3D11 and D3D12 swap chains.
  A D3D12 back buffer that covers the output (a primary, or at least the desktop's size) still
  takes scanout 0 (`Nvk12Present` 0, `umd_common/src/nvk_present.rs`). A composed D3D12 frame's
  present marker carries an RM fence for the frame's last ExecuteCommandLists and the
  copy-engine record of the presented buffer, as a D3D11 blt-model frame's does. A window DWM
  does not flip (DXGI copies the frame with the D3D12 `pfnBlt` into a GDI-compatible buffer of
  the window's size and presents that) reaches the window through the KMD's Blt of that buffer.

The choice is made once per process: processes that started before DWM moved (the shell after a
`restart-device`) keep Venus until they restart.

#### 4.2.0a Browsers on NVK: the stall was NAK in the sandbox, not the hand-off (2026-10-06)

* Cross-process hand-off cost on NVK (330.2, `tools/handoff_bench`, 1280x720 BGRA ping-pong, 300
  hand-offs, no stale reads): median round trip 0.42 ms with an NT-handle keyed mutex, 0.44 ms with a
  KMT keyed mutex, 0.47 ms with a shared `ID3D11Fence`.
* Edge's GPU process (low integrity, restricted token) lost its NVK device at its first pipeline:
  every NAK compile created a NIR instruction printer whose memstream is a `%TEMP%` file on
  Windows, which the sandbox may not create; NAK panicked (`from_nir.rs`, Access is denied),
  pipelines failed with `VK_ERROR_UNKNOWN`, DXVK lost the device, and Edge fell back to software
  after three GPU process restarts. The same panic killed AppContainer shell hosts
  (ShellExperienceHost c000027b in XAML) and blanked explorer's XAML islands. Fixed by NVK patch
  0048 (the printer only for `NAK_DEBUG=annotate`). `tools/sandbox_run` reproduces the sandbox.
* NT-handle sharing: B5G6R5, B5G5R5A1 and B4G4R4A4 are refused by the Microsoft runtime for WARP
  as well (policy, not a driver gap). R16G16 / R16G16_FLOAT are refused only on Helios (both
  backends, WDDM 1.3 and 2.3 alike): open.
* A process whose KMD restarted under it now gets NVK back at its next device creation
  (librmclient generations, a1fe219), so long-lived shell processes do not stay on Venus.

#### 4.2.1a Measured moves (22.22.326.1, 2026-10-06, Venus DWM with ForeignImport=1)

Each category on NVK by `NvkAllowList` only (`Icd=venus` kept), its processes restarted in the
user session, checked for: NVK device, resource ids, DWM's opens of their surfaces, a screen
capture of the composed window, crashes. Built into the UMD as `kBuiltinNvkDefault`, applied with
`HKLM\SOFTWARE\Helios!NvkDefaults=1` (default 0; `NvkDenyList` still wins).

| category | verdict | evidence / blocker |
|---|---|---|
| csrss, winlogon, fontdrvhost, rdpclip | move | never create a D3D device (no UMD log in 1038 logs) |
| taskmgr | move | NVK device, 3 resource ids, window composed |
| mmc | move | no D3D device |
| startmenuexperiencehost | move | NVK, 5 resource ids, A8 surfaces opened by DWM, Start menu drawn |
| systemsettings + applicationframehost | move | NVK, 3 and 12 resource ids, Settings drawn |
| explorer | blocked | taskbar, wallpaper, File Explorer drawn on NVK, but a device-wide DEVICE_REMOVED (every process, Venus too; DWM restarted) followed within a minute, after an NVK frame-wait timeout in explorer; cause open (KMD/host) |
| shellexperiencehost, textinputhost | blocked | ShellExperienceHost crashed at its NVK start (Windows.UI.Xaml c000027b, no UMD log); textinputhost caught in the removal |
| searchhost | n/a | crash-loops on Venus as well since 14:36 (KERNELBASE 0xe06d7363) |
| msedge, msedgewebview2 (Chromium family) | blocked | pages render, video stalls on NVK: YouTube VP9 frozen at 0.7 s, local H.264 6 frames in 10 s (Venus: 600). GPU process writes no UMD log (sandbox) |
| VLC (D3D11 output) | software decode: move; D3D11VA: blocked | D3D11VA on NVK shows green: decode into an NV12 texture array sampled through R8/R8G8 plane SRVs |
| MPC-HC (MPC Video Renderer, LAV) | not measured | NVK device + video DDI, playback did not start from the command line |
| nvidia share.exe | removed from the list | no NVIDIA driver in the guest |
| logonui, consent, lockapp | after the shell | |

#### 4.2.2 What NVK-to-NVK sharing still lacks (blocks the list above)

| gap | fix | owner |
|---|---|---|
| 8 bpp (`A8`, `R8`) resource ids: the shell's composition surfaces | `memory_res_id` (NVK 0025) beyond 32 bpp; KMD layout record fourccs (`DRM_FORMAT_R8` and others) and a per-format bpp in the stride/extent check; host `RmResourceImport` takes any RM-export object (no change expected) | NVK, KMD |
| NV12 / P010 (two planes), fp16 (8 bpp per texel), 10:10:10:2 | layout record with plane 1 (offset, stride); NVK 0025/0031 for multi-plane images; DXVK video textures marked shareable | NVK, KMD, DXVK |
| cross-process keyed mutex and producer ordering | S6 hand-off ledger (in progress) | UMD, KMD |
| KMD-made surfaces DWM opens (GDI redirection / standard allocations, `KmdOptimalGdiTexture`, shared primary; `mem_type` 0 in T1) | RM memory with a foreign id per allocation (`shared-surfaces.md` 7 item 4, level 5), or dma-buf with a modifier (4.2.3) | KMD |

#### 4.2.3 Fallback for the leftovers: Venus-to-NVK cross-import

| step | what | owner | state |
|---|---|---|---|
| a | Venus DXVK: ordinary shared images get `DMA_BUF` export with DRM-modifier tiling, not `OPAQUE_FD` (`0001-helios-nvk-backend-and-foreign-import.patch`). The modifier, stride and offset go into the WDDM meta / layout trailer | UMD + DXVK | not started |
| b | KMD op 3 (`RM_RESOURCE_IMPORT`) accepts a non-foreign resource that the caller's process opened. Today it is refused with `NoSuchResource` (`kmd_logic::rm_resource_import::authorize` requires a foreign record) | KMD | not started |
| c | Host msg 31 for a Venus blob whose renderer export is a dma-buf | host | **deployed**: backend a51a8c6 (host-all 489b71f, `venus/rm.rs` `rm_resource`) answers a dma-buf Venus blob with an unknown modifier, `EINVAL` for `OPAQUE_FD`, `ENOENT` for an unknown id |
| d | DXVK patch 0002 and the bridge's `nvk_can_open`: import a Venus dma-buf with the layout from the open's meta | UMD + DXVK | not started |

Until then, an NVK DWM gets a **blank placeholder** for any surface it cannot import (this branch),
not the `E_FAIL` that killed DWM.

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

### 4.5 Windowed apps without a copy (`FlipUpgrade`, `FlipLedger`)

A windowed blt-model swap chain (Heaven: `DISCARD`, one buffer, interval 0; PresentMon
"Composed: Copy with GPU GDI") reaches DWM only through the window's redirection surface: dxgkrnl
turns each present into a Blt into that surface, and DWM never opens the app's buffer. Here the
surface is a CPU-visible standard buffer (`vram-redirection.md` 0), so the Blt is a copy the app
waits for. Only a flip-model chain is shared with DWM ("In the flip model, all back buffers are
shared with the Desktop Window Manager (DWM). Therefore, the DWM can compose straight from those
back buffers without any additional copy operations", win32 `direct3ddxgi/dxgi-flip-model.md`).

**Why the driver upgrades the chain itself.** Windows' own upgrade ("optimizations for windowed
games", `SwapEffectUpgradeEnable=1;`) is applied only to processes DXGI classifies as games: its
`DXGI_ETW_SWAPCHAIN_CREATE` event for Heaven carries `WINDOWEDSWAPEFFECTUPGRADE_REASON_NONGAME`
(`guest/windows/docs/independent-flip.md` 11.5). Microsoft's descriptions of the feature (the
DirectX blog "Updates in Graphics and Gaming", the support article "Optimizations for windowed
games in Windows 11") name DX10/DX11 windowed games and no driver capability, and the driver
already reports what windowed flip promotion needs (`SupportDirectFlip`, `FlipIndependent |
DdiPresentForIFlip`: `kmd_render/src/ddi/query_adapter_info.rs`, `kmd_logic/src/independent_flip.rs`;
the UMD's `CheckDirectFlipSupport`). There is no cap left that would make DXGI upgrade a non-game.

**`FlipUpgrade`** (`HKLM\SOFTWARE\Helios`, REG_DWORD, default 1; `helios_umd_common::flip_upgrade`).
In an NVK process under a DWM that imports NVK surfaces (not `dwm.exe`; optionally only the programs
`FlipUpgradeList` names), the D3D11 UMD hooks DXGI's swap-chain creation in the process
(`IDXGIFactory::CreateSwapChain`, `IDXGIFactory2::CreateSwapChainForHwnd`). A windowed `DISCARD`
chain with one sample, an R8G8B8A8 or B8G8R8A8 UNORM buffer and no flag but `ALLOW_MODE_SWITCH` is
created `FLIP_DISCARD` with `FlipUpgradeBuffers` buffers (default 3). The flip model's documented
extra requirements (`direct3ddxgi/for-best-performance--use-dxgi-flip-model.md`, "What do I have
to do to use the flip model?") are met for the app:

| requirement | how |
|---|---|
| at least 2 buffers | `FlipUpgradeBuffers`; `ResizeBuffers` keeps the count |
| the back buffer is re-bound after `Present` | the hooked `Present` / `Present1` binds the immediate context's render targets and depth view again after the real call, as a blt-model present leaves them |
| `ResizeBuffers` after `SetFullscreenState` | only windowed chains are upgraded; exclusive fullscreen keeps its scanout path |
| no MSAA buffers | multisampled chains are not upgraded |

`GetDesc` / `GetDesc1` report what the app asked for. Interval-0 presents stay unthrottled when
DWM promotes the window to independent flip: the chain is created with `ALLOW_TEARING` when DXGI
allows it and such presents add `DXGI_PRESENT_ALLOW_TEARING` (`FlipUpgradeTearing`, default 1;
the conditions are `direct3ddxgi/dxgi-present.md`). `GDI_COMPATIBLE`, sRGB, 10-bit and fp16 chains
are left alone (GDI content in a flip window is not shown, `dxgi-flip-model.md`; NVK mints
resource ids for the 8-bit formats). The UMD is pinned in memory once the hooks are in.

Once the chain is flip-model, DWM opens its buffers by resource id at creation (as for any NVK
flip-model chain) and composes the presented buffer.

**`FlipLedger`** (REG_DWORD, default 1; DWM reads it when it starts; 0 turns it off). The problem
it solves: DWM on NVK samples an app's buffer from its own RM channel, which neither dxgkrnl's
waits nor the RM fence that retires the app's present hold, so without help it can compose a frame
the GPU is still drawing (`NvkComposedComplete`, the rule with `FlipLedger=0`, completes the frame
on the presenting thread instead, which serialises the app's CPU and GPU work). With 1:

1. **The app never waits.** A flip-model present's WDDM marker carries the frame's RM fence and
   names the presented buffer: the D3D11 UMD puts its resource id into the `HERF` `source_index`
   (`forward/present.rs`, `frame_resid`), a D3D12 `HEPR` has it already. Vulkan windows present
   through a D3D11 flip chain (NVK's WSI) and take the D3D11 path. A swap chain of fewer than
   three buffers, or a KMD without the table below, keeps the old rule.
2. **The KMD knows which frames are finished.** When it attaches the fence to the RM gate of the
   presenting WDDM context (at Render, before dxgkrnl can hand the frame to DWM) it records the
   frame as pending; when the fence fires (its `EventReady` DPC; also an expiry or a cancelled
   gate) that frame and every earlier pending frame of the same context are finished (a context's
   frames finish in order). Per buffer the table keeps the newest finished frame in it
   (`completed`, the content it holds, even while its next frame is queued) and whether the GPU
   may be writing it now (`writing`: its pending frame is the oldest of its context)
   (`kmd_logic/src/frame_done.rs`, `virtio/gpu/rm_gates.rs`). The table is published in the
   read-ledger page every UMD device maps already (`helios_protocol::HeliosFrameDoneTable` at
   `HELIOS_FRAME_DONE_OFFSET`, version 2; `adapter/read_ledger.rs`).
3. **DWM composes the newest finished frame and never waits for an app frame.** At every
   shader-resource bind DWM's UMD reads the table for each app present buffer it binds
   (`forward/bindings.rs` `dwm_flip_views`, `scanout_acquire.rs` `frame_done_lookup`). A buffer
   with nothing pending, or that the table does not track, stays. Otherwise DWM binds the buffer
   of the same swap chain (the same presenting process, size and format; three buffers or more)
   that holds the newest finished frame and is not being written, or, when none is, the buffer it
   composed for that chain last time: the window keeps its last frame. The app publishes nothing
   DWM's DXVK could wait for, so DWM never waits.
4. **Buffer release.** dxgkrnl's wait for DWM to release a buffer holds the app's WDDM context,
   which NVK's submissions bypass. DWM therefore publishes, at each of its presents, a hand-off
   ledger point on the app buffers it composed in that frame (`forward/present.rs`
   `dwm_flip_read_points`), and the D3D11 app's DXVK, at its next access of such a buffer, waits
   for DWM's read in its submission worker (DXVK patches 0007/0008; nothing waits on the app's
   thread, and only while DWM is still reading that very buffer). The key DXVK samples follows
   `RotateResourceIdentities` (`dxvk_bridge.cpp` `handoff_key`, `forward/present.rs`
   `rotate_ring`). A D3D12 app's buffer waits come through the runtime's admission of every ECL
   (`forward12/queue.rs`).

Counters and log lines (UMD logs, `C:\ProgramData\Helios\umd-<pid>.log`; KMD values in the
service key):

| where | line / value | reading |
|---|---|---|
| app (D3D11) | `FlipUpgrade: swap chain #1 upgraded: swap effect 0 -> 4 (FLIP_DISCARD) ...` | the chain is flip-model |
| app (D3D11) | `FlipLedger: N flip-model presents without a CPU wait; the KMD's frame-done table tracks the buffer ...` | the app does not wait |
| KMD | `FdSub` (frames recorded), `FdFire` (fences that finished at least one frame; a fence also finishes the earlier frames of its context, so `FdFire` may stay below `FdSub`), `FdPend` (frames pending after the last change), `FdOvf` (frames not tracked) | `FdPend` small (the frames the app is ahead of the GPU, about 1-3), `FdOvf` 0 |
| KMD | `FdD11` / `FdD12` (presents that named their buffer, per carrier) | a fullscreen app on scanout 0 sends no fence with its present: it adds nothing; with nothing pending a fence fire costs the table one compare |
| DWM | `DWM on NVK: X presents/s, Y new app flip frames shown/s ...; binds of app flip buffers: newest frame done A, newest done frame chosen B, last frame kept C, bound in flight (nothing to choose) D, not tracked E (in a tracked chain F); R read points ...` (once a second) | X is DWM's composition rate; Y is the app frames DWM put on screen per second: about min(app fps, X) for one window; C grows only while the app is slower than X; D stays near 0 after a chain's first frame; E counts other windows whose apps complete their frames before the present, F should stay near 0 |
| DWM | `handoff waits: N sleeps ...` | must not appear |

**Not covered yet.**

* DWM's read point goes out at its present, after its draws; an app write recorded between DWM's
  bind and that present is not ordered behind the read. With three buffers the app writes the
  replaced buffer two presents after showing it, so this needs a DWM frame that takes longer than
  two app frames between bind and present.
* A D3D12 app does not sample DWM's read points (vkd3d has no ledger hooks); its release rides
  the runtime's admission, which knows about the buffer DWM latched, not about an older one DWM
  composed instead (the same two-present margin).
* A D3D12 swap chain of two buffers is not replaced in (the frame before is the buffer the app
  renders into next) and a D3D12 frame does not wait before its present, so DWM can sample such a
  frame while it is drawn, as before `FlipLedger`.
* A blt-model window DWM never flips (a child window, or `FlipUpgrade` 0) is still copied into the
  redirection surface. The KMD's legacy Blt copies whole images only: a foreign source whose extent
  differs from the destination (`PBCpy` 0xE3, `PrFgWhy` 6) is skipped and the window keeps its old
  content. A child window presenting into its parent's redirection surface would look like that
  (GPU-Z's gray render-test window is the case to check with those two counters).

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
  * Three DWM restarts within 10 min, each before it reached `DwmNvkHealthySeconds`: the third start
    is Venus by the guard. Restarts of a DWM that ran on NVK longer than that never trip it.
  * A live driver update, then a reboot within 10 min: DWM stays on NVK.
  * The guard tripped while Venus cannot create DWM's device: DWM runs on NVK after all ("NVK after
    all" in its UMD log).
  * A TDR while on NVK: DWM recreates its device.
* **T7 soak.** 30 min of desktop use: a mode change, the lock screen (LogonUI is deny-listed: a
  Venus surface under an NVK DWM), sleep of the viewer.

## 6. Results

### T1 (2026-10-06 09:42, 22.22.319.2; `Icd` removed and `NvkAllowList=dwm.exe` for 30 s)

* **First NVK DWM (pid 9068) died within 2 s** with `STATUS_STACK_OVERFLOW` (0xc00000fd), Application
  event 1000. The dump is `W:\dumps\dwm.exe.9068.dmp`.
  * Where: thread "DWM LPC Port Thread", whose stack is **128 KiB** (TIB 0x...d4fd0000 to
    0x...d4fb0000; dwm.exe's PE default is 512 KiB). DWM creates its D3D11 device on that thread.
  * Stack: `d3d11!D3D11CoreCreateDevice` > `helios_umd!OpenAdapter10_2...` > the UMD's device creation
    (a 43 KiB frame) > `vulkan_nouveau` device bring-up (frames of 28 and 17 KiB) >
    `RtlAllocateHeap` > overflow.
  * Venus fits in 128 KiB; NVK does not. **Fixed in the UMD**: an NVK device creation on a thread with
    less than 8 MiB of stack runs on a helper thread with 8 MiB (`run_with_stack`,
    `dxvk_bridge.cpp`).
* **Second NVK DWM (pid 2788) came up** (it created its first device on a different thread) and lived
  about 20 s, until the revert:
  * three DXVK devices on NVK (icd caps 0xf7);
  * six 5120x1440 swap-chain buffers with foreign ids (res 538 to 543, holder ctx 63, modifier
    0x0300000000606014, stride 20480, 128-byte private data, three of them `Flags.Primary`): **the
    adoption path works for DWM's primaries**;
  * every `OpenResource` of a Venus window surface was refused (`E_FAIL`, res 77 800x704 A8, res 41
    1024x1024, res 43 704x704). DWM survived these but composed nothing;
  * **its first frame went to NVK's own scanout source** ("NVK present: 1 frames on scanout 0 on RM
    fences"). Present #3 and #4 then entered `nvk_present_frame` and never came back: DWM's render
    thread hung for the rest of the run. **Fixed in the UMD**: DWM always presents through the WDDM
    flip.
* KMD counters across the run:
  * `FgImp`/`FgAdo` +17 (the adoptions), `FgOpen` +17;
  * `CpImpSt` 16, `CpReq` 31457280 (one 5120x1440 buffer), `CpBit` 3 and `FcOff` +4: the KMD tried
    the foreign copy (Blt model) for a 5120x1440 NVK buffer while that copy is off;
  * `PBRet` 0xC000000D (`STATUS_INVALID_PARAMETER`);
  * `ScFnc` +143009 and `ScVs` +118824 in 30 s (to be read with the KMD session);
  * `VsR4` 536.
* Revert: `Icd=venus` restored, `NvkAllowList` removed, DWM killed once. The Venus DWM (pid 10916)
  kept the same pid for 30 s. No TDR, no reboot.

### T2 (2026-10-06 10:23, 22.22.319.3 with 4cc35bb; `DwmIcd=nvk`, `Icd=venus` left as is, 45 s)

* No crash, no hang. NVK device creation ran on the 8 MiB helper thread ("caller's stack 128 KiB"),
  so the stack overflow is gone.
* NVK then failed with "DxvkError: Failed to initialize DXVK". NVK's own policy (Mesa 0032) hides
  its GPU from a process that `Icd=venus` sends to Venus. The D3D12 bridge wraps its creation in
  `NvkPolicyScope`; the D3D11 bridge did not. Fixed in 1d513d1.
* **Failure safety confirmed live.** `note_nvk_failed` moved the process to Venus, and that DWM (pid
  2036) composed normally until the revert. The guard file recorded one NVK start.
* KMD: `PBRet` 0xC000000D and `PBCpy` 2 -> 225 although DWM never presented on NVK, so this
  `PBRet` comes with a Venus DWM restart (T1 had restarts too).
* Revert: `DwmIcd` removed, DWM killed once; Venus DWM pid 5128 stable for 30 s.

### T2c (2026-10-06 10:38, 22.22.319.4 with 1d513d1; `DwmIcd=nvk`, `ForeignFlip=0`, 50 s)

* **DWM ran on NVK**: DXVK devices on NVK, created on the helper thread (DWM's calling threads had
  128 KiB and 512 KiB of stack), with `NvkPolicyScope`.
* Six 5120x1440 swap-chain buffers and one 1024x1024 surface got foreign ids (res 143 to 149).
* Six Venus window surfaces became blank placeholders. No crash, no TDR.
* **Stops after two flips.** DWM made two `Present1` calls through the WDDM flip
  (`pfnPresentCb` hr 0, flags 0x1, then 0x2 onto an NVK primary). Its log is silent from then until
  the revert, about 45 s; a Venus DWM logs ~150 presents in that time. So DWM waits for the first
  flip of a foreign primary, which the Venus `SET_SCANOUT_BLOB` path never completes. This is K1/K3
  (`ForeignFlip`, KMD v320): next is T3 with `ForeignFlip=1`.
* Revert: `DwmIcd` removed; Venus DWM pid 7092 stable for 30 s.

### T3 (2026-10-06 10:52, 22.22.320.1 with 1d513d1; `ForeignFlip=1`, `DwmIcd=nvk`, 45 s)

* **ForeignFlip put the NVK DWM's frames on screen**: `FfProg`/`FfFrames`/`FfSeq` 4, `FfRegs` 1,
  `FfPres` 1, `FfMoved` 2, `FfSame` 1, `FfNoRec` 1. No refusals (`FfRef01..14` all 0); `FfFlipFail`,
  `FfStale` and `FfGaveUp` 0.
* After 4 frames nothing more was flipped, which matches the missing kept-picture completion
  (KMD v321).
* The Present arm skipped 7 Blt presents with a foreign source (`PrFgSkip`/`PrFgBlt` 7).
* No new `PBRet`. No crash, no TDR. Venus DWM pid 5792 stable for 30 s after the revert.
* **No UMD log.** `umd-6340.log` already existed, from a 01:23 process with the same pid under
  another DWM account, so the new DWM's appends were refused and nothing was logged. The UMD did run:
  the guard file was written at 10:52:21. Fixed: when the per-pid name refuses the append, the UMD
  logs to `umd-<pid>-<creation time>.log` (Rust `umd_common::log` and the D3D11 bridge agree on the
  name).
* The backend logs no `ScanoutFlip` lines at its current level (lines 3194843 to 3197753).
