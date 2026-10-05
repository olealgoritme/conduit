# A thinner GPU path for Windows guests

Question: Linux guests run NVIDIA's own driver and Conduit forwards its RM calls to the
host, which is one thin layer. Windows guests today go D3D → DXVK/vkd3d-proton → Mesa Venus
→ virtio → host Vulkan, which is several layers and clearly slower. Can a Windows guest use
NVIDIA's own Windows driver commands the way Linux guests do, or get close to that?

Written 2026-10-05 from two read-only research passes (public sources and NVIDIA's open
`open-gpu-kernel-modules` at tag 615.71.09). Nothing was run on a Windows machine with an
NVIDIA GPU, and no NVIDIA binaries were inspected. Web pages were read through a summarising
tool; quotes marked *(not byte-checked)* were not re-read as raw text. **Verified** means
read in source or primary documentation; **inferred** means reasoning, not shown.

## Short answer

- Forwarding NVIDIA's **own Windows user-mode drivers** (`nvwgf2umx.dll`, `nvcuda.dll`, ...)
  to a Linux host is not realistic. Nothing on a Linux host answers NVIDIA's Windows kernel
  dialect, the interface is closed and release-locked, and the licence forbids what it would
  take (see below).
- The **resource-manager (RM) API itself is shared** between NVIDIA's Windows and Linux
  ports, so the host backend's RM forwarding would carry over to a Windows client that speaks
  RM. NVIDIA's own Windows driver is not that client.
- The realistic thin path is **Route C**: an open Vulkan stack in the guest (NVK + DXVK /
  vkd3d-proton) that speaks RM through a Conduit-written Windows kernel driver. It removes
  the Venus serialization and the host-side replay. It is a large project with real unknowns
  (below), not a switch.
- Until then the practical choices are: keep improving the Venus path, a thin CUDA-only shim
  for Windows (`nvcuda.dll` interposer), or run Windows games with Proton on the host or in a
  Linux guest.

## Layers today and with Route C

```text
Today (Venus):
 D3D app → DXVK / vkd3d-proton → Mesa Venus ICD (serializes Vulkan calls)
         → Helios KMD → virtio → conduit-backend → conduit-venus
         (virglrenderer replays the calls) → host NVIDIA Vulkan driver → GPU

Route C (NVK on RM):
 D3D app → DXVK / vkd3d-proton → NVK (Vulkan driver in the guest builds the GPU
         command buffers itself) → RM calls via a Conduit Windows KMD → virtio
         → conduit-backend (the existing RM forwarding and allowlist)
         → host NVIDIA RM → GPU
```

What changes: the D3D → Vulkan step (DXVK) stays in both. The Venus encode, the virtio
command stream of serialized Vulkan, the decode and replay on the host, and the host-side
Vulkan driver all go away. The GPU command buffers are written in the guest and run on the
GPU through mapped channels, which is how Linux guests already work. It is therefore thinner
in the sense that matters (no serialization, no replay), though not in the number of guest
components.

What it costs (see "Unknowns"): the proprietary NVIDIA userspace driver is replaced by the
open one (NVK, with its own shader compiler), so raw driver quality and feature coverage
become the question; and the guest kernel driver, memory mapping, events and display are new
Windows work.

## What is known about NVIDIA's Windows stack

### Verified

- User mode reaches `nvlddmkm.sys` mainly through `D3DKMTEscape`
  (`D3DKMT_ESCAPE_DRIVERPRIVATE`) into `DxgkDdiEscape`, behind a custom NVIDIA header.
  Tarakanov (ZeroNights 2015) documents `NVIDIA_PRIVATE_DRIVER_DATA` with the magic
  `0x4e564441` ("NVDA"), a size, an `escapeAction` (`'NVDX'`, `'NVGL'`, `'NVCP'`, ...) and a
  function id; Project Zero (2017) describes about 400 escapes with a different, dated
  header layout. Both are old: today's layout and count are not public.
- Other kernel entry points bypass D3DKMT: `\\.\NvAdminDevice` (NVAPI IOCTLs),
  `\\.\UVMLiteController/Process*` (unified memory), `\\.\NvStreamKms`. A D3DKMT-level
  forwarder would miss them.
- No public NVIDIA document, nouveau/envytools note, Wine or ReactOS source describes the
  Windows escape payloads. `NVIDIA/nvapi` (MIT) is an API SDK, not a kernel ABI.
- The RM core is shared across OS ports. In `open-gpu-kernel-modules`:
  `src/nvidia/src/kernel/rmapi/entry_points.c` dispatches the NVOS21/54/64/33 calls with no
  OS conditionals; the Linux escape layer (`src/nvidia/arch/nvalloc/unix/`) is about 28k
  lines against about 457k in the shared RM core; `nvos.h` defines `FILE_DEVICE_NV` and
  comments NVOS00 as an "NT ioctl data structure"; control categories reserve a Windows
  namespace (`NV0000_CTRL_OS_WINDOWS = 0x3F`). An NVIDIA maintainer wrote that the code is
  "compiled for a wide variety of platforms - from Windows to custom NV-internal ISAs"
  (open-gpu-kernel-modules discussion #157) *(not byte-checked)*. R535 data-center release
  notes ship Linux 535.129.03 and Windows 537.70 as one branch.
- Under WDDM the RM does not own the GPU address space or paging: RM has
  `PDB_PROP_GPU_EXTERNAL_HEAP_CONTROL`, `NVOS32_ALLOC_FLAGS_EXTERNALLY_MANAGED`, and
  controls "only available on Windows and MODS ... kernel clients only"
  (`NV0080_CTRL_DMA_UPDATE_PDE_2`). The host backend is an unprivileged RM client, so those
  can never be forwarded.
- **WSL2 is the closest existing system.** NVIDIA's Linux `libcuda.so` for WSL goes
  `libdxcore` → `/dev/dxg` ioctls (`LX_DXESCAPE`, `LX_DXCREATEALLOCATION`,
  `LX_DXSUBMITCOMMAND`, ...) → VMBus → the *Windows host's* dxgkrnl → `nvlddmkm`.
  Microsoft's GPU paravirtualization rules say the UMD "can't pass any pointers in the
  private data" or any handles, and messages are capped at 128 KB, so NVIDIA's escape
  traffic there is pointer-free blobs (the layer is an opaque-blob transport). Hyper-V
  GPU-PV for Windows guests works the same way, with a Windows host.
- **NVIDIA vGPU** (the official equivalent) runs NVIDIA's full proprietary driver inside the
  guest on a partitioned virtual device, with a paravirtual channel to a host manager; the
  wire protocol is not public. It needs a licensed datacenter or pro GPU, not GeForce.
  (`vgpu_unlock` only works on pre-GSP cards.)
- Licence (NVIDIA GeForce driver licence, read as raw text in the research pass): no reverse
  engineering (2.2), no modification (2.3), no distribution (2.7; the 1.1(d) exception is
  for OSI-licensed OS kernels, which Windows is not), and GeForce software is not licensed
  for datacenter deployment (2.8). **This is a reading, not legal advice.** A first-boot step
  that installs the user's own driver is the defensible shape; shipping NVIDIA binaries in an
  image is not.

### Inferred, not verified

- The Windows user-mode driver most likely carries RM alloc/control/free (the shared
  `ctrl*.h` and `cl*.h` types) inside those escapes. No source says so.
- The escape wrapper and function ids do not match Linux's `NV_ESC_RM_*` ioctls.
- WSL2's lack of full managed memory is probably because the UVM device nodes are not
  tunnelled.

## Routes

| Route | Verdict |
|---|---|
| **A.** NVIDIA's unmodified Windows user-mode driver over a guest WDDM kernel driver that forwards to the host RM | Very hard. The guest driver would have to answer the closed, versioned, ~400-escape ABI of `nvlddmkm` plus the WDDM contract. Nobody has done it publicly. Licence problems. |
| **B.** NVIDIA's Windows CUDA libraries on a minimal non-WDDM guest driver | Unknown, probably hard. Depends on how `nvcuda.dll` finds and validates the driver (unknowns 6 and 7 below). |
| **C.** Open stack on RM: NVK + DXVK/vkd3d-proton with a Conduit Windows kernel driver | Realistic but large. The RM API is shared, so the host backend's tables and allowlist are reusable. New: guest KMD transport, MDL mapping, events, display, an NVK backend on RM. |
| **D.** Thin CUDA-only shim (`nvcuda.dll` interposer forwarding the documented CUDA API) | Avoids the escape ABI entirely, as rCUDA/GVirtuS do. CUDA only. |
| **E.** Keep Venus, improve it | Smaller gains, lowest risk. UTM's Neptune reports that moving D3D translation host-side beat Venus. |
| **F.** Proton on the host, or Wine/Proton in a Linux guest | The thin path for games today. |

Effort guesses from the research pass (estimates, not from a source): route A or B for
CUDA only 12–18 person-months, for D3D11/12 plus NVAPI plus CUDA 30–60 plus about one per
driver release, with a high chance of not converging.

## Route C: what it needs

Reusable as is: the host backend's RM forwarding (`host/backend/device/src/nvidia/`: the
allowlist, handle and fd translation, region 1 / region 2 windows, UVM tables, events,
fences), the Venus-independent parts of the protocol, and Helios's WDDM miniport skeleton.

New, per the research pass:

- A Windows kernel driver (KMD) that exposes the RM client interface to a user-mode Vulkan
  driver and forwards it over the virtio channel: pinned memory as page runs (MDL instead of
  Linux pins), mapping through the host-visible window (`MmMapIoSpace` / MDL into the user
  process instead of `mmap`), events (a KEVENT signalled from `EventReady` instead of an fd),
  and the WDDM display contract (VidPn, which Helios already implements).
- An NVK backend that talks RM. NVK normally uses the nouveau kernel interface; it would
  need a new kernel-driver layer speaking the RM calls the host already forwards. NVK also
  brings its own shader compiler.
- DXVK and vkd3d-proton on top, as today (Helios already builds them).
- Windows-specific RM differences the Linux forwarder does not meet: under WDDM, dxgkrnl
  owns GPU VA and paging, so a guest client must work with RM-managed VA; the kernel-only
  controls are refused by design; WDDM monitored fences and scheduling versus RM semaphores
  and channels; Windows UVM is not the `/dev/nvidia-uvm` ABI (so no managed memory at first).

## Unknowns

For a route that uses NVIDIA's own Windows driver (A, B) only a trace of the real thing
answers these:

1. Whether the user-mode driver's RM operations travel as D3DKMT escapes, as
   DeviceIoControl on `\\.\NvAdminDevice`, or both.
2. Whether escape payloads contain verbatim NVOS21/54/64/33 structs, and with what wrapper.
3. Which classes and controls the user-mode driver issues versus the kernel driver internally.
4. How user mode learns GPU VA, residency, allocation and memory handles.
5. How doorbells, USERD and fences work under WDDM.
6. How CUDA initialises: via the WDDM adapter, or only via `\\.\NvAdminDevice`.
7. What the user-mode driver checks about the kernel driver (version, name, signature).
8. Whether the escape ABI is stable across driver releases.

For route C (not answered by the research pass, to be checked before committing to it):

- How much of NVK would have to change to run on RM instead of nouveau, and whether the
  NVIDIA kernel interfaces NVK needs (channels, VA management, syncobjs) exist in RM for an
  unprivileged client.
- NVK's feature coverage and performance on this GPU generation. *Not verified: whether the
  Mesa version in use supports the host GPU (RTX 5090, Blackwell).* The proprietary driver's
  optimisation is given up in exchange for a thinner path.
- Whether the guest can hold the memory-mapping and GPU-VA model the RM expects for an
  unprivileged client under dxgkrnl.

## First experiments

- **Trace NVIDIA's Windows driver** (answers unknowns 1 and 2 in about half a day). On any
  Windows install with an NVIDIA GPU on the WDDM driver (a passthrough VM, or a second GPU),
  hook `NtGdiDdDDIEscape`, `D3DKMTEscape` and `NtDeviceIoControlFile`, log the code, size and
  first 256 bytes, running `nvidia-smi -q`, then a minimal CUDA driver-API program, then a
  D3D11 clear and present. Look for a call with `hRoot = hObjectParent = hObjectNew = 0` and
  `hClass = 0x41` at offset 12 (the NVOS64 layout), followed by `0x80` (`NV01_DEVICE_0`) and
  `0x2080` (`NV20_SUBDEVICE_0`) and the controls `0x201` and `0x214`; compare with
  `conduit trace` of the same workload on Linux. A positive result means the Linux backend's
  tables apply directly; a negative one closes routes A and B. Mind the licence: do not
  disassemble the kernel driver.
- **Route C feasibility spike** (not yet done): build NVK for Windows with a stub RM backend
  and check what it asks the kernel for during `vkCreateDevice` and a clear-and-present.

## Sources

- Microsoft: GPU paravirtualization, https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization
- Microsoft WSL2 kernel `d3dkmthk.h` and `microsoft/libdxg`
- NVIDIA WSL user guide, https://docs.nvidia.com/cuda/wsl-user-guide/index.html
- Tarakanov, "Windows NVIDIA driver", ZeroNights 2015, https://repo.zenk-security.com/Conferences/ZeroNights/11-Tarakanov.pdf
- Project Zero, "Attacking the Windows NVIDIA driver", https://projectzero.google/2017/02/attacking-windows-nvidia-driver.html
- NVIDIA/open-gpu-kernel-modules (`nvos.h`, `rmapi/entry_points.c`, `arch/nvalloc/unix/`), discussions #157, #312, #638
- NVIDIA GeForce driver licence, https://www.nvidia.com/en-us/drivers/geforce-license/
- Zhi Wang's upstream vGPU RFC (nouveau list) and `DualCoder/vgpu_unlock`
- `docs/research/windows-guest-prior-art.md` (other Windows-guest GPU projects)
- This repo: `docs/ARCHITECTURE.md`, `docs/ROADMAP.md` ("Windows guests beyond Venus"),
  `host/backend/device/src/nvidia/`, `host/backend/protocol/src/messages.rs`
