# GPU support: RTX 20 / 30 / 40 / 50

What Conduit needs to work on every GeForce RTX generation, for Linux guests
and Windows 11 guests (NVK-on-RM, Venus fallback). The reference machine is an
RTX 5090 (Blackwell, GB202). One Linux-guest run exists on an RTX 4070 SUPER
(Ada, AD104; [measured below](#measured-rtx-4070-super-ada-ad104)). Nothing
has been run on Turing or Ampere, and every other verdict is read from the
code and from upstream, not measured. Guesses are marked **(guess)**, things
to check on hardware **(to verify)**.

Related: [SECOND-MACHINE.md](SECOND-MACHINE.md) (setting up the second
machine), [NVK-ROADMAP.md](NVK-ROADMAP.md), [HANDOFF.md](HANDOFF.md).

## Generations

| | RTX 20 | RTX 30 | RTX 40 | RTX 50 |
|---|---|---|---|---|
| Architecture | Turing | Ampere | Ada Lovelace | Blackwell |
| Chips | TU102/104/106, TU116/117 | GA102/103/104/106/107 | AD102/103/104/106/107 (the RTX 4070 is AD104, PCI 0x2786, or AD103, 0x2709) | GB202/203/205/206/207 |
| RM architecture (`NV2080_CTRL_CMD_MC_GET_ARCH_INFO`) | 0x160 | 0x170 | 0x190 | 0x1B0 |
| `chipset` as NVK-on-RM computes it (arch \| impl) | 0x16x | 0x17x | 0x19x (AD104 = 0x194) | 0x1Bx |
| Shader model | SM75 | SM86 | SM89 | SM120 |
| NAK latency tables | SM75 | SM80 | SM80 | SM120 |
| 3D class | TURING_A 0xc597 | AMPERE_B 0xc797 | ADA_A 0xc997 | BLACKWELL_B 0xce97 (GB20x; BLACKWELL_A 0xcd97 is GB10x) |
| Compute class | TURING_COMPUTE_A 0xc5c0 | AMPERE_COMPUTE_B 0xc7c0 | ADA_COMPUTE_A 0xc9c0 | BLACKWELL_COMPUTE_B 0xcec0 |
| GPFIFO class | TURING_CHANNEL_GPFIFO_A 0xc46f | AMPERE_CHANNEL_GPFIFO_A 0xc56f | AMPERE_CHANNEL_GPFIFO_A 0xc56f | BLACKWELL_CHANNEL_GPFIFO_B 0xca6f |
| Copy class | TURING_DMA_COPY_A 0xc5b5 | AMPERE_DMA_COPY_B 0xc7b5 | AMPERE_DMA_COPY_B 0xc7b5 | BLACKWELL_DMA_COPY_B 0xcab5 |
| Usermode (doorbell) class | TURING_USERMODE_A 0xc461 | AMPERE_USERMODE_A 0xc561 | AMPERE_USERMODE_A 0xc561 | BLACKWELL_USERMODE_A 0xc761 |
| NVDEC class | 0xc4b0 | 0xc7b0 | 0xc9b0 | 0xcfb0 |
| Display core class | 0xc57d | 0xc67d | 0xc77d | 0xca7d |
| Resizable BAR (GeForce) | no: BAR1 is 256 MiB | official since 2021 with a VBIOS + board BIOS update (3060 shipped with it), otherwise 256 MiB | yes (4070 12 GB: 16 GiB BAR1 **(to verify; 4090 24 GB shows 32 GiB)**) | yes (5090: 32 GiB) |
| Block-linear GOBs | desktop (TuringColor2D) only | desktop only | desktop only | desktop + Blackwell 8-bit / 16-bit |
| DRM modifier sector layout `s` for 1-/2-byte formats | 1 | 1 | 1 | 2 / 3 |

The per-chip class lists are from the open modules 610.57.04,
`src/nvidia/generated/g_gpu_class_list.c` (`gpuGetEngClassDescriptorList_<chip>`),
values from `g_allclasses.h`. AMPERE_A 0xc697 is GA100 only, BLACKWELL_A 0xcd97
is GB100 only; older GPFIFO/usermode classes are listed too on every chip.
NVK-on-RM does not hardcode them: `nvkmd_rm_pdev.c` (Mesa patch 0003 and
later) reads `NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2` and takes the highest class
in a range per engine (3D 0xa097-0xce97, compute 0xa0c0-0xcec0, copy
0xa0b5-0xcab5, GPFIFO 0xc36f-0xca6f), usermode in the order Blackwell, Hopper,
Ampere, Turing, Volta; it accepts RM architectures GV1, TU1, GA1, GH1, AD1,
GB1, GB2. On Ada it should pick 0xc997 / 0xc9c0 / 0xc7b5 / 0xc56f / 0xc561
(the non-Hopper usermode parameters path; Hopper+ get `bBar1Mapping=1`).

## Matrix

Status: **works** (measured), **expected** (code and upstream say it should,
not run), **needs work** (a known code change is needed), **blocked** (not
possible with the current stack).

### Host

| Requirement | RTX 20 | RTX 30 | RTX 40 | RTX 50 | Evidence |
|---|---|---|---|---|---|
| NVIDIA open kernel modules | expected | expected | expected | works | Open modules support Turing and later only (README of [open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules)); Blackwell requires them. `conduit doctor` warns (not fails) on the closed modules for a release that has tables, and says Conduit is untested with them: `module_verdict` in `cli/src/doctor.rs`. Nothing in the backend checks for the open modules (it reads `/proc/driver/nvidia` and per-release tables). A 565.77 closed-module host (RTX 4070 Super) is the first closed-module host this was pointed at, and it is **not run yet** (to verify) |
| Driver release with RM ABI tables (535.129.03, 565.77, 580.178.04, 595.71.05, 595.104.02, 610.57.04, 615.71.09) | expected | expected | expected | works | Tables are **per driver release, not per GPU**: RM's ioctl/control layouts do not depend on the chip. `host/backend/gen/src/osdesc/mod.rs:185` picks the nearest older release; doctor checks the version against the supported list (`cli/src/doctor.rs`; `565.77` and `565.77.00` are one release). 565.77 was generated from open-gpu-kernel-modules tag `565.77`, which differs from `565.57.01` in no ioctl number or parameter struct (an I2C SMBus fix, a vGPU notifier enum, NVKMS null checks); gVisor's nvproxy has no 565.77, so its escape profile is the generated `versions/v565_57_01.rs`, which 565.77 selects by range. **Not exercised against a live 565.77 driver.** Parameter sizes differ per release (the `GF100_CHANNEL_GPFIFO` allocation is 360 bytes in the 565.77 table, 368 in 580.178.04 and 615.71.09, 376 in 595) and the backend refuses a block of the wrong size, while NVK-on-RM's patch 0002 asserts 376: whether NVK-on-RM allocates a channel on a 565.77 host is **to verify** on the host | A newer driver needs new tables (`host/backend/gen`, `.github/workflows/abi.yml`) |
| Turing still supported by the driver branch | expected | | | | 580 is the last branch for Maxwell/Pascal/Volta; 590+ supports Turing (GTX 16 / RTX 20) and later, no Linux EOL for Turing announced ([Arch news on 590](https://archlinux.org/news/nvidia-590-driver-drops-pascal-support-main-packages-switch-to-open-kernel-modules/)). 615 ships open modules only |
| BAR1 / RM window sizing | needs check: 256 MiB BAR1 | expected (ReBAR on) / needs check (off) | expected | works | `gpu.window_mib` defaults to the host GPU's BAR1 (`cli/src/config.rs:7`, `cli/src/lvrun.rs:639`); the KMD handles small windows (`guest/windows/kmd_logic/src/rm_window.rs:158`, test row "BAR1 256 MiB" at :814). Functionally fine. NVK-on-RM's host-visible VRAM heap is `MIN2(bar1, vram - 64K)` (Mesa 0045:158-163; 0022 had a 256 MiB cap), so a 256 MiB BAR1 gives a 256 MiB heap; past it allocations fall back to system memory (0025, 0045). A real 595.71.05 bug hit a 5070 with ReBAR off ([open-gpu-kernel-modules#1132](https://github.com/NVIDIA/open-gpu-kernel-modules/issues/1132)) **(guess: noticeable DXVK perf drop without ReBAR)** |
| Guest shared-memory BAR (64 GiB, QEMU) | expected | expected | expected | works | Not GPU-dependent: `cli/src/qemu.rs:147`, `docs/examples/win11.xml:71` |
| Venus host renderer (NVIDIA Vulkan) | expected | expected | expected | works | Conduit's virglrenderer patches use `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`, `VK_EXT_external_memory_host`, exposed by NVIDIA's Linux driver on every Turing+ GPU (dma-buf and modifiers since 515.43.04, external_memory_host since 440.66.17); they are hidden without `nvidia-drm modeset=1` and render-node access. No per-generation differences documented **(check `vulkaninfo` on the 4070)** |

### Escape sizes, per driver release

The NVIDIA userspace in a Linux guest is the host's, so the sizes of its
`NV_ESC_*` parameter blocks are the host release's, not the guest module's.
RM's `rm_ioctl` takes an escape at the sizes of the structs it knows and
answers anything else `NV_ERR_INVALID_ARGUMENT`; the guest module and the
backend apply the same list. It is generated (`host/backend/gen/nvabi_gen.py`,
`IoctlEntry` in `abi::versions`), the backend checks every call against it and
sends it to the guest in the last section of GET_SYS_FILES, and
`nvgpu_ioctl_fd` answers `EINVAL` to a size outside it. A backend that sends no
list leaves the check to the backend. The rows are in
`host/backend/gen/fixtures/escape_sizes.tsv`, which a test keeps equal to the
tables and `guest/linux/test/escape_test.c` applies. The sizes that differ:

| Escape | 535.129.03 | 565.57.01 (also 565.77) | 580.178.04 | 595.71.05 | 610.57.04 (also 615.71.09) |
|---|---|---|---|---|---|
| `RM_ALLOC` (0x2b) | 32 or 48 | 32 or 48 | 32 or 48 | 32 or 48 | 32 or 48 |
| `RM_MAP_MEMORY_DMA` (0x57) | 56 | 56 | 64 | 64 | 64 |
| `RM_UNMAP_MEMORY_DMA` (0x58) | 40 | 48 | 48 | 48 | 48 |
| `EXPORT_TO_DMABUF_FD` (0xd9) | 2600 | 2600 | 2608 | 2608 | 2608 |
| `WAIT_OPEN_COMPLETE` (0xda) | not taken | 8 | 8 | 8 | 8 |

Every other escape has one size in all five tables (`RM_CONTROL` 32,
`RM_FREE` 16, `RM_ALLOC_MEMORY` 56, `RM_VID_HEAP_CONTROL` 184,
`GET_EVENT_DATA` 16, ...), and `CARD_INFO`, `ATTACH_GPUS_TO_FD` and `NUMA_INFO`
take any length. `RM_ALLOC` is NVOS21 (32 bytes) or NVOS64 (48): the first five
fields agree, then NVOS21 has `paramsSize` at 24 and status at 28, NVOS64 has
`pRightsRequested` at 24, `paramsSize` at 32 and status at 40. 565.77's
userspace sends the 32-byte form for `NV01_ROOT`, so `nvidia-smi` in a guest
needs it. The 565.77 sizes were checked against the 565.77 open modules'
headers (NVOS21 32, NVOS64 48, NVOS46 56, NVOS47 48, NVOS54 32). **Not run
against a live 565.77 guest after this change.**

### Guest RM client (librmclient) and Linux guest

| Requirement | RTX 20 | RTX 30 | RTX 40 | RTX 50 | Evidence |
|---|---|---|---|---|---|
| librmclient: no hardcoded GPU classes | expected | expected | expected | works | Library code only names FERMI_VASPACE_A / root/device classes (`guest/rmclient/src/nv_ioctl_defs.h:272`); classes come from the caller (NVK) |
| crm_smoke usermode probe | expected | expected | expected | works | Tries 0xc761, 0xc661, 0xc561, 0xc461 in turn (`guest/rmclient/tests/crm_smoke.c:165`) |
| Linux guest kernel module (conduit_gpu) | expected | expected | expected | works | Page-kind/sector-layout dev_info words are passed through from the host's node, not hardcoded (`guest/linux/conduit_gpu.c`, `dri_dev.dev_info`). The `GET_DEV_INFO` struct is per release (20 bytes on 535, 32 on 565-570, 36 from 575): the backend asks and decodes it in the host's own layout (`abi::devinfo`, exact release only: a release without a table is not asked and gets no render node) and the module answers in the layout of the caller's ioctl size (`guest/linux/nvgpu_devinfo.h`), both generated by `host/backend/gen/devinfo_extract.py`; allocation-size table covers Turing..Blackwell classes (`guest/linux/gen/nvgpu_rmalloc_classes.h`) |
| Linux guest NVIDIA userspace (CUDA, Vulkan via the passed-through stack) | expected | expected | expected | works | Same RM, same driver release as the host |

### NVK / NAK (Mesa base 70c4c018, `guest/nvk-rm/build-windows.sh:50`)

| Requirement | RTX 20 | RTX 30 | RTX 40 | RTX 50 | Evidence |
|---|---|---|---|---|---|
| Upstream NVK hardware support | expected (conformant) | expected (conformant) | expected (conformant) | works | `docs/drivers/nvk.rst:9-17` at our base: Kepler through Ada plus consumer Blackwell, conformant Vulkan 1.4; `nvk_is_conformant` (`nvk_physical_device.c:100-116`) covers KEPLER_A..ADA_A and BLACKWELL_B; Turing+ get API 1.4 ([NVK docs](https://docs.mesa3d.org/drivers/nvk.html)) |
| NAK shader compiler | expected | expected | expected | works | One backend (`sm70.rs`, `sm70_encode.rs`) for all four; SM120 adds encodings and uniform ALU forms gated on SM>=100/120, SM89 vs SM86 differs only in f16 conversions. SM75/86/89 are older and better trodden than SM120. `sm` and shared-memory limits come from `chipset` (`nouveau_device_limits.c`) |
| NVK on RM: class and device info from RM | expected | expected | expected | works | Classes from the RM class list, chipset from `MC_GET_ARCH_INFO` (above); the only Blackwell gates are compression (0028) and a GB202 GP_GET comment (`nvkmd_rm_ctx.c:95`) |
| NVK on RM: GPC/TPC counts | **to verify** | **to verify** | **to verify** | works | `gpc_count` from `GR_INFO_INDEX_LITTER_NUM_GPCS`, `tpc_count` from `SHADER_PIPE_SUB_COUNT` (`nvkmd_rm_pdev.c:264-276`): "litter" values are per-design constants, not necessarily the enabled (floorswept) counts. Check the gpc/tpc log line (`nvkmd_rm_pdev.c:435`) against `nvidia-smi`/spec. If they are design maxima they over-size per-SM buffers, which wastes memory but is safe **(unverified)** |
| Per-draw patches (patches-common 0001-0007) | expected | expected | expected | works | Gated on `cls_eng3d >= TURING_A` (e.g. 0001 l.103); use Turing-era methods (NVC597) |
| Depth/stencil PTE kinds | **to verify** | **to verify** | **to verify** | works | GB20x uses only generic kinds (0x06/0x08, `nil/image.rs:745-765`); Turing-Ada use dedicated depth kinds (Z16, Z24S8, S8Z24, ZF32_X24S8, S8; `tu102_choose_pte_kind`, `image.rs:767-826`). NVK-on-RM allocates generic, `_COMPR_NONE` memory (`nvkmd_rm_mem.c:200-219`) and forces the kind per mapping with `OS46 _PAGE_KIND_OVERRIDE` (`nvkmd_rm_va.c:370-380`). RM under GSP was never asked for a depth kind that way; if it refuses, binds fail; the fallback without `crm_map_dma2` maps with the generic kind, harmless on Blackwell but likely corrupt depth on older GPUs **(unverified)** |
| Compression (0028) | off, works uncompressed | off | off | works | `has_compression = cls_eng3d >= BLACKWELL_A` (0028 l.55, l.175; `nvkmd_rm_pdev.c:403-405`): pre-Blackwell images stay uncompressed (kind 0x06). Correct but slower. Pre-Blackwell compression keeps its state in comptaglines RM writes into the PTEs, allocated when `NVOS32_ATTR_COMPR` is not NONE (open modules `mem_mgr_tu102.c:249-260, 457-483`), so it needs a COMPR_ANY allocation, mapping through RM and the Turing-Ada depth kinds (0x01-0x05 and their compressible forms) that 0028 does not know: **needs work** (perf only) |
| ZCULL (0029) | expected **(to verify)** | expected **(to verify)** | expected **(to verify)** | works | Sizes come from `NV2080_CTRL_CMD_GR_GET_ZCULL_INFO`, not constants |
| Video decode (0035) | expected | expected | expected | works | NVDEC class taken from the class list, Turing 0xc4b0 .. Blackwell 0xcfb0 (0035 l.141) |
| Block-linear scanout / swapchain modifiers (32 bpp) | expected | expected | expected | works | The 32 bpp modifier is `BLOCK_LINEAR_2D(c=0,s=1,g=2,k=0x06,h)` on every Turing+ GPU, the same value the KMD expects (`guest/windows/protocol/src/foreign.rs:410`) |
| Block-linear 1-/2-byte shared formats (A8, R8, NV12, P010, R16, RG8) | **needs work** | **needs work** | **needs work** | works | NVK names the GOB from NIL's gob type (0041 l.269: TuringColor2D gives s=1). Pre-Blackwell every format uses the desktop GOB, so these arrive with s=1; the KMD accepts only the GB20x 8-bit/16-bit families for them (`guest/windows/kmd_logic/src/foreign_resource.rs:306`, `plane_modifier_ok` :404) and refuses the import. Effect: shell/video shared surfaces in those formats are refused on NVK (S6c); 32 bpp DWM, scanout and games are unaffected |

### Windows guest (Helios KMD, UMD, ICD)

| Requirement | RTX 20 | RTX 30 | RTX 40 | RTX 50 | Evidence |
|---|---|---|---|---|---|
| KMD RM forwarding (escape NVRM) | expected | expected | expected | works | Forwards opaque RM calls; no class ids in the forwarding path |
| KMD scanout / ForeignFlip / Blt (32 bpp) | expected | expected | expected | works | Validation via `Layout::validate`, rgb32 only for flip/copy (`foreign_flip.rs:239`, `foreign_copy.rs:286`), and the 32 bpp family is the same on all gens |
| KMD shared-surface import of 1-/2-byte formats | needs work | needs work | needs work | works | See the NVK row above; fix in kmd_logic + protocol + kmd_render (below) |
| RM window / host-visible heap | expected (256 MiB) | expected | expected | works | `rm_window.rs` handles 256 MiB windows |
| UMD (DXVK, vkd3d-proton), ICD, Zink | expected | expected | expected | works | GPU-agnostic above Vulkan; features follow what NVK reports per GPU |
| Venus fallback | expected | expected | expected | works | Host NVIDIA Vulkan driver, see host table |

### Verdict per generation

- **RTX 50 (Blackwell)**: works (the reference machine).
- **RTX 40 (Ada)**: a Linux guest on NVIDIA's own user-mode driver works
  (measured on an RTX 4070 SUPER, see below). Expected to work for
  DWM-on-NVK, games and scanout;
  1-/2-byte shared formats need the KMD fix; compression off (perf below
  the 5090 numbers, by more than the GPU difference **(guess)**). The best
  candidate after Blackwell: ReBAR, same driver releases, NVK conformant.
- **RTX 30 (Ampere)**: as Ada, plus ReBAR may be off (256 MiB BAR1) unless the
  board/VBIOS supports it.
- **RTX 20 (Turing)**: as Ampere, without ReBAR: BAR1 256 MiB, the
  host-visible VRAM heap is tiny; expect lower DXVK performance **(guess)**.
  Functionally expected to work.

## Measured: RTX 4070 SUPER (Ada, AD104)

One session, in a Linux guest on NVIDIA's own user-mode driver (the host's
files shared over virtiofs); NVK-on-RM and Windows were not run.

| | |
|---|---|
| Host | Ubuntu 24.04, **closed** kernel modules 565.77 (GSP firmware on), PCI `10de:2783`, 12 GiB, BAR1 256 MiB (Resizable BAR off), a monitor connected to the same GPU |
| Guest | Omarchy (Arch), kernel 7.2.5, Hyprland, `conduit attach`ed libvirt VM |
| Protections | safe mode (automatic for an untested driver): 2 GiB video-memory limit, 1 s bounds on blocking calls |
| Bring-up | staged: guest module held at boot; loaded by hand; then `nvidia-smi`, `vulkaninfo`; then the compositor on the Conduit GPU |

What ran, with the host kernel log free of `NVRM` and `Xid` lines throughout:

- The backend picks the exact 565.77 tables; the guest module loads and reports
  `1 GPU(s), driver 565.77`; the backend log shows `page kind 6/2, sector
  layout 1` for the render node.
- `nvidia-smi` lists the GPU. `vulkaninfo --summary` lists `NVIDIA GeForce RTX
  4070 SUPER`, `driverVersion 565.77.0.0`, `deviceID 0x2783`,
  `DRIVER_ID_NVIDIA_PROPRIETARY`, conformance 1.3.8.2.
- Hyprland in the guest renders on the Conduit GPU and `vkcube` runs on it; the
  owner used the desktop for a while without a glitch. This was not a soak
  test and not a benchmark.

Seen in the backend log, harmless so far: `SYS_PARAMS` answered with a
synthesized success when the host returns EBUSY; `UNMAP_MEMORY: no mapping for
pLinearAddress=...` at process exit; the 1 s clamp on `RM_IDLE_CHANNELS`
(safe mode working); `RM_CONTROL GPU_GET_PIDS` refused by design.

Not covered: CUDA, NVENC/NVDEC, NVK-on-RM (its structs are sized for 610.57.04,
so it does not enumerate a device on 565.77), Windows guests, games, long runs,
the open modules on this card, and the limit's behaviour under memory pressure.

Two things the run fixed that no table could have shown: the guest module
accepted only the 48-byte `RM_ALLOC` layout while this release's NVML sends the
32-byte one, and it read `GET_DEV_INFO` in the 36-byte layout of 575 and later.

## Code changes needed

| Change | Gens | Size | Where | Status |
|---|---|---|---|---|
| KMD: accept the desktop (s=1) family for 1-/2-byte planes on pre-Blackwell GPUs; family rule takes the GOB scheme as a parameter (`GobScheme`, `Layout::validate_with`, `validate_request_with`; defaults unchanged) | 20/30/40 | S (~250 lines incl. tests) | `guest/windows/protocol/src/foreign.rs`, `guest/windows/kmd_logic/src/foreign_resource.rs` | done on branch `feat/multi-gpu` (not on main; kmd_logic and protocol tests pass) |
| KMD: learn the GOB scheme (registry knob, or the host reports the GPU architecture through a capability bit) and pass it to validation | 20/30/40 | M; needs a WDK build | `guest/windows/kmd_render` (escape_foreign.rs, knob reading), host protocol | needs work |
| Pre-Blackwell compression: allocate comptags through RM, use compressible kinds | 20/30/40 | L; Mesa rebuild + measurement | Mesa patch 0028 | needs work (perf only) |
| RM ABI tables for any new driver release | all | M per release (generated) | `host/backend/gen`, `abi.yml` | as needed |
| `conduit doctor`: report GPU name and BAR1, warn on a BAR1 of 256 MiB or less | all | S | `cli/src/doctor.rs`, `cli/src/host.rs` | done on branch `feat/multi-gpu` (cli tests + clippy pass; on the 5090 it adds `GPU: NVIDIA GeForce RTX 5090 at 0000:01:00.0, BAR1 32.0 GB`) |
| Backend caps tests pin the Turing/Ampere/Ada/Blackwell classes; comment fixes (0xfe/gen 0 is Fermi-Volta; 6/2/1 is right on every Turing+ GPU) | all | XS | `host/backend/device/src/caps.rs`, `nvidia/rm_import.rs`, `nvidia/files.rs` | done on branch `feat/multi-gpu` |
| KMD wiring: pass a `GobScheme` at the three `validate_request` call sites (`kmd_render/src/virtio/foreign.rs:171`, `virtio/rm_client/sysmem.rs:824`, `virtio/rm_foreign.rs:133`), from `NV2080_CTRL_CMD_MC_GET_ARCH_INFO` through the KMD's RM client or the PCI id from CardInfo | 20/30/40 | M; WDK build | `guest/windows/kmd_render` | needs work |
| `conduit doctor`: 535.129.03 is in the supported list but `major < 580` failed it | all | XS | `cli/src/doctor.rs` (`module_verdict`) | done: a release with tables and a closed module or a branch older than 580 is a warning that it is untested; a release without tables still fails |
| Venus scanout modifier hardcoded `0x0300000000606010 \| h` (`host/backend/device/src/venus/scanout.rs:34-42`) and the NVIDIA Vulkan driver's block-height heuristic (:56-73), measured on the 5090 only | all | S | backend | right on every Turing+ desktop GPU; heuristic **to verify** (override: `CONDUIT_VENUS_SCANOUT_MODIFIER`) |
| crm_smoke: architecture names per family (TU10x/GA10x/AD10x/GB20x) | all | XS | `guest/rmclient/tests/crm_smoke.c:32` | cosmetic |

## Testing a new GPU

What `conduit doctor` should verify (today it checks the driver: kernel
module flavour and branch (a warning when untested), supported release, and
shows BAR1 and whether the GPU has a monitor; GPU name and architecture are not
yet shown):

1. A release with tables; open kernel modules, 580 or newer, are the tested setup and anything else is a warning (`conduit doctor`).
2. GPU and BAR1: `nvidia-smi -q | grep -A3 -i 'bar1'` and `lspci -vv -s <gpu> | grep -i 'resizable\|Region 1'`.
   ReBAR on means BAR1 ≈ VRAM rounded up; 256 MiB means it is off (BIOS:
   Above 4G decoding + Re-Size BAR support).
3. Host Vulkan: `vulkaninfo --summary`, and that the device lists
   `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`,
   `VK_EXT_external_memory_host`.

Then in a Linux guest (fastest to iterate), in this order:

1. `guest/rmclient`: `make check` (no GPU), then `make smoke` (`crm_smoke`:
   prints the RM architecture, allocates the usermode class, maps sysmem and
   VRAM), then `crm_event_smoke`, `crm_pin_smoke`, `crm_semsurf_smoke`,
   `crm_share_smoke`, `crm_scanout_smoke`.
2. `guest/nvk-rm/tests`: `vk_summary` (device, classes, heaps as NVK sees
   them; compare with the 5090's), `vk_compute_test`, `vk_offscreen_test`,
   `vk_bar_test` (host-visible VRAM size), `vk_coherence_test`,
   `vk_bl_readback` (block-linear layout), `vk_dmabuf_test`,
   `vk_scanout_present`, `vk_video_probe`.
3. Venus: `vkcube` with `conduit up <vm> --venus`.

Then Windows: `Verify-Helios.ps1 -RunSmokeTests`, a D3D11 app with
`Icd=nvk`, DWM on NVK last.

## First hour on the RTX 4070 (Ada, AD104)

0. Note which 4070 it is: `lspci -nn | grep -i nvidia` shows 0x2786 (AD104),
   0x2709 (AD103) or, for the SUPER, 0x2783 (AD104). All are Ada; nothing in
   Conduit depends on which.
1. **Host (10 min)**: install the open modules at a supported release
   (same as the 5090 host, e.g. 610.57.04 or 615.71.09); check ReBAR is on in
   the BIOS; `conduit doctor` all ok; `nvidia-smi -q` shows BAR1 ≈ 16 GiB
   **(to verify)**; `vulkaninfo --summary` lists the three extensions.
2. **Linux guest (20 min)**: `conduit up` a Linux guest; `make check smoke`
   in `guest/rmclient` (expect `Ada (AD100)` = arch 0x190 and usermode class
   0xc561); the other crm_* smokes; then `vk_summary`, `vk_compute_test`,
   `vk_offscreen_test`, `vk_bar_test`, `vk_bl_readback`,
   `vk_scanout_present` from `guest/nvk-rm/tests`, then a depth/stencil test
   with `NVK_DEBUG=vm` (risk 3). Save `vk_summary` output
   next to a 5090 run: any class or heap difference is the first lead.
3. **Venus (5 min)**: `vkcube` over Venus in the Linux guest.
4. **Windows (25 min)**: the same `HeliosSetup.exe`/driver as on the 5090;
   keep `Icd=venus` first, check the desktop; then a D3D11 app (Heaven
   1600x900 windowed and fullscreen) on NVK; then the opt-ins of
   [SECOND-MACHINE.md](SECOND-MACHINE.md) step 5 one at a time. Note the
   KMD refusal counters for shared surfaces (`refused_request`): non-zero
   with NV12/A8 surfaces is the known 1-/2-byte modifier gap, not a new bug.

Top risks for the 4070, most likely first:

1. **1-/2-byte shared surfaces refused** by the KMD (known, above): shell
   pieces, video and some browser surfaces fall back or fail on NVK. 32 bpp
   paths are unaffected. Workaround: keep those processes on Venus (the
   deny-list does) until the KMD fix ships.
2. **GPC/TPC counts from RM "litter" values** may not be the enabled
   counts on a cut-down chip like AD104 (`nvkmd_rm_pdev.c:264-276`); wrong
   counts mis-size per-SM buffers (shader local memory, etc.). Check NVK's
   gpc/tpc log line first. Classes and chipset come from RM and should be
   right (expected 3D 0xc997, chipset 0x194). **(unverified)**
3. **Depth/stencil PTE kinds through RM** (matrix above): Ada uses depth
   kinds the 5090 never needed. First check: a Vulkan app with D16, D24S8,
   D32S8 and S8 attachments under `NVK_DEBUG=vm` in the Linux guest (any
   game's depth buffer shows it too: corrupt depth or failed binds).
   **(unverified)**
4. **ReBAR off / smaller BAR1**: works but the host-visible heap shrinks;
   check BAR1 before measuring anything.
5. **Driver release mismatch**: a newer driver than the five with tables
   fails `conduit doctor`; install a listed release instead of generating
   tables on the spot.
6. **Lower numbers than the 5090 for non-GPU reasons**: compression is off
   pre-Blackwell (0028), so compare against bare metal on the same 4070, not
   against the 5090 table in HANDOFF.md.

Back out at any point with `HKLM\SOFTWARE\Helios!Icd = venus`.
