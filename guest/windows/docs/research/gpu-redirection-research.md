# What keeps a blit-model window's redirection surface on the GPU

Status: research note with public sources only, no code. It answers the question that
`../vram-redirection.md` (lane F) leaves open after stage V1: what makes Windows (dxgkrnl, win32k/CDD,
DWM) keep the redirection surface of a windowed, blit-model D3D11 app GPU-resident and GPU-ordered, as on
NVIDIA/AMD/Intel hardware, rather than Blt the frame into a CPU-visible staging surface and lock and read
it on the CPU every frame (what this KMD gets today: `PBdStd`=3, DxgKrnl `Lock` events 41/42 on the app
thread, event 215 with `D3DKMT_PM_REDIRECTED_BLT`).

## 0. The short answer

1. **The switch is GDI hardware acceleration**, i.e. `DXGK_PRESENTATIONCAPS.SupportKernelModeCommandBuffer`
   plus `DxgkDdiRenderKm` and the GDI standard allocations. Microsoft's WDDM 1.2 feature-caps table lists
   it as **mandatory** for full-graphics and render-only drivers ("A required feature starting with WDDM
   1.1"), so every certified NVIDIA/AMD/Intel KMD sets it. With it, a window's redirection bitmap is a
   `D3DKMDT_GDISURFACE_TEXTURE`: not CPU visible, created shared, opened by a UMD (DWM's) as a texture for
   composition, and used by a UMD as a DirectX **render target**. The blit-model Present then writes it on
   the GPU, and nobody locks it.
2. **Without it, Windows uses the documented CPU path**, which is exactly what our trace shows:
   dxgkrnl Blts the back buffer into a `D3DKMDT_STANDARDALLOCATION_STAGINGSURFACE`, and "the staging surface
   is then locked and read by the CPU". `DXGK_VIDMMCAPS.NonCpuVisiblePrimary` does not change this
   (hardware result: `PBdStd`=3 with and without it). That matches the documentation, which ties GDI
   `TEXTURE` redirection to GDI acceleration.
3. **PresentMon's "Composed: Copy with GPU GDI" does not mean the driver accelerates GDI.** PresentMon
   assigns that label to every Win7-style redirected blit (DxgKrnl `Blit` with `bRedirectedPresent=0`,
   then a present-history token with model `D3DKMT_PM_REDIRECTED_BLT`). Bare metal and this VM show the
   same mode, and the label cannot tell a TEXTURE redirection surface from the staging path.
4. **No public WDDM driver implements GDI acceleration.** VirtualBox, viogpu3d and Microsoft's own render-only
   sample all report no `SupportKernelModeCommandBuffer` and get the CPU path (or never see a
   blit-model window). Implementing `DxgkDdiRenderKm` would be new ground; section 4 lists what is
   documented as required.
5. **Two untested one-bit middle steps exist** (public SDK header comments): `DriverSupportsCddDwmInterop`
   (bit 8, "Driver does not support hardware GDI acceleration, but supports Cdd-Dwm interop") and
   `SupportSoftwareDeviceBitmaps` (bit 21, "Driver supports D3DKMDT_GDISURFACE_TEXTURE_CPUVISIBLE
   redirection bitmaps"). The second still gives a CPU-visible surface, so it most likely keeps a CPU
   lock. Neither is documented as making blit-model redirection GPU-only.
6. **The flip-model upgrade** (Windows 11 "Optimizations for windowed games") removes the redirected Blt
   entirely and needs no driver cap, but DXGI's game classification gates it. It has been tried here and
   does not apply to Heaven (`docs/HANDOFF.md`, "Tried": `REASON_NONGAME`).

## 1. Microsoft documentation

### 1.1 GDI hardware acceleration (WDDM 1.1, Windows 7)

[GDI Hardware Acceleration](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gdi-hardware-acceleration):

* The KMD reports `DXGKDDI_INTERFACE_VERSION >= DXGKDDI_INTERFACE_VERSION_WIN7` and sets
  `DXGK_PRESENTATIONCAPS->SupportKernelModeCommandBuffer`. "The driver should report this type of support
  only if the cache-coherent GPU aperture segment exists and there's no significant performance penalty
  when the CPU accesses GPU memory."
* Required functions: `DxgkDdiCreateAllocation`, `DxgkDdiGetStandardAllocationDriverData`,
  `DxgkDdiRenderKm`.
* Structures: `D3DKMDT_GDISURFACEDATA`, `D3DKMDT_GDISURFACEFLAGS`, `DXGK_CREATECONTEXTFLAGS`,
  `DXGK_CREATEDEVICEFLAGS`, `DXGK_GDIARG_ALPHABLEND`, `_BITBLT`, `_CLEARTYPEBLEND`, `_COLORFILL`,
  `_STRETCHBLT`, `_TRANSPARENTBLT`, `DXGK_RENDERKM_COMMAND`, `DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA`,
  `DXGKARG_RENDER`, `D3DKM_TRANSPARENTBLTFLAGS`; enums `D3DKMDT_GDISURFACETYPE`, `DXGK_GDIROP_BITBLT`,
  `DXGK_GDIROP_COLORFILL`, `DXGK_RENDERKM_OPERATION`.

[Initialization and DMA Buffer Creation](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/initialization-and-dma-buffer-creation):
`DRIVER_INITIALIZATION_DATA.DxgkDdiRenderKm` must point to the driver's function; dxgkrnl calls it "to
generate a DMA buffer from the command buffer that is passed by the kernel-mode Canonical Display Driver
(CDD)"; the GDI context and device arrive with `DXGK_CREATECONTEXTFLAGS.GdiContext` and
`DXGK_CREATEDEVICEFLAGS.GdiDevice`.

[Specifying GDI Hardware-Accelerated Rendering Operations](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/specifying-gdi-hardware-accelerated-rendering-operations):
`DXGKARG_RENDER.pCommand` is an array of variable-size `DXGK_RENDERKM_COMMAND`s; the driver "must
translate the input DXGK_RENDERKM_COMMAND command buffer into DMA buffer commands and build the patch
location list". Opcodes (`DXGK_RENDERKM_OPERATION`): `DXGK_GDIOP_BITBLT`=1, `_COLORFILL`=2,
`_ALPHABLEND`=3, `_STRETCHBLT`=4, `_TRANSPARENTBLT`=6, `_CLEARTYPEBLEND`=7.
[Supporting Kernel-Mode Command Buffers](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/supporting-kernel-mode-command-buffers):
submission follows the normal command-buffer rules; `DXGKARG_RENDER.MultipassOffset` tracks progress.

[Setting the Size and Pitch of the Memory Allocation](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/setting-the-size-and-pitch-of-the-memory-allocation):
for CPU-visible GDI surfaces (`STAGING_CPUVISIBLE`, `EXISTINGSYSMEM`) the driver returns
`StandardAllocationType = D3DKMDT_STANDARDALLOCATION_GDISURFACE` and the `Pitch` in
`D3DKMDT_GDISURFACEDATA`.

### 1.2 Is it optional?

[WDDM Driver and Feature Caps](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/wddm-driver-and-feature-caps),
table "WDDM 1.2 feature caps": **GDI Hardware Acceleration, "A required feature starting with WDDM 1.1",
M (full graphics), M (render-only), NA (display-only), cap
`DXGK_PRESENTATIONCAPS.SupportKernelModeCommandBuffer`.** dxgkrnl loads an adapter without it (this KMD,
VirtualBox and viogpu3d all run without it, and `query_adapter_info.rs` records the 2026-07-06
`GdiAccelMode=0` A/B), so "mandatory" means the certification requirement, which hardware vendors meet.
Bare-metal redirection behaviour is therefore GDI-accelerated behaviour.

### 1.3 The GDI surface types: when the redirection bitmap is a GPU texture

[D3DKMDT_GDISURFACETYPE](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ne-d3dkmdt-_d3dkmdt_gdisurfacetype)
("the type of lockable surface that is used by the Desktop Window Manager (DWM) for redirection"), values
from the public `d3dkmdt.h`:

| value | type | documented properties |
|---|---|---|
| 1 | `TEXTURE` | one-level texture; **not visible to the CPU, VidMm creates it as a shared surface**; opened by a UMD and used as a texture during DWM composition; **used by a UMD as a render target for DirectX rendering**; source or destination of GDI accelerated operations |
| 2 | `STAGING_CPUVISIBLE` | CPU visible and used heavily by the CPU; linear, in a cache-coherent aperture segment; source of GDI accelerated operations, destination of copy-only BitBlt; driver returns `Pitch`; pitch and address aligned to `AlignmentShift` |
| 3 | `STAGING` | not CPU visible; source or destination of accelerated and copy-only operations |
| 4 | `LOOKUPTABLE` | not CPU visible; `D3DDDIFMT_A8`; ClearType gamma table, created once and filled by a BitBlt from a `STAGING_CPUVISIBLE` surface |
| 5 | `EXISTINGSYSMEM` | CPU visible, linear, cache-coherent aperture; the surface address is passed to the driver; used like `STAGING_CPUVISIBLE` |
| 6 | `TEXTURE_CPUVISIBLE` | "Reserved for system use" (Windows 8); see `SupportSoftwareDeviceBitmaps` below |
| 7 | `TEXTURE_CROSSADAPTER` | not CPU visible, shared cross-adapter surface; pitch and height aligned to `D3DKMT_CROSS_ADAPTER_RESOURCE_PITCH/HEIGHT_ALIGNMENT` (Windows 8.1) |
| 8 | `TEXTURE_CPUVISIBLE_CROSSADAPTER` | reserved for system use (Windows 8.1) |

So the window redirection bitmap that DWM samples and that D3D renders into is `TEXTURE`, and the
CPU-visible types are GDI's staging for software fallbacks and uploads. Nothing in the documentation says
when win32k/CDD picks `TEXTURE` other than this pairing with GDI acceleration; the WDDM 1.1-era material
says the same from the other side:

* [Engineering Windows 7 Graphics Performance](https://learn.microsoft.com/en-us/archive/blogs/e7/engineering-windows-7-graphics-performance)
  (E7 blog): in Vista every GDI window had a video-memory copy for DWM and a system-memory copy for CPU
  GDI; Windows 7 with WDDM 1.1 drivers removes the system-memory copy, GDI rendering is accelerated and
  falls back to aperture memory, and WDDM 1.0 drivers "do not take advantage of the new feature".
* [Redirecting GDI, DirectX, and WPF applications](https://learn.microsoft.com/en-us/archive/blogs/greg_schechter/redirecting-gdi-directx-and-wpf-applications)
  (Greg Schechter, 2006, the Vista model): GDI windows get a system-memory surface plus a video-memory
  surface; DirectX windows "only need a single window buffer", shared between the app and DWM.

### 1.4 The staging surface: our path, documented

[D3DKMDT_STAGINGSURFACEDATA](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ns-d3dkmdt-_d3dkmdt_stagingsurfacedata):
"The graphics subsystem calls the display miniport driver's DxgkDdiPresent function to issue bit-block
transfer (bitblt) requests to transfer data from an application's back buffer into the staging surface.
The staging surface is then locked and read by the CPU." It is always X8R8G8B8 and the size of the back
buffer. That is `PBdStd`=3 plus Lock 41/42 on the app thread: the redirected Blt's destination cannot be
the window's redirection bitmap (a CPU/GDI surface on this adapter), so dxgkrnl stages it on the GPU and
copies it into the CPU-side bitmap itself, and that copy must wait for the Blt DMA packet to retire.

### 1.5 The redirected-blit protocol on the D3D side

[DwmDxGetWindowSharedSurface](https://learn.microsoft.com/en-us/windows/win32/dwm/dwmdxgetwindowsharedsurface)
(Windows 7 documentation, runtime-only API): `S_OK` returns a DWM shared surface the runtime renders into
(`D3DKMTRender` with the update id); `DWM_S_GDI_REDIRECTION_SURFACE` (only with
`DWM_REDIRECTION_FLAG_SUPPORT_PRESENT_TO_GDI_SURFACE`) tells the runtime to present with
`D3DKMTPresent`, `PresentHistoryToken.Model = D3DKMT_PM_REDIRECTED_BLT`, into the window's GDI
redirection surface. Our event 215 (`PresentHistoryDetailed_Start`) carries model 3 =
`D3DKMT_PM_REDIRECTED_BLT`, so DWM gives the window its GDI redirection surface. Where that surface lives
(GPU `TEXTURE` or CPU memory) is decided by the GDI caps above. The page names no driver cap that would
make DWM hand out a shared D3D surface instead, and documents the API for Windows 7 only.

### 1.6 Other caps that were candidates

* [DXGK_VIDMMCAPS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_vidmmcaps):
  `NonCpuVisiblePrimary` "Indicates that GDI allocations are not required to be CPU visible" (WDDM 2.0);
  `ReplicateGdiContent` "supports the replication of GDI content" (WDDM 2.0). Neither creates GDI
  acceleration. Measured here: `NonCpuVisiblePrimary` leaves the destination at `PBdStd`=3.
* `CrossAdapterResource` / `CrossAdapterResourceTexture` / `CrossAdapterResourceScanout` (same page):
  hybrid render-here/display-there presentation. Cross-adapter resources are aperture only, CPU visible,
  write-combined and linear, so they do not lead to a GPU-only redirection surface.
* `DXGK_DRIVERCAPS.SupportDirectFlip`, `SupportMultiPlaneOverlay`, `DXGK_FLIPCAPS.FlipIndependent`
  (public `d3dkmddi.h` comment: "Support MMIO flip to redirected surfaces bypassing DWM Present"): all
  flip-model mechanisms. [Direct flip of video memory](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/direct-flip-of-video-memory)
  is likewise flip-model. None of them applies to a blit-model swap chain.

### 1.7 The PresentationCaps bits, with the SDK header comments

From the public SDK `shared/d3dkmddi.h` (10.0.16299, mirrored at
[tpn/winsdk-10 d3dkmddi.h L1583-L1620](https://github.com/tpn/winsdk-10/blob/master/Include/10.0.16299.0/shared/d3dkmddi.h#L1583-L1620))
and [DXGK_PRESENTATIONCAPS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_presentationcaps):

| bit | mask | field | header comment / doc |
|---|---|---|---|
| 0 | 0x1 | `NoScreenToScreenBlt` | no kernel-mode Present within the same allocation |
| 1 | 0x2 | `NoOverlapScreenBlt` | ... with overlapping rects |
| 2 | 0x4 | **`SupportKernelModeCommandBuffer`** | "Driver supports RenderKm DDI" |
| 3-7 | 0x8-0x80 | `NoSameBitmapAlphaBlend`, `NoSameBitmapStretchBlt`, `NoSameBitmapTransparentBlt`, `NoSameBitmapOverlappedAlphaBlend`, `NoSameBitmapOverlappedStretchBlt` | dxgkrnl will not request these |
| 8 | 0x100 | **`DriverSupportsCddDwmInterop`** | "Driver does not support hardware GDI acceleration, but supports Cdd-Dwm interop." Doc: supports CDD present operations to texture allocations created by the UMD for DWM; ignored (and implied) when GDI acceleration is on |
| 10-13 | | `AlignmentShift` | Blt pitch alignment `1 << n`, >= 2 |
| 14-16 | | `MaxTextureWidthShift` | max width `2^(n+11)` |
| 17-19 | | `MaxTextureHeightShift` | max height `2^(n+11)` |
| 20 | 0x100000 | `SupportAllBltRops` | all ROP3 with solid pattern in BitBlt/ColorFill |
| 21 | 0x200000 | `SupportMirrorStretchBlt` | |
| 22 | 0x400000 | `SupportMonoStretchBltModes` | BLACKONWHITE/WHITEONBLACK |
| 23 | 0x800000 | `StagingRectStartPitchAligned` | staging rect left must be 0 |
| 24 | | `NoSameBitmapBitBlt` | |
| 25 | | `NoSameBitmapOverlappedBitBlt` | |
| 27 | | `NoTempSurfaceForClearTypeBlend` | no temporary surface for ClearType |
| 28 | | **`SupportSoftwareDeviceBitmaps`** | "Driver supports D3DKMDT_GDISURFACE_TEXTURE_CPUVISIBLE redirection bitmaps." (Win8; the reference page says "reserved, set to zero") |
| 29 | | `NoCacheCoherentApertureMemory` | (Win8) |
| 30 | | `SupportLinearHeap` | linear heap allocation from staging surfaces (Win8) |

Bit positions follow the struct declaration order. The reference page's prose ("equivalent to setting
the ... bit") is unreliable after bit 9 because it counts the multi-bit fields as one bit each, and the
header order is authoritative: `AlignmentShift` occupies bits 10-13, so `SupportAllBltRops` is bit 20
and `SupportSoftwareDeviceBitmaps` is bit 28 (mask 0x10000000), **not** 0x200000 as the early message
from this lane said. Check the mask against the WDK header used by the build before any experiment.

## 2. PresentMon: GPU GDI vs CPU GDI, from the source

[PresentMon](https://github.com/GameTechDev/PresentMon), commit `38ec4a7e`:

* [PresentMonTraceConsumer.hpp L66-L82](https://github.com/GameTechDev/PresentMon/blob/38ec4a7e2998a7c4eea2c3c7dd4772f29ef32f76/PresentData/PresentMonTraceConsumer.hpp#L66-L82):
  *Composed_Copy_with_GPU_GDI (a.k.a. Win7 Blit)*: runtime PresentStart, DxgKrnl `Blit`,
  `PresentHistoryDetailed`, DxgKrnl `Present`, `PresentHistory_Info`, DWM UpdateWindow, DWM Present.
  *Composed_Copy_with_CPU_GDI (a.k.a. Vista Blit)*: `Blit`, `PresentHistory_Start` (with a legacy blit
  token), `PresentHistory_Info`, DWM `FlipChain`.
* [HandleDxgkBlt, PresentMonTraceConsumer.cpp L440-L467](https://github.com/GameTechDev/PresentMon/blob/38ec4a7e2998a7c4eea2c3c7dd4772f29ef32f76/PresentData/PresentMonTraceConsumer.cpp#L440-L467):
  `Blit_Info` (event 166) with `bRedirectedPresent != 0` gives `Composed_Copy_CPU_GDI`; otherwise
  `Hardware_Legacy_Copy_To_Front_Buffer` for the moment.
* [HandleDxgkPresentHistory, L809-L835](https://github.com/GameTechDev/PresentMon/blob/38ec4a7e2998a7c4eea2c3c7dd4772f29ef32f76/PresentData/PresentMonTraceConsumer.cpp#L809-L835):
  a present-history token (event 171 `PresentHistory_Start`, or 215 `PresentHistoryDetailed_Start`)
  following a non-redirected Blit, with model `D3DKMT_PM_UNINITIALIZED` or `D3DKMT_PM_REDIRECTED_BLT`,
  turns it into `Composed_Copy_GPU_GDI`.
* Model enum ([Microsoft_Windows_DxgKrnl.h](https://github.com/GameTechDev/PresentMon/blob/38ec4a7e2998a7c4eea2c3c7dd4772f29ef32f76/PresentData/ETW/Microsoft_Windows_DxgKrnl.h)):
  `REDIRECTED_GDI`=1, `REDIRECTED_FLIP`=2, `REDIRECTED_BLT`=3, `REDIRECTED_VISTABLT`=4,
  `SCREENCAPTUREFENCE`=5, `REDIRECTED_GDI_SYSMEM`=6, `REDIRECTED_COMPOSITION`=7, `SURFACECOMPLETE`=8,
  `FLIPMANAGER`=9.

So "GPU GDI" means "Win7 redirected blit into the window's GDI redirection surface, signalled by a token",
and "CPU GDI" means "Vista-style blit (`bRedirectedPresent`, legacy blit token, DWM FlipChain)". Neither
reflects whether the redirection surface is a GPU `TEXTURE` or CPU memory reached through staging. A
VM with our trace and a bare-metal NVIDIA box both print "Composed: Copy with GPU GDI". The
distinguishing signals are elsewhere: the Blt destination's standard type (`PBdStd` 3 vs 4 with
`PBdGdi` 1), DxgKrnl Lock 41/42 on the app thread, and whether DWM opens the destination.

## 3. Public drivers

| driver | PresentationCaps | RenderKm | GDI surfaces | redirection of blit-model windows |
|---|---|---|---|---|
| VirtualBox WDDM (VBoxMPWddm) | `NoScreenToScreenBlt`, `NoOverlapScreenBlt`, `AlignmentShift`=2, `MaxTextureWidthShift`/`HeightShift`=2 ([VBoxMPWddm.cpp L1834-L1839](https://github.com/VirtualBox/virtualbox/blob/7be7704a6e2325e7df374aa7192354fc8d037615/src/VBox/Additions/win/Graphics/Video/mp/wddm/VBoxMPWddm.cpp#L1834-L1839)) | none | `GDISURFACE` case commented out ("port to Win7 DDI", [L2781](https://github.com/VirtualBox/virtualbox/blob/7be7704a6e2325e7df374aa7192354fc8d037615/src/VBox/Additions/win/Graphics/Video/mp/wddm/VBoxMPWddm.cpp#L2781)) | CPU path: its Present arm "To GDI software drawing surface" reads the D3D surface back into the SHADOW/STAGING allocation through a GMRFB ([VBoxMPGaWddm.cpp L691-L720](https://github.com/VirtualBox/virtualbox/blob/7be7704a6e2325e7df374aa7192354fc8d037615/src/VBox/Additions/win/Graphics/Video/mp/wddm/gallium/VBoxMPGaWddm.cpp#L691-L720)) |
| viogpu3d (virtio-gpu 3D WDDM, [max8rr8 fork](https://github.com/max8rr8/kvm-guest-drivers-windows/tree/viogpu3d/viogpu/viogpu3d)) | 0 ([viogpu_adapter.cpp](https://github.com/max8rr8/kvm-guest-drivers-windows/blob/9ed3aab11fb46e55dc835ff008008623b290a6cf/viogpu/viogpu3d/viogpu_adapter.cpp#L485-L507): WDDM 1.3, `SupportDirectFlip`=1, `FlipOnVSyncMmIo`, `SectionBackedPrimary`) | none | none; SHADOW and STAGING are coherent-mapped virgl resources ([viogpu_allocation.cpp L186-L227](https://github.com/max8rr8/kvm-guest-drivers-windows/blob/9ed3aab11fb46e55dc835ff008008623b290a6cf/viogpu/viogpu3d/viogpu_allocation.cpp#L186-L227)) | CPU path, the same as ours today |
| viogpudo (upstream virtio-win) | display-only (KMDOD) | n/a | n/a | n/a (no render) |
| Microsoft RosKmd (render-only sample, [graphics-driver-samples](https://github.com/microsoft/graphics-driver-samples)) | `SupportKernelModeCommandBuffer`=FALSE, `SupportSoftwareDeviceBitmaps`=TRUE, `NoScreenToScreenBlt`, `NoOverlapScreenBlt`, `MaxTexture*Shift`=3 with the comment "Allow 16Kx16K texture (redirection device bitmap)" ([RosKmdAdapter.cpp L998-L1014](https://github.com/microsoft/graphics-driver-samples/blob/de4a2161991eda254013da6c18226f5ea06e4a9c/render-only-sample/roskmd/RosKmdAdapter.cpp#L998-L1014)) | registered but `NT_ASSERT(FALSE)` ([RosKmdContext.cpp L197-L204](https://github.com/microsoft/graphics-driver-samples/blob/de4a2161991eda254013da6c18226f5ea06e4a9c/render-only-sample/roskmd/RosKmdContext.cpp#L197-L204)) | STAGING and GDISURFACE return `STATUS_NOT_IMPLEMENTED` ([L1972-L1995](https://github.com/microsoft/graphics-driver-samples/blob/de4a2161991eda254013da6c18226f5ea06e4a9c/render-only-sample/roskmd/RosKmdAdapter.cpp#L1972-L1995)) | not handled (a flip/DirectFlip-only sample); the CosKmd compute sample only handles the cross-adapter GDI types |
| Linux dxgkrnl (WSL GPU-PV client, [drivers/hv/dxgkrnl](https://github.com/microsoft/WSL2-Linux-Kernel/tree/linux-msft-wsl-6.6.y/drivers/hv/dxgkrnl)) | none (not a KMD; forwards to the host over VMBus) | n/a | asks the host for `GDISURFACE` / `TEXTURE_CROSSADAPTER` data only to wrap existing system memory ([ioctl.c L1298-L1320](https://github.com/microsoft/WSL2-Linux-Kernel/blob/a07f9ea8a99139913acbcc1c160b132cb2a49c81/drivers/hv/dxgkrnl/ioctl.c#L1298-L1320)) | n/a (no DWM) |
| Hyper-V GPU-PV Windows guest | the guest's virtual render device runs the host vendor's driver stack; its display is a separate display-only/indirect adapter ([GPU paravirtualization](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization)) | host vendor's | host vendor's | not documented; windows are rendered on the VRD and shown on a different adapter |
| VMware SVGA WDDM, ReactOS | closed source (VMware); ReactOS has only the `dispmprt.h` header, no dxgkrnl presentation code | | | no public evidence |

No public driver implements `DxgkDdiRenderKm`; a GitHub code search for `DxgkDdiRenderKm` together
with `DXGK_GDIOP_BITBLT` finds only Microsoft's documentation. Every open virtual GPU therefore runs
blit-model windows through the staging/CPU path, which explains why this has not been solved elsewhere.

## 4. Routes that change the model for blit-model windows

| route | what it changes | driver side | status here |
|---|---|---|---|
| GDI hardware acceleration (`SupportKernelModeCommandBuffer` + `RenderKm`) | redirection bitmap becomes GDI `TEXTURE` (GPU-only, shared, DWM samples it, the redirected Blt renders into it) | section 5 | the documented route; lane "GDI acceleration on the copy engine" (`feat/vram-redirection-gdi-accel`); RM-VRAM backing for the TEXTURE arm exists behind `RedirVram` (`feat/vram-redirection` 82894740) |
| `DriverSupportsCddDwmInterop` (bit 8) without GDI acceleration | documented only as "CDD present operations to texture allocations created by the UMD for DWM"; whether the redirected D3D Blt then targets a DWM texture is not documented | Present Blts from GDI surfaces into DWM UMD textures | untested; one-bit experiment |
| `SupportSoftwareDeviceBitmaps` (bit 28) | redirection bitmaps become `TEXTURE_CPUVISIBLE` (one CPU-visible allocation that DWM can sample directly, no separate upload) | `GetStandardAllocationDriverData` for type 6; CPU-visible placement | untested; still CPU visible, so a lock per GPU write is expected (CPU and GPU writers of one surface); may still save DWM's upload |
| `NonCpuVisiblePrimary` | "GDI allocations are not required to be CPU visible" | none on its own | measured: no change (`PBdStd`=3) |
| flip-model upgrade ("Optimizations for windowed games", `SwapEffectUpgradeEnable=1` in `HKCU\Software\Microsoft\DirectX\UserGpuPreferences`, [DirectX blog](https://devblogs.microsoft.com/directx/updates-in-graphics-and-gaming/)) | DX10/11 blit-model windowed swap chains become flip model; no redirected Blt at all | none documented | tried: works for some apps, never Heaven; DXGI's game classification gates it (`REASON_NONGAME`, `docs/HANDOFF.md`) |
| DirectFlip / independent flip / MPO / cross-adapter caps | flip model, or a CPU-visible aperture surface | | do not apply to blit-model windows |
| DWM shared D3D surface (`DwmDxGetWindowSharedSurface` returning `S_OK`) | runtime renders into a DWM-owned shared surface | none documented | Windows 7-only documentation; our trace shows the `DWM_S_GDI_REDIRECTION_SURFACE` branch (model 3), and no cap selecting the other branch is documented |

## 5. Recommendation: the minimal set for a GPU-resident (TEXTURE) redirection surface

Everything below is the documented WDDM 1.1 GDI acceleration contract. Field names are from
`d3dkmddi.h`/`d3dkmdt.h`.

**Caps (`DXGK_DRIVERCAPS.PresentationCaps`, `DxgkDdiQueryAdapterInfo` `DXGKQAITYPE_DRIVERCAPS`):**

* `SupportKernelModeCommandBuffer = 1` (bit 2). Precondition per the docs: a cache-coherent aperture
  segment exists (ours: segment 1, `Aperture|CacheCoherent`). Leave `NoCacheCoherentApertureMemory = 0`.
* `AlignmentShift` >= 2 (bits 10-13). 6 (64 B) or 7 (128 B) suits the copy engine and NVK's linear
  import pitch; `STAGING_CPUVISIBLE`/`EXISTINGSYSMEM` pitch and address must honour it.
* `MaxTextureWidthShift = MaxTextureHeightShift = 3` (16K), as RosKmd does.
* Reduce what CDD can send while the executor is minimal: `NoSameBitmapBitBlt`,
  `NoSameBitmapOverlappedBitBlt`, `NoSameBitmapAlphaBlend`, `NoSameBitmapStretchBlt`,
  `NoSameBitmapTransparentBlt`, `NoSameBitmapOverlappedAlphaBlend`, `NoSameBitmapOverlappedStretchBlt`,
  `NoScreenToScreenBlt`, `NoOverlapScreenBlt` = 1; `SupportAllBltRops`, `SupportMirrorStretchBlt`,
  `SupportMonoStretchBltModes` = 0; `StagingRectStartPitchAligned` = 1 if the CE copy wants whole rows.
  The docs define no per-command refusal in `RenderKm`, so these "No*" bits are the only documented way
  to keep operations away; everything CDD still sends has to execute correctly.
* Keep `DriverSupportsCddDwmInterop` = 0 (ignored once GDI acceleration is on).

**DDIs:**

* `DRIVER_INITIALIZATION_DATA.DxgkDdiRenderKm`: parse `DXGK_RENDERKM_COMMAND`s and emit DMA plus the patch
  list for `DXGK_GDIOP_BITBLT` (copy and the ROPs left enabled), `_COLORFILL`, `_ALPHABLEND`,
  `_STRETCHBLT`, `_TRANSPARENTBLT`, `_CLEARTYPEBLEND`. Copies and fills map onto the copy engine;
  AlphaBlend, StretchBlt with filtering, TransparentBlt and ClearTypeBlend need a 3D/compute engine or a
  CPU executor on CPU-visible operands.
* `DxgkDdiCreateDevice`/`DxgkDdiCreateContext`: accept `DXGK_CREATEDEVICEFLAGS.GdiDevice` and
  `DXGK_CREATECONTEXTFLAGS.GdiContext`; the GDI context's DMA goes to the engine that executes the copies.
* `DxgkDdiGetStandardAllocationDriverData` for `D3DKMDT_STANDARDALLOCATION_GDISURFACE`
  (`pCreateGdiSurfaceData`):
  * `TEXTURE`: not CPU visible, shareable (DWM's UMD opens it; for DWM on NVK, an RM-VRAM object
    imported by resource id: the `RedirVram` arm), usable as a D3D render target and as the destination
    of the redirected `DxgkDdiPresent` Blt (app back buffer -> TEXTURE, VRAM-to-VRAM on the CE, GPU-ordered,
    no `Lock`).
  * `STAGING_CPUVISIBLE` and `EXISTINGSYSMEM`: linear, aperture (cache-coherent), CPU visible, `Pitch`
    returned and aligned to `AlignmentShift`; these are CDD's software-GDI side and the BitBlt sources
    that bring CPU-drawn content into the TEXTURE.
  * `STAGING`: GPU-only scratch.
  * `LOOKUPTABLE`: A8 gamma table for ClearType.
  * Keep the existing SHAREDPRIMARY/SHADOW/STAGINGSURFACE arms; dxgkrnl still uses them for the desktop
    and for other windows.
* `DxgkDdiCreateAllocation`: `Size` includes the pitch for CPU-visible GDI types
  ([size and pitch](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/setting-the-size-and-pitch-of-the-memory-allocation)).
* `DxgkDdiPresent`: Blt with a GDI `TEXTURE` destination from an app back buffer (redirected blit; the
  DMA carries `DXGK_SUBMITCOMMANDFLAGS.RedirectedPresent`), and GDI-surface-to-primary/shadow Blts as
  before.

**Expected evidence once it works:** the Blt destination becomes `PBdStd`=4 with `PBdGdi`=1
(`TEXTURE`); DWM opens it (`StdOpenPid` = dwm.exe); DxgKrnl Lock 41/42 disappear from the app thread;
PresentMon still prints "Composed: Copy with GPU GDI" (section 2), so judge by fps and
`msInPresentAPI`, not by the mode string.

**Cheap experiments before the full executor**, each a single knob read at StartDevice and reversible
with `pnputil /restart-device` (main session only, per the VM rules):
1. `DriverSupportsCddDwmInterop` (0x100) alone: does the destination change type, or does DWM open it?
2. `SupportSoftwareDeviceBitmaps` (mask from the header, 0x10000000 by declaration order) alone: does
   `GetStandardAllocationDriverData` see `TEXTURE_CPUVISIBLE` (type 6) for redirection bitmaps, and does
   the Lock on the app thread survive?
3. `SupportKernelModeCommandBuffer` with a `RenderKm` that handles copy and fill on the CE and a CPU
   executor for the remaining opcodes on CPU-visible operands (the opcodes with a GPU-only operand then
   need a staging round trip): enough to see whether the redirection bitmap becomes `TEXTURE` and the
   blit-model Present stops locking.

## 6. Sources

Microsoft:
[GDI Hardware Acceleration](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gdi-hardware-acceleration),
[WDDM Driver and Feature Caps](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/wddm-driver-and-feature-caps),
[DXGK_PRESENTATIONCAPS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_presentationcaps),
[D3DKMDT_GDISURFACETYPE](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ne-d3dkmdt-_d3dkmdt_gdisurfacetype),
[D3DKMDT_STAGINGSURFACEDATA](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ns-d3dkmdt-_d3dkmdt_stagingsurfacedata),
[DXGK_VIDMMCAPS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_vidmmcaps),
[Initialization and DMA Buffer Creation](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/initialization-and-dma-buffer-creation),
[Setting the Size and Pitch of the Memory Allocation](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/setting-the-size-and-pitch-of-the-memory-allocation),
[Specifying GDI Hardware-Accelerated Rendering Operations](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/specifying-gdi-hardware-accelerated-rendering-operations),
[Supporting Kernel-Mode Command Buffers](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/supporting-kernel-mode-command-buffers),
[Reporting Optional Support for Rendering Operations](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/reporting-optional-support-for-rendering-operations),
[DwmDxGetWindowSharedSurface](https://learn.microsoft.com/en-us/windows/win32/dwm/dwmdxgetwindowsharedsurface),
[Direct flip of video memory](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/direct-flip-of-video-memory),
[GPU paravirtualization](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization),
[Engineering Windows 7 Graphics Performance](https://learn.microsoft.com/en-us/archive/blogs/e7/engineering-windows-7-graphics-performance),
[Redirecting GDI, DirectX, and WPF applications](https://learn.microsoft.com/en-us/archive/blogs/greg_schechter/redirecting-gdi-directx-and-wpf-applications),
[DirectX blog: Updates in Graphics and Gaming](https://devblogs.microsoft.com/directx/updates-in-graphics-and-gaming/),
[microsoft/graphics-driver-samples](https://github.com/microsoft/graphics-driver-samples),
[public SDK d3dkmddi.h (mirror)](https://github.com/tpn/winsdk-10/blob/master/Include/10.0.16299.0/shared/d3dkmddi.h),
[public SDK d3dkmdt.h (mirror)](https://github.com/tpn/winsdk-10/blob/master/Include/10.0.16299.0/shared/d3dkmdt.h).

Others:
[PresentMon](https://github.com/GameTechDev/PresentMon),
[VirtualBox WDDM miniport](https://github.com/VirtualBox/virtualbox/tree/main/src/VBox/Additions/win/Graphics/Video/mp/wddm),
[viogpu3d](https://github.com/max8rr8/kvm-guest-drivers-windows/tree/viogpu3d/viogpu/viogpu3d),
[virtio-win kvm-guest-drivers-windows](https://github.com/virtio-win/kvm-guest-drivers-windows),
[WSL2 Linux dxgkrnl](https://github.com/microsoft/WSL2-Linux-Kernel/tree/linux-msft-wsl-6.6.y/drivers/hv/dxgkrnl).
