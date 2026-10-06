# GPU support: RTX 20 / 30 / 40 / 50

What Conduit needs to work on every GeForce RTX generation, for Linux guests
and Windows 11 guests (NVK-on-RM, Venus fallback). Everything so far ran on
one GPU only: an RTX 5090 (Blackwell, GB202). Nothing here has been run on
Turing, Ampere or Ada yet; every verdict for them is read from the code and
from upstream, not measured. Guesses are marked **(guess)**, things to check
on hardware **(to verify)**.

Related: [SECOND-MACHINE.md](SECOND-MACHINE.md) (setting up the second
machine), [NVK-ROADMAP.md](NVK-ROADMAP.md), [HANDOFF.md](HANDOFF.md).

## Generations

| | RTX 20 | RTX 30 | RTX 40 | RTX 50 |
|---|---|---|---|---|
| Architecture | Turing | Ampere | Ada Lovelace | Blackwell |
| Chips | TU102/104/106, TU116/117 | GA102/103/104/106/107 | AD102/103/104/106/107 | GB202/203/205/206/207 |
| RM architecture (`NV2080_CTRL_CMD_MC_GET_ARCH_INFO`) | 0x160 | 0x170 | 0x190 | 0x1B0 |
| Shader model | SM75 | SM86 | SM89 | SM120 |
| 3D class | TURING_A 0xc597 | AMPERE_B 0xc797 | ADA_A 0xc997 | BLACKWELL_B 0xce97 (GB20x; BLACKWELL_A 0xcd97 is GB10x) |
| Compute class | TURING_COMPUTE_A 0xc5c0 | AMPERE_COMPUTE_B 0xc7c0 | ADA_COMPUTE_A 0xc9c0 | BLACKWELL_COMPUTE_B 0xcec0 |
| GPFIFO class | TURING_CHANNEL_GPFIFO_A 0xc46f | AMPERE_CHANNEL_GPFIFO_A 0xc56f | AMPERE_CHANNEL_GPFIFO_A 0xc56f | BLACKWELL_CHANNEL_GPFIFO_B 0xca6f |
| Copy class | TURING_DMA_COPY_A 0xc5b5 | AMPERE_DMA_COPY_B 0xc7b5 | AMPERE_DMA_COPY_B 0xc7b5 | BLACKWELL_DMA_COPY_B 0xcab5 |
| Usermode (doorbell) class | TURING_USERMODE_A 0xc461 | AMPERE_USERMODE_A 0xc561 | AMPERE_USERMODE_A 0xc561 | BLACKWELL_USERMODE_A 0xc761 |
| Resizable BAR (GeForce) | no: BAR1 is 256 MiB | with a VBIOS/board update, otherwise 256 MiB | yes (e.g. 4070 12 GB: 16 GiB BAR1 **(guess: rounded up to pow2, to verify)**) | yes (5090: 32 GiB) |
| Block-linear GOBs | desktop (TuringColor2D) only | desktop only | desktop only | desktop + Blackwell 8-bit / 16-bit |
| DRM modifier sector layout `s` for 1-/2-byte formats | 1 | 1 | 1 | 2 / 3 |

The class ids are from the RM allocation table the Linux guest carries
(`guest/linux/gen/nvgpu_rmalloc_classes.h`, generated from the open modules)
and Mesa's class headers; which class each chip exposes **(to verify against
`NV0080_CTRL_CMD_GPU_GET_CLASSLIST` on the 4070)**: the per-chip pairing for
GPFIFO/copy/usermode on Ada (Ampere classes) is from memory of nouveau's
tables.

## Matrix

Status: **works** (measured), **expected** (code and upstream say it should,
not run), **needs work** (a known code change is needed), **blocked** (not
possible with the current stack).

### Host

| Requirement | RTX 20 | RTX 30 | RTX 40 | RTX 50 | Evidence |
|---|---|---|---|---|---|
| NVIDIA open kernel modules | expected | expected | expected | works | Open modules support Turing and later only (README of [open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules)); Blackwell requires them. `conduit doctor` refuses the closed modules: `cli/src/doctor.rs:77` |
| Driver release with RM ABI tables (580.178.04, 595.71.05, 595.104.02, 610.57.04, 615.71.09) | expected | expected | expected | works | Tables are **per driver release, not per GPU**: RM's ioctl/control layouts do not depend on the chip. `host/backend/gen/src/osdesc/mod.rs:185` picks the nearest older release; doctor checks the version against the supported list (`cli/src/doctor.rs:86`). A newer driver needs new tables (`host/backend/gen`, `.github/workflows/abi.yml`) |
| Turing still supported by the driver branch | expected | | | | The 580 branch is the last for Maxwell/Pascal/Volta; Turing stays supported in 590+ **(to verify in the release notes of the driver you install)** |
| BAR1 / RM window sizing | needs check: 256 MiB BAR1 | expected (ReBAR on) / needs check (off) | expected | works | `gpu.window_mib` defaults to the host GPU's BAR1 (`cli/src/config.rs:7`, `cli/src/lvrun.rs:639`); the KMD handles small windows (`guest/windows/kmd_logic/src/rm_window.rs:158`, test row "BAR1 256 MiB" at :814). Functionally fine, but a 256 MiB host-visible VRAM heap is far smaller than the 32 GiB the perf numbers were measured with (NVK's BAR heap, Mesa 0022/0045, falls back to system memory: 0025) **(guess: big perf drop for DXVK on Turing)** |
| Guest shared-memory BAR (64 GiB, QEMU) | expected | expected | expected | works | Not GPU-dependent: `cli/src/qemu.rs:147`, `docs/examples/win11.xml:71` |
| Venus host renderer (NVIDIA Vulkan) | expected | expected | expected | works | Conduit's virglrenderer patches use `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`, `VK_EXT_external_memory_host`, all exposed by NVIDIA's Linux driver on every Turing+ GPU in the 580+ branches **(to verify with `vulkaninfo` on the 4070)** |

### Guest RM client (librmclient) and Linux guest

| Requirement | RTX 20 | RTX 30 | RTX 40 | RTX 50 | Evidence |
|---|---|---|---|---|---|
| librmclient: no hardcoded GPU classes | expected | expected | expected | works | Library code only names FERMI_VASPACE_A / root/device classes (`guest/rmclient/src/nv_ioctl_defs.h:272`); classes come from the caller (NVK) |
| crm_smoke usermode probe | expected | expected | expected | works | Tries 0xc761, 0xc661, 0xc561, 0xc461 in turn (`guest/rmclient/tests/crm_smoke.c:165`) |
| Linux guest kernel module (conduit_gpu) | expected | expected | expected | works | Page-kind/sector-layout dev_info words are passed through from the host's node, not hardcoded (`guest/linux/conduit_gpu.c:580`); allocation-size table covers Turing..Blackwell classes (`guest/linux/gen/nvgpu_rmalloc_classes.h`) |
| Linux guest NVIDIA userspace (CUDA, Vulkan via the passed-through stack) | expected | expected | expected | works | Same RM, same driver release as the host |

### NVK / NAK (Mesa base 70c4c018, `guest/nvk-rm/build-windows.sh:50`)

| Requirement | RTX 20 | RTX 30 | RTX 40 | RTX 50 | Evidence |
|---|---|---|---|---|---|
| Upstream NVK hardware support | expected (conformant) | expected (conformant) | expected (conformant) | works | NVK is Vulkan 1.4 conformant on Turing, Ampere and Ada (Mesa 25.x, [docs/drivers/nvk.rst](https://docs.mesa3d.org/drivers/nvk.html)); Blackwell is newer upstream and is what we run |
| NAK shader compiler | expected | expected | expected | works | NAK targets SM50+ with Turing+ the primary target; SM75/86/89 are older and better trodden than SM120 |
| NVK on RM: class and device info from RM | expected **(to verify)** | expected **(to verify)** | expected **(to verify)** | works | Patch 0003 fills the physical device from RM; patch 0008 holds "fixes from the first run on an RTX 5090". Whether any value there is GB202-specific has to be checked on the 4070 (`vk_summary` prints them) |
| Per-draw patches (patches-common 0001-0007) | expected | expected | expected | works | Gated on `cls_eng3d >= TURING_A` (e.g. 0001 l.103); use Turing-era methods (NVC597) |
| Compression (0028) | off, works uncompressed | off | off | works | `has_compression = cls_eng3d >= BLACKWELL_A` (0028 l.55, l.175): pre-Blackwell images stay uncompressed. Correct but slower; pre-Blackwell compression needs comptags allocated through RM: **needs work** (perf only) |
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
- **RTX 40 (Ada)**: expected to work for DWM-on-NVK, games and scanout;
  1-/2-byte shared formats need the KMD fix; compression off (perf below
  the 5090 numbers, by more than the GPU difference **(guess)**). The best
  candidate after Blackwell: ReBAR, same driver releases, NVK conformant.
- **RTX 30 (Ampere)**: as Ada, plus ReBAR may be off (256 MiB BAR1) unless the
  board/VBIOS supports it.
- **RTX 20 (Turing)**: as Ampere, without ReBAR: BAR1 256 MiB, the
  host-visible VRAM heap is tiny; expect lower DXVK performance **(guess)**.
  Functionally expected to work.

## Code changes needed

| Change | Gens | Size | Where | Status |
|---|---|---|---|---|
| KMD: accept the desktop (s=1) family for 1-/2-byte planes on pre-Blackwell GPUs; family rule takes the GOB scheme as a parameter | 20/30/40 | S (~100 lines + tests) | `guest/windows/protocol/src/foreign.rs`, `guest/windows/kmd_logic/src/foreign_resource.rs` | in progress on branch `feat/multi-gpu` (not on main) |
| KMD: learn the GOB scheme (registry knob, or the host reports the GPU architecture through a capability bit) and pass it to validation | 20/30/40 | M; needs a WDK build | `guest/windows/kmd_render` (escape_foreign.rs, knob reading), host protocol | needs work |
| Pre-Blackwell compression: allocate comptags through RM, use compressible kinds | 20/30/40 | L; Mesa rebuild + measurement | Mesa patch 0028 | needs work (perf only) |
| RM ABI tables for any new driver release | all | M per release (generated) | `host/backend/gen`, `abi.yml` | as needed |
| `conduit doctor`: report GPU name, architecture, BAR1 size / ReBAR, warn on 256 MiB BAR1 | all | S | `cli/src/doctor.rs` | needs work (next) |
| crm_smoke: architecture names per family (TU10x/GA10x/AD10x/GB20x) | all | XS | `guest/rmclient/tests/crm_smoke.c:32` | cosmetic |

## Testing a new GPU

What `conduit doctor` should verify (today it checks the driver: open
modules, version, supported release; GPU name, architecture and BAR1 are not
yet shown):

1. Open kernel modules, a release with tables (`conduit doctor`).
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

1. **Host (10 min)**: install the open modules at a supported release
   (same as the 5090 host, e.g. 610.57.04 or 615.71.09); check ReBAR is on in
   the BIOS; `conduit doctor` all ok; `nvidia-smi -q` shows BAR1 ≈ 16 GiB
   **(to verify)**; `vulkaninfo --summary` lists the three extensions.
2. **Linux guest (20 min)**: `conduit up` a Linux guest; `make check smoke`
   in `guest/rmclient` (expect `Ada (AD100)` = arch 0x190 and usermode class
   0xc561); the other crm_* smokes; then `vk_summary`, `vk_compute_test`,
   `vk_offscreen_test`, `vk_bar_test`, `vk_bl_readback`,
   `vk_scanout_present` from `guest/nvk-rm/tests`. Save `vk_summary` output
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
2. **Something in NVK-on-RM's device setup tuned on GB202** (patches 0003,
   0008, 0022): a class, a heap size or an engine assumption. Shows up in
   `vk_summary` / the first NVK device creation. **(guess, to verify)**
3. **ReBAR off / smaller BAR1**: works but the host-visible heap shrinks;
   check BAR1 before measuring anything.
4. **Driver release mismatch**: a newer driver than the five with tables
   fails `conduit doctor`; install a listed release instead of generating
   tables on the spot.
5. **Lower numbers than the 5090 for non-GPU reasons**: compression is off
   pre-Blackwell (0028), so compare against bare metal on the same 4070, not
   against the 5090 table in HANDOFF.md.

Back out at any point with `HKLM\SOFTWARE\Helios!Icd = venus`.
