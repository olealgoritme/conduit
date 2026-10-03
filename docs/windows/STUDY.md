# Windows guests: feasibility study

**Question.** Can a Windows 11 guest run NVIDIA's own, unmodified Windows user-mode
drivers on top of a Conduit kernel driver that forwards to the existing Linux backend
(`virtio-nvgpu`, which forwards Linux RM ioctls to the host's 610.57.04 open driver)?
How does that compare with the open-stack route?

**Method.** Static analysis only, of public binaries: import/export tables, string tables,
32-bit constant scans checked against the public RM SDK headers, and reading the
instructions around a few call sites. Nothing was executed, and no GPU or VM was used.
No leaked material was used. This document describes behaviour in our own words. The only
things carried over are interface facts: magic numbers, sizes, offsets, IDs and registry
value names.

**Inputs.**

| package | Windows build | internal branch (from version strings) |
|---|---|---|
| vGPU 19.x guest (`NVIDIA-GRID-Linux-KVM-…-582.53.zip`) | 582.53 | r580 / r582_49 |
| vGPU 20.1 guest (`grid-20.1-595.71.03.zip`) | 596.36 | r596_25 |
| GeForce public | 610.62 | r610_45 |
| GeForce public | 610.74 | r610_00 |
| GeForce public | **610.88** | **r610_85**, the same branch tag found in the 610.57.04 open-kernel source |

The analysis scripts are in the session scratchpad (`winre/*.py`): imports, RM-constant
scan, envelope scan, churn, and cross-reference helpers. They are not in the repo yet. If
this work continues, move them into `tools/winre/`.

---

## TL;DR

1. **The private escape payload carries the RM API with an NVIDIA header in front.** Every
   NVIDIA UMD wraps its kernel requests in one envelope: a 0x30-byte header (`'NVDA'`
   magic, version, total size, `'NV**'` magic, function code) followed by a body shaped
   like an NVOS structure. The function group used for RM carries `RM_ALLOC` with the
   parameters inline, `RM_CONTROL` with the parameters behind a pointer (NVOS54 shape),
   `RM_FREE` and other operations. The UMDs embed 100–230 distinct RM control-command
   values from the 610 headers each. A control set of random values scores 0–10.
2. **Two transports carry the same envelope:** `D3DKMTEscape` (WDDM), and
   `DeviceIoControl` on `\\.\NvAdminDevice` (IOCTL `0x08DE0008`). The second never touches
   dxgkrnl. CUDA, NVML and nvcuvid contain both.
3. **The envelope is not the whole contract.** The UMDs also depend on:
   - a version-locked private adapter blob of about 49 KB (`KMTQAITYPE_UMDRIVERPRIVATE`)
     whose size changes with every branch;
   - NVIDIA-private allocation data on `CreateAllocation`;
   - escape function groups other than RM;
   - WDDM's own memory and scheduling model.

   A Conduit KMD would have to imitate the WDDM behaviour of `nvlddmkm.sys` in these
   places, not just forward RM calls.
4. **The ABI churn is mostly in the WDDM layer, not the RM layer.** Between 580 and 610,
   about 2% of the RM control parameter structs that the UMDs reference changed. The
   private adapter blob, on the other hand, changed size at every branch. We would pin one
   Windows build per host RM build: **610.88 ↔ 610.57.04**.
5. **Recommended order:**
   - **Phase 1:** CUDA, NVML and NVENC-via-CUDA, using the non-WDDM `NvAdminDevice`
     transport ("TCC-shaped"). This is the smallest surface and is RM-only.
   - **Phase 2:** the open stack (NVK on RM + DXVK/vkd3d-proton) for graphics.
   - **Later, as research:** full WDDM with NVIDIA's D3D/Vulkan UMDs. This needs real
     Windows traces first.

---

## 1. How the UMDs reach the kernel

### 1.1 Imports

None of the NVIDIA UMDs link `gdi32!D3DKMT*` statically. Each one resolves the thunks at
run time with `GetProcAddress`, and the names are present as strings. The D3D UMDs
(`nvwgf2umx`, `nvd3dumx`) do most of their kernel work through the D3D runtime's
callback tables (`pfnEscapeCb`, `pfnAllocateCb`, `pfnSubmitCommandCb`, …), which never
show up as imports. A missing D3DKMT name therefore does **not** mean the UMD avoids that
operation.

Thunks referenced by name in 582.53. 596.36 and 610.62 are essentially the same.

| DLL | role | D3DKMT names resolved | other kernel paths |
|---|---|---|---|
| `nvldumdx.dll` | UMD loader; the `UserModeDriverName` target. Exports `OpenAdapter10/10_2/12`, `OpenAdapter` | EnumAdapters2/3, QueryAdapterInfo, CloseAdapter | – |
| `nvwgf2umx.dll` | D3D10/11/12 UMD (also exports `NVENCODEAPI_Thunk`, `NVAPI_Thunk`) | 28: sync objects, keyed mutex, OpenResource, QueryVideoMemoryInfo, **Escape**, … (the bulk goes through runtime callbacks) | `DeviceIoControl` + `\\.\NvAdminDevice` |
| `nvd3dumx.dll` | D3D9 UMD | 25, similar set | NvAdminDevice |
| `nvoglv64.dll` | OpenGL ICD and **Vulkan ICD** (exports `vk_icd*`) | **103**, the full WDDM2 surface: CreateDevice, CreateContextVirtual, CreateHwQueue, CreatePagingQueue, Reserve/Map/Update/FreeGpuVirtualAddress, MakeResident/Evict, SubmitCommand, SubmitCommandToHwQueue, Submit{Signal,Wait}SyncObjectsToHwQueue, monitored fences, Present*, Lock2, Escape, … | NvAdminDevice; mentions a VMBus GUID (GPU-PV) |
| `nvcuda64.dll` | CUDA driver API (711 exports) | 25: Create/DestroyDevice, CreateAllocation(2), DestroyAllocation, Lock/Lock2/Unlock, MakeResident, SetAllocationPriority, QueryAllocationResidency, CreatePagingQueue, **Render**, **Escape**, QueryAdapterInfo, OpenResource, WaitForSynchronizationObjectFromCpu. **No SubmitCommand, no HwQueue, no MapGpuVirtualAddress** | `DeviceIoControl` on `\\.\NvAdminDevice` and `\\.\UVMLiteController` |
| `nvcuvid64.dll` | NVDEC/NVENC (CUDA side; exports `CreateEncoderInterface`) | 44, including CreateContextVirtual, CreateHwQueue, Map/Reserve/UpdateGpuVirtualAddress, SubmitCommand(ToHwQueue) | NvAdminDevice |
| `nvencodeapi64.dll` | NVENC front end | only EnumAdapters2/3 + QueryAdapterInfo; **no escapes, no RM constants** | – (it loads the D3D or CUDA UMD and thunks into it) |
| `nvapi64.dll` | NvAPI (in 610 most of it moved to `nvapi64_impl.dll`) | 21, including Escape, CreateContextVirtual | NvAdminDevice |
| `nvml.dll` | NVML / nvidia-smi | 18, including Escape, CreateAllocation, Render | NvAdminDevice, UVMLiteController |
| `nvdxgdmal64.dll` | interop helper | 57, the WDDM2 set including native fences | – |

The kernel driver (`nvlddmkm.sys`) creates the matching named objects:
`\Device\NvAdminDevice` / `\DosDevices\NvAdminDevice`,
`\Device\UVMLiteController…` / `\DosDevices\Global\UVMLiteController`, per-process
`UVMLiteProcess%lld`, and `NvLinkControl`. It imports only `IoCreateDevice`,
`IoCreateSymbolicLink` and `IoRegisterDeviceInterface` from the I/O manager. Nothing
about these names is protected by signatures, so a Conduit driver can create objects
with the same names.

**WSL build as a cross-check.** The Windows package also ships NVIDIA's WSL user-mode
libraries: `libcuda.so.1.1`, `libnvwgf2umx.so`, `libnvdxgdmal.so.1`. These are ELF files
that load `libdxcore.so`. WSL `libcuda` uses exactly 12 thunks: EnumAdapters2/3,
QueryAdapterInfo, CreateDevice, CreatePagingQueue, CreateAllocation2, DestroyAllocation,
Lock2, Unlock2, MakeResident, WaitForSynchronizationObjectFromCpu, **Escape**. There is
no submission thunk at all. CUDA on Windows therefore submits from user mode through RM
channels it creates itself. `nvcuda64` embeds the Blackwell GPFIFO (`0xC96F`), compute
(`0xCDC0`/`0xCEC0`) and copy (`0xC9B5`) class IDs, just as Linux CUDA does.

### 1.2 The envelope

The same envelope appears in every UMD, built inline at each call site:

| offset | size | content |
|---|---|---|
| 0x00 | 4 | magic `0x4E564441` (`'NVDA'`) |
| 0x04 | 4 | version: `0x00010002` for escapes, `0x00010004` for the adapter-private blob |
| 0x08 | 4 | total size including the header |
| 0x0C | 4 | magic `0x4E562A2A` (`'NV**'`) |
| 0x10 | 4 | function code = `group << 24 \| op` |
| 0x14… | | zeroed by callers. Callers read a status at 0x2C (and fall back to a generic error mapper when it is set) |
| 0x30 | – | body |

Group 5 is RM. The bodies seen in the group-5 call sites:

| op | body | matches |
|---|---|---|
| `0x2A` | hRoot, hParent, hNew, hClass, paramsSize, status, then the **params inline** (total = 0x48 + paramsSize). Seen allocating `NV01_ROOT_CLIENT 0x41`, `NV01_DEVICE_0 0x80` (0x38 params), `NV20_SUBDEVICE_0 0x2080` (4 params) | RM_ALLOC |
| `0x24` | hClient, hObject, cmd, flags, **pointer** to params, paramsSize, status (body 0x20). Example: cmd `0xB0CC010E`, 0x100 bytes | RM_CONTROL (NVOS54 shape) |
| `0x0C` | 0x10-byte body (three handles + status) | RM_FREE (NVOS00 shape), inferred |
| `0x03` and about 20 others | | not yet decoded |

A larger group-5 op set appears in `nvcuda64`/`nvml`: 0x03, 0x0A–0x0C, 0x10–0x14, 0x16,
0x18, 0x1B–0x1E, 0x24, 0x26, 0x2A–0x2C. These look like the rest of the RM entry points
(map/unmap, dup, share, vid-heap, events). They need to be mapped one by one.

**Op numbers are NVIDIA-Windows-specific.** Group 5 does *not* use the Linux
`NV_ESC_RM_*` numbers. On Linux, 0x2A is CONTROL and 0x2B is ALLOC. Here, 0x2A is ALLOC
and 0x24 is CONTROL. Translation is a table, not a passthrough.

**The second transport.** The same envelope, prefixed with one dword (a session/client
cookie read from a global), is sent with `DeviceIoControl(hNvAdmin, 0x08DE0008, …)`
(device type 0x8DE, function 2, METHOD_BUFFERED). In the CUDA helpers that use it, a
missing handle returns an error instead of falling back to `D3DKMTEscape`. The two
transports are separate code paths, chosen per mode. This matches Windows' TCC (WDM, no
dxgkrnl) versus WDDM split.

Other groups (1, 2, 3, 7, 9, 13, 15) appear mostly in the D3D, GL and NvAPI UMDs. Group 1
has about 60 distinct ops in NvAPI alone. These are driver-private WDDM/display/power/
stereo services. Nothing in them looks like RM structures, so each one we have to support
would have to be **emulated**.

### 1.3 How much of it is RM?

Static scan: how many distinct `NVxxxx_CTRL_CMD_*` values (1,468 in the 610 headers) each
binary contains as a 32-bit little-endian constant. A set of random values of the same
size is the noise floor.

| binary | 582.53 | 596.36 | 610.62 | random-control hits | top interfaces |
|---|---|---|---|---|---|
| nvwgf2umx (D3D10-12) | 205 | 198 | 199 | 7–10 | 2080, 0080, B0CC, 0073 |
| nvd3dumx (D3D9) | 106 | 102 | – | 2 | 0080, 2080, 0073 |
| nvoglv64 (GL+Vulkan) | 191 | 208 | – | 1–2 | 2080, 0080, B0CC, 0073 |
| nvcuda64 | 215 | 229 | 210 | 1 | 2080, 83DE (debugger), B0CC (profiler) |
| nvcuvid64 | 89 | 81 | – | 1–2 | 0080, 2080 |
| nvml | 189 | 191 | – | 0 | 2080, A081 |
| nvlddmkm.sys (for scale) | 1293 | 1316 | 1334 | 26–32 | – |

The UMDs speak RM controls directly, a few hundred distinct commands each.

Static analysis cannot give the *call-frequency* split between RM and non-RM escapes.
Many call sites pass the function code in a register through a shared wrapper:
`nvcuda64` has 139 envelope sites, of which 39 are statically group 5, 14 group 1 and 84
indeterminate. The D3D9 and GL UMDs show few static group-5 sites but many RM command
constants, which suggests a generic wrapper. Dynamic traces should settle this (§6). The
working estimate:
- CUDA/NVML: almost entirely RM plus WDDM allocation management.
- D3D/GL/Vulkan: RM for object setup, plus a long tail of group-1/2/3 private escapes,
  plus heavy use of WDDM DDIs with NVIDIA-private data.

### 1.4 ABI churn

**RM layer.** Comparison of RM SDK headers 580.159.03, 595.71.05 and 610.57.04:

| | common cmds | renumbered | removed | params struct changed | …of those, referenced by Windows UMDs |
|---|---|---|---|---|---|
| 580 → 610 | 1384 | 5 | 68 | 30 | 11 (mostly NVLink, ECC, G-Sync, DP; nothing on the hot path) |
| 595 → 610 | 1446 | 1 | 9 | 14 | 2 |

The struct comparison is textual and does not see changes inside nested types, so treat
these numbers as lower bounds. The RM layer is very stable.

**WDDM-private layer: version-locked.**

| constant | 582.53 (r582) | 596.36 (r596) | 610.62 / 610.74 / 610.88 (r610) |
|---|---|---|---|
| adapter-private blob (`UMDRIVERPRIVATE`) size, as used by CUDA/GL/NVML | 0xC115 (49,429 B) | 0xC2A8 (49,832 B) | **0xC5A8** (50,600 B) |
| D3D10-12 UMD: second v4-envelope structure | 0xDF198 | 0xDF198 | 0xDF198 |
| CUDA / nvdxgdmal second blob | 0x24A08 | 0x24A08 | 0x24A08 |
| escape header version | 0x00010002 | same | same |
| group-5 op numbering | – | identical | identical |

The KMD binaries contain the blob size that matches their own branch (`0xC115` five times
in 582.53's KMD, `0xC5A8` seven times in 610.62's). That points to an exact-size check.
⇒ **A Conduit KMD supports exactly one NVIDIA Windows branch at a time**, and should
match the host RM. 610.88 carries the same `r610_85` branch tag as the 610.57.04 open
kernel source, so that is the pairing.

---

## 2. What the UMD asks the KMD at startup

Evidence from the loader and the CUDA path:

1. **Adapter selection** (`nvldumdx`): EnumAdapters2, then for each adapter
   `KMTQAITYPE_PHYSICALADAPTERDEVICEIDS` (31). It keeps adapters with **VendorID ==
   0x10DE** and then issues `KMTQAITYPE_QUERYREGISTRY` (48) queries in a loop of four,
   one per UMD flavour, to read the driver's own registry values. QUERYREGISTRY is served
   by dxgkrnl from the adapter's software key, so the INF controls it.
2. **The adapter-private blob** (`KMTQAITYPE_UMDRIVERPRIVATE`, type 0): CUDA, GL, NVML,
   NvAPI and nvcuvid all request the ~50 KB blob in the `NVDA`/`NV**` v4 envelope with
   function 1. They read packed fields from it at unaligned offsets. One example is a
   count at about +0x3C69 followed by an array of 32-bit IDs.
   - The WSL D3D UMD names the type `NVL_PRIV_QUERYADAPTERINFO` (visible in its exported
     C++ signatures). It also names the allocation-private type
     `NVL_PRIV_ALLOCATIONINFO` and a `NVL_GPFIFO_ENTRY`.
   - The D3D10-12 UMD additionally builds a 0xDF198-byte (~892 KiB) v4-envelope structure, purpose not yet known.
   - **The contents are unknown statically and must be captured from a real machine.** At
     minimum they will include chip/arch IDs, the class list, segment and aperture
     layout, the RM client/device handles created on the UMD's behalf, and feature bits.
3. **Standard WDDM queries** also have to be answered convincingly: `DRIVERCAPS`,
   `QUERY_GPUMMU_CAPS` (VA bits, page-table levels, page sizes), segment sizes and groups,
   `WDDM_2_x/3_x_CAPS`, `ADAPTERTYPE`, `NODEMETADATA` (engine list), `KMD_DRIVER_VERSION`.
4. **Identity checks seen statically:**
   - Every UMD dynamically imports `WinVerifyTrust`/`CryptQueryObject` and embeds NVIDIA's
     production signer name. This almost certainly checks NVIDIA's own DLLs, which stay
     unmodified and signed. Whether any of it looks at the **KMD file** is unknown.
   - `nvcuda64` names `nvlddmkm.sys` and `SYSTEM\CurrentControlSet\Services\nvlddmkm`.
     `nvml` names `nvlddmkm`. CUDA reads the KMD's service key, possibly checking the
     image path or version, and we cannot yet tell what happens if it is missing.
   - `nvwgf2umx` imports BCrypt symmetric encrypt/decrypt. It is unclear whether this is
     only for content protection or also some escape authentication.
   - All four of these are the first things to confirm with traces.

### INF binding

`nvgridsw.inf` and the GeForce `nv_dispi.inf` family are DCH packages (`PnpLockdown=1`,
catalog-signed). The UMDs are bound only by **registry values in the adapter's software
key** (`HKR`), with paths under the DriverStore (`%13%`):

- `UserModeDriverName` = 4 × `nvldumdx.dll` (WoW: `nvldumd.dll`), and
  `UserModeDriverNameWsl` = `libnvwgf2umx.so`
- `OpenGLDriverName` = `nvoglv64.dll`; `VulkanDriverName` / `VulkanImplicitLayers` =
  `nv-vk64.json`; `OpenCLDriverName` = `nvopencl64.dll`
- `DriverSupportModules` lists `nvcuda.dll`, which is installed as a renamed
  `nvcuda_loader64.dll`.
- `CopyToVmWhenNewer` lists the files that Hyper-V GPU-PV copies into VMs. The UMDs are
  built to run with a *remote* KMD in that mode, which is a precedent for our design.

⇒ **Our INF can point those same value names at NVIDIA's DLLs without modifying them.**
Two problems remain:
- **Redistribution.** The DLLs must be in our DriverStore folder and covered by our
  catalog, so we cannot ship them. A guest-side installer would extract them from the
  user's own NVIDIA download and build the catalog locally. NVIDIA's Authenticode
  signatures stay intact.
- **Signing our KMD.** Test-signing works, but anti-cheat refuses it. Retail needs
  attestation signing (EV certificate + Microsoft Hardware Dev Center), which we can do as
  long as the package contains no NVIDIA files. That again argues for locally assembling
  a second, UMD-only package; that piece needs design work.

---

## 3. CUDA and NVENC

- **CUDA uses the same envelope, with two transports** (§1.2). On WDDM it uses
  `D3DKMTEscape` plus WDDM allocations (CreateAllocation with NVIDIA private data, Lock,
  MakeResident, a paging queue) and submits from user mode on its own RM channels. In the
  TCC/WDM style it uses `\\.\NvAdminDevice` IOCTLs. Unified memory goes to
  `\\.\UVMLiteController` in both cases.
- **NVENC** has no transport of its own. `nvencodeapi64` contains no escapes and no RM
  constants. It forwards into the D3D UMD (`NVENCODEAPI_Thunk` export) for D3D input
  surfaces, or into `nvcuvid64` (which holds the NVENC class `0xC9B7` and the decoder
  `0xC9B0`) for CUDA input. **NVENC with CUDA surfaces therefore costs nothing extra once
  CUDA works.** NVENC with D3D surfaces needs the D3D UMD.

---

## 4. Routes compared

### Route A: NVIDIA's own UMDs on a Conduit WDDM KMD

What the KMD must provide:
- the envelope with a group-5 → Linux RM translation table;
- the ~50 KB adapter blob;
- NVIDIA's allocation-private formats;
- emulation of the non-RM escape groups;
- full WDDM 2.x DDIs: devices, contexts, HW queues, paging buffers / GpuMmu page-table
  updates, residency, fences, present, and display or an IddCx display path.

The hard part is reconciling VidMm owning the GPU VA space (per kayfabe's
`THE_WINDOWS_AXIS.md` §2) with the host RM owning the real one.

| | |
|---|---|
| gets | DX9–12, Vulkan, OpenGL, CUDA, NVENC/NVDEC, NvAPI, DLSS. NVIDIA's own compilers, so best performance and compatibility |
| feasible? | Plausible, unproven. No published prior art. GPU-PV shows the UMDs tolerate a remote KMD, but in that case the far side is NVIDIA's own nvlddmkm |
| effort | Very large: person-years, and every NVIDIA branch moves the private blobs. Treat as research until traces exist |
| anti-cheat | KMD needs attestation signing. VM detection by kernel anti-cheats (Vanguard, some EAC/BattlEye titles) blocks it regardless of route |

### Route A-compute: CUDA/NVML/NVENC on a non-WDDM "TCC-shaped" driver (phase 1)

A plain KMDF driver creates `\\.\NvAdminDevice` and `\\.\UVMLiteController`. It answers
the `0x08DE0008` envelope by translating group 5 into Linux RM ioctls for
`virtio-nvgpu`. There is no dxgkrnl, VidMm or TDR involvement.

| | |
|---|---|
| gets | CUDA, NVML/nvidia-smi, NVENC/NVDEC with CUDA surfaces. No graphics |
| feasible? | Likely, if `nvcuda64` accepts a TCC-mode GeForce/Blackwell adapter. Our host backend reports the driver model and GPU info, so this is ours to answer. Unknown: client-side product gating (GeForce cannot select TCC on real hardware) and how CUDA discovers adapters without dxgkrnl. Both are decided in one afternoon on a real machine |
| effort | Moderate. One KMDF driver, an op table, and the existing backend. Small because the RM semantics already work for the Linux guest |
| anti-cheat | Not relevant (compute) |

### Route B: open stack, NVK on RM + DXVK/vkd3d-proton on a Conduit WDDM KMD

The KMD's escape **is our own ABI**: Linux RM ioctls passed through unchanged to
`virtio-nvgpu`, exactly like the Linux guest driver. There is no NVIDIA-private format
anywhere.

The guest-side pieces:
- Mesa NVK built for Windows, using the `nvkmd` RM backend that X547/nvidia-haiku built
  (NVK + Zink running on NVIDIA's open RM under Haiku);
- Mesa's win32 WSI;
- DXVK for D3D9–11 and vkd3d-proton for D3D12, as in Helios;
- the WDDM KMD skeleton and present path, which can follow anonymix007's
  virtio-drivers-windows-rs (a Rust WDDM 2.0 render+display virtio-gpu KMD) and Helios
  (a Rust WDDM render+display adapter that owns the virtio-gpu scanout).

| | |
|---|---|
| gets | Vulkan (NVK), D3D9–12 (via translation), OpenGL (Zink). **No CUDA, no NVENC** (NVK has no encode), no DLSS/Reflex/NvAPI |
| feasible? | Yes. Every piece exists separately: NVK-on-RM (Haiku), DXVK/vkd3d on a virtio WDDM driver (Helios renders the desktop and runs 3DMark Steel Nomad Vulkan), and RM forwarding (our backend). Nobody has combined them |
| effort | Large but bounded and open: an NVK Windows port, the WDDM KMD with the present path, and RM over escape. No version lock on NVIDIA private blobs. Only the host RM ABI moves, and our backend already tracks it |
| anti-cheat | Same VM/signing issues. DXVK itself is tolerated by most games |

### Route C: Venus API remoting (Helios as is)

Already working for Vulkan, DX11 and DX12 (DX12 has open frame-order bugs). It
contradicts Conduit's design (`ARCHITECTURE.md`, "Why not Venus?"). It is useful as a
baseline and as code to borrow the KMD from, not as the target.

### Route D: run NVIDIA's real nvlddmkm against an emulated GPU (kayfabe)

Out of scope for Conduit. It requires an emulated GPU/GSP below the KMD. Kayfabe's own
analysis lists WDDM page-table ownership, the 2 s TDR and signed-INF DEV-ID gating as open
walls.

---

## 5. Verdict

| route | feasible | effort | DX11/12 | Vulkan | CUDA | NVENC | notes |
|---|---|---|---|---|---|---|---|
| **A-compute** (TCC-shaped, NvAdminDevice) | likely; 1 gating unknown | moderate | – | – | **yes** | yes (CUDA surfaces) | **phase 1** |
| **B** NVK-RM + DXVK/vkd3d | yes | large, open | via translation | NVK | – | – | phase 2 graphics |
| **A** NVIDIA UMDs on WDDM | plausible, unproven | very large, version-locked | native | native | yes | yes | research track; needs traces |
| C Venus (Helios) | works today | – | via translation | forwarded | – | – | baseline only |

---

## 6. Phase 1 plan

### Static work (no hardware)

1. Decode the full group-5 op table. Read the escape and IOCTL dispatch in `nvlddmkm.sys`
   (610.88): op → NVOS shape → Linux `NV_ESC_RM_*`. Record the table as data.
2. List which group-5 ops and RM controls `nvcuda64`/`nvml`/`nvcuvid64` (610.88) use, and
   check them against what `virtio-nvgpu` already allows.
3. Write the translation layer in the backend's terms. The Windows client already sends
   inline alloc parameters and pointer-based control parameters. Handles map 1:1, and the
   NVOS shapes match the Linux ones.

### Dynamic work (needs a real Windows 11 + NVIDIA GPU machine, preferably a Blackwell GeForce, driver **610.88**)

4. Trace `D3DKMTEscape`/`QueryAdapterInfo`/`CreateAllocation*` and `DeviceIoControl`
   (NvAdminDevice, UVMLiteController) with a user-mode hook DLL we write ourselves,
   alongside a DxgKrnl ETW trace. Workloads:
   - `cuInit` → `cuCtxCreate` → kernel launch → `cuMemAllocManaged`;
   - an `nvidia-smi -q` run;
   - an NVENC session with CUDA input;
   - D3D11, D3D12 and Vulkan triangles (for route A sizing).
5. Capture the `UMDRIVERPRIVATE` blob and the D3D info blob. Diff them across two GPUs
   and two boots to separate constants from per-boot values (handles).
6. Settle the identity questions:
   - Does any UMD verify the KMD's file or signer?
   - What does CUDA do with the `nvlddmkm` service key?
   - What is BCrypt used for?
   - Does `nvcuda64` take the NvAdminDevice path when the driver model reads as TCC?
     One experiment: an MCDM/TCC-capable datacenter card, or report TCC from our
     prototype.
   - Record `QUERY_GPUMMU_CAPS` and segment layout (kayfabe's measurement #1).
7. Go/no-go. If CUDA runs over the NvAdminDevice path, build the KMDF driver. If it
   insists on WDDM, the fallback is a render-only (MCDM-style) WDDM KMD that supports
   escapes, allocations and paging queues, with no display.

What genuinely needs the real machine: steps 4–6. That covers the blob contents, the
non-RM escapes actually issued, the driver-model gating, and the KMD identity checks.
Everything else can continue statically.
